//! The SDK's half of the 2-of-2 co-sign, over the same Rust core the Python SDK embeds and the
//! .NET server calls. Nobody reimplements FROST: this is a JSON op in, a JSON op out, and the
//! bytes signed here are the bytes those two would sign.
//!
//! Only two operations run client-side in production:
//!
//! - **commit** — round one. Fresh per-session nonces plus the public commitment the server needs
//!   to build the signing package. Called once PER movement source; each source is its own
//!   signing session with its own nonces.
//! - **sign** — round two. This side's signature share for one signing package.
//!
//! `build_signing_package` and `aggregate` are server-side operations and are deliberately
//! absent.
//!
//! Every blob — key package, commitments, nonces, signing package, signature share — is opaque.
//! It is passed through verbatim, never inspected and never rebuilt. The key package never
//! reaches the wire: it only ever feeds the local core.

use serde_json::{json, Value};

use crate::error::{Error, Result};

fn call(req: Value) -> Result<Value> {
    let raw = wallet_mpc::mpc_call_json(&req.to_string());
    let r: Value = serde_json::from_str(&raw)
        .map_err(|e| Error::paymos(format!("mpc call returned non-JSON: {e}")))?;
    if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let msg = r.get("error").and_then(|v| v.as_str()).unwrap_or("mpc call failed");
        return Err(Error::paymos(msg));
    }
    Ok(r)
}

/// Round one for one signing session. Send the commitments to sign/begin; keep the nonces
/// private. Returns `(commitments, nonces)`.
pub fn commit(key_package: &Value) -> Result<(Value, Value)> {
    let r = call(json!({ "op": "commit", "key_package": key_package }));
    let r = r?;
    Ok((
        r.get("commitments").cloned().unwrap_or(Value::Null),
        r.get("nonces").cloned().unwrap_or(Value::Null),
    ))
}

/// Round two: this side's signature share, which the server aggregates with its own.
pub fn sign(signing_package: &Value, nonces: &Value, key_package: &Value) -> Result<Value> {
    let r = call(json!({
        "op": "sign",
        "signing_package": signing_package,
        "nonces": nonces,
        "key_package": key_package,
    }))?;
    Ok(r.get("signature_share").cloned().unwrap_or(Value::Null))
}
