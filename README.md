# paymos-wallet — Rust SDK

Send, swap and bridge **USDT, USDC, BTC, ETH** and the rest of the wallet's catalogue from
Rust — no browser, no wallet extension, and no seed phrase anywhere in your environment. It is
built for the services that pay people out: marketplaces, exchanges, payroll and treasury jobs.

Your process holds **one half** of the signing key; the Paymos server holds the other.
**Neither side alone can move funds** — every swap and withdrawal is co-signed, and your half
never reaches the server.

- **Non-custodial** — a leaked API key can't sign; your co-signing share stays in your process.
- **Fee-transparent** — every quote is a dry preview with a full `platform / network / route`
  fee breakdown you see *before* signing anything.
- **Human amounts, no floats** — you pass decimal strings like `"100"` / `"0.5"`; responses come
  back as raw integer strings.
- **Blocking, no runtime forced on you** — the method set is a port of the Python and TypeScript
  SDKs, and a blocking client compiles into any binary without an async runtime.

The SDK talks in plain asset terms (`USDC@base`, `ETH@arb`) and hides the routing and the
2-of-2 signing behind a small, safe surface.

## Install

```toml
[dependencies]
paymos-wallet = "1"
```

Requires **Rust ≥ 1.75**.

## Get access & your API key

The SDK controls a **Paymos vault**, which you own from the **Paymos wallet**. The wallet is a
**Telegram-only Mini App** — there is no website login; you use it entirely inside Telegram.

**1. Open the wallet.** In Telegram, open **@Ox0000000000000000000Bot** and tap **Open Wallet**.
Access is invite-based. Create your vault if you don't have one.

**2. Mint an API key.** In the Mini App go to **Settings → Vault API** and create one of:

| Key | Can do | Carries |
|---|---|---|
| **read** | `assets`, `balances`, `quote_swap`, `quote_withdraw`, `movement`, `movements` | API key only |
| **full** | everything a read key does, **plus** `swap` and `withdraw` | API key **+** your co-signing share |

The key is a `vs_live_…` string. A full key's co-signing share is decrypted on your device and
**never leaves your process**. Treat a full secret like a private key.

**3. Point the SDK at it** with an environment variable:

```bash
export PAYMOS_VAULT_SECRET="vs_live_…"
# optional — defaults to https://wallet.paymos.io
# export PAYMOS_BASE_URL="https://wallet.paymos.io"
```

## Quickstart

```rust
use paymos_wallet::{Wallet, SwapOptions};
use std::time::Duration;

fn main() -> paymos_wallet::Result<()> {
    let w = Wallet::from_env()?;                    // reads PAYMOS_VAULT_SECRET

    // Balances: per-asset, raw amount strings + a nullable USD value.
    for b in w.balances()? {
        println!("{} {} {:?}", b.asset, b.amount_raw, b.usd);
    }

    // Preview a withdraw — a dry, money-safe quote. Headline is the fee breakdown.
    let q = w.quote_withdraw("USDC@base", "25", "0xRecipient…", "exact_out")?;
    if let Some(fees) = &q.fees {
        println!("{} {} {}", fees.platform, fees.network, fees.total);
    }

    // Move money (full key only). Amounts are human decimal strings.
    let mv = w.swap("USDC@base", "ETH@arb", "100", SwapOptions::default())?;
    let settled = w.wait(&mv.id, Duration::from_secs(300), Duration::from_secs(2))?;
    println!("{}", settled.status);
    Ok(())
}
```

## Core concepts

### Amounts: human in, raw out

Amounts you **pass** are human decimal strings (`"100"`, `"0.5"`) — the server owns each asset's
decimals and scales them to raw. Amounts in **responses** come back as raw integer strings.
**Never a float**, in either direction. `parse_units` / `format_units` convert between the two.

### Quotes and fees

`quote_swap` / `quote_withdraw` always return a **dry** `Quote` — a pure preview that persists
nothing and works for read and full keys alike. The headline is `quote.fees`, and
`quote.receive.min` is the floor the quote states for your slippage tolerance.

### Moving money

`swap` and `withdraw` (full key only) create a **real** movement and drive the 2-of-2 co-sign,
then return a `Movement`. Before any signature the SDK re-checks that the server's quote echoes
**exactly** the assets and amount you approved — a decimals bug or contract drift fails the call
and **signs nothing**. For an `exact_out` withdraw the debit is computed by the server; pass
`max_debit` to bound it.

