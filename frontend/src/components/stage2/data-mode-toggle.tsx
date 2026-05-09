"use client";

// Stage 2 — Data mode toggle (DEMO only).
//
// Segmented control letting the operator pick which deterministic
// mock dataset to render: a healthy watcher, an offline/stale one,
// or one with transient + terminal evaluator errors. This is a
// developer / demo affordance — there is no production-mode flag
// here.

import type { Stage2DataMode } from "@/lib/stage2-watch-api";

const OPTIONS: ReadonlyArray<{ value: Stage2DataMode; label: string }> = [
  { value: "healthy", label: "Healthy" },
  { value: "offline", label: "Offline" },
  { value: "errors", label: "Errors" },
];

export function DataModeToggle({
  value,
  onChange,
}: {
  value: Stage2DataMode;
  onChange: (next: Stage2DataMode) => void;
}) {
  return (
    <div className="space-y-1" data-testid="data-mode-toggle">
      <div className="text-[11px] uppercase tracking-wide text-muted-foreground">
        Mock data mode
      </div>
      <div
        role="radiogroup"
        className="inline-flex rounded-md border bg-background p-0.5"
        aria-label="Mock data mode"
      >
        {OPTIONS.map((opt) => {
          const selected = opt.value === value;
          return (
            <button
              key={opt.value}
              role="radio"
              type="button"
              aria-checked={selected}
              onClick={() => onChange(opt.value)}
              data-testid={`data-mode-${opt.value}`}
              className={`text-xs px-3 py-1 rounded transition-colors ${
                selected
                  ? "bg-foreground/10 text-foreground"
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {opt.label}
            </button>
          );
        })}
      </div>
      <div className="text-[11px] text-muted-foreground italic">
        Demo / developer selector. The dashboard renders mock data only;
        no backend route, no RPC, no signing.
      </div>
    </div>
  );
}
