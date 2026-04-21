//! Production `JupiterJitExecutor` — the real implementation.
//!
//! # Flow
//!
//! 1. Fetch a FRESH Jupiter quote (`/v6/quote`) and FRESH build (`/swap/v2/build`).
//!    This always happens JIT — never before approval.
//! 2. Assemble the decomposed instructions + ALTs + blockhash into an unsigned
//!    V0 `VersionedTransaction` (via [`jupiter_tx::assemble_v0_transaction`]).
//! 3. Simulate the V0 tx via the RPC seam. Simulation failure → `SimulateFailed`.
//! 4. Route the V0 bytes through the existing `SignatureOrchestrator`:
//!    - Local path: `LocalWalletAdapter` signs V0 via `Signer::sign_versioned`.
//!      Then we submit the signed bytes via the RPC seam → `Submitted`.
//!    - External path: `ExternalWalletAdapter` returns `Pending` with the
//!      unsigned bytes → `AwaitingWalletSignature`.
//! 5. Any sign / submit error → `SignFailed`.
//!
//! # Invariants preserved
//!
//! - `/build` is never called before approval (resume task awaits oneshot first).
//! - Build/simulate/sign failures never reach `Submitted` (outcome mapping).
//! - Fresh route + fresh blockhash every attempt (no frozen data).

use std::sync::Arc;
use std::str::FromStr;

use async_trait::async_trait;
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use tracing::{info, warn};

use claw_types::swap::SwapIntent;

use crate::integrations::jupiter::{
    JupiterClient, JupiterError, SwapBuildRequest, SwapQuoteRequest,
};
use crate::integrations::jupiter_alt::{
    resolve_address_lookup_tables, AltAccountFetcher,
};
use crate::integrations::jupiter_jit::{JitOutcome, JupiterJitExecutor};
use crate::integrations::jupiter_tx::{
    assemble_v0_transaction_with_resolved_alts, AssembleError,
};
use crate::orchestrator::{
    types::{SignatureOutcome, SignatureRequest, SignatureRequestId},
    SignatureOrchestrator,
};

// ── RPC seam ───────────────────────────────────────────────────────────────

/// Minimal RPC interface for V0 simulate + send.
///
/// Gateway-local trait (not a generic backend dispatcher). Its sole purpose
/// is to let tests inject a mock without hitting the network. Production
/// impl wraps a Solana `RpcClient`.
#[async_trait]
pub trait V0Rpc: Send + Sync {
    /// Run `simulateTransaction` against a V0 tx. Returns success + optional error.
    async fn simulate_v0(&self, tx: &VersionedTransaction) -> V0SimOutcome;

    /// Submit signed V0 bytes via `sendTransaction`. Returns the signature string
    /// or an error.
    async fn send_v0(&self, signed_tx_bytes: &[u8]) -> Result<String, String>;
}

/// Result of a V0 simulation. Does not model partial results — this is the
/// minimum the executor needs to decide `SimulateFailed` vs continue.
#[derive(Debug, Clone)]
pub struct V0SimOutcome {
    pub success: bool,
    pub error: Option<String>,
}

// ── Production executor ───────────────────────────────────────────────────

/// Production impl of `JupiterJitExecutor`.
///
/// Wires the existing HttpJupiterClient → ALT resolution → V0 assembly →
/// RPC simulate → SignatureOrchestrator → RPC submit.
///
/// `alt_fetcher` is used only when the build response lacks a pre-resolved
/// `addresses_by_lookup_table_address` map. The live Jupiter API always
/// triggers this path; fixture-driven tests can skip it by populating the
/// map directly.
#[derive(Clone)]
pub struct HttpJupiterJitExecutor {
    jupiter: Arc<dyn JupiterClient>,
    rpc: Arc<dyn V0Rpc>,
    alt_fetcher: Arc<dyn AltAccountFetcher>,
    orchestrator: SignatureOrchestrator,
}

impl HttpJupiterJitExecutor {
    pub fn new(
        jupiter: Arc<dyn JupiterClient>,
        rpc: Arc<dyn V0Rpc>,
        alt_fetcher: Arc<dyn AltAccountFetcher>,
        orchestrator: SignatureOrchestrator,
    ) -> Self {
        Self { jupiter, rpc, alt_fetcher, orchestrator }
    }
}

