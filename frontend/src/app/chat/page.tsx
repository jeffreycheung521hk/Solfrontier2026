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

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Connection, PublicKey } from "@solana/web3.js";
import type { Transaction } from "@solana/web3.js";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { ToolResultCard } from "@/components/tool-cards";
import { WalletConnect } from "@/components/wallet-connect";
import {
  confirmW5hFunding,
  confirmWalletBindChallenge,
  createWalletBindChallenge,
  getOrCreateSession,
  getW5hOrderStatus,
  postChat,
} from "@/lib/api";
import { IS_SHOWCASE, MODE } from "@/lib/mode";
import { getPhantomProvider, signMessage } from "@/lib/phantom";
import {
  CONTROLLED_WALLET_BASE58,
  MAX_POLL_ATTEMPTS_DEFAULT,
  POLL_INTERVAL_MS_DEFAULT,
  SignatureStatusNetworkError,
  USDC_MINT_BASE58,
  buildW5hFundingTransaction,
  buildW5hMemoText,
  deriveAtaPubkey,
  pollSignatureStatus,
  solscanTxUrl,
} from "@/lib/stage2-funding";
import type {
  ChatMessage,
  ChatResponse,
  ChatRouteResult,
  SessionId,
  W5dConditionalDepositResult,
  W5gConditionalExecutionResult,
  W5hConditionalDepositResult,
} from "@/lib/types";

/// Public RPC endpoint. Operator can override to a Helius / Triton
/// URL via `NEXT_PUBLIC_SOLANA_RPC_URL`. Any value set here is
/// browser-visible so DO NOT include an API key the operator wouldn't
/// already accept exposing.
const SOLANA_RPC_URL =
  process.env.NEXT_PUBLIC_SOLANA_RPC_URL ??
  "https://api.mainnet-beta.solana.com";

/// Approval phrase the chat-route requires inside the user's W5g
/// execute command. Hard-coded copy — the frontend never inspects it
/// semantically, it's only embedded in the suggested command string.
const W5G_APPROVAL_PHRASE = "W5G LIVE CHAT CONDITIONAL DEPOSIT APPROVED";

/// Build the exact chat command the operator sends to trigger the
/// W5g executor. Single source of truth, used by both the copy panel
/// inside the W5f ready-to-execute card AND the safe-error detector
/// that recognises the user's submission.
///
/// Agent D's `ChatExecuteRequestDto.canonical_rule_hash_hex` is
/// REQUIRED (it's the rule-identity anchor — prevents re-applying an
/// execution against a replaced rule), so the command always includes
/// both hex tokens. If the W5f card somehow lacks the canonical hash
/// the panel is suppressed upstream.
function buildW5gExecuteCommand(
  ruleIdHex: string,
  canonicalRuleHashHex: string,
): string {
  return `Execute W5g conditional deposit ${ruleIdHex} ${canonicalRuleHashHex} with approval phrase ${W5G_APPROVAL_PHRASE}`;
}

/// Loose prefix check used by the network-error catch path. Matches
/// any string that opens with "Execute W5g conditional deposit" so we
/// detect the operator's intent without parsing the full grammar.
function looksLikeW5gExecuteCommand(text: string): boolean {
  return /^Execute W5g conditional deposit\s+\S/.test(text.trim());
}

/// Loose detector for the W5h chat command in either English or 繁中.
/// Used by the network-error catch path so a thrown `postChat` on a
/// W5h prompt renders a typed safe-error card (not a generic system
/// notice). The card preserves the user's chat text — they don't have
/// to re-type a 50-character bilingual conditional order.
function looksLikeW5hChatCommand(text: string): boolean {
  const t = text.trim();
  // English — accept BOTH the simplified W5h-lite grammar
  //   "If Save APY > X%, deposit 0.25 USDC"
  // and the original verbose form
  //   "If Solend Main Pool USDC deposit APY is above X%, …"
  if (
    /^If\s+(?:Save\s+APY|Solend\s+Main\s+Pool\s+USDC\s+deposit\s+APY)/i.test(
      t,
    ) &&
    /deposit\s+0\.25\s+USDC/i.test(t)
  ) {
    return true;
  }
  // 繁中 — same broad shape; "有效期 N 分鐘" is optional in W5h-lite.
  if (/^如果\s*Save\s*APY/i.test(t) && /deposit\s*0\.25\s*USDC/i.test(t)) {
    return true;
  }
  return false;
}

/// 250 000 USDC base units = 0.25 USDC, the W5h pinned demo amount.
/// Hard-coded as a bigint so the funding-tx builder gets a guaranteed
/// u64-safe value even if a hostile DTO returns a giant `amount_raw`.
const W5H_DEMO_AMOUNT_BASE_UNITS = BigInt(250_000);

/// Hard upper bound on the parsed `amount_raw` from a W5h DTO. The
/// chat-route in the canonical W5h grammar only proposes 0.25 USDC;
/// any DTO that asks for more than 1 USDC (= 1_000_000 raw) is
/// treated as a wire-shape drift and we fall back to the demo amount.
/// This is defence-in-depth: the demo budget is pinned in the rule,
/// not in the DTO field.
const W5H_AMOUNT_SAFETY_CAP_BASE_UNITS = BigInt(1_000_000);

/// Parse a string `amount_raw` from a W5h DTO into a bigint. Defensive
/// — rejects negative / non-numeric / above-cap values, and falls back
/// to the pinned demo amount in any unhappy case.
function safeParseW5hAmount(raw: string | null | undefined): bigint {
  if (typeof raw !== "string" || !/^\d+$/.test(raw)) {
    return W5H_DEMO_AMOUNT_BASE_UNITS;
  }
  try {
    const n = BigInt(raw);
    if (n < BigInt(0) || n > W5H_AMOUNT_SAFETY_CAP_BASE_UNITS) {
      return W5H_DEMO_AMOUNT_BASE_UNITS;
    }
    return n;
  } catch {
    return W5H_DEMO_AMOUNT_BASE_UNITS;
  }
}

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
      // postChat threw — this is a transport-level failure (network
      // drop, fetch abort, JSON parse). The chat-route can legitimately
      // take 20-30+ s during W5g mainnet confirmation, so a thrown
      // request does NOT prove the tx didn't land. Branch:
      //
      //   - If the user's last message looks like a W5g execute
      //     command, emit a typed `local_w5g_safe_error` so the UI
      //     surfaces a safe, no-overclaim card (no completed badge,
      //     "verify on Solscan / backend logs if a signature was
      //     shown" copy). The user's intent is preserved.
      //
      //   - Otherwise fall back to the existing centred system notice.
      const errText =
        err instanceof Error ? err.message : "Network or parse error";
      if (looksLikeW5gExecuteCommand(text)) {
        const localError: ChatMessage = {
          id: `local-w5g-err-${Date.now()}`,
          kind: "local_w5g_safe_error",
          userText: text,
          networkError: errText,
          at: new Date().toISOString(),
        };
        setMessages((prev) => [...prev, localError]);
      } else if (looksLikeW5hChatCommand(text)) {
        const localError: ChatMessage = {
          id: `local-w5h-err-${Date.now()}`,
          kind: "local_w5h_safe_error",
          userText: text,
          networkError: errText,
          at: new Date().toISOString(),
        };
        setMessages((prev) => [...prev, localError]);
      } else {
        const sysMessage: ChatMessage = {
          id: `sys-${Date.now()}`,
          kind: "system",
          text: `Request failed: ${errText}`,
          at: new Date().toISOString(),
        };
        setMessages((prev) => [...prev, sysMessage]);
      }
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
                {m.kind === "assistant" && (
                  <AssistantBubble
                    result={m.result}
                    sessionId={sessionId}
                    walletPubkey={walletPubkey}
                  />
                )}
                {m.kind === "system" && <SystemNotice text={m.text} />}
                {m.kind === "local_w5g_safe_error" && (
                  <LocalW5gSafeErrorCard
                    userText={m.userText}
                    networkError={m.networkError}
                  />
                )}
                {m.kind === "local_w5h_safe_error" && (
                  <LocalW5hSafeErrorCard
                    userText={m.userText}
                    networkError={m.networkError}
                  />
                )}
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

function AssistantBubble({
  result,
  sessionId,
  walletPubkey,
}: {
  result: ChatRouteResult;
  /// Session id for backend calls the bubble's nested cards might
  /// make (W5h funding-confirm). `null` while the page is still
  /// opening the session — disables in-bubble actions until ready.
  sessionId: SessionId | null;
  /// Currently-connected Phantom pubkey. The W5h card uses this to
  /// gate the Fund button (refuses when no wallet, or when the
  /// connected pubkey ≠ the DTO's `user_wallet`).
  walletPubkey: string | null;
}) {
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
  return (
    <ChatResponseCard
      response={result.response}
      sessionId={sessionId}
      walletPubkey={walletPubkey}
    />
  );
}

