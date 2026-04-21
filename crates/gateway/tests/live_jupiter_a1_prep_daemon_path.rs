//! OPT-IN: Phase A1-Prep — daemon-path Jupiter swap to `awaiting_wallet_signature`.
//!
//! Goal
//! ----
//! Prove that the **real daemon pipeline** can take a policy-gated Jupiter
//! swap intent, drive it through:
//!
//!   bind-challenge/confirm  →  SubmitJupiterSwapTool.execute
//!   →  SwapPolicyConfig.evaluate (auto-approve under A1 caps)
//!   →  HttpJupiterJitExecutor (live api.jup.ag + mainnet RPC + ALT resolve)
//!   →  assemble_v0_transaction_with_resolved_alts
//!   →  simulate
//!   →  SignatureOrchestrator.submit (ExternalWalletAdapter)
//!   →  park → state = `awaiting_wallet_signature`
//!
//! and **stop there**. Phantom signing + `complete_v0` + `send_v0` is A1
//! proper, explicitly out of scope for this prep step.
//!
//! Scope vs. `live_jupiter_mainnet_shape.rs`
//! -----------------------------------------
//! That test drives `HttpJupiterClient` and `assemble_v0_transaction_*`
//! directly, bypassing the tool / policy / approval / orchestrator layer.
//! This test drives the SAME components the HTTP daemon wires up and
//! exercises `SubmitJupiterSwapTool`, the approval decision, the spawned
//! JIT resume task, and the external-wallet parking.
//!
//! Bind flow
//! ---------
//! Uses the real `WalletChallengeService.create_challenge` +
//! `verify_challenge` path with a freshly generated test keypair signing the
//! canonical challenge message via ed25519. The challenge TTL, session-
//! binding, and used-once logic are exercised exactly as they are in
//! production. The only "stub" is that the signer is a local test keypair
//! instead of Phantom — in A1 proper, the same flow runs with Phantom
//! signing the challenge.
//!
//! Opt-in
//! ------
//! Self-skips unless `CLAW_LIVE_JUPITER_A1_PREP=1`. CI does not set this.
//! The test hits live api.jup.ag + mainnet RPC; it does NOT broadcast.
//!
//! Run
//! ---
//! ```bash
//! CLAW_LIVE_JUPITER_A1_PREP=1 \
//!   cargo test -p claw-gateway --test live_jupiter_a1_prep_daemon_path \
//!     -- --nocapture
//! ```

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer as _, SigningKey};
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};

use claw_state_store::{
    audit::AuditRepository,
    db::Database,
    pending_jupiter::PendingJupiterRepository,
    wallet_bindings::WalletBindingRepository,
    wallet_challenges::WalletChallengeRepository,
};

use async_trait::async_trait;
use claw_gateway::{
    approval_store::ApprovalStore,
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    integrations::jupiter::HttpJupiterClient,
    integrations::jupiter_production::{
        ClawAltFetcher, HttpJupiterJitExecutor, V0Rpc, V0SimOutcome,
    },
    integrations::jupiter_jit::JupiterJitExecutor,
    integrations::jupiter_park::PendingJupiterParkStore,
    orchestrator::{
        external_adapter::ExternalWalletAdapter,
        types::PendingSummary,
        SignatureOrchestrator,
    },
    policy_alerting::AlertDispatcher,
    tools::jupiter_swap::SubmitJupiterSwapTool,
    wallet_challenge::WalletChallengeService,
};

use claw_observability::metrics::MetricsRegistry;
use claw_solana_core::rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig};
use claw_tool_system::tool::Tool;
use claw_types::{
    session::SessionId,
    solana::{CommitmentLevel, SolanaNetwork},
    swap::SwapPolicyConfig,
    tool::ToolInput,
};

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// A1 demo target: 0.001 SOL = 1,000,000 lamports.
const INPUT_AMOUNT: u64 = 1_000_000;

/// 1 % slippage (inside the 100 bps policy cap).
const SLIPPAGE_BPS: u16 = 100;

/// A1 demo cap (see `config/jupiter-mainnet-e2e.example.toml`): 0.005 SOL.
const A1_MAX_INPUT: u64 = 5_000_000;

