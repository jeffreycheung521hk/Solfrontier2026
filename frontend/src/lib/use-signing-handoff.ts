// Phase 6 Day 2 — Signing handoff state machine.
//
// Polls `GET /sessions/:id/solend-signatures/:request_id`, exposes a
// `signWithPhantom()` action to drive the legacy `Transaction` signing
// path, and continues polling until a terminal lifecycle state is
// observed (`finalized`, `rejected`, `broadcast_failed`, `expired`,
// `confirmation_timeout`, `not_found`, or `execution_failed`).
//
// Strict rules upheld here (mirror Phase 4C / 5G safety posture):
//
//  - **No auto-popup.** The Phantom popup only fires inside
//    `signWithPhantom`, which the UI binds to a button click. The
//    polling loop never calls into Phantom on its own.
//  - **No auto-retry.** A terminal failure halts the loop. The UI may
//    expose a "reset" button, but the hook never re-broadcasts.
//  - **Daemon broadcasts.** This hook never calls
//    `signAndSendTransaction`; it calls Phantom's `signTransaction`,
//    base64-encodes the signed bytes, and POSTs to the daemon.
//
// Showcase mode runs a deterministic simulated lifecycle (timers, no
// network) using Phase 5G's real on-chain hash for the `finalized`
// state, so the UI's Solscan link points at a real Finalized tx
// during demo / recording.

import { useCallback, useEffect, useRef, useState } from "react";

