//! The entry point, ported from the Python SDK via the TypeScript one. Same methods, same
//! guarantees, same refusals. The class-level contract is documented on [`crate`].
//!
//! Blocking, deliberately: the method set is a port, and a blocking client forces no async
//! runtime choice on the integrator. A call that can take long — [`Wallet::wait`] — takes its
//! timeouts explicitly.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::amounts::parse_units;
use crate::digest::payload_digest_hex;
use crate::error::{Error, Result};
use crate::http::{url_encode, Http, Transport};
use crate::mpc::{commit as frost_commit, sign as frost_sign};
use crate::secret::VaultSecret;
use crate::types::{Asset, Balance, Movement, Quote};

/// A movement past `pending` in one of these states is already committed to the co-sign / relay
/// pipeline: a retry that replays it (same Idempotency-Key) converges on it instead of signing a
/// second time. The server's public vocabulary is `sdk/contract/fixtures/vocabulary.json`; an
/// earlier list here named internal states (signed, relaying, settling) the wire never carries, so
/// a replay of a `processing` movement raised instead of converging, and a `failed` one came back
/// as a success.
const PROGRESSED: [&str; 3] = ["processing", "completed", "refunded"];

/// A movement is settled (no more polling) once it reaches one of these.
const TERMINAL: [&str; 5] = ["completed", "failed", "refunded", "expired", "cancelled"];

const DEFAULT_BASE_URL: &str = "https://wallet.paymos.io";

/// Server-supplied hex. Slicing the string by byte index panicked on a multi-byte character, so a
/// malformed disclosure could abort the process instead of being refused; `hex::decode` cannot.
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    hex::decode(hex).map_err(|e| Error::paymos(format!("bad hex: {e}")))
}

/// A raw integer amount string → u128. An amount that overflows u128 is refused rather than
/// rounded: this code only ever runs on the money path, where fail-closed is the only answer.
fn raw_u128(raw: &str) -> Result<u128> {
    raw.parse::<u128>()
        .map_err(|_| Error::paymos(format!("bad raw amount: {raw:?}")))
}

/// uuid-v4-shaped hex without the dashes — the same shape the other SDKs generate.
fn fresh_idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex::encode(bytes)
}

/// Options for [`Wallet::swap`]. All fields optional; `Default` gives the SDK defaults.
#[derive(Debug, Default, Clone)]
pub struct SwapOptions {
    pub slippage_bps: Option<u32>,
    pub min_receive: Option<String>,
    pub max_debit: Option<String>,
    pub idempotency_key: Option<String>,
}

/// Options for [`Wallet::withdraw`]. All fields optional; `Default` gives the SDK defaults.
#[derive(Debug, Default, Clone)]
pub struct WithdrawOptions {
    pub mode: Option<String>,
    pub max_debit: Option<String>,
    pub idempotency_key: Option<String>,
}

/// One label's pinned fingerprints: a single `&str`/`String`, or several. Several are legitimate —
/// one coin can live at more than one contract under one label.
pub trait Fingerprints {
    fn into_fingerprints(self) -> Vec<String>;
}

impl Fingerprints for &str {
    fn into_fingerprints(self) -> Vec<String> {
        vec![self.to_string()]
    }
}

impl Fingerprints for String {
    fn into_fingerprints(self) -> Vec<String> {
        vec![self]
    }
}

impl<S: Into<String>> Fingerprints for Vec<S> {
    fn into_fingerprints(self) -> Vec<String> {
        self.into_iter().map(Into::into).collect()
    }
}

impl<S: Into<String> + Clone> Fingerprints for &[S] {
    fn into_fingerprints(self) -> Vec<String> {
        self.iter().cloned().map(Into::into).collect()
    }
}

pub struct Wallet {
    secret: VaultSecret,
    http: Http,
    /// The catalog as the server listed it, in order. A label may repeat (one coin, several
    /// contracts), which is why the fingerprint guard reads this and not the by-label map.
    asset_cache: Mutex<Option<Vec<Asset>>>,
    /// Label → lowercase fingerprints the integrator pinned out of band. A pinned label uses these
    /// and nothing /assets says; an unpinned one uses what the server publishes.
    pinned_fingerprints: HashMap<String, Vec<String>>,
}

