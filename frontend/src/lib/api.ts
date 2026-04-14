// Thin API client. In showcase mode everything resolves from fixtures.
// In live mode, functions fetch from the real gateway. The surface is stable
// so pages don't care which mode they're running under.

import { IS_SHOWCASE, GATEWAY_URL } from "@/lib/mode";
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
  const res = await fetch(`${GATEWAY_URL}${path}`, {
    ...init,
    headers: {
      "content-type": "application/json",
      ...(init?.headers ?? {}),
    },
    cache: "no-store",
  });
  if (!res.ok) throw new Error(`${path} ${res.status}`);
  return res.json() as Promise<T>;
}

export async function fetchDashboard(): Promise<DashboardSnapshot> {
  if (IS_SHOWCASE) return dashboardSnapshot;
  // Live path would aggregate /sessions/*/approvals + wallets + audit.
  // Until the gateway exposes a single dashboard route, this stays fixture-only.
  return dashboardSnapshot;
}

export async function fetchApproval(
  requestId: string,
): Promise<{ request: ApprovalRequest; workflow: ApprovalWorkflow; proposal: TransactionProposal }> {
  if (IS_SHOWCASE) {
    return { request: pendingView.request, workflow: pendingView.workflow, proposal: pendingView.proposal };
  }
  return live(`/approvals/${requestId}`);
}

export async function fetchPolicyRules(): Promise<PolicyRule[]> {
  if (IS_SHOWCASE) return policyRules;
  return live("/policy/rules");
}

export async function fetchAuditTrail(): Promise<AuditRow[]> {
  if (IS_SHOWCASE) return auditRows;
  return live("/audit");
}

export async function fetchWallets(): Promise<WalletSummary[]> {
  if (IS_SHOWCASE) return wallets;
  return live("/wallets");
}

// Demo-only helpers, for walking the canonical flow on a single page.
export const showcase = {
  pending: workflowPending,
  stageTwo: workflowStageTwo,
  approved: workflowApproved,
  expired: workflowExpired,
  flow: demoFlow,
};
