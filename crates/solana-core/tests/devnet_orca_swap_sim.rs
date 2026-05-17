//! Devnet probe: build an Orca swap TX and simulate it.
//! Run with: cargo test -p claw-solana-core --test devnet_orca_swap_sim -- --nocapture
//!
//! This test does NOT sign or submit — it only builds an unsigned TX and
//! calls simulateTransaction to verify the instruction format is correct.

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    system_program,
    transaction::Transaction,
};
use std::str::FromStr;

// ── Constants (copied from orca.rs to keep test self-contained) ──────────────

const ORCA_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bC8";

const DEVNET_POOL: &str = "3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const DEV_USDC_MINT: &str = "BRjpCHtyQLNCo8gqRUr8jtdAj5AjPYQaoqbvcZiHok1k";

const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
const MIN_SQRT_PRICE: u128 = 4295048016;
const TICK_ARRAY_SIZE: i32 = 88;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn derive_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let ata_prog = Pubkey::from_str(ATA_PROGRAM).unwrap();
    let token_prog = Pubkey::from_str(SPL_TOKEN).unwrap();
    let (pda, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token_prog.as_ref(), mint.as_ref()],
        &ata_prog,
    );
    pda
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

// ── Test ─────────────────────────────────────────────────────────────────────

#[test]
fn simulate_orca_swap_on_devnet() {
    let rpc = RpcClient::new("https://api.devnet.solana.com");
    let program_id = Pubkey::from_str(ORCA_PROGRAM).unwrap();
    let token_program = Pubkey::from_str(SPL_TOKEN).unwrap();
    let pool_pk = Pubkey::from_str(DEVNET_POOL).unwrap();

    // 1. Fetch pool state
    let pool_acc = match rpc.get_account(&pool_pk) {
        Ok(a) => a,
        Err(e) => { println!("SKIP: cannot fetch pool — {e}"); return; }
    };
    let data = &pool_acc.data;
    assert!(data.len() >= 245, "pool data too short");

    // Parse key fields
    let tick_spacing = u16::from_le_bytes(data[41..43].try_into().unwrap());
    let tick_current = i32::from_le_bytes(data[81..85].try_into().unwrap());
    let vault_a = Pubkey::try_from(&data[133..165]).unwrap();
    let vault_b = Pubkey::try_from(&data[213..245]).unwrap();
    let mint_a = Pubkey::from_str(SOL_MINT).unwrap();
    let mint_b = Pubkey::from_str(DEV_USDC_MINT).unwrap();

    println!("Pool: {}", DEVNET_POOL);
    println!("  tick_spacing={}, tick_current={}", tick_spacing, tick_current);
    println!("  vault_a={}, vault_b={}", vault_a, vault_b);

    // Use a dummy wallet (won't sign, just simulate)
    let wallet = Pubkey::new_unique();
    let ata_a = derive_ata(&wallet, &mint_a);
    let ata_b = derive_ata(&wallet, &mint_b);
    let oracle = derive_oracle(&pool_pk);

    // Tick arrays (a_to_b = true: current, current-1, current-2)
    let tia = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start0 = start_tick_for(tick_current, tick_spacing);
    let ta0 = derive_tick_array(&pool_pk, start0);
    let ta1 = derive_tick_array(&pool_pk, start0 - tia);
    let ta2 = derive_tick_array(&pool_pk, start0 - 2 * tia);

    println!("  tick_arrays: [{}, {}, {}]", ta0, ta1, ta2);
    println!("  oracle: {}", oracle);

    // 2. Build swap instruction (swap 1000 lamports SOL → devUSDC)
    let amount: u64 = 1000;
    let threshold: u64 = 0; // no slippage check for simulation
    let sqrt_price_limit = MIN_SQRT_PRICE;

    let mut ix_data = Vec::with_capacity(42);
    ix_data.extend_from_slice(&SWAP_DISCRIMINATOR);
    ix_data.extend_from_slice(&amount.to_le_bytes());
    ix_data.extend_from_slice(&threshold.to_le_bytes());
    ix_data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    ix_data.push(1u8); // amount_specified_is_input = true
    ix_data.push(1u8); // a_to_b = true

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

    // 3. Build unsigned transaction
    let message = Message::new(&[swap_ix], Some(&wallet));
    let tx = Transaction::new_unsigned(message);

    // 4. Simulate
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    match rpc.simulate_transaction_with_config(&tx, sim_config) {
        Ok(result) => {
            let val = result.value;
            if let Some(ref err) = val.err {
                println!("\nSIMULATION FAILED (expected for dummy wallet): {:?}", err);
            } else {
                println!("\nSIMULATION SUCCEEDED!");
            }

            if let Some(logs) = &val.logs {
                println!("\nLogs ({} lines):", logs.len());
                for (i, log) in logs.iter().enumerate().take(20) {
                    println!("  [{}] {}", i, log);
                }
            }

            // Key: if we get an ATA/token account error (not "invalid instruction data"),
            // it means the swap IX format is correct — the error is just
            // because the dummy wallet has no token accounts.
            let err_str = format!("{:?}", val.err);
            if err_str.contains("InvalidInstructionData") {
                println!("\n❌ SWAP IX FORMAT IS WRONG — InvalidInstructionData");
            } else if err_str.contains("InvalidAccountData") || err_str.contains("AccountNotFound")
                || err_str.contains("Custom(1)") || err_str.contains("InsufficientFunds")
                || err_str.contains("InvalidAccountOwner")
            {
                println!("\n✅ SWAP IX FORMAT LOOKS CORRECT — error is account/balance related, not instruction format");
            } else if val.err.is_none() {
                println!("\n✅ SIMULATION SUCCEEDED (unexpected but great!)");
            } else {
                println!("\n⚠ UNKNOWN ERROR — needs investigation: {:?}", val.err);
            }
        }
        Err(e) => println!("\nRPC simulate call failed: {}", e),
    }
}
