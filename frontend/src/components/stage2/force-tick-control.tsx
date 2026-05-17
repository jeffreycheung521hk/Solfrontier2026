"use client";

// Stage 2 — ForceTickControl (DRY-RUN ONLY).
//
// Targeted force-tick simulator. Operates on local mock state via
// `applyMockForceTickDryRun` ONLY. Does NOT call any backend route,
// does NOT issue an HTTP request, does NOT touch RPC, does NOT
// mutate any persisted state.
//
// Per the dashboard prompt:
//   - Label: "Force Tick (Dry-run)"
//   - Visually marked as Demo / Dry-run
//   - Explanation copy required
//   - Operates on the SELECTED rule only — never on all rules
//   - Hidden / deeply disabled if data mode is ever real/API mode
//     (this slice has no real API mode, so the control is always
//     dry-run; we still wire `realModeEnabled` for forward-compat)

import { Button } from "@/components/ui/button";

export function ForceTickControl({
  selectedRuleIdHex,
  onDryRunTick,
  realModeEnabled = false,
}: {
  selectedRuleIdHex: string | null;
  /** Caller-supplied callback that should run `applyMockForceTickDryRun`
   *  and update its local state. The callback MUST be a pure local
   *  state transition — no HTTP, no RPC. */
  onDryRunTick: (ruleIdHex: string) => void;
  /** Future-compat flag: when a real backend mode lands, the page
   *  passes `true` to deeply disable this control. This slice never
   *  passes `true`. */
  realModeEnabled?: boolean;
}) {
  const disabled = realModeEnabled || selectedRuleIdHex === null;
  const tooltip = realModeEnabled
    ? "Hidden in real-data mode (this slice never sets real-data mode)."
    : selectedRuleIdHex === null
      ? "Select a rule first."
      : "Simulates one watcher tick on the selected rule. No HTTP, no RPC, no signing — local mock state only.";

  if (realModeEnabled) {
    // Per the prompt: in real-data mode, hide this control entirely.
    return null;
  }

  return (
    <div className="space-y-1" data-testid="force-tick-control">
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={disabled}
        aria-disabled={disabled}
        title={tooltip}
        onClick={() => {
          if (selectedRuleIdHex !== null) {
            onDryRunTick(selectedRuleIdHex);
          }
        }}
        data-testid="force-tick-button"
      >
        Force Tick (Dry-run)
      </Button>
      <div className="text-[11px] text-muted-foreground italic">
        Simulates watcher scheduler. Advances local mock lifecycle state
        without executing real on-chain transactions, no HTTP, no RPC,
        no signing.
      </div>
    </div>
  );
}
