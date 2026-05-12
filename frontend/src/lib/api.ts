// API client: showcase mode reads fixtures; live mode fetches the gateway.
// The surface is stable so pages never branch on mode themselves.

import { IS_SHOWCASE, GATEWAY_URL, GATEWAY_TOKEN } from "@/lib/mode";
import type {
  ApprovalRequest,
  ApprovalWorkflow,
  ApproveRequest,
  ApproveResponse,
  AuditRow,
  ChatExecuteRequest,
  ChatRequest,
  ChatResponse,
  ChatRouteResult,
  DashboardSnapshot,
  OpenSessionRequest,
  OpenSessionResponse,
  PolicyRule,
  SessionId,
  SolendJitPrepareResult,
  SolendRetrievalResult,
  SolendSubmitResult,
  SolendWithdrawJitPrepareResult,
  TransactionProposal,
  Uuid,
  W5gConditionalExecutionResult,
  W5hConditionalDepositResult,
  W5hFundingConfirmEnvelope,
  W5hFundingConfirmRequest,
  WalletSummary,
} from "@/lib/types";
import {
  dashboardSnapshot,
  pendingView,
  policyRules,
  auditRows,
  solendApprovedView,
  wallets,
  workflowPending,
  workflowStageTwo,
  workflowApproved,
  workflowExpired,
  demoFlow,
} from "@/lib/fixtures/demo-flow";

async function live<T>(path: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    ...(init?.headers as Record<string, string> | undefined),
  };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(`${GATEWAY_URL}${path}`, {
    ...init,
    headers,
    cache: "no-store",
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`${path} ${res.status} ${text}`.trim());
  }
  return res.json() as Promise<T>;
}

// ── Wire normalization ──────────────────────────────────────────────────────
// Rust serializes unit-variant PolicyConditions and PolicyActions as bare JSON
// strings (e.g. "Always", "Approve") while the TS types expect { type: "..." }.

function normalizeCondition(raw: unknown): import("@/lib/types").PolicyCondition {
  if (typeof raw === "string") return { type: raw } as import("@/lib/types").PolicyCondition;
  return raw as import("@/lib/types").PolicyCondition;
}

function normalizeAction(raw: unknown): import("@/lib/types").PolicyAction {
  if (typeof raw === "string") return { type: raw } as import("@/lib/types").PolicyAction;
  return raw as import("@/lib/types").PolicyAction;
}

function normalizePolicyRule(raw: { name: string; description: string; condition: unknown; action: unknown }): import("@/lib/types").PolicyRule {
  return {
    name: raw.name,
    description: raw.description,
    condition: normalizeCondition(raw.condition),
    action: normalizeAction(raw.action),
  };
}

// ── Wire shapes (mirror Rust DTOs) ───────────────────────────────────────────

interface PendingApprovalsResponse {
  items: Array<{ request: ApprovalRequest; workflow: ApprovalWorkflow }>;
}
interface WalletsResponse { wallets: WalletSummary[] }
interface AuditResponse { rows: AuditRow[]; limit: number; offset: number }

// ── Public API surface ───────────────────────────────────────────────────────

export async function fetchPolicyRules(): Promise<PolicyRule[]> {
  if (IS_SHOWCASE) return policyRules;
  const data = await live<{ rules: Array<{ name: string; description: string; condition: unknown; action: unknown }> }>("/policy/rules");
  return data.rules.map(normalizePolicyRule);
}

export async function fetchAuditTrail(limit = 100): Promise<AuditRow[]> {
  if (IS_SHOWCASE) return auditRows;
  const data = await live<AuditResponse>(`/audit?limit=${limit}`);
  return data.rows;
}

export async function fetchWallets(): Promise<WalletSummary[]> {
  if (IS_SHOWCASE) return wallets;
  const data = await live<WalletsResponse>("/wallets");
  return data.wallets;
}

export async function fetchPendingApprovals(): Promise<
  Array<{ request: ApprovalRequest; workflow: ApprovalWorkflow }>
> {
  if (IS_SHOWCASE) {
    return [{ request: pendingView.request, workflow: pendingView.workflow }];
  }
  const data = await live<PendingApprovalsResponse>("/pending-approvals");
  return data.items;
}

export async function fetchApproval(
  requestId: string,
): Promise<{ request: ApprovalRequest; workflow: ApprovalWorkflow; proposal: TransactionProposal | null }> {
  if (IS_SHOWCASE) {
    // /approval/[id] is now the SigningFlow stage. Showcase narrates the
    // Phase 5G mainnet proof: 0.001 USDC Solend deposit, treasury-approved,
    // ready for the user to sign + submit. The four-state policy-chain demo
    // (50k USDC transfer, risk → treasury → CFO) lives on the same page's
    // "Demo flow" tab via the workflowPending/StageTwo/Approved/Expired
    // snapshots, so both narratives coexist without contradicting each other.
    return {
      request: solendApprovedView.request,
      workflow: solendApprovedView.workflow,
      proposal: solendApprovedView.proposal,
    };
  }
  const item = await live<{ request: ApprovalRequest; workflow: ApprovalWorkflow }>(
    `/approvals/${requestId}`,
  );
  return { request: item.request, workflow: item.workflow, proposal: null };
}

