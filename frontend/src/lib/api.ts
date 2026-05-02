// API client: showcase mode reads fixtures; live mode fetches the gateway.
// The surface is stable so pages never branch on mode themselves.

import { IS_SHOWCASE, GATEWAY_URL, GATEWAY_TOKEN } from "@/lib/mode";
import type {
  ApprovalRequest,
  ApprovalWorkflow,
  AuditRow,
  ChatRequest,
  ChatResponse,
  ChatRouteResult,
  DashboardSnapshot,
  OpenSessionRequest,
  OpenSessionResponse,
  PolicyRule,
  SessionId,
  TransactionProposal,
  WalletSummary,
} from "@/lib/types";
import {
  dashboardSnapshot,
  pendingView,
  policyRules,
  auditRows,
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
    return {
      request: pendingView.request,
      workflow: pendingView.workflow,
      proposal: pendingView.proposal,
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

/// Showcase-mode reply: returns a deterministic ChatRouteResult based on
/// the message content. The fixture mirrors the safe paths the live
/// backend would take. No network call.
function showcaseChatReply(message: string): ChatRouteResult {
  const lower = message.toLowerCase();
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
