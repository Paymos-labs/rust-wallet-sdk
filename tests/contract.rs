//! The wire contract, read from disk. `sdk/contract/fixtures` is what the server actually answers,
//! recorded from its real request pipeline; every test here serves those bytes through a mocked
//! transport and checks the SDK both parses them and sends what the server was recorded receiving.
//!
//! Why it exists: this SDK once read `{"assets": [...]}` from an endpoint that has always answered
//! a bare array. Its own tests mocked the server its author imagined and passed, and no money call
//! could succeed. A fixture that cannot be found is a failure, never a skip — a suite that quietly
//! stops reading the contract is exactly how that happened.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paymos_wallet::{
    payload_digest_hex, Error, Http, HttpRequest, HttpResponse, Method, Movement, Quote, Transport,
    VaultSecret, Wallet, WithdrawOptions, SwapOptions,
};
use serde_json::{json, Value};

const BASE: &str = "https://wallet.test";
const TO: &str = "0xabc0000000000000000000000000000000000000";

/// Python's `_TERMINAL_STATUSES` and `_PROGRESSED_STATUSES`, the reference this SDK must agree with.
const PY_TERMINAL: [&str; 5] = ["completed", "failed", "refunded", "expired", "cancelled"];
const PY_PROGRESSED: [&str; 3] = ["processing", "completed", "refunded"];

// ── fixtures ────────────────────────────────────────────────────────────────

/// Walk up from this test file: `contract/fixtures` (the public mirror's layout), then
/// `sdk/contract/fixtures` (this monorepo's). Neither found is a failure.
fn fixtures_dir() -> PathBuf {
    let start = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for candidate in ["contract/fixtures", "sdk/contract/fixtures"] {
        let mut dir: Option<&Path> = Some(&start);
        while let Some(d) = dir {
            let p = d.join(candidate);
            if p.is_dir() {
                return p;
            }
            dir = d.parent();
        }
    }
    panic!(
        "wire-contract fixtures not found walking up from {} — looked for contract/fixtures and \
         sdk/contract/fixtures. The contract tests never skip.",
        start.display()
    );
}

fn fixture(name: &str) -> Value {
    let path = fixtures_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn response_of(name: &str) -> (u16, Value) {
    let f = fixture(name);
    (f["response"]["status"].as_u64().unwrap() as u16, f["response"]["body"].clone())
}

// ── the mocked server ───────────────────────────────────────────────────────

type Responder = Box<dyn Fn(&HttpRequest) -> (u16, String)>;

/// Routes by method + path (query included). An unrouted request answers the server's own 404
/// envelope, and every request is recorded for the assertions that follow.
struct Server {
    routes: Vec<(Method, String, Responder)>,
    seen: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Server {
    fn new() -> Self {
        Server { routes: vec![], seen: Arc::new(Mutex::new(vec![])) }
    }

    fn route(mut self, method: Method, path: &str, f: impl Fn(&HttpRequest) -> (u16, String) + 'static) -> Self {
        self.routes.push((method, path.to_string(), Box::new(f)));
        self
    }

    /// Serve a fixture's recorded response at its recorded method and path.
    fn fixture(self, name: &str) -> Self {
        let f = fixture(name);
        let method = if f["request"]["method"] == "GET" { Method::Get } else { Method::Post };
        let path = f["request"]["path"].as_str().unwrap().to_string();
        let (status, body) = response_of(name);
        let body = body.to_string();
        self.route(method, &path, move |_| (status, body.clone()))
    }

    fn json(self, method: Method, path: &str, status: u16, body: Value) -> Self {
        let body = body.to_string();
        self.route(method, path, move |_| (status, body.clone()))
    }

    fn requests(&self) -> Arc<Mutex<Vec<HttpRequest>>> {
        self.seen.clone()
    }
}

impl Transport for Server {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        self.seen.lock().unwrap().push(req.clone());
        let path = req.url.strip_prefix(BASE).expect("request went to the configured base URL");
        for (method, p, f) in &self.routes {
            if *method == req.method && p == path {
                let (status, body) = f(req);
                return Ok(HttpResponse { status, headers: vec![], body });
            }
        }
        Ok(HttpResponse { status: 404, headers: vec![], body: json!({ "error": "not found" }).to_string() })
    }
}

fn read_secret() -> String {
    VaultSecret::pack("vk_live_contract", 7, None)
}

fn read_wallet(server: Server) -> Wallet {
    Wallet::with_transport(&read_secret(), BASE, Box::new(server)).unwrap()
}

fn find<'a>(seen: &'a [HttpRequest], method: Method, path: &str) -> &'a HttpRequest {
    seen.iter()
        .find(|r| r.method == method && r.url == format!("{BASE}{path}"))
        .unwrap_or_else(|| panic!("no {method:?} {path} was sent; sent: {:?}", seen.iter().map(|r| &r.url).collect::<Vec<_>>()))
}

