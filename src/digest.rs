//! The exact 32 bytes the vault's signature is computed over — a byte-for-byte port of the Python
//! SDK's digest, which is itself pinned to the server by a cross-language golden vector.
//!
//! The SDK uses this to VERIFY, before a single FROST commitment leaves the process, that the
//! server disclosed the true message it is asking the vault to sign. Never blind-sign.
//!
//! digest = sha256 of:
//!   u32   tag                 fixed domain separation; must byte-match the server
//!   string message            u32-LE length prefix + utf-8
//!   bytes32 nonce             raw, exactly 32
//!   string recipient          u32-LE length prefix + utf-8
//!   option<string> callback   0x00 = none, else 0x01 + string
//!
//! Message and recipient are hashed as their EXACT utf-8 bytes. Re-formatting the payload changes
//! the digest, which is precisely what the binding check is there to catch.

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Part of the wire contract. Change it and the client and server stop agreeing forever.
const MSG_TAG: u32 = 2147484061;

fn len_prefixed_utf8(s: &str) -> Vec<u8> {
    let body = s.as_bytes();
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// The 32-byte sha256 digest the vault's ed25519 signature is over. `nonce` must be exactly 32.
pub fn payload_digest(
    message: &str,
    nonce: &[u8],
    recipient: &str,
    callback_url: Option<&str>,
) -> Result<[u8; 32]> {
    if nonce.len() != 32 {
        return Err(Error::paymos(format!("nonce must be 32 bytes, got {}", nonce.len())));
    }
    let mut w = Vec::new();
    w.extend_from_slice(&MSG_TAG.to_le_bytes());
    w.extend_from_slice(&len_prefixed_utf8(message));
    w.extend_from_slice(nonce);
    w.extend_from_slice(&len_prefixed_utf8(recipient));
    match callback_url {
        None => w.push(0x00),
        Some(url) => {
            w.push(0x01);
            w.extend_from_slice(&len_prefixed_utf8(url));
        }
    }
    Ok(Sha256::digest(&w).into())
}

/// The digest as lowercase hex — the form the FROST signing package carries, so the two compare.
pub fn payload_digest_hex(
    message: &str,
    nonce: &[u8],
    recipient: &str,
    callback_url: Option<&str>,
) -> Result<String> {
    Ok(hex::encode(payload_digest(message, nonce, recipient, callback_url)?))
}
