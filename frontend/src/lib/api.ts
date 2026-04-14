// API client: showcase mode reads fixtures; live mode fetches the gateway.
// The surface is stable so pages never branch on mode themselves.

import { IS_SHOWCASE, GATEWAY_URL, GATEWAY_TOKEN } from "@/lib/mode";
import type {
  ApprovalRequest,
  ApprovalWorkflow,
  AuditRow,
  DashboardSnapshot,
  PolicyRule,
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

// ── Wire shapes (mirror Rust DTOs) ───────────────────────────────────────────

interface PolicyRulesResponse { rules: PolicyRule[] }
interface PendingApprovalsResponse {
  items: Array<{ request: ApprovalRequest; workflow: ApprovalWorkflow }>;
}
interface WalletsResponse { wallets: WalletSummary[] }
interface AuditResponse { rows: AuditRow[]; limit: number; offset: number }

// ── Public API surface ───────────────────────────────────────────────────────

export async function fetchPolicyRules(): Promise<PolicyRule[]> {
  if (IS_SHOWCASE) return policyRules;
  const data = await live<PolicyRulesResponse>("/policy/rules");
  return data.rules;
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
  // There is no single-request endpoint yet; filter from the list.
  // Once GET /approvals/:id exists, swap in the direct call.
  const { items } = await live<PendingApprovalsResponse>("/pending-approvals");
  const match = items.find((it) => it.request.id === requestId);
  if (!match) throw new Error(`approval ${requestId} not found`);
  return { request: match.request, workflow: match.workflow, proposal: null };
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
