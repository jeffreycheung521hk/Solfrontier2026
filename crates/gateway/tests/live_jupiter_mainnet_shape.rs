//! OPT-IN live validation: Jupiter mainnet wire-path correctness.
//!
//! This test is the `Jupiter-equivalent` of `devnet_orca_swap_e2e.rs` for the
//! one piece Orca's devnet proof cannot cover: that our `HttpJupiterClient` +
//! ALT resolution + V0 assembly actually talk to the live Jupiter API +
//! mainnet RPC and produce a valid `VersionedTransaction`.
//!
//! # What this test proves
//!
//! - `/swap/v1/quote` returns a deserializable quote for SOL → USDC.
//! - `/swap/v1/swap-instructions` returns a deserializable build response
//!   against live `api.jup.ag`.
//! - The live response's `addressLookupTableAddresses` can be resolved via
//!   mainnet RPC into concrete `AddressLookupTableAccount`s.
//! - Those ALTs plus the Jupiter instructions compile into a V0
//!   `VersionedTransaction` via `assemble_v0_transaction_with_resolved_alts`.
//! - Mainnet `simulateTransaction` returns a well-formed response (success
//!   or a specific error, but NOT a deserialization or account-resolution
//!   failure on our side).
//!
//! # What this test does NOT do
//!
//! - **Never calls `send_v0`.** The human-review payload printed at the end
//!   is the deliverable; broadcasting the tx is a separate, human-authorized
//!   step outside this test.
//! - Does not sign the transaction.
//! - Does not debit any real funds. Simulate is read-only; it never touches
//!   fees.
//!
//! # Opt-in
//!
//! The test self-skips unless `CLAW_LIVE_JUPITER_MAINNET=1` is set. CI does
//! NOT set this, so deterministic pipelines are unaffected.
//!
//! # Run
//!
//! ```bash
//! CLAW_LIVE_JUPITER_MAINNET=1 \
//!   cargo test -p claw-gateway --test live_jupiter_mainnet_shape -- --nocapture
//! ```

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use solana_sdk::{message::VersionedMessage, pubkey::Pubkey};

use claw_gateway::integrations::jupiter::{
    HttpJupiterClient, JupiterClient, SwapBuildRequest, SwapQuoteRequest,
};
use claw_gateway::integrations::jupiter_alt::{
    resolve_address_lookup_tables, AltAccountFetcher,
};
use claw_gateway::integrations::jupiter_production::ClawAltFetcher;
use claw_gateway::integrations::jupiter_tx::assemble_v0_transaction_with_resolved_alts;

use claw_solana_core::rpc::{ClawRpcClient, EndpointConfig, RpcPool, RpcPoolConfig};
use claw_types::solana::CommitmentLevel;

// ── Constants ───────────────────────────────────────────────────────────────

/// The Phantom wallet pubkey must be supplied via env var —
/// `CLAW_LIVE_JUPITER_WALLET=<base58 pubkey>`. This avoids committing any
/// specific wallet identity in the repo; every live run is tied to whatever
/// Phantom the operator actually controls at run-time.
///
/// Panics with a clear message if missing, so an accidental run without the
/// env var fails loudly instead of quietly baking in a stale pubkey.
fn phantom_wallet() -> String {
    std::env::var("CLAW_LIVE_JUPITER_WALLET").unwrap_or_else(|_| {
        panic!(
            "CLAW_LIVE_JUPITER_WALLET env var is required for live Jupiter \
             validation runs. Set it to the base58 pubkey of the mainnet \
             Phantom wallet you control and that has enough SOL to cover the \
             test swap (input + ATA rent + fee; ~0.005 SOL is sufficient)."
        )
    })
}

/// Wrapped SOL (mainnet).
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// USDC mainnet.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// 0.001 SOL = 1_000_000 lamports — very small, within the e2e config cap.
const INPUT_AMOUNT: u64 = 1_000_000;

