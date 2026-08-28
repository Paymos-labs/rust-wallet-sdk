//! The entry point, ported from the Python and TypeScript SDKs. Same methods, same guarantees,
//! same refusals — a caller who knows one SDK knows this one.
//!
//! Everything that moves money goes through a 2-of-2 co-sign in which this process holds one half
//! of the key and never sends it anywhere. The server cannot sign alone and neither can this —
//! which is only worth anything if this side refuses to sign things it has not checked, so it
//! does:
//!
//! - **the quote must echo what was approved** — a decimals bug or a lying server changes the
//!   numbers, and the numbers are compared before a commitment
//! - **the debit may be capped** — for an exact_out payout the input side is computed by the
//!   server, so `max_debit` is the caller's ceiling
//! - **never blind-sign** — the server discloses each message, this side recomputes the digest
//!   and refuses unless the package binds to it
//!
//! Any of those failing returns an error before a single signature share exists.

mod amounts;
mod digest;
mod error;
mod http;
mod mpc;
mod secret;
mod types;
mod wallet;

pub use amounts::{format_units, parse_units};
pub use digest::{payload_digest, payload_digest_hex};
pub use error::{Error, Result};
pub use http::{Http, HttpRequest, HttpResponse, Method, Transport, UreqTransport};
pub use mpc::{commit, sign};
pub use secret::VaultSecret;
pub use types::{Amount, Asset, Balance, Fees, Movement, Quote, Receive, RouteFee};
pub use wallet::{SwapOptions, Wallet, WithdrawOptions};
