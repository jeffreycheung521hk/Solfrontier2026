//! One-shot devnet probe: check if Orca Whirlpool program and pools exist.
//! Run with: cargo test -p claw-solana-core --test devnet_orca_check -- --nocapture

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[test]
fn probe_orca_whirlpool_devnet() {
    let rpc = RpcClient::new("https://api.devnet.solana.com");

    // 1. Check program account
    let program_id = Pubkey::from_str("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc").unwrap();
    match rpc.get_account(&program_id) {
        Ok(acc) => println!("PROGRAM: exists, executable={}, owner={}", acc.executable, acc.owner),
        Err(e) => {
            println!("PROGRAM: NOT FOUND — {}", e);
            return;
        }
    }

    // 2. Check the specific devnet pool from Orca docs
    let known_pool = "3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt";
    let pool_pk = Pubkey::from_str(known_pool).unwrap();
    match rpc.get_account(&pool_pk) {
        Ok(acc) => {
            println!("KNOWN POOL {}: exists, owner={}, data_len={}", known_pool, acc.owner, acc.data.len());
            if acc.owner == program_id && acc.data.len() >= 245 {
                let mint_a = Pubkey::try_from(&acc.data[101..133]).unwrap_or_default();
                let mint_b = Pubkey::try_from(&acc.data[181..213]).unwrap_or_default();
                let liquidity = u128::from_le_bytes(acc.data[49..65].try_into().unwrap_or([0;16]));
                println!("  mint_a={} mint_b={} liquidity={}", mint_a, mint_b, liquidity);
            }
        }
        Err(e) => println!("KNOWN POOL {}: NOT FOUND — {}", known_pool, e),
    }

    // 3. Check WhirlpoolsConfig for devnet
    let config_addr = "FcrweFY1G9HJAHG5inkGB6pKg1HZ6x9UC2WioAfWrGkR";
    let config_pk = Pubkey::from_str(config_addr).unwrap();
    match rpc.get_account(&config_pk) {
        Ok(acc) => println!("DEVNET CONFIG {}: exists, owner={}, data_len={}", config_addr, acc.owner, acc.data.len()),
        Err(e) => println!("DEVNET CONFIG {}: NOT FOUND — {}", config_addr, e),
    }

    // 4. Search via getProgramAccounts
    println!("\nSearching for Whirlpool pool accounts on devnet...");
    use solana_client::rpc_config::RpcProgramAccountsConfig;
    use solana_client::rpc_filter::{Memcmp, RpcFilterType, MemcmpEncodedBytes};
    use solana_account_decoder::UiAccountEncoding;
    use solana_client::rpc_config::RpcAccountInfoConfig;

    let discriminant = vec![0x96u8, 0x1a, 0xa8, 0x5a, 0x9d, 0x97, 0x0a, 0x75];
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::Memcmp(Memcmp::new(0, MemcmpEncodedBytes::Bytes(discriminant))),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: None,
            min_context_slot: None,
        },
        with_context: None,
        sort_results: None,
    };

    match rpc.get_program_accounts_with_config(&program_id, config) {
        Ok(accounts) => {
            println!("Found {} Whirlpool accounts on devnet", accounts.len());
            for (i, (pk, acc)) in accounts.iter().enumerate().take(10) {
                if acc.data.len() > 245 {
                    let mint_a = Pubkey::try_from(&acc.data[101..133]).unwrap_or_default();
                    let mint_b = Pubkey::try_from(&acc.data[181..213]).unwrap_or_default();
                    let liquidity = u128::from_le_bytes(acc.data[49..65].try_into().unwrap_or([0;16]));
                    println!("  [{}] pool={} mint_a={} mint_b={} liq={}", i, pk, mint_a, mint_b, liquidity);
                }
            }
            if accounts.is_empty() {
                println!("NO POOLS FOUND on devnet.");
            }
        }
        Err(e) => println!("getProgramAccounts failed: {}", e),
    }
}
