//! Phase 4C-5 — Solend signed-transaction verification + broadcast.
//!
//! # What this module owns
//!
//! One async entry point, [`submit_signed_solend_transaction`], plus
//! three narrow trait seams ([`SolendTransactionSender`],
//! [`SolendBlockHeightProvider`], [`SolendAuditSink`]) and one typed
//! outcome enum ([`SolendSubmitOutcome`]).
//!
//! The entry point is the FIRST Solend-slice code allowed to broadcast.
//! Broadcast happens only after the seven-step consume-time
//! verification pipeline (A–G below) passes. If any step fails, the
//! parked signing request is already gone (atomic consume happened in
//! step B), the obligation Keypair is dropped, a structured audit
//! event is emitted, and the outcome is a typed `Rejected` / `Expired`
//! / `NotFound` — NOT a broadcast.
//!
//! # Verification pipeline
//!
//! A. **Decode payload.** Base64 (`STANDARD`) decode the frontend-supplied
//!    `signed_tx_b64` into bytes, then `bincode::deserialize::<Transaction>`
//!    into the full legacy Transaction. Any decode / deserialize failure
//!    is `Rejected`.
//!
//! B. **Atomic consume.** `SolendSigningStore::consume(signing_request_id)`
//!    returns the parked artifact exactly once. Expiry and absence are
//!    handled inside `consume()`; the slot is gone after this call
//!    regardless of whether verification subsequently passes (prevents
//!    replay / double-submit by construction). If the consume returned
//!    `None`, we audit + return `NotFound` (never-existed OR
//!    expired-during-sweep).
//!
//! C. **Message-hash immutability.** `parked_tx.message.hash() ==
//!    signed_tx.message.hash()`. The message hash is the canonical
//!    commitment to every instruction, every account meta, every
//!    writable / signer flag, the fee payer, and the recent blockhash.
//!    A mismatch blocks instruction substitution, amount mutation,
//!    payer substitution, account-meta reshuffle, blockhash
//!    substitution, and signer-set reshuffle — all in one check.
//!
//! D. **Fee payer + signatures-length invariants.** After (C) these
//!    can't change, but we assert them for clear error labels if an
//!    attacker somehow constructs a colliding message (which would be
//!    a SHA-256 break, but we fail closed regardless).
//!
//! E. **Session-wallet signature.** Locate the payer's signer slot
//!    (the fee payer is always at `account_keys[0]` after `Message::new`
//!    with `Some(payer)`), assert the signature is non-default, run
//!    ed25519 verification against `signed_tx.message_data()`. Same
//!    primitive `verify_signed_tx` uses in [`crate::external_wallet`].
//!
//! F. **Obligation signature (if required).** For backend-partial-signed
//!    handoffs, locate the obligation's signer slot, assert the
//!    signature is **bit-equal** to the parked partial signature that
//!    was captured at handoff time, and ed25519-verify as a defense in
//!    depth. Bit-equality blocks a frontend or MITM from stripping the
//!    backend signature and replacing it with a matching-but-different
//!    one (e.g., a replay of an old valid backend signature against
//!    a different message — but that path is already blocked by (C);
//!    the bit-equal check is additional belt + suspenders).
//!
//! G. **Blockhash freshness.** Read current block height via the
//!    injected [`SolendBlockHeightProvider`]; reject if
//!    `current_block_height > parked.last_valid_block_height`. Do NOT
//!    attempt silent blockhash refresh + re-sign — retry / re-handoff
//!    belongs to a later slice.
//!
//! Only after A–G pass does this module call
//! `tx_sender.send_raw_transaction(&tx_bytes)`. The sender uses
//! `RpcSendTransactionConfig { skip_preflight: true, .. }` because the
//! tx has already been rigorously preflighted in Phase 4C-3.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT poll signature status. Confirmation polling is a
//!   follow-on concern; this slice's responsibility ends at broadcast
//!   returning a `Signature` string.
//! - Does NOT re-sign with the obligation Keypair. The backend's
//!   signature was applied at handoff time (4C-4) and is stored as
//!   `parked_obligation_signature` bytes for bit-equal comparison.
//! - Does NOT fabricate a session_wallet `Keypair`. The session wallet
//!   is external; only the user's wallet software produces its
//!   signature. This module only *verifies* that signature arrived.
//! - Does NOT mutate `tx.signatures` or any part of the signed
//!   transaction after deserialization. The signed tx is read-only
//!   between deserialize and `send_raw_transaction`.
//! - Does NOT retry broadcast. A single broadcast attempt with
//!   `max_retries = 0` is the existing `ClawRpcClient::send_raw_transaction`
//!   convention (also used by Jupiter's post-approval send path).
//! - Does NOT touch lending policy, Solend pure builders, approval
//!   store, approval routing, preflight module, or tx-plan module.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use claw_solana_core::rpc::client::ClawRpcClient;
use claw_state_store::audit::{AuditRepository, AuditSeverity};

use crate::integrations::solend_signing::{ParkedSolendSigning, SolendSigningStore};

// ── Stable audit event names ────────────────────────────────────────────────
//
// These strings are the wire-stable event_type values in AuditRepository.
// Changing them is a schema break; treat them like a database migration.
// Referenced from this module AND from solend_signing.rs handoff events.

pub const AUDIT_EVENT_HANDOFF_CREATED: &str = "solend_signing_handoff_created";
pub const AUDIT_EVENT_HANDOFF_FAILED: &str = "solend_signing_handoff_failed";
pub const AUDIT_EVENT_SIGNED_TX_RECEIVED: &str = "solend_signed_tx_received";
pub const AUDIT_EVENT_SIGNED_TX_VERIFIED: &str = "solend_signed_tx_verified";
pub const AUDIT_EVENT_SIGNED_TX_REJECTED: &str = "solend_signed_tx_rejected";
pub const AUDIT_EVENT_TRANSACTION_SUBMITTED: &str = "solend_transaction_submitted";
pub const AUDIT_EVENT_TRANSACTION_SUBMIT_FAILED: &str = "solend_transaction_submit_failed";

// ── Error surfaces ──────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SolendSubmitError {
    #[error("RPC failure: {reason}")]
    RpcFailure { reason: String },
}

// ── Trait seams ─────────────────────────────────────────────────────────────

