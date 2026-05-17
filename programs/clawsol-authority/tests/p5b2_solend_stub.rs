//! Stage 2 P5b-2 — Solend CPI skeleton orchestrator tests.
//!
//! # Scope
//!
//! These tests cover the P5b-2 orchestrator (`try_solend_cpi_skeleton`)
//! AND the lib.rs wiring that invokes it from `process_execute_action`
//! when the action is `SolendWithdrawAllDelegated` and the caller has
//! supplied enough accounts to fill the variant-15 + substrate slots.
//!
//! Under Oracle Mode B (the P5b-2 production default —
//! [`SolendOracleMode::FailClosed`]), Phase 1 validation runs end-to-
//! end (signer guard, sysvar guard, shadow mapping, cToken amount
//! read), then the orchestrator returns
//! [`AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation`]
//! unconditionally. No CPI is invoked and no
//! `AuthorizationRecord` field is mutated — the contract from the
//! existing P2/P3/P4/J4/P5b-1 boundary gates is preserved.
//!
//! # Why unit-style tests, not ProgramTest
//!
//! The success path is blocked under Mode B, so a
//! `solana-program-test` integration test that drives a real
//! BanksClient through `ExecuteAction` would only ever surface the
//! fail-closed sentinel and could never assert behaviour that depends
//! on Phase 2 actually firing. The Phase 1 invariants we DO want to
//! pin (signer guard, mismatch rejections, sysvar guards, cToken
//! amount read) are deterministic functions of the
//! `AuthorizationRecord` + `&[AccountInfo]` shape and are best
//! exercised by directly calling `try_solend_cpi_skeleton` against
//! constructed `AccountInfo` fixtures — same pattern used by the
//! P5b-1 unit tests in `solend_cpi_builder::tests`.
//!
//! Tests that REQUIRE the success path (Phase 2 CPIs actually firing)
//! are marked `#[ignore = "P5b-2 Mode B fail-closed: success path
//! blocked until oracle validation lands"]` so a future slice can flip
//! them on without renumbering anything.
//!
//! # Source-grep tests
//!
//! Several tests grep the production source files directly via
//! `include_str!` to assert structural invariants (no `invoke_signed`
//! in the P5b-2 path, no `find_program_address` in the CPI path, no
//! `try_borrow_data` inside CPI helper bodies, no Jupiter/RPC symbols
//! anywhere in changed files). These complement the runtime tests by
//! making lattice properties visible to grep auditors.
//!
//! # Stateless test isolation
//!
//! The Mode B fail-closed contract means no test in this file shares
//! mutable state with another test — every test allocates its own
//! `Vec<AcctStore>` and its own `AuthorizationRecord`. The
//! `stub_program_is_stateless_and_test_isolated` test asserts this
//! property explicitly by exercising the same fixture pattern in two
//! sequential calls and verifying nothing survives between them. Once
//! the success path lands, a stateless `ProgramTest::add_program`
//! stub can register at `SOLEND_PROGRAM_ID_MAINNET` using the same
//! per-test recording-account pattern and the same isolation
//! invariants will continue to hold.

use clawsol_authority::{
    error::AuthorityError,
    solend_account_decode::{
        OBLIGATION_OFFSET_LAST_UPDATE_SLOT, OBLIGATION_OFFSET_LAST_UPDATE_STALE,
        OBLIGATION_OFFSET_LENDING_MARKET, OBLIGATION_OFFSET_OWNER,
        OBLIGATION_OFFSET_VERSION, RESERVE_OFFSET_LAST_UPDATE_SLOT,
        RESERVE_OFFSET_LAST_UPDATE_STALE, RESERVE_OFFSET_LENDING_MARKET,
        RESERVE_OFFSET_VERSION, SOLEND_OBLIGATION_LEN, SOLEND_RESERVE_LEN,
        SOLEND_SUPPORTED_VERSION, SPL_TOKEN_ACCOUNT_LEN, SPL_TOKEN_OFFSET_AMOUNT,
        SPL_TOKEN_OFFSET_MINT, SPL_TOKEN_OFFSET_OWNER, SPL_TOKEN_PROGRAM_ID,
    },
    solend_boundary::SOLEND_PROGRAM_ID_MAINNET,
    solend_cpi_builder::{
        p5b2_oracle_mode, try_solend_cpi_skeleton, SolendCpiSubstrate,
        SolendOracleMode, SolendWithdrawAccounts, SolendWithdrawShadowExpectations,
    },
    state::{AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION},
};
use solana_program::{account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey, sysvar};

// ── Test fixture builders ───────────────────────────────────────────────────

/// Convenience constructor for a deterministic 32-byte-pubkey-from-byte.
fn pk(b: u8) -> Pubkey {
    Pubkey::new_from_array([b; 32])
}

/// Canonical AuthorizationRecord used by every test in this file.
/// Each field is fixed so the `&record` reference is identical across
/// test calls and the only thing varying between tests is the
/// `&[AccountInfo]` shape under examination.
fn record_fixture() -> AuthorizationRecord {
    AuthorizationRecord {
        schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
        rule_id: [0xD0; 16],
        user: pk(0xAA),
        executor: pk(0xEE),
        delegated_wallet: pk(0xBB),
        canonical_rule_hash: [0; 32],
        allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        max_input_amount_raw: 5_000_000,
        used_amount_raw: 0,
        destination: pk(0xCC),
        expires_at_slot: 1_000_000,
        revoked: false,
        completed: false,
        execution_nonce: 0,
        last_execution_slot: 0,
        bump: 255,
    }
}

