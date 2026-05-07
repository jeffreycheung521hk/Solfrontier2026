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
    case "get_solend_position":
      return <SolendPositionCard output={output} />;
    case "preview_solend_withdraw_all":
      return <SolendWithdrawPreviewCard output={output} />;
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
//   amount_ui             string  | null   (UI-formatted, e.g. "5.000000")
//   reserve_mint, session_wallet
//   policy_verdict, hard_block_reason
//   approval_required     boolean | null
//   approval_request_id   string | null  (UUID, "00..00" sentinel in showcase)
//   reason                string | null
//
// Phase 6G — risk-budget profile fields (present on both awaiting_approval
// and policy_blocked outputs; absent on legacy outputs):
//   profile_name             string | null   ("demo", "rehearsal", …)
//   policy_rule_name         string | null   ("solend-deposit-risk-budget")
//   requested_amount_raw     integer | null
//   requested_amount_ui      string  | null
//   max_allowed_amount_raw   integer | null
//   max_allowed_amount_ui    string  | null
//   risk_budget_note         string | null   (awaiting_approval only)
//   human_readable_reason    string | null   (policy_blocked only)

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
  amount_ui?: string | null;
  reserve_mint?: string | null;
  session_wallet?: string | null;
  policy_verdict?: string | null;
  hard_block_reason?: string | null;
  approval_required?: boolean | null;
  approval_request_id?: string | null;
  reason?: string | null;
  // Phase 6G risk-budget fields. All optional so legacy outputs render
  // cleanly without these — the UI falls back to amount_ui / amount_raw.
  profile_name?: string | null;
  policy_rule_name?: string | null;
  requested_amount_raw?: number | null;
  requested_amount_ui?: string | null;
  max_allowed_amount_raw?: number | null;
  max_allowed_amount_ui?: string | null;
  risk_budget_note?: string | null;
  human_readable_reason?: string | null;
}

const SHOWCASE_APPROVAL_SENTINEL = "00000000-0000-0000-0000-000000000000";

/// Prefer the backend-provided UI string; fall back to raw / 1e6 for
/// legacy outputs that didn't include `*_amount_ui` fields. Returns
/// the raw display string only — caller appends "USDC" / etc.
function formatUsdcUi(
  uiField: string | null | undefined,
  rawField: number | null | undefined,
): string {
  if (typeof uiField === "string" && uiField.length > 0) return uiField;
  if (typeof rawField === "number") return (rawField / 1_000_000).toFixed(6);
  return "—";
}

