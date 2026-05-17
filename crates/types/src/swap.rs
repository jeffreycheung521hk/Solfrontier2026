//! Jupiter swap intent and policy types.
//!
//! A `SwapIntent` describes WHAT the user wants to swap, not HOW.
//! It is the durable artifact persisted through the approval workflow.
//! The actual transaction is built JIT after approval (Phase 2).
//!
//! `SwapPolicyConfig` defines intent-level validation rules that are
//! evaluated BEFORE entering the approval workflow. These are distinct
//! from the transaction-level policy engine — they operate on mints,
//! amounts, and slippage, not on program IDs or account destinations.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{policy::PolicyVerdict, session::SessionId, solana::SolanaNetwork};

// ── SwapIntent ─────────────────────────────────────────────────────────────

/// A Jupiter swap intent — describes the desired swap, not the transaction.
///
/// This is the unit of persistence through the approval workflow.
/// No transaction bytes, no blockhash, no signatures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapIntent {
    /// Unique identifier for this swap intent.
    pub id: Uuid,
    /// The session that submitted this intent.
    pub session_id: SessionId,
    /// The wallet that will sign the eventual swap transaction.
    pub wallet_pubkey: String,
    /// Target Solana network.
    pub network: SolanaNetwork,
    /// Input token mint address (base58).
    pub input_mint: String,
    /// Output token mint address (base58).
    pub output_mint: String,
    /// Amount of input token in smallest unit (e.g., lamports, USDC base units).
    pub input_amount: u64,
    /// Maximum slippage tolerance in basis points (e.g., 50 = 0.5%).
    pub slippage_bps: u16,
    /// Human-readable description of the swap.
    pub description: String,
    /// When this intent was created.
    pub created_at: DateTime<Utc>,
}

impl SwapIntent {
    /// Create a new swap intent with a fresh UUID and current timestamp.
    pub fn new(
        session_id: SessionId,
        wallet_pubkey: impl Into<String>,
        network: SolanaNetwork,
        input_mint: impl Into<String>,
        output_mint: impl Into<String>,
        input_amount: u64,
        slippage_bps: u16,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            wallet_pubkey: wallet_pubkey.into(),
            network,
            input_mint: input_mint.into(),
            output_mint: output_mint.into(),
            input_amount,
            slippage_bps,
            description: description.into(),
            created_at: Utc::now(),
        }
    }
}

// ── SwapPreview ────────────────────────────────────────────────────────────

/// Optional informational preview from a Jupiter quote.
///
/// Stored alongside the intent for operator review, but MUST NOT be used
/// as the final execution quote. The actual quote is fetched JIT after
/// approval (Phase 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapPreview {
    /// Expected output amount (raw units) at preview time.
    pub expected_output_amount: u64,
    /// Price impact percentage as a decimal string (e.g., "0.0012").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_impact_pct: Option<String>,
    /// DEX labels in the route (e.g., ["Orca", "Raydium"]).
    #[serde(default)]
    pub route_labels: Vec<String>,
    /// When this preview was fetched.
    pub fetched_at: DateTime<Utc>,
}

// ── SwapPolicyConfig ───────────────────────────────────────────────────────

/// Intent-level policy configuration for Jupiter swaps.
///
/// These rules are evaluated against the `SwapIntent` BEFORE the intent
/// enters the approval workflow. They operate on mints, amounts, and
/// slippage — not on program IDs or transaction structure.
///
/// All fields are optional. An empty config approves everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwapPolicyConfig {
    /// Allowed input token mints. Empty = all allowed.
    #[serde(default)]
    pub allowed_input_mints: Vec<String>,

    /// Allowed output token mints. Empty = all allowed.
    #[serde(default)]
    pub allowed_output_mints: Vec<String>,

    /// Maximum input amount (in raw token units). None = no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_amount: Option<u64>,

    /// Maximum slippage in basis points. None = no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slippage_bps: Option<u16>,

    /// Role required to approve swap intents that exceed policy.
    /// Defaults to "treasury" if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_approver_role: Option<String>,
}

