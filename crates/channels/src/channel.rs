//! The `Channel` trait — the common interface all I/O adapters must implement.
//!
//! Each channel translates its native I/O into the unified
//! `InboundMessage` / `OutboundMessage` types from `claw-types`.
//!
//! Channels are always-on: they run as supervised tokio tasks and produce
//! messages into the gateway's inbound queue.

use async_trait::async_trait;

use claw_types::{messages::{InboundMessage, OutboundMessage}};

use crate::errors::ChannelError;

/// The channel identifier string (e.g. "cli", "telegram", "slack").
pub type ChannelId = String;

/// A bidirectional I/O adapter.
///
/// Implementations translate native input into `InboundMessage` values
/// and write `OutboundMessage` values to native output.
///
/// The `run` method is called once and is expected to loop forever
/// (or until the channel is closed). It sends inbound messages over
/// the provided `tokio::sync::mpsc::Sender` and receives outbound
/// messages from the provided `tokio::sync::mpsc::Receiver`.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Returns the stable identifier for this channel (e.g. "cli").
    fn id(&self) -> ChannelId;

    /// Sends an outbound message to this channel's output.
    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError>;

    /// Receives the next inbound message from this channel's input.
    /// Blocks until a message is available or the channel is closed.
    async fn recv(&mut self) -> Result<InboundMessage, ChannelError>;
}
