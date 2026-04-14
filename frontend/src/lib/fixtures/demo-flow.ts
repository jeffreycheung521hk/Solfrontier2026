// Canonical demo flow fixtures.
//
// Narrative: a market maker wallet proposes a 50,000 USDC transfer. The amount
// crosses the USDC chain threshold (25,000), so policy routes it through a
// risk → treasury → CFO approval chain with a 5-minute lease. The showcase
// renders four snapshots of the same request so the demo flow can be walked
// end-to-end: pending → stage-advancing → approved → (alt) expired.

import type {
  ApprovalRequest,
  ApprovalWorkflow,
  AuditRow,
  DashboardSnapshot,
  PendingApprovalView,
  PolicyRule,
  SimulationResult,
  TransactionProposal,
  WalletSummary,
} from "@/lib/types";

// ── Shared wallet pubkeys (devnet-style base58 placeholders) ─────────────────
const MM_WALLET = "MMwa11etPubkey1111111111111111111111111111";
const TREASURY_WALLET = "TreasuryWa11etPubkey2222222222222222222222";
const COUNTERPARTY = "Counterpty4444444444444444444444444444444";

const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

const REQUEST_ID = "d3b07384-d9a8-4a52-9f2c-1a2b3c4d5e6f";
const TRANSACTION_ID = "c1a2b3c4-1111-4222-8333-abcdef012345";
const SESSION_ID = "5e5e5e5e-0000-4000-8000-000000000001";

// ── Building blocks ──────────────────────────────────────────────────────────

const proposal: TransactionProposal = {
  id: TRANSACTION_ID,
  session_id: SESSION_ID,
  wallet_pubkey: MM_WALLET,
  network: "devnet",
  description: "Market maker: 50,000 USDC → counterparty",
  transaction_b64: "(omitted in showcase)",
  created_at: "2026-04-14T09:03:12Z",
  instructions_summary: [
    {
      program_id: TOKEN_PROGRAM,
      program_name: "spl-token",
      description: "TransferChecked 50,000 USDC",
      is_legacy_token_transfer: false,
      token_transfer: {
        mint: USDC_MINT,
        from: MM_WALLET,
        to: COUNTERPARTY,
        amount: 50_000_000_000, // 50,000 * 10^6
        decimals: 6,
      },
      accounts: [
        { pubkey: MM_WALLET, label: "source ATA owner", is_signer: true, is_writable: true },
        { pubkey: COUNTERPARTY, label: "destination ATA", is_signer: false, is_writable: true },
        { pubkey: USDC_MINT, label: "USDC mint", is_signer: false, is_writable: false },
      ],
    },
  ],
};

const simulation: SimulationResult = {
  success: true,
  error: null,
  compute_units_used: 12_450,
  logs: [
    "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [1]",
    "Program log: Instruction: TransferChecked",
    "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
  ],
  return_data: null,
  account_diffs: [
    { pubkey: MM_WALLET, lamports_before: 2_340_000_000, lamports_after: 2_339_995_000, data_changed: true },
    { pubkey: COUNTERPARTY, lamports_before: 1_100_000_000, lamports_after: 1_100_000_000, data_changed: true },
  ],
  fee_lamports: 5_000,
};

const baseRequest: ApprovalRequest = {
  id: REQUEST_ID,
  session_id: SESSION_ID,
  transaction_id: TRANSACTION_ID,
  description: "50,000 USDC transfer (exceeds 25k chain threshold)",
  policy_verdict: {
    type: "RequiresHumanApproval",
    reason: "USDC transfer amount 50,000 exceeds chain threshold 25,000",
    rule_name: "usdc-high-value-chain",
    required_approver_role: "risk",
    approval_chain: [
      { role: "risk", description: "first-line risk review", min_approvals: 1 },
      { role: "treasury", description: "treasury sign-off", min_approvals: 2 },
      { role: "cfo", description: "final authorization", min_approvals: 1 },
    ],
  },
  simulation,
  requested_at: "2026-04-14T09:03:14Z",
  decided: false,
  required_approver_role: "risk",
};

// ── Workflow states at four points in the demo ──────────────────────────────

export const workflowPending: ApprovalWorkflow = {
  request_id: REQUEST_ID,
  session_id: SESSION_ID,
  state: "pending",
  stages: [
    { index: 0, allowed_roles: ["risk"], min_approvals: 1, decisions: [] },
    { index: 1, allowed_roles: ["treasury"], min_approvals: 2, decisions: [] },
    { index: 2, allowed_roles: ["cfo"], min_approvals: 1, decisions: [] },
  ],
  created_at: "2026-04-14T09:03:14Z",
  updated_at: "2026-04-14T09:03:14Z",
  expires_at: "2026-04-14T09:08:14Z",
};

