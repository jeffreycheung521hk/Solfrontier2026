//! OPT-IN: Pre-Slice-3 read-only recon against Solend mainnet.
//!
//! Purpose
//! -------
//! Determine whether ANY Solend reserve on mainnet has its oracle feed
//! set configured such that `MaxOracleStalenessMs` would pass under
//! current V1 semantics — i.e., every non-sentinel feed in the reserve's
//! oracle slots decodes cleanly to `FeedPublishFreshness::KnownSlot` via
//! the formal decoders.
//!
//! What this test is
//! -----------------
//! Read-only classification recon. For a bounded set of Solend reserves
//! across a bounded set of lending markets, this test:
//!
//!   1. Fetches each reserve account via public RPC (`getAccountInfo`).
//!   2. Decodes it with the formal `integrations::solend::raw::decode_reserve`.
//!   3. Classifies its oracle configuration (`PythOnly` / `SwitchboardOnly`
//!      / `Dual` / `NoOracle`).
//!   4. For every `PythOnly` candidate, fetches the Pyth account and runs
//!      `integrations::solend::extract_publish_freshness` — the SAME
//!      decoder Slice 2C shipped — to confirm a `KnownSlot` outcome.
//!   5. Prints a conclusion: either a Slice-3-compatible target exists,
//!      or Switchboard On-Demand decoder verification remains the blocker.
//!
//! What this test is NOT
//! ---------------------
//! - Not a transaction path. No signing, no broadcast, no daemon.
//! - Not a refresh. No `RefreshObligation` / `RefreshReserve` ixs.
//! - Not a live deposit. No `Deposit` / `Repay` construction.
//! - Not a production execution path. The file lives under
//!   `crates/gateway/tests/` as an opt-in integration test and is
//!   explicitly temporary pre-Slice-3 recon.
//!
//! Opt-in
//! ------
//! Self-skips unless `CLAW_LIVE_SOLEND_RECON=1`. CI does not set this.
//!
//! Env vars
//! --------
//!   `CLAW_LIVE_SOLEND_RECON=1`                       (required to run)
//!   `CLAW_LIVE_SOLEND_RECON_RPC_URL=<url>`           (optional; default mainnet-beta)
//!   `CLAW_LIVE_SOLEND_RECON_RESERVE_MINT=<base58>`   (optional; narrow to one mint)
//!   `CLAW_LIVE_SOLEND_RECON_OBLIGATION=<base58>`     (optional; baseline obligation check)
//!   `CLAW_LIVE_SOLEND_RECON_OWNER=<base58>`          (optional; paired with obligation)
//!
//! Default baseline (used only when no obligation override is set):
//!
//!   obligation: 6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR
//!   owner     : 5YXCR5zWX8Ew1uPQoa2Gr22pm1MvHViQvNerqSBwyw4U
//!
//! This baseline is expected to remain blocked by Switchboard Unknown;
//! it is printed as comparison context, not as a success criterion.
//!
//! Run
//! ---
//! ```bash
//! CLAW_LIVE_SOLEND_RECON=1 \
//!   cargo test -p claw-gateway --test live_solend_recon -- --nocapture
//! ```

use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use claw_gateway::integrations::solend::{
    extract_publish_freshness,
    raw::{self, SOLEND_NULL_ORACLE_SENTINEL_BS58, SOLEND_PROGRAM_ID_BS58},
};
use claw_gateway::lending::FeedPublishFreshness;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

// ── Bounded scan configuration ─────────────────────────────────────────────

const DEFAULT_RPC: &str = "https://api.mainnet-beta.solana.com";

/// Total reserves fetched across all markets before the scan stops.
const MAX_RESERVES: usize = 30;

/// Reserves fetched per market before moving to the next market.
const MAX_RESERVES_PER_MARKET: usize = 8;

