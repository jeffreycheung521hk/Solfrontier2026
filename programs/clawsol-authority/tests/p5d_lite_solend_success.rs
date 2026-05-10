//! Stage 2 P5d-lite — Solend demo-tuple stub-CPI success path tests.
//!
//! # Scope
//!
//! Drives the P5c Mode-C oracle gate end-to-end under
//! `solana-program-test`. A stub Solend processor deployed at the
//! official `SOLEND_PROGRAM_ID_MAINNET` accepts variants 3 / 7 / 15
//! deterministically; the demo-pinned tuple flows through the
//! P5b-2 length-gated orchestrator and `ExecuteAction` mutates
//! `AuthorizationRecord` (completed=true, used_amount_raw>0,
//! execution_nonce++, last_execution_slot updated). Six negative tests
//! verify no-mutation on demo-tuple field flips and signer/owner flips.
//!
//! # STAGE 2 PROOF SLICE — STUB ProgramTest only
//!
//! - Does NOT prove live Solend economics.
//! - Does NOT execute on devnet/mainnet.
//! - Does NOT broaden general oracle validation.
//! - Not W4 executor glue.
//!
//! The stub Solend processor is stateless and deterministic; it only
//! validates each call's variant byte and returns `Ok(())`. CPI order
//! invariance is enforced by the orchestrator's call sequence in
//! `try_solend_cpi_skeleton`, not by the stub.

use borsh::BorshDeserialize;
use clawsol_authority::{
    condition_verifier::{
        ConditionLogic, RateKind, SolendReserveSnapshot, SolendSupplyAprCondition,
        SOLEND_WAD, SUPPORTED_SOLEND_FORMULA_VERSION,
    },
    derive_authorization_pda,
    instruction::{
        create_authorization_instruction, execute_action_instruction, ConditionProofPayload,
        ProofCondition,
    },
    solend_account_decode::{
        OBLIGATION_OFFSET_LAST_UPDATE_SLOT, OBLIGATION_OFFSET_LAST_UPDATE_STALE,
        OBLIGATION_OFFSET_LENDING_MARKET, OBLIGATION_OFFSET_OWNER,
        OBLIGATION_OFFSET_VERSION, RESERVE_OFFSET_LAST_UPDATE_SLOT,
        RESERVE_OFFSET_LAST_UPDATE_STALE, RESERVE_OFFSET_LENDING_MARKET,
        RESERVE_OFFSET_VERSION, SOLEND_OBLIGATION_LEN, SOLEND_RESERVE_LEN,
        SOLEND_SUPPORTED_VERSION, SPL_TOKEN_ACCOUNT_LEN, SPL_TOKEN_OFFSET_AMOUNT,
        SPL_TOKEN_OFFSET_MINT, SPL_TOKEN_OFFSET_OWNER, SPL_TOKEN_PROGRAM_ID,
    },
    solend_boundary::{
        ObligationFixture, SiblingIxDescriptor, SolendBoundaryProof,
        SOLEND_PROGRAM_ID_MAINNET, SOLEND_VARIANT_REFRESH_RESERVE,
        SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
    },
    solend_cpi_builder::{
        MAINNET_BETA_DEMO_USDC_TUPLE, SOLEND_VARIANT_REFRESH_OBLIGATION,
    },
    state::{AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION},
};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey,
};
use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction, InstructionError},
    rent::Rent,
    signature::{Keypair, Signer},
    system_instruction, sysvar,
    transaction::{Transaction, TransactionError},
};

const SCHEMA_VERSION: u8 = STAGE2_AUTHORITY_SCHEMA_VERSION;

// Distinct rule_id first byte per test so PDAs don't collide if multiple
// tests later share a single ProgramTestContext.
const RULE_ID_P5D_HAPPY: [u8; 16] = [0xD1; 16];
const RULE_ID_P5D_WRONG_PYTH: [u8; 16] = [0xD2; 16];
const RULE_ID_P5D_WRONG_RESERVE: [u8; 16] = [0xD3; 16];
const RULE_ID_P5D_WRONG_LM: [u8; 16] = [0xD4; 16];
const RULE_ID_P5D_WRONG_CTOKEN: [u8; 16] = [0xD5; 16];
const RULE_ID_P5D_NO_DELEGATED_SIG: [u8; 16] = [0xD6; 16];
const RULE_ID_P5D_MAIN_WALLET_OBL: [u8; 16] = [0xD7; 16];

