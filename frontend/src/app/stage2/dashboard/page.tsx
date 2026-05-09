"use client";

// Stage 2 — `/stage2/dashboard`.
//
// Active-rule dashboard built on top of A1 (canonical preview) and
// W2 (watcher scheduler substrate). This is a transparency UI:
// the operator can see what the watcher would be doing, NOT a place
// to execute on-chain actions.
//
// IMPORTANT — preview / transparency only:
//   - No signing, no wallet transaction, no broadcast.
//   - No backend mutation. No live RPC.
//   - No real revoke. The Revoke Rule button is a disabled skeleton.
//   - Force tick is a local mock simulator (dry-run).
//
// Data path: this slice ships with a deterministic mock adapter
// (`getMockDashboard`) selected by the `Stage2DataMode` toggle.
// A future slice that adds backend HTTP routes will replace the
// adapter; the page does NOT need to know the difference.

import { useMemo, useState } from "react";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ActiveRuleTable } from "@/components/stage2/active-rule-table";
import { DataModeToggle } from "@/components/stage2/data-mode-toggle";
import { RuleDetailPanel } from "@/components/stage2/rule-detail-panel";
import { WatcherHealthBanner } from "@/components/stage2/watcher-health-banner";
import {
  applyMockForceTickDryRun,
  countRulesByStatus,
  getMockDashboard,
  type Stage2DashboardData,
  type Stage2DataMode,
} from "@/lib/stage2-watch-api";

export default function Stage2DashboardPage() {
  const [dataMode, setDataMode] = useState<Stage2DataMode>("healthy");
  // Snapshot per-mode so the local force-tick simulator can mutate
  // the mock payload without re-fetching every render.
  const [data, setData] = useState<Stage2DashboardData>(() =>
    getMockDashboard("healthy"),
  );
  const [selectedRuleIdHex, setSelectedRuleIdHex] = useState<string | null>(
    null,
  );

  // The dashboard's "now" is fixed at module load so all "x seconds
  // ago" labels are stable across renders. (This file does not poll.)
  const nowMs = useMemo(() => BigInt(Date.now()), []);

  function handleModeChange(next: Stage2DataMode) {
    setDataMode(next);
    setData(getMockDashboard(next));
    // Preserve selection if the rule still exists; otherwise clear.
    setSelectedRuleIdHex((prev) => {
      if (prev === null) return null;
      const stillThere = getMockDashboard(next).rules.some(
        (r) => r.rule_id_hex === prev,
      );
      return stillThere ? prev : null;
    });
  }

  function handleDryRunTick(ruleIdHex: string) {
    setData((prev) => applyMockForceTickDryRun(prev, ruleIdHex).next);
  }

  const counters = countRulesByStatus(data.rules);
  const selectedSummary =
    selectedRuleIdHex === null
      ? null
      : (data.rules.find((r) => r.rule_id_hex === selectedRuleIdHex) ?? null);
  const selectedRule =
    selectedRuleIdHex === null
      ? null
      : (data.rule_by_rule_id[selectedRuleIdHex] ?? null);
  const selectedLifecycle =
    selectedRuleIdHex === null
      ? []
      : (data.lifecycle_by_rule_id[selectedRuleIdHex] ?? []);

  return (
    <div className="space-y-6">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">
          Stage 2 dashboard
        </div>
        <h1 className="text-2xl font-semibold tracking-tight">
          Active watch rules
        </h1>
        <p className="text-sm text-muted-foreground">
          Transparency surface for the Stage 2 watcher scheduler. No signing,
          no broadcast, no live RPC — this slice ships with a mock data
          adapter; future slices will swap it for real backend routes.
        </p>
      </header>

      <WatcherHealthBanner health={data.health} nowMs={nowMs} />

      <div className="flex flex-wrap items-end justify-between gap-4">
        <DataModeToggle value={dataMode} onChange={handleModeChange} />
        <SummaryCounters
          total={data.rules.length}
          active={counters.active}
          condition_met={counters.condition_met}
          executing={counters.executing}
          completed={counters.completed}
          expired={counters.expired}
          revoked={counters.revoked}
          failed={counters.failed}
          stale={Number(data.health.stale_rule_count)}
        />
      </div>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-sm font-medium">Rules</CardTitle>
        </CardHeader>
        <CardContent>
          <ActiveRuleTable
            rules={data.rules}
            selectedRuleIdHex={selectedRuleIdHex}
            onSelect={setSelectedRuleIdHex}
            nowMs={nowMs}
          />
        </CardContent>
      </Card>

      <RuleDetailPanel
        selected={selectedSummary}
        rule={selectedRule}
        lifecycle={selectedLifecycle}
        nowMs={nowMs}
        onDryRunTick={handleDryRunTick}
      />
    </div>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function SummaryCounters({
  total,
  active,
  condition_met,
  executing,
  completed,
  expired,
  revoked,
  failed,
  stale,
}: {
  total: number;
  active: number;
  condition_met: number;
  executing: number;
  completed: number;
  expired: number;
  revoked: number;
  failed: number;
  stale: number;
}) {
  return (
    <div
      className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px]"
      data-testid="summary-counters"
    >
      <Counter label="total" value={total} />
      <Counter label="active" value={active} />
      <Counter label="condition met" value={condition_met} />
      <Counter label="executing" value={executing} />
      <Counter label="completed" value={completed} />
      <Counter label="expired" value={expired} />
      <Counter label="revoked" value={revoked} />
      <Counter label="failed" value={failed} tone={failed > 0 ? "warn" : "muted"} />
      <Counter label="stale" value={stale} tone={stale > 0 ? "warn" : "muted"} />
    </div>
  );
}

function Counter({
  label,
  value,
  tone = "muted",
}: {
  label: string;
  value: number;
  tone?: "muted" | "warn";
}) {
  return (
    <span className="inline-flex items-baseline gap-1">
      <span className="text-muted-foreground">{label}:</span>
      <span
        className={`font-mono ${
          tone === "warn" ? "text-amber-600 font-semibold" : "text-foreground"
        }`}
      >
        {value}
      </span>
    </span>
  );
}
