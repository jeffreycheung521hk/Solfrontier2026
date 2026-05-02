"use client";

// Phase 6 Day 2 — `<SigningFlow>` panel for `/approval/[id]`.
//
// Renders three sequential phases on top of an existing approval
// request:
//
//  1. Approve (visible while `workflow.state === "pending"`):
//       Approve / Reject buttons → POST /sessions/:id/approve.
//
//  2. Awaiting handoff (visible after approve, before the daemon
//     parks a signing request):
//       In showcase mode the fixture id auto-fills; in live mode the
//       operator pastes the `signing_request_id` (printed by the
//       daemon's audit log / surfaced via the SSE event stream in a
//       later phase).
//
//  3. Sign + submit + confirm (driven by `useSigningHandoff`):
//       "Sign with Phantom" button → Phantom popup → submit to
//       daemon → polling lifecycle → Finalized + Solscan link.

import { useCallback, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { decideApproval, SHOWCASE_SIGNING_REQUEST_ID } from "@/lib/api";
import { IS_SHOWCASE } from "@/lib/mode";
import { useSigningHandoff } from "@/lib/use-signing-handoff";
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

  // In showcase mode the fixture id is known statically, so we can
  // seed it on first render instead of via an effect (avoids a
  // cascading render that ESLint flags). In live mode the operator
  // pastes the id and we set it imperatively below.
  const [signingRequestId, setSigningRequestId] = useState<Uuid | null>(() =>
    IS_SHOWCASE && workflowState === "approved" ? SHOWCASE_SIGNING_REQUEST_ID : null,
  );
  const [pasteValue, setPasteValue] = useState<string>("");

  const { state: handoffState, signWithPhantom, reset } = useSigningHandoff(
    approval.kind === "approved" ? sessionId : null,
    signingRequestId,
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
          if (IS_SHOWCASE) {
            setSigningRequestId(SHOWCASE_SIGNING_REQUEST_ID);
          }
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

  const handlePasteConfirm = useCallback(() => {
    const trimmed = pasteValue.trim();
    if (!isUuid(trimmed)) return;
    setSigningRequestId(trimmed);
  }, [pasteValue]);

  return (
    <div className="space-y-4" data-testid="signing-flow">
      <ApprovePanel
        approval={approval}
        onDecide={handleApprove}
      />

      {approval.kind === "approved" && signingRequestId === null && !IS_SHOWCASE && (
        <ManualHandoffIdPanel
          pasteValue={pasteValue}
          onPaste={setPasteValue}
          onConfirm={handlePasteConfirm}
        />
      )}

      {signingRequestId !== null && (
        <HandoffPanel
          state={handoffState}
          onSign={signWithPhantom}
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
          The operator has approved this request. The daemon resume task is
          parking a partially-signed transaction; once it surfaces the
          handoff, you can sign with Phantom.
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

// ── Manual handoff-id panel (live mode only) ───────────────────────────────

function ManualHandoffIdPanel({
  pasteValue,
  onPaste,
  onConfirm,
}: {
  pasteValue: string;
  onPaste: (v: string) => void;
  onConfirm: () => void;
}) {
  const isValid = isUuid(pasteValue.trim());
  return (
    <Card data-testid="handoff-id-panel">
      <CardHeader className="pb-3">
        <CardTitle className="text-sm">2. Awaiting daemon handoff</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-xs text-muted-foreground">
          The daemon parks the partially-signed transaction with a
          fresh <code>signing_request_id</code>. SSE wiring to surface this
          automatically lands in a later phase. For now, paste the id from
          the daemon&apos;s audit log (event{" "}
          <code>solend_signing_handoff_created</code>):
        </p>
        <div className="flex gap-2">
          <input
            type="text"
            value={pasteValue}
            onChange={(e) => onPaste(e.target.value)}
            placeholder="00000000-0000-0000-0000-000000000000"
            className="flex-1 rounded-md border bg-background px-3 py-1.5 text-sm font-mono
                       focus:outline-none focus:ring-2 focus:ring-ring"
            data-testid="handoff-id-input"
          />
          <Button
            size="sm"
            onClick={onConfirm}
            disabled={!isValid}
            data-testid="handoff-id-confirm"
          >
            Begin polling
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
  onReset,
}: {
  state: SigningHandoffState;
  onSign: () => void | Promise<void>;
  onReset: () => void;
}) {
  return (
    <Card data-testid="handoff-panel">
      <CardHeader className="pb-3">
        <CardTitle className="text-sm flex items-center gap-2">
          <span>{IS_SHOWCASE ? "2." : "3."} Sign &amp; submit</span>
          <HandoffStateBadge state={state} />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <HandoffStateBody state={state} onSign={onSign} onReset={onReset} />
      </CardContent>
    </Card>
  );
}

function HandoffStateBadge({ state }: { state: SigningHandoffState }) {
  const variantMap: Record<SigningHandoffState["kind"], "default" | "secondary" | "destructive" | "outline"> = {
    idle: "outline",
    polling: "secondary",
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
  onReset,
}: {
  state: SigningHandoffState;
  onSign: () => void | Promise<void>;
  onReset: () => void;
}) {
  switch (state.kind) {
    case "idle":
      return <p className="text-xs text-muted-foreground">Waiting to begin.</p>;
    case "polling":
      return (
        <p className="text-xs text-muted-foreground" data-testid="handoff-polling">
          Polling for parked signing handoff… (attempt {state.attempts})
        </p>
      );
    case "ready":
      return (
        <div className="space-y-3" data-testid="handoff-ready">
          <div className="text-xs text-muted-foreground space-y-1">
            <div>
              session wallet:{" "}
              <code className="text-foreground">{state.session_wallet.slice(0, 8)}…{state.session_wallet.slice(-4)}</code>
            </div>
            <div>verified slot: <code className="text-foreground">{state.verified_slot}</code></div>
          </div>
          <Separator />
          <div className="flex items-center gap-2">
            <Button size="sm" onClick={() => void onSign()} data-testid="sign-with-phantom">
              Sign with Phantom
            </Button>
            <span className="text-xs text-muted-foreground">
              Phantom signs the session-wallet slot only; the daemon broadcasts.
            </span>
          </div>
        </div>
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
      return <FailureBody title="Signing TTL elapsed" detail={state.reason} onReset={onReset} />;
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

// ── Util ────────────────────────────────────────────────────────────────────

function isUuid(s: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);
}
