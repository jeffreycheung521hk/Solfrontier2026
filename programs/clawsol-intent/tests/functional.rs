//! End-to-end behavioral tests for the clawsol-intent program, run inside
//! the in-process `solana-program-test` BanksClient harness.
//!
//! Fixtures here are intentionally local placeholders. Agent B's prompt 2
//! will land the canonical bytes/hash spec; this file should be updated
//! to consume those final fixtures rather than the placeholder bytes used
//! below.

use borsh::BorshDeserialize;
use clawsol_intent::{
    derive_intent_pda,
    error::IntentError,
    instruction::record_intent_instruction,
    state::{ActionType, IntentRecord},
};
use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::{
    hash::hashv,
    instruction::InstructionError,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    transaction::{Transaction, TransactionError},
};

const SCHEMA_VERSION: u8 = 1;

/// Local placeholder for canonical intent bytes — replaced when Agent B's
/// prompt-2 fixtures land.
fn placeholder_canonical_bytes() -> Vec<u8> {
    b"clawsol-intent-test-placeholder-v0".to_vec()
}

fn placeholder_hash() -> [u8; 32] {
    hashv(&[&placeholder_canonical_bytes()]).to_bytes()
}

fn build_program_test(program_id: Pubkey) -> ProgramTest {
    ProgramTest::new(
        "clawsol_intent",
        program_id,
        processor!(clawsol_intent::process_instruction),
    )
}

async fn current_slot(ctx: &mut ProgramTestContext) -> u64 {
    ctx.banks_client
        .get_root_slot()
        .await
        .expect("get_root_slot")
}

#[tokio::test]
async fn happy_path_records_pda_and_event_fields() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let intent_id = [0xA1u8; 16];
    let bytes = placeholder_canonical_bytes();
    let hash = placeholder_hash();
    let action_type = ActionType::SolendDeposit.to_u8();
    let expires_at_slot = 1_000_000u64;

    let (pda, expected_bump) =
        derive_intent_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &intent_id);

    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        hash,
        action_type,
        expires_at_slot,
        bytes,
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
        .expect("PDA must exist after record_intent");

    assert_eq!(account.owner, program_id, "PDA owner must be the program");
    assert_eq!(
        account.data.len(),
        IntentRecord::LEN,
        "PDA data length must match IntentRecord::LEN"
    );

    let record = IntentRecord::try_from_slice(&account.data).expect("decode IntentRecord");
    assert_eq!(record.schema_version, SCHEMA_VERSION);
    assert_eq!(record.intent_id, intent_id);
    assert_eq!(record.user, ctx.payer.pubkey());
    assert_eq!(record.canonical_intent_hash, hash);
    assert_eq!(
        record.action_type, action_type,
        "action_type must round-trip"
    );
    assert_eq!(record.expires_at_slot, expires_at_slot);
    assert_eq!(record.bump, expected_bump);
    // created_at_slot is sourced from on-chain Clock; it must be a real,
    // bounded value, not the dummy default.
    assert!(record.created_at_slot >= 1, "created_at_slot from Clock");
    assert!(record.created_at_slot < expires_at_slot);
}

#[tokio::test]
async fn wrong_hash_rejected() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let intent_id = [0xA2u8; 16];
    let bytes = placeholder_canonical_bytes();
    // deliberately wrong hash
    let bad_hash = [0xFFu8; 32];

    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        bad_hash,
        ActionType::SolendDeposit.to_u8(),
        1_000_000,
        bytes,
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
        .expect_err("wrong-hash tx must fail");
    assert_custom_err(err, IntentError::HashMismatch);
}

#[tokio::test]
async fn expired_intent_rejected() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    // Warp to slot 100 so we can set expires_at_slot <= current.
    ctx.warp_to_slot(100).expect("warp_to_slot");

    let intent_id = [0xA3u8; 16];
    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        placeholder_hash(),
        ActionType::SolendDeposit.to_u8(),
        // current slot is >= 100; expires_at_slot = 50 makes the intent expired
        50,
        placeholder_canonical_bytes(),
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
        .expect_err("expired-intent tx must fail");
    assert_custom_err(err, IntentError::IntentExpired);
}

#[tokio::test]
async fn duplicate_intent_rejected() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    let intent_id = [0xA4u8; 16];
    let bytes = placeholder_canonical_bytes();
    let hash = placeholder_hash();

    let ix1 = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        hash,
        ActionType::SolendDeposit.to_u8(),
        1_000_000,
        bytes.clone(),
    );
    let tx1 = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client
        .process_transaction(tx1)
        .await
        .expect("first record_intent must succeed");

    // Refresh blockhash so the second tx isn't deduped on signature.
    ctx.last_blockhash = ctx
        .banks_client
        .get_latest_blockhash()
        .await
        .expect("latest blockhash");

    let ix2 = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        hash,
        ActionType::SolendDeposit.to_u8(),
        1_000_000,
        bytes,
    );
    let tx2 = Transaction::new_signed_with_payer(
        &[ix2],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );

    let err = ctx
        .banks_client
        .process_transaction(tx2)
        .await
        .expect_err("duplicate record_intent must fail");
    // The system program returns AccountAlreadyInUse when create_account
    // hits an already-funded address. We just need to assert the second tx
    // fails — the exact InstructionError variant comes from the system
    // program, not our program.
    let TransactionError::InstructionError(_, _) = err.unwrap() else {
        panic!("expected InstructionError on duplicate record_intent");
    };
}

