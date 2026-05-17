"use client";

// Stage 2 — `/stage2/dashboard`.
//
// Active-rule dashboard built on top of A1 (canonical preview) and
// W2 (watcher scheduler substrate). Read-only / transparency UI.
//
// IMPORTANT — preview only:
//   - No signing, no wallet transaction, no broadcast.
//   - No backend mutation. No live RPC.
//   - No real revoke. The Revoke Rule button is a disabled skeleton.
//   - Force tick is a local mock simulator (dry-run) and is hidden in
//     read-only API mode.
//
// Data source model (A3):
//   - "mock" — deterministic mock data driven by Healthy / Offline /
//     Errors selector. Default on first load.
//   - "api"  — single GET against `/api/stage2/dashboard`. The route
//     does NOT exist in this slice; the call is expected to 404 and
//     the dashboard renders a clean "Backend pending" banner.
//
// Manual refresh button only — no auto-polling. Last-refreshed
// timestamp surfaces wall-clock recency.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ActiveRuleTable } from "@/components/stage2/active-rule-table";
import { DataModeToggle } from "@/components/stage2/data-mode-toggle";
import { RuleDetailPanel } from "@/components/stage2/rule-detail-panel";
import { WatcherHealthBanner } from "@/components/stage2/watcher-health-banner";
import {
  applyMockForceTickDryRun,
  countRulesByStatus,
  getStage2DashboardData,
  type Stage2DashboardApiResult,
  type Stage2DashboardData,
  type Stage2DashboardSource,
  type Stage2DataMode,
} from "@/lib/stage2-watch-api";

interface DashboardState {
  /** Last successful payload (mock or api). Preserved across mode
   *  switches and across an api error so the UI doesn't blank out. */
  lastOk: Stage2DashboardData | null;
  /** Most recent adapter result — drives the "API unavailable /
   *  validation error" banner. */
  lastResult: Stage2DashboardApiResult | null;
  /** True while a fetch is in flight (mock mode resolves
   *  synchronously so this is essentially api-mode-only). */
  loading: boolean;
}

