"use client";

// Stage 2 — ActiveRuleTable.
//
// Desktop: dense table.
// Mobile (< sm): card list — no horizontal overflow.
// Clicking a row selects that rule (drives the detail panel).
//
// The table renders `last_checked_slot` and `last_successful_tick_at_ms`
// per rule. These are watcher tick metrics — not on-chain confirmations.

import { Badge } from "@/components/ui/badge";
import { RuleStatusBadge } from "@/components/stage2/rule-status-badge";
import type { Stage2WatchRuleSummary } from "@/lib/stage2-watch-api";

export function ActiveRuleTable({
  rules,
  selectedRuleIdHex,
  onSelect,
  nowMs,
}: {
  rules: Stage2WatchRuleSummary[];
  selectedRuleIdHex: string | null;
  onSelect: (ruleIdHex: string) => void;
  nowMs: bigint;
}) {
  if (rules.length === 0) {
    return (
      <div
        className="rounded-md border border-dashed bg-muted/30 px-4 py-6 text-xs text-muted-foreground"
        data-testid="active-rule-table-empty"
      >
        No rules to display in this mock data mode.
      </div>
    );
  }

  return (
    <div data-testid="active-rule-table">
      {/* Desktop / tablet: real table */}
      <div className="hidden sm:block overflow-x-auto rounded-md border">
        <table className="w-full text-xs">
          <thead className="bg-muted/40">
            <tr className="text-left">
              <Th>Status</Th>
              <Th>Action</Th>
              <Th>Rule id</Th>
              <Th>Hash</Th>
              <Th>Expires (slot)</Th>
              <Th>Last checked (slot)</Th>
              <Th>Last tick</Th>
              <Th>Last error</Th>
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => {
              const selected = selectedRuleIdHex === r.rule_id_hex;
              return (
                <tr
                  key={r.rule_id_hex}
                  onClick={() => onSelect(r.rule_id_hex)}
                  className={`cursor-pointer border-t hover:bg-muted/30 transition-colors ${
                    selected ? "bg-foreground/5" : ""
                  }`}
                  data-testid={`rule-row-${r.rule_id_hex}`}
                  aria-selected={selected}
                >
                  <Td>
                    <RuleStatusBadge status={r.status} />
                  </Td>
                  <Td>
                    <ActionPill actionType={r.action_type} />
                  </Td>
                  <Td>
                    <code className="font-mono text-[10px]">
                      {shortenHex(r.rule_id_hex)}
                    </code>
                  </Td>
                  <Td>
                    <code className="font-mono text-[10px] text-muted-foreground">
                      {shortenHex(r.canonical_rule_hash_hex)}
                    </code>
                  </Td>
                  <Td>
                    <code className="font-mono">
                      {r.expires_at_slot.toString()}
                    </code>
                  </Td>
                  <Td>
                    <code className="font-mono text-muted-foreground">
                      {r.last_checked_slot === null
                        ? "—"
                        : r.last_checked_slot.toString()}
                    </code>
                  </Td>
                  <Td>
                    <span className="text-muted-foreground">
                      {formatTickGap(nowMs, r.last_successful_tick_at_ms)}
                    </span>
                  </Td>
                  <Td className="max-w-[280px]">
                    {r.last_error ? (
                      <span
                        className="text-destructive break-words"
                        title={r.last_error}
                      >
                        {truncate(r.last_error, 60)}
                      </span>
                    ) : (
                      <span className="text-muted-foreground">—</span>
                    )}
                  </Td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* Mobile: card list */}
      <ul className="sm:hidden space-y-2">
        {rules.map((r) => {
          const selected = selectedRuleIdHex === r.rule_id_hex;
          return (
            <li key={r.rule_id_hex}>
              <button
                type="button"
                onClick={() => onSelect(r.rule_id_hex)}
                aria-pressed={selected}
                className={`w-full text-left rounded-md border p-3 space-y-1 hover:bg-muted/30 transition-colors ${
                  selected ? "bg-foreground/5 border-foreground/30" : ""
                }`}
                data-testid={`rule-card-${r.rule_id_hex}`}
              >
                <div className="flex items-center justify-between">
                  <RuleStatusBadge status={r.status} />
                  <ActionPill actionType={r.action_type} />
                </div>
                <div className="text-[11px] space-y-0.5">
                  <div>
                    <span className="text-muted-foreground">id:</span>{" "}
                    <code className="font-mono">
                      {shortenHex(r.rule_id_hex)}
                    </code>
                  </div>
                  <div>
                    <span className="text-muted-foreground">hash:</span>{" "}
                    <code className="font-mono text-muted-foreground">
                      {shortenHex(r.canonical_rule_hash_hex)}
                    </code>
                  </div>
                  <div>
                    <span className="text-muted-foreground">expires slot:</span>{" "}
                    <code className="font-mono">
                      {r.expires_at_slot.toString()}
                    </code>
                  </div>
                  <div>
                    <span className="text-muted-foreground">last tick:</span>{" "}
                    {formatTickGap(nowMs, r.last_successful_tick_at_ms)}
                  </div>
                  {r.last_error && (
                    <div className="text-destructive break-words">
                      {truncate(r.last_error, 80)}
                    </div>
                  )}
                </div>
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function Th({ children }: { children: React.ReactNode }) {
  return <th className="px-3 py-2 font-medium text-muted-foreground">{children}</th>;
}

function Td({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return <td className={`px-3 py-2 align-top ${className ?? ""}`}>{children}</td>;
}

function ActionPill({
  actionType,
}: {
  actionType: Stage2WatchRuleSummary["action_type"];
}) {
  const label =
    actionType === "solend_withdraw_all_delegated"
      ? "solend withdraw-all"
      : "jupiter buy SOL";
  return (
    <Badge variant="outline" className="text-[10px]">
      {label}
    </Badge>
  );
}

function shortenHex(hex: string): string {
  if (hex.length <= 12) return hex;
  return `${hex.slice(0, 6)}…${hex.slice(-4)}`;
}

function truncate(s: string, n: number): string {
  return s.length <= n ? s : `${s.slice(0, n - 1)}…`;
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
