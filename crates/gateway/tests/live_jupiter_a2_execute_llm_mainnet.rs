//! OPT-IN: Phase A2-Execute — LLM-driven live mainnet proof.
//!
//! Extends `live_jupiter_a2_prep_llm_ingress.rs` (which stopped at
//! `awaiting_wallet_signature`) with the real completion + broadcast path
//! from `live_jupiter_a1_execute_daemon_path.rs`.
//!
//! # The exact path this test proves
//!
//! ```text
//!   (HTTP)   POST /sessions/:id/wallet-bind-challenge
//!   (HTML)   Phantom signMessage → POST /sessions/:id/wallet-bind-confirm
//!   (real)   WalletChallengeService verify → ExternalWalletStore.bind_wallet
//!   (HTTP)   POST /sessions/:id/messages   {"text": "<fixed English>"}
//!            → GatewayMessageHandler-equiv → Agent.run(OpenAI gpt-4o-mini)
//!            → LLM selects submit_jupiter_swap
//!            → tool.execute(): wallet_pubkey OMITTED → resolves from
//!              SessionBoundWallet lookup (session-binding invariant)
//!            → SwapPolicyConfig.evaluate → Approved
//!            → spawn JIT: live Jupiter quote/build + mainnet ALT resolve
//!              + V0 assemble + real simulate → Pending
//!            → orchestrator park → state = awaiting_wallet_signature
//!   (HTML)   Phantom signTransaction (NOT signAndSendTransaction)
//!            → POST /sessions/:id/wallet-signatures with signed bytes
//!   (real)   wallet-signatures handler →
//!              SignatureOrchestrator.complete()
//!              → ExternalWalletAdapter::complete_v0()
//!              → ClawRpcClient::send_raw_v0_transaction()
//!              → mainnet finalized
//! ```
//!
//! # Fixed conditions
//!
//! The first A2-Execute proof run MUST use:
//!
//!   - Message: `"Swap 0.001 SOL to USDC with 1% slippage."`
//!   - Pair:    wSOL → USDC (mainnet)
//!   - Amount:  0.001 SOL (1,000,000 lamports)
//!   - Slippage: 100 bps (1%)
//!
//! No deviation until a first mainnet-confirmed tx exists.
//!
//! # Opt-in
//!
//! Self-skips unless:
//!   - `CLAW_LIVE_JUPITER_A2_EXECUTE=1`
//!   - `CLAW_LIVE_JUPITER_WALLET=<Phantom pubkey>` (user's real Phantom)
//!   - OpenAI key reachable (env CLAW_LLM_API_KEY / OPENAI_API_KEY / claw.toml)
//!
//! # Run
//!
//! ```bash
//! $env:CLAW_LIVE_JUPITER_A2_EXECUTE = "1"
//! $env:CLAW_LIVE_JUPITER_WALLET = "<your Phantom mainnet pubkey>"
//! cargo test -p claw-gateway --test live_jupiter_a2_execute_llm_mainnet \
//!   -- --nocapture --test-threads=1
//! ```

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;
use solana_sdk::pubkey::Pubkey;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use claw_agent_runtime::{
    agent::Agent,
    llm::{openai::OpenAiClient, LlmClientRef},
    router::AgentRouter,
};
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
    pending_jupiter::PendingJupiterRepository, tool_traces::ToolTraceRepository,
    wallet_bindings::WalletBindingRepository,
    wallet_challenges::WalletChallengeRepository,
};
use claw_tool_system::{
    dispatch::ToolDispatcher, permissions::CapabilitySet, registry::ToolRegistry,
    tool::Tool,
};
use claw_types::{
    agent::{AgentCommand, AgentResponse, AgentRole},
    approval::{ApprovalDecision, ApprovalOutcome, ApprovalRequest},
    events::GatewayEvent,
    policy::PolicyRule,
    session::SessionId,
    solana::{CommitmentLevel, SolanaNetwork},
    swap::SwapPolicyConfig,
};

// ── Fixed spec — the first A2-Execute proof run must not deviate ────────────
const USER_MESSAGE: &str = "Swap 0.001 SOL to USDC with 1% slippage.";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const MAX_INPUT_CAP_LAMPORTS: u64 = 5_000_000; // 0.005 SOL, A1/A2 policy
const AUTH_TOKEN: &str = "a2-execute-local-only";
const HTTP_PORT: u16 = 7072; // distinct from A1-Execute's 7071

