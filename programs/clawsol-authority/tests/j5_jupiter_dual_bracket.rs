//! Stage 2 J5 BONUS PROTOTYPE — Jupiter trustless dual-bracket tests.
//!
//! # Scope
//!
//! Drives the J5 `PreJupiterBracketCheck` / `PostJupiterBracketCheck`
//! pair end-to-end under `solana-program-test` against a fresh
//! `clawsol_authority` program instance. Mock SPL Token accounts at the
//! destination address are seeded with raw 165-byte Pack bytes; the
//! tests mutate the destination's `amount` between pre and post to
//! drive each gate.
//!
//! # NOT live-proven
//!
//! - No Jupiter API call.
//! - No Jupiter swap.
//! - No mainnet broadcast.
//! - No live SPL Token CPI.
//!
//! Mock-only — proves the substrate, not a live trustless bracket.

use borsh::BorshDeserialize;
use clawsol_authority::{
    derive_authorization_pda,
    error::AuthorityError,
    instruction::{
        create_authorization_instruction, post_jupiter_bracket_check_instruction,
        pre_jupiter_bracket_check_instruction,
    },
    jupiter_bracket::{
        derive_jupiter_bracket_checkpoint_pda, JupiterBracketCheckpoint,
        JupiterPostBracketProof, JupiterPreBracketProof, JUPITER_BRACKET_SCHEMA_VERSION,
    },
    solend_account_decode::{
        SPL_TOKEN_ACCOUNT_LEN, SPL_TOKEN_OFFSET_AMOUNT, SPL_TOKEN_OFFSET_MINT,
        SPL_TOKEN_OFFSET_OWNER, SPL_TOKEN_PROGRAM_ID,
    },
    state::{Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION},
};
use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::{
    account::Account,
    instruction::InstructionError,
    pubkey::Pubkey,
    rent::Rent,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, TransactionError},
};

const SCHEMA_VERSION: u8 = STAGE2_AUTHORITY_SCHEMA_VERSION;

// Distinct rule_id first byte per test so PDAs don't collide.
const RULE_ID_PRE_RECORDS_LIVE_PRE: [u8; 16] = [0x4A; 16];
const RULE_ID_POST_EQUAL_MIN: [u8; 16] = [0x4B; 16];
const RULE_ID_POST_GREATER_MIN: [u8; 16] = [0x4C; 16];
const RULE_ID_POST_BELOW_MIN: [u8; 16] = [0x4D; 16];
const RULE_ID_POST_UNDERFLOW: [u8; 16] = [0x4E; 16];
const RULE_ID_POST_MISSING_CP: [u8; 16] = [0x4F; 16];
const RULE_ID_POST_REPLAY: [u8; 16] = [0x50; 16];
const RULE_ID_PRE_WRONG_MINT: [u8; 16] = [0x51; 16];
const RULE_ID_POST_WRONG_DEST: [u8; 16] = [0x52; 16];
const RULE_ID_TOKEN_LEN_REJECT: [u8; 16] = [0x53; 16];

// ── Pack-style mock SPL Token byte builder ─────────────────────────────────

fn make_spl_token_bytes(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
    data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&mint.to_bytes());
    data[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32]
        .copy_from_slice(&owner.to_bytes());
    data[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
        .copy_from_slice(&amount.to_le_bytes());
    // AccountState::Initialized at offset 108.
    data[108] = 1;
    data
}

// ── Fixture ─────────────────────────────────────────────────────────────────

struct J5Fixture {
    context: ProgramTestContext,
    program_id: Pubkey,
    /// Diagnostic-only — kept to make per-test failures self-describing.
    #[allow(dead_code)]
    rule_id: [u8; 16],
    pda: Pubkey,
    executor: Keypair,
    /// Diagnostic-only — Stage 2 model has the delegated wallet sign
    /// the Jupiter swap (out of J5's scope), and J5 only brackets the
    /// destination's balance.
    #[allow(dead_code)]
    delegated_wallet: Keypair,
    destination: Pubkey,
    destination_mint: Pubkey,
    destination_owner: Pubkey,
}

