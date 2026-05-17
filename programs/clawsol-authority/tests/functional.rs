//! End-to-end behavioural tests for clawsol-authority, run inside the
//! in-process `solana-program-test` BanksClient harness.
//!
//! Coverage required by P1:
//!   - create_authorization happy path (writes every field)
//!   - user-must-sign rejection
//!   - wrong-PDA rejection
//!   - expired-authorization rejection at create
//!   - duplicate authorization rejection
//!   - zero max_input_amount_raw rejection
//!   - revoke happy path
//!   - revoke wrong-user rejection
//!   - revoke is idempotent (revoked persists across a second call)
//!   - close happy path (rent returns to user)
//!   - close requires revoked / completed / expired

use borsh::BorshDeserialize;
use clawsol_authority::{
    condition_verifier::{
        BoundMode, Comparison, ConditionLogic, PythPriceCondition, PythPriceSnapshot,
        RateKind, SolendReserveSnapshot, SolendSupplyAprCondition, VerificationLevel,
        SOLEND_WAD, SUPPORTED_SOLEND_FORMULA_VERSION,
    },
    derive_authorization_pda,
    error::AuthorityError,
    instruction::{
        close_authorization_instruction, create_authorization_instruction,
        execute_action_instruction as raw_execute_action_instruction,
        revoke_authorization_instruction, AuthorityInstruction, ConditionProofPayload,
        ProofCondition,
    },
    solend_boundary::{
        ObligationFixture, SiblingIxDescriptor, SolendBoundaryProof, MAX_SIBLING_INSTRUCTIONS,
        SOLEND_PROGRAM_ID_MAINNET, SOLEND_VARIANT_REFRESH_RESERVE,
        SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
    },
    state::{
        AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION,
    },
};
use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::{
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    rent::Rent,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};

const SCHEMA_VERSION: u8 = STAGE2_AUTHORITY_SCHEMA_VERSION;

// Stable fixture rule_id — using a different first byte per test
// keeps PDAs distinct so we don't have to refresh blockhashes between
// tests that reuse the same payer.
const RULE_ID_HAPPY: [u8; 16] = [0xC0; 16];
const RULE_ID_WRONG_PDA: [u8; 16] = [0xC1; 16];
const RULE_ID_EXPIRED: [u8; 16] = [0xC2; 16];
const RULE_ID_DUP_A: [u8; 16] = [0xC3; 16];
const RULE_ID_ZERO_AMT: [u8; 16] = [0xC4; 16];
const RULE_ID_NO_SIGNER: [u8; 16] = [0xC5; 16];
const RULE_ID_REVOKE_OK: [u8; 16] = [0xC6; 16];
const RULE_ID_REVOKE_WRONG_USER: [u8; 16] = [0xC7; 16];
const RULE_ID_REVOKE_IDEMP: [u8; 16] = [0xC8; 16];
const RULE_ID_CLOSE_OK: [u8; 16] = [0xC9; 16];
const RULE_ID_CLOSE_TOO_EARLY: [u8; 16] = [0xCA; 16];

// Stage 2 P2 ExecuteAction rule_ids — distinct first byte per case so
// PDAs are unique across tests sharing a single ProgramTestContext.
const RULE_ID_EXEC_HAPPY: [u8; 16] = [0xE0; 16];
const RULE_ID_EXEC_NO_SIG: [u8; 16] = [0xE1; 16];
const RULE_ID_EXEC_BAD_PDA: [u8; 16] = [0xE2; 16];
const RULE_ID_EXEC_WRONG_USER: [u8; 16] = [0xE3; 16];
const RULE_ID_EXEC_WRONG_EXEC: [u8; 16] = [0xE4; 16];
const RULE_ID_EXEC_EXPIRY_EQ: [u8; 16] = [0xE5; 16];
const RULE_ID_EXEC_EXPIRY_GT: [u8; 16] = [0xE6; 16];
const RULE_ID_EXEC_REVOKED: [u8; 16] = [0xE7; 16];
const RULE_ID_EXEC_COMPLETED: [u8; 16] = [0xE8; 16];
const RULE_ID_EXEC_HASH_MISMATCH: [u8; 16] = [0xE9; 16];
const RULE_ID_EXEC_TYPE_MISMATCH: [u8; 16] = [0xEA; 16];
const RULE_ID_EXEC_ZERO_AMT: [u8; 16] = [0xEB; 16];
const RULE_ID_EXEC_OVER_CAP: [u8; 16] = [0xEC; 16];
const RULE_ID_EXEC_BAD_NONCE: [u8; 16] = [0xED; 16];
const RULE_ID_EXEC_REPLAY: [u8; 16] = [0xEE; 16];

/// A canonical "Scenario A passes" Solend proof: low-utilisation
/// reserve fixture, threshold of 1000 bps. The verifier's own tests
/// already prove this fires under the same numbers; here it acts as a
/// stand-in proof for every P2 boundary test that doesn't care about
/// the condition gate but still has to supply a structurally-valid
/// payload after P3 lands.
///
/// Returns a fresh value so callers can cheaply mutate one field for
/// negative tests without affecting other call sites.
fn default_solend_proof() -> ConditionProofPayload {
    ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: SolendSupplyAprCondition {
                comparison: Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: SUPPORTED_SOLEND_FORMULA_VERSION,
                max_reserve_staleness_slots: 16,
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
                // last_update_slot is set to the current slot at call time
                // by the proof builder; tests that need staleness just
                // override this field.
                last_update_slot: 0,
                stale_flag: false,
            },
        }],
    }
}