fn a1_policy() -> SwapPolicyConfig {
    SwapPolicyConfig {
        allowed_input_mints: vec![WSOL.to_string(), USDC.to_string()],
        allowed_output_mints: vec![WSOL.to_string(), USDC.to_string()],
        max_input_amount: Some(MAX_INPUT_CAP_LAMPORTS),
        max_slippage_bps: Some(100),
        required_approver_role: Some("treasury".to_string()),
    }
}

/// Locate an OpenAI API key in the same order the daemon does.
fn find_openai_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("CLAW_LLM_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    if let Ok(k) = std::env::var("OPENAI_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    let candidates = [
        cwd.join("claw.toml"),
        cwd.parent().and_then(|p| p.parent()).map(|p| p.join("claw.toml"))?,
    ];
    for path in &candidates {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(parsed) = contents.parse::<toml::Value>() {
                if let Some(k) = parsed
                    .get("llm")
                    .and_then(|l| l.get("api_key"))
                    .and_then(|k| k.as_str())
                {
                    if !k.is_empty() {
                        eprintln!("NOTE: using [llm] api_key from {}", path.display());
                        return Some(k.to_string());
                    }
                }
            }
        }
    }
    None
}

fn skip_unless_opted_in() -> Option<(String, String)> {
    match std::env::var("CLAW_LIVE_JUPITER_A2_EXECUTE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {}
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_JUPITER_A2_EXECUTE=1 + CLAW_LIVE_JUPITER_WALLET to \
                 run this opt-in LLM-ingress live mainnet test."
            );
            return None;
        }
    }
    let wallet = match std::env::var("CLAW_LIVE_JUPITER_WALLET") {
        Ok(w) if !w.is_empty() => w,
        _ => {
            eprintln!("SKIP: CLAW_LIVE_JUPITER_WALLET is required for A2-Execute.");
            return None;
        }
    };
    let Some(api_key) = find_openai_api_key() else {
        eprintln!(
            "SKIP: no OpenAI API key (CLAW_LLM_API_KEY / OPENAI_API_KEY / [llm] api_key in claw.toml)"
        );
        return None;
    };
    Some((api_key, wallet))
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
    ClawRpcClient::new(RpcPool::new(config), CommitmentLevel::Confirmed)
}

// ── AppState stubs ──────────────────────────────────────────────────────────

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

struct BusEvents(tokio::sync::broadcast::Sender<GatewayEvent>);
impl EventSubscriber for BusEvents {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GatewayEvent> {
        self.0.subscribe()
    }
}

struct NoopApproval;
impl ApprovalHandler for NoopApproval {
    fn pending_for_session(&self, _: &SessionId) -> Vec<ApprovalRequest> { vec![] }
    fn all_pending(&self) -> Vec<claw_api::state::PendingApprovalItem> { vec![] }
    fn get_by_id(&self, _: uuid::Uuid) -> Option<claw_api::state::PendingApprovalItem> { None }
    fn session_for_request(&self, _: uuid::Uuid) -> Option<SessionId> { None }
    fn decide(&self, _: ApprovalDecision)
        -> Pin<Box<dyn Future<Output = (ApprovalOutcome, Option<ApprovalRequest>)> + Send + '_>>
    { Box::pin(async { (ApprovalOutcome::NotFound, None) }) }
}

struct NoopAudit;
impl AuditReader for NoopAudit {
    fn list(&self, _: i64, _: i64)
        -> Pin<Box<dyn Future<Output = Result<Vec<AuditRowDto>, String>> + Send + '_>>
    { Box::pin(async { Ok(vec![]) }) }
}

struct NoopPolicy;
impl PolicyReader for NoopPolicy {
    fn rules(&self) -> Vec<PolicyRule> { vec![] }
}

struct NoopWalletDir;
impl WalletDirectory for NoopWalletDir {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<WalletSummaryDto>> + Send + '_>> {
        Box::pin(async { vec![] })
    }
}

// ── Real bind-challenge handler ─────────────────────────────────────────────

struct RealChallengeHandler {
    service: WalletChallengeService,
    store: Arc<ExternalWalletStore>,
}