fn body_of(req: &HttpRequest) -> Value {
    serde_json::from_str(req.body.as_deref().expect("a POST carries a body")).unwrap()
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Every field the recorded request carried, the SDK sent with the same value and JSON type. The
/// only field it may add is `dry: false` on a money quote (the server's default, and Python's).
fn assert_request_matches(sent: &Value, name: &str) {
    let recorded = fixture(name)["request"]["body"].clone();
    let recorded = recorded.as_object().unwrap();
    let sent = sent.as_object().unwrap();
    for (key, value) in recorded {
        assert_eq!(sent.get(key), Some(value), "{name}: field {key:?} differs; sent {sent:?}");
    }
    for (key, value) in sent {
        if !recorded.contains_key(key) {
            assert!(key == "dry" && value == &json!(false), "{name}: sent an unrecorded field {key:?}={value}");
        }
    }
}

// ── reads ───────────────────────────────────────────────────────────────────

#[test]
fn assets_is_a_bare_array_and_every_field_parses() {
    let server = Server::new().fixture("assets");
    let seen = server.requests();
    let assets = read_wallet(server).assets().unwrap();

    assert_eq!(assets.len(), 2, "an empty catalog here is the bug this file exists for");
    assert_eq!(assets[0].asset, "USDC@base");
    assert_eq!(assets[0].symbol, "USDC");
    assert_eq!(assets[0].chain, "base");
    assert_eq!(assets[0].decimals, 6);
    assert_eq!(
        assets[0].fingerprint.as_deref(),
        Some("a42c7e46411394e7c7719eef5be19239b2716b91d3a4a843fd3d2c0b6ca16b7c")
    );
    assert_eq!(assets[1].asset, "USDT@base");
    assert_eq!(assets[1].decimals, 6);
    assert_eq!(
        assets[1].fingerprint.as_deref(),
        Some("4aa51775b928a58a1ff49de71caf2cd78e1bf728d2853d7193d14cf30728f039")
    );
    // The fingerprint is sha256 of the raw token id, lowercase hex — the thing the guard recomputes.
    assert_eq!(assets[0].fingerprint.as_deref(), Some(sha256_hex(USDC_TOKEN).as_str()));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::Get);
    assert_eq!(seen[0].url, format!("{BASE}/vault/v1/assets"));
}

#[test]
fn balances_is_a_bare_array_and_every_field_parses() {
    let balances = read_wallet(Server::new().fixture("balances")).balances().unwrap();
    assert_eq!(balances.len(), 1);
    let b = &balances[0];
    assert_eq!(b.asset, "USDC@base");
    assert_eq!(b.symbol, "USDC");
    assert_eq!(b.chain, "base");
    assert_eq!(b.decimals, 6);
    assert_eq!(b.amount_raw, "1000000000");
    assert_eq!(b.usd.as_deref(), Some("1000"));
}

#[test]
fn an_unpriced_balance_omits_usd_and_still_parses() {
    let (_, mut body) = response_of("balances");
    body[0].as_object_mut().unwrap().remove("usd");
    let server = Server::new().json(Method::Get, "/vault/v1/balances", 200, body);
    assert_eq!(read_wallet(server).balances().unwrap()[0].usd, None);
}

#[test]
fn the_object_wrapper_the_old_sdk_expected_is_an_error_not_an_empty_list() {
    let server = Server::new().json(Method::Get, "/vault/v1/assets", 200, json!({ "assets": [] }));
    let err = read_wallet(server).assets().unwrap_err();
    assert!(matches!(err, Error::Paymos { .. }), "{err:?}");
    assert!(err.message().contains("expected an array"), "{}", err.message());
}

