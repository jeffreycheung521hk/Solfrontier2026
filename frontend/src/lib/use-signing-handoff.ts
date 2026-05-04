// Phase 6B Window 3 — Signing handoff state machine, JIT-prepare flow.
//
// New lifecycle (live mode):
//
//   idle
//     │  (user clicks Sign with Phantom — gesture-rooted)
//     ▼
//   preparing  ── POST /sessions/:s/approvals/:a/solend-signing/prepare
//     │  on Ready: capture signing_request_id, then →
//     ▼  GET /sessions/:s/solend-signatures/:r
//   signing     ── Phantom popup → signTransaction
//     │
//     ▼
//   submitting  ── POST /sessions/:s/solend-signatures/:r
//     │
//     ▼
//   submitted → confirming → finalized
//                (post-submit polling via getSolendSignature, unchanged)
//
// What changed vs the pre-Window-3 hook:
//
//  - Hook input: `approvalRequestId` instead of `signingRequestId`. The
//    operator no longer pastes a UUID; the prepare endpoint produces
//    a fresh signing_request_id on every Sign click, with a fresh
//    blockhash. This is the structural fix for the manual-flow timing
//    race observed twice on 2026-05-04 where the blockhash expired
//    between approval-time tx assembly and the user clicking Approve
//    in Phantom.
//
//  - Pre-sign polling is GONE. The hook does not poll the retrieve
//    endpoint until AFTER submit. Pre-sign work is one shot per click.
//
//  - `preparing` and `prepare_failed` states are new.
//
//  - `expired` state remains the signal for `pre_submit_expired`
//    coming back from submit. The UI now treats this as retryable —
//    a "Sign again" button calls signWithPhantom() which restarts
//    the prepare → fetch → sign → submit chain (with a fresh
//    blockhash via prepare).
//
// Strict rules upheld here (mirror Phase 4C / 5G safety posture):
//
//  - **No auto-popup.** The Phantom popup only fires inside
//    `signWithPhantom`, which the UI binds to a button click. Nothing
//    else triggers Phantom.
//  - **No auto-retry.** Failures halt the chain. The UI may expose a
//    retry button (which re-calls signWithPhantom from a click).
//  - **No internal prepare-retry loop.** One click = one prepare.
//  - **Daemon broadcasts.** This hook never calls
//    `signAndSendTransaction`; it calls Phantom's `signTransaction`,
//    base64-encodes the signed bytes, and POSTs to the daemon.
//
// Showcase mode runs the same deterministic timer-driven lifecycle as
// before; `signWithPhantom` triggers it without any HTTP / Phantom.

import { useCallback, useEffect, useRef, useState } from "react";

import { IS_SHOWCASE } from "@/lib/mode";
import { signSolendTransaction } from "@/lib/phantom";
import {
  getSolendSignature,
  prepareSolendSigning,
  submitSolendSignature,
} from "@/lib/api";
import type {
  SessionId,
  SigningHandoffState,
  SolendRetrievalResult,
  SolendSubmitResult,
  Uuid,
} from "@/lib/types";

// Phase 5G real on-chain hash, used by the showcase fixture so the
// `finalized` state's Solscan link goes to a real Finalized tx.
const SHOWCASE_FINALIZED_TX_SIGNATURE =
  "4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y";
const SHOWCASE_FINALIZED_SLOT = 415_571_964;

// Polling cadence for post-submit confirming → finalized.
const POLL_INTERVAL_MS = 1_500;

export interface UseSigningHandoffOptions {
  /** When false, the hook never calls Phantom even if `signWithPhantom`
   *  is invoked — used by showcase mode so judges don't need Phantom
   *  installed. */
  usePhantom?: boolean;
}

export interface UseSigningHandoffResult {
  state: SigningHandoffState;
  /** User-initiated. Drives the full prepare → retrieve → sign →
   *  submit chain inside one click handler. Phantom popup fires
   *  inside. No-op if a chain is already in flight. */
  signWithPhantom: () => Promise<void>;
  /** Reset to `idle`, stop the post-submit polling loop, drop any
   *  cached signing_request_id. Safe to call any time. */
  reset: () => void;
}

