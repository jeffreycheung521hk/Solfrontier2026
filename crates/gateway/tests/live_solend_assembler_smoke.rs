//! OPT-IN: Slice 1.5 — formal Solend assembler smoke test against mainnet.
//!
//! Runs the Slice 1 decoder / mapping / gate code paths against real Solend
//! mainnet account bytes to confirm the formal implementation survives real
//! state — not just synthetic fixtures.
//!
//! What this test is, and what it is not
//! -------------------------------------
//! This is a **reality check**, not a new feature. It exercises:
//!
//!   `integrations::solend::raw::decode_obligation`
//!   `integrations::solend::raw::decode_reserve`
//!   `integrations::solend::mapping::map_snapshot`
//!   `lending::gate::check_system_invariants`
//!
//! against real mainnet account bytes, pulled via public RPC `getAccountInfo`.
//! It builds **nothing** downstream of that:
//!
//!   - no refresh transaction
//!   - no deposit transaction
//!   - no Phantom / wallet-signature path
//!   - no daemon wiring
//!   - no broadcast, no signing, no state mutation
//!   - no dependency on `spikes/solend_read/` at runtime — the spike is used
//!     only as a human-readable reference for sample addresses below.
//!
//! Opt-in
//! ------
//! Self-skips unless `CLAW_LIVE_SOLEND_ASSEMBLE=1`. CI does not set this.
//!
//! Env vars
//! --------
//!   `CLAW_LIVE_SOLEND_ASSEMBLE=1`                       (required to run)
//!   `CLAW_LIVE_SOLEND_RPC_URL=<url>`                    (optional; default mainnet-beta)
//!   `CLAW_LIVE_SOLEND_OBLIGATION=<base58>`              (optional; overrides sample)
//!   `CLAW_LIVE_SOLEND_OWNER=<base58>`                   (optional; expected session wallet)
//!
//! The built-in fallback sample is the spike-verified Solend Coin98-pool
//! obligation from `spikes/solend_read/spike-report.md`. If that obligation
//! has drifted (closed / reshaped / malformed), the test prints a clear
//! instruction to set the env vars; it does NOT silently pass.
//!
//! Run
//! ---
//! ```bash
//! CLAW_LIVE_SOLEND_ASSEMBLE=1 \
//!   cargo test -p claw-gateway --test live_solend_assembler_smoke \
//!     -- --nocapture
//! ```

use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use claw_gateway::integrations::solend::{
    extract_publish_freshness,
    mapping::{self, OracleAccountInfo, ReserveInput, SolendAssemblyInputs},
    raw::{self, SOLEND_PROGRAM_ID_BS58},
};
use claw_gateway::lending::{
    check_system_invariants, ChainSlot, FeedPublishFreshness, LendingSnapshot,
    ProtocolTag, StaleMarker,
};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

// ── Fallback sample (spike-verified, see spike-report.md §3) ────────────────
//
// This is a SAMPLE, not production configuration. It is only used when env
// overrides are absent. The obligation is the Solend Coin98-pool target
// exercised by the read-only spike on 2026-04-19 (spike-report.md §3).
const SAMPLE_OBLIGATION_BS58: &str = "6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR";
const SAMPLE_OWNER_BS58: &str = "5YXCR5zWX8Ew1uPQoa2Gr22pm1MvHViQvNerqSBwyw4U";

const DEFAULT_RPC: &str = "https://api.mainnet-beta.solana.com";

// ── Helpers ────────────────────────────────────────────────────────────────