export const workflowStageTwo: ApprovalWorkflow = {
  ...workflowPending,
  state: "pending",
  updated_at: "2026-04-14T09:04:02Z",
  stages: [
    {
      index: 0,
      allowed_roles: ["risk"],
      min_approvals: 1,
      decisions: [
        { approved: true, operator_id: "op_risk_alice", approver_role: "risk", decided_at: "2026-04-14T09:04:02Z" },
      ],
    },
    { index: 1, allowed_roles: ["treasury"], min_approvals: 2, decisions: [
      { approved: true, operator_id: "op_treasury_bob", approver_role: "treasury", decided_at: "2026-04-14T09:05:31Z" },
    ] },
    { index: 2, allowed_roles: ["cfo"], min_approvals: 1, decisions: [] },
  ],
};

export const workflowApproved: ApprovalWorkflow = {
  ...workflowStageTwo,
  state: "approved",
  updated_at: "2026-04-14T09:07:20Z",
  stages: [
    workflowStageTwo.stages[0],
    {
      ...workflowStageTwo.stages[1],
      decisions: [
        ...workflowStageTwo.stages[1].decisions,
        { approved: true, operator_id: "op_treasury_carol", approver_role: "treasury", decided_at: "2026-04-14T09:06:45Z" },
      ],
    },
    {
      index: 2,
      allowed_roles: ["cfo"],
      min_approvals: 1,
      decisions: [
        { approved: true, operator_id: "op_cfo_dave", approver_role: "cfo", decided_at: "2026-04-14T09:07:20Z" },
      ],
    },
  ],
};

export const workflowExpired: ApprovalWorkflow = {
  ...workflowPending,
  state: "expired",
  updated_at: "2026-04-14T09:08:14Z",
};

// ── Default composed pending view (used by Dashboard / Review by default) ────

export const pendingView: PendingApprovalView = {
  request: baseRequest,
  workflow: workflowStageTwo,
  proposal,
};

// ── Additional pending items for dashboard list realism ──────────────────────

const otherPending: PendingApprovalView[] = [
  {
    request: {
      ...baseRequest,
      id: "a1111111-0000-4000-8000-000000000002",
      transaction_id: "b1111111-0000-4000-8000-000000000002",
      description: "250 SOL transfer from treasury hot wallet",
      policy_verdict: {
        type: "RequiresHumanApproval",
        reason: "Amount 250 SOL exceeds 100 SOL cap",
        rule_name: "treasury-high-value",
        required_approver_role: "treasury",
        approval_chain: null,
      },
      required_approver_role: "treasury",
    },
    workflow: {
      request_id: "a1111111-0000-4000-8000-000000000002",
      session_id: SESSION_ID,
      state: "pending",
      stages: [{ index: 0, allowed_roles: ["treasury"], min_approvals: 1, decisions: [] }],
      created_at: "2026-04-14T09:01:00Z",
      updated_at: "2026-04-14T09:01:00Z",
      expires_at: "2026-04-14T09:06:00Z",
    },
    proposal: {
      ...proposal,
      id: "b1111111-0000-4000-8000-000000000002",
      description: "Treasury: 250 SOL → Solend deposit",
      wallet_pubkey: TREASURY_WALLET,
      instructions_summary: [
        {
          program_id: "11111111111111111111111111111111",
          program_name: "system",
          description: "SOL transfer 250 SOL",
          transfer_lamports: 250_000_000_000,
          is_legacy_token_transfer: false,
          accounts: [],
        },
      ],
    },
  },
];

// ── Wallet summaries ────────────────────────────────────────────────────────

export const wallets: WalletSummary[] = [
  {
    pubkey: MM_WALLET,
    label: "Market maker hot",
    signer_type: "External",
    daily_spend_lamports: 72_500_000_000,
    policy: {
      max_amount_lamports: 1_000_000_000_000,
      program_allowlist: [TOKEN_PROGRAM, "11111111111111111111111111111111"],
      required_approver_role: "risk",
      rules: [],
    },
  },
  {
    pubkey: TREASURY_WALLET,
    label: "Treasury hot",
    signer_type: "External",
    daily_spend_lamports: 310_000_000_000,
    policy: {
      max_amount_lamports: 100_000_000_000,
      program_allowlist: [],
      required_approver_role: "treasury",
      rules: [],
    },
  },
];

