//! Approval request and decision types.
//!
//! An `ApprovalRequest` is issued by the pipeline when a transaction requires
//! human authorisation. It carries everything the operator needs to make an
//! informed decision. Once issued it is one-time-actionable: a second decision
//! on the same `request_id` is rejected.
//!
//! An `ApprovalDecision` is the operator's response. Approved decisions resume
//! the signing pipeline; rejected decisions are terminal for that request.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{policy::PolicyVerdict, session::SessionId, transaction::SimulationResult};

/// A pending approval request issued when the transaction pipeline reaches
/// `PipelineResult::AwaitingApproval`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Unique identifier for this approval request.
    pub id:              Uuid,

    /// The session that produced this request.
    pub session_id:      SessionId,

    /// The transaction proposal ID this approval is for.
    pub transaction_id:  Uuid,

    /// Human-readable description of what is being approved.
    pub description:     String,

    /// The policy verdict that triggered the human-approval requirement.
    pub policy_verdict:  PolicyVerdict,

    /// Simulation result attached so the operator can review costs/effects.
    pub simulation:      SimulationResult,

    /// When this request was issued.
    pub requested_at:    DateTime<Utc>,

    /// Whether this request has already been decided (one-time-actionable).
    pub decided:         bool,
}

impl ApprovalRequest {
    /// Creates a new undecided approval request.
    pub fn new(
        session_id:     SessionId,
        transaction_id: Uuid,
        description:    impl Into<String>,
        policy_verdict: PolicyVerdict,
        simulation:     SimulationResult,
    ) -> Self {
        Self {
            id:             Uuid::new_v4(),
            session_id,
            transaction_id,
            description:    description.into(),
            policy_verdict,
            simulation,
            requested_at:   Utc::now(),
            decided:        false,
        }
    }
}

/// An operator's decision on a pending approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecision {
    /// The approval request this decision responds to.
    pub request_id: Uuid,

    /// `true` = approved, `false` = rejected.
    pub approved:   bool,

    /// Optional operator note (reason for approval or rejection).
    pub note:       Option<String>,

    /// When this decision was made.
    pub decided_at: DateTime<Utc>,
}

impl ApprovalDecision {
    /// Creates an approval decision.
    pub fn approve(request_id: Uuid, note: Option<String>) -> Self {
        Self { request_id, approved: true, note, decided_at: Utc::now() }
    }

    /// Creates a rejection decision.
    pub fn reject(request_id: Uuid, note: Option<String>) -> Self {
        Self { request_id, approved: false, note, decided_at: Utc::now() }
    }
}

/// The outcome after an approval decision has been processed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// The transaction was approved and the signing pipeline will resume.
    Approved,
    /// The transaction was rejected by the operator.
    Rejected,
    /// This request was already decided — duplicate submission.
    AlreadyDecided,
    /// The request ID was not found (unknown or expired).
    NotFound,
}
