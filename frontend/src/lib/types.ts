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

/// Wire DTO mirroring Rust's `claw_api::state::W5dConditionalDepositResultDto`.
///
/// Produced by the chat handler's W5d demo-bridge interceptor when the
/// user types the deterministic demo grammar. The chat route NEVER
/// broadcasts a tx — `tx_signature` is always `null`. W5e extended the
/// status enum to `"watching" | "ready_to_execute" | "needs_funding"`.
/// W5f added the Save UI display APY as the primary decision metric
/// (with native on-chain APR retained as a secondary audit field).
export interface W5dConditionalDepositResult {
  input_text: string;
  /// Legacy `source` field. After W5f equals `"save_display_apy"`;
  /// new UI should read `decision_source` directly.
  source: string;
  /// Solend Main Pool USDC reserve pubkey, base58.
  reserve_pubkey: string;
  /// W5f wire-compat alias for `save_display_apy_bps`. Older callers
  /// reading this field see the same value that drives the decision.
  current_apr_bps: number;
  /// Threshold the user typed, in basis points.
  threshold_bps: number;
  /// Echo of the percent label the user typed (e.g. `"2.5"`).
  threshold_pct_label: string;
  /// Strict `save_display_apy_bps > threshold_bps` after W5f.
  condition_met: boolean;
  /// Always `false` from the chat route.
  execution_attempted: boolean;
  /// W5e status enum: `"watching"` (condition false, budget reserved) |
  /// `"ready_to_execute"` (condition true, budget reserved) |
  /// `"needs_funding"` (controlled-wallet USDC balance below required
  /// budget — hard precondition, decided before condition_met).
  status: string;
  /// Reserved; always `null` from the chat route (no broadcast).
  tx_signature: string | null;

  // ── W5e budget + liveness + persistence ──────────────────────────────

  /// Base58 of the pinned controlled wallet (the bounded executor).
  controlled_wallet: string;
  /// Base58 of the controlled wallet's USDC ATA — the source account
  /// for the would-be deposit; the UI surfaces this for Copy buttons.
  source_usdc_ata: string;
  /// Required budget in raw USDC units (1 USDC = 1_000_000). Constant
  /// `250_000` for the W5d/W5e deposit grammar.
  required_budget_raw: number;
  /// Live USDC balance at the controlled wallet's USDC ATA, raw units.
  current_budget_raw: number;
  /// `"reserved"` when `current_budget_raw >= required_budget_raw`;
  /// `"needs_funding"` otherwise. Independent of `condition_met`.
  budget_status: string;
  /// Solana slot at which the reserve + balance were read. Liveness
  /// anchor — the UI must surface this so the user can verify freshness.
  last_checked_slot: number;
  /// `last_checked_slot + W5E_DEMO_EXPIRY_SLOTS` (50 000 slots ≈ 6 h).
  expires_at_slot: number | null;
  /// 16-byte hex of the deterministic rule id derived from the parsed
  /// command + controlled wallet. Same command ⇒ same id (idempotent);
  /// changed threshold ⇒ different id.
  rule_id_hex: string | null;
  /// 32-byte hex of the canonical Borsh-encoded rule hash. Lets the UI
  /// surface an integrity anchor distinct from the rule id.
  canonical_rule_hash_hex: string | null;
  /// True iff the gateway successfully persisted (or found existing)
  /// the rule via `Stage2WatchRuleRepository`. False ⇒ preview-only.
  rule_persisted: boolean;

  // ── W5f decision metric + audit ──────────────────────────────────────

  /// Which metric drove `condition_met`. Always `"save_display_apy"`
  /// on the W5f happy path.
  decision_source: string;
  /// Save/Solend UI display APY in basis points, fetched live from
  /// the official Solend REST API at
  /// `https://api.solend.fi/v1/reserves?scope=solend&ids=<reserve>`
  /// (`results[0].rates.supplyInterest`, percent string ⇒ bps). This
  /// is the value the user sees on save.finance. Drives the decision.
  save_display_apy_bps: number;
  /// Native on-chain supply APR in basis points, decoded via B-O1
  /// reserve math. Audit-only — does NOT drive the decision. Surfaced
  /// so the user can see the gap between Save UI APY and native APR.
  native_onchain_apr_bps: number;
  /// Provenance for the native APR — always `"b_o1_reserve_math"`.
  native_onchain_apr_source: string;
}

