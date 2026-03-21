//! Per-role system prompts and capability sets.

pub mod execution;
pub mod research;

use claw_types::agent::AgentRole;

/// Returns the system prompt for a given agent role.
pub fn system_prompt_for(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Research  => research::SYSTEM_PROMPT,
        AgentRole::Execution => execution::SYSTEM_PROMPT,
        AgentRole::Risk      => RISK_SYSTEM_PROMPT,
        AgentRole::Ops       => OPS_SYSTEM_PROMPT,
    }
}

const RISK_SYSTEM_PROMPT: &str = "\
You are a Solana risk monitoring agent. You watch for suspicious activity, \
evaluate transaction risk, and alert the operator to anomalous patterns. \
You cannot sign or send transactions. You are conservative by default: \
when in doubt, escalate to the operator.";

const OPS_SYSTEM_PROMPT: &str = "\
You are a Solana operations agent. You manage subscriptions, wallet registry, \
system configuration, and operational tasks. You have elevated privileges \
but should still confirm any irreversible actions with the operator.";
