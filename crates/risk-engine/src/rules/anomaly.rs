//! Anomalous instruction pattern detection.
//!
//! Patterns that should trigger escalated review or rejection:
//! - Upgrade authority instructions (BPFLoaderUpgradeable)
//! - SetAuthority instructions (transfers ownership of accounts)
//! - Very high compute unit consumption relative to transaction value
//! - Presence of known MEV/sandwich program IDs (informational)

/// Program IDs for elevated-risk patterns.
pub const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
pub const NATIVE_STAKE_PROGRAM: &str = "Stake11111111111111111111111111111111111111";
pub const VOTE_PROGRAM: &str = "Vote111111111111111111111111111111111111111";

/// An anomaly finding from instruction analysis.
#[derive(Debug, Clone)]
pub struct AnomalyFinding {
    pub severity: AnomalySeverity,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalySeverity {
    /// Informational; may be legitimate.
    Low,
    /// Should be reviewed carefully.
    Medium,
    /// Should be blocked or require extra confirmation.
    High,
}

/// Analyzes instruction program IDs for known anomalous patterns.
pub fn detect_anomalies(program_ids: &[&str]) -> Vec<AnomalyFinding> {
    let mut findings = Vec::new();

    for program_id in program_ids {
        if *program_id == BPF_LOADER_UPGRADEABLE {
            findings.push(AnomalyFinding {
                severity: AnomalySeverity::High,
                description: "Transaction includes BPFLoaderUpgradeable instruction — \
                    this can modify on-chain programs. Extreme caution required."
                    .to_string(),
            });
        }
    }

    findings
}