#[test]
fn movements_page_parses_every_field_and_follows_next_cursor() {
    let server = Server::new()
        .fixture("movements_page")
        .json(Method::Get, "/vault/v1/movements?limit=2&cursor=cursor_1", 200, json!({ "items": [] }));
    let seen = server.requests();
    let wallet = read_wallet(server);

    let (items, next) = wallet.movements(2, None).unwrap();
    assert_eq!(next.as_deref(), Some("cursor_1"));
    assert_eq!(items.len(), 2);

    let a = &items[0];
    assert_eq!((a.id.as_str(), a.r#type.as_str(), a.status.as_str()), ("mv_3", "swap", "pending"));
    assert_eq!((a.send.amount.as_str(), a.send.asset.as_str()), ("5000000", "USDC@base"));
    assert_eq!(
        (a.receive.amount.as_str(), a.receive.min.as_str(), a.receive.asset.as_str()),
        ("5000000", "5000000", "USDT@base")
    );
    assert_eq!(a.created_at, "2026-01-01T00:00:00.0000000+00:00");
    // Omitted by the server when empty — a summary row carries no fee legs.
    assert!(a.fees.is_none());
    assert!(a.dest_chain_tx_hash.is_none());
    assert!(a.dest_chain_explorer_url.is_none());
    assert!(a.completed_at.is_none());

    let b = &items[1];
    assert_eq!((b.id.as_str(), b.r#type.as_str()), ("mv_2", "withdrawal"));
    assert_eq!(b.send.amount, "10000000");
    assert_eq!((b.receive.amount.as_str(), b.receive.min.as_str()), ("9975062", "9975062"));

    let (items, next) = wallet.movements(2, Some("cursor_1")).unwrap();
    assert!(items.is_empty());
    assert_eq!(next, None, "an omitted next_cursor is the end of history");

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].url, format!("{BASE}/vault/v1/movements?limit=2"));
    assert_eq!(seen[1].url, format!("{BASE}/vault/v1/movements?limit=2&cursor=cursor_1"));
}

#[test]
fn movement_parses_the_full_fee_breakdown() {
    let mv = read_wallet(Server::new().fixture("movement")).movement("mv_3").unwrap();
    assert_eq!((mv.id.as_str(), mv.r#type.as_str(), mv.status.as_str()), ("mv_3", "swap", "pending"));
    assert_eq!((mv.send.amount.as_str(), mv.send.asset.as_str()), ("5000000", "USDC@base"));
    assert_eq!(
        (mv.receive.amount.as_str(), mv.receive.min.as_str(), mv.receive.asset.as_str()),
        ("5000000", "5000000", "USDT@base")
    );
    assert_eq!(mv.created_at, "2026-01-01T00:00:00.0000000+00:00");
    assert!(mv.completed_at.is_none());
    assert!(mv.dest_chain_tx_hash.is_none());

    let fees = mv.fees.expect("the detail endpoint carries fees");
    assert_eq!(fees.asset, "USDC@base");
    assert_eq!((fees.platform.as_str(), fees.network.as_str(), fees.total.as_str()), ("0", "0", "0"));
    let route = fees.route.expect("route leg");
    assert_eq!(route.amount, "0");
    assert!(route.estimate);
    let usd = fees.usd.expect("usd leg");
    for key in ["platform", "network", "route", "total"] {
        assert_eq!(usd.get(key), Some(&json!("0")), "usd.{key}");
    }
}

// ── quotes ──────────────────────────────────────────────────────────────────

fn assert_withdraw_quote(q: &Quote, name: &str) {
    let r = &fixture(name)["response"]["body"];
    assert_eq!(q.movement_id.as_deref(), r["movement_id"].as_str());
    assert_eq!(q.mode, r["mode"].as_str().unwrap());
    assert_eq!(q.send.amount, r["send"]["amount"].as_str().unwrap());
    assert_eq!(q.send.asset, r["send"]["asset"].as_str().unwrap());
    assert_eq!(q.debit.amount, r["debit"]["amount"].as_str().unwrap());
    assert_eq!(q.debit.asset, r["debit"]["asset"].as_str().unwrap());
    assert_eq!(q.receive.amount, r["receive"]["amount"].as_str().unwrap());
    assert_eq!(q.receive.min, r["receive"]["min"].as_str().unwrap());
    assert_eq!(q.receive.asset, r["receive"]["asset"].as_str().unwrap());
    assert_eq!(q.expires_at, r["expires_at"].as_str().unwrap());
    assert_eq!(q.sources, r["sources"].as_u64().map(|n| n as u32));
    let fees = q.fees.as_ref().expect("fees");
    assert_eq!(fees.asset, r["fees"]["asset"].as_str().unwrap());
    assert_eq!(fees.platform, r["fees"]["platform"].as_str().unwrap());
    assert_eq!(fees.network, r["fees"]["network"].as_str().unwrap());
    assert_eq!(fees.total, r["fees"]["total"].as_str().unwrap());
    let route = fees.route.as_ref().expect("route");
    assert_eq!(route.amount, r["fees"]["route"]["amount"].as_str().unwrap());
    assert_eq!(route.estimate, r["fees"]["route"]["estimate"].as_bool().unwrap());
    assert_eq!(Value::Object(fees.usd.clone().expect("usd")), r["fees"]["usd"]);
}

#[test]
fn quote_withdraw_dry_sends_the_recorded_request_and_parses_every_field() {
    let server = Server::new().fixture("assets").fixture("quote_withdraw_dry");
    let seen = server.requests();
    let q = read_wallet(server).quote_withdraw("USDC@base", "10", TO, "exact_out").unwrap();

    assert_withdraw_quote(&q, "quote_withdraw_dry");
    assert_eq!(q.movement_id, None, "a dry preview creates nothing");
    assert_eq!(q.sources, None, "omitted on a dry preview");
    assert_eq!(q.debit.amount, "10025000");

    let seen = seen.lock().unwrap();
    let req = find(&seen, Method::Post, "/vault/v1/quote/withdraw");
    assert_eq!(body_of(req), fixture("quote_withdraw_dry")["request"]["body"]);
    assert_eq!(header(req, "idempotency-key"), None, "a preview carries no idempotency key");
}

#[test]
fn quote_swap_dry_sends_the_recorded_request_and_parses_every_field() {
    let server = Server::new().fixture("assets").fixture("quote_swap_dry");
    let seen = server.requests();
    let q = read_wallet(server).quote_swap("USDC@base", "USDT@base", "5", 50).unwrap();

    assert_withdraw_quote(&q, "quote_swap_dry");
    assert_eq!(q.mode, "exact_in");
    assert_eq!(q.receive.asset, "USDT@base");
    assert!(q.fees.as_ref().unwrap().route.as_ref().unwrap().estimate);

    let seen = seen.lock().unwrap();
    let req = find(&seen, Method::Post, "/vault/v1/quote/swap");
    let sent = body_of(req);
    assert_eq!(sent, fixture("quote_swap_dry")["request"]["body"]);
    assert!(sent["slippage_bps"].is_u64(), "slippage_bps is an integer");
    assert!(sent["amount"].is_string(), "amount is a decimal string");
}

#[test]
fn money_quotes_parse_every_field_including_movement_id_and_sources() {
    for name in ["quote_withdraw", "quote_withdraw_total_in", "quote_swap"] {
        let q: Quote = serde_json::from_value(response_of(name).1).unwrap();
        assert_withdraw_quote(&q, name);
        assert!(q.movement_id.is_some(), "{name}");
        assert!(q.sources.unwrap() >= 1, "{name}");
    }
}

// ── the co-sign flow, with a real 2-of-2 key ────────────────────────────────

fn mpc(req: Value) -> Value {
    serde_json::from_str(&wallet_mpc::mpc_call_json(&req.to_string())).unwrap()
}

fn package_for(packages: &Value, id: u64) -> Value {
    packages.as_array().unwrap().iter().find(|x| x[0].as_u64() == Some(id)).unwrap()[1].clone()
}

/// A real DKG between the SDK's half (id 1) and a server half (id 2), so the flow below signs for
/// real: the placeholders in sign_begin.json cannot get past the blind-sign guard, by design.
struct Keys {
    client: Value,
    server: Value,
}

impl Keys {
    fn new() -> Self {
        let a1 = mpc(json!({ "op": "dkg_part1", "id": 1, "max": 2, "min": 2 }));
        let b1 = mpc(json!({ "op": "dkg_part1", "id": 2, "max": 2, "min": 2 }));
        let a2 = mpc(json!({ "op": "dkg_part2", "secret": a1["secret"], "round1_packages": [[2, b1["package"]]] }));
        let b2 = mpc(json!({ "op": "dkg_part2", "secret": b1["secret"], "round1_packages": [[1, a1["package"]]] }));
        let a3 = mpc(json!({
            "op": "dkg_part3", "secret": a2["secret"],
            "round1_packages": [[2, b1["package"]]], "round2_packages": [[2, package_for(&b2["packages"], 1)]],
        }));
        let b3 = mpc(json!({
            "op": "dkg_part3", "secret": b2["secret"],
            "round1_packages": [[1, a1["package"]]], "round2_packages": [[1, package_for(&a2["packages"], 2)]],
        }));
        Keys { client: a3["key_package"].clone(), server: b3["key_package"].clone() }
    }

    fn secret(&self) -> String {
        VaultSecret::pack("vk_live_contract", 7, Some(&self.client.to_string()))
    }

    fn vault(&self) -> String {
        self.client["verifying_key"].as_str().expect("key package names its group key").to_string()
    }
}

/// The raw token id whose SHA-256 is USDC@base's `fingerprint` in assets.json — the send asset of
/// every money fixture, so an honest disclosure moves exactly this token.
const USDC_TOKEN: &str = "usdc-base";

/// What a server's sign/begin would disclose for one source moving `amount` of USDC from `signer`.
fn disclosure(signer: &str, amount: &str) -> Value {
    disclosure_with(signer, USDC_TOKEN, json!(amount))
}

fn disclosure_with(signer: &str, token: &str, amount: Value) -> Value {
    let message = json!({
        "signer_id": signer,
        "deadline": "2026-01-01T00:00:00.000Z",
        "intents": [{ "intent": "transfer", "receiver_id": "recipient.example", "tokens": { token: amount } }],
    })
    .to_string();
    json!({ "message": message, "nonce": "11".repeat(32), "recipient": "recipient.example" })
}

/// A sign/begin responder that plays the server half honestly: it commits, builds each signing
/// package over the disclosed message's digest, and answers with sign_begin.json's shape.
fn honest_begin(server_kp: Value, disclosures: Vec<Value>) -> impl Fn(&HttpRequest) -> (u16, String) {
    move |req| {
        let commitments = body_of(req)["commitments"].as_array().unwrap().clone();
        assert_eq!(commitments.len(), disclosures.len(), "one commitment per source");
        let mut packages = vec![];
        for (client_c, d) in commitments.iter().zip(&disclosures) {
            let server_c = mpc(json!({ "op": "commit", "key_package": server_kp }));
            let nonce = hex::decode(d["nonce"].as_str().unwrap()).unwrap();
            let digest = payload_digest_hex(d["message"].as_str().unwrap(), &nonce, d["recipient"].as_str().unwrap(), None).unwrap();
            let pkg = mpc(json!({
                "op": "build_signing_package",
                "commitments": [[1, client_c], [2, server_c["commitments"]]],
                "message_hex": digest,
            }));
            packages.push(pkg["signing_package"].clone());
        }
        (200, json!({ "token": "sign-session-token", "signing_packages": packages, "messages": disclosures }).to_string())
    }
}

/// Assets, the money quote, and the movement the flow reads at the end.
fn swap_server() -> Server {
    Server::new().fixture("assets").fixture("quote_swap").fixture("movement")
}

#[test]
fn swap_sends_the_recorded_quote_request_co_signs_and_reads_the_movement() {
    let keys = Keys::new();
    let server = swap_server()
        .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![disclosure(&keys.vault(), "5000000")]))
        .fixture("sign_aggregate");
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();

    let mv: Movement = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap();
    assert_eq!(mv.id, "mv_3");
    assert!(mv.fees.is_some());

    let seen = seen.lock().unwrap();
    let quote = find(&seen, Method::Post, "/vault/v1/quote/swap");
    assert_request_matches(&body_of(quote), "quote_swap");
    assert!(header(quote, "idempotency-key").is_some_and(|k| !k.is_empty()), "a money quote carries an idempotency key");

    let aggregate = body_of(find(&seen, Method::Post, "/vault/v1/movements/mv_3/sign/aggregate"));
    assert_eq!(aggregate["token"], "sign-session-token");
    assert_eq!(aggregate["shares"].as_array().unwrap().len(), 1);
}

#[test]
fn sign_aggregate_ok_false_is_a_typed_error_with_the_servers_message() {
    let keys = Keys::new();
    let (status, body) = response_of("sign_aggregate_error");
    assert_eq!(body["ok"], false);
    for (status, body) in [(status, body.clone()), (200, body)] {
        let server = swap_server()
            .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![disclosure(&keys.vault(), "5000000")]))
            .json(Method::Post, "/vault/v1/movements/mv_3/sign/aggregate", status, body);
        let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
        let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
        assert!(matches!(err, Error::Paymos { .. }), "{err:?}");
        assert_eq!(err.message(), "token and shares are required");
    }
}

