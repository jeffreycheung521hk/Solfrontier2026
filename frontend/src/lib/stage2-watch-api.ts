// Stage 2 — frontend dashboard types + mock adapter.
//
// This file defines the dashboard's data contract and provides
// deterministic mock data for the three operator data modes
// (healthy / offline / errors). It does NOT call any backend route;
// it does NOT issue live RPC; it does NOT touch a wallet.
//
// TODO (Stage 2 / route-wiring slice): replace `getMockDashboard`
// with a real adapter that calls the Stage 2 daemon HTTP routes
// (e.g. GET /sessions/:id/stage2/watcher/health,
// GET /sessions/:id/stage2/rules). The route handlers do not exist
// in this slice and adding them is out of scope.
//
// The types here mirror, where it matters, the Rust types in:
//   - crates/state-store/src/stage2_watch_rules.rs (WatchRuleStatus,
//     StoredWatchRule)
//   - crates/gateway/src/stage2_watcher.rs (Stage2WatcherHealth,
//     Stage2RuleTickResult)
// We deliberately keep the surface narrow — only fields the
// dashboard renders are mirrored. Full canonical rule details are
// drawn from the existing A1 `WatchRule` shape, not duplicated.

import {
  fixtureScenarioASolendAprBelow10,
  fixtureScenarioBBasketBuySol,
  EXPECTED_FIXTURE_RULE_HASHES,
  type WatchRule,
} from "./stage2-watch-rule";

// ── Backend mirror types (frontend projection only) ───────────────────────

/// Watch rule status. Mirrors `crates/state-store/src/stage2_watch_rules.rs::WatchRuleStatus`.
export type Stage2WatchRuleStatus =
  | "active"
  | "condition_met"
  | "executing"
  | "completed"
  | "expired"
  | "revoked"
  | "failed";

/// Watcher health — mirrors `crates/gateway/src/stage2_watcher.rs::Stage2WatcherHealth`.
/// Frontend uses bigint for ms-since-epoch fields so we don't lose
/// precision on large values; UI rendering converts to strings.
export interface Stage2WatcherHealth {
  enabled: boolean;
  last_successful_tick_at_ms: bigint | null;
  last_tick_started_at_ms: bigint | null;
  last_tick_finished_at_ms: bigint | null;
  last_tick_duration_ms: bigint | null;
  last_error: string | null;
  active_rule_count: bigint;
  stale_rule_count: bigint;
  in_flight_rule_count: bigint;
  expired_count_last_tick: number;
  condition_met_count_last_tick: number;
  transient_error_count_last_tick: number;
  terminal_error_count_last_tick: number;
  failed_count_last_tick: number;
  offline_threshold_ms: bigint;
  stale_threshold_ms: bigint;
  is_offline: boolean;
}

/// Compact summary of a watch rule for the active-rule table. Avoids
/// embedding the full `WatchRule` to keep table rows light.
export interface Stage2WatchRuleSummary {
  rule_id_hex: string;
  canonical_rule_hash_hex: string;
  status: Stage2WatchRuleStatus;
  action_type: "solend_withdraw_all_delegated" | "jupiter_buy_sol_with_usdc";
  expires_at_slot: bigint;
  last_checked_slot: bigint | null;
  last_successful_tick_at_ms: bigint | null;
  last_error: string | null;
  created_at_ms: bigint;
  updated_at_ms: bigint;
}

/// Single lifecycle event for the per-rule timeline. Free-form so
/// future tick-result variants drop in without a schema bump.
export interface Stage2RuleLifecycleEvent {
  /// Wall-clock ms — frontend renders relative.
  at_ms: bigint;
  /// Stable label, e.g. `"created"`, `"checked_condition_false"`,
  /// `"condition_met"`, `"force_tick_dry_run"`, `"expired"`,
  /// `"failed_terminal"`. The dashboard maps known labels to colored
  /// chips; unknown labels render as muted text.
  label: string;
  /// Optional human-readable detail (e.g. error message).
  detail: string | null;
}

