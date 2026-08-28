//! Proves the Rust SDK drives the same co-signing core the Python SDK and the server drive: a
//! full 2-of-2 round — two parties, distributed key generation, commit, sign, aggregate — and
//! the resulting ed25519 signature verifies. If this passes, the halves agree on what a
//! signature is.
//!
//! The server-side operations are driven directly here rather than through the SDK, because the
//! SDK deliberately cannot perform them: it holds one half and no more.

use paymos_wallet::{commit, sign};
use serde_json::{json, Value};

const CLIENT: u64 = 1;
const SERVER: u64 = 2;

/// The server half, driven straight through the core's JSON dispatch — what the .NET server
/// does over FFI.
fn call(req: Value) -> Value {
    let raw = wallet_mpc::mpc_call_json(&req.to_string());
    serde_json::from_str(&raw).expect("core answers JSON")
}

fn package_for(packages: &Value, id: u64) -> Value {
    packages
        .as_array()
        .expect("packages is an array")
        .iter()
        .find(|x| x[0].as_u64() == Some(id))
        .unwrap_or_else(|| panic!("no round2 package for {id}"))[1]
        .clone()
}

#[test]
fn co_signs_a_real_2of2_signature_with_the_server_half() {
    let a1 = call(json!({ "op": "dkg_part1", "id": CLIENT, "max": 2, "min": 2 }));
    let b1 = call(json!({ "op": "dkg_part1", "id": SERVER, "max": 2, "min": 2 }));
    let a2 = call(json!({ "op": "dkg_part2", "secret": a1["secret"], "round1_packages": [[SERVER, b1["package"]]] }));
    let b2 = call(json!({ "op": "dkg_part2", "secret": b1["secret"], "round1_packages": [[CLIENT, a1["package"]]] }));
    let for_client = package_for(&b2["packages"], CLIENT);
    let for_server = package_for(&a2["packages"], SERVER);
    let a3 = call(json!({
        "op": "dkg_part3",
        "secret": a2["secret"],
        "round1_packages": [[SERVER, b1["package"]]],
        "round2_packages": [[SERVER, for_client]],
    }));
    let b3 = call(json!({
        "op": "dkg_part3",
        "secret": b2["secret"],
        "round1_packages": [[CLIENT, a1["package"]]],
        "round2_packages": [[CLIENT, for_server]],
    }));
    assert_eq!(
        a3["group_pubkey_hex"], b3["group_pubkey_hex"],
        "both halves derive one group key"
    );

    // The client half goes through the SDK; the server half is driven directly, because the SDK
    // has no way to perform it and that is the point.
    let (client_commitments, client_nonces) = commit(&a3["key_package"]).expect("client commit");
    let server = call(json!({ "op": "commit", "key_package": b3["key_package"] }));

    let pkg = call(json!({
        "op": "build_signing_package",
        "commitments": [[CLIENT, client_commitments], [SERVER, server["commitments"]]],
        "message_hex": "deadbeef",
    }));

    let client_share = sign(&pkg["signing_package"], &client_nonces, &a3["key_package"]).expect("client sign");
    let server_share = call(json!({
        "op": "sign",
        "signing_package": pkg["signing_package"],
        "nonces": server["nonces"],
        "key_package": b3["key_package"],
    }));

    let agg = call(json!({
        "op": "aggregate",
        "signing_package": pkg["signing_package"],
        "signature_shares": [[CLIENT, client_share], [SERVER, server_share["signature_share"]]],
        "public_key_package": a3["public_key_package"],
    }));
    assert_eq!(agg["verified"], json!(true), "aggregate verifies: {agg}");
}

#[test]
fn reports_a_failed_op_instead_of_returning_nonsense() {
    assert!(sign(&json!({}), &json!({}), &json!({})).is_err());
}