#[async_trait]
impl JupiterJitExecutor for HttpJupiterJitExecutor {
    async fn execute_jit(&self, intent: &SwapIntent) -> JitOutcome {
        info!(
            intent_id = %intent.id,
            input_mint = %intent.input_mint,
            output_mint = %intent.output_mint,
            "JIT start: fetching fresh Jupiter quote"
        );

        // 1. Fresh quote.
        let quote_req = SwapQuoteRequest {
            input_mint: intent.input_mint.clone(),
            output_mint: intent.output_mint.clone(),
            amount: intent.input_amount,
            slippage_bps: Some(intent.slippage_bps),
            swap_mode: None,
            only_direct_routes: None,
            max_accounts: None,
        };
        let quote = match self.jupiter.quote(&quote_req).await {
            Ok(q) => q,
            Err(e) => return build_failed(e, "quote"),
        };

        // 2. Fresh build (the ONLY time /build is called — after approval).
        //
        // `prioritization_fee_lamports: "auto"` lets Jupiter size the priority
        // fee against current mainnet congestion. Without it, a daemon-submitted
        // tx (no wallet-provided fee bump) is liable to be dropped by leaders
        // under load — exactly as observed on the first A1-Execute attempt,
        // where the code path completed successfully but the tx never landed.
        let build_req = SwapBuildRequest {
            quote_response: quote,
            user_public_key: intent.wallet_pubkey.clone(),
            wrap_and_unwrap_sol: Some(true),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: Some(serde_json::json!("auto")),
        };
        let build = match self.jupiter.build(&build_req).await {
            Ok(b) => b,
            Err(e) => return build_failed(e, "build"),
        };

        // 3. Parse payer + assemble V0 tx.
        let payer = match Pubkey::from_str(&intent.wallet_pubkey) {
            Ok(p) => p,
            Err(e) => {
                return JitOutcome::BuildFailed(format!(
                    "invalid wallet_pubkey '{}': {e}",
                    intent.wallet_pubkey
                ));
            }
        };

        // 3a. Resolve ALTs. Prefer the pre-resolved map if present (fixture
        // / test path); otherwise fetch each ALT's account from RPC.
        let resolved_alts = if !build.addresses_by_lookup_table_address.is_empty() {
            match crate::integrations::jupiter_tx::convert_lookup_tables(
                &build.addresses_by_lookup_table_address,
            ) {
                Ok(a) => a,
                Err(e) => {
                    return JitOutcome::BuildFailed(format!(
                        "alt convert: {}",
                        assemble_err(&e)
                    ))
                }
            }
        } else {
            match resolve_address_lookup_tables(
                &build.address_lookup_table_addresses,
                self.alt_fetcher.as_ref(),
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    warn!(
                        intent_id = %intent.id,
                        error = %e,
                        "ALT resolution failed"
                    );
                    return JitOutcome::BuildFailed(format!("alt resolve: {e}"));
                }
            }
        };

        let tx = match assemble_v0_transaction_with_resolved_alts(&build, &payer, &resolved_alts) {
            Ok(t) => t,
            Err(e) => return JitOutcome::BuildFailed(format!("assemble: {}", assemble_err(&e))),
        };

        // 4. Simulate — failure here MUST NOT reach `submitted`.
        let sim = self.rpc.simulate_v0(&tx).await;
        if !sim.success {
            let err = sim.error.unwrap_or_else(|| "simulate failed".to_string());
            warn!(
                intent_id = %intent.id,
                error = %err,
                "JIT simulate failed"
            );
            return JitOutcome::SimulateFailed(err);
        }

        // 5. Route through existing SignatureOrchestrator.
        let tx_bytes = match bincode::serialize(&tx) {
            Ok(b) => b,
            Err(e) => return JitOutcome::BuildFailed(format!("serialize v0: {e}")),
        };

        let sig_request = SignatureRequest {
            id: SignatureRequestId::new(),
            session_id: intent.session_id.clone(),
            transaction_id: intent.id,
            finalized_tx_bytes: tx_bytes,
            expected_signer: payer,
            description: intent.description.clone(),
        };

        let outcome = match self.orchestrator.submit(sig_request).await {
            Ok(o) => o,
            Err(e) => {
                warn!(intent_id = %intent.id, error = %e, "orchestrator submit failed");
                return JitOutcome::SignFailed(format!("orchestrator: {e}"));
            }
        };