impl WalletChallengeHandler for RealChallengeHandler {
    fn create_challenge(
        &self, session_id: &SessionId, pubkey: &str,
    ) -> Pin<Box<dyn Future<Output = Result<WalletChallengeInfo, String>> + Send + '_>> {
        let sid = session_id.clone();
        let pk = pubkey.to_string();
        let svc = self.service.clone();
        Box::pin(async move {
            let ch = svc.create_challenge(&sid, &pk).await.map_err(|e| e.to_string())?;
            Ok(WalletChallengeInfo {
                challenge_id: ch.challenge_id, message: ch.message, expires_at: ch.expires_at,
            })
        })
    }
    fn verify_and_bind(
        &self, session_id: &SessionId, challenge_id: &str, pubkey: &str, signature_b64: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let sid = session_id.clone();
        let cid = challenge_id.to_string();
        let pk = pubkey.to_string();
        let sig_b64 = signature_b64.to_string();
        let svc = self.service.clone();
        let store = self.store.clone();
        Box::pin(async move {
            let sig = base64::engine::general_purpose::STANDARD
                .decode(&sig_b64).map_err(|e| format!("invalid base64: {e}"))?;
            let verified = svc.verify_challenge(&sid, &cid, &pk, &sig).await
                .map_err(|e| e.to_string())?;
            store.bind_wallet(&sid, &verified);
            Ok(verified)
        })
    }
}

// ── Message handler (mirrors GatewayMessageHandler core) ────────────────────

struct A2MessageHandler {
    agent: Arc<Agent>,
    agent_router: Arc<AgentRouter>,
    registry: ToolRegistry,
    tool_traces: ToolTraceRepository,
}

impl MessageHandler for A2MessageHandler {
    fn handle<'a>(
        &'a self, session_id: &'a SessionId, command: AgentCommand,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let role = self.agent_router.session_role(session_id).await
                .unwrap_or(AgentRole::Execution);
            let caps = CapabilitySet::for_role(role);
            let dispatcher = ToolDispatcher::with_capabilities(self.registry.clone(), caps);
            self.agent_router.dispatch(session_id, command, &self.agent, dispatcher, &self.tool_traces)
                .await.map_err(|e| e.to_string())
        })
    }
}

// ── Completion handler — real complete_v0 + real send_raw_v0_transaction ────

struct CompletionHandler {
    orchestrator: SignatureOrchestrator,
    rpc_client: ClawRpcClient,
    external_wallet: Arc<ExternalWalletStore>,
    completion: Arc<Mutex<Option<oneshot::Sender<WalletSignatureOutcome>>>>,
}

impl WalletSignatureHandler for CompletionHandler {
    fn submit_signed_tx(
        &self, session_id: &SessionId, request_id: uuid::Uuid, signed_tx_b64: String,
    ) -> Pin<Box<dyn Future<Output = WalletSignatureOutcome> + Send + '_>> {
        let sid = session_id.clone();
        Box::pin(async move { self.submit_inner(&sid, request_id, signed_tx_b64).await })
    }
    fn pending_for_session(&self, session_id: &SessionId) -> Vec<PendingWalletSignatureInfo> {
        self.orchestrator.pending_for_session(session_id).into_iter()
            .map(|s| PendingWalletSignatureInfo {
                request_id: s.request_id.0, transaction_id: s.transaction_id,
                description: s.description, expected_signer: s.expected_signer,
                unsigned_tx_b64: base64::engine::general_purpose::STANDARD.encode(&s.finalized_tx_bytes),
            }).collect()
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
        &self, _session_id: &SessionId, request_id: uuid::Uuid, signed_tx_b64: String,
    ) -> WalletSignatureOutcome {
        let raw = match base64::engine::general_purpose::STANDARD.decode(&signed_tx_b64) {
            Ok(b) => b,
            Err(e) => return WalletSignatureOutcome {
                request_id, accepted: false, signature: None, tx_signature: None,
                submitted: false, error: Some(format!("invalid base64: {e}")), rebuild_required: false,
            },
        };
        let sig_req_id = SignatureRequestId::from(request_id);
        match self.orchestrator.complete(&sig_req_id, &raw).await {
            Ok(claw_gateway::orchestrator::types::SignatureOutcome::Signed {
                signature, signed_tx_bytes, ..
            }) => {
                match self.rpc_client.send_raw_v0_transaction(&signed_tx_bytes).await {
                    Ok(tx_sig) => {
                        let outcome = WalletSignatureOutcome {
                            request_id, accepted: true, signature: Some(signature),
                            tx_signature: Some(tx_sig), submitted: true, error: None,
                            rebuild_required: false,
                        };
                        if let Some(s) = self.completion.lock().take() { let _ = s.send(outcome.clone()); }
                        outcome
                    }
                    Err(e) => {
                        let outcome = WalletSignatureOutcome {
                            request_id, accepted: true, signature: Some(signature),
                            tx_signature: None, submitted: false,
                            error: Some(format!("send_raw_v0_transaction: {e}")),
                            rebuild_required: false,
                        };
                        if let Some(s) = self.completion.lock().take() { let _ = s.send(outcome.clone()); }
                        outcome
                    }
                }
            }
            Ok(claw_gateway::orchestrator::types::SignatureOutcome::Pending { .. }) => WalletSignatureOutcome {
                request_id, accepted: false, signature: None, tx_signature: None, submitted: false,
                error: Some("unexpected Pending from complete()".into()), rebuild_required: false,
            },
            Err(e) => {
                use claw_gateway::orchestrator::types::SignatureError;
                let rebuild = matches!(e, SignatureError::BlockhashModified);
                let outcome = WalletSignatureOutcome {
                    request_id, accepted: false, signature: None, tx_signature: None,
                    submitted: false, error: Some(e.to_string()), rebuild_required: rebuild,
                };
                if let Some(s) = self.completion.lock().take() { let _ = s.send(outcome.clone()); }
                outcome
            }
        }
    }
}