/// Deterministic destination derived per `rule_id`. Used by both
/// `setup_execute_authz` (when creating the authz) and the test
/// wrapper (when building a passing Solend boundary proof) so the
/// proof's `destination_pubkey` matches `record.destination` without
/// having to thread the destination through every call site.
fn fx_destination(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x44; // 'D' for "destination"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic obligation pubkey derived per `rule_id`.
fn fx_obligation(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x4F; // 'O' for "obligation"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic reserve pubkey derived per `rule_id`.
fn fx_reserve(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x52; // 'R' for "reserve"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic lending-market pubkey derived per `rule_id`.
fn fx_lending_market(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x4C; // 'L' for "lending market"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// A canonical "passing Solend boundary proof" built from
/// `(delegated_wallet, rule_id)`. Returns a fresh value per call so
/// negative tests can mutate one field without affecting others.
fn default_solend_boundary_proof(
    delegated_wallet: &Pubkey,
    rule_id: &[u8; 16],
) -> SolendBoundaryProof {
    let reserve = fx_reserve(rule_id);
    let obligation = fx_obligation(rule_id);
    let lending_market = fx_lending_market(rule_id);
    let destination = fx_destination(rule_id);
    SolendBoundaryProof {
        solend_program_id: SOLEND_PROGRAM_ID_MAINNET,
        obligation_pubkey: obligation,
        obligation: ObligationFixture {
            account_owner_program: SOLEND_PROGRAM_ID_MAINNET,
            obligation_authority: *delegated_wallet,
            lending_market,
        },
        reserve_pubkey: reserve,
        lending_market_pubkey: lending_market,
        destination_pubkey: destination,
        sibling_instructions: vec![
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_REFRESH_RESERVE,
                target_reserve: Some(reserve),
                target_obligation: None,
                target_lending_market: None,
                target_destination: None,
            },
            SiblingIxDescriptor {
                program_id: SOLEND_PROGRAM_ID_MAINNET,
                variant_byte: SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM,
                target_reserve: Some(reserve),
                target_obligation: Some(obligation),
                target_lending_market: Some(lending_market),
                target_destination: Some(destination),
            },
        ],
    }
}

/// Test-side ExecuteAction builder for tests that are NOT exercising
/// the Solend boundary. The wrapper supplies a structurally-valid P3
/// `condition_proof` and unconditionally passes `None` for the P4
/// `solend_boundary_proof`. This deliberately does **not** auto-attach
/// a passing Solend boundary proof — passing fake valid proofs
/// everywhere would hide the new gate and let P1/P2/P3 regressions
/// silently slip past it.
///
/// Per the test-action-type convention adopted at P4 landing:
///
/// - `setup_execute_authz` defaults to
///   `Stage2ActionType::JupiterBuySolWithUsdc`. Existing P1/P2/P3
///   tests therefore exercise the Jupiter action path; the program
///   skips the Solend boundary gate and `None` is the correct value.
/// - P4 boundary tests opt in by using `setup_execute_authz_solend`
///   (Solend action) and call `raw_execute_action_instruction`
///   directly with an explicit `Some(boundary_proof)`.
#[allow(clippy::too_many_arguments)]
fn execute_action_instruction(
    program_id: &Pubkey,
    executor: &Pubkey,
    user: &Pubkey,
    delegated_wallet: &Pubkey,
    schema_version: u8,
    rule_id: [u8; 16],
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    input_amount_raw: u64,
    execution_nonce: u64,
) -> Instruction {
    // J4: when the test's authz uses the Jupiter action type, the
    // on-chain `process_execute_action` requires a non-None
    // `JupiterBoundaryProof` with the canonical sibling-ix list and
    // the right destination. We attach a default-passing proof here
    // so existing tests that target NON-Jupiter-boundary properties
    // (lifecycle, P2/P3 gates) keep working without each test having
    // to construct a verbose proof. Jupiter-boundary negative tests
    // bypass this wrapper and call `raw_execute_action_instruction`
    // directly with an explicit `Some(jupiter_proof)`.
    let jupiter_proof = if action_type
        == clawsol_authority::state::Stage2ActionType::JupiterBuySolWithUsdc.to_u8()
    {
        Some(default_jupiter_boundary_proof(delegated_wallet, &rule_id))
    } else {
        None
    };
    raw_execute_action_instruction(
        program_id,
        executor,
        user,
        delegated_wallet,
        schema_version,
        rule_id,
        canonical_rule_hash,
        action_type,
        input_amount_raw,
        execution_nonce,
        default_solend_proof(),
        None, // solend_boundary_proof: Option<SolendBoundaryProof>
        jupiter_proof,
    )
}

/// A canonical "passing Jupiter boundary proof" built from
/// `(delegated_wallet, rule_id)`. Returns a fresh value per call so
/// negative tests can mutate one field without affecting others.
///
/// **J4 deferral note.** This proof's `destination_pre_snapshot.amount`,
/// `destination_post_snapshot.amount`, and `min_output_amount_raw`
/// values are intentionally placeholders (0). The J4 verifier does
/// NOT consume them for any balance-delta computation; the future
/// trustless balance bracket slice will replace the snapshot fields
/// with on-chain SPL Token unpacks.
fn default_jupiter_boundary_proof(
    delegated_wallet: &Pubkey,
    rule_id: &[u8; 16],
) -> clawsol_authority::jupiter_boundary::JupiterBoundaryProof {
    use clawsol_authority::jupiter_boundary::{
        JupiterBoundaryProof, JupiterSiblingIxDescriptor, JupiterTokenAccountFixture,
        ATA_PROGRAM_ID, COMPUTE_BUDGET_PROGRAM_ID, JUPITER_V6_PROGRAM_ID_MAINNET,
        SPL_TOKEN_PROGRAM_ID,
    };
    let destination = fx_destination(rule_id);
    let dest_mint = fx_destination_mint(rule_id);
    let input_mint = fx_input_mint(rule_id);
    let in_ata = fx_in_ata(rule_id);
    let out_ata = fx_out_ata(rule_id);
    JupiterBoundaryProof {
        jupiter_program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
        expected_destination_token_account: destination,
        expected_destination_mint: dest_mint,
        expected_input_mint: input_mint,
        delegated_input_token_account: in_ata,
        delegated_output_token_account: out_ata,
        destination_pre_snapshot: JupiterTokenAccountFixture {
            address: destination,
            mint: dest_mint,
            owner: *delegated_wallet,
            amount: 0,
        },
        destination_post_snapshot: JupiterTokenAccountFixture {
            address: destination,
            mint: dest_mint,
            owner: *delegated_wallet,
            amount: 0,
        },
        min_output_amount_raw: 0,
        sibling_instructions: vec![
            JupiterSiblingIxDescriptor {
                program_id: COMPUTE_BUDGET_PROGRAM_ID,
                variant_byte: 2,
                writable_accounts: vec![],
            },
            JupiterSiblingIxDescriptor {
                program_id: ATA_PROGRAM_ID,
                variant_byte: 1,
                writable_accounts: vec![destination, *delegated_wallet],
            },
            JupiterSiblingIxDescriptor {
                program_id: JUPITER_V6_PROGRAM_ID_MAINNET,
                variant_byte: 0xE5,
                writable_accounts: vec![in_ata, out_ata],
            },
            JupiterSiblingIxDescriptor {
                program_id: SPL_TOKEN_PROGRAM_ID,
                variant_byte: 9, // CloseAccount on the wSOL ATA
                writable_accounts: vec![out_ata, *delegated_wallet],
            },
        ],
    }
}

/// Deterministic destination-mint pubkey derived per `rule_id`.
fn fx_destination_mint(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x4D; // 'M' for "mint"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic input-mint pubkey derived per `rule_id`.
fn fx_input_mint(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x49; // 'I' for "input"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic input ATA pubkey derived per `rule_id`.
fn fx_in_ata(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x69; // 'i' for "in_ata"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

/// Deterministic output ATA pubkey derived per `rule_id`.
fn fx_out_ata(rule_id: &[u8; 16]) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x6F; // 'o' for "out_ata"
    b[16..32].copy_from_slice(rule_id);
    Pubkey::new_from_array(b)
}

fn build_program_test(program_id: Pubkey) -> ProgramTest {
    ProgramTest::new(
        "clawsol_authority",
        program_id,
        processor!(clawsol_authority::process_instruction),
    )
}

fn dummy_canonical_hash(rule_id: &[u8; 16]) -> [u8; 32] {
    // Deterministic test-only hash. The program does NOT recompute or
    // verify this hash in P1 (it's a commitment to the off-chain rule
    // body) — the hash is just stored and round-trip-asserted.
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(rule_id);
    h[16..].copy_from_slice(rule_id);
    h
}

#[tokio::test]
async fn create_authorization_happy_path_writes_all_fields() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let executor = Pubkey::new_unique();
    let delegated_wallet = Pubkey::new_unique();
    let destination = Pubkey::new_unique();
    let canonical_rule_hash = dummy_canonical_hash(&RULE_ID_HAPPY);
    let max_input_amount_raw = 5_000_000u64; // 5 USDC
    let expires_at_slot = 1_000_000u64;
    let action_type = Stage2ActionType::SolendWithdrawAllDelegated.to_u8();

    let (pda, expected_bump) =
        derive_authorization_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &RULE_ID_HAPPY);

    let ix = create_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_HAPPY,
        executor,
        delegated_wallet,
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    );

    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("happy-path tx must succeed");

    let account = ctx
        .banks_client
        .get_account(pda)
        .await
        .expect("get_account rpc")
        .expect("PDA must exist after create_authorization");
    assert_eq!(account.owner, program_id);
    assert_eq!(account.data.len(), AuthorizationRecord::LEN);

    let record = AuthorizationRecord::try_from_slice(&account.data)
        .expect("decode AuthorizationRecord");
    assert_eq!(record.schema_version, SCHEMA_VERSION);
    assert_eq!(record.rule_id, RULE_ID_HAPPY);
    assert_eq!(record.user, ctx.payer.pubkey());
    assert_eq!(record.executor, executor);
    assert_eq!(record.delegated_wallet, delegated_wallet);
    assert_eq!(record.canonical_rule_hash, canonical_rule_hash);
    assert_eq!(record.allowed_action_type, action_type);
    assert_eq!(record.max_input_amount_raw, max_input_amount_raw);
    assert_eq!(record.used_amount_raw, 0);
    assert_eq!(record.destination, destination);
    assert_eq!(record.expires_at_slot, expires_at_slot);
    assert!(!record.revoked);
    assert!(!record.completed);
    assert_eq!(record.execution_nonce, 0);
    assert_eq!(record.last_execution_slot, 0);
    assert_eq!(record.bump, expected_bump);
}

#[tokio::test]
async fn create_rejects_wrong_pda() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    // Build an instruction by hand with a deliberately-wrong PDA in
    // the AccountMeta slot (some unrelated newly-generated pubkey).
    let bad_pda = Pubkey::new_unique();
    let data = borsh::to_vec(&AuthorityInstruction::CreateAuthorization {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_WRONG_PDA,
        executor: Pubkey::new_unique(),
        delegated_wallet: Pubkey::new_unique(),
        canonical_rule_hash: dummy_canonical_hash(&RULE_ID_WRONG_PDA),
        allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        max_input_amount_raw: 5_000_000,
        destination: Pubkey::new_unique(),
        expires_at_slot: 1_000_000,
    })
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(ctx.payer.pubkey(), true),
            AccountMeta::new(bad_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong-PDA tx must fail");
    assert_custom_err(err, AuthorityError::InvalidPda);
}

#[tokio::test]
async fn create_rejects_expired_authorization() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    // Warp past the expiry slot we'll request — current slot >= expires.
    ctx.warp_to_slot(100).expect("warp_to_slot");

    let ix = create_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_EXPIRED,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        dummy_canonical_hash(&RULE_ID_EXPIRED),
        Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        5_000_000,
        Pubkey::new_unique(),
        50, // already in the past
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("expired tx must fail");
    assert_custom_err(err, AuthorityError::AuthorizationExpired);
}

#[tokio::test]
async fn create_rejects_duplicate_authorization() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    let canonical_rule_hash = dummy_canonical_hash(&RULE_ID_DUP_A);

    let make_ix = || {
        create_authorization_instruction(
            &program_id,
            &ctx.payer.pubkey(),
            SCHEMA_VERSION,
            RULE_ID_DUP_A,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            canonical_rule_hash,
            Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            5_000_000,
            Pubkey::new_unique(),
            1_000_000,
        )
    };

    let tx1 = Transaction::new_signed_with_payer(
        &[make_ix()],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx1)
        .await
        .expect("first create_authorization must succeed");

    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");

    let tx2 = Transaction::new_signed_with_payer(
        &[make_ix()],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx2)
        .await
        .expect_err("duplicate create_authorization must fail");
    // The system program returns AccountAlreadyInUse; we just need to
    // see *some* InstructionError from the system-level CPI.
    let TransactionError::InstructionError(_, _) = err.unwrap() else {
        panic!("expected InstructionError on duplicate create_authorization");
    };
}

#[tokio::test]
async fn create_rejects_zero_max_amount() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let ix = create_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_ZERO_AMT,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        dummy_canonical_hash(&RULE_ID_ZERO_AMT),
        Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        0, // not allowed
        Pubkey::new_unique(),
        1_000_000,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("zero max_input_amount_raw must fail");
    assert_custom_err(err, AuthorityError::InvalidZeroMaxAmount);
}

#[tokio::test]
async fn create_rejects_missing_user_signature() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let unrelated_user = Keypair::new();
    let (pda, _) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &unrelated_user.pubkey(),
        &RULE_ID_NO_SIGNER,
    );

    let data = borsh::to_vec(&AuthorityInstruction::CreateAuthorization {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_NO_SIGNER,
        executor: Pubkey::new_unique(),
        delegated_wallet: Pubkey::new_unique(),
        canonical_rule_hash: dummy_canonical_hash(&RULE_ID_NO_SIGNER),
        allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        max_input_amount_raw: 5_000_000,
        destination: Pubkey::new_unique(),
        expires_at_slot: 1_000_000,
    })
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            // user is NOT marked as signer
            AccountMeta::new(unrelated_user.pubkey(), false),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("missing user signature must fail");
    assert_custom_err(err, AuthorityError::UserMustSign);
}

#[tokio::test]
async fn revoke_happy_path_flips_revoked_flag() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let (pda, _) =
        derive_authorization_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &RULE_ID_REVOKE_OK);
    create_authz_for_test(&mut ctx, program_id, &RULE_ID_REVOKE_OK, 5_000_000, 1_000_000).await;

    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");

    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_REVOKE_OK,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("revoke must succeed");

    let acc = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(record.revoked, "revoked must be true after revoke");
    assert!(!record.completed, "completed must remain false in P1");
    assert_eq!(record.execution_nonce, 0, "execute fields untouched");
    assert_eq!(record.last_execution_slot, 0);
    // Sanity: every other field must round-trip unchanged.
    assert_eq!(record.user, ctx.payer.pubkey());
    assert_eq!(record.rule_id, RULE_ID_REVOKE_OK);
}

#[tokio::test]
async fn revoke_rejects_wrong_user() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    create_authz_for_test(&mut ctx, program_id, &RULE_ID_REVOKE_WRONG_USER, 5_000_000, 1_000_000).await;

    let attacker = Keypair::new();
    // Fund the attacker so the test transaction can be signed by them
    // as fee payer (and therefore as Revoke's account[0] signer).
    fund_keypair(&mut ctx, &attacker.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");

    // The PDA was created under ctx.payer; the attacker tries to
    // revoke their own freshly-derived (different) PDA — wait, the
    // PDA is keyed by user, so a revoke from the attacker addresses a
    // *different* PDA address. To exercise RevokeWrongUser we must
    // pass the original PDA address but pretend the attacker is the
    // signer. We do that by hand-rolling the AccountMetas.
    let (pda, _) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &ctx.payer.pubkey(),
        &RULE_ID_REVOKE_WRONG_USER,
    );
    let data = borsh::to_vec(&AuthorityInstruction::Revoke).unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new(pda, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong-user revoke must fail");
    assert_custom_err(err, AuthorityError::RevokeWrongUser);
}

#[tokio::test]
async fn revoke_is_idempotent_and_revoked_persists() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let (pda, _) =
        derive_authorization_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &RULE_ID_REVOKE_IDEMP);
    create_authz_for_test(&mut ctx, program_id, &RULE_ID_REVOKE_IDEMP, 5_000_000, 1_000_000).await;

    // First revoke.
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_REVOKE_IDEMP,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("first revoke ok");

    // Second revoke — must succeed (no-op, idempotent).
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_REVOKE_IDEMP,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("second revoke must be a no-op success");

    let acc = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(
        record.revoked,
        "revoked must remain true after the idempotent second revoke"
    );
}

