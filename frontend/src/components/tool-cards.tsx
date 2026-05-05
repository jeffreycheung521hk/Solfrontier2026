"use client";

// Tool result cards — typed renderers for the chat surface's
// `tool_dispatched` ChatResponse variant.
//
// Each card extracts a tightly typed view of the daemon's tool output
// (mirroring the backend `output_schema` in
// `crates/gateway/src/tools/*.rs`) and renders an ergonomic summary.
// Raw JSON is ALWAYS available behind a collapsible `<details>` so
// nothing is hidden — these cards are formatting, not redaction.
//
// The dispatch component `ToolResultCard` switches on `tool_name` and
// falls back to a generic card when the name is unknown. The card-per-
// tool components are exported individually for unit testing if
// needed.
//
// No OpenAI / LLM is involved in this rendering. All formatting is
// pure TypeScript on the daemon's typed output.

import Link from "next/link";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { shortPubkey } from "@/lib/format";

// ── Mint → display name ────────────────────────────────────────────────────

const USDC_MAINNET_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL_MINT = "So11111111111111111111111111111111111111112";

function mintLabel(mint: string | null | undefined): string {
  if (!mint) return "—";
  if (mint === USDC_MAINNET_MINT) return "USDC";
  if (mint === WSOL_MINT) return "SOL";
  return shortPubkey(mint, 4);
}

// ── Dispatch entry point ───────────────────────────────────────────────────

export function ToolResultCard({
  toolName,
  output,
}: {
  toolName: string;
  output: unknown;
}) {
  switch (toolName) {
    case "get_wallet_balances":
      return <WalletBalancesCard output={output} />;
    case "get_jupiter_quote":
      return <JupiterQuoteCard output={output} />;
    case "solend_deposit_usdc":
      return <SolendDepositCard output={output} />;
    default:
      return <GenericToolCard toolName={toolName} output={output} />;
  }
}

// ── get_wallet_balances ─────────────────────────────────────────────────────
//
// Backend output_schema (crates/gateway/src/tools/get_wallet_balances.rs):
//   status:        "ok" | "wallet_not_bound" | "rpc_error"
//   wallet_pubkey: string | null
//   sol_lamports:  integer | null
//   sol_ui:        string  | null
//   usdc_mint:     string  | null
//   usdc_ata:      string  | null
//   usdc_raw:      integer | null
//   usdc_ui:       string  | null
//   error:         string  | null

interface WalletBalancesData {
  status?: "ok" | "wallet_not_bound" | "rpc_error";
  wallet_pubkey?: string | null;
  sol_lamports?: number | null;
  sol_ui?: string | null;
  usdc_mint?: string | null;
  usdc_ata?: string | null;
  usdc_raw?: number | null;
  usdc_ui?: string | null;
  error?: string | null;
}