fn skip_if_not_opted_in() -> bool {
    match std::env::var("CLAW_LIVE_SOLEND_ASSEMBLE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_ASSEMBLE=1 to run the opt-in Solend \
                 mainnet assembler smoke test. CI does not set this. This test \
                 reads on-chain state only; it never broadcasts a transaction."
            );
            true
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetSource {
    EnvOverride,
    BuiltInSample,
}

struct Target {
    obligation: Pubkey,
    expected_owner: Pubkey,
    source: TargetSource,
}

fn resolve_target() -> Result<Target, String> {
    let obl_env = std::env::var("CLAW_LIVE_SOLEND_OBLIGATION").ok();
    let owner_env = std::env::var("CLAW_LIVE_SOLEND_OWNER").ok();

    match (obl_env, owner_env) {
        (Some(obl), Some(owner)) => {
            let obligation = Pubkey::from_str(&obl).map_err(|e| {
                format!("CLAW_LIVE_SOLEND_OBLIGATION is not a valid base58 pubkey: {e}")
            })?;
            let expected_owner = Pubkey::from_str(&owner).map_err(|e| {
                format!("CLAW_LIVE_SOLEND_OWNER is not a valid base58 pubkey: {e}")
            })?;
            Ok(Target {
                obligation,
                expected_owner,
                source: TargetSource::EnvOverride,
            })
        }
        (Some(_), None) | (None, Some(_)) => Err(
            "CLAW_LIVE_SOLEND_OBLIGATION and CLAW_LIVE_SOLEND_OWNER must be set \
             together, or neither (to use the built-in sample)."
                .to_string(),
        ),
        (None, None) => {
            // Fallback to the spike-verified sample. This is deliberately NOT
            // loaded from spike files at runtime — the two constants above are
            // copied from `spikes/solend_read/spike-report.md` §3, and the
            // spike directory is NOT a runtime dependency.
            Ok(Target {
                obligation: Pubkey::from_str(SAMPLE_OBLIGATION_BS58).unwrap(),
                expected_owner: Pubkey::from_str(SAMPLE_OWNER_BS58).unwrap(),
                source: TargetSource::BuiltInSample,
            })
        }
    }
}

fn rpc_url() -> String {
    std::env::var("CLAW_LIVE_SOLEND_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string())
}

/// Minimal JSON-RPC `getAccountInfo` with base64 encoding.
/// Returns `(data, owner, context_slot)` or `None` if the account does not exist.
///
/// Kept intentionally minimal and local to this test: production RPC wrapping
/// belongs in `claw-solana-core`, not in a smoke-test driver.
async fn get_account_info(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<(Vec<u8>, Pubkey, u64)>, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [
            pubkey.to_string(),
            { "encoding": "base64", "commitment": "confirmed" }
        ]
    });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("RPC POST failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("RPC returned HTTP {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("RPC response was not JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("RPC error: {err}"));
    }
    let result = v
        .get("result")
        .ok_or_else(|| "RPC response missing `result`".to_string())?;
    let context_slot = result
        .pointer("/context/slot")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| "RPC response missing context.slot".to_string())?;
    let value = result.get("value").cloned().unwrap_or(Value::Null);
    if value.is_null() {
        return Ok(None);
    }
    let owner_str = value
        .get("owner")
        .and_then(|o| o.as_str())
        .ok_or_else(|| "account.value.owner missing".to_string())?;
    let owner = Pubkey::from_str(owner_str)
        .map_err(|e| format!("account.value.owner not a pubkey: {e}"))?;
    let data_arr = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "account.value.data missing or not array".to_string())?;
    let data_b64 = data_arr
        .first()
        .and_then(|d| d.as_str())
        .ok_or_else(|| "account.value.data[0] not a string".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("account.value.data base64 decode failed: {e}"))?;
    Ok(Some((bytes, owner, context_slot)))
}

fn divider(label: &str) {
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  {label}");
    println!("══════════════════════════════════════════════════════════════");
}

struct ReviewPayload {
    target_source: TargetSource,
    obligation: Pubkey,
    expected_owner: Pubkey,
    snapshot_owner: Pubkey,
    deposits_count: usize,
    borrows_count: usize,
    reserves_fetched: usize,
    oracle_feed_count: usize,
    oracle_feeds_known_slot: usize,
    oracle_feeds_unknown: usize,
    obligation_stale: StaleMarker,
    reserves_stale: Vec<StaleMarker>,
    snapshot_observed_slot: ChainSlot,
}

