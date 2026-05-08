//! Phase 4C-4 — Solend deposit signer handoff.
//!
//! # What this module owns
//!
//! A minimal, Solend-specific in-memory signing store plus the async
//! entry point [`create_signing_handoff`] that turns a preflight-passing
//! [`SolendDepositTxPlan`] into a pending external-wallet signature
//! request.
//!
//! # Why a Solend-specific store (instead of reusing `ExternalWalletStore.park`)
//!
//! The existing `ExternalWalletStore::park` holds a `PendingWalletSignature`
//! which fits unsigned-tx handoff fine, but has **no slot** for the
//! Solend-specific *obligation Keypair* that must stay alive across the
//! signing window (so the frontend can broadcast the same partially-signed
//! tx). Adding a `Keypair` field to `PendingWalletSignature` would pull
//! Solend-specific lifecycle concerns into a generic type consumed by
//! every signing tool. A small Solend-specific store keyed by the same
//! `request_id` keeps the obligation-keypair lifetime tightly scoped to
//! this protocol.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT broadcast. No `send_transaction`, no `send_raw_transaction`,
//!   no `send_raw_v0_transaction`, no confirmation polling.
//! - Does NOT sign as the session wallet. The session wallet is a
//!   session-bound external wallet; only the USER signs with it, via
//!   their own wallet software. The backend never holds a
//!   `session_wallet_keypair`.
//! - Does NOT use `Transaction::new_signed_with_payer`. Only
//!   `Transaction::new_unsigned` + `try_partial_sign(&[obligation], bh)`.
//! - Does NOT manually manipulate `tx.signatures`.
//! - Does NOT ask the RPC to substitute the blockhash (the preflight-only
//!   `RpcSimulateTransactionConfig` flag for that is NOT used here;
//!   signing requires a real recent blockhash).
//! - Does NOT call any Solend pure builder. It reuses plan instruction
//!   groups cloned from the caller.
//! - Does NOT persist the obligation Keypair in `SolendParkStore`,
//!   `ApprovalStore`, audit logs, tool output, or any frontend payload.
//!   The Keypair lives only in the in-memory `SolendSigningStore` and is
//!   dropped on TTL expiration, rejection, consumption, or manual
//!   removal.
//! - Does NOT mutate the input plan.
//!
//! # Partial-signing SOP (strict order — see §7 of the spec)
//!
//! 1. Replace `plan.transient_obligation_pubkey` with the new real
//!    obligation keypair's pubkey in every cloned instruction, every
//!    `AccountMeta`.
//! 2. Fetch the fresh blockhash via the injected provider.
//! 3. Compile a `Message` over the concatenated setup + refresh +
//!    deposit ixs, with `plan.session_wallet` strictly as `fee_payer`.
//! 4. Build `Transaction::new_unsigned(message)`.
//! 5. Call `tx.try_partial_sign(&[&obligation_keypair], recent_blockhash)`
//!    — this also writes the blockhash into `message.recent_blockhash`
//!    as part of signing, so the blockhash is applied exactly once.
//! 6. Serialize the partially-signed transaction with `bincode::serialize`
//!    and park the raw bytes + obligation Keypair in the store.
//!
//! **The transaction is NOT touched in any way after step 5.** Any
//! instruction, payer, blockhash, or signature mutation after
//! `try_partial_sign` is a hard violation of the SOP.
//!
//! For `plan.obligation_signer_required == false`: no Keypair is
//! generated, and step 5 is skipped. The plan instructions already list
//! the real on-chain obligation pubkey, and only the session wallet
//! needs to sign (which happens in the user's wallet, not here).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use solana_sdk::hash::Hash;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::integrations::solend_preflight::{replace_pubkey_in_instructions, SolendPreflightOutcome};
use crate::integrations::solend_submit::{
    SolendAuditSink, AUDIT_EVENT_HANDOFF_CREATED, AUDIT_EVENT_HANDOFF_FAILED,
};
use crate::integrations::solend_tx_plan::SolendDepositTxPlan;
use crate::integrations::solend_withdraw_tx_plan::SolendWithdrawTxPlan;
use crate::lending::ProposedAction;

// ── Blockhash provider seam ─────────────────────────────────────────────────

/// Errors raised by the blockhash provider. Thin; the signing layer wraps
/// these into [`SigningHandoffError`] before surfacing.
#[derive(Debug, Error)]
pub enum BlockhashError {
    #[error("blockhash fetch failed: {reason}")]
    FetchFailure { reason: String },
}

/// Async read-only blockhash source used by the signing handoff. The
/// production adapter wraps
/// [`claw_solana_core::blockhash::BlockhashManager`]; tests inject a stub.
///
/// Returns `(hash, last_valid_block_height)` so the signing request can
/// record a deterministic expiry bound.
#[async_trait]
pub trait RecentBlockhashProvider: Send + Sync {
    async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError>;
}

/// Production adapter wrapping `BlockhashManager::get_fresh`. The manager
/// caches aggressively (30s window) and uses the same `RpcPool` the rest
/// of the daemon uses.
pub struct ClawBlockhashProvider {
    manager: Arc<claw_solana_core::blockhash::BlockhashManager>,
}

