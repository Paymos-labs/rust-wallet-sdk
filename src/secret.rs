//! The `vs_live_…` vault secret: an API key, the vault id, and — for a full key — this side's
//! FROST signing share. Ported from the Python SDK, same envelope, same field names, so one
//! secret works with any of the SDKs.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::error::{Error, Result};

const PREFIX: &str = "vs_live_";

/// Show only the non-secret prefix, so a log line or a stack trace cannot carry a usable key.
fn mask(key: &str) -> String {
    if key.is_empty() { "…".to_string() } else { format!("{}…", key.get(..8).unwrap_or(key)) }
}

pub struct VaultSecret {
    pub api_key: String,
    pub vault_id: i64,
    /// This side's FROST key package, as the JSON string the core consumes. `None` on a
    /// read-only secret. Never rendered by [`fmt::Debug`] or [`fmt::Display`].
    pub share: Option<String>,
}

impl VaultSecret {
    pub fn has_share(&self) -> bool {
        self.share.is_some()
    }

    pub fn pack(api_key: &str, vault_id: i64, share: Option<&str>) -> String {
        let payload = match share {
            Some(s) => format!(r#"{{"k":{},"v":{},"s":{}}}"#, json_str(api_key), vault_id, json_str(s)),
            None => format!(r#"{{"k":{},"v":{},"s":null}}"#, json_str(api_key), vault_id),
        };
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
    }

    pub fn parse(secret: &str) -> Result<VaultSecret> {
        let body = secret
            .strip_prefix(PREFIX)
            .ok_or_else(|| Error::paymos("not a vs_live_ vault secret"))?;
        let parsed: serde_json::Value = (|| -> std::result::Result<serde_json::Value, String> {
            let bytes = URL_SAFE_NO_PAD.decode(body).map_err(|e| e.to_string())?;
            let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
            serde_json::from_str(&text).map_err(|e| e.to_string())
        })()
        .map_err(|e: String| Error::paymos(format!("malformed vault secret: {e}")))?;

        let api_key = parsed.get("k").and_then(|v| v.as_str()).unwrap_or("");
        if !api_key.starts_with("vk_live_") {
            return Err(Error::paymos("vault secret carries no valid api key"));
        }
        let vault_id = parsed
            .get("v")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::paymos("vault secret carries no valid vault id"))?;
        let share = parsed.get("s").and_then(|v| v.as_str()).map(str::to_string);
        Ok(VaultSecret { api_key: api_key.to_string(), vault_id, share })
    }
}

/// Minimal JSON string escaping for `pack` — enough for keys and shares, which carry no
/// control characters by construction, but a quote or backslash must not break the envelope.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

impl std::fmt::Debug for VaultSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VaultSecret(api_key={:?}, vault_id={}, has_share={})",
            mask(&self.api_key),
            self.vault_id,
            self.has_share()
        )
    }
}

impl std::fmt::Display for VaultSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}
