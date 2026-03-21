//! Wallet ownership proof via challenge-response.
//!
//! Before `bind_wallet()` accepts a pubkey, the client must prove ownership:
//!
//! 1. `POST /sessions/:id/wallet-bind-challenge` — server generates a nonce,
//!    builds a canonical message, persists the challenge with expiry.
//! 2. `POST /sessions/:id/wallet-bind-confirm` — client submits pubkey +
//!    ed25519 signature over the exact message. Server verifies, then calls
//!    the existing `bind_wallet()`.
//!
//! # Security properties
//!
//! - Nonce is cryptographically random (128 bits).
//! - Message is canonical and includes session_id + nonce to prevent replay.
//! - Challenge expires after 5 minutes.
//! - Each challenge can only be used once (mark_used is atomic).
//! - Fail-closed: any mismatch rejects the binding.

use chrono::Utc;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use tracing::{info, warn};

use claw_state_store::wallet_challenges::WalletChallengeRepository;
use claw_types::session::SessionId;

/// Default challenge TTL: 5 minutes.
const CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;

/// Canonical prefix for the challenge message.
/// Wallets display this to the user before signing.
const MESSAGE_PREFIX: &str = "ClawSolana wallet ownership proof";

/// Result of creating a challenge.
#[derive(Debug, Clone)]
pub struct ChallengeCreated {
    /// The challenge ID (for the confirm step).
    pub challenge_id: String,
    /// The canonical message the client must sign.
    pub message: String,
    /// When this challenge expires (Unix ms).
    pub expires_at: i64,
}

/// Errors from the challenge-response flow.
#[derive(Debug)]
pub enum ChallengeError {
    /// Challenge not found by ID.
    NotFound,
    /// Challenge has already been used.
    AlreadyUsed,
    /// Challenge has expired.
    Expired,
    /// The session in the confirm request doesn't match the challenge.
    SessionMismatch,
    /// The pubkey in the confirm request doesn't match the challenge.
    PubkeyMismatch,
    /// The pubkey is not a valid ed25519 public key.
    InvalidPubkey(String),
    /// The signature is not valid ed25519.
    InvalidSignature(String),
    /// The signature does not verify against the message + pubkey.
    VerificationFailed,
    /// Internal error (DB, etc.).
    Internal(String),
}

impl std::fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "challenge not found"),
            Self::AlreadyUsed => write!(f, "challenge already used"),
            Self::Expired => write!(f, "challenge expired"),
            Self::SessionMismatch => write!(f, "session does not match challenge"),
            Self::PubkeyMismatch => write!(f, "pubkey does not match challenge"),
            Self::InvalidPubkey(e) => write!(f, "invalid pubkey: {e}"),
            Self::InvalidSignature(e) => write!(f, "invalid signature: {e}"),
            Self::VerificationFailed => write!(f, "signature verification failed"),
            Self::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

/// Service that manages wallet bind challenges.
///
/// Wraps `WalletChallengeRepository` and provides the create/verify flow.
#[derive(Clone)]
pub struct WalletChallengeService {
    repo: WalletChallengeRepository,
}

impl WalletChallengeService {
    /// Create a new service.
    pub fn new(repo: WalletChallengeRepository) -> Self {
        Self { repo }
    }

    /// Generate a challenge for the given session and wallet pubkey.
    ///
    /// Returns the challenge ID and the canonical message to sign.
    pub async fn create_challenge(
        &self,
        session_id: &SessionId,
        wallet_pubkey: &str,
    ) -> Result<ChallengeCreated, ChallengeError> {
        // Generate 128-bit random nonce.
        let mut nonce_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        // Build canonical message.
        let message = format!(
            "{}\nSession: {}\nWallet: {}\nNonce: {}",
            MESSAGE_PREFIX,
            session_id,
            wallet_pubkey,
            nonce,
        );

        let expires_at = Utc::now().timestamp_millis() + CHALLENGE_TTL_MS;

        let challenge_id = self.repo.create_challenge(
            &session_id.to_string(),
            wallet_pubkey,
            &nonce,
            &message,
            expires_at,
        ).await.map_err(|e| ChallengeError::Internal(e.to_string()))?;

        info!(
            session = %session_id,
            wallet = %wallet_pubkey,
            challenge_id = %challenge_id,
            "wallet bind challenge created"
        );

        Ok(ChallengeCreated {
            challenge_id,
            message,
            expires_at,
        })
    }

