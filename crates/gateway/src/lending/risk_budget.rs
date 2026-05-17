//! Phase 6G — Solend deposit risk-budget profile.
//!
//! # Why a risk budget, not a hardcoded cap
//!
//! Pre-Phase-6G the `solend_deposit_usdc` tool's input was capped at
//! `MAX_STRUCTURAL_AMOUNT_RAW = 10_000` raw (= 0.01 USDC) — a smoke-test
//! ceiling baked into source code. The demo plan requires deposits up to
//! 5 USDC, and future profiles (`rehearsal`, `private_beta`, …) will
//! want to raise the ceiling further without rewriting the tool, the
//! chat allowlist, or the policy evaluator.
//!
//! This module owns the configurable layer: a `SolendDepositRiskBudgetConfig`
//! profile plus a precise decimal-string parser. The cap it produces is
//! threaded into [`MaxActionInputAmountConfig`][super::policy::MaxActionInputAmountConfig]
//! so the policy evaluator's `MaxActionInputAmount` rule enforces it.
//! The LLM cannot bypass this — the cap is enforced by daemon code, not
//! by the AI's chosen amount.
//!
//! # Money is integer
//!
//! `max_deposit_amount_ui` is a decimal STRING (e.g. `"5"`, `"5.000000"`,
//! `"0.001"`). [`parse_ui_decimal_to_raw`] uses pure integer arithmetic
//! via `u128` to compute the raw amount; floats are deliberately
//! avoided to eliminate `5.000001` → `5.0000009999…` rounding drift at
//! the cap boundary.

use thiserror::Error;

/// Phase 6G — operator-facing risk-budget profile for the Solend
/// deposit tool. The cap is enforced by the policy evaluator's
/// `MaxActionInputAmount` rule via
/// [`MaxActionInputAmountConfig::per_mint_caps`][super::policy::MaxActionInputAmountConfig].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendDepositRiskBudgetConfig {
    /// Free-form profile name for operator-visible labelling and audit
    /// trails. Examples: `"dev_smoke"`, `"rehearsal"`, `"demo"`,
    /// `"private_beta"`. Not consulted by code paths beyond display.
    pub profile_name: String,
    /// Maximum deposit amount in human-readable UI units (decimal
    /// string). Examples: `"5"`, `"5.000000"`, `"0.001"`. Convert to
    /// raw integer units via [`Self::max_deposit_amount_raw`].
    pub max_deposit_amount_ui: String,
    /// Asset ticker. V1 is USDC-only; future profiles may extend.
    pub asset: String,
    /// Decimal places for the asset (USDC = 6).
    pub decimals: u8,
}

impl SolendDepositRiskBudgetConfig {
    /// Stable label for the demo profile. Bake this into prompts /
    /// docs so operators know which profile is active.
    pub const DEMO_PROFILE_NAME: &'static str = "demo";

    /// Decimals for USDC (Phase 6G is USDC-only).
    pub const USDC_DECIMALS: u8 = 6;

    /// Stable label for the policy rule that enforces this cap. The
    /// tool emits this in the `policy_blocked` output's
    /// `policy_rule_name` field so callers can correlate audit rows.
    pub const POLICY_RULE_NAME: &'static str = "solend-deposit-risk-budget";

    /// Phase 6G default — demo profile capped at 5 USDC.
    pub fn demo() -> Self {
        Self {
            profile_name: Self::DEMO_PROFILE_NAME.to_string(),
            max_deposit_amount_ui: "5".to_string(),
            asset: "USDC".to_string(),
            decimals: Self::USDC_DECIMALS,
        }
    }

    /// Compute the cap in raw integer units. Pure — recompute on every
    /// call (cheap; no allocation beyond the parse).
    pub fn max_deposit_amount_raw(&self) -> Result<u64, RiskBudgetParseError> {
        parse_ui_decimal_to_raw(&self.max_deposit_amount_ui, self.decimals)
    }

