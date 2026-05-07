//! Phase 6C-B — production wiring for the read-only copilot tools.
//!
//! Per [`DEBT.md` D-2](../../../../DEBT.md), `daemon.rs` is the
//! "wiring + main loop only" surface. This module owns the tiny
//! constructor helpers that compose `get_wallet_balances` and
//! `get_jupiter_quote` from the dependencies the daemon already holds
//! and inserts them into the caller's [`ToolRegistry`].
//!
//! # What this module does
//!
//! Pure wiring. No background tasks, no HTTP routes, no signing /
//! submit / broadcast call sites. Both functions accept the existing
//! shared dependencies and return the augmented registry — no new
//! state is owned here.
//!
//! - [`wire_get_wallet_balances_tool`] wraps a [`ClawRpcClient`] in a
//!   [`RpcWalletBalanceReader`], pairs it with the session-bound
//!   [`ExternalWalletStore`], and registers the tool.
//! - [`wire_get_jupiter_quote_tool`] wraps an existing
//!   `Arc<dyn JupiterClient>` in a [`JupiterClientQuoteSource`] and
//!   registers the tool.
//!
//! Both tools advertise `required_capabilities: vec![]` — they are
//! pure reads and impose no capability burden on the chat dispatcher.

use std::sync::Arc;

use claw_solana_core::rpc::ClawRpcClient;
use claw_tool_system::registry::ToolRegistry;

use crate::external_wallet::ExternalWalletStore;
use crate::integrations::jupiter::JupiterClient;
use crate::tools::get_jupiter_quote::{GetJupiterQuoteTool, JupiterClientQuoteSource};
use crate::tools::get_solend_position::{GetSolendPositionTool, RpcSolendPositionReader, SolendPositionReader};
use crate::tools::get_wallet_balances::{GetWalletBalancesTool, RpcWalletBalanceReader};
use crate::tools::jupiter_swap::SessionBoundWallet;
use crate::tools::preview_solend_withdraw_all::PreviewSolendWithdrawAllTool;
use crate::tools::solend_withdraw_all_usdc::SubmitSolendWithdrawAllUsdcTool;
use crate::integrations::solend_withdraw_park::SolendWithdrawAllParkStore;

/// Register the read-only `get_wallet_balances` tool in the registry.
///
/// The session-bound wallet lookup is satisfied by the same
/// [`ExternalWalletStore`] the daemon already shares with the
/// `submit_jupiter_swap` and Solend wirings — the LLM's `wallet_pubkey`
/// resolution is identical across all three tools.
pub fn wire_get_wallet_balances_tool(
    registry: ToolRegistry,
    rpc_client: ClawRpcClient,
    external_wallet: ExternalWalletStore,
) -> ToolRegistry {
    let session_wallet: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
    let reader = Arc::new(RpcWalletBalanceReader::new(rpc_client));
    let tool = Arc::new(GetWalletBalancesTool::new(session_wallet, reader));
    registry.with_tool(tool)
}

/// Register the read-only `get_jupiter_quote` tool in the registry.
///
/// Accepts any `Arc<dyn JupiterClient>` so the daemon can either share
/// the swap-tool's `HttpJupiterClient` (when `[jupiter] enabled = true`)
/// or pass a freshly constructed quote-only client (when `enabled = false`
/// — quote is still safe to expose).
pub fn wire_get_jupiter_quote_tool(
    registry: ToolRegistry,
    jupiter_client: Arc<dyn JupiterClient>,
) -> ToolRegistry {
    let source = Arc::new(JupiterClientQuoteSource::new(jupiter_client));
    let tool = Arc::new(GetJupiterQuoteTool::new(source));
    registry.with_tool(tool)
}

/// Phase 6H — Register the read-only `get_solend_position` tool.
///
/// Reuses the daemon's shared `Arc<RpcPool>` (same pool the
/// confirmation tracker and assembler use) so circuit-breaker state
/// and endpoint selection stay consistent. Discovery uses
/// `getProgramAccounts` with `dataSize == 1300` + memcmp on the owner
/// field — a bounded scan, not a wide one.
pub fn wire_get_solend_position_tool(
    registry: ToolRegistry,
    rpc_pool: Arc<claw_solana_core::rpc::RpcPool>,
    external_wallet: ExternalWalletStore,
) -> ToolRegistry {
    let session_wallet: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
    let reader: Arc<dyn SolendPositionReader> =
        Arc::new(RpcSolendPositionReader::new(rpc_pool));
    let tool = Arc::new(GetSolendPositionTool::new(session_wallet, reader));
    registry.with_tool(tool)
}