```rust
let mv = w.swap("USDC@base", "ETH@arb", "100", SwapOptions {
    min_receive: Some("0.03".into()),               // optional slippage floor
    ..Default::default()
})?;

let mv = w.withdraw("USDC@base", "25", "0xRecipient…", Default::default())?;
```

### Withdraw modes

- `"exact_out"` *(default)* — the recipient receives exactly `amount`; the vault debit
  (`amount` + fees) is server-computed.
- `"total_in"` — `amount` is the **total** debited from the vault; the recipient gets that
  minus fees.

### Safety caps

- **`slippage_bps`** (swap / quote_swap, default `50` = **0.5%**) — the slippage tolerance sent
  with the quote; the quote's `receive.min` already reflects it.
- **`min_receive`** (swap) — a human decimal in the receive asset. If the quote's stated floor
  (`receive.min`) is below it, the call fails with `Error::SlippageExceeded` and signs nothing.
- **`max_debit`** (swap / withdraw) — a human decimal ceiling on the total vault debit. Strongly
  recommended for unattended `exact_out` payouts, whose input side is server-computed.

### Idempotency

Pass your own `idempotency_key` (e.g. a durable payout id) so a retry after an ambiguous failure
replays the **same** movement instead of paying twice; a retry that finds it already
signed/relayed **converges** on it rather than double-signing. Omit it and a fresh key is
generated per call.

### Waiting and statuses

`wait(id, timeout, interval)` polls a movement until it reaches a terminal status —
`completed | failed | refunded | expired | cancelled` — and fails on timeout.

## Errors

Every fallible call returns `Result<_, paymos_wallet::Error>`; branch on the variant instead of
string-matching a message. Each variant carries the server's client-safe `message` and the
HTTP `status` when one produced it (`None` for local refusals and transport failures).