/// Dashboard snapshot is composed client-side in live mode. The gateway does
/// not expose a single dashboard aggregate — each section has its own route.
export async function fetchDashboard(): Promise<DashboardSnapshot> {
  if (IS_SHOWCASE) return dashboardSnapshot;

  const [items, w, a] = await Promise.all([
    fetchPendingApprovals(),
    fetchWallets(),
    fetchAuditTrail(50),
  ]);

  // Compose expiring_soon from each workflow's expires_at.
  const nowMs = Date.now();
  const expiring = items
    .filter((p) => p.workflow.state === "pending" && p.workflow.expires_at)
    .map((p) => {
      const target = new Date(p.workflow.expires_at as string).getTime();
      return {
        request_id: p.request.id,
        expires_at: p.workflow.expires_at as string,
        seconds_remaining: Math.max(0, Math.floor((target - nowMs) / 1000)),
      };
    })
    .filter((e) => e.seconds_remaining <= 300)
    .sort((a, b) => a.seconds_remaining - b.seconds_remaining);

  // Build pending views without the full TransactionProposal (not exposed yet).
  // Pages that only show description / wallet pubkey will work; richer views
  // degrade gracefully with a placeholder proposal.
  const pending = items.map((it) => ({
    request: it.request,
    workflow: it.workflow,
    proposal: {
      id: it.request.transaction_id,
      session_id: it.request.session_id,
      wallet_pubkey: "",
      network: "devnet" as const,
      description: it.request.description,
      transaction_b64: "",
      instructions_summary: [],
      created_at: it.request.requested_at,
    },
  }));

  return {
    pending,
    wallets: w,
    recent_audit: a,
    expiring_soon: expiring,
  };
}

// Demo-only helpers, for walking the canonical flow on a single page.
export const showcase = {
  pending: workflowPending,
  stageTwo: workflowStageTwo,
  approved: workflowApproved,
  expired: workflowExpired,
  flow: demoFlow,
};

// ── Chat route client (Phase 5D.2 + 5E backend) ─────────────────────────────
//
// The chat surface is the only route that POSTs without a path-id (besides
// /sessions and signature submits). It needs slightly different fetch
// handling than the read-only helpers above:
//
//  - non-2xx is a domain outcome (400/404/409/503), not an exception
//  - 200 OK still requires status-string discrimination on the body
//
// `postChat` therefore returns a `ChatRouteResult` envelope rather than
// throwing. Callers branch on `.kind`.

let _liveSessionPromise: Promise<SessionId> | null = null;

/// Showcase-mode fixture: a stable hard-coded session id for fixture flows.
const SHOWCASE_SESSION_ID: SessionId = "11111111-2222-3333-4444-555555555555";

/// Open a new session. Caches the result for the lifetime of this client
/// so the chat page does not pile up server-side sessions on every render.
export async function getOrCreateSession(role: "execution" = "execution"): Promise<SessionId> {
  if (IS_SHOWCASE) return SHOWCASE_SESSION_ID;
  if (_liveSessionPromise) return _liveSessionPromise;
  _liveSessionPromise = (async () => {
    const body: OpenSessionRequest = { role, channel: "frontend-chat" };
    const data = await live<OpenSessionResponse>("/sessions", {
      method: "POST",
      body: JSON.stringify(body),
    });
    return data.session_id;
  })();
  return _liveSessionPromise;
}

/// POST /sessions/:id/chat — strict one-turn LLM dispatch.
/// Returns a `ChatRouteResult` envelope. Does NOT throw on domain
/// failures; only network-level / parse failures bubble up.
export async function postChat(
  sessionId: SessionId,
  message: string,
): Promise<ChatRouteResult> {
  if (IS_SHOWCASE) {
    return showcaseChatReply(message);
  }

  const body: ChatRequest = { message };
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(`${GATEWAY_URL}/sessions/${sessionId}/chat`, {
    method: "POST",
    headers,
    cache: "no-store",
    body: JSON.stringify(body),
  });

  // 200 OK — body is `ChatResponse`
  if (res.ok) {
    const parsed = (await res.json()) as ChatResponse;
    return { kind: "ok", response: parsed };
  }

  // 409 — body is `ChatResponse` with status "pending_action_exists"
  if (res.status === 409) {
    const parsed = (await res.json()) as ChatResponse;
    if (parsed.status === "pending_action_exists") {
      return { kind: "conflict", response: parsed };
    }
    return { kind: "unexpected", httpStatus: 409, error: "409 with non-pending body" };
  }

  // 400 / 404 / 503 — body is `{ "error": "..." }`
  let errorText = "";
  try {
    const errBody = (await res.json()) as { error?: string };
    errorText = errBody.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }

  if (res.status === 400) return { kind: "bad_request", error: errorText };
  if (res.status === 404) return { kind: "not_found", error: errorText };
  if (res.status === 503) return { kind: "disabled", error: errorText };

  return { kind: "unexpected", httpStatus: res.status, error: errorText };
}

/// W5g — typed envelope around `POST /sessions/:id/stage2/w5g/execute`.
/// The backend always returns the `ChatExecuteResultDto` shape on 200;
/// 400 / 404 / 503 carry an `{ error: string }` body.
export type W5gExecuteResult =
  | { kind: "ok"; response: W5gConditionalExecutionResult }
  | { kind: "bad_request"; error: string }
  | { kind: "not_found"; error: string }
  | { kind: "disabled"; error: string }
  | { kind: "unexpected"; httpStatus: number; error: string };