impl Wallet {
    pub fn new(secret: &str) -> Result<Self> {
        Self::with_options(secret, None, None)
    }

    pub fn with_options(
        secret: &str,
        base_url: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let secret = VaultSecret::parse(secret)?;
        let base_url = base_url
            .map(str::to_string)
            .or_else(|| std::env::var("PAYMOS_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Wallet {
            http: Http::new(&base_url, &secret.api_key, timeout),
            secret,
            asset_cache: Mutex::new(None),
            pinned_fingerprints: HashMap::new(),
        })
    }

    /// Pin asset fingerprints out of band: label (`"USDC@base"`) → one fingerprint or several
    /// (lowercase hex SHA-256 of the raw token id). The co-sign guard otherwise trusts the
    /// fingerprints `/assets` publishes, which come from the same server that builds the messages
    /// to sign — enough to catch a bug or a compromised signing path, not a fully compromised
    /// server. For a pinned label only the pinned values count and the published ones are ignored;
    /// an unpinned label keeps using the published set. Compared case-insensitively.
    pub fn with_pinned_fingerprints<K, F, I>(mut self, pins: I) -> Self
    where
        I: IntoIterator<Item = (K, F)>,
        K: Into<String>,
        F: Fingerprints,
    {
        for (label, fps) in pins {
            self.pinned_fingerprints
                .entry(label.into())
                .or_default()
                .extend(fps.into_fingerprints().into_iter().map(|f| f.trim().to_lowercase()));
        }
        self
    }

    /// A wallet over a stubbed transport — tests never reach the network.
    pub fn with_transport(secret: &str, base_url: &str, transport: Box<dyn Transport>) -> Result<Self> {
        let secret = VaultSecret::parse(secret)?;
        Ok(Wallet {
            http: Http::with_transport(base_url, &secret.api_key, transport),
            secret,
            asset_cache: Mutex::new(None),
            pinned_fingerprints: HashMap::new(),
        })
    }

    /// Reads `PAYMOS_VAULT_SECRET`, the way a deployed process is configured.
    pub fn from_env() -> Result<Self> {
        let secret = std::env::var("PAYMOS_VAULT_SECRET")
            .map_err(|_| Error::paymos("PAYMOS_VAULT_SECRET is not set"))?;
        Self::new(&secret)
    }

    // ── reads ────────────────────────────────────────────────────────────────

    /// The server answers a BARE JSON array. This used to read `body.assets`, found nothing, and
    /// returned an empty catalog without an error — so every quote, swap and withdraw then failed
    /// with "unknown asset". Anything that is not an array is now a typed error, not an empty list.
    pub fn assets(&self) -> Result<Vec<Asset>> {
        let body = self.http.get("/vault/v1/assets", &[])?;
        from_array(body, "/vault/v1/assets")
    }

    /// A bare JSON array, like [`Wallet::assets`] — and it had the same silent-empty bug.
    pub fn balances(&self) -> Result<Vec<Balance>> {
        let body = self.http.get("/vault/v1/balances", &[])?;
        from_array(body, "/vault/v1/balances")
    }

    pub fn movement(&self, movement_id: &str) -> Result<Movement> {
        let body = self
            .http
            .get(&format!("/vault/v1/movements/{}", url_encode(movement_id)), &[])?;
        Ok(serde_json::from_value(body)?)
    }

    /// A page of the caller's movement history (newest first, summary rows without fee legs).
    /// Returns `(items, next_cursor)`; pass `next_cursor` back as `cursor` for the next page
    /// (`None` when exhausted). The server speaks keyset pagination: `items` + `next_cursor`.
    pub fn movements(&self, limit: u32, cursor: Option<&str>) -> Result<(Vec<Movement>, Option<String>)> {
        let body = self.http.get(
            "/vault/v1/movements",
            &[
                ("limit", Some(limit.to_string())),
                ("cursor", cursor.map(str::to_string)),
            ],
        )?;
        // `items` is required: a page without it is not this contract, and an empty history would
        // be the wrong way to say so.
        let items = from_array(
            body.get("items").cloned().unwrap_or(Value::Null),
            "/vault/v1/movements items",
        )?;
        let next_cursor = body
            .get("next_cursor")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok((items, next_cursor))
    }

