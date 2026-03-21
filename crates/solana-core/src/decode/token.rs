//! SPL Token and Token-2022 account decoders.

use serde::{Deserialize, Serialize};
use solana_account_decoder::parse_token::{UiTokenAccount, UiMint};
use solana_sdk::account::Account;

use crate::errors::SolanaError;

/// Decoded SPL token mint data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintData {
    pub address: String,
    pub decimals: u8,
    pub supply: u64,
    pub is_initialized: bool,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub is_token_2022: bool,
}

/// Decoded SPL token account data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenAccountData {
    pub address: String,
    pub mint: String,
    pub owner: String,
    pub amount: u64,
    pub decimals: u8,
    /// Human-readable amount (amount / 10^decimals).
    pub ui_amount: f64,
    pub is_frozen: bool,
    pub is_native: bool,
    pub is_token_2022: bool,
}

impl TokenAccountData {
    /// The SPL Token program ID.
    pub const TOKEN_PROGRAM_ID: &'static str =
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    /// The Token-2022 program ID.
    pub const TOKEN_2022_PROGRAM_ID: &'static str =
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

    /// Returns `true` if the given program ID is a token program.
    pub fn is_token_program(program_id: &str) -> bool {
        program_id == Self::TOKEN_PROGRAM_ID || program_id == Self::TOKEN_2022_PROGRAM_ID
    }
}
