// API client: showcase mode reads fixtures; live mode fetches the gateway.
// The surface is stable so pages never branch on mode themselves.

import { IS_SHOWCASE, GATEWAY_URL, GATEWAY_TOKEN } from "@/lib/mode";
import type {
  ApprovalRequest,
  ApprovalWorkflow,
  ApproveRequest,
  ApproveResponse,
  AuditRow,
  ChatRequest,
  ChatResponse,
  ChatRouteResult,
  DashboardSnapshot,
  OpenSessionRequest,
  OpenSessionResponse,
  PolicyRule,
  SessionId,
  SolendRetrievalResult,
  SolendSubmitResult,
  TransactionProposal,
  Uuid,
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