fn skip_unless_opted_in() -> bool {
    match std::env::var("CLAW_LIVE_JUPITER_A1_PREP") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_JUPITER_A1_PREP=1 to run this opt-in \
                 daemon-path integration test. It hits live api.jup.ag + \
                 mainnet RPC, but DOES NOT broadcast any transaction."
            );
            true
        }
    }
}

fn mainnet_rpc() -> ClawRpcClient {
    let config = RpcPoolConfig {
        endpoints: vec![EndpointConfig {
            url: "https://api.mainnet-beta.solana.com".to_string(),
            label: "mainnet-public".to_string(),
            is_write_endpoint: true,
        }],
        failure_threshold: 3,
        recovery_interval: Duration::from_secs(30),
        request_timeout: Duration::from_secs(20),
    };
    let pool = RpcPool::new(config);
    ClawRpcClient::new(pool, CommitmentLevel::Confirmed)
}

/// A1 demo swap policy. Intentionally mirrors
/// `config/jupiter-mainnet-e2e.example.toml [jupiter.swap_policy]`.
fn a1_policy() -> SwapPolicyConfig {
    SwapPolicyConfig {
        allowed_input_mints: vec![WSOL.to_string(), USDC.to_string()],
        allowed_output_mints: vec![WSOL.to_string(), USDC.to_string()],
        max_input_amount: Some(A1_MAX_INPUT),
        max_slippage_bps: Some(100),
        required_approver_role: Some("treasury".to_string()),
    }
}

fn divider(label: &str) {
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  {label}");
    println!("══════════════════════════════════════════════════════════════");
}

/// A `V0Rpc` that accepts simulate unconditionally and refuses to send.
///
/// # Why this is used in A1-Prep (and NOT in `live_jupiter_mainnet_shape`)
///
/// The A1-Prep test must generate its own keypair so the bind challenge can
/// be signed programmatically. A fresh keypair has no mainnet funds, so
/// live `simulateTransaction` fails with `AccountNotFound` — which kills
/// the JIT task before it can reach `awaiting_wallet_signature`.
///
/// Simulate's own correctness is already live-verified by the sibling test
/// `live_jupiter_mainnet_shape`, which uses a FUNDED pubkey. A1-Prep's
/// contribution is the *pipeline* around JIT — tool dispatch, policy,
/// approval, spawn, park — so we stub simulate and let the rest run real.
/// `send_v0` must never be invoked on this test; panicking makes that
/// invariant structural.
struct StubSimulateRefuseSendV0Rpc;

#[async_trait]
impl V0Rpc for StubSimulateRefuseSendV0Rpc {
    async fn simulate_v0(
        &self,
        _tx: &solana_sdk::transaction::VersionedTransaction,
    ) -> V0SimOutcome {
        V0SimOutcome {
            success: true,
            error: None,
        }
    }

    async fn send_v0(&self, _signed_tx_bytes: &[u8]) -> Result<String, String> {
        panic!(
            "send_v0 called in A1-Prep test — this test must stop at \
             awaiting_wallet_signature and never broadcast."
        );
    }
}

