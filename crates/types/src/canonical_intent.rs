//! Stage 1 Tail — canonical intent schema + hash.
//!
//! # What this module owns
//!
//! A small, closed schema for "what the user is being asked to review
//! and sign right now" — the canonical intent — plus a deterministic
//! Borsh-based byte layout and SHA-256 hash. The hash is the
//! tamper-evident commitment that ties together:
//!
//!   - the chat-side proposal (as built by the assistant)
//!   - the operator approval page (what the human reviews)
//!   - the Phantom signing handoff (what the user signs)
//!   - the daemon submit / confirmation pipeline (what reaches chain)
//!
//! It is NOT a Stage 2 Authorization PDA. The hash commits to the
//! data the user reviews; strong action-binding (i.e. proving the
//! signed transaction's instructions match the canonical intent) is a
//! Stage 2 concern. This Stage 1 Tail layer gives downstream slices a
//! stable identifier they can carry across surfaces without re-deriving
//! it from JSON or string formatting.
//!
//! # Closed schema
//!
//! Three action variants supported in Stage 1 Tail:
//!   - [`IntentAction::SolendDeposit`]
//!   - [`IntentAction::SolendWithdrawAll`]
//!   - [`IntentAction::JupiterSwap`]
//!
//! Field order in the Borsh layout is deterministic — the order each
//! field is declared in this file IS the on-the-wire order. Do not
//! reorder existing fields without bumping
//! [`STAGE1_TAIL_SCHEMA_VERSION`]; the schema-version byte is the
//! first thing in the canonical bytes, so a version bump produces a
//! distinct hash domain and downstream consumers fail closed on
//! mismatch.
//!
//! Any extra / unknown field on a JSON deserialization path fails
//! closed via `#[serde(deny_unknown_fields)]`. Free-form maps are
//! intentionally not supported.
//!
//! # Hash construction
//!
//! ```text
//! canonical_bytes(intent) = borsh::to_vec(intent)
//! canonical_hash(intent)  = SHA-256(canonical_bytes(intent))
//! ```
//!
//! The hash is computed over the BORSH bytes, NOT JSON. JSON
//! serialization is provided only for human-readable fixtures /
//! audit / debug dumps.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT call any network or RPC.
//! - Does NOT build a Solana transaction.
//! - Does NOT sign or broadcast anything.
//! - Does NOT enforce session-binding (that lives in the gateway tool
//!   layer; the intent merely records the wallet the action will
//!   target so the canonical hash binds to it).
//! - Does NOT modify any existing Solend / Jupiter transaction
//!   builder.
//! - Does NOT introduce conditions, scheduling, or any
//!   execute-later semantics. Stage 1 Tail is "user instructs now,
//!   user signs now".

use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Schema version embedded at the start of every canonical intent.
///
/// The version byte is the FIRST byte of the canonical Borsh layout, so
/// it is included in every hash. A future schema change MUST bump this
/// constant; downstream consumers that pin a specific version will
/// see a hash mismatch and fail closed instead of silently accepting
/// reinterpretations of older bytes.
pub const STAGE1_TAIL_SCHEMA_VERSION: u8 = 1;

// ── PubkeyBytes ────────────────────────────────────────────────────────────

/// 32-byte Solana pubkey, with a deterministic Borsh layout (32 raw
/// bytes) and a base58 JSON repr.
///
/// `claw-types` deliberately does not depend on `solana_sdk`, so we
/// keep our own minimal newtype here. Conversions to/from
/// `solana_sdk::Pubkey` exist by `Pubkey::to_bytes()` / `Pubkey::from()`
/// at every call site; the byte layout is identical so the canonical
/// hash matches whatever any other crate would produce from the same
/// 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct PubkeyBytes(pub [u8; 32]);