/// Known Solend lending markets (target-discovery inherited from the
/// spike; `docs/lending_policy_vocabulary.md` §30.5). Main pool is
/// intentionally listed last AND capped small because public RPC
/// `getProgramAccounts` scan-aborts on it.
const MARKETS: &[(&str, &str)] = &[
    ("Coin98", "7tiNvRHSjYDfc6usrWnSNPyuN68xQfKs1ZG2oqtR5F46"),
    ("AMM", "Au3S1ZSkGwm1fo7g3WFhkD1rcPoUXj7h5ubsGsUFqbLX"),
    ("NFT", "29yTiqjGdoNiRLMVc7ZoqFpbW3gkmefwMG9SUiMMD4J9"),
    ("Hedge", "AQWuUZyhUQsUNRcw5GqhKSzQZNSNd3jwteS1X1A9C5g5"),
    ("EUROe", "Hs5f8ymzu8TTBMY6te5AkwBztSs48UCoeUJC498GwPm1"),
    ("PumpSwap", "4QYw8FbGBYqRnEWACZCBu1zHpMoYhnAHcqSKZZMv95RK"),
    ("Nazare", "3HGyDbSY5JJRcx1ZXJ2xqxqXJHcKEjBhLmks8th36fQ9"),
];

// ── Baseline target (no overrides) ────────────────────────────────────────

const BASELINE_OBLIGATION_BS58: &str = "6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR";
const BASELINE_OWNER_BS58: &str = "5YXCR5zWX8Ew1uPQoa2Gr22pm1MvHViQvNerqSBwyw4U";

// ── Skip / env helpers ────────────────────────────────────────────────────

fn skip_if_not_opted_in() -> bool {
    match std::env::var("CLAW_LIVE_SOLEND_RECON") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set CLAW_LIVE_SOLEND_RECON=1 to run the opt-in Solend \
                 mainnet recon. CI does not set this. This test reads on-chain \
                 state only; it never broadcasts or signs a transaction."
            );
            true
        }
    }
}

fn rpc_url() -> String {
    std::env::var("CLAW_LIVE_SOLEND_RECON_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string())
}

// ── Minimal JSON-RPC helpers (test-local; not pushed into production) ─────

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("RPC POST failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err("RPC_RATE_LIMIT".to_string());
    }
    if !status.is_success() {
        return Err(format!("RPC returned HTTP {status}"));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("RPC response was not JSON: {e}"))?;
    if let Some(err) = v.get("error") {
        // Embed rate-limit / scan-abort detection in the error string so
        // callers can distinguish.
        let msg = err.to_string();
        if msg.contains("-32005") || msg.to_lowercase().contains("too many") {
            return Err("RPC_RATE_LIMIT".to_string());
        }
        if msg.contains("scan aborted") {
            return Err(format!("RPC_SCAN_ABORTED: {msg}"));
        }
        return Err(format!("RPC error: {msg}"));
    }
    Ok(v)
}

async fn get_account_info(
    client: &reqwest::Client,
    url: &str,
    pubkey: &Pubkey,
) -> Result<Option<(Vec<u8>, Pubkey, u64)>, String> {
    let v = rpc_call(
        client,
        url,
        "getAccountInfo",
        json!([pubkey.to_string(), { "encoding": "base64", "commitment": "confirmed" }]),
    )
    .await?;
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
        .ok_or_else(|| "account.value.data missing".to_string())?;
    let data_b64 = data_arr
        .first()
        .and_then(|d| d.as_str())
        .ok_or_else(|| "account.value.data[0] not a string".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    Ok(Some((bytes, owner, context_slot)))
}

/// Pull the reserves for one lending market. Capped; on rate-limit,
/// returns Err("RPC_RATE_LIMIT") so the caller can decide whether to
/// keep scanning other markets.
async fn get_reserves_for_market(
    client: &reqwest::Client,
    url: &str,
    solend_program: &str,
    market: &str,
    cap: usize,
) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
    let params = json!([
        solend_program,
        {
            "encoding": "base64",
            "filters": [
                { "dataSize": 619 },
                { "memcmp": { "offset": 10, "bytes": market } }
            ],
            "commitment": "confirmed"
        }
    ]);
    let v = rpc_call(client, url, "getProgramAccounts", params).await?;
    let arr = v
        .get("result")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "getProgramAccounts result not array".to_string())?;
    let mut out = Vec::with_capacity(cap.min(arr.len()));
    for item in arr.iter().take(cap) {
        let pk = item
            .get("pubkey")
            .and_then(|p| p.as_str())
            .ok_or_else(|| "reserve pubkey missing".to_string())?;
        let data_b64 = item
            .pointer("/account/data/0")
            .and_then(|d| d.as_str())
            .ok_or_else(|| "reserve data[0] missing".to_string())?;
        let pk = Pubkey::from_str(pk).map_err(|e| format!("bad pubkey: {e}"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| format!("base64 decode: {e}"))?;
        out.push((pk, bytes));
    }
    Ok(out)
}

