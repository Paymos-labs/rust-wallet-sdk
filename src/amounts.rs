//! Human decimal ↔ raw integer, ported from the Python SDK.
//!
//! Raw amounts are strings rather than numbers on purpose. A token with 18 decimals overflows the
//! exact range of an IEEE double at nine significant digits of whole units, so a float would round
//! silently — the one failure nobody notices until it is somebody's balance. This module is
//! string-only: no numeric conversion ever happens, so no amount is too long to parse exactly.

use crate::error::{Error, Result};

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn strip_leading_zeros(s: &str) -> &str {
    let stripped = s.trim_start_matches('0');
    if stripped.is_empty() { "0" } else { stripped }
}

/// `"1.5"`, 6 → `"1500000"`. Rejects anything that is not a non-negative decimal.
pub fn parse_units(human: &str, decimals: u32) -> Result<String> {
    let s = human.trim();
    let mut parts = s.splitn(3, '.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    if parts.next().is_some() || !is_digits(whole) || (!frac.is_empty() && !is_digits(frac)) {
        return Err(Error::paymos(format!("not a non-negative decimal: {human:?}")));
    }
    if frac.len() > decimals as usize {
        return Err(Error::paymos(format!("more than {decimals} fractional digits: {human:?}")));
    }
    let mut raw = String::with_capacity(whole.len() + decimals as usize);
    raw.push_str(whole);
    raw.push_str(frac);
    for _ in frac.len()..decimals as usize {
        raw.push('0');
    }
    Ok(strip_leading_zeros(&raw).to_string())
}

/// `"1500000"`, 6 → `"1.5"`. Trailing zeros in the fraction are dropped.
pub fn format_units(raw: &str, decimals: u32) -> Result<String> {
    let (sign, digits) = match raw.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", raw),
    };
    if !is_digits(digits) {
        return Err(Error::paymos(format!("not a raw integer amount: {raw:?}")));
    }
    let digits = strip_leading_zeros(digits);
    // "-0" is 0: the sign of nothing is nothing.
    let sign = if digits == "0" { "" } else { sign };
    if decimals == 0 {
        return Ok(format!("{sign}{digits}"));
    }
    let decimals = decimals as usize;
    let padded = format!("{:0>width$}", digits, width = decimals + 1);
    let (whole, frac) = padded.split_at(padded.len() - decimals);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        Ok(format!("{sign}{whole}"))
    } else {
        Ok(format!("{sign}{whole}.{frac}"))
    }
}