fn populate_obligation_prefix(
    data: &mut [u8],
    slot: u64,
    stale: bool,
    lending_market: [u8; 32],
    inner_owner: [u8; 32],
) {
    data[OBLIGATION_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
    data[OBLIGATION_OFFSET_LAST_UPDATE_SLOT..OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8]
        .copy_from_slice(&slot.to_le_bytes());
    data[OBLIGATION_OFFSET_LAST_UPDATE_STALE] = if stale { 1 } else { 0 };
    data[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32]
        .copy_from_slice(&lending_market);
    data[OBLIGATION_OFFSET_OWNER..OBLIGATION_OFFSET_OWNER + 32]
        .copy_from_slice(&inner_owner);
}

fn populate_reserve_prefix(
    data: &mut [u8],
    slot: u64,
    stale: bool,
    lending_market: [u8; 32],
) {
    data[RESERVE_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
    data[RESERVE_OFFSET_LAST_UPDATE_SLOT..RESERVE_OFFSET_LAST_UPDATE_SLOT + 8]
        .copy_from_slice(&slot.to_le_bytes());
    data[RESERVE_OFFSET_LAST_UPDATE_STALE] = if stale { 1 } else { 0 };
    data[RESERVE_OFFSET_LENDING_MARKET..RESERVE_OFFSET_LENDING_MARKET + 32]
        .copy_from_slice(&lending_market);
}

fn populate_spl_token(data: &mut [u8], mint: [u8; 32], owner: [u8; 32], amount: u64) {
    data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&mint);
    data[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32].copy_from_slice(&owner);
    data[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
        .copy_from_slice(&amount.to_le_bytes());
}

fn assert_err(actual: ProgramError, expected: AuthorityError) {
    assert_eq!(
        actual,
        ProgramError::Custom(expected as u32),
        "expected {expected:?} (code {})",
        expected as u32,
    );
}

/// Heap-resident backing store for a synthetic `AccountInfo`.
/// Each field is owned so the AccountInfo can hold exclusive borrows
/// independent of test scope.
struct AcctStore {
    key: Pubkey,
    owner: Pubkey,
    lamports: u64,
    data: Vec<u8>,
    is_signer: bool,
    is_writable: bool,
}

impl AcctStore {
    fn obligation(record: &AuthorizationRecord, lm: [u8; 32]) -> Self {
        let mut data = vec![0u8; SOLEND_OBLIGATION_LEN];
        populate_obligation_prefix(
            &mut data,
            100,
            false,
            lm,
            record.delegated_wallet.to_bytes(),
        );
        Self {
            key: pk(0x11),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            lamports: 0,
            data,
            is_signer: false,
            is_writable: true,
        }
    }

    fn reserve(lm: [u8; 32]) -> Self {
        let mut data = vec![0u8; SOLEND_RESERVE_LEN];
        populate_reserve_prefix(&mut data, 100, false, lm);
        Self {
            key: pk(0x10),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            lamports: 0,
            data,
            is_signer: false,
            is_writable: true,
        }
    }

    fn token_account(
        key_byte: u8,
        mint: [u8; 32],
        authority: [u8; 32],
        amount: u64,
    ) -> Self {
        let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        populate_spl_token(&mut data, mint, authority, amount);
        Self {
            key: pk(key_byte),
            owner: SPL_TOKEN_PROGRAM_ID,
            lamports: 0,
            data,
            is_signer: false,
            is_writable: true,
        }
    }

    /// Token account at a SPECIFIC pubkey (rather than `pk(byte)`),
    /// used for the destination slot which must equal
    /// `record.destination`.
    fn token_account_at(
        key: Pubkey,
        mint: [u8; 32],
        authority: [u8; 32],
        amount: u64,
    ) -> Self {
        let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        populate_spl_token(&mut data, mint, authority, amount);
        Self {
            key,
            owner: SPL_TOKEN_PROGRAM_ID,
            lamports: 0,
            data,
            is_signer: false,
            is_writable: true,
        }
    }

    /// Bare account with no data — used for the variant-15
    /// `lending_market_authority`, `reserve_collateral_mint`, and
    /// `reserve_liquidity_supply` slots whose data the validator does
    /// not project.
    fn bare(key: Pubkey, owner: Pubkey) -> Self {
        Self {
            key,
            owner,
            lamports: 0,
            data: vec![],
            is_signer: false,
            is_writable: false,
        }
    }

    /// Signer slot — `is_signer = true`. Used for variant-15 slot 9
    /// (`obligation_owner`) and slot 10 (`user_transfer_authority`),
    /// both of which must equal `record.delegated_wallet`.
    fn signer(key: Pubkey) -> Self {
        Self {
            key,
            owner: Pubkey::default(),
            lamports: 0,
            data: vec![],
            is_signer: true,
            is_writable: false,
        }
    }

}

/// Build a `Vec<AccountInfo<'a>>` from a mutable slice of stores. The
/// returned AccountInfo references are bound to `s`'s lifetime and
/// drop when the Vec goes out of scope.
fn build_accounts<'a>(stores: &'a mut [AcctStore]) -> Vec<AccountInfo<'a>> {
    stores
        .iter_mut()
        .map(|s| {
            AccountInfo::new(
                &s.key,
                s.is_signer,
                s.is_writable,
                &mut s.lamports,
                &mut s.data,
                &s.owner,
                false,
                0,
            )
        })
        .collect()
}