function ChatResponseCard({
  response,
  sessionId,
  walletPubkey,
}: {
  response: ChatResponse;
  sessionId: SessionId | null;
  walletPubkey: string | null;
}) {
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

    case "w5h_conditional_order":
      // W5h chat-driven funding-gated conditional order. Card owns
      // the Phantom-funding flow state machine internally so each
      // chat message preserves its own progress; sessionId +
      // walletPubkey are threaded in so the Fund click can build
      // and submit a USDC TransferChecked and then call the
      // backend confirm route.
      return (
        <W5hConditionalOrderCard
          initial={response.result}
          sessionId={sessionId}
          walletPubkey={walletPubkey}
        />
      );

    case "w5g_conditional_execution":
      // W5g chat-first execution result. The user's second chat
      // message ("Execute W5g conditional deposit …") drives the
      // env-gated executor; this card surfaces the lifecycle.
      // Typed-only, NO raw JSON, NO action buttons.
      return <W5gConditionalExecutionCard result={response.result} />;

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

/// Pure clipboard control. Click ⇒ write text + flash a high-contrast
/// "Copied!" state for 1.4 s, then revert. NEVER submits chat, NEVER
/// calls execute, NEVER touches Phantom — it is a clipboard primitive.
///
/// The "Copied!" flash is intentionally loud (emerald background +
/// dark text) so the live-demo audience can see it from across the
/// room; the prompt explicitly called this out as central to the
/// chat-first execution flow.
///
/// Props:
///   - `value`    the literal text to write to the clipboard.
///   - `label`    short noun used in the aria-label (e.g. "wallet").
///   - `size`     "sm" (default, inline w/ short pubkey rows) or "md"
///                (used for the prominent execute-command panel).
function CopyButton({
  value,
  label,
  size = "sm",
}: {
  value: string;
  label: string;
  size?: "sm" | "md";
}) {
  const [copied, setCopied] = useState(false);
  const onClick = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      // navigator.clipboard can throw in non-secure contexts; swallow
      // silently — the value is still selectable in the DOM.
    }
  }, [value]);
  const baseSize =
    size === "md"
      ? "px-2.5 py-1 text-xs"
      : "ml-2 px-1.5 py-0.5 text-[10px]";
  const stateClasses = copied
    ? "border-emerald-500 bg-emerald-100 text-emerald-900 font-medium dark:bg-emerald-950 dark:text-emerald-100"
    : "border-foreground/30 hover:bg-muted text-foreground";
  return (
    <button
      type="button"
      aria-label={`Copy ${label}`}
      aria-live="polite"
      onClick={onClick}
      data-testid={`copy-button-${label.replace(/\s+/g, "-")}`}
      data-copied={copied ? "true" : "false"}
      className={`inline-flex shrink-0 items-center rounded border transition-colors ${baseSize} ${stateClasses}`}
    >
      {copied ? "Copied!" : "Copy"}
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
  // W5e/W5g briefs.
  let banner: { tone: string; text: string };
  switch (result.status) {
    case "ready_to_execute":
      banner = {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Ready to execute — copy the chat command below and send it to authorise the live deposit.",
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
  // W5g ready-to-execute precondition gate. We surface the copy-command
  // panel ONLY when all of the following are true, exactly mirroring
  // the W5g spec:
  //   - status === "ready_to_execute"        (W5e condition + budget pass)
  //   - budget_status === "reserved"         (no funding shortfall)
  //   - rule_persisted === true              (durable rule in state-store)
  //   - tx_signature == null                 (not already executed)
  //   - rule_id_hex is non-null              (we have a rule to reference)
  //   - canonical_rule_hash_hex is non-null  (Agent D requires it in
  //                                           the execute request body)
  // If any of these flips, the panel collapses — preventing a stale
  // "ready" panel from sitting under an already-executed rule, and
  // never rendering a command the backend would 400 on.
  const showW5gReadyPanel =
    result.status === "ready_to_execute" &&
    result.budget_status === "reserved" &&
    result.rule_persisted === true &&
    result.tx_signature === null &&
    result.rule_id_hex !== null &&
    result.rule_id_hex !== undefined &&
    result.canonical_rule_hash_hex !== null &&
    result.canonical_rule_hash_hex !== undefined;
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

        {/* ── W5g ready-to-execute panel (CHAT-FIRST, copy-command) ──
            The operator copies the suggested command and pastes it
            into the chat input. The chat-route detects the W5g
            approval grammar, dispatches to `Stage2ChatExecutor`, and
            replies with a typed `W5gConditionalExecution` card. There
            is intentionally NO Execute button here — every live send
            must go through the chat surface so the audit trail and
            the safety gates stay aligned. */}
        {showW5gReadyPanel &&
          result.rule_id_hex &&
          result.canonical_rule_hash_hex && (
            <ReadyToExecuteCommandPanel
              ruleIdHex={result.rule_id_hex}
              canonicalRuleHashHex={result.canonical_rule_hash_hex}
            />
          )}

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

/// W5g "Ready to execute — send this chat command" sub-panel.
///
/// Lives INSIDE the W5f conditional-order card when all of W5g's
/// ready-to-execute preconditions hold. The panel is **chat-first**:
///
///   - It surfaces the EXACT chat command the operator types or pastes
///     into the chat input below.
///   - It has ONE control: `Copy command` (a CopyButton). That control
///     ONLY writes the command text to the clipboard; it does NOT
///     submit chat, does NOT call any execute endpoint, does NOT
///     touch Phantom.
///   - It has NO Execute / Approve / Send Transaction / Confirm
///     Deposit button. Live execution travels EXCLUSIVELY through the
///     user's second chat message — keeping the demo provably
///     chat-driven.
///
// ── W5h chat-driven funding-gated conditional order ─────────────────
//
// Renders the user-facing card for the W5h chat command:
//
//   "If Solend Main Pool USDC deposit APY is above X%, deposit 0.25
//    USDC from my wallet, expires in N minutes."
//   "如果 Save APY > X%，deposit 0.25 USDC，有效期 N 分鐘"
//
// Lifecycle (server status → client funding-flow):
//
//   funding_required        → Fund button enabled (when wallet matches)
//   funding_required + idle → "Fund 0.25 USDC with Phantom" CTA
//   client.preparing        → "Preparing transaction…"
//   client.awaiting_signature → "Awaiting Phantom signature…"
//   client.broadcasting     → "Broadcasting funding tx…"
//   client.submitted        → "Submitted; polling chain status"
//   client.polling_chain    → "Confirming on Mainnet… N/M"
//   client.confirming_backend → "Backend verifying budget reservation…"
//   budget_reserved         → green banner, hand off to W5g panel
//   watching                → blue/amber: condition false but rule live
//   ready_to_execute        → emerald: condition met, W5g panel rendered
//   expired                 → amber: countdown done, refund prompt
//   refunded                → muted: budget returned
//   funding_failed          → red: typed error_reason
//
// Allowed buttons: exactly ONE — "Fund 0.25 USDC with Phantom".
// Plus three copy controls (Copy wallet, Copy USDC ATA, Copy execute
// command — the last one only after budget_reserved & condition_met).
//
// FORBIDDEN here: Execute / Approve / Send Solend Deposit / Confirm
// Deposit. Solend execution stays chat-first via the W5g approval
// command, which is rendered via the existing
// `ReadyToExecuteCommandPanel` below.
//
// Safety invariants enforced inside this component:
//   1. `signTransaction` only fires inside the `handleFund` user
//      gesture path. NEVER from an effect.
//   2. `sendRawTransaction` fires at most ONCE per Fund click. The
//      Fund button disables for the rest of the flow.
//   3. No private-key handling. No `Keypair` import. The destination
//      ATA is read from the DTO (`controlled_usdc_ata`) and used
//      verbatim — no controlled-wallet keypair is loaded to derive it.
//   4. Mint stays pinned to USDC. The transfer instruction asserts
//      decimals via TransferChecked, so a hostile DTO that injected
//      a non-USDC ATA pubkey would fail on-chain before settling.
//   5. Amount is parsed via `safeParseW5hAmount` with a cap at 1 USDC
//      so a wire-shape drift cannot escalate the spend.
//   6. The funding amount + destination are displayed in the card
//      BEFORE the Fund button is enabled, so the user reads them
//      pre-Phantom-popup.
//   7. Phantom-rejected signatures map to a typed `error` flow
//      state — they never claim a finalised budget.

/// In-flight state the W5h card owns locally. Sits ON TOP of the
/// server-side `status` in `W5hConditionalDepositResult.status` — the
/// server status drives the display BRANCH (funding_required vs
/// budget_reserved vs expired etc.) and the local flow drives the
/// LOADING UX inside the funding_required branch.
type W5hFundingFlow =
  | { kind: "idle" }
  | { kind: "preparing" }
  | { kind: "awaiting_signature" }
  | { kind: "broadcasting" }
  /// sendRawTransaction returned. Signature is on the wire.
  | { kind: "submitted"; signature: string }
  /// Polling `getSignatureStatuses` for finalization.
  | { kind: "polling_chain"; signature: string; attempts: number }
  /// Backend confirm POST in flight, OR backend last returned
  /// `funding_pending` and we're inside the bounded re-poll loop.
  /// `attempts` is 1 on the first call and increments on each
  /// funding_pending re-poll. Never claims `budget_reserved`.
  | { kind: "confirming_backend"; signature: string; attempts: number }
  /// Backend confirm POST 4xx/5xx. Signature MAY have landed; we
  /// keep it so the operator can verify on Solscan.
  | { kind: "error"; reason: string; signature?: string };

// ── Bounded backend confirm-poll loop ────────────────────────────────
//
// After Phantom funding tx is broadcast AND we observe chain
// finality, we POST to the backend confirm route. If it answers
// `funding_pending` the frontend keeps polling on a fixed interval
// for up to a bounded number of attempts (≈75 s ceiling). During
// the loop we keep the same "Funding submitted — waiting for chain
// confirmation" copy visible. We NEVER mark the order as
// `budget_reserved` client-side — only when the backend re-emits a
// status that is not `funding_pending`.
const W5H_BACKEND_CONFIRM_MAX_ATTEMPTS = 30;
const W5H_BACKEND_CONFIRM_INTERVAL_MS = 2_500;

function sleepMs(ms: number): Promise<void> {
  return new Promise((resolve) => {
    window.setTimeout(resolve, ms);
  });
}

function W5hConditionalOrderCard({
  initial,
  sessionId,
  walletPubkey,
}: {
  initial: W5hConditionalDepositResult;
  sessionId: SessionId | null;
  walletPubkey: string | null;
}) {
  const [result, setResult] = useState<W5hConditionalDepositResult>(initial);
  const [flow, setFlow] = useState<W5hFundingFlow>({ kind: "idle" });

  const connection = useMemo(
    () => new Connection(SOLANA_RPC_URL, "confirmed"),
    [],
  );

  // ── Expiry display (W5h-lite: informational only) ─────────────────
  //
  // W5h-lite (2026-05-12) scope reduction: the frontend no longer
  // promises an automatic refund or auto-expiry. If the backend
  // omits `expires_at_ms` we show NO countdown at all. If it carries
  // one we show a live remaining-time label as INFORMATIONAL metadata
  // only — never as a Fund-button gate, never as a status flip.
  // Cancellation / refund are explicit manual operator actions.
  const [nowMs, setNowMs] = useState<number>(() => Date.now());
  const hasExpiresAtMs =
    typeof result.expires_at_ms === "string" &&
    /^\d+$/.test(result.expires_at_ms);
  useEffect(() => {
    if (!hasExpiresAtMs) return;
    const id = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [hasExpiresAtMs]);
  const expiresAtMs = useMemo(() => {
    if (!hasExpiresAtMs) return null;
    const n = Number(result.expires_at_ms);
    return Number.isSafeInteger(n) ? n : null;
  }, [hasExpiresAtMs, result.expires_at_ms]);
  const remainingMs =
    expiresAtMs === null ? null : Math.max(0, expiresAtMs - nowMs);
  // Show the row only when the backend gave us a value AND the order
  // is still in a "live" state. After budget_reserved / etc. the
  // information is largely irrelevant for the W5h-lite demo.
  const showCountdown =
    hasExpiresAtMs &&
    (result.status === "funding_required" ||
      result.status === "funding_pending" ||
      result.status === "watching" ||
      result.status === "budget_reserved" ||
      result.status === "ready_to_execute");

  // ── W5i auto-execution status polling ─────────────────────────────
  //
  // Once the order is `budget_reserved` or `ready_to_execute` and the
  // backend gives us a `rule_id_hex`, poll the status route every 5 s
  // so the demo viewer sees the W5i watcher → executing → completed
  // transition in real time. Stops on terminal auto-execution states
  // (completed / failed / broadcasted_timeout) and on network errors
  // (we keep the last result and surface a small "status refresh
  // delayed" notice — never overwriting good state with a bad one).
  //
  // READ-ONLY: the polling effect performs ONLY a GET. It never signs,
  // never broadcasts, never constructs a Solend instruction.
  const [statusPollError, setStatusPollError] = useState<string | null>(
    null,
  );
  const ruleIdForPoll = result.rule_id_hex;
  const autoExecStatus = result.auto_execution_status ?? null;
  const autoExecTerminal =
    autoExecStatus === "completed" ||
    autoExecStatus === "failed" ||
    autoExecStatus === "broadcasted_timeout";
  const shouldPollOrderStatus =
    sessionId !== null &&
    typeof ruleIdForPoll === "string" &&
    ruleIdForPoll.length > 0 &&
    (result.status === "budget_reserved" ||
      result.status === "ready_to_execute") &&
    !autoExecTerminal;
  // 5 s ticker — bounded by `shouldPollOrderStatus` flipping to false
  // (terminal state reached, or order moved out of the watching
  // window). No artificial attempt cap; the upstream lifecycle is the
  // natural exit.
  useEffect(() => {
    if (!shouldPollOrderStatus) return;
    if (sessionId === null) return;
    if (typeof ruleIdForPoll !== "string" || ruleIdForPoll.length === 0) {
      return;
    }
    let cancelled = false;
    const pollOnce = async () => {
      if (cancelled) return;
      try {
        const env = await getW5hOrderStatus(sessionId, ruleIdForPoll);
        if (cancelled) return;
        if (env.kind === "ok") {
          setResult(env.response);
          setStatusPollError(null);
        } else {
          // Backend transient — keep last good state visible.
          setStatusPollError(`status refresh delayed (${env.httpStatus})`);
        }
      } catch {
        if (cancelled) return;
        setStatusPollError("status refresh delayed (network)");
      }
    };
    // First poll immediately so the demo viewer doesn't wait 5 s
    // for the very first transition.
    void pollOnce();
    const id = window.setInterval(() => {
      void pollOnce();
    }, 5_000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [shouldPollOrderStatus, sessionId, ruleIdForPoll]);

  // ── Wallet gating ─────────────────────────────────────────────────
  const expectedUserWallet = result.user_wallet ?? null;
  const walletConnected = walletPubkey !== null;
  const walletMatchesExpected =
    expectedUserWallet === null
      ? true
      : walletPubkey === expectedUserWallet;
  const walletOk = walletConnected && walletMatchesExpected;
  const walletMismatch =
    walletConnected &&
    expectedUserWallet !== null &&
    walletPubkey !== expectedUserWallet;

  // ── Fund button gate ──────────────────────────────────────────────
  const fundInFlight =
    flow.kind === "preparing" ||
    flow.kind === "awaiting_signature" ||
    flow.kind === "broadcasting" ||
    flow.kind === "submitted" ||
    flow.kind === "polling_chain" ||
    flow.kind === "confirming_backend";
  const canFund =
    result.status === "funding_required" &&
    !fundInFlight &&
    walletOk &&
    sessionId !== null;

  const handleFund = useCallback(async () => {
    if (!canFund) return;
    if (walletPubkey === null || sessionId === null) return;

    setFlow({ kind: "preparing" });

    // 1. Derive the source ATA from the connected wallet (the DTO
    //    can carry `user_usdc_ata` but we derive defensively — the
    //    derivation is pure and the backend will reject mismatches).
    let payer: PublicKey;
    let sourceAta: PublicKey;
    let destinationAta: PublicKey;
    let controlledWallet: PublicKey;
    try {
      payer = new PublicKey(walletPubkey);
      const usdcMint = new PublicKey(USDC_MINT_BASE58);
      sourceAta = result.user_usdc_ata
        ? new PublicKey(result.user_usdc_ata)
        : deriveAtaPubkey(payer, usdcMint);
      destinationAta = new PublicKey(result.controlled_usdc_ata);
      controlledWallet = new PublicKey(
        result.controlled_wallet || CONTROLLED_WALLET_BASE58,
      );
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `pubkey parse failed: ${err.message}`
            : "pubkey parse failed",
      });
      return;
    }

    // 2. Fresh blockhash.
    let blockhash: string;
    try {
      const { blockhash: bh } =
        await connection.getLatestBlockhash("confirmed");
      blockhash = bh;
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `RPC blockhash fetch failed: ${err.message}`
            : "RPC blockhash fetch failed",
      });
      return;
    }

    // 3. Build the W5h transfer tx (TransferChecked + idempotent
    //    CreateAta). Amount is parsed via the defensive helper.
    let tx: Transaction;
    try {
      tx = buildW5hFundingTransaction({
        payer,
        sourceAta,
        destinationAta,
        includeCreateAta: true,
        amountBaseUnits: safeParseW5hAmount(result.amount_raw),
        controlledWallet,
        // Memo anchor — `claw:w5h:<rule_id_hex>:<canonical_rule_hash_hex>`
        // is inserted as instruction 0. Pass the EXACT DTO values
        // (no transformation) so the on-chain audit trail matches the
        // persisted rule byte-for-byte.
        ruleIdHex: result.rule_id_hex,
        canonicalRuleHashHex: result.canonical_rule_hash_hex,
        recentBlockhash: blockhash,
      });
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `tx build failed: ${err.message}`
            : "tx build failed",
      });
      return;
    }

    // 4. Phantom signs (popup). The user APPROVED this by clicking
    //    Fund and seeing the amount + destination above; signing
    //    happens here and only here.
    const provider = getPhantomProvider();
    if (!provider) {
      setFlow({
        kind: "error",
        reason: "Phantom provider not detected — please connect first.",
      });
      return;
    }
    setFlow({ kind: "awaiting_signature" });
    let signedTx: Transaction;
    try {
      signedTx = await provider.signTransaction(tx);
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `Phantom rejected / signing failed: ${err.message}`
            : "Phantom rejected / signing failed",
      });
      return;
    }

    // 5. Broadcast signed bytes — ONCE. No retry on failure to
    //    preserve "exactly one sendRawTransaction per Fund click".
    setFlow({ kind: "broadcasting" });
    let signature: string;
    try {
      const raw = signedTx.serialize();
      signature = await connection.sendRawTransaction(raw, {
        skipPreflight: false,
        preflightCommitment: "confirmed",
      });
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `RPC send failed: ${err.message}`
            : "RPC send failed",
      });
      return;
    }

    setFlow({ kind: "submitted", signature });

    // 6. Poll on-chain finality. Replaces the old
    //    `confirmTransaction({ blockhash, lastValidBlockHeight: 0 }, ...)`
    //    path that gave up at block-height-exceeded BEFORE late-
    //    landing txs finalized.
    try {
      const pollResult = await pollSignatureStatus(connection, signature, {
        maxAttempts: MAX_POLL_ATTEMPTS_DEFAULT,
        intervalMs: POLL_INTERVAL_MS_DEFAULT,
        onAttempt: (n) => {
          setFlow({ kind: "polling_chain", signature, attempts: n });
        },
      });
      if (pollResult.kind === "failed_on_chain") {
        setFlow({
          kind: "error",
          reason: `Transaction failed on chain: ${JSON.stringify(
            pollResult.err,
          )}`,
          signature,
        });
        return;
      }
      if (pollResult.kind === "timeout") {
        setFlow({
          kind: "error",
          reason:
            "Confirmation timeout — the tx may still land. Verify on Solscan.",
          signature,
        });
        return;
      }
      // pollResult.kind === "finalized" — proceed to backend confirm.
    } catch (err) {
      if (err instanceof SignatureStatusNetworkError) {
        setFlow({
          kind: "error",
          reason: "Network error polling status; verify on Solscan.",
          signature,
        });
        return;
      }
      throw err;
    }

    // 7. Tell the backend so it re-reads on-chain authoritatively
    //    and flips the W5h state to budget_reserved / watching /
    //    ready_to_execute / funding_failed.
    //
    //    W5h-lite addendum: the backend can answer `funding_pending`
    //    while it's still catching up to the chain. The frontend
    //    polls the confirm route on a bounded loop while that holds,
    //    keeping the "Funding submitted — waiting for chain
    //    confirmation" banner visible. We NEVER mark budget_reserved
    //    client-side; only the backend can.
    const confirmBody = {
      rule_id_hex: result.rule_id_hex,
      funding_signature: signature,
      user_wallet: walletPubkey,
      user_usdc_ata: sourceAta.toBase58(),
      controlled_wallet: result.controlled_wallet,
      controlled_usdc_ata: result.controlled_usdc_ata,
      amount_raw: result.amount_raw,
    };
    setFlow({ kind: "confirming_backend", signature, attempts: 1 });
    let attempt = 0;
    try {
      while (true) {
        attempt += 1;
        const env = await confirmW5hFunding(sessionId, confirmBody);
        if (env.kind !== "ok") {
          setFlow({
            kind: "error",
            reason: `Backend confirm failed (${env.httpStatus}): ${env.error || "no error body"}`,
            signature,
          });
          return;
        }
        // Mirror the backend's latest view into the card. This includes
        // the funding_pending status — we do NOT swallow it.
        setResult(env.response);
        if (env.response.status !== "funding_pending") {
          // Terminal (budget_reserved / watching / ready_to_execute /
          // expired / refunded / funding_failed). Drop the loop;
          // server status now drives the render.
          setFlow({ kind: "idle" });
          return;
        }
        // Still pending — bail if we've hit the bounded cap; otherwise
        // sleep and retry while updating the attempts counter for UI.
        if (attempt >= W5H_BACKEND_CONFIRM_MAX_ATTEMPTS) {
          setFlow({
            kind: "error",
            reason:
              `Backend still reports funding_pending after ` +
              `${(attempt * W5H_BACKEND_CONFIRM_INTERVAL_MS) / 1000}s; ` +
              "verify on Solscan and reload to retry.",
            signature,
          });
          return;
        }
        await sleepMs(W5H_BACKEND_CONFIRM_INTERVAL_MS);
        setFlow({
          kind: "confirming_backend",
          signature,
          attempts: attempt + 1,
        });
      }
    } catch (err) {
      setFlow({
        kind: "error",
        reason:
          err instanceof Error
            ? `Backend confirm threw: ${err.message}`
            : "Backend confirm threw",
        signature,
      });
    }
  }, [canFund, connection, result, sessionId, walletPubkey]);

  // ── Banner copy ──────────────────────────────────────────────────
  const banner = w5hBanner(result.status, flow, walletMismatch);
  const showW5gPanel =
    (result.status === "budget_reserved" ||
      result.status === "ready_to_execute") &&
    result.rule_id_hex.length > 0 &&
    result.canonical_rule_hash_hex.length > 0;
  // Active funding signature — either an in-flight one (flow.signature)
  // or one preserved on the result (post-confirm).
  const liveFundingSig =
    flow.kind === "submitted" ||
    flow.kind === "polling_chain" ||
    flow.kind === "confirming_backend" ||
    flow.kind === "error"
      ? (flow as { signature?: string }).signature ?? null
      : result.funding_signature ?? null;

  return (
    <div className="flex justify-start">
      <div
        data-testid="w5h-conditional-order-card"
        data-status={result.status}
        className="max-w-[85%] rounded-2xl rounded-bl-sm bg-card border px-4 py-3 text-sm space-y-2"
      >
        <div className="font-medium">W5h conditional order (funding-gated)</div>
        <div className="text-xs text-muted-foreground italic break-words">
          &ldquo;{result.input_text}&rdquo;
        </div>

        <div
          data-testid="w5h-status-banner"
          data-status={result.status}
          data-flow-kind={flow.kind}
          className={`mt-2 inline-block rounded border px-2 py-1 text-xs ${banner.tone}`}
        >
          {banner.text}
        </div>

        {/* ── Funding parameters ────────────────────────────────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">
            Funding parameters
          </div>
          <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
            <dt className="text-muted-foreground">amount</dt>
            <dd className="font-mono" data-testid="w5h-amount">
              <span className="text-foreground">
                {formatRawUsdcDisplay(result.amount_raw)}
              </span>
              <span className="ml-2 text-muted-foreground">
                ({result.amount_raw} raw)
              </span>
            </dd>

            <dt className="text-muted-foreground">USDC mint</dt>
            <dd className="font-mono break-all">{USDC_MINT_BASE58}</dd>

            <dt className="text-muted-foreground">controlled wallet</dt>
            <dd className="font-mono break-all flex items-start">
              <span
                className="break-all"
                data-testid="w5h-controlled-wallet"
              >
                {result.controlled_wallet}
              </span>
              <CopyButton
                value={result.controlled_wallet}
                label="controlled wallet"
              />
            </dd>

            <dt className="text-muted-foreground">controlled USDC ATA</dt>
            <dd className="font-mono break-all flex items-start">
              <span
                className="break-all"
                data-testid="w5h-controlled-usdc-ata"
              >
                {result.controlled_usdc_ata}
              </span>
              <CopyButton
                value={result.controlled_usdc_ata}
                label="controlled USDC ATA"
              />
            </dd>

            <dt className="text-muted-foreground">instruction hash memo</dt>
            <dd
              className="font-mono break-words"
              data-testid="w5h-memo-row"
            >
              <W5hMemoCell
                ruleIdHex={result.rule_id_hex}
                canonicalRuleHashHex={result.canonical_rule_hash_hex}
              />
            </dd>

            {showCountdown && (
              <>
                <dt className="text-muted-foreground">expires in</dt>
                <dd
                  className="font-mono"
                  data-testid="w5h-countdown"
                  data-expired={
                    remainingMs !== null && remainingMs === 0
                      ? "true"
                      : "false"
                  }
                  title="Informational only — the demo does not auto-expire or auto-refund from the frontend."
                >
                  {remainingMs === null
                    ? "—"
                    : formatRemaining(remainingMs)}
                  <span className="ml-2 text-[10px] text-muted-foreground italic">
                    (informational)
                  </span>
                </dd>
              </>
            )}
          </dl>
        </div>

        {/* ── Decision metrics ──────────────────────────────────── */}
        {(result.save_display_apy_bps !== undefined ||
          result.native_onchain_apr_bps !== undefined) && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              Decision metrics
            </div>
            <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              {result.save_display_apy_bps !== undefined &&
                result.save_display_apy_bps !== null && (
                  <>
                    <dt className="text-muted-foreground">
                      Save display APY
                    </dt>
                    <dd data-testid="w5h-save-display-apy">
                      {result.save_display_apy_bps} bps (
                      {bpsToPctLabel(result.save_display_apy_bps)})
                    </dd>
                  </>
                )}
              {result.native_onchain_apr_bps !== undefined &&
                result.native_onchain_apr_bps !== null && (
                  <>
                    <dt className="text-muted-foreground">
                      native on-chain APR (audit)
                    </dt>
                    <dd data-testid="w5h-native-onchain-apr">
                      {result.native_onchain_apr_bps} bps (
                      {bpsToPctLabel(result.native_onchain_apr_bps)})
                    </dd>
                  </>
                )}
              <dt className="text-muted-foreground">threshold</dt>
              <dd>
                {result.threshold_bps} bps (
                {bpsToPctLabel(result.threshold_bps)})
              </dd>
              {result.condition_met !== undefined && (
                <>
                  <dt className="text-muted-foreground">condition_met</dt>
                  <dd className="font-mono">
                    {result.condition_met ? "true" : "false"}
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}

        {/* ── Wallet-bind hint (when no Phantom yet) ───────────── */}
        {result.status === "funding_required" && !walletConnected && (
          <div className="mt-3 rounded border border-amber-300 bg-amber-50/70 p-3 text-xs text-amber-900">
            Connect Phantom in the header to enable funding.
          </div>
        )}
        {walletMismatch && (
          <div
            className="mt-3 rounded border border-rose-300 bg-rose-50/70 p-3 text-xs text-rose-900 break-words"
            data-testid="w5h-wallet-mismatch"
          >
            Connected wallet does NOT match the expected user wallet
            for this order. Fund button is disabled — switch Phantom
            accounts to{" "}
            <code className="font-mono">{expectedUserWallet}</code>.
          </div>
        )}

        {/* ── Fund button — the SINGLE allowed action button ──── */}
        {result.status === "funding_required" && (
          <div className="mt-3 space-y-2">
            <p
              className="text-xs text-foreground"
              data-testid="w5h-fund-helper-copy"
            >
              Fund this conditional order with Phantom. The controlled
              wallet will hold the 0.25 USDC budget until execution or
              manual cancellation.
            </p>
            <div className="flex items-center gap-3 flex-wrap">
              <Button
                size="sm"
                onClick={() => void handleFund()}
                disabled={!canFund}
                data-testid="w5h-fund-button"
              >
                {fundButtonLabel(flow, result.amount_raw)}
              </Button>
              <span className="text-[10px] text-muted-foreground italic">
                Phantom pops up only on this click. One signature; one
                broadcast.
              </span>
            </div>
          </div>
        )}

        {/* ── Funding signature display ─────────────────────────── */}
        {liveFundingSig && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              Funding tx
            </div>
            <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              <dt className="text-muted-foreground">signature</dt>
              <dd
                className="font-mono break-all"
                data-testid="w5h-funding-signature"
              >
                {liveFundingSig}
              </dd>
              <dt className="text-muted-foreground">solscan</dt>
              <dd>
                <a
                  href={solscanTxUrl(liveFundingSig)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="font-mono underline hover:no-underline break-all"
                  data-testid="w5h-solscan-link"
                >
                  {solscanTxUrl(liveFundingSig)}
                </a>
              </dd>
              {result.funding_confirmation_slot && (
                <>
                  <dt className="text-muted-foreground">
                    confirmation slot
                  </dt>
                  <dd className="font-mono break-all">
                    {result.funding_confirmation_slot}
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}

        {/* ── W5i auto-execution status section ────────────────── */}
        {(result.status === "budget_reserved" ||
          result.status === "ready_to_execute") && (
          <W5iAutoExecutionSection
            result={result}
            statusPollError={statusPollError}
          />
        )}

        {/* ── Hand-off: W5g approval-command panel ───────────────
            When the backend reports `auto_execution_enabled === true`
            the demo is fully autonomous — the manual W5g chat command
            collapses under a "Manual fallback" toggle so the demo
            audience sees the autopilot path. When `false` / undefined
            (e.g. W5h-lite mode without auto-watcher), the panel stays
            visible as the primary execution affordance. */}
        {showW5gPanel && (
          result.auto_execution_enabled === true ? (
            <details
              className="mt-3"
              data-testid="w5g-manual-fallback-details"
            >
              <summary className="cursor-pointer text-[11px] text-muted-foreground hover:text-foreground">
                Manual fallback command (backup path — auto-execution is
                enabled)
              </summary>
              <ReadyToExecuteCommandPanel
                ruleIdHex={result.rule_id_hex}
                canonicalRuleHashHex={result.canonical_rule_hash_hex}
              />
            </details>
          ) : (
            <ReadyToExecuteCommandPanel
              ruleIdHex={result.rule_id_hex}
              canonicalRuleHashHex={result.canonical_rule_hash_hex}
            />
          )
        )}

        {/* ── Expired branch (W5h-lite: no refund promise) ─────── */}
        {result.status === "expired" && (
          <div
            className="mt-3 rounded border border-amber-300 bg-amber-50/70 p-3 text-xs text-amber-900"
            data-testid="w5h-expired-card"
          >
            <div className="font-medium">Order window closed</div>
            <p className="mt-1">
              The conditional order is no longer eligible for
              execution. The W5h-lite demo does not promise an
              automatic refund — cancellation / refund is a manual
              operator action. If the backend later reports a refund
              signature it will appear below for reference.
            </p>
            {result.refund_signature && (
              <div className="mt-2 font-mono break-all">
                refund:{" "}
                <a
                  href={solscanTxUrl(result.refund_signature)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="underline hover:no-underline"
                >
                  {result.refund_signature}
                </a>
              </div>
            )}
          </div>
        )}

        {/* ── Refunded branch ─────────────────────────────────── */}
        {result.status === "refunded" && (
          <div
            className="mt-3 rounded border border-muted-foreground/30 bg-muted/40 p-3 text-xs text-muted-foreground"
            data-testid="w5h-refunded-card"
          >
            Refunded — budget returned to user wallet.
            {result.refund_signature && (
              <div className="mt-1 font-mono break-all">
                refund:{" "}
                <a
                  href={solscanTxUrl(result.refund_signature)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="underline hover:no-underline"
                >
                  {result.refund_signature}
                </a>
              </div>
            )}
          </div>
        )}

        {/* ── Funding-failed branch ──────────────────────────── */}
        {result.status === "funding_failed" && (
          <div
            className="mt-3 rounded border border-rose-300 bg-rose-50/70 p-3 text-xs text-rose-900 space-y-1 break-words"
            data-testid="w5h-funding-failed-card"
          >
            <div className="font-medium">Funding failed</div>
            {result.error_code && (
              <div className="font-mono">
                code: <code>{result.error_code}</code>
              </div>
            )}
            {result.error_reason && (
              <div>{result.error_reason}</div>
            )}
          </div>
        )}

        {/* ── Client-side flow error (signing / broadcast / poll) */}
        {flow.kind === "error" && (
          <div
            className="mt-3 rounded border border-rose-300 bg-rose-50/70 p-3 text-xs text-rose-900 space-y-1 break-words"
            data-testid="w5h-flow-error"
          >
            <div className="font-medium">Funding flow error</div>
            <div>{flow.reason}</div>
            {flow.signature && (
              <div className="font-mono break-all">
                tx broadcasted:{" "}
                <a
                  href={solscanTxUrl(flow.signature)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="underline hover:no-underline"
                >
                  {flow.signature}
                </a>
                <span className="ml-1 italic">
                  — verify on Solscan; no completed claim made
                  client-side.
                </span>
              </div>
            )}
          </div>
        )}

        {/* ── No-overclaim footer ────────────────────────────── */}
        <div className="mt-2 text-[10px] text-muted-foreground leading-snug">
          <p>
            W5h funding goes from your wallet to the controlled wallet
            via a single SPL TransferChecked. No Solend / Jupiter
            execution is triggered from this page. The W5g approval
            command above (when shown) is the only path to the live
            Solend deposit, and it is a chat message — not a button.
          </p>
        </div>
      </div>
    </div>
  );
}

// ── W5h on-chain Memo cell ──────────────────────────────────────────
//
// Renders the EXACT UTF-8 bytes the W5h funding tx will anchor in its
// `MemoSq4gqAB…` instruction. Short form is shown inline; the full
// value lives behind a <details> toggle with a Copy control. This is
// PURELY display — the same `buildW5hMemoText` helper is used by the
// tx builder, so what the operator sees here byte-matches what Phantom
// will sign.
function W5hMemoCell({
  ruleIdHex,
  canonicalRuleHashHex,
}: {
  ruleIdHex: string;
  canonicalRuleHashHex: string;
}) {
  const full = buildW5hMemoText(ruleIdHex, canonicalRuleHashHex);
  // Truncate-with-ellipsis on the two hex tokens for the inline label.
  // Hex values are 32 / 64 chars; first 8 + last 4 is enough to be
  // recognisable while still fitting in a row.
  const shortHex = (hex: string): string => {
    if (hex.length <= 14) return hex;
    return `${hex.slice(0, 8)}…${hex.slice(-4)}`;
  };
  const shortMemo = `claw:w5h:${shortHex(ruleIdHex)}:${shortHex(canonicalRuleHashHex)}`;
  return (
    <div>
      <div className="flex items-start gap-2 flex-wrap">
        <span
          className="text-foreground"
          data-testid="w5h-memo-short"
        >
          {shortMemo}
        </span>
        <CopyButton value={full} label="memo" />
      </div>
      <details className="mt-1">
        <summary className="cursor-pointer text-[10px] text-muted-foreground hover:text-foreground">
          show full memo bytes
        </summary>
        <pre
          className="mt-1 overflow-x-auto rounded bg-muted px-2 py-1 text-[10px] leading-snug whitespace-pre-wrap break-all"
          data-testid="w5h-memo-full"
        >
          {full}
        </pre>
        <p className="mt-1 text-[10px] text-muted-foreground italic">
          Inserted as instruction 0 of the W5h funding transaction
          via the SPL Memo program. Anchors the rule identity on-chain
          alongside the SPL Token TransferChecked.
        </p>
      </details>
    </div>
  );
}

// ── W5i auto-execution status sub-section ───────────────────────────
//
// Rendered inside the W5h card once the order is `budget_reserved`
// or `ready_to_execute`. PURELY read-only — surfaces what the
// backend watcher / executor reported via the order-status poll.
// Renders NO buttons (Execute, Approve, Send Solend, Confirm Deposit
// are all forbidden by spec). The Solend deposit signature is
// SERVER-CONSTRUCTED and SERVER-SIGNED; the frontend mirrors it into
// the DOM via a Solscan link so the audience can verify off-page.
function W5iAutoExecutionSection({
  result,
  statusPollError,
}: {
  result: W5hConditionalDepositResult;
  statusPollError: string | null;
}) {
  // No backend signal at all → render nothing. Keeps the card clean
  // when running against a backend version that pre-dates W5i.
  if (
    result.auto_execution_enabled === undefined &&
    result.auto_execution_status === undefined
  ) {
    return null;
  }

  const autoOn = result.auto_execution_enabled === true;
  const autoStatus = result.auto_execution_status ?? null;
  const banner = w5iAutoBanner(autoOn, autoStatus);

  // Resolve Solscan URL — backend-supplied one wins; otherwise build
  // from the signature.
  const sig = result.auto_tx_signature ?? null;
  const solscan =
    result.auto_solscan_url ?? (sig ? solscanTxUrl(sig) : null);

  // Last-check timestamp display — a relative "Ns ago" string. Uses
  // safe-int guard against absurd payloads.
  const lastCheckedLabel = formatRelativeMsAgo(result.auto_last_checked_at_ms);

  return (
    <div
      className="mt-3"
      data-testid="w5i-auto-execution-section"
      data-auto-enabled={autoOn ? "true" : "false"}
      data-auto-status={autoStatus ?? "(none)"}
    >
      <div
        data-testid="w5i-auto-banner"
        className={`inline-block rounded border px-2 py-1 text-xs ${banner.tone}`}
      >
        {banner.text}
      </div>

      <dl className="mt-2 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
        <dt className="text-muted-foreground">auto-execution</dt>
        <dd className="font-mono">
          {autoOn ? "enabled" : "disabled"}
        </dd>
        {autoStatus !== null && (
          <>
            <dt className="text-muted-foreground">watcher status</dt>
            <dd
              className="font-mono"
              data-testid="w5i-auto-watcher-status"
            >
              {autoStatus}
            </dd>
          </>
        )}
        {lastCheckedLabel && (
          <>
            <dt className="text-muted-foreground">last checked</dt>
            <dd className="font-mono">{lastCheckedLabel}</dd>
          </>
        )}
        {sig && (
          <>
            <dt className="text-muted-foreground">auto tx signature</dt>
            <dd
              className="font-mono break-all"
              data-testid="w5i-auto-tx-signature"
            >
              {sig}
            </dd>
          </>
        )}
        {solscan && (
          <>
            <dt className="text-muted-foreground">solscan</dt>
            <dd>
              <a
                href={solscan}
                target="_blank"
                rel="noopener noreferrer"
                className="font-mono underline hover:no-underline break-all"
                data-testid="w5i-auto-solscan-link"
              >
                {solscan}
              </a>
            </dd>
          </>
        )}
        {result.auto_confirmation_slot && (
          <>
            <dt className="text-muted-foreground">confirmation slot</dt>
            <dd className="font-mono break-all">
              {result.auto_confirmation_slot}
            </dd>
          </>
        )}
        {result.auto_usdc_delta_raw && (
          <>
            <dt className="text-muted-foreground">USDC delta</dt>
            <dd
              className="font-mono break-all"
              data-testid="w5i-auto-usdc-delta"
            >
              {result.auto_usdc_delta_raw} raw
              {(() => {
                const ui = formatRawUsdcDisplay(result.auto_usdc_delta_raw);
                return ui === "—" ? null : (
                  <span className="ml-2 text-muted-foreground">
                    ({ui})
                  </span>
                );
              })()}
            </dd>
          </>
        )}
        {result.auto_ctoken_delta_raw && (
          <>
            <dt className="text-muted-foreground">cToken delta</dt>
            <dd
              className="font-mono break-all"
              data-testid="w5i-auto-ctoken-delta"
            >
              {result.auto_ctoken_delta_raw} raw
            </dd>
          </>
        )}
        {(result.auto_error_code || result.auto_error_reason) && (
          <>
            {result.auto_error_code && (
              <>
                <dt className="text-muted-foreground">error code</dt>
                <dd
                  className="font-mono"
                  data-testid="w5i-auto-error-code"
                >
                  {result.auto_error_code}
                </dd>
              </>
            )}
            {result.auto_error_reason && (
              <>
                <dt className="text-muted-foreground">error reason</dt>
                <dd
                  className="break-words whitespace-pre-wrap"
                  data-testid="w5i-auto-error-reason"
                >
                  {result.auto_error_reason}
                </dd>
              </>
            )}
          </>
        )}
      </dl>

      {statusPollError && (
        <div
          className="mt-2 text-[10px] text-amber-700 italic"
          data-testid="w5i-status-refresh-delayed"
        >
          {statusPollError} — last-known state still shown above.
        </div>
      )}
    </div>
  );
}

/// Per-(auto_enabled, auto_status) banner copy + tone. The W5i prompt
/// pins specific copy for each lifecycle state.
function w5iAutoBanner(
  autoOn: boolean,
  autoStatus: W5hConditionalDepositResult["auto_execution_status"] | null,
): { tone: string; text: string } {
  if (!autoOn) {
    return {
      tone: "bg-muted text-muted-foreground border-muted",
      text: "Budget reserved — execution gate is off.",
    };
  }
  switch (autoStatus) {
    case "watching":
    case null:
    case undefined:
      return {
        tone: "bg-sky-50 text-sky-900 border-sky-200",
        text: "Budget reserved — watching Save APY every 30 seconds.",
      };
    case "ready_to_execute":
      return {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Condition met — backend is about to execute.",
      };
    case "executing":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Executing on mainnet…",
      };
    case "completed":
      return {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Completed — Solend deposit finalized.",
      };
    case "failed":
      return {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Execution failed — no retry was attempted.",
      };
    case "broadcasted_timeout":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Broadcasted but finality timed out — check explorer.",
      };
    default: {
      const exhaustive: never = autoStatus;
      return { tone: "", text: String(exhaustive) };
    }
  }
}

/// "Ns ago" string from a Unix-millis timestamp. Returns `null` when
/// the input isn't a parseable safe-int — defensive against backend
/// drift. Bounded to "now" minimum (clamps a stale clock to "0s ago").
function formatRelativeMsAgo(
  raw: string | null | undefined,
): string | null {
  if (typeof raw !== "string" || !/^\d+$/.test(raw)) return null;
  const n = Number(raw);
  if (!Number.isSafeInteger(n)) return null;
  const ms = Math.max(0, Date.now() - n);
  const sec = Math.floor(ms / 1000);
  if (sec < 60) return `${sec}s ago`;
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  return `${m}m ${s}s ago`;
}

function fundButtonLabel(
  flow: W5hFundingFlow,
  amountRaw: string,
): string {
  switch (flow.kind) {
    case "idle":
      return `Fund ${formatRawUsdcDisplay(amountRaw)} with Phantom`;
    case "preparing":
      return "Preparing…";
    case "awaiting_signature":
      return "Awaiting Phantom…";
    case "broadcasting":
      return "Broadcasting…";
    case "submitted":
      return "Submitted, confirming…";
    case "polling_chain":
      return `Confirming on chain… (${flow.attempts}/${MAX_POLL_ATTEMPTS_DEFAULT})`;
    case "confirming_backend":
      return `Backend confirming… (${flow.attempts}/${W5H_BACKEND_CONFIRM_MAX_ATTEMPTS})`;
    case "error":
      return "Funding failed";
    default: {
      const exhaustive: never = flow;
      return String(exhaustive);
    }
  }
}

/// "0.25 USDC" display for a base-units string. Uses safe-integer
/// guarded math; falls back to "—" when the raw value is outside
/// safe-integer range (defensive — shouldn't happen for realistic
/// amounts, but we never crash on a weird DTO).
function formatRawUsdcDisplay(raw: string | null | undefined): string {
  if (typeof raw !== "string" || !/^\d+$/.test(raw)) return "—";
  const n = Number(raw);
  if (!Number.isSafeInteger(n)) return "—";
  return `${(n / 1_000_000).toFixed(6)} USDC`;
}

/// Render `mm:ss` from a millisecond remainder.
function formatRemaining(remainingMs: number): string {
  const totalSec = Math.floor(remainingMs / 1000);
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

/// Per-(server-status, client-flow) banner copy + tone. Server status
/// has primacy — once the backend says `budget_reserved` we render
/// the green banner regardless of any stale client-flow state.
function w5hBanner(
  status: W5hConditionalDepositResult["status"],
  flow: W5hFundingFlow,
  walletMismatch: boolean,
): { tone: string; text: string } {
  // Terminal server-side states come first.
  switch (status) {
    case "budget_reserved":
      return {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Budget reserved — controlled wallet holds the 0.25 USDC. Watching the condition.",
      };
    case "ready_to_execute":
      return {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Ready to execute — APY threshold met. Copy the W5g approval command below to authorise the live deposit.",
      };
    case "watching":
      return {
        tone: "bg-sky-50 text-sky-900 border-sky-200",
        text: "Watching — budget reserved; waiting for APY threshold to be met.",
      };
    case "expired":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Expired — order window closed. Cancellation / refund is a manual operator action in the W5h-lite demo.",
      };
    case "refunded":
      return {
        tone: "bg-muted text-muted-foreground border-muted",
        text: "Refunded — budget returned to user wallet.",
      };
    case "funding_failed":
      return {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Funding failed — see error details below.",
      };
    case "funding_pending":
      // The backend has the signature but hasn't observed on-chain
      // budget yet. The frontend's bounded confirm-poll loop keeps
      // hitting the confirm route every ~2.5 s while this status
      // holds. We surface the SAME "waiting for chain confirmation"
      // copy as the in-flight `confirming_backend` flow.
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Funding submitted — waiting for chain confirmation.",
      };
    case "funding_required":
      // Fall through to flow-dependent copy below.
      break;
    default: {
      const exhaustive: never = status;
      return { tone: "", text: String(exhaustive) };
    }
  }

  if (walletMismatch) {
    return {
      tone: "bg-rose-50 text-rose-900 border-rose-200",
      text: "Wallet mismatch — connected pubkey is not the expected user wallet.",
    };
  }

  switch (flow.kind) {
    case "idle":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Funding required — click Fund to send 0.25 USDC into the controlled wallet.",
      };
    case "preparing":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Preparing funding transaction…",
      };
    case "awaiting_signature":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Awaiting Phantom signature… approve in the popup.",
      };
    case "broadcasting":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Broadcasting signed funding tx…",
      };
    case "submitted":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Funding submitted — waiting for chain confirmation.",
      };
    case "polling_chain":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: `Funding submitted — confirming on chain (${flow.attempts}/${MAX_POLL_ATTEMPTS_DEFAULT}).`,
      };
    case "confirming_backend":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: `Funding submitted — waiting for chain confirmation (${flow.attempts}/${W5H_BACKEND_CONFIRM_MAX_ATTEMPTS}).`,
      };
    case "error":
      return {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Funding interrupted — see details below.",
      };
    default: {
      const exhaustive: never = flow;
      return { tone: "", text: String(exhaustive) };
    }
  }
}

