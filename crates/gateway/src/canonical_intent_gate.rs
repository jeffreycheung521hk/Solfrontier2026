//! Stage 1 Tail — canonical intent expiry gate.
//!
//! Backend-side fail-closed expiry helper that consumes the canonical
//! intent metadata snapshot ([`claw_types::CanonicalIntentMetadata`])
//! and an injected current-slot source. The helper is the **single
//! place** the gateway side checks
//!
//! ```text
//! current_slot < expires_at_slot
//! ```
//!
//! and produces a stable typed error when that fails. Future Stage 1
//! Tail wiring slices ("Prompt I" and beyond) will call
//! [`check_intent_expiry`] from:
//!
//!   - the approval-decision path
//!   - the JIT signing-prepare path
//!   - the signed-tx submit verifier
//!
//! …once each of those carries `Option<CanonicalIntentMetadata>`. This
//! slice deliberately does NOT modify those code paths — Stage 1 Tail
//! brief explicitly forbids touching the tx-builder / signing /
//! submit / confirmation pipelines. The helper is built and tested
//! standalone so the production wiring slice has a stable surface to
//! call into.
//!
//! # Backwards-compat semantics
//!
//! Existing approval / signing flows that were created BEFORE
//! canonical-intent metadata was emitted carry no metadata — the
//! callers pass `None`, the gate returns
//! [`GateOutcome::NoCanonicalMetadata`] (Ok), and the existing code
//! path proceeds unchanged. This is the explicit "old approvals
//! continue to behave exactly as before" requirement: until the
//! emit-canonical-intent slice lands for every tool, the gate is a
//! no-op pass for those flows.
//!
//! # Why a sync trait
//!
//! The gate is purely arithmetic against an already-known slot. The
//! actual fetch (`getSlot` RPC, ledger-cache read, etc.) happens at
//! the call site that owns its own RPC connection. Keeping
//! [`SlotProvider`] sync avoids forcing async into approval-decide /
//! signing-prepare boundaries that are otherwise sync, and keeps unit
//! tests cheap (no tokio runtime needed).
//!
//! # Solana slot semantics
//!
//! The boundary is exclusive at `expires_at_slot` to mirror Solana's
//! `last_valid_block_height` semantics: a transaction with
//! `last_valid_block_height = X` is no longer accepted at block
//! height X. We mirror that here so an intent's "lifetime" matches
//! the lifetime of any transaction the user would sign against it.
//! See `claw_types::canonical_intent::validate_not_expired` for the
//! upstream definition; this module re-uses it via
//! [`CanonicalIntentMetadata::validate_not_expired`].

use thiserror::Error;

use claw_types::{CanonicalIntentMetadata, IntentValidationError};

/// Sync source-of-current-slot seam. Production wires a thin adapter
/// over the daemon's existing slot/blockheight cache (e.g. the same
/// data feeding `solend::SolanaAccountFetcher::get_slot`); tests
/// inject a fixed-value stub.
///
/// Kept narrow on purpose:
///   - sync (no `async_trait`),
///   - returns `u64` (not `Result<u64, ...>`) — fetch errors belong to
///     whatever layer owns the RPC client, not to the expiry gate
///     itself; the gate's contract is "given a slot, decide".
pub trait SlotProvider: Send + Sync {
    fn current_slot(&self) -> u64;
}

/// Trivial slot provider for tests / dry runs / one-off callers that
/// already know the slot. Holds an immutable `u64`.
#[derive(Debug, Clone, Copy)]
pub struct StaticSlotProvider(pub u64);

impl SlotProvider for StaticSlotProvider {
    fn current_slot(&self) -> u64 {
        self.0
    }
}

/// Outcomes the gate can produce on a successful pass.
///
/// Kept distinct so callers can log / observe whether the pass was
/// "no canonical metadata attached" (legacy flow) versus "canonical
/// metadata present and not expired" (Stage 1 Tail flow). Both are
/// `Ok(_)` returns from [`check_intent_expiry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// The flow predates canonical-intent emission. Backwards-compat
    /// pass-through; production wiring continues exactly as before.
    NoCanonicalMetadata,
    /// Canonical metadata was attached and `current_slot < expires_at_slot`.
    /// Production wiring proceeds.
    NotExpired {
        current_slot: u64,
        expires_at_slot: u64,
    },
}

/// Errors raised by [`check_intent_expiry`]. Currently single-variant;
/// adding more strictness (e.g. action-type allow-list, schema-version
/// gate) is a future-slice concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GateError {
    /// Canonical metadata was attached and the gate fail-closed
    /// because `current_slot >= expires_at_slot`. The error carries
    /// both slots and the intent id so audit / observability can
    /// correlate without the metadata snapshot itself being logged.
    #[error(
        "intent_expired: current_slot {current_slot} >= expires_at_slot {expires_at_slot} \
         (intent_id={intent_id_hex})"
    )]
    IntentExpired {
        current_slot: u64,
        expires_at_slot: u64,
        intent_id_hex: IntentIdHex,
    },
}