// ── Audit trail ──────────────────────────────────────────────────────────────

export const auditRows: AuditRow[] = [
  {
    id: "evt-0001",
    session_id: SESSION_ID,
    correlation_id: REQUEST_ID,
    occurred_at: Date.parse("2026-04-14T09:07:20Z"),
    event_type: "human_approved",
    actor: "op_cfo_dave",
    payload: JSON.stringify({ rule: "usdc-high-value-chain", stage: 2, role: "cfo" }),
    severity: "info",
  },
  {
    id: "evt-0002",
    session_id: SESSION_ID,
    correlation_id: REQUEST_ID,
    occurred_at: Date.parse("2026-04-14T09:06:45Z"),
    event_type: "quorum_progress",
    actor: "op_treasury_carol",
    payload: JSON.stringify({ stage: 1, approvals_so_far: 2, approvals_required: 2 }),
    severity: "info",
  },
  {
    id: "evt-0003",
    session_id: SESSION_ID,
    correlation_id: "deadbeef-0000-4000-8000-000000000099",
    occurred_at: Date.parse("2026-04-14T08:41:00Z"),
    event_type: "approval_lease_expired",
    actor: "system",
    payload: JSON.stringify({ rule: "usdc-medium-value-requires-human", lease_seconds: 300 }),
    severity: "warning",
  },
  {
    id: "evt-0004",
    session_id: SESSION_ID,
    correlation_id: "feedface-0000-4000-8000-0000000000aa",
    occurred_at: Date.parse("2026-04-14T08:22:10Z"),
    event_type: "policy_rejected",
    actor: "system",
    payload: JSON.stringify({ rule: "block-legacy-token-transfer", reason: "legacy SPL Token Transfer is not allowed" }),
    severity: "warning",
  },
  {
    id: "evt-0005",
    session_id: SESSION_ID,
    correlation_id: "cafef00d-0000-4000-8000-0000000000bb",
    occurred_at: Date.parse("2026-04-14T07:58:44Z"),
    event_type: "human_rejected",
    actor: "op_risk_alice",
    payload: JSON.stringify({ rule: "usdc-high-value-chain", stage: 0, note: "counterparty not whitelisted" }),
    severity: "warning",
  },
];

// ── Policy rules shown in Policy View ────────────────────────────────────────

export const policyRules: PolicyRule[] = [
  {
    name: "block-legacy-token-transfer",
    description: "Reject SPL Token legacy Transfer (use TransferChecked)",
    condition: { type: "LegacyTokenTransferPresent" },
    action: { type: "Reject", reason: "legacy SPL Token Transfer is not allowed; use TransferChecked so the mint is visible to policy" },
  },
  {
    name: "usdc-high-value-chain",
    description: "USDC > 25k requires risk → treasury (2-quorum) → CFO",
    condition: { type: "TokenAmountExceeds", mint: USDC_MINT, threshold: 25_000_000_000 },
    action: {
      type: "RequireApprovalChain",
      reason: "USDC transfer above chain threshold",
      stages: [
        { role: "risk", description: "first-line risk review", min_approvals: 1 },
        { role: "treasury", description: "treasury sign-off", min_approvals: 2 },
        { role: "cfo", description: "final authorization", min_approvals: 1 },
      ],
    },
  },
  {
    name: "usdc-medium-value-requires-human",
    description: "USDC > 500 requires single human approver",
    condition: { type: "TokenAmountExceeds", mint: USDC_MINT, threshold: 500_000_000 },
    action: { type: "RequireHumanApproval", reason: "USDC transfer above single-approver threshold", required_approver_role: "treasury" },
  },
  {
    name: "global-approve",
    description: "Fallback: auto-approve everything else",
    condition: { type: "Always" },
    action: { type: "Approve" },
  },
];

// ── Dashboard snapshot ──────────────────────────────────────────────────────

export const dashboardSnapshot: DashboardSnapshot = {
  pending: [pendingView, ...otherPending],
  wallets,
  recent_audit: auditRows,
  expiring_soon: [
    {
      request_id: "a1111111-0000-4000-8000-000000000002",
      expires_at: "2026-04-14T09:06:00Z",
      seconds_remaining: 42,
    },
    {
      request_id: REQUEST_ID,
      expires_at: "2026-04-14T09:08:14Z",
      seconds_remaining: 176,
    },
  ],
};

export const demoFlow = {
  proposal,
  simulation,
  request: baseRequest,
  workflowPending,
  workflowStageTwo,
  workflowApproved,
  workflowExpired,
};