export default function Stage2DashboardPage() {
  const [source, setSource] = useState<Stage2DashboardSource>("mock");
  const [mockMode, setMockMode] = useState<Stage2DataMode>("healthy");
  const [selectedRuleIdHex, setSelectedRuleIdHex] = useState<string | null>(
    null,
  );
  const [state, setState] = useState<DashboardState>({
    lastOk: null,
    lastResult: null,
    loading: false,
  });

  // The dashboard's "now" is fixed at module load so all "x seconds
  // ago" labels are stable across renders. (This file does not poll.)
  const nowMs = useMemo(() => BigInt(Date.now()), []);

  // Keep an `AbortController` for the current in-flight fetch so a
  // mode switch / unmount cancels it.
  const abortRef = useRef<AbortController | null>(null);

  const load = useCallback(
    async (
      nextSource: Stage2DashboardSource,
      nextMockMode: Stage2DataMode,
    ): Promise<void> => {
      // Cancel any prior in-flight request.
      abortRef.current?.abort();
      const controller = new AbortController();
      abortRef.current = controller;

      setState((prev) => ({ ...prev, loading: true }));
      try {
        const result = await getStage2DashboardData({
          source: nextSource,
          mockMode: nextMockMode,
          signal: controller.signal,
        });
        if (controller.signal.aborted) return;
        setState((prev) => ({
          loading: false,
          lastResult: result,
          // Only update lastOk on a successful result; on
          // unavailable / validation_error keep the previous payload
          // (or null on first load) so the UI degrades gracefully.
          lastOk: result.kind === "ok" ? result.data : prev.lastOk,
        }));
      } catch (err) {
        // AbortError flows here; ignore it. Anything else is surfaced
        // as a synthetic unavailable result so the banner renders.
        if (err instanceof DOMException && err.name === "AbortError") return;
        setState((prev) => ({
          loading: false,
          lastResult: {
            kind: "unavailable",
            source: "api",
            reason:
              err instanceof Error
                ? `unexpected error: ${err.message}`
                : "unexpected error",
            fetched_at_ms: BigInt(Date.now()),
          },
          lastOk: prev.lastOk,
        }));
      }
    },
    [],
  );

  // Initial load — mock mode, healthy.
  useEffect(() => {
    void load("mock", "healthy");
    return () => {
      abortRef.current?.abort();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function handleSourceChange(next: Stage2DashboardSource) {
    setSource(next);
    void load(next, mockMode);
    // Source change can drop a previously-selected rule (api payload
    // doesn't include rule_by_rule_id in this slice).
    setSelectedRuleIdHex(null);
  }

  function handleMockModeChange(next: Stage2DataMode) {
    setMockMode(next);
    void load("mock", next);
    // Mock-mode switch may drop rules; preserve selection only when
    // it survives the new payload. We let the next render pass clear
    // the selection if needed.
  }

  function handleRefresh() {
    void load(source, mockMode);
  }

  function handleDryRunTick(ruleIdHex: string) {
    if (source !== "mock") return; // belt-and-braces; control is hidden in api mode.
    setState((prev) => {
      if (prev.lastOk === null) return prev;
      const next = applyMockForceTickDryRun(prev.lastOk, ruleIdHex).next;
      return {
        ...prev,
        lastOk: next,
        // Reflect the new payload as the last "result" too, so
        // last-refreshed timestamp ticks visibly.
        lastResult: {
          kind: "ok",
          data: next,
          source: "mock",
          fetched_at_ms: BigInt(Date.now()),
        },
      };
    });
  }

  const data = state.lastOk;
  const counters =
    data === null
      ? { active: 0, condition_met: 0, executing: 0, completed: 0, expired: 0, revoked: 0, failed: 0 }
      : countRulesByStatus(data.rules);

  const selectedSummary =
    data === null || selectedRuleIdHex === null
      ? null
      : (data.rules.find((r) => r.rule_id_hex === selectedRuleIdHex) ?? null);
  const selectedRule =
    data === null || selectedRuleIdHex === null
      ? null
      : (data.rule_by_rule_id[selectedRuleIdHex] ?? null);
  const selectedLifecycle =
    data === null || selectedRuleIdHex === null
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
          Read-only status. No on-chain action is triggered from this page.
          Mock data renders by default; the read-only API mode previews the
          shape a future backend route will return.
        </p>
      </header>

      <SourceErrorBanner result={state.lastResult} />

      {data && <WatcherHealthBanner health={data.health} nowMs={nowMs} />}

      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="flex flex-wrap items-end gap-4">
          <SourceToggle value={source} onChange={handleSourceChange} />
          {source === "mock" ? (
            <DataModeToggle value={mockMode} onChange={handleMockModeChange} />
          ) : (
            <ApiModeNotice />
          )}
        </div>
        <RefreshControl
          loading={state.loading}
          onRefresh={handleRefresh}
          lastFetchedAtMs={state.lastResult?.fetched_at_ms ?? null}
        />
      </div>

      {data && (
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
      )}

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-sm font-medium">Rules</CardTitle>
        </CardHeader>
        <CardContent>
          {state.loading && data === null ? (
            <p className="text-xs text-muted-foreground italic">Loading…</p>
          ) : data === null ? (
            <p className="text-xs text-muted-foreground italic">
              No data yet. Select a source and refresh.
            </p>
          ) : (
            <ActiveRuleTable
              rules={data.rules}
              selectedRuleIdHex={selectedRuleIdHex}
              onSelect={setSelectedRuleIdHex}
              nowMs={nowMs}
            />
          )}
        </CardContent>
      </Card>

      {/*
        Force tick is dry-run mock-only. When source === "api" we pass
        `realModeEnabled` to the detail panel so it hides the control
        entirely. The Revoke Rule skeleton remains disabled in either
        source mode (backend route not added in this slice).
      */}
      <RuleDetailPanel
        selected={selectedSummary}
        rule={selectedRule}
        lifecycle={selectedLifecycle}
        nowMs={nowMs}
        onDryRunTick={handleDryRunTick}
        realModeEnabled={source === "api"}
      />
    </div>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function SourceToggle({
  value,
  onChange,
}: {
  value: Stage2DashboardSource;
  onChange: (next: Stage2DashboardSource) => void;
}) {
  return (
    <div className="space-y-1" data-testid="source-toggle">
      <div className="text-[11px] uppercase tracking-wide text-muted-foreground">
        Data source
      </div>
      <div
        role="radiogroup"
        className="inline-flex rounded-md border bg-background p-0.5"
        aria-label="Dashboard data source"
      >
        <SourceButton
          checked={value === "mock"}
          label="Mock"
          onClick={() => onChange("mock")}
          testid="source-mock"
        />
        <SourceButton
          checked={value === "api"}
          label="Read-only API (Preview)"
          onClick={() => onChange("api")}
          testid="source-api"
        />
      </div>
    </div>
  );
}

function SourceButton({
  checked,
  label,
  onClick,
  testid,
}: {
  checked: boolean;
  label: string;
  onClick: () => void;
  testid: string;
}) {
  return (
    <button
      role="radio"
      type="button"
      aria-checked={checked}
      onClick={onClick}
      data-testid={testid}
      className={`text-xs px-3 py-1 rounded transition-colors ${
        checked
          ? "bg-foreground/10 text-foreground"
          : "text-muted-foreground hover:text-foreground"
      }`}
    >
      {label}
    </button>
  );
}

function ApiModeNotice() {
  return (
    <div
      className="text-[11px] text-muted-foreground italic max-w-[320px]"
      data-testid="api-mode-notice"
    >
      Read-only API preview. Backend route pending — the mock-data mode
      selector is hidden because the data source comes from the future
      `/api/stage2/dashboard` GET endpoint.
    </div>
  );
}

function RefreshControl({
  loading,
  onRefresh,
  lastFetchedAtMs,
}: {
  loading: boolean;
  onRefresh: () => void;
  lastFetchedAtMs: bigint | null;
}) {
  return (
    <div
      className="flex items-end gap-3"
      data-testid="refresh-control"
    >
      <div className="text-[11px] text-muted-foreground">
        last refreshed:{" "}
        <span
          className="font-mono text-foreground"
          data-testid="last-refreshed"
        >
          {formatLastRefreshed(lastFetchedAtMs)}
        </span>
      </div>
      <Button
        type="button"
        size="sm"
        variant="outline"
        onClick={onRefresh}
        disabled={loading}
        aria-busy={loading}
        data-testid="refresh-button"
      >
        {loading ? "Refreshing…" : "Refresh"}
      </Button>
    </div>
  );
}

function SourceErrorBanner({
  result,
}: {
  result: Stage2DashboardApiResult | null;
}) {
  if (result === null) return null;
  if (result.kind === "ok") return null;
  if (result.kind === "unavailable") {
    return (
      <Alert
        className="border-amber-500/40"
        data-testid="api-unavailable-banner"
      >
        <AlertTitle className="text-sm">
          Read-only API unavailable
        </AlertTitle>
        <AlertDescription className="text-xs space-y-1">
          <span className="block">
            Backend route pending — dashboard data is currently rendered
            from the previous successful response (or the empty state on
            first load). No on-chain action is triggered from this page.
          </span>
          <span className="block break-words">
            <span className="text-muted-foreground">reason:</span>{" "}
            <span className="text-foreground">{result.reason}</span>
            {typeof result.httpStatus === "number" && (
              <span className="ml-1 text-muted-foreground">
                (HTTP {result.httpStatus})
              </span>
            )}
          </span>
        </AlertDescription>
      </Alert>
    );
  }
  // result.kind === "validation_error"
  return (
    <Alert variant="destructive" data-testid="api-validation-error-banner">
      <AlertTitle className="text-sm">API response failed validation</AlertTitle>
      <AlertDescription className="text-xs break-words">
        {result.reason}
      </AlertDescription>
    </Alert>
  );
}

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
      <Counter
        label="failed"
        value={failed}
        tone={failed > 0 ? "warn" : "muted"}
      />
      <Counter
        label="stale"
        value={stale}
        tone={stale > 0 ? "warn" : "muted"}
      />
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

function formatLastRefreshed(atMs: bigint | null): string {
  if (atMs === null) return "—";
  // Convert bigint ms to a Date for a stable HH:mm:ss label.
  // BigInt → Number is safe for any wall-clock ms in our lifetime.
  const d = new Date(Number(atMs));
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}