async fn fund_account(context: &mut ProgramTestContext, recipient: &Pubkey, lamports: u64) {
    context.last_blockhash = context.banks_client.get_latest_blockhash().await.unwrap();
    let ix = system_instruction::transfer(&context.payer.pubkey(), recipient, lamports);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context.banks_client.process_transaction(tx).await.unwrap();
}

/// Build a base J5 fixture with:
///   - a fresh `clawsol_authority` program instance,
///   - an AuthorizationRecord (action_type = JupiterBuySolWithUsdc) at
///     `derive_authorization_pda(program_id, version, payer, rule_id)`,
///   - a pre-seeded SPL Token destination account at `destination` with
///     `amount = pre_balance_amount` and `mint = destination_mint`,
///   - an externally-generated executor keypair, funded for tx fees.
async fn setup_fixture(
    rule_id: [u8; 16],
    destination_amount: u64,
    destination_mint_override: Option<Pubkey>,
    destination_data_len_override: Option<usize>,
) -> J5Fixture {
    let program_id = Pubkey::new_unique();
    let mut program_test = ProgramTest::new(
        "clawsol_authority",
        program_id,
        processor!(clawsol_authority::process_instruction),
    );

    let executor = Keypair::new();
    let delegated_wallet = Keypair::new();
    let destination = Pubkey::new_unique();
    let destination_mint = destination_mint_override.unwrap_or(Pubkey::new_unique());
    let destination_owner = Pubkey::new_unique();

    // Seed the destination SPL Token account at the requested length.
    let rent = Rent::default();
    let data = if let Some(len) = destination_data_len_override {
        // Length-override fixture: build a buffer of the chosen size with
        // the right bytes at the canonical offsets — the decoder MUST
        // reject anything that is not exactly 165 bytes regardless.
        let mut buf = vec![0u8; len];
        let cap = len.min(SPL_TOKEN_ACCOUNT_LEN);
        if cap >= SPL_TOKEN_OFFSET_OWNER + 32 {
            buf[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32]
                .copy_from_slice(&destination_mint.to_bytes());
            buf[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32]
                .copy_from_slice(&destination_owner.to_bytes());
        }
        if cap >= SPL_TOKEN_OFFSET_AMOUNT + 8 {
            buf[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
                .copy_from_slice(&destination_amount.to_le_bytes());
        }
        buf
    } else {
        make_spl_token_bytes(&destination_mint, &destination_owner, destination_amount)
    };
    let dest_lamports = rent.minimum_balance(data.len().max(SPL_TOKEN_ACCOUNT_LEN));
    program_test.add_account(
        destination,
        Account {
            lamports: dest_lamports,
            data,
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    );

    let mut context = program_test.start_with_context().await;

    fund_account(&mut context, &executor.pubkey(), 1_000_000_000).await;
    fund_account(&mut context, &delegated_wallet.pubkey(), 100_000_000).await;

    // Create the AuthorizationRecord — Jupiter action.
    let canonical_rule_hash: [u8; 32] = {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(&rule_id);
        h[16..].copy_from_slice(&rule_id);
        h
    };
    let action_type = Stage2ActionType::JupiterBuySolWithUsdc.to_u8();
    let max_input_amount_raw: u64 = 1_000_000_000;
    let expires_at_slot: u64 = 100_000_000;

    let (pda, _bump) = derive_authorization_pda(
        &program_id,
        SCHEMA_VERSION,
        &context.payer.pubkey(),
        &rule_id,
    );

    context.last_blockhash = context.banks_client.get_latest_blockhash().await.unwrap();
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
        .expect("create_authorization for J5 fixture");

    J5Fixture {
        context,
        program_id,
        rule_id,
        pda,
        executor,
        delegated_wallet,
        destination,
        destination_mint,
        destination_owner,
    }
}

async fn load_checkpoint(
    context: &mut ProgramTestContext,
    checkpoint_pda: Pubkey,
) -> JupiterBracketCheckpoint {
    let acc = context
        .banks_client
        .get_account(checkpoint_pda)
        .await
        .expect("get_account checkpoint")
        .expect("checkpoint exists");
    JupiterBracketCheckpoint::try_from_slice(&acc.data).expect("checkpoint borsh decode")
}

async fn checkpoint_exists(
    context: &mut ProgramTestContext,
    checkpoint_pda: Pubkey,
) -> bool {
    context
        .banks_client
        .get_account(checkpoint_pda)
        .await
        .expect("get_account checkpoint")
        .is_some()
}

async fn run_pre_check(fixture: &mut J5Fixture, proof: JupiterPreBracketProof) -> Result<(), TransactionError> {
    let ix = pre_jupiter_bracket_check_instruction(
        &fixture.program_id,
        &fixture.executor.pubkey(),
        &fixture.pda,
        proof,
    );
    fixture.context.last_blockhash = fixture
        .context
        .banks_client
        .get_latest_blockhash()
        .await
        .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fixture.executor.pubkey()),
        &[&fixture.executor],
        fixture.context.last_blockhash,
    );
    fixture
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .map_err(|e| match e {
            solana_program_test::BanksClientError::TransactionError(te) => te,
            other => panic!("unexpected banks-client error: {other:?}"),
        })
}

