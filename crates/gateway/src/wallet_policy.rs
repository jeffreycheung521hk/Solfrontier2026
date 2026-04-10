//! Per-wallet policy rules derived from wallet config.
//!
//! At daemon startup, each wallet's `[wallets.policy]` section is compiled
//! into `PolicyRule` entries and stored here. The signing tool looks up
//! the wallet pubkey and prepends these rules before evaluation.
//!
//! Evaluation order: wallet rules → session rules → global rules → defaults.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use claw_types::policy::{PolicyAction, PolicyCondition, PolicyRule};

use crate::config::WalletConfig;

/// Read-only store of per-wallet policy rules, built once at startup.
#[derive(Debug, Clone)]
pub struct WalletPolicyStore {
    /// wallet_pubkey → compiled policy rules
    rules: Arc<HashMap<String, Vec<PolicyRule>>>,
}

impl WalletPolicyStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            rules: Arc::new(HashMap::new()),
        }
    }

    /// Build from wallet config entries (external wallets only — pubkey is in config).
    pub fn from_config(wallets: &[WalletConfig]) -> Self {
        let mut rules = HashMap::new();

        for wallet in wallets {
            let pubkey = match wallet_pubkey(wallet) {
                Some(pk) => pk,
                None => continue,
            };

            let policy = match &wallet.policy {
                Some(p) => p,
                None => continue,
            };

            let mut wallet_rules: Vec<PolicyRule> = Vec::new();

            // max_amount_lamports → RequireHumanApproval above threshold
            if let Some(max_amount) = policy.max_amount_lamports {
                wallet_rules.push(PolicyRule {
                    name: format!("wallet-{}-amount-cap", &pubkey[..8.min(pubkey.len())]),
                    description: format!("Wallet {} amount cap: {} lamports", wallet.label, max_amount),
                    condition: PolicyCondition::AmountExceedsLamports(max_amount),
                    action: PolicyAction::RequireHumanApproval {
                        reason: format!("wallet '{}': amount exceeds {} lamports", wallet.label, max_amount),
                        required_approver_role: policy.required_approver_role.clone(),
                    },
                });
            }

            // program_allowlist → Reject unlisted programs
            if !policy.program_allowlist.is_empty() {
                wallet_rules.push(PolicyRule {
                    name: format!("wallet-{}-program-allowlist", &pubkey[..8.min(pubkey.len())]),
                    description: format!("Wallet {} program allowlist", wallet.label),
                    condition: PolicyCondition::ProgramNotInAllowlist,
                    action: PolicyAction::Reject {
                        reason: format!("wallet '{}': program not in wallet allowlist", wallet.label),
                    },
                });
            }

            // required_approver_role without amount cap → always require this role
            if policy.required_approver_role.is_some() && policy.max_amount_lamports.is_none() {
                wallet_rules.push(PolicyRule {
                    name: format!("wallet-{}-requires-role", &pubkey[..8.min(pubkey.len())]),
                    description: format!("Wallet {} requires specific approver", wallet.label),
                    condition: PolicyCondition::Always,
                    action: PolicyAction::RequireHumanApproval {
                        reason: format!("wallet '{}': requires designated approver", wallet.label),
                        required_approver_role: policy.required_approver_role.clone(),
                    },
                });
            }

            // Custom rules from config
            wallet_rules.extend(policy.rules.clone());

            if !wallet_rules.is_empty() {
                info!(
                    wallet = %wallet.label,
                    pubkey = %pubkey,
                    rules = wallet_rules.len(),
                    "compiled per-wallet policy rules"
                );
                rules.insert(pubkey, wallet_rules);
            }
        }

        Self {
            rules: Arc::new(rules),
        }
    }

    /// Register wallet policy rules for a loaded wallet (local or external).
    /// Called after keystore loading when the pubkey is known.
    pub fn register(&mut self, pubkey: String, wallet_cfg: &WalletConfig) {
        let policy = match &wallet_cfg.policy {
            Some(p) => p,
            None => return,
        };

        let label = &wallet_cfg.label;
        let mut wallet_rules: Vec<PolicyRule> = Vec::new();

        if let Some(max_amount) = policy.max_amount_lamports {
            wallet_rules.push(PolicyRule {
                name: format!("wallet-{}-amount-cap", &pubkey[..8.min(pubkey.len())]),
                description: format!("Wallet {} amount cap: {} lamports", label, max_amount),
                condition: PolicyCondition::AmountExceedsLamports(max_amount),
                action: PolicyAction::RequireHumanApproval {
                    reason: format!("wallet '{}': amount exceeds {} lamports", label, max_amount),
                    required_approver_role: policy.required_approver_role.clone(),
                },
            });
        }

        if !policy.program_allowlist.is_empty() {
            wallet_rules.push(PolicyRule {
                name: format!("wallet-{}-program-allowlist", &pubkey[..8.min(pubkey.len())]),
                description: format!("Wallet {} program allowlist", label),
                condition: PolicyCondition::ProgramNotInAllowlist,
                action: PolicyAction::Reject {
                    reason: format!("wallet '{}': program not in wallet allowlist", label),
                },
            });
        }

        if policy.required_approver_role.is_some() && policy.max_amount_lamports.is_none() {
            wallet_rules.push(PolicyRule {
                name: format!("wallet-{}-requires-role", &pubkey[..8.min(pubkey.len())]),
                description: format!("Wallet {} requires specific approver", label),
                condition: PolicyCondition::Always,
                action: PolicyAction::RequireHumanApproval {
                    reason: format!("wallet '{}': requires designated approver", label),
                    required_approver_role: policy.required_approver_role.clone(),
                },
            });
        }

        wallet_rules.extend(policy.rules.clone());

        if !wallet_rules.is_empty() {
            info!(
                wallet = %label,
                pubkey = %pubkey,
                rules = wallet_rules.len(),
                "registered per-wallet policy rules"
            );
            Arc::make_mut(&mut self.rules).insert(pubkey, wallet_rules);
        }
    }

    /// Returns the policy rules for a wallet, if any are configured.
    pub fn get(&self, wallet_pubkey: &str) -> Option<&[PolicyRule]> {
        self.rules.get(wallet_pubkey).map(|v| v.as_slice())
    }

    /// Returns true if any wallet has policy rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Default for WalletPolicyStore {
    fn default() -> Self {
        Self {
            rules: Arc::new(HashMap::new()),
        }
    }
}

/// Extract the effective pubkey from a wallet config.
fn wallet_pubkey(wallet: &WalletConfig) -> Option<String> {
    // External wallets have an explicit pubkey.
    if let Some(ref pk) = wallet.pubkey {
        return Some(pk.clone());
    }
    // Local keypairs: pubkey is derived at load time, not available in config.
    // For local wallets, the pubkey is known after keystore loading.
    // We'll match by label as a fallback, but pubkey matching is preferred.
    None
}
