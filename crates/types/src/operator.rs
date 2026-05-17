//! Operator identity types.
//!
//! An `Operator` represents a human who can interact with the ClawSolana API.
//! Each operator has a unique ID, a bearer token, and a set of roles that
//! determine what actions they can perform (e.g., approve transactions as
//! "treasury" or "risk").
//!
//! # Anonymous fallback
//!
//! When no `[[operators]]` are configured (legacy single-token mode),
//! the system creates an anonymous operator with all roles. This preserves
//! backward compatibility.

use serde::{Deserialize, Serialize};

/// Unique identifier for an operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OperatorId(pub String);

impl OperatorId {
    pub fn anonymous() -> Self {
        Self("anonymous".to_string())
    }
}

impl std::fmt::Display for OperatorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An operator's identity, resolved from their bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorIdentity {
    /// Unique operator identifier (used in audit trail).
    pub id: OperatorId,

    /// Human-readable display name.
    pub display_name: String,

    /// Set of roles this operator holds (e.g., "risk", "treasury", "admin").
    /// Used to validate `required_approver_role` on approval requests.
    pub roles: Vec<String>,
}

impl OperatorIdentity {
    /// Creates an anonymous operator with all-role access (legacy compatibility).
    pub fn anonymous() -> Self {
        Self {
            id: OperatorId::anonymous(),
            display_name: "anonymous operator".to_string(),
            roles: vec![],
        }
    }

    /// Returns true if this operator holds the given role.
    /// Anonymous operators with empty roles list match ANY role (backward compat).
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.is_empty() || self.roles.iter().any(|r| r == role)
    }
}