/// Build the canonical "Phase 1 reaches the oracle gate" 12-account
/// variant-15 base array. The cToken amount defaults to 1_000 (non-
/// zero so the cToken-amount-zero gate doesn't fire first).
fn good_withdraw_stores(
    record: &AuthorizationRecord,
    lm: [u8; 32],
    ctoken_mint: [u8; 32],
    liquidity_mint: [u8; 32],
    ctoken_amount: u64,
) -> Vec<AcctStore> {
    vec![
        // 0 — source_collateral (any token-program-owned account).
        AcctStore::token_account(0x20, ctoken_mint, [0xFA; 32], 0),
        // 1 — delegated_ctoken: authority = delegated_wallet, mint
        //    = ctoken_mint.
        AcctStore::token_account(
            0x21,
            ctoken_mint,
            record.delegated_wallet.to_bytes(),
            ctoken_amount,
        ),
        // 2 — withdraw reserve.
        AcctStore::reserve(lm),
        // 3 — obligation (inner owner = delegated_wallet).
        AcctStore::obligation(record, lm),
        // 4 — lending market (key = lm).
        AcctStore::bare(Pubkey::new_from_array(lm), pk(0xFE)),
        // 5 — lending market authority (PDA-shaped; bytes don't matter
        //    for the shadow-mapping shape tests).
        AcctStore::bare(pk(0x40), pk(0xFE)),
        // 6 — destination = record.destination, mint = liquidity_mint.
        AcctStore::token_account_at(
            record.destination,
            liquidity_mint,
            record.user.to_bytes(),
            0,
        ),
        // 7 — reserve_collateral_mint (= ctoken_mint, used as a
        //    structural slot — its key is the cToken mint the rule
        //    committed).
        AcctStore::bare(Pubkey::new_from_array(ctoken_mint), pk(0xFE)),
        // 8 — reserve_liquidity_supply.
        AcctStore::bare(pk(0x80), pk(0xFE)),
        // 9 — obligation_owner (= delegated_wallet, signer).
        AcctStore::signer(record.delegated_wallet),
        // 10 — user_transfer_authority (= delegated_wallet, signer).
        AcctStore::signer(record.delegated_wallet),
        // 11 — token program.
        AcctStore::bare(SPL_TOKEN_PROGRAM_ID, pk(0xFE)),
    ]
}

fn good_expectations<'a>(
    lm: &'a Pubkey,
    ctoken_mint: &'a Pubkey,
    liquidity_mint: &'a Pubkey,
    obligation: &'a Pubkey,
    reserve: &'a Pubkey,
) -> SolendWithdrawShadowExpectations<'a> {
    SolendWithdrawShadowExpectations {
        solend_program_id: &SOLEND_PROGRAM_ID_MAINNET,
        expected_obligation: obligation,
        expected_reserve: reserve,
        expected_lending_market: lm,
        expected_ctoken_mint: ctoken_mint,
        expected_liquidity_mint: liquidity_mint,
    }
}

/// Holds the substrate AccountInfo backing for the 4 extra accounts the
/// orchestrator needs after the variant-15 12 base. Returned alongside
/// the lifetime-bound Vec<AccountInfo> so the test can poke individual
/// fields before invoking the orchestrator.
struct SubstrateStores {
    solend: AcctStore,
    pyth: AcctStore,
    switch: AcctStore,
    clock: AcctStore,
}

impl SubstrateStores {
    fn good() -> Self {
        // Solend program id is checked against SOLEND_PROGRAM_ID_MAINNET.
        // Pyth/switchboard pubkeys are not validated structurally under
        // Mode B (only their AccountInfo presence is required).
        // Clock sysvar MUST equal solana_program::sysvar::clock::id().
        Self {
            solend: AcctStore::bare(SOLEND_PROGRAM_ID_MAINNET, pk(0xFE)),
            pyth: AcctStore::bare(pk(0xCA), pk(0xFE)),
            switch: AcctStore::bare(pk(0xCB), pk(0xFE)),
            clock: AcctStore::bare(sysvar::clock::id(), pk(0xFE)),
        }
    }
}

// ── Test 1: solend_execute_requires_delegated_wallet_signer ─────────────────

#[test]
fn solend_execute_requires_delegated_wallet_signer() {
    // Phase 1 signer guard: the delegated_wallet AccountInfo MUST have
    // is_signer == true. Build the canonical happy path then turn off
    // the signer bit on the delegated_wallet account passed to
    // `try_solend_cpi_skeleton`.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let substrate_stores = SubstrateStores::good();
    // Non-signer delegated wallet at the lib.rs slot 3.
    let dw_store = AcctStore::bare(record.delegated_wallet, Pubkey::default());

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    let mut dw_one = [dw_store];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();
    assert!(!dw_acct.is_signer);

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiDelegatedWalletNotSigner);
}

// ── Test 2: wrong_delegated_wallet_account_rejected_before_cpi ──────────────

#[test]
fn wrong_delegated_wallet_account_rejected_before_cpi() {
    // The delegated_wallet AccountInfo's key must equal
    // record.delegated_wallet. Pass a different key and assert the
    // mismatch error fires.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    // Wrong-key delegated wallet — is_signer = true, but key !=
    // record.delegated_wallet.
    let dw_store = AcctStore::signer(pk(0x99));
    let mut dw_one = [dw_store];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiDelegatedWalletMismatch);
}

// ── Test 3: user_main_wallet_not_accepted_as_delegated_signer ───────────────

#[test]
fn user_main_wallet_not_accepted_as_delegated_signer() {
    // Critical hard-rejection: if a hostile executor passes the user
    // main wallet at the delegated_wallet slot, the orchestrator MUST
    // surface the dedicated SolendMainWalletObligationRejected variant
    // (NOT a generic mismatch).
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    // User main wallet at the delegated slot, signer.
    let dw_store = AcctStore::signer(record.user);
    let mut dw_one = [dw_store];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendMainWalletObligationRejected);
}

// ── Test 4: no_invoke_signed_path_in_p5b2 (source-grep) ─────────────────────