/// Wire DTO for the W5h conditional-order result.
///
/// Produced by the chat handler's W5h interceptor when the user types
/// the W5h funding-gated demo grammar (English or 繁中):
///
/// > "If Solend Main Pool USDC deposit APY is above X%, deposit
/// >  0.25 USDC from my wallet, expires in N minutes."
/// > "如果 Save APY > X%，deposit 0.25 USDC，有效期 N 分鐘"
///
/// W5h differs from W5d/W5e/W5f in that the budget is sourced from
/// the USER's wallet (not the pre-funded controlled wallet). The
/// chat-route returns `status: "funding_required"`; the frontend
/// renders a card with a "Fund 0.25 USDC with Phantom" button that
/// drives an SPL TransferChecked from the user's USDC ATA to the
/// pinned controlled-wallet USDC ATA. After broadcast the frontend
/// POSTs to `/sessions/:id/stage2/w5h/funding/confirm` and the
/// backend re-emits a `W5hConditionalDepositResult` with an updated
/// `status` (budget_reserved / watching / ready_to_execute / etc.).
///
/// ── COORDINATION NOTE ─────────────────────────────────────────────
/// As of 2026-05-12 Agent D has NOT yet committed a W5h DTO — this
/// type is the frontend's first-draft contract derived from the W5h
/// prompt. Field names use the same conventions as Agent D's W5g
/// `ChatExecuteResultDto` (snake_case, `used_*_bps` for echoed
/// metrics, u64 → JSON string for raw integers). If Agent D ships a
/// different shape, the integration step adjusts this interface and
/// the `w5h_conditional_order` ChatResponse tag.
///
/// ── INTEGER SAFETY ────────────────────────────────────────────────
/// Raw u64 fields are STRINGS (`amount_raw`, `current_budget_raw`,
/// balance deltas). bps fields stay as numbers. `expires_at_ms` is
/// modelled as `string` to dodge JS Number precision on a wall-clock
/// millisecond timestamp far enough in the future; the countdown
/// component clamps to Number after a safe-integer guard.
export interface W5hConditionalDepositResult {
  /// Echo of the user's chat command. Rendered verbatim in the
  /// card's italic header so the demo viewer sees the round-trip.
  input_text: string;

  /// Server-side lifecycle status. See module doc for the full
  /// transition graph. The frontend's local in-flight states
  /// (signing / submitted / chain-polling) sit on top of these.
  ///
  /// `funding_pending` is the backend's "I accepted the funding
  /// signature but haven't yet observed it as on-chain budget" state.
  /// The frontend polls the confirm route on a bounded loop while
  /// this status holds and does NOT treat it as failure.
  ///
  /// `expired` / `refunded` may still be emitted by the backend, but
  /// the W5h-lite demo (2026-05-12) no longer promises an automatic
  /// refund — manual operator action is the assumed cancellation
  /// path. The frontend renders these informationally only.
  status:
    | "funding_required"
    | "funding_pending"
    | "budget_reserved"
    | "watching"
    | "ready_to_execute"
    | "expired"
    | "refunded"
    | "funding_failed";

  /// 32-char hex of the deterministic rule id. Survives across the
  /// funding lifecycle so the W5g approval command can reference it
  /// once budget_reserved.
  rule_id_hex: string;
  /// 64-char hex of the canonical Borsh-encoded rule hash. Required
  /// by the W5g approval command body.
  canonical_rule_hash_hex: string;

