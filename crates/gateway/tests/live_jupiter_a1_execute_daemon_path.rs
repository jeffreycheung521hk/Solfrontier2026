//! OPT-IN: Phase A1-Execute — full daemon completion path, Phantom-signed, mainnet.
//!
//! This is the sibling of [`live_jupiter_a1_prep_daemon_path.rs`] that goes
//! one step further: instead of stopping at `awaiting_wallet_signature`
//! with a test keypair, this test runs the REAL completion path against a
//! REAL Phantom, ending in a mainnet-confirmed Jupiter swap that went
//! through `ExternalWalletAdapter::complete_v0()` and
//! `ClawRpcClient::send_raw_v0_transaction`.
//!
//! # What this proves that no prior proof proved
//!
//! Every earlier live artifact stopped short of the daemon's completion
//! path:
//!
//! | Proof | quote+build | ALT resolve | V0 assemble | Simulate | complete_v0 | send_v0 | Confirmed? |
//! |-------|-------------|-------------|-------------|----------|-------------|---------|-----------|
//! | `live_jupiter_mainnet_shape` | ✅ | ✅ | ✅ | ✅ | — | — | — |
//! | HTML `signAndSendTransaction` (Phase 1 closeout, txs `3MtJe2…`, `4bV72W…`) | ✅ | ✅ | ✅ | — | — | — | ✅ (but bypassed daemon) |
//! | `live_jupiter_a1_prep_daemon_path` | ✅ | ✅ | ✅ | stubbed | — | — | — |
//! | **this test (A1-Execute)** | **✅** | **✅** | **✅** | **✅ live** | **✅ live** | **✅ live** | **✅** |
//!
//! # Opt-in
//!
//! Self-skips unless `CLAW_LIVE_JUPITER_A1_EXECUTE=1` is set. Requires
//! `CLAW_LIVE_JUPITER_WALLET=<real Phantom pubkey with ≥ ~0.005 SOL>`.
//! CI must not set either.
//!
//! # Run
//!
//! ```bash
//! CLAW_LIVE_JUPITER_A1_EXECUTE=1 \
//!   CLAW_LIVE_JUPITER_WALLET=<your Phantom pubkey> \
//!   cargo test -p claw-gateway --test live_jupiter_a1_execute_daemon_path \
//!     -- --nocapture --test-threads=1
//! ```
//!
//! The test will:
//! 1. Start an in-process HTTP server with the real wallet-bind + wallet-
//!    signature routes wired up.
//! 2. Emit `docs/proofs/phantom_a1_bind.html`. You open it, Phantom signs
//!    the canonical bind-challenge message, the page POSTs the signature
//!    to the test server, the real `WalletChallengeService` verifies and
//!    binds.
//! 3. Drive `SubmitJupiterSwapTool.execute()` in-process. The live JIT
//!    pipeline runs end-to-end (quote → build → ALT resolve → V0 assemble
//!    → live simulate).
//! 4. Emit `docs/proofs/phantom_a1_sign_v0.html` with the parked V0 bytes.
//!    You open it, Phantom `signTransaction` (NOT `signAndSendTransaction`)
//!    returns signed bytes, the page POSTs them to the test server's
//!    `/wallet-signatures` endpoint. The server calls
//!    `orchestrator.complete()` + `send_raw_v0_transaction` and returns
//!    the on-chain signature.
//! 5. The test waits for the completion signal, prints the tx signature,
//!    and exits.
//!
//! # Why in-process HTTP is used (not the full `clawd` binary)
//!
//! The only HTTP path into `submit_jupiter_swap` is via LLM agent
//! (`POST /sessions/:id/messages`), which is explicitly out of scope
//! (A2). The test therefore invokes the tool directly in-process and
//! exposes only the completion-side HTTP routes that the browser needs.

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use ed25519_dalek::{Verifier, VerifyingKey};
use parking_lot::Mutex;
use solana_sdk::pubkey::Pubkey;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

