//! Typed errors, ported one-for-one from the Python and TypeScript SDKs, so a caller who knows
//! one knows them all. Branching on the variant is the whole reason this enum exists.
//!
//! Every read, quote and sign call routes through `http`, which turns the server's error envelope
//! into one of these. `message` is the server's already client-safe reason; `status` is the HTTP
//! code that produced it, or `None` when the error did not come from a response.
//!
//! The set is fixed by the SDK spec — deliberately no `NotFound`, since a 404 surfaces as the
//! base [`Error::Paymos`]. Nothing here re-parses or rewrites a server message.

use std::fmt;

/// The ten error types of the SDK spec. `Paymos` is the base case; the other nine are the
/// classifications a caller branches on.
#[derive(Debug)]
pub enum Error {
    /// Any failure without a narrower type: local refusals, 404s, 5xx, transport wraps.
    Paymos { message: String, status: Option<u16> },
    /// 401 — the API key is missing, malformed, or not recognised.
    Auth { message: String, status: Option<u16> },
    /// 403 — the key is valid but lacks the scope this call requires.
    Forbidden { message: String, status: Option<u16> },
    /// 400 — the vault's available balance cannot cover the amount plus fees.
    InsufficientFunds { message: String, status: Option<u16> },
    /// 400 — the referenced quote has expired; fetch a fresh one and retry.
    QuoteExpired { message: String, status: Option<u16> },
    /// 400 — the delivered amount fell outside the accepted slippage bound.
    SlippageExceeded { message: String, status: Option<u16> },
    /// 400 — a withdraw changed the asset; that is a swap, and it is rejected here.
    CrossAssetWithdrawNotAllowed { message: String, status: Option<u16> },
    /// 400 — no route could be quoted for this pair right now; retry shortly.
    RouteUnavailable { message: String, status: Option<u16> },
    /// 429 — too many requests. `retry_after` is the Retry-After header when the server sent one.
    RateLimited { message: String, status: Option<u16>, retry_after: Option<u64> },
    /// 409 — idempotency-key conflict: reused with a different request, or a quote still forming.
    Conflict { message: String, status: Option<u16> },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The base case, used for every local refusal and unmapped failure.
    pub(crate) fn paymos(message: impl Into<String>) -> Self {
        Error::Paymos { message: message.into(), status: None }
    }

    /// The server's reason, client-safe as it arrived.
    pub fn message(&self) -> &str {
        match self {
            Error::Paymos { message, .. }
            | Error::Auth { message, .. }
            | Error::Forbidden { message, .. }
            | Error::InsufficientFunds { message, .. }
            | Error::QuoteExpired { message, .. }
            | Error::SlippageExceeded { message, .. }
            | Error::CrossAssetWithdrawNotAllowed { message, .. }
            | Error::RouteUnavailable { message, .. }
            | Error::RateLimited { message, .. }
            | Error::Conflict { message, .. } => message,
        }
    }

    /// The HTTP status that produced this error, or `None` for local refusals and transport wraps.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Paymos { status, .. }
            | Error::Auth { status, .. }
            | Error::Forbidden { status, .. }
            | Error::InsufficientFunds { status, .. }
            | Error::QuoteExpired { status, .. }
            | Error::SlippageExceeded { status, .. }
            | Error::CrossAssetWithdrawNotAllowed { status, .. }
            | Error::RouteUnavailable { status, .. }
            | Error::RateLimited { status, .. }
            | Error::Conflict { status, .. } => *status,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}
