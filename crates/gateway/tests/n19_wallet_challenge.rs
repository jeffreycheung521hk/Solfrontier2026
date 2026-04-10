//! Wallet ownership proof via challenge-response — integration tests.
//!
//! Tests the WalletChallengeService end-to-end:
//! 1. valid signature → success
//! 2. wrong signature → reject
//! 3. expired challenge → reject
//! 4. reused challenge → reject
//! 5. pubkey mismatch → reject
//! 6. invalid signature format → reject

use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;

use claw_gateway::wallet_challenge::{ChallengeError, WalletChallengeService};
use claw_state_store::{
    db::Database,
    wallet_challenges::WalletChallengeRepository,
};
use claw_types::session::SessionId;
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn setup() -> (WalletChallengeService, Database) {
    let db = Database::open_in_memory().await.expect("in-memory DB");
    let repo = WalletChallengeRepository::new(db.pool().clone());
    let service = WalletChallengeService::new(repo);
    (service, db)
}

fn make_keypair() -> (SigningKey, String) {
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let signing_key = SigningKey::from_bytes(&secret);
    let verifying_key = signing_key.verifying_key();
    let pubkey_b58 = bs58::encode(verifying_key.as_bytes()).into_string();
    (signing_key, pubkey_b58)
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 1: Valid signature → success
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn valid_signature_binds_wallet() {
    let (service, _db) = setup().await;
    let (signing_key, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    // Create challenge.
    let challenge = service.create_challenge(&session_id, &pubkey).await
        .expect("create challenge should succeed");

    assert!(!challenge.challenge_id.is_empty());
    assert!(challenge.message.contains(&pubkey));
    assert!(challenge.message.contains(&session_id.to_string()));

    // Sign the message with the correct key.
    let signature = signing_key.sign(challenge.message.as_bytes());
    let sig_bytes = signature.to_bytes();

    // Verify.
    let result = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &sig_bytes,
    ).await;

    assert!(result.is_ok(), "valid signature must succeed: {result:?}");
    assert_eq!(result.unwrap(), pubkey);
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 2: Wrong signature → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn wrong_signature_rejected() {
    let (service, _db) = setup().await;
    let (_signing_key, pubkey) = make_keypair();
    let (wrong_key, _) = make_keypair(); // different key
    let session_id = SessionId::from(Uuid::new_v4());

    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    // Sign with the WRONG key.
    let bad_signature = wrong_key.sign(challenge.message.as_bytes());
    let sig_bytes = bad_signature.to_bytes();

    let result = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &sig_bytes,
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::VerificationFailed));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 3: Expired challenge → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn expired_challenge_rejected() {
    let db = Database::open_in_memory().await.unwrap();
    let repo = WalletChallengeRepository::new(db.pool().clone());
    let service = WalletChallengeService::new(repo.clone());
    let (signing_key, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    // Create a challenge, then manually set its expiry to the past.
    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    // Directly update the DB to expire it.
    sqlx::query("UPDATE wallet_bind_challenges SET expires_at = 0 WHERE id = ?")
        .bind(&challenge.challenge_id)
        .execute(db.pool())
        .await
        .unwrap();

    let signature = signing_key.sign(challenge.message.as_bytes());
    let result = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &signature.to_bytes(),
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::Expired));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 4: Reused challenge → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn reused_challenge_rejected() {
    let (service, _db) = setup().await;
    let (signing_key, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    let signature = signing_key.sign(challenge.message.as_bytes());
    let sig_bytes = signature.to_bytes();

    // First use — should succeed.
    let result1 = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &sig_bytes,
    ).await;
    assert!(result1.is_ok());

    // Second use — should fail (already consumed).
    let result2 = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &sig_bytes,
    ).await;
    assert!(result2.is_err());
    assert!(matches!(result2.unwrap_err(), ChallengeError::AlreadyUsed));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 5: Pubkey mismatch → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn pubkey_mismatch_rejected() {
    let (service, _db) = setup().await;
    let (signing_key, pubkey) = make_keypair();
    let (_, other_pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    // Create challenge for pubkey A.
    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    // Sign correctly but submit with pubkey B.
    let signature = signing_key.sign(challenge.message.as_bytes());

    let result = service.verify_challenge(
        &session_id, &challenge.challenge_id, &other_pubkey, &signature.to_bytes(),
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::PubkeyMismatch));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 6: Invalid signature format → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn invalid_signature_format_rejected() {
    let (service, _db) = setup().await;
    let (_, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    // Submit garbage bytes (wrong length).
    let result = service.verify_challenge(
        &session_id, &challenge.challenge_id, &pubkey, &[0u8; 32], // 32 bytes instead of 64
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::InvalidSignature(_)));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 7: Unknown challenge ID → not found
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn unknown_challenge_not_found() {
    let (service, _db) = setup().await;
    let (_signing_key, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    let result = service.verify_challenge(
        &session_id, "nonexistent-challenge-id", &pubkey, &[0u8; 64],
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::NotFound));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 8: Challenge message is canonical and contains required fields
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn challenge_message_is_canonical() {
    let (service, _db) = setup().await;
    let (_, pubkey) = make_keypair();
    let session_id = SessionId::from(Uuid::new_v4());

    let challenge = service.create_challenge(&session_id, &pubkey).await.unwrap();

    // Must contain the prefix, session, wallet, and nonce.
    assert!(challenge.message.starts_with("ClawSolana wallet ownership proof"));
    assert!(challenge.message.contains(&format!("Session: {session_id}")));
    assert!(challenge.message.contains(&format!("Wallet: {pubkey}")));
    assert!(challenge.message.contains("Nonce: "));
}

// ══════════════════════════════════════════════════════════════════════════════
// TEST 9: Cross-session hijack → reject
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cross_session_hijack_rejected() {
    let (service, _db) = setup().await;
    let (signing_key, pubkey) = make_keypair();
    let session_a = SessionId::from(Uuid::new_v4());
    let session_b = SessionId::from(Uuid::new_v4());

    // Create challenge on session A.
    let challenge = service.create_challenge(&session_a, &pubkey).await.unwrap();

    // Sign correctly.
    let signature = signing_key.sign(challenge.message.as_bytes());

    // Try to verify from session B — must be rejected.
    let result = service.verify_challenge(
        &session_b, &challenge.challenge_id, &pubkey, &signature.to_bytes(),
    ).await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ChallengeError::SessionMismatch));
}
