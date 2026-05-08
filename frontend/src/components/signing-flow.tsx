"use client";

// Phase 6B Window 3 — `<SigningFlow>` panel for `/approval/[id]`.
//
// Now drives the JIT-prepare flow: when the operator clicks Sign with
// Phantom, the frontend POSTs the new prepare endpoint to mint a fresh
// signing_request_id with a fresh blockhash, then immediately runs the
// existing retrieve → Phantom-sign → submit chain. No manual UUID
// paste is required on the happy path.
//
// Flow:
//
//   1. Approve (visible while workflow is `pending`):
//        Approve / Reject buttons → POST /sessions/:id/approve.
//
//   2. Sign + submit + confirm (visible once approved):
//        "Sign with Phantom" button (idle state) → preparing →
//        signing → submitting → submitted → confirming → finalized.
//        On `expired` (pre_submit_expired from submit), a "Sign again"
//        button re-runs the chain with a fresh blockhash via prepare,
//        without needing re-approval.

import { useCallback, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { decideApproval } from "@/lib/api";
import { IS_SHOWCASE } from "@/lib/mode";
import {
  type SolendSigningAction,
  useSigningHandoff,
} from "@/lib/use-signing-handoff";
import type {
  ApprovalWorkflowState,
  SessionId,
  SigningHandoffState,
  Uuid,
} from "@/lib/types";

interface SigningFlowProps {
  approvalRequestId: Uuid;
  sessionId: SessionId;
  workflowState: ApprovalWorkflowState;
  /** Phase 6I-G — selects the Solend prepare endpoint. Approval page
   *  derives this from `request.policy_verdict.rule_name`. Defaults to
   *  `"deposit"` so older callers (and showcase) keep their behavior. */
  action?: SolendSigningAction;
}

type ApprovalUiState =
  | { kind: "pending" }
  | { kind: "approving" }
  | { kind: "approved" }
  | { kind: "rejected"; reason?: string }
  | { kind: "error"; error: string };

const SOLSCAN_TX = (sig: string) => `https://solscan.io/tx/${sig}`;

export function SigningFlow({
  approvalRequestId,
  sessionId,
  workflowState,
  action = "deposit",
}: SigningFlowProps) {
  // Track approval UI state separately from the workflow prop so we can
  // optimistically transition through `approving → approved` after a
  // successful POST without waiting for a workflow refetch.
  const [approval, setApproval] = useState<ApprovalUiState>(() =>
    workflowState === "pending"
      ? { kind: "pending" }
      : workflowState === "approved"
        ? { kind: "approved" }
        : workflowState === "rejected"
          ? { kind: "rejected" }
          : { kind: "error", error: `workflow is ${workflowState}` },
  );

  // Phase 6B Window 3: hook now takes approvalRequestId. The
  // signing_request_id is minted JIT inside signWithPhantom() on each
  // user click — no manual paste, no upfront polling.
  //
  // Phase 6I-G: `action` selects deposit vs withdraw_all prepare
  // endpoint inside the hook. Default is "deposit" for back-compat.
  const { state: handoffState, signWithPhantom, reset } = useSigningHandoff(
    approval.kind === "approved" ? sessionId : null,
    approval.kind === "approved" ? approvalRequestId : null,
    { action },
  );

  const handleApprove = useCallback(
    async (approved: boolean) => {
      setApproval({ kind: "approving" });
      try {
        const res = await decideApproval(sessionId, approvalRequestId, approved);
        if (!res.ok) {
          setApproval({
            kind: "error",
            error: res.error ?? `gateway returned HTTP ${res.httpStatus}`,
          });
          return;
        }
        if (approved) {
          setApproval({ kind: "approved" });
        } else {
          setApproval({ kind: "rejected", reason: "operator chose reject" });
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : "network error";
        setApproval({ kind: "error", error: msg });
      }
    },
    [sessionId, approvalRequestId],
  );

  const handleSignAgain = useCallback(() => {
    reset();
    // Restart the chain on the next event loop tick so the reset's
    // state housekeeping flushes before the new gesture call.
    queueMicrotask(() => {
      void signWithPhantom();
    });
  }, [reset, signWithPhantom]);

  return (
    <div className="space-y-4" data-testid="signing-flow">
      <ApprovePanel
        approval={approval}
        onDecide={handleApprove}
      />

      {approval.kind === "approved" && (
        <HandoffPanel
          state={handoffState}
          onSign={signWithPhantom}
          onSignAgain={handleSignAgain}
          onReset={reset}
        />
      )}
    </div>
  );
}

// ── Approve panel ──────────────────────────────────────────────────────────

function ApprovePanel({
  approval,
  onDecide,
}: {
  approval: ApprovalUiState;
  onDecide: (approved: boolean) => void | Promise<void>;
}) {
  if (approval.kind === "approved") {
    return (
      <Card data-testid="approve-panel-approved">
        <CardHeader className="pb-3">
          <CardTitle className="text-sm flex items-center gap-2">
            <span>1. Approval</span>
            <Badge variant="default">approved</Badge>
          </CardTitle>
        </CardHeader>
        <CardContent className="text-xs text-muted-foreground">
          The operator has approved this request. Click Sign with Phantom
          below — the daemon will assemble a fresh transaction with a
          fresh blockhash on demand and hand it to your wallet.
        </CardContent>
      </Card>
    );
  }
  if (approval.kind === "rejected") {
    return (
      <Card data-testid="approve-panel-rejected">
        <CardHeader className="pb-3">
          <CardTitle className="text-sm flex items-center gap-2">
            <span>1. Approval</span>
            <Badge variant="destructive">rejected</Badge>
          </CardTitle>
        </CardHeader>
        <CardContent className="text-xs text-muted-foreground">
          {approval.reason ?? "Rejected. The parked signing task has been signalled and dropped."}
        </CardContent>
      </Card>
    );
  }
  if (approval.kind === "error") {
    return (
      <Alert>
        <AlertTitle>Approval failed</AlertTitle>
        <AlertDescription>{approval.error}</AlertDescription>
      </Alert>
    );
  }
  return (
    <Card data-testid="approve-panel-pending">
      <CardHeader className="pb-3">
        <CardTitle className="text-sm">1. Approval</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-xs text-muted-foreground">
          The LLM proposed only — clicking <strong>Approve</strong> below
          signals the daemon&apos;s resume task. Approval does NOT broadcast;
          the next step is wallet signing.
        </p>
        <div className="flex items-center gap-2">
          <Button
            size="sm"
            onClick={() => void onDecide(true)}
            disabled={approval.kind === "approving"}
            data-testid="approve-button"
          >
            {approval.kind === "approving" ? "Approving…" : "Approve"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => void onDecide(false)}
            disabled={approval.kind === "approving"}
            data-testid="reject-button"
          >
            Reject
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Handoff lifecycle panel ────────────────────────────────────────────────

function HandoffPanel({
  state,
  onSign,
  onSignAgain,
  onReset,
}: {
  state: SigningHandoffState;
  onSign: () => void | Promise<void>;
  onSignAgain: () => void;
  onReset: () => void;
}) {
  return (
    <Card data-testid="handoff-panel">
      <CardHeader className="pb-3">
        <CardTitle className="text-sm flex items-center gap-2">
          <span>2. Sign &amp; submit</span>
          <HandoffStateBadge state={state} />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <HandoffStateBody
          state={state}
          onSign={onSign}
          onSignAgain={onSignAgain}
          onReset={onReset}
        />
      </CardContent>
    </Card>
  );
}

function HandoffStateBadge({ state }: { state: SigningHandoffState }) {
  const variantMap: Record<
    SigningHandoffState["kind"],
    "default" | "secondary" | "destructive" | "outline"
  > = {
    idle: "default",
    polling: "secondary",
    preparing: "secondary",
    prepare_failed: "destructive",
    ready: "default",
    signing: "secondary",
    submitting: "secondary",
    submitted: "secondary",
    confirming: "secondary",
    finalized: "default",
    rejected: "destructive",
    broadcast_failed: "destructive",
    expired: "outline",
    not_found: "outline",
    execution_failed: "destructive",
    confirmation_timeout: "destructive",
    error: "destructive",
  };
  const label = state.kind.replace(/_/g, " ");
  return <Badge variant={variantMap[state.kind]}>{label}</Badge>;
}

function HandoffStateBody({
  state,
  onSign,
  onSignAgain,
  onReset,
}: {
  state: SigningHandoffState;
  onSign: () => void | Promise<void>;
  onSignAgain: () => void;
  onReset: () => void;
}) {
  switch (state.kind) {
    case "idle":
      return (
        <div className="space-y-3" data-testid="handoff-idle">
          <p className="text-xs text-muted-foreground">
            Approval recorded. Clicking Sign with Phantom triggers the
            daemon to assemble a fresh transaction with a fresh
            blockhash, then immediately hands it to your wallet for
            signing. No transaction is built or signed in advance.
          </p>
          <Separator />
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              onClick={() => void onSign()}
              data-testid="sign-with-phantom"
            >
              Sign with Phantom
            </Button>
            <span className="text-xs text-muted-foreground">
              Phantom signs the session-wallet slot only; the daemon broadcasts.
            </span>
          </div>
        </div>
      );
    case "polling":
      return (
        <p className="text-xs text-muted-foreground" data-testid="handoff-polling">
          Awaiting confirmation… (attempt {state.attempts})
        </p>
      );
    case "preparing":
      return (
        <p className="text-xs text-muted-foreground" data-testid="handoff-preparing">
          Asking daemon to assemble a fresh signing handoff…
        </p>
      );
    case "prepare_failed":
      return (
        <FailureBody
          title="Could not prepare signing handoff"
          detail={state.reason}
          onReset={onReset}
          extra={
            <Button
              size="sm"
              onClick={() => void onSign()}
              data-testid="prepare-retry"
            >
              Retry
            </Button>
          }
        />
      );
    case "ready":
      // Transient state if the daemon's retrieve returned `found`
      // but the hook is still in flight. Render a calm wait — the
      // chain should advance to `signing` within a few ms.
      return (
        <p className="text-xs text-muted-foreground">
          Handoff fetched — opening Phantom…
        </p>
      );
    case "signing":
      return (
        <p className="text-xs text-muted-foreground">
          Phantom popup open — confirm in your wallet…
        </p>
      );
    case "submitting":
      return <p className="text-xs text-muted-foreground">Submitting signed tx to daemon…</p>;
    case "submitted":
      return (
        <div className="space-y-2" data-testid="handoff-submitted">
          <p className="text-xs text-muted-foreground">
            Daemon accepted submission. Confirmation tracker is polling
            <code className="ml-1">getSignatureStatuses</code>…
          </p>
          <SolscanRow signature={state.tx_signature} />
        </div>
      );
    case "confirming":
      return (
        <div className="space-y-2" data-testid="handoff-confirming">
          <p className="text-xs text-muted-foreground">
            Confirming on chain. slot <code className="text-foreground">{state.slot}</code>
          </p>
          <SolscanRow signature={state.tx_signature} />
        </div>
      );
    case "finalized":
      return (
        <div className="space-y-2" data-testid="handoff-finalized">
          <Alert>
            <AlertTitle>Finalized on Solana mainnet</AlertTitle>
            <AlertDescription>
              slot <code>{state.slot}</code>. The lifecycle reached terminal
              success — the proposal was simulated, approved, signed by the
              user&apos;s wallet, broadcast and confirmed without any LLM
              touching signing or submit.
            </AlertDescription>
          </Alert>
          <SolscanRow signature={state.tx_signature} />
        </div>
      );
    case "rejected":
      return <FailureBody title="Verification rejected" detail={state.error} onReset={onReset} />;
    case "broadcast_failed":
      return (
        <FailureBody
          title="Broadcast failed"
          detail={state.error}
          onReset={onReset}
          extra={state.tx_signature ? <SolscanRow signature={state.tx_signature} /> : null}
        />
      );
    case "expired":
      // Phase 6B Window 3: blockhash expired between sign and submit.
      // The fix is to click Sign again so prepare mints a fresh
      // signing handoff. No re-approval needed.
      return (
        <FailureBody
          title="Blockhash expired before broadcast"
          detail={`${state.reason}. Click Sign again to prepare a fresh transaction — no re-approval needed.`}
          onReset={onReset}
          extra={
            <Button
              size="sm"
              onClick={onSignAgain}
              data-testid="sign-again"
            >
              Sign again
            </Button>
          }
        />
      );
    case "not_found":
      return (
        <FailureBody
          title="Handoff not found"
          detail="Either the id is wrong, the entry expired, or the session does not own it."
          onReset={onReset}
        />
      );
    case "execution_failed":
      return (
        <FailureBody
          title="Transaction failed on chain"
          detail={state.err}
          onReset={onReset}
          extra={<SolscanRow signature={state.tx_signature} />}
        />
      );
    case "confirmation_timeout":
      return (
        <FailureBody
          title="Confirmation timeout"
          detail={`${state.reason}${state.requires_reproposal ? " — re-proposal required" : ""}`}
          onReset={onReset}
          extra={<SolscanRow signature={state.tx_signature} />}
        />
      );
    case "error":
      return <FailureBody title="Transport error" detail={state.error} onReset={onReset} />;
  }
}

function FailureBody({
  title,
  detail,
  onReset,
  extra,
}: {
  title: string;
  detail: string;
  onReset: () => void;
  extra?: React.ReactNode;
}) {
  return (
    <div className="space-y-2">
      <Alert>
        <AlertTitle>{title}</AlertTitle>
        <AlertDescription className="break-words">{detail}</AlertDescription>
      </Alert>
      {extra}
      <Button size="sm" variant="ghost" onClick={onReset} className="text-xs">
        reset
      </Button>
    </div>
  );
}

function SolscanRow({ signature }: { signature: string }) {
  return (
    <div className="text-xs text-muted-foreground">
      tx{" "}
      <a
        href={SOLSCAN_TX(signature)}
        target="_blank"
        rel="noopener noreferrer"
        className="font-mono underline hover:text-foreground"
        data-testid="solscan-link"
      >
        {signature.slice(0, 8)}…{signature.slice(-6)}
      </a>
    </div>
  );
}

// Note: the manual signing_request_id paste UI from prior phases is
// intentionally removed in Window 3. The prepare endpoint mints the
// id JIT on every Sign click; pasting an external id would bypass
// the freshness guarantee and is no longer offered as a UI affordance.

// Suppress unused-import warning when IS_SHOWCASE is referenced only
// indirectly through the hook's branching (see use-signing-handoff.ts).
void IS_SHOWCASE;