#[test]
fn sign_begin_fixture_parses_and_its_placeholders_are_refused_not_signed() {
    let keys = Keys::new();
    let server = swap_server().fixture("sign_begin").fixture("sign_aggregate");
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();

    let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Paymos { .. }), "{err:?}");
    assert!(err.message().starts_with("co-sign refused"), "{}", err.message());

    let seen = seen.lock().unwrap();
    let begin = body_of(find(&seen, Method::Post, "/vault/v1/movements/mv_3/sign/begin"));
    assert_eq!(begin["commitments"].as_array().unwrap().len(), 1, "quote_swap has sources: 1");
    assert!(
        !seen.iter().any(|r| r.url.ends_with("/sign/aggregate")),
        "nothing may reach aggregate once the guard refuses"
    );
}

#[test]
fn the_blind_sign_guard_refuses_every_lie_before_a_share_exists() {
    let keys = Keys::new();
    let other = "ab".repeat(32);
    let honest = disclosure(&keys.vault(), "5000000");
    let mut swapped_message = honest.clone();
    swapped_message["message"] = disclosure(&keys.vault(), "4000000")["message"].clone();

    // (what the server claims in `messages`, what it actually builds packages over, expected refusal)
    let cases: Vec<(Value, Value, &str)> = vec![
        (swapped_message, honest.clone(), "does not bind"),
        (disclosure(&keys.vault(), "5000001"), disclosure(&keys.vault(), "5000001"), "above the approved debit"),
        (disclosure(&other, "5000000"), disclosure(&other, "5000000"), "different vault"),
    ];
    for (claimed, built, refusal) in cases {
        let begin = honest_begin(keys.server.clone(), vec![built]);
        let claimed_for_route = claimed.clone();
        let server = swap_server()
            .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", move |req| {
                let (status, body) = begin(req);
                let mut body: Value = serde_json::from_str(&body).unwrap();
                body["messages"] = json!([claimed_for_route.clone()]);
                (status, body.to_string())
            })
            .fixture("sign_aggregate");
        let seen = server.requests();
        let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
        let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
        assert!(err.message().contains(refusal), "expected {refusal:?}, got {:?}", err.message());
        assert!(!seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate")), "{refusal}");
    }

    // No disclosure at all, and a package count that does not match the sources.
    for (body, refusal) in [
        (json!({ "token": "t", "signing_packages": [{ "message": "00" }] }), "refusing to blind-sign"),
        (json!({ "token": "t", "signing_packages": [], "messages": [] }), "expected 1 signing packages"),
    ] {
        let server = swap_server().json(Method::Post, "/vault/v1/movements/mv_3/sign/begin", 200, body);
        let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
        let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
        assert!(err.message().contains(refusal), "expected {refusal:?}, got {:?}", err.message());
    }
}

