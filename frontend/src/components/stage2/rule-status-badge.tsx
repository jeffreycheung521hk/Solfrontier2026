"use client";

// Stage 2 — RuleStatusBadge.
//
// Restrained colors + careful copy so `condition_met` reads as "the
// watcher's gate has been reached" — NOT as "execution succeeded".
// Per the dashboard prompt's UX rules:
//
//   - "Condition Met (Execution Simulated)" / "Gate Reached" — OK
//   - "Execution Successful" / "Transaction Sent" — NOT acceptable

import { Badge } from "@/components/ui/badge";
import type { Stage2WatchRuleStatus } from "@/lib/stage2-watch-api";

const LABEL: Record<Stage2WatchRuleStatus, string> = {
  active: "active",
  condition_met: "condition met",
  executing: "executing (simulated)",
  completed: "completed (simulated)",
  expired: "expired",
  revoked: "revoked",
  failed: "failed",
};

type BadgeVariant = "default" | "secondary" | "destructive" | "outline";

const VARIANT: Record<Stage2WatchRuleStatus, BadgeVariant> = {
  active: "default",
  condition_met: "secondary",
  executing: "secondary",
  completed: "secondary",
  expired: "outline",
  revoked: "outline",
  failed: "destructive",
};

export function RuleStatusBadge({
  status,
  className,
}: {
  status: Stage2WatchRuleStatus;
  className?: string;
}) {
  return (
    <Badge
      variant={VARIANT[status]}
      className={`text-xs ${className ?? ""}`}
      data-testid={`rule-status-${status}`}
    >
      {LABEL[status]}
    </Badge>
  );
}