/// The `data-testid` hooks let smoke tests assert "no Execute button
/// rendered on ready_to_execute" by grepping for absence of
/// `data-testid="w5g-execute-button"` (no such hook exists in this
/// codebase — the panel itself is the proof).
function ReadyToExecuteCommandPanel({
  ruleIdHex,
  canonicalRuleHashHex,
}: {
  ruleIdHex: string;
  canonicalRuleHashHex: string;
}) {
  const command = buildW5gExecuteCommand(ruleIdHex, canonicalRuleHashHex);
  return (
    <div
      data-testid="w5g-ready-to-execute-panel"
      className="mt-3 rounded border border-amber-300 bg-amber-50/70 p-3 space-y-2"
    >
      <div className="text-sm font-medium text-amber-950">Ready to execute</div>
      <p className="text-xs text-amber-900/90">
        To execute, send this chat command:
      </p>
      <div className="rounded bg-white border border-amber-200 p-2 font-mono text-[11px] leading-snug break-all text-foreground">
        <span data-testid="w5g-execute-command">{command}</span>
      </div>
      <div className="flex items-center gap-2">
        <CopyButton value={command} label="execute command" size="md" />
        <span className="text-[11px] text-amber-900/80 italic">
          Copy &amp; paste into the chat input — this button does not send
          chat or execute anything itself.
        </span>
      </div>
    </div>
  );
}