        match outcome {
            SignatureOutcome::Signed {
                signature,
                signed_tx_bytes,
                ..
            } => {
                // Local wallet path: submit to the network.
                match self.rpc.send_v0(&signed_tx_bytes).await {
                    Ok(onchain_sig) => {
                        info!(
                            intent_id = %intent.id,
                            signer_sig = %signature,
                            onchain_sig = %onchain_sig,
                            "JIT submitted"
                        );
                        // Prefer the on-chain signature if it differs; usually
                        // they match (the signature IS the transaction id).
                        JitOutcome::Submitted { signature: onchain_sig }
                    }
                    Err(e) => {
                        warn!(intent_id = %intent.id, error = %e, "send_v0 failed");
                        JitOutcome::SignFailed(format!("send_v0: {e}"))
                    }
                }
            }
            SignatureOutcome::Pending { request_id, .. } => {
                info!(
                    intent_id = %intent.id,
                    wallet_request_id = %request_id,
                    "JIT handed off to external wallet signing"
                );
                JitOutcome::AwaitingWalletSignature {
                    wallet_signature_request_id: request_id.0,
                }
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn build_failed(err: JupiterError, stage: &str) -> JitOutcome {
    JitOutcome::BuildFailed(format!("jupiter {stage}: {err}"))
}

fn assemble_err(err: &AssembleError) -> String {
    err.to_string()
}

// ── Production V0 RPC wrapper ──────────────────────────────────────────────

/// Production `V0Rpc` impl backed by `ClawRpcClient`.
///
/// Delegates to `simulate_v0_transaction` + `send_raw_v0_transaction` on
/// the shared RPC client. Tests use a mock `V0Rpc` impl instead.
pub struct ClawV0Rpc {
    rpc: claw_solana_core::rpc::ClawRpcClient,
}

impl ClawV0Rpc {
    pub fn new(rpc: claw_solana_core::rpc::ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl V0Rpc for ClawV0Rpc {
    async fn simulate_v0(&self, tx: &VersionedTransaction) -> V0SimOutcome {
        match self.rpc.simulate_v0_transaction(tx).await {
            Ok((success, error, _cu)) => V0SimOutcome { success, error },
            Err(e) => V0SimOutcome {
                success: false,
                error: Some(format!("rpc: {e}")),
            },
        }
    }

    async fn send_v0(&self, signed_tx_bytes: &[u8]) -> Result<String, String> {
        self.rpc
            .send_raw_v0_transaction(signed_tx_bytes)
            .await
            .map_err(|e| e.to_string())
    }
}

// ── Production ALT fetcher ────────────────────────────────────────────────

/// Production `AltAccountFetcher` backed by `ClawRpcClient::get_account`.
///
/// One RPC round-trip per ALT. The JIT executor calls this only when the
/// Jupiter build response does NOT include a pre-resolved
/// `addresses_by_lookup_table_address` map — which, against the live
/// `api.jup.ag`, is always.
pub struct ClawAltFetcher {
    rpc: claw_solana_core::rpc::ClawRpcClient,
}

impl ClawAltFetcher {
    pub fn new(rpc: claw_solana_core::rpc::ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl AltAccountFetcher for ClawAltFetcher {
    async fn fetch_alt_data(&self, alt_address: &str) -> Result<Vec<u8>, String> {
        let account = self
            .rpc
            .get_account(alt_address, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(account.data)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::jupiter::{
        BlockhashWithMetadata, JupiterAccountMeta, JupiterInstruction,
        SwapBuildResponse, SwapQuoteResponse,
    };
    use crate::integrations::jupiter::SwapMode;
    use crate::orchestrator::adapter::{ExternalWalletBindings, WalletAdapter};
    use crate::orchestrator::external_adapter::ExternalWalletAdapter;
    use claw_types::{
        session::SessionId, solana::SolanaNetwork,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use uuid::Uuid;

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
    const BLOCKHASH: &str = "GHtXQBpokKZYzitZ9bHnDZbUXZwqfsFQ2NUjRmiaV5Hf";
    const WALLET: &str = "CL4w7VNnCpGh3oJ8qfPFR4RjV6R3K4fZ1234567890ab";

    // ── Mock Jupiter client ────────────────────────────────────────────

    struct MockJupiter {
        quote_response: SwapQuoteResponse,
        build_response: Result<SwapBuildResponse, JupiterError>,
        quote_called: Arc<AtomicUsize>,
        build_called: Arc<AtomicUsize>,
    }

    impl MockJupiter {
        fn success() -> (Self, Arc<AtomicUsize>) {
            let build_called = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    quote_response: dummy_quote(),
                    build_response: Ok(dummy_build()),
                    quote_called: Arc::new(AtomicUsize::new(0)),
                    build_called: build_called.clone(),
                },
                build_called,
            )
        }

        fn build_fails(err: JupiterError) -> Self {
            Self {
                quote_response: dummy_quote(),
                build_response: Err(err),
                quote_called: Arc::new(AtomicUsize::new(0)),
                build_called: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl JupiterClient for MockJupiter {
        async fn quote(
            &self,
            _req: &SwapQuoteRequest,
        ) -> Result<SwapQuoteResponse, JupiterError> {
            self.quote_called.fetch_add(1, Ordering::SeqCst);
            Ok(self.quote_response.clone())
        }

        async fn build(
            &self,
            _req: &SwapBuildRequest,
        ) -> Result<SwapBuildResponse, JupiterError> {
            self.build_called.fetch_add(1, Ordering::SeqCst);
            match &self.build_response {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(clone_jup_err(e)),
            }
        }
    }

    fn clone_jup_err(e: &JupiterError) -> JupiterError {
        match e {
            JupiterError::Network(m) => JupiterError::Network(m.clone()),
            JupiterError::Api { status, message } => JupiterError::Api {
                status: *status,
                message: message.clone(),
            },
            JupiterError::Deserialize(m) => JupiterError::Deserialize(m.clone()),
        }
    }

    fn dummy_quote() -> SwapQuoteResponse {
        SwapQuoteResponse {
            input_mint: USDC.to_string(),
            in_amount: "100000".to_string(),
            output_mint: WSOL.to_string(),
            out_amount: "670".to_string(),
            other_amount_threshold: "666".to_string(),
            swap_mode: SwapMode::ExactIn,
            slippage_bps: 50,
            price_impact_pct: None,
            route_plan: vec![],
            context_slot: None,
        }
    }

    fn ix(program: &str, payload: &[u8]) -> JupiterInstruction {
        use base64::Engine;
        JupiterInstruction {
            program_id: program.to_string(),
            accounts: vec![JupiterAccountMeta {
                pubkey: WALLET.to_string(),
                is_signer: true,
                is_writable: true,
            }],
            data: base64::engine::general_purpose::STANDARD.encode(payload),
        }
    }

    fn dummy_build() -> SwapBuildResponse {
        SwapBuildResponse {
            compute_budget_instructions: vec![ix(COMPUTE_BUDGET_PROGRAM, b"CB1")],
            setup_instructions: vec![],
            swap_instruction: ix(JUPITER_PROGRAM, b"SWAP"),
            cleanup_instruction: None,
            other_instructions: vec![],
            address_lookup_table_addresses: vec![],
            addresses_by_lookup_table_address: HashMap::new(),
            blockhash_with_metadata: BlockhashWithMetadata {
                blockhash: BLOCKHASH.to_string(),
                last_valid_block_height: 100_000,
                fetched_at: None,
            },
            token_ledger_instruction: None,
        }
    }

    // ── Mock ALT fetcher ──────────────────────────────────────────────
    //
    // All tests in this file use `dummy_build()` / `build_response_for_payer()`
    // which populate `addresses_by_lookup_table_address` (empty or inline),
    // so the production path never reaches the RPC ALT fetcher. This mock
    // enforces that invariant — if the executor ever tries to fetch, the
    // test panics rather than silently doing the wrong thing.
    struct NeverCalledAltFetcher;

    #[async_trait]
    impl AltAccountFetcher for NeverCalledAltFetcher {
        async fn fetch_alt_data(&self, addr: &str) -> Result<Vec<u8>, String> {
            panic!(
                "NeverCalledAltFetcher was invoked for {addr} — test set up the \
                 pre-resolved ALT map but the executor fell through to RPC anyway"
            );
        }
    }

    fn never_alt() -> Arc<dyn AltAccountFetcher> {
        Arc::new(NeverCalledAltFetcher)
    }

    // ── Mock V0Rpc ─────────────────────────────────────────────────────

    struct MockV0Rpc {
        sim_success: bool,
        sim_error: Option<String>,
        send_result: Result<String, String>,
        simulate_called: Arc<AtomicUsize>,
        send_called: Arc<AtomicUsize>,
        last_sent_bytes: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl MockV0Rpc {
        fn ok(signature: &str) -> Self {
            Self {
                sim_success: true,
                sim_error: None,
                send_result: Ok(signature.to_string()),
                simulate_called: Arc::new(AtomicUsize::new(0)),
                send_called: Arc::new(AtomicUsize::new(0)),
                last_sent_bytes: Arc::new(Mutex::new(None)),
            }
        }

        fn sim_fails(err: &str) -> Self {
            Self {
                sim_success: false,
                sim_error: Some(err.to_string()),
                send_result: Err("unreachable".to_string()),
                simulate_called: Arc::new(AtomicUsize::new(0)),
                send_called: Arc::new(AtomicUsize::new(0)),
                last_sent_bytes: Arc::new(Mutex::new(None)),
            }
        }

        fn send_fails(err: &str) -> Self {
            Self {
                sim_success: true,
                sim_error: None,
                send_result: Err(err.to_string()),
                simulate_called: Arc::new(AtomicUsize::new(0)),
                send_called: Arc::new(AtomicUsize::new(0)),
                last_sent_bytes: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl V0Rpc for MockV0Rpc {
        async fn simulate_v0(&self, _tx: &VersionedTransaction) -> V0SimOutcome {
            self.simulate_called.fetch_add(1, Ordering::SeqCst);
            V0SimOutcome {
                success: self.sim_success,
                error: self.sim_error.clone(),
            }
        }

        async fn send_v0(&self, signed_tx_bytes: &[u8]) -> Result<String, String> {
            self.send_called.fetch_add(1, Ordering::SeqCst);
            *self.last_sent_bytes.lock().unwrap() = Some(signed_tx_bytes.to_vec());
            self.send_result.clone()
        }
    }

    // ── Test keystore + local signer ───────────────────────────────────

    fn local_orchestrator_with_wallet(
        _pubkey_str: &str,
    ) -> (SignatureOrchestrator, String) {
        use claw_wallet_engine::{keystore::SecretKeystore, local::LocalKeypairSigner};
        use solana_sdk::signer::Signer as SdkSigner;

        let kp = solana_sdk::signature::Keypair::new();
        let pubkey_str = kp.pubkey().to_string();
        let keystore = SecretKeystore::new();
        keystore.load_from_bytes(&kp.to_bytes()).unwrap();

        let signer: claw_wallet_engine::signer::SignerRef =
            Arc::new(LocalKeypairSigner::new(kp.pubkey(), keystore.clone()));

        let local_adapter = crate::orchestrator::local_adapter::LocalWalletAdapter::new(
            signer, keystore,
        );

        let orchestrator = SignatureOrchestrator::new(vec![Arc::new(local_adapter)]);
        (orchestrator, pubkey_str)
    }

    struct NoExternalBindings;
    impl ExternalWalletBindings for NoExternalBindings {
        fn is_bound(&self, _session_id: &SessionId, _pubkey: &str) -> bool {
            true // pretend every pubkey is an external wallet
        }
    }

    fn external_only_orchestrator() -> SignatureOrchestrator {
        let adapter = ExternalWalletAdapter::new(Arc::new(NoExternalBindings));
        SignatureOrchestrator::new(vec![Arc::new(adapter)])
    }

    // ── Test fixtures ──────────────────────────────────────────────────

    fn intent_with_wallet(wallet: &str) -> SwapIntent {
        SwapIntent::new(
            SessionId::from(Uuid::new_v4()),
            wallet,
            SolanaNetwork::Devnet,
            USDC,
            WSOL,
            100_000,
            50,
            "prod test swap",
        )
    }

    // Build a Jupiter build response whose ONLY static signer is the payer.
    // Needed for the signing test — we use a fresh keypair in each test run.
    fn build_response_for_payer(payer: &str) -> SwapBuildResponse {
        use base64::Engine;
        SwapBuildResponse {
            compute_budget_instructions: vec![JupiterInstruction {
                program_id: COMPUTE_BUDGET_PROGRAM.to_string(),
                accounts: vec![],
                data: base64::engine::general_purpose::STANDARD.encode(b"CB"),
            }],
            setup_instructions: vec![],
            swap_instruction: JupiterInstruction {
                program_id: JUPITER_PROGRAM.to_string(),
                accounts: vec![JupiterAccountMeta {
                    pubkey: payer.to_string(),
                    is_signer: true,
                    is_writable: true,
                }],
                data: base64::engine::general_purpose::STANDARD.encode(b"SWAP"),
            },
            cleanup_instruction: None,
            other_instructions: vec![],
            address_lookup_table_addresses: vec![],
            addresses_by_lookup_table_address: HashMap::new(),
            blockhash_with_metadata: BlockhashWithMetadata {
                blockhash: BLOCKHASH.to_string(),
                last_valid_block_height: 100_000,
                fetched_at: None,
            },
            token_ledger_instruction: None,
        }
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn local_path_reaches_submitted_end_to_end() {
        let (orchestrator, payer_pubkey) = local_orchestrator_with_wallet(WALLET);

        // Mock Jupiter must return a build where the payer is the static signer.
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let rpc_calls_sim = jupiter.build_called.clone();
        let rpc = Arc::new(MockV0Rpc::ok("ABCDE123_onchain_signature"));
        let rpc_simulate_counter = rpc.simulate_called.clone();
        let rpc_send_counter = rpc.send_called.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(&payer_pubkey);

        let outcome = executor.execute_jit(&intent).await;

        match outcome {
            JitOutcome::Submitted { signature } => {
                assert_eq!(signature, "ABCDE123_onchain_signature");
            }
            other => panic!("expected Submitted, got: {other:?}"),
        }

        // Exactly one /build call, one simulate, one send.
        assert_eq!(rpc_calls_sim.load(Ordering::SeqCst), 1);
        assert_eq!(rpc_simulate_counter.load(Ordering::SeqCst), 1);
        assert_eq!(rpc_send_counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn external_path_reaches_awaiting_wallet_signature() {
        let orchestrator = external_only_orchestrator();
        // Use a valid fresh pubkey so parsing succeeds (but it's not in any keystore).
        use solana_sdk::signer::Signer as SdkSigner;
        let external_pubkey =
            solana_sdk::signature::Keypair::new().pubkey().to_string();
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&external_pubkey)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let build_count = jupiter.build_called.clone();
        let rpc = Arc::new(MockV0Rpc::ok("NEVER_USED"));
        let rpc_send_counter = rpc.send_called.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(&external_pubkey);

        let outcome = executor.execute_jit(&intent).await;

        match outcome {
            JitOutcome::AwaitingWalletSignature { .. } => {}
            other => panic!("expected AwaitingWalletSignature, got: {other:?}"),
        }

        // External path must still have called /build once.
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
        // But MUST NOT have called send_v0 (no signature yet).
        assert_eq!(
            rpc_send_counter.load(Ordering::SeqCst),
            0,
            "external wallet path must NOT submit to RPC"
        );
    }

    #[tokio::test]
    async fn build_api_failure_maps_to_build_failed() {
        let (orchestrator, _) = local_orchestrator_with_wallet(WALLET);
        let jupiter = Arc::new(MockJupiter::build_fails(JupiterError::Api {
            status: 502,
            message: "Jupiter bad gateway".to_string(),
        }));
        let rpc = Arc::new(MockV0Rpc::ok("UNUSED"));
        let rpc_simulate_counter = rpc.simulate_called.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(WALLET);

        let outcome = executor.execute_jit(&intent).await;
        match outcome {
            JitOutcome::BuildFailed(msg) => {
                assert!(msg.contains("bad gateway") || msg.contains("502"));
            }
            other => panic!("expected BuildFailed, got: {other:?}"),
        }

        // simulate MUST NOT run if build failed.
        assert_eq!(rpc_simulate_counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn simulate_failure_maps_to_simulate_failed_not_submitted() {
        let (orchestrator, payer_pubkey) = local_orchestrator_with_wallet(WALLET);
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let rpc = Arc::new(MockV0Rpc::sim_fails("InsufficientFunds"));
        let rpc_send_counter = rpc.send_called.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(&payer_pubkey);

        let outcome = executor.execute_jit(&intent).await;
        match &outcome {
            JitOutcome::SimulateFailed(msg) => {
                assert!(msg.contains("InsufficientFunds"));
            }
            other => panic!("expected SimulateFailed, got: {other:?}"),
        }

        // CRITICAL: simulate failure must NEVER submit to the network.
        assert_eq!(
            rpc_send_counter.load(Ordering::SeqCst),
            0,
            "send_v0 must NOT be called when simulate fails"
        );
    }

    #[tokio::test]
    async fn send_failure_maps_to_sign_failed_not_submitted() {
        let (orchestrator, payer_pubkey) = local_orchestrator_with_wallet(WALLET);
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let rpc = Arc::new(MockV0Rpc::send_fails("rpc timeout"));

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(&payer_pubkey);

        let outcome = executor.execute_jit(&intent).await;
        match outcome {
            JitOutcome::SignFailed(msg) => assert!(msg.contains("rpc timeout")),
            other => panic!("expected SignFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_failure_produces_build_failed_before_orchestrator() {
        // Regression guard: if Jupiter build fails, we must NEVER call the
        // orchestrator (which would be pointless and could mutate state).
        let orchestrator = external_only_orchestrator();
        let jupiter = Arc::new(MockJupiter::build_fails(JupiterError::Network(
            "dns failure".to_string(),
        )));
        let rpc = Arc::new(MockV0Rpc::ok("UNUSED"));

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(WALLET);

        let outcome = executor.execute_jit(&intent).await;
        assert!(matches!(outcome, JitOutcome::BuildFailed(_)));
    }

    #[tokio::test]
    async fn signed_bytes_submitted_to_rpc_are_v0_format() {
        // After local sign, the bytes passed to send_v0 must be V0-format
        // (bincode-serialized VersionedTransaction), not legacy.
        let (orchestrator, payer_pubkey) = local_orchestrator_with_wallet(WALLET);
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let rpc = Arc::new(MockV0Rpc::ok("SIG"));
        let sent_bytes = rpc.last_sent_bytes.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc, never_alt(), orchestrator);
        let intent = intent_with_wallet(&payer_pubkey);

        let _ = executor.execute_jit(&intent).await;

        let bytes = sent_bytes.lock().unwrap().clone().expect("send_v0 called");
        // Deserializing as VersionedTransaction succeeds AND message is V0.
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        use solana_sdk::message::VersionedMessage;
        assert!(matches!(tx.message, VersionedMessage::V0(_)));
        // And it has a real signature (not default all-zeros).
        assert!(!tx.signatures.is_empty());
        assert_ne!(tx.signatures[0], solana_sdk::signature::Signature::default());
    }

    // ── Layer 4: Jupiter external path E2E ────────────────────────────────

    #[tokio::test]
    async fn external_v0_path_complete_returns_signed_bytes_and_send_not_called_by_executor() {
        use solana_sdk::signer::Signer as SdkSigner;

        // Fresh keypair — its pubkey is the payer/wallet for this test.
        let payer = solana_sdk::signature::Keypair::new();
        let payer_pubkey_str = payer.pubkey().to_string();

        // External-only orchestrator — clone it so we can call complete() after execute_jit().
        let orchestrator = external_only_orchestrator();
        let orch_for_test = orchestrator.clone();

        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey_str)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });
        let rpc = Arc::new(MockV0Rpc::ok("NEVER_SUBMITTED_BY_EXECUTOR"));
        let send_called = rpc.send_called.clone();

        let executor = HttpJupiterJitExecutor::new(jupiter, rpc.clone(), never_alt(), orchestrator);
        let intent = intent_with_wallet(&payer_pubkey_str);

        // 1. execute_jit() → AwaitingWalletSignature.
        let outcome = executor.execute_jit(&intent).await;
        let wallet_request_id = match outcome {
            JitOutcome::AwaitingWalletSignature { wallet_signature_request_id } => {
                wallet_signature_request_id
            }
            other => panic!("expected AwaitingWalletSignature, got: {other:?}"),
        };

        // 2. Executor must NOT have called send_v0.
        assert_eq!(
            send_called.load(Ordering::SeqCst),
            0,
            "executor must NOT call send_v0 on external path"
        );

        // 3. Retrieve the pending SignatureRequest from the orchestrator.
        let pending = orch_for_test.pending_for_session(&intent.session_id);
        assert_eq!(pending.len(), 1, "should have exactly one pending request");

        let sig_summary = &pending[0];
        // The request ID in the pending entry must match what execute_jit returned.
        use crate::orchestrator::types::SignatureRequestId;
        let request_id = SignatureRequestId(wallet_request_id);
        assert_eq!(sig_summary.request_id, request_id);

        // 4. Deserialize the finalized V0 tx and sign it with the payer keypair.
        let finalized_vtx: VersionedTransaction =
            bincode::deserialize(&sig_summary.finalized_tx_bytes)
                .expect("finalized_tx_bytes must be valid VersionedTransaction");

        use solana_sdk::message::VersionedMessage;
        assert!(
            matches!(finalized_vtx.message, VersionedMessage::V0(_)),
            "finalized tx must be V0 format"
        );

        let msg_bytes = finalized_vtx.message.serialize();
        let signature = payer.sign_message(&msg_bytes);

        let mut signed_vtx = finalized_vtx;
        signed_vtx.signatures[0] = signature;
        let signed_bytes = bincode::serialize(&signed_vtx).expect("serialize signed");

        // 5. Call orchestrator.complete() with the signed bytes.
        let complete_outcome = orch_for_test
            .complete(&request_id, &signed_bytes)
            .await
            .expect("complete should succeed with valid payer signature");

        let returned_signed_bytes = match complete_outcome {
            SignatureOutcome::Signed { signed_tx_bytes, .. } => signed_tx_bytes,
            other => panic!("expected Signed, got: {other:?}"),
        };

        // 6. The returned signed bytes must deserialize as V0 VersionedTransaction.
        let returned_vtx: VersionedTransaction = bincode::deserialize(&returned_signed_bytes)
            .expect("returned signed_tx_bytes must be valid VersionedTransaction");
        // VersionedMessage is already in scope from the `use` above.
        assert!(
            matches!(returned_vtx.message, VersionedMessage::V0(_)),
            "returned signed tx must be V0 format"
        );
        assert!(
            !returned_vtx.signatures.is_empty(),
            "returned signed tx must have signatures"
        );
        assert_ne!(
            returned_vtx.signatures[0],
            solana_sdk::signature::Signature::default(),
            "returned signed tx must have a real (non-default) signature"
        );

        // 7. Orchestrator did NOT auto-submit — send_called is still 0.
        assert_eq!(
            send_called.load(Ordering::SeqCst),
            0,
            "executor must NOT have submitted to RPC; caller must submit separately"
        );

        // 8. Simulate caller submitting: send_v0 succeeds.
        let submit_result = rpc.send_v0(&returned_signed_bytes).await;
        assert!(submit_result.is_ok(), "caller send_v0 should succeed");
    }

    // ── Smoke test: full external wallet path (tool → approve → sign) ─────────

    fn sign_v0_tx_smoke(
        vtx: &mut solana_sdk::transaction::VersionedTransaction,
        keypair: &solana_sdk::signature::Keypair,
    ) {
        use solana_sdk::signer::Signer as SdkSigner2;
        let msg_bytes = vtx.message.serialize();
        let sig = keypair.sign_message(&msg_bytes);
        vtx.signatures[0] = sig;
    }

    /// Full path smoke test: SubmitJupiterSwapTool (with required_approver_role)
    /// → pending_approval → operator approve → awaiting_wallet_signature →
    /// external wallet signs → orchestrator.complete() returns Signed.
    ///
    /// Uses real HttpJupiterJitExecutor wired with MockJupiter + MockV0Rpc.
    /// Verifies the entire signal chain without hitting the network.
    #[tokio::test]
    async fn smoke_full_external_wallet_path_submit_approve_sign_submit() {
        use claw_state_store::db::Database;
        use claw_state_store::pending_jupiter::PendingJupiterRepository;
        use claw_state_store::audit::AuditRepository;
        use crate::approval_store::ApprovalStore;
        use crate::event_bus::EventBus;
        use crate::integrations::jupiter_park::PendingJupiterParkStore;
        use crate::policy_alerting::AlertDispatcher;
        use claw_observability::metrics::MetricsRegistry;
        use crate::tools::jupiter_swap::SubmitJupiterSwapTool;
        use claw_types::approval::{ApprovalDecision, ApprovalWorkflowState};
        use claw_types::swap::SwapPolicyConfig;
        use claw_types::solana::SolanaNetwork;
        use claw_tool_system::tool::Tool;
        use claw_types::tool::ToolInput;

        // 1. Fresh keypair — this is the payer / wallet for the swap.
        use solana_sdk::signer::Signer as SdkSigner;
        let payer = solana_sdk::signature::Keypair::new();
        let payer_pubkey_str = payer.pubkey().to_string();

        // 2. MockJupiter using build_response_for_payer.
        let jupiter = Arc::new(MockJupiter {
            quote_response: dummy_quote(),
            build_response: Ok(build_response_for_payer(&payer_pubkey_str)),
            quote_called: Arc::new(AtomicUsize::new(0)),
            build_called: Arc::new(AtomicUsize::new(0)),
        });

        // 3. MockV0Rpc — external path must NOT call send_v0.
        let rpc = Arc::new(MockV0Rpc::ok("SMOKE_ONCHAIN_SIG"));
        let send_called = rpc.send_called.clone();

        // 4. External-only orchestrator; clone for inspection after execute_jit.
        let orchestrator = external_only_orchestrator();
        let orch_for_test = orchestrator.clone();

        // 5. Build the HttpJupiterJitExecutor.
        let executor: Arc<dyn crate::integrations::jupiter_jit::JupiterJitExecutor> =
            Arc::new(HttpJupiterJitExecutor::new(
                jupiter,
                rpc,
                never_alt(),
                orchestrator,
            ));

        // 6. In-memory DB + repositories + stores.
        let db = Database::open_in_memory().await.unwrap();
        let pool = db.pool().clone();
        let pending_repo = PendingJupiterRepository::new(pool.clone());
        let audit = AuditRepository::new(pool);
        let approval_store = ApprovalStore::new();
        let event_bus = EventBus::new();
        let alerts = AlertDispatcher::default();
        let metrics = Arc::new(MetricsRegistry::default());
        let park_store = PendingJupiterParkStore::new();

        // 7. Build SubmitJupiterSwapTool with max_input_amount = 1 and
        //    required_approver_role = "treasury" so the intent always routes to
        //    pending_approval (input_amount of 100_000 exceeds the cap of 1).
        let swap_policy = SwapPolicyConfig {
            max_input_amount: Some(1),
            required_approver_role: Some("treasury".to_string()),
            ..Default::default()
        };

        let tool = SubmitJupiterSwapTool::new(
            pending_repo.clone(),
            approval_store.clone(),
            event_bus,
            audit,
            swap_policy,
            SolanaNetwork::Devnet,
            600,
            alerts,
            metrics,
            executor,
            park_store.clone(),
        );

        // 8. Execute the tool — should return pending_approval.
        let session_id = SessionId::from(Uuid::new_v4());
        let tool_input = ToolInput {
            tool_name: "submit_jupiter_swap".to_string(),
            parameters: serde_json::json!({
                "input_mint":    USDC,
                "output_mint":   WSOL,
                "input_amount":  100_000_u64,
                "slippage_bps":  50_u16,
                "wallet_pubkey": payer_pubkey_str,
                "description":   "smoke test swap",
            }),
            session_id: session_id.clone(),
            correlation_id: Uuid::new_v4(),
        };

        let output = tool.execute(tool_input).await.unwrap();
        assert!(output.success, "tool should succeed for pending_approval");
        let data = output.data.unwrap();

        // The tool returns "awaiting_approval" for pending_approval.
        assert_eq!(
            data["status"], "awaiting_approval",
            "expected awaiting_approval, got: {}",
            data["status"]
        );

        let intent_id: Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
        let request_id: Uuid = data["approval_request_id"].as_str().unwrap().parse().unwrap();

        // JIT executor must NOT have been called yet.
        assert_eq!(
            send_called.load(Ordering::SeqCst),
            0,
            "send_v0 must not be called before approval"
        );

        // 10. Drive approval via ApprovalStore::decide + park_store signal.
        //     This mimics what route_approval_outcome does in the daemon.
        let decision = ApprovalDecision::approve_as(request_id, None, "treasury".to_string());
        let (outcome, _) = approval_store.decide(&decision);
        assert!(
            matches!(outcome, claw_types::approval::ApprovalOutcome::Approved),
            "expected Approved outcome"
        );
        // Signal the park store — wakes the resume task.
        park_store.signal(request_id, ApprovalWorkflowState::Approved);

        // 12. Wait for state "awaiting_wallet_signature".
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let stored = pending_repo.get(intent_id).await.unwrap();
            if let Some(s) = &stored {
                if s.state == "awaiting_wallet_signature" {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                let actual = stored.map(|s| s.state).unwrap_or_else(|| "<missing>".to_string());
                panic!("timed out waiting for awaiting_wallet_signature, current: {actual}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // 13. Get pending entry from orchestrator.
        let pending = orch_for_test.pending_for_session(&session_id);
        assert_eq!(pending.len(), 1, "should have exactly one pending request");

        let sig_summary = &pending[0];

        // 14. Deserialize and sign the V0 tx.
        let mut finalized_vtx: VersionedTransaction =
            bincode::deserialize(&sig_summary.finalized_tx_bytes)
                .expect("finalized_tx_bytes must be valid VersionedTransaction");

        use solana_sdk::message::VersionedMessage;
        assert!(
            matches!(finalized_vtx.message, VersionedMessage::V0(_)),
            "finalized tx must be V0"
        );

        sign_v0_tx_smoke(&mut finalized_vtx, &payer);
        let signed_bytes = bincode::serialize(&finalized_vtx).expect("serialize signed");

        // 15. Call orchestrator.complete() with signed bytes.
        let request_id_typed = sig_summary.request_id.clone();
        let complete_outcome = orch_for_test
            .complete(&request_id_typed, &signed_bytes)
            .await
            .expect("complete should succeed with valid payer signature");

        match complete_outcome {
            SignatureOutcome::Signed { .. } => {}
            other => panic!("expected Signed, got: {other:?}"),
        }

        // 16. Executor did NOT call send_v0 — caller submits separately.
        assert_eq!(
            send_called.load(Ordering::SeqCst),
            0,
            "executor must NOT call send_v0; caller submits separately"
        );
    }
}