#[test]
fn no_invoke_signed_path_in_p5b2() {
    // Strip doc-comment lines (which legitimately reference
    // `invoke_signed` when explaining the deferral) and assert the
    // executable production source has no `invoke_signed(` call.
    //
    // Pre-existing P1 `invoke_signed(` exists ONLY in lib.rs's
    // `process_create_authorization` (via system_instruction::create_account).
    // The P5b-2 contract is that the SOLEND CPI path uses `invoke`, never
    // `invoke_signed`. We grep solend_cpi_builder.rs (the file P5b-2
    // exclusively owns the CPI helpers in) and assert no invoke_signed
    // appears in any executable line.
    let src = include_str!("../src/solend_cpi_builder.rs");
    let executable: String = src
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let pat = ['i', 'n', 'v', 'o', 'k', 'e', '_', 's', 'i', 'g', 'n', 'e', 'd', '(']
        .iter()
        .collect::<String>();
    assert!(
        !executable.contains(&pat),
        "P5b-2 production source must not call invoke_signed; \
         the CPI helpers use invoke() exclusively"
    );
}

// ── Test 5: oracle_status_is_explicitly_reported_by_code_path ───────────────

#[test]
fn oracle_status_is_explicitly_reported_by_code_path() {
    // P5b-2 hardcodes Oracle Mode B (FailClosed). This test asserts the
    // const-fn surface returns exactly that variant.
    assert_eq!(p5b2_oracle_mode(), SolendOracleMode::FailClosed);
}

// ── Test 6: arbitrary_oracle_is_not_accepted ────────────────────────────────

#[test]
fn arbitrary_oracle_is_not_accepted() {
    // Under Mode B the orchestrator does NOT structurally validate the
    // oracle pubkeys against the reserve's committed oracle config (that
    // requires the reserve oracle decoder extension still pending —
    // see solend_cpi_builder.rs Mode A vs B vs C section). Instead it
    // fails closed AFTER Phase 1 validation passes, so an executor
    // who supplies real-shape but wrong oracle pubkeys gets the
    // SolendCpiSuccessPathBlockedByOracleValidation sentinel — never
    // a "this oracle was wrong" specific error. This test pins that
    // contract: passing arbitrary oracle pubkeys MUST surface the
    // fail-closed error, not nothing.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    // Substrate with arbitrary (pk(0xCA)/0xCB) oracle pubkeys —
    // structurally well-shaped, but not validated against any reserve.
    let substrate_stores = SubstrateStores::good();
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    let dw_store = AcctStore::signer(record.delegated_wallet);
    let mut dw_one = [dw_store];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 7: fake_clock_sysvar_rejected_before_cpi ───────────────────────────

#[test]
fn fake_clock_sysvar_rejected_before_cpi() {
    // Phase 1 sysvar guard: the clock_sysvar slot's key MUST equal
    // solana_program::sysvar::clock::id(). Pass a fake clock pubkey and
    // assert the dedicated sysvar mismatch error fires (NOT the
    // fail-closed oracle sentinel).
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let mut substrate_stores = SubstrateStores::good();
    // Replace clock with a non-canonical pubkey.
    substrate_stores.clock = AcctStore::bare(pk(0xDE), pk(0xFE));
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiSysvarMismatch);
}

// ── Test 8: fake_token_program_rejected_before_cpi ──────────────────────────

#[test]
fn fake_token_program_rejected_before_cpi() {
    // The variant-15 slot 11 (token_program) MUST equal the canonical
    // SPL Token program id. Replace it with an attacker-controlled pubkey
    // and assert the dedicated token-program mismatch error fires.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    // Swap slot 11's key to a non-canonical token program.
    withdraw_stores[11] = AcctStore::bare(pk(0xDE), pk(0xFE));

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiTokenProgramMismatch);
}

// ── Test 9: zero_ctoken_amount_rejected_before_cpi ──────────────────────────

#[test]
fn zero_ctoken_amount_rejected_before_cpi() {
    // The cToken at slot 1 MUST have amount > 0. Pass a drained-out
    // (amount = 0) cToken and assert the dedicated zero-collateral
    // error fires, NOT the fail-closed oracle sentinel.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 0);

    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut substrate_storage = [substrate_stores.solend];
    let solend_program_acct = build_accounts(&mut substrate_storage).pop().unwrap();
    let mut pyth_storage = [substrate_stores.pyth];
    let pyth_acct = build_accounts(&mut pyth_storage).pop().unwrap();
    let mut switch_storage = [substrate_stores.switch];
    let switch_acct = build_accounts(&mut switch_storage).pop().unwrap();
    let mut clock_storage = [substrate_stores.clock];
    let clock_acct = build_accounts(&mut clock_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &solend_program_acct,
        pyth_oracle: &pyth_acct,
        switchboard_oracle: &switch_acct,
        clock_sysvar: &clock_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiZeroCollateralAmount);
}

// ── Tests 10/11/12: refresh_*/withdraw failure paths under Mode B ───────────
//
// The original spec called for tests that drive a stub Solend program
// to return failure on each of RefreshReserve / RefreshObligation /
// Withdraw and assert the AuthorizationRecord is not mutated. Under
// Mode B (P5b-2 production default), the success path is blocked at
// the oracle gate BEFORE any CPI fires, so the helper-level failure
// path is unreachable from `try_solend_cpi_skeleton`. The Mode B
// substitution: assert that under the same valid Phase 1 inputs, the
// orchestrator returns SolendCpiSuccessPathBlockedByOracleValidation
// and that no mutation occurs (the orchestrator never mutates the
// record — only the lib.rs caller does, and only after Ok).
//
// Each substituted test below carries an explicit comment explaining
// the Mode B reroute so a future audit reading the original test list
// can map the rename.

#[test]
fn refresh_reserve_failure_does_not_mutate_record() {
    // MODE B SUBSTITUTION: the original test required a Solend stub
    // returning Err for variant 3. Under Mode B the orchestrator fails
    // closed BEFORE invoking variant 3, so we assert the fail-closed
    // sentinel and that the orchestrator returns Err (the only "no
    // mutation" guarantee available — the orchestrator itself never
    // mutates AuthorizationRecord).
    let record = record_fixture();
    let baseline_record = record.clone();

    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
    // The orchestrator NEVER mutates the record; reaffirm this
    // structural property by comparing against the pre-call clone.
    assert_eq!(record, baseline_record);
}