fn print_review(p: &ReviewPayload) {
    divider("Review payload — formal assembler against real mainnet bytes");
    println!(
        "  target_source       : {}",
        match p.target_source {
            TargetSource::EnvOverride => "ENV OVERRIDE (CLAW_LIVE_SOLEND_OBLIGATION/OWNER)",
            TargetSource::BuiltInSample => "BUILT-IN SAMPLE (spike-report.md §3)",
        }
    );
    println!("  obligation pubkey   : {}", p.obligation);
    println!("  expected_owner      : {}", p.expected_owner);
    println!("  snapshot owner      : {}", p.snapshot_owner);
    println!("  deposits count      : {}", p.deposits_count);
    println!("  borrows count       : {}", p.borrows_count);
    println!("  reserves fetched    : {}", p.reserves_fetched);
    println!("  oracle feeds kept   : {}", p.oracle_feed_count);
    println!(
        "  oracle freshness    : KnownSlot={} Unknown={}",
        p.oracle_feeds_known_slot, p.oracle_feeds_unknown
    );
    println!("  obligation stale    : {:?}", p.obligation_stale);
    println!("  reserves stale bits : {:?}", p.reserves_stale);
    println!(
        "  snapshot observed   : slot {}",
        p.snapshot_observed_slot.raw()
    );
    println!();
    println!(
        "  NOTE: `{:?}` on obligation/reserves is the default Solend bare-read",
        StaleMarker::Stale
    );
    println!(
        "        state. This smoke test does NOT issue RefreshObligation /"
    );
    println!(
        "        RefreshReserve — §66 refresh strategy is a future slice."
    );
    divider("END review payload");
}

