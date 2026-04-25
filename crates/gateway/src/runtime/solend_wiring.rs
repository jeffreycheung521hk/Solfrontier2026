//! Phase 4C-3a — Solend deposit tool wiring.
//!
//! This module owns the ONE function that composes every Solend-specific
//! dependency the `solend_deposit_usdc` tool needs and registers the
//! tool into the caller's [`ToolRegistry`]. It exists because
//! [`DEBT.md` D-2](../../../../DEBT.md) requires `daemon.rs` to stay
//! "wiring + main loop only"; migrating the ~95-line Solend wiring
//! block out of the daemon ahead of the Phase 4C-4 signer-handoff slice
//! keeps the daemon's attrition trajectory intact.
//!
//! # What this module does
//!
//! Pure wiring. Given the dependencies the daemon already holds
//! (an [`RpcPool`], an [`ExternalWalletStore`], the shared
//! [`ApprovalStore`], the Solend [`SolendParkStore`], and the approval
//! lease seconds), it:
//!
//! 1. Builds a [`ClawRpcPoolAccountFetcher`] for the read-only assembler.
//! 2. Wraps it in a [`SolendSnapshotAssembler`] and the
//!    [`ProductionSolendDepositSnapshotAssembler`] seam.
//! 3. Constructs the V1 [`LendingRuleConfig`] (Solend+USDC only, cap
//!    pinned to the tool's structural max).
//! 4. Builds the [`ClawRpcPoolPreflightRpc`] structural-preflight
//!    simulator.
//! 5. Constructs the [`SubmitSolendDepositTool`] and attaches the
//!    preflight simulator via `.with_preflight_simulator(...)`.
//! 6. Registers the tool into the passed-in registry and returns it.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT spawn tasks. No `tokio::spawn`, no background loop.
//! - Does NOT add HTTP routes. No Axum handler registration here.
//! - Does NOT sign, simulate, broadcast, or fetch blockhashes.
//! - Does NOT call any Solend pure builder.
//! - Does NOT evaluate lending policy itself — wiring only.
//! - Does NOT change tool output shape or any semantics visible to
//!   tests downstream of registry dispatch. Phase 4C-3a is a pure
//!   refactor; behaviour is identical to the pre-extraction
//!   inline-in-daemon form.

use std::sync::Arc;

use tracing::info;

use claw_solana_core::blockhash::BlockhashManager;
use claw_solana_core::rpc::RpcPool;
use claw_tool_system::registry::ToolRegistry;

use crate::approval_store::ApprovalStore;
use crate::external_wallet::ExternalWalletStore;
use crate::integrations::solend::{
    ClawRpcPoolAccountFetcher, SolendSnapshotAssembler,
};
use crate::integrations::solend_park::{SolendParkStore, SolendSigningDeps};
use crate::integrations::solend_preflight::{
    ClawRpcPoolPreflightRpc, SolendPreflightSimulator,
};
use crate::integrations::solend_signing::{
    ClawBlockhashProvider, RecentBlockhashProvider, SolendSigningStore,
};
use crate::integrations::solend_submit::SolendAuditSink;
use crate::lending::{
    AllowedLendingProtocolsConfig, AllowedMintsConfig, DurationMs, LendingRuleConfig,
    MaxActionInputAmountConfig, MaxOracleStalenessConfig, ProtocolTag,
    RequireFreshStateConfig, UnderlyingAmount,
};
use crate::tools::solend_deposit::{
    ProductionSolendDepositSnapshotAssembler, SolendDepositSnapshotAssembler,
    SubmitSolendDepositTool, MAX_STRUCTURAL_AMOUNT_RAW, USDC_MINT_BASE58,
};