export function useSigningHandoff(
  sessionId: SessionId | null,
  approvalRequestId: Uuid | null,
  options?: UseSigningHandoffOptions,
): UseSigningHandoffResult {
  const usePhantom = options?.usePhantom ?? !IS_SHOWCASE;

  const [state, setState] = useState<SigningHandoffState>({ kind: "idle" });

  // Refs the post-submit polling loop reads. signingRequestIdRef is
  // set once a prepare call returns Ready, then used by the polling
  // loop after submit accepts. Reset clears it.
  const sessionIdRef = useRef<SessionId | null>(sessionId);
  const approvalRequestIdRef = useRef<Uuid | null>(approvalRequestId);
  const signingRequestIdRef = useRef<Uuid | null>(null);
  sessionIdRef.current = sessionId;
  approvalRequestIdRef.current = approvalRequestId;

  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelledRef = useRef(false);
  const inFlightRef = useRef(false);

  function clearPollTimer() {
    if (pollTimerRef.current !== null) {
      clearTimeout(pollTimerRef.current);
      pollTimerRef.current = null;
    }
  }

  // Showcase mode timers (cleared on reset / unmount).
  const showcaseTimers = useRef<ReturnType<typeof setTimeout>[]>([]);
  function clearShowcaseTimers() {
    showcaseTimers.current.forEach((t) => clearTimeout(t));
    showcaseTimers.current = [];
  }

  const reset = useCallback(() => {
    clearPollTimer();
    clearShowcaseTimers();
    cancelledRef.current = true;
    inFlightRef.current = false;
    signingRequestIdRef.current = null;
    setState({ kind: "idle" });
    // Reopen the gate after the synchronous reset settles so a
    // subsequent click can run cleanly.
    queueMicrotask(() => {
      cancelledRef.current = false;
    });
  }, []);

  // Reset whenever the input identity changes (new approval, new
  // session). No popup is triggered here — only state housekeeping.
  useEffect(() => {
    cancelledRef.current = false;
    inFlightRef.current = false;
    signingRequestIdRef.current = null;
    clearPollTimer();
    clearShowcaseTimers();
    setState({ kind: "idle" });

    return () => {
      cancelledRef.current = true;
      clearPollTimer();
      clearShowcaseTimers();
    };
  }, [sessionId, approvalRequestId]);

  // ── Post-submit polling loop ──────────────────────────────────────────
  //
  // Runs only AFTER submit returns submitted/recovered. Drives the
  // confirming → finalized lifecycle by polling the existing GET
  // retrieve endpoint. Never triggers Phantom, never re-broadcasts.
  //
  // Declared as a hoisted `function` so the body can recurse via
  // `setTimeout(pollOnce, ...)` without temporal-dead-zone errors.
  // Identity changes per render, but it is only referenced by its
  // own setTimeout (started from within `signWithPhantom`) — never
  // captured in a deps array.
  async function pollOnce(): Promise<void> {
    const sid = sessionIdRef.current;
    const rid = signingRequestIdRef.current;
    if (!sid || !rid) return;

    let envelope;
    try {
      envelope = await getSolendSignature(sid, rid);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "network error";
      if (cancelledRef.current) return;
      setState({ kind: "error", error: msg });
      return;
    }

    if (cancelledRef.current) return;

    if (envelope.kind === "error") {
      if (envelope.httpStatus === 404) {
        setState({ kind: "not_found" });
      } else {
        setState({
          kind: "error",
          error: envelope.error || `gateway returned HTTP ${envelope.httpStatus}`,
        });
      }
      return;
    }

    const next = mapRetrievalToState(envelope.response);

    // Only let the post-submit poll progress states forward —
    // never roll BACK to ready (which would surface an unsigned tx
    // again). If the daemon reports "found" again here, treat it as
    // a stale read and keep polling without changing state.
    if (next.kind !== "ready") {
      setState(next);
    }

    if (isTerminal(next.kind)) return;

    pollTimerRef.current = setTimeout(() => {
      void pollOnce();
    }, POLL_INTERVAL_MS);
  }

  // ── User action: signWithPhantom (the only Phantom-popup site) ────────
  const signWithPhantom = useCallback(async (): Promise<void> => {
    if (inFlightRef.current) return;

    const sid = sessionIdRef.current;
    const aid = approvalRequestIdRef.current;
    if (!sid || !aid) return;

    inFlightRef.current = true;
    cancelledRef.current = false;
    clearPollTimer();
    clearShowcaseTimers();

    setState({ kind: "preparing" });

    if (IS_SHOWCASE) {
      // Showcase: skip real prepare/retrieve/sign/submit. Drive the
      // lifecycle with timers, using Phase 5G's real on-chain hash so
      // the Solscan link in `finalized` points at a real Finalized tx.
      // Same five-state walk as before; the start point is now
      // user-click instead of effect-driven.
      const t1 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({ kind: "signing" });
      }, 600);
      const t2 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({ kind: "submitting" });
      }, 1_200);
      const t3 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "submitted",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          last_valid_block_height: 393_666_166,
        });
      }, 2_000);
      const t4 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "confirming",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          slot: SHOWCASE_FINALIZED_SLOT,
        });
      }, 3_500);
      const t5 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "finalized",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          slot: SHOWCASE_FINALIZED_SLOT,
        });
        inFlightRef.current = false;
      }, 5_500);
      showcaseTimers.current.push(t1, t2, t3, t4, t5);
      return;
    }

    // ── 1. POST .../prepare ────────────────────────────────────────────
    let prepEnv;
    try {
      prepEnv = await prepareSolendSigning(sid, aid);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "prepare network error";
      inFlightRef.current = false;
      if (cancelledRef.current) return;
      setState({ kind: "prepare_failed", reason: msg });
      return;
    }

    if (cancelledRef.current) {
      inFlightRef.current = false;
      return;
    }

    if (prepEnv.kind === "error") {
      inFlightRef.current = false;
      const reason =
        prepEnv.error || `prepare returned HTTP ${prepEnv.httpStatus}`;
      // 503 → handler not wired (daemon not running JIT-aware build).
      // 400 → malformed ids (caller bug). Both surface as
      // prepare_failed with the daemon's error string.
      setState({ kind: "prepare_failed", reason });
      return;
    }

    const prep = prepEnv.response;
    if (prep.status !== "ready") {
      inFlightRef.current = false;
      setState({ kind: "prepare_failed", reason: prepareFailureReason(prep) });
      return;
    }

    signingRequestIdRef.current = prep.signing_request_id;

    // ── 2. GET .../solend-signatures/:r — fetch the unsigned tx ─────────
    let sigEnv;
    try {
      sigEnv = await getSolendSignature(sid, prep.signing_request_id);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "retrieve network error";
      inFlightRef.current = false;
      if (cancelledRef.current) return;
      setState({ kind: "error", error: msg });
      return;
    }

    if (cancelledRef.current) {
      inFlightRef.current = false;
      return;
    }

    if (sigEnv.kind === "error") {
      inFlightRef.current = false;
      if (sigEnv.httpStatus === 404) {
        setState({ kind: "not_found" });
      } else {
        setState({
          kind: "error",
          error: sigEnv.error || `retrieve returned HTTP ${sigEnv.httpStatus}`,
        });
      }
      return;
    }

    if (sigEnv.response.status !== "found") {
      inFlightRef.current = false;
      // The signing_request_id we just minted should always be
      // Found here. Anything else is an unexpected state — surface
      // it via the existing retrieval mapper.
      setState(mapRetrievalToState(sigEnv.response));
      return;
    }

    const unsignedTxB64 = sigEnv.response.unsigned_tx_b64;

    // ── 3. Phantom signTransaction (popup fires here) ───────────────────
    setState({ kind: "signing" });

    let signedB64: string;
    try {
      if (usePhantom) {
        signedB64 = await signSolendTransaction(unsignedTxB64);
      } else {
        inFlightRef.current = false;
        setState({
          kind: "error",
          error: "usePhantom disabled but no alternative signer wired",
        });
        return;
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : "signing failed";
      inFlightRef.current = false;
      setState({ kind: "rejected", error: `Phantom signing rejected: ${msg}` });
      return;
    }

    if (cancelledRef.current) {
      inFlightRef.current = false;
      return;
    }

    // ── 4. POST .../solend-signatures/:r — submit signed bytes ──────────
    setState({ kind: "submitting" });

    let submitEnv;
    try {
      submitEnv = await submitSolendSignature(
        sid,
        prep.signing_request_id,
        signedB64,
      );
    } catch (err) {
      const msg = err instanceof Error ? err.message : "submit network error";
      inFlightRef.current = false;
      setState({ kind: "error", error: msg });
      return;
    }

    if (cancelledRef.current) {
      inFlightRef.current = false;
      return;
    }

    if (submitEnv.kind === "error") {
      inFlightRef.current = false;
      if (submitEnv.httpStatus === 404) {
        setState({ kind: "not_found" });
      } else {
        setState({
          kind: "error",
          error:
            submitEnv.error || `submit returned HTTP ${submitEnv.httpStatus}`,
        });
      }
      return;
    }

    const submitNext = mapSubmitToState(submitEnv.response);
    setState(submitNext);

    // After submit, kick off post-submit polling for the
    // confirming → finalized lifecycle. The hook is no longer
    // "in flight" with respect to a NEW user gesture; the user
    // can now ignore the page and watch confirmation happen.
    inFlightRef.current = false;

    if (!isTerminal(submitNext.kind)) {
      // Kick off the post-submit polling loop. pollOnce self-
      // schedules until a terminal state is reached. We trust the
      // daemon's confirmation tracker to reach a terminal state
      // well under 5 minutes; if the loop runs longer the ref
      // guards still keep it bounded by component lifetime.
      pollTimerRef.current = setTimeout(() => {
        void pollOnce();
      }, POLL_INTERVAL_MS);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [usePhantom]);

  return { state, signWithPhantom, reset };
}

