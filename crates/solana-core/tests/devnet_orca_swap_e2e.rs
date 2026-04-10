//! Devnet E2E: real Orca swap with real keypair.
//! Run with: cargo test -p claw-solana-core --test devnet_orca_swap_e2e -- --nocapture
//!
//! This test uses a REAL devnet keypair to:
//! 1. Fetch pool state
//! 2. Build swap instruction (SOL → devUSDC)
//! 3. Create ATA for devUSDC if needed
//! 4. Simulate the transaction
//! 5. If simulation passes → sign and submit
//!
//! ⚠ Requires: devnet keypair with SOL balance at
//!   C:/Users/jeffr/Downloads/New/testingcrypto2/data/devnet.json

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;

const ORCA_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bC8";
const DEVNET_POOL: &str = "3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const DEV_USDC_MINT: &str = "BRjpCHtyQLNCo8gqRUr8jtdAj5AjPYQaoqbvcZiHok1k";
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
const MIN_SQRT_PRICE: u128 = 4295048016;
const TICK_ARRAY_SIZE: i32 = 88;
const KEYPAIR_PATH: &str = "C:/Users/jeffr/Downloads/New/testingcrypto2/data/devnet.json";

fn load_keypair() -> Keypair {
    let data = std::fs::read_to_string(KEYPAIR_PATH)
        .expect("cannot read keypair file");
    let bytes: Vec<u8> = serde_json::from_str(&data)
        .expect("keypair file is not valid JSON array");
    Keypair::from_bytes(&bytes)
        .expect("invalid keypair bytes")
}

fn derive_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address(wallet, mint)
}

fn derive_oracle(whirlpool: &Pubkey) -> Pubkey {
    let prog = Pubkey::from_str(ORCA_PROGRAM).unwrap();
    let (pda, _) = Pubkey::find_program_address(
        &[b"oracle", whirlpool.as_ref()],
        &prog,
    );
    pda
}

fn derive_tick_array(whirlpool: &Pubkey, start: i32) -> Pubkey {
    let prog = Pubkey::from_str(ORCA_PROGRAM).unwrap();
    let s = start.to_string();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), s.as_bytes()],
        &prog,
    );
    pda
}

fn start_tick_for(tick: i32, tick_spacing: u16) -> i32 {
    let tia = TICK_ARRAY_SIZE * tick_spacing as i32;
    (tick as f64 / tia as f64).floor() as i32 * tia
}

fn create_ata_idem(funder: &Pubkey, wallet: &Pubkey, mint: &Pubkey) -> Instruction {
    create_associated_token_account_idempotent(funder, wallet, mint, &Pubkey::from_str(SPL_TOKEN).unwrap())
}