  /// Required budget in raw USDC units (1 USDC = 1_000_000).
  /// Constant `"250000"` for the W5h 0.25 USDC demo grammar. u64 →
  /// string per the integer-safety rule.
  amount_raw: string;
  /// Live USDC balance at the controlled-wallet USDC ATA, raw units.
  /// Optional — populated after backend re-reads on confirm.
  current_budget_raw?: string | null;

  /// User's wallet pubkey, base58. The chat-route either binds this
  /// from the session-wallet binding OR echoes the user-supplied
  /// pubkey; the frontend uses it to gate the Fund button (refuse
  /// if connected Phantom pubkey ≠ this value).
  user_wallet?: string | null;
  /// User's USDC ATA derived from `(user_wallet, USDC_MINT)`, base58.
  /// Optional — frontend can derive this from `user_wallet` if the
  /// backend omits it.
  user_usdc_ata?: string | null;

  /// Controlled (bounded-executor) wallet pubkey, base58. Always
  /// the project-pinned `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`.
  controlled_wallet: string;
  /// Controlled-wallet USDC ATA, base58. The funding-tx destination.
  controlled_usdc_ata: string;

  /// Wall-clock expiry as Unix milliseconds. STRING because some
  /// chat commands could land us > 2^53 ms in the future
  /// hypothetically; defensive against any future expiry math.
  ///
  /// OPTIONAL as of the W5h-lite simplification (2026-05-12). When
  /// the user types the short grammar ("If Save APY > X%, deposit
  /// 0.25 USDC") the chat-route may omit expiry entirely; the card
  /// then renders no countdown and the Fund button is NOT gated on
  /// any client-side timer. When present, the field is rendered as
  /// informational metadata only — the demo does not auto-expire
  /// or auto-refund from the frontend.
  expires_at_ms?: string | null;
  /// Solana slot at which the reserve + balance were read. Liveness
  /// anchor.
  last_checked_slot?: string | null;

  /// Echo of the threshold the user typed, in basis points.
  threshold_bps: number;
  /// Echo of the percent label the user typed (e.g. `"1"` or
  /// `"2.5"`).
  threshold_pct_label: string;
  /// `true` when `save_display_apy_bps > threshold_bps`. Only
  /// meaningful for `watching` / `ready_to_execute`.
  condition_met?: boolean;

  /// Save UI display APY at last check, in basis points. Primary
  /// decision metric.
  save_display_apy_bps?: number | null;
  /// Native on-chain supply APR at last check, in basis points.
  /// Audit-only — Save APY drives the decision.
  native_onchain_apr_bps?: number | null;
  /// Provenance for the native APR — typically `"b_o1_reserve_math"`.
  native_onchain_apr_source?: string | null;
  /// Which metric drove the decision. Always `"save_display_apy"`
  /// on the W5h happy path.
  decision_source?: string | null;

  /// Once a funding tx exists, its base58 signature. Survives across
  /// confirm round-trips so the card can keep the Solscan link.
  funding_signature?: string | null;
  /// Slot at which the funding tx confirmed. u64 → string.
  funding_confirmation_slot?: string | null;

  /// If the backend supports refund (status === "expired" or
  /// "refunded"), this carries the refund tx signature.
  refund_signature?: string | null;

  /// Human-readable failure reason for `funding_failed`. Sent
  /// through verbatim; never inspected for structural meaning.
  error_reason?: string | null;
  /// Snake-case error variant. Optional.
  error_code?: string | null;

  // ── W5i — backend auto-watcher / auto-execution status ────────────
  //
  // After Phantom funding completes and the order is `budget_reserved`,
  // the W5i backend watcher takes over: re-evaluates the Save APY
  // condition every ~30 s and, when it crosses the threshold, builds +
  // signs + broadcasts the Solend deposit using the bounded executor's
  // keypair (server-side). These fields surface that lifecycle to the
  // chat card; the frontend POLLS them via the order-status route and
  // RENDERS only — no signing, no Solend tx construction.
  //
  // All raw u64 fields (deltas, slot, last_checked) are STRINGS per
  // the integer-safety rule.