// ── Pure mapping helpers (no React deps) ────────────────────────────────────

function prepareFailureReason(
  result: Exclude<
    import("@/lib/types").SolendJitPrepareResult,
    { status: "ready" }
  >,
): string {
  switch (result.status) {
    case "not_approved":
      return `approval workflow is ${result.state}, not approved`;
    case "jit_ready_missing":
      return "no JIT-ready entry exists for this approval — re-propose to refresh";
    case "wallet_mismatch":
      return `bound wallet differs from approval-time wallet (expected ${result.expected.slice(0, 8)}…, bound ${result.bound ? result.bound.slice(0, 8) + "…" : "none"})`;
    case "handoff_create_failed":
      return `handoff creation failed: ${result.error_type} — ${result.message}`;
    case "not_found":
      return "approval not found or does not belong to this session";
  }
}

function mapRetrievalToState(
  result: SolendRetrievalResult,
): SigningHandoffState {
  switch (result.status) {
    case "found":
      return {
        kind: "ready",
        unsigned_tx_b64: result.unsigned_tx_b64,
        session_wallet: result.session_wallet,
        verified_slot: result.verified_slot,
        last_valid_block_height: result.last_valid_block_height,
      };
    case "submitted":
      return {
        kind: "submitted",
        tx_signature: result.tx_signature,
        last_valid_block_height: result.last_valid_block_height,
      };
    case "confirming":
      return {
        kind: "confirming",
        tx_signature: result.tx_signature,
        slot: result.slot,
      };
    case "finalized":
      return {
        kind: "finalized",
        tx_signature: result.tx_signature,
        slot: result.slot,
      };
    case "failed":
      return {
        kind: "execution_failed",
        err: result.err,
        tx_signature: result.tx_signature,
      };
    case "confirmation_timeout":
      return {
        kind: "confirmation_timeout",
        reason: result.reason,
        requires_reproposal: result.requires_reproposal,
        tx_signature: result.tx_signature,
      };
    case "rejected":
      return { kind: "rejected", error: `${result.error_type}: ${result.message}` };
    case "broadcast_failed":
      return {
        kind: "broadcast_failed",
        error: `${result.error_type}: ${result.message}`,
      };
    case "pre_submit_expired":
      return { kind: "expired", reason: result.reason };
    case "not_found":
      return { kind: "not_found" };
    case "expired":
      return { kind: "expired", reason: "signing TTL elapsed before submit" };
  }
}

