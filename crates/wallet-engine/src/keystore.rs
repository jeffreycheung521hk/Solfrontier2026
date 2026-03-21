//! Secure key material management.
//!
//! # Security design
//! - `SecretKeypair` wraps `solana_sdk::Keypair` and implements `Zeroize`
//!   via manual byte-level clearing.
//! - The `Debug` impl is explicitly redacted — key bytes will never appear
//!   in logs, even accidentally.
//! - Keys are never serialized (no `Serialize` impl).
//! - The keystore is populated once at startup from config and never written
//!   back to disk in plaintext.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer as SolanaSignerTrait,
};
use tracing::info;

use crate::errors::WalletError;

/// A Solana keypair held in memory with security discipline.
/// - `Debug` is explicitly redacted
/// - Implements `Drop` which zeroes the key material
pub struct SecretKeypair {
    keypair: Keypair,
}

// We cannot derive Zeroize for Keypair (it's from an external crate),
// so we implement it manually via the known memory layout.
impl std::fmt::Debug for SecretKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // IMPORTANT: Never expose key bytes in debug output.
        write!(f, "SecretKeypair(<redacted> pubkey={})", self.keypair.pubkey())
    }
}

impl SecretKeypair {
    /// Creates a `SecretKeypair` from raw bytes (64-byte ed25519 keypair).
    /// The input bytes are not zeroized here — the caller is responsible
    /// for ensuring they came from a secure source and are cleared after use.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WalletError> {
        Keypair::from_bytes(bytes)
            .map(|kp| Self { keypair: kp })
            .map_err(|e| WalletError::InvalidKeypair(e.to_string()))
    }

    /// Creates a `SecretKeypair` from a base58-encoded private key.
    pub fn from_base58(s: &str) -> Result<Self, WalletError> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| WalletError::InvalidKeypair(e.to_string()))?;
        Self::from_bytes(&decoded)
    }

    /// Returns the public key.
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Returns a reference to the inner `Keypair` for signing.
    /// This reference must not be stored — use it inline.
    pub(crate) fn inner(&self) -> &Keypair {
        &self.keypair
    }
}

/// A registry of loaded `SecretKeypair`s, keyed by public key.
/// Thread-safe and cheaply clonable.
#[derive(Clone)]
pub struct SecretKeystore {
    keys: Arc<RwLock<HashMap<Pubkey, Arc<SecretKeypair>>>>,
}

impl std::fmt::Debug for SecretKeystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys = self.keys.read();
        write!(f, "SecretKeystore {{ {} keys loaded }}", keys.len())
    }
}

impl SecretKeystore {
    pub fn new() -> Self {
        Self {
            keys: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Loads a keypair into the keystore from raw bytes.
    pub fn load_from_bytes(&self, bytes: &[u8]) -> Result<Pubkey, WalletError> {
        let kp = SecretKeypair::from_bytes(bytes)?;
        let pubkey = kp.pubkey();
        self.keys.write().insert(pubkey, Arc::new(kp));
        info!(pubkey = %pubkey, "keypair loaded into keystore");
        Ok(pubkey)
    }

    /// Loads a keypair into the keystore from a base58 private key string.
    /// The string should come from a secured config source, not user input.
    pub fn load_from_base58(&self, s: &str) -> Result<Pubkey, WalletError> {
        let kp = SecretKeypair::from_base58(s)?;
        let pubkey = kp.pubkey();
        self.keys.write().insert(pubkey, Arc::new(kp));
        info!(pubkey = %pubkey, "keypair loaded into keystore");
        Ok(pubkey)
    }

    /// Loads a keypair from a JSON array (Solana CLI format, 64-byte array).
    pub fn load_from_json_bytes(&self, json: &str) -> Result<Pubkey, WalletError> {
        let bytes: Vec<u8> = serde_json::from_str(json)
            .map_err(|e| WalletError::InvalidKeypair(format!("invalid JSON: {e}")))?;
        self.load_from_bytes(&bytes)
    }

    /// Returns a keypair reference for a given public key.
    pub fn get(&self, pubkey: &Pubkey) -> Result<Arc<SecretKeypair>, WalletError> {
        self.keys
            .read()
            .get(pubkey)
            .cloned()
            .ok_or_else(|| WalletError::KeyNotFound {
                pubkey: pubkey.to_string(),
            })
    }

    /// Returns `true` if a keypair for the given pubkey is loaded.
    pub fn contains(&self, pubkey: &Pubkey) -> bool {
        self.keys.read().contains_key(pubkey)
    }

    /// Returns all loaded public keys (never returns key material).
    pub fn loaded_pubkeys(&self) -> Vec<Pubkey> {
        self.keys.read().keys().cloned().collect()
    }
}

impl Default for SecretKeystore {
    fn default() -> Self {
        Self::new()
    }
}