export function WalletBalancesCard({ output }: { output: unknown }) {
  const data = (output as { data?: WalletBalancesData })?.data ?? {};
  const status = data.status;
  const variant = status === "ok" ? "default" : status === "wallet_not_bound" ? "outline" : "destructive";

  return (
    <Card className="border-foreground/15" data-testid="card-wallet-balances">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Tool call: <code>get_wallet_balances</code>
          </CardTitle>
          {status && (
            <Badge variant={variant} className="text-xs">
              {status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {status === "ok" && (
          <div className="space-y-2 text-xs">
            {data.wallet_pubkey && (
              <KeyValueRow
                k="wallet"
                v={<code className="text-foreground">{shortPubkey(data.wallet_pubkey, 6)}</code>}
              />
            )}
            <KeyValueRow
              k="SOL"
              v={
                <span>
                  <span className="font-mono text-foreground">{data.sol_ui ?? "—"}</span>
                  {typeof data.sol_lamports === "number" && (
                    <span className="ml-2 text-muted-foreground">
                      ({data.sol_lamports.toLocaleString()} lamports)
                    </span>
                  )}
                </span>
              }
            />
            <KeyValueRow
              k="USDC"
              v={
                <span>
                  <span className="font-mono text-foreground">{data.usdc_ui ?? "—"}</span>
                  {typeof data.usdc_raw === "number" && (
                    <span className="ml-2 text-muted-foreground">
                      ({data.usdc_raw.toLocaleString()} raw)
                    </span>
                  )}
                </span>
              }
            />
            {data.usdc_ata && (
              <KeyValueRow
                k="USDC ATA"
                v={<code className="text-muted-foreground">{shortPubkey(data.usdc_ata, 6)}</code>}
              />
            )}
          </div>
        )}
        {status === "wallet_not_bound" && (
          <Alert>
            <AlertTitle>No wallet bound to this session</AlertTitle>
            <AlertDescription>
              Connect Phantom on this page and complete the bind challenge before asking
              for balances. The daemon refuses to read balances without an explicit
              session-wallet binding.
            </AlertDescription>
          </Alert>
        )}
        {status === "rpc_error" && (
          <Alert variant="destructive">
            <AlertTitle>RPC error</AlertTitle>
            <AlertDescription className="break-words">
              {data.error ?? "No error message returned."}
            </AlertDescription>
          </Alert>
        )}
        <RawOutputDetails output={output} />
      </CardContent>
    </Card>
  );
}

// ── get_jupiter_quote ───────────────────────────────────────────────────────
//
// Backend output_schema (crates/gateway/src/tools/get_jupiter_quote.rs):
//   status:                 "ok" | "policy_blocked" | "quote_unavailable" | "provider_error"
//   input_mint, output_mint
//   input_amount, out_amount, other_amount_threshold (raw integers)
//   price_impact_pct        string ("0.0042" style decimal)
//   route_summary           string[]   (e.g. ["Orca", "Raydium"])
//   slippage_bps            integer
//   policy_rule_name        string | null
//   error                   string | null

interface JupiterQuoteData {
  status?: "ok" | "policy_blocked" | "quote_unavailable" | "provider_error";
  input_mint?: string | null;
  output_mint?: string | null;
  input_amount?: number | null;
  out_amount?: number | null;
  other_amount_threshold?: number | null;
  price_impact_pct?: string | null;
  route_summary?: string[] | null;
  slippage_bps?: number | null;
  policy_rule_name?: string | null;
  error?: string | null;
}

export function JupiterQuoteCard({ output }: { output: unknown }) {
  const data = (output as { data?: JupiterQuoteData })?.data ?? {};
  const status = data.status;
  const variant = status === "ok" ? "default" : status === "policy_blocked" ? "outline" : "destructive";

  return (
    <Card className="border-foreground/15" data-testid="card-jupiter-quote">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Tool call: <code>get_jupiter_quote</code>
          </CardTitle>
          {status && (
            <Badge variant={variant} className="text-xs">
              {status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {status === "ok" && (
          <div className="space-y-2 text-xs">
            <div className="flex items-center gap-2 font-mono">
              <span className="text-muted-foreground">{mintLabel(data.input_mint)}</span>
              <span className="text-foreground">
                {typeof data.input_amount === "number" ? data.input_amount.toLocaleString() : "—"}
              </span>
              <span className="text-muted-foreground">→</span>
              <span className="text-muted-foreground">{mintLabel(data.output_mint)}</span>
              <span className="text-foreground">
                {typeof data.out_amount === "number" ? data.out_amount.toLocaleString() : "—"}
              </span>
              <span className="text-muted-foreground">(raw)</span>
            </div>
            {typeof data.other_amount_threshold === "number" && (
              <KeyValueRow
                k="min after slippage"
                v={
                  <span className="font-mono text-foreground">
                    {data.other_amount_threshold.toLocaleString()} raw
                  </span>
                }
              />
            )}
            {typeof data.slippage_bps === "number" && (
              <KeyValueRow
                k="slippage cap"
                v={
                  <span className="font-mono text-foreground">
                    {data.slippage_bps} bps ({(data.slippage_bps / 100).toFixed(2)}%)
                  </span>
                }
              />
            )}
            {data.price_impact_pct && (
              <KeyValueRow
                k="price impact"
                v={<span className="font-mono text-foreground">{data.price_impact_pct}</span>}
              />
            )}
            {Array.isArray(data.route_summary) && data.route_summary.length > 0 && (
              <KeyValueRow
                k="route"
                v={
                  <span className="font-mono text-foreground">
                    {data.route_summary.join(" → ")}
                  </span>
                }
              />
            )}
            <div className="text-[11px] text-muted-foreground italic pt-1">
              Quote is read-only. Submitting the swap requires a separate{" "}
              <code>submit_jupiter_swap</code> tool dispatch and human approval.
            </div>
          </div>
        )}
        {status === "policy_blocked" && (
          <Alert>
            <AlertTitle>Quote blocked by policy</AlertTitle>
            <AlertDescription className="space-y-1">
              <span className="block">{data.error ?? "Policy rejected this quote."}</span>
              {data.policy_rule_name && (
                <span className="block text-xs text-muted-foreground">
                  rule: <code>{data.policy_rule_name}</code>
                </span>
              )}
            </AlertDescription>
          </Alert>
        )}
        {status === "quote_unavailable" && (
          <Alert>
            <AlertTitle>Jupiter has no route for this pair</AlertTitle>
            <AlertDescription className="break-words">
              {data.error ?? "The aggregator returned no route. Try a different amount or pair."}
            </AlertDescription>
          </Alert>
        )}
        {status === "provider_error" && (
          <Alert variant="destructive">
            <AlertTitle>Jupiter provider error</AlertTitle>
            <AlertDescription className="break-words">
              {data.error ?? "Upstream provider returned an error."}
            </AlertDescription>
          </Alert>
        )}
        <RawOutputDetails output={output} />
      </CardContent>
    </Card>
  );
}

// ── solend_deposit_usdc ─────────────────────────────────────────────────────
//
// Backend output_schema (crates/gateway/src/tools/solend_deposit.rs):
//   status:               "invalid_amount" | "no_session_binding"
//                       | "assembly_failed" | "policy_blocked"
//                       | "awaiting_approval"
//   intent_id, protocol, asset
//   amount_raw            integer | null
//   reserve_mint, session_wallet
//   policy_verdict, hard_block_reason
//   assembly_error        object | null
//   approval_required     boolean | null
//   approval_request_id   string | null  (UUID, "00..00" sentinel in showcase)
//   reason                string | null

interface SolendDepositData {
  status?:
    | "invalid_amount"
    | "no_session_binding"
    | "assembly_failed"
    | "policy_blocked"
    | "awaiting_approval";
  intent_id?: string | null;
  protocol?: string | null;
  asset?: string | null;
  amount_raw?: number | null;
  reserve_mint?: string | null;
  session_wallet?: string | null;
  policy_verdict?: string | null;
  hard_block_reason?: string | null;
  approval_required?: boolean | null;
  approval_request_id?: string | null;
  reason?: string | null;
}

const SHOWCASE_APPROVAL_SENTINEL = "00000000-0000-0000-0000-000000000000";

export function SolendDepositCard({ output }: { output: unknown }) {
  const data = (output as { data?: SolendDepositData })?.data ?? {};
  const status = data.status;
  const variant =
    status === "awaiting_approval"
      ? "default"
      : status === "policy_blocked"
        ? "outline"
        : "destructive";

  const approvalId = data.approval_request_id ?? null;
  const isShowcase = approvalId === SHOWCASE_APPROVAL_SENTINEL;

  return (
    <Card className="border-foreground/15" data-testid="card-solend-deposit">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Tool call: <code>solend_deposit_usdc</code>
          </CardTitle>
          {status && (
            <Badge variant={variant} className="text-xs">
              {status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {status === "awaiting_approval" && (
          <div className="space-y-3 text-xs">
            <div className="space-y-1">
              <KeyValueRow
                k="protocol"
                v={<span className="font-mono text-foreground">{data.protocol ?? "Solend"}</span>}
              />
              <KeyValueRow
                k="asset"
                v={<span className="font-mono text-foreground">{data.asset ?? "USDC"}</span>}
              />
              <KeyValueRow
                k="amount"
                v={
                  <span>
                    <span className="font-mono text-foreground">
                      {typeof data.amount_raw === "number"
                        ? (data.amount_raw / 1_000_000).toFixed(6)
                        : "—"}
                    </span>
                    <span className="ml-2 text-muted-foreground">
                      ({typeof data.amount_raw === "number"
                        ? `${data.amount_raw.toLocaleString()} raw`
                        : "—"})
                    </span>
                  </span>
                }
              />
              {data.session_wallet && (
                <KeyValueRow
                  k="session wallet"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.session_wallet, 6)}
                    </code>
                  }
                />
              )}
            </div>
            <Separator />
            {approvalId && !isShowcase && (
              <div className="flex items-center justify-between">
                <div className="text-muted-foreground">
                  Approval request{" "}
                  <code className="text-xs text-foreground">{shortPubkey(approvalId, 6)}</code>
                </div>
                <Link href={`/approval/${approvalId}`}>
                  <Button size="sm">Review &amp; Approve →</Button>
                </Link>
              </div>
            )}
            {isShowcase && (
              <div className="text-muted-foreground italic">
                Showcase fixture — no real approval was created. In live mode this card links
                to the operator approval page.
              </div>
            )}
            <div className="text-[11px] text-muted-foreground italic">
              The LLM proposed only — no transaction has been built or signed. Approval is the
              first irreversible step.
            </div>
          </div>
        )}
        {status === "policy_blocked" && (
          <Alert>
            <AlertTitle>Policy hard-blocked the proposal</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <span className="block">{data.reason ?? data.hard_block_reason ?? "Blocked."}</span>
              {data.policy_verdict && (
                <span className="block text-xs text-muted-foreground">
                  verdict: <code>{data.policy_verdict}</code>
                </span>
              )}
            </AlertDescription>
          </Alert>
        )}
        {status === "no_session_binding" && (
          <Alert>
            <AlertTitle>No wallet bound to this session</AlertTitle>
            <AlertDescription>
              {data.reason ??
                "Bind a wallet via the wallet-bind challenge on /chat before proposing a Solend deposit."}
            </AlertDescription>
          </Alert>
        )}
        {status === "invalid_amount" && (
          <Alert variant="destructive">
            <AlertTitle>Invalid amount</AlertTitle>
            <AlertDescription className="break-words">
              {data.reason ?? "Amount failed structural guardrails."}
            </AlertDescription>
          </Alert>
        )}
        {status === "assembly_failed" && (
          <Alert variant="destructive">
            <AlertTitle>Snapshot assembly failed</AlertTitle>
            <AlertDescription className="break-words">
              {data.reason ?? "Read-only Solend snapshot could not be assembled."}
            </AlertDescription>
          </Alert>
        )}
        <RawOutputDetails output={output} />
      </CardContent>
    </Card>
  );
}

// ── Generic fallback (unknown tool name) ───────────────────────────────────

export function GenericToolCard({
  toolName,
  output,
}: {
  toolName: string;
  output: unknown;
}) {
  const data = (output as { data?: { status?: string } })?.data;
  const status = data?.status;

  return (
    <Card className="border-foreground/15" data-testid="card-generic-tool">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Tool call: <code>{toolName}</code>
          </CardTitle>
          {status && (
            <Badge variant="secondary" className="text-xs">
              {status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-xs text-muted-foreground italic">
          No typed renderer for this tool yet. Raw output below.
        </p>
        <RawOutputDetails output={output} defaultOpen />
      </CardContent>
    </Card>
  );
}

// ── Shared building blocks ──────────────────────────────────────────────────

function KeyValueRow({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="text-muted-foreground min-w-[90px]">{k}</span>
      <span className="flex-1">{v}</span>
    </div>
  );
}

function RawOutputDetails({
  output,
  defaultOpen,
}: {
  output: unknown;
  defaultOpen?: boolean;
}) {
  return (
    <details
      className="text-xs text-muted-foreground"
      open={defaultOpen}
      data-testid="raw-output"
    >
      <summary className="cursor-pointer hover:text-foreground">raw output</summary>
      <pre className="mt-2 overflow-x-auto rounded bg-muted px-3 py-2 text-[11px] leading-snug">
        {JSON.stringify(output, null, 2)}
      </pre>
    </details>
  );
}
