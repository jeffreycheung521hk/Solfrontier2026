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

    /// If set, only an approver claiming this role may approve/reject.
    /// Derived from `PolicyAction::RequireHumanApproval { required_approver_role }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_approver_role: Option<String>,
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
        // Extract required_approver_role from the verdict if it's RequiresHumanApproval.
        let required_approver_role = match &policy_verdict {
            PolicyVerdict::RequiresHumanApproval { required_approver_role, .. } =>
                required_approver_role.clone(),
            _ => None,
        };
        Self {
            id:             Uuid::new_v4(),
            session_id,
            transaction_id,
            description:    description.into(),
            policy_verdict,
            simulation,
            requested_at:   Utc::now(),
            decided:        false,
            required_approver_role,
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

    /// The role claimed by the approver (e.g., "risk", "treasury").
    /// If the request has a `required_approver_role`, this must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver_role: Option<String>,

    /// Authenticated operator identity (resolved from bearer token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,

    /// When this decision was made.
    pub decided_at: DateTime<Utc>,
}

impl ApprovalDecision {
    /// Creates an approval decision.
    pub fn approve(request_id: Uuid, note: Option<String>) -> Self {
        Self { request_id, approved: true, note, approver_role: None, operator_id: None, decided_at: Utc::now() }
    }

    /// Creates an approval decision with a specific approver role.
    pub fn approve_as(request_id: Uuid, note: Option<String>, role: String) -> Self {
        Self { request_id, approved: true, note, approver_role: Some(role), operator_id: None, decided_at: Utc::now() }
    }

    /// Creates a rejection decision.
    pub fn reject(request_id: Uuid, note: Option<String>) -> Self {
        Self { request_id, approved: false, note, approver_role: None, operator_id: None, decided_at: Utc::now() }
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
    /// The approver's role does not match the required approver role.
    RoleMismatch {
        required: String,
        provided: Option<String>,
    },
    /// The decision was accepted but the workflow has more stages remaining.
    /// The transaction is NOT yet approved — the next stage must be decided.
    StageAdvanced {
        completed_stage: usize,
        next_stage: usize,
        next_required_role: Option<String>,
    },
    /// The decision was accepted for quorum but the stage needs more approvals.
    QuorumProgress {
        stage: usize,
        approvals_so_far: usize,
        approvals_required: u32,
    },
    /// This operator has already voted on the current stage (dedup).
    DuplicateOperator {
        operator_id: String,
        stage: usize,
    },
}

/// A stage definition in a multi-step approval chain (from policy config).
///
/// Used in `PolicyAction::RequireApprovalChain` and carried through the
/// verdict to the signing tool, which creates the corresponding
/// `ApprovalWorkflow` with `ApprovalStage` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalChainStage {
    /// The role required to approve this stage (e.g., "risk", "treasury").
    pub role: String,
    /// Human-readable description of what this stage reviews.
    #[serde(default)]
    pub description: String,
    /// Minimum number of approvals required for this stage (quorum).
    /// Defaults to 1. When > 1, multiple operators with the allowed role
    /// must each approve before the stage completes.
    #[serde(default = "default_min_approvals")]
    pub min_approvals: u32,
}

fn default_min_approvals() -> u32 { 1 }

// ── Approval Workflow State Machine ──────────────────────────────────────────
//
// Foundation for multi-step approval chains (Step 5) and quorum (Step 6).
// Step 4 establishes the state machine with single-stage support.
// The workflow state replaces the raw `bool` in the approval signaling path.

/// The lifecycle state of an approval workflow.
///
/// Used as the signaling value between `PendingSigningStore` and the
/// `resume_after_approval` task (replaces the previous `bool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalWorkflowState {
    /// Awaiting operator decision(s). Not terminal.
    Pending,
    /// All required approvals received. Terminal — signing may proceed.
    Approved,
    /// Explicitly rejected by an operator. Terminal.
    Rejected,
    /// Timed out or channel dropped. Terminal.
    Expired,
}

impl ApprovalWorkflowState {
    /// Returns `true` if this is a terminal state (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Approved | Self::Rejected | Self::Expired)
    }

    /// Returns `true` if signing should proceed.
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }

    /// Short label for audit and logging.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending  => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired  => "expired",
        }
    }
}

/// A single stage in an approval workflow.
///
/// Supports both single-approval (min_approvals=1) and quorum (min_approvals>1).
/// Any rejection at any stage is immediately terminal for the entire workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalStage {
    /// Zero-based stage index within the workflow.
    pub index: usize,
    /// Roles allowed to approve this stage. Empty = any role allowed.
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    /// Minimum number of approvals required for this stage.
    /// Default 1 (single approver). Set > 1 for quorum.
    #[serde(default = "default_min_approvals")]
    pub min_approvals: u32,
    /// All decisions recorded for this stage (supports quorum).
    /// A stage is complete when `approval_count() >= min_approvals` or any rejection.
    #[serde(default)]
    pub decisions: Vec<StageDecision>,
}