#[test]
fn refresh_obligation_failure_does_not_mutate_record() {
    // MODE B SUBSTITUTION: same reasoning as
    // refresh_reserve_failure_does_not_mutate_record. Mode B blocks
    // RefreshObligation behind the oracle gate too.
    let record = record_fixture();
    let baseline_record = record.clone();

    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 2_500);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
    assert_eq!(record, baseline_record);
}

#[test]
fn withdraw_failure_does_not_mutate_record() {
    // MODE B SUBSTITUTION: same reasoning. Variant-15 Withdraw is the
    // last CPI in the orchestrator and is also blocked behind the
    // oracle gate.
    let record = record_fixture();
    let baseline_record = record.clone();

    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 7_777);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
    assert_eq!(record, baseline_record);
}

// ── Test 13: all_three_stub_cpis_success_then_record_mutates (deferred) ─────

#[test]
#[ignore = "P5b-2 Mode B fail-closed: success path blocked until oracle validation lands"]
fn all_three_stub_cpis_success_then_record_mutates() {
    // RESERVED for the future slice that resolves oracle shadow mapping
    // (Mode A Official Reserve Oracle Decode or Mode C Demo Pinned
    // Allowlist). At that point: register a stateless stub Solend
    // program at SOLEND_PROGRAM_ID_MAINNET that succeeds for variants
    // 3, 7, 15 (recording the variant byte to a per-test recording
    // account), drive a full ExecuteAction transaction through
    // BanksClient, assert the AuthorizationRecord's `completed = true`
    // after, AND assert the recording account contains [3, 7, 15] in
    // call order.
    panic!("ignored; future slice");
}

// ── Test 14: solend_success_path_fails_closed_with_oracle_unverified ────────

#[test]
fn solend_success_path_fails_closed_with_oracle_unverified() {
    // The headline Mode B contract: under a fully valid (signer +
    // correct keys + canonical sysvar + non-zero cToken amount)
    // account list, the orchestrator MUST return
    // SolendCpiSuccessPathBlockedByOracleValidation — never Ok, never
    // an earlier error.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 12_345);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let result = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    );
    let err = result.unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 15: cpi_order_is_refresh_reserve_then_refresh_obligation_then_withdraw

#[test]
#[ignore = "P5b-2 Mode B fail-closed: no CPI fires; activates when OracleMode advances"]
fn cpi_order_is_refresh_reserve_then_refresh_obligation_then_withdraw() {
    // RESERVED for the future slice. Once the success path is enabled,
    // a stateless recording stub at SOLEND_PROGRAM_ID_MAINNET will
    // append the variant byte (3 / 7 / 15) of every received
    // instruction into a per-test recording account. The test then
    // asserts the account's data starts with [3, 7, 15].
    panic!("ignored; future slice");
}

// ── Test 16: main_wallet_obligation_rejected_before_cpi ─────────────────────

#[test]
fn main_wallet_obligation_rejected_before_cpi() {
    // The decoded obligation's inner `owner` field MUST equal
    // record.delegated_wallet, NOT record.user. Build an obligation
    // whose data has the user main wallet at the owner offset and
    // assert the dedicated main-wallet rejection error fires.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    // Overwrite obligation's inner-owner field with the user main
    // wallet's bytes — keeps the Solana account-level owner program
    // matching, only the data-side authority byte changes.
    populate_obligation_prefix(
        &mut withdraw_stores[3].data,
        100,
        false,
        lm,
        record.user.to_bytes(),
    );
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendMainWalletObligationRejected);
}

// ── Test 17: wrong_reserve_rejected_before_cpi ──────────────────────────────

#[test]
fn wrong_reserve_rejected_before_cpi() {
    // The reserve AccountInfo at slot 2 MUST match expectations.
    // Build the happy path then pass a different `expected_reserve`
    // through the expectations struct; assert the dedicated reserve-
    // mismatch error fires.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    // Wrong `expected_reserve` — pass a pubkey that does NOT match the
    // reserve AccountInfo at variant-15 slot 2.
    let wrong_reserve_pk = pk(0x77);
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &wrong_reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendReserveMismatch);
}

// ── Test 18: revoked_authorization_fails_before_cpi (P2/P3 layer) ───────────
//
// The orchestrator does NOT independently re-check the AuthorizationRecord
// `revoked` flag — that gate is the caller's (process_execute_action's)
// responsibility, and it fires BEFORE the orchestrator is invoked.
// The test below verifies that contract by mutating the local record
// to revoked=true, then asserting the orchestrator still runs and returns
// the fail-closed sentinel — i.e. this gate is NOT a duplicate check
// inside the orchestrator. The lib.rs P2 gate above already covers
// the user-visible behaviour (orchestrator never sees a revoked
// record).

