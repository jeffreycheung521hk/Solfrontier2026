//! Stage 2 P5c — Demo Pinned Oracle Allowlist (Mode C) tests.
//!
//! # Scope
//!
//! These tests cover [`MAINNET_BETA_DEMO_USDC_TUPLE`] +
//! [`verify_demo_pinned_oracle_tuple`] +
//! [`select_oracle_mode_for_runtime_tuple`], the P5c additions in
//! `solend_cpi_builder.rs` that lift P5b-2's Mode B fail-closed gate
//! for exactly one mainnet-beta tuple (the canonical Solend Save Main
//! Pool USDC reserve `BgxfHJ…`).
//!
//! # WARNING: STAGE 2 DEMO ONLY
//!
//! The pinned tuple is NOT a production oracle resolver and NOT a
//! dynamic reserve parser. The P5c guarantee is narrow:
//!
//! - The runtime tuple equals [`MAINNET_BETA_DEMO_USDC_TUPLE`] in every
//!   field → `select_oracle_mode_for_runtime_tuple` returns Mode C and
//!   the orchestrator falls through to the Phase 2 CPI helpers.
//! - Any field disagreement → fail-closed (Mode B; error 85 from the
//!   orchestrator). When the verifier is called directly, the more
//!   specific error 88 / 89 fires.
//!
//! # No live network
//!
//! These tests run as pure unit tests against the in-process pubkey
//! comparator. No `RpcClient`. No `reqwest`. No `signTransaction`. No
//! `sendRawTransaction`. No Jupiter. The `no_live_rpc_or_fetch_paths`
//! and `no_jupiter_paths` source-grep tests verify this property
//! against the actual changed source files.

use clawsol_authority::{
    error::AuthorityError,
    solend_cpi_builder::{
        select_oracle_mode_for_runtime_tuple, verify_demo_pinned_oracle_tuple,
        DemoOracleTuple, SolendOracleMode, MAINNET_BETA_DEMO_USDC_TUPLE,
    },
};
use solana_program::{program_error::ProgramError, pubkey::Pubkey};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Convenience: deterministic 32-byte non-pinned pubkey (used as a
/// "wrong" value that cannot accidentally collide with the pinned
/// allowlist — `0xCC` repeated 32 times is not a base58-decoded
/// pubkey of any in-repo constant).
fn wrong(b: u8) -> Pubkey {
    Pubkey::new_from_array([b; 32])
}

fn assert_err(actual: ProgramError, expected: AuthorityError) {
    assert_eq!(
        actual,
        ProgramError::Custom(expected as u32),
        "expected {expected:?} (code {})",
        expected as u32,
    );
}

// ── Test 1: demo_pinned_tuple_passes_oracle_validation ─────────────────────

#[test]
fn demo_pinned_tuple_passes_oracle_validation() {
    // Positive path: pass the exact pinned tuple's pubkeys (read from
    // the canonical const) and assert the verifier returns Ok and the
    // mode-selector returns DemoPinnedAllowlist.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;

    verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        // Coerce demo.extra_oracle (Pubkey::default()) ↔ Option::None.
        None,
    )
    .expect("pinned demo tuple must pass verification");

    let mode = select_oracle_mode_for_runtime_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    );
    assert_eq!(mode, SolendOracleMode::DemoPinnedAllowlist);
}

// ── Test 2: correct_oracle_wrong_reserve_rejected ─────────────────────────

#[test]
fn correct_oracle_wrong_reserve_rejected() {
    // Correct oracle pubkeys, wrong reserve → unknown reserve error (88).
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_reserve = wrong(0x01);

    let err = verify_demo_pinned_oracle_tuple(
        &bogus_reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleUnknownReserve);
}

// ── Test 3: correct_reserve_wrong_oracle_rejected ─────────────────────────

#[test]
fn correct_reserve_wrong_oracle_rejected() {
    // Correct reserve, wrong pyth oracle → tuple mismatch error (89).
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_pyth = wrong(0xAA);

    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &bogus_pyth,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);

    // Also: wrong switchboard
    let bogus_sb = wrong(0xBB);
    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &bogus_sb,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 4: correct_reserve_wrong_lending_market_rejected ─────────────────

