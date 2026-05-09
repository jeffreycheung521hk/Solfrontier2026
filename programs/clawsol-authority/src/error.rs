use solana_program::program_error::ProgramError;
use thiserror::Error;

/// Custom program errors. Each variant is encoded into
/// [`ProgramError::Custom`] using the discriminant, so the numeric
/// position of every variant is part of the on-chain wire shape and
/// MUST be append-only.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AuthorityError {
    #[error("user (account[0]) must sign the transaction")]
    UserMustSign = 0,

    #[error("authorization PDA account does not match the canonical PDA derivation")]
    InvalidPda = 1,

    #[error("authorization expired: current slot >= expires_at_slot")]
    AuthorizationExpired = 2,

    #[error("max_input_amount_raw must be > 0")]
    InvalidZeroMaxAmount = 3,

    #[error("schema_version does not match the Stage 2 schema this program was built against")]
    UnsupportedSchemaVersion = 4,

    #[error("allowed_action_type is not a recognized Stage 2 discriminator (1=SolendWithdrawAllDelegated, 2=JupiterBuySolWithUsdc)")]
    InvalidActionType = 5,

    #[error("revoke signer does not match the user recorded in the authorization PDA")]
    RevokeWrongUser = 6,

    #[error("close signer does not match the user recorded in the authorization PDA")]
    CloseWrongUser = 7,

    #[error("authorization is not yet eligible to be closed (must be revoked, completed, or expired)")]
    NotCloseable = 8,
}

impl From<AuthorityError> for ProgramError {
    fn from(e: AuthorityError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
