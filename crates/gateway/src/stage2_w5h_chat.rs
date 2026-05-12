//! Stage 2 W5h — chat-route parser for the budget-funding + 3-minute
//! expiry / refund flow.
//!
//! Supported grammar (case-insensitive, whitespace tolerant):
//!
//! English:
//!   "If Solend Main Pool USDC deposit APY is above 1%, deposit 0.25
//!    USDC from my wallet, expires in 3 minutes."
//!
//! Chinese / mixed:
//!   "如果 Save APY > 1%, deposit 0.25 USDC, 有效期 3 分鐘"
//!
//! What's intentionally rigid in this slice:
//!
//! - Amount MUST be exactly `0.25 USDC` / `250_000` raw. Any other
//!   amount surfaces as `UnsupportedAmount`.
//! - Expiry MUST be exactly `3 minutes` (English) or `3 分鐘`/`3分鐘`
//!   (Chinese). Any other expiry surfaces as `UnsupportedExpiry`.
//! - Pool name accepts `Save` / `Solend` / `Save Finance` as synonyms.
//! - APY / APR wording is both accepted; the decision metric stays
//!   Save UI display APY (W5f).
//!
//! The parser does NOT make any RPC call. It produces a [`W5hParsed`]
//! that the gateway bridge passes downstream.

use serde::{Deserialize, Serialize};

/// Decoded shape of the supported W5h grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct W5hParsed {
    /// Threshold in basis points (1 % == 100 bps).
    pub threshold_bps: u32,
    /// Verbatim percent label the user typed (e.g. `"1"` or `"2.5"`).
    pub threshold_pct_label: String,
    /// Always `W5H_DEPOSIT_AMOUNT_RAW` (250 000 raw) in this slice.
    pub amount_raw: u64,
    /// Always `W5H_EXPIRY_SECONDS` (180 s) in this slice.
    pub expires_seconds: u64,
}

/// Hard-coded amount: 250 000 raw == 0.25 USDC.
pub const W5H_DEPOSIT_AMOUNT_RAW: u64 = 250_000;

/// Hard-coded expiry: 180 s == 3 minutes.
pub const W5H_EXPIRY_SECONDS: u64 = 180;

/// Typed parser errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum W5hParseError {
    MissingMarker { what: String },
    WrongPool { detail: String },
    UnsupportedAmount { detail: String },
    UnsupportedExpiry { detail: String },
    MalformedPercent { detail: String },
    NegativeThreshold { value: String },
    ThresholdOverflow { value: String },
}

impl std::fmt::Display for W5hParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            W5hParseError::MissingMarker { what } => write!(f, "missing marker: {what}"),
            W5hParseError::WrongPool { detail } => write!(f, "wrong pool: {detail}"),
            W5hParseError::UnsupportedAmount { detail } => {
                write!(f, "unsupported amount: {detail}")
            }
            W5hParseError::UnsupportedExpiry { detail } => {
                write!(f, "unsupported expiry: {detail}")
            }
            W5hParseError::MalformedPercent { detail } => {
                write!(f, "malformed percent: {detail}")
            }
            W5hParseError::NegativeThreshold { value } => {
                write!(f, "negative threshold: {value}")
            }
            W5hParseError::ThresholdOverflow { value } => {
                write!(f, "threshold overflow: {value}")
            }
        }
    }
}

/// Lightweight prefilter: returns `true` for any message that's
/// *worth* trying to parse against the W5h grammar. The strict
/// parser then accepts or rejects.
///
/// W5h-lite (2026-05-12) — accepts BOTH the verbose form with the
/// explicit expiry phrase (`expires in 3 minutes` / `有效期 3 分鐘`)
/// AND the simplified demo form without expiry. The discriminator
/// against W5d/W5e/W5f is no longer the expiry phrase alone; instead
/// we require the W5h-specific `from my wallet` qualifier (English),
/// the literal Chinese 如果 ... USDC head, OR the explicit expiry
/// phrase. W5d's grammar uses `from my bounded executor wallet` and
/// is rejected by the `from my wallet` check.
pub fn looks_like_w5h_chat_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let pool_named = lower.contains("save")
        || lower.contains("solend");
    let amount_named =
        lower.contains("0.25 usdc") || lower.contains("0.25usdc") || lower.contains("250000");
    let expires_named = lower.contains("expires in")
        || lower.contains("有效期")
        || lower.contains("expiry");
    // W5h-lite simplified-form discriminators (no expiry needed).
    // Reject W5d's "from my bounded executor wallet" form by hand.
    let bounded_executor_form = lower.contains("from my bounded executor wallet")
        || lower.contains("into solend.");
    let w5h_lite_english_marker =
        lower.contains("from my wallet") && !bounded_executor_form;
    // Chinese 如果 ... deposit 0.25 USDC head — never used by W5d/W5e/W5f.
    let w5h_lite_chinese_marker = text.contains("如果");
    if !pool_named || !amount_named {
        return false;
    }
    if bounded_executor_form {
        return false;
    }
    expires_named || w5h_lite_english_marker || w5h_lite_chinese_marker
}