// ── W5g typed result card ─────────────────────────────────────────────
//
// Renders one of five lifecycle states (pending / completed / rejected
// / failed / broadcasted_timeout). Hard rules:
//
//   - NO raw JSON blob; every field is typed.
//   - NO Execute / Approve / Send Transaction / Confirm Deposit button.
//   - NO sign / broadcast / clipboard-of-private-key call sites.
//   - Raw u64 fields are STRINGS — never coerce to JS number without
//     a `Number.isSafeInteger` guard.

const SOLEND_SOLSCAN_PREFIX = "https://solscan.io/tx/";

/// Build a Solscan URL when the backend didn't pre-build one. Returns
/// `null` if no signature is present.
function solscanUrlFor(
  sig: string | null | undefined,
  pre: string | null | undefined,
): string | null {
  if (pre && pre.length > 0) return pre;
  if (sig && sig.length > 0) return `${SOLEND_SOLSCAN_PREFIX}${sig}`;
  return null;
}

/// Parse a raw integer string and return a `{ display, raw }` shape.
/// `display` is `null` when the value would overflow JS safe-integer
/// arithmetic — the consumer renders the raw string only in that case.
function safeUsdcUiFromRawString(
  raw: string | null | undefined,
): { uiUsdc: string | null; raw: string } | null {
  if (raw === null || raw === undefined || raw === "") return null;
  // Reject anything that isn't a sign + digits — defence against a
  // misbehaving backend.
  if (!/^-?\d+$/.test(raw)) return { uiUsdc: null, raw };
  const n = Number(raw);
  if (!Number.isSafeInteger(n)) return { uiUsdc: null, raw };
  // USDC has 6 decimals. Negative deltas keep their sign.
  const sign = n < 0 ? "-" : "";
  const abs = Math.abs(n);
  const whole = Math.floor(abs / 1_000_000);
  const frac = (abs % 1_000_000).toString().padStart(6, "0");
  return { uiUsdc: `${sign}${whole}.${frac}`, raw };
}