/// Top-level dashboard payload. Includes the watcher snapshot, the
/// summarized rule list, the per-rule lifecycle history, AND the
/// full canonical `WatchRule` for the rules whose detail view we
/// can render with `<Stage2WatchRulePreview>`.
export interface Stage2DashboardData {
  health: Stage2WatcherHealth;
  rules: Stage2WatchRuleSummary[];
  lifecycle_by_rule_id: Record<string, Stage2RuleLifecycleEvent[]>;
  /// Full canonical rules keyed by `rule_id_hex`. May not include
  /// every rule (e.g. rules created in a future slice the frontend
  /// hasn't ingested) — UI must handle absence gracefully.
  rule_by_rule_id: Record<string, WatchRule>;
}

// ── Operator data mode (mock-only selector) ───────────────────────────────

/// Demo data selector. Drives the mock adapter; in a future
/// real-backend slice this would be a NoOp or replaced.
export type Stage2DataMode = "healthy" | "offline" | "errors";

// ── Mock fixtures + adapter ───────────────────────────────────────────────

const NOW_MS = BigInt(Date.UTC(2026, 4, 11, 0, 0, 0)); // 2026-05-11

const HEALTHY_TICK_GAP_MS = BigInt(2_000); // 2s ago
const STALE_TICK_GAP_MS = BigInt(45_000); // 45s ago
const OFFLINE_TICK_GAP_MS = BigInt(120_000); // 2m ago

const OFFLINE_THRESHOLD_MS = BigInt(60_000);
const STALE_THRESHOLD_MS = BigInt(20_000);

/// Stable rule_ids for mock rules. Each is 16 bytes hex; the first
/// few bytes are ASCII for ease of reading in audit dumps.
const RULE_IDS = {
  active_solend: "52554c45000000000000000000000001", // "RULE...01" (matches A1 Scenario A)
  condition_met_basket: "52554c45000000000000000000000002", // "RULE...02" (matches A1 Scenario B shape)
  expired_solend: "455850440000000000000000000000a1", // "EXPD...a1"
  revoked_basket: "5256504b00000000000000000000000b", // "RVPK...0b"
  failed_solend: "464149440000000000000000000000ff", // "FAID...ff"
  executing_solend: "455845430000000000000000000000c0", // "EXEC...c0"
} as const;

function copyRuleWithIdAndExpiry(
  base: WatchRule,
  ruleIdHex: string,
  expiresAtSlot: bigint,
): WatchRule {
  return {
    ...base,
    rule_id: hexToBytes(ruleIdHex, 16),
    expires_at_slot: expiresAtSlot,
  };
}