    /// Return the configured cap formatted to exactly `decimals`
    /// fractional digits (e.g. `"5.000000"`). Falls back to the raw
    /// configured string if the cap is unparseable.
    pub fn max_deposit_amount_ui_padded(&self) -> String {
        match self.max_deposit_amount_raw() {
            Ok(raw) => raw_to_ui_decimal(raw, self.decimals),
            Err(_) => self.max_deposit_amount_ui.clone(),
        }
    }
}

/// Errors produced by [`parse_ui_decimal_to_raw`]. The variants are
/// stable; the policy/tool surface includes the `Display` form in
/// `error` messages, so reordering or renaming them is a wire change.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RiskBudgetParseError {
    #[error("empty amount string")]
    Empty,
    #[error("invalid character `{0}` in decimal amount")]
    InvalidCharacter(char),
    #[error("multiple decimal points in `{0}`")]
    MultipleDecimalPoints(String),
    #[error("fractional part has {got} digits but asset has only {max} decimals")]
    TooManyFractionalDigits { got: usize, max: u8 },
    #[error("overflow when computing raw amount from `{ui}` × 10^{decimals}")]
    Overflow { ui: String, decimals: u8 },
}

/// Phase 6G — parse a UI decimal string into raw integer units.
/// Pure integer arithmetic; floats are forbidden in this path.
///
/// **Accepted inputs** (with `decimals = 6`):
///   - `"0"`  →  `0`
///   - `"5"`  →  `5_000_000`
///   - `"5.0"`  →  `5_000_000`
///   - `"5.000000"`  →  `5_000_000`
///   - `"5.000001"`  →  `5_000_001`
///   - `"0.001"`  →  `1_000`
///
/// **Rejected**:
///   - empty string
///   - leading sign (`+`, `-`)
///   - non-digit characters (e.g. `"5a"`)
///   - more fractional digits than `decimals`
///   - overflow (raw exceeds `u64::MAX`)
pub fn parse_ui_decimal_to_raw(
    ui: &str,
    decimals: u8,
) -> Result<u64, RiskBudgetParseError> {
    if ui.is_empty() {
        return Err(RiskBudgetParseError::Empty);
    }
    if ui.matches('.').count() > 1 {
        return Err(RiskBudgetParseError::MultipleDecimalPoints(ui.to_string()));
    }
    let mut parts = ui.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next().unwrap_or("");

    for c in int_part.chars().chain(frac_part.chars()) {
        if !c.is_ascii_digit() {
            return Err(RiskBudgetParseError::InvalidCharacter(c));
        }
    }
    if frac_part.len() > decimals as usize {
        return Err(RiskBudgetParseError::TooManyFractionalDigits {
            got: frac_part.len(),
            max: decimals,
        });
    }

    // Pad fractional to exactly `decimals` digits with trailing zeros
    // so `int * 10^decimals + frac` works uniformly.
    let mut padded_frac = String::with_capacity(decimals as usize);
    padded_frac.push_str(frac_part);
    while padded_frac.len() < decimals as usize {
        padded_frac.push('0');
    }

    let int_value: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse::<u128>().map_err(|_| RiskBudgetParseError::Overflow {
            ui: ui.to_string(),
            decimals,
        })?
    };
    let frac_value: u128 = if padded_frac.is_empty() {
        0
    } else {
        padded_frac.parse::<u128>().map_err(|_| RiskBudgetParseError::Overflow {
            ui: ui.to_string(),
            decimals,
        })?
    };
    let factor = 10u128
        .checked_pow(decimals as u32)
        .ok_or(RiskBudgetParseError::Overflow {
            ui: ui.to_string(),
            decimals,
        })?;
    let raw_u128 = int_value
        .checked_mul(factor)
        .and_then(|v| v.checked_add(frac_value))
        .ok_or(RiskBudgetParseError::Overflow {
            ui: ui.to_string(),
            decimals,
        })?;
    if raw_u128 > u64::MAX as u128 {
        return Err(RiskBudgetParseError::Overflow {
            ui: ui.to_string(),
            decimals,
        });
    }
    Ok(raw_u128 as u64)
}