/// Posts the W5g execution request. Default request timeout is
/// unbounded on the frontend side — the backend has a bounded
/// `getSignatureStatuses` poll window (~120 s) and will return a
/// `broadcasted_timeout` DTO rather than hang the HTTP call.
///
/// SHOWCASE MODE: returns a deterministic `prechecks_failed` result
/// so the showcase doesn't pretend to broadcast.
export async function postW5gExecute(
  sessionId: SessionId,
  body: ChatExecuteRequest,
): Promise<W5gExecuteResult> {
  if (IS_SHOWCASE) {
    return {
      kind: "ok",
      response: {
        status: "prechecks_failed",
        rule_id_hex: body.rule_id_hex,
        canonical_rule_hash_hex: body.canonical_rule_hash_hex,
        error_code: "master_gate_missing",
        error_reason:
          "Showcase mode: W5g live execution is disabled. No on-chain send was attempted.",
        tx_signature: null,
        solscan_url: null,
        confirmation_slot: null,
      },
    };
  }

  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/stage2/w5g/execute`,
    {
      method: "POST",
      headers,
      cache: "no-store",
      body: JSON.stringify(body),
    },
  );

  if (res.ok) {
    const parsed = (await res.json()) as W5gConditionalExecutionResult;
    return { kind: "ok", response: parsed };
  }

  let errorText = "";
  try {
    const errBody = (await res.json()) as { error?: string };
    errorText = errBody.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }

  if (res.status === 400) return { kind: "bad_request", error: errorText };
  if (res.status === 404) return { kind: "not_found", error: errorText };
  if (res.status === 503) return { kind: "disabled", error: errorText };
  return { kind: "unexpected", httpStatus: res.status, error: errorText };
}

/// Showcase-mode reply: returns a deterministic ChatRouteResult based on
/// the message content. The fixture mirrors the safe paths the live
/// backend would take. No network call.
function showcaseChatReply(message: string): ChatRouteResult {
  const lower = message.toLowerCase();

  // ── W5h chat-driven funding-gated fixture ───────────────────────
  //
  // Matches the W5h demo grammar in English OR Traditional Chinese:
  //   "If Solend Main Pool USDC deposit APY is above X%, deposit
  //    0.25 USDC from my wallet, expires in N minutes."
  //   "如果 Save APY > X%，deposit 0.25 USDC，有效期 N 分鐘"
  //
  // Returns a `funding_required` card so the demo viewer can see the
  // Fund-with-Phantom button + countdown without standing up the
  // live W5h backend. The chat fixture path NEVER produces other
  // statuses — those flow back from the confirm route fixture below
  // (`showcaseW5hFundingConfirmReply`).
  const w5hOrder = parseShowcaseW5hConditional(message);
  if (w5hOrder !== null) {
    return { kind: "ok", response: w5hOrder };
  }

  // ── W5d/W5e/W5f conditional-order fixture ───────────────────────
  //
  // Lets a showcase-mode operator see the W5f conditional-order card
  // (including W5g's "Ready to execute" copy-command panel) without
  // standing up the live backend. Match is intentionally narrow so it
  // does not steal traffic from the legacy `usdc`/`solend` fallback.
  const w5dOrder = parseShowcaseW5dConditional(message);
  if (w5dOrder !== null) {
    return { kind: "ok", response: w5dOrder };
  }

  // ── W5g chat-first execution fixture ────────────────────────────
  //
  // Matches the deterministic command grammar:
  //   "Execute W5g conditional deposit <rule_id_hex> <canonical_hash>
  //    with approval phrase W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED"
  //
  // Suffix selectors on the trailing rule_id let demo viewers preview
  // each lifecycle state without changing code. Status enum mirrors
  // Agent D's `ChatExecuteResultDto.status`:
  //
  //   …deadbeef / default       → completed
  //   …timeout / …timeout01     → broadcasted_timeout
  //   …prefail / …precheck01    → prechecks_failed
  //   …execfail / …exec01       → execution_failed
  //
  // All amounts / slots are STRINGS — Solana u64 / u128 don't fit JS
  // Number safely. The frontend's W5g card renders strings verbatim
  // and only does decimal conversion under a safe-integer guard.
  const w5gExec = parseShowcaseW5gExecute(message);
  if (w5gExec !== null) {
    return { kind: "ok", response: w5gExec };
  }

  if (lower.includes("solend") || lower.includes("usdc")) {
    return {
      kind: "ok",
      response: {
        status: "tool_dispatched",
        tool_name: "solend_deposit_usdc",
        output: {
          tool_name: "solend_deposit_usdc",
          success: true,
          data: {
            status: "awaiting_approval",
            protocol: "Solend",
            asset: "USDC",
            amount_raw: 1000,
            approval_request_id: "00000000-0000-0000-0000-000000000000",
            human_readable_next_step: "review and approve via the operator dashboard",
          },
          error: null,
          duration_ms: 0,
        },
      },
    };
  }
  return {
    kind: "ok",
    response: {
      status: "assistant_text",
      assistant_text:
        "Showcase mode is rendering a fixture reply. " +
        "Ask me to deposit USDC into Solend to see the LLM tool-call shape.",
    },
  };
}

/// W5h showcase fixture — pinned demo values aligned with the
/// project memory pointers (controlled wallet BPfDMm…hhs5L, controlled
/// USDC ATA 7LFdKc…BBmk3 per the W5h prompt, USDC mint EPjFWd…TDt1v).
const W5H_FIXTURE_PINNED = {
  rule_id_hex: "deadbeefcafef00d1234567890abcdef",
  canonical_rule_hash_hex:
    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  amount_raw: "250000",
  controlled_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
  controlled_usdc_ata: "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3",
  save_display_apy_bps: 312,
  native_onchain_apr_bps: 287,
  native_onchain_apr_source: "b_o1_reserve_math",
  decision_source: "save_display_apy",
} as const;

/// Parse the W5h chat command in either English or 繁體中文 grammar.
/// Returns a `funding_required` W5h DTO showcase fixture, or `null`
/// when the message doesn't match. Strict regex so legacy W5d/W5g
/// chatter doesn't get hijacked.
function parseShowcaseW5hConditional(
  message: string,
): ChatResponse | null {
  const trimmed = message.trim();

  // English grammar:
  //   If Solend Main Pool USDC deposit APY is above X%, deposit 0.25 USDC
  //   from my wallet, expires in N minutes[.]
  const en = trimmed.match(
    /^If Solend Main Pool USDC deposit APY is above\s+(\d+(?:\.\d+)?)%,\s+deposit\s+0\.25\s+USDC\s+from my wallet,\s+expires in\s+(\d+)\s+minutes?\.?\s*$/i,
  );
  // 繁中 grammar:
  //   如果 Save APY > X%，deposit 0.25 USDC，有效期 N 分鐘
  // (allow optional spaces, half/full-width comma, optional 0 leading)
  const zh = trimmed.match(
    /^如果\s*Save\s*APY\s*>\s*(\d+(?:\.\d+)?)%[，,]\s*deposit\s*0\.25\s*USDC[，,]\s*有效期\s*(\d+)\s*分鐘\s*$/i,
  );
  const m = en ?? zh;
  if (m === null) return null;

  const thresholdPctLabel = m[1];
  const expireMinutes = Math.max(1, Math.min(60, parseInt(m[2], 10) || 3));
  const thresholdBps = Math.round(parseFloat(thresholdPctLabel) * 100);
  const expiresAtMs = (Date.now() + expireMinutes * 60_000).toString();

  // Showcase user wallet: in showcase mode we don't know the live
  // Phantom pubkey; surface `null` and let the card fall back to the
  // connected-wallet pubkey, deriving the source ATA at click time.
  return {
    status: "w5h_conditional_order",
    result: {
      input_text: trimmed,
      status: "funding_required",
      rule_id_hex: W5H_FIXTURE_PINNED.rule_id_hex,
      canonical_rule_hash_hex: W5H_FIXTURE_PINNED.canonical_rule_hash_hex,
      amount_raw: W5H_FIXTURE_PINNED.amount_raw,
      current_budget_raw: "0",
      user_wallet: null,
      user_usdc_ata: null,
      controlled_wallet: W5H_FIXTURE_PINNED.controlled_wallet,
      controlled_usdc_ata: W5H_FIXTURE_PINNED.controlled_usdc_ata,
      expires_at_ms: expiresAtMs,
      last_checked_slot: "418961171",
      threshold_bps: thresholdBps,
      threshold_pct_label: thresholdPctLabel,
      condition_met:
        W5H_FIXTURE_PINNED.save_display_apy_bps > thresholdBps,
      save_display_apy_bps: W5H_FIXTURE_PINNED.save_display_apy_bps,
      native_onchain_apr_bps: W5H_FIXTURE_PINNED.native_onchain_apr_bps,
      native_onchain_apr_source: W5H_FIXTURE_PINNED.native_onchain_apr_source,
      decision_source: W5H_FIXTURE_PINNED.decision_source,
      funding_signature: null,
      funding_confirmation_slot: null,
      refund_signature: null,
      error_reason: null,
      error_code: null,
    },
  };
}

/// Parse the W5d/W5e/W5f conditional-order grammar and produce a
/// `ready_to_execute` fixture. Strict prefix match so this fixture is
/// only fired by the canonical demo command — random "solend" chatter
/// continues to fall through to the legacy `tool_dispatched` fixture.
function parseShowcaseW5dConditional(
  message: string,
): ChatResponse | null {
  const trimmed = message.trim();
  if (
    !/^If Solend Main Pool USDC deposit APR is above\s+\d+(?:\.\d+)?%,\s+deposit 0\.25 USDC from my bounded executor wallet into Solend\.?\s*$/i.test(
      trimmed,
    )
  ) {
    return null;
  }
  // Echo the user's threshold so the card shows the right percent.
  const pctMatch = trimmed.match(/above\s+(\d+(?:\.\d+)?)%/i);
  const pctLabel = pctMatch ? pctMatch[1] : "1";
  const thresholdBps = Math.round(parseFloat(pctLabel) * 100);
  return {
    status: "w5d_conditional_deposit",
    result: {
      input_text: trimmed,
      source: "save_display_apy",
      reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
      current_apr_bps: 312,
      threshold_bps: thresholdBps,
      threshold_pct_label: pctLabel,
      condition_met: 312 > thresholdBps,
      execution_attempted: false,
      // Force the ready-to-execute path so the demo viewer sees the
      // W5g copy-command panel inside the W5f card.
      status: 312 > thresholdBps ? "ready_to_execute" : "watching",
      tx_signature: null,
      controlled_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
      source_usdc_ata: "73CnYmQAUKgaiQ4mY3ub2j8wPqkfaUmEnu3GuVHzefVB",
      required_budget_raw: 250_000,
      current_budget_raw: 1_000_000,
      budget_status: "reserved",
      last_checked_slot: 415_571_900,
      expires_at_slot: 415_621_900,
      rule_id_hex: "deadbeefcafef00d0000000000000000",
      canonical_rule_hash_hex:
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
      rule_persisted: true,
      decision_source: "save_display_apy",
      save_display_apy_bps: 312,
      native_onchain_apr_bps: 287,
      native_onchain_apr_source: "b_o1_reserve_math",
    },
  };
}

/// Parse a W5g execute command and produce a deterministic fixture
/// `ChatResponse`. Returns `null` if the message doesn't match the
/// W5g grammar.
function parseShowcaseW5gExecute(
  message: string,
): ChatResponse | null {
  const m = message
    .trim()
    .match(
      /^Execute W5g conditional deposit\s+([0-9a-fA-F]{1,64})\s+([0-9a-fA-F]{1,64})\s+with approval phrase\s+(.+)$/,
    );
  if (m === null) return null;
  const ruleIdHex = m[1];
  const canonicalHashHex = m[2];
  const trailing = ruleIdHex.toLowerCase();

  // Common fields shared across all lifecycle fixtures. Field names
  // mirror Agent D's `ChatExecuteResultDto`; the optional enrichment
  // fields (amount_raw, controlled_wallet, …) are populated here for
  // a richer demo card and gracefully omitted by the renderer when
  // absent on the live wire.
  const baseFields = {
    rule_id_hex: ruleIdHex,
    canonical_rule_hash_hex: canonicalHashHex,
    used_save_display_apy_bps: 312,
    used_native_onchain_apr_bps: 287,
    used_threshold_bps: 100,
    decision_source: "save_display_apy",
    amount_raw: "250000",
    controlled_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
    source_usdc_ata: "73CnYmQAUKgaiQ4mY3ub2j8wPqkfaUmEnu3GuVHzefVB",
    reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
    obligation_pubkey: "BdFLjCcPdpe1xkhfH1jWaPKJEWE9rZ87dpW48VtUra1wN",
  } as const;

  if (trailing.endsWith("prefail") || trailing.endsWith("precheck01")) {
    return {
      status: "w5g_conditional_execution",
      result: {
        ...baseFields,
        status: "prechecks_failed",
        tx_signature: null,
        solscan_url: null,
        confirmation_slot: null,
        serialized_tx_bytes: null,
        instruction_count: null,
        ctoken_ata_create_included: null,
        before_usdc_raw: null,
        after_usdc_raw: null,
        usdc_delta_raw: null,
        before_ctoken_amount: null,
        after_ctoken_amount: null,
        ctoken_delta_raw: null,
        error_code: "approval_phrase_mismatch",
        error_reason:
          "Approval phrase did not match the canonical W5g phrase (showcase fixture).",
      },
    };
  }

  if (trailing.endsWith("execfail") || trailing.endsWith("exec01")) {
    return {
      status: "w5g_conditional_execution",
      result: {
        ...baseFields,
        status: "execution_failed",
        tx_signature: null,
        solscan_url: null,
        confirmation_slot: null,
        serialized_tx_bytes: "1232",
        instruction_count: "4",
        ctoken_ata_create_included: false,
        before_usdc_raw: "1000000",
        after_usdc_raw: "1000000",
        usdc_delta_raw: "0",
        before_ctoken_amount: "0",
        after_ctoken_amount: "0",
        ctoken_delta_raw: "0",
        error_code: "simulation_failed",
        error_reason:
          "Simulation reported insufficient funds (showcase fixture).",
      },
    };
  }

  if (trailing.endsWith("timeout") || trailing.endsWith("timeout01")) {
    // Broadcasted but never observed finalized — has a signature, no
    // deltas, no completed badge.
    return {
      status: "w5g_conditional_execution",
      result: {
        ...baseFields,
        status: "broadcasted_timeout",
        tx_signature:
          "4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y",
        solscan_url: null,
        confirmation_slot: null,
        serialized_tx_bytes: "1232",
        instruction_count: "4",
        ctoken_ata_create_included: true,
        before_usdc_raw: "1000000",
        after_usdc_raw: null,
        usdc_delta_raw: null,
        before_ctoken_amount: "0",
        after_ctoken_amount: null,
        ctoken_delta_raw: null,
        error_code: "confirmation_timeout",
        error_reason:
          "Broadcasted at slot 415_571_900; finalization not observed within 120 s (showcase fixture).",
      },
    };
  }

  // Default: completed happy path.
  return {
    status: "w5g_conditional_execution",
    result: {
      ...baseFields,
      status: "completed",
      tx_signature:
        "4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y",
      solscan_url:
        "https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y",
      confirmation_slot: "415571964",
      serialized_tx_bytes: "1232",
      instruction_count: "4",
      ctoken_ata_create_included: true,
      before_usdc_raw: "1000000",
      after_usdc_raw: "750000",
      usdc_delta_raw: "-250000",
      before_ctoken_amount: "0",
      after_ctoken_amount: "240156",
      ctoken_delta_raw: "240156",
      error_code: null,
      error_reason: null,
    },
  };
}

// ── Wallet bind challenge-response (Phase 6 Day 3) ──────────────────────────
//
// Two-step proof-of-ownership flow that binds the user's Phantom wallet
// to the daemon session. Without this binding, the Solend tool's session
// resolver (`SessionBoundWallet::session_wallet_pubkey`) returns None
// and chat → tool dispatch fails before producing `awaiting_approval`.
//
// 1. POST /sessions/:id/wallet-bind-challenge { pubkey }
//    → { challenge_id, message, expires_at }
// 2. Phantom signs the canonical message via signMessage().
// 3. POST /sessions/:id/wallet-bind-confirm { challenge_id, pubkey, signature_b64 }
//    → { session_id, pubkey, bound: true, verified: true }
//
// Daemon enforces: 5-minute TTL, single-use, session+pubkey match,
// ed25519 signature verification, atomic mark-used. See
// `crates/gateway/src/wallet_challenge.rs` for the full security model.
//
// SECURITY NOTE: never wire this UI to /sessions/:id/bind-wallet — that
// route accepts a pubkey claim with no proof of ownership. The
// challenge-response pair above is the only product-UI path.

export interface WalletBindChallenge {
  challenge_id: string;
  message: string;
  expires_at: number;
}

export interface WalletBindConfirm {
  session_id: string;
  pubkey: string;
  bound: boolean;
  verified: boolean;
}

export async function createWalletBindChallenge(
  sessionId: SessionId,
  pubkey: string,
): Promise<WalletBindChallenge> {
  if (IS_SHOWCASE) {
    throw new Error("wallet binding is not used in showcase mode");
  }
  return live<WalletBindChallenge>(
    `/sessions/${sessionId}/wallet-bind-challenge`,
    {
      method: "POST",
      body: JSON.stringify({ pubkey }),
    },
  );
}

export async function confirmWalletBindChallenge(
  sessionId: SessionId,
  challengeId: string,
  pubkey: string,
  signatureB64: string,
): Promise<WalletBindConfirm> {
  if (IS_SHOWCASE) {
    throw new Error("wallet binding is not used in showcase mode");
  }
  return live<WalletBindConfirm>(
    `/sessions/${sessionId}/wallet-bind-confirm`,
    {
      method: "POST",
      body: JSON.stringify({
        challenge_id: challengeId,
        pubkey,
        signature_b64: signatureB64,
      }),
    },
  );
}

// ── Approval decide + Solend signature flow (Phase 6 Day 2) ─────────────────
//
// `decideApproval`     POST /sessions/:id/approve
// `getSolendSignature` GET  /sessions/:id/solend-signatures/:request_id
// `submitSolendSig...` POST /sessions/:id/solend-signatures/:request_id
//
// All three return a typed result envelope (HTTP-status-aware on the
// retrieval / submit pair) — domain failures (404 / 410 / 422 / 502) are
// branches in the result, not exceptions. Network and parse errors still
// bubble up as thrown errors.
//
// Showcase fixtures: `decideApproval` always returns an "approved"
// outcome; the signature retrieval / submit fixtures live in the hook
// (see `use-signing-handoff.ts`) so the polling state machine has a
// single place to drive the simulated lifecycle. This file just exposes
// the wire-shape-correct functions.

/// Showcase fixture id for the parked Solend signing handoff. The
/// `useSigningHandoff` hook recognises this id and runs a simulated
/// lifecycle (no daemon, no Phantom popup). Live mode never sees this
/// id — it's a sentinel for the fixture path only.
export const SHOWCASE_SIGNING_REQUEST_ID: Uuid =
  "fffffff0-0000-0000-0000-000000000000";

/// HTTP-aware result envelope for `getSolendSignature`. The HTTP
/// status comes back through `httpStatus` so the hook can branch
/// without re-deriving from body shape (the body is the typed enum
/// for 200 / 410 / 422 / 502; an `{ error }` shape for 400 / 404 /
/// 503 misconfiguration paths).
export type SolendRetrievalEnvelope =
  | { kind: "ok"; response: SolendRetrievalResult }
  | { kind: "error"; httpStatus: number; error: string };

export async function getSolendSignature(
  sessionId: SessionId,
  signingRequestId: Uuid,
): Promise<SolendRetrievalEnvelope> {
  if (IS_SHOWCASE) {
    // Showcase: hook owns simulation; return a synthetic Found shape
    // so the hook's terminal logic can map it. Hook's auto-fixture
    // path will normally bypass this and drive its own simulation.
    return {
      kind: "ok",
      response: {
        status: "found",
        signing_request_id: signingRequestId,
        intent_id: "00000000-0000-0000-0000-000000000000",
        session_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
        unsigned_tx_b64: "AQAAAAAA",
        obligation_signer_backend_partial: true,
        last_valid_block_height: 393666166,
        expires_at_unix_ms: Date.now() + 60_000,
        verified_slot: 415_571_900,
        simulation_slot: 415_571_900,
        units_consumed: 22_000,
      },
    };
  }

  const headers: Record<string, string> = {};
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;
  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/solend-signatures/${signingRequestId}`,
    { method: "GET", headers, cache: "no-store" },
  );

  // Successful body shapes (200 / 410 / 422 / 502 all carry the typed enum).
  // 400 / 404 (session-not-active) / 503 carry `{ error }`.
  if (res.status === 200 || res.status === 410 || res.status === 422 || res.status === 502) {
    const body = (await res.json()) as SolendRetrievalResult;
    return { kind: "ok", response: body };
  }
  if (res.status === 404) {
    // Could be either "session not found" ({ error }) OR the typed
    // `{ status: "not_found" }` body. Try parsing as JSON either way.
    const body = (await res.json().catch(() => ({}))) as
      | SolendRetrievalResult
      | { error?: string };
    if ("status" in body && body.status === "not_found") {
      return { kind: "ok", response: body };
    }
    const errMsg = (body as { error?: string }).error ?? "session not found";
    return { kind: "error", httpStatus: 404, error: errMsg };
  }

  let errorText = "";
  try {
    const body = (await res.json()) as { error?: string };
    errorText = body.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { kind: "error", httpStatus: res.status, error: errorText };
}