async fn run_post_check(
    fixture: &mut J5Fixture,
    proof: JupiterPostBracketProof,
) -> Result<(), TransactionError> {
    let ix = post_jupiter_bracket_check_instruction(
        &fixture.program_id,
        &fixture.executor.pubkey(),
        &fixture.pda,
        proof,
    );
    fixture.context.last_blockhash = fixture
        .context
        .banks_client
        .get_latest_blockhash()
        .await
        .unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&fixture.executor.pubkey()),
        &[&fixture.executor],
        fixture.context.last_blockhash,
    );
    fixture
        .context
        .banks_client
        .process_transaction(tx)
        .await
        .map_err(|e| match e {
            solana_program_test::BanksClientError::TransactionError(te) => te,
            other => panic!("unexpected banks-client error: {other:?}"),
        })
}

/// Re-set the destination SPL Token account's `amount` field to a new
/// value. Re-uses the same address and `(mint, owner)` byte layout the
/// fixture seeded.
async fn set_destination_amount(fixture: &mut J5Fixture, new_amount: u64) {
    let data = make_spl_token_bytes(
        &fixture.destination_mint,
        &fixture.destination_owner,
        new_amount,
    );
    let rent = Rent::default();
    let lamports = rent.minimum_balance(SPL_TOKEN_ACCOUNT_LEN);
    fixture.context.set_account(
        &fixture.destination,
        &Account {
            lamports,
            data,
            owner: SPL_TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
        .into(),
    );
}

fn assert_custom_error(err: TransactionError, expected: AuthorityError) {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(
                code, expected as u32,
                "expected {expected:?} (code {}); got Custom({code})",
                expected as u32
            );
        }
        other => panic!("expected Custom({}) [{:?}]; got {:?}", expected as u32, expected, other),
    }
}

// ── P0 tests ────────────────────────────────────────────────────────────────

/// Pre-check records the LIVE pre-balance read from the destination's
/// SPL Token data, never an executor-supplied number. We seed the
/// destination at amount=12345, run the pre-check (which has no
/// pre-balance field on the wire), and assert the checkpoint stored
/// 12345.
#[tokio::test]
async fn j5_pre_check_records_live_pre_balance() {
    let mint = Pubkey::new_unique();
    let mut fx =
        setup_fixture(RULE_ID_PRE_RECORDS_LIVE_PRE, 12_345, Some(mint), None).await;
    let dest = fx.destination;
    let pda = fx.pda;
    let program_id = fx.program_id;
    let proof = JupiterPreBracketProof {
        execution_nonce: 1,
        destination_token_account: dest,
        expected_destination_mint: mint,
        min_output_amount_raw: 100,
    };
    run_pre_check(&mut fx, proof).await.expect("pre-check happy path");

    let (cp_pda, _) =
        derive_jupiter_bracket_checkpoint_pda(&program_id, &pda, 1, &dest);
    let cp = load_checkpoint(&mut fx.context, cp_pda).await;
    assert_eq!(cp.schema_version, JUPITER_BRACKET_SCHEMA_VERSION);
    assert_eq!(cp.authorization_pda, pda);
    assert_eq!(cp.execution_nonce, 1);
    assert_eq!(cp.destination_token_account, dest);
    assert_eq!(cp.destination_mint, mint);
    assert_eq!(cp.pre_balance, 12_345, "pre_balance MUST be live-read");
    assert_eq!(cp.min_output_amount_raw, 100);
    assert_eq!(cp.consumed, false);
}

