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
    derive_authorization_pda,
    error::AuthorityError,
    instruction::{
        close_authorization_instruction, create_authorization_instruction,
        execute_action_instruction, revoke_authorization_instruction, AuthorityInstruction,
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
struct ExecuteFixture {
    pda: Pubkey,
    executor: Keypair,
    delegated_wallet: Pubkey,
    canonical_rule_hash: [u8; 32],
    action_type: u8,
    max_input_amount_raw: u64,
    expires_at_slot: u64,
}

async fn setup_execute_authz(
    ctx: &mut ProgramTestContext,
    program_id: Pubkey,
    rule_id: &[u8; 16],
    max_input_amount_raw: u64,
    expires_at_slot: u64,
) -> ExecuteFixture {
    let executor = Keypair::new();
    let delegated_wallet = Pubkey::new_unique();
    let canonical_rule_hash = dummy_canonical_hash(rule_id);
    let action_type = Stage2ActionType::SolendWithdrawAllDelegated.to_u8();
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
        .expect("setup create_authorization for execute");

    ExecuteFixture {
        pda,
        executor,
        delegated_wallet,
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
    let data = borsh::to_vec(&AuthorityInstruction::ExecuteAction {
        schema_version: SCHEMA_VERSION,
        rule_id: RULE_ID_EXEC_NO_SIG,
        canonical_rule_hash: fx.canonical_rule_hash,
        action_type: fx.action_type,
        input_amount_raw: 5_000_000,
        execution_nonce: 1,
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
    // Authz was created with SolendWithdrawAllDelegated. Try to execute
    // as JupiterBuySolWithUsdc.
    let wrong_action = Stage2ActionType::JupiterBuySolWithUsdc.to_u8();
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