impl ClawBlockhashProvider {
    pub fn new(manager: Arc<claw_solana_core::blockhash::BlockhashManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl RecentBlockhashProvider for ClawBlockhashProvider {
    async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError> {
        self.manager
            .get_fresh()
            .await
            .map_err(|e| BlockhashError::FetchFailure { reason: e.to_string() })
    }
}

// ── Error surface ───────────────────────────────────────────────────────────

/// Typed handoff-creation errors. Every variant is a fail-closed terminal
/// for the signing step — the resume task maps these into
/// `SolendResumeOutcome::SignerHandoffFailed`.
#[derive(Debug, Error)]
pub enum SigningHandoffError {
    #[error("plan inconsistency: obligation_signer_required but transient_obligation_pubkey is None")]
    MissingTransientObligation,
    #[error("plan inconsistency: transient_obligation_pubkey set but obligation_signer_required is false")]
    UnexpectedTransientObligation,
    #[error("blockhash fetch failed: {0}")]
    BlockhashFetchFailed(String),
    #[error("transaction serialization failed: {0}")]
    SerializationFailed(String),
    #[error("partial sign failed: {0}")]
    PartialSignFailed(String),
}

// ── Parked artifact + store ─────────────────────────────────────────────────

/// Data parked for a single Solend deposit signing handoff.
///
/// **Custody hardening (Phase 4C-5 W1 remediation):** the obligation
/// `Keypair` field is private. Callers that need to verify the
/// obligation signer use the narrow accessor methods below
/// ([`Self::obligation_pubkey`],
/// [`Self::parked_obligation_signature`],
/// [`Self::requires_backend_obligation_signature`]). The raw `Keypair`
/// is never exposed publicly and is dropped when the parked entry is
/// removed from [`SolendSigningStore`]. Phase 4C-5's submit path does
/// its bit-equal obligation-signature check against
/// [`Self::parked_obligation_signature`], NOT against the raw Keypair.
pub struct ParkedSolendSigning {
    pub intent_id: Uuid,
    pub session_id: SessionId,
    pub session_wallet: Pubkey,
    pub action: ProposedAction,
    pub verified_slot: u64,
    pub simulation_slot: Option<u64>,
    pub units_consumed: Option<u64>,
    /// The bincode-serialized, partially-signed (if signer required)
    /// legacy [`Transaction`] that the user's external wallet will
    /// co-sign.
    pub tx_bytes: Vec<u8>,
    pub recent_blockhash: Hash,
    pub last_valid_block_height: u64,
    /// The real obligation Keypair, alive ONLY for the signing window.
    /// `None` when `plan.obligation_signer_required == false`.
    ///
    /// **Private by design** (4C-5 W1): callers access the obligation
    /// identity via [`Self::obligation_pubkey`] and verify the backend
    /// signature via [`Self::parked_obligation_signature`]. No public
    /// accessor hands out the Keypair.
    obligation_keypair: Option<Keypair>,
    /// Bit-copy of the backend obligation signature that was written
    /// into `tx_bytes` at partial-sign time. 4C-5 verifies the
    /// user-returned tx's obligation slot is bit-equal to this value —
    /// this means submit never needs the raw Keypair for verification.
    /// `None` when no backend partial signature was produced.
    parked_obligation_signature: Option<Signature>,
    pub proposed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl ParkedSolendSigning {
    /// Public pubkey of the obligation signer, if the handoff required
    /// one. Never reveals the secret key.
    pub fn obligation_pubkey(&self) -> Option<Pubkey> {
        self.obligation_keypair.as_ref().map(|k| k.pubkey())
    }

    /// The exact signature bytes the backend partially-signed into
    /// `tx_bytes` at handoff time. 4C-5 submit verification uses this
    /// to bit-equal-compare the user-returned transaction's obligation
    /// slot, blocking frontend or MITM strip / re-sign attempts
    /// without needing access to the raw Keypair.
    pub fn parked_obligation_signature(&self) -> Option<&Signature> {
        self.parked_obligation_signature.as_ref()
    }

    /// Whether the handoff required a backend partial signature. Equal
    /// to `obligation_keypair.is_some()` but phrased for the submit
    /// path's intent-readable API surface.
    pub fn requires_backend_obligation_signature(&self) -> bool {
        self.obligation_keypair.is_some()
    }
}

/// Summary handed upward (to [`SolendResumeOutcome`]) when a signing
/// request is successfully created. No Keypair, no tx bytes — this is
/// safe to log / audit / surface to the tool-output layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningHandoffSummary {
    pub signing_request_id: Uuid,
    pub intent_id: Uuid,
    /// Session that owns this signing request. 4C-6 retrieval handlers
    /// compare the authenticated session against this value to block
    /// cross-session reads even when a request_id leaks.
    pub session_id: SessionId,
    pub session_wallet: Pubkey,
    pub verified_slot: u64,
    pub simulation_slot: Option<u64>,
    pub units_consumed: Option<u64>,
    pub obligation_signer_backend_partial: bool,
    pub last_valid_block_height: u64,
    pub expires_at: DateTime<Utc>,
}

/// In-memory Solend-specific signing store. DashMap-backed for
/// non-awaiting lock discipline (matches `SolendParkStore`).
///
/// Lazy TTL semantics: `get`/`consume` return `None` for expired
/// entries and opportunistically remove them. `cleanup_expired()`
/// sweeps on demand. **Tests exercise both the expired-on-get and
/// expired-on-signal semantics; `tests` module below proves no Keypair
/// residual remains in any of those paths.**
#[derive(Clone)]
pub struct SolendSigningStore {
    inner: Arc<DashMap<Uuid, ParkedSolendSigning>>,
}

impl SolendSigningStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Park a freshly-built signing handoff. Returns the parked entry's
    /// metadata summary (useful for immediate caller logging); the
    /// Keypair + tx_bytes remain in the store.
    pub fn park(&self, request_id: Uuid, parked: ParkedSolendSigning) -> SigningHandoffSummary {
        let summary = SigningHandoffSummary {
            signing_request_id: request_id,
            intent_id: parked.intent_id,
            session_id: parked.session_id.clone(),
            session_wallet: parked.session_wallet,
            verified_slot: parked.verified_slot,
            simulation_slot: parked.simulation_slot,
            units_consumed: parked.units_consumed,
            obligation_signer_backend_partial: parked.obligation_keypair.is_some(),
            last_valid_block_height: parked.last_valid_block_height,
            expires_at: parked.expires_at,
        };
        self.inner.insert(request_id, parked);
        summary
    }

    /// Return a read-only snapshot of the parked entry's non-secret
    /// metadata, if present AND not expired. The Keypair + tx_bytes are
    /// NOT exposed via this method — Phase 4C-5 will add a
    /// frontend-facing endpoint that serves the tx_bytes through a
    /// deliberately narrow API.
    pub fn summary(&self, request_id: Uuid) -> Option<SigningHandoffSummary> {
        let now = Utc::now();
        if self
            .inner
            .get(&request_id)
            .map(|e| e.expires_at <= now)
            .unwrap_or(true)
        {
            // Expired or missing — opportunistic sweep.
            self.inner.remove(&request_id);
            return None;
        }
        self.inner.get(&request_id).map(|e| SigningHandoffSummary {
            signing_request_id: request_id,
            intent_id: e.intent_id,
            session_id: e.session_id.clone(),
            session_wallet: e.session_wallet,
            verified_slot: e.verified_slot,
            simulation_slot: e.simulation_slot,
            units_consumed: e.units_consumed,
            obligation_signer_backend_partial: e.obligation_keypair.is_some(),
            last_valid_block_height: e.last_valid_block_height,
            expires_at: e.expires_at,
        })
    }

    /// Session-scoped retrieval. Returns `Some(summary)` only when
    /// a non-expired parked entry exists AND its `session_id` matches
    /// the caller's authenticated session. Mismatched sessions — even
    /// if the caller guessed a valid `request_id` — receive `None` and
    /// the entry is NOT swept (so the legitimate owner's window isn't
    /// disrupted by a cross-session probe).
    ///
    /// Used by the 4C-6 GET retrieval handler to serve the parked
    /// `tx_bytes` metadata to the session that originally proposed the
    /// intent.
    pub fn summary_for_session(
        &self,
        session_id: &SessionId,
        request_id: Uuid,
    ) -> Option<SigningHandoffSummary> {
        let now = Utc::now();
        let entry = self.inner.get(&request_id)?;
        if entry.expires_at <= now {
            // Expired — opportunistic sweep, then miss.
            drop(entry);
            self.inner.remove(&request_id);
            return None;
        }
        if &entry.session_id != session_id {
            return None;
        }
        Some(SigningHandoffSummary {
            signing_request_id: request_id,
            intent_id: entry.intent_id,
            session_id: entry.session_id.clone(),
            session_wallet: entry.session_wallet,
            verified_slot: entry.verified_slot,
            simulation_slot: entry.simulation_slot,
            units_consumed: entry.units_consumed,
            obligation_signer_backend_partial: entry.obligation_keypair.is_some(),
            last_valid_block_height: entry.last_valid_block_height,
            expires_at: entry.expires_at,
        })
    }

    /// Session-scoped snapshot of the raw parked `tx_bytes` plus
    /// summary metadata. Used by the 4C-6 retrieval handler to return
    /// the partially-signed transaction to the legitimate session. The
    /// obligation Keypair is NOT returned.
    ///
    /// Behaves like [`Self::summary_for_session`] for expiration and
    /// session-mismatch — misses do not leak existence to the caller.
    pub fn tx_bytes_for_session(
        &self,
        session_id: &SessionId,
        request_id: Uuid,
    ) -> Option<(SigningHandoffSummary, Vec<u8>)> {
        let now = Utc::now();
        let entry = self.inner.get(&request_id)?;
        if entry.expires_at <= now {
            drop(entry);
            self.inner.remove(&request_id);
            return None;
        }
        if &entry.session_id != session_id {
            return None;
        }
        let summary = SigningHandoffSummary {
            signing_request_id: request_id,
            intent_id: entry.intent_id,
            session_id: entry.session_id.clone(),
            session_wallet: entry.session_wallet,
            verified_slot: entry.verified_slot,
            simulation_slot: entry.simulation_slot,
            units_consumed: entry.units_consumed,
            obligation_signer_backend_partial: entry.obligation_keypair.is_some(),
            last_valid_block_height: entry.last_valid_block_height,
            expires_at: entry.expires_at,
        };
        Some((summary, entry.tx_bytes.clone()))
    }

    /// Consume a parked entry. Returns the raw partially-signed tx bytes
    /// and, if applicable, the obligation Keypair — for the 4C-5 send
    /// path to use once the user's signed bytes arrive. Removes the
    /// entry from the store atomically; subsequent calls see `None`.
    ///
    /// Expired entries are treated as already gone: returns `None` and
    /// the expired entry is removed.
    pub fn consume(&self, request_id: Uuid) -> Option<ParkedSolendSigning> {
        let now = Utc::now();
        // If expired, drop without returning.
        let is_expired = self
            .inner
            .get(&request_id)
            .map(|e| e.expires_at <= now)
            .unwrap_or(true);
        if is_expired {
            self.inner.remove(&request_id);
            return None;
        }
        self.inner.remove(&request_id).map(|(_, v)| v)
    }

    /// Explicit removal. The parked entry (and its Keypair) is dropped.
    pub fn remove(&self, request_id: &Uuid) {
        self.inner.remove(request_id);
    }

    /// Count of parked entries, including any whose `expires_at` has
    /// passed but have not yet been swept.
    pub fn parked_count(&self) -> usize {
        self.inner.len()
    }

    /// Session-scoped enumeration of all currently-parked signing
    /// handoffs for this session. Returns non-expired entries whose
    /// `session_id` matches; sweeps any encountered expired entries.
    ///
    /// Phase 4C-8 — added so callers (tests, future list endpoint) can
    /// discover the `signing_request_id` a post-approval resume task
    /// has just parked without needing to snoop on the private resume
    /// task's return value. The returned summary carries NO secret
    /// material (no Keypair, no tx bytes).
    pub fn pending_for_session(
        &self,
        session_id: &SessionId,
    ) -> Vec<SigningHandoffSummary> {
        let now = Utc::now();
        // Opportunistic expired sweep in a separate pass to avoid
        // holding a mutable iterator across removes.
        let expired: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|e| e.expires_at <= now)
            .map(|e| *e.key())
            .collect();
        for rid in expired {
            self.inner.remove(&rid);
        }
        self.inner
            .iter()
            .filter(|e| &e.session_id == session_id)
            .map(|e| SigningHandoffSummary {
                signing_request_id: *e.key(),
                intent_id: e.intent_id,
                session_id: e.session_id.clone(),
                session_wallet: e.session_wallet,
                verified_slot: e.verified_slot,
                simulation_slot: e.simulation_slot,
                units_consumed: e.units_consumed,
                obligation_signer_backend_partial: e.obligation_keypair.is_some(),
                last_valid_block_height: e.last_valid_block_height,
                expires_at: e.expires_at,
            })
            .collect()
    }

