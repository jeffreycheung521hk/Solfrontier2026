//! Capability-based permission model for tools.
//!
//! # Design
//!
//! `Capability` is a typed enum representing all operations a session can be
//! granted. Each tool declares which capabilities it requires via `ToolSpec.required_capabilities`
//! (stored as `Vec<String>` for serialization compatibility), but enforcement
//! at the dispatch site uses the typed `Capability` enum.
//!
//! # Enforcement point
//!
//! The `CapabilitySet` is checked in `ToolDispatcher::dispatch()` before any
//! tool execution. A tool without the required capabilities returns a
//! `ToolError::PermissionDenied` — it is never executed.
//!
//! # Security-sensitive capabilities
//!
//! `SignTransaction` and `SendTransaction` are the most critical. These are
//! never in the default capability set. They must be explicitly granted by the
//! policy engine after simulation and policy evaluation pass.
//!
//! # Per-role grants
//!
//! `CapabilitySet::for_role()` returns the least-privilege set for each `AgentRole`.
//! The gateway passes the session's role at construction time so every session
//! is born with the narrowest capability set that still lets it do its job.

use std::collections::HashSet;

use claw_types::agent::AgentRole;

/// The capabilities a session can hold, scoped to a specific operation class.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    ReadWallet,
    ReadChain,
    BuildTransaction,
    SimulateTransaction,
    /// Authorizes the agent to *propose* a transaction for signing via the
    /// gateway pipeline. The pipeline enforces simulation → policy → approval
    /// before any signing occurs. This is NOT the same as `SignTransaction`:
    /// the agent can propose, but the gateway infrastructure controls signing.
    ProposeSigning,
    /// Highly sensitive: authorizes the agent to sign transactions.
    /// Never in the default capability set. Must be explicitly granted.
    SignTransaction,
    /// Highly sensitive: authorizes the agent to submit signed transactions.
    /// Never in the default capability set. Must be explicitly granted.
    SendTransaction,
    ManageSubscriptions,
    ManageWallets,
    ManagePolicies,
    ReadAuditLog,
    SystemAdmin,
}

impl Capability {
    /// Returns the canonical string key for this capability.
    /// Must match what tools declare in `ToolSpec.required_capabilities`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::ReadWallet          => "read_wallet",
            Capability::ReadChain           => "read_chain",
            Capability::BuildTransaction    => "build_transaction",
            Capability::SimulateTransaction => "simulate_transaction",
            Capability::ProposeSigning       => "propose_signing",
            Capability::SignTransaction     => "sign_transaction",
            Capability::SendTransaction     => "send_transaction",
            Capability::ManageSubscriptions => "manage_subscriptions",
            Capability::ManageWallets       => "manage_wallets",
            Capability::ManagePolicies      => "manage_policies",
            Capability::ReadAuditLog        => "read_audit_log",
            Capability::SystemAdmin         => "system_admin",
        }
    }

    /// Parses a capability from its string key.
    /// Returns `None` for unknown capability strings.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "read_wallet"          => Some(Capability::ReadWallet),
            "read_chain"           => Some(Capability::ReadChain),
            "build_transaction"    => Some(Capability::BuildTransaction),
            "simulate_transaction" => Some(Capability::SimulateTransaction),
            "propose_signing"      => Some(Capability::ProposeSigning),
            "sign_transaction"     => Some(Capability::SignTransaction),
            "send_transaction"     => Some(Capability::SendTransaction),
            "manage_subscriptions" => Some(Capability::ManageSubscriptions),
            "manage_wallets"       => Some(Capability::ManageWallets),
            "manage_policies"      => Some(Capability::ManagePolicies),
            "read_audit_log"       => Some(Capability::ReadAuditLog),
            "system_admin"         => Some(Capability::SystemAdmin),
            _                      => None,
        }
    }

    /// Returns `true` if this capability is considered security-sensitive.
    /// These are capabilities that cannot be granted by default and require
    /// explicit operator configuration.
    pub fn is_security_sensitive(&self) -> bool {
        matches!(
            self,
            Capability::SignTransaction
                | Capability::SendTransaction
                | Capability::ManageWallets
                | Capability::ManagePolicies
                | Capability::SystemAdmin
        )
    }
}

/// A set of capabilities granted to a session.
///
/// Used by `ToolDispatcher` to check whether a tool can be invoked.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    granted: HashSet<Capability>,
}