#[tokio::test]
async fn close_after_revoke_returns_rent_to_user() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let (pda, _) =
        derive_authorization_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &RULE_ID_CLOSE_OK);
    create_authz_for_test(&mut ctx, program_id, &RULE_ID_CLOSE_OK, 5_000_000, 1_000_000).await;

    // Revoke first to make the PDA closeable.
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_CLOSE_OK,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("revoke before close");

    let pda_acct = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    let pda_lamports = pda_acct.lamports;
    assert!(pda_lamports > 0);

    let user_before = ctx
        .banks_client
        .get_account(ctx.payer.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;

    // Close.
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let ix = close_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_CLOSE_OK,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("close ok");

    // PDA must be gone (zero lamports / closed) and the user balance
    // must have grown by at least pda_lamports minus the tx fee.
    let pda_after = ctx.banks_client.get_account(pda).await.unwrap();
    let pda_after_lamports = pda_after.as_ref().map(|a| a.lamports).unwrap_or(0);
    assert_eq!(pda_after_lamports, 0, "PDA must be closed (lamports = 0)");

    let user_after = ctx
        .banks_client
        .get_account(ctx.payer.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    // user_after = user_before + pda_lamports - tx_fee. We don't know
    // the exact fee, but the delta must be > 0 and <= pda_lamports.
    let delta = user_after as i128 - user_before as i128;
    assert!(
        delta > 0,
        "rent reclaim must net positive for user (got delta {delta})"
    );
    assert!(
        delta <= pda_lamports as i128,
        "user delta ({delta}) must not exceed PDA lamports ({pda_lamports})"
    );
}

#[tokio::test]
async fn close_before_revoke_or_expiry_rejected() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    create_authz_for_test(&mut ctx, program_id, &RULE_ID_CLOSE_TOO_EARLY, 5_000_000, 1_000_000).await;

    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let ix = close_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_CLOSE_TOO_EARLY,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("close must fail before revoke/expire/complete");
    assert_custom_err(err, AuthorityError::NotCloseable);
}

// ── Stage 2 P2: ExecuteAction tests ─────────────────────────────────────────

/// Inputs every ExecuteAction setup needs: rule_id, the authz hash the
/// user signed, the executor keypair the daemon signs with, and the
/// (writable) PDA the processor mutates.
///
/// `destination` is captured here in P4 so test-side P4 boundary
/// proofs can pin against `record.destination` without re-deriving
/// from the rule_id. The same pubkey is supplied by `fx_destination`
/// during create-authz time and is reused by the boundary proof
/// builder during execute time.
///
/// `destination` and `expires_at_slot` are intentionally captured
/// even though most current call sites rebuild them from `rule_id`
/// helpers — keeping them on the fixture documents the create-time
/// values and lets future tests assert against them without
/// recomputing.
#[allow(dead_code)]
struct ExecuteFixture {
    pda: Pubkey,
    executor: Keypair,
    delegated_wallet: Pubkey,
    destination: Pubkey,
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    max_input_amount_raw: u64,
    expires_at_slot: u64,
}

/// Default `setup_execute_authz`. Action type is
/// `Stage2ActionType::JupiterBuySolWithUsdc` so existing P1/P2/P3
/// tests do NOT trip the new P4 Solend boundary gate. Tests that
/// specifically need a Solend authz call `setup_execute_authz_solend`
/// instead.
async fn setup_execute_authz(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    rule_id: &[u8; 16],
    max_input_amount_raw: u64,
    expires_at_slot: u64,
) -> ExecuteFixture {
    setup_execute_authz_with_action(
        ctx,
        program_id,
        rule_id,
        max_input_amount_raw,
        expires_at_slot,
        Stage2ActionType::JupiterBuySolWithUsdc.to_u8(),
    )
    .await
}

/// Solend-action variant of [`setup_execute_authz`]. Used by the P4
/// boundary tests that DO need to exercise the Solend gate. Callers
/// MUST attach an explicit `solend_boundary_proof` via
/// `raw_execute_action_instruction` — the wrapper
/// `execute_action_instruction` always passes `None`.
async fn setup_execute_authz_solend(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    rule_id: &[u8; 16],
    max_input_amount_raw: u64,
    expires_at_slot: u64,
) -> ExecuteFixture {
    setup_execute_authz_with_action(
        ctx,
        program_id,
        rule_id,
        max_input_amount_raw,
        expires_at_slot,
        Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
    )
    .await
}

async fn setup_execute_authz_with_action(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    rule_id: &[u8; 16],
    max_input_amount_raw: u64,
    expires_at_slot: u64,
    action_type: u8,
) -> ExecuteFixture {
    let executor = Keypair::new();
    let delegated_wallet = Pubkey::new_unique();
    // Destination is deterministic per rule_id so the test-side
    // `default_solend_boundary_proof` can rebuild a matching proof
    // for Solend tests without threading destination through every
    // call site.
    let destination = fx_destination(rule_id);
    let canonical_rule_hash = dummy_canonical_hash(rule_id);
    let (pda, _) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &ctx.payer.pubkey(),
        rule_id,
    );

    let ix = create_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        *rule_id,
        executor.pubkey(),
        delegated_wallet,
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        destination,
        expires_at_slot,
    );
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("setup create_authorization for execute");

    ExecuteFixture {
        pda,
        executor,
        delegated_wallet,
        destination,
        canonical_rule_hash,
        action_type,
        max_input_amount_raw,
        expires_at_slot,
    }
}

#[tokio::test]
async fn execute_action_happy_path_mutates_state() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_HAPPY,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let input_amount = 5_000_000u64;
    let nonce = 1u64;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_HAPPY,
        fx.canonical_rule_hash,
        fx.action_type,
        input_amount,
        nonce,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("execute happy path must succeed");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, input_amount, "used_amount_raw advanced");
    assert!(record.completed, "completed must flip true");
    assert_eq!(record.execution_nonce, nonce, "execution_nonce set to arg");
    assert!(
        record.last_execution_slot > 0,
        "last_execution_slot must record clock slot"
    );
    // record.revoked must remain false; rule body must be unchanged.
    assert!(!record.revoked);
    assert_eq!(record.canonical_rule_hash, fx.canonical_rule_hash);
    assert_eq!(record.max_input_amount_raw, fx.max_input_amount_raw);
}

#[tokio::test]
async fn execute_rejects_missing_executor_signature() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_NO_SIG,
        5_000_000,
        1_000_000,
    )
    .await;

    // Hand-roll the ix so executor account is NOT marked signer.
    // The MissingExecutorSignature gate (P2 step 1) fires before the
    // P4 Solend boundary check, so passing `None` here is fine.
    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_EXEC_NO_SIG,
        canonical_rule_hash: fx.canonical_rule_hash,
        action_type: fx.action_type,
        input_amount_raw: 5_000_000,
        execution_nonce: 1,
        condition_proof: default_solend_proof(),
        solend_boundary_proof: None,
        jupiter_boundary_proof: None,
    })
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(fx.executor.pubkey(), false), // signer flag false
            AccountMeta::new(fx.pda, false),
            AccountMeta::new_readonly(ctx.payer.pubkey(), false),
            AccountMeta::new_readonly(fx.delegated_wallet, false),
        ],
        data,
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("missing executor signature must fail");
    assert_custom_err(err, AuthorityError::MissingExecutorSignature);
}

#[tokio::test]
async fn execute_rejects_wrong_pda() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_BAD_PDA,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Derive a PDA from a *different* rule_id but pass the original
    // rule_id in the instruction args. The arg-derived seed list won't
    // match the wrong-PDA account key.
    let other_rule_id = [0xDD; 16];
    let (wrong_pda, _) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &ctx.payer.pubkey(),
        &other_rule_id,
    );
    // wrong_pda doesn't even exist on chain (no account yet) — owner
    // check would reject first if the program checked owner before PDA,
    // so here we test the explicit InvalidAuthorizationPda path by
    // creating an account at the wrong derivation. Easier alternative:
    // pass an account owned by program_id but at a wrong-PDA address
    // via cooking the test. To keep the test simple, we just pass the
    // wrong-PDA address; the program's owner check fires first
    // (owner != program_id because account is nonexistent / system-owned).
    let _ = wrong_pda;

    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_EXEC_BAD_PDA, // arg drives derivation
        canonical_rule_hash: fx.canonical_rule_hash,
        action_type: fx.action_type,
        input_amount_raw: 5_000_000,
        execution_nonce: 1,
        condition_proof: default_solend_proof(),
        // owner-mismatch fires before the P4 boundary, so None is fine.
        solend_boundary_proof: None,
        jupiter_boundary_proof: None,
    })
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(fx.executor.pubkey(), true),
            AccountMeta::new(wrong_pda, false), // wrong PDA address
            AccountMeta::new_readonly(ctx.payer.pubkey(), false),
            AccountMeta::new_readonly(fx.delegated_wallet, false),
        ],
        data,
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong PDA must fail");
    // Owner check (account is system-owned) fires before PDA derivation.
    assert_custom_err(err, AuthorityError::AuthorizationOwnerMismatch);
}

#[tokio::test]
async fn execute_rejects_wrong_user_account() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_WRONG_USER,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Pass a user account that isn't the one baked into the record.
    // Note: PDA derivation uses the *passed* user account, so swapping
    // the user changes the expected PDA, which means the arg's rule_id
    // + the wrong user => a different derived PDA than the one we
    // supply in account[1]. The InvalidAuthorizationPda check fires
    // first.
    let attacker = Pubkey::new_unique();

    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_EXEC_WRONG_USER,
        canonical_rule_hash: fx.canonical_rule_hash,
        action_type: fx.action_type,
        input_amount_raw: 5_000_000,
        execution_nonce: 1,
        condition_proof: default_solend_proof(),
        // PDA-derivation mismatch fires before the P4 boundary check.
        solend_boundary_proof: None,
        jupiter_boundary_proof: None,
    })
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(fx.executor.pubkey(), true),
            AccountMeta::new(fx.pda, false),
            AccountMeta::new_readonly(attacker, false), // wrong user
            AccountMeta::new_readonly(fx.delegated_wallet, false),
        ],
        data,
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong user must fail");
    // Seeds derived from the wrong user => derived PDA != supplied PDA.
    assert_custom_err(err, AuthorityError::InvalidAuthorizationPda);
}

#[tokio::test]
async fn execute_rejects_wrong_executor_signer() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_WRONG_EXEC,
        5_000_000,
        1_000_000,
    )
    .await;

    let attacker_executor = Keypair::new();
    fund_keypair(&mut ctx, &attacker_executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &attacker_executor.pubkey(), // wrong executor
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_WRONG_EXEC,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker_executor.pubkey()),
        &[&attacker_executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong executor must fail");
    assert_custom_err(err, AuthorityError::ExecutorMismatch);
}

#[tokio::test]
async fn execute_rejects_at_expiry_boundary() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    // Set expiry close to current slot + 1, then warp so current_slot
    // == expires_at_slot exactly. The check is strict (<), so equality
    // must fail.
    let expires_at_slot = 50u64;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_EXPIRY_EQ,
        5_000_000,
        expires_at_slot,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;
    ctx.warp_to_slot(expires_at_slot).expect("warp_to_slot ==");

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_EXPIRY_EQ,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("expiry == current slot must fail (strict <)");
    assert_custom_err(err, AuthorityError::AuthorizationExpired);
}

#[tokio::test]
async fn execute_rejects_past_expiry() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let expires_at_slot = 50u64;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_EXPIRY_GT,
        5_000_000,
        expires_at_slot,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;
    ctx.warp_to_slot(expires_at_slot + 100).expect("warp past");

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_EXPIRY_GT,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("expiry < current slot must fail");
    assert_custom_err(err, AuthorityError::AuthorizationExpired);
}