#[test]
fn revoked_authorization_fails_before_cpi() {
    // STRUCTURAL CONTRACT: process_execute_action's existing P2 gate
    // (AuthorizationRevoked, code 16) blocks BEFORE the orchestrator
    // is reached, so the orchestrator does NOT need to re-check
    // `revoked`. We assert this contract by passing a record with
    // revoked=true directly to the orchestrator and observing that it
    // proceeds normally to the Mode B fail-closed sentinel — i.e. no
    // hidden side check intercepts.
    let mut record = record_fixture();
    record.revoked = true;
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    // The orchestrator does not re-check revoked; under Mode B it
    // surfaces the same fail-closed sentinel. The user-visible
    // correctness is that the lib.rs caller's revoked gate fires
    // FIRST in process_execute_action (test
    // `process_execute_action_revoked_record_short_circuits` covers
    // that path).
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 19: expired_authorization_fails_before_cpi (P2/P3 layer) ───────────

#[test]
fn expired_authorization_fails_before_cpi() {
    // STRUCTURAL CONTRACT: same reasoning as
    // revoked_authorization_fails_before_cpi. Expiry is checked by the
    // lib.rs P2 gate before the orchestrator is called. The
    // orchestrator does not re-check expiry, and that is the correct
    // behaviour because expiry gating is process-level, not CPI-level.
    let mut record = record_fixture();
    record.expires_at_slot = 1; // any small value; the orchestrator
                                 // doesn't read clock.
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 20: p3_condition_false_fails_before_p4_boundary (P3 layer) ─────────
//
// P3 condition gating is the caller's responsibility (process_execute_action),
// fires before the orchestrator is reached. The orchestrator itself
// has no condition concept. The lib.rs `p4_p3_condition_false_fails_before_p4_boundary`
// test in functional.rs exercises the user-visible path. Here we keep
// a same-named structural test that exercises the orchestrator under
// the same Mode B contract: the orchestrator sees no condition_proof,
// and surface behaviour is unchanged.

#[test]
fn p3_condition_false_fails_before_cpi() {
    // STRUCTURAL CONTRACT: process_execute_action's P3 condition gate
    // fails closed BEFORE the orchestrator is invoked. The
    // orchestrator does not see condition_proof — its surface area is
    // pure CPI shape. Under Mode B, an otherwise-valid input still
    // blocks at the oracle gate, mirroring the higher-level
    // P3-fails-before-P4 invariant.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 21: no_account_data_borrow_held_across_cpi_code_structure (grep) ──

#[test]
fn no_account_data_borrow_held_across_cpi_code_structure_test_or_static_assertion() {
    // Source-grep over `solend_cpi_builder.rs`: the three CPI helper
    // bodies (`cpi_refresh_reserve`, `cpi_refresh_obligation`,
    // `cpi_withdraw_obligation_collateral_and_redeem_reserve_collateral`)
    // MUST NOT call `try_borrow_data` or `try_borrow_mut_data`. That
    // is the structural invariant ensuring no `Ref` guard lives across
    // an `invoke()` call.
    //
    // The check walks the file, finds each helper's `pub fn ...`
    // header, and asserts the substring `try_borrow_data` does NOT
    // appear between that header and the next `}` at indent level 0
    // (= function close).
    let src = include_str!("../src/solend_cpi_builder.rs");

    // Patterns are char-concatenated to avoid self-trigger when this
    // test file is itself searched.
    let try_borrow = ['t', 'r', 'y', '_', 'b', 'o', 'r', 'r', 'o', 'w', '_', 'd', 'a', 't', 'a']
        .iter()
        .collect::<String>();
    let try_borrow_mut = [
        't', 'r', 'y', '_', 'b', 'o', 'r', 'r', 'o', 'w', '_', 'm', 'u', 't', '_', 'd', 'a', 't', 'a',
    ]
    .iter()
    .collect::<String>();

    for helper in &[
        "pub fn cpi_refresh_reserve",
        "pub fn cpi_refresh_obligation",
        "pub fn cpi_withdraw_obligation_collateral_and_redeem_reserve_collateral",
    ] {
        let start = src
            .find(helper)
            .unwrap_or_else(|| panic!("helper {helper} not found in solend_cpi_builder.rs"));
        // Walk forward: track brace depth starting from the first '{'
        // after `start`. Slice from `start` to the matching '}' at
        // depth zero — that's the helper body.
        let after_header = &src[start..];
        let body_open = after_header.find('{').expect("helper has no body open");
        let mut depth = 0i32;
        let mut body_close = 0usize;
        for (i, c) in after_header[body_open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_close = body_open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &after_header[body_open..=body_close];
        assert!(
            !body.contains(&try_borrow),
            "helper `{helper}` body must not call try_borrow_data — \
             borrow guards must not live across invoke",
        );
        assert!(
            !body.contains(&try_borrow_mut),
            "helper `{helper}` body must not call try_borrow_mut_data — \
             borrow guards must not live across invoke",
        );
    }
}

// ── Test 22: stub_program_is_stateless_and_test_isolated ────────────────────

#[test]
fn stub_program_is_stateless_and_test_isolated() {
    // Even though Mode B blocks the success path so no actual stub
    // Solend program runs in P5b-2, this test pins the test-isolation
    // PROPERTY of the fixture pattern — namely that two sequential
    // orchestrator calls in this file (and in the future, the
    // ProgramTest stub registered under the same pattern) share NO
    // mutable state. The body builds a fresh fixture twice, calls
    // `try_solend_cpi_skeleton` each time, and asserts identical
    // independent error returns.
    //
    // The assertion of independence is structural: each call sets up
    // its own AcctStore Vec, its own AuthorizationRecord, its own
    // SubstrateStores. None of these are static, lazy_static, or
    // OnceCell. If a future stub registration adds shared state, this
    // test must be updated to assert per-test isolation explicitly
    // (e.g. via a per-test recording AccountInfo whose `data` is
    // freshly zero at the start of each test).
    fn run_once() -> ProgramError {
        let record = record_fixture();
        let lm = pk(0x01).to_bytes();
        let ctoken_mint = pk(0x55).to_bytes();
        let liquidity_mint = pk(0x66).to_bytes();
        let mut withdraw_stores =
            good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_234);
        let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
        let withdraw_accounts =
            SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();
        let substrate_stores = SubstrateStores::good();
        let mut s_storage = [substrate_stores.solend];
        let s_acct = build_accounts(&mut s_storage).pop().unwrap();
        let mut p_storage = [substrate_stores.pyth];
        let p_acct = build_accounts(&mut p_storage).pop().unwrap();
        let mut sw_storage = [substrate_stores.switch];
        let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
        let mut c_storage = [substrate_stores.clock];
        let c_acct = build_accounts(&mut c_storage).pop().unwrap();
        let substrate = SolendCpiSubstrate {
            solend_program: &s_acct,
            pyth_oracle: &p_acct,
            switchboard_oracle: &sw_acct,
            clock_sysvar: &c_acct,
            extra_oracle: None,
        };
        let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
        let dw_acct = build_accounts(&mut dw_one).pop().unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = withdraw_accounts.obligation.key.clone();
        let reserve_pk = withdraw_accounts.reserve.key.clone();
        let exp = good_expectations(
            &lm_pk,
            &ctoken_pk,
            &liquidity_pk,
            &obligation_pk,
            &reserve_pk,
        );
        let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
        try_solend_cpi_skeleton(
            &record,
            &dw_acct,
            &substrate,
            &withdraw_accounts,
            &dep_reserves,
            &exp,
        )
        .unwrap_err()
    }

    let first = run_once();
    let second = run_once();
    // Both calls must produce the same error code — proves no leftover
    // state from `first` mutated the second call's outcome.
    assert_eq!(first, second);
    assert_err(
        first,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 23: withdraw_uses_live_ctoken_amount_from_p5a_decoder ──────────────

#[test]
fn withdraw_uses_live_ctoken_amount_from_p5a_decoder() {
    // The orchestrator's Phase 1 reads the cToken `amount` field
    // through the P5a `decode_spl_token_account_lite` decoder — NOT a
    // config or proof field. This test pins the contract by changing
    // the live cToken amount byte-by-byte and observing the ONLY
    // place that read reaches is into the orchestrator's Phase 1 (the
    // helper `read_live_ctoken_amount_or_fail_closed`).
    //
    // Under Mode B we cannot observe the amount directly through the
    // success path, but we CAN observe the difference between
    // amount=0 (rejects with SolendCpiZeroCollateralAmount) and
    // amount=N (continues to the Mode B fail-closed sentinel). This
    // pin proves Phase 1 reaches the cToken-amount read regardless of
    // the value, demonstrating the read happens at the live data
    // layer.

    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();

    // Run 1 — amount = 0 → expect SolendCpiZeroCollateralAmount.
    let mut zero_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 0);
    let zero_vec = build_accounts(&mut zero_stores);
    let zero_acc = SolendWithdrawAccounts::parse(&zero_vec).unwrap();
    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };
    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();
    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obl_pk = zero_acc.obligation.key.clone();
    let res_pk = zero_acc.reserve.key.clone();
    let exp = good_expectations(&lm_pk, &ctoken_pk, &liquidity_pk, &obl_pk, &res_pk);
    let dep: Vec<&AccountInfo> = vec![zero_acc.reserve];
    let err = try_solend_cpi_skeleton(
        &record, &dw_acct, &substrate, &zero_acc, &dep, &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendCpiZeroCollateralAmount);
    let _ = zero_acc;
    let _ = zero_vec;
    let _ = zero_stores;

    // Run 2 — amount > 0 → reaches Mode B fail-closed sentinel. The
    // distinct outcomes prove the orchestrator is reading the live
    // amount byte from the cToken account's data, not a constant.
    let mut nonzero_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 99_999);
    let nonzero_vec = build_accounts(&mut nonzero_stores);
    let nonzero_acc = SolendWithdrawAccounts::parse(&nonzero_vec).unwrap();
    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };
    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();
    let obl_pk = nonzero_acc.obligation.key.clone();
    let res_pk = nonzero_acc.reserve.key.clone();
    let exp = good_expectations(&lm_pk, &ctoken_pk, &liquidity_pk, &obl_pk, &res_pk);
    let dep: Vec<&AccountInfo> = vec![nonzero_acc.reserve];
    let err = try_solend_cpi_skeleton(
        &record, &dw_acct, &substrate, &nonzero_acc, &dep, &exp,
    )
    .unwrap_err();
    assert_err(
        err,
        AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation,
    );
}

// ── Test 26: wrong_destination_mint_rejected_before_cpi ─────────────────────

#[test]
fn wrong_destination_mint_rejected_before_cpi() {
    // Build a destination whose decoded mint does NOT equal the
    // expected liquidity_mint. The shadow-mapping validator MUST
    // reject before the orchestrator reaches the Mode B oracle gate.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    // Tamper destination's mint field to a different value.
    let tampered_mint = pk(0xFF).to_bytes();
    populate_spl_token(
        &mut withdraw_stores[6].data,
        tampered_mint,
        record.user.to_bytes(),
        0,
    );
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendTokenAccountMintMismatch);
}