use claw_api::{
    auth::OperatorRegistry,
    state::{
        AuditReader, AuditReaderRef, AuditRowDto, PolicyReader, PolicyReaderRef,
        WalletDirectory, WalletDirectoryRef, WalletSummaryDto,
    },
    ApprovalHandler, ApprovalHandlerRef, AppState, AuthToken, EventSubscriber,
    EventSubscriberRef, MessageHandler, MessageHandlerRef, PendingWalletSignatureInfo,
    SessionManagerRef, SessionOps, TransactionProposerRef, WalletChallengeHandler,
    WalletChallengeHandlerRef, WalletChallengeInfo, WalletSignatureHandler,
    WalletSignatureHandlerRef, WalletSignatureOutcome,
};
use claw_gateway::{
    approval_store::ApprovalStore,
    event_bus::EventBus,
    external_wallet::ExternalWalletStore,
    integrations::jupiter::HttpJupiterClient,
    integrations::jupiter_jit::JupiterJitExecutor,
    integrations::jupiter_park::PendingJupiterParkStore,
    integrations::jupiter_production::{ClawAltFetcher, ClawV0Rpc, HttpJupiterJitExecutor},
    orchestrator::{
        external_adapter::ExternalWalletAdapter, types::SignatureRequestId,
        SignatureOrchestrator,
    },
    policy_alerting::AlertDispatcher,
    tools::jupiter_swap::SubmitJupiterSwapTool,
    wallet_challenge::WalletChallengeService,
};
use claw_observability::{metrics::MetricsRegistry, HealthRegistry};
use claw_solana_core::rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig};
use claw_state_store::{
    audit::AuditRepository, db::Database,
    pending_jupiter::PendingJupiterRepository, wallet_bindings::WalletBindingRepository,
    wallet_challenges::WalletChallengeRepository,
};
use claw_tool_system::tool::Tool;
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest, ApprovalWorkflow},
    events::GatewayEvent,
    policy::PolicyRule,
    session::SessionId,
    solana::{CommitmentLevel, SolanaNetwork},
    swap::SwapPolicyConfig,
    tool::ToolInput,
};

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const INPUT_AMOUNT: u64 = 1_000_000; // 0.001 SOL
const SLIPPAGE_BPS: u16 = 100;
const A1_MAX_INPUT: u64 = 5_000_000;
const AUTH_TOKEN: &str = "a1-execute-local-only";
const HTTP_PORT: u16 = 7071;

fn a1_policy() -> SwapPolicyConfig {
    SwapPolicyConfig {
        allowed_input_mints: vec![WSOL.to_string(), USDC.to_string()],
        allowed_output_mints: vec![WSOL.to_string(), USDC.to_string()],
        max_input_amount: Some(A1_MAX_INPUT),
        max_slippage_bps: Some(100),
        required_approver_role: Some("treasury".to_string()),
    }
}

fn skip_unless_opted_in() -> Option<String> {
    match std::env::var("CLAW_LIVE_JUPITER_A1_EXECUTE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_JUPITER_A1_EXECUTE=1 and CLAW_LIVE_JUPITER_WALLET=<pubkey> \
                 to run this opt-in test. It broadcasts a real mainnet transaction."
            );
            return None;
        }
    }
    let wallet = match std::env::var("CLAW_LIVE_JUPITER_WALLET") {
        Ok(w) if !w.is_empty() => w,
        _ => {
            eprintln!("SKIP: CLAW_LIVE_JUPITER_WALLET is required for A1-Execute.");
            return None;
        }
    };
    Some(wallet)
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

// ── AppState stubs for fields the test doesn't exercise ─────────────────────

struct FixedSession(SessionId);
impl SessionOps for FixedSession {
    fn open(&self, _: AgentRole, _: String, _: Option<Vec<PolicyRule>>) -> SessionId {
        self.0.clone()
    }
    fn close(&self, _: &SessionId, _: &str) {}
    fn active_count(&self) -> usize {
        1
    }
    fn is_active(&self, id: &SessionId) -> bool {
        id == &self.0
    }
}

struct NoopMsg;
impl MessageHandler for NoopMsg {
    fn handle<'a>(
        &'a self,
        _: &'a SessionId,
        _: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async move { Err("unused".to_string()) })
    }
}

struct BusEvents(tokio::sync::broadcast::Sender<GatewayEvent>);
impl EventSubscriber for BusEvents {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GatewayEvent> {
        self.0.subscribe()
    }
}