#[tokio::test]
async fn execute_rejects_revoked_authorization() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_REVOKED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Revoke first.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let revoke_ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_EXEC_REVOKED,
    );
    let tx = Transaction::new_signed_with_payer(
        &[revoke_ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("revoke ok");

    // Then try to execute.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_REVOKED,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("revoked authz must reject execute");
    assert_custom_err(err, AuthorityError::AuthorizationRevoked);
}

#[tokio::test]
async fn execute_rejects_already_completed() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_COMPLETED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // First execute consumes the cap and flips completed=true.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix1 = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_COMPLETED,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("first execute ok");

    // Second execute must fail with AuthorizationCompleted (note: nonce
    // would also be wrong; the completed check fires earlier in the
    // boundary order).
    ctx.warp_to_slot(50).expect("warp to next slot to dodge SameSlotReplay");
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix2 = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_COMPLETED,
        fx.canonical_rule_hash,
        fx.action_type,
        1, // tiny non-zero amount, won't matter — completed check fires first
        2,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("second execute must fail");
    assert_custom_err(err, AuthorityError::AuthorizationCompleted);
}

#[tokio::test]
async fn execute_rejects_canonical_rule_hash_mismatch() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_HASH_MISMATCH,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let bad_hash = [0xFFu8; 32];

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_HASH_MISMATCH,
        bad_hash, // mismatched
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("hash mismatch must fail");
    assert_custom_err(err, AuthorityError::RuleHashMismatch);
}

#[tokio::test]
async fn execute_rejects_action_type_mismatch() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_TYPE_MISMATCH,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;
    // setup_execute_authz defaults to JupiterBuySolWithUsdc (per the
    // P4 test-action-type convention). Try to execute as
    // SolendWithdrawAllDelegated to trip the action_type gate.
    let wrong_action = Stage2ActionType::SolendWithdrawAllDelegated.to_u8();
    assert_ne!(wrong_action, fx.action_type);

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_TYPE_MISMATCH,
        fx.canonical_rule_hash,
        wrong_action,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("action_type mismatch must fail");
    assert_custom_err(err, AuthorityError::ActionTypeMismatch);
}

#[tokio::test]
async fn execute_rejects_zero_input_amount() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_ZERO_AMT,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_ZERO_AMT,
        fx.canonical_rule_hash,
        fx.action_type,
        0, // zero
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("zero input amount must fail");
    assert_custom_err(err, AuthorityError::InputAmountZero);
}

#[tokio::test]
async fn execute_rejects_input_over_cap() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_OVER_CAP,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_OVER_CAP,
        fx.canonical_rule_hash,
        fx.action_type,
        fx.max_input_amount_raw + 1, // over cap
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("over-cap input must fail");
    assert_custom_err(err, AuthorityError::InputAmountExceeded);
}

#[tokio::test]
async fn execute_rejects_wrong_execution_nonce() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_BAD_NONCE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;
    // record.execution_nonce starts at 0, so the first valid nonce is
    // 1. Submit nonce=2 which is wrong by +1.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_BAD_NONCE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        2, // wrong (should be 1)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong nonce must fail");
    assert_custom_err(err, AuthorityError::ExecutionNonceMismatch);
}

#[tokio::test]
async fn execute_replay_after_completed_fails() {
    // Variant of execute_rejects_already_completed — explicit "second
    // execute attempt with the *next* nonce on the same record" path.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_EXEC_REPLAY,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // First execute consumes 1 lamport unit (under cap), succeeds.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix1 = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_REPLAY,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("first ok");

    ctx.warp_to_slot(50).expect("warp slot");
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix2 = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_EXEC_REPLAY,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        2, // next valid nonce (would pass nonce check) — but completed=true blocks
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("replay after completed must fail");
    assert_custom_err(err, AuthorityError::AuthorizationCompleted);
}

// ── P1 regressions: revoke is still idempotent, close still gated ────────────

#[tokio::test]
async fn p1_revoke_idempotent_after_p2_addition() {
    // Same shape as the original `revoke_is_idempotent_and_revoked_persists`
    // test, repeated under a P2-distinct rule_id to surface any P1
    // regression introduced by adding ExecuteAction.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let regression_id = [0xF0; 16];
    let (pda, _) =
        derive_authorization_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &regression_id);
    create_authz_for_test(&mut ctx, program_id, &regression_id, 5_000_000, 1_000_000).await;

    // First revoke.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        regression_id,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("first revoke");
    // Second revoke: still no-op success.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        regression_id,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("second revoke noop");

    let acc = ctx.banks_client.get_account(pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(record.revoked);
}

#[tokio::test]
async fn p1_close_still_requires_revoked_or_completed_or_expired() {
    // Same gate as the original `close_before_revoke_or_expiry_rejected`
    // test under a fresh rule_id, to confirm P2 didn't inadvertently
    // open a close path.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let regression_id = [0xF1; 16];
    create_authz_for_test(&mut ctx, program_id, &regression_id, 5_000_000, 1_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = close_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        regression_id,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("close still gated");
    assert_custom_err(err, AuthorityError::NotCloseable);
}

#[tokio::test]
async fn close_after_p2_completed_succeeds() {
    // P2 sets completed=true on a successful execute. close must accept
    // a completed authz (this codifies one of P1's three close gates,
    // now that P2 actually flips `completed`).
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let regression_id = [0xF2; 16];
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &regression_id,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Execute -> completed=true.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        regression_id,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("execute ok");

    // close must succeed now.
    ctx.warp_to_slot(50).expect("warp");
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = close_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        regression_id,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.expect("close after completed ok");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap();
    assert!(acc.is_none() || acc.unwrap().lamports == 0);
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn create_authz_for_test(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    rule_id: &[u8; 16],
    max_input_amount_raw: u64,
    expires_at_slot: u64,
) {
    let ix = create_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        *rule_id,
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        dummy_canonical_hash(rule_id),
        Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
        max_input_amount_raw,
        Pubkey::new_unique(),
        expires_at_slot,
    );
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("setup create_authorization must succeed");
}

async fn fund_keypair(ctx: &mut ProgramTestContext, to: &Pubkey, lamports: u64) {
    let rent = Rent::default();
    let _ = rent; // placate unused-import in case future tests use it
    let ix = system_instruction::transfer(&ctx.payer.pubkey(), to, lamports);
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("fund_keypair");
}

fn assert_custom_err(err: solana_program_test::BanksClientError, expected: AuthorityError) {
    let tx_err = err.unwrap();
    match tx_err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(
                code, expected as u32,
                "expected AuthorityError::{:?} (code {}), got code {}",
                expected, expected as u32, code
            );
        }
        other => panic!("expected Custom InstructionError, got {other:?}"),
    }
}

trait BanksClientErrorExt {
    fn unwrap(self) -> TransactionError;
}

impl BanksClientErrorExt for solana_program_test::BanksClientError {
    fn unwrap(self) -> TransactionError {
        match self {
            solana_program_test::BanksClientError::TransactionError(e) => e,
            solana_program_test::BanksClientError::SimulationError { err, .. } => err,
            other => panic!("unexpected BanksClientError variant: {other:?}"),
        }
    }
}

// ── Stage 2 P3: condition gate tests ────────────────────────────────────────
//
// These tests prove every documented P3 failure mode fails closed BEFORE
// state mutation. Each negative case reloads the AuthorizationRecord
// after the failed tx and asserts (`assert_authz_unmutated`) that
// `used_amount_raw == 0`, `completed == false`, `execution_nonce == 0`,
// `last_execution_slot == 0` — i.e. the boundary skeleton genuinely
// gated mutation behind the condition verifier.

// Distinct rule_ids to avoid PDA collisions across the P3 test set.
const RULE_ID_P3_SOLEND_TRUE: [u8; 16] = [0xF3; 16];
const RULE_ID_P3_SOLEND_FALSE: [u8; 16] = [0xF4; 16];
const RULE_ID_P3_PYTH_BASKET_TRUE: [u8; 16] = [0xF5; 16];
const RULE_ID_P3_PYTH_BASKET_FALSE: [u8; 16] = [0xF6; 16];
const RULE_ID_P3_MISSING_PROOF: [u8; 16] = [0xF7; 16];
const RULE_ID_P3_STALE_PYTH: [u8; 16] = [0xF8; 16];
const RULE_ID_P3_WRONG_FEED: [u8; 16] = [0xF9; 16];
const RULE_ID_P3_EXCESS_CONF: [u8; 16] = [0xFA; 16];
const RULE_ID_P3_PARTIAL_VERIF: [u8; 16] = [0xFB; 16];
const RULE_ID_P3_STALE_SOLEND: [u8; 16] = [0xFC; 16];
const RULE_ID_P3_BAD_FORMULA: [u8; 16] = [0xFD; 16];
const RULE_ID_P3_BAD_LOGIC: [u8; 16] = [0xFE; 16];
const RULE_ID_P3_REVOKED_VS_GATE: [u8; 16] = [0x53; 16];

// Mirror BTC/ETH/SOL feed ids from claw_types fixture B (same bytes the
// verifier's own internal tests use). Repeated here to avoid a runtime
// dep on `claw-types` from the on-chain crate's Cargo.toml.
const BTC_USD_FEED_ID: [u8; 32] = [
    0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1, 0x2d, 0xe5,
    0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe, 0xdf, 0x0f, 0x4a, 0x41,
    0x5b, 0x43,
];
const ETH_USD_FEED_ID: [u8; 32] = [
    0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd, 0x1b, 0x64,
    0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80, 0x87, 0x46, 0x34, 0xfd,
    0x0a, 0xce,
];
const SOL_USD_FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda,
    0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80,
    0xb5, 0x6d,
];

/// Fetch the on-chain Clock so test snapshots can be built relative to
/// the same `unix_timestamp` / `slot` the program will see in
/// `Clock::get()`.
async fn current_clock(ctx: &mut ProgramTestContext) -> solana_sdk::clock::Clock {
    let acct = ctx
        .banks_client
        .get_account(solana_sdk::sysvar::clock::id())
        .await
        .expect("get_account for clock sysvar")
        .expect("clock sysvar account exists");
    bincode::deserialize::<solana_sdk::clock::Clock>(&acct.data).expect("Clock decode")
}

/// Solend "low-utilisation reserve, current slot" snapshot — the verifier's
/// own tests prove this fixture is fresh and well-formed.
fn fresh_solend_snapshot(last_update_slot: u64) -> SolendReserveSnapshot {
    SolendReserveSnapshot {
        available_amount: 5_000_000_000_000,
        borrowed_amount_wads: 5_000_000_000_000u128 * SOLEND_WAD,
        min_borrow_rate_pct: 0,
        optimal_borrow_rate_pct: 4,
        max_borrow_rate_pct: 30,
        super_max_borrow_rate_pct: 300,
        optimal_utilization_rate_pct: 80,
        max_utilization_rate_pct: 95,
        protocol_take_rate_pct: 20,
        last_update_slot,
        stale_flag: false,
    }
}

fn solend_apr_lt_condition(threshold_bps: u32) -> SolendSupplyAprCondition {
    SolendSupplyAprCondition {
        comparison: Comparison::Lt,
        threshold_bps,
        rate_kind: RateKind::Apr,
        formula_version: SUPPORTED_SOLEND_FORMULA_VERSION,
        max_reserve_staleness_slots: 1_000_000,
    }
}

