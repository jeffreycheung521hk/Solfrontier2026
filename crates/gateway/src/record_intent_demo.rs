//! Stage 1 Tail Agent I — demo-mode gate for `record_intent` prefix.
//!
//! # Why this module exists
//!
//! Stage 1 Tail's tamper-evidence relies on two coordinated changes:
//!
//!   1. The `clawsol-intent` program records canonical intent metadata
//!      (hash + expires_at_slot + action_type) under a deterministic
//!      PDA when invoked via `record_intent`.
//!   2. Off-chain transaction builders (here: the Solend deposit JIT
//!      handoff) prepend that `record_intent` instruction so it lands
//!      atomically in the same transaction as the action instructions.
//!
//! Mainnet behaviour MUST NOT change until the program is deployed,
//! audited, and explicitly opted-in. This module owns the demo-mode
//! gate that enforces that invariant: production builds without the
//! env-var overlay see `RecordIntentDemoConfig::from_env()` return
//! `Ok(None)` → no `record_intent` prefix, no canonical-metadata
//! expiry gate, behaviour identical to the pre-Agent-I code path.
//!
//! # What this module does NOT do
//!
//! - Does NOT deploy the program.
//! - Does NOT verify that the recorded hash matches the action ix
//!   bytes (that's strong action-binding, Stage 2 scope).
//! - Does NOT mutate any submit / confirmation / broadcast semantics.
//! - Does NOT introduce a new RPC call or background task.

use std::str::FromStr;

use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

/// Env var that enables the Agent-I record-intent prefix on the Solend
/// deposit JIT handoff path. Default (unset / `0` / `false`) keeps the
/// pre-Agent-I behaviour byte-for-byte.
pub const ENV_RECORD_INTENT_DEMO_ENABLED: &str = "CLAW_RECORD_INTENT_DEMO_ENABLED";

/// Env var that supplies the deployed `clawsol-intent` program pubkey
/// when demo mode is enabled. Without a valid base58 pubkey here, demo
/// mode is rejected at configuration time (fail-closed) so an operator
/// cannot enable record_intent without specifying which program id to
/// invoke.
pub const ENV_RECORD_INTENT_PROGRAM_ID: &str = "CLAW_RECORD_INTENT_PROGRAM_ID";

/// Demo-mode configuration. `Some(_)` means "prepend record_intent on
/// the Solend deposit JIT handoff path"; `None` means "behave exactly
/// as the pre-Agent-I code path did".
///
/// Construction is fail-closed: invalid env values return an error.
/// The daemon is expected to log + ignore any error (treating it as
/// "demo off") OR fail startup, depending on operator policy. See
/// the daemon wiring call site for the chosen treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordIntentDemoConfig {
    /// Pubkey of the deployed `clawsol-intent` program. Stamped into
    /// the `record_intent` instruction and into the PDA derivation.
    pub program_id: Pubkey,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RecordIntentDemoConfigError {
    #[error("{ENV_RECORD_INTENT_DEMO_ENABLED}=1 but {ENV_RECORD_INTENT_PROGRAM_ID} is unset")]
    ProgramIdUnset,
    #[error("{ENV_RECORD_INTENT_PROGRAM_ID} is not a valid base58 pubkey: {0}")]
    InvalidProgramId(String),
}

impl RecordIntentDemoConfig {
    /// Construct from raw env values. Pure function so callers can
    /// inject a mock env in tests (the daemon-side wrapper reads
    /// `std::env::var(...)` and forwards through here).
    ///
    /// Returns:
    ///   - `Ok(None)`             — demo mode OFF (default). The Solend
    ///                              deposit JIT handoff behaves exactly
    ///                              as before.
    ///   - `Ok(Some(cfg))`        — demo mode ON; `record_intent` is
    ///                              prepended on the Solend deposit
    ///                              JIT handoff path.
    ///   - `Err(...)`             — demo mode requested but config is
    ///                              malformed; daemon decides whether
    ///                              to fail-startup or fall back to
    ///                              demo-off.
    pub fn from_env_strings(
        enabled: Option<&str>,
        program_id: Option<&str>,
    ) -> Result<Option<Self>, RecordIntentDemoConfigError> {
        let enabled = match enabled {
            Some(v) => {
                let t = v.trim();
                t == "1" || t.eq_ignore_ascii_case("true")
            }
            None => false,
        };
        if !enabled {
            return Ok(None);
        }
        let raw = match program_id {
            Some(v) if !v.trim().is_empty() => v.trim(),
            _ => return Err(RecordIntentDemoConfigError::ProgramIdUnset),
        };
        let pk = Pubkey::from_str(raw).map_err(|e| {
            RecordIntentDemoConfigError::InvalidProgramId(format!("{e}"))
        })?;
        Ok(Some(Self { program_id: pk }))
    }

    /// Convenience: read directly from `std::env`. Production daemon
    /// path. Returns the same shape as [`Self::from_env_strings`].
    pub fn from_env() -> Result<Option<Self>, RecordIntentDemoConfigError> {
        Self::from_env_strings(
            std::env::var(ENV_RECORD_INTENT_DEMO_ENABLED).ok().as_deref(),
            std::env::var(ENV_RECORD_INTENT_PROGRAM_ID).ok().as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_pk() -> &'static str {
        // 32 ones in base58. A real (off-curve) pubkey isn't required for
        // construction — we only verify base58 parse.
        "11111111111111111111111111111111"
    }

    #[test]
    fn unset_env_yields_disabled() {
        assert_eq!(
            RecordIntentDemoConfig::from_env_strings(None, None).unwrap(),
            None
        );
    }

    #[test]
    fn enabled_zero_yields_disabled() {
        assert_eq!(
            RecordIntentDemoConfig::from_env_strings(Some("0"), Some(valid_pk())).unwrap(),
            None
        );
    }

    #[test]
    fn enabled_false_yields_disabled() {
        assert_eq!(
            RecordIntentDemoConfig::from_env_strings(Some("false"), Some(valid_pk())).unwrap(),
            None
        );
    }

    #[test]
    fn enabled_one_with_pk_yields_some() {
        let cfg =
            RecordIntentDemoConfig::from_env_strings(Some("1"), Some(valid_pk())).unwrap();
        assert!(cfg.is_some());
        assert_eq!(cfg.unwrap().program_id, Pubkey::from_str(valid_pk()).unwrap());
    }

    #[test]
    fn enabled_true_with_pk_yields_some() {
        let cfg =
            RecordIntentDemoConfig::from_env_strings(Some("TRUE"), Some(valid_pk())).unwrap();
        assert!(cfg.is_some());
    }

    #[test]
    fn enabled_without_program_id_fails_closed() {
        match RecordIntentDemoConfig::from_env_strings(Some("1"), None) {
            Err(RecordIntentDemoConfigError::ProgramIdUnset) => {}
            other => panic!("expected ProgramIdUnset, got {other:?}"),
        }
    }

    #[test]
    fn enabled_with_empty_program_id_fails_closed() {
        match RecordIntentDemoConfig::from_env_strings(Some("1"), Some("   ")) {
            Err(RecordIntentDemoConfigError::ProgramIdUnset) => {}
            other => panic!("expected ProgramIdUnset, got {other:?}"),
        }
    }

    #[test]
    fn enabled_with_invalid_program_id_fails_closed() {
        match RecordIntentDemoConfig::from_env_strings(Some("1"), Some("not-base58!!!")) {
            Err(RecordIntentDemoConfigError::InvalidProgramId(_)) => {}
            other => panic!("expected InvalidProgramId, got {other:?}"),
        }
    }
}
