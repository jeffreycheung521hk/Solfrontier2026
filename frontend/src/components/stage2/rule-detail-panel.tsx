"use client";

// Stage 2 — RuleDetailPanel.
//
// Surfaces the selected rule's:
//   - status-specific copy (deliberately careful about
//     "execution simulated" vs "execution successful" — never the
//     latter)
//   - canonical preview (reusing A1's `<Stage2WatchRulePreview>`
//     when the full `WatchRule` is in the dashboard payload)
//   - lifecycle timeline
//   - revoke-rule skeleton (disabled per scope)
//   - dry-run force-tick control

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { Stage2WatchRulePreview } from "@/components/stage2-watch-rule-preview";
import { ForceTickControl } from "@/components/stage2/force-tick-control";
import { RevokeRuleSkeleton } from "@/components/stage2/revoke-rule-skeleton";
import { RuleStatusBadge } from "@/components/stage2/rule-status-badge";
import {
  EXPECTED_FIXTURE_RULE_HASHES,
  type WatchRule,
} from "@/lib/stage2-watch-rule";
import type {
  Stage2RuleLifecycleEvent,
  Stage2WatchRuleStatus,
  Stage2WatchRuleSummary,
} from "@/lib/stage2-watch-api";

interface RuleDetailPanelProps {
  selected: Stage2WatchRuleSummary | null;
  rule: WatchRule | null;
  lifecycle: Stage2RuleLifecycleEvent[];
  nowMs: bigint;
  onDryRunTick: (ruleIdHex: string) => void;
  /** A3: when the dashboard's data source is the read-only API,
   *  the dry-run force tick is hidden entirely. */
  realModeEnabled?: boolean;
}