  /// `true` when the backend has the W5i watcher enabled for this
  /// rule. When `false` or undefined, the frontend renders the
  /// existing manual W5g approval-command panel; when `true`, that
  /// panel is collapsed under a "Manual fallback" toggle so the demo
  /// reads as fully autonomous.
  auto_execution_enabled?: boolean;
  /// Lifecycle of the backend watcher / executor for this rule.
  /// Distinct from `status` so the demo can show simultaneous facts
  /// (e.g. order is `budget_reserved` AND watcher is `watching`).
  auto_execution_status?:
    | "watching"
    | "ready_to_execute"
    | "executing"
    | "completed"
    | "failed"
    | "broadcasted_timeout";
  /// Unix-millis timestamp of the watcher's most recent condition
  /// check. Rendered as a relative "checked Ns ago" label.
  auto_last_checked_at_ms?: string | null;
  /// Base58 of the auto-executed Solend deposit signature, when one
  /// exists. Distinct from `funding_signature` (which is the
  /// user's funding TransferChecked).
  auto_tx_signature?: string | null;
  /// Pre-built Solscan URL for `auto_tx_signature`. The frontend
  /// falls back to `https://solscan.io/tx/<sig>` when omitted.
  auto_solscan_url?: string | null;
  /// Slot at which `auto_tx_signature` finalized. u64 → string.
  auto_confirmation_slot?: string | null;
  /// Signed delta in USDC raw units. May carry a `-` prefix on the
  /// controlled wallet's USDC ATA delta. u64/i64 → string.
  auto_usdc_delta_raw?: string | null;
  /// Signed delta in cToken raw units. u64/i64 → string.
  auto_ctoken_delta_raw?: string | null;
  /// Snake-case error variant for `failed` / `broadcasted_timeout`.
  auto_error_code?: string | null;
  /// Human-readable reason — sent through verbatim.
  auto_error_reason?: string | null;
}

/// Envelope returned by the W5i order-status helper. Mirrors the
/// existing `confirmW5hFunding` envelope shape so the polling effect
/// can branch identically.
export type W5iOrderStatusEnvelope =
  | { kind: "ok"; response: W5hConditionalDepositResult }
  | { kind: "error"; httpStatus: number; error: string };

/// Wire request body for `POST /sessions/:id/stage2/w5h/funding/confirm`.
/// The frontend POSTs this immediately after a successful
/// `sendRawTransaction` so the backend can authoritatively verify the
/// funding move and re-emit an updated `W5hConditionalDepositResult`.
export interface W5hFundingConfirmRequest {
  rule_id_hex: string;
  funding_signature: string;
  user_wallet: string;
  user_usdc_ata: string;
  controlled_wallet: string;
  controlled_usdc_ata: string;
  amount_raw: string;
}

/// Response envelope for the confirm route. On 200, the body IS the
/// updated `W5hConditionalDepositResult` (same shape; new status).
/// 4xx/5xx surface as `{ kind: "error", httpStatus, error }`.
export type W5hFundingConfirmEnvelope =
  | { kind: "ok"; response: W5hConditionalDepositResult }
  | { kind: "error"; httpStatus: number; error: string };