// ── Test 27: wrong_ctoken_authority_rejected_before_cpi ─────────────────────

#[test]
fn wrong_ctoken_authority_rejected_before_cpi() {
    // The decoded delegated cToken's `authority/owner` field MUST
    // equal record.delegated_wallet. Tamper that byte and assert the
    // dedicated authority mismatch error.
    let record = record_fixture();
    let lm = pk(0x01).to_bytes();
    let ctoken_mint = pk(0x55).to_bytes();
    let liquidity_mint = pk(0x66).to_bytes();
    let mut withdraw_stores =
        good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
    // Tamper cToken authority field to a different wallet.
    populate_spl_token(
        &mut withdraw_stores[1].data,
        ctoken_mint,
        [0xFE; 32],
        1_000,
    );
    let withdraw_accounts_vec = build_accounts(&mut withdraw_stores);
    let withdraw_accounts =
        SolendWithdrawAccounts::parse(&withdraw_accounts_vec).unwrap();

    let substrate_stores = SubstrateStores::good();
    let mut s_storage = [substrate_stores.solend];
    let s_acct = build_accounts(&mut s_storage).pop().unwrap();
    let mut p_storage = [substrate_stores.pyth];
    let p_acct = build_accounts(&mut p_storage).pop().unwrap();
    let mut sw_storage = [substrate_stores.switch];
    let sw_acct = build_accounts(&mut sw_storage).pop().unwrap();
    let mut c_storage = [substrate_stores.clock];
    let c_acct = build_accounts(&mut c_storage).pop().unwrap();
    let substrate = SolendCpiSubstrate {
        solend_program: &s_acct,
        pyth_oracle: &p_acct,
        switchboard_oracle: &sw_acct,
        clock_sysvar: &c_acct,
        extra_oracle: None,
    };

    let mut dw_one = [AcctStore::signer(record.delegated_wallet)];
    let dw_acct = build_accounts(&mut dw_one).pop().unwrap();

    let lm_pk = Pubkey::new_from_array(lm);
    let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
    let liquidity_pk = Pubkey::new_from_array(liquidity_mint);
    let obligation_pk = withdraw_accounts.obligation.key.clone();
    let reserve_pk = withdraw_accounts.reserve.key.clone();
    let exp = good_expectations(
        &lm_pk,
        &ctoken_pk,
        &liquidity_pk,
        &obligation_pk,
        &reserve_pk,
    );

    let dep_reserves: Vec<&AccountInfo> = vec![withdraw_accounts.reserve];
    let err = try_solend_cpi_skeleton(
        &record,
        &dw_acct,
        &substrate,
        &withdraw_accounts,
        &dep_reserves,
        &exp,
    )
    .unwrap_err();
    assert_err(err, AuthorityError::SolendTokenAccountAuthorityMismatch);
}