impl ApprovalStage {
    /// Number of approvals received so far.
    pub fn approval_count(&self) -> usize {
        self.decisions.iter().filter(|d| d.approved).count()
    }

    /// Returns true if the quorum is met (enough approvals).
    pub fn quorum_met(&self) -> bool {
        self.approval_count() >= self.min_approvals as usize
    }

    /// Returns true if any decision is a rejection.
    pub fn has_rejection(&self) -> bool {
        self.decisions.iter().any(|d| !d.approved)
    }

    /// Returns true if this stage still needs more decisions.
    pub fn is_pending(&self) -> bool {
        !self.quorum_met() && !self.has_rejection()
    }

    /// Returns true if a given operator has already voted on this stage.
    pub fn has_operator_voted(&self, operator_id: &str) -> bool {
        self.decisions.iter().any(|d| d.operator_id.as_deref() == Some(operator_id))
    }
}

/// A recorded decision for one stage of an approval workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDecision {
    /// `true` = approved, `false` = rejected.
    pub approved: bool,
    /// Authenticated operator who made this decision.
    pub operator_id: Option<String>,
    /// Role claimed or verified for this decision.
    pub approver_role: Option<String>,
    /// When this decision was made.
    pub decided_at: DateTime<Utc>,
}

/// A durable approval workflow.
///
/// Tracks the full lifecycle of an approval request from creation to
/// terminal state. Persisted to SQLite for restart recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalWorkflow {
    /// The approval request ID (same as `ApprovalRequest.id`).
    pub request_id: Uuid,
    /// The session that owns this workflow.
    pub session_id: SessionId,
    /// Current workflow state.
    pub state: ApprovalWorkflowState,
    /// Ordered list of approval stages. Step 4: always exactly one stage.
    pub stages: Vec<ApprovalStage>,
    /// When the workflow was created.
    pub created_at: DateTime<Utc>,
    /// When the workflow state last changed.
    pub updated_at: DateTime<Utc>,
    /// When this workflow expires (becomes terminal Expired if still Pending).
    /// None = no explicit expiry (blockhash expiry is the implicit bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl ApprovalWorkflow {
    /// Creates a new single-stage workflow (backward-compatible default).
    pub fn single_stage(
        request_id: Uuid,
        session_id: SessionId,
        required_role: Option<String>,
    ) -> Self {
        let allowed_roles = match required_role {
            Some(r) => vec![r],
            None => vec![],
        };
        let now = Utc::now();
        Self {
            request_id,
            session_id,
            state: ApprovalWorkflowState::Pending,
            stages: vec![ApprovalStage {
                index: 0,
                allowed_roles,
                min_approvals: 1,
                decisions: vec![],
            }],
            created_at: now,
            updated_at: now,
            expires_at: None,
        }
    }

    /// Sets the lease duration from now. Returns self for chaining.
    pub fn with_lease_seconds(mut self, seconds: u64) -> Self {
        if seconds > 0 {
            self.expires_at = Some(Utc::now() + chrono::Duration::seconds(seconds as i64));
        }
        self
    }

    /// Returns true if this workflow has expired.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() > exp,
            None => false,
        }
    }

    /// Returns the first stage that still needs decisions (not yet met quorum and not rejected).
    pub fn current_stage(&self) -> Option<&ApprovalStage> {
        self.stages.iter().find(|s| s.is_pending())
    }

    /// Returns a mutable reference to the current pending stage.
    fn current_stage_mut(&mut self) -> Option<&mut ApprovalStage> {
        self.stages.iter_mut().find(|s| s.is_pending())
    }

    /// Applies a decision to the current stage and transitions the workflow state.
    ///
    /// Returns `true` if the decision was recorded.
    /// Returns `false` if the workflow was already terminal or no pending stage exists.
    pub fn apply_decision(&mut self, decision: StageDecision) -> bool {
        if self.state.is_terminal() {
            return false;
        }

        // Find the index of the current pending stage.
        let stage_idx = match self.stages.iter().position(|s| s.is_pending()) {
            Some(i) => i,
            None => return false,
        };

        let approved = decision.approved;
        self.stages[stage_idx].decisions.push(decision);
        self.updated_at = Utc::now();

        if !approved {
            // Any rejection is terminal for the entire workflow.
            self.state = ApprovalWorkflowState::Rejected;
        } else if self.stages[stage_idx].quorum_met() {
            // Current stage quorum met — check if all stages are done.
            if self.stages.iter().all(|s| s.quorum_met()) {
                self.state = ApprovalWorkflowState::Approved;
            }
            // Otherwise: more stages to go, state stays Pending.
        }
        // Approval recorded but quorum not yet met: state stays Pending.

        true
    }

    /// Marks the workflow as expired. Returns `true` if it was still pending.
    pub fn expire(&mut self) -> bool {
        if self.state == ApprovalWorkflowState::Pending {
            self.state = ApprovalWorkflowState::Expired;
            self.updated_at = Utc::now();
            true
        } else {
            false
        }
    }
}
