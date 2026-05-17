//! Session manager — creates, tracks, and closes sessions.

use dashmap::DashMap;
use std::sync::Arc;
use tracing::{info, warn};

use claw_types::{
    agent::AgentRole,
    events::{GatewayEvent, SessionClosedEvent, SessionOpenedEvent, EventHeader},
    session::{SessionId, SessionState},
};
use chrono::Utc;

use claw_observability::metrics::MetricsRegistry;
use crate::event_bus::EventBus;

/// Internal session record.
struct SessionRecord {
    _id:          SessionId,
    _state:       SessionState,
    _agent_role:  AgentRole,
    _channel:     String,
    _created_at:  chrono::DateTime<chrono::Utc>,
}

/// Thread-safe session manager.
#[derive(Clone)]
pub struct SessionManager {
    sessions:  Arc<DashMap<SessionId, SessionRecord>>,
    event_bus: EventBus,
    metrics:   Arc<MetricsRegistry>,
}

impl SessionManager {
    pub fn new(event_bus: EventBus, metrics: Arc<MetricsRegistry>) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            event_bus,
            metrics,
        }
    }

    /// Opens a new session.
    pub fn open(&self, role: AgentRole, channel: impl Into<String>) -> SessionId {
        let id = SessionId::new();
        let channel = channel.into();

        let record = SessionRecord {
            _id:         id.clone(),
            _state:      SessionState::Active,
            _agent_role: role,
            _channel:    channel.clone(),
            _created_at: Utc::now(),
        };

        self.sessions.insert(id.clone(), record);
        self.metrics.sessions_opened.increment();
        self.metrics.sessions_active.increment();
        info!(session = %id, role = %role, channel = %channel, "session opened");

        self.event_bus.publish(GatewayEvent::SessionOpened(SessionOpenedEvent {
            header:     EventHeader::new(Some(id.clone())),
            session_id: id.clone(),
            agent_role: role,
            channel,
        }));

        id
    }

    /// Closes a session.
    pub fn close(&self, id: &SessionId, reason: &str) {
        if let Some((_, _record)) = self.sessions.remove(id) {
            self.metrics.sessions_closed.increment();
            self.metrics.sessions_active.decrement();
            info!(session = %id, reason, "session closed");
            self.event_bus.publish(GatewayEvent::SessionClosed(SessionClosedEvent {
                header:     EventHeader::new(Some(id.clone())),
                session_id: id.clone(),
                reason:     reason.to_string(),
            }));
        } else {
            warn!(session = %id, "close called on unknown session");
        }
    }

    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_active(&self, id: &SessionId) -> bool {
        self.sessions.contains_key(id)
    }
}
