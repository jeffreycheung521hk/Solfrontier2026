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

import { useEffect, useRef, useState } from "react";
import Link from "next/link";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { WalletConnect } from "@/components/wallet-connect";
import { getOrCreateSession, postChat } from "@/lib/api";
import { IS_SHOWCASE, MODE } from "@/lib/mode";
import type {
  ChatMessage,
  ChatResponse,
  ChatRouteResult,
  SessionId,
} from "@/lib/types";

// Backend caps the request body at 4096 bytes; the harness caps the
// message string at 4000 chars after trim. Mirror the char cap here so
// we don't burn a round-trip on a known-rejection.
const MAX_MESSAGE_CHARS = 4000;

const SUGGESTED_PROMPTS = [
  "Deposit 0.001 USDC into Solend.",
  "Propose a 0.001 USDC Solend deposit. Don't approve, sign, or broadcast.",
];

export default function ChatPage() {
  const [sessionId, setSessionId] = useState<SessionId | null>(null);
  const [sessionError, setSessionError] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);

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

  const trimmed = input.trim();
  const canSend =
    !!sessionId && !sending && trimmed.length > 0 && trimmed.length <= MAX_MESSAGE_CHARS;

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
            <WalletConnect />
          </div>
        </div>
        <p className="text-sm text-muted-foreground">
          Natural-language proposal entry. The assistant proposes only — approval and wallet
          signing remain human-controlled at every step.
        </p>
      </header>

      <SessionStatus sessionId={sessionId} sessionError={sessionError} />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Conversation</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {messages.length === 0 && (
            <EmptyState
              onSelect={(prompt) => setInput(prompt)}
              disabled={!sessionId}
            />
          )}
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
            sessionId
              ? "Type a request — e.g. 'Deposit 0.001 USDC into Solend'"
              : "Opening session…"
          }
          disabled={!sessionId}
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

function EmptyState({
  onSelect,
  disabled,
}: {
  onSelect: (prompt: string) => void;
  disabled: boolean;
}) {
  return (
    <div className="rounded-md border border-dashed bg-muted/30 px-4 py-6 space-y-3">
      <p className="text-sm text-muted-foreground">
        Try one of these to see the LLM dispatch shape:
      </p>
      <div className="flex flex-wrap gap-2">
        {SUGGESTED_PROMPTS.map((prompt) => (
          <button
            key={prompt}
            type="button"
            onClick={() => onSelect(prompt)}
            disabled={disabled}
            className="text-left text-sm rounded-md border bg-background px-3 py-2
                       hover:border-foreground/30 transition-colors disabled:opacity-50"
          >
            {prompt}
          </button>
        ))}
      </div>
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
      return <ToolDispatchedCard toolName={response.tool_name} output={response.output} />;

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

    default: {
      // Exhaustiveness check — `never` assertion fails to compile if
      // the ChatResponse union grows a new variant without a case
      // above. Returning the never-typed value keeps eslint happy.
      const exhaustive: never = response;
      return exhaustive;
    }
  }
}

function ToolDispatchedCard({ toolName, output }: { toolName: string; output: unknown }) {
  // Best-effort extraction of the inner status + approval id without
  // assuming a tight schema. Day 2 will tighten this.
  const data = (output as { data?: { status?: string; approval_request_id?: string; amount_raw?: number } })?.data;
  const innerStatus = data?.status;
  const approvalId = data?.approval_request_id;
  const amount = data?.amount_raw;

  const isAwaitingApproval = innerStatus === "awaiting_approval";

  return (
    <Card className="border-foreground/15">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Tool call: <code>{toolName}</code>
          </CardTitle>
          {innerStatus && (
            <Badge variant={isAwaitingApproval ? "default" : "secondary"} className="text-xs">
              {innerStatus.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {amount !== undefined && (
          <div className="text-xs text-muted-foreground">
            Amount: <span className="font-mono text-foreground">{amount}</span> raw units
          </div>
        )}
        {isAwaitingApproval && approvalId && approvalId !== "00000000-0000-0000-0000-000000000000" && (
          <>
            <Separator />
            <div className="flex items-center justify-between text-sm">
              <div className="text-muted-foreground">
                Approval request <code className="text-xs">{approvalId.slice(0, 8)}…</code>
              </div>
              <Link href={`/approval/${approvalId}`}>
                <Button size="sm">Review &amp; Approve →</Button>
              </Link>
            </div>
          </>
        )}
        {isAwaitingApproval && approvalId === "00000000-0000-0000-0000-000000000000" && (
          <>
            <Separator />
            <div className="text-xs text-muted-foreground">
              Showcase fixture — no real approval was created. In live mode this card links to
              the operator approval page.
            </div>
          </>
        )}
        <details className="text-xs text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">raw output</summary>
          <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug">
            {JSON.stringify(output, null, 2)}
          </pre>
        </details>
      </CardContent>
    </Card>
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