#[test]
fn e2e_orca_swap_devnet() {
    // Load real keypair
    let keypair = load_keypair();
    let wallet = keypair.pubkey();
    println!("Wallet: {}", wallet);

    let rpc = RpcClient::new("https://api.devnet.solana.com");

    // Check SOL balance
    let balance = rpc.get_balance(&wallet).unwrap_or(0);
    println!("SOL balance: {} lamports ({:.4} SOL)", balance, balance as f64 / 1e9);
    if balance < 10_000_000 {
        println!("SKIP: insufficient SOL balance (need >= 0.01 SOL)");
        return;
    }

    // Fetch pool
    let pool_pk = Pubkey::from_str(DEVNET_POOL).unwrap();
    let pool_acc = match rpc.get_account(&pool_pk) {
        Ok(a) => a,
        Err(e) => { println!("SKIP: cannot fetch pool — {e}"); return; }
    };
    let data = &pool_acc.data;
    let tick_spacing = u16::from_le_bytes(data[41..43].try_into().unwrap());
    let tick_current = i32::from_le_bytes(data[81..85].try_into().unwrap());
    let vault_a = Pubkey::try_from(&data[133..165]).unwrap();
    let vault_b = Pubkey::try_from(&data[213..245]).unwrap();
    let sqrt_price = u128::from_le_bytes(data[65..81].try_into().unwrap());
    let liquidity = u128::from_le_bytes(data[49..65].try_into().unwrap());

    println!("Pool: tick_spacing={} tick_current={} liquidity={}", tick_spacing, tick_current, liquidity);

    let price = (sqrt_price as f64 / (1u128 << 64) as f64).powi(2);
    println!("Price (devUSDC per SOL): {:.6}", price);

    let program_id = Pubkey::from_str(ORCA_PROGRAM).unwrap();
    let token_program = Pubkey::from_str(SPL_TOKEN).unwrap();
    let mint_a = Pubkey::from_str(SOL_MINT).unwrap();
    let mint_b = Pubkey::from_str(DEV_USDC_MINT).unwrap();
    let ata_a = derive_ata(&wallet, &mint_a);
    let ata_b = derive_ata(&wallet, &mint_b);
    let oracle = derive_oracle(&pool_pk);

    let tia = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start0 = start_tick_for(tick_current, tick_spacing);
    let ta0 = derive_tick_array(&pool_pk, start0);
    let ta1 = derive_tick_array(&pool_pk, start0 - tia);
    let ta2 = derive_tick_array(&pool_pk, start0 - 2 * tia);

    // Check which accounts exist
    println!("\nAccount existence check:");
    for (name, pk) in [
        ("ata_a (wSOL)", ata_a),
        ("ata_b (devUSDC)", ata_b),
        ("vault_a", vault_a),
        ("vault_b", vault_b),
        ("oracle", oracle),
        ("ta0", ta0),
        ("ta1", ta1),
        ("ta2", ta2),
    ] {
        match rpc.get_account(&pk) {
            Ok(a) => println!("  ✅ {} ({}) — {} bytes, owner={}", name, pk, a.data.len(), a.owner),
            Err(_) => println!("  ❌ {} ({}) — NOT FOUND", name, pk),
        }
    }

    // Swap 0.001 SOL → devUSDC
    let swap_amount: u64 = 1_000_000; // 0.001 SOL
    let threshold: u64 = 0; // no min output for test

    let mut ix_data = Vec::with_capacity(42);
    ix_data.extend_from_slice(&SWAP_DISCRIMINATOR);
    ix_data.extend_from_slice(&swap_amount.to_le_bytes());
    ix_data.extend_from_slice(&threshold.to_le_bytes());
    ix_data.extend_from_slice(&MIN_SQRT_PRICE.to_le_bytes());
    ix_data.push(1u8); // amount_specified_is_input
    ix_data.push(1u8); // a_to_b (SOL → devUSDC)

    let swap_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(wallet, true),
            AccountMeta::new(pool_pk, false),
            AccountMeta::new(ata_a, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(ata_b, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new(ta0, false),
            AccountMeta::new(ta1, false),
            AccountMeta::new(ta2, false),
            AccountMeta::new_readonly(oracle, false),
        ],
        data: ix_data,
    };

    // Split into separate steps to diagnose issues

    // ── Step A: Create ATAs first (separate TX) ────────────────────────
    println!("\n--- STEP A: Create ATAs ---");
    {
        let ata_instructions = vec![
            create_ata_idem(&wallet, &wallet, &mint_a),
            create_ata_idem(&wallet, &wallet, &mint_b),
        ];
        let bh = rpc.get_latest_blockhash().unwrap();
        let msg = Message::new(&ata_instructions, Some(&wallet));
        let mut ata_tx = Transaction::new_unsigned(msg);
        ata_tx.sign(&[&keypair], bh);
        match rpc.send_and_confirm_transaction_with_spinner(&ata_tx) {
            Ok(sig) => println!("  ATAs created: {}", sig),
            Err(e) => println!("  ATA creation: {} (may already exist)", e),
        }
    }

    // ── Step B: Fund wSOL ATA ────────────────────────────────────────
    println!("\n--- STEP B: Fund wSOL ATA ---");
    {
        let fund_instructions = vec![
            solana_sdk::system_instruction::transfer(&wallet, &ata_a, swap_amount + 10_000),
            Instruction {
                program_id: token_program,
                accounts: vec![AccountMeta::new(ata_a, false)],
                data: vec![17], // SyncNative
            },
        ];
        let bh = rpc.get_latest_blockhash().unwrap();
        let msg = Message::new(&fund_instructions, Some(&wallet));
        let mut fund_tx = Transaction::new_unsigned(msg);
        fund_tx.sign(&[&keypair], bh);
        match rpc.send_and_confirm_transaction_with_spinner(&fund_tx) {
            Ok(sig) => println!("  wSOL funded: {}", sig),
            Err(e) => println!("  wSOL funding failed: {}", e),
        }
    }

    // Re-check accounts
    println!("\n--- Account check after setup ---");
    for (name, pk) in [("ata_a", ata_a), ("ata_b", ata_b), ("oracle", oracle)] {
        match rpc.get_account(&pk) {
            Ok(a) => println!("  ✅ {} — {} bytes", name, a.data.len()),
            Err(_) => println!("  ❌ {} — NOT FOUND", name),
        }
    }

    // ── Step C: Swap (only swap IX, ATAs already exist) ──────────────
    // Rebuild TX with just the swap instruction
    let swap_only_instructions = vec![swap_ix];
    let blockhash = rpc.get_latest_blockhash().unwrap();
    let message = Message::new(&swap_only_instructions, Some(&wallet));
    let mut tx = Transaction::new_unsigned(message);
    tx.message.recent_blockhash = blockhash;

    println!("\n--- SIMULATION ---");
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    let sim_result = match rpc.simulate_transaction_with_config(&tx, sim_config) {
        Ok(r) => r,
        Err(e) => {
            println!("RPC simulate call failed: {}", e);
            return;
        }
    };

    if let Some(logs) = &sim_result.value.logs {
        println!("Logs ({} lines):", logs.len());
        for (i, log) in logs.iter().enumerate() {
            println!("  [{}] {}", i, log);
        }
    } else {
        println!("(no logs returned)");
    }

    if let Some(ref err) = sim_result.value.err {
        println!("\nSimulation FAILED: {:?}", err);

        // Check if it's a known recoverable error
        let err_str = format!("{:?}", err);
        if err_str.contains("AccountNotInitialized") || err_str.contains("3012") {
            println!("→ Token account not initialized. May need to wrap SOL first.");
            println!("  (For SOL swaps on Whirlpool, the input is a wrapped SOL ATA,");
            println!("   which needs to be created and funded with the swap amount.)");
        }
        println!("\nStopping before sign+submit due to simulation failure.");
        return;
    }

    println!("\n✅ Simulation PASSED! Proceeding to sign and submit...");

    // ── Sign and submit ─────────────────────────────────────────────────
    tx.sign(&[&keypair], blockhash);

    match rpc.send_and_confirm_transaction_with_spinner(&tx) {
        Ok(sig) => {
            println!("\n🎉 SWAP CONFIRMED ON DEVNET!");
            println!("   Signature: {}", sig);
            println!("   Explorer: https://explorer.solana.com/tx/{}?cluster=devnet", sig);
        }
        Err(e) => {
            println!("\nSubmission failed: {}", e);
        }
    }
}