/// Wire DTO for the W5g chat-first execution result.
///
/// Field names + status values mirror Agent D's
/// `claw_api::state::ChatExecuteResultDto` (Rust) — the canonical
/// backend contract. The user's second chat message
///
/// > "Execute W5g conditional deposit <rule_id_hex> <canonical_rule_hash_hex>
/// >  with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED"
///
/// is parsed by the chat-route, which dispatches to Agent D's
/// `Stage2ChatExecuteHandler` (also reachable directly via
/// `POST /sessions/:id/stage2/w5g/execute`). Whatever transport the
/// chat-route uses, the *result body* the frontend renders is this
/// DTO — wrapped in a `ChatResponse` variant with status tag
/// `"w5g_conditional_execution"`.
///
/// ── INTEGER SAFETY ────────────────────────────────────────────────
///
/// Solana raw amounts (USDC u64, slot u64, cToken u64/u128) can exceed
/// `Number.MAX_SAFE_INTEGER` (2^53 − 1). Agent D serialises these as
/// JSON strings via `crate::serde_str::opt_u64_string` /
/// `opt_i64_string`. The frontend MUST treat them as strings and
/// render them as-is. If a UI conversion to "X.YYYYYY USDC" is needed
/// for display, the consumer should check
/// `Number.isSafeInteger(Number(raw))` before any arithmetic and
/// gracefully degrade to the raw string when not safe.
///
/// ── FIELDS NOT IN AGENT D'S DTO ───────────────────────────────────
///
/// `amount_raw`, `controlled_wallet`, `source_usdc_ata`,
/// `reserve_pubkey`, `obligation_pubkey`, `decision_source` are NOT in
/// Agent D's strict `ChatExecuteResultDto` — Agent D went with
/// "rule_id-only" as the user explicitly preferred. They remain here
/// as **optional** so that:
///   - the showcase fixture can populate them for a richer demo card,
///   - if Agent D (or the chat-route wire seam) chooses to enrich the
///     wire shape during integration, the frontend already supports
///     it without a re-type,
///   - and the card degrades gracefully (omits the row) when absent.
export interface W5gConditionalExecutionResult {
  /// Discrete lifecycle status returned by Agent D's
  /// `ChatExecuteResultDto.status`. The frontend renders one card
  /// variant per status. `prechecks_failed` covers all pre-execution
  /// rejections (rule lookup, approval phrase, condition re-check,
  /// budget re-check); `execution_failed` covers post-build /
  /// broadcast / verification failures.
  status:
    | "completed"
    | "broadcasted_timeout"
    | "prechecks_failed"
    | "execution_failed";

  /// 16-byte hex of the deterministic rule id (same as the W5e/W5f
  /// `rule_id_hex`). Anchors this execution to the persisted rule.
  rule_id_hex: string;
  /// 32-byte hex of the canonical Borsh-encoded rule hash. Agent D's
  /// DTO declares this REQUIRED — the integrity anchor that prevents
  /// re-applying an execution against a replaced rule.
  canonical_rule_hash_hex: string;

  /// Base58 of the broadcast signature, when one exists. Present on
  /// `completed` / `broadcasted_timeout`; usually null on
  /// `prechecks_failed`; may be present on `execution_failed`
  /// (broadcast accepted but verification then failed).
  tx_signature?: string | null;
  /// Pre-built Solscan URL. If the backend doesn't include it, the
  /// frontend constructs `https://solscan.io/tx/<sig>` from
  /// `tx_signature` directly.
  solscan_url?: string | null;
  /// Slot at which the tx confirmed. u64 → string.
  confirmation_slot?: string | null;

  /// Save UI display APY at re-check time, in basis points. Echo of
  /// the W5f primary decision metric — Agent D names this with the
  /// `used_` prefix to make "value the executor saw" unambiguous.
  used_save_display_apy_bps?: number | null;
  /// Native on-chain supply APR at re-check time, in basis points.
  /// Audit-only, mirrors the W5f secondary metric.
  used_native_onchain_apr_bps?: number | null;
  /// Threshold extracted from the persisted rule, in basis points.
  used_threshold_bps?: number | null;

  // ── Token balance deltas — Agent D serialises u64/i64 as strings ─
  before_usdc_raw?: string | null;
  after_usdc_raw?: string | null;
  /// May carry a leading `-` if the controlled wallet's USDC went
  /// down (the expected deposit case).
  usdc_delta_raw?: string | null;
  before_ctoken_amount?: string | null;
  after_ctoken_amount?: string | null;
  ctoken_delta_raw?: string | null;

  // ── TX audit ─────────────────────────────────────────────────────
  /// Serialized v0 message length in bytes. u64 → string.
  serialized_tx_bytes?: string | null;
  /// Number of instructions in the assembled tx. u64 → string.
  instruction_count?: string | null;
  /// `true` iff a CreateIdempotent for the cToken ATA was prepended.
  ctoken_ata_create_included?: boolean | null;

