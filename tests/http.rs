//! The error mapping, which is the whole reason this layer exists: a caller branches on a type
//! instead of matching a string, and the types must land the same way they do in Python.

use std::sync::{Arc, Mutex};

use paymos_wallet::{Error, Http, HttpRequest, HttpResponse, Transport};

struct Stub {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

impl Stub {
    fn json(status: u16, body: &serde_json::Value) -> Self {
        Stub { status, body: body.to_string(), headers: vec![] }
    }

    fn text(status: u16, body: &str) -> Self {
        Stub { status, body: body.to_string(), headers: vec![] }
    }

    fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        self.headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self
    }
}

impl Transport for Stub {
    fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: self.status,
            headers: self.headers.clone(),
            body: self.body.clone(),
        })
    }
}

struct Failing;

impl Transport for Failing {
    fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, String> {
        Err("connection reset".to_string())
    }
}

/// Records the request it answered, so header assertions can read it after the transport moved
/// into the client.
struct Capture {
    seen: Mutex<Option<HttpRequest>>,
}

impl Transport for Capture {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        *self.seen.lock().unwrap() = Some(req.clone());
        Ok(HttpResponse { status: 200, headers: vec![], body: "{}".to_string() })
    }
}

fn http(stub: impl Transport + 'static) -> Http {
    Http::with_transport("https://example.test", "vk_live_x", Box::new(stub))
}

#[test]
fn returns_parsed_json_on_a_2xx() {
    // /assets answers a bare array — the object wrapper this test used to mock was never sent.
    let assets = serde_json::json!([{ "asset": "USDC@base", "symbol": "USDC", "chain": "base", "decimals": 6 }]);
    let body = http(Stub::json(200, &assets)).get("/x", &[]).unwrap();
    assert_eq!(body, assets);
}

#[test]
fn maps_status_codes_to_types() {
    let cases: [(u16, &str, &str); 5] = [
        (401, "bad key", "auth"),
        (403, "no scope", "forbidden"),
        (409, "reused", "conflict"),
        (404, "gone", "base"),
        (500, "boom", "base"),
    ];
    for (status, message, kind) in cases {
        let err = http(Stub::json(status, &serde_json::json!({ "error": message })))
            .get("/x", &[])
            .unwrap_err();
        let ok = matches!(
            (kind, &err),
            ("auth", Error::Auth { .. })
                | ("forbidden", Error::Forbidden { .. })
                | ("conflict", Error::Conflict { .. })
                | ("base", Error::Paymos { .. })
        );
        assert!(ok, "status {status} mapped to {err:?}");
        assert_eq!(err.status(), Some(status));
    }
}

#[test]
fn classifies_a_400_by_its_message_first_match_winning() {
    let cases: [(&str, &str); 8] = [
        ("cross-asset withdraw is not allowed", "cross"),
        ("slippage exceeded", "slippage"),
        ("quote expired", "expired"),
        ("insufficient balance", "funds"),
        ("available balance is lower", "funds"),
        ("No liquidity available, try again in a moment", "route"),
        ("route can't be quoted", "route"),
        ("something else entirely", "base"),
    ];
    for (message, kind) in cases {
        let err = http(Stub::json(400, &serde_json::json!({ "error": message })))
            .get("/x", &[])
            .unwrap_err();
        let ok = matches!(
            (kind, &err),
            ("cross", Error::CrossAssetWithdrawNotAllowed { .. })
                | ("slippage", Error::SlippageExceeded { .. })
                | ("expired", Error::QuoteExpired { .. })
                | ("funds", Error::InsufficientFunds { .. })
                | ("route", Error::RouteUnavailable { .. })
                | ("base", Error::Paymos { .. })
        );
        assert!(ok, "{message:?} mapped to {err:?}");
    }
}

#[test]
fn carries_retry_after_on_a_429_when_the_server_sends_one() {
    let err = http(
        Stub::json(429, &serde_json::json!({ "error": "slow down" }))
            .with_headers(&[("retry-after", "12")]),
    )
    .get("/x", &[])
    .unwrap_err();
    match err {
        Error::RateLimited { retry_after, .. } => assert_eq!(retry_after, Some(12)),
        other => panic!("expected RateLimited, got {other:?}"),
    }

    let err = http(Stub::json(429, &serde_json::json!({ "error": "slow down" })))
        .get("/x", &[])
        .unwrap_err();
    match err {
        Error::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[test]
fn falls_back_to_the_raw_body_when_there_is_no_error_envelope() {
    let err = http(Stub::text(500, "upstream exploded")).get("/x", &[]).unwrap_err();
    assert_eq!(err.message(), "upstream exploded");
}

#[test]
fn wraps_a_transport_failure_instead_of_leaking_it() {
    let err = http(Failing).get("/x", &[]).unwrap_err();
    assert!(matches!(err, Error::Paymos { .. }));
    assert_eq!(err.status(), None);
}

#[test]
fn sends_the_bearer_token_and_the_idempotency_key() {
    let capture = Arc::new(Capture { seen: Mutex::new(None) });
    let client = Http::with_transport(
        "https://example.test",
        "vk_live_secret",
        Box::new(ArcClone(capture.clone())),
    );
    client
        .post("/x", &serde_json::json!({ "a": 1 }), &[("idempotency-key", "abc")])
        .unwrap();

    let seen = capture.seen.lock().unwrap().clone().expect("a request was sent");
    let header = |name: &str| {
        seen.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer vk_live_secret"));
    assert_eq!(header("idempotency-key"), Some("abc"));
}

/// Forwards into the shared capture; the test keeps its own reference to read afterwards.
struct ArcClone(Arc<Capture>);

impl Transport for ArcClone {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        self.0.send(req)
    }
}