function formatRawCount(raw: number | null | undefined): string {
  return typeof raw === "number" ? `${raw.toLocaleString()} raw` : "—";
}

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
                k="requested"
                v={
                  <span>
                    <span className="font-mono text-foreground">
                      {formatUsdcUi(
                        data.requested_amount_ui ?? data.amount_ui,
                        data.requested_amount_raw ?? data.amount_raw,
                      )}{" "}
                      {data.asset ?? "USDC"}
                    </span>
                    <span className="ml-2 text-muted-foreground">
                      ({formatRawCount(data.requested_amount_raw ?? data.amount_raw)})
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

            {/* Phase 6G — risk-budget context. Renders only when the
                backend included risk-budget fields (i.e., post-6G
                outputs). Legacy outputs without these fields skip this
                block silently for backward compatibility. */}
            {(data.profile_name || data.max_allowed_amount_raw != null) && (
              <>
                <Separator />
                <div className="space-y-1">
                  <div className="text-foreground/80 font-medium">Risk budget</div>
                  {data.profile_name && (
                    <KeyValueRow
                      k="profile"
                      v={<span className="font-mono text-foreground">{data.profile_name}</span>}
                    />
                  )}
                  <KeyValueRow
                    k="max allowed"
                    v={
                      <span>
                        <span className="font-mono text-foreground">
                          {formatUsdcUi(data.max_allowed_amount_ui, data.max_allowed_amount_raw)}{" "}
                          {data.asset ?? "USDC"}
                        </span>
                        <span className="ml-2 text-muted-foreground">
                          ({formatRawCount(data.max_allowed_amount_raw)})
                        </span>
                      </span>
                    }
                  />
                  {data.policy_rule_name && (
                    <KeyValueRow
                      k="rule"
                      v={
                        <code className="text-foreground">{data.policy_rule_name}</code>
                      }
                    />
                  )}
                  <div className="text-[11px] text-muted-foreground italic pt-1">
                    This profile&apos;s risk budget allows this proposal. Approval and
                    Phantom signing are still required.
                  </div>
                </div>
              </>
            )}

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
          <div className="space-y-3 text-xs">
            <Alert variant="destructive">
              <AlertTitle>Blocked by risk-budget policy</AlertTitle>
              <AlertDescription className="break-words">
                {data.human_readable_reason ??
                  data.reason ??
                  data.hard_block_reason ??
                  "Request blocked by policy."}
              </AlertDescription>
            </Alert>
            {/* Phase 6G — show the cap that fired. Renders only when
                the backend included risk-budget fields; older outputs
                (no profile_name) get the alert above and the verdict
                line in the fallback block below. */}
            {(data.profile_name ||
              data.max_allowed_amount_raw != null ||
              data.requested_amount_raw != null) && (
              <div className="space-y-1">
                {data.profile_name && (
                  <KeyValueRow
                    k="profile"
                    v={<span className="font-mono text-foreground">{data.profile_name}</span>}
                  />
                )}
                <KeyValueRow
                  k="requested"
                  v={
                    <span>
                      <span className="font-mono text-foreground">
                        {formatUsdcUi(
                          data.requested_amount_ui ?? data.amount_ui,
                          data.requested_amount_raw ?? data.amount_raw,
                        )}{" "}
                        {data.asset ?? "USDC"}
                      </span>
                      <span className="ml-2 text-muted-foreground">
                        ({formatRawCount(data.requested_amount_raw ?? data.amount_raw)})
                      </span>
                    </span>
                  }
                />
                <KeyValueRow
                  k="max allowed"
                  v={
                    <span>
                      <span className="font-mono text-foreground">
                        {formatUsdcUi(data.max_allowed_amount_ui, data.max_allowed_amount_raw)}{" "}
                        {data.asset ?? "USDC"}
                      </span>
                      <span className="ml-2 text-muted-foreground">
                        ({formatRawCount(data.max_allowed_amount_raw)})
                      </span>
                    </span>
                  }
                />
                {data.policy_rule_name && (
                  <KeyValueRow
                    k="rule"
                    v={<code className="text-foreground">{data.policy_rule_name}</code>}
                  />
                )}
                {data.policy_verdict && (
                  <KeyValueRow
                    k="verdict"
                    v={<code className="text-foreground">{data.policy_verdict}</code>}
                  />
                )}
              </div>
            )}
            {/* Legacy fallback: pre-6G outputs only carry policy_verdict +
                hard_block_reason. The alert above already shows the
                reason; surface the verdict here so the card isn't bare. */}
            {!data.profile_name &&
              data.max_allowed_amount_raw == null &&
              data.policy_verdict && (
                <div className="text-muted-foreground">
                  verdict: <code className="text-foreground">{data.policy_verdict}</code>
                </div>
              )}
          </div>
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

// ── get_solend_position ─────────────────────────────────────────────────────
//
// Phase 6H — read-only scanner for the session wallet's Solend / Save USDC
// obligation(s). No signing, no withdraw, no approval; this card is purely
// informational. See `crates/gateway/src/tools/get_solend_position.rs`.
//
// Statuses (from STATUS_* constants in the backend):
//   - "ok"               position(s) found and decoded successfully
//   - "no_position"      wallet bound, no obligations on chain
//   - "wallet_not_bound" no wallet bound to this session
//   - "rpc_error"        RPC failure during obligation scan
//   - "decode_error"     obligation account bytes failed to decode
//
// Per-position shape (from `position_entry` / `borrow_entry`):
//   - kind                              "deposit" | "borrow"
//   - obligation_pubkey, owner_pubkey, lending_market, reserve_pubkey
//   - is_usdc_main_pool_reserve         bool
//   - deposited_collateral_amount_raw   string (u64) | null  (deposits)
//   - supplied_usdc_estimate_raw        null  (cToken→USDC conversion deferred)
//   - supplied_usdc_estimate_ui         null
//   - estimate_unavailable_reason       string | null
//   - borrowed_amount_raw               string (u128 wad-scaled) | null  (borrows)
//   - borrowed_amount_ui                null
//   - has_borrow                        bool
//   - source                            "obligation_scan"
//
// IMPORTANT (per Phase 6H prompt): this card MUST NOT
//   - link to /approval for any position row
//   - call any withdraw API
//   - render a clickable withdraw button (the muted "Withdraw preview
//     coming in Phase 6I" line is intentionally a non-interactive note)

interface SolendPositionEntry {
  kind?: "deposit" | "borrow" | string;
  obligation_pubkey?: string | null;
  owner_pubkey?: string | null;
  lending_market?: string | null;
  reserve_pubkey?: string | null;
  is_usdc_main_pool_reserve?: boolean | null;
  deposited_collateral_amount_raw?: string | null;
  supplied_usdc_estimate_raw?: string | number | null;
  supplied_usdc_estimate_ui?: string | null;
  estimate_unavailable_reason?: string | null;
  borrowed_amount_raw?: string | null;
  borrowed_amount_ui?: string | null;
  has_borrow?: boolean | null;
  source?: string | null;
}

interface SolendPositionData {
  status?: "ok" | "no_position" | "wallet_not_bound" | "rpc_error" | "decode_error";
  wallet_pubkey?: string | null;
  network?: string | null;
  protocol?: string | null;
  program_id?: string | null;
  lending_market?: string | null;
  usdc_main_pool_reserve?: string | null;
  usdc_main_pool_mint?: string | null;
  obligation_count?: number | null;
  usdc_deposit_position_count?: number | null;
  positions?: SolendPositionEntry[] | null;
  decode_warnings?: string[] | null;
  dashboard_visibility_note?: string | null;
  reason?: string | null;
  phase?: string | null;
}

export function SolendPositionCard({ output }: { output: unknown }) {
  const data = (output as { data?: SolendPositionData })?.data ?? {};
  const status = data.status;
  const variant =
    status === "ok"
      ? "default"
      : status === "no_position" || status === "wallet_not_bound"
        ? "outline"
        : "destructive";

  const positions = Array.isArray(data.positions) ? data.positions : [];

  return (
    <Card className="border-foreground/15" data-testid="card-solend-position">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Solend Position
          </CardTitle>
          {status && (
            <Badge variant={variant} className="text-xs">
              {status === "ok" ? "found" : status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {status === "ok" && (
          <div className="space-y-3 text-xs">
            <div className="text-foreground/90">
              Found{" "}
              <span className="font-mono text-foreground">
                {data.usdc_deposit_position_count ?? positions.filter((p) => p.kind === "deposit").length}
              </span>{" "}
              Solend / Save USDC position(s) owned by this wallet.
            </div>

            <div className="space-y-1">
              {data.wallet_pubkey && (
                <KeyValueRow
                  k="wallet"
                  v={
                    <code className="text-foreground">{shortPubkey(data.wallet_pubkey, 6)}</code>
                  }
                />
              )}
              <KeyValueRow
                k="protocol"
                v={
                  <span className="font-mono text-foreground">
                    {data.protocol ?? "Solend/Save"}
                  </span>
                }
              />
              <KeyValueRow
                k="network"
                v={
                  <span className="font-mono text-foreground">
                    {data.network ?? "mainnet"}
                  </span>
                }
              />
              {data.lending_market && (
                <KeyValueRow
                  k="lending market"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.lending_market, 6)}
                    </code>
                  }
                />
              )}
              {data.usdc_main_pool_reserve && (
                <KeyValueRow
                  k="USDC reserve"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.usdc_main_pool_reserve, 6)}
                    </code>
                  }
                />
              )}
              {typeof data.obligation_count === "number" && (
                <KeyValueRow
                  k="obligations"
                  v={
                    <span className="font-mono text-foreground">
                      {data.obligation_count}
                    </span>
                  }
                />
              )}
              {typeof data.usdc_deposit_position_count === "number" && (
                <KeyValueRow
                  k="USDC deposits"
                  v={
                    <span className="font-mono text-foreground">
                      {data.usdc_deposit_position_count}
                    </span>
                  }
                />
              )}
            </div>

            <Separator />

            <div className="space-y-1">
              <div className="text-foreground/80 font-medium">Custody</div>
              <p className="text-[11px] text-muted-foreground">
                Owner field matches your bound wallet. Withdraw will require your
                Phantom signature.
              </p>
            </div>

            {data.dashboard_visibility_note && (
              <Alert className="border-amber-500/40">
                <AlertTitle className="text-xs">Dashboard visibility note</AlertTitle>
                <AlertDescription className="text-[11px] break-words">
                  Solend dashboard may not show these positions because it may use
                  different obligation discovery rules. (Discovery / indexing only — not
                  a custody transfer.)
                </AlertDescription>
              </Alert>
            )}

            {positions.length > 0 && (
              <>
                <Separator />
                <div className="space-y-2">
                  <div className="text-foreground/80 font-medium">Positions</div>
                  <ul className="space-y-2">
                    {positions.map((p, i) => (
                      <li key={i}>
                        <PositionRow position={p} />
                      </li>
                    ))}
                  </ul>
                </div>
              </>
            )}

            {Array.isArray(data.decode_warnings) && data.decode_warnings.length > 0 && (
              <Alert className="border-amber-500/40">
                <AlertTitle className="text-xs">Decode warnings</AlertTitle>
                <AlertDescription className="text-[11px] space-y-1">
                  {data.decode_warnings.map((w, i) => (
                    <div key={i} className="break-words font-mono">{w}</div>
                  ))}
                </AlertDescription>
              </Alert>
            )}

            <Separator />
            {/*
              Phase 6H scope explicitly excludes withdraw execution. This
              line is a non-interactive placeholder ONLY — no onClick, no
              <Button>, no Link, no withdraw API call. Surfacing the
              roadmap signal without implying any action available now.
            */}
            <div
              className="text-[11px] text-muted-foreground italic"
              data-testid="withdraw-coming-soon"
            >
              Withdraw preview coming in Phase 6I.
            </div>
          </div>
        )}

        {status === "no_position" && (
          <div className="space-y-2 text-xs">
            <Alert>
              <AlertTitle>No Solend / Save USDC position found</AlertTitle>
              <AlertDescription>
                No obligations were discovered on chain for this bound wallet. If you
                expected positions here, double-check that the wallet currently bound
                to this session is the same wallet that holds the Solend deposit.
              </AlertDescription>
            </Alert>
            {data.wallet_pubkey && (
              <KeyValueRow
                k="bound wallet"
                v={<code className="text-foreground">{shortPubkey(data.wallet_pubkey, 6)}</code>}
              />
            )}
          </div>
        )}

        {status === "wallet_not_bound" && (
          <Alert>
            <AlertTitle>No wallet bound to this session</AlertTitle>
            <AlertDescription>
              {data.reason ??
                "Bind a wallet via the wallet-bind challenge on /chat before scanning Solend positions."}
            </AlertDescription>
          </Alert>
        )}

        {status === "rpc_error" && (
          <Alert variant="destructive">
            <AlertTitle>RPC error scanning Solend</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>{data.reason ?? "Upstream RPC failed."}</div>
              {data.phase && (
                <div className="text-[11px] text-muted-foreground">
                  phase: <code>{data.phase}</code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "decode_error" && (
          <Alert variant="destructive">
            <AlertTitle>Obligation decode failed</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>{data.reason ?? "Obligation account bytes failed to decode."}</div>
              {Array.isArray(data.decode_warnings) && data.decode_warnings.length > 0 && (
                <ul className="text-[11px] font-mono space-y-0.5 mt-1">
                  {data.decode_warnings.map((w, i) => (
                    <li key={i} className="break-words">{w}</li>
                  ))}
                </ul>
              )}
            </AlertDescription>
          </Alert>
        )}

        <RawOutputDetails output={output} />
      </CardContent>
    </Card>
  );
}

function PositionRow({ position }: { position: SolendPositionEntry }) {
  const isBorrow = position.kind === "borrow" || position.has_borrow === true;
  const isDeposit = position.kind === "deposit";

  return (
    <div
      className={`rounded-md border px-3 py-2 space-y-1 ${
        isBorrow ? "border-amber-500/50 bg-amber-500/5" : "border-border"
      }`}
      data-testid={`position-row-${position.kind ?? "unknown"}`}
    >
      <div className="flex items-center justify-between text-[11px]">
        <span className="uppercase tracking-wide text-muted-foreground">
          {position.kind ?? "position"}
          {position.is_usdc_main_pool_reserve && (
            <span className="ml-2 text-foreground">· USDC main pool</span>
          )}
        </span>
        {isBorrow && (
          <Badge variant="outline" className="text-[10px] border-amber-500/60">
            has borrow
          </Badge>
        )}
      </div>

      {position.obligation_pubkey && (
        <KeyValueRow
          k="obligation"
          v={
            <code className="text-foreground">
              {shortPubkey(position.obligation_pubkey, 6)}
            </code>
          }
        />
      )}
      {position.owner_pubkey && (
        <KeyValueRow
          k="owner"
          v={
            <code className="text-foreground">{shortPubkey(position.owner_pubkey, 6)}</code>
          }
        />
      )}
      {position.reserve_pubkey && (
        <KeyValueRow
          k="reserve"
          v={
            <code className="text-muted-foreground">
              {shortPubkey(position.reserve_pubkey, 6)}
            </code>
          }
        />
      )}

      {isDeposit && position.deposited_collateral_amount_raw != null && (
        <KeyValueRow
          k="cToken raw"
          v={
            <span className="font-mono text-foreground">
              {position.deposited_collateral_amount_raw}
            </span>
          }
        />
      )}

      {isDeposit && (
        <KeyValueRow
          k="supplied USDC"
          v={
            position.supplied_usdc_estimate_ui ? (
              <span className="font-mono text-foreground">
                {position.supplied_usdc_estimate_ui} USDC
              </span>
            ) : (
              <span
                className="text-muted-foreground italic"
                title={position.estimate_unavailable_reason ?? undefined}
              >
                USDC estimate unavailable
                {position.estimate_unavailable_reason && (
                  <span className="ml-1 text-[10px]">
                    (hover for reason)
                  </span>
                )}
              </span>
            )
          }
        />
      )}

      {isBorrow && position.borrowed_amount_raw != null && (
        <KeyValueRow
          k="borrowed (wad)"
          v={
            <span className="font-mono text-foreground break-all">
              {position.borrowed_amount_raw}
            </span>
          }
        />
      )}
    </div>
  );
}

// ── preview_solend_withdraw_all ─────────────────────────────────────────────
//
// Phase 6I-C — read-only preview card for withdraw-all on the session
// wallet's Solend / Save USDC obligation. PURELY informational.
//
// IMPORTANT (per Phase 6I-C prompt): this card MUST NOT
//   - render an active withdraw button (no <Button> with onClick)
//   - call any API (no fetch, no signTransaction, no signMessage,
//     no submit, no broadcast)
//   - link to /approval (no Review & Approve)
//   - render a Review & Approve flow
//
// The single "Withdraw execution coming in Phase 6I-D" line is a
// non-interactive plain <div>, not a Button — surfacing the roadmap
// without implying an action available now.
//
// Backend statuses (`crates/gateway/src/tools/preview_solend_withdraw_all.rs`):
//   - "ok"                          preview computed successfully
//   - "wallet_not_bound"            no wallet bound to this session
//   - "invalid_obligation_pubkey"   pubkey arg failed to parse
//   - "obligation_not_found"        on-chain account doesn't exist
//   - "owner_mismatch"              obligation belongs to a different wallet
//   - "no_usdc_deposit"             obligation has no USDC main-pool deposit
//   - "unsafe_to_withdraw_all"      obligation has borrows; withdraw-all blocked
//   - "rpc_error"                   RPC call failed during scan
//   - "decode_error"                obligation bytes failed to decode

interface SolendWithdrawPreviewData {
  status?:
    | "ok"
    | "wallet_not_bound"
    | "invalid_obligation_pubkey"
    | "obligation_not_found"
    | "owner_mismatch"
    | "no_usdc_deposit"
    | "unsafe_to_withdraw_all"
    | "rpc_error"
    | "decode_error";
  mode?: string | null;
  protocol?: string | null;
  network?: string | null;
  program_id?: string | null;
  wallet_pubkey?: string | null;
  obligation_pubkey?: string | null;
  lending_market?: string | null;
  reserve_pubkey?: string | null;
  reserve_mint?: string | null;
  collateral_amount_raw?: string | number | null;
  underlying_usdc_estimate_raw?: string | number | null;
  underlying_usdc_estimate_ui?: string | null;
  estimate_unavailable_reason?: string | null;
  requires_user_signature?: boolean | null;
  required_signers?: string[] | null;
  requires_obligation_keypair?: boolean | null;
  will_create_approval?: boolean | null;
  will_sign?: boolean | null;
  will_broadcast?: boolean | null;
  next_step?: string | null;
  reason?: string | null;
  phase?: string | null;
  decoded_owner?: string | null;
  borrow_entry_count?: number | null;
}

export function SolendWithdrawPreviewCard({ output }: { output: unknown }) {
  const data = (output as { data?: SolendWithdrawPreviewData })?.data ?? {};
  const status = data.status;
  const variant =
    status === "ok"
      ? "default"
      : status === "wallet_not_bound" ||
          status === "invalid_obligation_pubkey" ||
          status === "obligation_not_found" ||
          status === "no_usdc_deposit"
        ? "outline"
        : "destructive";

  return (
    <Card className="border-foreground/15" data-testid="card-solend-withdraw-preview">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Solend Withdraw Preview
          </CardTitle>
          {status && (
            <Badge variant={variant} className="text-xs">
              {status === "ok" ? "preview only" : status.replace(/_/g, " ")}
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {status === "ok" && (
          <div className="space-y-3 text-xs">
            <div className="text-foreground/90">
              This position can be previewed for withdraw-all. No approval, signing, or
              broadcast has happened.
            </div>

            <div className="space-y-1">
              <KeyValueRow
                k="protocol"
                v={
                  <span className="font-mono text-foreground">
                    {data.protocol ?? "Solend/Save"}
                  </span>
                }
              />
              <KeyValueRow
                k="network"
                v={
                  <span className="font-mono text-foreground">
                    {data.network ?? "mainnet"}
                  </span>
                }
              />
              <KeyValueRow
                k="mode"
                v={
                  <span className="font-mono text-foreground">
                    {data.mode ?? "withdraw_all_collateral"}
                  </span>
                }
              />
              {data.wallet_pubkey && (
                <KeyValueRow
                  k="wallet"
                  v={
                    <code className="text-foreground">
                      {shortPubkey(data.wallet_pubkey, 6)}
                    </code>
                  }
                />
              )}
              {data.obligation_pubkey && (
                <KeyValueRow
                  k="obligation"
                  v={
                    <code className="text-foreground">
                      {shortPubkey(data.obligation_pubkey, 6)}
                    </code>
                  }
                />
              )}
              {data.lending_market && (
                <KeyValueRow
                  k="lending market"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.lending_market, 6)}
                    </code>
                  }
                />
              )}
              {data.reserve_pubkey && (
                <KeyValueRow
                  k="reserve"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.reserve_pubkey, 6)}
                    </code>
                  }
                />
              )}
              {data.reserve_mint && (
                <KeyValueRow
                  k="reserve mint"
                  v={
                    <code className="text-muted-foreground">
                      {shortPubkey(data.reserve_mint, 6)}
                    </code>
                  }
                />
              )}
              {data.collateral_amount_raw != null && (
                <KeyValueRow
                  k="cToken raw"
                  v={
                    <span className="font-mono text-foreground break-all">
                      {String(data.collateral_amount_raw)}
                    </span>
                  }
                />
              )}
            </div>

            <Separator />

            <div className="space-y-1">
              <div className="text-foreground/80 font-medium">Underlying USDC estimate</div>
              {data.underlying_usdc_estimate_ui ? (
                <div className="font-mono text-foreground">
                  {data.underlying_usdc_estimate_ui} USDC
                </div>
              ) : (
                <div
                  className="text-muted-foreground italic"
                  title={data.estimate_unavailable_reason ?? undefined}
                >
                  USDC estimate unavailable
                  {data.estimate_unavailable_reason && (
                    <span className="ml-1 text-[10px]">(hover for reason)</span>
                  )}
                </div>
              )}
            </div>

            <Separator />

            <div className="space-y-1">
              <div className="text-foreground/80 font-medium">Signing requirements</div>
              <KeyValueRow
                k="requires user signature"
                v={
                  <span className="font-mono text-foreground">
                    {data.requires_user_signature === true ? "yes" : data.requires_user_signature === false ? "no" : "—"}
                  </span>
                }
              />
              <KeyValueRow
                k="required signers"
                v={
                  <span className="font-mono text-foreground">
                    {Array.isArray(data.required_signers) && data.required_signers.length > 0
                      ? data.required_signers.join(", ")
                      : "—"}
                  </span>
                }
              />
              <KeyValueRow
                k="requires obligation keypair"
                v={
                  <span className="font-mono text-foreground">
                    {data.requires_obligation_keypair === true ? "yes" : data.requires_obligation_keypair === false ? "no" : "—"}
                  </span>
                }
              />
            </div>

            <Separator />

            <div className="space-y-1">
              <div className="text-foreground/80 font-medium">Safety checklist</div>
              <SafetyFlagRow label="will create approval" value={data.will_create_approval} />
              <SafetyFlagRow label="will sign" value={data.will_sign} />
              <SafetyFlagRow label="will broadcast" value={data.will_broadcast} />
            </div>

            {data.next_step && (
              <>
                <Separator />
                <p className="text-[11px] text-muted-foreground italic break-words">
                  {data.next_step}
                </p>
              </>
            )}

            {/*
              Phase 6I-C scope explicitly excludes withdraw execution.
              This line is a non-interactive plain <div> — no <Button>,
              no <Link>, no onClick, no fetch. Surfacing the roadmap
              signal without implying any action available now.
            */}
            <div
              className="text-[11px] text-muted-foreground italic"
              data-testid="withdraw-execution-coming-soon"
            >
              Withdraw execution coming in Phase 6I-D.
            </div>
          </div>
        )}

        {status === "wallet_not_bound" && (
          <Alert>
            <AlertTitle>No wallet bound to this session</AlertTitle>
            <AlertDescription>
              {data.reason ??
                "Bind a wallet via the wallet-bind challenge on /chat before previewing Solend withdraws."}
            </AlertDescription>
          </Alert>
        )}

        {status === "invalid_obligation_pubkey" && (
          <Alert>
            <AlertTitle>Invalid obligation pubkey</AlertTitle>
            <AlertDescription className="break-words">
              {data.reason ??
                "The obligation pubkey supplied is not a valid base58 Solana address."}
            </AlertDescription>
          </Alert>
        )}

        {status === "obligation_not_found" && (
          <Alert>
            <AlertTitle>Obligation not found on chain</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>{data.reason ?? "Obligation account does not exist on chain."}</div>
              {data.obligation_pubkey && (
                <div className="text-[11px] text-muted-foreground">
                  pubkey:{" "}
                  <code className="text-foreground">
                    {shortPubkey(data.obligation_pubkey, 6)}
                  </code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "owner_mismatch" && (
          <Alert variant="destructive">
            <AlertTitle>Obligation owner does not match this wallet</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>
                {data.reason ??
                  "This obligation is not owned by the bound wallet. Preview blocked to prevent acting on someone else's position."}
              </div>
              {data.wallet_pubkey && (
                <div className="text-[11px] text-muted-foreground">
                  bound wallet:{" "}
                  <code className="text-foreground">{shortPubkey(data.wallet_pubkey, 6)}</code>
                </div>
              )}
              {data.decoded_owner && (
                <div className="text-[11px] text-muted-foreground">
                  obligation owner:{" "}
                  <code className="text-foreground">{shortPubkey(data.decoded_owner, 6)}</code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "no_usdc_deposit" && (
          <Alert>
            <AlertTitle>No Solend Main Pool USDC deposit found</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>
                {data.reason ??
                  "Obligation has no non-zero deposit entry for the Solend / Save Main Pool USDC reserve; nothing to withdraw via this preview."}
              </div>
              {data.obligation_pubkey && (
                <div className="text-[11px] text-muted-foreground">
                  obligation:{" "}
                  <code className="text-foreground">
                    {shortPubkey(data.obligation_pubkey, 6)}
                  </code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "unsafe_to_withdraw_all" && (
          <Alert variant="destructive">
            <AlertTitle>Unsafe to withdraw all collateral</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>
                {data.reason ??
                  "Withdraw-all preview blocked because this obligation has borrow entries. Withdrawing all collateral here would risk health-factor / liquidation."}
              </div>
              {typeof data.borrow_entry_count === "number" && (
                <div className="text-[11px] text-muted-foreground">
                  borrow entries:{" "}
                  <span className="font-mono text-foreground">{data.borrow_entry_count}</span>
                </div>
              )}
              {data.obligation_pubkey && (
                <div className="text-[11px] text-muted-foreground">
                  obligation:{" "}
                  <code className="text-foreground">
                    {shortPubkey(data.obligation_pubkey, 6)}
                  </code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "rpc_error" && (
          <Alert variant="destructive">
            <AlertTitle>RPC error during withdraw preview</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>{data.reason ?? "Upstream RPC call failed."}</div>
              {data.phase && (
                <div className="text-[11px] text-muted-foreground">
                  phase: <code>{data.phase}</code>
                </div>
              )}
            </AlertDescription>
          </Alert>
        )}

        {status === "decode_error" && (
          <Alert variant="destructive">
            <AlertTitle>Obligation decode failed</AlertTitle>
            <AlertDescription className="break-words">
              {data.reason ?? "Obligation account bytes failed to decode."}
            </AlertDescription>
          </Alert>
        )}

        <RawOutputDetails output={output} />
      </CardContent>
    </Card>
  );
}

function SafetyFlagRow({
  label,
  value,
}: {
  label: string;
  value: boolean | null | undefined;
}) {
  // The preview tool always asserts these as `false`. We render any
  // unexpected `true` (or missing) value distinctly so a future-proof
  // operator catches a wire-shape change in the audit detail panel.
  const display =
    value === false ? "false" : value === true ? "true" : "—";
  const tone =
    value === false
      ? "text-foreground"
      : value === true
        ? "text-destructive font-semibold"
        : "text-muted-foreground";
  return (
    <KeyValueRow
      k={label}
      v={<span className={`font-mono ${tone}`}>{display}</span>}
    />
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