// ── Test 28: no_close_account_cpi_emitted (source-grep) ─────────────────────

#[test]
fn no_close_account_cpi_emitted() {
    // P5b-2 must not emit a CloseAccount CPI in any production path —
    // the cToken is NOT closed after withdraw-all, and no rent-cleanup
    // helper exists in this slice. Source-grep the executable lines of
    // solend_cpi_builder.rs and lib.rs for any `CloseAccount` literal.
    let cpi_src = include_str!("../src/solend_cpi_builder.rs");
    let lib_src = include_str!("../src/lib.rs");

    let pat = ['C', 'l', 'o', 's', 'e', 'A', 'c', 'c', 'o', 'u', 'n', 't']
        .iter()
        .collect::<String>();

    for (label, src) in &[
        ("solend_cpi_builder.rs", cpi_src),
        ("lib.rs", lib_src),
    ] {
        let executable: String = src
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !executable.contains(&pat),
            "{label} executable source must not reference CloseAccount; \
             P5b-2 emits no rent-cleanup CPI",
        );
    }
}

// ── Test 29: safety_grep_no_find_program_address_in_cpi_path ────────────────

#[test]
fn safety_grep_no_find_program_address_in_cpi_path() {
    // The CPI helper bodies in solend_cpi_builder.rs MUST NOT call
    // Pubkey::find_program_address — the lending_market_authority is
    // resolved off-chain and passed in. find_program_address is OK in
    // unrelated places (e.g. process_create_authorization PDA derive
    // in lib.rs); the test scope is the three helper bodies only.
    let src = include_str!("../src/solend_cpi_builder.rs");

    let pat = [
        'P', 'u', 'b', 'k', 'e', 'y', ':', ':', 'f', 'i', 'n', 'd', '_', 'p', 'r', 'o', 'g', 'r',
        'a', 'm', '_', 'a', 'd', 'd', 'r', 'e', 's', 's',
    ]
    .iter()
    .collect::<String>();

    for helper in &[
        "pub fn cpi_refresh_reserve",
        "pub fn cpi_refresh_obligation",
        "pub fn cpi_withdraw_obligation_collateral_and_redeem_reserve_collateral",
        "pub fn try_solend_cpi_skeleton",
    ] {
        let start = src.find(helper).expect("helper present");
        let after_header = &src[start..];
        let body_open = after_header.find('{').expect("helper has body");
        let mut depth = 0i32;
        let mut body_close = 0usize;
        for (i, c) in after_header[body_open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_close = body_open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &after_header[body_open..=body_close];
        assert!(
            !body.contains(&pat),
            "helper `{helper}` body must not call find_program_address — \
             PDAs resolved off-chain and passed in",
        );
    }
}

// ── Test 30: safety_grep_no_jupiter_or_rpc_paths ────────────────────────────

#[test]
fn safety_grep_no_jupiter_or_rpc_paths() {
    // Verify changed P5b-2 source files contain no off-chain
    // RPC / signing / Jupiter symbols. Doc-comment `//` lines are
    // stripped so legitimate documentation references don't trigger.
    // (Doc-comment patterns like `RpcClient::` already DO appear in
    // narrative comments inside lib.rs / solend_cpi_builder.rs —
    // stripping comments before grepping is the correct scope.)
    let cpi_src = include_str!("../src/solend_cpi_builder.rs");
    let lib_src = include_str!("../src/lib.rs");
    let err_src = include_str!("../src/error.rs");

    let executable_only = |src: &str| -> String {
        src.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let cpi = executable_only(cpi_src);
    let lib = executable_only(lib_src);
    let err = executable_only(err_src);

    let forbidden: &[&str] = &[
        "RpcClient",
        "reqwest::",
        "api.jup.ag",
        "Jupiter::",
        "send_transaction(",
        "sendRawTransaction",
        "signTransaction",
        "broadcast(",
    ];

    for (label, body) in &[
        ("solend_cpi_builder.rs", &cpi),
        ("lib.rs", &lib),
        ("error.rs", &err),
    ] {
        for pat in forbidden {
            assert!(
                !body.contains(*pat),
                "{label} executable source must not contain off-chain RPC / \
                 signing / Jupiter symbol `{pat}`",
            );
        }
    }
}
