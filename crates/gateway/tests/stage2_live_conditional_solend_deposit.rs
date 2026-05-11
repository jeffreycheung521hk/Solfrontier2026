//! Stage 2 W5c — Live conditional Solend deposit harness.
//!
//! First real Stage 2 live conditional path: a `condition_met` rule in
//! the state-store → W4 [`Stage2Executor`] atomic CAS lease → test-local
//! [`common::w5c_deposit_support::LiveSolendDepositExecutionClient`]
//! (implements the existing `Stage2ExecutionClient` trait) → controlled
//! wallet signs a direct Solend deposit on mainnet → confirmation →
//! rule transitions to `completed` or `failed`.
//!
//! # 2026-05-11 W5d-prep refactor
//!
//! The W5c-specific *behaviour* in this file is unchanged. The deposit
//! client, RPC plumbing, fixture-rule builder, P5c invariants, and
//! retry/poll helpers have moved verbatim into
//! [`common::w5c_deposit_support`] so the W5d demo-bridge harness can
//! reuse them without duplicating the live broadcast logic. What
//! stays here:
//!
//!   1. W5c env-var names + the W5c-specific `HarnessConfig` reader.
//!   2. `FIXTURE_RULE_ID` (W5c uses a single id; W5d uses two).
//!   3. `FORBIDDEN_CALL_FORMS` (this file's own source-scan deny-list).
//!   4. The W5c live-test function + its banner.
//!   5. The unit tests that originally sat in this file — they still
//!      cover W5c's invariants end-to-end via the moved-but-otherwise-
//!      unchanged code path.
//!
//! See the module doc at the top of `common/w5c_deposit_support.rs`
//! for the full extraction policy.
//!
//! # Test-only adapter
//!
//! This slice **intentionally bypasses** `clawsol-authority`. There is
//! no `ExecuteAction`, no `AuthorizationRecord` PDA, and no on-chain
//! verifier. The controlled wallet is the direct signer of the Solend
//! ix list.
//!
//! # Two-phase env gating
//!
//! - **Phase 0** (`CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT=1`):
//!   Insert fixture rule → mark_condition_met_if_active → BUILD and
//!   SIMULATE the deposit tx (no CAS, no broadcast, durable state
//!   unchanged). Print banner. STOP.
//!
//! - **Phase 2** (`CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED="W5C LIVE
//!   CONDITIONAL DEPOSIT APPROVED"`): require both env vars. Insert
//!   fixture rule → mark_condition_met_if_active → invoke
//!   `executor.execute_rule_once(rule_id, ctx)` → wait for finalized →
//!   reload rule and assert `status == completed` → print banner.

mod common;

use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::{pubkey::Pubkey, signature::Signer};

use claw_gateway::integrations::solend::raw::decode_obligation;
use claw_gateway::stage2_executor::{
    MockExecutionClient, Stage2Executor, Stage2ExecutorRuleResult,
};
use claw_gateway::stage2_watcher::Stage2TickContext;

use claw_state_store::db::Database;
use claw_state_store::stage2_watch_rules::{Stage2WatchRuleRepository, WatchRuleStatus};

use common::w5c_deposit_support::*;

// ── W5c-specific env var names + approval phrase ──────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT";
const ENV_RPC_PRIMARY: &str = "HELIUS_RPC_URL";
const ENV_RPC_SECONDARY: &str = "CLAW_RPC_URL";
const ENV_CLUSTER: &str = "CLAW_STAGE2_CLUSTER";
const ENV_KEYPAIR_PATH: &str = "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH";
const ENV_DEPOSIT_AMOUNT_RAW: &str = "CLAW_STAGE2_CONDITIONAL_DEPOSIT_AMOUNT_RAW";

// Approval phrase + env var are sourced from the common module
// (`W5C_ENV_APPROVAL`, `W5C_APPROVAL_PHRASE`) so the W5d bridge reads
// the same constants when deciding whether to forward live-send
// authorisation.

// Test-deterministic id for the fixture rule.
const FIXTURE_RULE_ID: [u8; 16] = [
    0xC0, 0x5E, 0x05, 0x1C, 0xDE, 0x10, 0x5C, 0x57,
    0x05, 0xC0, 0x05, 0xC0, 0x05, 0xC0, 0x05, 0xC0,
];