/// withdraw drives the recorded money quote; sign/begin answers sign_begin_error.json and the
/// movement is still pending, so the 400 surfaces — after the request shape has been checked.
fn withdraw_until_begin(name: &str, mode: &str, movement_id: &str) -> (Error, Vec<HttpRequest>) {
    let keys = Keys::new();
    let (status, begin_error) = response_of("sign_begin_error");
    let server = Server::new()
        .fixture("assets")
        .fixture(name)
        .json(Method::Post, &format!("/vault/v1/movements/{movement_id}/sign/begin"), status, begin_error)
        .json(Method::Get, &format!("/vault/v1/movements/{movement_id}"), 200, {
            let mut mv = response_of("movement").1;
            mv["id"] = json!(movement_id);
            mv
        });
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
    let options = WithdrawOptions { mode: Some(mode.to_string()), ..Default::default() };
    let err = wallet.withdraw("USDC@base", "10", TO, options).unwrap_err();
    let seen = seen.lock().unwrap().clone();
    (err, seen)
}

#[test]
fn withdraw_exact_out_sends_the_recorded_request_and_commits_once_per_source() {
    let (err, seen) = withdraw_until_begin("quote_withdraw", "exact_out", "mv_1");
    assert_eq!(err.message(), "commitments are required");
    assert_eq!(err.status(), Some(400));

    let quote = find(&seen, Method::Post, "/vault/v1/quote/withdraw");
    assert_request_matches(&body_of(quote), "quote_withdraw");
    assert!(header(quote, "idempotency-key").is_some());
    let begin = body_of(find(&seen, Method::Post, "/vault/v1/movements/mv_1/sign/begin"));
    assert_eq!(begin["commitments"].as_array().unwrap().len(), 2, "quote_withdraw has sources: 2");
}

#[test]
fn withdraw_total_in_sends_the_recorded_request_and_passes_the_echo_check() {
    let (err, seen) = withdraw_until_begin("quote_withdraw_total_in", "total_in", "mv_2");
    // Reaching sign/begin at all means the total_in echo (debit ≤ approved total) accepted it.
    assert_eq!(err.message(), "commitments are required");
    assert_request_matches(&body_of(find(&seen, Method::Post, "/vault/v1/quote/withdraw")), "quote_withdraw_total_in");
    let begin = body_of(find(&seen, Method::Post, "/vault/v1/movements/mv_2/sign/begin"));
    assert_eq!(begin["commitments"].as_array().unwrap().len(), 2);
}

// ── errors ──────────────────────────────────────────────────────────────────

#[test]
fn every_error_fixture_maps_to_its_typed_error_and_message() {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().trim_end_matches(".json").to_string())
        .filter(|n| n.starts_with("error_") || n.ends_with("_error"))
        .collect();
    names.sort();
    assert!(names.len() >= 9, "error fixtures went missing: {names:?}");

    for name in names {
        let f = fixture(&name);
        let status = f["response"]["status"].as_u64().unwrap() as u16;
        let message = f["response"]["body"]["error"].as_str().unwrap().to_string();
        let path = f["request"]["path"].as_str().unwrap();
        let (status_c, body) = response_of(&name);
        let body_text = body.to_string();
        let server = Server::new().route(
            if f["request"]["method"] == "GET" { Method::Get } else { Method::Post },
            path,
            move |_| (status_c, body_text.clone()),
        );
        let http = Http::with_transport(BASE, "vk_live_contract", Box::new(server));
        let err = if f["request"]["method"] == "GET" {
            http.get(path, &[]).unwrap_err()
        } else {
            let req_body = &f["request"]["body"];
            let req_body = if req_body.is_string() { json!({}) } else { req_body.clone() };
            http.post(path, &req_body, &[]).unwrap_err()
        };

        let kind = match (&err, status) {
            (Error::Auth { .. }, 401) => "ok",
            (Error::Forbidden { .. }, 403) => "ok",
            (Error::Conflict { .. }, 409) => "ok",
            (Error::Paymos { .. }, 400 | 404) => "ok",
            _ => "wrong",
        };
        assert_eq!(kind, "ok", "{name}: status {status} mapped to {err:?}");
        assert_eq!(err.message(), message, "{name}");
        assert_eq!(err.status(), Some(status), "{name}");
    }
}

