//! The read model, ported field-for-field from the Python SDK.
//!
//! Every amount is a STRING of raw integer units, never a number. A token with 18 decimals runs
//! past what an IEEE double holds exactly, so a JSON parser that produced numbers here would
//! round somebody's balance without saying so. Use [`crate::format_units`] to show one and
//! [`crate::parse_units`] to build one.
//!
//! The server speaks snake_case; camelCase aliases are accepted because the TypeScript port
//! already taught some wire fixtures that spelling. A missing optional becomes `None` — what the
//! server means and what JSON round-trips.

use serde::Deserialize;

/// One asset the vault can hold, as the server names it.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub decimals: u32,
}

/// What the vault holds of one asset. `usd` is a display estimate and may be absent.
#[derive(Debug, Clone, Deserialize)]
pub struct Balance {
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub decimals: u32,
    #[serde(default, alias = "amountRaw")]
    pub amount_raw: String,
    #[serde(default)]
    pub usd: Option<String>,
}

/// The routing layer's own fee. `estimate` marks a figure that can still move.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteFee {
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub estimate: bool,
}

/// Every charge on an operation, split so none of them can hide inside another.
#[derive(Debug, Clone, Deserialize)]
pub struct Fees {
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub network: String,
    #[serde(default)]
    pub route: Option<RouteFee>,
    #[serde(default)]
    pub total: String,
    #[serde(default)]
    pub usd: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Amount {
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub asset: String,
}

/// What arrives. `min` is the floor the operation is allowed to deliver, and it is guaranteed.
#[derive(Debug, Clone, Deserialize)]
pub struct Receive {
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub min: String,
    #[serde(default)]
    pub asset: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quote {
    #[serde(default, alias = "movementId")]
    pub movement_id: Option<String>,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub send: Amount,
    /// The MAXIMUM this quote may take from the vault, reserve against slippage included — a
    /// ceiling, not a prediction. Approve against this and nothing can debit more than you agreed
    /// to; an exact-output route routinely spends less and returns the difference. Compare the
    /// finished movement's `send` to see what it actually cost.
    #[serde(default)]
    pub debit: Amount,
    #[serde(default)]
    pub receive: Receive,
    #[serde(default)]
    pub fees: Option<Fees>,
    #[serde(default, alias = "expiresAt")]
    pub expires_at: String,
    #[serde(default)]
    pub sources: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Movement {
    #[serde(default)]
    pub id: String,
    #[serde(default, rename = "type")]
    pub r#type: String,
    #[serde(default)]
    pub status: String,
    /// What the movement actually cost the vault: the approved maximum minus anything the route
    /// handed back. This is the figure that matches the on-chain balance — reconcile on it. It is
    /// NOT the quote's `debit`, which is the ceiling that was approved before the route ran.
    #[serde(default)]
    pub send: Amount,
    #[serde(default)]
    pub receive: Receive,
    #[serde(default)]
    pub fees: Option<Fees>,
    #[serde(default, alias = "destChainTxHash")]
    pub dest_chain_tx_hash: Option<String>,
    #[serde(default, alias = "destChainExplorerUrl")]
    pub dest_chain_explorer_url: Option<String>,
    #[serde(default, alias = "createdAt")]
    pub created_at: String,
    #[serde(default, alias = "completedAt")]
    pub completed_at: Option<String>,
}

// `#[serde(default)]` needs Default on every field type.
impl Default for Amount {
    fn default() -> Self {
        Amount { amount: String::new(), asset: String::new() }
    }
}

impl Default for Receive {
    fn default() -> Self {
        Receive { amount: String::new(), min: String::new(), asset: String::new() }
    }
}