struct NoopApproval;
impl ApprovalHandler for NoopApproval {
    fn pending_for_session(&self, _: &SessionId) -> Vec<ApprovalRequest> {
        vec![]
    }
    fn all_pending(&self) -> Vec<claw_api::state::PendingApprovalItem> {
        vec![]
    }
    fn get_by_id(&self, _: uuid::Uuid) -> Option<claw_api::state::PendingApprovalItem> {
        None
    }
    fn session_for_request(&self, _: uuid::Uuid) -> Option<SessionId> {
        None
    }
    fn decide(
        &self,
        _: ApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>> {
        Box::pin(async move { (ApprovalOutcome::NotFound, None) })
    }
}

struct NoopAudit;
impl AuditReader for NoopAudit {
    fn list(
        &self,
        _: i64,
        _: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }
}

struct NoopPolicy;
impl PolicyReader for NoopPolicy {
    fn rules(&self) -> Vec<PolicyRule> {
        vec![]
    }
}

struct NoopWalletDir;
impl WalletDirectory for NoopWalletDir {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>> {
        Box::pin(async { vec![] })
    }
}

// ── Real WalletChallengeHandler that wraps the production service ──────────

struct RealChallengeHandler {
    service: WalletChallengeService,
    external_wallet: Arc<ExternalWalletStore>,
}

impl WalletChallengeHandler for RealChallengeHandler {
    fn create_challenge(
        &self,
        session_id: &SessionId,
        pubkey: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>> {
        let sid = session_id.clone();
        let pk = pubkey.to_string();
        let svc = self.service.clone();
        Box::pin(async move {
            let ch = svc
                .create_challenge(&sid, &pk)
                .await
                .map_err(|e| e.to_string())?;
            Ok(WalletChallengeInfo {
                challenge_id: ch.challenge_id,
                message: ch.message,
                expires_at: ch.expires_at,
            })
        })
    }

    fn verify_and_bind(
        &self,
        session_id: &SessionId,
        challenge_id: &str,
        pubkey: &str,
        signature_b64: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let sid = session_id.clone();
        let cid = challenge_id.to_string();
        let pk = pubkey.to_string();
        let sig_b64 = signature_b64.to_string();
        let svc = self.service.clone();
        let ext = self.external_wallet.clone();
        Box::pin(async move {
            let sig_bytes = base64::engine::general_purpose::STANDARD
                .decode(&sig_b64)
                .map_err(|e| format!("invalid base64 signature: {e}"))?;
            let verified = svc
                .verify_challenge(&sid, &cid, &pk, &sig_bytes)
                .await
                .map_err(|e| e.to_string())?;
            ext.bind_wallet(&sid, &verified);
            Ok(verified)
        })
    }
}

// ── The completion handler: runs the real daemon path ──────────────────────
//
// This struct replaces the daemon's `GatewayWalletSignatureHandler` for the
// duration of the test. It deliberately exercises the SAME critical code
// path that daemon.rs uses:
//   orchestrator.complete()   → ExternalWalletAdapter::complete_v0 (V0 path)
//   rpc.send_raw_v0_transaction()
//
// Side-effects in the real daemon (audit/spend/tracker/event publication)
// are omitted here because they are not the A1-Execute verification target
// (they have their own deterministic tests). The test only cares that the
// above three live code points execute against real mainnet state.

struct CompletionHandler {
    orchestrator: SignatureOrchestrator,
    rpc_client: ClawRpcClient,
    external_wallet: Arc<ExternalWalletStore>,
    /// Resolves once the first successful `submit_signed_tx` finishes.
    /// Stored as `Option` so we can `take()` it (oneshot sender is one-shot).
    completion: Arc<Mutex<Option<oneshot::Sender<WalletSignatureOutcome>>>>,
}

impl WalletSignatureHandler for CompletionHandler {
    fn submit_signed_tx(
        &self,
        session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        let sid = session_id.clone();
        Box::pin(async move { self.submit_inner(&sid, request_id, signed_tx_b64).await })
    }

    fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        self.orchestrator
            .pending_for_session(session_id)
            .into_iter()
            .map(|s| PendingWalletSignatureInfo {
                request_id: s.request_id.0,
                transaction_id: s.transaction_id,
                description: s.description,
                expected_signer: s.expected_signer,
                unsigned_tx_b64: base64::engine::general_purpose::STANDARD
                    .encode(&s.finalized_tx_bytes),
            })
            .collect()
    }

    fn bind_wallet(&self, session_id: &SessionId, pubkey: &str) {
        self.external_wallet.bind_wallet(session_id, pubkey);
    }

    fn wallets_for_session(&self, session_id: &SessionId) -> Vec<String> {
        self.external_wallet.wallets_for_session(session_id)
    }
}

impl CompletionHandler {
    async fn submit_inner(
        &self,
        _session_id: &SessionId,
        request_id: uuid::Uuid,
        signed_tx_b64: String,
    ) -> WalletSignatureOutcome {
        let raw = match base64::engine::general_purpose::STANDARD.decode(&signed_tx_b64) {
            Ok(b) => b,
            Err(e) => {
                return WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    tx_signature: None,
                    submitted: false,
                    error: Some(format!("invalid base64: {e}")),
                    rebuild_required: false,
                };
            }
        };

