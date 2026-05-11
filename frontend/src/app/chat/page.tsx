"use client";

// Phase 6 Day 1 — `/chat` page skeleton.
//
// This is the user-facing entry point for the LLM-guided proposal flow
// proven mainnet on 2026-04-25 (Phase 5G). The page drives the strict
// one-turn `ConversationHandler` via `POST /sessions/:id/chat` and
// renders the resulting `ChatResponse` discriminated union.
//
// Day 1 scope: skeleton + state machine + happy path (assistant_text /
// tool_dispatched). Error variants render their typed banner but do not
// yet have polished UX. Phantom signing and live balance display are
// Day 2+.

import { useCallback, useEffect, useRef, useState } from "react";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { ToolResultCard } from "@/components/tool-cards";
import { WalletConnect } from "@/components/wallet-connect";
import {
  confirmWalletBindChallenge,
  createWalletBindChallenge,
  getOrCreateSession,
  postChat,
} from "@/lib/api";
import { IS_SHOWCASE, MODE } from "@/lib/mode";
import { signMessage } from "@/lib/phantom";
import type {
  ChatMessage,
  ChatResponse,
  ChatRouteResult,
  SessionId,
  W5dConditionalDepositResult,
} from "@/lib/types";

// Live-mode wallet bind state. The Phantom popup for signMessage is
// triggered ONLY inside `handleBindWallet` (a click handler), never from
// an effect — Phantom rejects popups that aren't rooted in a user
// gesture, and we want to uphold INV-1 even for the binding step.
type BindState =
  | { kind: "idle" }
  | { kind: "challenge_fetching" }
  | { kind: "awaiting_signature"; message: string }
  | { kind: "confirming" }
  | { kind: "bound"; pubkey: string }
  | { kind: "error"; reason: string };

// Backend caps the request body at 4096 bytes; the harness caps the
// message string at 4000 chars after trim. Mirror the char cap here so
// we don't burn a round-trip on a known-rejection.
const MAX_MESSAGE_CHARS = 4000;

