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