export function RuleDetailPanel({
  selected,
  rule,
  lifecycle,
  nowMs,
  onDryRunTick,
  realModeEnabled = false,
}: RuleDetailPanelProps) {
  if (selected === null) {
    return (
      <Card data-testid="rule-detail-empty">
        <CardHeader className="pb-3">
          <CardTitle className="text-sm font-medium">Rule detail</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-xs text-muted-foreground">
            Select a rule from the table to view its canonical preview, watcher
            lifecycle, and dry-run controls.
          </p>
        </CardContent>
      </Card>
    );
  }

  // The expected hash is only pinned for the A1 fixture. For mock rules
  // derived from A1 (different rule_id / expires_at_slot), the hash is
  // not pinned — the preview component renders without an
  // `expectedHashHex` and just shows the locally-computed hash.
  const A1_FIXTURE_A_RULE_ID = "52554c45000000000000000000000001";
  const expectedHashHex =
    selected.rule_id_hex === A1_FIXTURE_A_RULE_ID
      ? EXPECTED_FIXTURE_RULE_HASHES.scenario_a_solend_apr_below_10
      : null;

  return (
    <Card data-testid="rule-detail-panel">
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between gap-3 flex-wrap">
          <CardTitle className="text-sm font-medium">Rule detail</CardTitle>
          <RuleStatusBadge status={selected.status} />
        </div>
      </CardHeader>
      <CardContent className="space-y-3 text-xs">
        <StatusCopy status={selected.status} />

        {/* Canonical preview reused from A1. Renders the rule's
            structured fields, conditions, action, and locally-
            computed canonical hash. Component does NOT sign, submit,
            or call any backend. */}
        {rule !== null && (
          <Stage2WatchRulePreview
            rule={rule}
            expectedHashHex={expectedHashHex}
          />
        )}

        {rule === null && (
          <Alert>
            <AlertTitle>Canonical rule not in dashboard payload</AlertTitle>
            <AlertDescription>
              The dashboard mock payload does not include the full canonical
              rule for this id. A future Stage 2 slice that wires real backend
              routes will fetch it on demand.
            </AlertDescription>
          </Alert>
        )}

        <Separator />

        <LifecycleTimeline events={lifecycle} nowMs={nowMs} />

        <Separator />

        <div className="space-y-3">
          <div className="text-foreground/80 font-medium">
            Operator actions
          </div>
          <div className="grid sm:grid-cols-2 gap-3">
            <RevokeRuleSkeleton ruleIdHex={selected.rule_id_hex} />
            {realModeEnabled ? (
              <div
                className="text-[11px] text-muted-foreground italic"
                data-testid="force-tick-hidden-api-mode"
              >
                Force Tick is demo-only and unavailable in read-only API
                mode.
              </div>
            ) : (
              <ForceTickControl
                selectedRuleIdHex={selected.rule_id_hex}
                onDryRunTick={onDryRunTick}
              />
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function StatusCopy({ status }: { status: Stage2WatchRuleStatus }) {
  switch (status) {
    case "active":
      return (
        <p className="text-foreground/90">
          The watcher is actively evaluating this rule each tick. Conditions
          have not yet been met.
        </p>
      );
    case "condition_met":
      return (
        <Alert>
          <AlertTitle>Condition Met (Execution Simulated)</AlertTitle>
          <AlertDescription>
            All gates have been reached. In Stage 2 substrate, the
            condition-met transition flips a row in the database; no on-chain
            transaction has been built or sent. The downstream executor slice
            will pick this rule up.
          </AlertDescription>
        </Alert>
      );
    case "executing":
      return (
        <Alert>
          <AlertTitle>Executor pickup (simulated)</AlertTitle>
          <AlertDescription>
            The executor has claimed this rule for processing. Stage 2
            substrate at this slice does not build, sign, or broadcast a
            transaction.
          </AlertDescription>
        </Alert>
      );
    case "completed":
      return (
        <Alert>
          <AlertTitle>Lifecycle complete (simulated)</AlertTitle>
          <AlertDescription>
            The watcher lifecycle has reached its terminal state. This is a
            substrate-level signal — funds have not been moved by this slice.
          </AlertDescription>
        </Alert>
      );
    case "expired":
      return (
        <Alert className="border-amber-500/40">
          <AlertTitle>Expired</AlertTitle>
          <AlertDescription>
            The rule&apos;s <code>expires_at_slot</code> was reached before any
            condition was met. The watcher flipped the rule to{" "}
            <code>expired</code> and stopped evaluating it.
          </AlertDescription>
        </Alert>
      );
    case "revoked":
      return (
        <Alert className="border-amber-500/40">
          <AlertTitle>Revoked</AlertTitle>
          <AlertDescription>
            The rule was revoked. (Stage 2 substrate slice — no on-chain
            authorization exists yet.)
          </AlertDescription>
        </Alert>
      );
    case "failed":
      return (
        <Alert variant="destructive">
          <AlertTitle>Failed (terminal evaluator/simulator error)</AlertTitle>
          <AlertDescription>
            The watcher recorded a terminal error and stopped evaluating this
            rule. The recorded error appears in the timeline below.
          </AlertDescription>
        </Alert>
      );
  }
}

function LifecycleTimeline({
  events,
  nowMs,
}: {
  events: Stage2RuleLifecycleEvent[];
  nowMs: bigint;
}) {
  if (events.length === 0) {
    return (
      <p className="text-muted-foreground italic">
        No lifecycle events recorded for this rule yet.
      </p>
    );
  }
  return (
    <div className="space-y-2">
      <div className="text-foreground/80 font-medium">Lifecycle</div>
      <ul className="space-y-2">
        {events.map((e, i) => (
          <li
            key={i}
            className="rounded-md border border-border px-3 py-2 space-y-1"
            data-testid={`lifecycle-event-${i}`}
          >
            <div className="flex items-center justify-between gap-2 text-[11px]">
              <code className="font-mono text-foreground">{e.label}</code>
              <span className="text-muted-foreground">
                {formatTickGap(nowMs, e.at_ms)}
              </span>
            </div>
            {e.detail && (
              <p className="text-[11px] text-muted-foreground break-words">
                {e.detail}
              </p>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

function formatTickGap(nowMs: bigint, atMs: bigint): string {
  const gapMs = nowMs > atMs ? nowMs - atMs : BigInt(0);
  const seconds = Number(gapMs / BigInt(1000));
  if (seconds < 1) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const mins = Math.floor(seconds / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}