fn pyth_btc_gt_75k_condition() -> PythPriceCondition {
    PythPriceCondition {
        feed_id: BTC_USD_FEED_ID,
        comparison: Comparison::Gt,
        threshold_mantissa: 7_500_000,
        threshold_exponent: -2,
        max_age_seconds: u32::MAX, // freshness controlled by snapshot age
        max_confidence_bps: 50,
        verification_level_required: VerificationLevel::Full,
        bound_mode: BoundMode::AdverseLowerForGt,
    }
}

fn pyth_eth_gt_2300_condition() -> PythPriceCondition {
    PythPriceCondition {
        feed_id: ETH_USD_FEED_ID,
        comparison: Comparison::Gt,
        threshold_mantissa: 230_000,
        threshold_exponent: -2,
        max_age_seconds: u32::MAX,
        max_confidence_bps: 50,
        verification_level_required: VerificationLevel::Full,
        bound_mode: BoundMode::AdverseLowerForGt,
    }
}

fn pyth_sol_lt_90_condition() -> PythPriceCondition {
    PythPriceCondition {
        feed_id: SOL_USD_FEED_ID,
        comparison: Comparison::Lt,
        threshold_mantissa: 9_000,
        threshold_exponent: -2,
        max_age_seconds: u32::MAX,
        max_confidence_bps: 50,
        verification_level_required: VerificationLevel::Full,
        bound_mode: BoundMode::AdverseUpperForLt,
    }
}

fn pyth_snapshot(
    feed_id: [u8; 32],
    price_mantissa: i64,
    conf: u64,
    publish_time: i64,
) -> PythPriceSnapshot {
    PythPriceSnapshot {
        feed_id,
        price_mantissa,
        price_exponent: -8,
        conf,
        publish_time,
        verification_level: VerificationLevel::Full,
    }
}

/// Reload the PDA after a failed tx and assert no mutation happened —
/// every replay-protection field is still in the zero state set at
/// `create_authorization` time.
async fn assert_authz_unmutated(ctx: &mut ProgramTestContext, pda: Pubkey) {
    let account = ctx
        .banks_client
        .get_account(pda)
        .await
        .expect("get_account")
        .expect("PDA must still exist (failed tx must NOT close)");
    let record = AuthorizationRecord::try_from_slice(&account.data)
        .expect("decode AuthorizationRecord");
    assert_eq!(
        record.used_amount_raw, 0,
        "used_amount_raw must NOT advance on a failed condition"
    );
    assert!(
        !record.completed,
        "completed must NOT flip on a failed condition"
    );
    assert_eq!(
        record.execution_nonce, 0,
        "execution_nonce must NOT advance on a failed condition"
    );
    assert_eq!(
        record.last_execution_slot, 0,
        "last_execution_slot must NOT advance on a failed condition"
    );
    assert!(
        !record.revoked,
        "revoked must NOT flip on a failed condition (P3 doesn't revoke)"
    );
}

#[tokio::test]
async fn p3_scenario_a_solend_condition_true_succeeds_and_mutates_state() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_SOLEND_TRUE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: solend_apr_lt_condition(1_000), // 10% threshold
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    // Action defaults to Jupiter via setup_execute_authz, so the P4
    // Solend boundary gate is skipped (action_type != Solend), but
    // J4 requires a Jupiter boundary proof; supply a default-passing
    // one so this P3-condition test reaches the state-mutation path.
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_SOLEND_TRUE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_P3_SOLEND_TRUE,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("Scenario A condition-true must succeed");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, 5_000_000);
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1);
    assert!(record.last_execution_slot > 0);
}

#[tokio::test]
async fn p3_scenario_a_solend_condition_false_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_SOLEND_FALSE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    // Threshold 1 bps — APR is ~100 bps under low utilisation, so Lt
    // 1 bps does NOT fire.
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: solend_apr_lt_condition(1),
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_SOLEND_FALSE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        // P3 ConditionNotMet fires before the P4 boundary check, so
        // None is fine here.
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("Lt 1bps under ~100bps APR must NOT fire");
    assert_custom_err(err, AuthorityError::ConditionNotMet);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_scenario_b_three_pyth_conditions_true_succeeds_and_mutates_state() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_PYTH_BASKET_TRUE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let now_unix = clock.unix_timestamp;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![
            ProofCondition::Pyth {
                condition: pyth_btc_gt_75k_condition(),
                // BTC = 75,001.23 with -8 exp → adverse-lower well above 75000
                snapshot: pyth_snapshot(BTC_USD_FEED_ID, 7_500_123_000_000, 100, now_unix),
            },
            ProofCondition::Pyth {
                condition: pyth_eth_gt_2300_condition(),
                snapshot: pyth_snapshot(ETH_USD_FEED_ID, 232_000_000_000, 100, now_unix),
            },
            ProofCondition::Pyth {
                condition: pyth_sol_lt_90_condition(),
                // SOL = 89.50 with -8 exp → adverse-upper still < 90
                snapshot: pyth_snapshot(SOL_USD_FEED_ID, 8_950_000_000, 100, now_unix),
            },
        ],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    // Jupiter action via default setup; P4 boundary is skipped, J4
    // requires a Jupiter boundary proof to reach state mutation.
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_PYTH_BASKET_TRUE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_P3_PYTH_BASKET_TRUE,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("Scenario B all-three-conditions-true must succeed");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, 5_000_000);
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1);
}

