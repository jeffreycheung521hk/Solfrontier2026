//! CLI channel adapter.
//!
//! Reads from stdin line-by-line, treats each line as an agent command,
//! and writes responses to stdout. This is the V1 operator interface.
//!
//! Each `CliChannel` owns a `SessionId`. The operator creates one session
//! per REPL invocation. For multi-session support, run multiple `clawd`
//! client instances or use the HTTP API.

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};
use uuid::Uuid;

use claw_types::{
    agent::{AgentCommand, AgentRole},
    messages::{InboundMessage, MessageContent, OutboundContent, OutboundMessage},
    session::SessionId,
};

use crate::{channel::Channel, errors::ChannelError};

/// A line-oriented stdin/stdout channel for the local operator.
///
/// **Responsibility:** Read user text from stdin, wrap it into
/// `InboundMessage(MessageContent::Command)`, and write agent responses
/// or approval requests to stdout.
///
/// **What does NOT belong here:** session routing, agent selection,
/// policy evaluation, or Solana logic.
pub struct CliChannel {
    session_id: SessionId,
    role:       AgentRole,
    stdin:      BufReader<tokio::io::Stdin>,
}

impl CliChannel {
    pub fn new(session_id: SessionId, role: AgentRole) -> Self {
        Self {
            session_id,
            role,
            stdin: BufReader::new(tokio::io::stdin()),
        }
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn id(&self) -> crate::channel::ChannelId {
        "cli".to_string()
    }

    async fn recv(&mut self) -> Result<InboundMessage, ChannelError> {
        let mut line = String::new();

        let bytes_read = self.stdin.read_line(&mut line).await?;

        if bytes_read == 0 {
            // EOF — operator closed stdin
            return Err(ChannelError::Closed);
        }

        let text = line.trim().to_string();
        if text.is_empty() {
            // Empty line — produce a minimal no-op command that the agent
            // will handle gracefully (it'll just respond with a prompt).
            let cmd = AgentCommand {
                id:          Uuid::new_v4(),
                text:        String::new(),
                parameters:  Default::default(),
                target_role: Some(self.role),
            };
            return Ok(InboundMessage::new(
                self.session_id.clone(),
                "cli",
                MessageContent::Command(cmd),
            ));
        }

        debug!(text = %text, "cli received input");

        // Parse special control commands starting with '/'
        if let Some(msg) = parse_control_command(&text, &self.session_id) {
            return Ok(msg);
        }

        let cmd = AgentCommand {
            id:          Uuid::new_v4(),
            text,
            parameters:  Default::default(),
            target_role: Some(self.role),
        };

        Ok(InboundMessage::new(
            self.session_id.clone(),
            "cli",
            MessageContent::Command(cmd),
        ))
    }

    async fn send(&self, message: OutboundMessage) -> Result<(), ChannelError> {
        let mut stdout = tokio::io::stdout();

        let output = format_outbound(&message);
        stdout.write_all(output.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;

        Ok(())
    }
}

/// Parse `/command` prefixed control messages from the CLI.
fn parse_control_command(text: &str, session_id: &SessionId) -> Option<InboundMessage> {
    use claw_types::messages::SystemControlMessage;

    match text {
        "/exit" | "/quit" | "/q" => Some(InboundMessage::new(
            session_id.clone(),
            "cli",
            MessageContent::SystemControl(SystemControlMessage::Shutdown),
        )),
        "/reload" => Some(InboundMessage::new(
            session_id.clone(),
            "cli",
            MessageContent::SystemControl(SystemControlMessage::ReloadConfig),
        )),
        "/sessions" => Some(InboundMessage::new(
            session_id.clone(),
            "cli",
            MessageContent::SystemControl(SystemControlMessage::ListSessions),
        )),
        "/wallets" => Some(InboundMessage::new(
            session_id.clone(),
            "cli",
            MessageContent::SystemControl(SystemControlMessage::ListWallets),
        )),
        "/subs" | "/subscriptions" => Some(InboundMessage::new(
            session_id.clone(),
            "cli",
            MessageContent::SystemControl(SystemControlMessage::ListSubscriptions),
        )),
        _ => None,
    }
}

/// Formats an `OutboundMessage` for display in the terminal.
fn format_outbound(msg: &OutboundMessage) -> String {
    match &msg.content {
        OutboundContent::Text(t) => t.clone(),

        OutboundContent::AgentResponse(r) => {
            let mut out = r.text.clone();
            if !r.tool_calls.is_empty() {
                out.push_str("\n\n[Tools used:");
                for tc in &r.tool_calls {
                    out.push_str(&format!(" {}({}ms)", tc.tool_name, tc.duration_ms));
                }
                out.push(']');
            }
            out
        }

        OutboundContent::ApprovalRequest(req) => {
            format!(
                "\n⚠  APPROVAL REQUIRED\n{}\n\n[Type 'yes' to approve or 'no' to reject]",
                req.explanation
            )
        }

        OutboundContent::Alert(alert) => {
            format!("\n🔔 ALERT [{}]: {}", alert.severity, alert.message)
        }

        OutboundContent::Error(e) => {
            format!("ERROR [{}]: {}", e.code, e.message)
        }

        OutboundContent::StreamChunk(chunk) => {
            if chunk.is_final {
                chunk.text.clone()
            } else {
                chunk.text.clone()
            }
        }
    }
}
