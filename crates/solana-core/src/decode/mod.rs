//! Account data decoders for known Solana programs.

pub mod system;
pub mod token;

use serde::{Deserialize, Serialize};

/// The decoded representation of a Solana account, dispatched
/// by owner program ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecodedAccount {
    /// A system program account (holds only lamports).
    System(system::SystemAccount),
    /// An SPL Token or Token-2022 mint account.
    Mint(token::MintData),
    /// An SPL Token or Token-2022 token account.
    TokenAccount(token::TokenAccountData),
    /// An account whose owner program is not in our decoder registry.
    Unknown { owner: String, data_len: usize },
}