#[tokio::test]
async fn user_must_sign() {
    // Spin up the test with a separate fee payer, then issue an instruction
    // whose `user` AccountMeta has is_signer=false. The transaction itself
    // is still signed (by `payer`), so the runtime accepts it; the program
    // must reject it with UserMustSign.
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let unrelated_user = Keypair::new();
    let intent_id = [0xA5u8; 16];
    let (pda, _) = derive_intent_pda(
        &program_id,
        SCHEMA_VERSION,
        &unrelated_user.pubkey(),
        &intent_id,
    );

    use solana_sdk::instruction::{AccountMeta, Instruction};
    let data = borsh::to_vec(
        &clawsol_intent::instruction::IntentInstruction::RecordIntent {
            schema_version: SCHEMA_VERSION,
            intent_id,
            canonical_intent_hash: placeholder_hash(),
            action_type: ActionType::SolendDeposit.to_u8(),
            expires_at_slot: 1_000_000,
            canonical_intent_bytes: placeholder_canonical_bytes(),
        },
    )
    .unwrap();
    let ix = Instruction {
        program_id,
        accounts: vec![
            // user is NOT a signer at the top level of this transaction
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
    assert_custom_err(err, IntentError::UserMustSign);
}

#[tokio::test]
async fn invalid_action_type_rejected() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let intent_id = [0xA6u8; 16];
    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        placeholder_hash(),
        99, // not in {1,2,3}
        1_000_000,
        placeholder_canonical_bytes(),
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
        .expect_err("invalid action_type must fail");
    assert_custom_err(err, IntentError::InvalidActionType);
}

#[tokio::test]
async fn canonical_intent_bytes_too_large_rejected() {
    let program_id = Pubkey::new_unique();
    let ctx = build_program_test(program_id).start_with_context().await;

    let intent_id = [0xA7u8; 16];
    let oversized = vec![0u8; clawsol_intent::MAX_CANONICAL_INTENT_BYTES + 1];
    let hash = solana_sdk::hash::hashv(&[&oversized]).to_bytes();

    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        hash,
        ActionType::SolendDeposit.to_u8(),
        1_000_000,
        oversized,
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
        .expect_err("oversized canonical_intent_bytes must fail");
    assert_custom_err(err, IntentError::CanonicalIntentBytesTooLarge);
}

#[tokio::test]
async fn action_type_round_trips_for_all_known_discriminators() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    for (i, action) in [
        ActionType::SolendDeposit,
        ActionType::SolendWithdrawAll,
        ActionType::JupiterSwap,
    ]
    .iter()
    .enumerate()
    {
        // distinct intent_id per record so PDAs don't collide
        let mut intent_id = [0u8; 16];
        intent_id[0] = 0xB0 + i as u8;

        let bytes = placeholder_canonical_bytes();
        let hash = solana_sdk::hash::hashv(&[&bytes]).to_bytes();
        let (pda, _) =
            derive_intent_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &intent_id);

        let ix = record_intent_instruction(
            &program_id,
            &ctx.payer.pubkey(),
            SCHEMA_VERSION,
            intent_id,
            hash,
            action.to_u8(),
            1_000_000,
            bytes,
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
            .unwrap_or_else(|e| panic!("record_intent for action {} failed: {e}", action.to_u8()));

        let acc = ctx
            .banks_client
            .get_account(pda)
            .await
            .unwrap()
            .expect("PDA exists");
        let record = IntentRecord::try_from_slice(&acc.data).unwrap();
        assert_eq!(record.action_type, action.to_u8());
    }
}

#[tokio::test]
async fn created_at_slot_comes_from_clock() {
    let program_id = Pubkey::new_unique();
    let mut ctx = build_program_test(program_id).start_with_context().await;

    let target_slot = 75u64;
    ctx.warp_to_slot(target_slot).expect("warp_to_slot");

    let intent_id = [0xC0u8; 16];
    let bytes = placeholder_canonical_bytes();
    let hash = placeholder_hash();
    let (pda, _) =
        derive_intent_pda(&program_id, SCHEMA_VERSION, &ctx.payer.pubkey(), &intent_id);

    let ix = record_intent_instruction(
        &program_id,
        &ctx.payer.pubkey(),
        SCHEMA_VERSION,
        intent_id,
        hash,
        ActionType::SolendDeposit.to_u8(),
        target_slot + 10_000,
        bytes,
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&ctx.payer.pubkey()),
        &[&ctx.payer],
        ctx.last_blockhash,
    );
    ctx.banks_client.process_transaction(tx).await.unwrap();

    let acc = ctx
        .banks_client
        .get_account(pda)
        .await
        .unwrap()
        .expect("PDA exists");
    let record = IntentRecord::try_from_slice(&acc.data).unwrap();
    // The processor reads Clock::get(), which after warp_to_slot reflects
    // the warped slot. created_at_slot must be at least the warped target.
    assert!(
        record.created_at_slot >= target_slot,
        "created_at_slot ({}) must come from Clock and reflect warped slot ({})",
        record.created_at_slot,
        target_slot
    );

    let _ = current_slot; // silence unused if compiler ever flags it
}

fn assert_custom_err(err: solana_program_test::BanksClientError, expected: IntentError) {
    let tx_err = err.unwrap();
    match tx_err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(
                code, expected as u32,
                "expected IntentError::{:?} (code {}), got code {}",
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