function hexToBytes(hex: string, expectedLen: number): Uint8Array {
  if (hex.length !== expectedLen * 2) {
    throw new Error(
      `hexToBytes: expected ${expectedLen} bytes (${expectedLen * 2} chars), got ${hex.length}`,
    );
  }
  const out = new Uint8Array(expectedLen);
  for (let i = 0; i < expectedLen; i++) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

function buildMockRules(): {
  rules: Stage2WatchRuleSummary[];
  lifecycle_by_rule_id: Record<string, Stage2RuleLifecycleEvent[]>;
  rule_by_rule_id: Record<string, WatchRule>;
} {
  // Reuse A1 fixtures verbatim where the canonical preview will be
  // shown — that keeps the detail-view's hash matching the pinned A1
  // expected hash. For non-A1 status variants we vary `expires_at_slot`
  // (and rule_id) to differentiate, accepting that those rules' hashes
  // are NOT pinned (and the preview component renders without an
  // expectedHashHex for them).
  const A1_FIXTURE_A_ID = "52554c45000000000000000000000001";

  const ruleA1Solend = fixtureScenarioASolendAprBelow10();
  const ruleA1Basket = fixtureScenarioBBasketBuySol();

  const expiredSolend = copyRuleWithIdAndExpiry(
    ruleA1Solend,
    RULE_IDS.expired_solend,
    BigInt(415_499_000), // before created_at; intentionally already-expired
  );
  const revokedBasket = copyRuleWithIdAndExpiry(
    ruleA1Basket,
    RULE_IDS.revoked_basket,
    BigInt(415_700_000),
  );
  const failedSolend = copyRuleWithIdAndExpiry(
    ruleA1Solend,
    RULE_IDS.failed_solend,
    BigInt(415_700_000),
  );
  const executingSolend = copyRuleWithIdAndExpiry(
    ruleA1Solend,
    RULE_IDS.executing_solend,
    BigInt(415_700_000),
  );
  const conditionMetBasket = copyRuleWithIdAndExpiry(
    ruleA1Basket,
    RULE_IDS.condition_met_basket,
    BigInt(415_700_000),
  );

  const rule_by_rule_id: Record<string, WatchRule> = {
    [A1_FIXTURE_A_ID]: ruleA1Solend, // active_solend uses A1's pinned id
    [RULE_IDS.condition_met_basket]: conditionMetBasket,
    [RULE_IDS.expired_solend]: expiredSolend,
    [RULE_IDS.revoked_basket]: revokedBasket,
    [RULE_IDS.failed_solend]: failedSolend,
    [RULE_IDS.executing_solend]: executingSolend,
  };

  const summaries: Stage2WatchRuleSummary[] = [
    {
      rule_id_hex: A1_FIXTURE_A_ID,
      canonical_rule_hash_hex:
        EXPECTED_FIXTURE_RULE_HASHES.scenario_a_solend_apr_below_10,
      status: "active",
      action_type: "solend_withdraw_all_delegated",
      expires_at_slot: ruleA1Solend.expires_at_slot,
      last_checked_slot: BigInt(415_512_000),
      last_successful_tick_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
      last_error: null,
      created_at_ms: NOW_MS - BigInt(8 * 60 * 60 * 1000), // 8h ago
      updated_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
    },
    {
      rule_id_hex: RULE_IDS.condition_met_basket,
      // Computed at runtime in the detail view; leave a placeholder
      // here — the table only displays the short prefix.
      canonical_rule_hash_hex: "(computed)",
      status: "condition_met",
      action_type: "jupiter_buy_sol_with_usdc",
      expires_at_slot: BigInt(415_700_000),
      last_checked_slot: BigInt(415_580_000),
      last_successful_tick_at_ms: NOW_MS - BigInt(15_000),
      last_error: null,
      created_at_ms: NOW_MS - BigInt(2 * 60 * 60 * 1000),
      updated_at_ms: NOW_MS - BigInt(15_000),
    },
    {
      rule_id_hex: RULE_IDS.executing_solend,
      canonical_rule_hash_hex: "(computed)",
      status: "executing",
      action_type: "solend_withdraw_all_delegated",
      expires_at_slot: BigInt(415_700_000),
      last_checked_slot: BigInt(415_590_000),
      last_successful_tick_at_ms: NOW_MS - BigInt(8_000),
      last_error: null,
      created_at_ms: NOW_MS - BigInt(20 * 60 * 1000),
      updated_at_ms: NOW_MS - BigInt(8_000),
    },
    {
      rule_id_hex: RULE_IDS.expired_solend,
      canonical_rule_hash_hex: "(computed)",
      status: "expired",
      action_type: "solend_withdraw_all_delegated",
      expires_at_slot: BigInt(415_499_000),
      last_checked_slot: BigInt(415_499_500),
      last_successful_tick_at_ms: NOW_MS - BigInt(6 * 60 * 60 * 1000),
      last_error: null,
      created_at_ms: NOW_MS - BigInt(24 * 60 * 60 * 1000),
      updated_at_ms: NOW_MS - BigInt(6 * 60 * 60 * 1000),
    },
    {
      rule_id_hex: RULE_IDS.revoked_basket,
      canonical_rule_hash_hex: "(computed)",
      status: "revoked",
      action_type: "jupiter_buy_sol_with_usdc",
      expires_at_slot: BigInt(415_700_000),
      last_checked_slot: BigInt(415_530_000),
      last_successful_tick_at_ms: NOW_MS - BigInt(40 * 60 * 1000),
      last_error: null,
      created_at_ms: NOW_MS - BigInt(3 * 60 * 60 * 1000),
      updated_at_ms: NOW_MS - BigInt(40 * 60 * 1000),
    },
    {
      rule_id_hex: RULE_IDS.failed_solend,
      canonical_rule_hash_hex: "(computed)",
      status: "failed",
      action_type: "solend_withdraw_all_delegated",
      expires_at_slot: BigInt(415_700_000),
      last_checked_slot: BigInt(415_545_000),
      last_successful_tick_at_ms: NOW_MS - BigInt(60 * 60 * 1000),
      last_error: "evaluator terminal: reserve account decode failed",
      created_at_ms: NOW_MS - BigInt(4 * 60 * 60 * 1000),
      updated_at_ms: NOW_MS - BigInt(60 * 60 * 1000),
    },
  ];

  const lifecycle_by_rule_id: Record<string, Stage2RuleLifecycleEvent[]> = {
    [A1_FIXTURE_A_ID]: [
      {
        at_ms: NOW_MS - BigInt(8 * 60 * 60 * 1000),
        label: "created",
        detail: "Rule registered with watcher",
      },
      {
        at_ms: NOW_MS - BigInt(8 * 60 * 60 * 1000) + BigInt(2_000),
        label: "checked_condition_false",
        detail: "Solend APR 11.42% — above threshold 10.00%",
      },
      {
        at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
        label: "checked_condition_false",
        detail: "Solend APR 10.18% — above threshold",
      },
    ],
    [RULE_IDS.condition_met_basket]: [
      {
        at_ms: NOW_MS - BigInt(2 * 60 * 60 * 1000),
        label: "created",
        detail: "Rule registered with watcher",
      },
      {
        at_ms: NOW_MS - BigInt(15_000),
        label: "condition_met",
        detail: "All three Pyth bands cleared. Execution simulated.",
      },
    ],
    [RULE_IDS.executing_solend]: [
      {
        at_ms: NOW_MS - BigInt(20 * 60 * 1000),
        label: "created",
        detail: null,
      },
      {
        at_ms: NOW_MS - BigInt(20_000),
        label: "condition_met",
        detail: "Solend APR 9.81% — gate reached",
      },
      {
        at_ms: NOW_MS - BigInt(8_000),
        label: "executing",
        detail:
          "Executor slice picked up the rule. (Stage 2 substrate slice — no on-chain transaction has been built or sent.)",
      },
    ],
    [RULE_IDS.expired_solend]: [
      {
        at_ms: NOW_MS - BigInt(24 * 60 * 60 * 1000),
        label: "created",
        detail: null,
      },
      {
        at_ms: NOW_MS - BigInt(6 * 60 * 60 * 1000),
        label: "expired",
        detail: "current_slot >= expires_at_slot",
      },
    ],
    [RULE_IDS.revoked_basket]: [
      {
        at_ms: NOW_MS - BigInt(3 * 60 * 60 * 1000),
        label: "created",
        detail: null,
      },
      {
        at_ms: NOW_MS - BigInt(40 * 60 * 1000),
        label: "revoked",
        detail: "Operator revoked rule (Stage 2 substrate — no on-chain action).",
      },
    ],
    [RULE_IDS.failed_solend]: [
      {
        at_ms: NOW_MS - BigInt(4 * 60 * 60 * 1000),
        label: "created",
        detail: null,
      },
      {
        at_ms: NOW_MS - BigInt(60 * 60 * 1000),
        label: "failed_terminal",
        detail: "evaluator terminal: reserve account decode failed",
      },
    ],
  };

  return {
    rules: summaries,
    lifecycle_by_rule_id,
    rule_by_rule_id,
  };
}

function buildHealth(
  mode: Stage2DataMode,
  active_rule_count: bigint,
  in_flight_rule_count: bigint,
): Stage2WatcherHealth {
  if (mode === "offline") {
    return {
      enabled: true,
      last_successful_tick_at_ms: NOW_MS - OFFLINE_TICK_GAP_MS,
      last_tick_started_at_ms: NOW_MS - OFFLINE_TICK_GAP_MS,
      last_tick_finished_at_ms: NOW_MS - OFFLINE_TICK_GAP_MS + BigInt(180),
      last_tick_duration_ms: BigInt(180),
      last_error:
        "no successful tick within offline_threshold_ms — scheduler may be paused",
      active_rule_count,
      stale_rule_count: BigInt(2),
      in_flight_rule_count,
      expired_count_last_tick: 0,
      condition_met_count_last_tick: 0,
      transient_error_count_last_tick: 0,
      terminal_error_count_last_tick: 0,
      failed_count_last_tick: 0,
      offline_threshold_ms: OFFLINE_THRESHOLD_MS,
      stale_threshold_ms: STALE_THRESHOLD_MS,
      is_offline: true,
    };
  }
  if (mode === "errors") {
    return {
      enabled: true,
      last_successful_tick_at_ms: NOW_MS - STALE_TICK_GAP_MS,
      last_tick_started_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
      last_tick_finished_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS + BigInt(420),
      last_tick_duration_ms: BigInt(420),
      last_error:
        "evaluator transient: pyth price update account stale (max_age 30s)",
      active_rule_count,
      stale_rule_count: BigInt(1),
      in_flight_rule_count,
      expired_count_last_tick: 0,
      condition_met_count_last_tick: 0,
      transient_error_count_last_tick: 1,
      terminal_error_count_last_tick: 1,
      failed_count_last_tick: 1,
      offline_threshold_ms: OFFLINE_THRESHOLD_MS,
      stale_threshold_ms: STALE_THRESHOLD_MS,
      is_offline: false,
    };
  }
  // healthy
  return {
    enabled: true,
    last_successful_tick_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
    last_tick_started_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS,
    last_tick_finished_at_ms: NOW_MS - HEALTHY_TICK_GAP_MS + BigInt(140),
    last_tick_duration_ms: BigInt(140),
    last_error: null,
    active_rule_count,
    stale_rule_count: BigInt(0),
    in_flight_rule_count,
    expired_count_last_tick: 0,
    condition_met_count_last_tick: 1,
    transient_error_count_last_tick: 0,
    terminal_error_count_last_tick: 0,
    failed_count_last_tick: 0,
    offline_threshold_ms: OFFLINE_THRESHOLD_MS,
    stale_threshold_ms: STALE_THRESHOLD_MS,
    is_offline: false,
  };
}

/// Mock dashboard adapter. Returns deterministic data — same call
/// always returns the same shape — so the dashboard renders without
/// any backend, RPC, or wallet involvement. Replace with a real
/// adapter in a future Stage 2 slice once the daemon exposes the
/// required HTTP routes.
export function getMockDashboard(mode: Stage2DataMode): Stage2DashboardData {
  const { rules, lifecycle_by_rule_id, rule_by_rule_id } = buildMockRules();

  const active_rule_count = BigInt(
    rules.filter((r) => r.status === "active").length,
  );
  const in_flight_rule_count = BigInt(
    rules.filter((r) => r.status === "executing").length,
  );

  return {
    health: buildHealth(mode, active_rule_count, in_flight_rule_count),
    rules,
    lifecycle_by_rule_id,
    rule_by_rule_id,
  };
}

/// Test/diagnostic counter used by summary cards on the dashboard.
export interface Stage2RuleCounters {
  active: number;
  condition_met: number;
  executing: number;
  completed: number;
  expired: number;
  revoked: number;
  failed: number;
  /// `stale` is sourced from `health.stale_rule_count` — it is a
  /// watcher-side concept (rules that haven't been ticked recently),
  /// not a rule status, so we surface it separately on the dashboard
  /// header rather than mixing it into the per-status counts.
}

export function countRulesByStatus(
  rules: Stage2WatchRuleSummary[],
): Stage2RuleCounters {
  return rules.reduce<Stage2RuleCounters>(
    (acc, r) => {
      acc[r.status] += 1;
      return acc;
    },
    {
      active: 0,
      condition_met: 0,
      executing: 0,
      completed: 0,
      expired: 0,
      revoked: 0,
      failed: 0,
    },
  );
}

// ── Mock-only force tick (dry-run; never calls a backend route) ───────────

/// Result of a dry-run "force tick" applied locally to a single
/// rule. Returns a NEW dashboard payload with the targeted rule's
/// lifecycle/state advanced one step. The original payload is not
/// mutated; the dashboard component owns whichever it shows.
///
/// IMPORTANT: this is a UI-only simulator. It does NOT POST to any
/// backend route, does NOT read RPC, and does NOT touch a wallet.
export function applyMockForceTickDryRun(
  data: Stage2DashboardData,
  rule_id_hex: string,
): { next: Stage2DashboardData; outcome_label: string } {
  const idx = data.rules.findIndex((r) => r.rule_id_hex === rule_id_hex);
  if (idx < 0) {
    return {
      next: data,
      outcome_label: "rule_id not present in mock data",
    };
  }
  const target = data.rules[idx];
  // Mock state machine for the dry-run: active → checked_condition_false
  // (cosmetic), executing → completed (simulated), condition_met →
  // executing. All other states are terminal and the dry-run is a
  // no-op (we still surface a message).
  let nextStatus: Stage2WatchRuleStatus = target.status;
  let event_label = "force_tick_dry_run";
  let event_detail =
    "Dry-run: simulated watcher tick (no backend call, no RPC, no signing).";

  if (target.status === "active") {
    // Cosmetic: just advance last_checked + record the dry-run.
    event_detail =
      "Dry-run: simulated watcher tick observed condition still false.";
  } else if (target.status === "condition_met") {
    nextStatus = "executing";
    event_label = "executing";
    event_detail =
      "Dry-run: simulated executor pickup. Stage 2 substrate slice — no on-chain transaction has been built.";
  } else if (target.status === "executing") {
    nextStatus = "completed";
    event_label = "completed_simulated";
    event_detail =
      "Dry-run: simulated execution success. Stage 2 substrate slice — no funds moved.";
  }

  const updated_at_ms = NOW_MS;
  const nextSummary: Stage2WatchRuleSummary = {
    ...target,
    status: nextStatus,
    last_checked_slot:
      target.last_checked_slot === null
        ? BigInt(415_600_000)
        : target.last_checked_slot + BigInt(1_000),
    last_successful_tick_at_ms: updated_at_ms,
    updated_at_ms,
  };

  const nextRules = [...data.rules];
  nextRules[idx] = nextSummary;

  const prevTimeline =
    data.lifecycle_by_rule_id[rule_id_hex] ?? [];
  const nextTimeline = [
    ...prevTimeline,
    {
      at_ms: updated_at_ms,
      label: event_label,
      detail: event_detail,
    } satisfies Stage2RuleLifecycleEvent,
  ];

  return {
    next: {
      ...data,
      rules: nextRules,
      lifecycle_by_rule_id: {
        ...data.lifecycle_by_rule_id,
        [rule_id_hex]: nextTimeline,
      },
    },
    outcome_label: nextStatus === target.status
      ? `dry-run no-op (status remained ${target.status})`
      : `dry-run advanced ${target.status} → ${nextStatus}`,
  };
}