/// Post-check accepts a delta exactly equal to min_output_amount_raw —
/// the boundary case `delta == min_out` MUST pass.
#[tokio::test]
async fn j5_post_check_accepts_delta_equal_min_out() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_EQUAL_MIN, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    let pda = fx.pda;
    let program_id = fx.program_id;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 500,
        },
    )
    .await
    .expect("pre-check");

    // Bump destination amount by exactly min_out (1000 → 1500, delta = 500).
    set_destination_amount(&mut fx, 1_500).await;

    run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect("post-check delta == min_out must pass");

    let (cp_pda, _) = derive_jupiter_bracket_checkpoint_pda(&program_id, &pda, 1, &dest);
    let cp = load_checkpoint(&mut fx.context, cp_pda).await;
    assert_eq!(cp.consumed, true, "checkpoint MUST be consumed after success");
}

/// Post-check accepts deltas strictly greater than min_output_amount_raw.
#[tokio::test]
async fn j5_post_check_accepts_delta_greater_than_min_out() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_GREATER_MIN, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 100,
        },
    )
    .await
    .expect("pre-check");

    // Bump by 9000 (delta = 9000 ≫ min_out=100).
    set_destination_amount(&mut fx, 10_000).await;

    run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect("post-check delta >> min_out must pass");
}

/// Post-check fails closed when delta < min_output_amount_raw.
#[tokio::test]
async fn j5_post_check_rejects_delta_below_min_out() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_BELOW_MIN, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 500,
        },
    )
    .await
    .expect("pre-check");

    // Bump by only 1 (delta = 1 < min_out=500).
    set_destination_amount(&mut fx, 1_001).await;

    let err = run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect_err("post-check must fail when delta < min_out");
    assert_custom_error(err, AuthorityError::JupiterBalanceDeltaTooSmall);
}

/// Post-check fails closed (with the dedicated underflow code) when
/// post_balance < pre_balance — i.e. checked_sub returns None.
#[tokio::test]
async fn j5_post_check_rejects_underflow() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_UNDERFLOW, 5_000, Some(mint), None).await;
    let dest = fx.destination;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 1,
        },
    )
    .await
    .expect("pre-check");

    // Drop destination amount BELOW pre_balance (5000 → 1).
    set_destination_amount(&mut fx, 1).await;

    let err = run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect_err("post-check must fail underflow");
    assert_custom_error(err, AuthorityError::JupiterPostBalanceUnderflow);
}

/// Post-check fails closed when no pre-check ever ran for the (authz,
/// nonce, destination) triple — the checkpoint PDA does not exist.
#[tokio::test]
async fn j5_post_check_rejects_missing_checkpoint() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_MISSING_CP, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    // NO pre_check. Post-check should fail because the checkpoint PDA
    // does not exist (owner is not this program).
    let err = run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect_err("post-check must fail when checkpoint missing");
    assert_custom_error(err, AuthorityError::JupiterBracketCheckpointMissing);
}

/// Replay protection: a second post-check against the same checkpoint
/// MUST fail with the dedicated `JupiterBracketCheckpointConsumed`
/// code.
#[tokio::test]
async fn j5_post_check_rejects_replay_consumed_checkpoint() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_REPLAY, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 100,
        },
    )
    .await
    .expect("pre-check");

    set_destination_amount(&mut fx, 2_000).await;

    // First post-check — succeeds.
    run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect("first post-check succeeds");

    // Second post-check — MUST fail with consumed.
    let err = run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
        },
    )
    .await
    .expect_err("second post-check must fail consumed");
    assert_custom_error(err, AuthorityError::JupiterBracketCheckpointConsumed);
}