// ── Test ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_mainnet_assembler_smoke() {
    if skip_if_not_opted_in() {
        return;
    }

    let target = match resolve_target() {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("FAIL: target resolution: {msg}");
            eprintln!(
                "Set BOTH CLAW_LIVE_SOLEND_OBLIGATION and CLAW_LIVE_SOLEND_OWNER, \
                 or neither (to use the built-in sample)."
            );
            panic!("target resolution failed");
        }
    };
    let url = rpc_url();
    let solend_program =
        Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).expect("Solend program id parses");

    divider("STEP 0 — configuration");
    println!("  rpc url             : {url}");
    println!(
        "  target source       : {}",
        match target.source {
            TargetSource::EnvOverride => "env override",
            TargetSource::BuiltInSample => "built-in sample",
        }
    );
    println!("  obligation          : {}", target.obligation);
    println!("  expected_owner      : {}", target.expected_owner);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-smoke")
        .build()
        .expect("reqwest client");

    // ── 1. Fetch obligation ─────────────────────────────────────────────────
    divider("STEP 1 — fetch + decode obligation");
    let (obl_bytes, obl_owner, obl_ctx_slot) =
        match get_account_info(&client, &url, &target.obligation).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                eprintln!(
                    "FAIL: obligation account {} not found on chain. Target source: {:?}.",
                    target.obligation, target.source
                );
                if matches!(target.source, TargetSource::BuiltInSample) {
                    eprintln!(
                        "The built-in sample may have closed or been reshaped \
                         since spike-report.md was written. Set \
                         CLAW_LIVE_SOLEND_OBLIGATION and CLAW_LIVE_SOLEND_OWNER \
                         to a currently-active obligation you control or trust."
                    );
                }
                panic!("obligation account not found");
            }
            Err(e) => {
                eprintln!("FAIL: fetching obligation: {e}");
                panic!("RPC fetch failed: {e}");
            }
        };

    println!("  fetched             : {} bytes", obl_bytes.len());
    println!("  account owner prog  : {obl_owner}");
    println!("  RPC context slot    : {obl_ctx_slot}");

    assert_eq!(
        obl_owner, solend_program,
        "obligation account owner must be the Solend program id"
    );

    let obligation_raw = match raw::decode_obligation(&obl_bytes) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("FAIL: decode_obligation: {e}");
            if matches!(target.source, TargetSource::BuiltInSample) {
                eprintln!(
                    "The built-in sample may have version-drifted. Provide \
                     CLAW_LIVE_SOLEND_OBLIGATION + CLAW_LIVE_SOLEND_OWNER \
                     for a known-good target."
                );
            }
            panic!("decode_obligation failed: {e}");
        }
    };

    println!("  obligation owner    : {}", obligation_raw.owner);
    println!("  obligation market   : {}", obligation_raw.lending_market);
    println!("  obligation stale    : {}", obligation_raw.last_update_stale);
    println!(
        "  last_update.slot    : {}",
        obligation_raw.last_update_slot
    );
    println!("  deposits            : {}", obligation_raw.deposits.len());
    println!("  borrows             : {}", obligation_raw.borrows.len());

    // ── 2. Fetch + decode each referenced reserve ───────────────────────────
    divider("STEP 2 — fetch + decode referenced reserves");
    let mut unique_reserves: Vec<Pubkey> = Vec::new();
    for d in &obligation_raw.deposits {
        if !unique_reserves.contains(&d.deposit_reserve) {
            unique_reserves.push(d.deposit_reserve);
        }
    }
    for b in &obligation_raw.borrows {
        if !unique_reserves.contains(&b.borrow_reserve) {
            unique_reserves.push(b.borrow_reserve);
        }
    }
    println!("  unique reserves     : {}", unique_reserves.len());
    if unique_reserves.is_empty() {
        println!(
            "  NOTE: obligation has no deposits or borrows; this exercises the \
             empty-obligation path (Part 5B §48.6 valid-empty)."
        );
    }

    let mut reserve_inputs: Vec<ReserveInput> = Vec::with_capacity(unique_reserves.len());
    let mut reserves_stale: Vec<StaleMarker> = Vec::with_capacity(unique_reserves.len());
    for res_pk in &unique_reserves {
        let (bytes, owner, ctx_slot) = match get_account_info(&client, &url, res_pk).await {
            Ok(Some(v)) => v,
            Ok(None) => panic!("reserve {res_pk} referenced by obligation is not on chain"),
            Err(e) => panic!("fetching reserve {res_pk}: {e}"),
        };
        assert_eq!(
            owner, solend_program,
            "reserve {res_pk} owner must be the Solend program id"
        );
        let reserve_raw = match raw::decode_reserve(&bytes) {
            Ok(r) => r,
            Err(e) => panic!("decode_reserve({res_pk}) failed: {e}"),
        };
        println!(
            "  reserve {res_pk} — mint {} decimals {} stale {} ctx_slot {ctx_slot}",
            reserve_raw.liquidity_mint,
            reserve_raw.liquidity_mint_decimals,
            reserve_raw.last_update_stale
        );
        reserves_stale.push(if reserve_raw.last_update_stale {
            StaleMarker::Stale
        } else {
            StaleMarker::Fresh
        });
        reserve_inputs.push(ReserveInput {
            pubkey: *res_pk,
            raw: reserve_raw,
            fetched_at_slot: ChainSlot::new(ctx_slot),
        });
    }

    // ── 3. Oracle metadata for each non-sentinel slot ───────────────────────
    divider("STEP 3 — collect non-sentinel oracle metadata");
    let mut oracle_infos: Vec<OracleAccountInfo> = Vec::new();
    for r in &reserve_inputs {
        for oracle_pk in [r.raw.pyth_oracle, r.raw.switchboard_oracle] {
            if raw::is_null_oracle_sentinel(&oracle_pk) {
                println!("  [sentinel] {oracle_pk}  (skip — not fetched)");
                continue;
            }
            if oracle_infos.iter().any(|o| o.pubkey == oracle_pk) {
                continue;
            }
            match get_account_info(&client, &url, &oracle_pk).await {
                Ok(Some((data, owner, ctx_slot))) => {
                    // Slice 2C: call the formal provider decoder. Pyth
                    // Solana Receiver accounts decode to
                    // `FeedPublishFreshness::KnownSlot(posted_slot)`;
                    // Switchboard On-Demand accounts continue to surface
                    // as `Unknown` pending a verified upstream source.
                    let freshness = extract_publish_freshness(&owner, &data);
                    println!(
                        "  [oracle] {oracle_pk}  owner={owner}  size={} ctx_slot={ctx_slot}  freshness={:?}",
                        data.len(),
                        freshness
                    );
                    oracle_infos.push(OracleAccountInfo {
                        pubkey: oracle_pk,
                        owner_program: owner,
                        fetched_at_slot: ChainSlot::new(ctx_slot),
                        publish: freshness,
                    });
                }
                Ok(None) => {
                    println!(
                        "  [oracle] {oracle_pk}  MISSING on chain — non-sentinel but not found"
                    );
                }
                Err(e) => {
                    println!("  [oracle] {oracle_pk}  fetch error: {e}");
                }
            }
        }
    }

    // ── 4. Run formal mapping ───────────────────────────────────────────────
    divider("STEP 4 — formal map_snapshot(...)");
    let inputs = SolendAssemblyInputs {
        session_wallet: target.expected_owner,
        obligation_pubkey: target.obligation,
        obligation_raw: obligation_raw.clone(),
        obligation_fetched_at_slot: ChainSlot::new(obl_ctx_slot),
        reserves: reserve_inputs,
        oracles: oracle_infos,
        snapshot_observed_slot: ChainSlot::new(obl_ctx_slot),
    };
    let snapshot: LendingSnapshot = match mapping::map_snapshot(inputs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL: map_snapshot: {e}");
            panic!(
                "map_snapshot failed — this indicates a real schema mismatch \
                 between the formal mapping and the on-chain shape. Error: {e}"
            );
        }
    };

    assert_eq!(
        snapshot.protocol_tag,
        ProtocolTag::Solend,
        "mapped snapshot must carry ProtocolTag::Solend"
    );
    assert_eq!(
        snapshot.obligation.identifier, target.obligation,
        "snapshot identifier must match fetched pubkey"
    );
    assert_eq!(
        snapshot.obligation.owner, obligation_raw.owner,
        "snapshot owner must match raw-decoded owner"
    );

    // ── 5. System Invariant Gate ───────────────────────────────────────────
    divider("STEP 5 — System Invariant Gate");
    if snapshot.obligation.owner != target.expected_owner {
        eprintln!(
            "On-chain obligation owner {} does not match expected_owner {}.",
            snapshot.obligation.owner, target.expected_owner
        );
        if matches!(target.source, TargetSource::BuiltInSample) {
            eprintln!(
                "The built-in sample obligation appears to have a different \
                 owner than recorded in spike-report.md. This is a real \
                 mainnet-state drift signal, not a code bug. Provide \
                 CLAW_LIVE_SOLEND_OBLIGATION + CLAW_LIVE_SOLEND_OWNER for \
                 a current, known-good target."
            );
        }
        panic!("expected_owner mismatch");
    }
    check_system_invariants(&snapshot, &target.expected_owner, ProtocolTag::Solend)
        .expect("owner + protocol-tag invariants must hold");
    println!("  gate: OK — owner and protocol-tag invariants hold");

    // ── 6. Review payload ──────────────────────────────────────────────────
    let feed_count: usize = snapshot.oracles.iter().map(|s| s.feeds.len()).sum();
    let known_count: usize = snapshot
        .oracles
        .iter()
        .flat_map(|s| s.feeds.iter())
        .filter(|f| matches!(f.publish, FeedPublishFreshness::KnownSlot(_)))
        .count();
    let unknown_count = feed_count - known_count;
    let review = ReviewPayload {
        target_source: target.source,
        obligation: target.obligation,
        expected_owner: target.expected_owner,
        snapshot_owner: snapshot.obligation.owner,
        deposits_count: snapshot.obligation.deposits.len(),
        borrows_count: snapshot.obligation.borrows.len(),
        reserves_fetched: snapshot.reserves.len(),
        oracle_feed_count: feed_count,
        oracle_feeds_known_slot: known_count,
        oracle_feeds_unknown: unknown_count,
        obligation_stale: snapshot.obligation.protocol_native_stale,
        reserves_stale,
        snapshot_observed_slot: snapshot.fetched_at.observed_slot,
    };
    print_review(&review);
}