    /// Polls until the movement leaves the states that are still in flight. Terminal:
    /// `completed | failed | refunded | expired | cancelled`.
    pub fn wait(&self, movement_id: &str, timeout: Duration, interval: Duration) -> Result<Movement> {
        let deadline = Instant::now() + timeout;
        loop {
            let mv = self.movement(movement_id)?;
            if TERMINAL.contains(&mv.status.as_str()) {
                return Ok(mv);
            }
            if Instant::now() >= deadline {
                return Err(Error::paymos(format!("timed out waiting for movement {movement_id}")));
            }
            std::thread::sleep(interval);
        }
    }

    // ── quotes ───────────────────────────────────────────────────────────────

    /// A dry preview. Nothing is created and nothing is signed.
    pub fn quote_swap(
        &self,
        send: &str,
        receive: &str,
        amount: &str,
        slippage_bps: u32,
    ) -> Result<Quote> {
        self.validate_amount(amount, send)?;
        self.decimals_of(receive)?;
        self.quote(
            "/vault/v1/quote/swap",
            json!({
                "send": send,
                "receive": receive,
                "amount": amount,
                "mode": "exact_in",
                "slippage_bps": slippage_bps,
            }),
            true,
            None,
        )
    }

    /// A dry preview. Nothing is created and nothing is signed.
    pub fn quote_withdraw(&self, asset: &str, amount: &str, to: &str, mode: &str) -> Result<Quote> {
        self.validate_amount(amount, asset)?;
        self.quote(
            "/vault/v1/quote/withdraw",
            json!({ "asset": asset, "amount": amount, "to": to, "mode": mode }),
            true,
            None,
        )
    }

    // ── money ────────────────────────────────────────────────────────────────

    pub fn swap(&self, send: &str, receive: &str, amount: &str, options: SwapOptions) -> Result<Movement> {
        self.require_share("swap")?;
        let send_dec = self.validate_amount(amount, send)?;
        let recv_dec = self.decimals_of(receive)?;

        let q = self.quote(
            "/vault/v1/quote/swap",
            json!({
                "send": send,
                "receive": receive,
                "amount": amount,
                "mode": "exact_in",
                "slippage_bps": options.slippage_bps.unwrap_or(50),
            }),
            false,
            Some(
                options
                    .idempotency_key
                    .clone()
                    .unwrap_or_else(fresh_idempotency_key),
            ),
        )?;

        if let Some(min_receive) = &options.min_receive {
            let floor = raw_u128(&parse_units(min_receive, recv_dec)?)?;
            if raw_u128(&q.receive.min)? < floor {
                return Err(Error::SlippageExceeded {
                    message: format!(
                        "guaranteed receive {} is below the requested minimum {floor} ({min_receive} {receive})",
                        q.receive.min
                    ),
                    status: None,
                });
            }
        }
        // A swap is exact_in, so the send leg must be exactly what was approved.
        let expect_send = raw_u128(&parse_units(amount, send_dec)?)?;
        verify_quote_echo(&q, send, receive, Some(expect_send), None, None)?;
        enforce_max_debit(&q, options.max_debit.as_deref(), send_dec)?;
        self.cosign(&q)?;
        self.movement(require_movement_id(&q)?)
    }

