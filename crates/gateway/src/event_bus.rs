//! The gateway event bus.
//!
//! A `tokio::sync::broadcast` channel carrying `GatewayEvent`s.
//! All significant system events flow through here.
//!
//! Consumers receive events by calling `subscribe()` and then
//! awaiting on the returned `Receiver`. Slow consumers will receive
//! `RecvError::Lagged` when they fall too far behind.

use tokio::sync::broadcast;

use claw_types::events::GatewayEvent;

/// The configured channel capacity. Size this to accommodate burst events
/// without dropping on slow consumers during normal operation.
const BUS_CAPACITY: usize = 1024;

/// The event bus handle. Clone freely.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<GatewayEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BUS_CAPACITY);
        Self { sender }
    }

    /// Subscribes to the event bus. Returns a receiver that will get
    /// all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.sender.subscribe()
    }

    /// Publishes an event. Returns the number of active receivers.
    /// Non-blocking: if no receivers exist, the event is silently dropped.
    pub fn publish(&self, event: GatewayEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Returns the number of active receivers.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