impl PubkeyBytes {
    /// Construct from raw bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow as a byte slice (used by tests + audit dumps).
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse a base58-encoded pubkey string into a `PubkeyBytes`.
    /// Returns `Err` on invalid base58 or wrong byte length.
    pub fn from_base58(s: &str) -> Result<Self, PubkeyParseError> {
        let v = bs58::decode(s.as_bytes())
            .into_vec()
            .map_err(|e| PubkeyParseError::InvalidBase58(e.to_string()))?;
        let arr: [u8; 32] = v
            .try_into()
            .map_err(|v: Vec<u8>| PubkeyParseError::WrongLength(v.len()))?;
        Ok(Self(arr))
    }

    /// Format as a base58 string. Standard Solana display form.
    pub fn to_base58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }
}

impl fmt::Display for PubkeyBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PubkeyParseError {
    #[error("invalid base58: {0}")]
    InvalidBase58(String),
    #[error("pubkey must be exactly 32 bytes, got {0}")]
    WrongLength(usize),
}

impl Serialize for PubkeyBytes {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for PubkeyBytes {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Fully-qualified to disambiguate from `BorshDeserialize::deserialize`.
        let s = <String as Deserialize<'de>>::deserialize(de)?;
        Self::from_base58(&s).map_err(serde::de::Error::custom)
    }
}

// ── Action variants ─────────────────────────────────────────────────────────

/// The three Stage 1 Tail action variants.
///
/// Variants are explicit (no catch-all). Adding a new variant later
/// MUST be paired with bumping [`STAGE1_TAIL_SCHEMA_VERSION`] so that
/// older consumers do not silently parse newer-shaped bytes as one of
/// the existing variants.
///
/// Field order in each variant is the canonical Borsh order — the
/// order written below.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntentAction {
    /// Deposit `amount_raw` of `input_mint` into the Solend reserve at
    /// `reserve_pubkey` (under the configured `lending_market`) on
    /// behalf of `wallet_pubkey`. `amount_decimals` is the underlying
    /// token's decimal scale (6 for USDC; 9 for SOL/wSOL); duplication
    /// in the intent record is intentional so the canonical hash binds
    /// to the unit-domain interpretation, not just the raw integer.
    SolendDeposit {
        wallet_pubkey: PubkeyBytes,
        input_mint: PubkeyBytes,
        reserve_pubkey: PubkeyBytes,
        lending_market: PubkeyBytes,
        amount_raw: u64,
        amount_decimals: u8,
    },
    /// Withdraw 100 % of the user's deposited collateral from
    /// `obligation_pubkey` on the Solend reserve at `reserve_pubkey`.
    /// No amount field is exposed — the on-chain Solend program
    /// interprets `u64::MAX` as "redeem all", and the gateway
    /// transaction builder supplies that sentinel internally. The
    /// canonical intent records the obligation explicitly so the hash
    /// commits to which deposit position the user is approving the
    /// withdrawal of.
    SolendWithdrawAll {
        wallet_pubkey: PubkeyBytes,
        obligation_pubkey: PubkeyBytes,
        reserve_pubkey: PubkeyBytes,
        lending_market: PubkeyBytes,
    },
    /// Swap `input_amount_raw` of `input_mint` to `output_mint` on
    /// behalf of `wallet_pubkey`, with `slippage_bps` basis points of
    /// slippage tolerance. The canonical intent does NOT record the
    /// expected output amount — that is a quote-time concern and
    /// rebinding the canonical hash to a quote would require a
    /// re-confirmation flow that is out of Stage 1 Tail scope.
    JupiterSwap {
        wallet_pubkey: PubkeyBytes,
        input_mint: PubkeyBytes,
        output_mint: PubkeyBytes,
        input_amount_raw: u64,
        slippage_bps: u16,
    },
}

// ── CanonicalIntent ─────────────────────────────────────────────────────────