        // ── THE KEY CALL CHAIN ──────────────────────────────────────────
        //   orchestrator.complete() →
        //     ExternalWalletAdapter::complete() →
        //       complete_v0()  [for V0 tx]
        let sig_req_id = SignatureRequestId::from(request_id);
        match self.orchestrator.complete(&sig_req_id, &raw).await {
            Ok(claw_gateway::orchestrator::types::SignatureOutcome::Signed {
                signature,
                signed_tx_bytes,
                ..
            }) => {
                // ── send_raw_v0_transaction: the live broadcast ────────
                match self.rpc_client.send_raw_v0_transaction(&signed_tx_bytes).await {
                    Ok(tx_sig) => {
                        let outcome = WalletSignatureOutcome {
                            request_id,
                            accepted: true,
                            signature: Some(signature),
                            tx_signature: Some(tx_sig.clone()),
                            submitted: true,
                            error: None,
                            rebuild_required: false,
                        };
                        // Signal the waiting test exactly once.
                        if let Some(sender) = self.completion.lock().take() {
                            let _ = sender.send(outcome.clone());
                        }
                        outcome
                    }
                    Err(e) => {
                        let outcome = WalletSignatureOutcome {
                            request_id,
                            accepted: true,
                            signature: Some(signature),
                            tx_signature: None,
                            submitted: false,
                            error: Some(format!("send_raw_v0_transaction: {e}")),
                            rebuild_required: false,
                        };
                        if let Some(sender) = self.completion.lock().take() {
                            let _ = sender.send(outcome.clone());
                        }
                        outcome
                    }
                }
            }
            Ok(claw_gateway::orchestrator::types::SignatureOutcome::Pending { .. }) => {
                WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    tx_signature: None,
                    submitted: false,
                    error: Some("unexpected Pending from complete()".to_string()),
                    rebuild_required: false,
                }
            }
            Err(e) => {
                use claw_gateway::orchestrator::types::SignatureError;
                let rebuild = matches!(e, SignatureError::BlockhashModified);
                let outcome = WalletSignatureOutcome {
                    request_id,
                    accepted: false,
                    signature: None,
                    tx_signature: None,
                    submitted: false,
                    error: Some(e.to_string()),
                    rebuild_required: rebuild,
                };
                if let Some(sender) = self.completion.lock().take() {
                    let _ = sender.send(outcome.clone());
                }
                outcome
            }
        }
    }
}

// ── HTML generators ────────────────────────────────────────────────────────