/// Strict parser. Returns a `W5hParsed` on success; a typed error
/// otherwise.
pub fn parse_w5h_chat_command(text: &str) -> Result<W5hParsed, W5hParseError> {
    let lower = text.to_ascii_lowercase();
    let normalized = normalize_whitespace(&lower);

    // ── Pool name ────────────────────────────────────────────────────
    let pool_ok = normalized.contains("save")
        || normalized.contains("solend");
    if !pool_ok {
        return Err(W5hParseError::WrongPool {
            detail: "expected 'Save' / 'Solend' / 'Save Finance'".to_string(),
        });
    }

    // ── Amount ───────────────────────────────────────────────────────
    let amount_ok = normalized.contains("0.25 usdc")
        || normalized.contains("0.25usdc")
        || (normalized.contains("250000") && normalized.contains("usdc"));
    if !amount_ok {
        return Err(W5hParseError::UnsupportedAmount {
            detail: "only 0.25 USDC is supported in W5h".to_string(),
        });
    }
    // Reject other written-out amounts (e.g. "0.5 USDC", "1 USDC",
    // "0.30 USDC"). Look for any "<digit>(.<digit>)? usdc" that
    // ISN'T "0.25 usdc".
    if let Some(amt_str) = find_explicit_amount_usdc(&normalized) {
        if amt_str != "0.25" {
            return Err(W5hParseError::UnsupportedAmount {
                detail: format!("amount {amt_str} USDC is not supported (only 0.25 USDC)"),
            });
        }
    }

    // ── Expiry (W5h-lite: optional) ──────────────────────────────────
    //
    // The original W5h grammar required `expires in 3 minutes` (or
    // 有效期 3 分鐘); under W5h-lite the simplified demo command
    // omits the expiry clause and inherits the default 180 s window.
    // The parser:
    //   - silently accepts ZERO mention of expiry (lite form);
    //   - accepts the canonical `3 minutes` / `3 分鐘` form;
    //   - REJECTS any explicit OTHER expiry (e.g. `5 minutes`),
    //     because that's a user-typed mismatch that should fail loud.
    let saw_expiry_phrase =
        find_expiry_minutes(&normalized).is_some() || find_expiry_minutes(text).is_some();
    let expiry_ok =
        matches_three_minute_expiry(&normalized) || matches_three_minute_expiry(text);
    if saw_expiry_phrase && !expiry_ok {
        if let Some(n) = find_expiry_minutes(&normalized).or_else(|| find_expiry_minutes(text)) {
            return Err(W5hParseError::UnsupportedExpiry {
                detail: format!("expiry {n} minutes is not supported (only 3 minutes)"),
            });
        }
    }

    // ── Threshold percent ────────────────────────────────────────────
    let pct_str = find_threshold_pct(&normalized)
        .ok_or_else(|| W5hParseError::MissingMarker {
            what: "'above X%' or '> X%'".to_string(),
        })?;
    if pct_str.starts_with('-') {
        return Err(W5hParseError::NegativeThreshold {
            value: pct_str.clone(),
        });
    }
    let threshold_bps = decimal_percent_to_bps(&pct_str)?;

    Ok(W5hParsed {
        threshold_bps,
        threshold_pct_label: pct_str,
        amount_raw: W5H_DEPOSIT_AMOUNT_RAW,
        expires_seconds: W5H_EXPIRY_SECONDS,
    })
}

/// Find the threshold percent label. Accepts:
///   - "above X%"   (English W5d/W5f form)
///   - "> X%"       (operator shorthand)
///   - "is above X%"
///   - "大於 X%"    (defense — not strictly required by the brief)
fn find_threshold_pct(s: &str) -> Option<String> {
    // Search in priority order; first match wins.
    for marker in &["above ", "> ", ">"] {
        if let Some(idx) = s.find(marker) {
            let after = &s[idx + marker.len()..];
            if let Some(pct_end) = after.find('%') {
                let pct = after[..pct_end].trim().to_string();
                if !pct.is_empty() {
                    return Some(pct);
                }
            }
        }
    }
    None
}