/// Top-level canonical intent record. All Stage 1 Tail surfaces refer
/// to a proposal by its [`canonical_hash`].
///
/// Field order = canonical Borsh order. `schema_version` first so the
/// hash is immediately namespaced; `expires_at_slot` last so a future
/// addition (e.g. `notes` or `correlation_id`) can land between
/// `action` and `expires_at_slot` only by bumping
/// [`STAGE1_TAIL_SCHEMA_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalIntent {
    pub schema_version: u8,
    pub intent_id: [u8; 16],
    pub user: PubkeyBytes,
    pub action: IntentAction,
    pub expires_at_slot: u64,
}

// ── Canonical bytes + hash ──────────────────────────────────────────────────

/// Borsh-serialize the intent into its canonical byte form.
///
/// Borsh is deterministic by construction (no map iteration order, no
/// floats, no enum discriminant ambiguity) so two callers running the
/// same Rust crate version on the same intent always produce
/// byte-identical output. Borsh `to_vec` on a `BorshSerialize` impl
/// derived for plain structs / enums is infallible in practice; we
/// surface a panic here only on a programmer error (e.g. a custom
/// impl returning Err), which is unreachable for the auto-derived
/// types in this module.
pub fn canonical_bytes(intent: &CanonicalIntent) -> Vec<u8> {
    borsh::to_vec(intent).expect(
        "Borsh serialization of CanonicalIntent is infallible for the auto-derived layout",
    )
}