// ── Classification ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum OracleSlotClass {
    /// The reserve slot itself was set to the `nu11…` sentinel —
    /// excluded from the feed set at assembly time.
    Sentinel,
    Pyth,
    Switchboard,
    UnknownOwner,
    NotFetched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReserveOracleShape {
    /// Pyth slot real, Switchboard slot sentinel.
    PythOnly,
    /// Switchboard slot real, Pyth slot sentinel.
    SwitchboardOnly,
    /// Both slots real.
    DualOracle,
    /// Both slots sentinel.
    NoOracle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum TargetStatus {
    /// Every non-sentinel feed decodes to `KnownSlot`.
    CandidatePassableForOracleGate,
    /// A Switchboard slot is real and stays `Unknown` under current
    /// decoder evidence. (Surfaced by future recon variants; this recon
    /// only attempts PythOnly candidates and never fetches real
    /// Switchboard slots.)
    BlockedBySwitchboardUnknown,
    /// A non-sentinel oracle slot could not be fetched / is missing.
    BlockedByMissingFeed,
    /// Some other read-time condition that this recon surfaces but does
    /// not classify further.
    BlockedByOtherReason,
}

// ── Main test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_solend_recon() {
    if skip_if_not_opted_in() {
        return;
    }

    let url = rpc_url();
    let solend_program_str = SOLEND_PROGRAM_ID_BS58;
    let solend_program = Pubkey::from_str(solend_program_str).unwrap();
    let sentinel = Pubkey::from_str(SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap();

    let reserve_mint_override = std::env::var("CLAW_LIVE_SOLEND_RECON_RESERVE_MINT")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok());
    let obligation_override = std::env::var("CLAW_LIVE_SOLEND_RECON_OBLIGATION")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok());
    let owner_override = std::env::var("CLAW_LIVE_SOLEND_RECON_OWNER")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok());

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  STEP 0 — configuration");
    println!("══════════════════════════════════════════════════════════════");
    println!("  rpc url             : {url}");
    println!("  scan strategy       : per-market getProgramAccounts (dataSize=619 + lending_market memcmp)");
    println!("  markets covered     : {} (Main pool excluded — public RPC scan-aborts)", MARKETS.len());
    println!("  max reserves total  : {MAX_RESERVES}");
    println!("  max reserves/market : {MAX_RESERVES_PER_MARKET}");
    println!(
        "  reserve-mint ovrde  : {}",
        reserve_mint_override
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    println!(
        "  obligation override : {}",
        obligation_override
            .map(|p| p.to_string())
            .unwrap_or_else(|| format!("(baseline {})", BASELINE_OBLIGATION_BS58))
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("claw-gateway/live-solend-recon")
        .build()
        .expect("reqwest client");

    // ── STEP 1 — enumerate + classify reserves ──────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  STEP 1 — enumerate + classify reserves");
    println!("══════════════════════════════════════════════════════════════");

    #[derive(Debug)]
    struct ReserveRecord {
        market_name: &'static str,
        reserve_pubkey: Pubkey,
        mint: Pubkey,
        decimals: u8,
        pyth_slot: Pubkey,
        switchboard_slot: Pubkey,
        shape: ReserveOracleShape,
    }

    let mut records: Vec<ReserveRecord> = Vec::new();
    let mut scan_terminated_by: &'static str = "completion";

    'market_loop: for (market_name, market_pk) in MARKETS {
        if records.len() >= MAX_RESERVES {
            scan_terminated_by = "reserve cap reached";
            break;
        }
        let fetched = match get_reserves_for_market(
            &client,
            &url,
            solend_program_str,
            market_pk,
            MAX_RESERVES_PER_MARKET,
        )
        .await
        {
            Ok(v) => v,
            Err(e) if e == "RPC_RATE_LIMIT" => {
                scan_terminated_by = "RPC rate-limit";
                println!("  [{market_name}]  RPC rate-limit hit — stopping scan");
                break 'market_loop;
            }
            Err(e) => {
                println!("  [{market_name}]  market skipped: {e}");
                continue;
            }
        };
        println!(
            "  [{market_name:<9}] pool={market_pk}  returned={} (capped at {MAX_RESERVES_PER_MARKET})",
            fetched.len()
        );
        for (res_pk, bytes) in fetched {
            if records.len() >= MAX_RESERVES {
                scan_terminated_by = "reserve cap reached";
                break 'market_loop;
            }
            let reserve_raw = match raw::decode_reserve(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    println!("    skip {res_pk}: decode_reserve failed: {e}");
                    continue;
                }
            };
            let shape = match (
                reserve_raw.pyth_oracle == sentinel,
                reserve_raw.switchboard_oracle == sentinel,
            ) {
                (false, true) => ReserveOracleShape::PythOnly,
                (true, false) => ReserveOracleShape::SwitchboardOnly,
                (false, false) => ReserveOracleShape::DualOracle,
                (true, true) => ReserveOracleShape::NoOracle,
            };
            records.push(ReserveRecord {
                market_name,
                reserve_pubkey: res_pk,
                mint: reserve_raw.liquidity_mint,
                decimals: reserve_raw.liquidity_mint_decimals,
                pyth_slot: reserve_raw.pyth_oracle,
                switchboard_slot: reserve_raw.switchboard_oracle,
                shape,
            });
        }
    }

    // Counts
    let mut n_pyth_only = 0usize;
    let mut n_swb_only = 0usize;
    let mut n_dual = 0usize;
    let mut n_none = 0usize;
    for r in &records {
        match r.shape {
            ReserveOracleShape::PythOnly => n_pyth_only += 1,
            ReserveOracleShape::SwitchboardOnly => n_swb_only += 1,
            ReserveOracleShape::DualOracle => n_dual += 1,
            ReserveOracleShape::NoOracle => n_none += 1,
        }
    }

    println!();
    println!("  classified reserves : {}", records.len());
    println!("    PythOnly          : {n_pyth_only}");
    println!("    SwitchboardOnly   : {n_swb_only}");
    println!("    DualOracle        : {n_dual}");
    println!("    NoOracle          : {n_none}");
    println!("  scan ended by       : {scan_terminated_by}");

    // ── STEP 2 — freshness verification for PythOnly candidates ─────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  STEP 2 — freshness verification for PythOnly candidates");
    println!("══════════════════════════════════════════════════════════════");

    let pyth_only: Vec<&ReserveRecord> = records
        .iter()
        .filter(|r| {
            reserve_mint_override
                .map(|m| r.mint == m)
                .unwrap_or(r.shape == ReserveOracleShape::PythOnly)
        })
        .collect();

    if pyth_only.is_empty() {
        println!(
            "  no PythOnly reserves in the scanned set{} — skipping freshness verification",
            if reserve_mint_override.is_some() {
                " matching the reserve-mint override"
            } else {
                ""
            }
        );
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct PyCandidate {
        record_index: usize,
        status: TargetStatus,
        pyth_freshness: FeedPublishFreshness,
        /// Kept for debug-print review; not consumed by the verdict.
        pyth_slot_class: OracleSlotClass,
    }

    let mut verified: Vec<PyCandidate> = Vec::new();
    for rec in &pyth_only {
        let idx = records.iter().position(|r| r.reserve_pubkey == rec.reserve_pubkey).unwrap();
        let (data, owner, _ctx_slot) = match get_account_info(&client, &url, &rec.pyth_slot).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                println!(
                    "  [{} @ {}] pyth_slot {} NOT FOUND — {:?}",
                    rec.mint,
                    rec.reserve_pubkey,
                    rec.pyth_slot,
                    TargetStatus::BlockedByMissingFeed
                );
                verified.push(PyCandidate {
                    record_index: idx,
                    status: TargetStatus::BlockedByMissingFeed,
                    pyth_freshness: FeedPublishFreshness::Unknown,
                    pyth_slot_class: OracleSlotClass::NotFetched,
                });
                continue;
            }
            Err(e) if e == "RPC_RATE_LIMIT" => {
                println!("  RPC rate-limit during freshness verification — stopping");
                break;
            }
            Err(e) => {
                println!("  fetch error on pyth_slot {}: {e}", rec.pyth_slot);
                verified.push(PyCandidate {
                    record_index: idx,
                    status: TargetStatus::BlockedByOtherReason,
                    pyth_freshness: FeedPublishFreshness::Unknown,
                    pyth_slot_class: OracleSlotClass::NotFetched,
                });
                continue;
            }
        };
        let owner_class = match owner.to_string().as_str() {
            "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ" => OracleSlotClass::Pyth,
            "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f" => OracleSlotClass::Switchboard,
            _ => OracleSlotClass::UnknownOwner,
        };
        let freshness = extract_publish_freshness(&owner, &data);
        // For a PythOnly reserve: the Switchboard slot is sentinel and is
        // excluded from the feed set at snapshot-assembly time. The only
        // feed the policy evaluates is the Pyth one. So the reserve is
        // Slice-3-compatible iff this Pyth feed reports KnownSlot.
        let status = match freshness {
            FeedPublishFreshness::KnownSlot(_) => TargetStatus::CandidatePassableForOracleGate,
            FeedPublishFreshness::Unknown => TargetStatus::BlockedByOtherReason,
        };
        println!(
            "  [{} @ {}] market={} decimals={} pyth={} switchboard={}",
            rec.mint,
            rec.reserve_pubkey,
            rec.market_name,
            rec.decimals,
            rec.pyth_slot,
            if rec.switchboard_slot == sentinel {
                "Sentinel".to_string()
            } else {
                rec.switchboard_slot.to_string()
            }
        );
        println!(
            "    pyth_feed freshness: {freshness:?}  owner_class: {owner_class:?}  => {status:?}"
        );
        verified.push(PyCandidate {
            record_index: idx,
            status,
            pyth_freshness: freshness,
            pyth_slot_class: owner_class,
        });
    }

    // ── STEP 3 — baseline / override obligation status ──────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  STEP 3 — obligation status (baseline or override)");
    println!("══════════════════════════════════════════════════════════════");
    let (obligation_pk, _owner_pk, obl_label) = match obligation_override {
        Some(o) => {
            let owner = owner_override.unwrap_or(o);
            (o, owner, "override")
        }
        None => (
            Pubkey::from_str(BASELINE_OBLIGATION_BS58).unwrap(),
            Pubkey::from_str(BASELINE_OWNER_BS58).unwrap(),
            "baseline",
        ),
    };
    match get_account_info(&client, &url, &obligation_pk).await {
        Ok(Some((data, owner, ctx_slot))) => {
            if owner != solend_program {
                println!(
                    "  obligation {obligation_pk} ({obl_label}) — owner {owner} is NOT Solend; skipping decode"
                );
            } else {
                match raw::decode_obligation(&data) {
                    Ok(o) => {
                        println!(
                            "  obligation {obligation_pk} ({obl_label})  context_slot={ctx_slot}"
                        );
                        println!(
                            "    owner={}  market={}  stale={}  deposits={} borrows={}",
                            o.owner,
                            o.lending_market,
                            o.last_update_stale,
                            o.deposits.len(),
                            o.borrows.len()
                        );
                        let mut referenced: Vec<Pubkey> = Vec::new();
                        for d in &o.deposits {
                            if !referenced.contains(&d.deposit_reserve) {
                                referenced.push(d.deposit_reserve);
                            }
                        }
                        for b in &o.borrows {
                            if !referenced.contains(&b.borrow_reserve) {
                                referenced.push(b.borrow_reserve);
                            }
                        }
                        println!("    referenced reserves: {}", referenced.len());
                        for res_pk in &referenced {
                            // Check each referenced reserve's oracle shape. We
                            // may not have scanned this one (different market)
                            // — fetch it and classify.
                            if let Ok(Some((rb, rowner, _))) =
                                get_account_info(&client, &url, res_pk).await
                            {
                                if rowner != solend_program {
                                    println!("      {res_pk}  owner={rowner} (not Solend) — SKIP");
                                    continue;
                                }
                                match raw::decode_reserve(&rb) {
                                    Ok(rr) => {
                                        let shape = match (
                                            rr.pyth_oracle == sentinel,
                                            rr.switchboard_oracle == sentinel,
                                        ) {
                                            (false, true) => "PythOnly",
                                            (true, false) => "SwitchboardOnly",
                                            (false, false) => "DualOracle",
                                            (true, true) => "NoOracle",
                                        };
                                        println!(
                                            "      {res_pk}  mint={}  decimals={}  shape={shape}",
                                            rr.liquidity_mint, rr.liquidity_mint_decimals
                                        );
                                    }
                                    Err(e) => {
                                        println!("      {res_pk}  decode failed: {e}");
                                    }
                                }
                            } else {
                                println!("      {res_pk}  FETCH failed / missing");
                            }
                        }
                    }
                    Err(e) => {
                        println!("  obligation {obligation_pk} ({obl_label}) decode failed: {e}");
                    }
                }
            }
        }
        Ok(None) => {
            println!("  obligation {obligation_pk} ({obl_label}) NOT FOUND on chain");
        }
        Err(e) => {
            println!("  obligation {obligation_pk} ({obl_label}) fetch error: {e}");
        }
    }

    // ── STEP 4 — conclusion ─────────────────────────────────────────────
    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  STEP 4 — RECON CONCLUSION");
    println!("══════════════════════════════════════════════════════════════");

    let passable: Vec<&PyCandidate> = verified
        .iter()
        .filter(|v| v.status == TargetStatus::CandidatePassableForOracleGate)
        .collect();

    println!("  reserves classified              : {}", records.len());
    println!("  PythOnly reserves inspected      : {}", pyth_only.len());
    println!("  PythOnly reserves verified passable: {}", passable.len());
    println!("  scan terminated by               : {scan_terminated_by}");

    if !passable.is_empty() {
        println!();
        println!("  ⇒ Slice-3-compatible reserve(s) FOUND under current V1 oracle semantics:");
        for c in &passable {
            let rec = &records[c.record_index];
            let slot = match c.pyth_freshness {
                FeedPublishFreshness::KnownSlot(s) => s.raw(),
                FeedPublishFreshness::Unknown => 0,
            };
            println!(
                "    mint={}  reserve={}  market={}  decimals={}  pyth_feed=KnownSlot({slot})",
                rec.mint, rec.reserve_pubkey, rec.market_name, rec.decimals
            );
        }
        println!();
        println!("  NEXT STEP — Slice 3 is unblocked at the oracle gate for these");
        println!("  target mints, BUT still requires:");
        println!("    (a) an obligation that references ONLY PythOnly reserves");
        println!("        (otherwise other reserves' feed sets may contain");
        println!("        Switchboard-Unknown and HardBlock a different rule path),");
        println!("        OR a Slice 3 assembly/spec decision to include the");
        println!("        intended deposit reserve in the pre-action snapshot");
        println!("        for an empty obligation (see Part 6B §66 + prompt's");
        println!("        empty-obligation caveat); AND");
        println!("    (b) §66 refresh precondition handling (protocol-native");
        println!("        stale markers still default to Stale on bare reads");
        println!("        — RequireFreshState HardBlocks independently of");
        println!("        oracle freshness).");
    } else {
        println!();
        println!("  ⇒ NO Slice-3-compatible Solend reserve found in the scanned set.");
        println!();
        println!("    Every PythOnly reserve either failed to decode to");
        println!("    KnownSlot, or no PythOnly reserves exist in the scanned");
        println!("    markets. Every other reserve has a real Switchboard slot");
        println!("    whose decoder currently returns Unknown (Slice 2C");
        println!("    documented evidence gap).");
        println!();
        println!("  BLOCKER: Switchboard On-Demand decoder verification remains");
        println!("  the blocker for Slice 3 live deposit on Solend. See");
        println!("  `integrations/solend/oracle_decoder.rs` module doc for the");
        println!("  evidence bar (pinned upstream source + mainnet fixture");
        println!("  round-trip).");
    }

    println!();
    println!("  empty-obligation caveat: this recon does NOT treat an empty");
    println!("  obligation as passable by absence of feeds. Snapshot assembly");
    println!("  for an empty-obligation first Deposit requires a Slice 3 spec");
    println!("  decision about including the intended deposit reserve in the");
    println!("  pre-action snapshot.");
    println!("══════════════════════════════════════════════════════════════");
}