/// Find an explicit amount-USDC token like "0.25 USDC" or "1 USDC".
/// Returns the digit-string portion; `None` if no pattern matched.
fn find_explicit_amount_usdc(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for a digit start, then read a decimal-percent-like token.
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let num = &s[start..i];
        // Skip optional space.
        let mut j = i;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        // Match "usdc" (lower-cased input).
        if j + 4 <= bytes.len() && &s[j..j + 4] == "usdc" {
            // Skip whole-integer matches that don't have a fractional
            // part — common in expiry strings like "3 minutes". But
            // "1 USDC" / "0.25 USDC" remain explicit.
            return Some(num.to_string());
        }
    }
    None
}

/// Matches "expires in 3 minutes" / "expires in 3 minute" /
/// "有效期 3 分鐘" / "有效期 3分鐘" / "有效期 3 分钟".
fn matches_three_minute_expiry(s: &str) -> bool {
    let trimmed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // English
    if trimmed.contains("expires in 3 minute") {
        return true;
    }
    if trimmed.contains("expiry 3 minute") {
        return true;
    }
    // Chinese (trad)
    let no_spaces: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if no_spaces.contains("有效期3分鐘") {
        return true;
    }
    // Chinese (simp)
    if no_spaces.contains("有效期3分钟") {
        return true;
    }
    false
}

/// Find "expires in N minute(s)" / "有效期 N 分鐘" — returns N as a
/// string. Used to produce a useful error message when the expiry
/// isn't exactly 3.
fn find_expiry_minutes(s: &str) -> Option<String> {
    let trimmed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Try "expires in N minute"
    for prefix in &["expires in ", "expiry "] {
        if let Some(idx) = trimmed.find(prefix) {
            let after = &trimmed[idx + prefix.len()..];
            if let Some(end_idx) = after.find(" minute") {
                let n = after[..end_idx].trim().to_string();
                if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() {
                    return Some(n);
                }
            }
        }
    }
    // Chinese
    let no_spaces: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(idx) = no_spaces.find("有效期") {
        let after = &no_spaces[idx + "有效期".len()..];
        let n: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !n.is_empty() {
            return Some(n);
        }
    }
    None
}

fn decimal_percent_to_bps(s: &str) -> Result<u32, W5hParseError> {
    if s.is_empty() {
        return Err(W5hParseError::MalformedPercent {
            detail: "empty".to_string(),
        });
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 2 {
        return Err(W5hParseError::MalformedPercent {
            detail: "too many dots".to_string(),
        });
    }
    let whole_str = if parts[0].is_empty() { "0" } else { parts[0] };
    let frac_str = if parts.len() == 2 { parts[1] } else { "" };
    if frac_str.len() > 2 {
        return Err(W5hParseError::MalformedPercent {
            detail: "more than two fractional digits".to_string(),
        });
    }
    let whole: u32 = whole_str
        .parse()
        .map_err(|_| W5hParseError::MalformedPercent {
            detail: format!("non-numeric whole part: {whole_str:?}"),
        })?;
    let frac: u32 = if frac_str.is_empty() {
        0
    } else {
        let padded = format!("{:0<2}", frac_str);
        padded
            .parse()
            .map_err(|_| W5hParseError::MalformedPercent {
                detail: format!("non-numeric fractional part: {frac_str:?}"),
            })?
    };
    let bps = whole
        .checked_mul(100)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| W5hParseError::ThresholdOverflow {
            value: s.to_string(),
        })?;
    Ok(bps)
}

fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_canonical_one_percent_three_minutes() {
        let s = "If Solend Main Pool USDC deposit APY is above 1%, \
                 deposit 0.25 USDC from my wallet, expires in 3 minutes.";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
        assert_eq!(p.threshold_pct_label, "1");
        assert_eq!(p.amount_raw, 250_000);
        assert_eq!(p.expires_seconds, 180);
        assert!(looks_like_w5h_chat_command(s));
    }

    #[test]
    fn english_apr_synonym_accepted() {
        let s = "If Solend Main Pool USDC deposit APR is above 0.5%, \
                 deposit 0.25 USDC from my wallet, expires in 3 minutes.";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 50);
    }

    #[test]
    fn english_save_synonym_accepted() {
        let s = "If Save Main Pool USDC deposit APY > 2.5%, \
                 deposit 0.25 USDC from my wallet, expires in 3 minutes.";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 250);
    }

    #[test]
    fn english_save_finance_accepted() {
        let s = "If Save Finance Main Pool USDC APY > 1%, \
                 deposit 0.25 USDC from my wallet, expires in 3 minutes.";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
    }

    #[test]
    fn chinese_traditional_canonical() {
        let s = "如果 Save APY > 1%，deposit 0.25 USDC，有效期 3 分鐘";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
        assert_eq!(p.amount_raw, 250_000);
        assert_eq!(p.expires_seconds, 180);
        assert!(looks_like_w5h_chat_command(s));
    }

    #[test]
    fn chinese_traditional_no_space_inside_expiry() {
        let s = "如果 Save APY > 1%，deposit 0.25 USDC，有效期3分鐘";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
    }

    #[test]
    fn chinese_simplified_accepted() {
        let s = "如果 Save APY > 1%，deposit 0.25 USDC，有效期 3 分钟";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
    }

    #[test]
    fn rejects_wrong_amount_one_usdc() {
        let s = "If Save APY > 1%, deposit 1 USDC, expires in 3 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        match err {
            W5hParseError::UnsupportedAmount { detail } => {
                assert!(detail.contains("0.25 USDC"));
            }
            other => panic!("expected UnsupportedAmount, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_amount_half_usdc() {
        let s = "If Save APY > 1%, deposit 0.5 USDC, expires in 3 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        assert!(matches!(err, W5hParseError::UnsupportedAmount { .. }));
    }

    #[test]
    fn rejects_wrong_expiry_five_minutes() {
        let s = "If Save APY > 1%, deposit 0.25 USDC, expires in 5 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        match err {
            W5hParseError::UnsupportedExpiry { detail } => {
                assert!(detail.contains("5") && detail.contains("3 minutes"));
            }
            other => panic!("expected UnsupportedExpiry, got {other:?}"),
        }
    }

    #[test]
    fn w5h_lite_simplified_english_accepted() {
        // W5h-lite: no expiry clause → parser accepts and defaults
        // to 180 s.
        let s = "If Save APY > 1%, deposit 0.25 USDC";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
        assert_eq!(p.amount_raw, 250_000);
        assert_eq!(p.expires_seconds, 180);
    }

    #[test]
    fn w5h_lite_simplified_chinese_accepted() {
        // W5h-lite 簡化 form, no 有效期 clause.
        let s = "如果 Save APY > 1%，deposit 0.25 USDC";
        let p = parse_w5h_chat_command(s).unwrap();
        assert_eq!(p.threshold_bps, 100);
        assert_eq!(p.amount_raw, 250_000);
        assert_eq!(p.expires_seconds, 180);
        assert!(looks_like_w5h_chat_command(s));
    }

    #[test]
    fn w5h_lite_detector_accepts_simplified_english() {
        let s = "If Save APY > 1%, deposit 0.25 USDC from my wallet";
        assert!(looks_like_w5h_chat_command(s));
    }

    #[test]
    fn rejects_negative_threshold() {
        let s = "If Save APY > -1%, deposit 0.25 USDC, expires in 3 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        assert!(matches!(err, W5hParseError::NegativeThreshold { .. }));
    }

    #[test]
    fn rejects_malformed_percent() {
        let s = "If Save APY > abc%, deposit 0.25 USDC, expires in 3 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        assert!(matches!(err, W5hParseError::MalformedPercent { .. }));
    }

    #[test]
    fn rejects_wrong_pool() {
        let s = "If Marginfi USDC > 1%, deposit 0.25 USDC, expires in 3 minutes";
        let err = parse_w5h_chat_command(s).unwrap_err();
        // Pool detector requires "save" OR "solend".
        assert!(matches!(err, W5hParseError::WrongPool { .. }));
    }

    #[test]
    fn detector_distinguishes_w5h_from_w5d() {
        // W5d/W5f form lacks the expiry phrase.
        let w5d = "If Solend Main Pool USDC deposit APR is above 1%, \
                   deposit 0.25 USDC from my bounded executor wallet into Solend.";
        assert!(!looks_like_w5h_chat_command(w5d));

        // W5h adds the expiry phrase.
        let w5h = "If Save APY > 1%, deposit 0.25 USDC, expires in 3 minutes";
        assert!(looks_like_w5h_chat_command(w5h));
    }

    #[test]
    fn decimal_table() {
        assert_eq!(decimal_percent_to_bps("0").unwrap(), 0);
        assert_eq!(decimal_percent_to_bps("1").unwrap(), 100);
        assert_eq!(decimal_percent_to_bps("2.5").unwrap(), 250);
        assert_eq!(decimal_percent_to_bps("0.75").unwrap(), 75);
        assert_eq!(decimal_percent_to_bps("10.05").unwrap(), 1005);
        assert!(decimal_percent_to_bps("1.234").is_err()); // > 2 frac digits
        assert!(decimal_percent_to_bps("abc").is_err());
    }
}