/// Phase 4C-8G-Staleness-Calibration — Solend USDC oracle publish-age
/// budget, in milliseconds.
///
/// **Why 120 s, not the previous 30 s:** Pyth pushes a new publish
/// slot when the price moves outside its confidence band. USDC is
/// pegged near $1.00; during calm market windows the same publish
/// slot can persist for tens of seconds without anything actually
/// being wrong. The previous 30 s threshold was a stale-asset-class
/// default that fail-closed on calm-USDC moments — a false positive
/// that blocked otherwise-healthy deposits. 120 s widens the accepted
/// window to a value comfortably above typical stable-market gaps
/// while still failing closed on a multi-minute oracle outage, which
/// is the actual risk this rule is meant to catch.
///
/// **What this is NOT:** a bypass. `evaluate_lending_policy` is
/// unchanged. `MaxOracleStalenessConfig` is the same struct. The
/// `OraclePublishAgeExceeded` rule still fires beyond this threshold.
/// All other freshness rules (`require_fresh_state`,
/// allowed-protocol/mint, amount cap) keep their identical semantics.
///
/// Future asset classes with tighter oracle SLOs (e.g., volatile
/// majors) should NOT inherit this stable-asset budget. When this
/// repo grows multi-asset Solend support, the constant should split
/// per asset (or move to TOML config) rather than apply globally.
pub const SOLEND_USDC_MAX_ORACLE_PUBLISH_AGE_MS: u64 = 120_000;

/// Pure builder for the Solend USDC `LendingRuleConfig` used by
/// `wire_solend_deposit_tool`. Extracted so unit tests can assert the
/// calibrated thresholds + non-oracle rule shape without standing up
/// the full runtime wiring (RPC pool, blockhash manager, audit sink,
/// signing store, …).
///
/// **Pure:** no RPC, no clock, no I/O. Returns a fresh
/// `LendingRuleConfig` every call. Cheap to recompute.
pub fn solend_usdc_lending_rule_config() -> LendingRuleConfig {
    let usdc_mint = solana_sdk::pubkey::Pubkey::try_from(USDC_MINT_BASE58)
        .expect("USDC_MINT_BASE58 parses");
    LendingRuleConfig {
        require_fresh_state: RequireFreshStateConfig {
            // Snapshot fetch age — kept at 30 s. Distinct from oracle
            // publish age: this is "how stale is OUR fetched account
            // state", which should be tight regardless of asset
            // volatility because we control our own polling cadence.
            max_fetch_age: DurationMs::new(30_000),
        },
        max_oracle_staleness: MaxOracleStalenessConfig {
            // 4C-8G-Staleness-Calibration — see
            // `SOLEND_USDC_MAX_ORACLE_PUBLISH_AGE_MS` doc above.
            max_publish_age: DurationMs::new(SOLEND_USDC_MAX_ORACLE_PUBLISH_AGE_MS),
        },
        allowed_lending_protocols: AllowedLendingProtocolsConfig {
            allowlist: vec![ProtocolTag::Solend],
        },
        allowed_mints: AllowedMintsConfig {
            allowlist: vec![usdc_mint],
        },
        max_action_input_amount: MaxActionInputAmountConfig {
            per_mint_caps: vec![(
                usdc_mint,
                UnderlyingAmount::new(MAX_STRUCTURAL_AMOUNT_RAW),
            )],
        },
    }
}

