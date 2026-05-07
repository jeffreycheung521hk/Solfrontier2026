//! Gateway-private tool implementations.
//!
//! These tools require access to gateway-level state (pipeline, approval store,
//! event bus) and cannot live in `claw-tool-system` without creating circular
//! crate dependencies. They are injected into the `ToolRegistry` at startup.

pub mod get_jupiter_quote;
pub mod get_solend_position;
pub mod get_wallet_balances;
pub mod jupiter_swap;
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
pub use signing::SubmitForSigningTool;
pub use signing::resume_after_approval;
pub use solend_deposit::SubmitSolendDepositTool;
