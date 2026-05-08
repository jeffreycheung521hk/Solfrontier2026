//! Stage 1 Tail — convergence test: backend canonical hash MUST equal
//! the on-chain program hash for the three Agent B fixtures.
//!
//! # Why this test exists
//!
//! [`claw_types::canonical_intent::canonical_hash`] computes
//! `SHA-256(borsh::to_vec(intent))` via the off-chain `sha2` crate.
//! [`solana_program::hash::hashv`] computes SHA-256 via the on-chain
//! Agave implementation (BPF + native test-runner). Stage 1 Tail's
//! tamper-evidence guarantee depends on these two surfaces producing
//! byte-identical 32-byte outputs for the same canonical-Borsh input;
//! divergence (e.g., a future SHA-256 implementation difference, or a
//! Borsh-derive layout drift, or one side switching to a 64-byte
//! pre-image padding scheme) would silently break the program's
//! `verify_canonical_hash` check.
//!
//! This test exercises the SAME three canonical fixtures pinned by
//! Agent B's `crates/types/src/canonical_intent.rs` test module
//! (`fixture_solend_deposit`, `fixture_solend_withdraw_all`,
//! `fixture_jupiter_swap`) — re-built here verbatim against the SAME
//! constants — and asserts:
//!
//!   1. `canonical_hash(intent) == hashv(&[&canonical_bytes(intent)]).to_bytes()`
//!   2. Both equal the expected hex committed in
//!      `solend_deposit_fixture_hash_is_stable` /
//!      `solend_withdraw_all_fixture_hash_is_stable` /
//!      `jupiter_swap_fixture_hash_is_stable`.
//!
//! Equality (1) is the cross-surface convergence proof. Equality (2)
//! anchors the result to Agent B's committed expected output, so a
//! drift on either side surfaces here as a hash mismatch.
//!
//! # Official sources cited
//!
//! - **Borsh field order / enum tag / Vec length prefix / integer
//!   endian**: borsh 1.x `BorshSerialize` derive. Per the borsh-rs spec
//!   ([github.com/near/borsh-rs](https://github.com/near/borsh-rs)
//!   `README.md` "Specification"), Borsh serializes structs in declared
//!   field order, enum variants as a `u8` tag (0-indexed by declaration
//!   order) followed by the variant's fields, integers as
//!   little-endian, and `Vec<T>` as a `u32` length prefix followed by
//!   the elements. `[u8; N]` (used by `intent_id` and the inner array
//!   of `PubkeyBytes`) is a fixed-size array with no length prefix.
//!   The auto-derived `BorshSerialize` is `infallible` for plain
//!   structs and enums (no custom impls in `canonical_intent.rs`).
//!
//! - **`solana_program::hash::hashv`**: SHA-256, returns a 32-byte
//!   `Hash`. Cited verbatim in `programs/clawsol-intent/src/lib.rs`
//!   line 36 doc-comment ("`solana_program::hash::hashv` is SHA-256,
//!   returns 32-byte `Hash`"). Confirmed against
//!   [`solana-program` 2.1.21 docs.rs entry][1] and the Agave source
//!   at `agave/sdk/program/src/hash.rs`. Both `hashv(&[bytes])` and
//!   `Sha256::new().update(bytes).finalize()` reduce to the same
//!   FIPS 180-4 SHA-256 over the same input bytes — the `&[&[u8]]`
//!   slice form simply iterates `update(...)` calls without
//!   intermediate padding, which is identity-equivalent to a single
//!   update on the concatenation.
//!
//!   [1]: https://docs.rs/solana-program/2.1.21/solana_program/hash/fn.hashv.html
//!
//! - **`sha2::Sha256`**: FIPS 180-4 SHA-256. The `RustCrypto/hashes`
//!   crate group is the upstream; `sha2 = "0.10"` is in
//!   `crates/types/Cargo.toml`. The output is a 32-byte digest, same
//!   as `solana_program::hash::Hash`.
//!
//! Both `hashv` and `Sha256` reduce to the same NIST-standardised
//! function over the same input bytes; this test is the operational
//! check that no pre-image padding or domain-separation drift exists
//! between the two crates' implementations.

use claw_types::canonical_intent::{
    canonical_bytes, canonical_hash, CanonicalIntent, IntentAction, PubkeyBytes,
    STAGE1_TAIL_SCHEMA_VERSION,
};
use solana_program::hash::hashv;