  /// Snake-case error variant from
  /// `claw_gateway::stage2_chat_execute::ChatExecuteErrorCode`, or
  /// `null` on the happy path.
  error_code?: string | null;
  /// Human-readable failure reason. Sent through verbatim — never
  /// inspected for structural meaning on the frontend.
  error_reason?: string | null;

  // ── Optional enrichment fields (NOT in Agent D's strict DTO) ─────
  /// USDC raw amount the rule reserved. u64 → string. May be enriched
  /// by the chat-route wire seam from the persisted rule.
  amount_raw?: string | null;
  /// Base58 of the controlled (bounded-executor) wallet.
  controlled_wallet?: string | null;
  /// Source USDC ATA on the controlled wallet, base58.
  source_usdc_ata?: string | null;
  /// Solend Main Pool USDC reserve, base58.
  reserve_pubkey?: string | null;
  /// Obligation account pubkey for the deposit, base58.
  obligation_pubkey?: string | null;
  /// Which metric drove `condition_met` at execution. Always
  /// `"save_display_apy"` on the W5f happy path.
  decision_source?: string | null;
}

/// Wire body for `POST /sessions/:id/stage2/w5g/execute`. Mirrors
/// `claw_api::state::ChatExecuteRequestDto`. All three fields are
/// required and must be the exact strings from the `ready_to_execute`
/// W5f card plus the literal operator-approval phrase.
export interface ChatExecuteRequest {
  rule_id_hex: string;
  canonical_rule_hash_hex: string;
  approval_phrase: string;
}

/// Literal approval phrase the W5g backend requires both in env and
/// in the HTTP request body. Exposed as a constant so the UI can
/// match user input verbatim before enabling the Execute button.
export const W5G_REQUIRED_APPROVAL_PHRASE =
  "W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";

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
  | { status: "pending_action_exists"; reason: string }
  | { status: "w5d_conditional_deposit"; result: W5dConditionalDepositResult }
  /// W5h chat-first funding-gated conditional order. Tag string is
  /// the frontend's first-draft contract (Agent D hadn't shipped a
  /// W5h DTO as of 2026-05-12); integration step adjusts if Agent D
  /// chooses a different name.
  | {
      status: "w5h_conditional_order";
      result: W5hConditionalDepositResult;
    }
  /// W5g chat-first execution result. Tag string aligns with the Rust
  /// enum's `#[serde(rename_all = "snake_case")]` projection of the
  /// variant `W5gConditionalExecution { result }`. Agent D may rename
  /// the Rust variant during integration — keep this in sync.
  | {
      status: "w5g_conditional_execution";
      result: W5gConditionalExecutionResult;
    };

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
///
/// `local_w5g_safe_error` is emitted client-side when `postChat` itself
/// throws (network drop, fetch abort, JSON parse failure) while the
/// user's last typed message looked like a W5g execute command. The
/// chat-route may legitimately take 20-30+ s during mainnet
/// confirmation; if the HTTP request collapses we MUST NOT drop the
/// user's intent — we render a safe, no-overclaim card so the operator
/// knows to verify on Solscan / backend logs before retrying.
export type ChatMessage =
  | { id: string; kind: "user"; text: string; sentAt: IsoDate }
  | { id: string; kind: "assistant"; result: ChatRouteResult; receivedAt: IsoDate }
  | { id: string; kind: "system"; text: string; at: IsoDate }
  | {
      id: string;
      kind: "local_w5g_safe_error";
      /// The chat command the user sent (verbatim). Surfaced so the
      /// operator can re-issue or re-verify without re-typing.
      userText: string;
      /// The thrown error's message string. Rendered in a small detail
      /// region so the operator can see the underlying transport
      /// failure — never used to decide whether the tx succeeded.
      networkError: string;
      at: IsoDate;
    }
  /// Client-side safe-error card for the W5h CHAT path. Emitted when
  /// `postChat` itself throws while the user's last message looked
  /// like a W5h conditional-order command. Distinct from a Phantom
  /// signing rejection (which surfaces inside the W5h card's own
  /// flow state) — this covers the case where the chat request never
  /// returned a response body at all.
  | {
      id: string;
      kind: "local_w5h_safe_error";
      userText: string;
      networkError: string;
      at: IsoDate;
    };

