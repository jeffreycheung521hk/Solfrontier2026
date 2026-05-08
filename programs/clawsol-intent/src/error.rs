use solana_program::program_error::ProgramError;
use thiserror::Error;

/// Custom program errors. Encoded into [`ProgramError::Custom`] using the
/// discriminant value, so each variant must keep a stable numeric position.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum IntentError {
    #[error("user (account[0]) must sign the transaction")]
    UserMustSign = 0,

    #[error("recomputed canonical hash does not match the provided hash")]
    HashMismatch = 1,

    #[error("intent expired: current slot >= expires_at_slot")]
    IntentExpired = 2,

    #[error("intent_pda account does not match the canonical PDA derivation")]
    InvalidPda = 3,

    #[error("action_type is not a recognized discriminator (1=SolendDeposit, 2=SolendWithdrawAll, 3=JupiterSwap)")]
    InvalidActionType = 4,

    #[error("canonical_intent_bytes exceeds MAX_CANONICAL_INTENT_BYTES")]
    CanonicalIntentBytesTooLarge = 5,
}

impl From<IntentError> for ProgramError {
    fn from(e: IntentError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