/// Phase 6I-B — Register the read-only `preview_solend_withdraw_all` tool.
///
/// Composes against the same [`SolendPositionReader`] seam Phase 6H wires
/// for `get_solend_position` (single-account `getAccountInfo` against the
/// caller-supplied `obligation_pubkey`, plus `find_obligations_for_owner`
/// for trait completeness — the preview tool only invokes
/// `get_account_data`). Exposes `required_capabilities: vec![]`. Returns
/// a structured preview only — no approval, no signing handoff, no
/// broadcast call site is added by this wiring.
pub fn wire_preview_solend_withdraw_all_tool(
    registry: ToolRegistry,
    rpc_pool: Arc<claw_solana_core::rpc::RpcPool>,
    external_wallet: ExternalWalletStore,
) -> ToolRegistry {
    let session_wallet: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
    let reader: Arc<dyn SolendPositionReader> =
        Arc::new(RpcSolendPositionReader::new(rpc_pool));
    let tool = Arc::new(PreviewSolendWithdrawAllTool::new(session_wallet, reader));
    registry.with_tool(tool)
}

/// Phase 6I-D — Register the `solend_withdraw_all_usdc` execution tool.
///
/// The tool returns `awaiting_approval` with a parked-intent-keyed
/// `approval_request_id` and inserts a [`ParkedSolendWithdrawAllIntent`][1]
/// into the supplied [`SolendWithdrawAllParkStore`]. It does NOT
/// register with `ApprovalStore`, does NOT spawn a resume task, does
/// NOT build / sign / broadcast a transaction. The follow-up slice
/// will integrate the parked store with the approval-routing pipeline
/// and add the JIT signing handoff.
///
/// [1]: crate::integrations::solend_withdraw_park::ParkedSolendWithdrawAllIntent
pub fn wire_solend_withdraw_all_usdc_tool(
    registry: ToolRegistry,
    rpc_pool: Arc<claw_solana_core::rpc::RpcPool>,
    external_wallet: ExternalWalletStore,
    park_store: SolendWithdrawAllParkStore,
) -> ToolRegistry {
    let session_wallet: Arc<dyn SessionBoundWallet> = Arc::new(external_wallet);
    let reader: Arc<dyn SolendPositionReader> =
        Arc::new(RpcSolendPositionReader::new(rpc_pool));
    let tool = Arc::new(SubmitSolendWithdrawAllUsdcTool::new(
        session_wallet,
        reader,
        park_store,
    ));
    registry.with_tool(tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::time::Duration;

    use claw_solana_core::rpc::{EndpointConfig, RpcPool, RpcPoolConfig};
    use claw_types::solana::CommitmentLevel;

    use crate::integrations::jupiter::{
        JupiterError, SwapBuildRequest, SwapBuildResponse, SwapQuoteRequest, SwapQuoteResponse,
    };

    /// Build a `ClawRpcClient` against an unreachable URL. The wiring
    /// helper does not call the network — it only constructs the tool
    /// and registers it — so the URL choice does not matter here.
    fn dummy_rpc_client() -> ClawRpcClient {
        let pool = RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://localhost:0".to_string(),
                is_write_endpoint: false,
                label: "phase6c-test".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(100),
        });
        ClawRpcClient::new(pool, CommitmentLevel::Confirmed)
    }

    /// Minimal `JupiterClient` stub — the wiring helper never invokes
    /// either method; both arms are unreachable in this test scope.
    struct StubJupiterClient;

    #[async_trait]
    impl JupiterClient for StubJupiterClient {
        async fn quote(
            &self,
            _: &SwapQuoteRequest,
        ) -> Result<SwapQuoteResponse, JupiterError> {
            unreachable!("stub never called in registry-shape test")
        }
        async fn build(
            &self,
            _: &SwapBuildRequest,
        ) -> Result<SwapBuildResponse, JupiterError> {
            unreachable!("stub never called in registry-shape test")
        }
    }

    #[test]
    fn wire_get_wallet_balances_tool_registers_under_correct_name() {
        let rpc = dummy_rpc_client();
        let wallet = ExternalWalletStore::new();
        let registry = wire_get_wallet_balances_tool(ToolRegistry::new(), rpc, wallet);
        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "get_wallet_balances"),
            "registry must contain `get_wallet_balances` after wiring; got {names:?}"
        );
    }

    #[test]
    fn wire_get_jupiter_quote_tool_registers_under_correct_name() {
        let client: Arc<dyn JupiterClient> = Arc::new(StubJupiterClient);
        let registry = wire_get_jupiter_quote_tool(ToolRegistry::new(), client);
        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "get_jupiter_quote"),
            "registry must contain `get_jupiter_quote` after wiring; got {names:?}"
        );
    }

    #[test]
    fn wire_get_solend_position_tool_registers_under_correct_name() {
        let pool = Arc::new(RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://localhost:0".to_string(),
                is_write_endpoint: false,
                label: "phase6h-test".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(100),
        }));
        let wallet = ExternalWalletStore::new();
        let registry = wire_get_solend_position_tool(ToolRegistry::new(), pool, wallet);
        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "get_solend_position"),
            "registry must contain `get_solend_position` after wiring; got {names:?}"
        );
    }

    /// Phase 6I-D wiring guard. The execution-proposal tool registers
    /// under its canonical name and composes with the park store the
    /// caller supplied.
    #[test]
    fn wire_solend_withdraw_all_usdc_tool_registers_under_correct_name() {
        let pool = Arc::new(RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://localhost:0".to_string(),
                is_write_endpoint: false,
                label: "phase6id-test".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(100),
        }));
        let wallet = ExternalWalletStore::new();
        let park = SolendWithdrawAllParkStore::new();
        let registry =
            wire_solend_withdraw_all_usdc_tool(ToolRegistry::new(), pool, wallet, park);
        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "solend_withdraw_all_usdc"),
            "registry must contain `solend_withdraw_all_usdc` after wiring; got {names:?}"
        );
    }

    /// Phase 6I-B wiring guard. Mirror of the Phase 6H test, asserting
    /// that the preview helper inserts the tool under its canonical name.
    #[test]
    fn wire_preview_solend_withdraw_all_tool_registers_under_correct_name() {
        let pool = Arc::new(RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://localhost:0".to_string(),
                is_write_endpoint: false,
                label: "phase6ib-test".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(100),
        }));
        let wallet = ExternalWalletStore::new();
        let registry =
            wire_preview_solend_withdraw_all_tool(ToolRegistry::new(), pool, wallet);
        let names = registry.names();
        assert!(
            names.iter().any(|n| n == "preview_solend_withdraw_all"),
            "registry must contain `preview_solend_withdraw_all` after wiring; got {names:?}"
        );
    }

    /// Phase 6I-B compositional guard. After wiring all read-only chat
    /// tools (Phase 6C balances + Phase 6C quote + Phase 6H scanner +
    /// Phase 6I-B preview), every name appears once and only once and
    /// the chat narrowing returns all four.
    #[test]
    fn all_read_only_chat_tools_compose_and_narrow_to_chat() {
        let rpc = dummy_rpc_client();
        let wallet = ExternalWalletStore::new();
        let stub_client: Arc<dyn JupiterClient> = Arc::new(StubJupiterClient);
        let pool = Arc::new(RpcPool::new(RpcPoolConfig {
            endpoints: vec![EndpointConfig {
                url: "http://localhost:0".to_string(),
                is_write_endpoint: false,
                label: "phase6ib-compose".to_string(),
            }],
            failure_threshold: 3,
            recovery_interval: Duration::from_secs(30),
            request_timeout: Duration::from_millis(100),
        }));

        let registry = ToolRegistry::new();
        let registry = wire_get_wallet_balances_tool(registry, rpc, wallet.clone());
        let registry = wire_get_jupiter_quote_tool(registry, stub_client);
        let registry =
            wire_get_solend_position_tool(registry, pool.clone(), wallet.clone());
        let registry =
            wire_preview_solend_withdraw_all_tool(registry, pool, wallet);

        let mut names = registry.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "get_jupiter_quote".to_string(),
                "get_solend_position".to_string(),
                "get_wallet_balances".to_string(),
                "preview_solend_withdraw_all".to_string(),
            ]
        );

        // Chat narrowing must surface all four read-only tools, and
        // must NOT surface any execution tool name.
        let narrowed = crate::runtime::chat_wiring::narrow_registry_for_chat(&registry)
            .expect("narrow_registry_for_chat must return Some when read-only tools present");
        let mut narrowed_names = narrowed.names();
        narrowed_names.sort();
        assert_eq!(
            narrowed_names,
            vec![
                "get_jupiter_quote".to_string(),
                "get_solend_position".to_string(),
                "get_wallet_balances".to_string(),
                "preview_solend_withdraw_all".to_string(),
            ]
        );
        assert!(
            !narrowed_names.contains(&"solend_withdraw_usdc".to_string()),
            "execution tool `solend_withdraw_usdc` must NOT appear in narrowed chat registry"
        );
    }

    #[test]
    fn both_helpers_compose_into_one_registry_with_both_tools() {
        let rpc = dummy_rpc_client();
        let wallet = ExternalWalletStore::new();
        let stub_client: Arc<dyn JupiterClient> = Arc::new(StubJupiterClient);

        let registry = ToolRegistry::new();
        let registry = wire_get_wallet_balances_tool(registry, rpc, wallet);
        let registry = wire_get_jupiter_quote_tool(registry, stub_client);

        let mut names = registry.names();
        names.sort();
        assert!(
            names.contains(&"get_wallet_balances".to_string()),
            "registry must contain `get_wallet_balances`; got {names:?}"
        );
        assert!(
            names.contains(&"get_jupiter_quote".to_string()),
            "registry must contain `get_jupiter_quote`; got {names:?}"
        );
    }

    /// Phase 6C invariant — the chat handler tolerates a registry that
    /// contains ONLY the read-only tools (no Solend, no Jupiter swap).
    /// `narrow_registry_for_chat` returns Some(...) as long as at least
    /// one allowlisted tool is present, so a daemon that ships with
    /// only the copilot tools still wires chat successfully.
    #[test]
    fn registry_with_only_read_only_tools_still_satisfies_chat_narrowing() {
        let rpc = dummy_rpc_client();
        let wallet = ExternalWalletStore::new();
        let stub_client: Arc<dyn JupiterClient> = Arc::new(StubJupiterClient);
        let registry = ToolRegistry::new();
        let registry = wire_get_wallet_balances_tool(registry, rpc, wallet);
        let registry = wire_get_jupiter_quote_tool(registry, stub_client);

        let narrowed = crate::runtime::chat_wiring::narrow_registry_for_chat(&registry)
            .expect(
                "narrow_registry_for_chat must return Some when at least one \
                 chat-allowlisted tool is present (read-only tools count)",
            );
        let mut names = narrowed.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "get_jupiter_quote".to_string(),
                "get_wallet_balances".to_string(),
            ]
        );
    }

    /// Source guard — this wiring module is read-only by construction.
    /// It must not import or invoke any signing / submit / broadcast /
    /// key-material call shape. Needles are split at compile time so
    /// the test does not match its own source text.
    #[test]
    fn no_signing_or_broadcast_imports_in_wiring_module() {
        const SOURCE: &str = include_str!("copilot_tools_wiring.rs");
        let needles: Vec<String> = vec![
            format!("{}{}", "send_", "transaction("),
            format!("{}{}", "send_raw_", "transaction("),
            format!("{}{}", "send_raw_v0_", "transaction("),
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", "confirm_", "transaction("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "Keypair::", "from"),
            format!("{}{}", "private_", "key"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "transaction_", "base64"),
            format!("{}{}", "to", "do!("),
            format!("{}{}", "unimplem", "ented!("),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n.as_str()),
                "copilot_tools_wiring.rs must not contain `{n}`"
            );
        }
    }
}