function mapSubmitToState(
  result: SolendSubmitResult,
): SigningHandoffState {
  switch (result.status) {
    case "submitted":
      return {
        kind: "submitted",
        tx_signature: result.tx_signature,
        last_valid_block_height: result.last_valid_block_height,
      };
    case "recovered":
      return {
        kind: "submitted",
        tx_signature: result.tx_signature,
        last_valid_block_height: 0,
      };
    case "not_found":
      return { kind: "not_found" };
    case "expired":
      // The daemon's pre_submit_expired gate fired between sign and
      // submit. UI surfaces this as a retryable expired state — the
      // user clicks Sign again to call prepare with a fresh
      // blockhash. No re-approval is required.
      return { kind: "expired", reason: result.reason };
    case "rejected":
      return { kind: "rejected", error: `${result.error_type}: ${result.message}` };
    case "broadcast_failed":
      return {
        kind: "broadcast_failed",
        error: `${result.error_type}: ${result.message}`,
        tx_signature: undefined,
      };
  }
}

function isTerminal(kind: SigningHandoffState["kind"]): boolean {
  return (
    kind === "finalized" ||
    kind === "rejected" ||
    kind === "broadcast_failed" ||
    kind === "expired" ||
    kind === "not_found" ||
    kind === "execution_failed" ||
    kind === "confirmation_timeout" ||
    kind === "error" ||
    kind === "prepare_failed"
  );
}