export type SolendSubmitEnvelope =
  | { kind: "ok"; response: SolendSubmitResult }
  | { kind: "error"; httpStatus: number; error: string };

export async function submitSolendSignature(
  sessionId: SessionId,
  signingRequestId: Uuid,
  signedTxB64: string,
): Promise<SolendSubmitEnvelope> {
  if (IS_SHOWCASE) {
    // Live mode handles real submit; showcase path is driven by the
    // hook's simulation. If we get here in showcase, return a synthetic
    // accepted shape using the Phase 5G real on-chain hash so the UI
    // shows a working Solscan link end-to-end.
    return {
      kind: "ok",
      response: {
        status: "submitted",
        signing_request_id: signingRequestId,
        intent_id: "00000000-0000-0000-0000-000000000000",
        session_wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
        tx_signature:
          "4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y",
        verified_slot: 415_571_900,
        last_valid_block_height: 393666166,
      },
    };
  }

  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;
  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/solend-signatures/${signingRequestId}`,
    {
      method: "POST",
      headers,
      cache: "no-store",
      body: JSON.stringify({ signed_tx_b64: signedTxB64 }),
    },
  );

  // 200 (Recovered), 202 (Submitted), 410 (Expired), 422 (Rejected),
  // 502 (BroadcastFailed) — all carry the typed `SolendSubmitResult`.
  if ([200, 202, 410, 422, 502].includes(res.status)) {
    const body = (await res.json()) as SolendSubmitResult;
    return { kind: "ok", response: body };
  }
  if (res.status === 404) {
    const body = (await res.json().catch(() => ({}))) as
      | SolendSubmitResult
      | { error?: string };
    if ("status" in body && body.status === "not_found") {
      return { kind: "ok", response: body };
    }
    const errMsg = (body as { error?: string }).error ?? "session not found";
    return { kind: "error", httpStatus: 404, error: errMsg };
  }

  let errorText = "";
  try {
    const body = (await res.json()) as { error?: string };
    errorText = body.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { kind: "error", httpStatus: res.status, error: errorText };
}

// ── Phase 6B Window 3 — JIT signing-handoff prepare ─────────────────────────
//
// `POST /sessions/:s/approvals/:a/solend-signing/prepare` is the new
// Sign-click backend seam: the frontend calls it from the user's
// "Sign with Phantom" click handler, and the daemon constructs a
// fresh signing handoff with a fresh blockhash. The returned
// `signing_request_id` is then immediately used with the existing
// GET retrieve / POST submit endpoints.
//
// Showcase mode short-circuits to a synthetic Ready response that
// echoes `SHOWCASE_SIGNING_REQUEST_ID`, so the existing showcase
// `useSigningHandoff` hook continues to play out its timer-driven
// lifecycle without any daemon traffic.

export type SolendJitPrepareEnvelope =
  | { kind: "ok"; response: SolendJitPrepareResult }
  | { kind: "error"; httpStatus: number; error: string };

export async function prepareSolendSigning(
  sessionId: SessionId,
  approvalRequestId: Uuid,
): Promise<SolendJitPrepareEnvelope> {
  if (IS_SHOWCASE) {
    // Showcase: synthesize a Ready response. The hook's showcase
    // simulation drives the rest of the lifecycle from timers; no
    // network call is made.
    return {
      kind: "ok",
      response: {
        status: "ready",
        approval_request_id: approvalRequestId,
        signing_request_id: SHOWCASE_SIGNING_REQUEST_ID,
        session_id: sessionId,
        wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
        last_valid_block_height: 393_666_166,
        verified_slot: 415_571_900,
        expires_at_unix_ms: Date.now() + 60_000,
      },
    };
  }

  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/approvals/${approvalRequestId}/solend-signing/prepare`,
    { method: "POST", headers, cache: "no-store" },
  );

  // 200 = Ready; 404 = NotFound | JitReadyMissing;
  // 422 = NotApproved | WalletMismatch; 502 = HandoffCreateFailed.
  // All of these carry the typed `SolendJitPrepareResult` body.
  if ([200, 404, 422, 502].includes(res.status)) {
    const body = (await res.json()) as SolendJitPrepareResult;
    return { kind: "ok", response: body };
  }

  // 400 (malformed ids) and 503 (handler not wired) carry `{ error }`.
  let errorText = "";
  try {
    const body = (await res.json()) as { error?: string };
    errorText = body.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { kind: "error", httpStatus: res.status, error: errorText };
}