impl CapabilitySet {
    /// Creates a capability set with no granted capabilities.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the default capability set for agent sessions.
    ///
    /// Grants read-only and non-sensitive capabilities.
    /// Does NOT grant `SignTransaction`, `SendTransaction`, `ManageWallets`,
    /// `ManagePolicies`, or `SystemAdmin`.
    pub fn default_agent() -> Self {
        let mut set = Self::empty();
        set.grant(Capability::ReadWallet);
        set.grant(Capability::ReadChain);
        set.grant(Capability::BuildTransaction);
        set.grant(Capability::SimulateTransaction);
        set.grant(Capability::ManageSubscriptions);
        set
    }

    /// Returns a fully-privileged capability set.
    ///
    /// # Warning
    ///
    /// This grants ALL capabilities including `SignTransaction` and `SendTransaction`.
    /// Only use in devnet/testnet contexts where the policy engine also approves.
    pub fn all() -> Self {
        let mut set = Self::default_agent();
        set.grant(Capability::ProposeSigning);
        set.grant(Capability::SignTransaction);
        set.grant(Capability::SendTransaction);
        set.grant(Capability::ManageWallets);
        set.grant(Capability::ManagePolicies);
        set.grant(Capability::ReadAuditLog);
        set.grant(Capability::SystemAdmin);
        set
    }

    /// Returns the least-privilege capability set for the given `AgentRole`.
    ///
    /// # Role grants
    ///
    /// | Role       | Capabilities |
    /// |------------|-------------|
    /// | Research   | ReadWallet, ReadChain, SimulateTransaction |
    /// | Execution  | Research + BuildTransaction + ProposeSigning (Sign/Send require policy approval) |
    /// | Risk       | ReadChain, ReadWallet, ManageSubscriptions |
    /// | Ops        | ReadWallet, ReadChain, BuildTransaction, SimulateTransaction, ManageSubscriptions, ManageWallets, ManagePolicies, ReadAuditLog |
    ///
    /// `SignTransaction` and `SendTransaction` are never granted at session construction.
    /// They must be explicitly granted by the policy engine for a specific transaction.
    pub fn for_role(role: AgentRole) -> Self {
        let mut set = Self::empty();
        match role {
            AgentRole::Research => {
                set.grant(Capability::ReadWallet);
                set.grant(Capability::ReadChain);
                set.grant(Capability::SimulateTransaction);
            }
            AgentRole::Execution => {
                set.grant(Capability::ReadWallet);
                set.grant(Capability::ReadChain);
                set.grant(Capability::SimulateTransaction);
                set.grant(Capability::BuildTransaction);
                set.grant(Capability::ProposeSigning);
            }
            AgentRole::Risk => {
                set.grant(Capability::ReadChain);
                set.grant(Capability::ReadWallet);
                set.grant(Capability::ManageSubscriptions);
            }
            AgentRole::Ops => {
                set.grant(Capability::ReadWallet);
                set.grant(Capability::ReadChain);
                set.grant(Capability::BuildTransaction);
                set.grant(Capability::SimulateTransaction);
                set.grant(Capability::ManageSubscriptions);
                set.grant(Capability::ManageWallets);
                set.grant(Capability::ManagePolicies);
                set.grant(Capability::ReadAuditLog);
                // NOTE: SignTransaction and SendTransaction intentionally excluded.
            }
        }
        set
    }

    pub fn grant(&mut self, cap: Capability) {
        self.granted.insert(cap);
    }

    pub fn revoke(&mut self, cap: &Capability) {
        self.granted.remove(cap);
    }

    /// Returns `true` if the given capability is in this set.
    pub fn has(&self, cap: &Capability) -> bool {
        self.granted.contains(cap)
    }