/// Render a u64 raw amount alongside a safe UI conversion. Falls back
/// to the raw string if the number is outside JS-safe-integer range.
function RawUsdcCell({ raw }: { raw: string | null | undefined }) {
  const parsed = safeUsdcUiFromRawString(raw);
  if (parsed === null) {
    return <span className="text-muted-foreground italic">N/A</span>;
  }
  return (
    <span>
      <span className="font-mono">{parsed.raw} raw</span>
      {parsed.uiUsdc !== null && (
        <span className="ml-2 text-muted-foreground">
          ({parsed.uiUsdc} USDC)
        </span>
      )}
    </span>
  );
}

/// Render a u64/u128 raw count (no decimal conversion).
function RawCountCell({ raw }: { raw: string | null | undefined }) {
  if (raw === null || raw === undefined || raw === "") {
    return <span className="text-muted-foreground italic">N/A</span>;
  }
  return <span className="font-mono break-all">{raw}</span>;
}

/// Format a basis-point integer when present.
function maybeBps(bps: number | null | undefined): string {
  if (bps === null || bps === undefined) return "N/A";
  return `${bps} bps (${bpsToPctLabel(bps)})`;
}

/// Pick banner tone for a W5g status. Mirrors the W5e/W5f banner
/// style for visual continuity inside the same chat feed.
///
/// Status enum aligns with Agent D's `ChatExecuteResultDto.status`:
///   - completed             → mainnet finalized; completed badge shown
///   - broadcasted_timeout   → sig on chain but not observed finalized
///   - prechecks_failed      → blocked before any broadcast attempt
///   - execution_failed      → broadcast / verification / build failed
function w5gStatusBanner(status: W5gConditionalExecutionResult["status"]): {
  tone: string;
  text: string;
} {
  switch (status) {
    case "completed":
      return {
        tone: "bg-emerald-50 text-emerald-900 border-emerald-200",
        text: "Completed — mainnet deposit finalized.",
      };
    case "prechecks_failed":
      return {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Pre-execution checks failed — no broadcast was attempted.",
      };
    case "execution_failed":
      return {
        tone: "bg-rose-50 text-rose-900 border-rose-200",
        text: "Execution failed.",
      };
    case "broadcasted_timeout":
      return {
        tone: "bg-amber-50 text-amber-900 border-amber-200",
        text: "Broadcasted, but finalization was not observed before timeout.",
      };
    default: {
      const exhaustive: never = status;
      return { tone: "", text: String(exhaustive) };
    }
  }
}

