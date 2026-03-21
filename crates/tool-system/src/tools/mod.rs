//! Built-in tool implementations.

pub mod chain;
pub mod protocols;
pub mod simulation;
pub mod transactions;
pub mod transfer;
pub mod wallet;

use std::sync::Arc;

use claw_solana_core::{rpc::ClawRpcClient, SimulationClient};

use crate::{registry::ToolRegistry, tool::Tool};

/// Creates the default tool registry with all built-in tools registered.
///
/// # Included tools
///
/// **Chain reads**: `get_wallet_balance`, `get_token_accounts`, `get_account_info`,
/// `get_slot`, `get_recent_transactions`
///
/// **Transaction building**: `build_transfer` (unsigned only), `simulate_transaction`
///
/// **Orca read-only intelligence**: `orca_get_whirlpool`, `orca_get_quote`, `orca_check_pool`
/// (no swap execution — read-only protocol intelligence for agent reasoning)
pub fn default_registry(
    rpc: ClawRpcClient,
    sim: SimulationClient,
) -> ToolRegistry {
    let tools: Vec<Arc<dyn Tool>> = vec![
        // ── Chain read tools ─────────────────────────────────────────────
        Arc::new(wallet::GetWalletBalanceTool::new(rpc.clone())),
        Arc::new(wallet::GetTokenAccountsTool::new(rpc.clone())),
        Arc::new(chain::GetAccountInfoTool::new(rpc.clone())),
        Arc::new(chain::GetSlotTool::new(rpc.clone())),
        Arc::new(transactions::GetRecentTransactionsTool::new(rpc.clone())),

        // ── Transaction building ──────────────────────────────────────────
        Arc::new(simulation::SimulateTransactionTool::new(sim)),
        Arc::new(transfer::BuildTransferTool::new(rpc.clone())),

        // ── Orca read-only protocol intelligence ──────────────────────────
        // These tools fetch and decode Orca Whirlpool on-chain state.
        // They are READ-ONLY — no swap execution is possible from these tools.
        Arc::new(protocols::orca::OrcaGetWhirlpoolTool::new(rpc.clone())),
        Arc::new(protocols::orca::OrcaGetQuoteTool::new(rpc.clone())),
        Arc::new(protocols::orca::OrcaPoolAllowlistTool::new(rpc.clone(), vec![])),
    ];

    ToolRegistry::from_tools(tools)
}
