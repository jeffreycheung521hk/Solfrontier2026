//! System program account decoder.

use serde::{Deserialize, Serialize};

/// A decoded system account (no data, just lamports).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemAccount {
    pub pubkey: String,
    pub lamports: u64,
    pub sol: f64,
}

impl SystemAccount {
    pub fn from_account(pubkey: &str, lamports: u64) -> Self {
        Self {
            pubkey: pubkey.to_string(),
            lamports,
            sol: lamports as f64 / 1_000_000_000.0,
        }
    }
}