/// Phase 6G — format a raw token amount as a UI decimal string padded
/// to exactly `decimals` fractional digits. Pure integer arithmetic.
///
/// `raw_to_ui_decimal(5_000_000, 6) == "5.000000"`
/// `raw_to_ui_decimal(1_000, 6)     == "0.001000"`
/// `raw_to_ui_decimal(0, 6)         == "0.000000"`
pub fn raw_to_ui_decimal(raw: u64, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let factor = 10u128.pow(decimals as u32);
    let int_part = (raw as u128) / factor;
    let frac_part = (raw as u128) % factor;
    format!(
        "{}.{:0width$}",
        int_part,
        frac_part,
        width = decimals as usize
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zero_is_zero_raw() {
        assert_eq!(parse_ui_decimal_to_raw("0", 6).unwrap(), 0);
        assert_eq!(parse_ui_decimal_to_raw("0.0", 6).unwrap(), 0);
        assert_eq!(parse_ui_decimal_to_raw("0.000000", 6).unwrap(), 0);
    }

    #[test]
    fn parse_smoke_amounts() {
        assert_eq!(parse_ui_decimal_to_raw("0.001", 6).unwrap(), 1_000);
        assert_eq!(parse_ui_decimal_to_raw("0.01", 6).unwrap(), 10_000);
    }

    #[test]
    fn parse_demo_5_usdc() {
        assert_eq!(parse_ui_decimal_to_raw("5", 6).unwrap(), 5_000_000);
        assert_eq!(parse_ui_decimal_to_raw("5.0", 6).unwrap(), 5_000_000);
        assert_eq!(parse_ui_decimal_to_raw("5.000000", 6).unwrap(), 5_000_000);
    }

    #[test]
    fn parse_5_usdc_plus_one_micro_unit() {
        // The exact boundary case. Float arithmetic would have rounded.
        assert_eq!(parse_ui_decimal_to_raw("5.000001", 6).unwrap(), 5_000_001);
    }

    #[test]
    fn parse_20_usdc_above_demo_cap() {
        assert_eq!(parse_ui_decimal_to_raw("20", 6).unwrap(), 20_000_000);
        assert_eq!(parse_ui_decimal_to_raw("20.000000", 6).unwrap(), 20_000_000);
    }

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(
            parse_ui_decimal_to_raw("", 6).unwrap_err(),
            RiskBudgetParseError::Empty
        );
    }

    #[test]
    fn parse_rejects_non_digit() {
        match parse_ui_decimal_to_raw("5a", 6).unwrap_err() {
            RiskBudgetParseError::InvalidCharacter(c) => assert_eq!(c, 'a'),
            other => panic!("expected InvalidCharacter, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_leading_sign() {
        match parse_ui_decimal_to_raw("+5", 6).unwrap_err() {
            RiskBudgetParseError::InvalidCharacter(c) => assert_eq!(c, '+'),
            other => panic!("expected InvalidCharacter('+'), got {other:?}"),
        }
        match parse_ui_decimal_to_raw("-1", 6).unwrap_err() {
            RiskBudgetParseError::InvalidCharacter(c) => assert_eq!(c, '-'),
            other => panic!("expected InvalidCharacter('-'), got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_multiple_decimal_points() {
        match parse_ui_decimal_to_raw("5.0.0", 6).unwrap_err() {
            RiskBudgetParseError::MultipleDecimalPoints(s) => {
                assert_eq!(s, "5.0.0");
            }
            other => panic!("expected MultipleDecimalPoints, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_too_many_fractional_digits() {
        match parse_ui_decimal_to_raw("5.0000001", 6).unwrap_err() {
            RiskBudgetParseError::TooManyFractionalDigits { got, max } => {
                assert_eq!(got, 7);
                assert_eq!(max, 6);
            }
            other => panic!("expected TooManyFractionalDigits, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_overflow() {
        // u64::MAX in USDC is roughly 1.84 × 10^13 USDC ≈ 18 trillion.
        // 99_999_999_999_999 USDC × 10^6 = 9.99…×10^19 > u64::MAX.
        match parse_ui_decimal_to_raw("99999999999999", 6).unwrap_err() {
            RiskBudgetParseError::Overflow { .. } => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
    }

    #[test]
    fn raw_to_ui_round_trip() {
        let cases: &[(u64, u8, &str)] = &[
            (0, 6, "0.000000"),
            (1, 6, "0.000001"),
            (1_000, 6, "0.001000"),
            (10_000, 6, "0.010000"),
            (5_000_000, 6, "5.000000"),
            (5_000_001, 6, "5.000001"),
            (20_000_000, 6, "20.000000"),
        ];
        for (raw, dec, expected) in cases {
            assert_eq!(&raw_to_ui_decimal(*raw, *dec), expected);
        }
    }

    #[test]
    fn raw_to_ui_zero_decimals() {
        assert_eq!(raw_to_ui_decimal(42, 0), "42");
    }

    #[test]
    fn round_trip_via_parser() {
        // For every well-formed UI string, parse → format-padded → parse
        // gives the same raw value.
        let inputs = ["0", "0.001", "0.01", "5", "5.000000", "5.000001", "20"];
        for s in inputs {
            let raw = parse_ui_decimal_to_raw(s, 6).unwrap();
            let padded = raw_to_ui_decimal(raw, 6);
            let raw2 = parse_ui_decimal_to_raw(&padded, 6).unwrap();
            assert_eq!(raw, raw2, "round-trip diverged for {s}");
        }
    }

    // ── Risk-budget config tests ──────────────────────────────────────

    #[test]
    fn demo_profile_is_5_usdc() {
        let cfg = SolendDepositRiskBudgetConfig::demo();
        assert_eq!(cfg.profile_name, "demo");
        assert_eq!(cfg.max_deposit_amount_ui, "5");
        assert_eq!(cfg.asset, "USDC");
        assert_eq!(cfg.decimals, 6);
        assert_eq!(cfg.max_deposit_amount_raw().unwrap(), 5_000_000);
        assert_eq!(cfg.max_deposit_amount_ui_padded(), "5.000000");
    }

    #[test]
    fn rehearsal_profile_at_100_usdc_is_supported_without_code_change() {
        // Demonstrates that future profiles raise the cap purely via
        // config — no Rust changes required to the tool / wiring /
        // chat allowlist.
        let cfg = SolendDepositRiskBudgetConfig {
            profile_name: "rehearsal".to_string(),
            max_deposit_amount_ui: "100".to_string(),
            asset: "USDC".to_string(),
            decimals: 6,
        };
        assert_eq!(cfg.max_deposit_amount_raw().unwrap(), 100_000_000);
        assert_eq!(cfg.max_deposit_amount_ui_padded(), "100.000000");
    }

    #[test]
    fn no_float_keyword_in_module_source() {
        // Phase 6G structural guard: this module must not import or
        // use floats — money is integer. Doc-comments are stripped
        // before scanning, AND the test-module body itself is excluded
        // (otherwise the needle table here matches itself). Needles
        // are also assembled at runtime so the literal text never
        // appears in source as a contiguous match.
        const SOURCE: &str = include_str!("risk_budget.rs");
        let mut sanitized = String::with_capacity(SOURCE.len());
        let mut in_test_mod = false;
        for line in SOURCE.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("mod tests {") {
                in_test_mod = true;
                continue;
            }
            if in_test_mod {
                continue;
            }
            sanitized.push_str(line);
            sanitized.push('\n');
        }
        let f = "f";
        let needles: [String; 4] = [
            format!("{f}{}", "32"),
            format!("{f}{}", "64"),
            format!(" as {f}{}", "32"),
            format!(" as {f}{}", "64"),
        ];
        for needle in &needles {
            assert!(
                !sanitized.contains(needle.as_str()),
                "risk_budget.rs source must not contain `{needle}` outside doc/test"
            );
        }
    }
}