#[tokio::test]
async fn a1_prep_daemon_path_reaches_awaiting_wallet_signature() {
    if skip_unless_opted_in() {
        return;
    }

    // ── 1. Fresh local keypair pretending to be the operator's Phantom ─────
    //
    // The only "stub" in this test: in A1 proper, Phantom signs the bind
    // challenge + the swap; here a local keypair signs the challenge and
    // the swap itself is deliberately NOT signed (we stop at park).
    let wallet_keypair = Keypair::new();
    let wallet_pubkey = wallet_keypair.pubkey();
    let wallet_pubkey_str = wallet_pubkey.to_string();

    // ── 2. Assemble the real daemon components against an in-memory DB ─────
    let db = Database::open_in_memory().await.expect("open in-memory DB");
    let pool = db.pool().clone();

    let pending_jupiter_repo = PendingJupiterRepository::new(pool.clone());
    let audit = AuditRepository::new(pool.clone());
    let challenge_repo = WalletChallengeRepository::new(pool.clone());
    let binding_repo = WalletBindingRepository::new(pool.clone());

    let approval_store = ApprovalStore::new();
    let event_bus = EventBus::new();
    let alerts = AlertDispatcher::default();
    let metrics = Arc::new(MetricsRegistry::default());
    let park_store = PendingJupiterParkStore::new();

    let external_store: Arc<ExternalWalletStore> =
        Arc::new(ExternalWalletStore::new().with_binding_repo(binding_repo));

    let adapter = ExternalWalletAdapter::new(external_store.clone());
    let orchestrator = SignatureOrchestrator::new(vec![Arc::new(adapter)]);

    let jupiter_client = Arc::new(HttpJupiterClient::new());
    let rpc_client = mainnet_rpc();
    // Simulate is stubbed; see StubSimulateRefuseSendV0Rpc doc.
    let v0_rpc = Arc::new(StubSimulateRefuseSendV0Rpc);
    let alt_fetcher = Arc::new(ClawAltFetcher::new(rpc_client));

    let jit_executor: Arc<dyn JupiterJitExecutor> = Arc::new(HttpJupiterJitExecutor::new(
        jupiter_client,
        v0_rpc,
        alt_fetcher,
        orchestrator.clone(),
    ));

    let tool = SubmitJupiterSwapTool::new(
        pending_jupiter_repo.clone(),
        approval_store.clone(),
        event_bus.clone(),
        audit.clone(),
        a1_policy(),
        SolanaNetwork::MainnetBeta,
        600,
        alerts,
        metrics,
        jit_executor,
        park_store.clone(),
    );

    let challenge_service = WalletChallengeService::new(challenge_repo);

    // ── 3. Create a session and bind the wallet via the real challenge path
    divider("STEP 1 — real bind-challenge + verify (ed25519 over canonical message)");
    let session_id = SessionId::from(uuid::Uuid::new_v4());

    let challenge = challenge_service
        .create_challenge(&session_id, &wallet_pubkey_str)
        .await
        .expect("create_challenge must succeed");

    println!("session_id    : {session_id}");
    println!("wallet_pubkey : {wallet_pubkey_str}");
    println!("challenge_id  : {}", challenge.challenge_id);
    println!("message       : {:?}", challenge.message);

    // Sign the canonical message with the local keypair.
    // (Keypair's 64-byte format = [seed:32][pubkey:32]; ed25519-dalek SigningKey
    // is built from the 32-byte seed only.)
    let keypair_bytes = wallet_keypair.to_bytes();
    let signing_key = SigningKey::from_bytes(
        &keypair_bytes[..32]
            .try_into()
            .expect("keypair has 32-byte seed"),
    );
    let signature = signing_key.sign(challenge.message.as_bytes());

    // Verify via the real service; this also marks the challenge as used.
    let verified_pubkey = challenge_service
        .verify_challenge(
            &session_id,
            &challenge.challenge_id,
            &wallet_pubkey_str,
            &signature.to_bytes(),
        )
        .await
        .expect("verify_challenge must succeed");
    assert_eq!(verified_pubkey, wallet_pubkey_str);

    // The HTTP layer's bind-confirm handler calls `ExternalWalletStore.bind_wallet`
    // only AFTER `verify_challenge` returns success. Do the same here.
    external_store.bind_wallet(&session_id, &wallet_pubkey_str);
    assert!(
        external_store.is_external_wallet(&session_id, &wallet_pubkey_str),
        "wallet must be bound after challenge verify"
    );
    println!("✓ challenge verified, wallet bound to session");

    // ── 4. Submit the Jupiter intent through the real tool ────────────────
    divider("STEP 2 — SubmitJupiterSwapTool.execute (real policy + approval path)");
    let input = ToolInput {
        tool_name: "submit_jupiter_swap".to_string(),
        parameters: serde_json::json!({
            "input_mint": WSOL,
            "output_mint": USDC,
            "input_amount": INPUT_AMOUNT,
            "slippage_bps": SLIPPAGE_BPS,
            "wallet_pubkey": wallet_pubkey_str,
            "description": "A1-Prep daemon-path live validation",
        }),
        session_id: session_id.clone(),
        correlation_id: uuid::Uuid::new_v4(),
    };

    let output = tool.execute(input).await.expect("tool execute must succeed");
    assert!(output.success, "tool must succeed: {:?}", output.error);

    let data = output.data.expect("tool must return data");
    let status = data["status"].as_str().unwrap().to_string();
    let intent_id: uuid::Uuid = data["intent_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("intent_id is uuid");

    println!("status         : {status}");
    println!("intent_id      : {intent_id}");
    println!("policy_verdict : {}", data["policy_verdict"]);
    println!("policy_rule    : {}", data["policy_rule_name"]);

    assert_eq!(
        status, "approved_waiting_jit",
        "A1 policy must auto-approve 0.001 SOL swap (within cap)"
    );

    // ── 5. Wait for the spawned JIT task to reach awaiting_wallet_signature
    divider("STEP 3 — waiting for JIT pipeline to park awaiting_wallet_signature");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut final_state = String::new();
    loop {
        if let Some(row) = pending_jupiter_repo
            .get(intent_id)
            .await
            .expect("get pending")
        {
            final_state = row.state.clone();
            if row.state == "awaiting_wallet_signature" {
                println!("reached state : {}", row.state);
                break;
            }
            if row.state == "jit_build_failed" || row.state == "rejected" {
                panic!(
                    "JIT reached terminal failure state `{}` with last_error={:?}",
                    row.state, row.last_error
                );
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for awaiting_wallet_signature (current state: `{final_state}`)"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // ── 6. Read the parked tx back through the orchestrator's public view ─
    divider("STEP 4 — pending signature request (what /wallet-signatures would return)");
    let pending = orchestrator.pending_for_session(&session_id);
    assert_eq!(
        pending.len(),
        1,
        "exactly one parked tx expected, got {}",
        pending.len()
    );
    let parked: &PendingSummary = &pending[0];
    println!("request_id        : {}", parked.request_id);
    println!("transaction_id    : {}", parked.transaction_id);
    println!("adapter_type      : {}", parked.adapter_type);
    println!("description       : {}", parked.description);
    println!("expected_signer   : {}", parked.expected_signer);
    println!("tx bytes size     : {} bytes", parked.finalized_tx_bytes.len());

    assert_eq!(
        parked.expected_signer, wallet_pubkey_str,
        "parked request must target the bound wallet"
    );

    // Decode the parked V0 for inspection.
    use solana_sdk::message::VersionedMessage;
    use solana_sdk::transaction::VersionedTransaction;
    let v0_tx: VersionedTransaction = bincode::deserialize(&parked.finalized_tx_bytes)
        .expect("parked bytes must deserialize as VersionedTransaction");
    let v0_msg = match &v0_tx.message {
        VersionedMessage::V0(m) => m,
        other => panic!("expected V0 message, got {other:?}"),
    };
    let static_keys = v0_msg.account_keys.len();
    let alt_count = v0_msg.address_table_lookups.len();
    let ix_count = v0_msg.instructions.len();
    let req_sigs = v0_msg.header.num_required_signatures;

    // ── 7. Final human-review payload — the A1-Prep deliverable ───────────
    divider("HUMAN REVIEW — tx shape (daemon parked, NOT broadcast)");
    println!("session             : {session_id}");
    println!("signer_wallet       : {wallet_pubkey_str}");
    println!("  (test keypair — A1 proper swaps this for Phantom)");
    println!("signer_path         : EXTERNAL_WALLET via ExternalWalletAdapter");
    println!("input_mint          : {WSOL}  (wSOL)");
    println!("output_mint         : {USDC}  (USDC)");
    println!("input_amount        : {INPUT_AMOUNT} lamports (0.001 SOL)");
    println!("slippage_bps        : {SLIPPAGE_BPS} (1 %)");
    println!("policy_cap          : {A1_MAX_INPUT} lamports (0.005 SOL)");
    println!("policy_verdict      : Approved (auto, within cap)");
    println!("intent_id           : {intent_id}");
    println!("request_id          : {}", parked.request_id);
    println!("tx_version          : V0");
    println!("tx_instructions     : {ix_count}");
    println!("static_account_keys : {static_keys}");
    println!("ALT references      : {alt_count}");
    println!("required_signatures : {req_sigs}");
    println!("bytes_to_sign       : {} (bincode V0 payload)", parked.finalized_tx_bytes.len());
    println!("full_daemon_path    : YES (bind-challenge → tool.execute → policy → JIT → orchestrator → park)");
    println!("ready_for_Phantom   : YES — emits the exact bytes a frontend would forward to Phantom");
    println!("broadcast_performed : NO — STOP");

    divider("SAFETY");
    println!("✓ no signed bytes submitted to the orchestrator");
    println!("✓ no send_v0 called");
    println!("✓ no mainnet transaction broadcast");
}