impl SwapPolicyConfig {
    /// Evaluate a swap intent against this policy.
    ///
    /// Returns `PolicyVerdict::Approved` if all checks pass,
    /// `PolicyVerdict::Rejected` if a hard block is hit, or
    /// `PolicyVerdict::RequiresHumanApproval` if the intent needs review.
    pub fn evaluate(&self, intent: &SwapIntent) -> PolicyVerdict {
        // Check input mint allowlist.
        if !self.allowed_input_mints.is_empty()
            && !self.allowed_input_mints.contains(&intent.input_mint)
        {
            return PolicyVerdict::Rejected {
                reason: format!(
                    "input mint {} is not in the allowed list",
                    intent.input_mint
                ),
                rule_name: "swap-input-mint-not-allowed".to_string(),
            };
        }

        // Check output mint allowlist.
        if !self.allowed_output_mints.is_empty()
            && !self.allowed_output_mints.contains(&intent.output_mint)
        {
            return PolicyVerdict::Rejected {
                reason: format!(
                    "output mint {} is not in the allowed list",
                    intent.output_mint
                ),
                rule_name: "swap-output-mint-not-allowed".to_string(),
            };
        }

        // Check slippage cap.
        if let Some(max_bps) = self.max_slippage_bps {
            if intent.slippage_bps > max_bps {
                return PolicyVerdict::Rejected {
                    reason: format!(
                        "slippage {} bps exceeds maximum {} bps",
                        intent.slippage_bps, max_bps
                    ),
                    rule_name: "swap-slippage-exceeds-cap".to_string(),
                };
            }
        }

        // Check input amount cap → requires human approval (not a hard reject).
        if let Some(max_amount) = self.max_input_amount {
            if intent.input_amount > max_amount {
                return PolicyVerdict::RequiresHumanApproval {
                    reason: format!(
                        "input amount {} exceeds cap {}",
                        intent.input_amount, max_amount
                    ),
                    rule_name: "swap-input-amount-exceeds-cap".to_string(),
                    required_approver_role: Some(
                        self.required_approver_role
                            .clone()
                            .unwrap_or_else(|| "treasury".to_string()),
                    ),
                    approval_chain: None,
                };
            }
        }

        PolicyVerdict::Approved {
            rule_name: "swap-policy-default-approve".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
    const BONK_MINT: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";

    fn test_intent(input_mint: &str, output_mint: &str, amount: u64, slippage: u16) -> SwapIntent {
        SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            "wallet123",
            SolanaNetwork::Devnet,
            input_mint,
            output_mint,
            amount,
            slippage,
            "test swap",
        )
    }

    #[test]
    fn empty_policy_approves_everything() {
        let policy = SwapPolicyConfig::default();
        let intent = test_intent(USDC_MINT, WSOL_MINT, 1_000_000, 50);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.is_auto_approved());
    }