/// Source-scan list specific to this binary. The
/// `no_send_path_default_invariant` test enforces that none of these
/// tokens appear more than once (the FORBIDDEN_CALL_FORMS allowlist
/// entry itself counts as 1). Tokens reachable only through the
/// gated common-module call sites (`sendTransaction`) are NOT in this
/// list — that's the common module's surface.
const FORBIDDEN_CALL_FORMS: &[&str] = &[
    "approve(",
    "setAuthority(",
    "set_authority(",
    "delegate(",
    "closeAccount(",
    "close_account(",
    "api.jup.ag/",
    "skipPreflight: true",
    "signTransaction(",
    "window.solana",
    "confirmTransaction(",
    "Keypair::from_base58_string",
    "Helius-Sender",
    "helius-sender",
];

// ── env-reader ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum EnvStatus {
    Skipped(String),
    Mismatch(String),
    Ready(Box<HarnessConfig>),
}

#[derive(Debug)]
struct HarnessConfig {
    rpc_url: String,
    cluster: Cluster,
    keypair_path: String,
    amount_raw: u64,
    approved: bool,
}

impl HarnessConfig {
    fn from_env_with<F>(getter: F) -> EnvStatus
    where
        F: Fn(&str) -> Option<String>,
    {
        if getter(ENV_GATE).as_deref() != Some("1") {
            return EnvStatus::Skipped(format!(
                "{ENV_GATE} not set to 1 — conditional deposit harness self-skipped"
            ));
        }
        let rpc_url = match getter(ENV_RPC_PRIMARY).or_else(|| getter(ENV_RPC_SECONDARY)) {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "neither {ENV_RPC_PRIMARY} nor {ENV_RPC_SECONDARY} is set"
                ))
            }
        };
        let cluster_str = match getter(ENV_CLUSTER) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => return EnvStatus::Skipped(format!("{ENV_CLUSTER} not set")),
        };
        let cluster = match Cluster::parse(&cluster_str) {
            Some(c) => c,
            None => {
                return EnvStatus::Mismatch(format!(
                    "{ENV_CLUSTER}='{cluster_str}' is not one of mainnet-beta | devnet | localnet"
                ))
            }
        };
        if cluster != Cluster::MainnetBeta {
            return EnvStatus::Mismatch(format!(
                "conditional deposit target is mainnet-beta only; got cluster={cluster:?}"
            ));
        }
        let keypair_path = match getter(ENV_KEYPAIR_PATH) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return EnvStatus::Skipped(format!(
                    "{ENV_KEYPAIR_PATH} not set — conditional deposit needs a keypair path"
                ))
            }
        };
        let amount_raw = match getter(ENV_DEPOSIT_AMOUNT_RAW) {
            Some(s) if !s.trim().is_empty() => match s.trim().parse::<u64>() {
                Ok(n) => n,
                Err(e) => {
                    return EnvStatus::Mismatch(format!(
                        "{ENV_DEPOSIT_AMOUNT_RAW}='{s}' is not a u64: {e}"
                    ))
                }
            },
            _ => DEFAULT_DEPOSIT_AMOUNT_RAW,
        };
        if amount_raw == 0 {
            return EnvStatus::Mismatch(format!("{ENV_DEPOSIT_AMOUNT_RAW} must be > 0"));
        }
        let approved = getter(W5C_ENV_APPROVAL).as_deref() == Some(W5C_APPROVAL_PHRASE);
        EnvStatus::Ready(Box::new(HarnessConfig {
            rpc_url,
            cluster,
            keypair_path,
            amount_raw,
            approved,
        }))
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Live test (env-gated)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage2_w5c_live_conditional_solend_deposit() {
    let status = HarnessConfig::from_env_with(|k| env::var(k).ok());
    let cfg = match status {
        EnvStatus::Skipped(reason) => {
            eprintln!("[skip] W5c conditional deposit: {reason}");
            return;
        }
        EnvStatus::Mismatch(msg) => panic!("STOP — W5c conditional deposit: {msg}"),
        EnvStatus::Ready(c) => c,
    };

    eprintln!("──── W5c live conditional Solend deposit harness ────");
    eprintln!("cluster           : {:?}", cfg.cluster);
    eprintln!("rpc               : {}", redact_url(&cfg.rpc_url));
    eprintln!(
        "keypair path      : {} (contents NOT printed)",
        cfg.keypair_path
    );
    eprintln!("target obligation : {DEFAULT_TARGET_OBLIGATION_BS58}");
    eprintln!("amount (raw)      : {}", cfg.amount_raw);
    eprintln!(
        "amount (UI)       : {}.{:06} USDC",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!(
        "approved          : {}",
        if cfg.approved { "yes" } else { "no — Phase 0 only" }
    );

    // ── Keypair load ────────────────────────────────────────────────────
    let kp = load_keypair_from_file(&cfg.keypair_path)
        .unwrap_or_else(|e| panic!("STOP — keypair load failed: {e}"));
    let controlled_pk = kp.pubkey();
    let expected_controlled = Pubkey::from_str(CONTROLLED_WALLET_BS58).unwrap();
    if controlled_pk != expected_controlled {
        panic!(
            "STOP — keypair pubkey mismatch: file={controlled_pk} expected={expected_controlled}"
        );
    }
    eprintln!("\n✓ keypair loaded; pubkey matches pinned controlled wallet {controlled_pk}");

    // ── Bring up an in-memory state-store + insert fixture rule ─────────
    let db = Database::open_in_memory()
        .await
        .unwrap_or_else(|e| panic!("STOP — open_in_memory: {e}"));
    let repo = Stage2WatchRuleRepository::new(db.pool().clone());
    let target_obligation = Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap();

    // ── Pull current slot (used to set rule.expires_at_slot) ────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .expect("build reqwest client");
    let chain_slot = retry(
        || rpc_get_slot(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getSlot: {e}"));
    let ver = retry(
        || rpc_get_version(&http, &cfg.rpc_url),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — getVersion: {e}"));
    eprintln!("✓ RPC OK            solana-core={ver} chain_slot={chain_slot}");

    let rule = fixture_rule(
        FIXTURE_RULE_ID,
        cfg.amount_raw,
        DEFAULT_TARGET_OBLIGATION_BS58,
        CONTROLLED_WALLET_BS58,
        CONTROLLED_WALLET_BS58,
        chain_slot.saturating_add(50_000), // expiry well ahead
    );

    insert_and_mark_condition_met(&repo, &rule)
        .await
        .unwrap_or_else(|e| panic!("STOP — fixture insert: {e}"));
    let stored = repo
        .get(&rule.rule_id)
        .await
        .ok()
        .flatten()
        .expect("rule readable post-insert");
    assert_eq!(stored.status, WatchRuleStatus::ConditionMet);
    eprintln!(
        "✓ fixture rule inserted; rule_id={} status={:?}",
        hex_id(&rule.rule_id),
        stored.status
    );

    // ── Construct live client ───────────────────────────────────────────
    let client = LiveSolendDepositExecutionClient::new(
        kp,
        cfg.rpc_url.clone(),
        target_obligation,
    );

    // The Stage2Executor builds a request from the rule. We build one
    // here too so Phase 0 can simulate via the client.
    let dummy_executor_for_request = Stage2Executor::new(
        repo.clone(),
        Arc::new(MockExecutionClient::with_success_default()),
    );
    let preview_request = dummy_executor_for_request
        .build_execute_action_request(&stored, stored.execution_nonce.saturating_add(1))
        .unwrap_or_else(|e| panic!("STOP — build_execute_action_request: {e}"));
    eprintln!(
        "✓ Stage2ExecuteActionRequest built  input_amount_raw={} delegated_wallet={}",
        preview_request.input_amount_raw,
        Pubkey::new_from_array(preview_request.delegated_wallet.0)
    );

    // ── Phase 0: simulate-only (durable state unchanged) ────────────────
    eprintln!("\n── Phase 0: simulation ──");
    let (latest, sim, plan) = client
        .simulate_only(&preview_request)
        .await
        .unwrap_or_else(|e| panic!("STOP — simulation failed: {e}"));
    eprintln!(
        "✓ blockhash         hash={} lvbh={}",
        latest.hash, latest.last_valid_block_height
    );
    eprintln!("simulated CU       : {:?}", sim.units_consumed);
    eprintln!("simulated err      : {:?}", sim.err);
    for l in &sim.logs {
        eprintln!("  log: {l}");
    }
    if let Some(err) = &sim.err {
        panic!("STOP — simulation failed: {err}");
    }
    eprintln!(
        "✓ simulation passed; source USDC before={} cToken-amount before={}",
        plan.source_usdc_before, plan.obligation_pinned_reserve_amount_before
    );

    // Phase 0 banner.
    let priority_fee_sim =
        estimated_priority_fee_lamports(COMPUTE_UNIT_LIMIT, COMPUTE_UNIT_PRICE_MICRO_LAMPORTS);
    eprintln!(
        "\n──────────────────────────────────────────────────────────────"
    );
    eprintln!("W5c conditional deposit preflight passed.");
    eprintln!(
        "Ready to deposit {}.{:06} USDC from controlled wallet {controlled_pk}",
        cfg.amount_raw / 1_000_000,
        cfg.amount_raw % 1_000_000
    );
    eprintln!("  via source USDC ATA   {}", plan.source_usdc_ata);
    eprintln!("  into obligation        {}", plan.target_obligation);
    eprintln!("  reserve                {}", plan.reserve_pubkey);
    eprintln!("  cToken minted to ATA   {}", plan.ctoken_ata);
    eprintln!("");
    eprintln!("Awaiting exact approval phrase: {W5C_APPROVAL_PHRASE}");
    eprintln!("To proceed to Phase 2 live send, re-run with env var:");
    eprintln!("  {W5C_ENV_APPROVAL}=\"{W5C_APPROVAL_PHRASE}\"");
    eprintln!(
        "──────────────────────────────────────────────────────────────"
    );

    if !cfg.approved {
        let after = repo
            .get(&rule.rule_id)
            .await
            .ok()
            .flatten()
            .expect("rule readable post-phase-0");
        assert_eq!(
            after.status,
            WatchRuleStatus::ConditionMet,
            "Phase 0 must not mutate rule past condition_met"
        );
        print_banner(BannerInputs {
            rule_id: rule.rule_id,
            previous_status: "condition_met",
            leased_status: "N/A (simulation-only did not mutate durable state)",
            final_status: "simulated",
            amount_raw: cfg.amount_raw,
            controlled_wallet: controlled_pk,
            source_usdc_ata: plan.source_usdc_ata,
            obligation: plan.target_obligation,
            before_usdc_raw: Some(plan.source_usdc_before),
            after_usdc_raw: None,
            before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
            after_ctoken_amount: None,
            priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            estimated_or_actual_cu: sim.units_consumed.unwrap_or(0),
            priority_fee_lamports: estimated_priority_fee_lamports(
                sim.units_consumed.map(|u| u as u32).unwrap_or(COMPUTE_UNIT_LIMIT),
                COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            )
            .max(priority_fee_sim),
            tx_signature: None,
        });
        return;
    }

    // ── Phase 2: invoke W4 executor with the live client ────────────────
    eprintln!("\n── Phase 2: live send via Stage2Executor ──");
    let executor = Stage2Executor::new(repo.clone(), Arc::new(client));
    let ctx = Stage2TickContext::new(
        chain_slot,
        chrono::Utc::now().timestamp(),
        chrono::Utc::now().timestamp_millis(),
    );
    let result = executor.execute_rule_once(rule.rule_id, ctx).await;

    let (signature, slot, used) = match result {
        Stage2ExecutorRuleResult::Completed {
            execution_nonce,
            used_amount_raw,
            confirmation_slot,
            signature_sentinel,
            ..
        } => {
            eprintln!(
                "✓ executor Completed: execution_nonce={execution_nonce} \
                 slot={confirmation_slot} used={used_amount_raw}"
            );
            (signature_sentinel, confirmation_slot, used_amount_raw)
        }
        Stage2ExecutorRuleResult::Failed { error, .. } => {
            let after = repo
                .get(&rule.rule_id)
                .await
                .ok()
                .flatten()
                .map(|r| format!("{:?}", r.status))
                .unwrap_or_else(|| "<unreadable>".to_string());
            print_banner(BannerInputs {
                rule_id: rule.rule_id,
                previous_status: "condition_met",
                leased_status: "executing",
                final_status: "failed",
                amount_raw: cfg.amount_raw,
                controlled_wallet: controlled_pk,
                source_usdc_ata: plan.source_usdc_ata,
                obligation: plan.target_obligation,
                before_usdc_raw: Some(plan.source_usdc_before),
                after_usdc_raw: None,
                before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
                after_ctoken_amount: None,
                priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
                estimated_or_actual_cu: sim.units_consumed.unwrap_or(COMPUTE_UNIT_LIMIT as u64),
                priority_fee_lamports: priority_fee_sim,
                tx_signature: None,
            });
            panic!(
                "STOP — executor Failed: {error}; rule reloaded status={after}"
            );
        }
        other => panic!("STOP — unexpected executor outcome: {other:?}"),
    };
    eprintln!("✓ signature  {signature}");
    eprintln!("  Solscan:   https://solscan.io/tx/{signature}");

    let after = repo
        .get(&rule.rule_id)
        .await
        .ok()
        .flatten()
        .expect("rule readable post-phase-2");
    assert_eq!(
        after.status,
        WatchRuleStatus::Completed,
        "executor must transition rule to completed on Ok receipt"
    );
    assert!(after.completed, "completed flag must be set");

    // The state-store keeps `used_amount_raw` in a SQL column updated
    // by `mark_completed`; the JSON-serialised `rule_json` column is
    // frozen at insert time. Cross-check the SQL column directly,
    // matching the pattern used by claw-state-store's own tests.
    let used_in_sql: (i64,) = sqlx::query_as(
        "SELECT used_amount_raw FROM stage2_watch_rules WHERE rule_id = ?",
    )
    .bind(hex_id(&rule.rule_id))
    .fetch_one(db.pool())
    .await
    .unwrap_or_else(|e| panic!("STOP — SELECT used_amount_raw: {e}"));
    assert_eq!(
        used_in_sql.0 as u64,
        cfg.amount_raw,
        "state-store SQL column used_amount_raw must equal amount_raw"
    );

    // Re-fetch USDC + obligation for hard delta assertions.
    let source_after = retry(
        || rpc_get_token_account_balance(&http, &cfg.rpc_url, &plan.source_usdc_ata),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — source USDC after: {e}"))
    .unwrap_or_else(|| panic!("STOP — source ATA vanished"));
    let obligation_bytes_after = retry(
        || rpc_get_account_data(&http, &cfg.rpc_url, &plan.target_obligation),
        RPC_MAX_RETRY_ATTEMPTS,
        Duration::from_millis(RPC_INITIAL_BACKOFF_MS),
    )
    .await
    .unwrap_or_else(|e| panic!("STOP — refetch obligation: {e}"))
    .unwrap_or_else(|| panic!("STOP — obligation vanished after tx"));
    let obligation_after = decode_obligation(&obligation_bytes_after)
        .unwrap_or_else(|e| panic!("STOP — re-decode obligation: {e}"));

    let after_ctoken = obligation_pinned_reserve_amount(&obligation_after);

    // Delta-based assertions (NEVER hardcoded absolute balances).
    let usdc_delta = plan
        .source_usdc_before
        .checked_sub(source_after.raw)
        .unwrap_or(0);
    assert_eq!(
        usdc_delta, cfg.amount_raw,
        "source USDC must decrease by exactly amount_raw"
    );
    assert!(
        after_ctoken > plan.obligation_pinned_reserve_amount_before,
        "obligation pinned-reserve cToken amount must strictly increase"
    );

    eprintln!(
        "\n── deltas ──\nUSDC: {} → {} (Δ -{})\ncToken: {} → {} (Δ +{})",
        plan.source_usdc_before,
        source_after.raw,
        usdc_delta,
        plan.obligation_pinned_reserve_amount_before,
        after_ctoken,
        after_ctoken - plan.obligation_pinned_reserve_amount_before
    );

    let priority_fee_actual =
        estimated_priority_fee_lamports(COMPUTE_UNIT_LIMIT, COMPUTE_UNIT_PRICE_MICRO_LAMPORTS);
    print_banner(BannerInputs {
        rule_id: rule.rule_id,
        previous_status: "condition_met",
        leased_status: "executing",
        final_status: "completed",
        amount_raw: cfg.amount_raw,
        controlled_wallet: controlled_pk,
        source_usdc_ata: plan.source_usdc_ata,
        obligation: plan.target_obligation,
        before_usdc_raw: Some(plan.source_usdc_before),
        after_usdc_raw: Some(source_after.raw),
        before_ctoken_amount: Some(plan.obligation_pinned_reserve_amount_before),
        after_ctoken_amount: Some(after_ctoken),
        priority_fee_micro_lamports_per_cu: COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        estimated_or_actual_cu: sim.units_consumed.unwrap_or(COMPUTE_UNIT_LIMIT as u64),
        priority_fee_lamports: priority_fee_actual,
        tx_signature: Some(signature.clone()),
    });

    eprintln!("\n──── W5c conditional Solend deposit complete ────");
    eprintln!("signature : {signature}");
    eprintln!("slot      : {slot}");
    eprintln!("used      : {used}");
    eprintln!("Solscan   : https://solscan.io/tx/{signature}");
}

// ── Banner rendering ──────────────────────────────────────────────────────

struct BannerInputs {
    rule_id: [u8; 16],
    previous_status: &'static str,
    leased_status: &'static str,
    final_status: &'static str,
    amount_raw: u64,
    controlled_wallet: Pubkey,
    source_usdc_ata: Pubkey,
    obligation: Pubkey,
    before_usdc_raw: Option<u64>,
    after_usdc_raw: Option<u64>,
    before_ctoken_amount: Option<u64>,
    after_ctoken_amount: Option<u64>,
    priority_fee_micro_lamports_per_cu: u64,
    estimated_or_actual_cu: u64,
    priority_fee_lamports: u64,
    tx_signature: Option<String>,
}

fn print_banner(i: BannerInputs) {
    eprintln!("\n============================================================");
    eprintln!("W5C LIVE CONDITIONAL DEPOSIT RESULT");
    eprintln!("rule_id: {}", hex_id(&i.rule_id));
    eprintln!("previous_status: {}", i.previous_status);
    eprintln!("leased_status: {}", i.leased_status);
    eprintln!("final_status: {}", i.final_status);
    eprintln!("amount_raw: {}", i.amount_raw);
    eprintln!("controlled_wallet: {}", i.controlled_wallet);
    eprintln!("source_usdc_ata: {}", i.source_usdc_ata);
    eprintln!("obligation: {}", i.obligation);
    eprintln!("before_usdc_raw: {}", opt(i.before_usdc_raw));
    eprintln!("after_usdc_raw: {}", opt(i.after_usdc_raw));
    eprintln!(
        "usdc_delta_raw: {}",
        match (i.before_usdc_raw, i.after_usdc_raw) {
            (Some(b), Some(a)) => format!("-{}", b.saturating_sub(a)),
            _ => "N/A".to_string(),
        }
    );
    eprintln!("before_ctoken_amount: {}", opt(i.before_ctoken_amount));
    eprintln!("after_ctoken_amount: {}", opt(i.after_ctoken_amount));
    eprintln!(
        "ctoken_delta_raw: {}",
        match (i.before_ctoken_amount, i.after_ctoken_amount) {
            (Some(b), Some(a)) => format!("+{}", a.saturating_sub(b)),
            _ => "N/A".to_string(),
        }
    );
    eprintln!(
        "priority_fee_micro_lamports_per_cu: {}",
        i.priority_fee_micro_lamports_per_cu
    );
    eprintln!("estimated_or_actual_cu: {}", i.estimated_or_actual_cu);
    eprintln!("priority_fee_lamports: {}", i.priority_fee_lamports);
    let sig_field = i
        .tx_signature
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "N/A".to_string());
    let solscan_field = i
        .tx_signature
        .as_deref()
        .map(|s| format!("https://solscan.io/tx/{s}"))
        .unwrap_or_else(|| "N/A".to_string());
    eprintln!("tx_signature: {sig_field}");
    eprintln!("solscan: {solscan_field}");
    eprintln!("============================================================");
}

fn opt(x: Option<u64>) -> String {
    x.map(|n| n.to_string()).unwrap_or_else(|| "N/A".to_string())
}

// ──────────────────────────────────────────────────────────────────────────
// Unit tests (always run; non-live; no network; no keypair)
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use claw_gateway::stage2_executor::{
        Stage2ExecuteActionRequest, Stage2ExecutionClient, Stage2ExecutionError,
        Stage2ExecutionReceipt, DEMO_CTOKEN_MINT_BS58, DEMO_LENDING_MARKET_BS58,
        DEMO_RESERVE_BS58,
    };
    use claw_types::canonical_intent::PubkeyBytes;
    use claw_types::stage2_watch_rule::STAGE2_WATCH_RULE_SCHEMA_VERSION;
    use solana_sdk::signature::Keypair;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── env-reader tests ──────────────────────────────────────────────────

    #[test]
    fn env_gate_skips_without_master_flag() {
        let status = HarnessConfig::from_env_with(|_| None);
        match status {
            EnvStatus::Skipped(reason) => assert!(reason.contains(ENV_GATE)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn cluster_must_be_mainnet_beta() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("devnet".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("mainnet-beta")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_keypair_path_rejects() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Skipped(m) => assert!(m.contains(ENV_KEYPAIR_PATH)),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn deposit_amount_zero_rejected() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_AMOUNT_RAW" => Some("0".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Mismatch(m) => assert!(m.contains("> 0")),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn default_amount_is_quarter_usdc_when_unset() {
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => {
                assert_eq!(c.amount_raw, DEFAULT_DEPOSIT_AMOUNT_RAW);
                assert_eq!(c.amount_raw, 250_000);
                assert!(!c.approved);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn approval_phrase_is_exact_match() {
        // lowercase variant must NOT approve
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => {
                Some("w5c live conditional deposit approved".to_string())
            }
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(!c.approved, "lowercase must not approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
        let status = HarnessConfig::from_env_with(|k| match k {
            "CLAW_STAGE2_LIVE_CONDITIONAL_DEPOSIT" => Some("1".to_string()),
            "HELIUS_RPC_URL" => Some("https://example.invalid/".to_string()),
            "CLAW_STAGE2_CLUSTER" => Some("mainnet-beta".to_string()),
            "CLAW_STAGE2_DELEGATED_KEYPAIR_PATH" => Some("/tmp/fake".to_string()),
            "CLAW_STAGE2_CONDITIONAL_DEPOSIT_APPROVED" => Some(W5C_APPROVAL_PHRASE.to_string()),
            _ => None,
        });
        match status {
            EnvStatus::Ready(c) => assert!(c.approved, "exact phrase must approve"),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn redact_url_strips_api_key() {
        let r = redact_url("https://mainnet.helius-rpc.com/?api-key=abc123XYZ");
        assert!(r.contains("***REDACTED***"));
        assert!(!r.contains("abc123XYZ"));
    }

    #[test]
    fn priority_fee_math_matches_spec() {
        // 400_000 CU × 50_000 μ/CU / 1_000_000 = 20_000 lamports
        assert_eq!(estimated_priority_fee_lamports(400_000, 50_000), 20_000);
        // 87_803 CU (W5b empirical) × 50_000 / 1_000_000 = 4_390 lamports
        assert_eq!(estimated_priority_fee_lamports(87_803, 50_000), 4_390);
    }

    #[test]
    fn pin_constants_match_stage2_executor() {
        assert_eq!(
            DEMO_RESERVE_BS58,
            claw_gateway::stage2_executor::DEMO_RESERVE_BS58
        );
        assert_eq!(
            DEMO_LENDING_MARKET_BS58,
            claw_gateway::stage2_executor::DEMO_LENDING_MARKET_BS58
        );
        assert_eq!(
            DEMO_CTOKEN_MINT_BS58,
            claw_gateway::stage2_executor::DEMO_CTOKEN_MINT_BS58
        );
    }

    // ── State-machine tests using MockExecutionClient ─────────────────────

    async fn test_repo() -> (Database, Stage2WatchRuleRepository) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        (db, repo)
    }

    fn ctx() -> Stage2TickContext {
        Stage2TickContext::new(
            415_500_000,
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp_millis(),
        )
    }

    #[tokio::test]
    async fn cas_lease_prevents_double_execution() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();

        let counter = Arc::new(AtomicUsize::new(0));

        #[derive(Debug)]
        struct CountingClient {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Stage2ExecutionClient for CountingClient {
            async fn send_and_confirm(
                &self,
                request: Stage2ExecuteActionRequest,
            ) -> Result<Stage2ExecutionReceipt, Stage2ExecutionError> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(Stage2ExecutionReceipt {
                    rule_id: request.rule_id,
                    execution_nonce: request.execution_nonce,
                    confirmation_slot: 415_500_000,
                    used_amount_raw: request.input_amount_raw,
                    signature_sentinel: "mock-sig".into(),
                })
            }
        }

        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(CountingClient {
                counter: counter.clone(),
            }),
        );

        let r1 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r1, Stage2ExecutorRuleResult::Completed { .. }));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let r2 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r2, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "expected LeaseLost on second call, got {r2:?}"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "client must not be called on a lost lease"
        );
    }

    #[tokio::test]
    async fn send_signature_without_confirmation_does_not_mark_completed() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();

        let mock = MockExecutionClient::new();
        mock.push_failure(Stage2ExecutionError::ConfirmationFailed(
            "confirmation timeout (signature 5xxxx never reached finalized)".into(),
        ));
        let executor = Stage2Executor::new(repo.clone(), Arc::new(mock));
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::Failed { .. }),
            "expected Failed, got {r:?}"
        );
        let after = repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(after.status, WatchRuleStatus::Failed);
        assert!(!after.completed, "completed flag MUST stay false on failure");
    }

    #[tokio::test]
    async fn failure_marks_failed_and_no_retry() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();

        let mock = MockExecutionClient::new();
        mock.push_failure(Stage2ExecutionError::SendFailed("RPC down".into()));
        mock.push_failure(Stage2ExecutionError::SendFailed("should not fire".into()));
        let executor = Stage2Executor::new(repo.clone(), Arc::new(mock));

        let r1 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r1, Stage2ExecutorRuleResult::Failed { .. }));
        let r2 = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(matches!(r2, Stage2ExecutorRuleResult::LeaseLost { .. }));
    }

    #[tokio::test]
    async fn revoked_rule_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        repo.insert(&rule).await.unwrap();
        repo.mark_revoked(&rule.rule_id).await.unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "revoked rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn active_rule_not_in_condition_met_is_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        repo.insert(&rule).await.unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "active rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn completed_rule_is_ignored() {
        let (_db, repo) = test_repo().await;
        let rule = fixture_rule(
            FIXTURE_RULE_ID,
            250_000,
            DEFAULT_TARGET_OBLIGATION_BS58,
            CONTROLLED_WALLET_BS58,
            CONTROLLED_WALLET_BS58,
            415_700_000,
        );
        insert_and_mark_condition_met(&repo, &rule).await.unwrap();
        repo.mark_completed(&rule.rule_id, 250_000, 415_500_001)
            .await
            .unwrap();
        let executor = Stage2Executor::new(
            repo.clone(),
            Arc::new(MockExecutionClient::with_success_default()),
        );
        let r = executor.execute_rule_once(rule.rule_id, ctx()).await;
        assert!(
            matches!(r, Stage2ExecutorRuleResult::LeaseLost { .. }),
            "completed rule must not be executed; got {r:?}"
        );
    }

    #[tokio::test]
    async fn live_client_rejects_mismatching_delegated_wallet() {
        let kp = Keypair::new();
        let other = Pubkey::new_unique();
        let client = LiveSolendDepositExecutionClient::new(
            kp,
            "https://example.invalid/".to_string(),
            Pubkey::from_str(DEFAULT_TARGET_OBLIGATION_BS58).unwrap(),
        );
        let bogus_request = Stage2ExecuteActionRequest {
            rule_id: [0; 16],
            canonical_rule_hash: [0; 32],
            action_type: claw_types::stage2_watch_rule::WatchRuleActionType::SolendWithdrawAllDelegated,
            action_type_byte: 1,
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            input_amount_raw: 250_000,
            execution_nonce: 1,
            user: PubkeyBytes::new([0; 32]),
            executor: PubkeyBytes::new([0; 32]),
            delegated_wallet: PubkeyBytes::new(other.to_bytes()),
            destination: PubkeyBytes::new([0; 32]),
            expires_at_slot: 1_000_000_000,
            solend: None,
        };
        let r = client.build_tx_plan(&bogus_request).await;
        match r {
            Err(Stage2ExecutionError::InvalidRequest(s)) => {
                assert!(
                    s.contains("delegated_wallet") || s.contains("keypair pubkey"),
                    "expected delegated_wallet mismatch, got {s}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ── Source-scan invariant ─────────────────────────────────────────────

    #[test]
    fn no_send_path_default_invariant() {
        const SOURCE: &str = include_str!("stage2_live_conditional_solend_deposit.rs");
        for forbidden in FORBIDDEN_CALL_FORMS {
            let count = SOURCE.matches(forbidden).count();
            assert!(
                count <= 1,
                "forbidden call-form `{forbidden}` appears {count} times in W5c harness; \
                 only the FORBIDDEN_CALL_FORMS allowlist entry is permitted"
            );
        }
    }

    #[test]
    fn retry_policy_is_bounded() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let calls = Arc::new(AtomicUsize::new(0));
                let calls_ref = calls.clone();
                let r: Result<(), &'static str> = retry(
                    || {
                        let c = calls_ref.clone();
                        async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            Err::<(), &'static str>("always fails")
                        }
                    },
                    3,
                    Duration::from_millis(1),
                )
                .await;
                assert!(r.is_err());
                assert_eq!(calls.load(Ordering::SeqCst), 3);
            });
    }
}