impl GateError {
    /// Stable wire `error_type` string. Kept as a `&'static str` so
    /// audit dashboards / chat-route renderers can match exactly. The
    /// brief specifies the prefix `intent_expired`; we keep that
    /// exact value so old/new code paths can interoperate.
    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::IntentExpired { .. } => "intent_expired",
        }
    }
}

/// 16-byte intent id rendered as a 32-char hex string. Carrying the
/// hex string in the error keeps `GateError` `Copy` / `Eq` (a
/// `Vec<u8>` would force a heap allocation) while still being
/// printable to logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentIdHex(pub [u8; 32]);

impl IntentIdHex {
    pub fn from_bytes(b: [u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 32];
        for (i, byte) in b.iter().enumerate() {
            out[i * 2] = HEX[(byte >> 4) as usize];
            out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
        Self(out)
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: `from_bytes` only writes ASCII hex characters.
        std::str::from_utf8(&self.0).expect("hex bytes are ASCII")
    }
}

impl std::fmt::Display for IntentIdHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stage 1 Tail expiry gate.
///
/// - `metadata = None` → backwards-compat pass-through. Returns
///   `Ok(GateOutcome::NoCanonicalMetadata)`. Used by approval /
///   signing flows that have not yet been migrated to emit canonical
///   intents; they continue to behave exactly as before.
/// - `metadata = Some(m)` and `current_slot < m.expires_at_slot` →
///   `Ok(GateOutcome::NotExpired { current_slot, expires_at_slot })`.
/// - `metadata = Some(m)` and `current_slot >= m.expires_at_slot` →
///   `Err(GateError::IntentExpired { ... })`.
///
/// Boundary is **exclusive** at `expires_at_slot` (mirrors Solana
/// `last_valid_block_height`). Caller is expected to choose
/// `expires_at_slot` with enough headroom to cover human review +
/// signing latency.
pub fn check_intent_expiry(
    metadata: Option<&CanonicalIntentMetadata>,
    current_slot: u64,
) -> Result<GateOutcome, GateError> {
    let Some(m) = metadata else {
        return Ok(GateOutcome::NoCanonicalMetadata);
    };
    match m.validate_not_expired(current_slot) {
        Ok(()) => Ok(GateOutcome::NotExpired {
            current_slot,
            expires_at_slot: m.expires_at_slot,
        }),
        Err(IntentValidationError::Expired { current, expires }) => {
            Err(GateError::IntentExpired {
                current_slot: current,
                expires_at_slot: expires,
                intent_id_hex: IntentIdHex::from_bytes(m.intent_id),
            })
        }
    }
}

/// Convenience wrapper that pulls the slot from a [`SlotProvider`].
/// Production wiring uses this; pure-data callers that already know
/// the slot can call [`check_intent_expiry`] directly.
pub fn check_intent_expiry_with_provider(
    metadata: Option<&CanonicalIntentMetadata>,
    slot_provider: &dyn SlotProvider,
) -> Result<GateOutcome, GateError> {
    check_intent_expiry(metadata, slot_provider.current_slot())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::{
        canonical_hash, CanonicalIntent, CanonicalIntentActionType, IntentAction,
        PubkeyBytes, STAGE1_TAIL_SCHEMA_VERSION,
    };

    fn pk(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn fixture_intent(expires_at_slot: u64) -> CanonicalIntent {
        CanonicalIntent {
            schema_version: STAGE1_TAIL_SCHEMA_VERSION,
            intent_id: [0xAA; 16],
            user: pk(1),
            action: IntentAction::SolendDeposit {
                wallet_pubkey: pk(2),
                input_mint: pk(3),
                reserve_pubkey: pk(4),
                lending_market: pk(5),
                amount_raw: 5_000_000,
                amount_decimals: 6,
            },
            expires_at_slot,
        }
    }

    fn fixture_metadata(expires_at_slot: u64) -> CanonicalIntentMetadata {
        CanonicalIntentMetadata::from_intent(&fixture_intent(expires_at_slot))
    }

    // ── current_slot vs expires_at_slot boundary ────────────────────────

    #[test]
    fn current_slot_below_expires_passes() {
        let m = fixture_metadata(1_000);
        match check_intent_expiry(Some(&m), 999) {
            Ok(GateOutcome::NotExpired {
                current_slot,
                expires_at_slot,
            }) => {
                assert_eq!(current_slot, 999);
                assert_eq!(expires_at_slot, 1_000);
            }
            other => panic!("expected NotExpired, got {other:?}"),
        }
    }

    #[test]
    fn current_slot_far_below_expires_passes() {
        let m = fixture_metadata(1_000_000);
        let result = check_intent_expiry(Some(&m), 0);
        assert!(matches!(result, Ok(GateOutcome::NotExpired { .. })));
    }

    #[test]
    fn current_slot_equal_to_expires_fails() {
        let m = fixture_metadata(1_000);
        match check_intent_expiry(Some(&m), 1_000) {
            Err(GateError::IntentExpired {
                current_slot,
                expires_at_slot,
                ..
            }) => {
                assert_eq!(current_slot, 1_000);
                assert_eq!(expires_at_slot, 1_000);
            }
            other => panic!("expected IntentExpired at boundary, got {other:?}"),
        }
    }

    #[test]
    fn current_slot_above_expires_fails() {
        let m = fixture_metadata(1_000);
        match check_intent_expiry(Some(&m), 1_001) {
            Err(GateError::IntentExpired {
                current_slot,
                expires_at_slot,
                ..
            }) => {
                assert_eq!(current_slot, 1_001);
                assert_eq!(expires_at_slot, 1_000);
            }
            other => panic!("expected IntentExpired beyond boundary, got {other:?}"),
        }
    }

    // ── Backwards-compatibility: legacy flows ──────────────────────────

    #[test]
    fn no_metadata_is_pass_through_at_any_slot() {
        // Brief: "old approvals without canonical metadata must
        // continue to behave exactly as before". We test multiple
        // slots to make the pass-through unconditional.
        for slot in [0u64, 1, 1_000_000, u64::MAX - 1, u64::MAX] {
            let result = check_intent_expiry(None, slot);
            assert_eq!(result, Ok(GateOutcome::NoCanonicalMetadata));
        }
    }

    // ── Provider-driven check ──────────────────────────────────────────

    #[test]
    fn provider_form_passes_below_expiry() {
        let m = fixture_metadata(500);
        let provider = StaticSlotProvider(499);
        let result = check_intent_expiry_with_provider(Some(&m), &provider);
        assert!(matches!(result, Ok(GateOutcome::NotExpired { .. })));
    }

    #[test]
    fn provider_form_fails_at_or_after_expiry() {
        let m = fixture_metadata(500);
        let provider_at = StaticSlotProvider(500);
        assert!(matches!(
            check_intent_expiry_with_provider(Some(&m), &provider_at),
            Err(GateError::IntentExpired { .. })
        ));
        let provider_after = StaticSlotProvider(700);
        assert!(matches!(
            check_intent_expiry_with_provider(Some(&m), &provider_after),
            Err(GateError::IntentExpired { .. })
        ));
    }

    #[test]
    fn provider_form_pass_through_when_metadata_none() {
        let provider = StaticSlotProvider(0);
        let result = check_intent_expiry_with_provider(None, &provider);
        assert_eq!(result, Ok(GateOutcome::NoCanonicalMetadata));
    }

    // ── Stable error shape ─────────────────────────────────────────────

    #[test]
    fn error_type_label_is_stable() {
        let m = fixture_metadata(100);
        let err = check_intent_expiry(Some(&m), 100).unwrap_err();
        // Wire-stable label expected by audit / chat-route renderers.
        assert_eq!(err.error_type(), "intent_expired");
    }

    #[test]
    fn error_carries_current_and_expires_slots_and_intent_id() {
        let m = fixture_metadata(1_500);
        let err = check_intent_expiry(Some(&m), 2_000).unwrap_err();
        let GateError::IntentExpired {
            current_slot,
            expires_at_slot,
            intent_id_hex,
        } = err;
        assert_eq!(current_slot, 2_000);
        assert_eq!(expires_at_slot, 1_500);
        // intent_id is [0xAA; 16] in the fixture; hex repr is 32 'a's.
        assert_eq!(intent_id_hex.as_str(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn error_display_contains_load_bearing_fields() {
        let m = fixture_metadata(1_500);
        let err = check_intent_expiry(Some(&m), 2_000).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("intent_expired"));
        assert!(s.contains("2000"));
        assert!(s.contains("1500"));
    }

    // ── Boundary semantics match the upstream helper ────────────────────

    /// Cross-references the gateway-side gate against the
    /// `claw_types::validate_not_expired` upstream helper. If the
    /// upstream boundary semantics ever change, this test fails so
    /// the divergence is caught here, not in production.
    #[test]
    fn gate_agrees_with_upstream_validate_not_expired() {
        let intent = fixture_intent(2_500);
        let m = CanonicalIntentMetadata::from_intent(&intent);
        for current in [0u64, 100, 2_499, 2_500, 2_501, 10_000] {
            let upstream_ok =
                claw_types::validate_not_expired(&intent, current).is_ok();
            let gate_ok = check_intent_expiry(Some(&m), current).is_ok();
            assert_eq!(
                upstream_ok, gate_ok,
                "boundary mismatch at current_slot={current}"
            );
        }
    }

    /// Cross-references hash-binding: a metadata snapshot's
    /// `canonical_intent_hash` matches `canonical_hash(&intent)`.
    /// This is the property the future signing-prepare gate will
    /// rely on to bind the parked unsigned tx to the canonical hash
    /// the user reviewed at approval time.
    #[test]
    fn metadata_hash_matches_canonical_hash() {
        let intent = fixture_intent(7_777);
        let m = CanonicalIntentMetadata::from_intent(&intent);
        assert_eq!(m.canonical_intent_hash, canonical_hash(&intent));
        assert_eq!(m.action_type, CanonicalIntentActionType::SolendDeposit);
    }

    // ── IntentIdHex helper ─────────────────────────────────────────────

    #[test]
    fn intent_id_hex_round_trip() {
        let id: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ];
        let h = IntentIdHex::from_bytes(id);
        assert_eq!(h.as_str(), "00112233445566778899aabbccddeeff");
    }
}
