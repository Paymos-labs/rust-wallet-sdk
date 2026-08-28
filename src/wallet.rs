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

use crate::amounts::parse_units;
use crate::digest::payload_digest_hex;
use crate::error::{Error, Result};
use crate::http::{url_encode, Http, Transport};
use crate::mpc::{commit as frost_commit, sign as frost_sign};
use crate::secret::VaultSecret;
use crate::types::{Asset, Balance, Movement, Quote};

/// A movement in any of these states has already been signed; a retry must not sign again.
const PROGRESSED: [&str; 6] = ["signed", "relaying", "relayed", "settling", "completed", "failed"];

/// A movement is settled (no more polling) once it reaches one of these.
const TERMINAL: [&str; 5] = ["completed", "failed", "refunded", "expired", "cancelled"];

const DEFAULT_BASE_URL: &str = "https://wallet.paymos.io";

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(Error::paymos(format!("odd-length hex: {} chars", hex.len())));
    }
    (0..hex.len() / 2)
        .map(|i| {
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| Error::paymos(format!("bad hex: {e}")))
        })
        .collect()
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

pub struct Wallet {
    secret: VaultSecret,
    http: Http,
    asset_cache: Mutex<Option<HashMap<String, Asset>>>,
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
        })
    }

    /// A wallet over a stubbed transport — tests never reach the network.
    pub fn with_transport(secret: &str, base_url: &str, transport: Box<dyn Transport>) -> Result<Self> {
        let secret = VaultSecret::parse(secret)?;
        Ok(Wallet {
            http: Http::with_transport(base_url, &secret.api_key, transport),
            secret,
            asset_cache: Mutex::new(None),
        })
    }

    /// Reads `PAYMOS_VAULT_SECRET`, the way a deployed process is configured.
    pub fn from_env() -> Result<Self> {
        let secret = std::env::var("PAYMOS_VAULT_SECRET")
            .map_err(|_| Error::paymos("PAYMOS_VAULT_SECRET is not set"))?;
        Self::new(&secret)
    }

    // ── reads ────────────────────────────────────────────────────────────────

    pub fn assets(&self) -> Result<Vec<Asset>> {
        let body = self.http.get("/vault/v1/assets", &[])?;
        Ok(serde_json::from_value(
            body.get("assets").cloned().unwrap_or(Value::Array(vec![])),
        )?)
    }

    pub fn balances(&self) -> Result<Vec<Balance>> {
        let body = self.http.get("/vault/v1/balances", &[])?;
        Ok(serde_json::from_value(
            body.get("balances").cloned().unwrap_or(Value::Array(vec![])),
        )?)
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
        let items = serde_json::from_value(
            body.get("items").cloned().unwrap_or(Value::Array(vec![])),
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
            json!({ "send": send, "receive": receive, "amount": amount, "slippage_bps": slippage_bps }),
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

    fn asset_map(&self) -> Result<HashMap<String, Asset>> {
        let mut cache = self.asset_cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            let map = self
                .assets()?
                .into_iter()
                .map(|a| (a.asset.clone(), a))
                .collect();
            *cache = Some(map);
        }
        Ok(cache.as_ref().unwrap().clone())
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

        let kp = self.client_key_package()?;
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

        verify_signing_disclosure(q, &packages, begin.get("messages").cloned().unwrap_or(Value::Null), &kp)?;

        let mut shares = Vec::new();
        for (pkg, nn) in packages.iter().zip(nonces.iter()) {
            shares.push(frost_sign(pkg, nn, &kp)?);
        }
        let res = self.http.post(
            &format!("/vault/v1/movements/{mid}/sign/aggregate"),
            &json!({ "token": begin.get("token").cloned().unwrap_or(Value::Null), "shares": shares }),
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

/// Refuse to sign unless the server's quote echoes what the caller approved. This is what makes
/// co-signing a server-built movement safe: a units bug, contract drift or a compromised server
/// changes one of these numbers, and the change is caught before a commitment exists.
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
    if let Some(expect) = expect_receive_raw {
        if raw_u128(&q.receive.min)? < expect {
            return Err(Error::paymos(format!(
                "quote guarantees {}, below the approved {expect}",
                q.receive.min
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
/// exactly that message; confirms the message is a single transfer from THIS vault; and confirms
/// the sources together move no more than the approved debit. Any mismatch aborts before a share
/// is produced.
fn verify_signing_disclosure(q: &Quote, packages: &[Value], messages: Value, kp: &Value) -> Result<()> {
    let messages = messages.as_array().ok_or_else(|| {
        Error::paymos("co-sign refused: the server did not disclose the signing messages — refusing to blind-sign")
    })?;
    if messages.len() != packages.len() {
        return Err(Error::paymos(
            "co-sign refused: the server did not disclose the signing messages — refusing to blind-sign",
        ));
    }
    let expected_signer = kp
        .get("verifying_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let mut total: u128 = 0;

    for (i, package) in packages.iter().enumerate() {
        let disc = &messages[i];
        let message = disc.get("message").and_then(|v| v.as_str());
        let nonce_hex = disc.get("nonce").and_then(|v| v.as_str());
        let recipient = disc.get("recipient").and_then(|v| v.as_str());
        let (Some(message), Some(nonce_hex), Some(recipient)) = (message, nonce_hex, recipient) else {
            return Err(Error::paymos(format!("co-sign refused: malformed disclosure for source {i}")));
        };

        // (1) binding: the package about to be signed hashes to EXACTLY the disclosed message.
        let want = payload_digest_hex(message, &hex_to_bytes(nonce_hex)?, recipient, None)?;
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
        let signer_id = parsed
            .get("signer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if list.map(|l| l.len()) != Some(1) {
            return Err(Error::paymos(format!("co-sign refused: source {i} is not a single transfer")));
        }
        let one = &list.unwrap()[0];
        if one.get("intent").and_then(|v| v.as_str()) != Some("transfer") {
            return Err(Error::paymos(format!("co-sign refused: source {i} is not a single transfer")));
        }
        if !expected_signer.is_empty() && signer_id != expected_signer {
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
        let amount = tokens.values().next().and_then(|v| v.as_str()).ok_or_else(|| {
            Error::paymos(format!("co-sign refused: bad transfer amount for source {i}"))
        })?;
        total = total
            .checked_add(raw_u128(amount).map_err(|_| {
                Error::paymos(format!("co-sign refused: bad transfer amount for source {i}"))
            })?)
            .ok_or_else(|| Error::paymos("co-sign refused: transfer amounts overflow"))?;
    }

    // (3) no inflation: the sources together move no more than the approved debit.
    let approved = raw_u128(&q.debit.amount)?;
    if total > approved {
        return Err(Error::paymos(format!(
            "co-sign refused: sources move {total}, above the approved debit {approved}"
        )));
    }
    Ok(())
}

// serde_json::Error → crate Error, keeping the conversion at the call sites invisible.
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::paymos(format!("bad wire JSON: {e}"))
    }
}