/// SHA-256 hash of the canonical Borsh bytes. Stable across runs and
/// across crate consumers.
pub fn canonical_hash(intent: &CanonicalIntent) -> [u8; 32] {
    let bytes = canonical_bytes(intent);
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.finalize().into()
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Errors raised by [`validate_not_expired`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IntentValidationError {
    #[error("intent expired: current_slot {current} >= expires_at_slot {expires}")]
    Expired { current: u64, expires: u64 },
}

/// Returns `Ok(())` iff `current_slot < intent.expires_at_slot`. The
/// boundary is exclusive: an intent at `expires_at_slot` is already
/// expired, mirroring how Solana's last-valid-block-height works
/// downstream. Fail-closed semantics — the caller that chose
/// `expires_at_slot` is responsible for picking a slot that gives the
/// user enough time to review and sign.
pub fn validate_not_expired(
    intent: &CanonicalIntent,
    current_slot: u64,
) -> Result<(), IntentValidationError> {
    if current_slot < intent.expires_at_slot {
        Ok(())
    } else {
        Err(IntentValidationError::Expired {
            current: current_slot,
            expires: intent.expires_at_slot,
        })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_hex(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len() * 2);
        for byte in b {
            // SAFETY: writing ascii hex into a String never errors.
            std::fmt::Write::write_fmt(&mut s, format_args!("{byte:02x}")).unwrap();
        }
        s
    }

    // ── Concrete fixtures ───────────────────────────────────────────────

    /// Stable test pubkeys. The base58 strings here are the live
    /// mainnet pubkeys referenced in the broader Stage 1 spec; they
    /// are public chain data and committing them to source is fine.
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
    const HCKRV_OBLIGATION: &str = "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV";
    const TEST_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

    /// Stable intent id for fixtures. 16 bytes, all zero except a
    /// short marker so the bytes are recognisable in audit dumps.
    const FIXTURE_INTENT_ID: [u8; 16] =
        [0x57, 0x41, 0x4C, 0x4C, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];

    /// Stable expiry slot (mainnet slot ~Apr 2026). Choosing a
    /// concrete value makes the canonical hash stable across runs.
    const FIXTURE_EXPIRES_AT: u64 = 415_500_000;

    fn pk(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test pubkey parses")
    }

    fn fixture_solend_deposit() -> CanonicalIntent {
        CanonicalIntent {
            schema_version: STAGE1_TAIL_SCHEMA_VERSION,
            intent_id: FIXTURE_INTENT_ID,
            user: pk(TEST_WALLET),
            action: IntentAction::SolendDeposit {
                wallet_pubkey: pk(TEST_WALLET),
                input_mint: pk(USDC_MINT),
                reserve_pubkey: pk(SOLEND_USDC_RESERVE),
                lending_market: pk(SOLEND_LENDING_MARKET),
                amount_raw: 5_000_000, // 5 USDC, 6 decimals
                amount_decimals: 6,
            },
            expires_at_slot: FIXTURE_EXPIRES_AT,
        }
    }

    fn fixture_solend_withdraw_all() -> CanonicalIntent {
        CanonicalIntent {
            schema_version: STAGE1_TAIL_SCHEMA_VERSION,
            intent_id: FIXTURE_INTENT_ID,
            user: pk(TEST_WALLET),
            action: IntentAction::SolendWithdrawAll {
                wallet_pubkey: pk(TEST_WALLET),
                obligation_pubkey: pk(HCKRV_OBLIGATION),
                reserve_pubkey: pk(SOLEND_USDC_RESERVE),
                lending_market: pk(SOLEND_LENDING_MARKET),
            },
            expires_at_slot: FIXTURE_EXPIRES_AT,
        }
    }

    fn fixture_jupiter_swap() -> CanonicalIntent {
        CanonicalIntent {
            schema_version: STAGE1_TAIL_SCHEMA_VERSION,
            intent_id: FIXTURE_INTENT_ID,
            user: pk(TEST_WALLET),
            action: IntentAction::JupiterSwap {
                wallet_pubkey: pk(TEST_WALLET),
                input_mint: pk(WSOL_MINT),
                output_mint: pk(USDC_MINT),
                input_amount_raw: 1_000_000,
                slippage_bps: 50,
            },
            expires_at_slot: FIXTURE_EXPIRES_AT,
        }
    }

    // ── Stable hash fixtures ────────────────────────────────────────────

    /// The committed hex strings are the canonical SHA-256 hashes for
    /// the three fixtures above. Any change to the schema, field
    /// order, or fixture contents will surface here as a hash
    /// mismatch — this is the cross-surface tamper-evidence guarantee
    /// downstream consumers rely on.
    #[test]
    fn solend_deposit_fixture_hash_is_stable() {
        let h = canonical_hash(&fixture_solend_deposit());
        assert_eq!(
            fmt_hex(&h),
            "0e2cbf3f57400e84a548c5a142d9e246214c1b26b85a75e9cca65cbfe8965bf1"
        );
    }

    #[test]
    fn solend_withdraw_all_fixture_hash_is_stable() {
        let h = canonical_hash(&fixture_solend_withdraw_all());
        assert_eq!(
            fmt_hex(&h),
            "05a840f284b9c7b2163a1862806fc83a5b05127ac2e1833d5394c60b3b2ecf7a"
        );
    }

    #[test]
    fn jupiter_swap_fixture_hash_is_stable() {
        let h = canonical_hash(&fixture_jupiter_swap());
        assert_eq!(
            fmt_hex(&h),
            "a9de08671a9d6811a142723043a43223a3e7a8bd567d6476d7c5821b1286ca6d"
        );
    }

    // ── Determinism ────────────────────────────────────────────────────

    #[test]
    fn same_struct_serialises_to_same_bytes() {
        let a = fixture_solend_deposit();
        let b = fixture_solend_deposit();
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn distinct_variants_have_distinct_hashes() {
        let h1 = canonical_hash(&fixture_solend_deposit());
        let h2 = canonical_hash(&fixture_solend_withdraw_all());
        let h3 = canonical_hash(&fixture_jupiter_swap());
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_ne!(h1, h3);
    }

    // ── Mutation tests ─────────────────────────────────────────────────

    #[test]
    fn changing_amount_changes_deposit_hash() {
        let mut m = fixture_solend_deposit();
        if let IntentAction::SolendDeposit {
            ref mut amount_raw, ..
        } = m.action
        {
            *amount_raw += 1;
        }
        assert_ne!(
            canonical_hash(&fixture_solend_deposit()),
            canonical_hash(&m)
        );
    }

    #[test]
    fn changing_input_amount_changes_swap_hash() {
        let mut m = fixture_jupiter_swap();
        if let IntentAction::JupiterSwap {
            ref mut input_amount_raw,
            ..
        } = m.action
        {
            *input_amount_raw = 2_000_000;
        }
        assert_ne!(canonical_hash(&fixture_jupiter_swap()), canonical_hash(&m));
    }

    #[test]
    fn changing_slippage_changes_swap_hash() {
        let mut m = fixture_jupiter_swap();
        if let IntentAction::JupiterSwap {
            ref mut slippage_bps,
            ..
        } = m.action
        {
            *slippage_bps = 100;
        }
        assert_ne!(canonical_hash(&fixture_jupiter_swap()), canonical_hash(&m));
    }

    #[test]
    fn changing_expires_at_slot_changes_hash() {
        // Brief: "Add tests proving changing expires_at_slot changes the hash."
        for fixture in [
            fixture_solend_deposit(),
            fixture_solend_withdraw_all(),
            fixture_jupiter_swap(),
        ] {
            let original_hash = canonical_hash(&fixture);
            let mut bumped = fixture.clone();
            bumped.expires_at_slot += 1;
            assert_ne!(
                original_hash,
                canonical_hash(&bumped),
                "expires_at_slot must be part of the canonical hash"
            );
        }
    }

    #[test]
    fn changing_user_changes_hash() {
        let mut m = fixture_solend_deposit();
        // Flip a single bit in the user pubkey.
        m.user.0[0] ^= 0x01;
        assert_ne!(
            canonical_hash(&fixture_solend_deposit()),
            canonical_hash(&m)
        );
    }

    #[test]
    fn changing_intent_id_changes_hash() {
        let mut m = fixture_solend_deposit();
        m.intent_id[15] ^= 0x01;
        assert_ne!(
            canonical_hash(&fixture_solend_deposit()),
            canonical_hash(&m)
        );
    }

    #[test]
    fn changing_obligation_in_withdraw_changes_hash() {
        let mut m = fixture_solend_withdraw_all();
        if let IntentAction::SolendWithdrawAll {
            ref mut obligation_pubkey,
            ..
        } = m.action
        {
            obligation_pubkey.0[0] ^= 0x01;
        }
        assert_ne!(
            canonical_hash(&fixture_solend_withdraw_all()),
            canonical_hash(&m)
        );
    }

    #[test]
    fn schema_version_is_included_in_hash() {
        // Brief: "schema_version is included in the hash"
        let mut m = fixture_solend_deposit();
        m.schema_version = STAGE1_TAIL_SCHEMA_VERSION.wrapping_add(1);
        assert_ne!(
            canonical_hash(&fixture_solend_deposit()),
            canonical_hash(&m)
        );
        // And — concretely — the FIRST byte of the canonical bytes IS
        // the schema version, by Borsh field-order construction.
        let bytes = canonical_bytes(&fixture_solend_deposit());
        assert_eq!(bytes[0], STAGE1_TAIL_SCHEMA_VERSION);
    }

    // ── Expiry validation ──────────────────────────────────────────────

    #[test]
    fn validate_not_expired_passes_below_expiry() {
        let intent = fixture_solend_deposit();
        assert_eq!(
            validate_not_expired(&intent, intent.expires_at_slot - 1),
            Ok(())
        );
        assert_eq!(validate_not_expired(&intent, 0), Ok(()));
    }

    #[test]
    fn validate_not_expired_fails_at_or_after_expiry() {
        let intent = fixture_solend_deposit();
        let exp = intent.expires_at_slot;
        assert_eq!(
            validate_not_expired(&intent, exp),
            Err(IntentValidationError::Expired {
                current: exp,
                expires: exp,
            })
        );
        assert_eq!(
            validate_not_expired(&intent, exp + 1_000),
            Err(IntentValidationError::Expired {
                current: exp + 1_000,
                expires: exp,
            })
        );
    }

    // ── Closed-schema (deny_unknown_fields) ────────────────────────────

    #[test]
    fn json_with_unknown_top_level_field_fails() {
        let json = serde_json::to_string(&fixture_solend_deposit()).unwrap();
        // Inject an extra field at the top level.
        let injected = json.replacen("\"user\":", "\"injected\":\"x\",\"user\":", 1);
        let res: Result<CanonicalIntent, _> = serde_json::from_str(&injected);
        assert!(
            res.is_err(),
            "deny_unknown_fields must reject extra top-level field"
        );
    }

    #[test]
    fn json_with_unknown_action_field_fails() {
        let json = serde_json::to_string(&fixture_jupiter_swap()).unwrap();
        let injected = json.replacen(
            "\"slippage_bps\":50",
            "\"slippage_bps\":50,\"hidden_fee\":1000",
            1,
        );
        let res: Result<CanonicalIntent, _> = serde_json::from_str(&injected);
        assert!(
            res.is_err(),
            "deny_unknown_fields must reject extra field on action variant"
        );
    }

    // ── Borsh round-trip ───────────────────────────────────────────────

    #[test]
    fn borsh_round_trip_preserves_intent() {
        for fixture in [
            fixture_solend_deposit(),
            fixture_solend_withdraw_all(),
            fixture_jupiter_swap(),
        ] {
            let bytes = canonical_bytes(&fixture);
            let decoded: CanonicalIntent =
                borsh::from_slice(&bytes).expect("borsh round-trip");
            assert_eq!(decoded, fixture);
        }
    }

    // ── Output fixture for downstream agents (run with --nocapture) ────

    /// Emits a human-readable fixture for each intent variant — JSON,
    /// Borsh-bytes-hex, hash-hex. Frontend / on-chain program agents
    /// can consume these to verify their own canonical-bytes
    /// implementations match.
    ///
    /// Run: `cargo test -p claw-types canonical_intent::tests::print_fixtures -- --nocapture`
    #[test]
    fn print_fixtures() {
        for (label, fixture) in [
            ("solend_deposit", fixture_solend_deposit()),
            ("solend_withdraw_all", fixture_solend_withdraw_all()),
            ("jupiter_swap", fixture_jupiter_swap()),
        ] {
            let bytes = canonical_bytes(&fixture);
            let hash = canonical_hash(&fixture);
            let json = serde_json::to_string_pretty(&fixture).unwrap();
            println!("\n=== fixture: {label} ===");
            println!("schema_version = {STAGE1_TAIL_SCHEMA_VERSION}");
            println!("canonical_json:\n{json}");
            println!("canonical_bytes_hex ({} bytes):", bytes.len());
            println!("  {}", fmt_hex(&bytes));
            println!("canonical_hash_hex (32 bytes):");
            println!("  {}", fmt_hex(&hash));
        }
    }

    // ── PubkeyBytes round-trip ──────────────────────────────────────────

    #[test]
    fn pubkey_bytes_base58_round_trip() {
        let pk = pk(TEST_WALLET);
        assert_eq!(pk.to_base58(), TEST_WALLET);
        // Borsh layout is exactly 32 raw bytes.
        let bytes = borsh::to_vec(&pk).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..], pk.as_bytes());
    }

    #[test]
    fn pubkey_bytes_invalid_base58_fails() {
        assert!(matches!(
            PubkeyBytes::from_base58("not!base58"),
            Err(PubkeyParseError::InvalidBase58(_))
        ));
        // 31 bytes (one short).
        let short = bs58::encode(&[0u8; 31]).into_string();
        assert!(matches!(
            PubkeyBytes::from_base58(&short),
            Err(PubkeyParseError::WrongLength(31))
        ));
    }
}
