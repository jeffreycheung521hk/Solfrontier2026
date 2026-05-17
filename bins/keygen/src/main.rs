use solana_sdk::signer::{keypair::Keypair, Signer};

fn main() {
    let kp = Keypair::new();
    let bytes: Vec<u8> = kp.to_bytes().to_vec();
    let json = serde_json::to_string(&bytes).unwrap();
    std::fs::create_dir_all("data").ok();
    std::fs::write("data/devnet.json", &json).unwrap();
    println!("Pubkey: {}", kp.pubkey());
    println!("Saved:  data/devnet.json");
    println!();
    println!("Go fund it: https://faucet.solana.com/?address={}", kp.pubkey());
}