// ── HTML generators ─────────────────────────────────────────────────────────

fn write_bind_html(
    path: &std::path::Path, expected_wallet: &str, challenge_id: &str, message: &str,
    daemon_url: &str, session_id: &SessionId, auth_token: &str,
) {
    let html = format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>A2-Execute — bind Phantom</title>
<style>
body {{ font-family: ui-sans-serif, system-ui, sans-serif; max-width: 720px; margin: 32px auto; padding: 0 16px; color: #222; }}
.panel {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px; margin: 16px 0; }}
button {{ font-size: 1.05em; padding: 10px 20px; cursor: pointer; }}
code {{ word-break: break-all; background:#f4f4f4; padding:2px 4px; border-radius:3px; }}
pre  {{ background:#f4f4f4; padding:12px; border-radius:4px; white-space:pre-wrap; word-break:break-all; }}
</style></head>
<body>
<h1>A2-Execute step 1 / 2 — bind Phantom to session</h1>
<div class="panel">
 <p><b>Wallet expected:</b> <code>{expected_wallet}</code></p>
 <p><b>Message Phantom will sign (plain text, ownership proof, NOT a transaction):</b></p>
 <pre>{message}</pre>
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
      status.innerHTML = '<span style="color:#b00">Connected as ' + p.publicKey.toString() + ', expected ' + EXPECTED + '</span>';
      return;
    }}
    status.textContent = "Signing challenge…";
    const enc = new TextEncoder();
    const {{ signature }} = await p.signMessage(enc.encode(MESSAGE), "utf8");
    let binary = ""; for (const b of signature) binary += String.fromCharCode(b);
    const sigB64 = btoa(binary);
    status.textContent = "POSTing bind-confirm…";
    const r = await fetch(DAEMON + "/sessions/" + SESSION + "/wallet-bind-confirm", {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", "Authorization": "Bearer " + TOKEN }},
      body: JSON.stringify({{ challenge_id: CHALLENGE_ID, pubkey: EXPECTED, signature_b64: sigB64 }}),
    }});
    const body = await r.text();
    if (r.ok) {{ status.innerHTML = '<span style="color:#080">Bound ✓ ' + body + '</span>'; }}
    else {{ status.innerHTML = '<span style="color:#b00">Bind failed (' + r.status + '): ' + body + '</span>'; }}
  }} catch (e) {{ status.innerHTML = '<span style="color:#b00">Error: ' + (e?.message || e) + '</span>'; }}
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

#[allow(clippy::too_many_arguments)]
fn write_sign_v0_html(
    path: &std::path::Path, expected_wallet: &str, base64_tx: &str, request_id: &str,
    daemon_url: &str, session_id: &SessionId, auth_token: &str, user_message: &str,
) {
    let html = format!(
        r##"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>A2-Execute — sign V0 via daemon</title>
<style>
body {{ font-family: ui-sans-serif, system-ui, sans-serif; max-width: 760px; margin: 32px auto; padding: 0 16px; color: #222; }}
.panel {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px; margin: 16px 0; }}
.panel.warn {{ border-color: #e99; background: #fff6f6; }}
table {{ border-collapse: collapse; width: 100%; }}
td {{ padding: 6px 8px; border-bottom: 1px solid #eee; font-family: ui-monospace, monospace; font-size: 0.95em; }}
td.k {{ color:#666; width:220px; }}
button {{ font-size: 1.05em; padding: 10px 20px; cursor: pointer; }}
button:disabled {{ opacity: 0.5; cursor: not-allowed; }}
#result {{ font-family: ui-monospace, monospace; word-break: break-all; background: #f4f4f4; padding: 8px; border-radius: 4px; }}
</style></head>
<body>
<h1>A2-Execute step 2 / 2 — sign V0, daemon broadcasts</h1>
<div class="panel warn">
 <b>Real mainnet swap.</b> Phantom will <code>signTransaction</code> (NOT
 signAndSendTransaction) — signed bytes go to the local daemon, which
 runs <code>complete_v0</code> + <code>send_raw_v0_transaction</code>.
</div>
<div class="panel">
 <table>
  <tr><td class="k">triggered by message</td><td>{user_message}</td></tr>
  <tr><td class="k">signer</td><td>{expected_wallet}</td></tr>
  <tr><td class="k">pair</td><td>wSOL → USDC (mainnet)</td></tr>
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
    if (p.publicKey.toString() !== EXPECTED) throw new Error("Connected as " + p.publicKey.toString() + ", expected " + EXPECTED);
    const bin = atob(BASE64_TX);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const tx = web3.VersionedTransaction.deserialize(bytes);
    status.textContent = "Asking Phantom to sign…";
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
      status.innerHTML = '<span style="color:#b00">Failed: ' + (body.error || '') + '</span>';
    }}
  }} catch (e) {{ status.innerHTML = '<span style="color:#b00">Error: ' + (e?.message || e) + '</span>'; go.disabled = false; }}
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
        user_message = html_escape(user_message),
    );
    std::fs::write(path, html).expect("write v0 sign html");
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

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

fn divider(label: &str) {
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  {label}");
    println!("══════════════════════════════════════════════════════════════");
}

// ── Main test ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a2_execute_llm_mainnet_full_daemon_path() {
    let Some((api_key, target_wallet)) = skip_unless_opted_in() else {
        return;
    };
    Pubkey::from_str(&target_wallet).expect("CLAW_LIVE_JUPITER_WALLET must be a valid base58 pubkey");

    // ── 1. Build daemon components ────────────────────────────────────────
    let db = Database::open_in_memory().await.unwrap();
    let pool = db.pool().clone();

    let pending_jupiter_repo = PendingJupiterRepository::new(pool.clone());
    let audit = AuditRepository::new(pool.clone());
    let challenge_repo = WalletChallengeRepository::new(pool.clone());
    let binding_repo = WalletBindingRepository::new(pool.clone());
    let tool_traces = ToolTraceRepository::new(pool.clone());

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
    // REAL V0 RPC (simulate + send). The user's real Phantom signs with real funds.
    let v0_rpc = Arc::new(ClawV0Rpc::new(rpc_client.clone()));
    let alt_fetcher = Arc::new(ClawAltFetcher::new(rpc_client.clone()));

    let jit_executor: Arc<dyn JupiterJitExecutor> = Arc::new(HttpJupiterJitExecutor::new(
        jupiter_client,
        v0_rpc,
        alt_fetcher,
        orchestrator.clone(),
    ));

    let jupiter_tool = Arc::new(
        SubmitJupiterSwapTool::new(
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
        )
        .with_session_wallet_lookup(
            external_store.clone()
                as Arc<dyn claw_gateway::tools::jupiter_swap::SessionBoundWallet>,
        ),
    ) as Arc<dyn Tool>;

    let registry = ToolRegistry::new().with_tool(jupiter_tool);

    // ── 2. LLM + Agent + AgentRouter ─────────────────────────────────────
    let llm: LlmClientRef = Arc::new(OpenAiClient::new(api_key).with_model("gpt-4o-mini"));
    let agent = Arc::new(Agent::new(llm));
    let agent_router = Arc::new(AgentRouter::new());

    // ── 3. Session + challenge service ───────────────────────────────────
    let session_id = SessionId::from(uuid::Uuid::new_v4());
    let challenge_service = WalletChallengeService::new(challenge_repo);

    agent_router
        .open_session(session_id.clone(), AgentRole::Execution, "a2-execute", None)
        .await;

    // ── 4. Completion oneshot + handler ──────────────────────────────────
    let (completion_tx, completion_rx) = oneshot::channel::<WalletSignatureOutcome>();
    let completion_handler = Arc::new(CompletionHandler {
        orchestrator: orchestrator.clone(),
        rpc_client: rpc_client.clone(),
        external_wallet: external_store.clone(),
        completion: Arc::new(Mutex::new(Some(completion_tx))),
    });

    // ── 5. AppState + router + HTTP listener ─────────────────────────────
    let (tx, _) = tokio::sync::broadcast::channel::<GatewayEvent>(16);
    let msg_handler = Arc::new(A2MessageHandler {
        agent: agent.clone(),
        agent_router: agent_router.clone(),
        registry: registry.clone(),
        tool_traces,
    });

    let state = AppState {
        session_mgr: SessionManagerRef::new(Arc::new(FixedSession(session_id.clone()))),
        message_handler: MessageHandlerRef::new(msg_handler),
        approval: ApprovalHandlerRef::new(Arc::new(NoopApproval)),
        events: EventSubscriberRef::new(Arc::new(BusEvents(tx))),
        wallet_signatures: WalletSignatureHandlerRef::new(completion_handler),
        solend_signatures: None,
        solend_jit_prepare: None,
        solend_withdraw_jit_prepare: None,
        wallet_challenges: WalletChallengeHandlerRef::new(Arc::new(RealChallengeHandler {
            service: challenge_service.clone(),
            store: external_store.clone(),
        })),
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
        chat_execute: None,
    };

    let router = claw_api::create_router(state, HealthRegistry::new());
    let listener = TcpListener::bind(("127.0.0.1", HTTP_PORT))
        .await
        .expect("bind HTTP listener");
    let daemon_url = format!("http://127.0.0.1:{HTTP_PORT}");
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service()).await.unwrap();
    });

    // ── 6. Emit bind HTML ─────────────────────────────────────────────────
    let challenge = challenge_service
        .create_challenge(&session_id, &target_wallet)
        .await
        .expect("create_challenge");
    let dir = proofs_dir();
    let bind_html = dir.join("phantom_a2_bind.html");
    write_bind_html(&bind_html, &target_wallet, &challenge.challenge_id,
        &challenge.message, &daemon_url, &session_id, AUTH_TOKEN);

    divider("A2-EXECUTE STEP 1 — bind Phantom to session");
    println!("session          : {session_id}");
    println!("expected wallet  : {target_wallet}");
    println!("daemon URL       : {daemon_url}");
    println!("bind HTML        : {}", bind_html.display());
    println!();
    println!("NEXT:");
    println!("  1. cd docs/proofs && python -m http.server 8080");
    println!("  2. Open http://localhost:8080/phantom_a2_bind.html");
    println!("  3. Sign the challenge (signMessage, NOT a transaction)");
    println!();
    println!("Waiting up to 5 minutes for bind…");

    let bind_deadline = Instant::now() + Duration::from_secs(300);
    while !external_store.is_external_wallet(&session_id, &target_wallet) {
        if Instant::now() >= bind_deadline {
            server_handle.abort();
            panic!("timed out waiting for bind-confirm");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("✓ wallet bound");

    // ── 7. POST /sessions/:id/messages with the fixed English message ────
    divider("A2-EXECUTE STEP 2 — POST /sessions/:id/messages (LLM ingress)");
    println!("user_message : {USER_MESSAGE:?}");
    println!("model        : gpt-4o-mini");
    println!("sending…");

    // POST via reqwest against the live HTTP server — faithful to the
    // "/sessions/:id/messages must drive the tool" spec.
    let http = reqwest::Client::new();
    let msg_resp = http
        .post(format!("{daemon_url}/sessions/{session_id}/messages"))
        .bearer_auth(AUTH_TOKEN)
        .json(&serde_json::json!({ "text": USER_MESSAGE }))
        .send()
        .await
        .expect("POST /messages must complete");
    let status = msg_resp.status();
    let body_text = msg_resp.text().await.unwrap();
    println!("http status  : {status}");
    println!("http body    : {}", body_text.chars().take(500).collect::<String>());
    assert!(status.is_success(), "/messages failed: {body_text}");

    let agent_resp: serde_json::Value = serde_json::from_str(&body_text)
        .expect("agent response must be JSON");
    let tool_calls = agent_resp.get("tool_calls").and_then(|v| v.as_array())
        .expect("tool_calls array");
    let jupiter_call = tool_calls.iter()
        .find(|c| c.get("tool_name").and_then(|t| t.as_str()) == Some("submit_jupiter_swap"))
        .expect("LLM must route to submit_jupiter_swap");
    println!("LLM → tool   : {}", jupiter_call);

    // ── 8. Wait for park ─────────────────────────────────────────────────
    divider("A2-EXECUTE STEP 3 — JIT → awaiting_wallet_signature");
    let park_deadline = Instant::now() + Duration::from_secs(60);
    let (intent_id, stored_intent) = loop {
        let pending = orchestrator.pending_for_session(&session_id);
        if let Some(p) = pending.first() {
            let id = p.transaction_id;
            if let Some(row) = pending_jupiter_repo.get(id).await.expect("get pending") {
                if row.state == "awaiting_wallet_signature" { break (id, row); }
                if row.state == "jit_build_failed" || row.state == "rejected" {
                    server_handle.abort();
                    panic!("JIT failure `{}`: {:?}", row.state, row.last_error);
                }
            }
        }
        if Instant::now() >= park_deadline {
            server_handle.abort();
            panic!("timed out waiting for awaiting_wallet_signature");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    println!("intent_id    : {intent_id}");
    println!("intent state : {}", stored_intent.state);

    // Invariant: session-bound wallet == tool's resolved wallet
    assert_eq!(
        stored_intent.intent.wallet_pubkey, target_wallet,
        "tool's resolved wallet_pubkey MUST match session-bound Phantom, \
         not an LLM-supplied / hallucinated address"
    );

    let pending = orchestrator.pending_for_session(&session_id);
    let parked = &pending[0];
    let base64_tx = base64::engine::general_purpose::STANDARD.encode(&parked.finalized_tx_bytes);
    let v0_html = dir.join("phantom_a2_sign_v0.html");
    write_sign_v0_html(&v0_html, &target_wallet, &base64_tx,
        &parked.request_id.to_string(), &daemon_url, &session_id, AUTH_TOKEN, USER_MESSAGE);

    divider("A2-EXECUTE STEP 4 — sign V0 + daemon broadcasts");
    println!("request_id     : {}", parked.request_id);
    println!("expected_signer: {}", parked.expected_signer);
    println!("tx bytes       : {}", parked.finalized_tx_bytes.len());
    println!("html file      : {}", v0_html.display());
    println!();
    println!("NEXT:");
    println!("  1. REFRESH http://localhost:8080/phantom_a2_sign_v0.html");
    println!("  2. Click \"Connect Phantom & sign V0\"");
    println!("  3. Phantom pops up — review swap details — approve");
    println!("  4. Daemon runs complete_v0 + send_raw_v0_transaction");
    println!();
    println!("Waiting up to 3 minutes for completion…");

    // ── 9. Wait for completion signal ────────────────────────────────────
    let outcome = tokio::time::timeout(Duration::from_secs(180), completion_rx)
        .await
        .expect("completion timeout")
        .expect("completion oneshot dropped");

    divider("A2-EXECUTE RESULT");
    println!("accepted             : {}", outcome.accepted);
    println!("submitted            : {}", outcome.submitted);
    println!("rebuild_required     : {}", outcome.rebuild_required);
    println!("wallet signature     : {:?}", outcome.signature);
    println!("on-chain tx_signature: {:?}", outcome.tx_signature);
    println!("error                : {:?}", outcome.error);

    assert!(outcome.accepted, "completion must be accepted");
    assert!(outcome.submitted, "daemon must submit to mainnet");
    assert!(!outcome.rebuild_required, "blockhash must not have been modified");
    let tx_sig = outcome.tx_signature.clone().expect("tx_signature must be present");
    println!("\nExplorer: https://explorer.solana.com/tx/{tx_sig}");

    let sig_path = dir.join("a2_execute_tx_signature.txt");
    std::fs::write(&sig_path, &tx_sig).expect("write signature");
    println!("signature artifact   : {}", sig_path.display());

    server_handle.abort();
}
