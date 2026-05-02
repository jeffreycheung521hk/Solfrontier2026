// Wire-compatible TypeScript types mirroring the Rust serde shapes in
// crates/types/src/{approval,policy,transaction,session}.rs.
// Field names match serde output so fixtures work unchanged against the live API.

export type Uuid = string;
export type IsoDate = string; // RFC3339 / chrono::DateTime<Utc>
export type Lamports = number;
export type SessionId = string;

export type SolanaNetwork = "mainnet-beta" | "devnet" | "testnet" | "localnet";

// ── Transaction ──────────────────────────────────────────────────────────────

export interface AccountRole {
  pubkey: string;
  label?: string | null;
  is_signer: boolean;
  is_writable: boolean;
}

export interface TokenTransfer {
  mint: string;
  amount: number;
  decimals?: number | null;
  source: string;
  destination: string;
}

export interface InstructionSummary {
  program_id: string;
  program_name?: string | null;
  description: string;
  transfer_lamports?: Lamports | null;
  token_transfer?: TokenTransfer | null;
  is_legacy_token_transfer?: boolean;
  accounts: AccountRole[];
}

export interface AccountDiff {
  pubkey: string;
  lamports_before?: Lamports | null;
  lamports_after?: Lamports | null;
  data_changed: boolean;
}

export interface SimulationResult {
  success: boolean;
  error?: string | null;
  compute_units_used?: number | null;
  logs: string[];
  return_data?: string | null;
  account_diffs: AccountDiff[];
  fee_lamports?: Lamports | null;
}

export interface TransactionProposal {
  id: Uuid;
  session_id: SessionId;
  wallet_pubkey: string;
  network: SolanaNetwork;
  description: string;
  transaction_b64: string;
  instructions_summary: InstructionSummary[];
  created_at: IsoDate;
}

// ── Policy ───────────────────────────────────────────────────────────────────

// Rust serializes unit-variant conditions as bare strings (e.g. "Always")
// and tagged variants as objects (e.g. { type: "NetworkIn", networks: [...] }).
// Use normalizePolicyRule() in api.ts to coerce bare strings into { type }.
// Field names match the Rust serde output — note `threshold` not `sol`.
export type PolicyCondition =
  | { type: "Always" }
  | { type: "NetworkIn"; networks: SolanaNetwork[] }
  | { type: "ProgramNotInAllowlist" }
  | { type: "DestinationInDenylist" }
  | { type: "AmountExceedsLamports"; threshold: Lamports }
  | { type: "CostExceedsSol"; threshold: number }
  | { type: "DailySpendExceedsSol"; threshold: number }
  | { type: "SimulationNotPassed" }
  | { type: "TokenAmountExceeds"; mint: string; threshold: number }
  | { type: "MintNotInAllowlist"; allowed_mints: string[] }
  | { type: "OutsideAllowedHours"; start_hour: number; end_hour: number; allowed_days: number[]; utc_offset_hours: number }
  | { type: "LegacyTokenTransferPresent" };

export type PolicyAction =
  | { type: "Approve" }
  | { type: "RequireHumanApproval"; reason: string; required_approver_role?: string | null }
  | { type: "RequireApprovalChain"; reason: string; stages: ApprovalChainStage[] }
  | { type: "Reject"; reason: string };

export interface PolicyRule {
  name: string;
  description: string;
  condition: PolicyCondition;
  action: PolicyAction;
}

// PolicyVerdict uses `#[serde(tag = "verdict", rename_all = "snake_case")]`
// on the Rust side, so the discriminator is `verdict` (not `type`) and
// variant names are snake_case.
export type PolicyVerdict =
  | { verdict: "approved"; rule_name: string }
  | {
      verdict: "requires_human_approval";
      reason: string;
      rule_name: string;
      required_approver_role?: string | null;
      approval_chain?: ApprovalChainStage[] | null;
    }
  | { verdict: "rejected"; reason: string; rule_name: string }
  | { verdict: "simulation_required" }
  | { verdict: "simulation_failed"; simulation_error: string };

// ── Approval workflow ────────────────────────────────────────────────────────

export interface ApprovalChainStage {
  role: string;
  description?: string;
  min_approvals: number;
}

export type ApprovalWorkflowState = "pending" | "approved" | "rejected" | "expired";

export interface StageDecision {
  approved: boolean;
  operator_id?: string | null;
  approver_role?: string | null;
  decided_at: IsoDate;
}

export interface ApprovalStage {
  index: number;
  allowed_roles: string[];
  min_approvals: number;
  decisions: StageDecision[];
}

export interface ApprovalWorkflow {
  request_id: Uuid;
  session_id: SessionId;
  state: ApprovalWorkflowState;
  stages: ApprovalStage[];
  created_at: IsoDate;
  updated_at: IsoDate;
  expires_at?: IsoDate | null;
}

