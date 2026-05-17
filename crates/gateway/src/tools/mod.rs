//! Gateway-private tool implementations.
//!
//! These tools require access to gateway-level state (pipeline, approval store,
//! event bus) and cannot live in `claw-tool-system` without creating circular
//! crate dependencies. They are injected into the `ToolRegistry` at startup.

pub mod get_jupiter_quote;
pub mod get_solend_position;
pub mod get_wallet_balances;
pub mod jupiter_swap;
// Phase 6I-A — read-only preview for Solend withdraw-all against a
// Phase 6H-discovered obligation. Module is `pub mod` so a future slice
// can register the tool, but the tool is intentionally NOT yet added to
// `runtime::chat_wiring::CHAT_TOOL_ALLOWLIST` and not yet registered in
// the production tool registry.
pub mod preview_solend_withdraw_all;
pub mod signing;
pub mod solend_deposit;
// Phase 6I-D — first execution-side surface for Solend withdraw.
// Strictly bounded to withdraw-all from one explicit Phase 6H-discovered
// obligation. Returns `awaiting_approval` with a parked intent; does
// NOT register with the daemon-wide ApprovalStore yet, does NOT spawn
// a resume task, does NOT build / sign / broadcast a transaction. The
// withdraw-execution substrate (`integrations::solend::withdraw` +
// `integrations::solend_withdraw_tx_plan`) remains `#[cfg(test)]` gated.
pub mod solend_withdraw_all_usdc;

pub use get_jupiter_quote::{
    GetJupiterQuoteTool, JupiterClientQuoteSource, JupiterQuoteSource,
};
pub use get_solend_position::{
    GetSolendPositionTool, SolendPositionReader,
};
pub use get_wallet_balances::{
    GetWalletBalancesTool, TokenAccountSnapshot, WalletBalanceReader,
};
pub use jupiter_swap::SubmitJupiterSwapTool;
pub use preview_solend_withdraw_all::PreviewSolendWithdrawAllTool;
pub use signing::SubmitForSigningTool;
pub use signing::resume_after_approval;
pub use solend_deposit::SubmitSolendDepositTool;
pub use solend_withdraw_all_usdc::SubmitSolendWithdrawAllUsdcTool;