/// Pre-check fails closed when the destination's live mint disagrees
/// with the proof's `expected_destination_mint`.
#[tokio::test]
async fn j5_pre_check_rejects_wrong_destination_mint() {
    let real_mint = Pubkey::new_unique();
    let mut fx =
        setup_fixture(RULE_ID_PRE_WRONG_MINT, 1_000, Some(real_mint), None).await;
    let dest = fx.destination;
    let pda = fx.pda;
    let program_id = fx.program_id;

    // Pre-check claims a DIFFERENT mint than the destination's data.
    let wrong_mint = Pubkey::new_unique();
    let err = run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: wrong_mint,
            min_output_amount_raw: 100,
        },
    )
    .await
    .expect_err("pre-check must reject wrong mint");
    assert_custom_error(err, AuthorityError::JupiterMintMismatch);

    // Confirm no checkpoint PDA was created.
    let (cp_pda, _) =
        derive_jupiter_bracket_checkpoint_pda(&program_id, &pda, 1, &dest);
    assert!(
        !checkpoint_exists(&mut fx.context, cp_pda).await,
        "no checkpoint PDA should exist after a failed pre-check"
    );
}

/// Post-check fails closed when the proof's destination_token_account
/// doesn't match what the checkpoint stored. The checkpoint PDA
/// derivation includes the destination, so a wrong destination yields
/// a different PDA address — the supplied checkpoint AccountInfo will
/// either point at a non-existent address (Missing) or, if the caller
/// happened to also swap the AccountInfo, the cross-check between the
/// AccountInfo key and the proof catches it. Here we exercise the
/// "PDA doesn't exist for the wrong-destination triple" path — which
/// surfaces JupiterBracketCheckpointMissing.
#[tokio::test]
async fn j5_post_check_rejects_wrong_destination_account() {
    let mint = Pubkey::new_unique();
    let mut fx = setup_fixture(RULE_ID_POST_WRONG_DEST, 1_000, Some(mint), None).await;
    let dest = fx.destination;
    run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 100,
        },
    )
    .await
    .expect("pre-check");

    // Build a post-check proof claiming a DIFFERENT destination. The
    // builder derives the checkpoint PDA from the proof's destination,
    // so the checkpoint at THAT address does not exist.
    let other_destination = Pubkey::new_unique();
    let err = run_post_check(
        &mut fx,
        JupiterPostBracketProof {
            execution_nonce: 1,
            destination_token_account: other_destination,
        },
    )
    .await
    .expect_err("post-check must fail wrong destination");
    assert_custom_error(err, AuthorityError::JupiterBracketCheckpointMissing);
}

/// SPL Token unpack: the destination must be exactly 165 bytes. A
/// 64-byte destination (just enough to hold the mint and start of the
/// owner field) MUST be rejected at the strict length gate even though
/// the right bytes appear at the right offsets.
#[tokio::test]
async fn j5_token_account_must_be_165_bytes() {
    let mint = Pubkey::new_unique();
    // 64-byte data buffer (less than the full 165-byte Pack length).
    let mut fx =
        setup_fixture(RULE_ID_TOKEN_LEN_REJECT, 1_000, Some(mint), Some(64)).await;
    let dest = fx.destination;
    let err = run_pre_check(
        &mut fx,
        JupiterPreBracketProof {
            execution_nonce: 1,
            destination_token_account: dest,
            expected_destination_mint: mint,
            min_output_amount_raw: 100,
        },
    )
    .await
    .expect_err("pre-check must reject non-165-byte destination");
    // The decoder in `solend_account_decode::decode_spl_token_account_lite`
    // is the canonical strict-length gate; J5 reuses it. The error
    // surfaces as `SolendTokenAccountInvalidLength` (code 72).
    assert_custom_error(err, AuthorityError::SolendTokenAccountInvalidLength);
}