    pub fn withdraw(
        &self,
        asset: &str,
        amount: &str,
        to: &str,
        options: WithdrawOptions,
    ) -> Result<Movement> {
        self.require_share("withdraw")?;
        let dec = self.validate_amount(amount, asset)?;
        let mode = options.mode.as_deref().unwrap_or("exact_out");

        let q = self.quote(
            "/vault/v1/quote/withdraw",
            json!({ "asset": asset, "amount": amount, "to": to, "mode": mode }),
            false,
            Some(
                options
                    .idempotency_key
                    .clone()
                    .unwrap_or_else(fresh_idempotency_key),
            ),
        )?;

        // A withdraw is same-asset. For exact_out the approved amount is the guaranteed OUT; for
        // total_in it is the total DEBIT — and the send leg is total minus fees, so pinning send
        // would false-fail every time.
        let expected = raw_u128(&parse_units(amount, dec)?)?;
        if mode == "total_in" {
            verify_quote_echo(&q, asset, asset, None, None, Some(expected))?;
        } else {
            verify_quote_echo(&q, asset, asset, None, Some(expected), None)?;
        }

        enforce_max_debit(&q, options.max_debit.as_deref(), dec)?;
        self.cosign(&q)?;
        self.movement(require_movement_id(&q)?)
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn require_share(&self, op: &str) -> Result<()> {
        if !self.secret.has_share() {
            return Err(Error::paymos(format!(
                "{op} needs a full-scope secret: this one carries no co-signing share"
            )));
        }
        Ok(())
    }

    fn catalog(&self) -> Result<Vec<Asset>> {
        let mut cache = self.asset_cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            *cache = Some(self.assets()?);
        }
        Ok(cache.as_ref().unwrap().clone())
    }

    /// By label, for decimals. A label listed more than once resolves to its FIRST entry, as the
    /// server resolves it; a plain `collect()` kept the last, so amount conversion and the echo
    /// check could scale by decimals the server never used.
    fn asset_map(&self) -> Result<HashMap<String, Asset>> {
        let mut map = HashMap::new();
        for a in self.catalog()? {
            map.entry(a.asset.clone()).or_insert(a);
        }
        Ok(map)
    }