#[test]
fn errors_surface_through_the_wallet_methods_too() {
    let (s, b) = response_of("error_401");
    let err = read_wallet(Server::new().json(Method::Get, "/vault/v1/balances", s, b)).balances().unwrap_err();
    assert!(matches!(err, Error::Auth { status: Some(401), .. }), "{err:?}");

    let err = read_wallet(Server::new().fixture("error_404_movement")).movement("does-not-exist").unwrap_err();
    assert!(matches!(err, Error::Paymos { status: Some(404), .. }), "{err:?}");
    assert_eq!(err.message(), "movement not found");

    let err = read_wallet(Server::new().fixture("assets").fixture("error_403"))
        .quote_withdraw("USDC@base", "10", TO, "exact_out");
    // error_403 was recorded without `dry`; the dry preview is still routed to the same path.
    let err = err.unwrap_err();
    assert!(matches!(err, Error::Forbidden { status: Some(403), .. }), "{err:?}");
}

#[test]
fn an_html_404_from_a_proxy_is_a_typed_error_that_says_what_happened() {
    let page = "<!DOCTYPE html><html><head><title>404</title></head><body><h1>Page not found</h1></body></html>";
    let server = Server::new().route(Method::Get, "/vault/v1/balances", move |_| (404, page.to_string()));
    let err = read_wallet(server).balances().unwrap_err();
    assert!(matches!(err, Error::Paymos { status: Some(404), .. }), "{err:?}");
    assert!(err.message().contains("HTML page instead of the vault API"), "{}", err.message());
}

#[test]
fn an_html_200_from_a_site_fallback_is_a_typed_error_not_a_parse_panic() {
    let page = "<!doctype html><html><body>Private Wallet</body></html>";
    let server = Server::new().route(Method::Get, "/vault/v1/assets", move |_| (200, page.to_string()));
    let err = read_wallet(server).assets().unwrap_err();
    assert!(matches!(err, Error::Paymos { status: Some(200), .. }), "{err:?}");
    assert!(err.message().contains("non-JSON"), "{}", err.message());
}

// ── status vocabulary ───────────────────────────────────────────────────────

fn vocabulary() -> BTreeSet<String> {
    let v = fixture("vocabulary");
    v["movement_statuses"]
        .as_array()
        .unwrap()
        .iter()
        .chain(v["sign_statuses"].as_array().unwrap())
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn python_status_sets_use_only_words_the_server_sends() {
    let vocab = vocabulary();
    for s in PY_TERMINAL.iter().chain(PY_PROGRESSED.iter()) {
        assert!(vocab.contains(*s), "{s:?} is not in vocabulary.json");
    }
}

fn movement_with_status(status: &str) -> Value {
    let mut mv = response_of("movement").1;
    mv["status"] = json!(status);
    mv
}

#[test]
fn every_status_is_terminal_or_not_exactly_as_in_python() {
    for status in vocabulary() {
        let server = Server::new().json(Method::Get, "/vault/v1/movements/mv_3", 200, movement_with_status(&status));
        let result = read_wallet(server).wait("mv_3", Duration::ZERO, Duration::ZERO);
        let terminal = PY_TERMINAL.contains(&status.as_str());
        match result {
            Ok(_) => assert!(terminal, "wait() stopped on {status:?}, which Python keeps polling"),
            Err(e) => {
                assert!(!terminal, "wait() kept polling {status:?}, which Python treats as terminal: {e:?}");
                assert!(e.message().contains("timed out"), "{e:?}");
            }
        }
    }
}

#[test]
fn every_status_converges_or_raises_on_a_rejected_sign_begin_exactly_as_in_python() {
    let keys = Keys::new();
    for status in vocabulary() {
        let server = Server::new()
            .fixture("assets")
            .fixture("quote_swap")
            .json(Method::Post, "/vault/v1/movements/mv_3/sign/begin", 409, json!({ "error": "movement is not awaiting signature" }))
            .json(Method::Get, "/vault/v1/movements/mv_3", 200, movement_with_status(&status));
        let seen = server.requests();
        let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
        let result = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default());
        let progressed = PY_PROGRESSED.contains(&status.as_str());
        match result {
            Ok(mv) => {
                assert!(progressed, "a {status:?} movement was treated as already signed");
                assert_eq!(mv.status, status);
            }
            Err(e) => {
                assert!(!progressed, "a {status:?} movement should converge, got {e:?}");
                assert!(matches!(e, Error::Conflict { .. }), "the original sign/begin error surfaces: {e:?}");
            }
        }
        assert!(!seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate")), "{status}");
    }
}

// ── signed and malformed per-source amounts ─────────────────────────────────

#[test]
fn a_per_source_amount_must_be_plain_digits_and_positive() {
    let keys = Keys::new();
    let amounts = ["-900", "-1", "+5", "", "0", "00", " 5", "5.0", "0x10"]
        .into_iter()
        .map(|a| json!(a))
        .chain([json!(5000000), json!(null)]);
    for amount in amounts {
        let d = disclosure_with(&keys.vault(), USDC_TOKEN, amount.clone());
        let server = swap_server()
            .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![d]))
            .fixture("sign_aggregate");
        let seen = server.requests();
        let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
        let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
        assert!(
            err.message().starts_with("co-sign refused") && err.message().contains("bad transfer amount"),
            "{amount:?}: {}",
            err.message()
        );
        assert!(!seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate")), "{amount:?}");
    }
}