// ── Approval decide route (Phase 6 Day 2) ────────────────────────────────────

/// Wire body for `POST /sessions/:id/approve` (mirrors
/// `claw-api::routes::approve::ApproveRequest`). Note the
/// `approved: boolean` shape — not a "decision" string discriminator.
export interface ApproveRequest {
  request_id: Uuid;
  approved: boolean;
  note?: string | null;
  approver_role?: string | null;
}

/// Wire body for the success branch (200 / 202).
export interface ApproveResponse {
  request_id: Uuid;
  outcome: string;
  transaction_id: Uuid;
  note: string | null;
}

// ── Solend signature retrieval / submit wire shapes ──────────────────────────
//
// Mirror `claw-api::state::{SolendRetrievalResult, SolendSubmitResult}` —
// both are `#[serde(tag = "status", rename_all = "snake_case")]`.

export type SolendRetrievalResult =
  | {
      status: "found";
      signing_request_id: Uuid;
      intent_id: Uuid;
      session_wallet: string;
      unsigned_tx_b64: string;
      obligation_signer_backend_partial: boolean;
      last_valid_block_height: number;
      expires_at_unix_ms: number;
      verified_slot: number;
      simulation_slot: number | null;
      units_consumed: number | null;
    }
  | {
      status: "submitted";
      signing_request_id: Uuid;
      intent_id: Uuid;
      tx_signature: string;
      last_valid_block_height: number;
    }
  | {
      status: "confirming";
      signing_request_id: Uuid;
      intent_id: Uuid;
      tx_signature: string;
      slot: number;
      last_valid_block_height: number;
    }
  | {
      status: "finalized";
      signing_request_id: Uuid;
      intent_id: Uuid;
      tx_signature: string;
      slot: number;
    }
  | {
      status: "failed";
      signing_request_id: Uuid;
      intent_id: Uuid;
      tx_signature: string;
      err: string;
    }
  | {
      status: "confirmation_timeout";
      signing_request_id: Uuid;
      tx_signature: string;
      last_valid_block_height: number;
      current_block_height: number;
      requires_reproposal: boolean;
      reason: string;
    }
  | {
      status: "rejected";
      signing_request_id: Uuid;
      error_type: string;
      message: string;
    }
  | {
      status: "broadcast_failed";
      signing_request_id: Uuid;
      error_type: string;
      message: string;
    }
  | { status: "pre_submit_expired"; reason: string }
  | { status: "not_found" }
  | { status: "expired" };

export type SolendSubmitResult =
  | {
      status: "submitted";
      signing_request_id: Uuid;
      intent_id: Uuid;
      session_wallet: string;
      tx_signature: string;
      verified_slot: number;
      last_valid_block_height: number;
    }
  | {
      status: "recovered";
      signing_request_id: Uuid;
      tx_signature: string;
      recorded_at_unix_ms: number;
    }
  | { status: "not_found" }
  | { status: "expired"; reason: string }
  | { status: "rejected"; error_type: string; message: string }
  | {
      status: "broadcast_failed";
      signing_request_id: Uuid;
      error_type: string;
      message: string;
    };