#[tokio::test]
async fn p3_scenario_b_one_condition_false_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_PYTH_BASKET_FALSE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let now_unix = clock.unix_timestamp;
    // ETH = 2,290 < 2300 → ETH condition fails; All-fold returns false.
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![
            ProofCondition::Pyth {
                condition: pyth_btc_gt_75k_condition(),
                snapshot: pyth_snapshot(BTC_USD_FEED_ID, 7_500_123_000_000, 100, now_unix),
            },
            ProofCondition::Pyth {
                condition: pyth_eth_gt_2300_condition(),
                snapshot: pyth_snapshot(ETH_USD_FEED_ID, 229_000_000_000, 100, now_unix),
            },
            ProofCondition::Pyth {
                condition: pyth_sol_lt_90_condition(),
                snapshot: pyth_snapshot(SOL_USD_FEED_ID, 8_950_000_000, 100, now_unix),
            },
        ],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_PYTH_BASKET_FALSE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("ETH-miss under All-logic must fail");
    assert_custom_err(err, AuthorityError::ConditionNotMet);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_missing_condition_proof_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_MISSING_PROOF,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_MISSING_PROOF,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("empty proof must fail");
    assert_custom_err(err, AuthorityError::MissingConditionProof);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_stale_pyth_proof_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_STALE_PYTH,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    // 30 s window, snapshot 1 hour old.
    let mut cond = pyth_btc_gt_75k_condition();
    cond.max_age_seconds = 30;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Pyth {
            condition: cond,
            snapshot: pyth_snapshot(
                BTC_USD_FEED_ID,
                7_500_123_000_000,
                100,
                clock.unix_timestamp - 3_600,
            ),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_STALE_PYTH,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("stale Pyth must fail");
    assert_custom_err(err, AuthorityError::PythSnapshotStale);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_wrong_pyth_feed_id_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_WRONG_FEED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Pyth {
            condition: pyth_btc_gt_75k_condition(), // expects BTC feed
            // ...but snapshot.feed_id is SOL.
            snapshot: pyth_snapshot(SOL_USD_FEED_ID, 8_950_000_000, 100, clock.unix_timestamp),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_WRONG_FEED,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("wrong feed_id must fail");
    assert_custom_err(err, AuthorityError::PythFeedIdMismatch);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_excessive_pyth_confidence_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_EXCESS_CONF,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let cond = pyth_btc_gt_75k_condition();
    let price = 7_500_100_000_000i64;
    // conf = 1% of price → 100 bps, exceeds the 50-bps gate.
    let conf = (price as u64) / 100;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Pyth {
            condition: cond,
            snapshot: pyth_snapshot(BTC_USD_FEED_ID, price, conf, clock.unix_timestamp),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_EXCESS_CONF,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("excess confidence must fail");
    assert_custom_err(err, AuthorityError::PythConfidenceTooWide);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_partial_pyth_verification_fails_before_mutation() {
    // The mirror VerificationLevel only declares `Full` (audit U-5);
    // any byte > 0 in that slot is structurally invalid. We construct
    // an ExecuteAction whose serialised bytes carry a deliberately-
    // out-of-range verification_level byte and confirm Borsh
    // deserialization in the program rejects it (mapped to
    // `ProgramError::InvalidInstructionData`). State must NOT
    // mutate.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_PARTIAL_VERIF,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Pyth {
            condition: pyth_btc_gt_75k_condition(),
            snapshot: pyth_snapshot(BTC_USD_FEED_ID, 7_500_123_000_000, 100, clock.unix_timestamp),
        }],
    };
    let ix_base = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_PARTIAL_VERIF,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    // Locate the verification_level byte inside the serialized
    // PythPriceSnapshot — last byte of the snapshot (after price/exp/
    // conf/publish_time). We re-encode the proof and look for the
    // first 0x00 that corresponds to the snapshot's `verification_level`
    // field. Simpler: tail-scan and replace the LAST `0x00` byte (the
    // final byte of the conditions Vec body for the single condition,
    // which is the snapshot.verification_level slot).
    let mut data = ix_base.data.clone();
    let last_byte = data.len() - 1;
    assert_eq!(
        data[last_byte], 0,
        "verification_level slot must be 0 (Full) before tampering"
    );
    data[last_byte] = 0xFF; // out of range — Borsh rejects
    let tampered = Instruction {
        program_id: ix_base.program_id,
        accounts: ix_base.accounts.clone(),
        data,
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[tampered],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("invalid verification_level byte must fail Borsh decode");
    let tx_err = err.unwrap();
    match tx_err {
        TransactionError::InstructionError(_, InstructionError::InvalidInstructionData) => {}
        other => panic!(
            "expected InvalidInstructionData on partial-verification byte, got {other:?}"
        ),
    }
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_stale_solend_reserve_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_STALE_SOLEND,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Warp far enough that snapshot's last_update_slot (= 0) is
    // beyond max_reserve_staleness_slots = 16 from the current slot.
    ctx.warp_to_slot(100).expect("warp_to_slot");

    let mut cond = solend_apr_lt_condition(1_000);
    cond.max_reserve_staleness_slots = 16;
    let mut snap = fresh_solend_snapshot(0); // very old
    snap.last_update_slot = 0;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: cond,
            snapshot: snap,
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_STALE_SOLEND,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("stale Solend reserve must fail");
    assert_custom_err(err, AuthorityError::SolendReserveStale);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_unsupported_solend_formula_version_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_BAD_FORMULA,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let mut cond = solend_apr_lt_condition(1_000);
    cond.formula_version = SUPPORTED_SOLEND_FORMULA_VERSION + 1; // unsupported
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: cond,
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_BAD_FORMULA,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("unsupported formula_version must fail");
    assert_custom_err(err, AuthorityError::SolendFormulaVersionUnsupported);
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_invalid_condition_logic_byte_fails_borsh_decode() {
    // Build a structurally-valid ExecuteAction, then corrupt the
    // condition_logic byte at the appropriate offset to a value out
    // of range. Borsh-deserialization fails at the program level →
    // `ProgramError::InvalidInstructionData`. State must NOT mutate.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_BAD_LOGIC,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: solend_apr_lt_condition(1_000),
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };
    let ix_base = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_BAD_LOGIC,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    // The condition_logic byte sits at offset 67 inside ix.data
    // (1 enum tag + 66 bytes of fixed P2 fields). Pinned by the
    // unit test `execute_action_borsh_field_order_is_pinned_through_p3`.
    let mut data = ix_base.data.clone();
    assert_eq!(
        data[67], 0,
        "condition_logic byte must be 0 (All) before tampering"
    );
    data[67] = 0xFF;
    let tampered = Instruction {
        program_id: ix_base.program_id,
        accounts: ix_base.accounts.clone(),
        data,
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[tampered],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("invalid condition_logic byte must fail Borsh decode");
    let tx_err = err.unwrap();
    match tx_err {
        TransactionError::InstructionError(_, InstructionError::InvalidInstructionData) => {}
        other => panic!("expected InvalidInstructionData, got {other:?}"),
    }
    assert_authz_unmutated(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn p3_p2_revoked_authorization_still_fails_before_condition_verification() {
    // P2 boundary check (AuthorizationRevoked) must fire before P3's
    // condition gate. We supply a perfectly-valid passing proof so
    // the only reason for failure is the revoke flag.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_P3_REVOKED_VS_GATE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Revoke first.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let revoke_ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_P3_REVOKED_VS_GATE,
    );
    let tx = Transaction::new_signed_with_payer(
        &[revoke_ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.unwrap();

    let clock = current_clock(&mut ctx).await;
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: solend_apr_lt_condition(1_000),
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P3_REVOKED_VS_GATE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        proof,
        None,
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("revoked authz must reject before condition gate");
    // Critically: the error MUST be the P2 AuthorizationRevoked code,
    // NOT a P3 condition error — the boundary order is preserved.
    assert_custom_err(err, AuthorityError::AuthorizationRevoked);

    // The PDA's revoked flag is true, but used_amount/completed/etc
    // remain at zero — the failed execute did not mutate the
    // execution-tracking fields.
    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(record.revoked, "revoke must have flipped the flag");
    assert_eq!(record.used_amount_raw, 0);
    assert!(!record.completed);
    assert_eq!(record.execution_nonce, 0);
    assert_eq!(record.last_execution_slot, 0);
}

// ── Stage 2 P4: Solend delegated withdraw boundary tests ─────────────────────
//
// These tests exercise the on-chain Solend boundary gate. Each negative
// test reloads the AuthorizationRecord after the failed tx and asserts
// no execution-tracking field was mutated.
//
// Tests opt in by:
//   1. using `setup_execute_authz_solend` (Solend action_type) and
//   2. calling `raw_execute_action_instruction` directly with an
//      explicit `Some(boundary_proof)`.
// The wrapper `execute_action_instruction` always passes `None`, so
// using the wrapper would fail with `SolendBoundaryProofMissing`
// before reaching any boundary semantics — this is by design.

const RULE_ID_P4_HAPPY: [u8; 16] = [0x55; 16];
const RULE_ID_P4_PROOF_MISSING: [u8; 16] = [0x56; 16];
const RULE_ID_P4_MAIN_WALLET: [u8; 16] = [0x57; 16];
const RULE_ID_P4_WRONG_PROGRAM: [u8; 16] = [0x58; 16];
const RULE_ID_P4_WRONG_RESERVE: [u8; 16] = [0x59; 16];
const RULE_ID_P4_WRONG_LM: [u8; 16] = [0x5A; 16];
const RULE_ID_P4_WRONG_DEST: [u8; 16] = [0x5B; 16];
const RULE_ID_P4_MISSING_REFRESH: [u8; 16] = [0x5C; 16];
const RULE_ID_P4_MISSING_WITHDRAW: [u8; 16] = [0x5D; 16];
const RULE_ID_P4_BAD_ORDER: [u8; 16] = [0x5E; 16];
const RULE_ID_P4_DUP_WITHDRAW: [u8; 16] = [0x5F; 16];
const RULE_ID_P4_CONFLICT_OTHER: [u8; 16] = [0x60; 16];
const RULE_ID_P4_P3_FALSE_VS_GATE: [u8; 16] = [0x61; 16];
const RULE_ID_P4_P3_OK_BOUNDARY_BAD: [u8; 16] = [0x62; 16];
const RULE_ID_P4_REVOKE_GREEN: [u8; 16] = [0x63; 16];
const RULE_ID_P4_REPLAY_GREEN: [u8; 16] = [0x64; 16];
const RULE_ID_P4_OBLIG_OWNER_BAD: [u8; 16] = [0x65; 16];

/// Reload the PDA after a failed tx and assert no execution-tracking
/// field was mutated. Required by the P4 prompt for every failure
/// path.
async fn assert_no_mutation_after_failure(
    ctx: &mut ProgramTestContext,
    pda: Pubkey,
) {
    let account = ctx
        .banks_client
        .get_account(pda)
        .await
        .expect("get_account")
        .expect("PDA must still exist (failed tx must NOT close the PDA)");
    let record = AuthorizationRecord::try_from_slice(&account.data)
        .expect("decode AuthorizationRecord");
    assert_eq!(
        record.used_amount_raw, 0,
        "used_amount_raw must NOT advance on a failed Solend boundary"
    );
    assert!(
        !record.completed,
        "completed must NOT flip on a failed Solend boundary"
    );
    assert_eq!(
        record.execution_nonce, 0,
        "execution_nonce must NOT advance on a failed Solend boundary"
    );
    assert_eq!(
        record.last_execution_slot, 0,
        "last_execution_slot must NOT advance on a failed Solend boundary"
    );
}

/// Submit a Solend `ExecuteAction` with the supplied (potentially
/// poisoned) boundary proof, expect failure with the given error,
/// and assert no PDA mutation. Used by every P4 negative test.
#[allow(clippy::too_many_arguments)]
async fn execute_solend_expect_failure(
    ctx: &mut ProgramTestContext,
    program_id: &Pubkey,
    fx: &ExecuteFixture,
    rule_id: [u8; 16],
    proof: SolendBoundaryProof,
    expected: AuthorityError,
    description: &'static str,
) {
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        rule_id,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        Some(proof),
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err(description);
    assert_custom_err(err, expected);
    assert_no_mutation_after_failure(ctx, fx.pda).await;
}

#[tokio::test]
async fn p4_valid_solend_boundary_succeeds_and_mutates_state() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_HAPPY,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let proof = default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_HAPPY);

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P4_HAPPY,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        Some(proof),
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("Solend happy path with valid boundary must succeed");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, 5_000_000);
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1);
    assert!(record.last_execution_slot > 0);
}

#[tokio::test]
async fn p4_solend_action_without_boundary_proof_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_PROOF_MISSING,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P4_PROOF_MISSING,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        None, // Solend action without boundary proof
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("Solend action without boundary proof must fail");
    assert_custom_err(err, AuthorityError::SolendBoundaryProofMissing);
    assert_no_mutation_after_failure(&mut ctx, fx.pda).await;
}

/// REQUIRED POISON TEST (P4 prompt): obligation fixture whose
/// authority == user main wallet. This must fail with the SPECIFIC
/// `SolendMainWalletObligationRejected` error before any mutation.
#[tokio::test]
async fn p4_main_wallet_obligation_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_MAIN_WALLET,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof = default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_MAIN_WALLET);
    // Poison: the obligation fixture claims the user's MAIN wallet
    // (`ctx.payer`) is the obligation authority. Stage 2 hard-rejects
    // this with a dedicated error code separate from the generic
    // mismatch path.
    proof.obligation.obligation_authority = ctx.payer.pubkey();

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_MAIN_WALLET,
        proof,
        AuthorityError::SolendMainWalletObligationRejected,
        "main-wallet obligation must be rejected with the dedicated error",
    )
    .await;
}

/// Sibling test for the poison test above — random non-delegated,
/// non-user wallet authority surfaces the *generic*
/// `SolendObligationAuthorityMismatch`. Together these prove the
/// main-wallet path has its own dedicated discriminant (P4 prompt:
/// "Do not satisfy this requirement with a random wrong-wallet test
/// only").
#[tokio::test]
async fn p4_random_wrong_wallet_obligation_authority_rejected() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_OBLIG_OWNER_BAD,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_P4_OBLIG_OWNER_BAD,
    );
    // Some unrelated third-party pubkey — not the user, not the
    // delegated wallet. Surfaces the GENERIC mismatch error.
    proof.obligation.obligation_authority = Pubkey::new_unique();

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_OBLIG_OWNER_BAD,
        proof,
        AuthorityError::SolendObligationAuthorityMismatch,
        "third-party-wallet obligation authority must surface generic mismatch",
    )
    .await;
}

#[tokio::test]
async fn p4_wrong_solend_program_id_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_WRONG_PROGRAM,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_WRONG_PROGRAM);
    proof.solend_program_id = Pubkey::new_unique(); // not Solend

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_WRONG_PROGRAM,
        proof,
        AuthorityError::SolendProgramMismatch,
        "wrong solend_program_id must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_wrong_reserve_in_withdraw_sibling_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_WRONG_RESERVE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_WRONG_RESERVE);
    // The withdraw sibling targets our obligation but a DIFFERENT
    // reserve — exactly the audit case "withdraw with right
    // obligation but wrong reserve".
    proof.sibling_instructions[1].target_reserve = Some(Pubkey::new_unique());

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_WRONG_RESERVE,
        proof,
        AuthorityError::SolendReserveMismatch,
        "withdraw with right obligation but wrong reserve must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_wrong_lending_market_in_withdraw_sibling_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_WRONG_LM,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_WRONG_LM);
    proof.sibling_instructions[1].target_lending_market = Some(Pubkey::new_unique());

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_WRONG_LM,
        proof,
        AuthorityError::SolendLendingMarketMismatch,
        "withdraw with wrong lending_market must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_wrong_destination_against_authz_record_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_WRONG_DEST,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_WRONG_DEST);
    proof.destination_pubkey = Pubkey::new_unique(); // not record.destination

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_WRONG_DEST,
        proof,
        AuthorityError::SolendDestinationMismatch,
        "destination not matching record.destination must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_missing_refresh_reserve_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_MISSING_REFRESH,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_MISSING_REFRESH);
    proof.sibling_instructions.remove(0); // drop the refresh

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_MISSING_REFRESH,
        proof,
        AuthorityError::SolendRefreshMissing,
        "missing RefreshReserve must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_missing_withdraw_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_MISSING_WITHDRAW,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_MISSING_WITHDRAW);
    proof.sibling_instructions.pop(); // drop the withdraw

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_MISSING_WITHDRAW,
        proof,
        AuthorityError::SolendWithdrawMissing,
        "missing Withdraw must fail",
    )
    .await;
}

#[tokio::test]
async fn p4_wrong_instruction_order_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_BAD_ORDER,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_BAD_ORDER);
    proof.sibling_instructions.swap(0, 1); // Withdraw before Refresh

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_BAD_ORDER,
        proof,
        AuthorityError::SolendInstructionOrderInvalid,
        "withdraw before refresh must fail",
    )
    .await;
}

