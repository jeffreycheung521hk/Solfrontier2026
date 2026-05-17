"use client";

// Stage 2 — RevokeRuleSkeleton.
//
// Skeleton button for the future revoke flow. By design this slice
// has NO route handler, NO wallet signature path, and NO state
// mutation behind this control. The button is permanently disabled
// in the UI and surfaces a tooltip explaining why.
//
// Per the dashboard prompt:
//   - Button text: "Revoke Rule"
//   - Disabled by default
//   - Tooltip / help text:
//       "Backend routing pending. Revoke action is temporarily
//        disabled in UI."
//   - Secondary / outline visual style (NOT alarming red primary)
//   - No click handler that signs, sends, broadcasts, or mutates
//     backend state.

import { Button } from "@/components/ui/button";

export function RevokeRuleSkeleton({
  ruleIdHex,
}: {
  ruleIdHex?: string;
}) {
  // We intentionally do NOT hook up any onClick handler. The button
  // remains disabled regardless of `ruleIdHex` — the prop is here so
  // a future slice that wires in the real revoke route has the
  // identifier to hand without a refactor.
  void ruleIdHex;
  const tooltip =
    "Backend routing pending. Revoke action is temporarily disabled in UI.";
  return (
    <div className="space-y-1" data-testid="revoke-rule-skeleton">
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled
        aria-disabled="true"
        title={tooltip}
        data-testid="revoke-rule-button"
      >
        Revoke Rule
      </Button>
      <div className="text-[11px] text-muted-foreground italic">
        {tooltip}
      </div>
    </div>
  );
}
