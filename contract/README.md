# Vault API wire contract

`fixtures/` is what the server actually answers, recorded from its real request pipeline by a test in
the server's own suite. That test fails when the server's answer stops matching these files; every
SDK's suite fails when it stops parsing them. Neither side is allowed to mock the other from memory.

Why this exists: seven of eight SDKs shipped reading `{"assets": [...]}` from an endpoint that has
always returned a bare array. Every one of their suites mocked the server its author imagined and
passed, and no money call in any of them could succeed.

## Files

Each fixture is `{"request": {method, path, body}, "response": {status, body}}`. Ids, cursors and
timestamps are normalized placeholders (`mv_1`, `cursor_1`, `2026-01-01T00:00:00.0000000+00:00`);
the same movement id keeps the same placeholder across files.

`sign_begin.json` and `sign_aggregate.json` carry `"synthesized"`: an offline host holds no DKG key
package, so their bodies are the server's own DTOs serialized with the app's JSON options — the
shape is real, the values are placeholders and do not verify cryptographically.

`vocabulary.json` is every public movement status and sign status, derived from the projection
code rather than listed by hand.

## What the server does that an SDK must survive

- **Null fields are omitted, not sent as `null`.** `min_deposit_raw`, `fees` on a list item,
  `dest_chain_tx_hash`, `completed_at`, `usd`, `code` — absent when empty.
- `/assets` and `/balances` answer a **bare JSON array**.
- Each asset carries `fingerprint`: lowercase hex SHA-256 of its raw asset id. The co-sign
  disclosure names tokens by that raw id; a blind-sign guard hashes each disclosed token and refuses
  unless it equals a fingerprint published under the send asset's label (one label can be listed
  more than once — two contracts of one coin — so any of them counts). An older server omits it:
  reads keep working, and a co-sign refuses rather than skipping the check.
  The fingerprint comes from the same server that builds the messages, so it catches a server bug or
  a compromised signing path, not a fully compromised server. Every SDK lets an integrator pin
  fingerprints out of band (`pinned_fingerprints`) for a trust anchor the server cannot move.
- Every non-2xx under `/vault/` has a JSON body with `error`; `sign/aggregate` failures also carry
  `ok: false` and `status`. A response that is not JSON at all (a proxy's HTML page) must still
  surface as the SDK's typed error, never a raw parse exception.

## Changing the wire

The fixtures are regenerated on the server side (`UPDATE_WIRE_CONTRACT=1` on its contract test), then
every SDK's suite is run. A fixture change that no SDK suite notices means a suite is not reading
the fixtures.

`scripts/publish-sdk.sh` vendors this folder into every public mirror as `contract/`, so an SDK's
tests look for `contract/fixtures` walking up from the test file, then `sdk/contract/fixtures`.
The fixtures are public: the server's contract test fails if one names the routing layer beneath the
wallet.