/// "valid refresh + malicious withdraw + valid withdraw" — required
/// sibling-spoof scenario from the P4 prompt. The duplicate withdraw
/// targeting our obligation is detected as a conflict.
#[tokio::test]
async fn p4_duplicate_withdraw_for_same_obligation_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_DUP_WITHDRAW,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_DUP_WITHDRAW);
    let valid_withdraw = proof.sibling_instructions[1].clone();
    // valid refresh (idx 0) + duplicate withdraw (inserted at idx 1) +
    // valid withdraw (now at idx 2). All target our obligation.
    proof
        .sibling_instructions
        .insert(1, valid_withdraw);

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_DUP_WITHDRAW,
        proof,
        AuthorityError::SolendDuplicateOrConflictingInstruction,
        "duplicate withdraw for same obligation must fail",
    )
    .await;
}

/// "extra conflicting Solend instruction for the same target" —
/// required scenario from the P4 prompt. Defends against e.g. an
/// inserted `BorrowObligationLiquidity` siphoning collateral.
#[tokio::test]
async fn p4_other_solend_ix_touching_same_obligation_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_CONFLICT_OTHER,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof =
        default_solend_boundary_proof(&fx.delegated_wallet, &RULE_ID_P4_CONFLICT_OTHER);
    proof.sibling_instructions.push(SiblingIxDescriptor {
        program_id: SOLEND_PROGRAM_ID_MAINNET,
        // BorrowObligationLiquidity = variant 10. Targeting our
        // obligation means an attacker is trying to sneak in a borrow
        // through the same delegated obligation in the same tx.
        variant_byte: 10,
        target_reserve: None,
        target_obligation: Some(proof.obligation_pubkey),
        target_lending_market: None,
        target_destination: None,
    });

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_CONFLICT_OTHER,
        proof,
        AuthorityError::SolendDuplicateOrConflictingInstruction,
        "other Solend ix touching our obligation must fail",
    )
    .await;
}

/// P3 condition false must fail BEFORE the P4 boundary check (the
/// gate ordering is condition → boundary → mutation). The boundary
/// proof here is structurally valid; the failure is a P3 condition
/// mismatch and that's the error we expect.
#[tokio::test]
async fn p4_p3_condition_false_fails_before_p4_boundary() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_P3_FALSE_VS_GATE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Build a P3 condition payload with a Lt-1bps threshold — under
    // ~100 bps APR fixture this fold returns false.
    let condition_proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: SolendSupplyAprCondition {
                comparison: Comparison::Lt,
                threshold_bps: 1,
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
    };
    let boundary_proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_P4_P3_FALSE_VS_GATE,
    );

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P4_P3_FALSE_VS_GATE,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        condition_proof,
        Some(boundary_proof),
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("P3 false condition must fail before P4 boundary");
    // Critically: ConditionNotMet (P3) — NOT any P4 boundary error.
    assert_custom_err(err, AuthorityError::ConditionNotMet);
    assert_no_mutation_after_failure(&mut ctx, fx.pda).await;
}

/// P3 condition true but P4 boundary false must fail with a P4
/// boundary error (the gate ordering ensures the boundary is reached
/// only when conditions pass).
#[tokio::test]
async fn p4_p3_true_but_boundary_bad_fails_with_boundary_error() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_P3_OK_BOUNDARY_BAD,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // P3: passing condition (default Solend Lt 1000bps with low-util
    // fixture). P4: boundary poisoned with main-wallet authority.
    let mut boundary_proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_P4_P3_OK_BOUNDARY_BAD,
    );
    boundary_proof.obligation.obligation_authority = ctx.payer.pubkey();

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_P3_OK_BOUNDARY_BAD,
        boundary_proof,
        AuthorityError::SolendMainWalletObligationRejected,
        "P3 ok + P4 main-wallet poison must surface the P4 main-wallet error",
    )
    .await;
}

/// Revoke remains green for Solend authz: revoke flips the flag,
/// subsequent execute fails with `AuthorizationRevoked` BEFORE
/// reaching the P4 boundary.
#[tokio::test]
async fn p4_revoke_still_green_for_solend_authz() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_REVOKE_GREEN,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Revoke first.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let revoke_ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_P4_REVOKE_GREEN,
    );
    let tx = Transaction::new_signed_with_payer(
        &[revoke_ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.unwrap();

    // Now try execute with a perfectly-valid Solend boundary proof —
    // AuthorizationRevoked must fire BEFORE the P4 boundary check.
    let proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_P4_REVOKE_GREEN,
    );

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        RULE_ID_P4_REVOKE_GREEN,
        proof,
        AuthorityError::AuthorizationRevoked,
        "revoked Solend authz must reject before P4 boundary",
    )
    .await;
}

/// Replay/completed remains green for Solend: a successful first
/// execute flips `completed=true`, and a second execute (even with a
/// perfectly-valid boundary proof) fails with
/// `AuthorizationCompleted` BEFORE reaching the P4 boundary.
#[tokio::test]
async fn p4_replay_after_completed_still_green_for_solend() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_P4_REPLAY_GREEN,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_P4_REPLAY_GREEN,
    );

    // First execute — happy path, mutates state.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix1 = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P4_REPLAY_GREEN,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        Some(proof.clone()),
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.unwrap();

    // Warp + refresh blockhash so the second tx has a different sig
    // and isn't blocked by SameSlotReplay.
    ctx.warp_to_slot(50).expect("warp_to_slot");
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix2 = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_P4_REPLAY_GREEN,
        fx.canonical_rule_hash,
        fx.action_type,
        1, // tiny non-zero amount
        2,
        default_solend_proof(),
        Some(proof),
        None, // jupiter_boundary_proof (J4)
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("second execute on completed authz must fail");
    assert_custom_err(err, AuthorityError::AuthorizationCompleted);

    // Reload and assert the FIRST execute's mutations stuck and the
    // SECOND execute did NOT advance the nonce/slot.
    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, 5_000_000, "first execute mutation persists");
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1, "second execute did NOT advance nonce");
}

/// Ensure the `MAX_SIBLING_INSTRUCTIONS` cap is enforced on-chain
/// too — the unit tests cover the boundary verifier directly; this
/// test confirms the cap holds across the full process_execute_action
/// path with no mutation.
#[tokio::test]
async fn p4_oversized_sibling_list_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let rule_id: [u8; 16] = [0x66; 16];
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &rule_id,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let mut proof = default_solend_boundary_proof(&fx.delegated_wallet, &rule_id);
    let unrelated = SiblingIxDescriptor {
        program_id: Pubkey::new_unique(),
        variant_byte: 0,
        target_reserve: None,
        target_obligation: None,
        target_lending_market: None,
        target_destination: None,
    };
    for _ in 0..MAX_SIBLING_INSTRUCTIONS {
        proof.sibling_instructions.push(unrelated.clone());
    }
    assert!(proof.sibling_instructions.len() > MAX_SIBLING_INSTRUCTIONS);

    execute_solend_expect_failure(
        &mut ctx,
        &program_id,
        &fx,
        rule_id,
        proof,
        AuthorityError::SolendBoundaryVerificationFailed,
        "oversized sibling list must fail",
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────
// Stage 2 J4 — Jupiter bracket sibling-instruction verifier tests
// ─────────────────────────────────────────────────────────────────────
//
// Coverage required by J4:
//   - Jupiter action with valid skeleton proof succeeds and mutates
//     state (sibling-verifier-only success — explicitly NOT a full
//     trustless balance bracket).
//   - Jupiter action with no boundary proof fails before mutation.
//   - P3 condition false fails before reaching the J4 boundary.
//   - Illegal sibling program rejected before mutation.
//   - Fake Jupiter program id rejected before mutation.
//   - Unexpected writable destination toucher rejected before mutation.
//   - Malicious SPL Token Approve in middle band rejected before mutation.
//   - Wrong relative ordering (duplicate swap) rejected before mutation.
//   - Wrong destination_token_account rejected before mutation.
//   - Oversized sibling list rejected before mutation.
//   - Solend action does NOT require a Jupiter proof (Solend boundary
//     stays the only relevant gate for SolendWithdrawAllDelegated).
//   - Revoked authorization still fails before reaching the J4 boundary.
//   - Already-completed (replay) still fails before reaching the J4 boundary.
//
// Every negative test asserts AuthorizationRecord is unmutated using
// the existing `assert_no_mutation_after_failure` helper (re-used from
// the P4 block).

const RULE_ID_J4_HAPPY: [u8; 16] = [0xF8; 16];
const RULE_ID_J4_PROOF_MISSING: [u8; 16] = [0xF9; 16];
const RULE_ID_J4_P3_FALSE: [u8; 16] = [0xFA; 16];
const RULE_ID_J4_ILLEGAL_SIBLING: [u8; 16] = [0xFB; 16];
const RULE_ID_J4_FAKE_PROGRAM: [u8; 16] = [0xFC; 16];
const RULE_ID_J4_UNEXPECTED_WRITABLE: [u8; 16] = [0xFD; 16];
const RULE_ID_J4_MAL_TOKEN: [u8; 16] = [0xFE; 16];
const RULE_ID_J4_DUP_SWAP: [u8; 16] = [0xFF; 16];
const RULE_ID_J4_WRONG_DEST: [u8; 16] = [0xB1; 16];
const RULE_ID_J4_OVERSIZED: [u8; 16] = [0xB2; 16];
const RULE_ID_J4_SOLEND_INDEPENDENT: [u8; 16] = [0xB3; 16];
const RULE_ID_J4_REVOKED: [u8; 16] = [0xB4; 16];
const RULE_ID_J4_COMPLETED: [u8; 16] = [0xB5; 16];

/// Helper: drives a Jupiter execute with a custom proof-mutator and
/// asserts the on-chain failure surfaces `expected` AND that the
/// AuthorizationRecord stays unmutated (used_amount_raw=0, completed=
/// false, execution_nonce=0, last_execution_slot=0). Mirrors the
/// P4-block `execute_solend_expect_failure` helper.
async fn execute_jupiter_expect_failure<F>(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    fx: &ExecuteFixture,
    rule_id: [u8; 16],
    proof_mutator: F,
    expected: AuthorityError,
    failure_label: &'static str,
) where
    F: FnOnce(&mut clawsol_authority::jupiter_boundary::JupiterBoundaryProof),
{
    let mut proof = default_jupiter_boundary_proof(&fx.delegated_wallet, &rule_id);
    proof_mutator(&mut proof);
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        rule_id,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        1,
        default_solend_proof(),
        None,
        Some(proof),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err(failure_label);
    assert_custom_err(err, expected);
    assert_no_mutation_after_failure(ctx, fx.pda).await;
}

#[tokio::test]
async fn j4_valid_jupiter_boundary_skeleton_succeeds_and_mutates_state() {
    // **Note:** this is the sibling-verifier-only success test for J4.
    // J4 does NOT implement the trustless balance bracket; the
    // `min_output_amount_raw` field on the proof is intentionally not
    // enforced here. Renamed accordingly so the deferral is visible.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_HAPPY,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_HAPPY,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        None,
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_J4_HAPPY,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("J4 sibling-verifier happy path must succeed");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert_eq!(record.used_amount_raw, 5_000_000);
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1);
    assert!(record.last_execution_slot > 0);
}