| Variant | When |
|---|---|
| `Auth` | `401` — API key missing, malformed, or unrecognized |
| `Forbidden` | `403` — the key is valid but lacks the scope this call needs |
| `InsufficientFunds` | `400` — the vault balance can't cover the amount (plus fees) |
| `QuoteExpired` | `400` — the referenced quote expired; fetch a fresh one |
| `SlippageExceeded` | `400` or local — delivered / quoted minimum amount fell below your floor |
| `CrossAssetWithdrawNotAllowed` | `400` — a withdraw changed the asset (that's a swap) |
| `RouteUnavailable` | `400` — no route could be quoted right now; retry shortly |
| `RateLimited` | `429` — carries `retry_after` (seconds, or `None`) |
| `Conflict` | `409` — idempotency key reused with a different request |
| `Paymos` | base — `404`, any other 4xx/5xx, local refusals, transport errors |

Calling `swap` / `withdraw` with a **read-only** key fails *before* any movement is created — a
read key can never sign.

## Security

- A leaked **API key alone cannot move funds** — no share, no signature (and the key can be
  revoked).
- A leaked **share alone cannot move funds** — no server co-sign.
- Both are required, on purpose. The SDK additionally checks every message it co-signs: it binds
  to the disclosed text, signs from your vault, moves the approved asset, and moves no more than
  the approved debit. What it does **not** check is the recipient — each transfer goes to a route
  deposit address the server chooses. And for `exact_out` the approved debit is itself computed by
  the server unless you pass `max_debit`, so set `max_debit` for unattended payouts.

Treat a full `vs_live_…` secret like a private key, and back up the share — losing it means
losing your ability to co-sign.

## License

**Proprietary** — see `LICENSE`. The source is readable; that grants no reuse rights.

---

## FAQ

### How do I send USDT (TRC20) from code?

Install the SDK, point it at your vault, and ask for a withdrawal with the asset written the way the
wallet writes it — `USDT@tron`. The same call sends USDT on BNB Chain (`USDT@bsc`), Ethereum
(`USDT@eth`), Polygon, Solana, Arbitrum, Avalanche or TON: the asset string is the only difference.

```
cargo add paymos-wallet
```

### Is this custodial? Who can move the money?

Nobody alone. The vault is **2-of-2**: your process holds one signing share, the server holds the
other, and a transfer needs both. A stolen API key cannot sign, and the server cannot sign without
you. Your share never leaves your process.

### What is the difference between a quote and a transfer?

A quote is a dry run. It prices the route, separates platform, network and route fees, and reserves
nothing — ask for as many as you like. A transfer is the same numbers, signed. Nothing moves until
your half of the signature exists.

### Does it blind-sign whatever the server asks for?

No, and this is the part worth reading twice. The server discloses the exact message it wants
signed; the SDK recomputes the digest itself and refuses unless that message belongs to your vault
and moves no more than the debit you approved. Two limits: the recipient is a route deposit address
the server chooses and is not checked, and for `exact_out` the approved debit is the server's own
figure unless you pass `max_debit`.

It also checks that every transfer moves the asset you approved, by comparing the token's SHA-256
with the fingerprint `/assets` publishes for it. That fingerprint comes from the same server that
builds the messages, so it catches a server bug or a compromised signing path, not a server
compromised end to end. For an anchor that does not depend on the server, pin the fingerprints
yourself — for a pinned asset only your values count:

```rust
let w = Wallet::from_env()?
    .with_pinned_fingerprints([("USDC@base", "a42c7e46411394e7c7719eef5be19239b2716b91d3a4a843fd3d2c0b6ca16b7c")]);
```

### Which networks and assets are supported?

USDT moves on nine chains — Tron, Ethereum, BNB Chain, Polygon, Solana, Arbitrum, Optimism,
Avalanche and TON — and USDC on eight: that same list without Tron and TON, with Base instead. Add
the native coin of each of those chains, and BTC, LTC, DOGE, XRP, BCH, DASH, ZEC and ADA, and the
catalogue is 21 assets across 18 chains. It is also the authority: an asset it does not list cannot
be quoted or sent, in any SDK.

### How are fees reported?

Every quote breaks the cost into platform, network and route, and amounts come back as raw integer
strings rather than floats: money that has been through a float is money with a rounding error in
it. You pass human amounts in (`"100"`, `"0.5"`) and read exact integers back.

### Can I use it for automated payouts?

That is what it is for: headless, no browser, no wallet extension, no seed phrase in the environment.
A worker holding one share can pay out continuously, and losing that machine costs the ability to
sign — not the funds.

### What happens if a transfer is interrupted?

Every money-moving call takes an idempotency key. Repeating a call with the same key returns the
original operation instead of starting a second one, so a retry after a timeout with the same key
replays the first payment rather than making another.

## The same wallet, in eight languages

Every SDK here speaks to the same vault API and enforces the same rules; pick the one your service is
written in. The signing core is shared, so a fix there reaches all of them.

| Language | Package | Repository |
|---|---|---|
| Python | `pip install paymos-wallet` | [python-wallet-sdk](https://github.com/paymos-labs/python-wallet-sdk) |
| TypeScript / Node.js | `npm install @paymos/wallet` | [typescript-wallet-sdk](https://github.com/paymos-labs/typescript-wallet-sdk) |
| Go | `go get github.com/paymos-labs/go-wallet-sdk` | [go-wallet-sdk](https://github.com/paymos-labs/go-wallet-sdk) |
| Rust *(this one)* | `cargo add paymos-wallet` | [rust-wallet-sdk](https://github.com/paymos-labs/rust-wallet-sdk) |
| C# / .NET | `dotnet add package Paymos.Wallet` | [csharp-wallet-sdk](https://github.com/paymos-labs/csharp-wallet-sdk) |
| Java | `io.paymos:wallet` | [java-wallet-sdk](https://github.com/paymos-labs/java-wallet-sdk) |
| Ruby | `gem install paymos-wallet` | [ruby-wallet-sdk](https://github.com/paymos-labs/ruby-wallet-sdk) |
| PHP | `composer require paymos/wallet` | [php-wallet-sdk](https://github.com/paymos-labs/php-wallet-sdk) |
| Signing core | `paymos-wallet-core` | [paymos-wallet-core](https://github.com/paymos-labs/paymos-wallet-core) |

## Documentation

- [wallet.paymos.io](https://wallet.paymos.io) — the product, the supported networks, and guides for
  the routes people ask about most: USDT BEP20 to TRC20, USDC between chains, and the rest.
- [Networks and assets](https://wallet.paymos.io/networks) — every chain and token the wallet
  carries, with the standard each one is named by.
- [USDT BEP20 → TRC20](https://wallet.paymos.io/guides/usdt-bep20-to-trc20) — a worked route, priced
  live; the other pairs are linked from it.