    #[test]
    fn input_mint_not_in_allowlist_is_rejected() {
        let policy = SwapPolicyConfig {
            allowed_input_mints: vec![USDC_MINT.to_string()],
            ..Default::default()
        };
        let intent = test_intent(BONK_MINT, WSOL_MINT, 1_000, 50);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.is_blocked());
        assert_eq!(verdict.rule_name(), Some("swap-input-mint-not-allowed"));
    }

    #[test]
    fn output_mint_not_in_allowlist_is_rejected() {
        let policy = SwapPolicyConfig {
            allowed_output_mints: vec![WSOL_MINT.to_string()],
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, BONK_MINT, 1_000, 50);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.is_blocked());
        assert_eq!(verdict.rule_name(), Some("swap-output-mint-not-allowed"));
    }

    #[test]
    fn allowed_mints_pass() {
        let policy = SwapPolicyConfig {
            allowed_input_mints: vec![USDC_MINT.to_string()],
            allowed_output_mints: vec![WSOL_MINT.to_string()],
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 1_000, 50);
        assert!(policy.evaluate(&intent).is_auto_approved());
    }

    #[test]
    fn slippage_exceeds_cap_is_rejected() {
        let policy = SwapPolicyConfig {
            max_slippage_bps: Some(100),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 1_000, 200);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.is_blocked());
        assert_eq!(verdict.rule_name(), Some("swap-slippage-exceeds-cap"));
    }

    #[test]
    fn slippage_at_cap_passes() {
        let policy = SwapPolicyConfig {
            max_slippage_bps: Some(100),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 1_000, 100);
        assert!(policy.evaluate(&intent).is_auto_approved());
    }

    #[test]
    fn amount_exceeds_cap_requires_human_approval() {
        let policy = SwapPolicyConfig {
            max_input_amount: Some(1_000_000),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 2_000_000, 50);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.requires_human());
        assert_eq!(verdict.rule_name(), Some("swap-input-amount-exceeds-cap"));
    }

    #[test]
    fn amount_at_cap_passes() {
        let policy = SwapPolicyConfig {
            max_input_amount: Some(1_000_000),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 1_000_000, 50);
        assert!(policy.evaluate(&intent).is_auto_approved());
    }

    #[test]
    fn amount_cap_uses_custom_approver_role() {
        let policy = SwapPolicyConfig {
            max_input_amount: Some(100),
            required_approver_role: Some("risk".to_string()),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 200, 50);
        let verdict = policy.evaluate(&intent);
        match &verdict {
            PolicyVerdict::RequiresHumanApproval { required_approver_role, .. } => {
                assert_eq!(required_approver_role.as_deref(), Some("risk"));
            }
            other => panic!("expected RequiresHumanApproval, got: {:?}", other),
        }
    }

    #[test]
    fn mint_check_is_evaluated_before_amount_cap() {
        // Rejected mints should block even if amount would only require approval.
        let policy = SwapPolicyConfig {
            allowed_input_mints: vec![USDC_MINT.to_string()],
            max_input_amount: Some(100),
            ..Default::default()
        };
        let intent = test_intent(BONK_MINT, WSOL_MINT, 200, 50);
        let verdict = policy.evaluate(&intent);
        // Should be hard reject (mint), not requires_human (amount).
        assert!(verdict.is_blocked());
        assert_eq!(verdict.rule_name(), Some("swap-input-mint-not-allowed"));
    }

    #[test]
    fn slippage_check_is_evaluated_before_amount_cap() {
        let policy = SwapPolicyConfig {
            max_slippage_bps: Some(50),
            max_input_amount: Some(100),
            ..Default::default()
        };
        let intent = test_intent(USDC_MINT, WSOL_MINT, 200, 100);
        let verdict = policy.evaluate(&intent);
        assert!(verdict.is_blocked());
        assert_eq!(verdict.rule_name(), Some("swap-slippage-exceeds-cap"));
    }

    #[test]
    fn swap_intent_round_trips_through_json() {
        let intent = test_intent(USDC_MINT, WSOL_MINT, 42_000_000, 50);
        let json = serde_json::to_string(&intent).unwrap();
        let parsed: SwapIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.input_mint, USDC_MINT);
        assert_eq!(parsed.output_mint, WSOL_MINT);
        assert_eq!(parsed.input_amount, 42_000_000);
        assert_eq!(parsed.slippage_bps, 50);
    }

    #[test]
    fn swap_preview_round_trips_through_json() {
        let preview = SwapPreview {
            expected_output_amount: 670_000_000,
            price_impact_pct: Some("0.0012".to_string()),
            route_labels: vec!["Orca".to_string()],
            fetched_at: Utc::now(),
        };
        let json = serde_json::to_string(&preview).unwrap();
        let parsed: SwapPreview = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.expected_output_amount, 670_000_000);
        assert_eq!(parsed.route_labels, vec!["Orca"]);
    }

    #[test]
    fn swap_policy_config_deserializes_from_toml() {
        let toml_str = r#"
allowed_input_mints = ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"]
allowed_output_mints = ["So11111111111111111111111111111111111111112"]
max_input_amount = 100000000
max_slippage_bps = 100
required_approver_role = "treasury"
"#;
        let config: SwapPolicyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.allowed_input_mints.len(), 1);
        assert_eq!(config.max_input_amount, Some(100_000_000));
        assert_eq!(config.max_slippage_bps, Some(100));
    }
}