#[test]
fn two_sources_of_2d_and_minus_d_are_refused_though_they_sum_to_the_debit() {
    // quote_withdraw: sources 2, debit 10025000. Disclose 2D and -D: the sum is D, which a signed
    // parser would wave through while the first source alone moved twice the approved debit.
    let keys = Keys::new();
    let d: u128 = 10_025_000;
    let disclosures = vec![disclosure(&keys.vault(), &(2 * d).to_string()), disclosure(&keys.vault(), &format!("-{d}"))];
    let server = Server::new()
        .fixture("assets")
        .fixture("quote_withdraw")
        .route(Method::Post, "/vault/v1/movements/mv_1/sign/begin", honest_begin(keys.server.clone(), disclosures))
        .json(Method::Post, "/vault/v1/movements/mv_1/sign/aggregate", 200, json!({ "ok": true, "status": "processing" }));
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
    let err = wallet.withdraw("USDC@base", "10", TO, WithdrawOptions::default()).unwrap_err();
    assert_eq!(err.message(), "co-sign refused: bad transfer amount for source 1");
    assert!(!seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate")));
}

// ── the token binding ───────────────────────────────────────────────────────

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[test]
fn a_source_moving_another_assets_token_is_refused() {
    // USDT@base's own token: a real catalog asset, a valid transfer, the right amount — and not
    // the USDC the debit was approved in.
    let keys = Keys::new();
    let usdt = "usdt-base";
    let (_, assets) = response_of("assets");
    assert_eq!(assets[1]["fingerprint"].as_str(), Some(sha256_hex(usdt).as_str()));

    let d = disclosure_with(&keys.vault(), usdt, json!("5000000"));
    let server = swap_server()
        .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![d]))
        .fixture("sign_aggregate");
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
    let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
    assert_eq!(err.message(), "co-sign refused: source 0 moves a token other than the approved USDC@base");
    assert!(!seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate")));
}

#[test]
fn a_catalog_without_fingerprints_still_reads_but_never_co_signs() {
    let keys = Keys::new();
    let (_, mut assets) = response_of("assets");
    for a in assets.as_array_mut().unwrap() {
        a.as_object_mut().unwrap().remove("fingerprint");
    }
    let server = Server::new()
        .json(Method::Get, "/vault/v1/assets", 200, assets)
        .fixture("quote_swap")
        .fixture("movement")
        .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![disclosure(&keys.vault(), "5000000")]))
        .fixture("sign_aggregate");
    let seen = server.requests();
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();

    // An older server: reads keep working, the field is simply absent.
    assert!(wallet.assets().unwrap().iter().all(|a| a.fingerprint.is_none()));

    let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
    assert_eq!(
        err.message(),
        "co-sign refused: the server did not publish an asset fingerprint — update the server"
    );
    assert!(
        !seen.lock().unwrap().iter().any(|r| r.url.contains("/sign/")),
        "refused before a commitment left the process"
    );
}

#[test]
fn a_key_package_without_a_verifying_key_refuses_instead_of_skipping_the_signer_check() {
    let keys = Keys::new();
    let mut kp = keys.client.clone();
    kp.as_object_mut().unwrap().remove("verifying_key");
    let secret = VaultSecret::pack("vk_live_contract", 7, Some(&kp.to_string()));
    let server = swap_server()
        .route(Method::Post, "/vault/v1/movements/mv_3/sign/begin", honest_begin(keys.server.clone(), vec![disclosure(&keys.vault(), "5000000")]))
        .fixture("sign_aggregate");
    let seen = server.requests();
    let wallet = Wallet::with_transport(&secret, BASE, Box::new(server)).unwrap();
    let err = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default()).unwrap_err();
    assert_eq!(err.message(), "co-sign refused: the key package carries no verifying key");
    assert!(!seen.lock().unwrap().iter().any(|r| r.url.contains("/sign/")));
}

// ── one label, several fingerprints; pinning ────────────────────────────────

/// assets.json with a second USDC@base entry whose fingerprint is another contract of the same
/// coin — a label the server may legitimately list twice.
fn catalog_with_two_usdc_contracts(second_token: &str) -> Value {
    let (_, mut assets) = response_of("assets");
    let mut twin = assets[0].clone();
    twin["fingerprint"] = json!(sha256_hex(second_token));
    assets.as_array_mut().unwrap().insert(1, twin);
    assets
}

fn swap_with(keys: &Keys, catalog: Value, token: &str, pins: Option<Vec<(&str, Vec<String>)>>) -> (Result<Movement, Error>, bool) {
    let server = Server::new()
        .json(Method::Get, "/vault/v1/assets", 200, catalog)
        .fixture("quote_swap")
        .fixture("movement")
        .route(
            Method::Post,
            "/vault/v1/movements/mv_3/sign/begin",
            honest_begin(keys.server.clone(), vec![disclosure_with(&keys.vault(), token, json!("5000000"))]),
        )
        .fixture("sign_aggregate");
    let seen = server.requests();
    let mut wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
    if let Some(pins) = pins {
        wallet = wallet.with_pinned_fingerprints(pins);
    }
    let result = wallet.swap("USDC@base", "USDT@base", "5", SwapOptions::default());
    let aggregated = seen.lock().unwrap().iter().any(|r| r.url.ends_with("/sign/aggregate"));
    (result, aggregated)
}

#[test]
fn a_label_listed_twice_accepts_a_token_matching_either_fingerprint() {
    let keys = Keys::new();
    for token in [USDC_TOKEN, "usdc-base-bridged"] {
        let (result, aggregated) = swap_with(&keys, catalog_with_two_usdc_contracts("usdc-base-bridged"), token, None);
        assert_eq!(result.unwrap().id, "mv_3", "{token}");
        assert!(aggregated, "{token} signed past the guard");
    }
    let (result, aggregated) = swap_with(&keys, catalog_with_two_usdc_contracts("usdc-base-bridged"), "usdt-base", None);
    assert_eq!(
        result.unwrap_err().message(),
        "co-sign refused: source 0 moves a token other than the approved USDC@base"
    );
    assert!(!aggregated);
}

