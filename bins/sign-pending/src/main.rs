//! Quick signing tool for A4 E2E testing.
//!
//! Recovers a Solana keypair from a BIP39 seed phrase, signs a pending
//! transaction, and submits it back to the ClawSolana daemon.
//!
//! Usage:
//!   sign-pending <session_id> <api_token>
//!
//! Environment:
//!   SEED_PHRASE — 12-word BIP39 mnemonic (required)
//!   CLAW_API_URL — daemon URL (default: http://127.0.0.1:7070)

use base64::Engine;
use solana_sdk::{
    signature::Signer,
    signer::keypair::keypair_from_seed_phrase_and_passphrase,
    transaction::Transaction,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: sign-pending <session_id> <api_token>");
        eprintln!("  SEED_PHRASE env var required");
        std::process::exit(1);
    }

    let session_id = &args[1];
    let api_token = &args[2];
    let api_url = std::env::var("CLAW_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:7070".to_string());

    // 1. Recover keypair from seed phrase
    let seed_phrase = std::env::var("SEED_PHRASE")
        .expect("SEED_PHRASE env var required");

    let keypair = keypair_from_seed_phrase_and_passphrase(&seed_phrase, "")
        .expect("failed to recover keypair from seed phrase");

    println!("Recovered keypair: {}", keypair.pubkey());

    // 2. Fetch pending wallet signature requests
    let client = reqwest::Client::new();
    let pending_url = format!("{}/sessions/{}/wallet-signatures", api_url, session_id);

    let resp = client
        .get(&pending_url)
        .header("Authorization", format!("Bearer {}", api_token))
        .send()
        .await
        .expect("failed to fetch pending signatures");

    let body: serde_json::Value = resp.json().await.expect("invalid JSON response");
    let requests = body["requests"].as_array().expect("no requests array");

    if requests.is_empty() {
        eprintln!("No pending signature requests. Make sure to run propose-transfer first.");
        eprintln!("(Requests expire after 120 seconds)");
        std::process::exit(1);
    }

    // 3. Sign each pending request matching our pubkey
    let my_pubkey = keypair.pubkey().to_string();

    for req in requests {
        let request_id = req["request_id"].as_str().unwrap();
        let expected_signer = req["expected_signer"].as_str().unwrap();
        let unsigned_tx_b64 = req["unsigned_tx_b64"].as_str().unwrap();
        let description = req["description"].as_str().unwrap_or("unknown");

        if expected_signer != my_pubkey {
            println!("Skipping request {} (signer {} != {})", request_id, expected_signer, my_pubkey);
            continue;
        }

        println!("\n--- Signing request: {} ---", request_id);
        println!("Description: {}", description);

        // Decode unsigned transaction
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(unsigned_tx_b64)
            .expect("invalid base64 in unsigned_tx_b64");

        let mut tx: Transaction = bincode::deserialize(&tx_bytes)
            .expect("failed to deserialize transaction");

        println!("Transaction has {} instructions", tx.message.instructions.len());
        println!("Recent blockhash: {}", tx.message.recent_blockhash);

        // Sign the transaction
        tx.try_sign(&[&keypair], tx.message.recent_blockhash)
            .expect("failed to sign transaction");

        println!("Signed! Signature: {}", tx.signatures[0]);

        // Serialize signed transaction
        let signed_bytes = bincode::serialize(&tx)
            .expect("failed to serialize signed transaction");
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        // 4. POST signed transaction back to daemon
        let submit_url = format!("{}/sessions/{}/wallet-signatures", api_url, session_id);
        let submit_body = serde_json::json!({
            "request_id": request_id,
            "signed_tx_b64": signed_b64,
        });

        println!("Submitting to {}...", submit_url);

        let resp = client
            .post(&submit_url)
            .header("Authorization", format!("Bearer {}", api_token))
            .header("Content-Type", "application/json")
            .json(&submit_body)
            .send()
            .await
            .expect("failed to submit signed transaction");

        let status = resp.status();
        let resp_body: serde_json::Value = resp.json().await
            .unwrap_or_else(|_| serde_json::json!({"error": "no response body"}));

        println!("Response [{}]: {}", status, serde_json::to_string_pretty(&resp_body).unwrap());

        if let Some(tx_sig) = resp_body["tx_signature"].as_str() {
            println!("\n=== SUCCESS ===");
            println!("On-chain signature: {}", tx_sig);
            println!("Explorer: https://explorer.solana.com/tx/{}?cluster=devnet", tx_sig);
        }
    }
}