    /// Verify a challenge response and return the verified wallet pubkey.
    ///
    /// On success, the challenge is marked as used and will not be accepted again.
    /// The caller should then call `bind_wallet()` with the verified pubkey.
    pub async fn verify_challenge(
        &self,
        session_id: &SessionId,
        challenge_id: &str,
        wallet_pubkey: &str,
        signature_bytes: &[u8],
    ) -> Result<String, ChallengeError> {
        // Load the challenge.
        let challenge = self.repo.get_challenge(challenge_id).await
            .map_err(|e| ChallengeError::Internal(e.to_string()))?
            .ok_or(ChallengeError::NotFound)?;

        // Check not already used.
        if challenge.used_at.is_some() {
            warn!(challenge_id = %challenge_id, "challenge already used");
            return Err(ChallengeError::AlreadyUsed);
        }

        // Check not expired.
        let now = Utc::now().timestamp_millis();
        if now > challenge.expires_at {
            warn!(challenge_id = %challenge_id, "challenge expired");
            return Err(ChallengeError::Expired);
        }

        // Check session_id matches (prevents cross-session binding hijack).
        if challenge.session_id != session_id.to_string() {
            warn!(
                challenge_id = %challenge_id,
                expected_session = %challenge.session_id,
                got_session = %session_id,
                "session mismatch — possible cross-session hijack attempt"
            );
            return Err(ChallengeError::SessionMismatch);
        }

        // Check pubkey matches.
        if challenge.wallet_pubkey != wallet_pubkey {
            warn!(
                challenge_id = %challenge_id,
                expected = %challenge.wallet_pubkey,
                got = %wallet_pubkey,
                "pubkey mismatch"
            );
            return Err(ChallengeError::PubkeyMismatch);
        }

        // Parse the pubkey as ed25519.
        let pubkey_bytes = bs58::decode(wallet_pubkey)
            .into_vec()
            .map_err(|e| ChallengeError::InvalidPubkey(format!("base58 decode: {e}")))?;

        if pubkey_bytes.len() != 32 {
            return Err(ChallengeError::InvalidPubkey(format!(
                "expected 32 bytes, got {}", pubkey_bytes.len()
            )));
        }

        let verifying_key = VerifyingKey::from_bytes(
            pubkey_bytes.as_slice().try_into().unwrap()
        ).map_err(|e| ChallengeError::InvalidPubkey(format!("ed25519: {e}")))?;

        // Parse the signature.
        if signature_bytes.len() != 64 {
            return Err(ChallengeError::InvalidSignature(format!(
                "expected 64 bytes, got {}", signature_bytes.len()
            )));
        }
        let signature = Signature::from_bytes(
            signature_bytes.try_into().unwrap()
        );

        // Verify: signature over the canonical message bytes.
        verifying_key.verify(challenge.message.as_bytes(), &signature)
            .map_err(|_| ChallengeError::VerificationFailed)?;

        // Mark as used (atomic — prevents replay).
        let marked = self.repo.mark_used(challenge_id).await
            .map_err(|e| ChallengeError::Internal(e.to_string()))?;

        if !marked {
            // Race: someone else consumed it between our check and mark.
            return Err(ChallengeError::AlreadyUsed);
        }

        info!(
            challenge_id = %challenge_id,
            wallet = %wallet_pubkey,
            "wallet ownership verified"
        );

        Ok(wallet_pubkey.to_string())
    }
}
