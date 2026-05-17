//! `claw-channels` — channel adapters for ClawSolana.
//!
//! A channel is a bidirectional I/O adapter that translates a specific
//! interface (CLI, Telegram, Slack, HTTP WebSocket) into the gateway's
//! unified `InboundMessage` / `OutboundMessage` schema.
//!
//! V1 ships with the CLI channel only. Future channels implement the
//! `Channel` trait and are registered in the gateway's channel router.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod channel;
pub mod cli;
pub mod errors;

pub use channel::{Channel, ChannelId};
pub use cli::CliChannel;
pub use errors::ChannelError;