    /// Every fingerprint a source of `label` may carry: the pinned ones if the label is pinned (the
    /// integrator vouches for them), otherwise every one /assets publishes under it with the same
    /// decimals as the label's first entry. One label is one coin, so any of its contracts is the
    /// approved asset; a by-label map would keep only the last and refuse the rest. None at all
    /// refuses the co-sign — never skip the check.
    fn accepted_fingerprints(&self, label: &str) -> Result<Vec<String>> {
        if let Some(pinned) = self.pinned_fingerprints.get(label) {
            let pinned: Vec<String> = pinned.iter().filter(|f| !f.is_empty()).cloned().collect();
            if pinned.is_empty() {
                return Err(Error::paymos(format!(
                    "co-sign refused: {label} is pinned with no fingerprint"
                )));
            }
            return Ok(pinned);
        }
        // Only entries whose decimals match the FIRST entry's: the debit this guard compares raw
        // amounts against was computed in those decimals, so a same-label token with other
        // decimals could move 10^Δ times the value within the same raw figure.
        let entries: Vec<Asset> = self.catalog()?.into_iter().filter(|a| a.asset == label).collect();
        let first_decimals = entries.first().map(|a| a.decimals);
        let published: Vec<String> = entries
            .into_iter()
            .filter(|a| Some(a.decimals) == first_decimals)
            .filter_map(|a| a.fingerprint)
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty())
            .collect();
        if published.is_empty() {
            return Err(Error::paymos(
                "co-sign refused: the server did not publish an asset fingerprint — update the server",
            ));
        }
        Ok(published)
    }

    fn decimals_of(&self, asset: &str) -> Result<u32> {
        self.asset_map()?
            .get(asset)
            .map(|a| a.decimals)
            .ok_or_else(|| Error::paymos(format!("unknown asset: {asset}")))
    }

    fn validate_amount(&self, amount: &str, asset: &str) -> Result<u32> {
        let dec = self.decimals_of(asset)?;
        parse_units(amount, dec)?; // errors on a bad decimal before anything reaches the server
        Ok(dec)
    }

    fn quote(
        &self,
        path: &str,
        mut body: Value,
        dry: bool,
        idempotency_key: Option<String>,
    ) -> Result<Quote> {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("dry".to_string(), Value::Bool(dry));
        }
        let headers: Vec<(&str, &str)> = match &idempotency_key {
            Some(key) => vec![("idempotency-key", key.as_str())],
            None => vec![],
        };
        let body = self.http.post(path, &body, &headers)?;
        Ok(serde_json::from_value(body)?)
    }

    /// Drives the 2-of-2 round: commit per source, verify the disclosure, sign, aggregate.
    fn cosign(&self, q: &Quote) -> Result<()> {
        let mid = require_movement_id(q)?;
        let n = q
            .sources
            .filter(|n| *n >= 1)
            .ok_or_else(|| Error::paymos("quote is missing its source count — cannot co-sign"))?;

        // The tokens a source may move: the send asset's fingerprints, pinned or published.
        // Resolved before a single commitment exists; none at all gets no signature — skipping
        // the check would be the blind sign it exists to prevent.
        let fingerprints = self.accepted_fingerprints(&q.send.asset)?;

        let kp = self.client_key_package()?;
        let signer = vault_signer(&kp)?;
        let mut commitments = Vec::new();
        let mut nonces = Vec::new();
        for _ in 0..n {
            let (c, nn) = frost_commit(&kp)?;
            commitments.push(c);
            nonces.push(nn);
        }

        let begin = match self
            .http
            .post(&format!("/vault/v1/movements/{mid}/sign/begin"), &json!({ "commitments": commitments }), &[])
        {
            Ok(body) => body,
            Err(e) => {
                // An ambiguous failure after the server already progressed the movement is not a
                // reason to sign it a second time.
                let mv = self.movement(mid)?;
                if PROGRESSED.contains(&mv.status.as_str()) {
                    return Ok(());
                }
                return Err(e);
            }
        };

        let token = begin
            .get("token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::paymos("co-sign protocol mismatch: sign/begin returned no session token"))?
            .to_string();
        let packages = begin
            .get("signing_packages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if packages.len() != n as usize {
            return Err(Error::paymos(format!(
                "co-sign protocol mismatch: expected {n} signing packages, got {}",
                packages.len()
            )));
        }

        verify_signing_disclosure(
            q,
            &packages,
            begin.get("messages").cloned().unwrap_or(Value::Null),
            &signer,
            &fingerprints,
        )?;

        let mut shares = Vec::new();
        for (pkg, nn) in packages.iter().zip(nonces.iter()) {
            shares.push(frost_sign(pkg, nn, &kp)?);
        }
        let res = self.http.post(
            &format!("/vault/v1/movements/{mid}/sign/aggregate"),
            &json!({ "token": token, "shares": shares }),
            &[],
        )?;
        if res.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let msg = res
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("sign failed");
            return Err(Error::paymos(msg));
        }
        Ok(())
    }

    fn client_key_package(&self) -> Result<Value> {
        let share = self
            .secret
            .share
            .as_ref()
            .ok_or_else(|| Error::paymos("this secret carries no co-signing share"))?;
        Ok(serde_json::from_str(share)?)
    }
}

fn require_movement_id(q: &Quote) -> Result<&str> {
    q.movement_id
        .as_deref()
        .ok_or_else(|| Error::paymos("quote created no movement — cannot sign a dry preview"))
}

/// Refuse to sign unless the server's quote echoes what the caller approved: a units bug or
/// contract drift changes one of these numbers, and the change is caught before a commitment
/// exists. It compares assets and amounts only. The debit of an exact_out quote is computed by the
/// server and is bounded here only when the caller passes `max_debit`.
fn verify_quote_echo(
    q: &Quote,
    send_asset: &str,
    receive_asset: &str,
    expect_send_raw: Option<u128>,
    expect_receive_raw: Option<u128>,
    max_debit_raw: Option<u128>,
) -> Result<()> {
    if q.send.asset != send_asset || q.receive.asset != receive_asset {
        return Err(Error::paymos(format!(
            "quote is for {}→{}, not the {send_asset}→{receive_asset} that was approved",
            q.send.asset, q.receive.asset
        )));
    }
    if let Some(expect) = expect_send_raw {
        if raw_u128(&q.send.amount)? != expect {
            return Err(Error::paymos(format!(
                "quote sends {}, not the approved {expect}",
                q.send.amount
            )));
        }
    }
    // exact_out: the recipient gets exactly the approved amount, not "at least" it. An earlier
    // version compared `receive.min >= expect`, which let a quote paying out MORE than approved
    // (and debiting for it) through; Python pins the amount, and so does this.
    if let Some(expect) = expect_receive_raw {
        if raw_u128(&q.receive.amount)? != expect {
            return Err(Error::paymos(format!(
                "quote receives {}, not the approved {expect}",
                q.receive.amount
            )));
        }
    }
    if let Some(cap) = max_debit_raw {
        if raw_u128(&q.debit.amount)? > cap {
            return Err(Error::paymos(format!(
                "quote debits {}, above the approved {cap}",
                q.debit.amount
            )));
        }
    }
    Ok(())
}