    pub fn contains(&self, request_id: &Uuid) -> bool {
        self.inner.contains_key(request_id)
    }

    /// Explicit sweep. Returns the number of entries removed.
    pub fn cleanup_expired(&self) -> usize {
        let now = Utc::now();
        let expired: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|e| e.expires_at <= now)
            .map(|e| *e.key())
            .collect();
        let removed = expired.len();
        for rid in expired {
            self.inner.remove(&rid);
        }
        removed
    }
}

impl Default for SolendSigningStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Async entry point ───────────────────────────────────────────────────────

/// Build + park a signing handoff for a preflight-passing Solend deposit
/// plan. See module-level docs for the strict partial-signing SOP.
///
/// This function is called ONLY on
/// `SolendResumeOutcome::RecheckPassedPreflighted` where the nested
/// `preflight` is `SolendPreflightOutcome::Passed`. The caller is
/// responsible for that gating; this function does not re-check the
/// preflight outcome.
///
/// `signing_lease_seconds` governs the TTL on the parked entry. Matches
/// the approval-lease pattern; production wires it from
/// `config.policy.approval_lease_seconds`.
pub async fn create_signing_handoff(
    plan: &SolendDepositTxPlan,
    preflight: &SolendPreflightOutcome,
    signing_store: &SolendSigningStore,
    blockhash_provider: &dyn RecentBlockhashProvider,
    audit: &dyn SolendAuditSink,
    signing_lease_seconds: u64,
) -> Result<SigningHandoffSummary, SigningHandoffError> {
    // ── 0. Config sanity (same shape as 4C-3 ConfigError guards) ───────────
    match (plan.obligation_signer_required, plan.transient_obligation_pubkey) {
        (true, None) => return Err(SigningHandoffError::MissingTransientObligation),
        (false, Some(_)) => return Err(SigningHandoffError::UnexpectedTransientObligation),
        _ => {}
    }

    // Pull preflight-derived fields for the summary payload.
    let (simulation_slot, units_consumed) = match preflight {
        SolendPreflightOutcome::Passed {
            simulation_slot,
            units_consumed,
            ..
        } => (*simulation_slot, *units_consumed),
        // Caller MUST gate on Passed. If somehow called with a non-Passed
        // outcome, treat as a config inconsistency rather than silently
        // proceeding. We do not produce raw Rust debug leaks.
        _ => {
            return Err(SigningHandoffError::PartialSignFailed(
                "create_signing_handoff called with non-Passed preflight".to_string(),
            ));
        }
    };

    // ── 1. Real obligation Keypair (only if required) ──────────────────────
    //
    // Held in an Option so the signer_required==false path doesn't
    // construct one. The Keypair lives in this function's scope until
    // it's moved into `ParkedSolendSigning` at park time. It is NEVER
    // serialized, cloned, or exposed outside this module.
    let obligation_keypair_opt: Option<Keypair> = if plan.obligation_signer_required {
        Some(Keypair::new())
    } else {
        None
    };

    // ── 2. Placeholder replacement ─────────────────────────────────────────
    //
    // Mirrors the 4C-3 preflight: full traversal of every instruction in
    // every group, preserving program_id / is_signer / is_writable / data.
    let (setup_ixs, refresh_ixs, deposit_ixs) =
        match (plan.transient_obligation_pubkey, obligation_keypair_opt.as_ref()) {
            (Some(placeholder), Some(keypair)) => {
                let real = keypair.pubkey();
                (
                    replace_pubkey_in_instructions(&plan.setup_instructions, placeholder, real),
                    replace_pubkey_in_instructions(&plan.refresh_instructions, placeholder, real),
                    replace_pubkey_in_instructions(&plan.deposit_instructions, placeholder, real),
                )
            }
            _ => (
                plan.setup_instructions.clone(),
                plan.refresh_instructions.clone(),
                plan.deposit_instructions.clone(),
            ),
        };

    // ── 3. Fetch real blockhash (via injected provider, NOT daemon) ─────────
    let (recent_blockhash, last_valid_block_height) =
        blockhash_provider.latest_blockhash().await.map_err(|e| {
            SigningHandoffError::BlockhashFetchFailed(format!("{e}"))
        })?;

    // ── 4. Concatenate and compile Message (session_wallet strictly payer) ──
    let mut all_ixs = Vec::with_capacity(
        setup_ixs.len() + refresh_ixs.len() + deposit_ixs.len(),
    );
    all_ixs.extend(setup_ixs.iter().cloned());
    all_ixs.extend(refresh_ixs.iter().cloned());
    all_ixs.extend(deposit_ixs.iter().cloned());
    let message = Message::new(&all_ixs, Some(&plan.session_wallet));

    // ── 5. Build unsigned transaction ───────────────────────────────────────
    let mut transaction = Transaction::new_unsigned(message);

    // ── 6. Partial-sign with the obligation Keypair if required ─────────────
    //
    // `try_partial_sign` also writes `recent_blockhash` into the message,
    // so the blockhash is applied exactly once. The session wallet's
    // signature slot remains `Signature::default()` — the user's wallet
    // will fill it via co-signing.
    //
    // From this point forward the transaction is NOT touched in any way.
    // No instruction edits, no payer edits, no blockhash edits, no
    // signature-array edits.
    // Capture the exact signature bytes the backend partial-sign
    // produced. `parked_obligation_signature` is what 4C-5 bit-equal
    // compares against the user-returned transaction's obligation slot
    // — the submit path never reads `obligation_keypair` directly.
    let parked_obligation_signature: Option<Signature>;
    if let Some(ref obligation_keypair) = obligation_keypair_opt {
        transaction
            .try_partial_sign(&[obligation_keypair], recent_blockhash)
            .map_err(|e| SigningHandoffError::PartialSignFailed(e.to_string()))?;
        // Locate the obligation pubkey in the compiled message and
        // snapshot the signature at that index. `try_partial_sign`
        // writes to the correct slot via the SDK's lookup rules.
        let obligation_pk = obligation_keypair.pubkey();
        let idx = transaction
            .message
            .account_keys
            .iter()
            .position(|k| *k == obligation_pk)
            .ok_or_else(|| {
                SigningHandoffError::PartialSignFailed(
                    "obligation pubkey missing from account_keys after partial sign".to_string(),
                )
            })?;
        parked_obligation_signature = Some(transaction.signatures[idx]);
    } else {
        // No partial signer needed; set blockhash directly on the message
        // BEFORE any downstream serialization. (This is the only branch
        // where the blockhash is set without going through
        // `try_partial_sign`.) No signature is produced.
        transaction.message.recent_blockhash = recent_blockhash;
        parked_obligation_signature = None;
    }

    // ── 7. Serialize + park ─────────────────────────────────────────────────
    //
    // Bincode-serialized `Transaction` matches the shape the rest of the
    // signing-path infrastructure uses (see `ExternalWalletStore.park`,
    // `ParkedApproval.tx_bytes`, `SignatureRequest.finalized_tx_bytes`).
    let tx_bytes = bincode::serialize(&transaction).map_err(|e| {
        SigningHandoffError::SerializationFailed(format!("bincode serialize: {e}"))
    })?;

    let signing_request_id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at = now + Duration::seconds(signing_lease_seconds as i64);

    let parked = ParkedSolendSigning {
        intent_id: plan.intent_id,
        session_id: plan.session_id.clone(),
        session_wallet: plan.session_wallet,
        action: plan.action.clone(),
        verified_slot: plan.verified_slot,
        simulation_slot,
        units_consumed,
        tx_bytes,
        recent_blockhash,
        last_valid_block_height,
        obligation_keypair: obligation_keypair_opt,
        parked_obligation_signature,
        proposed_at: now,
        expires_at,
    };

    let summary = signing_store.park(signing_request_id, parked);
    info!(
        signing_request_id = %signing_request_id,
        intent_id = %plan.intent_id,
        wallet = %plan.session_wallet,
        verified_slot = plan.verified_slot,
        obligation_partial = summary.obligation_signer_backend_partial,
        last_valid_block_height = summary.last_valid_block_height,
        "solend signing handoff parked (no broadcast)"
    );

    // Phase 4C-5 W3 remediation: emit an append-only audit event for
    // the handoff so downstream auditability matches the rigor of the
    // submit path. Payload is safe metadata only (no Keypair, no tx
    // bytes, no secret material).
    audit
        .append_solend_event(
            AUDIT_EVENT_HANDOFF_CREATED,
            signing_request_id.to_string(),
            Some(plan.session_id.to_string()),
            claw_state_store::audit::AuditSeverity::Info,
            serde_json::json!({
                "signing_request_id":               signing_request_id,
                "intent_id":                        plan.intent_id,
                "session_id":                       plan.session_id.to_string(),
                "session_wallet":                   plan.session_wallet.to_string(),
                "protocol":                         "Solend",
                "action":                           "Deposit",
                "asset":                            "USDC",
                // Deposit's `amount_raw()` is in underlying USDC base
                // units (the `Deposit` variant carries `UnderlyingAmount`).
                // Phase 5H's `Withdraw` variant carries
                // `CollateralTokenAmount`; this code path remains
                // Deposit-only because the deposit signing flow only
                // produces `SolendDepositTxPlan` artifacts.
                "amount_raw":                       plan.action.amount_raw(),
                "verified_slot":                    plan.verified_slot,
                "simulation_slot":                  summary.simulation_slot,
                "last_valid_block_height":          summary.last_valid_block_height,
                "obligation_signer_backend_partial": summary.obligation_signer_backend_partial,
            }),
        )
        .await;

    Ok(summary)
}