export default function ChatPage() {
  const [sessionId, setSessionId] = useState<SessionId | null>(null);
  const [sessionError, setSessionError] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);

  // Live-mode wallet binding: tracked here (not in <WalletConnect>) so
  // the Send button can gate on it and the bind state is reset by
  // disconnect / accountChanged events surfaced by <WalletConnect>.
  const [walletPubkey, setWalletPubkey] = useState<string | null>(null);
  const [bind, setBind] = useState<BindState>({ kind: "idle" });

  const listEndRef = useRef<HTMLDivElement | null>(null);

  // Open a session on mount. Caches in `lib/api.ts` so refresh is fine.
  useEffect(() => {
    let cancelled = false;
    getOrCreateSession()
      .then((id) => {
        if (!cancelled) setSessionId(id);
      })
      .catch((err) => {
        if (!cancelled) {
          setSessionError(
            err instanceof Error ? err.message : "Failed to open session",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Auto-scroll to bottom on new message.
  useEffect(() => {
    listEndRef.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [messages]);

  // If the connected wallet changes (or disconnects), invalidate any
  // existing bind. Daemon would also reject a stale binding because the
  // session+pubkey check on /wallet-bind-confirm is exact-match.
  useEffect(() => {
    setBind((prev) => {
      if (prev.kind === "bound" && prev.pubkey !== walletPubkey) {
        return { kind: "idle" };
      }
      return prev;
    });
  }, [walletPubkey]);

  const handleBindWallet = useCallback(async () => {
    if (IS_SHOWCASE) return;
    if (!sessionId || !walletPubkey) return;
    setBind({ kind: "challenge_fetching" });
    try {
      const challenge = await createWalletBindChallenge(sessionId, walletPubkey);
      setBind({ kind: "awaiting_signature", message: challenge.message });
      const { signatureB64 } = await signMessage(challenge.message);
      setBind({ kind: "confirming" });
      const result = await confirmWalletBindChallenge(
        sessionId,
        challenge.challenge_id,
        walletPubkey,
        signatureB64,
      );
      if (result.bound && result.verified) {
        setBind({ kind: "bound", pubkey: walletPubkey });
      } else {
        setBind({
          kind: "error",
          reason: "daemon returned bound=false or verified=false",
        });
      }
    } catch (err) {
      const reason = err instanceof Error ? err.message : "binding failed";
      setBind({ kind: "error", reason });
    }
  }, [sessionId, walletPubkey]);

  const trimmed = input.trim();
  // In live mode, sending is gated on a verified wallet bind. Solend
  // tool dispatch will fail without it (SessionBoundWallet has no
  // pubkey for this session), so blocking up-front is more honest than
  // letting the chat error out at tool-resolve time.
  const liveBindBlocked =
    !IS_SHOWCASE && (walletPubkey === null || bind.kind !== "bound");
  const canSend =
    !!sessionId &&
    !sending &&
    trimmed.length > 0 &&
    trimmed.length <= MAX_MESSAGE_CHARS &&
    !liveBindBlocked;

  async function send() {
    if (!canSend || !sessionId) return;
    const text = trimmed;
    const now = new Date().toISOString();

    const userMessage: ChatMessage = {
      id: `user-${now}-${Math.random().toString(36).slice(2, 8)}`,
      kind: "user",
      text,
      sentAt: now,
    };
    setMessages((prev) => [...prev, userMessage]);
    setInput("");
    setSending(true);

    try {
      const result = await postChat(sessionId, text);
      const replyMessage: ChatMessage = {
        id: `assistant-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        kind: "assistant",
        result,
        receivedAt: new Date().toISOString(),
      };
      setMessages((prev) => [...prev, replyMessage]);
    } catch (err) {
      const errText =
        err instanceof Error ? err.message : "Network or parse error";
      const sysMessage: ChatMessage = {
        id: `sys-${Date.now()}`,
        kind: "system",
        text: `Request failed: ${errText}`,
        at: new Date().toISOString(),
      };
      setMessages((prev) => [...prev, sysMessage]);
    } finally {
      setSending(false);
    }
  }

  function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    void send();
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    // Ctrl/Cmd+Enter sends; plain Enter inserts newline.
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      void send();
    }
  }

  return (
    <div className="space-y-6 max-w-3xl mx-auto">
      <header className="space-y-1">
        <div className="flex items-center gap-3 flex-wrap">
          <h1 className="text-2xl font-semibold tracking-tight">Chat</h1>
          <Badge variant={MODE === "live" ? "default" : "secondary"} className="uppercase">
            {MODE}
          </Badge>
          <div className="ml-auto">
            <WalletConnect onChange={setWalletPubkey} />
          </div>
        </div>
        <p className="text-sm text-muted-foreground">
          Natural-language proposal entry. The assistant proposes only — approval and wallet
          signing remain human-controlled at every step.
        </p>
      </header>

      <SessionStatus sessionId={sessionId} sessionError={sessionError} />

      {!IS_SHOWCASE && (
        <WalletBindStatus
          sessionId={sessionId}
          walletPubkey={walletPubkey}
          bind={bind}
          onBindClick={handleBindWallet}
        />
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Conversation</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {messages.length === 0 && <EmptyState />}
          <ul className="space-y-3">
            {messages.map((m) => (
              <li key={m.id}>
                {m.kind === "user" && <UserBubble text={m.text} />}
                {m.kind === "assistant" && <AssistantBubble result={m.result} />}
                {m.kind === "system" && <SystemNotice text={m.text} />}
              </li>
            ))}
          </ul>
          <div ref={listEndRef} />
        </CardContent>
      </Card>

      <form onSubmit={onSubmit} className="space-y-2">
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={onKeyDown}
          rows={3}
          maxLength={MAX_MESSAGE_CHARS}
          placeholder={
            !sessionId
              ? "Opening session…"
              : liveBindBlocked
                ? walletPubkey === null
                  ? "Connect Phantom and bind your wallet first"
                  : "Bind your wallet to the session before sending"
                : "Type a natural-language DeFi request"
          }
          disabled={!sessionId || liveBindBlocked}
          className="w-full rounded-md border bg-background px-3 py-2 text-sm font-sans
                     focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-60"
        />
        <div className="flex items-center justify-between">
          <span className="text-xs text-muted-foreground">
            {trimmed.length} / {MAX_MESSAGE_CHARS} chars · ⌘/Ctrl + Enter to send
          </span>
          <Button type="submit" disabled={!canSend}>
            {sending ? "Sending…" : "Send"}
          </Button>
        </div>
      </form>
    </div>
  );
}

// ── Sub-components ──────────────────────────────────────────────────────────

function WalletBindStatus({
  sessionId,
  walletPubkey,
  bind,
  onBindClick,
}: {
  sessionId: SessionId | null;
  walletPubkey: string | null;
  bind: BindState;
  onBindClick: () => void;
}) {
  // Bound (and matches the currently-connected wallet): minimal green confirmation.
  if (bind.kind === "bound" && bind.pubkey === walletPubkey) {
    return (
      <div
        className="rounded-md border bg-card px-3 py-2 text-xs flex items-center gap-2"
        data-testid="wallet-bind-bound"
      >
        <span className="inline-block h-2 w-2 rounded-full bg-green-600" />
        <span className="text-foreground">Wallet bound to session</span>
        <span className="text-muted-foreground font-mono">
          ({walletPubkey?.slice(0, 4)}…{walletPubkey?.slice(-4)})
        </span>
      </div>
    );
  }

  // No wallet yet: muted nudge.
  if (walletPubkey === null) {
    return (
      <Alert className="border-amber-500/40">
        <AlertTitle>Connect Phantom to bind your wallet</AlertTitle>
        <AlertDescription>
          Live mode requires the wallet to prove ownership of the session before any
          Solend tool can resolve a signer. Connect Phantom in the header to begin.
        </AlertDescription>
      </Alert>
    );
  }

  // Wallet connected but not yet bound: explicit user action required.
  if (bind.kind === "idle") {
    return (
      <Alert className="border-amber-500/40">
        <AlertTitle>Wallet not bound to session</AlertTitle>
        <AlertDescription className="space-y-2">
          <span className="block">
            Phantom is connected but the daemon hasn&apos;t verified ownership for this
            session. Click below to request a challenge — you&apos;ll be asked to sign
            a short message in Phantom (no transaction).
          </span>
          <Button
            size="sm"
            onClick={onBindClick}
            disabled={!sessionId}
            data-testid="wallet-bind-start"
          >
            Bind wallet to session
          </Button>
        </AlertDescription>
      </Alert>
    );
  }

  if (bind.kind === "challenge_fetching") {
    return (
      <Alert>
        <AlertTitle>Requesting challenge…</AlertTitle>
        <AlertDescription>
          Asking the daemon for a one-time message to sign.
        </AlertDescription>
      </Alert>
    );
  }

  if (bind.kind === "awaiting_signature") {
    return (
      <Alert>
        <AlertTitle>Approve the message in Phantom</AlertTitle>
        <AlertDescription className="space-y-2">
          <span className="block">
            Phantom should be prompting you to sign a short ownership-proof message.
            No transaction is being signed — this is text only.
          </span>
          <details className="text-xs text-muted-foreground">
            <summary className="cursor-pointer hover:text-foreground">
              show challenge text
            </summary>
            <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug whitespace-pre-wrap">
              {bind.message}
            </pre>
          </details>
        </AlertDescription>
      </Alert>
    );
  }

  if (bind.kind === "confirming") {
    return (
      <Alert>
        <AlertTitle>Verifying signature…</AlertTitle>
        <AlertDescription>
          Sending the signed challenge back to the daemon for verification.
        </AlertDescription>
      </Alert>
    );
  }

  if (bind.kind === "error") {
    return (
      <Alert variant="destructive" data-testid="wallet-bind-error">
        <AlertTitle>Wallet bind failed</AlertTitle>
        <AlertDescription className="space-y-2">
          <span className="block break-all">{bind.reason}</span>
          <Button
            size="sm"
            variant="outline"
            onClick={onBindClick}
            disabled={!sessionId || !walletPubkey}
          >
            Retry bind
          </Button>
        </AlertDescription>
      </Alert>
    );
  }

  // bind.kind === "bound" but bind.pubkey doesn't match walletPubkey —
  // transient state between an `accountChanged` from <WalletConnect>
  // and the page-level effect that resets bind to idle. Render nothing
  // for that single tick instead of a confusing stale-bound chip.
  return null;
}

function SessionStatus({
  sessionId,
  sessionError,
}: {
  sessionId: SessionId | null;
  sessionError: string | null;
}) {
  if (sessionError) {
    return (
      <Alert>
        <AlertTitle>Session unavailable</AlertTitle>
        <AlertDescription>{sessionError}</AlertDescription>
      </Alert>
    );
  }
  if (!sessionId) {
    return (
      <Alert>
        <AlertTitle>Opening session…</AlertTitle>
        <AlertDescription>Connecting to the gateway.</AlertDescription>
      </Alert>
    );
  }
  return (
    <div className="text-xs text-muted-foreground flex items-center gap-2">
      <span>Session</span>
      <code className="rounded bg-muted px-1.5 py-0.5">{sessionId.slice(0, 8)}…</code>
      {IS_SHOWCASE && (
        <Badge variant="secondary" className="text-[10px]">
          fixture
        </Badge>
      )}
    </div>
  );
}

function EmptyState() {
  return (
    <div className="rounded-md border border-dashed bg-muted/30 px-4 py-6">
      <p className="text-sm text-muted-foreground">
        Type a natural-language DeFi request below. The assistant proposes only;
        approval and wallet signing stay human-controlled.
      </p>
    </div>
  );
}

function UserBubble({ text }: { text: string }) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[85%] rounded-2xl rounded-br-sm bg-primary text-primary-foreground px-4 py-2 text-sm whitespace-pre-wrap">
        {text}
      </div>
    </div>
  );
}

function AssistantBubble({ result }: { result: ChatRouteResult }) {
  // ── HTTP-envelope-level branches first ───────────────────────────────
  if (result.kind === "disabled") {
    return (
      <Alert>
        <AlertTitle>Chat is not enabled in this daemon (503)</AlertTitle>
        <AlertDescription>
          The gateway is running, but no LLM provider is configured.
          Set <code>CLAW_CHAT_PROVIDER=openai</code> + <code>OPENAI_API_KEY</code> and restart.
          <span className="block mt-1 text-xs text-muted-foreground">{result.error}</span>
        </AlertDescription>
      </Alert>
    );
  }
  if (result.kind === "not_found") {
    return (
      <Alert>
        <AlertTitle>Session not found (404)</AlertTitle>
        <AlertDescription>{result.error || "Session may have been swept."}</AlertDescription>
      </Alert>
    );
  }
  if (result.kind === "bad_request") {
    return (
      <Alert>
        <AlertTitle>Bad request (400)</AlertTitle>
        <AlertDescription>{result.error}</AlertDescription>
      </Alert>
    );
  }
  if (result.kind === "unexpected") {
    return (
      <Alert>
        <AlertTitle>Unexpected response ({result.httpStatus})</AlertTitle>
        <AlertDescription>{result.error || "No body"}</AlertDescription>
      </Alert>
    );
  }
  if (result.kind === "conflict") {
    return <PendingActionCard reason={result.response.reason} />;
  }

  // ── 200 OK domain variants ───────────────────────────────────────────
  return <ChatResponseCard response={result.response} />;
}

function ChatResponseCard({ response }: { response: ChatResponse }) {
  switch (response.status) {
    case "assistant_text":
      return (
        <div className="flex justify-start">
          <div className="max-w-[85%] rounded-2xl rounded-bl-sm bg-muted px-4 py-2 text-sm whitespace-pre-wrap">
            {response.assistant_text ?? <em className="text-muted-foreground">(no text)</em>}
          </div>
        </div>
      );

    case "tool_dispatched":
      return <ToolResultCard toolName={response.tool_name} output={response.output} />;

    case "multiple_tool_calls_rejected":
      return (
        <Alert>
          <AlertTitle>Multiple tool calls rejected</AlertTitle>
          <AlertDescription>
            The model requested {response.count} tools in one turn. The control plane only
            allows one tool call per turn.
          </AlertDescription>
        </Alert>
      );

    case "unknown_or_denied_tool":
      return (
        <Alert>
          <AlertTitle>Tool not in narrowed registry</AlertTitle>
          <AlertDescription>
            <code>{response.tool_name}</code> — {response.reason}
          </AlertDescription>
        </Alert>
      );

    case "malformed_tool_arguments":
      return (
        <Alert>
          <AlertTitle>Malformed tool arguments</AlertTitle>
          <AlertDescription>
            <code>{response.tool_name}</code> — {response.reason}
          </AlertDescription>
        </Alert>
      );

    case "malformed_provider_output":
      return (
        <Alert>
          <AlertTitle>Malformed provider output</AlertTitle>
          <AlertDescription>{response.reason}</AlertDescription>
        </Alert>
      );

    case "tool_error":
      return (
        <Alert>
          <AlertTitle>Tool refused proposal</AlertTitle>
          <AlertDescription>
            <code>{response.tool_name}</code> — {response.message}
          </AlertDescription>
        </Alert>
      );

    case "pending_action_exists":
      // Backend usually surfaces this as 409, but defense-in-depth: render
      // the same card if it ever shows up under 200.
      return <PendingActionCard reason={response.reason} />;

    case "w5d_conditional_deposit":
      // W5d demo-bridge: deterministic-parser + B-O1 on-chain APR.
      // Render a typed card — never a raw JSON blob.
      return <W5dConditionalDepositCard result={response.result} />;

    default: {
      // Exhaustiveness check — `never` assertion fails to compile if
      // the ChatResponse union grows a new variant without a case
      // above. Returning the never-typed value keeps eslint happy.
      const exhaustive: never = response;
      return exhaustive;
    }
  }
}

/// Format a basis-point integer as a percent label (e.g. 163 → "1.63%").
function bpsToPctLabel(bps: number): string {
  const whole = Math.floor(bps / 100);
  const frac = Math.abs(bps % 100);
  return `${whole}.${frac.toString().padStart(2, "0")}%`;
}

function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  const onClick = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      // navigator.clipboard can throw in non-secure contexts; swallow
      // silently — the value is still selectable in the DOM.
    }
  }, [value]);
  return (
    <button
      type="button"
      aria-label={`Copy ${label}`}
      onClick={onClick}
      className="ml-2 inline-flex shrink-0 items-center rounded border px-1.5 py-0.5 text-[10px] hover:bg-muted"
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

function W5dConditionalDepositCard({
  result,
}: {
  result: W5dConditionalDepositResult;
}) {
  const conditionLabel = result.condition_met ? "true" : "false";
  // W5e status enum: watching | ready_to_execute | needs_funding.
  // Each gets its own banner copy + tone exactly as specified by the
  // W5e brief.
  let banner: { tone: string; text: string };
  switch (result.status) {
    case "ready_to_execute":
      banner = {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Ready to execute — live send is not authorised from chat.",
      };
      break;
    case "needs_funding":
      banner = {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Needs funding — send 0.25 USDC to the controlled wallet before this conditional order can execute.",
      };
      break;
    case "watching":
    default:
      banner = {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Watching — budget reserved until condition is met, expired, or cancelled.",
      };
      break;
  }
  // Required-budget label: raw 250_000 → "0.25 USDC". Always 6 decimals
  // for USDC; rendered as a fixed-precision string.
  const requiredUsdc = (result.required_budget_raw / 1_000_000).toFixed(2);
  const currentUsdc = (result.current_budget_raw / 1_000_000).toFixed(6);

  // W5f degraded-path detector: if the gateway didn't wire a Save
  // fetcher, the orchestrator falls back to native APR for the
  // decision and `save_display_apy_bps == native_onchain_apr_bps`.
  const decisionMetricsEqual =
    result.save_display_apy_bps === result.native_onchain_apr_bps;
  const degradedW5fPath =
    decisionMetricsEqual && result.decision_source === "save_display_apy";

  return (
    <div className="flex justify-start">
      <div
        data-testid="w5d-conditional-deposit-card"
        className="max-w-[85%] rounded-2xl rounded-bl-sm bg-card border px-4 py-3 text-sm space-y-2"
      >
        <div className="font-medium">W5f conditional order (Save display APY)</div>
        <div className="text-xs text-muted-foreground italic break-words">
          &ldquo;{result.input_text}&rdquo;
        </div>
        <div
          data-testid="w5e-status-banner"
          data-status={result.status}
          className={`mt-2 inline-block rounded border px-2 py-1 text-xs ${banner.tone}`}
        >
          {banner.text}
        </div>

        {/* ── Primary: Save display APY drives the decision ─────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">
            Primary — Save display APY
          </div>
          <dl
            className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs"
            data-testid="w5f-primary-block"
          >
            <dt className="text-muted-foreground">Save display APY</dt>
            <dd data-testid="w5f-save-display-apy">
              {result.save_display_apy_bps} bps (
              {bpsToPctLabel(result.save_display_apy_bps)})
            </dd>

            <dt className="text-muted-foreground">threshold</dt>
            <dd>
              {result.threshold_bps} bps ({bpsToPctLabel(result.threshold_bps)})
            </dd>

            <dt className="text-muted-foreground">decision source</dt>
            <dd className="font-mono" data-testid="w5f-decision-source">
              {result.decision_source}
            </dd>

            <dt className="text-muted-foreground">condition_met</dt>
            <dd className="font-mono">{conditionLabel}</dd>
          </dl>
        </div>

        {/* ── Audit: native B-O1 on-chain APR (secondary) ───────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">
            Audit — Native on-chain APR
          </div>
          <dl
            className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs"
            data-testid="w5f-audit-block"
          >
            <dt className="text-muted-foreground">native APR</dt>
            <dd data-testid="w5f-native-onchain-apr">
              {result.native_onchain_apr_bps} bps (
              {bpsToPctLabel(result.native_onchain_apr_bps)})
            </dd>

            <dt className="text-muted-foreground">native source</dt>
            <dd className="font-mono" data-testid="w5f-native-source">
              {result.native_onchain_apr_source}
            </dd>

            <dt className="text-muted-foreground">reserve pubkey</dt>
            <dd className="font-mono break-all">{result.reserve_pubkey}</dd>
          </dl>
        </div>

        {/* ── Budget block ─────────────────────────────────────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">Budget</div>
          <dl
            className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs"
            data-testid="w5f-budget-block"
          >
            <dt className="text-muted-foreground">budget status</dt>
            <dd className="font-mono">{result.budget_status}</dd>

            <dt className="text-muted-foreground">required budget</dt>
            <dd>
              {result.required_budget_raw} raw ({requiredUsdc} USDC)
            </dd>

            <dt className="text-muted-foreground">current budget</dt>
            <dd>
              {result.current_budget_raw} raw ({currentUsdc} USDC)
            </dd>

            <dt className="text-muted-foreground">controlled wallet</dt>
            <dd className="font-mono break-all flex items-start">
              <span className="break-all" data-testid="w5e-controlled-wallet">
                {result.controlled_wallet}
              </span>
              <CopyButton
                value={result.controlled_wallet}
                label="controlled wallet"
              />
            </dd>

            <dt className="text-muted-foreground">source USDC ATA</dt>
            <dd className="font-mono break-all flex items-start">
              <span className="break-all" data-testid="w5e-source-usdc-ata">
                {result.source_usdc_ata}
              </span>
              <CopyButton
                value={result.source_usdc_ata}
                label="source USDC ATA"
              />
            </dd>
          </dl>
        </div>

        {/* ── Rule identity + liveness ─────────────────────────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">Rule</div>
          <dl
            className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs"
            data-testid="w5f-rule-block"
          >
            <dt className="text-muted-foreground">last_checked_slot</dt>
            <dd className="font-mono" data-testid="w5e-last-checked-slot">
              {result.last_checked_slot}
            </dd>

            <dt className="text-muted-foreground">expires_at_slot</dt>
            <dd className="font-mono" data-testid="w5e-expires-at-slot">
              {result.expires_at_slot ?? "N/A"}
            </dd>

            <dt className="text-muted-foreground">rule_id</dt>
            <dd className="font-mono break-all" data-testid="w5e-rule-id">
              {result.rule_id_hex ?? "N/A"}
            </dd>

            <dt className="text-muted-foreground">canonical_rule_hash</dt>
            <dd className="font-mono break-all" data-testid="w5e-canonical-hash">
              {result.canonical_rule_hash_hex ?? "N/A"}
            </dd>

            <dt className="text-muted-foreground">rule_persisted</dt>
            <dd className="font-mono" data-testid="w5e-rule-persisted">
              {result.rule_persisted ? "true" : "false (preview-only)"}
            </dd>

            <dt className="text-muted-foreground">execution_attempted</dt>
            <dd className="font-mono">
              {result.execution_attempted ? "true" : "false"}
            </dd>

            <dt className="text-muted-foreground">tx_signature</dt>
            <dd className="font-mono break-all">{result.tx_signature ?? "N/A"}</dd>
          </dl>
        </div>

        {/* ── No-overclaim footer ─────────────────────────────── */}
        <div className="mt-2 text-[10px] text-muted-foreground leading-snug space-y-1">
          <p>
            This card follows <b>Save display APY</b> for the user-facing
            condition. Native B-O1 APR is shown as an audit field. The
            persisted WatchRule&apos;s on-chain condition still
            evaluates against native APR when the W2 watcher ticks — the
            two metrics may diverge.
          </p>
          {degradedW5fPath && (
            <p
              className="text-amber-700"
              data-testid="w5f-degraded-banner"
            >
              Note: the gateway is running in W5e degraded mode (no Save
              REST fetcher wired); the metrics shown are identical
              because both come from the on-chain B-O1 evaluator.
            </p>
          )}
          <p>
            Demo bridge: deterministic parser → Save REST API APY +
            B-O1 on-chain APR → Stage 2 WatchRule persistence. NOT a
            first-class production SolendDeposit ActionSpec; the action
            carrier is <code>SolendWithdrawAllDelegated</code> and live
            execution is gated outside this chat surface.
          </p>
        </div>
      </div>
    </div>
  );
}

function PendingActionCard({ reason }: { reason: string }) {
  return (
    <Alert>
      <AlertTitle>A prior proposal is awaiting approval (409)</AlertTitle>
      <AlertDescription>
        {reason}
        <span className="block mt-2 text-xs">
          Resolve the pending approval before sending another proposal.
        </span>
      </AlertDescription>
    </Alert>
  );
}

function SystemNotice({ text }: { text: string }) {
  return (
    <div className="text-center text-xs text-muted-foreground italic">{text}</div>
  );
}