// ── Phase 6I-G — Solend WITHDRAW JIT-prepare ────────────────────────────────
//
// Backend route added in Agent B Phase 6I-F:
//   POST /sessions/:s/approvals/:a/solend-withdraw-jit/prepare
//
// Mirrors the deposit-side `prepareSolendSigning` envelope shape.
// Frontend dispatches to this when the approval's policy verdict
// rule_name identifies a withdraw flow; the resulting
// `signing_request_id` is fed into the same existing
// `getSolendSignature` / `submitSolendSignature` endpoints — only
// the prepare URL differs from deposit.
//
// Showcase mode short-circuits to a synthetic Ready response so the
// hook's existing showcase simulation continues to play out; no
// network call is made and Phantom is never invoked.

export type SolendWithdrawJitPrepareEnvelope =
  | { kind: "ok"; response: SolendWithdrawJitPrepareResult }
  | { kind: "error"; httpStatus: number; error: string };

export async function prepareSolendWithdrawSigningHandoff(
  sessionId: SessionId,
  approvalRequestId: Uuid,
): Promise<SolendWithdrawJitPrepareEnvelope> {
  if (IS_SHOWCASE) {
    return {
      kind: "ok",
      response: {
        status: "ready",
        approval_request_id: approvalRequestId,
        signing_request_id: SHOWCASE_SIGNING_REQUEST_ID,
        session_id: sessionId,
        wallet: "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
        obligation_pubkey: "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV",
        reserve_pubkey: "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw",
        last_valid_block_height: 393_666_166,
        verified_slot: 415_571_900,
        expires_at_unix_ms: Date.now() + 60_000,
      },
    };
  }

  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/approvals/${approvalRequestId}/solend-withdraw-jit/prepare`,
    { method: "POST", headers, cache: "no-store" },
  );

  // 200 = Ready;
  // 404 = NotFound | WithdrawIntentMissing;
  // 422 = NotApproved | WalletMismatch | RecheckBlocked;
  // 502 = SnapshotAssembleFailed | PlanAssemblyFailed | HandoffCreateFailed.
  // All of these carry the typed `SolendWithdrawJitPrepareResult` body.
  if ([200, 404, 422, 502].includes(res.status)) {
    const body = (await res.json()) as SolendWithdrawJitPrepareResult;
    return { kind: "ok", response: body };
  }

  // 400 (malformed ids) and 503 (handler not wired) carry `{ error }`.
  let errorText = "";
  try {
    const body = (await res.json()) as { error?: string };
    errorText = body.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { kind: "error", httpStatus: res.status, error: errorText };
}

/// Decide an approval. Backend route: `POST /sessions/:id/approve`.
/// Backend uses `approved: boolean` (not a `decision` string).
export async function decideApproval(
  sessionId: SessionId,
  approvalRequestId: Uuid,
  approved: boolean,
  note?: string,
): Promise<{ ok: boolean; httpStatus: number; outcome?: string; error?: string }> {
  if (IS_SHOWCASE) {
    return {
      ok: true,
      httpStatus: 200,
      outcome: approved ? "approved" : "rejected",
    };
  }

  const body: ApproveRequest = {
    request_id: approvalRequestId,
    approved,
    note: note ?? null,
  };
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;
  const res = await fetch(`${GATEWAY_URL}/sessions/${sessionId}/approve`, {
    method: "POST",
    headers,
    cache: "no-store",
    body: JSON.stringify(body),
  });
  if (res.status === 200 || res.status === 202) {
    const data = (await res.json()) as ApproveResponse;
    return { ok: true, httpStatus: res.status, outcome: data.outcome };
  }
  let errorText = "";
  try {
    const data = (await res.json()) as { error?: string };
    errorText = data.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { ok: false, httpStatus: res.status, error: errorText };
}

// ── W5h funding-confirm route ───────────────────────────────────────────────
//
// `POST /sessions/:id/stage2/w5h/funding/confirm` — frontend tells the
// backend that a USDC TransferChecked was broadcast from the user's
// USDC ATA to the controlled-wallet USDC ATA. Backend authoritatively
// re-verifies on-chain (signature exists, transfer instruction match,
// finality) and re-emits the `W5hConditionalDepositResult` with an
// updated `status` — typically `budget_reserved` on the happy path.
//
// As of 2026-05-12 Agent D has NOT shipped this route yet. The
// showcase fixture path below stands in so the manual UI walkthrough
// works end-to-end without a live backend. In live mode the function
// returns a 503 envelope ("route not wired") until Agent D lands it.

const SHOWCASE_FUNDING_SLOT = "419010000";

export async function confirmW5hFunding(
  sessionId: SessionId,
  body: W5hFundingConfirmRequest,
): Promise<W5hFundingConfirmEnvelope> {
  if (IS_SHOWCASE) {
    return showcaseW5hFundingConfirmReply(body);
  }

  const headers: Record<string, string> = { "content-type": "application/json" };
  if (GATEWAY_TOKEN) headers["authorization"] = `Bearer ${GATEWAY_TOKEN}`;

  const res = await fetch(
    `${GATEWAY_URL}/sessions/${sessionId}/stage2/w5h/funding/confirm`,
    {
      method: "POST",
      headers,
      cache: "no-store",
      body: JSON.stringify(body),
    },
  );

  if (res.status === 200) {
    const data = (await res.json()) as W5hConditionalDepositResult;
    return { kind: "ok", response: data };
  }

  let errorText = "";
  try {
    const errBody = (await res.json()) as { error?: string };
    errorText = errBody.error ?? "";
  } catch {
    errorText = (await res.text().catch(() => "")) || res.statusText;
  }
  return { kind: "error", httpStatus: res.status, error: errorText };
}

/// Showcase reply for the W5h funding-confirm route. Returns a
/// `budget_reserved` DTO populated from the request body, so the
/// chat-card flips into the "watching / ready_to_execute" branch and
/// surfaces the W5g approval-command panel.
///
/// The fixture honours an opt-out: if `body.funding_signature` ends
/// in `"fail"` we return `funding_failed` so a demo viewer can
/// exercise the red branch without a real failure on chain.
function showcaseW5hFundingConfirmReply(
  body: W5hFundingConfirmRequest,
): W5hFundingConfirmEnvelope {
  const wantsFailure = body.funding_signature.toLowerCase().endsWith("fail");
  if (wantsFailure) {
    return {
      kind: "ok",
      response: {
        ...buildW5hShowcaseShared(body),
        status: "funding_failed",
        funding_signature: body.funding_signature,
        funding_confirmation_slot: null,
        error_code: "funding_signature_not_found",
        error_reason:
          "Showcase fixture: signature suffix '…fail' triggers the funding_failed branch.",
      },
    };
  }
  return {
    kind: "ok",
    response: {
      ...buildW5hShowcaseShared(body),
      status: W5H_FIXTURE_PINNED.save_display_apy_bps > 100
        ? "ready_to_execute"
        : "budget_reserved",
      funding_signature: body.funding_signature,
      funding_confirmation_slot: SHOWCASE_FUNDING_SLOT,
      current_budget_raw: body.amount_raw,
      condition_met: W5H_FIXTURE_PINNED.save_display_apy_bps > 100,
      error_code: null,
      error_reason: null,
    },
  };
}

/// Build the constant prefix shared by the success + failure W5h
/// confirm fixtures. Honours the request body for rule_id /
/// canonical_hash so the round-trip is internally consistent.
function buildW5hShowcaseShared(body: W5hFundingConfirmRequest) {
  return {
    input_text:
      "(showcase confirm fixture — original chat input not re-echoed)",
    rule_id_hex: body.rule_id_hex,
    canonical_rule_hash_hex:
      body.rule_id_hex === W5H_FIXTURE_PINNED.rule_id_hex
        ? W5H_FIXTURE_PINNED.canonical_rule_hash_hex
        : W5H_FIXTURE_PINNED.canonical_rule_hash_hex,
    amount_raw: body.amount_raw,
    user_wallet: body.user_wallet,
    user_usdc_ata: body.user_usdc_ata,
    controlled_wallet: body.controlled_wallet,
    controlled_usdc_ata: body.controlled_usdc_ata,
    expires_at_ms: (Date.now() + 3 * 60_000).toString(),
    last_checked_slot: SHOWCASE_FUNDING_SLOT,
    threshold_bps: 100,
    threshold_pct_label: "1",
    save_display_apy_bps: W5H_FIXTURE_PINNED.save_display_apy_bps,
    native_onchain_apr_bps: W5H_FIXTURE_PINNED.native_onchain_apr_bps,
    native_onchain_apr_source: W5H_FIXTURE_PINNED.native_onchain_apr_source,
    decision_source: W5H_FIXTURE_PINNED.decision_source,
    refund_signature: null,
  };
}
