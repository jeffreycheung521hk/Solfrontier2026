//! `claw-agent-runtime` — agent orchestration layer.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod errors;
pub mod llm;
pub mod personas;
pub mod planner;
pub mod router;
pub mod session;

pub use agent::Agent;
pub use errors::AgentError;
pub use llm::{LlmClient, LlmClientRef};
pub use router::AgentRouter;
pub use session::AgentSession;
