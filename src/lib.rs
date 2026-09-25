//! The entry point, ported from the Python and TypeScript SDKs. Same methods, same guarantees,
//! same refusals — a caller who knows one SDK knows this one.
//!
//! Everything that moves money goes through a 2-of-2 co-sign in which this process holds one half
//! of the key and never sends it anywhere. The server cannot sign alone and neither can this —
//! which is only worth anything if this side refuses to sign things it has not checked, so it
//! does:
//!
//! - **the quote must echo what was approved** — a decimals bug or contract drift changes the
//!   assets or amounts, and they are compared before a commitment
//! - **the debit may be capped** — for an exact_out payout the input side is computed by the
//!   server, so `max_debit` is the caller's ceiling; without it, the debit the guard enforces is
//!   the server's own figure. Set it for unattended payouts
//! - **never blind-sign** — the server discloses each message, this side recomputes the digest
//!   and refuses unless the package binds to it, signs from this vault, moves the approved asset
//!   (by the fingerprint the server publishes, or one the caller pinned) and moves no more than
//!   the approved debit
//!
//! Any of those failing returns an error before a single signature share exists. Not checked: the
//! recipient. Each transfer goes to a route deposit address the server chooses.

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
pub use wallet::{Fingerprints, SwapOptions, Wallet, WithdrawOptions};