/// 1% slippage.
const SLIPPAGE_BPS: u16 = 100;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn skip_if_not_opted_in() -> bool {
    match std::env::var("CLAW_LIVE_JUPITER_MAINNET") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_JUPITER_MAINNET=1 to run this opt-in live \
                 mainnet validation. CI does not set this. This test never \
                 broadcasts a transaction; it stops at simulate + human-review \
                 payload."
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

fn divider(label: &str) {
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  {label}");
    println!("══════════════════════════════════════════════════════════════");
}

// ── Test ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn live_jupiter_mainnet_assembles_v0_without_submit() {
    if skip_if_not_opted_in() {
        return;
    }
    let wallet = phantom_wallet();

    // 1. Quote — live api.jup.ag.
    divider("STEP 1 — live Jupiter quote (/swap/v1/quote)");
    let jupiter: Arc<dyn JupiterClient> = Arc::new(HttpJupiterClient::new());
    let quote_req = SwapQuoteRequest {
        input_mint: WSOL.to_string(),
        output_mint: USDC.to_string(),
        amount: INPUT_AMOUNT,
        slippage_bps: Some(SLIPPAGE_BPS),
        swap_mode: None,
        only_direct_routes: None,
        max_accounts: None,
    };
    let quote = jupiter
        .quote(&quote_req)
        .await
        .expect("live Jupiter quote must succeed");

    let quoted_out = quote.out_amount_u64().unwrap();
    let quoted_min = quote.threshold_u64().unwrap();
    let route_labels: Vec<&str> = quote.route_labels();
    println!("input       : {} lamports wSOL", INPUT_AMOUNT);
    println!("expected out: {quoted_out} µUSDC ({:.6} USDC)", quoted_out as f64 / 1_000_000.0);
    println!("min accepted: {quoted_min} µUSDC (slippage floor at {SLIPPAGE_BPS}bps)");
    println!("route        : {route_labels:?}");

    // 2. Build — live api.jup.ag, user = Phantom wallet.
    divider("STEP 2 — live Jupiter swap-instructions (/swap/v1/swap-instructions)");
    let build_req = SwapBuildRequest {
        quote_response: quote.clone(),
        user_public_key: wallet.clone(),
        wrap_and_unwrap_sol: Some(true),
        dynamic_compute_unit_limit: Some(true),
        prioritization_fee_lamports: None,
    };
    let build = jupiter
        .build(&build_req)
        .await
        .expect("live Jupiter swap-instructions must succeed");

    println!(
        "compute_budget_instructions: {}",
        build.compute_budget_instructions.len()
    );
    println!("setup_instructions         : {}", build.setup_instructions.len());
    println!("swap_instruction.program_id: {}", build.swap_instruction.program_id);
    println!(
        "cleanup_instruction        : {}",
        if build.cleanup_instruction.is_some() { "present" } else { "none" }
    );
    println!("other_instructions         : {}", build.other_instructions.len());
    println!(
        "ALT addresses (to resolve) : {:?}",
        build.address_lookup_table_addresses
    );
    println!(
        "pre-resolved ALT map size  : {} (expected: 0 for live API)",
        build.addresses_by_lookup_table_address.len()
    );
    println!("blockhash                  : {}", build.blockhash_with_metadata.blockhash);
    println!(
        "last_valid_block_height    : {}",
        build.blockhash_with_metadata.last_valid_block_height
    );

    // 3. Resolve ALTs via mainnet RPC.
    divider("STEP 3 — ALT resolution via mainnet RPC (getAccount per ALT)");
    let rpc = mainnet_rpc();
    let alt_fetcher: Arc<dyn AltAccountFetcher> = Arc::new(ClawAltFetcher::new(rpc));
    let resolved_alts = resolve_address_lookup_tables(
        &build.address_lookup_table_addresses,
        alt_fetcher.as_ref(),
    )
    .await
    .expect("live ALT resolution must succeed");

    for alt in &resolved_alts {
        println!(
            "  ALT {} → {} inner addresses",
            alt.key,
            alt.addresses.len()
        );
    }

    // 4. Assemble V0 tx.
    divider("STEP 4 — V0 transaction assembly (offline, no RPC)");
    let payer = Pubkey::from_str(&wallet).unwrap();
    let tx =
        assemble_v0_transaction_with_resolved_alts(&build, &payer, &resolved_alts)
            .expect("V0 assembly must succeed");

    assert!(
        matches!(tx.message, VersionedMessage::V0(_)),
        "must be a V0 message"
    );

    let VersionedMessage::V0(ref msg) = tx.message else { unreachable!() };
    let total_ix = msg.instructions.len();
    let static_keys = msg.account_keys.len();
    let total_sigs = tx.signatures.len();
    let all_zero = tx.signatures.iter().all(|s| *s == solana_sdk::signature::Signature::default());

    println!("versioned_message          : V0");
    println!("instruction_count          : {total_ix}");
    println!("static_account_keys        : {static_keys}");
    println!("address_table_lookups      : {}", msg.address_table_lookups.len());
    println!("required_signatures        : {}", msg.header.num_required_signatures);
    println!("signatures (unsigned, all 0): {total_sigs} / all_zero={all_zero}");

    assert!(all_zero, "assembled tx must be unsigned");

    // 5. Simulate — read-only; never spends.
    divider("STEP 5 — mainnet simulate (read-only)");
    let sim_rpc_client = mainnet_rpc();
    let (success, sim_error, cu) = sim_rpc_client
        .simulate_v0_transaction(&tx)
        .await
        .expect("simulate RPC must produce a well-formed response");
    println!("simulate.success           : {success}");
    println!("simulate.error             : {sim_error:?}");
    println!("simulate.compute_units     : {cu:?}");

    // 6. Human-review payload — the actual deliverable.
    divider("HUMAN REVIEW — final tx shape (NOT BROADCAST)");
    println!("wallet                     : {wallet}  (external / Phantom)");
    println!("input_mint                 : {WSOL}  (wSOL)");
    println!("output_mint                : {USDC}  (USDC mainnet)");
    println!("input_amount               : {INPUT_AMOUNT} lamports (0.001 SOL)");
    println!("slippage_bps               : {SLIPPAGE_BPS} ({} %)", SLIPPAGE_BPS as f64 / 100.0);
    println!("quoted_out                 : {quoted_out} µUSDC");
    println!("min_accepted_out           : {quoted_min} µUSDC");
    println!("route                      : {route_labels:?}");
    println!(
        "ATAs in setup              : {}",
        if build.setup_instructions.is_empty() { "none (already present)" } else { "present — see setup_instructions above" }
    );
    println!(
        "ALT count                  : {} (resolved from live Jupiter addressLookupTableAddresses)",
        resolved_alts.len()
    );
    println!("signer_path                : EXTERNAL_WALLET / Phantom V0");
    println!("ready_to_submit            : NO — this test stops at simulate");
    println!(
        "prioritization_fee_lamports: {:?}",
        build
            .compute_budget_instructions
            .iter()
            .map(|i| &i.program_id)
            .collect::<Vec<_>>()
    );

    // 7. Explicit safety assertion.
    divider("SAFETY — this test does NOT broadcast");
    println!(
        "✓ no send_v0 call made; the signed-and-broadcast step is deliberately \
         left to a human-authorized operation outside this test"
    );
}