    /// Checks whether all capabilities declared in a tool's `required_capabilities`
    /// (as strings) are present in this set.
    ///
    /// Returns `Ok(())` if all requirements are met.
    /// Returns `Err(Vec<String>)` listing the missing capability strings.
    pub fn check_required(&self, required: &[String]) -> Result<(), Vec<String>> {
        let mut missing = Vec::new();
        for req in required {
            match Capability::from_str(req) {
                Some(cap) => {
                    if !self.has(&cap) {
                        missing.push(req.clone());
                    }
                }
                None => {
                    // Unknown capability string — treat as missing (fail-closed).
                    missing.push(format!("unknown:{req}"));
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_cannot_sign_or_send() {
        let caps = CapabilitySet::default_agent();
        assert!(
            !caps.has(&Capability::SignTransaction),
            "default agent must NOT have SignTransaction"
        );
        assert!(
            !caps.has(&Capability::SendTransaction),
            "default agent must NOT have SendTransaction"
        );
    }

    #[test]
    fn default_agent_can_read_and_simulate() {
        let caps = CapabilitySet::default_agent();
        assert!(caps.has(&Capability::ReadChain));
        assert!(caps.has(&Capability::ReadWallet));
        assert!(caps.has(&Capability::SimulateTransaction));
        assert!(caps.has(&Capability::BuildTransaction));
    }

    #[test]
    fn check_required_fails_closed_on_unknown_capability() {
        let caps = CapabilitySet::default_agent();
        let result = caps.check_required(&["definitely_not_a_real_capability".to_string()]);
        assert!(result.is_err(), "unknown capability strings must be treated as missing");
    }

    #[test]
    fn check_required_passes_for_granted_capabilities() {
        let caps = CapabilitySet::default_agent();
        let result = caps.check_required(&["read_chain".to_string(), "simulate_transaction".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn security_sensitive_flag_is_set_correctly() {
        assert!(Capability::SignTransaction.is_security_sensitive());
        assert!(Capability::SendTransaction.is_security_sensitive());
        assert!(Capability::ManageWallets.is_security_sensitive());
        assert!(!Capability::ReadChain.is_security_sensitive());
        assert!(!Capability::SimulateTransaction.is_security_sensitive());
    }

    #[test]
    fn for_role_research_is_read_only() {
        let caps = CapabilitySet::for_role(AgentRole::Research);
        assert!(caps.has(&Capability::ReadChain));
        assert!(caps.has(&Capability::ReadWallet));
        assert!(caps.has(&Capability::SimulateTransaction));
        assert!(!caps.has(&Capability::BuildTransaction));
        assert!(!caps.has(&Capability::SignTransaction));
        assert!(!caps.has(&Capability::SendTransaction));
    }

    #[test]
    fn for_role_execution_can_build_but_not_sign() {
        let caps = CapabilitySet::for_role(AgentRole::Execution);
        assert!(caps.has(&Capability::BuildTransaction));
        assert!(caps.has(&Capability::SimulateTransaction));
        assert!(!caps.has(&Capability::SignTransaction));
        assert!(!caps.has(&Capability::SendTransaction));
    }

    #[test]
    fn for_role_risk_is_read_and_subscribe() {
        let caps = CapabilitySet::for_role(AgentRole::Risk);
        assert!(caps.has(&Capability::ReadChain));
        assert!(caps.has(&Capability::ReadWallet));
        assert!(caps.has(&Capability::ManageSubscriptions));
        assert!(!caps.has(&Capability::BuildTransaction));
        assert!(!caps.has(&Capability::SignTransaction));
    }

    #[test]
    fn for_role_execution_can_propose_signing() {
        let caps = CapabilitySet::for_role(AgentRole::Execution);
        assert!(caps.has(&Capability::ProposeSigning),
            "Execution role must be able to propose transactions for signing");
        assert!(!caps.has(&Capability::SignTransaction),
            "Execution role must NOT have direct SignTransaction");
        assert!(!caps.has(&Capability::SendTransaction),
            "Execution role must NOT have direct SendTransaction");
    }

    #[test]
    fn propose_signing_is_not_security_sensitive() {
        assert!(!Capability::ProposeSigning.is_security_sensitive(),
            "ProposeSigning is gated by the pipeline, not a direct signing authority");
    }

    #[test]
    fn for_role_ops_never_gets_sign_or_send() {
        let caps = CapabilitySet::for_role(AgentRole::Ops);
        assert!(caps.has(&Capability::ManageWallets));
        assert!(caps.has(&Capability::ManagePolicies));
        assert!(caps.has(&Capability::ReadAuditLog));
        assert!(!caps.has(&Capability::SignTransaction),
            "Ops must NOT have SignTransaction at session construction");
        assert!(!caps.has(&Capability::SendTransaction),
            "Ops must NOT have SendTransaction at session construction");
    }
}