/// Top-level W5g card. Shape mirrors the W5d/W5e/W5f card so the chat
/// feed reads as a continuous narrative — same rounded bubble, same
/// dl-grid sections, same break-all monospace for hashes.
function W5gConditionalExecutionCard({
  result,
}: {
  result: W5gConditionalExecutionResult;
}) {
  const banner = w5gStatusBanner(result.status);
  const txSig = result.tx_signature ?? null;
  const solscan = solscanUrlFor(txSig, result.solscan_url);
  // The "completed badge" is shown ONLY for status === "completed".
  // rejected / failed / broadcasted_timeout MUST NOT show it.
  const showCompletedBadge = result.status === "completed";

  return (
    <div className="flex justify-start">
      <div
        data-testid="w5g-conditional-execution-card"
        data-status={result.status}
        className="max-w-[85%] rounded-2xl rounded-bl-sm bg-card border px-4 py-3 text-sm space-y-2"
      >
        <div className="flex items-center gap-2 flex-wrap">
          <span className="font-medium">W5g chat-first execution</span>
          {showCompletedBadge && (
            <Badge
              variant="outline"
              className="border-emerald-500/60 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              data-testid="w5g-completed-badge"
            >
              Finalized
            </Badge>
          )}
        </div>

        <div
          data-testid="w5g-status-banner"
          data-status={result.status}
          className={`mt-1 inline-block rounded border px-2 py-1 text-xs ${banner.tone}`}
        >
          {banner.text}
        </div>

        {/* ── Rule + amount + accounts (always present) ─────────── */}
        <div className="mt-3">
          <div className="text-xs font-medium text-foreground/80">
            Rule &amp; accounts
          </div>
          <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
            <dt className="text-muted-foreground">rule_id</dt>
            <dd
              className="font-mono break-all"
              data-testid="w5g-rule-id"
            >
              {result.rule_id_hex}
            </dd>

            <dt className="text-muted-foreground">canonical_rule_hash</dt>
            <dd
              className="font-mono break-all"
              data-testid="w5g-canonical-hash"
            >
              {result.canonical_rule_hash_hex}
            </dd>

            {result.amount_raw && (
              <>
                <dt className="text-muted-foreground">amount</dt>
                <dd data-testid="w5g-amount">
                  <RawUsdcCell raw={result.amount_raw} />
                </dd>
              </>
            )}

            {result.controlled_wallet && (
              <>
                <dt className="text-muted-foreground">controlled wallet</dt>
                <dd className="font-mono break-all flex items-start">
                  <span
                    className="break-all"
                    data-testid="w5g-controlled-wallet"
                  >
                    {result.controlled_wallet}
                  </span>
                  <CopyButton
                    value={result.controlled_wallet}
                    label="controlled wallet"
                  />
                </dd>
              </>
            )}

            {result.source_usdc_ata && (
              <>
                <dt className="text-muted-foreground">source USDC ATA</dt>
                <dd className="font-mono break-all flex items-start">
                  <span
                    className="break-all"
                    data-testid="w5g-source-usdc-ata"
                  >
                    {result.source_usdc_ata}
                  </span>
                  <CopyButton
                    value={result.source_usdc_ata}
                    label="source USDC ATA"
                  />
                </dd>
              </>
            )}

            {result.reserve_pubkey && (
              <>
                <dt className="text-muted-foreground">reserve</dt>
                <dd
                  className="font-mono break-all"
                  data-testid="w5g-reserve"
                >
                  {result.reserve_pubkey}
                </dd>
              </>
            )}

            {result.obligation_pubkey && (
              <>
                <dt className="text-muted-foreground">obligation</dt>
                <dd className="font-mono break-all">
                  {result.obligation_pubkey}
                </dd>
              </>
            )}
          </dl>
        </div>

        {/* ── Decision metrics (Agent D `used_*_bps` echoes) ───── */}
        {(result.used_save_display_apy_bps !== undefined ||
          result.used_native_onchain_apr_bps !== undefined ||
          result.used_threshold_bps !== undefined ||
          result.decision_source) && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              Decision metrics at execution
            </div>
            <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              {result.used_save_display_apy_bps !== undefined && (
                <>
                  <dt className="text-muted-foreground">
                    Save display APY (used)
                  </dt>
                  <dd data-testid="w5g-used-save-display-apy">
                    {maybeBps(result.used_save_display_apy_bps)}
                  </dd>
                </>
              )}
              {result.used_native_onchain_apr_bps !== undefined && (
                <>
                  <dt className="text-muted-foreground">
                    native on-chain APR (audit, used)
                  </dt>
                  <dd data-testid="w5g-used-native-onchain-apr">
                    {maybeBps(result.used_native_onchain_apr_bps)}
                  </dd>
                </>
              )}
              {result.used_threshold_bps !== undefined && (
                <>
                  <dt className="text-muted-foreground">threshold (used)</dt>
                  <dd data-testid="w5g-used-threshold">
                    {maybeBps(result.used_threshold_bps)}
                  </dd>
                </>
              )}
              {result.decision_source && (
                <>
                  <dt className="text-muted-foreground">decision source</dt>
                  <dd
                    className="font-mono"
                    data-testid="w5g-decision-source"
                  >
                    {result.decision_source}
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}

        {/* ── Tx signature + Solscan (pending / completed /
              broadcasted_timeout typically; rejected/failed only when
              backend chose to surface one) ──────────────────────── */}
        {txSig && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              On-chain
            </div>
            <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              <dt className="text-muted-foreground">tx_signature</dt>
              <dd
                className="font-mono break-all"
                data-testid="w5g-tx-signature"
              >
                {txSig}
              </dd>
              {solscan && (
                <>
                  <dt className="text-muted-foreground">solscan</dt>
                  <dd>
                    <a
                      href={solscan}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="font-mono underline hover:no-underline break-all"
                      data-testid="w5g-solscan-link"
                    >
                      {solscan}
                    </a>
                  </dd>
                </>
              )}
              {result.confirmation_slot && (
                <>
                  <dt className="text-muted-foreground">confirmation slot</dt>
                  <dd>
                    <RawCountCell raw={result.confirmation_slot} />
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}

        {/* ── USDC + cToken deltas (typically completed only) ─── */}
        {(result.before_usdc_raw !== undefined ||
          result.after_usdc_raw !== undefined ||
          result.usdc_delta_raw !== undefined ||
          result.before_ctoken_amount !== undefined ||
          result.after_ctoken_amount !== undefined ||
          result.ctoken_delta_raw !== undefined) && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              Account deltas
            </div>
            <dl
              className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs"
              data-testid="w5g-deltas-block"
            >
              {result.before_usdc_raw !== undefined && (
                <>
                  <dt className="text-muted-foreground">USDC before</dt>
                  <dd>
                    <RawUsdcCell raw={result.before_usdc_raw} />
                  </dd>
                </>
              )}
              {result.after_usdc_raw !== undefined && (
                <>
                  <dt className="text-muted-foreground">USDC after</dt>
                  <dd>
                    <RawUsdcCell raw={result.after_usdc_raw} />
                  </dd>
                </>
              )}
              {result.usdc_delta_raw !== undefined && (
                <>
                  <dt className="text-muted-foreground">USDC delta</dt>
                  <dd>
                    <RawUsdcCell raw={result.usdc_delta_raw} />
                  </dd>
                </>
              )}
              {result.before_ctoken_amount !== undefined && (
                <>
                  <dt className="text-muted-foreground">cToken before</dt>
                  <dd>
                    <RawCountCell raw={result.before_ctoken_amount} />
                  </dd>
                </>
              )}
              {result.after_ctoken_amount !== undefined && (
                <>
                  <dt className="text-muted-foreground">cToken after</dt>
                  <dd>
                    <RawCountCell raw={result.after_ctoken_amount} />
                  </dd>
                </>
              )}
              {result.ctoken_delta_raw !== undefined && (
                <>
                  <dt className="text-muted-foreground">cToken delta</dt>
                  <dd>
                    <RawCountCell raw={result.ctoken_delta_raw} />
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}

        {/* ── Tx shape (audit) ──────────────────────────────────── */}
        {(result.serialized_tx_bytes !== undefined ||
          result.instruction_count !== undefined ||
          result.ctoken_ata_create_included !== undefined) && (
          <div className="mt-3">
            <div className="text-xs font-medium text-foreground/80">
              Tx shape
            </div>
            <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
              {result.serialized_tx_bytes !== undefined && (
                <>
                  <dt className="text-muted-foreground">serialized bytes</dt>
                  <dd>
                    <RawCountCell raw={result.serialized_tx_bytes} />
                  </dd>
                </>
              )}
              {result.instruction_count !== undefined && (
                <>
                  <dt className="text-muted-foreground">instruction count</dt>
                  <dd>
                    <RawCountCell raw={result.instruction_count} />
                  </dd>
                </>
              )}
              {result.ctoken_ata_create_included !== undefined &&
                result.ctoken_ata_create_included !== null && (
                  <>
                    <dt className="text-muted-foreground">
                      cToken ATA create included
                    </dt>
                    <dd className="font-mono">
                      {result.ctoken_ata_create_included ? "true" : "false"}
                    </dd>
                  </>
                )}
            </dl>
          </div>
        )}

        {/* ── Failure / timeout reason copy ────────────────────── */}
        {(result.status === "prechecks_failed" ||
          result.status === "execution_failed" ||
          result.status === "broadcasted_timeout") &&
          (result.error_reason || result.error_code) && (
            <div className="mt-3">
              <div className="text-xs font-medium text-foreground/80">
                Reason
              </div>
              {result.error_code && (
                <dl className="mt-1 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 text-xs">
                  <dt className="text-muted-foreground">error code</dt>
                  <dd
                    className="font-mono"
                    data-testid="w5g-error-code"
                  >
                    {result.error_code}
                  </dd>
                </dl>
              )}
              {result.error_reason && (
                <p
                  className="mt-1 text-xs break-words whitespace-pre-wrap"
                  data-testid="w5g-error-reason"
                >
                  {result.error_reason}
                </p>
              )}
            </div>
          )}

        {/* ── No-overclaim footer ──────────────────────────────── */}
        <div className="mt-2 text-[10px] text-muted-foreground leading-snug">
          <p>
            W5g chat-first execution — the user&apos;s second chat
            message drives the env-gated executor. No Execute / Approve
            / Send button exists; the only frontend action is copying
            text. Live finality is whatever Solscan says for the
            signature above; this card reports the backend&apos;s
            typed view.
          </p>
        </div>
      </div>
    </div>
  );
}

/// Safe-error card emitted client-side when `postChat` itself throws
/// while the user's last message looked like a W5g execute command.
/// Distinct from the typed `broadcasted_timeout` status — that status
/// comes from the backend after a real broadcast; this card means the
/// HTTP request never returned a body at all. The copy makes the
/// uncertainty explicit and avoids any "completed" affordance.
function LocalW5gSafeErrorCard({
  userText,
  networkError,
}: {
  userText: string;
  networkError: string;
}) {
  return (
    <Alert
      className="border-amber-500/40"
      data-testid="w5g-local-safe-error-card"
    >
      <AlertTitle>Execution request — status unknown</AlertTitle>
      <AlertDescription className="space-y-2 text-xs">
        <p>
          The execution request may still be pending or broadcasted. Check
          backend logs or Solscan if a signature was shown.
        </p>
        <details className="text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">
            chat command sent
          </summary>
          <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug whitespace-pre-wrap break-all">
            {userText}
          </pre>
        </details>
        <details className="text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">
            transport error
          </summary>
          <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug whitespace-pre-wrap break-words">
            {networkError}
          </pre>
        </details>
        <p className="text-[10px] italic">
          No completed badge — the frontend never claims finalization on
          a thrown HTTP request.
        </p>
      </AlertDescription>
    </Alert>
  );
}

/// Safe-error card for a thrown `postChat` on a W5h chat command. The
/// chat-route hadn't returned a body, so no W5h order exists yet —
/// nothing to render in the funding flow. The card preserves the
/// user's command text so they can re-submit without re-typing the
/// bilingual conditional-order grammar.
function LocalW5hSafeErrorCard({
  userText,
  networkError,
}: {
  userText: string;
  networkError: string;
}) {
  return (
    <Alert
      className="border-amber-500/40"
      data-testid="w5h-local-safe-error-card"
    >
      <AlertTitle>W5h order request — status unknown</AlertTitle>
      <AlertDescription className="space-y-2 text-xs">
        <p>
          The chat request failed before a W5h order card could be
          rendered. No funding tx has been built or signed — Phantom
          stays closed until you re-submit and click Fund.
        </p>
        <details className="text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">
            chat command sent
          </summary>
          <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug whitespace-pre-wrap break-all">
            {userText}
          </pre>
        </details>
        <details className="text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">
            transport error
          </summary>
          <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug whitespace-pre-wrap break-words">
            {networkError}
          </pre>
        </details>
        <p className="text-[10px] italic">
          No funding has happened — re-send the W5h command to retry.
        </p>
      </AlertDescription>
    </Alert>
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