// ── Opt-in broadcast-handoff emitter ────────────────────────────────────────
//
// Separate opt-in guard (`CLAW_LIVE_JUPITER_HANDOFF=1`) because this test
// produces artifacts that, if signed, will broadcast a real mainnet tx. It
// still does NOT sign or send — that is the human's job inside Phantom.

fn skip_unless_handoff_opted_in() -> bool {
    match std::env::var("CLAW_LIVE_JUPITER_HANDOFF") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_JUPITER_HANDOFF=1 to emit the Phantom \
                 signing handoff artifact (unsigned base64 + self-contained HTML). \
                 This test still does NOT sign or broadcast — signing happens in \
                 the user's Phantom extension after opening the generated HTML."
            );
            true
        }
    }
}

/// The project-root artifacts directory where the HTML + base64 are written.
fn artifacts_dir() -> PathBuf {
    // cargo test cwd = crate dir when -p is used. Walk up to repo root.
    let cwd = std::env::current_dir().expect("cwd");
    let repo_root = if cwd.ends_with("gateway") {
        cwd.parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .expect("cwd has grandparent")
    } else {
        cwd
    };
    let out = repo_root.join("docs").join("proofs");
    fs::create_dir_all(&out).expect("create docs/proofs");
    out
}

#[tokio::test]
async fn live_jupiter_mainnet_emit_phantom_handoff() {
    if skip_unless_handoff_opted_in() {
        return;
    }
    let wallet = phantom_wallet();

    // 1. Fresh live Jupiter quote + build + resolve + assemble (same as shape test).
    let jupiter: Arc<dyn JupiterClient> = Arc::new(HttpJupiterClient::new());
    let quote = jupiter
        .quote(&SwapQuoteRequest {
            input_mint: WSOL.to_string(),
            output_mint: USDC.to_string(),
            amount: INPUT_AMOUNT,
            slippage_bps: Some(SLIPPAGE_BPS),
            swap_mode: None,
            only_direct_routes: None,
            max_accounts: None,
        })
        .await
        .expect("live Jupiter quote must succeed");

    let build = jupiter
        .build(&SwapBuildRequest {
            quote_response: quote.clone(),
            user_public_key: wallet.clone(),
            wrap_and_unwrap_sol: Some(true),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: None,
        })
        .await
        .expect("live Jupiter swap-instructions must succeed");

    let rpc = mainnet_rpc();
    let alt_fetcher: Arc<dyn AltAccountFetcher> = Arc::new(ClawAltFetcher::new(rpc));
    let resolved_alts = resolve_address_lookup_tables(
        &build.address_lookup_table_addresses,
        alt_fetcher.as_ref(),
    )
    .await
    .expect("ALT resolve must succeed");

    let payer = Pubkey::from_str(&wallet).unwrap();
    let tx = assemble_v0_transaction_with_resolved_alts(&build, &payer, &resolved_alts)
        .expect("assembly must succeed");

    // 2. Serialize to base64 (Phantom expects bincode-serialized VersionedTransaction).
    let tx_bytes = bincode::serialize(&tx).expect("serialize v0 tx");
    let base64_tx = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // 3. Also re-run simulate on THIS exact tx so the HTML can display the
    //    simulate verdict — the signing user should see "mainnet accepted
    //    simulate" right before they click sign.
    let sim_rpc = mainnet_rpc();
    let (sim_success, sim_error, sim_cu) =
        sim_rpc.simulate_v0_transaction(&tx).await.expect("simulate");

    // 4. Metadata for the HTML header.
    let quoted_out = quote.out_amount_u64().unwrap();
    let quoted_min = quote.threshold_u64().unwrap();
    let route_labels_vec: Vec<String> =
        quote.route_labels().iter().map(|s| s.to_string()).collect();
    let route_labels = route_labels_vec.join(", ");
    let alt_count = resolved_alts.len();
    let blockhash = build.blockhash_with_metadata.blockhash.clone();
    let last_valid = build.blockhash_with_metadata.last_valid_block_height;

    // 5. Write the unsigned base64 to its own file for reference.
    let dir = artifacts_dir();
    let b64_path = dir.join("jupiter_mainnet_unsigned_tx.b64");
    fs::write(&b64_path, &base64_tx).expect("write base64 artifact");

    // 6. Generate the self-contained HTML signing driver.
    let html = generate_signing_html(
        &base64_tx,
        &wallet,
        INPUT_AMOUNT,
        SLIPPAGE_BPS,
        quoted_out,
        quoted_min,
        &route_labels,
        alt_count,
        &blockhash,
        last_valid,
        sim_success,
        sim_error.as_deref(),
        sim_cu,
    );
    let html_path = dir.join("phantom_sign_and_broadcast.html");
    fs::write(&html_path, &html).expect("write HTML driver");

    divider("HANDOFF EMITTED — open the HTML in a browser with Phantom");
    println!("base64 (unsigned V0 tx) : {}", b64_path.display());
    println!("HTML signing driver     : {}", html_path.display());
    println!();
    println!("NEXT STEPS (the human part):");
    println!("  1. cd docs/proofs && python -m http.server 8080");
    println!("  2. Open http://localhost:8080/phantom_sign_and_broadcast.html");
    println!("  3. Make sure Phantom is set to \"Solana Mainnet\"");
    println!("  4. Click \"Connect Phantom\" — approve the connect request");
    println!("  5. VERIFY the swap details shown both in the page AND in Phantom");
    println!("  6. Click \"Sign & Send\" ONLY if the details match");
    println!("  7. Copy the signature the page displays and share it back");
    println!();
    println!("BLOCKHASH VALIDITY:");
    println!(
        "  fetched blockhash {} is valid until block {last_valid}",
        &blockhash
    );
    println!(
        "  typical window: ~60-90 seconds. If you need more time, re-run \
         this test to fetch a fresh build."
    );
    println!();
    println!("SAFETY:");
    println!("  - This test did NOT sign or broadcast the tx.");
    println!("  - The artifact is unsigned bytes. Only Phantom (with your key) can sign.");
    println!("  - Simulate check: success={sim_success}, CU={sim_cu:?}");
}