// ── Stub Solend processor ───────────────────────────────────────────────────
//
// Stateless. Dispatches by leading instruction-data byte:
//   variant 3  RefreshReserve     → Ok
//   variant 7  RefreshObligation  → Ok
//   variant 15 WithdrawObl…       → Ok (no-op; AuthorizationRecord trust
//                                     model uses input_amount_raw, not a
//                                     post-CPI destination read).
//
// No global mutable state. No counter. Each call independent.
fn stub_solend_processor(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let variant = *instruction_data
        .first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match variant {
        SOLEND_VARIANT_REFRESH_RESERVE
        | SOLEND_VARIANT_REFRESH_OBLIGATION
        | SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM => Ok(()),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ── Pack-style mock byte builders ───────────────────────────────────────────
//
// Solend Obligation / Reserve / SPL Token accounts are NOT Borsh — they
// use `solana_program::program_pack::Pack` raw byte layout. We construct
// zero-filled buffers at the exact pack length and overwrite the pinned
// prefix offsets P5a / P5b-2 / P5c read.

fn make_obligation_bytes(
    last_update_slot: u64,
    lending_market: &Pubkey,
    inner_owner: &Pubkey,
) -> Vec<u8> {
    let mut data = vec![0u8; SOLEND_OBLIGATION_LEN];
    data[OBLIGATION_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
    data[OBLIGATION_OFFSET_LAST_UPDATE_SLOT..OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8]
        .copy_from_slice(&last_update_slot.to_le_bytes());
    data[OBLIGATION_OFFSET_LAST_UPDATE_STALE] = 0;
    data[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32]
        .copy_from_slice(&lending_market.to_bytes());
    data[OBLIGATION_OFFSET_OWNER..OBLIGATION_OFFSET_OWNER + 32]
        .copy_from_slice(&inner_owner.to_bytes());
    data
}

fn make_reserve_bytes(last_update_slot: u64, lending_market: &Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; SOLEND_RESERVE_LEN];
    data[RESERVE_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
    data[RESERVE_OFFSET_LAST_UPDATE_SLOT..RESERVE_OFFSET_LAST_UPDATE_SLOT + 8]
        .copy_from_slice(&last_update_slot.to_le_bytes());
    data[RESERVE_OFFSET_LAST_UPDATE_STALE] = 0;
    data[RESERVE_OFFSET_LENDING_MARKET..RESERVE_OFFSET_LENDING_MARKET + 32]
        .copy_from_slice(&lending_market.to_bytes());
    data
}

fn make_spl_token_bytes(mint: &Pubkey, authority: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
    data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&mint.to_bytes());
    data[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32]
        .copy_from_slice(&authority.to_bytes());
    data[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
        .copy_from_slice(&amount.to_le_bytes());
    // SPL Token AccountState::Initialized lives at offset 108. Our
    // `decode_spl_token_account_lite` only projects (mint, owner, amount)
    // and does not read `state`, but we set the byte for parity with a
    // real Pack'd Initialized account in case a future tightening looks
    // at it.
    data[108] = 1;
    data
}

// ── Fixture: P5dFixture ─────────────────────────────────────────────────────

/// Fixture the success path and all six negative tests share. Each
/// negative test reuses `setup_p5d_demo_tuple_env` then mutates ONE
/// field of the AccountInfo list before sending the tx.
struct P5dFixture {
    context: ProgramTestContext,
    program_id: Pubkey,
    rule_id: [u8; 16],
    /// On-chain authorization PDA (derived from program_id + user + rule_id).
    pda: Pubkey,
    /// Pre-mutation snapshot of the AuthorizationRecord for byte-identical
    /// comparison after a failed tx.
    pre_record: AuthorizationRecord,
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    max_input_amount_raw: u64,
    /// Externally-generated delegated wallet keypair (must sign the tx).
    delegated_wallet: Keypair,
    /// The delegated cToken ATA we mint our cToken balance into.
    delegated_ctoken: Pubkey,
    /// User's USDC destination ATA (= record.destination).
    destination: Pubkey,
    /// Demo-pinned obligation pubkey (our mock obligation account).
    obligation: Pubkey,
    /// Demo-pinned reserve pubkey = MAINNET_BETA_DEMO_USDC_TUPLE.reserve.
    reserve: Pubkey,
    /// Demo-pinned lending market pubkey = MAINNET_BETA_DEMO_USDC_TUPLE.lending_market.
    lending_market: Pubkey,
    /// Lending-market-authority placeholder (not derived; we just hand
    /// any pubkey here — Solend's PDA derivation only matters to the
    /// real Solend processor, which we replace with a stub).
    lending_market_authority: Pubkey,
    /// Reserve liquidity supply / source collateral / collateral mint.
    /// Source collateral: any SPL Token account.
    source_collateral: Pubkey,
    /// reserve_collateral_mint slot 7 = demo cToken mint
    /// (MAINNET_BETA_DEMO_USDC_TUPLE.ctoken_mint = 9n4nbM75…).
    reserve_collateral_mint: Pubkey,
    reserve_liquidity_supply: Pubkey,
    /// Demo Pyth oracle pubkey = MAINNET_BETA_DEMO_USDC_TUPLE.pyth_oracle.
    demo_pyth_oracle: Pubkey,
    /// Demo Switchboard sentinel = MAINNET_BETA_DEMO_USDC_TUPLE.switchboard_oracle (`nu11…`).
    demo_switchboard_sentinel: Pubkey,
    /// Liquidity mint (USDC) = MAINNET_BETA_DEMO_USDC_TUPLE.liquidity_mint.
    /// Stored on the fixture for negative-test diagnostic visibility
    /// even though the success path reads it via the orchestrator's
    /// destination-mint peek rather than off the fixture directly.
    #[allow(dead_code)]
    demo_liquidity_mint: Pubkey,
}

/// Build the success-path P5dFixture. Caller will then build an
/// `ExecuteAction` ix using `build_execute_action_ix` and either run it
/// as-is (success path) or mutate one slot before signing (negative
/// tests).
async fn setup_p5d_demo_tuple_env(rule_id: [u8; 16]) -> P5dFixture {
    let program_id = Pubkey::new_unique();

    // Solend deploys at the OFFICIAL mainnet program id (per § 3.2 of
    // the spec). Production code in `solend_cpi_builder.rs` hard-checks
    // this id; we point our stub at the SAME id so production CPI
    // dispatches into the stub and the production constant remains
    // unmodified.
    let mut program_test = ProgramTest::new(
        "clawsol_authority",
        program_id,
        processor!(clawsol_authority::process_instruction),
    );
    program_test.add_program(
        "solend_stub",
        SOLEND_PROGRAM_ID_MAINNET,
        processor!(stub_solend_processor),
    );

    // Fix the bootstrap slot to a known low value. We do NOT need to
    // pin a clock sysvar value — the on-chain processor reads
    // `Clock::get()` and our mock Reserve/Obligation `last_update.slot`
    // is set to `0` (no staleness check is enforced on the success path
    // by P5b-2 / P5c — `require_reserve_not_stale` is NOT called by
    // `try_solend_cpi_skeleton` and the stub Solend processor accepts
    // RefreshReserve/RefreshObligation unconditionally regardless of
    // any slot).

    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let reserve = demo.reserve;
    let lending_market = demo.lending_market;
    let demo_pyth_oracle = demo.pyth_oracle;
    let demo_switchboard_sentinel = demo.switchboard_oracle;
    let reserve_collateral_mint = demo.ctoken_mint;
    let demo_liquidity_mint = demo.liquidity_mint;

    // Externally-generated delegated wallet (signs every successful
    // ExecuteAction). The Stage 2 model is "external delegated wallet" —
    // not a PDA — so we just generate a Keypair.
    let delegated_wallet = Keypair::new();

    // Deterministic ATA-shaped pubkeys (we do NOT derive the canonical
    // ATA because the Stage 2 processor does not require it; it requires
    // SPL-Token-Program-owned accounts at the right keys).
    let destination = Pubkey::new_unique();
    let delegated_ctoken = Pubkey::new_unique();
    let obligation = Pubkey::new_unique();
    let lending_market_authority = Pubkey::new_unique();
    let source_collateral = Pubkey::new_unique();
    let reserve_liquidity_supply = Pubkey::new_unique();

    // ── Seed mock accounts via add_account ──────────────────────────────
    //
    // Each seeded account holds Pack-style raw bytes (NOT Borsh). The
    // Solana account-level `owner` field is set to the program that
    // would own such an account on mainnet:
    //   - Obligation, Reserve  → SOLEND_PROGRAM_ID_MAINNET
    //   - SPL token accounts   → SPL_TOKEN_PROGRAM_ID
    //
    // The `lamports` field is set to a non-zero value so the runtime
    // does not consider the account ephemeral.

    let rent = Rent::default();
    let solend_rent = rent.minimum_balance(SOLEND_OBLIGATION_LEN.max(SOLEND_RESERVE_LEN));
    let spl_rent = rent.minimum_balance(SPL_TOKEN_ACCOUNT_LEN);

    program_test.add_account(
        obligation,
        Account {
            lamports: solend_rent,
            data: make_obligation_bytes(0, &lending_market, &delegated_wallet.pubkey()),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            executable: false,
            rent_epoch: 0,
        },
    );
    program_test.add_account(
        reserve,
        Account {
            lamports: solend_rent,
            data: make_reserve_bytes(0, &lending_market),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            executable: false,
            rent_epoch: 0,
        },
    );
    // Source collateral = any SPL Token account; the validator only
    // checks the destination's mint (not source's). Authority is
    // arbitrary; mint is the cToken mint.
    program_test.add_account(
        source_collateral,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&reserve_collateral_mint, &Pubkey::new_unique(), 0),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    // Delegated cToken = SPL Token account, mint = ctoken_mint, authority =
    // delegated_wallet, amount > 0 (the cToken-amount-zero gate).
    program_test.add_account(
        delegated_ctoken,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&reserve_collateral_mint, &delegated_wallet.pubkey(), 1_000),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    // Destination liquidity = SPL Token account, mint = liquidity_mint
    // (USDC), authority = arbitrary (the validator does not check
    // destination authority — only the mint, plus the destination key
    // matching record.destination).
    program_test.add_account(
        destination,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&demo_liquidity_mint, &Pubkey::new_unique(), 0),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    // Bare AccountInfo placeholders for the slots whose data the
    // shadow-mapping validator does not project: lending_market (key
    // matches expected_lending_market), lending_market_authority,
    // reserve_collateral_mint (key = ctoken_mint), reserve_liquidity_supply,
    // and the oracle accounts. Each is owned by a non-zero arbitrary
    // owner so the runtime does not collapse them; the orchestrator
    // does not enforce a specific account-level owner on these slots.
    let placeholder_owner = Pubkey::new_unique();
    for k in [
        lending_market,
        lending_market_authority,
        reserve_collateral_mint,
        reserve_liquidity_supply,
        demo_pyth_oracle,
        demo_switchboard_sentinel,
    ] {
        program_test.add_account(
            k,
            Account {
                lamports: 1,
                data: vec![],
                owner: placeholder_owner,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    let mut context = program_test.start_with_context().await;

    // Fund the delegated wallet so it can be a tx-level signer.
    let fund_ix = system_instruction::transfer(
        &context.payer.pubkey(),
        &delegated_wallet.pubkey(),
        1_000_000_000,
    );
    let tx = Transaction::new_signed_with_payer(
        &[fund_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(tx)
        .await
        .expect("fund delegated wallet");

    // ── Create AuthorizationRecord PDA ──────────────────────────────────
    //
    // - delegated_wallet = freshly generated keypair above.
    // - destination = our pre-seeded SPL Token account at `destination`
    //   (mint = USDC).
    // - max_input_amount_raw = 1 USDC (1_000_000 base units, plenty of
    //   headroom for the test's input_amount_raw=1).
    // - expires_at_slot = far in the future.
    // - executor = same as the daemon's executor (we use a separate
    //   keypair for the executor).
    let executor = Keypair::new();
    let canonical_rule_hash: [u8; 32] = {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(&rule_id);
        h[16..].copy_from_slice(&rule_id);
        h
    };
    let action_type = Stage2ActionType::SolendWithdrawAllDelegated.to_u8();
    let max_input_amount_raw: u64 = 1_000_000;
    let expires_at_slot: u64 = 100_000_000;

    let (pda, _bump) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &context.payer.pubkey(),
        &rule_id,
    );

    let create_ix = create_authorization_instruction(
        &program_id,
        &context.payer.pubkey(),
        SCHEMA_VERSION,
        rule_id,
        executor.pubkey(),
        delegated_wallet.pubkey(),
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    );
    context.last_blockhash = context
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash for create");
    let tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(tx)
        .await
        .expect("create_authorization for P5d-lite");

    // Fund the executor so it can pay tx fees as the ExecuteAction signer.
    context.last_blockhash = context.banks_client.get_latest_blockhash().await.unwrap();
    let fund_executor = system_instruction::transfer(
        &context.payer.pubkey(),
        &executor.pubkey(),
        1_000_000_000,
    );
    let tx = Transaction::new_signed_with_payer(
        &[fund_executor],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context.banks_client.process_transaction(tx).await.unwrap();

    // Snapshot the AuthorizationRecord pre-execute for the no-mutation
    // negative tests.
    let pre_record = load_record(&mut context, pda).await;

    // The fixture caller stashes `executor` along with the tx-builder
    // closures we expose later. Wrap into the fixture struct.
    P5dFixture {
        context,
        program_id,
        rule_id,
        pda,
        pre_record,
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        delegated_wallet,
        delegated_ctoken,
        destination,
        obligation,
        reserve,
        lending_market,
        lending_market_authority,
        source_collateral,
        reserve_collateral_mint,
        reserve_liquidity_supply,
        demo_pyth_oracle,
        demo_switchboard_sentinel,
        demo_liquidity_mint,
    }
    // executor is dropped here; we re-derive a fresh one inside each
    // ExecuteAction since the AuthorizationRecord stores the executor
    // pubkey, not a keypair. To sign, we keep an executor in a sibling
    // helper. For simplicity, every test rebuilds its own executor —
    // and the pubkey is read from the stored record. (See `executor_for`
    // below.)
}

async fn load_record(ctx: &mut ProgramTestContext, pda: Pubkey) -> AuthorizationRecord {
    let acc = ctx
        .banks_client
        .get_account(pda)
        .await
        .expect("get_account")
        .expect("PDA exists after create");
    AuthorizationRecord::try_from_slice(&acc.data).expect("decode AuthorizationRecord")
}

/// A canonical Solend ConditionProofPayload that the on-chain condition
/// gate (P3) will accept: low-utilisation reserve, threshold 1000bps —
/// the verifier's own fixture proves this is True. We attach this so
/// the P3 gate passes and execution proceeds to the P4/P5 boundary.
fn passing_solend_condition_proof() -> ConditionProofPayload {
    ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: SolendSupplyAprCondition {
                comparison: clawsol_authority::condition_verifier::Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: SUPPORTED_SOLEND_FORMULA_VERSION,
                max_reserve_staleness_slots: 1_000_000,
            },
            snapshot: SolendReserveSnapshot {
                available_amount: 5_000_000_000_000,
                borrowed_amount_wads: 5_000_000_000_000u128 * SOLEND_WAD,
                min_borrow_rate_pct: 0,
                optimal_borrow_rate_pct: 4,
                max_borrow_rate_pct: 30,
                super_max_borrow_rate_pct: 300,
                optimal_utilization_rate_pct: 80,
                max_utilization_rate_pct: 95,
                protocol_take_rate_pct: 20,
                last_update_slot: 0,
                stale_flag: false,
            },
        }],
    }
}

/// Build the full P5b-2 length-gated `ExecuteAction` Instruction (>= 21
/// accounts) for the fixture. The tail is:
///
///   0  executor                  (signer)
///   1  authz_pda                  (writable)
///   2  user                       (readonly)
///   3  delegated_wallet           (signer; key = record.delegated_wallet)
///   4  source_collateral          (variant-15 slot 0)
///   5  delegated_ctoken           (variant-15 slot 1)
///   6  reserve                    (variant-15 slot 2)
///   7  obligation                 (variant-15 slot 3)
///   8  lending_market             (variant-15 slot 4)
///   9  lending_market_authority   (variant-15 slot 5)
///  10  destination                (variant-15 slot 6)
///  11  reserve_collateral_mint    (variant-15 slot 7)
///  12  reserve_liquidity_supply   (variant-15 slot 8)
///  13  obligation_owner           (variant-15 slot 9, signer = delegated)
///  14  user_transfer_authority    (variant-15 slot 10, signer = delegated)
///  15  token_program              (variant-15 slot 11)
///  16  solend_program             (substrate)
///  17  pyth_oracle                (substrate)
///  18  switchboard_oracle         (substrate)
///  19  clock_sysvar               (substrate)
///  20  deposit_reserve            (= reserve, single-deposit demo case)
fn build_execute_action_ix(
    fx: &P5dFixture,
    user: &Pubkey,
    executor_pubkey: &Pubkey,
    input_amount_raw: u64,
    nonce: u64,
    // Account-list overrides. `None` means "use the fixture's
    // demo-tuple value"; `Some(pk)` swaps that slot for negative tests.
    override_pyth_oracle: Option<Pubkey>,
    override_reserve: Option<Pubkey>,
    override_lending_market: Option<Pubkey>,
    override_ctoken_mint: Option<Pubkey>,
    override_obligation_owner: Option<(Pubkey, bool)>, // (key, is_signer)
) -> Instruction {
    let pyth_oracle_key = override_pyth_oracle.unwrap_or(fx.demo_pyth_oracle);
    let reserve_key = override_reserve.unwrap_or(fx.reserve);
    let lending_market_key = override_lending_market.unwrap_or(fx.lending_market);
    let ctoken_mint_key = override_ctoken_mint.unwrap_or(fx.reserve_collateral_mint);

    // The boundary proof's reserve_pubkey/lending_market_pubkey/
    // obligation_pubkey/destination_pubkey must reflect the values the
    // tx is dispatching against — gate (5) is `proof.destination_pubkey
    // == record.destination`, and the orchestrator's runtime tuple
    // pulls reserve / lending_market / cToken mint from the proof
    // (reserve / lending_market) and from variant-15 slot 7 (cToken
    // mint). For the negative tests that swap a single account-list
    // slot, the boundary proof's matching field MUST be swapped too —
    // otherwise the P4 boundary verifier (sibling-ix descriptor walk)
    // rejects the proof BEFORE the P5b-2 orchestrator engages and we
    // surface a P4 error rather than the expected P5c demo-allowlist
    // error. We honour that invariant by swapping the proof's matching
    // field alongside the AccountInfo slot.
    let proof = SolendBoundaryProof {
        solend_program_id: SOLEND_PROGRAM_ID_MAINNET,
        obligation_pubkey: fx.obligation,
        obligation: ObligationFixture {
            account_owner_program: SOLEND_PROGRAM_ID_MAINNET,
            obligation_authority: fx.delegated_wallet.pubkey(),
            lending_market: lending_market_key,
        },
        reserve_pubkey: reserve_key,
        lending_market_pubkey: lending_market_key,
        destination_pubkey: fx.destination,
        sibling_instructions: vec![
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_REFRESH_RESERVE,
                target_reserve: Some(reserve_key),
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            },
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
                target_reserve: Some(reserve_key),
                target_obligation: Some(fx.obligation),
                target_lending_market: Some(lending_market_key),
                target_destination: Some(fx.destination),
            },
        ],
    };

    // Determine signer slot identity at slot 13 (obligation_owner) and
    // slot 14 (user_transfer_authority). Per the P5b-2 invariant, both
    // MUST equal record.delegated_wallet AND have is_signer=true on the
    // tx. For the missing-delegated-signer / main-wallet-obligation
    // negative tests, the override allows passing a non-signer or the
    // user pubkey; the P5b-2 orchestrator then surfaces a specific
    // failure code BEFORE reaching the P5c gate.
    let (sig_slot_key, sig_slot_signer) =
        override_obligation_owner.unwrap_or((fx.delegated_wallet.pubkey(), true));

    // The lib.rs orchestrator unpacks accounts[3] as
    // delegated_wallet_account. The P5b-2 `require_external_delegated_wallet_signer`
    // requires accounts[3].is_signer = true AND
    // accounts[3].key == record.delegated_wallet (and != record.user).
    // For the missing-delegated-signer test we pass a non-signer at
    // slot 3 to surface SolendCpiDelegatedWalletNotSigner; for the
    // main-wallet-obligation test the obligation's INNER owner is the
    // user, surfacing SolendMainWalletObligationRejected via the
    // shadow-mapping validator's main-wallet poison gate before the
    // demo-tuple gate runs.
    let dw_account_meta = AccountMeta {
        pubkey: fx.delegated_wallet.pubkey(),
        is_signer: sig_slot_signer,
        is_writable: false,
    };

    let mut accounts = vec![
        AccountMeta::new_readonly(*executor_pubkey, true),
        AccountMeta::new(fx.pda, false),
        AccountMeta::new_readonly(*user, false),
        dw_account_meta,
        // Variant-15 slots 0..=11
        AccountMeta::new(fx.source_collateral, false),
        AccountMeta::new(fx.delegated_ctoken, false),
        AccountMeta::new(reserve_key, false),
        AccountMeta::new(fx.obligation, false),
        AccountMeta::new(lending_market_key, false),
        AccountMeta::new_readonly(fx.lending_market_authority, false),
        AccountMeta::new(fx.destination, false),
        AccountMeta::new(ctoken_mint_key, false),
        AccountMeta::new(fx.reserve_liquidity_supply, false),
        AccountMeta {
            pubkey: sig_slot_key,
            is_signer: sig_slot_signer,
            is_writable: false,
        },
        AccountMeta {
            pubkey: sig_slot_key,
            is_signer: sig_slot_signer,
            is_writable: false,
        },
        AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
        // Substrate slots
        AccountMeta::new_readonly(SOLEND_PROGRAM_ID_MAINNET, false),
        AccountMeta::new_readonly(pyth_oracle_key, false),
        AccountMeta::new_readonly(fx.demo_switchboard_sentinel, false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        // Deposit reserves tail (writable, single element = reserve).
        AccountMeta::new(reserve_key, false),
    ];

    // `execute_action_instruction` would reorder accounts — we want the
    // account list above verbatim, so build the Instruction manually.
    let data = borsh::to_vec(
        &clawsol_authority::instruction::AuthorityInstruction::ExecuteAction {
            schema_version: SCHEMA_VERSION,
            rule_id: fx.rule_id,
            canonical_rule_hash: fx.canonical_rule_hash,
            action_type: fx.action_type,
            input_amount_raw,
            execution_nonce: nonce,
            condition_proof: passing_solend_condition_proof(),
            solend_boundary_proof: Some(proof),
            jupiter_boundary_proof: None,
        },
    )
    .expect("borsh ExecuteAction");

    // The convenience builder in the crate would only emit the 4 base
    // accounts; we use the raw Instruction so we can append the P5b-2
    // length-gated tail.
    let _ = (
        execute_action_instruction,
        fx.max_input_amount_raw,
        fx.action_type,
    );
    let mut accounts_pinned = Vec::with_capacity(accounts.len());
    accounts_pinned.append(&mut accounts);
    Instruction {
        program_id: fx.program_id,
        accounts: accounts_pinned,
        data,
    }
}

/// Re-derive the executor keypair the fixture stored. We use a separate
/// keypair generated inside `setup_p5d_demo_tuple_env` — but we did not
/// retain it on the fixture (it would force an extra borrow), so each
/// test instead generates its OWN executor keypair, and we update the
/// AuthorizationRecord to point at the new executor pubkey by recreating
/// the PDA. To avoid that complexity, the fixture below is reorganised:
/// the fixture caller passes in the executor keypair AT setup time.
///
/// Implementation note: this avoided pattern is replaced by the
/// `setup_with_executor` form below. The above helper signature is
/// retained as documentation only.
fn _executor_signature_doc() {}

// To make the executor signable, we re-derive `setup_p5d_demo_tuple_env`
// to ALSO return an executor Keypair. Refactor below.

struct P5dFixtureFull {
    fx: P5dFixture,
    executor: Keypair,
}

async fn setup_full(rule_id: [u8; 16]) -> P5dFixtureFull {
    let program_id = Pubkey::new_unique();
    let mut program_test = ProgramTest::new(
        "clawsol_authority",
        program_id,
        processor!(clawsol_authority::process_instruction),
    );
    program_test.add_program(
        "solend_stub",
        SOLEND_PROGRAM_ID_MAINNET,
        processor!(stub_solend_processor),
    );

    let demo = &MAINNET_BETA_DEMO_USDC_TUPLE;
    let reserve = demo.reserve;
    let lending_market = demo.lending_market;
    let demo_pyth_oracle = demo.pyth_oracle;
    let demo_switchboard_sentinel = demo.switchboard_oracle;
    let reserve_collateral_mint = demo.ctoken_mint;
    let demo_liquidity_mint = demo.liquidity_mint;

    let delegated_wallet = Keypair::new();
    let executor = Keypair::new();

    let destination = Pubkey::new_unique();
    let delegated_ctoken = Pubkey::new_unique();
    let obligation = Pubkey::new_unique();
    let lending_market_authority = Pubkey::new_unique();
    let source_collateral = Pubkey::new_unique();
    let reserve_liquidity_supply = Pubkey::new_unique();

    let rent = Rent::default();
    let solend_rent = rent.minimum_balance(SOLEND_OBLIGATION_LEN.max(SOLEND_RESERVE_LEN));
    let spl_rent = rent.minimum_balance(SPL_TOKEN_ACCOUNT_LEN);

    program_test.add_account(
        obligation,
        Account {
            lamports: solend_rent,
            data: make_obligation_bytes(0, &lending_market, &delegated_wallet.pubkey()),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            executable: false,
            rent_epoch: 0,
        },
    );
    program_test.add_account(
        reserve,
        Account {
            lamports: solend_rent,
            data: make_reserve_bytes(0, &lending_market),
            owner: SOLEND_PROGRAM_ID_MAINNET,
            executable: false,
            rent_epoch: 0,
        },
    );
    program_test.add_account(
        source_collateral,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&reserve_collateral_mint, &Pubkey::new_unique(), 0),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    program_test.add_account(
        delegated_ctoken,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&reserve_collateral_mint, &delegated_wallet.pubkey(), 1_000),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    program_test.add_account(
        destination,
        Account {
            lamports: spl_rent,
            data: make_spl_token_bytes(&demo_liquidity_mint, &Pubkey::new_unique(), 0),
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );
    let placeholder_owner = Pubkey::new_unique();
    for k in [
        lending_market,
        lending_market_authority,
        reserve_collateral_mint,
        reserve_liquidity_supply,
        demo_pyth_oracle,
        demo_switchboard_sentinel,
    ] {
        program_test.add_account(
            k,
            Account {
                lamports: 1,
                data: vec![],
                owner: placeholder_owner,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    let mut context = program_test.start_with_context().await;

    // Fund delegated_wallet and executor.
    let fund_ix1 = system_instruction::transfer(
        &context.payer.pubkey(),
        &delegated_wallet.pubkey(),
        1_000_000_000,
    );
    let fund_ix2 = system_instruction::transfer(
        &context.payer.pubkey(),
        &executor.pubkey(),
        1_000_000_000,
    );
    let tx = Transaction::new_signed_with_payer(
        &[fund_ix1, fund_ix2],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(tx)
        .await
        .expect("fund delegated + executor");

    let canonical_rule_hash: [u8; 32] = {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(&rule_id);
        h[16..].copy_from_slice(&rule_id);
        h
    };
    let action_type = Stage2ActionType::SolendWithdrawAllDelegated.to_u8();
    let max_input_amount_raw: u64 = 1_000_000;
    let expires_at_slot: u64 = 100_000_000;

    let (pda, _bump) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &context.payer.pubkey(),
        &rule_id,
    );

    let create_ix = create_authorization_instruction(
        &program_id,
        &context.payer.pubkey(),
        SCHEMA_VERSION,
        rule_id,
        executor.pubkey(),
        delegated_wallet.pubkey(),
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    );
    context.last_blockhash = context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(tx)
        .await
        .expect("create_authorization for P5d-lite");

    let pre_record = load_record(&mut context, pda).await;

    P5dFixtureFull {
        fx: P5dFixture {
            context,
            program_id,
            rule_id,
            pda,
            pre_record,
            canonical_rule_hash,
            action_type,
            max_input_amount_raw,
            delegated_wallet,
            delegated_ctoken,
            destination,
            obligation,
            reserve,
            lending_market,
            lending_market_authority,
            source_collateral,
            reserve_collateral_mint,
            reserve_liquidity_supply,
            demo_pyth_oracle,
            demo_switchboard_sentinel,
            demo_liquidity_mint,
        },
        executor,
    }
}

// Suppress dead-code warning for the deprecated single-fixture form.
#[allow(dead_code)]
fn _unused_setup_marker() {
    let _ = setup_p5d_demo_tuple_env;
}

// ── Success path ────────────────────────────────────────────────────────────

#[tokio::test]
async fn p5d_lite_demo_tuple_reaches_stub_cpi_success_path() {
    let mut env = setup_full(RULE_ID_P5D_HAPPY).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();
    let input_amount: u64 = 1; // smallest non-zero amount
    let nonce: u64 = 1;

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        input_amount,
        nonce,
        None,
        None,
        None,
        None,
        None,
    );

    env.fx.context.last_blockhash = env
        .fx
        .context
        .banks_client
        .get_latest_blockhash()
        .await
        .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    env.fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect("P5d-lite demo-tuple success path: stub CPI must succeed");

    // ── Assert state mutated ────────────────────────────────────────────
    let record = load_record(&mut env.fx.context, env.fx.pda).await;
    // 1. used_amount_raw advanced (Stage 2 v1 semantics: += input_amount_raw).
    assert!(
        record.used_amount_raw > 0,
        "used_amount_raw must be non-zero after success"
    );
    assert!(
        record.used_amount_raw <= record.max_input_amount_raw,
        "used_amount_raw ({}) must be <= max_input_amount_raw ({})",
        record.used_amount_raw,
        record.max_input_amount_raw,
    );
    assert_eq!(record.used_amount_raw, input_amount);
    // 2. completed = true (one-shot v1).
    assert!(record.completed, "completed must flip true");
    // 3. execution_nonce incremented (was 0, now equals arg nonce = 1).
    assert_eq!(record.execution_nonce, nonce);
    assert!(record.execution_nonce > 0);
    // 4. last_execution_slot updated (non-zero).
    assert!(
        record.last_execution_slot > 0,
        "last_execution_slot must be set to the BanksClient clock at tx"
    );
    // 5. revoked unchanged (still false).
    assert!(!record.revoked);
    // 6. canonical_rule_hash unchanged.
    assert_eq!(record.canonical_rule_hash, env.fx.canonical_rule_hash);
    // 7. delegated_wallet unchanged.
    assert_eq!(record.delegated_wallet, env.fx.delegated_wallet.pubkey());
    // 8. user unchanged.
    assert_eq!(record.user, user);
}

// ── No-mutation helper ──────────────────────────────────────────────────────

async fn assert_no_mutation_and_get_err_code(
    ctx: &mut ProgramTestContext,
    pda: Pubkey,
    pre: &AuthorizationRecord,
    err: solana_program_test::BanksClientError,
) -> u32 {
    let post = load_record(ctx, pda).await;
    // Bytes-identical for the four fields the no-mutation contract pins.
    assert_eq!(
        post.used_amount_raw, pre.used_amount_raw,
        "used_amount_raw must NOT change on a failed P5d-lite tx (pre={}, post={})",
        pre.used_amount_raw, post.used_amount_raw,
    );
    assert!(
        !post.completed,
        "completed must remain false on a failed P5d-lite tx"
    );
    assert_eq!(
        post.execution_nonce, pre.execution_nonce,
        "execution_nonce must NOT advance on a failed tx"
    );
    assert_eq!(
        post.last_execution_slot, pre.last_execution_slot,
        "last_execution_slot must NOT advance on a failed tx"
    );
    // Defence in depth: revoked stays false.
    assert!(!post.revoked);

    // Pull the actual error code for documentation in test logs.
    let tx_err = match err {
        solana_program_test::BanksClientError::TransactionError(e) => e,
        solana_program_test::BanksClientError::SimulationError { err, .. } => err,
        other => panic!("unexpected BanksClientError variant: {other:?}"),
    };
    match tx_err {
        TransactionError::InstructionError(_, ie) => match ie {
            InstructionError::Custom(code) => code,
            InstructionError::MissingRequiredSignature => u32::MAX, // sentinel
            other => panic!("unexpected InstructionError: {other:?}"),
        },
        other => panic!("unexpected TransactionError: {other:?}"),
    }
}

// ── Negative tests (NO-MUTATION rule) ───────────────────────────────────────
//
// Each negative test reuses the success fixture, swaps ONE field, sends
// the tx, asserts an Err, and asserts AuthorizationRecord is byte-
// identical to its pre-tx state.

#[tokio::test]
async fn wrong_demo_oracle_fails_before_stub_cpi_and_no_mutation() {
    // Swap demo_pyth_oracle for a random pubkey. Boundary proof's
    // reserve / lending_market remain the demo values, so the runtime
    // tuple's pyth_oracle field disagrees with
    // MAINNET_BETA_DEMO_USDC_TUPLE.pyth_oracle. The mode-selector
    // returns Mode B (fail-closed); orchestrator surfaces error 85
    // SolendCpiSuccessPathBlockedByOracleValidation.
    let mut env = setup_full(RULE_ID_P5D_WRONG_PYTH).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    let bogus_pyth = Pubkey::new_unique();
    // Seed the bogus pyth so account-existence checks (if any) don't
    // fail spuriously. The orchestrator never reads pyth data — only
    // its key — so any account with any owner / data is fine.
    let placeholder_owner = Pubkey::new_unique();
    {
        // Insert via set_account on the live context.
        let acct = Account {
            lamports: 1,
            data: vec![],
            owner: placeholder_owner,
            executable: false,
            rent_epoch: 0,
        };
        env.fx
            .context
            .set_account(&bogus_pyth, &acct.into());
    }

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        Some(bogus_pyth),
        None,
        None,
        None,
        None,
    );
    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong demo pyth must fail");

    // expects error 85 SolendCpiSuccessPathBlockedByOracleValidation.
    let pre = env.fx.pre_record.clone();
    let code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
    assert_eq!(
        code,
        clawsol_authority::error::AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation
            as u32,
        "expected error 85 (Mode B fail-closed); got code {}",
        code
    );
}

#[tokio::test]
async fn wrong_demo_reserve_fails_before_stub_cpi_and_no_mutation() {
    // Swap demo reserve key for a random pubkey. We seed a fresh 619-
    // byte mock at the new pubkey (so the validator's data decode
    // doesn't fail before the demo-tuple gate runs). The runtime
    // tuple's reserve disagrees with MAINNET_BETA_DEMO_USDC_TUPLE.reserve
    // and Mode B fail-closes (error 85).
    let mut env = setup_full(RULE_ID_P5D_WRONG_RESERVE).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    let bogus_reserve = Pubkey::new_unique();
    let mock_reserve_data = make_reserve_bytes(0, &env.fx.lending_market);
    let acct = Account {
        lamports: Rent::default().minimum_balance(SOLEND_RESERVE_LEN),
        data: mock_reserve_data,
        owner: SOLEND_PROGRAM_ID_MAINNET,
        executable: false,
        rent_epoch: 0,
    };
    env.fx
        .context
        .set_account(&bogus_reserve, &acct.into());

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        None,
        Some(bogus_reserve),
        None,
        None,
        None,
    );
    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong demo reserve must fail");
    // expects error 85 SolendCpiSuccessPathBlockedByOracleValidation
    // (Mode B fail-closed, since runtime reserve != demo reserve).
    let pre = env.fx.pre_record.clone();
    let code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
    assert_eq!(
        code,
        clawsol_authority::error::AuthorityError::SolendCpiSuccessPathBlockedByOracleValidation
            as u32,
        "expected error 85 (Mode B fail-closed); got code {}",
        code
    );
}

#[tokio::test]
async fn wrong_demo_lending_market_fails_before_stub_cpi_and_no_mutation() {
    // Swap demo lending_market key for a random pubkey. Both the
    // boundary proof's lending_market_pubkey AND the obligation's
    // INNER lending_market field would also need to change to keep
    // ALL the obligation/reserve cross-checks passing. We DO NOT swap
    // the obligation's inner field — that lets the obligation/reserve
    // cross-checks (validator gates 5 / 8) fail FIRST and surface a
    // P5b-1 lending-market-mismatch error rather than the demo gate.
    // To exercise the demo-gate path we keep the obligation's inner
    // field on the demo lending_market — but the tx-level account-list
    // lending_market slot disagrees, so the validator gate (9) trips
    // (`accounts.lending_market.key != expected_lending_market`).
    //
    // expects: SolendLendingMarketMismatch (error 39) — fires BEFORE
    // the demo-tuple gate is reached. Either way the no-mutation
    // contract holds.
    let mut env = setup_full(RULE_ID_P5D_WRONG_LM).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    let bogus_lm = Pubkey::new_unique();
    // Seed a placeholder for the bogus lm key so banks doesn't reject
    // the tx for missing account.
    let acct = Account {
        lamports: 1,
        data: vec![],
        owner: Pubkey::new_unique(),
        executable: false,
        rent_epoch: 0,
    };
    env.fx
        .context
        .set_account(&bogus_lm, &acct.into());

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        None,
        None,
        Some(bogus_lm),
        None,
        None,
    );
    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong demo lending_market must fail");
    // expects error 66 SolendObligationLendingMarketMismatch — the
    // obligation's INNER lending_market is the demo lm but the proof
    // claims the bogus lm; validator gate (5) trips before the
    // demo-tuple gate. The no-mutation contract is what we pin.
    let pre = env.fx.pre_record.clone();
    let _code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
}

#[tokio::test]
async fn wrong_demo_ctoken_mint_fails_before_stub_cpi_and_no_mutation() {
    // Swap demo cToken mint at variant-15 slot 7 for a random pubkey.
    // The orchestrator reads the slot's KEY as
    // `expected_ctoken_mint`; the validator then checks the delegated
    // cToken account (slot 1)'s mint == expected_ctoken_mint — slot 1
    // still carries the demo cToken mint, so the mint-mismatch fires
    // (error 70) BEFORE the demo-tuple gate runs.
    //
    // expects: SolendTokenAccountMintMismatch (70).
    let mut env = setup_full(RULE_ID_P5D_WRONG_CTOKEN).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    let bogus_ctoken_mint = Pubkey::new_unique();
    let acct = Account {
        lamports: 1,
        data: vec![],
        owner: Pubkey::new_unique(),
        executable: false,
        rent_epoch: 0,
    };
    env.fx
        .context
        .set_account(&bogus_ctoken_mint, &acct.into());

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        None,
        None,
        None,
        Some(bogus_ctoken_mint),
        None,
    );
    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong demo cToken mint must fail");
    let pre = env.fx.pre_record.clone();
    let code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
    assert_eq!(
        code,
        clawsol_authority::error::AuthorityError::SolendTokenAccountMintMismatch as u32,
        "expected error 70 SolendTokenAccountMintMismatch; got code {}",
        code
    );
}

#[tokio::test]
async fn missing_delegated_wallet_signature_fails_before_stub_cpi_and_no_mutation() {
    // Sign with [executor] only — delegated_wallet is in the account
    // list but is_signer=false. The Stage 2 lib.rs picks up
    // accounts[3] as `delegated_wallet_account` and immediately runs
    // `require_external_delegated_wallet_signer` once the orchestrator
    // engages. expects: SolendCpiDelegatedWalletNotSigner (error 80).
    let mut env = setup_full(RULE_ID_P5D_NO_DELEGATED_SIG).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    // Build the ix with the delegated_wallet at slot 3 NOT marked as
    // signer, AND the obligation_owner / user_transfer_authority slots
    // ALSO not marked signer (override_obligation_owner with
    // is_signer=false).
    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        None,
        None,
        None,
        None,
        Some((env.fx.delegated_wallet.pubkey(), false)),
    );
    // Override accounts[3] in the built ix to drop is_signer.
    let mut accounts = ix.accounts.clone();
    accounts[3].is_signer = false;
    let ix = Instruction {
        program_id: ix.program_id,
        accounts,
        data: ix.data,
    };

    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    // Sign ONLY with executor (delegated_wallet does NOT sign).
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("missing delegated signer must fail");
    let pre = env.fx.pre_record.clone();
    let code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
    assert_eq!(
        code,
        clawsol_authority::error::AuthorityError::SolendCpiDelegatedWalletNotSigner as u32,
        "expected error 80 SolendCpiDelegatedWalletNotSigner; got code {}",
        code
    );
}

#[tokio::test]
async fn main_wallet_obligation_fails_before_stub_cpi_and_no_mutation() {
    // The obligation INNER `owner` field (byte offset 42..74) is set
    // to the user's main wallet pubkey instead of the delegated
    // wallet. The shadow-mapping validator's main-wallet poison gate
    // surfaces SolendMainWalletObligationRejected (error 37) BEFORE
    // the demo-tuple gate runs.
    let mut env = setup_full(RULE_ID_P5D_MAIN_WALLET_OBL).await;
    let user = env.fx.context.payer.pubkey();
    let executor_pk = env.executor.pubkey();

    // Overwrite the seeded obligation account's INNER owner to the
    // user main wallet. lending_market stays on the demo lm.
    let new_data = make_obligation_bytes(0, &env.fx.lending_market, &user);
    let acct = Account {
        lamports: Rent::default().minimum_balance(SOLEND_OBLIGATION_LEN),
        data: new_data,
        owner: SOLEND_PROGRAM_ID_MAINNET,
        executable: false,
        rent_epoch: 0,
    };
    env.fx
        .context
        .set_account(&env.fx.obligation, &acct.into());

    let ix = build_execute_action_ix(
        &env.fx,
        &user,
        &executor_pk,
        1,
        1,
        None,
        None,
        None,
        None,
        None,
    );
    env.fx.context.last_blockhash = env.fx.context.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.executor.pubkey()),
        &[&env.executor, &env.fx.delegated_wallet],
        env.fx.context.last_blockhash,
    );
    let err = env
        .fx
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("main-wallet obligation must fail");
    let pre = env.fx.pre_record.clone();
    let code =
        assert_no_mutation_and_get_err_code(&mut env.fx.context, env.fx.pda, &pre, err).await;
    assert_eq!(
        code,
        clawsol_authority::error::AuthorityError::SolendMainWalletObligationRejected as u32,
        "expected error 37 SolendMainWalletObligationRejected; got code {}",
        code
    );
}