// ── Agent B fixture constants — copied verbatim from
//     `crates/types/src/canonical_intent.rs` test module. The
//     identifiers and values MUST match Agent B's source so that
//     this test's fixture builder produces the same `CanonicalIntent`
//     bytes that Agent B's `solend_deposit_fixture_hash_is_stable`
//     test pins to. Drift here would cause both this test AND Agent
//     B's hash-stability test to fail in lock-step — surfacing the
//     drift, not hiding it.

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const HCKRV_OBLIGATION: &str = "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV";
const TEST_WALLET: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

const FIXTURE_INTENT_ID: [u8; 16] =
    [0x57, 0x41, 0x4C, 0x4C, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];
const FIXTURE_EXPIRES_AT: u64 = 415_500_000;

// Committed expected SHA-256 hex digests from Agent B's stability
// tests. If any of these change, EITHER Agent B's source has drifted
// OR our fixture builder has drifted — both surface as test failures
// here, not silent mismatches.
const EXPECTED_HEX_SOLEND_DEPOSIT: &str =
    "0e2cbf3f57400e84a548c5a142d9e246214c1b26b85a75e9cca65cbfe8965bf1";
const EXPECTED_HEX_SOLEND_WITHDRAW_ALL: &str =
    "05a840f284b9c7b2163a1862806fc83a5b05127ac2e1833d5394c60b3b2ecf7a";
const EXPECTED_HEX_JUPITER_SWAP: &str =
    "a9de08671a9d6811a142723043a43223a3e7a8bd567d6476d7c5821b1286ca6d";

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

/// Convergence assertion factored once. For each fixture:
///   - serialise once via claw-types
///   - compute backend hash via claw-types' SHA-256 (sha2)
///   - compute on-chain hash via `solana_program::hash::hashv` over the
///     SAME bytes
///   - assert byte-identical
///   - assert both equal the committed expected hex
fn assert_convergence(intent: &CanonicalIntent, expected_hex: &str) {
    let bytes = canonical_bytes(intent);
    let backend = canonical_hash(intent);
    let onchain = hashv(&[&bytes]).to_bytes();
    assert_eq!(
        backend, onchain,
        "claw-types canonical_hash must equal solana_program hashv for the SAME canonical bytes"
    );
    assert_eq!(
        hex::encode(backend),
        expected_hex,
        "fixture's canonical_hash must equal the committed Agent B expected hex; \
         drift here means EITHER our fixture builder OR claw-types' source has changed"
    );
}

#[test]
fn solend_deposit_backend_hash_equals_program_hash() {
    assert_convergence(&fixture_solend_deposit(), EXPECTED_HEX_SOLEND_DEPOSIT);
}

#[test]
fn solend_withdraw_all_backend_hash_equals_program_hash() {
    assert_convergence(
        &fixture_solend_withdraw_all(),
        EXPECTED_HEX_SOLEND_WITHDRAW_ALL,
    );
}

#[test]
fn jupiter_swap_backend_hash_equals_program_hash() {
    assert_convergence(&fixture_jupiter_swap(), EXPECTED_HEX_JUPITER_SWAP);
}

/// Sanity check that re-running canonical_bytes is deterministic.
/// Borsh is deterministic by spec, but exercising this concretely
/// makes the test self-explanatory if the convergence assertion ever
/// fires — a maintainer can rule out non-determinism on the bytes
/// side first.
#[test]
fn canonical_bytes_are_deterministic_across_calls() {
    let intent = fixture_solend_deposit();
    let a = canonical_bytes(&intent);
    let b = canonical_bytes(&intent);
    assert_eq!(a, b, "canonical_bytes must be deterministic across calls");
}

/// Sanity check that the two intent kinds with different payload
/// shapes produce different bytes (and therefore different hashes).
/// This is not a security property — Borsh's enum tag already
/// disambiguates — but a regression here would indicate a serious
/// schema collapse and is cheap to assert.
#[test]
fn three_fixtures_produce_three_distinct_hashes() {
    let h1 = canonical_hash(&fixture_solend_deposit());
    let h2 = canonical_hash(&fixture_solend_withdraw_all());
    let h3 = canonical_hash(&fixture_jupiter_swap());
    assert_ne!(h1, h2, "deposit and withdraw must have distinct hashes");
    assert_ne!(h1, h3, "deposit and swap must have distinct hashes");
    assert_ne!(h2, h3, "withdraw and swap must have distinct hashes");
}