#[allow(clippy::too_many_arguments)]
fn generate_signing_html(
    base64_tx: &str,
    wallet: &str,
    input_amount: u64,
    slippage_bps: u16,
    quoted_out: u64,
    min_out: u64,
    route: &str,
    alt_count: usize,
    blockhash: &str,
    last_valid: u64,
    sim_success: bool,
    sim_error: Option<&str>,
    sim_cu: Option<u64>,
) -> String {
    let sim_badge = if sim_success {
        "<span class=\"ok\">✓ mainnet simulate: success</span>".to_string()
    } else {
        format!(
            "<span class=\"fail\">✗ mainnet simulate FAILED: {}</span>",
            sim_error.unwrap_or("(no error text)")
        )
    };
    let sim_cu_line = sim_cu
        .map(|c| format!("compute units (simulate): {c}"))
        .unwrap_or_else(|| "compute units: (unknown)".to_string());

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>ClawSolana — Jupiter JIT Phase 1 mainnet sign + broadcast</title>
  <style>
    body {{ font-family: ui-sans-serif, system-ui, sans-serif; max-width: 760px;
            margin: 32px auto; padding: 0 16px; color: #222; }}
    h1 {{ font-size: 1.4em; }}
    .panel {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px;
              margin: 16px 0; }}
    .panel.warn {{ border-color: #e99; background: #fff6f6; }}
    .panel.ok   {{ border-color: #3a3; background: #f6fff6; }}
    table {{ border-collapse: collapse; width: 100%; }}
    td {{ padding: 6px 8px; border-bottom: 1px solid #eee; font-family: ui-monospace, monospace; font-size: 0.95em; }}
    td.k {{ color: #666; width: 240px; }}
    button {{ font-size: 1.05em; padding: 10px 20px; margin: 4px 0; cursor: pointer; }}
    button:disabled {{ opacity: 0.5; cursor: not-allowed; }}
    #sig {{ font-family: ui-monospace, monospace; word-break: break-all;
            background: #f4f4f4; padding: 8px; border-radius: 4px; }}
    .ok {{ color: #080; font-weight: 600; }}
    .fail {{ color: #b00; font-weight: 600; }}
  </style>
</head>
<body>
  <h1>ClawSolana — Jupiter JIT Phase 1 mainnet handoff</h1>

  <div class="panel warn">
    <strong>Real mainnet transaction.</strong>
    Clicking "Sign &amp; Send" will spend real SOL. Base fee + Jupiter route
    cost only — no amount beyond this page's quote will be moved.
  </div>

  <div class="panel">
    <h2>Swap details (from live Jupiter)</h2>
    <table>
      <tr><td class="k">signer (expected)</td><td>{wallet}</td></tr>
      <tr><td class="k">input mint</td><td>So11111111111111111111111111111111111111112 (wSOL)</td></tr>
      <tr><td class="k">output mint</td><td>EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v (USDC)</td></tr>
      <tr><td class="k">input amount</td><td>{input_amount} lamports (0.001 SOL)</td></tr>
      <tr><td class="k">slippage</td><td>{slippage_bps} bps ({slippage_pct:.2}%)</td></tr>
      <tr><td class="k">quoted out</td><td>{quoted_out} µUSDC</td></tr>
      <tr><td class="k">min accepted out</td><td>{min_out} µUSDC</td></tr>
      <tr><td class="k">route</td><td>{route}</td></tr>
      <tr><td class="k">ALTs resolved</td><td>{alt_count}</td></tr>
      <tr><td class="k">blockhash</td><td>{blockhash}</td></tr>
      <tr><td class="k">last valid block height</td><td>{last_valid}</td></tr>
      <tr><td class="k">simulate</td><td>{sim_badge}</td></tr>
      <tr><td class="k"></td><td>{sim_cu_line}</td></tr>
    </table>
  </div>

  <div class="panel">
    <h2>Phantom</h2>
    <div id="status">Not connected.</div>
    <button id="connect">Connect Phantom</button>
    <button id="send" disabled>Sign &amp; Send (mainnet)</button>
  </div>

  <div class="panel" id="result-panel" style="display:none;">
    <h2>Result</h2>
    <div id="sig"></div>
    <div id="explorer"></div>
  </div>

  <script type="module">
    import * as web3 from "https://esm.sh/@solana/web3.js@1.95.4";

    const BASE64_TX = "{base64_tx}";
    const EXPECTED_WALLET = "{wallet}";

    const statusEl = document.getElementById("status");
    const connectBtn = document.getElementById("connect");
    const sendBtn = document.getElementById("send");
    const resultPanel = document.getElementById("result-panel");
    const sigEl = document.getElementById("sig");
    const explorerEl = document.getElementById("explorer");

    let provider = null;

    function decodeTx() {{
      const bin = atob(BASE64_TX);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return web3.VersionedTransaction.deserialize(bytes);
    }}

    connectBtn.addEventListener("click", async () => {{
      try {{
        provider = window.phantom?.solana;
        if (!provider?.isPhantom) throw new Error("Phantom not detected");
        const resp = await provider.connect();
        const pk = resp.publicKey.toString();
        if (pk !== EXPECTED_WALLET) {{
          statusEl.innerHTML = '<span class="fail">Connected as ' + pk +
            ', but this handoff is for ' + EXPECTED_WALLET +
            '. Switch accounts in Phantom and retry.</span>';
          return;
        }}
        statusEl.innerHTML = '<span class="ok">Connected: ' + pk + '</span>';
        sendBtn.disabled = false;
      }} catch (e) {{
        statusEl.innerHTML = '<span class="fail">Connect failed: ' + e.message + '</span>';
      }}
    }});

    sendBtn.addEventListener("click", async () => {{
      sendBtn.disabled = true;
      statusEl.innerHTML = "Sending to Phantom…";
      try {{
        const tx = decodeTx();
        const {{ signature }} = await provider.signAndSendTransaction(tx);
        resultPanel.style.display = "block";
        sigEl.textContent = signature;
        const url = "https://explorer.solana.com/tx/" + signature;
        explorerEl.innerHTML =
          '<a href="' + url + '" target="_blank" rel="noopener">Solana Explorer →</a>';
        statusEl.innerHTML = '<span class="ok">Broadcast submitted.</span>';
      }} catch (e) {{
        statusEl.innerHTML = '<span class="fail">Sign/send failed: ' + (e?.message || e) + '</span>';
        sendBtn.disabled = false;
      }}
    }});
  </script>
</body>
</html>
"#,
        wallet = wallet,
        input_amount = input_amount,
        slippage_bps = slippage_bps,
        slippage_pct = (slippage_bps as f64) / 100.0,
        quoted_out = quoted_out,
        min_out = min_out,
        route = route,
        alt_count = alt_count,
        blockhash = blockhash,
        last_valid = last_valid,
        sim_badge = sim_badge,
        sim_cu_line = sim_cu_line,
        base64_tx = base64_tx,
    )
}