export interface ApprovalRequest {
  id: Uuid;
  session_id: SessionId;
  transaction_id: Uuid;
  description: string;
  policy_verdict: PolicyVerdict;
  simulation: SimulationResult;
  requested_at: IsoDate;
  decided: boolean;
  required_approver_role?: string | null;
}

export type ApprovalOutcome =
  | "approved"
  | "rejected"
  | "already_decided"
  | "not_found"
  | "expired"
  | { role_mismatch: { required: string; provided?: string | null } }
  | {
      stage_advanced: {
        completed_stage: number;
        next_stage: number;
        next_required_role?: string | null;
      };
    }
  | {
      quorum_progress: {
        stage: number;
        approvals_so_far: number;
        approvals_required: number;
      };
    }
  | { duplicate_operator: { operator_id: string; stage: number } };

// ── Wallet config / per-wallet policy ────────────────────────────────────────

export interface WalletPolicyConfig {
  max_amount_lamports?: Lamports | null;
  program_allowlist: string[];
  required_approver_role?: string | null;
  rules: PolicyRule[];
}

// Matches Rust SignerType serde output (rename_all = "snake_case").
export type SignerType = "local_keypair" | "ledger" | "external" | "read_only";

/// Matches WalletPolicySummaryDto returned by GET /wallets.
export interface WalletPolicySummary {
  max_amount_lamports?: Lamports | null;
  program_allowlist: string[];
  required_approver_role?: string | null;
}

export interface WalletSummary {
  pubkey: string;
  label: string;
  signer_type: SignerType;
  policy?: WalletPolicySummary | null;
  daily_spend_lamports: number;
}

// ── Audit ────────────────────────────────────────────────────────────────────

export type AuditSeverity = "info" | "warning" | "error" | "critical";

export interface AuditRow {
  id: Uuid;
  session_id?: string | null;
  correlation_id: Uuid;
  occurred_at: number; // unix millis
  event_type: string; // policy_rejected | human_rejected | approval_lease_expired | ...
  actor: string; // "operator" | "system" | "agent" | operator_id
  payload: string; // JSON-encoded string
  severity: AuditSeverity;
}

// ── Composite view models used by showcase pages ─────────────────────────────

export interface PendingApprovalView {
  request: ApprovalRequest;
  workflow: ApprovalWorkflow;
  proposal: TransactionProposal;
}

export interface DashboardSnapshot {
  pending: PendingApprovalView[];
  wallets: WalletSummary[];
  recent_audit: AuditRow[];
  expiring_soon: Array<{ request_id: Uuid; expires_at: IsoDate; seconds_remaining: number }>;
}

// ── Chat route wire shapes (Phase 5D.2 + 5E backend) ─────────────────────────
//
// Mirror the Rust DTOs in `claw-api::state::{ChatResponse, ChatRouteOutcome}`.
// Wire shape is tag-on-field: `{ "status": "<variant>", ...fields }`.

export type AgentRole = "research" | "execution" | "risk" | "ops";

export interface OpenSessionRequest {
  role: AgentRole;
  channel?: string;
  policy_overrides?: PolicyRule[];
}

export interface OpenSessionResponse {
  session_id: SessionId;
  role: AgentRole;
}

export interface ChatRequest {
  message: string;
}

/// Discriminated union mirroring Rust's `ChatResponse` enum.
/// `status` is the discriminant; the runtime guarantees no other shape.
export type ChatResponse =
  | { status: "assistant_text"; assistant_text: string | null }
  | { status: "tool_dispatched"; tool_name: string; output: unknown }
  | { status: "multiple_tool_calls_rejected"; count: number }
  | { status: "unknown_or_denied_tool"; tool_name: string; reason: string }
  | { status: "malformed_tool_arguments"; tool_name: string; reason: string }
  | { status: "malformed_provider_output"; reason: string }
  | { status: "tool_error"; tool_name: string; message: string }
  | { status: "pending_action_exists"; reason: string };

/// HTTP-status-aware envelope used by the chat client.
/// The chat route maps domain outcomes to: 200 OK, 400 Bad Request,
/// 404 Not Found, 409 Conflict (PendingActionExists), 503 Service
/// Unavailable. Anything else (5xx other than 503) is `unexpected`.
export type ChatRouteResult =
  | { kind: "ok"; response: ChatResponse }
  | { kind: "conflict"; response: Extract<ChatResponse, { status: "pending_action_exists" }> }
  | { kind: "bad_request"; error: string }
  | { kind: "not_found"; error: string }
  | { kind: "disabled"; error: string }
  | { kind: "unexpected"; httpStatus: number; error: string };

/// Local UI message — rendered in the chat history.
export type ChatMessage =
  | { id: string; kind: "user"; text: string; sentAt: IsoDate }
  | { id: string; kind: "assistant"; result: ChatRouteResult; receivedAt: IsoDate }
  | { id: string; kind: "system"; text: string; at: IsoDate };