/// Wire the Solend deposit tool into `registry` and return the updated
/// registry. Pure wiring; no behaviour change from the pre-extraction
/// daemon code.
///
/// # Parameters
///
/// - `registry` — the tool registry the solend tool is added to.
/// - `rpc_pool` — cloned into the read-only account fetcher and into
///   the preflight simulator. Both inner adapters share the primary
///   `RpcPool` so the circuit-breaker state stays consistent across
///   the daemon.
/// - `external_wallet` — cast to `Arc<dyn SessionBoundWallet>` for the
///   tool's session-binding lookup. `ExternalWalletStore` already
///   implements that trait.
/// - `approval_store`, `pending_solend_park` — shared with the
///   approval-routing code path; the tool holds its own `Clone` of
///   each, matching the Jupiter reference shape.
/// - `approval_lease_seconds` — pulled from `config.policy` at the
///   call site; passed through verbatim.
///
/// The returned `ToolRegistry` is the caller's input with the Solend
/// tool registered.
pub fn wire_solend_deposit_tool(
    registry: ToolRegistry,
    rpc_pool: RpcPool,
    external_wallet: ExternalWalletStore,
    approval_store: ApprovalStore,
    pending_solend_park: SolendParkStore,
    signing_store: SolendSigningStore,
    blockhash_manager: Arc<BlockhashManager>,
    audit_sink: Arc<dyn SolendAuditSink>,
    approval_lease_seconds: u64,
) -> ToolRegistry {
    // ── Read-only RPC adapter: get_slot / get_account / get_multiple_accounts ─
    //
    // Shares the primary `RpcPool` so the circuit breaker and endpoint
    // selection are consistent across all daemon RPC paths. No signer,
    // no broadcast, no blockhash fetch.
    let fetcher = Arc::new(ClawRpcPoolAccountFetcher::new(
        rpc_pool.clone(),
        claw_types::solana::CommitmentLevel::Confirmed,
    ));
    let concrete_assembler = Arc::new(SolendSnapshotAssembler::new(fetcher));
    let assembler_seam: Arc<dyn SolendDepositSnapshotAssembler> = Arc::new(
        ProductionSolendDepositSnapshotAssembler::new(concrete_assembler),
    );

    // ── V1 default policy config (Solend+USDC only) ─────────────────────────
    //
    // Conservative defaults; cap pinned to the tool's structural max so
    // the evaluator's cap rule aligns with the tool-surface guard.
    // Future slices may promote this to a TOML-driven config. Extracted
    // into a pure helper [`solend_usdc_lending_rule_config`] so unit
    // tests can assert calibration without standing up the full wiring.
    let rule_config = solend_usdc_lending_rule_config();

    // ── Structural preflight simulator (Phase 4C-3) ─────────────────────────
    //
    // Runs ONLY `simulateTransaction` (sig_verify=false,
    // replace_recent_blockhash=true) and `getMinimumBalanceForRentExemption`.
    // It never broadcasts, never signs, never touches external wallet /
    // Phantom.
    let preflight_simulator: Arc<dyn SolendPreflightSimulator> = Arc::new(
        ClawRpcPoolPreflightRpc::new(
            rpc_pool,
            claw_types::solana::CommitmentLevel::Confirmed,
        ),
    );

    // ── Phase 4C-4 signing handoff deps ─────────────────────────────────────
    //
    // `SolendSigningStore` holds the obligation Keypair + partially-signed
    // tx bytes for the signing window only (TTL-enforced). The blockhash
    // provider wraps the daemon's `BlockhashManager` so the signing
    // handoff uses a real cached recent blockhash — NOT
    // `replace_recent_blockhash`, which is preflight-only.
    let blockhash_provider: Arc<dyn RecentBlockhashProvider> =
        Arc::new(ClawBlockhashProvider::new(blockhash_manager));

    // Phase 4C-5 W3 remediation: audit sink is shared between handoff
    // events (4C-4) and submit events (4C-5) so every Solend signing /
    // submit lifecycle step is append-only recorded. Production passes
    // `Arc<AuditRepositorySink>` wrapping the shared `AuditRepository`;
    // tests pass `Arc<NullSolendAuditSink>` or a recording mock.
    let signing_deps = SolendSigningDeps {
        store: signing_store,
        blockhash_provider,
        audit: audit_sink,
        signing_lease_seconds: approval_lease_seconds,
    };

    // ── Compose + register the tool ─────────────────────────────────────────
    let solend_deposit_tool = Arc::new(
        SubmitSolendDepositTool::new(
            Arc::new(external_wallet)
                as Arc<dyn crate::tools::jupiter_swap::SessionBoundWallet>,
            assembler_seam,
            rule_config,
            approval_store,
            pending_solend_park,
            approval_lease_seconds,
        )
        .with_preflight_simulator(preflight_simulator)
        .with_signing_deps(signing_deps),
    );

    info!("solend_deposit_usdc tool active (propose + policy + park + preflight + signing handoff)");
    registry.with_tool(solend_deposit_tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_solana_core::rpc::RpcPoolConfig;

    /// Build a minimal in-memory `RpcPool` for unit tests. The pool is
    /// NEVER called in this slice — the wiring function only holds an
    /// `RpcPool` handle, constructs adapters from it, and hands them to
    /// the tool. No request is made.
    fn stub_rpc_pool() -> RpcPool {
        use claw_solana_core::rpc::EndpointConfig;
        RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://127.0.0.1:0".to_string(),
                is_write_endpoint: true,
                label: "stub".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: std::time::Duration::from_secs(30),
            request_timeout: std::time::Duration::from_secs(5),
        })
    }

    #[tokio::test]
    async fn wiring_registers_solend_deposit_usdc_in_registry() {
        let registry = ToolRegistry::new();
        let rpc_pool = stub_rpc_pool();
        let external_wallet = ExternalWalletStore::new();
        let approval_store = ApprovalStore::new();
        let park = SolendParkStore::new();

        let registry = wire_solend_deposit_tool(
            registry,
            rpc_pool.clone(),
            external_wallet,
            approval_store,
            park,
            SolendSigningStore::new(),
            Arc::new(BlockhashManager::new(rpc_pool)),
            Arc::new(crate::integrations::solend_submit::NullSolendAuditSink),
            120,
        );

        assert!(
            registry.names().iter().any(|n| n == "solend_deposit_usdc"),
            "wiring must register solend_deposit_usdc; got {:?}",
            registry.names()
        );
    }

    #[tokio::test]
    async fn wiring_preserves_only_amount_input_schema() {
        let registry = wire_solend_deposit_tool(
            ToolRegistry::new(),
            stub_rpc_pool(),
            ExternalWalletStore::new(),
            ApprovalStore::new(),
            SolendParkStore::new(),
            SolendSigningStore::new(),
            Arc::new(BlockhashManager::new(stub_rpc_pool())),
            Arc::new(crate::integrations::solend_submit::NullSolendAuditSink),
            120,
        );
        let tool = registry
            .get("solend_deposit_usdc")
            .expect("tool registered");
        let spec = tool.spec();
        let props = spec.input_schema["properties"].as_object().unwrap();
        let keys: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["amount"]);
    }

    #[tokio::test]
    async fn wiring_does_not_mutate_other_tools_in_registry() {
        // Register an unrelated tool first and assert the wiring function
        // preserves it. Uses a trivial pass-through tool to avoid dragging
        // in another integration's dependencies.
        use async_trait::async_trait;
        use claw_tool_system::errors::ToolError;
        use claw_tool_system::tool::Tool;
        use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

        struct NoopTool;
        #[async_trait]
        impl Tool for NoopTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "noop_for_wiring_test".to_string(),
                    description: "unit-test only".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: serde_json::json!({"type": "object"}),
                    required_capabilities: vec![],
                    supports_streaming: false,
                    timeout_ms: 1000,
                }
            }
            async fn execute(&self, _i: ToolInput) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    tool_name: "noop_for_wiring_test".to_string(),
                    success: true,
                    data: None,
                    error: None,
                    duration_ms: 0,
                })
            }
        }

        let registry = ToolRegistry::new().with_tool(Arc::new(NoopTool) as Arc<dyn Tool>);
        let registry = wire_solend_deposit_tool(
            registry,
            stub_rpc_pool(),
            ExternalWalletStore::new(),
            ApprovalStore::new(),
            SolendParkStore::new(),
            SolendSigningStore::new(),
            Arc::new(BlockhashManager::new(stub_rpc_pool())),
            Arc::new(crate::integrations::solend_submit::NullSolendAuditSink),
            120,
        );
        let names = registry.names();
        assert!(names.iter().any(|n| n == "noop_for_wiring_test"));
        assert!(names.iter().any(|n| n == "solend_deposit_usdc"));
    }

    // ── Phase 4C-8G-Staleness-Calibration tests ─────────────────────────────
    //
    // These tests pin the calibrated USDC oracle publish-age budget at
    // both the constant level AND through the real
    // `evaluate_lending_policy` evaluator, so a future regression on
    // either the constant OR the helper structure fails fast at lib
    // tests.

    use crate::integrations::solend::mapping::{
        map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, OracleAccountInfo,
        ReserveInput,
    };
    use crate::integrations::solend::raw::{
        decode_reserve, synth_reserve, SOLEND_NULL_ORACLE_SENTINEL_BS58,
    };
    use crate::lending::{
        evaluate_lending_policy, ChainSlot, EvaluationContext, FeedPublishFreshness,
        HardBlockReason, LendingPolicyVerdict, ProposedAction, ProtocolTag, RuleKind,
        RuleRejectionDetail,
    };
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";
    /// Conservative slot duration the policy uses internally to convert
    /// `max_publish_age` (ms) → max age (slots): `400 ms / slot`. With a
    /// 120_000 ms budget that yields 300 slots.
    const CONSERVATIVE_SLOT_MS: u64 = 400;

    fn usdc_mint_pk() -> Pubkey {
        Pubkey::from_str(USDC_MINT_BS58).unwrap()
    }

    /// Build a single-reserve, single-Pyth-feed Solend USDC snapshot
    /// where `snapshot_observed_slot` and `publish_slot` differ by
    /// exactly `slots_apart` slots. Used to simulate "publish age = N
    /// slots × 400 ms" without any RPC.
    fn snapshot_with_pyth_age_slots(
        slots_apart: u64,
    ) -> crate::lending::LendingSnapshot {
        let session_wallet = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let reserve_pubkey = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let sentinel = Pubkey::from_str(SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap();
        let pyth_owner = Pubkey::from_str(PYTH_RECEIVER_PROGRAM_BS58).unwrap();
        let collateral_mint = Pubkey::new_unique();
        let collateral_supply = Pubkey::new_unique();

        let reserve_bytes = synth_reserve(
            lending_market,
            usdc_mint_pk(),
            6,
            supply,
            pyth,
            sentinel,
            9_999_999,
            collateral_mint,
            collateral_supply,
            100,
            /*stale=*/ false,
        );
        let reserve_raw = decode_reserve(&reserve_bytes).unwrap();

        let observed = 100_000u64; // arbitrary; only the *delta* matters
        let publish = observed.saturating_sub(slots_apart);
        let inputs = FirstDepositAssemblyInputs {
            session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pubkey,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(observed),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(observed),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(publish)),
            }],
            snapshot_observed_slot: ChainSlot::new(observed),
        };
        map_snapshot_for_first_deposit(inputs).unwrap()
    }

    fn deposit_action_usdc(amount_raw: u64) -> ProposedAction {
        ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: usdc_mint_pk(),
            amount: crate::lending::UnderlyingAmount::new(amount_raw),
        }
    }

    #[test]
    fn calibrated_publish_age_constant_is_120_000_ms() {
        assert_eq!(SOLEND_USDC_MAX_ORACLE_PUBLISH_AGE_MS, 120_000);
    }

    #[test]
    fn helper_config_uses_calibrated_publish_age_and_keeps_other_fields() {
        let cfg = solend_usdc_lending_rule_config();
        assert_eq!(
            cfg.max_oracle_staleness.max_publish_age.raw(),
            SOLEND_USDC_MAX_ORACLE_PUBLISH_AGE_MS,
            "max_publish_age must be the calibrated 120_000 ms constant"
        );
        // Every other field must remain identical to the pre-calibration
        // shape — this is calibration, not a config rewrite.
        assert_eq!(cfg.require_fresh_state.max_fetch_age.raw(), 30_000);
        assert_eq!(cfg.allowed_lending_protocols.allowlist, vec![ProtocolTag::Solend]);
        assert_eq!(cfg.allowed_mints.allowlist, vec![usdc_mint_pk()]);
        assert_eq!(cfg.max_action_input_amount.per_mint_caps.len(), 1);
        let (capped_mint, cap) = &cfg.max_action_input_amount.per_mint_caps[0];
        assert_eq!(*capped_mint, usdc_mint_pk());
        assert_eq!(cap.raw(), MAX_STRUCTURAL_AMOUNT_RAW);
    }

    #[test]
    fn evaluator_passes_within_calibrated_window_at_200_slots_age() {
        // 200 slots × 400 ms = 80 000 ms. Pre-calibration (30 s) would
        // have BLOCKED this; post-calibration (120 s) must PASS the
        // oracle-staleness rule.
        let snap = snapshot_with_pyth_age_slots(200);
        let cfg = solend_usdc_lending_rule_config();
        let action = deposit_action_usdc(1_000);
        let ctx = EvaluationContext {
            session_wallet: snap.session_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &cfg, &ctx);
        assert!(
            matches!(verdict, LendingPolicyVerdict::Pass),
            "expected Pass at 80 s publish age under 120 s budget; got {verdict:?}"
        );
    }

    #[test]
    fn evaluator_passes_at_threshold_boundary_300_slots_age() {
        // 300 slots × 400 ms = 120 000 ms; equal to the budget. The
        // rule uses `age > max_slots` (strict), so the boundary passes.
        let snap = snapshot_with_pyth_age_slots(300);
        let cfg = solend_usdc_lending_rule_config();
        let action = deposit_action_usdc(1_000);
        let ctx = EvaluationContext {
            session_wallet: snap.session_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &cfg, &ctx);
        assert!(
            matches!(verdict, LendingPolicyVerdict::Pass),
            "expected Pass at exactly 120 s publish age (boundary); got {verdict:?}"
        );
    }

    #[test]
    fn evaluator_still_blocks_when_publish_age_exceeds_120_seconds() {
        // 350 slots × 400 ms = 140 000 ms — beyond the calibrated 120 s.
        // Calibration is NOT a bypass: the rule still fires.
        let snap = snapshot_with_pyth_age_slots(350);
        let cfg = solend_usdc_lending_rule_config();
        let action = deposit_action_usdc(1_000);
        let ctx = EvaluationContext {
            session_wallet: snap.session_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &cfg, &ctx);
        match verdict {
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(rule, detail)) => {
                assert!(
                    matches!(rule, RuleKind::MaxOracleStalenessMs),
                    "expected RuleKind::MaxOracleStalenessMs, got {rule:?}"
                );
                assert!(
                    matches!(detail, RuleRejectionDetail::OraclePublishAgeExceeded),
                    "expected OraclePublishAgeExceeded, got {detail:?}"
                );
            }
            other => panic!(
                "expected HardBlock(RuleRejected: MaxOracleStalenessMs/OraclePublishAgeExceeded) at 140 s; got {other:?}"
            ),
        }
    }

    #[test]
    fn calibration_does_not_relax_protocol_allowlist() {
        // Wrong protocol must still hard-block. Since the V1
        // `ProposedAction` enum currently only spans Solend, this test
        // pins the allowlist value rather than constructing a foreign
        // protocol action — which would require widening the action
        // type. We assert the allowlist is exactly `[Solend]`.
        let cfg = solend_usdc_lending_rule_config();
        assert_eq!(
            cfg.allowed_lending_protocols.allowlist.len(),
            1,
            "exactly one protocol allowed"
        );
        assert!(matches!(
            cfg.allowed_lending_protocols.allowlist[0],
            ProtocolTag::Solend
        ));
    }

    #[test]
    fn calibration_still_blocks_wrong_reserve_mint() {
        // Construct an action that targets a non-USDC mint. Snapshot
        // is fresh USDC; the action's reserve_mint != USDC. The
        // evaluator must reject before any oracle check.
        let snap = snapshot_with_pyth_age_slots(50); // healthy oracle
        let cfg = solend_usdc_lending_rule_config();
        let bogus_mint = Pubkey::new_unique();
        let action = ProposedAction::Deposit {
            protocol: ProtocolTag::Solend,
            reserve_mint: bogus_mint,
            amount: crate::lending::UnderlyingAmount::new(1_000),
        };
        let ctx = EvaluationContext {
            session_wallet: snap.session_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &cfg, &ctx);
        assert!(
            matches!(verdict, LendingPolicyVerdict::HardBlock(_)),
            "wrong mint must still hard-block; got {verdict:?}"
        );
    }

    #[test]
    fn calibration_still_blocks_amount_above_cap() {
        // Cap is `MAX_STRUCTURAL_AMOUNT_RAW = 10_000`. An action above
        // the cap must hard-block on the AmountOverCap rule, not on
        // anything oracle-related.
        let snap = snapshot_with_pyth_age_slots(50); // healthy oracle
        let cfg = solend_usdc_lending_rule_config();
        let action = deposit_action_usdc(MAX_STRUCTURAL_AMOUNT_RAW + 1);
        let ctx = EvaluationContext {
            session_wallet: snap.session_wallet,
        };
        let verdict = evaluate_lending_policy(&snap, &action, &cfg, &ctx);
        match verdict {
            LendingPolicyVerdict::HardBlock(HardBlockReason::RuleRejected(_, detail)) => {
                assert!(
                    matches!(detail, RuleRejectionDetail::AmountOverCap),
                    "expected AmountOverCap, got {detail:?}"
                );
            }
            other => panic!("expected HardBlock at over-cap amount; got {other:?}"),
        }
    }

    #[test]
    fn calibration_does_not_call_signing_or_submit_functions_directly() {
        // Static guard: this wiring module must not call signing/submit
        // functions directly. It legitimately *holds references* to the
        // signing store TYPE and the audit sink TRAIT (both passed
        // through as parameters), but actual call sites for the
        // `submit_*` and `create_*` functions belong to the consume
        // path, not wiring.
        //
        // Needles are assembled at runtime so this scanner does not
        // match its own source text.
        const SOURCE: &str = include_str!("solend_wiring.rs");
        let needles = [
            format!("{}{}", "submit_signed_solend_", "transaction"),
            format!("{}{}", "create_signing_", "handoff"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "solend_wiring.rs must not invoke `{n}` directly; it is wiring-only"
            );
        }
    }

    /// Structural guard: this module must not contain any runtime
    /// references to spawn/broadcast/signer/blockhash symbols. Patterns
    /// are built at runtime so the scanner doesn't match its own source.
    #[test]
    fn module_has_no_forbidden_wiring_references() {
        const SOURCE: &str = include_str!("solend_wiring.rs");
        // Build patterns from fragments so the scanner does not match
        // its own source text.
        let spawn = format!("{}{}{}", "tokio::", "spawn", "(");
        let route = format!("{}{}", ".route", "(");
        let send_tx = format!("{}{}", "send_", "transaction(");
        let send_raw = format!("{}{}", "send_raw_", "transaction(");
        let send_raw_v0 = format!("{}{}", "send_raw_v0_", "transaction(");
        let confirm = format!("{}{}", "confirm_", "transaction(");
        let get_bh = format!("{}{}", "get_latest_", "blockhash(");
        let new_signed = format!("{}{}", "new_signed_with_", "payer(");
        for pat in [&spawn, &route, &send_tx, &send_raw, &send_raw_v0, &confirm, &get_bh, &new_signed] {
            assert!(
                !SOURCE.contains(pat),
                "solend_wiring.rs must not contain `{pat}`"
            );
        }
    }
}