/// Read-only broadcast surface for Solend submit. Production adapter
/// wraps [`ClawRpcClient::send_raw_transaction`] which already uses
/// `skip_preflight: true` and `max_retries: Some(0)` per repo
/// convention. Tests inject a stub.
#[async_trait]
pub trait SolendTransactionSender: Send + Sync {
    /// Send bincode-serialized legacy `Transaction` bytes. Returns the
    /// base58 signature string on success.
    async fn send_raw_transaction(&self, tx_bytes: &[u8]) -> Result<String, SolendSubmitError>;
}

/// Current block-height provider for blockhash-expiry gating. Production
/// adapter wraps [`ClawRpcClient::get_block_height`].
#[async_trait]
pub trait SolendBlockHeightProvider: Send + Sync {
    async fn current_block_height(&self) -> Result<u64, SolendSubmitError>;
}

/// Narrow audit sink for Solend signing / submit lifecycle events.
/// Production adapter wraps [`AuditRepository::append`]. Tests inject a
/// recording mock.
///
/// Event names are stable (`AUDIT_EVENT_*` constants); payloads are
/// `serde_json::Value` to match the `AuditRepository` shape exactly.
/// Correlation id is the wrapping request identifier (intent_id for
/// handoff events; signing_request_id for submit events).
#[async_trait]
pub trait SolendAuditSink: Send + Sync {
    async fn append_solend_event(
        &self,
        event_name: &'static str,
        correlation_id: String,
        session_id: Option<String>,
        severity: AuditSeverity,
        payload: serde_json::Value,
    );
}

// ── Outcome ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolendSubmitOutcome {
    Submitted {
        tx_signature: String,
        signing_request_id: Uuid,
        intent_id: Uuid,
        session_wallet: Pubkey,
        verified_slot: u64,
        last_valid_block_height: u64,
    },
    /// Parked entry not present at consume time (never existed, already
    /// consumed, or expired-and-swept).
    NotFound,
    /// Parked entry existed but TTL has passed OR the parked
    /// `last_valid_block_height` is below the chain's current height.
    Expired {
        reason: String,
    },
    /// A–F verification failed. Typed `error_type` is wire-stable; the
    /// `message` contains a short human summary (no secret material).
    Rejected {
        error_type: String,
        message: String,
    },
    /// Verification passed; broadcast itself failed (RPC-level error,
    /// network, etc.). The request is already consumed — retry requires
    /// a new handoff.
    BroadcastFailed {
        error_type: String,
        message: String,
        signing_request_id: Uuid,
    },
}

// ── Production sender / blockheight / audit adapters ────────────────────────

/// Production adapter: wraps [`ClawRpcClient`]. Uses the existing
/// `send_raw_transaction` path which is already configured with
/// `skip_preflight: true` + `max_retries: Some(0)` (see
/// `crates/solana-core/src/rpc/client.rs`).
#[derive(Clone)]
pub struct ClawRpcSolendSender {
    rpc: ClawRpcClient,
}