#[tokio::test]
async fn j4_jupiter_action_without_boundary_proof_fails_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_PROOF_MISSING,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_PROOF_MISSING,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        1,
        default_solend_proof(),
        None,
        None, // J4: missing Jupiter proof on a Jupiter action
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("missing Jupiter proof must fail");
    assert_custom_err(err, AuthorityError::JupiterBoundaryProofMissing);
    assert_no_mutation_after_failure(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn j4_p3_condition_false_fails_before_jupiter_boundary() {
    // Verifies the P3 → J4 ordering: a false P3 condition fails with
    // ConditionNotMet, NOT JupiterBoundaryProofMissing. The Jupiter
    // gate runs strictly after P3.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_P3_FALSE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let clock = current_clock(&mut ctx).await;
    // A condition that returns false (Lt 1 bps when APR is ~100 bps).
    let proof = ConditionProofPayload {
        condition_logic: ConditionLogic::All,
        conditions: vec![ProofCondition::Solend {
            condition: solend_apr_lt_condition(1),
            snapshot: fresh_solend_snapshot(clock.slot),
        }],
    };

    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_P3_FALSE,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        1,
        proof,
        None,
        // Even if a valid Jupiter proof is supplied, P3 fails first.
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_J4_P3_FALSE,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("P3 false must fail before J4 gate");
    assert_custom_err(err, AuthorityError::ConditionNotMet);
    assert_no_mutation_after_failure(&mut ctx, fx.pda).await;
}

#[tokio::test]
async fn j4_illegal_sibling_program_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_ILLEGAL_SIBLING,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_ILLEGAL_SIBLING,
        |proof| {
            // Insert an unknown program id at the head of the middle band.
            proof.sibling_instructions.insert(
                0,
                clawsol_authority::jupiter_boundary::JupiterSiblingIxDescriptor {
                    program_id: Pubkey::new_from_array([0x99; 32]),
                    variant_byte: 0,
                    writable_accounts: vec![],
                },
            );
        },
        AuthorityError::JupiterIllegalSiblingInstruction,
        "unknown program in middle band must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_fake_jupiter_program_id_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_FAKE_PROGRAM,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_FAKE_PROGRAM,
        |proof| {
            // Replace the proof's pinned `jupiter_program_id` with a
            // look-alike pubkey. The Gate-2 program-id check fires.
            proof.jupiter_program_id = Pubkey::new_from_array([0xCA; 32]);
        },
        AuthorityError::JupiterProgramIdMismatch,
        "fake Jupiter program id must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_unexpected_writable_destination_toucher_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_UNEXPECTED_WRITABLE,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_UNEXPECTED_WRITABLE,
        |proof| {
            // Inject an ATA-class ix that writes an unrecognised pubkey.
            proof.sibling_instructions.insert(
                1,
                clawsol_authority::jupiter_boundary::JupiterSiblingIxDescriptor {
                    program_id: clawsol_authority::jupiter_boundary::ATA_PROGRAM_ID,
                    variant_byte: 1,
                    writable_accounts: vec![Pubkey::new_from_array([0xEA; 32])],
                },
            );
        },
        AuthorityError::JupiterUnexpectedWritableAccount,
        "unexpected writable account must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_malicious_token_transfer_between_brackets_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_MAL_TOKEN,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_MAL_TOKEN,
        |proof| {
            // Insert an SPL Token Approve (variant 4) — a delegation
            // attack — between Compute Budget and the swap. Variant
            // gate catches it even though the writable account is
            // legitimate.
            proof.sibling_instructions.insert(
                1,
                clawsol_authority::jupiter_boundary::JupiterSiblingIxDescriptor {
                    program_id: clawsol_authority::jupiter_boundary::SPL_TOKEN_PROGRAM_ID,
                    variant_byte: 4, // Approve
                    writable_accounts: vec![proof.delegated_input_token_account],
                },
            );
        },
        AuthorityError::JupiterIllegalSiblingInstruction,
        "Approve in middle band must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_wrong_relative_order_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_DUP_SWAP,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_DUP_SWAP,
        |proof| {
            // Duplicate the Jupiter swap descriptor — second swap fails
            // the relative-ordering state machine.
            let swap = proof
                .sibling_instructions
                .iter()
                .find(|ix| {
                    ix.program_id
                        == clawsol_authority::jupiter_boundary::JUPITER_V6_PROGRAM_ID_MAINNET
                })
                .unwrap()
                .clone();
            let swap_idx = proof
                .sibling_instructions
                .iter()
                .position(|ix| {
                    ix.program_id
                        == clawsol_authority::jupiter_boundary::JUPITER_V6_PROGRAM_ID_MAINNET
                })
                .unwrap();
            proof.sibling_instructions.insert(swap_idx + 1, swap);
        },
        AuthorityError::JupiterInstructionOrderInvalid,
        "duplicate swap must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_wrong_destination_account_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_WRONG_DEST,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_WRONG_DEST,
        |proof| {
            // The proof's expected_destination_token_account points
            // somewhere other than AuthorizationRecord.destination.
            proof.expected_destination_token_account =
                Pubkey::new_from_array([0xCD; 32]);
        },
        AuthorityError::JupiterDestinationMismatch,
        "wrong destination must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_oversized_sibling_list_rejected_before_mutation() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_OVERSIZED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_OVERSIZED,
        |proof| {
            for _ in 0..clawsol_authority::jupiter_boundary::MAX_JUPITER_SIBLING_INSTRUCTIONS
            {
                proof.sibling_instructions.push(
                    clawsol_authority::jupiter_boundary::JupiterSiblingIxDescriptor {
                        program_id:
                            clawsol_authority::jupiter_boundary::COMPUTE_BUDGET_PROGRAM_ID,
                        variant_byte: 0,
                        writable_accounts: vec![],
                    },
                );
            }
        },
        AuthorityError::JupiterSiblingListTooLarge,
        "oversized sibling list must fail",
    )
    .await;
}

#[tokio::test]
async fn j4_solend_action_does_not_require_jupiter_proof() {
    // A Solend authz with a valid Solend boundary proof and `None` for
    // `jupiter_boundary_proof` succeeds. The Jupiter boundary gate
    // is scoped strictly to JupiterBuySolWithUsdc actions; Solend
    // executions never enter it.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz_solend(
        &mut ctx,
        program_id,
        &RULE_ID_J4_SOLEND_INDEPENDENT,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    let proof = default_solend_boundary_proof(
        &fx.delegated_wallet,
        &RULE_ID_J4_SOLEND_INDEPENDENT,
    );
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_SOLEND_INDEPENDENT,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        Some(proof),
        None, // Solend action: jupiter_boundary_proof MUST be None
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("Solend execute must not require a Jupiter proof");

    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(record.completed);
}

#[tokio::test]
async fn j4_revoked_authorization_still_fails_before_jupiter_boundary() {
    // P2 step 11 (revoked) fires before the J4 boundary; the failure
    // surfaces AuthorizationRevoked, NOT a Jupiter-class error.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_REVOKED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // Revoke the authorization first.
    let revoke_ix = revoke_authorization_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        RULE_ID_J4_REVOKED,
    );
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[revoke_ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("revoke");

    execute_jupiter_expect_failure(
        &mut ctx,
        program_id,
        &fx,
        RULE_ID_J4_REVOKED,
        |_proof| { /* unmodified valid proof */ },
        AuthorityError::AuthorizationRevoked,
        "revoked authz must fail before J4 gate",
    )
    .await;
}

#[tokio::test]
async fn j4_completed_replay_still_fails_before_jupiter_boundary() {
    // P2 step 12 (completed) fires before the J4 boundary; the
    // second-execute failure surfaces AuthorizationCompleted, NOT a
    // Jupiter-class error.
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;
    let fx = setup_execute_authz(
        &mut ctx,
        program_id,
        &RULE_ID_J4_COMPLETED,
        5_000_000,
        1_000_000,
    )
    .await;
    fund_keypair(&mut ctx, &fx.executor.pubkey(), 1_000_000_000).await;

    // First execute succeeds.
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix1 = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_COMPLETED,
        fx.canonical_rule_hash,
        fx.action_type,
        5_000_000,
        1,
        default_solend_proof(),
        None,
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_J4_COMPLETED,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx)
        .await
        .expect("first execute");

    // Second execute on the completed authz fails BEFORE the J4 gate.
    ctx.warp_to_slot(50).expect("warp_to_slot");
    ctx.last_blockhash = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let ix2 = raw_execute_action_instruction(
        &program_id,
        &fx.executor.pubkey(),
        &ctx.payer.pubkey(),
        &fx.delegated_wallet,
        SCHEMA_VERSION,
        RULE_ID_J4_COMPLETED,
        fx.canonical_rule_hash,
        fx.action_type,
        1,
        2,
        default_solend_proof(),
        None,
        Some(default_jupiter_boundary_proof(
            &fx.delegated_wallet,
            &RULE_ID_J4_COMPLETED,
        )),
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&fx.executor.pubkey()),
        &[&fx.executor],
        ctx.last_blockhash,
    );
    let err = ctx
        .banks_client
        .process_transaction(tx)
        .await
        .expect_err("second execute on completed authz must fail");
    assert_custom_err(err, AuthorityError::AuthorizationCompleted);

    // FIRST execute's mutations stuck; SECOND did not advance.
    let acc = ctx.banks_client.get_account(fx.pda).await.unwrap().unwrap();
    let record = AuthorizationRecord::try_from_slice(&acc.data).unwrap();
    assert!(record.completed);
    assert_eq!(record.execution_nonce, 1);
}

// ── Stage 2 P5a: safety grep ────────────────────────────────────────────────
//
// Compile-time scan of the P5a-touched source files for forbidden CPI /
// transaction / signing / broadcast symbols. The Solend live-account
// decode substrate (`solend_account_decode.rs`) MUST NOT contain any
// executable CPI path. Solend boundary additions (`solend_boundary.rs`
// AccountInfo wrappers) MUST NOT add `invoke` / `invoke_signed` calls.
//
// Comments may reference future P5b CPI by name (e.g. "P5b will land
// same-tx Refresh + Withdraw"); the grep targets *call patterns*
// (function name immediately followed by `(`) so plain prose mentions
// don't trip it.

#[test]
fn safety_grep_no_cpi_symbols_in_p5a_decode_module() {
    let source = include_str!("../src/solend_account_decode.rs");
    for forbidden in &[
        "invoke(",
        "invoke_signed(",
        "send_transaction(",
        "sendRawTransaction(",
        "signTransaction(",
        "broadcast(",
        "RpcClient::",
        "reqwest::",
    ] {
        assert!(
            !source.contains(forbidden),
            "P5a decode module must not contain CPI / RPC / signing symbol `{forbidden}`",
        );
    }
}

#[test]
fn safety_grep_no_new_cpi_symbols_in_solend_boundary() {
    // The Solend boundary file has existed since P4. P5a only ADDS
    // AccountInfo wrappers (no CPI). The forbidden-call-pattern grep
    // here scans the entire current file because P5a's change set
    // adds no new executable CPI paths anywhere in it.
    //
    // Pre-existing P1 `invoke_signed(` exists ONLY in lib.rs (the
    // CreateAuthorization processor); this test deliberately does not
    // scan lib.rs.
    let source = include_str!("../src/solend_boundary.rs");
    for forbidden in &[
        "invoke(",
        "invoke_signed(",
        "send_transaction(",
        "sendRawTransaction(",
        "signTransaction(",
        "broadcast(",
        "RpcClient::",
        "reqwest::",
    ] {
        assert!(
            !source.contains(forbidden),
            "Solend boundary must not contain CPI / RPC / signing symbol `{forbidden}`",
        );
    }
}
