"use client";

// Stage 2 — `<Stage2WatchRulePreview>`.
//
// Renders a Stage 2 WatchRule and its locally-computed canonical
// rule hash so the operator can visually verify "this is exactly
// the rule I'm authorizing" before any future Authorization-PDA
// signing flow exists. When the caller supplies an
// `expectedHashHex`, the component re-computes the hash locally
// over the same Borsh bytes and refuses to render a green state on
// mismatch (with a destructive banner).
//
// Stage 2 boundary (mirrors the Rust crate doc):
//
//   - Stage 2 is *delegated conditional execution*, not propose-now.
//   - This preview is *tamper-evident*, not full action enforcement.
//   - This component does NOT trigger any signing / Authorization
//     PDA / executor / watcher flow. There is no "Sign", "Execute",
//     or "Submit" button. Authorization-PDA wiring lives in a
//     downstream Stage 2 slice.

import { useEffect, useMemo, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { shortPubkey } from "@/lib/format";
import { bytesToHex } from "@/lib/canonical-intent";
import {
  canonicalRuleHash,
  type ActionSpec,
  type Comparison,
  type Condition,
  type WatchRule,
} from "@/lib/stage2-watch-rule";

// ── Local hash-verification state machine ─────────────────────────────────

type VerificationState =
  | { kind: "computing" }
  | {
      kind: "computed";
      computed_hash_hex: string;
      expected_hash_hex: string | null;
      matches: boolean | null;
    }
  | { kind: "error"; error: string };

interface Stage2WatchRulePreviewProps {
  rule: WatchRule;
  /** Pinned expected hash. When supplied and the locally-computed
   *  hash differs, the component renders a destructive refusal
   *  banner. */
  expectedHashHex?: string | null;
  /** Optional caller-supplied chain slot for live expiry display. */
  currentSlot?: bigint | null;
}

export function Stage2WatchRulePreview({
  rule,
  expectedHashHex,
  currentSlot,
}: Stage2WatchRulePreviewProps) {
  const [verification, setVerification] = useState<VerificationState>({
    kind: "computing",
  });

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const computed = await canonicalRuleHash(rule);
        if (cancelled) return;
        if (typeof expectedHashHex === "string" && expectedHashHex.length > 0) {
          const expected = expectedHashHex.toLowerCase();
          setVerification({
            kind: "computed",
            computed_hash_hex: computed,
            expected_hash_hex: expected,
            matches: computed === expected,
          });
        } else {
          setVerification({
            kind: "computed",
            computed_hash_hex: computed,
            expected_hash_hex: null,
            matches: null,
          });
        }
      } catch (err) {
        if (cancelled) return;
        setVerification({
          kind: "error",
          error: err instanceof Error ? err.message : "rule hash compute failed",
        });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rule, expectedHashHex]);

  const expiryStatus = useMemo(() => {
    if (currentSlot === null || currentSlot === undefined) return "unknown";
    return currentSlot < rule.expires_at_slot ? "valid" : "expired";
  }, [rule.expires_at_slot, currentSlot]);

  return (
    <Card
      className="border-foreground/15"
      data-testid="stage2-watch-rule-preview"
    >
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-sm font-medium">
            Stage 2 watch rule preview
          </CardTitle>
          <ActionKindBadge kind={rule.action.kind} />
        </div>
      </CardHeader>
      <CardContent className="space-y-3 text-xs">
        {/* Hard mismatch refusal — always first. */}
        {verification.kind === "computed" && verification.matches === false && (
          <Alert variant="destructive" data-testid="stage2-rule-hash-mismatch">
            <AlertTitle>Canonical rule hash mismatch — refusing</AlertTitle>
            <AlertDescription className="space-y-1 break-words">
              <div>
                The hash of the rule rendered here does not match the pinned
                expected hash. This is preview-only; do NOT proceed with any
                future authorization flow on a mismatched rule.
              </div>
              <div>
                computed:{" "}
                <code className="break-all">{verification.computed_hash_hex}</code>
              </div>
              <div>
                expected:{" "}
                <code className="break-all">{verification.expected_hash_hex}</code>
              </div>
            </AlertDescription>
          </Alert>
        )}

        <RuleSummary rule={rule} />

        <Separator />

        <div className="space-y-1">
          <div className="text-foreground/80 font-medium">Canonical fields</div>
          <KeyValueRow
            k="schema version"
            v={
              <span className="font-mono text-foreground">
                {rule.schema_version}
              </span>
            }
          />
          <KeyValueRow
            k="rule id"
            v={
              <code className="font-mono text-foreground break-all">
                {bytesToHex(rule.rule_id)}
              </code>
            }
          />
          <KeyValueRow
            k="user"
            v={<code className="text-foreground">{shortPubkey(rule.user.base58, 6)}</code>}
          />
          <KeyValueRow
            k="executor"
            v={<code className="text-foreground">{shortPubkey(rule.executor.base58, 6)}</code>}
          />
          <KeyValueRow
            k="delegated wallet"
            v={
              <code className="text-foreground">
                {shortPubkey(rule.delegated_wallet.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="created at slot"
            v={
              <span className="font-mono text-foreground">
                {rule.created_at_slot.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="expires at slot"
            v={
              <span className="font-mono text-foreground">
                {rule.expires_at_slot.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="expiry status"
            v={
              expiryStatus === "expired" ? (
                <span className="font-mono text-destructive font-semibold">
                  EXPIRED
                </span>
              ) : expiryStatus === "valid" ? (
                <span className="font-mono text-foreground">valid</span>
              ) : (
                <span className="text-muted-foreground italic">
                  current slot unknown — static expiry only
                </span>
              )
            }
          />
          <KeyValueRow
            k="one shot"
            v={
              <span className="font-mono text-foreground">
                {rule.one_shot ? "yes" : "no"}
              </span>
            }
          />
          <KeyValueRow
            k="condition logic"
            v={
              <span className="font-mono uppercase text-foreground">
                {rule.condition_logic}
              </span>
            }
          />
          <KeyValueRow
            k="max input amount raw"
            v={
              <span className="font-mono text-foreground">
                {rule.max_input_amount_raw.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="used amount raw"
            v={
              <span className="font-mono text-foreground">
                {rule.used_amount_raw.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="destination"
            v={
              <code className="text-foreground">
                {shortPubkey(rule.destination.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="slippage bps"
            v={
              <span className="font-mono text-foreground">
                {rule.slippage_bps} ({(rule.slippage_bps / 100).toFixed(2)}%)
              </span>
            }
          />
        </div>

        <Separator />

        <div className="space-y-2">
          <div className="text-foreground/80 font-medium">
            Conditions ({rule.conditions.length})
          </div>
          <ul className="space-y-2">
            {rule.conditions.map((c, i) => (
              <li key={i}>
                <ConditionRow condition={c} index={i} />
              </li>
            ))}
          </ul>
        </div>

        <Separator />

        <div className="space-y-2">
          <div className="text-foreground/80 font-medium">Action</div>
          <ActionRow action={rule.action} />
        </div>

        <Separator />

        <HashSection verification={verification} />

        <Alert className="border-amber-500/40">
          <AlertTitle className="text-xs">Tamper-evidence only</AlertTitle>
          <AlertDescription className="text-[11px]">
            This is preview-only. Stage 2 substrate at this slice is the
            canonical schema + hash; the Authorization PDA, watcher, and
            on-chain comparator are downstream slices. No signing,
            authorization, or execution happens here.
          </AlertDescription>
        </Alert>
      </CardContent>
    </Card>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function ActionKindBadge({ kind }: { kind: ActionSpec["kind"] }) {
  const label =
    kind === "solend_withdraw_all_delegated"
      ? "solend withdraw-all (delegated)"
      : "jupiter buy SOL with USDC";
  return (
    <Badge variant="default" className="text-xs">
      {label}
    </Badge>
  );
}

function RuleSummary({ rule }: { rule: WatchRule }) {
  const conds = rule.conditions
    .map((c) => describeCondition(c))
    .join(rule.condition_logic === "all" ? " AND " : " OR ");
  const actionDesc = describeAction(rule.action);
  return (
    <p className="text-foreground/90">
      <span className="font-mono">If</span> {conds}{" "}
      <span className="font-mono">then</span> {actionDesc}.
    </p>
  );
}

function describeCondition(c: Condition): string {
  switch (c.kind) {
    case "pyth_price": {
      const human = humanReadablePrice(
        c.threshold_mantissa,
        c.threshold_exponent,
      );
      const op = comparisonSymbol(c.comparison);
      return `Pyth(${shortFeed(c.feed_id)}) ${op} ${human}`;
    }
    case "solend_reserve_supply_rate": {
      const op = comparisonSymbol(c.comparison);
      const pct = (c.threshold_bps / 100).toFixed(2);
      return `Solend ${c.rate_kind.toUpperCase()}(${shortPubkey(
        c.reserve_pubkey.base58,
        4,
      )}) ${op} ${pct}%`;
    }
  }
}

function describeAction(a: ActionSpec): string {
  switch (a.kind) {
    case "solend_withdraw_all_delegated":
      return `withdraw all delegated Solend collateral on obligation ${shortPubkey(
        a.target_obligation.base58,
        4,
      )} → ${shortPubkey(a.destination_wallet.base58, 4)}`;
    case "jupiter_buy_sol_with_usdc":
      return `swap ${a.input_amount_raw.toString()} raw USDC → SOL via Jupiter (${a.jupiter_api_version})`;
  }
}

function comparisonSymbol(c: Comparison): string {
  switch (c) {
    case "lt":
      return "<";
    case "lte":
      return "≤";
    case "gt":
      return ">";
    case "gte":
      return "≥";
  }
}

function humanReadablePrice(mantissa: bigint, exponent: number): string {
  // Render `mantissa * 10^exponent` exactly without floating point.
  if (exponent === 0) return mantissa.toString();
  if (exponent > 0) return `${mantissa.toString()}e${exponent}`;
  // exponent < 0
  const absExp = -exponent;
  const sign = mantissa < BigInt(0) ? "-" : "";
  const absMant = mantissa < BigInt(0) ? -mantissa : mantissa;
  const s = absMant.toString().padStart(absExp + 1, "0");
  const intPart = s.slice(0, -absExp);
  const fracPart = s.slice(-absExp);
  return `${sign}${intPart}.${fracPart}`;
}

function shortFeed(feed: Uint8Array): string {
  const hex = bytesToHex(feed);
  return `${hex.slice(0, 4)}…${hex.slice(-4)}`;
}

function ConditionRow({
  condition,
  index,
}: {
  condition: Condition;
  index: number;
}) {
  return (
    <div
      className="rounded-md border border-border px-3 py-2 space-y-1"
      data-testid={`stage2-condition-${index}`}
    >
      <div className="flex items-center justify-between text-[11px]">
        <span className="uppercase tracking-wide text-muted-foreground">
          {condition.kind === "pyth_price"
            ? `pyth #${index + 1}`
            : `solend supply rate #${index + 1}`}
        </span>
      </div>
      {condition.kind === "pyth_price" && (
        <div className="space-y-0.5">
          <KeyValueRow
            k="feed id"
            v={
              <code className="font-mono text-foreground break-all text-[10px]">
                {bytesToHex(condition.feed_id)}
              </code>
            }
          />
          <KeyValueRow
            k="price update acct"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(condition.price_update_account.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="comparison"
            v={
              <span className="font-mono text-foreground">
                {comparisonSymbol(condition.comparison)} (
                {condition.comparison.toUpperCase()})
              </span>
            }
          />
          <KeyValueRow
            k="threshold"
            v={
              <span>
                <span className="font-mono text-foreground">
                  {humanReadablePrice(
                    condition.threshold_mantissa,
                    condition.threshold_exponent,
                  )}
                </span>
                <span className="ml-2 text-muted-foreground">
                  (mantissa {condition.threshold_mantissa.toString()}, exp{" "}
                  {condition.threshold_exponent})
                </span>
              </span>
            }
          />
          <KeyValueRow
            k="max age (s)"
            v={
              <span className="font-mono text-foreground">
                {condition.max_age_seconds}
              </span>
            }
          />
          <KeyValueRow
            k="max conf (bps)"
            v={
              <span className="font-mono text-foreground">
                {condition.max_confidence_bps}
              </span>
            }
          />
          <KeyValueRow
            k="verification"
            v={
              <span className="font-mono uppercase text-foreground">
                {condition.verification_level_required}
              </span>
            }
          />
          <KeyValueRow
            k="bound mode"
            v={
              <span className="font-mono text-foreground">
                {condition.bound_mode}
              </span>
            }
          />
        </div>
      )}
      {condition.kind === "solend_reserve_supply_rate" && (
        <div className="space-y-0.5">
          <KeyValueRow
            k="reserve"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(condition.reserve_pubkey.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="lending market"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(condition.lending_market.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="solend program"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(condition.solend_program_id.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="rate kind"
            v={
              <span className="font-mono uppercase text-foreground">
                {condition.rate_kind}
              </span>
            }
          />
          <KeyValueRow
            k="formula version"
            v={
              <span className="font-mono text-foreground">
                {condition.formula_version}
              </span>
            }
          />
          <KeyValueRow
            k="threshold"
            v={
              <span>
                <span className="font-mono text-foreground">
                  {comparisonSymbol(condition.comparison)}{" "}
                  {(condition.threshold_bps / 100).toFixed(2)}%
                </span>
                <span className="ml-2 text-muted-foreground">
                  ({condition.threshold_bps} bps)
                </span>
              </span>
            }
          />
          <KeyValueRow
            k="max staleness slots"
            v={
              <span className="font-mono text-foreground">
                {condition.max_reserve_staleness_slots}
              </span>
            }
          />
          <KeyValueRow
            k="refresh same tx"
            v={
              <span className="font-mono text-foreground">
                {condition.required_refresh_same_tx ? "yes" : "no"}
              </span>
            }
          />
        </div>
      )}
    </div>
  );
}

function ActionRow({ action }: { action: ActionSpec }) {
  return (
    <div
      className="rounded-md border border-border px-3 py-2 space-y-1"
      data-testid={`stage2-action-${action.kind}`}
    >
      {action.kind === "solend_withdraw_all_delegated" && (
        <div className="space-y-0.5">
          <KeyValueRow
            k="target obligation"
            v={
              <code className="text-foreground">
                {shortPubkey(action.target_obligation.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="reserve"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(action.reserve_pubkey.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="lending market"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(action.lending_market.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="destination wallet"
            v={
              <code className="text-foreground">
                {shortPubkey(action.destination_wallet.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="withdraw mode"
            v={
              <span className="font-mono text-foreground">
                {action.withdraw_mode}
              </span>
            }
          />
        </div>
      )}
      {action.kind === "jupiter_buy_sol_with_usdc" && (
        <div className="space-y-0.5">
          <KeyValueRow
            k="input mint"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(action.input_mint.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="output mint"
            v={
              <code className="text-muted-foreground">
                {shortPubkey(action.output_mint.base58, 6)}
              </code>
            }
          />
          <KeyValueRow
            k="input amount raw"
            v={
              <span className="font-mono text-foreground">
                {action.input_amount_raw.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="min output (raw)"
            v={
              <span className="font-mono text-foreground">
                {action.min_output_amount_raw === null
                  ? "— (defer to slippage)"
                  : action.min_output_amount_raw.toString()}
              </span>
            }
          />
          <KeyValueRow
            k="jupiter API"
            v={
              <span className="font-mono uppercase text-foreground">
                {action.jupiter_api_version}
              </span>
            }
          />
          <KeyValueRow
            k="max accounts hint"
            v={
              <span className="font-mono text-foreground">
                {action.max_accounts_hint}
              </span>
            }
          />
          <KeyValueRow
            k="pre/post bracket"
            v={
              <span className="font-mono text-foreground">
                {action.require_pre_post_bracket ? "yes" : "no"}
              </span>
            }
          />
        </div>
      )}
    </div>
  );
}

function HashSection({ verification }: { verification: VerificationState }) {
  return (
    <div className="space-y-2">
      <div className="text-foreground/80 font-medium">
        Canonical rule hash
      </div>
      {verification.kind === "computing" && (
        <p className="text-muted-foreground italic">computing SHA-256…</p>
      )}
      {verification.kind === "computed" && (
        <div className="space-y-2">
          <div className="flex items-start gap-2">
            <code
              className="font-mono text-foreground break-all flex-1"
              data-testid="stage2-computed-hash"
            >
              {verification.computed_hash_hex}
            </code>
            <CopyHashButton hashHex={verification.computed_hash_hex} />
          </div>
          {verification.expected_hash_hex && verification.matches === true && (
            <p className="text-[11px] text-foreground/80">
              ✓ Matches the pinned expected hash.
            </p>
          )}
          {verification.expected_hash_hex === null && (
            <p className="text-[11px] text-muted-foreground italic">
              No expected hash supplied — local hash only.
            </p>
          )}
        </div>
      )}
      {verification.kind === "error" && (
        <Alert variant="destructive">
          <AlertTitle>Hash compute error</AlertTitle>
          <AlertDescription className="break-words">
            {verification.error}
          </AlertDescription>
        </Alert>
      )}
    </div>
  );
}

function CopyHashButton({ hashHex }: { hashHex: string }) {
  const [copied, setCopied] = useState(false);
  const onClick = async () => {
    try {
      await navigator.clipboard.writeText(hashHex);
      setCopied(true);
      setTimeout(() => setCopied(false), 1_500);
    } catch {
      // Clipboard API may be unavailable — fail silently.
    }
  };
  return (
    <button
      type="button"
      onClick={onClick}
      className="text-[11px] rounded-md border bg-background px-2 py-1 hover:border-foreground/30 transition-colors"
      data-testid="stage2-copy-hash-button"
    >
      {copied ? "copied" : "copy"}
    </button>
  );
}

function KeyValueRow({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="text-muted-foreground min-w-[140px]">{k}</span>
      <span className="flex-1 break-all">{v}</span>
    </div>
  );
}