impl ClawRpcSolendSender {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl SolendTransactionSender for ClawRpcSolendSender {
    async fn send_raw_transaction(&self, tx_bytes: &[u8]) -> Result<String, SolendSubmitError> {
        self.rpc
            .send_raw_transaction(tx_bytes)
            .await
            .map_err(|e| SolendSubmitError::RpcFailure {
                reason: format!("send_raw_transaction (legacy, skip_preflight=true): {e}"),
            })
    }
}

/// Production adapter for block-height lookup.
#[derive(Clone)]
pub struct ClawRpcBlockHeightProvider {
    rpc: ClawRpcClient,
}

impl ClawRpcBlockHeightProvider {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl SolendBlockHeightProvider for ClawRpcBlockHeightProvider {
    async fn current_block_height(&self) -> Result<u64, SolendSubmitError> {
        self.rpc
            .get_block_height(None)
            .await
            .map_err(|e| SolendSubmitError::RpcFailure {
                reason: format!("get_block_height: {e}"),
            })
    }
}

/// Production adapter wrapping [`AuditRepository`]. Non-blocking —
/// logs (but does NOT return) any storage failure. Audit failure is
/// intentionally non-blocking for submit: we'd rather broadcast a
/// verified tx than block it on an audit write. A warning trace is
/// left so operators notice.
#[derive(Clone)]
pub struct AuditRepositorySink {
    repo: AuditRepository,
}

impl AuditRepositorySink {
    pub fn new(repo: AuditRepository) -> Self {
        Self { repo }
    }
}

#[async_trait]
impl SolendAuditSink for AuditRepositorySink {
    async fn append_solend_event(
        &self,
        event_name: &'static str,
        correlation_id: String,
        session_id: Option<String>,
        severity: AuditSeverity,
        payload: serde_json::Value,
    ) {
        if let Err(e) = self
            .repo
            .append(
                session_id.as_deref(),
                &correlation_id,
                event_name,
                "solend",
                &payload,
                severity,
            )
            .await
        {
            warn!(event = event_name, error = %e, "audit append failed for solend event");
        }
    }
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// Submit a user-signed Solend deposit transaction. See module-level
/// docs for the 7-step verification pipeline.
///
/// Post-conditions on success:
/// - The parked signing request is removed from `signing_store`.
/// - The obligation Keypair (if any) has been dropped.
/// - `solend_signed_tx_received`, `solend_signed_tx_verified`, and
///   `solend_transaction_submitted` events have been appended to the
///   audit sink, in that order.
/// - The transaction was broadcast exactly once with
///   `skip_preflight: true`.
///
/// Post-conditions on any failure path:
/// - If `consume()` returned `Some(parked)`, the parked entry is still
///   removed (atomic consume happened before verification). This is
///   intentional: a rejected or broadcast-failed signing request must
///   not be retryable without a new handoff.
/// - A `solend_signed_tx_rejected` / `solend_transaction_submit_failed`
///   event has been appended.
pub async fn submit_signed_solend_transaction(
    signing_request_id: Uuid,
    signed_tx_b64: &str,
    signing_store: &SolendSigningStore,
    tx_sender: &dyn SolendTransactionSender,
    audit: &dyn SolendAuditSink,
    block_height_provider: &dyn SolendBlockHeightProvider,
) -> SolendSubmitOutcome {
    // ── A. Decode base64 + deserialize ──────────────────────────────────────
    let signed_bytes = match base64::engine::general_purpose::STANDARD.decode(signed_tx_b64) {
        Ok(b) => b,
        Err(e) => {
            audit_rejected(
                audit,
                signing_request_id,
                None,
                None,
                "MalformedBase64",
                &format!("base64 decode: {e}"),
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "MalformedBase64".to_string(),
                message: format!("base64 decode: {e}"),
            };
        }
    };

    let signed_tx: Transaction = match bincode::deserialize(&signed_bytes) {
        Ok(t) => t,
        Err(e) => {
            audit_rejected(
                audit,
                signing_request_id,
                None,
                None,
                "MalformedTransaction",
                &format!("bincode deserialize: {e}"),
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "MalformedTransaction".to_string(),
                message: format!("bincode deserialize: {e}"),
            };
        }
    };

    // ── B. Atomic consume ────────────────────────────────────────────────────
    //
    // Atomic because `SolendSigningStore::consume` uses `DashMap::remove`
    // which is an atomic take-and-return. The slot is gone after this
    // call regardless of whether verification subsequently passes; this
    // is the replay / double-submit guarantee.
    let parked = match signing_store.consume(signing_request_id) {
        Some(p) => p,
        None => {
            // Either never existed or swept by TTL. We report
            // `NotFound` (the store treats expired as "already gone")
            // so callers have a single semantic bucket for "no longer
            // actionable."
            audit
                .append_solend_event(
                    AUDIT_EVENT_SIGNED_TX_REJECTED,
                    signing_request_id.to_string(),
                    None,
                    AuditSeverity::Warning,
                    json!({
                        "signing_request_id": signing_request_id,
                        "error_type":         "NotFound",
                        "message":            "signing request missing or expired at consume",
                    }),
                )
                .await;
            return SolendSubmitOutcome::NotFound;
        }
    };

    // Signed-tx received audit event (after we have a session_id from parked).
    audit
        .append_solend_event(
            AUDIT_EVENT_SIGNED_TX_RECEIVED,
            signing_request_id.to_string(),
            Some(parked.session_id.to_string()),
            AuditSeverity::Info,
            handoff_safe_payload(signing_request_id, &parked, None, None),
        )
        .await;

    // Deserialize parked tx for hash + signature comparisons. The
    // parked bytes are internal, always well-formed (we wrote them
    // ourselves in 4C-4) — a failure here is an internal bug and we
    // fail closed.
    let parked_tx: Transaction = match bincode::deserialize(&parked.tx_bytes) {
        Ok(t) => t,
        Err(e) => {
            audit_rejected(
                audit,
                signing_request_id,
                Some(&parked),
                None,
                "InternalParkedTxCorrupted",
                &format!("{e}"),
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "InternalParkedTxCorrupted".to_string(),
                message: format!("{e}"),
            };
        }
    };

    // ── C. Message-hash immutability ─────────────────────────────────────────
    if parked_tx.message.hash() != signed_tx.message.hash() {
        audit_rejected(
            audit,
            signing_request_id,
            Some(&parked),
            None,
            "MessageHashMismatch",
            "signed_tx.message.hash() != parked_tx.message.hash()",
        )
        .await;
        return SolendSubmitOutcome::Rejected {
            error_type: "MessageHashMismatch".to_string(),
            message: "signed tx message hash differs from parked message hash".to_string(),
        };
    }

    // ── D. Signatures-length + fee-payer invariants ─────────────────────────
    let required_sigs = signed_tx.message.header.num_required_signatures as usize;
    if signed_tx.signatures.len() != required_sigs {
        audit_rejected(
            audit,
            signing_request_id,
            Some(&parked),
            None,
            "SignaturesLengthMismatch",
            &format!(
                "signatures.len()={} != num_required_signatures={}",
                signed_tx.signatures.len(),
                required_sigs
            ),
        )
        .await;
        return SolendSubmitOutcome::Rejected {
            error_type: "SignaturesLengthMismatch".to_string(),
            message: "signatures vector length does not match header.num_required_signatures"
                .to_string(),
        };
    }
    // Fee payer must still be session_wallet (tautologically true after
    // C, but phrased for clean error taxonomy).
    if signed_tx
        .message
        .account_keys
        .first()
        .copied()
        .unwrap_or_default()
        != parked.session_wallet
    {
        audit_rejected(
            audit,
            signing_request_id,
            Some(&parked),
            None,
            "FeePayerMismatch",
            "fee payer in signed tx differs from session_wallet",
        )
        .await;
        return SolendSubmitOutcome::Rejected {
            error_type: "FeePayerMismatch".to_string(),
            message: "fee payer changed away from session_wallet".to_string(),
        };
    }

    // ── E. Session-wallet signature ──────────────────────────────────────────
    let session_idx = 0usize; // Message::new with Some(payer) => payer at account_keys[0].
    let session_sig = signed_tx.signatures[session_idx];
    if session_sig == Signature::default() {
        audit_rejected(
            audit,
            signing_request_id,
            Some(&parked),
            None,
            "SessionWalletSignatureMissing",
            "session-wallet signature slot is default",
        )
        .await;
        return SolendSubmitOutcome::Rejected {
            error_type: "SessionWalletSignatureMissing".to_string(),
            message: "session wallet signature slot is default (unsigned)".to_string(),
        };
    }
    let msg_bytes = signed_tx.message_data();
    if !session_sig.verify(parked.session_wallet.as_ref(), &msg_bytes) {
        audit_rejected(
            audit,
            signing_request_id,
            Some(&parked),
            None,
            "SessionWalletSignatureInvalid",
            "ed25519 verify(session_wallet, signed_tx.message_data()) failed",
        )
        .await;
        return SolendSubmitOutcome::Rejected {
            error_type: "SessionWalletSignatureInvalid".to_string(),
            message: "session wallet signature is not valid ed25519 over the message".to_string(),
        };
    }

    // ── F. Obligation signature (if required) ────────────────────────────────
    if parked.requires_backend_obligation_signature() {
        let obligation_pk = match parked.obligation_pubkey() {
            Some(pk) => pk,
            None => {
                // Logically unreachable: requires_backend_obligation_signature()
                // implies obligation_pubkey().is_some(). Fail closed rather
                // than panic.
                audit_rejected(
                    audit,
                    signing_request_id,
                    Some(&parked),
                    None,
                    "InternalMissingObligationPubkey",
                    "custody invariant violated",
                )
                .await;
                return SolendSubmitOutcome::Rejected {
                    error_type: "InternalMissingObligationPubkey".to_string(),
                    message: "obligation pubkey missing despite signer-required flag".to_string(),
                };
            }
        };
        let obligation_idx = match signed_tx
            .message
            .account_keys
            .iter()
            .position(|k| *k == obligation_pk)
        {
            Some(i) if i < required_sigs => i,
            Some(_) => {
                audit_rejected(
                    audit,
                    signing_request_id,
                    Some(&parked),
                    None,
                    "ObligationNotInSignerSlots",
                    "obligation pubkey present but outside signer slots",
                )
                .await;
                return SolendSubmitOutcome::Rejected {
                    error_type: "ObligationNotInSignerSlots".to_string(),
                    message: "obligation pubkey present in account_keys but not in signer slots"
                        .to_string(),
                };
            }
            None => {
                audit_rejected(
                    audit,
                    signing_request_id,
                    Some(&parked),
                    None,
                    "ObligationNotInAccountKeys",
                    "obligation pubkey missing from signed tx account_keys",
                )
                .await;
                return SolendSubmitOutcome::Rejected {
                    error_type: "ObligationNotInAccountKeys".to_string(),
                    message: "obligation pubkey missing from signed tx account_keys".to_string(),
                };
            }
        };
        let signed_obligation_sig = signed_tx.signatures[obligation_idx];
        let parked_obligation_sig = match parked.parked_obligation_signature() {
            Some(s) => *s,
            None => {
                audit_rejected(
                    audit,
                    signing_request_id,
                    Some(&parked),
                    None,
                    "InternalMissingParkedSignature",
                    "custody invariant violated",
                )
                .await;
                return SolendSubmitOutcome::Rejected {
                    error_type: "InternalMissingParkedSignature".to_string(),
                    message: "parked_obligation_signature missing despite signer-required flag"
                        .to_string(),
                };
            }
        };
        if signed_obligation_sig != parked_obligation_sig {
            audit_rejected(
                audit,
                signing_request_id,
                Some(&parked),
                None,
                "ObligationSignatureChanged",
                "obligation signature bit-unequal to parked partial signature",
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "ObligationSignatureChanged".to_string(),
                message: "obligation signature in returned tx differs from parked partial signature"
                    .to_string(),
            };
        }
        // Defense in depth: ed25519-verify the obligation signature
        // against the (already-hash-matched) message bytes.
        if !signed_obligation_sig.verify(obligation_pk.as_ref(), &msg_bytes) {
            audit_rejected(
                audit,
                signing_request_id,
                Some(&parked),
                None,
                "ObligationSignatureInvalid",
                "ed25519 verify(obligation_pk, signed_tx.message_data()) failed",
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "ObligationSignatureInvalid".to_string(),
                message: "obligation signature is not valid ed25519 over the message".to_string(),
            };
        }
    }

    // ── G. Blockhash freshness ──────────────────────────────────────────────
    let current_height = match block_height_provider.current_block_height().await {
        Ok(h) => h,
        Err(e) => {
            // Treat as fail-closed reject — we cannot prove freshness
            // without a block height.
            audit_rejected(
                audit,
                signing_request_id,
                Some(&parked),
                None,
                "BlockHeightFetchFailed",
                &format!("{e}"),
            )
            .await;
            return SolendSubmitOutcome::Rejected {
                error_type: "BlockHeightFetchFailed".to_string(),
                message: format!("{e}"),
            };
        }
    };
    if current_height > parked.last_valid_block_height {
        let msg = format!(
            "current_block_height {} > last_valid_block_height {}",
            current_height, parked.last_valid_block_height
        );
        audit
            .append_solend_event(
                AUDIT_EVENT_SIGNED_TX_REJECTED,
                signing_request_id.to_string(),
                Some(parked.session_id.to_string()),
                AuditSeverity::Warning,
                json!({
                    "signing_request_id":      signing_request_id,
                    "intent_id":               parked.intent_id,
                    "session_wallet":          parked.session_wallet.to_string(),
                    "error_type":              "BlockhashExpired",
                    "message":                 msg,
                    "last_valid_block_height": parked.last_valid_block_height,
                    "current_block_height":    current_height,
                }),
            )
            .await;
        return SolendSubmitOutcome::Expired { reason: msg };
    }

    // ── Verified. Emit the verified audit event. ────────────────────────────
    audit
        .append_solend_event(
            AUDIT_EVENT_SIGNED_TX_VERIFIED,
            signing_request_id.to_string(),
            Some(parked.session_id.to_string()),
            AuditSeverity::Info,
            handoff_safe_payload(signing_request_id, &parked, Some(current_height), None),
        )
        .await;

    info!(
        signing_request_id = %signing_request_id,
        intent_id = %parked.intent_id,
        wallet = %parked.session_wallet,
        verified_slot = parked.verified_slot,
        "solend submit: verification passed — broadcasting"
    );

    // ── Broadcast ───────────────────────────────────────────────────────────
    //
    // The sender uses skip_preflight=true (via ClawRpcClient's existing
    // config). Preflight already happened in 4C-3 under sig_verify=false
    // + replace_recent_blockhash=true. At this point we have a real
    // signed tx with a real blockhash — asking the RPC node to
    // preflight again would waste a round trip and widen the window
    // for blockhash expiry during queueing.
    match tx_sender.send_raw_transaction(&signed_bytes).await {
        Ok(signature) => {
            audit
                .append_solend_event(
                    AUDIT_EVENT_TRANSACTION_SUBMITTED,
                    signing_request_id.to_string(),
                    Some(parked.session_id.to_string()),
                    AuditSeverity::Info,
                    json!({
                        "signing_request_id":      signing_request_id,
                        "intent_id":               parked.intent_id,
                        "session_id":              parked.session_id.to_string(),
                        "session_wallet":          parked.session_wallet.to_string(),
                        "protocol":                "Solend",
                        "action":                  "Deposit",
                        "asset":                   "USDC",
                        // Deposit's `amount_raw()` is in underlying USDC
                        // base units. The submit pipeline currently only
                        // consumes deposit-flow `ParkedSolendSigning`
                        // artifacts, so `parked.action` is always Deposit
                        // here in Phase 5H-B.
                        "amount_raw":              parked.action.amount_raw(),
                        "verified_slot":           parked.verified_slot,
                        "simulation_slot":         parked.simulation_slot,
                        "last_valid_block_height": parked.last_valid_block_height,
                        "tx_signature":            signature,
                    }),
                )
                .await;
            info!(
                signing_request_id = %signing_request_id,
                intent_id = %parked.intent_id,
                tx_signature = %signature,
                "solend submit: transaction broadcast (skip_preflight=true, max_retries=0)"
            );
            SolendSubmitOutcome::Submitted {
                tx_signature: signature,
                signing_request_id,
                intent_id: parked.intent_id,
                session_wallet: parked.session_wallet,
                verified_slot: parked.verified_slot,
                last_valid_block_height: parked.last_valid_block_height,
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            audit
                .append_solend_event(
                    AUDIT_EVENT_TRANSACTION_SUBMIT_FAILED,
                    signing_request_id.to_string(),
                    Some(parked.session_id.to_string()),
                    AuditSeverity::Error,
                    json!({
                        "signing_request_id": signing_request_id,
                        "intent_id":          parked.intent_id,
                        "session_wallet":     parked.session_wallet.to_string(),
                        "error_type":         "RpcBroadcastError",
                        "message":            msg,
                    }),
                )
                .await;
            warn!(
                signing_request_id = %signing_request_id,
                error = %msg,
                "solend submit: broadcast failed after successful verification"
            );
            SolendSubmitOutcome::BroadcastFailed {
                error_type: "RpcBroadcastError".to_string(),
                message: msg,
                signing_request_id,
            }
        }
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Safe audit payload covering the fields 4C-5 requires: correlation
/// identifiers + protocol metadata + freshness slots. Never includes
/// Keypair, signature bytes, tx bytes, or any secret material.
fn handoff_safe_payload(
    signing_request_id: Uuid,
    parked: &ParkedSolendSigning,
    current_block_height: Option<u64>,
    tx_signature: Option<&str>,
) -> serde_json::Value {
    let mut payload = json!({
        "signing_request_id":      signing_request_id,
        "intent_id":               parked.intent_id,
        "session_id":              parked.session_id.to_string(),
        "session_wallet":          parked.session_wallet.to_string(),
        "protocol":                "Solend",
        "action":                  "Deposit",
        "asset":                   "USDC",
        // Deposit's `amount_raw()` is in underlying USDC base units
        // (Phase 5H-B: this code path is deposit-flow only).
        "amount_raw":              parked.action.amount_raw(),
        "verified_slot":           parked.verified_slot,
        "simulation_slot":         parked.simulation_slot,
        "last_valid_block_height": parked.last_valid_block_height,
        "obligation_signer_backend_partial": parked.requires_backend_obligation_signature(),
    });
    if let Some(h) = current_block_height {
        payload["current_block_height"] = json!(h);
    }
    if let Some(sig) = tx_signature {
        payload["tx_signature"] = json!(sig);
    }
    payload
}

/// Emit a rejected-audit event. Centralizes the payload shape so
/// every rejection path has the same field set.
async fn audit_rejected(
    audit: &dyn SolendAuditSink,
    signing_request_id: Uuid,
    parked: Option<&ParkedSolendSigning>,
    current_block_height: Option<u64>,
    error_type: &str,
    message: &str,
) {
    let session_id = parked.map(|p| p.session_id.to_string());
    let mut payload = json!({
        "signing_request_id": signing_request_id,
        "error_type":         error_type,
        "message":            message,
    });
    if let Some(p) = parked {
        payload["intent_id"] = json!(p.intent_id);
        payload["session_wallet"] = json!(p.session_wallet.to_string());
        payload["protocol"] = json!("Solend");
        payload["action"] = json!("Deposit");
        payload["asset"] = json!("USDC");
        // Deposit's `amount_raw()` is in underlying USDC base units
        // (Phase 5H-B: this audit-payload helper is deposit-flow only).
        payload["amount_raw"] = json!(p.action.amount_raw());
        payload["verified_slot"] = json!(p.verified_slot);
        payload["last_valid_block_height"] = json!(p.last_valid_block_height);
    }
    if let Some(h) = current_block_height {
        payload["current_block_height"] = json!(h);
    }
    audit
        .append_solend_event(
            AUDIT_EVENT_SIGNED_TX_REJECTED,
            signing_request_id.to_string(),
            session_id,
            AuditSeverity::Warning,
            payload,
        )
        .await;
}

// ── Module-level exports for Arc convenience ────────────────────────────────

/// Boxed sender for daemon-level injection.
pub type SolendTransactionSenderArc = Arc<dyn SolendTransactionSender>;
/// Boxed block-height provider.
pub type SolendBlockHeightProviderArc = Arc<dyn SolendBlockHeightProvider>;
/// Boxed audit sink.
pub type SolendAuditSinkArc = Arc<dyn SolendAuditSink>;

/// No-op audit sink. Useful for tests and for any code path that would
/// otherwise short-circuit audit emission for local reasons. Prefer the
/// production [`AuditRepositorySink`] in all shipping code paths.
#[derive(Clone, Default)]
pub struct NullSolendAuditSink;

#[async_trait]
impl SolendAuditSink for NullSolendAuditSink {
    async fn append_solend_event(
        &self,
        _event_name: &'static str,
        _correlation_id: String,
        _session_id: Option<String>,
        _severity: AuditSeverity,
        _payload: serde_json::Value,
    ) {
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::mapping::{
        map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, OracleAccountInfo,
        ReserveInput,
    };
    use crate::integrations::solend::raw::{decode_reserve, synth_reserve};
    use crate::integrations::solend::AssembledSolendDepositSnapshot;
    use crate::integrations::solend_park::ParkedSolendDepositIntent;
    use crate::integrations::solend_preflight::SolendPreflightOutcome;
    use crate::integrations::solend_signing::{
        create_signing_handoff, BlockhashError, RecentBlockhashProvider, SolendSigningStore,
    };
    use crate::integrations::solend_tx_plan::{
        assemble_solend_deposit_tx_plan, PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
    };
    use crate::lending::{
        ChainSlot, FeedPublishFreshness, LendingPolicyVerdict, ProposedAction, ProtocolTag,
        UnderlyingAmount,
    };
    use async_trait::async_trait;
    use claw_types::session::SessionId;
    use solana_sdk::hash::Hash;
    use solana_sdk::signature::{Keypair, Signer};
    use std::sync::Mutex;

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    // ── Stubs ──────────────────────────────────────────────────────────────

    /// Controlled blockhash source for test handoffs.
    struct StubBlockhash {
        hash: Hash,
        last_valid: u64,
    }
    impl StubBlockhash {
        fn new(b: u8, last_valid: u64) -> Self {
            Self {
                hash: Hash::new_from_array([b; 32]),
                last_valid,
            }
        }
    }
    #[async_trait]
    impl RecentBlockhashProvider for StubBlockhash {
        async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError> {
            Ok((self.hash, self.last_valid))
        }
    }

    /// Records tx_bytes passed to send; optionally returns an error.
    struct StubSender {
        calls: Mutex<Vec<Vec<u8>>>,
        inject_err: bool,
    }
    impl StubSender {
        fn ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                inject_err: false,
            }
        }
        fn erroring() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                inject_err: true,
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn last_payload(&self) -> Option<Vec<u8>> {
            self.calls.lock().unwrap().last().cloned()
        }
    }
    #[async_trait]
    impl SolendTransactionSender for StubSender {
        async fn send_raw_transaction(
            &self,
            tx_bytes: &[u8],
        ) -> Result<String, SolendSubmitError> {
            self.calls.lock().unwrap().push(tx_bytes.to_vec());
            if self.inject_err {
                Err(SolendSubmitError::RpcFailure {
                    reason: "stub rpc error".into(),
                })
            } else {
                Ok("stubSig".to_string())
            }
        }
    }

    /// Records block-height-provider calls.
    struct StubHeight {
        h: u64,
    }
    #[async_trait]
    impl SolendBlockHeightProvider for StubHeight {
        async fn current_block_height(&self) -> Result<u64, SolendSubmitError> {
            Ok(self.h)
        }
    }

    #[derive(Default)]
    struct RecordingAudit {
        events: Mutex<Vec<(String, String, Option<String>, AuditSeverity, serde_json::Value)>>,
    }
    impl RecordingAudit {
        fn events(&self) -> Vec<(String, String, Option<String>, AuditSeverity, serde_json::Value)> {
            self.events.lock().unwrap().clone()
        }
        fn event_names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.0.clone())
                .collect()
        }
    }
    #[async_trait]
    impl SolendAuditSink for RecordingAudit {
        async fn append_solend_event(
            &self,
            event_name: &'static str,
            correlation_id: String,
            session_id: Option<String>,
            severity: AuditSeverity,
            payload: serde_json::Value,
        ) {
            self.events.lock().unwrap().push((
                event_name.to_string(),
                correlation_id,
                session_id,
                severity,
                payload,
            ));
        }
    }

    // ── Fixture builders ───────────────────────────────────────────────────

    fn usdc_mint() -> Pubkey {
        Pubkey::try_from(USDC_MINT_BS58).unwrap()
    }

    fn fresh_snapshot(
        session_wallet: Pubkey,
        obligation_pubkey: Pubkey,
        obligation_exists: bool,
    ) -> AssembledSolendDepositSnapshot {
        let market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let collateral_mint = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel: Pubkey =
            "nu11111111111111111111111111111111111111111".parse().unwrap();
        let pyth_owner: Pubkey = PYTH_RECEIVER_PROGRAM_BS58.parse().unwrap();
        let supply = Pubkey::new_unique();
        let reserve_bytes = synth_reserve(
            market,
            usdc_mint(),
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            collateral_mint,
            Pubkey::new_unique(),
            100,
            false,
        );
        let reserve_raw = decode_reserve(&reserve_bytes).unwrap();
        let snapshot = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey,
            lending_market: market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(1_000),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(1_000),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(1_000)),
            }],
            snapshot_observed_slot: ChainSlot::new(1_000),
        })
        .unwrap();

        AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists,
            source_ata_exists: true,
            collateral_ata_exists: false,
            reserve_pubkey,
            obligation_pubkey,
            source_liquidity_ata: Pubkey::new_unique(),
            user_collateral_ata: Pubkey::new_unique(),
            lending_market: market,
        }
    }

    fn parked_intent(session_wallet: Pubkey) -> ParkedSolendDepositIntent {
        ParkedSolendDepositIntent {
            intent_id: Uuid::new_v4(),
            action: ProposedAction::Deposit {
                protocol: ProtocolTag::Solend,
                reserve_mint: usdc_mint(),
                amount: UnderlyingAmount::new(1_000),
            },
            snapshot: fresh_snapshot(session_wallet, Pubkey::new_unique(), false).snapshot,
            obligation_exists: false,
            source_ata_exists: true,
            collateral_ata_exists: false,
            verdict_at_propose: LendingPolicyVerdict::Pass,
            proposed_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
            session_id: SessionId::new(),
            session_wallet,
        }
    }

    fn fresh_plan(session_wallet: Pubkey, obligation_exists: bool) -> crate::integrations::solend_tx_plan::SolendDepositTxPlan {
        let obligation_pubkey = Pubkey::new_unique();
        let mut parked = parked_intent(session_wallet);
        parked.session_wallet = session_wallet;
        let assembled = fresh_snapshot(session_wallet, obligation_pubkey, obligation_exists);
        assemble_solend_deposit_tx_plan(
            Uuid::new_v4(),
            &parked,
            &assembled,
            555_555,
            PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
        )
        .unwrap()
    }

    fn passed_preflight() -> SolendPreflightOutcome {
        SolendPreflightOutcome::Passed {
            verified_slot: 555_555,
            simulation_slot: Some(555_600),
            units_consumed: Some(21_000),
            logs: vec!["Program ... consumed 21000 ...".into()],
            rent_updated_for_preflight: false,
            planning_rent_lamports: Some(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS),
            fresh_rent_lamports: Some(PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS),
            sig_verify: false,
            replace_recent_blockhash: true,
        }
    }

    /// Helper that runs the full handoff pipeline, then deserializes the
    /// parked tx, has the caller-supplied test session_wallet Keypair
    /// sign the message, and re-serializes + base64-encodes the signed
    /// transaction so submit has a valid input.
    async fn park_and_co_sign(
        session_kp: &Keypair,
        obligation_exists: bool,
    ) -> (SolendSigningStore, Uuid, Transaction, Vec<u8>) {
        let plan = fresh_plan(session_kp.pubkey(), obligation_exists);
        let bh = StubBlockhash::new(0xAB, 10_000_000);
        let store = SolendSigningStore::new();
        let summary =
            create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &NullSolendAuditSink, 600)
                .await
                .expect("handoff");
        // Peek inside via summary; the parked tx_bytes are only reachable
        // by consuming — test needs the tx_bytes without consuming, so we
        // copy via a fresh deserialize of the store's entry through a
        // second handoff. But that invalidates the blockhash. Instead
        // read via `contains` + lookup: build our own path by reading the
        // DashMap via store.inner.
        // To keep the test module self-contained, use a consumed-and-
        // re-parked pattern: consume() returns the parked struct, we
        // co-sign its tx_bytes, and we re-park a throwaway fresh handoff
        // for the same request_id so `submit` can find it.
        let request_id = summary.signing_request_id;
        let parked = store.consume(request_id).expect("parked present");
        let mut signed_tx: Transaction = bincode::deserialize(&parked.tx_bytes).unwrap();
        // Sign as session_wallet at its payer slot.
        let payer_idx = 0;
        let msg_bytes = signed_tx.message_data();
        let sig = session_kp.sign_message(&msg_bytes);
        signed_tx.signatures[payer_idx] = sig;
        let signed_bytes = bincode::serialize(&signed_tx).unwrap();
        // Re-park so the store has the same parked entry the submit
        // function will consume.
        store.park(request_id, parked);
        (store, request_id, signed_tx, signed_bytes)
    }

    // ── (A) Valid signed tx submits ────────────────────────────────────────

    #[tokio::test]
    async fn valid_signed_tx_submits_and_emits_audit_chain() {
        let session_kp = Keypair::new();
        let (store, request_id, _signed_tx, signed_bytes) = park_and_co_sign(&session_kp, false).await;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };

        let outcome =
            submit_signed_solend_transaction(request_id, &signed_b64, &store, &sender, &audit, &height)
                .await;

        match outcome {
            SolendSubmitOutcome::Submitted {
                tx_signature,
                signing_request_id,
                ..
            } => {
                assert_eq!(tx_signature, "stubSig");
                assert_eq!(signing_request_id, request_id);
            }
            other => panic!("expected Submitted, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 1);
        assert_eq!(sender.last_payload().unwrap(), signed_bytes);
        // (Q) Store consumed.
        assert!(!store.contains(&request_id));
        // (O) Audit chain: received → verified → submitted.
        let names = audit.event_names();
        assert!(names.contains(&AUDIT_EVENT_SIGNED_TX_RECEIVED.to_string()));
        assert!(names.contains(&AUDIT_EVENT_SIGNED_TX_VERIFIED.to_string()));
        assert!(names.contains(&AUDIT_EVENT_TRANSACTION_SUBMITTED.to_string()));
    }

    // ── (B) Missing signing request → NotFound, no send ───────────────────

    #[tokio::test]
    async fn missing_signing_request_returns_not_found_no_send() {
        // Need a VALID base64 + VALID bincode-serialized Transaction so
        // decode step (A) passes — consume step (B) then finds nothing.
        let empty_tx = Transaction::new_unsigned(solana_sdk::message::Message::default());
        let bytes = bincode::serialize(&empty_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let store = SolendSigningStore::new();
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome = submit_signed_solend_transaction(
            Uuid::new_v4(),
            &b64,
            &store,
            &sender,
            &audit,
            &height,
        )
        .await;
        assert!(matches!(outcome, SolendSubmitOutcome::NotFound));
        assert_eq!(sender.call_count(), 0);
        assert!(audit
            .event_names()
            .contains(&AUDIT_EVENT_SIGNED_TX_REJECTED.to_string()));
    }

    // ── (D/E) Malformed base64 / malformed tx bytes → Rejected, no send ───

    #[tokio::test]
    async fn malformed_base64_rejected_no_send() {
        let store = SolendSigningStore::new();
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome = submit_signed_solend_transaction(
            Uuid::new_v4(),
            "!!!not valid base64!!!",
            &store,
            &sender,
            &audit,
            &height,
        )
        .await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "MalformedBase64");
            }
            other => panic!("expected Rejected MalformedBase64, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
    }

    #[tokio::test]
    async fn malformed_tx_bytes_rejected_no_send() {
        let store = SolendSigningStore::new();
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        // Valid base64 of garbage (not a Transaction bincode).
        let bad = base64::engine::general_purpose::STANDARD.encode(b"\x00\x01\x02garbage");
        let outcome = submit_signed_solend_transaction(
            Uuid::new_v4(),
            &bad,
            &store,
            &sender,
            &audit,
            &height,
        )
        .await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "MalformedTransaction");
            }
            other => panic!("expected Rejected MalformedTransaction, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
    }

    // ── (F/T) Message hash mismatch → Rejected, no send ───────────────────

    #[tokio::test]
    async fn mutated_message_changes_hash_and_rejects_no_send() {
        let session_kp = Keypair::new();
        let (store, request_id, mut signed_tx, _) = park_and_co_sign(&session_kp, false).await;
        // Swap the recent_blockhash to a different non-default value.
        // Still deserializes as a valid Transaction — but `message.hash()`
        // differs from the parked message hash, triggering the (C)
        // immutability guard.
        signed_tx.message.recent_blockhash = Hash::new_from_array([0x42; 32]);
        let bytes = bincode::serialize(&signed_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };

        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "MessageHashMismatch");
            }
            other => panic!("expected MessageHashMismatch, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
        assert!(!store.contains(&request_id), "consumed even on rejection");
    }

    // ── (G) Session-wallet signature missing → Rejected ───────────────────

    #[tokio::test]
    async fn missing_session_signature_rejects_no_send() {
        let session_kp = Keypair::new();
        let (store, request_id, mut signed_tx, _) = park_and_co_sign(&session_kp, false).await;
        signed_tx.signatures[0] = Signature::default();
        let bytes = bincode::serialize(&signed_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "SessionWalletSignatureMissing");
            }
            other => panic!("expected SessionWalletSignatureMissing, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
    }

    // ── (H) Session-wallet signature invalid → Rejected ────────────────────

    #[tokio::test]
    async fn invalid_session_signature_rejects_no_send() {
        let session_kp = Keypair::new();
        let (store, request_id, mut signed_tx, _) = park_and_co_sign(&session_kp, false).await;
        // Replace with a valid-looking but wrong signature: sign a
        // DIFFERENT message with the same keypair, then stick it into
        // the payer slot of the original tx.
        let attacker_msg = b"attacker payload";
        signed_tx.signatures[0] = session_kp.sign_message(attacker_msg);
        let bytes = bincode::serialize(&signed_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "SessionWalletSignatureInvalid");
            }
            other => panic!("expected SessionWalletSignatureInvalid, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
    }

    // ── (I) Obligation signature changed → Rejected ───────────────────────

    #[tokio::test]
    async fn changed_obligation_signature_rejects_no_send() {
        let session_kp = Keypair::new();
        let (store, request_id, mut signed_tx, _) = park_and_co_sign(&session_kp, false).await;
        // Locate obligation slot and flip a byte in the signature.
        let parked_summary = store.summary(request_id).unwrap();
        assert!(parked_summary.obligation_signer_backend_partial);
        // Obligation is at index 1 in a first-deposit plan because
        // account_keys[0] is session_wallet (payer). The SDK places
        // signer accounts first; we loop to find a non-payer signer.
        let mut obligation_idx = None;
        for (i, key) in signed_tx.message.account_keys.iter().enumerate().skip(1) {
            let _ = key;
            if (i as u8) < signed_tx.message.header.num_required_signatures {
                obligation_idx = Some(i);
                break;
            }
        }
        let idx = obligation_idx.expect("obligation signer slot");
        // Mutate signature bytes.
        let mut sig_bytes = signed_tx.signatures[idx].as_ref().to_vec();
        sig_bytes[0] ^= 0xFF;
        signed_tx.signatures[idx] = Signature::try_from(sig_bytes.as_slice()).unwrap();
        let bytes = bincode::serialize(&signed_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::Rejected { error_type, .. } => {
                assert_eq!(error_type, "ObligationSignatureChanged");
            }
            other => panic!("expected ObligationSignatureChanged, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 0);
    }

    // ── (K) Existing obligation path: no backend obligation signature ──────

    #[tokio::test]
    async fn existing_obligation_path_needs_only_session_wallet_signature() {
        let session_kp = Keypair::new();
        // obligation_exists=true → no backend partial sign
        let (store, request_id, _signed_tx, signed_bytes) =
            park_and_co_sign(&session_kp, true).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::Submitted { .. } => {}
            other => panic!("expected Submitted, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 1);
    }

    // ── (N) Blockhash expired → Expired, no send ──────────────────────────

    #[tokio::test]
    async fn expired_blockhash_returns_expired_no_send() {
        let session_kp = Keypair::new();
        let (store, request_id, _, signed_bytes) = park_and_co_sign(&session_kp, false).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        // current height well beyond last_valid_block_height
        let height = StubHeight {
            h: 99_999_999_999,
        };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        assert!(matches!(outcome, SolendSubmitOutcome::Expired { .. }));
        assert_eq!(sender.call_count(), 0);
    }

    // ── (R) Consume is one-shot — double submit cannot send twice ──────────

    #[tokio::test]
    async fn consume_is_one_shot_no_double_send() {
        let session_kp = Keypair::new();
        let (store, request_id, _, signed_bytes) = park_and_co_sign(&session_kp, false).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };

        let first =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        assert!(matches!(first, SolendSubmitOutcome::Submitted { .. }));
        assert_eq!(sender.call_count(), 1);

        // Second submit — same request_id, same signed bytes.
        let second =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        assert!(matches!(second, SolendSubmitOutcome::NotFound));
        assert_eq!(sender.call_count(), 1, "no second send");
    }

    // ── (S) Failed verification leaves no keypair in store ─────────────────

    #[tokio::test]
    async fn failed_verification_leaves_no_keypair_in_store() {
        let session_kp = Keypair::new();
        let (store, request_id, mut signed_tx, _) = park_and_co_sign(&session_kp, false).await;
        signed_tx.signatures[0] = Signature::default(); // triggers rejection
        let bytes = bincode::serialize(&signed_tx).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        assert!(matches!(outcome, SolendSubmitOutcome::Rejected { .. }));
        // Store is empty — the parked entry (and its Keypair) was
        // removed by atomic consume BEFORE verification ran.
        assert_eq!(store.parked_count(), 0);
        assert!(!store.contains(&request_id));
    }

    // ── (P/Q) Audit payload contains no secret material ───────────────────

    #[tokio::test]
    async fn audit_payloads_never_contain_secret_material() {
        let session_kp = Keypair::new();
        let (store, request_id, _, signed_bytes) = park_and_co_sign(&session_kp, false).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::ok();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let _ = submit_signed_solend_transaction(
            request_id,
            &b64,
            &store,
            &sender,
            &audit,
            &height,
        )
        .await;

        for (name, _corr, _sid, _sev, payload) in audit.events() {
            let s = payload.to_string();
            assert!(!s.contains("secret"), "event {name} has `secret` string");
            assert!(!s.contains("Keypair"), "event {name} has `Keypair` string");
            assert!(!s.contains("to_bytes"), "event {name} has `to_bytes` string");
            assert!(
                !payload.get("tx_bytes").is_some(),
                "event {name} has `tx_bytes` field"
            );
            assert!(
                !payload.get("obligation_keypair").is_some(),
                "event {name} has `obligation_keypair` field"
            );
        }
    }

    // ── (U) Broadcast failure surfaces cleanly ────────────────────────────

    #[tokio::test]
    async fn broadcast_failure_returns_broadcast_failed_and_audits() {
        let session_kp = Keypair::new();
        let (store, request_id, _, signed_bytes) = park_and_co_sign(&session_kp, false).await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
        let sender = StubSender::erroring();
        let audit = RecordingAudit::default();
        let height = StubHeight { h: 100 };
        let outcome =
            submit_signed_solend_transaction(request_id, &b64, &store, &sender, &audit, &height).await;
        match outcome {
            SolendSubmitOutcome::BroadcastFailed { error_type, .. } => {
                assert_eq!(error_type, "RpcBroadcastError");
            }
            other => panic!("expected BroadcastFailed, got {other:?}"),
        }
        assert_eq!(sender.call_count(), 1, "attempted once");
        assert!(audit
            .event_names()
            .contains(&AUDIT_EVENT_TRANSACTION_SUBMIT_FAILED.to_string()));
    }

    // ── (V/W) Forbidden-term structural scan ───────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_submit.rs");
        let needles = [
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", "get_signature_", "statuses("),
            format!("{}{}", "poll_signature", "("),
            format!("{}{}", "new_signed_with_", "payer("),
            format!("{}{}", "tx.signatures", " = "),
            format!("{}{}", "tx.signatures", ".push("),
            format!("{}{}", "tx.signatures", ".pop("),
            format!("{}{}", "tx.signatures", ".clear("),
            format!("{}{}", "tx.signatures", ".remove("),
            format!("{}{}", "tx.signatures", ".insert("),
            format!("{}{}", "simulate_", "transaction("),
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "solend_submit.rs must not contain `{n}`"
            );
        }
    }
}
