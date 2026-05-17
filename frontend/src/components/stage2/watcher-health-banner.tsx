"use client";

// Stage 2 — WatcherHealthBanner.
//
// Compact one-stripe banner showing wall-clock watcher health. The
// banner state is derived from `health.is_offline` plus
// `health.last_error` — wall-clock health is intentionally distinct
// from per-rule slot expiry, which surfaces inside the rule table.

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import type { Stage2WatcherHealth } from "@/lib/stage2-watch-api";

export function WatcherHealthBanner({
  health,
  nowMs,
}: {
  health: Stage2WatcherHealth;
  /** Wall-clock ms used for the "x seconds ago" labels. Caller is
   *  responsible for updating this — the banner does not poll. */
  nowMs: bigint;
}) {
  const tone =
    health.is_offline
      ? "destructive"
      : health.last_error
        ? "amber"
        : "ok";

  const title =
    tone === "destructive"
      ? "Watcher offline"
      : tone === "amber"
        ? "Watcher online — last tick reported errors"
        : "Watcher online";

  const lastSuccess = formatTickGap(
    nowMs,
    health.last_successful_tick_at_ms,
  );
  const lastTick = formatTickGap(nowMs, health.last_tick_finished_at_ms);

  return (
    <Alert
      className={
        tone === "destructive"
          ? "border-destructive/40"
          : tone === "amber"
            ? "border-amber-500/40"
            : "border-foreground/15"
      }
      data-testid={`watcher-health-${tone}`}
    >
      <AlertTitle className="flex items-center gap-2 text-sm">
        <span
          aria-hidden
          className={`inline-block h-2 w-2 rounded-full ${
            tone === "destructive"
              ? "bg-destructive"
              : tone === "amber"
                ? "bg-amber-500"
                : "bg-green-600"
          }`}
        />
        <span>{title}</span>
      </AlertTitle>
      <AlertDescription className="text-xs space-y-1">
        <div className="flex flex-wrap gap-x-6 gap-y-1">
          <Field
            k="last successful tick"
            v={
              <span className="font-mono text-foreground">{lastSuccess}</span>
            }
          />
          <Field
            k="last tick finished"
            v={<span className="font-mono text-foreground">{lastTick}</span>}
          />
          <Field
            k="active rules"
            v={
              <span className="font-mono text-foreground">
                {health.active_rule_count.toString()}
              </span>
            }
          />
          <Field
            k="in-flight"
            v={
              <span className="font-mono text-foreground">
                {health.in_flight_rule_count.toString()}
              </span>
            }
          />
          <Field
            k="stale"
            v={
              <span
                className={`font-mono ${
                  health.stale_rule_count > BigInt(0)
                    ? "text-amber-600 font-semibold"
                    : "text-foreground"
                }`}
              >
                {health.stale_rule_count.toString()}
              </span>
            }
          />
        </div>
        {health.last_error && (
          <div className="break-words">
            <span className="text-muted-foreground">last error:</span>{" "}
            <span className="text-foreground">{health.last_error}</span>
          </div>
        )}
        <div className="text-[11px] text-muted-foreground italic">
          Wall-clock health (watcher tick freshness) — separate from per-rule
          slot expiry shown in the rule table.
        </div>
      </AlertDescription>
    </Alert>
  );
}

function Field({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <span className="inline-flex items-baseline gap-1.5">
      <span className="text-muted-foreground">{k}:</span>
      <span>{v}</span>
    </span>
  );
}

function formatTickGap(nowMs: bigint, atMs: bigint | null): string {
  if (atMs === null) return "—";
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
