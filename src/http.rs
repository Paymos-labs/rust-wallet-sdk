//! Authenticated transport, ported from the Python SDK so every language fails the same way.
//!
//! Attaches `Authorization: Bearer <api key>` to every request, returns parsed JSON on a 2xx, and
//! maps anything else onto a typed error through the server's envelope. The server's contract:
//! business errors are `{"error": "…"}` and the sign flow answers
//! `{"ok":false,"status":"failed","error":"…"}` — both carry the message under `error`. Those
//! messages arrive already client-safe, so this layer maps them and never rewrites them, and it
//! never logs or echoes a request body.
//!
//! Transport failures are wrapped into a typed error with no status, so a caller on the money
//! path never catches a raw client exception. The timeout is generous because the co-sign is a
//! multi-round exchange that outlasts anything a default would allow.

use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};

/// The sign/aggregate round can trigger a relay that takes a while.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One outgoing request, transport-agnostic so tests never touch the network.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// The swappable wire. The default is [`UreqTransport`]; tests hand in a stub.
pub trait Transport {
    /// `Err` means the request never got a response (DNS, reset, timeout) — not a bad status.
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String>;
}

/// Blocking transport over `ureq`. No async runtime is forced on the integrator.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // A 4xx/5xx is an answer, not a transport failure: the error mapping needs the
            // response body (the server's envelope) and headers (Retry-After) intact.
            .http_status_as_error(false)
            .build();
        UreqTransport { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Transport for UreqTransport {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        let headers: Vec<(String, String)> = req.headers.clone();
        let result = match (&req.method, &req.body) {
            (Method::Get, _) => {
                let mut b = self.agent.get(&req.url);
                for (name, value) in &headers {
                    b = b.header(name, value);
                }
                b.call()
            }
            (Method::Post, body) => {
                let mut b = self.agent.post(&req.url);
                for (name, value) in &headers {
                    b = b.header(name, value);
                }
                match body {
                    Some(body) => b.send(body.as_str()),
                    None => b.send(""),
                }
            }
        };
        match result {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (name.to_string(), value.to_str().unwrap_or("").to_string())
                    })
                    .collect();
                let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
                Ok(HttpResponse { status, headers, body })
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

pub struct Http {
    base_url: String,
    api_key: String,
    transport: Box<dyn Transport>,
}

/// How much of a body that is not the envelope makes it into a message.
const SNIPPET_CHARS: usize = 200;

fn message_of(body: &Value, status: u16, text: &str) -> String {
    if let Some(msg) = body
        .get("error")
        .or_else(|| body.get("detail"))
        .and_then(|v| v.as_str())
    {
        if !msg.is_empty() {
            return msg.to_string();
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return format!("request failed with status {status}");
    }
    // Every non-2xx under /vault/ carries a JSON envelope, so markup means something in front of
    // the API answered instead — a proxy's or the site's own 404 page. Echoing a whole HTML
    // document as the message told the caller nothing; naming what happened does.
    if trimmed.starts_with('<') {
        return format!(
            "HTTP {status} with an HTML page instead of the vault API's JSON — check the base URL \
             (PAYMOS_BASE_URL) and any proxy in front of it: {}",
            snippet(trimmed)
        );
    }
    snippet(trimmed)
}

fn snippet(text: &str) -> String {
    match text.char_indices().nth(SNIPPET_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// Map a 400's message to a type. First match wins, in this exact order.
fn classify400(message: &str, status: Option<u16>) -> Error {
    let m = message.to_lowercase();
    let message = message.to_string();
    if m.contains("cross-asset") {
        return Error::CrossAssetWithdrawNotAllowed { message, status };
    }
    if m.contains("slippage") {
        return Error::SlippageExceeded { message, status };
    }
    if m.contains("expired") {
        return Error::QuoteExpired { message, status };
    }
    for t in ["insufficient", "not enough balance", "exceeds the vault", "available balance"] {
        if m.contains(t) {
            return Error::InsufficientFunds { message, status };
        }
    }
    // The server's route and liquidity rejections come back sounding transient — "No liquidity
    // available … try again in a moment", "route can't be quoted". That whole family is one type.
    for t in ["route", "can't be quoted", "liquidity", "try again in a moment"] {
        if m.contains(t) {
            return Error::RouteUnavailable { message, status };
        }
    }
    Error::Paymos { message, status }
}

fn raise_for(status: u16, body: &Value, text: &str, retry_after: Option<u64>) -> Error {
    let message = message_of(body, status, text);
    let s = Some(status);
    match status {
        401 => Error::Auth { message, status: s },
        403 => Error::Forbidden { message, status: s },
        409 => Error::Conflict { message, status: s },
        429 => Error::RateLimited { message, status: s, retry_after },
        400 => classify400(&message, s),
        // 404 has no type of its own by design; it and every other non-2xx land on the base error.
        _ => Error::Paymos { message, status: s },
    }
}

/// RFC 3986 unreserved characters pass through; everything else is percent-encoded. Covers both
/// query values and path segments (movement ids).
pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Http {
    pub fn new(base_url: &str, api_key: &str, timeout: Option<Duration>) -> Self {
        Http {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            transport: Box::new(UreqTransport::new(timeout.unwrap_or(DEFAULT_TIMEOUT))),
        }
    }

    /// A client over a stubbed transport — tests never reach the network.
    pub fn with_transport(base_url: &str, api_key: &str, transport: Box<dyn Transport>) -> Self {
        Http {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            transport,
        }
    }

    fn send(&self, req: HttpRequest) -> Result<Value> {
        let mut req = req;
        req.headers
            .insert(0, ("authorization".to_string(), format!("Bearer {}", self.api_key)));

        let resp = self
            .transport
            .send(&req)
            .map_err(|e| Error::paymos(format!("transport error: {e}")))?;

        let parsed = serde_json::from_str::<Value>(&resp.body);
        if (200..300).contains(&resp.status) {
            // A 2xx that is not JSON is not the API either — most likely a site or SPA fallback
            // serving its own page on this path. Mapping it to `null` let every reader downstream
            // report a shape error (or, before the array fix, an empty list) instead of the cause.
            return parsed.map_err(|_| {
                let text = resp.body.trim();
                let what = if text.is_empty() {
                    "an empty body".to_string()
                } else {
                    format!("a non-JSON body: {}", snippet(text))
                };
                Error::Paymos {
                    message: format!(
                        "HTTP {} with {what} — expected the vault API's JSON; check the base URL \
                         (PAYMOS_BASE_URL) and any proxy in front of it",
                        resp.status
                    ),
                    status: Some(resp.status),
                }
            });
        }
        let body = parsed.unwrap_or(Value::Null);
        let retry_after = resp
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.parse::<u64>().ok());
        Err(raise_for(resp.status, &body, &resp.body, retry_after))
    }

    pub fn get(&self, path: &str, params: &[(&str, Option<String>)]) -> Result<Value> {
        let mut query = String::new();
        for (key, value) in params {
            if let Some(v) = value {
                if !query.is_empty() {
                    query.push('&');
                }
                query.push_str(&format!("{}={}", url_encode(key), url_encode(v)));
            }
        }
        let url = if query.is_empty() {
            format!("{}{path}", self.base_url)
        } else {
            format!("{}{path}?{query}", self.base_url)
        };
        self.send(HttpRequest { method: Method::Get, url, headers: vec![], body: None })
    }

    pub fn post(&self, path: &str, json_body: &Value, extra_headers: &[(&str, &str)]) -> Result<Value> {
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        for (name, value) in extra_headers {
            headers.push((name.to_string(), value.to_string()));
        }
        self.send(HttpRequest {
            method: Method::Post,
            url: format!("{}{path}", self.base_url),
            headers,
            body: Some(json_body.to_string()),
        })
    }
}