#[test]
fn correct_reserve_wrong_lending_market_rejected() {
    // Correct reserve, wrong lending_market → tuple mismatch error (89).
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_lm = wrong(0x02);

    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &bogus_lm,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 5: correct_reserve_wrong_liquidity_mint_rejected ─────────────────

#[test]
fn correct_reserve_wrong_liquidity_mint_rejected() {
    // Correct reserve, wrong liquidity_mint (should be USDC) → mismatch.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_mint = wrong(0x03);

    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &bogus_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 6: correct_reserve_wrong_ctoken_mint_rejected ────────────────────

#[test]
fn correct_reserve_wrong_ctoken_mint_rejected() {
    // Correct reserve, wrong cToken mint (should be cUSDC) → mismatch.
    // This is the field the operator specifically authorized in-place
    // pinning for during the May 2026 demo window; the test guards
    // against silent drift.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_ctoken = wrong(0x04);

    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &bogus_ctoken,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 7: unknown_reserve_rejected_even_with_known_oracle ───────────────

#[test]
fn unknown_reserve_rejected_even_with_known_oracle() {
    // Hostile case: an unknown reserve, but every other field comes
    // from the demo allowlist. The reserve check fires FIRST and we
    // surface SolendDemoOracleUnknownReserve (NOT TupleMismatch).
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_reserve = wrong(0xBE);

    let err = verify_demo_pinned_oracle_tuple(
        &bogus_reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleUnknownReserve);
}

// ── Test 8: known_mint_unknown_reserve_rejected ───────────────────────────

#[test]
fn known_mint_unknown_reserve_rejected() {
    // Variation on test 7: the known liquidity_mint (USDC) by itself
    // is not enough to bypass the reserve gate.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let bogus_reserve = wrong(0xBF);

    let err = verify_demo_pinned_oracle_tuple(
        &bogus_reserve,
        &wrong(0xC0),
        &demo.liquidity_mint, // known USDC mint
        &wrong(0xC1),
        &wrong(0xC2),
        &wrong(0xC3),
        None,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleUnknownReserve);
}

// ── Test 9: mode_reports_demo_pinned_allowlist ────────────────────────────

#[test]
fn mode_reports_demo_pinned_allowlist() {
    // The mode-selector returns DemoPinnedAllowlist for the exact
    // pinned tuple and FailClosed for any deviation.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;

    assert_eq!(
        select_oracle_mode_for_runtime_tuple(
            &demo.reserve,
            &demo.lending_market,
            &demo.liquidity_mint,
            &demo.ctoken_mint,
            &demo.pyth_oracle,
            &demo.switchboard_oracle,
            None,
        ),
        SolendOracleMode::DemoPinnedAllowlist,
    );
}

// ── Test 10: non_demo_tuple_remains_fail_closed ───────────────────────────

#[test]
fn non_demo_tuple_remains_fail_closed() {
    // Any non-demo tuple → FailClosed at the mode-selector layer (the
    // orchestrator then surfaces error 85).
    assert_eq!(
        select_oracle_mode_for_runtime_tuple(
            &wrong(0x01),
            &wrong(0x02),
            &wrong(0x03),
            &wrong(0x04),
            &wrong(0x05),
            &wrong(0x06),
            None,
        ),
        SolendOracleMode::FailClosed,
    );

    // Also: a tuple that's "almost" demo (one wrong field) stays
    // FailClosed.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    assert_eq!(
        select_oracle_mode_for_runtime_tuple(
            &demo.reserve,
            &demo.lending_market,
            &demo.liquidity_mint,
            &demo.ctoken_mint,
            &demo.pyth_oracle,
            &wrong(0xFF), // wrong switchboard
            None,
        ),
        SolendOracleMode::FailClosed,
    );
}

// ── Test 11: localnet_clone_uses_source_network_pubkeys_not_random_localnet_keys ─

#[test]
fn localnet_clone_uses_source_network_pubkeys_not_random_localnet_keys() {
    // P5c targets mainnet-beta directly (no localnet clone). This test
    // verifies we did NOT use random-localnet pubkeys: every pinned
    // field equals a known mainnet-beta literal. We re-decode the
    // base58 strings here independently and assert byte equality
    // against the const. If anyone replaces a literal with a
    // randomly-generated localnet key, this test fails loudly.
    let expected_reserve = bs58_to_pubkey("BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw");
    let expected_lm = bs58_to_pubkey("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY");
    let expected_liq_mint = bs58_to_pubkey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    let expected_ctoken = bs58_to_pubkey("993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk");
    let expected_pyth = bs58_to_pubkey("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX");
    let expected_sb = bs58_to_pubkey("nu11111111111111111111111111111111111111111");
    let expected_extra = Pubkey::new_from_array([0u8; 32]);

    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    assert_eq!(demo.reserve, expected_reserve, "reserve drift");
    assert_eq!(demo.lending_market, expected_lm, "lending_market drift");
    assert_eq!(demo.liquidity_mint, expected_liq_mint, "liquidity_mint drift");
    assert_eq!(demo.ctoken_mint, expected_ctoken, "ctoken_mint drift");
    assert_eq!(demo.pyth_oracle, expected_pyth, "pyth_oracle drift");
    assert_eq!(demo.switchboard_oracle, expected_sb, "switchboard drift");
    assert_eq!(demo.extra_oracle, expected_extra, "extra_oracle drift");
}

/// Tiny independent base58 decoder for the test layer. Avoids pulling
/// in `bs58` as a new dev-dependency. Decodes strings of length up to
/// 44 chars into a 32-byte Pubkey. Panics on invalid input — this is
/// test-only code.
fn bs58_to_pubkey(s: &str) -> Pubkey {
    // Use solana_program's pubkey type with a from_str via FromStr.
    use core::str::FromStr;
    Pubkey::from_str(s).expect("valid base58 pubkey")
}

// ── Test 12: demo_tuple_struct_comparison_rejects_misaligned_tuple ────────

#[test]
fn demo_tuple_struct_comparison_rejects_misaligned_tuple() {
    // Build a misaligned `DemoOracleTuple` (correct reserve but every
    // other field zeroed) and assert the verifier surfaces a
    // tuple-mismatch error. Also sanity-check that the struct is
    // value-comparable (PartialEq) against the canonical const.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let zero = Pubkey::new_from_array([0u8; 32]);

    let misaligned = DemoOracleTuple {
        reserve: demo.reserve,
        lending_market: zero,
        liquidity_mint: zero,
        ctoken_mint: zero,
        pyth_oracle: zero,
        switchboard_oracle: zero,
        extra_oracle: zero,
    };
    assert_ne!(misaligned, *demo, "misaligned tuple must not equal demo");

    let err = verify_demo_pinned_oracle_tuple(
        &misaligned.reserve,
        &misaligned.lending_market,
        &misaligned.liquidity_mint,
        &misaligned.ctoken_mint,
        &misaligned.pyth_oracle,
        &misaligned.switchboard_oracle,
        Some(&misaligned.extra_oracle),
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 13: default_unused_extra_oracle_does_not_trigger_owner_validation ─

#[test]
fn default_unused_extra_oracle_does_not_trigger_owner_validation() {
    // The verifier only ever pubkey-compares extra_oracle. It does NOT
    // dereference an AccountInfo or run owner/data validation when
    // extra_oracle is `Pubkey::default()`. We exercise both shapes —
    // `None` (coerced to default) and `Some(&Pubkey::default())` — and
    // assert both succeed for the demo tuple. If a future refactor
    // accidentally added an owner-check, it would crash here because
    // we never wire an AccountInfo.
    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let default_key = Pubkey::default();

    // Variant A: None
    verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        None,
    )
    .expect("None must coerce to Pubkey::default()");

    // Variant B: Some(&Pubkey::default()) — the verifier compares the
    // copied pubkey, not an AccountInfo, and the demo's extra_oracle
    // is itself Pubkey::default().
    verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        Some(&default_key),
    )
    .expect("Some(default) must equal demo.extra_oracle");

    // Variant C: Some(&non-default) — must fail with TupleMismatch.
    let bogus = wrong(0x77);
    let err = verify_demo_pinned_oracle_tuple(
        &demo.reserve,
        &demo.lending_market,
        &demo.liquidity_mint,
        &demo.ctoken_mint,
        &demo.pyth_oracle,
        &demo.switchboard_oracle,
        Some(&bogus),
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendDemoOracleTupleMismatch);
}

// ── Test 14: no_live_rpc_or_fetch_paths ──────────────────────────────────

#[test]
fn no_live_rpc_or_fetch_paths() {
    // Source-grep over the production source files this slice owns or
    // touches. Assert that none of the blocked symbols appear. These
    // strings are matched as substrings — to avoid this test
    // self-matching, we build the search needles char-by-char so the
    // file's source bytes don't contain the literal substring this
    // test scans for.
    //
    // Files scanned: solend_cpi_builder.rs, error.rs (the two changed
    // production files).

    let src_builder = include_str!("../src/solend_cpi_builder.rs");
    let src_error = include_str!("../src/error.rs");

    // Production-only slice: drop everything from the first
    // `#[cfg(test)]` marker onward (the existing P5b-1 in-file test
    // module legitimately scans for banned needles via char-array
    // patterns like `pat_broadcast`, which would self-trigger if we
    // included them). Then strip comment lines so doc-comments that
    // reference banned symbols when explaining what the module
    // explicitly does NOT do (e.g. "No `RpcClient`. No broadcast.")
    // do not self-trigger either. Mirrors the strategy used by P5b-2's
    // `no_invoke_signed_path_in_p5b2` test.
    fn production_executable(src: &str) -> String {
        let cutoff = src
            .find("#[cfg(test)]")
            .unwrap_or(src.len());
        let prod = &src[..cutoff];
        prod.lines()
            .filter(|line| {
                let t = line.trim_start();
                !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with("*")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    let exec_builder = production_executable(src_builder);
    let exec_error = production_executable(src_error);

    // Build needles char-by-char so this test's own source doesn't
    // contain the literal substrings.
    let needle_reqwest: String = ['r', 'e', 'q', 'w', 'e', 's', 't'].iter().collect();
    let needle_rpcclient: String = ['R', 'p', 'c', 'C', 'l', 'i', 'e', 'n', 't'].iter().collect();
    let needle_jup_api: String = ['a', 'p', 'i', '.', 'j', 'u', 'p', '.', 'a', 'g'].iter().collect();
    let needle_send_tx: String =
        ['s', 'e', 'n', 'd', '_', 't', 'r', 'a', 'n', 's', 'a', 'c', 't', 'i', 'o', 'n']
            .iter()
            .collect();
    let needle_send_raw: String = [
        's', 'e', 'n', 'd', 'R', 'a', 'w', 'T', 'r', 'a', 'n', 's', 'a', 'c', 't', 'i', 'o', 'n',
    ]
    .iter()
    .collect();
    let needle_sign_tx: String =
        ['s', 'i', 'g', 'n', 'T', 'r', 'a', 'n', 's', 'a', 'c', 't', 'i', 'o', 'n']
            .iter()
            .collect();
    let needle_broadcast: String =
        ['b', 'r', 'o', 'a', 'd', 'c', 'a', 's', 't'].iter().collect();

    for (name, src) in [
        ("solend_cpi_builder.rs", exec_builder.as_str()),
        ("error.rs", exec_error.as_str()),
    ] {
        assert!(
            !src.contains(&needle_reqwest),
            "{name} (executable lines) must not reference {needle_reqwest}",
        );
        assert!(
            !src.contains(&needle_rpcclient),
            "{name} (executable lines) must not reference {needle_rpcclient}",
        );
        assert!(
            !src.contains(&needle_jup_api),
            "{name} (executable lines) must not reference {needle_jup_api}",
        );
        assert!(
            !src.contains(&needle_send_tx),
            "{name} (executable lines) must not reference {needle_send_tx}",
        );
        assert!(
            !src.contains(&needle_send_raw),
            "{name} (executable lines) must not reference {needle_send_raw}",
        );
        assert!(
            !src.contains(&needle_sign_tx),
            "{name} (executable lines) must not reference {needle_sign_tx}",
        );
        assert!(
            !src.contains(&needle_broadcast),
            "{name} (executable lines) must not reference {needle_broadcast}",
        );
    }
}

// ── Test 15: no_jupiter_paths ────────────────────────────────────────────

#[test]
fn no_jupiter_paths() {
    // Source-grep specifically for Jupiter-related symbols in
    // `solend_cpi_builder.rs`. Mirrors test 14's needle-build pattern
    // so the test itself doesn't self-match. Slice off the existing
    // in-file `#[cfg(test)]` module and strip comments first.
    let src_builder = include_str!("../src/solend_cpi_builder.rs");
    let cutoff = src_builder
        .find("#[cfg(test)]")
        .unwrap_or(src_builder.len());
    let prod = &src_builder[..cutoff];
    let exec_builder: String = prod
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with("*")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let needle_jupiter_word: String =
        ['J', 'u', 'p', 'i', 't', 'e', 'r'].iter().collect();
    let needle_jup_api: String = ['a', 'p', 'i', '.', 'j', 'u', 'p', '.', 'a', 'g'].iter().collect();

    // solend_cpi_builder.rs: must not contain Jupiter or api.jup.ag in
    // executable production code.
    assert!(
        !exec_builder.contains(&needle_jupiter_word),
        "solend_cpi_builder.rs (executable lines) must not reference {needle_jupiter_word}",
    );
    assert!(
        !exec_builder.contains(&needle_jup_api),
        "solend_cpi_builder.rs (executable lines) must not reference {needle_jup_api}",
    );

    // error.rs holds the Jupiter boundary error variants from J4 (and
    // P5c does NOT touch them). The pre-existing Jupiter*-prefixed
    // variants are explicitly OK; this test only guards
    // `solend_cpi_builder.rs` for new Jupiter references.
}