/// Local UI state for the `useSigningHandoff` hook. Collapses the
/// retrieval + submit DTOs into a single state machine the
/// `<SigningFlow>` component renders one branch at a time.
export type SigningHandoffState =
  | { kind: "idle" }
  | { kind: "polling"; attempts: number }
  /// Phase 6B Window 3: POST /sessions/:s/approvals/:a/solend-signing/prepare
  /// is in flight. Entered on the user's Sign-with-Phantom click.
  | { kind: "preparing" }
  /// Phase 6B Window 3: prepare returned a typed failure variant
  /// other than Ready. The UI shows a retryable error so the operator
  /// can click Sign again (or fix wallet binding) without re-approval.
  | { kind: "prepare_failed"; reason: string }
  | {
      kind: "ready";
      unsigned_tx_b64: string;
      session_wallet: string;
      verified_slot: number;
      last_valid_block_height: number;
    }
  | { kind: "signing" }
  | { kind: "submitting" }
  | {
      kind: "submitted";
      tx_signature: string;
      last_valid_block_height: number;
    }
  | { kind: "confirming"; tx_signature: string; slot: number }
  | { kind: "finalized"; tx_signature: string; slot: number }
  | { kind: "rejected"; error: string }
  | { kind: "broadcast_failed"; error: string; tx_signature?: string }
  | { kind: "expired"; reason: string }
  | { kind: "not_found" }
  | { kind: "execution_failed"; err: string; tx_signature: string }
  | {
      kind: "confirmation_timeout";
      reason: string;
      requires_reproposal: boolean;
      tx_signature: string;
    }
  | { kind: "error"; error: string };

// ── Phase 6B Window 3 — JIT-prepare wire shape ──────────────────────────────
//
// Wire shape of the `POST /sessions/:s/approvals/:a/solend-signing/prepare`
// response. Tagged union by `status`; HTTP status mapping:
//   200  → "ready"
//   404  → "not_found" | "jit_ready_missing"
//   422  → "not_approved" | "wallet_mismatch"
//   502  → "handoff_create_failed"
// 400 / 503 / network errors are surfaced through the api.ts envelope as
// `{ kind: "error", httpStatus, error }` rather than as a status variant.
export type SolendJitPrepareResult =
  | {
      status: "ready";
      approval_request_id: Uuid;
      signing_request_id: Uuid;
      session_id: SessionId;
      wallet: string;
      last_valid_block_height: number;
      verified_slot: number;
      expires_at_unix_ms: number;
    }
  | { status: "not_approved"; state: string }
  | { status: "jit_ready_missing" }
  | { status: "wallet_mismatch"; expected: string; bound: string | null }
  | { status: "handoff_create_failed"; error_type: string; message: string }
  | { status: "not_found" };

// ── Phase 6I-G — Solend WITHDRAW JIT-prepare wire shape ─────────────────────
//
// Mirrors `SolendWithdrawJitPrepareResult` in
// `crates/api/src/state.rs`. Tagged union by `status`. HTTP status
// mapping (per `crates/api/src/routes/solend_withdraw_jit_signing.rs`):
//   200  → "ready"
//   404  → "withdraw_intent_missing" | "not_found"
//   422  → "not_approved" | "wallet_mismatch" | "recheck_blocked"
//   502  → "snapshot_assemble_failed" | "plan_assembly_failed" | "handoff_create_failed"
// 400 / 503 / network errors are surfaced through the api.ts envelope as
// `{ kind: "error", httpStatus, error }` rather than as a status variant.
export type SolendWithdrawJitPrepareResult =
  | {
      status: "ready";
      approval_request_id: Uuid;
      signing_request_id: Uuid;
      session_id: SessionId;
      wallet: string;
      obligation_pubkey: string;
      reserve_pubkey: string;
      last_valid_block_height: number;
      verified_slot: number;
      expires_at_unix_ms: number;
    }
  | { status: "not_approved"; state: string }
  | { status: "withdraw_intent_missing" }
  | { status: "wallet_mismatch"; expected: string; bound: string | null }
  | { status: "recheck_blocked"; reason: string; detail?: string | null }
  | { status: "snapshot_assemble_failed"; error_type: string; message: string }
  | { status: "plan_assembly_failed"; error_type: string; message: string }
  | { status: "handoff_create_failed"; error_type: string; message: string }
  | { status: "not_found" };