#[test]
fn a_pinned_fingerprint_is_the_only_one_that_counts() {
    let keys = Keys::new();
    // Pinned value matches the token (upper-cased: compared lowercase) → signs.
    let pin = sha256_hex(USDC_TOKEN).to_uppercase();
    let (result, aggregated) =
        swap_with(&keys, response_of("assets").1, USDC_TOKEN, Some(vec![("USDC@base", vec![pin])]));
    assert_eq!(result.unwrap().id, "mv_3");
    assert!(aggregated);

    // The server publishes a fingerprint that matches the token, but the pin says otherwise:
    // the published one is ignored and the co-sign is refused.
    let (result, aggregated) = swap_with(
        &keys,
        response_of("assets").1,
        USDC_TOKEN,
        Some(vec![("USDC@base", vec![sha256_hex("usdc-base-pinned")])]),
    );
    assert_eq!(
        result.unwrap_err().message(),
        "co-sign refused: source 0 moves a token other than the approved USDC@base"
    );
    assert!(!aggregated);

    // A pin on another label leaves USDC@base on the published set.
    let (result, _) =
        swap_with(&keys, response_of("assets").1, USDC_TOKEN, Some(vec![("USDT@base", vec![sha256_hex("elsewhere")])]));
    assert_eq!(result.unwrap().id, "mv_3");
}

#[test]
fn a_pin_rescues_a_catalog_that_publishes_no_fingerprint() {
    let keys = Keys::new();
    let (_, mut assets) = response_of("assets");
    for a in assets.as_array_mut().unwrap() {
        a.as_object_mut().unwrap().remove("fingerprint");
    }
    let (result, aggregated) =
        swap_with(&keys, assets, USDC_TOKEN, Some(vec![("USDC@base", vec![sha256_hex(USDC_TOKEN)])]));
    assert_eq!(result.unwrap().id, "mv_3");
    assert!(aggregated);
}

#[test]
fn pins_take_a_single_string_or_several() {
    // Compiles with each shape the option accepts; the behaviour is covered above.
    let secret = read_secret();
    let fp = sha256_hex(USDC_TOKEN);
    let _ = Wallet::with_transport(&secret, BASE, Box::new(Server::new())).unwrap()
        .with_pinned_fingerprints([("USDC@base", fp.as_str())]);
    let _ = Wallet::with_transport(&secret, BASE, Box::new(Server::new())).unwrap()
        .with_pinned_fingerprints([("USDC@base", fp.clone())]);
    let _ = Wallet::with_transport(&secret, BASE, Box::new(Server::new())).unwrap()
        .with_pinned_fingerprints(vec![("USDC@base".to_string(), vec![fp.clone(), sha256_hex("x")])]);
}

// ── a repeated label resolves to its first entry ────────────────────────────

/// USDC@base listed twice: 6 decimals first (as in assets.json), then 18. The server resolves the
/// label to the first entry, and so must every per-label lookup except the fingerprint set.
fn catalog_with_usdc_twice_18_decimals_last() -> Value {
    let (_, mut assets) = response_of("assets");
    let mut twin = assets[0].clone();
    twin["decimals"] = json!(18);
    assets.as_array_mut().unwrap().insert(1, twin);
    assets
}

#[test]
fn a_repeated_label_converts_amounts_with_its_first_entrys_decimals() {
    // 7 fractional digits: too many for the first entry's 6, fine for the last one's 18.
    let server = Server::new()
        .json(Method::Get, "/vault/v1/assets", 200, catalog_with_usdc_twice_18_decimals_last())
        .fixture("quote_withdraw_dry");
    let seen = server.requests();
    let err = read_wallet(server).quote_withdraw("USDC@base", "10.0000001", TO, "exact_out").unwrap_err();
    assert!(err.message().contains("more than 6 fractional digits"), "{}", err.message());
    assert!(!seen.lock().unwrap().iter().any(|r| r.url.contains("/quote/")), "refused locally");
}

#[test]
fn a_repeated_label_runs_the_echo_check_with_its_first_entrys_decimals() {
    // quote_withdraw.json receives 10000000 raw = 10 at 6 decimals. With the last entry's 18 the
    // approved amount would be 10^19 raw and the echo check would refuse a correct quote.
    let keys = Keys::new();
    let (status, begin_error) = response_of("sign_begin_error");
    let server = Server::new()
        .json(Method::Get, "/vault/v1/assets", 200, catalog_with_usdc_twice_18_decimals_last())
        .fixture("quote_withdraw")
        .json(Method::Post, "/vault/v1/movements/mv_1/sign/begin", status, begin_error)
        .json(Method::Get, "/vault/v1/movements/mv_1", 200, movement_with_status("pending"));
    let wallet = Wallet::with_transport(&keys.secret(), BASE, Box::new(server)).unwrap();
    let err = wallet.withdraw("USDC@base", "10", TO, WithdrawOptions::default()).unwrap_err();
    // Reaching sign/begin means the echo accepted the quote at 6 decimals.
    assert_eq!(err.message(), "commitments are required");
}

#[test]
fn a_same_label_entry_with_other_decimals_is_not_in_the_fingerprint_set() {
    // USDC@base twice: the first at 6 decimals, a second contract at 18. The debit is in 6-decimal
    // units, so the 18-decimal token could move 10^12 times the value within the same raw figure.
    let keys = Keys::new();
    let mut catalog = catalog_with_two_usdc_contracts("usdc-base-18");
    catalog[1]["decimals"] = json!(18);

    let (result, aggregated) = swap_with(&keys, catalog.clone(), "usdc-base-18", None);
    assert_eq!(
        result.unwrap_err().message(),
        "co-sign refused: source 0 moves a token other than the approved USDC@base"
    );
    assert!(!aggregated, "refused before any share");

    // The first entry's own token still signs.
    let (result, aggregated) = swap_with(&keys, catalog, USDC_TOKEN, None);
    assert_eq!(result.unwrap().id, "mv_3");
    assert!(aggregated);
}