import { IS_SHOWCASE } from "@/lib/mode";
import { signSolendTransaction } from "@/lib/phantom";
import {
  getSolendSignature,
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
const SHOWCASE_SESSION_WALLET = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

// Polling cadence and bounds for live mode.
const POLL_INTERVAL_MS = 1_500;
const MAX_POLL_ATTEMPTS_BEFORE_GIVE_UP = 200; // ~5 minutes

export interface UseSigningHandoffOptions {
  /** When false, the hook never calls Phantom even if `signWithPhantom`
   *  is invoked — used by showcase mode so judges don't need Phantom
   *  installed. */
  usePhantom?: boolean;
}

export interface UseSigningHandoffResult {
  state: SigningHandoffState;
  /** User-initiated. Idempotent while signing/submitting; no-op
   *  unless current state is `ready`. Phantom popup fires inside. */
  signWithPhantom: () => Promise<void>;
  /** Reset to `idle` and stop the polling loop. The caller is
   *  expected to discard the hook's `signing_request_id` afterwards
   *  if they want to start fresh; just calling reset() leaves the
   *  hook ready to re-engage if state inputs change. */
  reset: () => void;
}

export function useSigningHandoff(
  sessionId: SessionId | null,
  signingRequestId: Uuid | null,
  options?: UseSigningHandoffOptions,
): UseSigningHandoffResult {
  const usePhantom = options?.usePhantom ?? !IS_SHOWCASE;

  const [state, setState] = useState<SigningHandoffState>({ kind: "idle" });

  // Refs the polling loop reads to know whether it should keep going.
  // We keep the ID inputs in refs so a reset doesn't cause an effect
  // re-run loop.
  const sessionIdRef = useRef<SessionId | null>(sessionId);
  const signingIdRef = useRef<Uuid | null>(signingRequestId);
  sessionIdRef.current = sessionId;
  signingIdRef.current = signingRequestId;

  // The polling timer + a boolean to stop the loop on terminal states.
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelledRef = useRef(false);

  function clearPollTimer() {
    if (pollTimerRef.current !== null) {
      clearTimeout(pollTimerRef.current);
      pollTimerRef.current = null;
    }
  }

  const reset = useCallback(() => {
    clearPollTimer();
    cancelledRef.current = true;
    setState({ kind: "idle" });
  }, []);

  // ── Showcase mode: deterministic simulated lifecycle ──────────────────
  //
  // Drives idle → polling → ready → (signing → submitting →) submitted
  // → confirming → finalized using setTimeout chains. No network. No
  // Phantom call. Lets the demo run on a clean machine without daemon
  // or wallet state.

  const showcaseTimers = useRef<ReturnType<typeof setTimeout>[]>([]);
  function clearShowcaseTimers() {
    showcaseTimers.current.forEach((t) => clearTimeout(t));
    showcaseTimers.current = [];
  }

  // ── Live polling loop ─────────────────────────────────────────────────

  const pollOnce = useCallback(async (): Promise<void> => {
    const sid = sessionIdRef.current;
    const rid = signingIdRef.current;
    if (!sid || !rid) return;
    let attempts = 0;
    setState((prev) => {
      attempts = prev.kind === "polling" ? prev.attempts + 1 : 1;
      return { kind: "polling", attempts };
    });

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
      // 404 (session not active) / 503 / etc — terminal at the
      // envelope layer. Map to typed local states.
      if (envelope.httpStatus === 404) {
        setState({ kind: "not_found" });
      } else {
        setState({
          kind: "error",
          error:
            envelope.error ||
            `gateway returned HTTP ${envelope.httpStatus}`,
        });
      }
      return;
    }

    const next = mapRetrievalToState(envelope.response);
    setState(next);

    // Continue polling unless terminal.
    if (isTerminal(next.kind)) return;

    if (attempts >= MAX_POLL_ATTEMPTS_BEFORE_GIVE_UP) {
      setState({
        kind: "error",
        error: `polling gave up after ${attempts} attempts (~${
          (attempts * POLL_INTERVAL_MS) / 1000
        }s)`,
      });
      return;
    }

    pollTimerRef.current = setTimeout(() => {
      void pollOnce();
    }, POLL_INTERVAL_MS);
  }, []);

  // ── Effect: start / stop the appropriate driver ───────────────────────

  useEffect(() => {
    cancelledRef.current = false;
    if (!sessionId || !signingRequestId) {
      clearPollTimer();
      clearShowcaseTimers();
      setState({ kind: "idle" });
      return;
    }

    if (IS_SHOWCASE) {
      // Showcase: simulate "polling → ready" with a short delay.
      setState({ kind: "polling", attempts: 1 });
      const t = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "ready",
          unsigned_tx_b64: "AQAAAAAA",
          session_wallet: SHOWCASE_SESSION_WALLET,
          verified_slot: 415_571_900,
          last_valid_block_height: 393666166,
        });
      }, 1_000);
      showcaseTimers.current.push(t);
    } else {
      // Live: drive the polling loop.
      void pollOnce();
    }

    return () => {
      cancelledRef.current = true;
      clearPollTimer();
      clearShowcaseTimers();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, signingRequestId]);

  // ── User action: signWithPhantom ──────────────────────────────────────

  const signWithPhantom = useCallback(async (): Promise<void> => {
    // Defensive: only act when ready.
    let unsigned: string | null = null;
    setState((prev) => {
      if (prev.kind === "ready") {
        unsigned = prev.unsigned_tx_b64;
      }
      return prev;
    });
    if (!unsigned) return;

    const sid = sessionIdRef.current;
    const rid = signingIdRef.current;
    if (!sid || !rid) return;

    setState({ kind: "signing" });

    if (IS_SHOWCASE) {
      // Showcase: simulate the full sign → submit → confirm → finalize
      // chain with timers. Skip Phantom (no real signing — judges may
      // not have Phantom installed). Use Phase 5G real tx hash so the
      // Solscan link in the `finalized` UI points at a real Finalized
      // transaction.
      const t1 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({ kind: "submitting" });
      }, 600);
      const t2 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "submitted",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          last_valid_block_height: 393666166,
        });
      }, 1_400);
      const t3 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "confirming",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          slot: SHOWCASE_FINALIZED_SLOT,
        });
      }, 3_000);
      const t4 = setTimeout(() => {
        if (cancelledRef.current) return;
        setState({
          kind: "finalized",
          tx_signature: SHOWCASE_FINALIZED_TX_SIGNATURE,
          slot: SHOWCASE_FINALIZED_SLOT,
        });
      }, 5_000);
      showcaseTimers.current.push(t1, t2, t3, t4);
      return;
    }

    // Live mode — call Phantom (or a mock if usePhantom = false).
    let signedB64: string;
    try {
      if (usePhantom) {
        signedB64 = await signSolendTransaction(unsigned);
      } else {
        // Caller opted out of real Phantom — they must provide their
        // own signed bytes. We don't synthesise here; surface an error.
        setState({
          kind: "error",
          error: "usePhantom disabled but no alternative signer wired",
        });
        return;
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : "signing failed";
      setState({ kind: "rejected", error: `Phantom signing rejected: ${msg}` });
      return;
    }

    if (cancelledRef.current) return;

    setState({ kind: "submitting" });

    let submitEnv;
    try {
      submitEnv = await submitSolendSignature(sid, rid, signedB64);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "submit network error";
      setState({ kind: "error", error: msg });
      return;
    }

    if (cancelledRef.current) return;

    if (submitEnv.kind === "error") {
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

    // After submit, resume polling for confirming → finalized unless
    // submit returned a terminal failure.
    if (!isTerminal(submitNext.kind)) {
      pollTimerRef.current = setTimeout(() => {
        void pollOnce();
      }, POLL_INTERVAL_MS);
    }
  }, [pollOnce, usePhantom]);

  return { state, signWithPhantom, reset };
}

// ── Pure mapping helpers (no React deps) ────────────────────────────────────

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
      // Idempotent replay — UI shows the original signature; treat as
      // submitted so the polling loop will pick up confirming /
      // finalized states for the original tx.
      return {
        kind: "submitted",
        tx_signature: result.tx_signature,
        last_valid_block_height: 0,
      };
    case "not_found":
      return { kind: "not_found" };
    case "expired":
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
    kind === "error"
  );
}