// ── Phase 6I-F — Solend WITHDRAW signing handoff ─────────────────────────────

/// Phase 6I-F — turn a freshly-assembled [`SolendWithdrawTxPlan`] into
/// a parked [`ParkedSolendSigning`] entry.
///
/// Withdraw differs from deposit in three load-bearing ways, all handled
/// here:
///
///   1. **No obligation `Keypair`.** Withdraw spends an existing
///      obligation, so `plan.obligation_signer_required` is always
///      `false` and `plan.transient_obligation_pubkey` is always
///      `None`. We assert the contract and skip the partial-sign step
///      entirely. The parked entry's `obligation_keypair` is `None`;
///      `parked_obligation_signature` is `None`. The submit verifier
///      treats both as "no backend partial signature expected" and
///      relies solely on the message-hash bit-equal check + the
///      session-wallet fee-payer invariant — exactly what the
///      deposit's `obligation_signer_backend_partial = false` path
///      already does.
///   2. **No preflight outcome required.** Phase 6I-E does not run
///      structural preflight on withdraw; the `simulation_slot` /
///      `units_consumed` fields on the parked entry are recorded as
///      `None`. The submit verifier does not require these.
///   3. **No transient-obligation-pubkey replacement.** Withdraw plans
///      never embed a placeholder, so the message can be compiled
///      directly from the cloned instruction groups.
///
/// Everything else mirrors `create_signing_handoff` exactly: fresh
/// blockhash via the injected provider, session-wallet as strict fee
/// payer, message compiled once, transaction serialized once,
/// bincode-parked under a fresh `signing_request_id`. The same
/// [`SolendSigningStore`] is reused — the submit pipeline
/// ([`crate::integrations::solend_submit::submit_signed_solend_transaction`])
/// is action-agnostic at the bytes level and accepts the parked
/// entry without further changes.
///
/// Audit emits `solend_handoff_created` with `action: "Withdraw"` so
/// the same dashboards that watch deposit handoffs can disambiguate.
pub async fn create_withdraw_signing_handoff(
    plan: &SolendWithdrawTxPlan,
    signing_store: &SolendSigningStore,
    blockhash_provider: &dyn RecentBlockhashProvider,
    audit: &dyn SolendAuditSink,
    signing_lease_seconds: u64,
) -> Result<SigningHandoffSummary, SigningHandoffError> {
    // ── 0. Plan-shape contract for withdraw ─────────────────────────────────
    if plan.obligation_signer_required {
        return Err(SigningHandoffError::UnexpectedTransientObligation);
    }
    if plan.transient_obligation_pubkey.is_some() {
        return Err(SigningHandoffError::UnexpectedTransientObligation);
    }

    // ── 1. Fresh blockhash ──────────────────────────────────────────────────
    let (recent_blockhash, last_valid_block_height) =
        blockhash_provider.latest_blockhash().await.map_err(|e| {
            SigningHandoffError::BlockhashFetchFailed(format!("{e}"))
        })?;

    // ── 2. Concatenate instruction groups (setup → refresh → withdraw) ──────
    let mut all_ixs = Vec::with_capacity(
        plan.setup_instructions.len()
            + plan.refresh_instructions.len()
            + plan.withdraw_instructions.len(),
    );
    all_ixs.extend(plan.setup_instructions.iter().cloned());
    all_ixs.extend(plan.refresh_instructions.iter().cloned());
    all_ixs.extend(plan.withdraw_instructions.iter().cloned());

    // ── 3. Compile message — session_wallet strictly as fee payer ───────────
    let message = Message::new(&all_ixs, Some(&plan.session_wallet));

    // ── 4. Unsigned transaction with the fresh blockhash applied ────────────
    let mut transaction = Transaction::new_unsigned(message);
    transaction.message.recent_blockhash = recent_blockhash;

    // ── 5. Serialize ─────────────────────────────────────────────────────────
    let tx_bytes = bincode::serialize(&transaction).map_err(|e| {
        SigningHandoffError::SerializationFailed(format!("bincode serialize: {e}"))
    })?;

    let signing_request_id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at = now + Duration::seconds(signing_lease_seconds as i64);

    let parked = ParkedSolendSigning {
        intent_id: plan.intent_id,
        session_id: plan.session_id.clone(),
        session_wallet: plan.session_wallet,
        action: plan.action.clone(),
        verified_slot: plan.verified_slot,
        simulation_slot: None,
        units_consumed: None,
        tx_bytes,
        recent_blockhash,
        last_valid_block_height,
        // Withdraw never holds a backend obligation Keypair.
        obligation_keypair: None,
        parked_obligation_signature: None,
        proposed_at: now,
        expires_at,
    };

    let summary = signing_store.park(signing_request_id, parked);
    info!(
        signing_request_id = %signing_request_id,
        intent_id = %plan.intent_id,
        wallet = %plan.session_wallet,
        verified_slot = plan.verified_slot,
        last_valid_block_height = summary.last_valid_block_height,
        "solend WITHDRAW signing handoff parked (no broadcast)"
    );

    audit
        .append_solend_event(
            AUDIT_EVENT_HANDOFF_CREATED,
            signing_request_id.to_string(),
            Some(plan.session_id.to_string()),
            claw_state_store::audit::AuditSeverity::Info,
            serde_json::json!({
                "signing_request_id":               signing_request_id,
                "intent_id":                        plan.intent_id,
                "session_id":                       plan.session_id.to_string(),
                "session_wallet":                   plan.session_wallet.to_string(),
                "protocol":                         "Solend",
                "action":                           "Withdraw",
                "asset":                            "USDC",
                // For Withdraw, `amount_raw()` is the cToken collateral
                // amount (typically `u64::MAX` sentinel for withdraw-all).
                "collateral_amount_raw":            plan.action.amount_raw(),
                "obligation_pubkey":                plan
                    .accounts_summary
                    .obligation_pubkey
                    .to_string(),
                "reserve_pubkey":                   plan
                    .accounts_summary
                    .reserve_pubkey
                    .to_string(),
                "verified_slot":                    plan.verified_slot,
                "last_valid_block_height":          summary.last_valid_block_height,
                "obligation_signer_backend_partial": false,
            }),
        )
        .await;

    Ok(summary)
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
    use crate::integrations::solend_tx_plan::{
        assemble_solend_deposit_tx_plan, PHASE_4C2_OBLIGATION_RENT_EXEMPT_LAMPORTS,
    };
    use crate::integrations::solend_park::ParkedSolendDepositIntent;
    use crate::lending::{
        ChainSlot, FeedPublishFreshness, LendingPolicyVerdict, ProposedAction, ProtocolTag,
        UnderlyingAmount,
    };
    use std::sync::Mutex;

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    // ── Stubs ──────────────────────────────────────────────────────────────

    struct StubBlockhash {
        hash: Hash,
        last_valid: u64,
        calls: Mutex<usize>,
        inject_error: bool,
    }

    impl StubBlockhash {
        fn ok(hash_byte: u8, last_valid: u64) -> Self {
            Self {
                hash: Hash::new_from_array([hash_byte; 32]),
                last_valid,
                calls: Mutex::new(0),
                inject_error: false,
            }
        }
        fn erroring() -> Self {
            Self {
                hash: Hash::default(),
                last_valid: 0,
                calls: Mutex::new(0),
                inject_error: true,
            }
        }
        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl RecentBlockhashProvider for StubBlockhash {
        async fn latest_blockhash(&self) -> Result<(Hash, u64), BlockhashError> {
            *self.calls.lock().unwrap() += 1;
            if self.inject_error {
                Err(BlockhashError::FetchFailure {
                    reason: "stub error".to_string(),
                })
            } else {
                Ok((self.hash, self.last_valid))
            }
        }
    }

    // ── Fixtures ───────────────────────────────────────────────────────────

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
                amount: UnderlyingAmount::new(1000),
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

    fn fresh_plan(obligation_exists: bool) -> SolendDepositTxPlan {
        let session_wallet = Pubkey::new_unique();
        let obligation_pubkey = Pubkey::new_unique();
        let parked = parked_intent(session_wallet);
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

    // ── (E/H/I/J/K) First-deposit: generates exactly one obligation KP,
    //               SOP order enforced, no mutation after partial sign,
    //               real blockhash used, Base64 decodes back to Transaction
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn first_deposit_handoff_signs_with_obligation_keypair_and_leaves_session_pending() {
        use base64::Engine as _;

        let plan = fresh_plan(false);
        let bh = StubBlockhash::ok(0xAB, 1_234_000);
        let store = SolendSigningStore::new();
        let preflight = passed_preflight();

        let summary = create_signing_handoff(&plan, &preflight, &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .expect("handoff created");

        assert_eq!(store.parked_count(), 1);
        assert_eq!(bh.call_count(), 1, "blockhash fetched exactly once");
        assert_eq!(summary.session_wallet, plan.session_wallet);
        assert!(summary.obligation_signer_backend_partial);
        assert_eq!(summary.last_valid_block_height, 1_234_000);

        // Consume and inspect the parked tx bytes.
        let parked = store.consume(summary.signing_request_id).expect("present");
        assert!(parked.obligation_keypair.is_some());
        // Serialize summary still matches:
        assert!(!store.contains(&summary.signing_request_id), "consume removes the entry");

        // (K) Base64-encode matches shape; decodes back to Transaction.
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&parked.tx_bytes);
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64.as_bytes())
            .unwrap();
        let decoded_tx: Transaction = bincode::deserialize(&decoded_bytes).unwrap();

        // (J) Real blockhash in the message, NOT Hash::default().
        assert_eq!(decoded_tx.message.recent_blockhash, Hash::new_from_array([0xAB; 32]));
        assert_ne!(decoded_tx.message.recent_blockhash, Hash::default());

        // (H) Exactly two required signers: [obligation, session_wallet].
        // The obligation slot is populated (non-default) because we
        // partial-signed. The session-wallet slot remains default —
        // the user's wallet fills it externally.
        let required_sigs = decoded_tx.message.header.num_required_signatures as usize;
        assert_eq!(required_sigs, 2, "two required signers: obligation + fee payer");
        assert_eq!(decoded_tx.signatures.len(), required_sigs);

        // Fee payer is first in account_keys and its signature is default.
        let payer_index = 0;
        let payer_key = decoded_tx.message.account_keys[payer_index];
        assert_eq!(payer_key, plan.session_wallet, "fee payer is session wallet");
        assert_eq!(
            decoded_tx.signatures[payer_index],
            solana_sdk::signature::Signature::default(),
            "(G) session-wallet signature slot stays pending — server never signs as session wallet"
        );

        // Obligation slot signature is non-default (we partially signed).
        // Find the obligation signer in account_keys by matching against
        // the consumed keypair's pubkey.
        let real_obligation_pk = parked.obligation_keypair.as_ref().unwrap().pubkey();
        let obligation_index = decoded_tx
            .message
            .account_keys
            .iter()
            .position(|k| *k == real_obligation_pk)
            .expect("real obligation pubkey must appear in account_keys");
        assert!(obligation_index < required_sigs);
        assert_ne!(
            decoded_tx.signatures[obligation_index],
            solana_sdk::signature::Signature::default(),
            "obligation slot is partially signed"
        );

        // (E) Transient placeholder absent from every account_meta.
        let placeholder = plan.transient_obligation_pubkey.unwrap();
        assert_ne!(placeholder, real_obligation_pk);
        for ix in &decoded_tx.message.instructions {
            for &acct_idx in &ix.accounts {
                let k = decoded_tx.message.account_keys[acct_idx as usize];
                assert_ne!(
                    k, placeholder,
                    "transient placeholder must not appear anywhere in the final tx"
                );
            }
        }
    }

    // ── (F) Existing obligation: no obligation Keypair generated ────────────

    #[tokio::test]
    async fn existing_obligation_generates_no_obligation_keypair() {
        let plan = fresh_plan(true);
        assert!(!plan.obligation_signer_required);
        let bh = StubBlockhash::ok(0x33, 100);
        let store = SolendSigningStore::new();

        let summary = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        assert!(!summary.obligation_signer_backend_partial);

        let parked = store.consume(summary.signing_request_id).unwrap();
        assert!(parked.obligation_keypair.is_none());

        // Transaction signatures: only 1 required (session wallet) and is default.
        let tx: Transaction = bincode::deserialize(&parked.tx_bytes).unwrap();
        let required = tx.message.header.num_required_signatures as usize;
        assert_eq!(required, 1, "only session wallet required to sign");
        assert_eq!(
            tx.signatures[0],
            solana_sdk::signature::Signature::default(),
            "session-wallet signature stays pending"
        );
    }

    // ── (B) Config errors: inconsistent flags → typed error ─────────────────

    #[tokio::test]
    async fn signer_required_true_but_transient_none_returns_config_error() {
        let mut plan = fresh_plan(false);
        plan.transient_obligation_pubkey = None;
        let bh = StubBlockhash::ok(0x01, 1);
        let store = SolendSigningStore::new();
        let err = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap_err();
        assert!(matches!(err, SigningHandoffError::MissingTransientObligation));
        assert_eq!(store.parked_count(), 0);
        assert_eq!(bh.call_count(), 0, "fails before RPC");
    }

    #[tokio::test]
    async fn signer_required_false_but_transient_some_returns_config_error() {
        let mut plan = fresh_plan(true);
        plan.transient_obligation_pubkey = Some(Pubkey::new_unique());
        let bh = StubBlockhash::ok(0x01, 1);
        let store = SolendSigningStore::new();
        let err = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap_err();
        assert!(matches!(err, SigningHandoffError::UnexpectedTransientObligation));
        assert_eq!(store.parked_count(), 0);
    }

    // ── Blockhash RPC failure surfaces cleanly ──────────────────────────────

    #[tokio::test]
    async fn blockhash_failure_surfaces_typed_error_no_park() {
        let plan = fresh_plan(false);
        let bh = StubBlockhash::erroring();
        let store = SolendSigningStore::new();
        let err = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap_err();
        assert!(matches!(err, SigningHandoffError::BlockhashFetchFailed(_)));
        assert_eq!(store.parked_count(), 0);
    }

    // ── (M) TTL: expired request yields no residual Keypair ─────────────────

    #[tokio::test]
    async fn expired_request_drops_keypair_via_summary_and_consume() {
        let plan = fresh_plan(false);
        let bh = StubBlockhash::ok(0x77, 100);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();

        // Backdate expiration.
        {
            let mut entry = store.inner.get_mut(&summary.signing_request_id).unwrap();
            entry.expires_at = Utc::now() - Duration::seconds(10);
        }

        // Summary returns None AND sweeps the expired entry.
        assert!(store.summary(summary.signing_request_id).is_none());
        assert!(!store.contains(&summary.signing_request_id));
        assert_eq!(store.parked_count(), 0, "no residual Keypair lingering");

        // Re-park and validate consume() on an expired entry drops it too.
        let summary2 = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        {
            let mut entry = store.inner.get_mut(&summary2.signing_request_id).unwrap();
            entry.expires_at = Utc::now() - Duration::seconds(10);
        }
        assert!(store.consume(summary2.signing_request_id).is_none());
        assert!(!store.contains(&summary2.signing_request_id));
        assert_eq!(store.parked_count(), 0);
    }

    /// (M continued) Consumed-or-removed request leaves no residual.
    #[tokio::test]
    async fn consumed_and_removed_requests_leave_no_residual_keypair() {
        let plan = fresh_plan(false);
        let bh = StubBlockhash::ok(0x88, 100);
        let store = SolendSigningStore::new();

        // Case 1: consume
        let s1 = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        let _ = store.consume(s1.signing_request_id).expect("present");
        assert!(!store.contains(&s1.signing_request_id));

        // Case 2: explicit remove
        let s2 = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        store.remove(&s2.signing_request_id);
        assert!(!store.contains(&s2.signing_request_id));

        // Case 3: cleanup_expired sweeps only expired
        let s3 = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        let s4 = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();
        {
            let mut e3 = store.inner.get_mut(&s3.signing_request_id).unwrap();
            e3.expires_at = Utc::now() - Duration::seconds(1);
        }
        let removed = store.cleanup_expired();
        assert_eq!(removed, 1);
        assert!(!store.contains(&s3.signing_request_id));
        assert!(store.contains(&s4.signing_request_id));
    }

    // ── (L) Handoff summary exposes no private key material ─────────────────

    #[tokio::test]
    async fn signing_handoff_summary_contains_no_secret_material() {
        let plan = fresh_plan(false);
        let bh = StubBlockhash::ok(0x99, 100);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(&plan, &passed_preflight(), &store, &bh, &crate::integrations::solend_submit::NullSolendAuditSink, 120)
            .await
            .unwrap();

        // The summary struct is Debug-printable; ensure the Debug form
        // does not contain anything that looks like a secret key leak.
        let dbg = format!("{summary:?}");
        assert!(!dbg.contains("secret"));
        assert!(!dbg.contains("Keypair"));
        assert!(!dbg.contains("to_bytes"));
        // And nothing that looks like 64 hex bytes dumped.
        // (Heuristic; the real protection is that Keypair isn't a field.)
    }

    // ── (4C-6) Session-scoped retrieval enforces ownership ─────────────────

    #[tokio::test]
    async fn summary_for_session_returns_summary_when_session_matches() {
        let plan = fresh_plan(false);
        let owner_session = plan.session_id.clone();
        let bh = StubBlockhash::ok(0x12, 200);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(
            &plan,
            &passed_preflight(),
            &store,
            &bh,
            &crate::integrations::solend_submit::NullSolendAuditSink,
            120,
        )
        .await
        .unwrap();

        assert_eq!(summary.session_id, owner_session);

        let got = store
            .summary_for_session(&owner_session, summary.signing_request_id)
            .expect("matching session sees summary");
        assert_eq!(got.signing_request_id, summary.signing_request_id);
        assert_eq!(got.session_id, owner_session);
        // Retrieval does NOT consume the entry.
        assert!(store.contains(&summary.signing_request_id));
    }

    #[tokio::test]
    async fn summary_for_session_rejects_wrong_session_without_sweeping() {
        let plan = fresh_plan(false);
        let owner_session = plan.session_id.clone();
        let bh = StubBlockhash::ok(0x13, 200);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(
            &plan,
            &passed_preflight(),
            &store,
            &bh,
            &crate::integrations::solend_submit::NullSolendAuditSink,
            120,
        )
        .await
        .unwrap();

        let attacker = SessionId::new();
        assert_ne!(attacker, owner_session);

        assert!(store
            .summary_for_session(&attacker, summary.signing_request_id)
            .is_none());
        // Legitimate session's window is not disrupted.
        assert!(store.contains(&summary.signing_request_id));
        assert!(store
            .summary_for_session(&owner_session, summary.signing_request_id)
            .is_some());
    }

    #[tokio::test]
    async fn summary_for_session_treats_expired_as_miss_and_sweeps() {
        let plan = fresh_plan(false);
        let owner_session = plan.session_id.clone();
        let bh = StubBlockhash::ok(0x14, 200);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(
            &plan,
            &passed_preflight(),
            &store,
            &bh,
            &crate::integrations::solend_submit::NullSolendAuditSink,
            120,
        )
        .await
        .unwrap();

        {
            let mut entry = store.inner.get_mut(&summary.signing_request_id).unwrap();
            entry.expires_at = Utc::now() - Duration::seconds(1);
        }

        assert!(store
            .summary_for_session(&owner_session, summary.signing_request_id)
            .is_none());
        assert!(!store.contains(&summary.signing_request_id));
    }

    #[tokio::test]
    async fn tx_bytes_for_session_returns_bytes_only_to_owner() {
        let plan = fresh_plan(false);
        let owner_session = plan.session_id.clone();
        let bh = StubBlockhash::ok(0x15, 200);
        let store = SolendSigningStore::new();
        let summary = create_signing_handoff(
            &plan,
            &passed_preflight(),
            &store,
            &bh,
            &crate::integrations::solend_submit::NullSolendAuditSink,
            120,
        )
        .await
        .unwrap();

        let attacker = SessionId::new();
        assert!(store
            .tx_bytes_for_session(&attacker, summary.signing_request_id)
            .is_none());

        let (got_summary, bytes) = store
            .tx_bytes_for_session(&owner_session, summary.signing_request_id)
            .expect("owner sees bytes");
        assert_eq!(got_summary.signing_request_id, summary.signing_request_id);
        assert!(!bytes.is_empty());
        // Bytes round-trip through bincode as a Transaction.
        let _tx: Transaction = bincode::deserialize(&bytes).unwrap();
        // Retrieval is NOT consumptive.
        assert!(store.contains(&summary.signing_request_id));
    }

    // ── Forbidden-term structural scan on this module's source ─────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_signing.rs");
        // Build needles at runtime so the scanner never matches its own
        // source text.
        let needles = [
            format!("{}{}", "send_", "transaction("),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "send_raw_v0_", "transaction("),
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", "get_signature_", "statuses("),
            // Note: `replace_recent_blockhash` is intentionally NOT scanned
            // here. It appears in this file only inside cfg(test) struct-
            // literal initializers for the preflight outcome (a field name,
            // not an RPC-config toggle). Production code that wanted to
            // actually set it would have to import RpcSimulateTransactionConfig;
            // this module imports no solana_client::rpc_config types at all,
            // which unit tests (first_deposit_handoff... etc.) verify
            // indirectly by asserting the blockhash reaches the Transaction.
            format!("{}{}", "new_signed_with_", "payer("),
            format!("{}{}", "tx.signatures", " = "),
            format!("{}{}", "tx.signatures", ".push("),
            format!("{}{}", "tx.signatures", ".pop("),
            format!("{}{}", "tx.signatures", ".clear("),
            format!("{}{}", "tx.signatures", ".remove("),
            // No versioned-tx / ALT / message-v0 usage. Needles built at
            // runtime so the scanner does not match its own source text.
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "solend_signing.rs must not contain `{n}`"
            );
        }
    }
}