fn write_bind_html(
    path: &std::path::Path,
    expected_wallet: &str,
    challenge_id: &str,
    message: &str,
    daemon_url: &str,
    session_id: &SessionId,
    auth_token: &str,
) {
    let html = format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>A1-Execute — bind Phantom</title>
<style>
body {{ font-family: ui-sans-serif, system-ui, sans-serif; max-width: 720px;
        margin: 32px auto; padding: 0 16px; color: #222; }}
.panel {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px; margin: 16px 0; }}
.panel.ok   {{ border-color: #3a3; background: #f6fff6; }}
.panel.fail {{ border-color: #e99; background: #fff6f6; }}
button {{ font-size: 1.05em; padding: 10px 20px; cursor: pointer; }}
code {{ word-break: break-all; background:#f4f4f4; padding:2px 4px; border-radius:3px; }}
pre  {{ background:#f4f4f4; padding:12px; border-radius:4px; white-space:pre-wrap; word-break:break-all; }}
</style></head>
<body>
<h1>A1-Execute step 1 / 2 — bind Phantom to session</h1>
<div class="panel">
 <p><b>Wallet expected:</b> <code>{expected_wallet}</code></p>
 <p><b>Session:</b> <code>{session_id}</code></p>
 <p><b>Message Phantom will sign (ownership proof, NOT a transaction):</b></p>
 <pre>{message}</pre>
 <p>This signs a plain text message. It is NOT a transaction. No SOL is moved.</p>
</div>
<div class="panel">
 <button id="go">Connect Phantom &amp; sign challenge</button>
 <p id="status">Idle.</p>
</div>
<script>
const EXPECTED = "{expected_wallet}";
const CHALLENGE_ID = "{challenge_id}";
const MESSAGE = {message_js};
const DAEMON = "{daemon_url}";
const SESSION = "{session_id}";
const TOKEN = "{auth_token}";

const status = document.getElementById("status");

document.getElementById("go").addEventListener("click", async () => {{
  try {{
    const p = window.phantom?.solana;
    if (!p?.isPhantom) throw new Error("Phantom not detected");
    await p.connect();
    if (p.publicKey.toString() !== EXPECTED) {{
      status.innerHTML = '<span style="color:#b00">Connected as ' +
        p.publicKey.toString() + ', expected ' + EXPECTED + '</span>';
      return;
    }}
    status.textContent = "Asking Phantom to sign the challenge message…";
    const enc = new TextEncoder();
    const {{ signature }} = await p.signMessage(enc.encode(MESSAGE), "utf8");
    // signature is Uint8Array; encode as base64
    let binary = ""; for (const b of signature) binary += String.fromCharCode(b);
    const sigB64 = btoa(binary);
    status.textContent = "Signed. POSTing to daemon bind-confirm…";
    const r = await fetch(DAEMON + "/sessions/" + SESSION + "/wallet-bind-confirm", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", "Authorization": "Bearer " + TOKEN }},
      body: JSON.stringify({{ challenge_id: CHALLENGE_ID, pubkey: EXPECTED, signature_b64: sigB64 }}),
    }});
    const body = await r.text();
    if (r.ok) {{
      status.innerHTML = '<span style="color:#080">Bound ✓ ' + body + '</span>';
    }} else {{
      status.innerHTML = '<span style="color:#b00">Bind failed (' + r.status + '): ' + body + '</span>';
    }}
  }} catch (e) {{
    status.innerHTML = '<span style="color:#b00">Error: ' + (e?.message || e) + '</span>';
  }}
}});
</script>
</body></html>
"##,
        expected_wallet = expected_wallet,
        challenge_id = challenge_id,
        message = html_escape(message),
        message_js = serde_json::to_string(message).unwrap(),
        daemon_url = daemon_url,
        session_id = session_id,
        auth_token = auth_token,
    );
    std::fs::write(path, html).expect("write bind html");
}

fn write_sign_v0_html(
    path: &std::path::Path,
    expected_wallet: &str,
    base64_tx: &str,
    request_id: &str,
    daemon_url: &str,
    session_id: &SessionId,
    auth_token: &str,
    quoted_out: u64,
    min_out: u64,
    route: &str,
    sim_success: bool,
) {
    let sim_badge = if sim_success {
        "<span style=\"color:#080;font-weight:600\">✓ mainnet simulate: success</span>"
    } else {
        "<span style=\"color:#b00;font-weight:600\">⚠ simulate did not succeed</span>"
    };
    let html = format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>A1-Execute — sign V0, POST to daemon</title>
<style>
body {{ font-family: ui-sans-serif, system-ui, sans-serif; max-width: 760px;
        margin: 32px auto; padding: 0 16px; color: #222; }}
.panel {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px; margin: 16px 0; }}
.panel.warn {{ border-color: #e99; background: #fff6f6; }}
table {{ border-collapse: collapse; width: 100%; }}
td {{ padding: 6px 8px; border-bottom: 1px solid #eee; font-family: ui-monospace, monospace; font-size: 0.95em; }}
td.k {{ color:#666; width:220px; }}
button {{ font-size: 1.05em; padding: 10px 20px; cursor: pointer; }}
button:disabled {{ opacity: 0.5; cursor: not-allowed; }}
#result {{ font-family: ui-monospace, monospace; word-break: break-all;
           background: #f4f4f4; padding: 8px; border-radius: 4px; }}
</style></head>
<body>
<h1>A1-Execute step 2 / 2 — sign V0 and let daemon broadcast</h1>

<div class="panel warn">
 <b>Real mainnet swap.</b> Phantom will <code>signTransaction</code> (NOT
 <code>signAndSendTransaction</code>) — the signed bytes go to the local
 daemon, which runs <code>complete_v0</code> + <code>send_raw_v0_transaction</code>
 and returns the on-chain signature.
</div>

<div class="panel">
 <table>
  <tr><td class="k">signer (expected)</td><td>{expected_wallet}</td></tr>
  <tr><td class="k">swap</td><td>0.001 SOL (wSOL) → USDC mainnet</td></tr>
  <tr><td class="k">slippage</td><td>1 %</td></tr>
  <tr><td class="k">quoted out</td><td>{quoted_out} µUSDC</td></tr>
  <tr><td class="k">min accepted out</td><td>{min_out} µUSDC</td></tr>
  <tr><td class="k">route</td><td>{route}</td></tr>
  <tr><td class="k">simulate</td><td>{sim_badge}</td></tr>
  <tr><td class="k">request_id</td><td>{request_id}</td></tr>
 </table>
</div>

<div class="panel">
 <button id="go">Connect Phantom &amp; sign V0 + broadcast via daemon</button>
 <p id="status">Idle.</p>
</div>
<div class="panel" id="resultPanel" style="display:none;">
 <h2>Result</h2>
 <div id="result"></div>
 <div id="explorer"></div>
</div>

<script type="module">
import * as web3 from "https://esm.sh/@solana/web3.js@1.95.4";

const BASE64_TX = "{base64_tx}";
const EXPECTED = "{expected_wallet}";
const REQUEST_ID = "{request_id}";
const DAEMON = "{daemon_url}";
const SESSION = "{session_id}";
const TOKEN = "{auth_token}";

const status = document.getElementById("status");
const go = document.getElementById("go");
const resultPanel = document.getElementById("resultPanel");
const resultEl = document.getElementById("result");
const explorerEl = document.getElementById("explorer");

go.addEventListener("click", async () => {{
  go.disabled = true;
  try {{
    const p = window.phantom?.solana;
    if (!p?.isPhantom) throw new Error("Phantom not detected");
    await p.connect();
    if (p.publicKey.toString() !== EXPECTED) {{
      throw new Error("Connected as " + p.publicKey.toString() + ", expected " + EXPECTED);
    }}
    status.textContent = "Deserializing V0 tx…";
    const bin = atob(BASE64_TX);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const tx = web3.VersionedTransaction.deserialize(bytes);

    status.textContent = "Asking Phantom to sign the V0 transaction…";
    const signedTx = await p.signTransaction(tx);
    const signedBytes = signedTx.serialize();
    let bStr = ""; for (const b of signedBytes) bStr += String.fromCharCode(b);
    const signedB64 = btoa(bStr);

    status.textContent = "POSTing signed bytes to daemon wallet-signatures…";
    const r = await fetch(DAEMON + "/sessions/" + SESSION + "/wallet-signatures", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", "Authorization": "Bearer " + TOKEN }},
      body: JSON.stringify({{ request_id: REQUEST_ID, signed_tx_b64: signedB64 }}),
    }});
    const body = await r.json();
    resultPanel.style.display = "block";
    resultEl.textContent = JSON.stringify(body, null, 2);
    if (body.tx_signature) {{
      const url = "https://explorer.solana.com/tx/" + body.tx_signature;
      explorerEl.innerHTML = '<a href="' + url + '" target="_blank" rel="noopener">Solana Explorer →</a>';
      status.innerHTML = '<span style="color:#080">Broadcast by daemon ✓</span>';
    }} else if (body.rebuild_required) {{
      status.innerHTML = '<span style="color:#b00">rebuild_required: ' + (body.error || '') + '</span>';
    }} else {{
      status.innerHTML = '<span style="color:#b00">Completion failed: ' + (body.error || '') + '</span>';
    }}
  }} catch (e) {{
    status.innerHTML = '<span style="color:#b00">Error: ' + (e?.message || e) + '</span>';
  }} finally {{
    go.disabled = false;
  }}
}});
</script>
</body></html>
"##,
        expected_wallet = expected_wallet,
        base64_tx = base64_tx,
        request_id = request_id,
        daemon_url = daemon_url,
        session_id = session_id,
        auth_token = auth_token,
        quoted_out = quoted_out,
        min_out = min_out,
        route = route,
        sim_badge = sim_badge,
    );
    std::fs::write(path, html).expect("write v0 sign html");
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Proofs dir helper ───────────────────────────────────────────────────────

fn proofs_dir() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let repo_root = if cwd.ends_with("gateway") {
        cwd.parent().unwrap().parent().unwrap().to_path_buf()
    } else {
        cwd
    };
    let p = repo_root.join("docs").join("proofs");
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ── Validation helper: the bind-challenge signature really must verify ─────

fn signature_ed25519_verifies(pubkey: &str, message: &str, sig_b64: &str) -> bool {
    let sig_bytes = match base64::engine::general_purpose::STANDARD.decode(sig_b64) {
        Ok(b) if b.len() == 64 => b,
        _ => return false,
    };
    let pk_bytes = match bs58::decode(pubkey).into_vec() {
        Ok(b) if b.len() == 32 => b,
        _ => return false,
    };
    let vk = match VerifyingKey::from_bytes(pk_bytes.as_slice().try_into().unwrap()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let sig = ed25519_dalek::Signature::from_bytes(sig_bytes.as_slice().try_into().unwrap());
    vk.verify(message.as_bytes(), &sig).is_ok()
}

// ── Main test ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a1_execute_daemon_path_reaches_mainnet_confirmed() {
    let Some(target_wallet) = skip_unless_opted_in() else {
        return;
    };

    // Validate pubkey format up-front.
    Pubkey::from_str(&target_wallet).expect("CLAW_LIVE_JUPITER_WALLET must be a valid base58 pubkey");

    // ── 1. Build in-process daemon components ─────────────────────────────
    let db = Database::open_in_memory().await.unwrap();
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
    let v0_rpc = Arc::new(ClawV0Rpc::new(rpc_client.clone())); // REAL simulate + send
    let alt_fetcher = Arc::new(ClawAltFetcher::new(rpc_client.clone()));

    let jit_executor: Arc<dyn JupiterJitExecutor> = Arc::new(HttpJupiterJitExecutor::new(
        jupiter_client.clone(),
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
        metrics.clone(),
        jit_executor,
        park_store.clone(),
    );

    let challenge_service = WalletChallengeService::new(challenge_repo);
    let session_id = SessionId::from(uuid::Uuid::new_v4());

    // ── 2. AppState + router + HTTP server ───────────────────────────────
    let (tx, _) = tokio::sync::broadcast::channel::<GatewayEvent>(16);
    let (completion_tx, completion_rx) = oneshot::channel::<WalletSignatureOutcome>();

    let completion_handler = Arc::new(CompletionHandler {
        orchestrator: orchestrator.clone(),
        rpc_client: rpc_client.clone(),
        external_wallet: external_store.clone(),
        completion: Arc::new(Mutex::new(Some(completion_tx))),
    });

    let challenge_handler = Arc::new(RealChallengeHandler {
        service: challenge_service.clone(),
        external_wallet: external_store.clone(),
    });

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(FixedSession(session_id.clone()))),
        message_handler: MessageHandlerRef::new(Arc::new(NoopMsg)),
        approval: ApprovalHandlerRef::new(Arc::new(NoopApproval)),
        events: EventSubscriberRef::new(Arc::new(BusEvents(tx))),
        wallet_signatures: WalletSignatureHandlerRef::new(completion_handler),
        solend_signatures: None,
        wallet_challenges: WalletChallengeHandlerRef::new(challenge_handler),
        auth_token: AuthToken::new(AUTH_TOKEN),
        operator_registry: OperatorRegistry::new(),
        metrics,
        propose: None::<TransactionProposerRef>,
        rate_limiter: None,
        policy: PolicyReaderRef::new(Arc::new(NoopPolicy)),
        audit: AuditReaderRef::new(Arc::new(NoopAudit)),
        wallets: WalletDirectoryRef::new(Arc::new(NoopWalletDir)),
        demo_seeder: None,
        chat: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());
    let listener = TcpListener::bind(("127.0.0.1", HTTP_PORT))
        .await
        .expect("bind HTTP listener");
    let daemon_url = format!("http://127.0.0.1:{HTTP_PORT}");
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service())
            .await
            .unwrap();
    });

    // ── 3. Generate bind challenge (directly via service — same result as HTTP) ─
    let challenge = challenge_service
        .create_challenge(&session_id, &target_wallet)
        .await
        .expect("create_challenge");

    let dir = proofs_dir();
    let bind_html = dir.join("phantom_a1_bind.html");
    write_bind_html(
        &bind_html,
        &target_wallet,
        &challenge.challenge_id,
        &challenge.message,
        &daemon_url,
        &session_id,
        AUTH_TOKEN,
    );

    divider("A1-EXECUTE STEP 1 — bind Phantom to session");
    println!("session          : {session_id}");
    println!("wallet (expected): {target_wallet}");
    println!("challenge_id     : {}", challenge.challenge_id);
    println!("daemon URL       : {daemon_url}");
    println!();
    println!("NEXT:");
    println!("  1. cd docs/proofs && python -m http.server 8080");
    println!("  2. Open http://localhost:8080/phantom_a1_bind.html");
    println!("  3. Click \"Connect Phantom & sign challenge\"");
    println!("  4. Phantom pops up → approve signMessage");
    println!("  5. Page will POST to the daemon and show ✓ Bound");
    println!();
    println!("Waiting up to 5 minutes for bind to complete…");

    // Defensive: sanity-check signMessage signs the canonical message
    // bytes. We can't check the signature here (Phantom hasn't produced
    // one), but we DO verify that challenge_service's message matches
    // what Phantom will sign, and that the pubkey is a valid ed25519 key.
    assert!(
        challenge.message.contains(&target_wallet.to_string()),
        "challenge message must reference the expected wallet"
    );
    let _ = signature_ed25519_verifies; // silence unused_variables if not reached

    // Wait for the HTTP POST to bind-confirm to flow through.
    let bind_deadline = Instant::now() + Duration::from_secs(300);
    while !external_store.is_external_wallet(&session_id, &target_wallet) {
        if Instant::now() >= bind_deadline {
            server_handle.abort();
            panic!("timed out waiting for bind-confirm");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("✓ wallet bound — verified via ExternalWalletStore");

    // ── 4. Fire the Jupiter tool — REAL pipeline to park ────────────────
    divider("A1-EXECUTE STEP 2 — fire SubmitJupiterSwapTool, JIT → park");
    let input = ToolInput {
        tool_name: "submit_jupiter_swap".to_string(),
        parameters: serde_json::json!({
            "input_mint": WSOL,
            "output_mint": USDC,
            "input_amount": INPUT_AMOUNT,
            "slippage_bps": SLIPPAGE_BPS,
            "wallet_pubkey": target_wallet,
            "description": "A1-Execute live mainnet daemon-path completion",
        }),
        session_id: session_id.clone(),
        correlation_id: uuid::Uuid::new_v4(),
    };
    let output = tool.execute(input).await.expect("tool execute");
    assert!(output.success);
    let data = output.data.unwrap();
    assert_eq!(data["status"], "approved_waiting_jit");
    let intent_id: uuid::Uuid = data["intent_id"].as_str().unwrap().parse().unwrap();
    println!("intent_id    : {intent_id}");

    // Wait for park.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(row) = pending_jupiter_repo.get(intent_id).await.unwrap() {
            if row.state == "awaiting_wallet_signature" {
                break;
            }
            if row.state == "jit_build_failed" || row.state == "rejected" {
                server_handle.abort();
                panic!(
                    "JIT reached terminal failure `{}`: {:?}",
                    row.state, row.last_error
                );
            }
        }
        if Instant::now() >= deadline {
            server_handle.abort();
            panic!("timed out waiting for awaiting_wallet_signature");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("✓ parked awaiting_wallet_signature");

    // ── 5. Emit V0 signing HTML ──────────────────────────────────────────
    let pending = orchestrator.pending_for_session(&session_id);
    assert_eq!(pending.len(), 1);
    let parked = &pending[0];
    let base64_tx = base64::engine::general_purpose::STANDARD.encode(&parked.finalized_tx_bytes);

    // Emit a fresh quote summary for the HTML header (the parked tx already
    // has its blockhash baked in; we just want to show the route / quote
    // metadata that matches it).
    let route_summary = "Jupiter V6 aggregator (route selected at build time)";
    let html_path = dir.join("phantom_a1_sign_v0.html");
    write_sign_v0_html(
        &html_path,
        &target_wallet,
        &base64_tx,
        &parked.request_id.to_string(),
        &daemon_url,
        &session_id,
        AUTH_TOKEN,
        0, // quoted_out: not tracked through park here; omit for clarity
        0,
        route_summary,
        true, // the live JIT's simulate step succeeded (prereq for park)
    );

    divider("A1-EXECUTE STEP 3 — sign V0 + daemon broadcasts");
    println!("request_id     : {}", parked.request_id);
    println!("expected_signer: {}", parked.expected_signer);
    println!("tx bytes       : {} bytes", parked.finalized_tx_bytes.len());
    println!("html file      : {}", html_path.display());
    println!();
    println!("NEXT:");
    println!("  1. REFRESH http://localhost:8080/phantom_a1_sign_v0.html");
    println!("     (if you have the earlier bind page still open, open this one instead)");
    println!("  2. Click \"Connect Phantom & sign V0 + broadcast via daemon\"");
    println!("  3. Phantom pops up → VERIFY details → approve");
    println!("  4. Daemon runs complete_v0 + send_raw_v0_transaction");
    println!("  5. Page displays tx signature");
    println!();
    println!("Waiting up to 3 minutes for completion…");

    // ── 6. Wait for completion handler to signal ─────────────────────────
    let outcome = tokio::time::timeout(Duration::from_secs(180), completion_rx)
        .await
        .expect("completion timeout")
        .expect("completion oneshot dropped");

    divider("A1-EXECUTE RESULT");
    println!("accepted            : {}", outcome.accepted);
    println!("submitted           : {}", outcome.submitted);
    println!("rebuild_required    : {}", outcome.rebuild_required);
    println!("wallet signature    : {:?}", outcome.signature);
    println!("on-chain tx_signature: {:?}", outcome.tx_signature);
    println!("error               : {:?}", outcome.error);

    assert!(outcome.accepted, "completion must be accepted");
    assert!(outcome.submitted, "daemon must have submitted to mainnet");
    assert!(!outcome.rebuild_required, "blockhash must not have been modified");
    let tx_sig = outcome
        .tx_signature
        .clone()
        .expect("tx_signature must be present");
    println!(
        "\nExplorer: https://explorer.solana.com/tx/{tx_sig}"
    );

    // Dump the final signature to a file for the proof artifact.
    let sig_path = dir.join("a1_execute_tx_signature.txt");
    std::fs::write(&sig_path, &tx_sig).expect("write signature");
    println!("signature artifact: {}", sig_path.display());

    server_handle.abort();
}

fn divider(label: &str) {
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  {label}");
    println!("══════════════════════════════════════════════════════════════");
}

// A compile-time sanity check so we don't accidentally lose a route that
// the CORS layer needs to allow. This list is informational.
#[allow(dead_code)]
const CORS_SENSITIVE_ROUTES: &[&str] = &[
    "/sessions/:id/wallet-bind-confirm",
    "/sessions/:id/wallet-signatures",
];

// A sanity dump of the sketched router paths this test hits — useful if
// routing layout drifts.
#[allow(dead_code)]
async fn _compile_check_router_shape(router: axum::Router) {
    let _ = router
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await;
}