fn enforce_max_debit(q: &Quote, max_debit: Option<&str>, decimals: u32) -> Result<()> {
    let Some(max_debit) = max_debit else { return Ok(()) };
    let cap = raw_u128(&parse_units(max_debit, decimals)?)?;
    if raw_u128(&q.debit.amount)? > cap {
        return Err(Error::paymos(format!(
            "quote debits {}, above the cap {cap} ({max_debit})",
            q.debit.amount
        )));
    }
    Ok(())
}

/// Never blind-sign. The server discloses, per source, the plaintext message it wants signed.
/// For each one this recomputes the digest and confirms the package about to be signed binds to
/// exactly that message; confirms the message is a single transfer from THIS vault, of a token
/// whose SHA-256 is one of the send asset's fingerprints; and confirms the sources together move
/// no more than the approved debit. Any mismatch aborts before a share is produced.
///
/// What it does NOT check: the recipient. The transfer goes to a route deposit address the server
/// chooses, and nothing here verifies it. "No more than the approved debit" is also only as tight
/// as that debit — for exact_out it is server-computed unless the caller set `max_debit`.
///
/// The fingerprints are the ones the server publishes unless the integrator pinned them. Published
/// ones come from the server that also builds these messages: the binding catches a server bug or
/// a compromised signing path, not a server compromised end to end — pinning is the anchor for
/// that.
fn verify_signing_disclosure(
    q: &Quote,
    packages: &[Value],
    messages: Value,
    expected_signer: &str,
    send_fingerprints: &[String],
) -> Result<()> {
    let messages = messages.as_array().ok_or_else(|| {
        Error::paymos("co-sign refused: the server did not disclose the signing messages — refusing to blind-sign")
    })?;
    if messages.len() != packages.len() {
        return Err(Error::paymos(
            "co-sign refused: the server did not disclose the signing messages — refusing to blind-sign",
        ));
    }
    let mut total: u128 = 0;

    for (i, package) in packages.iter().enumerate() {
        let disc = &messages[i];
        let message = disc.get("message").and_then(|v| v.as_str());
        let nonce_hex = disc.get("nonce").and_then(|v| v.as_str());
        let recipient = disc.get("recipient").and_then(|v| v.as_str());
        let (Some(message), Some(nonce_hex), Some(recipient)) = (message, nonce_hex, recipient) else {
            return Err(Error::paymos(format!("co-sign refused: malformed disclosure for source {i}")));
        };

        // (1) binding: the package about to be signed hashes to EXACTLY the disclosed message. A
        // nonce that is not 32 bytes of hex is a malformed disclosure, refused like any other.
        let want = hex_to_bytes(nonce_hex)
            .and_then(|nonce| payload_digest_hex(message, &nonce, recipient, None))
            .map_err(|e| {
                Error::paymos(format!("co-sign refused: malformed disclosure for source {i}: {}", e.message()))
            })?;
        let got = package.get("message").and_then(|v| v.as_str()).unwrap_or("");
        if !got.eq_ignore_ascii_case(&want) {
            return Err(Error::paymos(format!(
                "co-sign refused: signing package {i} does not bind to the disclosed message"
            )));
        }

        // (2) structure and own-vault: one transfer, signed from this vault and no other.
        let parsed: Value = serde_json::from_str(message).map_err(|e| {
            Error::paymos(format!("co-sign refused: unreadable message for source {i}: {e}"))
        })?;
        let list = parsed.get("intents").and_then(|v| v.as_array());
        // A message that names no signer is unreadable, not "from anyone".
        let signer_id = parsed
            .get("signer_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::paymos(format!("co-sign refused: unreadable message for source {i}: no signer_id")))?
            .to_lowercase();
        if list.map(|l| l.len()) != Some(1) {
            return Err(Error::paymos(format!("co-sign refused: source {i} is not a single transfer")));
        }
        let one = &list.unwrap()[0];
        if one.get("intent").and_then(|v| v.as_str()) != Some("transfer") {
            return Err(Error::paymos(format!("co-sign refused: source {i} is not a single transfer")));
        }
        if signer_id != expected_signer {
            return Err(Error::paymos(format!("co-sign refused: source {i} signs from a different vault")));
        }
        let tokens = one.get("tokens").and_then(|v| v.as_object()).ok_or_else(|| {
            Error::paymos(format!("co-sign refused: source {i} does not transfer exactly one token"))
        })?;
        if tokens.len() != 1 {
            return Err(Error::paymos(format!(
                "co-sign refused: source {i} does not transfer exactly one token"
            )));
        }
        // (3) the token: the sum below is only a limit in the send asset's units if every source
        // moves the send asset — as far as the fingerprints in hand can tell. Without this, a buggy
        // or subverted signing path could spend 10.025 of a costlier token against a USDC debit.
        let Some((token, value)) = tokens.iter().next() else {
            return Err(Error::paymos(format!("co-sign refused: source {i} does not transfer exactly one token")));
        };
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        if !send_fingerprints.iter().any(|f| *f == hash) {
            return Err(Error::paymos(format!(
                "co-sign refused: source {i} moves a token other than the approved {}",
                q.send.asset
            )));
        }
        let amount = Some(value)
            .and_then(|v| v.as_str())
            .and_then(positive_raw)
            .ok_or_else(|| Error::paymos(format!("co-sign refused: bad transfer amount for source {i}")))?;
        total = total
            .checked_add(amount)
            .ok_or_else(|| Error::paymos("co-sign refused: transfer amounts overflow"))?;
    }

    // (4) no inflation: the sources together move no more than the approved debit.
    let approved = raw_u128(&q.debit.amount)?;
    if total > approved {
        return Err(Error::paymos(format!(
            "co-sign refused: sources move {total}, above the approved debit {approved}"
        )));
    }
    Ok(())
}

/// This vault's group key, which every disclosed `signer_id` must equal. Without it there is
/// nothing to compare against, and a check with nothing to compare is a skipped check — so its
/// absence refuses the co-sign before a commitment exists.
fn vault_signer(kp: &Value) -> Result<String> {
    kp.get("verifying_key")
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_lowercase)
        .ok_or_else(|| Error::paymos("co-sign refused: the key package carries no verifying key"))
}

/// A disclosed per-source amount: ASCII digits only, and more than zero. The guard below sums the
/// sources and compares the sum to the approved debit, so a parser that took a sign would let a
/// server disclose `2D` and `-D` — a sum of `D` that passes while the first source alone moves
/// `2D`. `u128::from_str` refuses `-` but accepts a leading `+`, so it is not trusted with this.
fn positive_raw(s: &str) -> Option<u128> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u128>().ok().filter(|n| *n > 0)
}

/// A bare JSON array of `T`. Anything else — the object wrapper an older SDK imagined, `null` from
/// an empty 2xx — is the server not speaking this contract, and says so.
fn from_array<T: serde::de::DeserializeOwned>(body: Value, path: &str) -> Result<Vec<T>> {
    if !body.is_array() {
        let shape = match &body {
            Value::Object(_) => "an object",
            Value::Null => "no JSON",
            _ => "a scalar",
        };
        return Err(Error::paymos(format!("bad wire JSON: {path} answered {shape}, expected an array")));
    }
    Ok(serde_json::from_value(body)?)
}

// serde_json::Error → crate Error, keeping the conversion at the call sites invisible.
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::paymos(format!("bad wire JSON: {e}"))
    }
}
