//! Quick script to generate a devnet keypair JSON file.
//! Run with: cargo run --example gen_keypair

use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;

fn main() {
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let bytes: Vec<u8> = keypair.to_bytes().to_vec();

    // Solana CLI format: JSON array of 64 bytes (secret + public)
    let json = serde_json::to_string(&bytes).unwrap();

    let out_path = "data/devnet.json";
    std::fs::write(out_path, &json).expect("failed to write keypair file");

    println!("Generated new keypair:");
    println!("  Pubkey:  {pubkey}");
    println!("  Saved:   {out_path}");
    println!();
    println!("To fund on devnet, visit:");
    println!("  https://faucet.solana.com/?address={pubkey}");
}
