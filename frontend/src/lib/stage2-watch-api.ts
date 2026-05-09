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

// ── A3 — read-only API adapter skeleton ───────────────────────────────────
//
// Adds a `Stage2DashboardSource` selector ("mock" | "api") and a unified
// entry point `getStage2DashboardData(options)` that the dashboard page
// calls regardless of which source is active. Mock mode keeps the A2
// behavior intact. API mode attempts a single GET against a future
// read-only endpoint (`/api/stage2/dashboard`) whose route handler does
// NOT yet exist — the call is expected to 404 and the adapter surfaces
// that cleanly via a typed `unavailable` result so the UI can render a
// "Backend pending / API unavailable" banner without crashing.
//
// SAFETY rules upheld in this section:
//   - GET only. No POST / PUT / DELETE.
//   - No mutation, signing, broadcast, transaction submit, RPC.
//   - No credentials, wallet pubkeys, or session secrets in the request.
//   - Endpoint is a relative path on the same origin — no external URL,
//     no Solana RPC URL, no Jupiter / Solend / Pyth RPC.
//   - The fetch call below is the ONLY `fetch(` call site in the
//     dashboard code path. The safety grep gate guards against
//     accidental fan-out.
//
// TODO (route-wiring slice): replace the unavailable-by-default behavior
// once a backend handler at `/api/stage2/dashboard` is added. The wire
// shape consumed by `validateApiResponse` below is the contract that
// future handler must satisfy.

export type Stage2DashboardSource = "mock" | "api";

export interface Stage2DashboardRequest {
  source: Stage2DashboardSource;
  /** Only consulted when `source === "mock"`. */
  mockMode?: Stage2DataMode;
  /** Caller may pass an `AbortSignal` to cancel an in-flight fetch
   *  (e.g. on unmount or rapid refresh). Mock-mode ignores the signal. */
  signal?: AbortSignal;
}

/// Tagged union surfacing the three outcomes the dashboard renders.
/// `fetched_at_ms` is the wall-clock time the adapter resolved — used
/// by the page's "last refreshed" stamp.
export type Stage2DashboardApiResult =
  | {
      kind: "ok";
      data: Stage2DashboardData;
      source: Stage2DashboardSource;
      fetched_at_ms: bigint;
    }
  | {
      kind: "unavailable";
      reason: string;
      httpStatus?: number;
      source: "api";
      fetched_at_ms: bigint;
    }
  | {
      kind: "validation_error";
      reason: string;
      source: "api";
      fetched_at_ms: bigint;
    };

/// Future read-only endpoint. **The backend handler does NOT exist in
/// this slice.** Adding the handler is out of scope; once it lands, the
/// response MUST conform to the wire shape consumed by
/// `validateApiResponse` below.
export const STAGE2_DASHBOARD_API_PATH = "/api/stage2/dashboard";

export async function getStage2DashboardData(
  options: Stage2DashboardRequest,
): Promise<Stage2DashboardApiResult> {
  const fetched_at_ms = BigInt(Date.now());
  if (options.source === "mock") {
    return {
      kind: "ok",
      data: getMockDashboard(options.mockMode ?? "healthy"),
      source: "mock",
      fetched_at_ms,
    };
  }

  // ── source === "api" ────────────────────────────────────────────────
  // Read-only future endpoint; route not added in this slice.
  let res: Response;
  try {
    res = await fetch(STAGE2_DASHBOARD_API_PATH, {
      method: "GET",
      headers: { accept: "application/json" },
      signal: options.signal,
      cache: "no-store",
      credentials: "same-origin",
    });
  } catch (err) {
    // AbortError isn't a true "unavailable" — re-throw so the caller's
    // own cleanup path handles it.
    if (err instanceof DOMException && err.name === "AbortError") {
      throw err;
    }
    return {
      kind: "unavailable",
      reason:
        err instanceof Error
          ? `network error: ${err.message}`
          : "network error",
      source: "api",
      fetched_at_ms,
    };
  }

  if (!res.ok) {
    // Most relevant case in this slice: 404, because the route is
    // intentionally absent. We still surface other non-2xx as
    // unavailable rather than crashing the dashboard.
    return {
      kind: "unavailable",
      reason: `backend returned HTTP ${res.status}`,
      httpStatus: res.status,
      source: "api",
      fetched_at_ms,
    };
  }

  let raw: unknown;
  try {
    raw = await res.json();
  } catch (err) {
    return {
      kind: "validation_error",
      reason:
        err instanceof Error
          ? `response is not valid JSON: ${err.message}`
          : "response is not valid JSON",
      source: "api",
      fetched_at_ms,
    };
  }

  const validated = validateApiResponse(raw);
  if (validated.kind === "error") {
    return {
      kind: "validation_error",
      reason: validated.reason,
      source: "api",
      fetched_at_ms,
    };
  }
  return {
    kind: "ok",
    data: validated.data,
    source: "api",
    fetched_at_ms,
  };
}

// ── API wire shape validator ──────────────────────────────────────────────
//
// The future API will serialise bigints as decimal strings (JSON has no
// native bigint). The validator coerces them back to bigint and refuses
// the payload if any required field is missing / wrong-type / unknown
// enum. The full canonical `WatchRule` shape is intentionally NOT parsed
// here — the dashboard's `<RuleDetailPanel>` already renders a graceful
// "rule not in payload" fallback for any rule whose canonical preview
// can't be drawn. A future slice can teach the validator the full shape.

const KNOWN_RULE_STATUSES: ReadonlySet<Stage2WatchRuleStatus> = new Set<
  Stage2WatchRuleStatus
>(["active", "condition_met", "executing", "completed", "expired", "revoked", "failed"]);

const KNOWN_ACTION_TYPES: ReadonlySet<Stage2WatchRuleSummary["action_type"]> =
  new Set<Stage2WatchRuleSummary["action_type"]>([
    "solend_withdraw_all_delegated",
    "jupiter_buy_sol_with_usdc",
  ]);

type Validated =
  | { kind: "ok"; data: Stage2DashboardData }
  | { kind: "error"; reason: string };

function validateApiResponse(raw: unknown): Validated {
  if (raw === null || typeof raw !== "object") {
    return { kind: "error", reason: "top-level payload is not an object" };
  }
  const obj = raw as Record<string, unknown>;

  const healthResult = parseHealth(obj.health);
  if (healthResult.kind === "error") return healthResult;

  if (!Array.isArray(obj.rules)) {
    return { kind: "error", reason: "`rules` is not an array" };
  }
  const rules: Stage2WatchRuleSummary[] = [];
  for (let i = 0; i < obj.rules.length; i++) {
    const r = parseRuleSummary(obj.rules[i], i);
    if (r.kind === "error") return r;
    rules.push(r.value);
  }

  const lifecycleResult = parseLifecycleMap(obj.lifecycle_by_rule_id);
  if (lifecycleResult.kind === "error") return lifecycleResult;

  return {
    kind: "ok",
    data: {
      health: healthResult.value,
      rules,
      lifecycle_by_rule_id: lifecycleResult.value,
      // Full canonical rule deep-parse deferred to a future slice; the
      // dashboard renders a graceful "not in payload" fallback.
      rule_by_rule_id: {},
    },
  };
}

function parseHealth(
  raw: unknown,
):
  | { kind: "ok"; value: Stage2WatcherHealth }
  | { kind: "error"; reason: string } {
  if (raw === null || typeof raw !== "object") {
    return { kind: "error", reason: "`health` is not an object" };
  }
  const o = raw as Record<string, unknown>;
  if (typeof o.enabled !== "boolean") {
    return { kind: "error", reason: "`health.enabled` is not boolean" };
  }
  if (typeof o.is_offline !== "boolean") {
    return { kind: "error", reason: "`health.is_offline` is not boolean" };
  }
  return {
    kind: "ok",
    value: {
      enabled: o.enabled,
      last_successful_tick_at_ms: parseOptionalBigInt(
        o.last_successful_tick_at_ms,
      ),
      last_tick_started_at_ms: parseOptionalBigInt(o.last_tick_started_at_ms),
      last_tick_finished_at_ms: parseOptionalBigInt(o.last_tick_finished_at_ms),
      last_tick_duration_ms: parseOptionalBigInt(o.last_tick_duration_ms),
      last_error: parseOptionalString(o.last_error),
      active_rule_count: parseBigInt(o.active_rule_count) ?? BigInt(0),
      stale_rule_count: parseBigInt(o.stale_rule_count) ?? BigInt(0),
      in_flight_rule_count: parseBigInt(o.in_flight_rule_count) ?? BigInt(0),
      expired_count_last_tick: parseNumber(o.expired_count_last_tick) ?? 0,
      condition_met_count_last_tick:
        parseNumber(o.condition_met_count_last_tick) ?? 0,
      transient_error_count_last_tick:
        parseNumber(o.transient_error_count_last_tick) ?? 0,
      terminal_error_count_last_tick:
        parseNumber(o.terminal_error_count_last_tick) ?? 0,
      failed_count_last_tick: parseNumber(o.failed_count_last_tick) ?? 0,
      offline_threshold_ms:
        parseBigInt(o.offline_threshold_ms) ?? BigInt(60_000),
      stale_threshold_ms: parseBigInt(o.stale_threshold_ms) ?? BigInt(20_000),
      is_offline: o.is_offline,
    },
  };
}

function parseRuleSummary(
  raw: unknown,
  i: number,
):
  | { kind: "ok"; value: Stage2WatchRuleSummary }
  | { kind: "error"; reason: string } {
  if (raw === null || typeof raw !== "object") {
    return { kind: "error", reason: `rules[${i}] is not an object` };
  }
  const o = raw as Record<string, unknown>;
  const status = o.status;
  if (
    typeof status !== "string" ||
    !KNOWN_RULE_STATUSES.has(status as Stage2WatchRuleStatus)
  ) {
    return {
      kind: "error",
      reason: `rules[${i}].status "${String(status)}" is not a known Stage2WatchRuleStatus`,
    };
  }
  const action_type = o.action_type;
  if (
    typeof action_type !== "string" ||
    !KNOWN_ACTION_TYPES.has(
      action_type as Stage2WatchRuleSummary["action_type"],
    )
  ) {
    return {
      kind: "error",
      reason: `rules[${i}].action_type "${String(action_type)}" is not a known action`,
    };
  }
  if (typeof o.rule_id_hex !== "string") {
    return { kind: "error", reason: `rules[${i}].rule_id_hex is not a string` };
  }
  if (typeof o.canonical_rule_hash_hex !== "string") {
    return {
      kind: "error",
      reason: `rules[${i}].canonical_rule_hash_hex is not a string`,
    };
  }
  return {
    kind: "ok",
    value: {
      rule_id_hex: o.rule_id_hex,
      canonical_rule_hash_hex: o.canonical_rule_hash_hex,
      status: status as Stage2WatchRuleStatus,
      action_type: action_type as Stage2WatchRuleSummary["action_type"],
      expires_at_slot: parseBigInt(o.expires_at_slot) ?? BigInt(0),
      last_checked_slot: parseOptionalBigInt(o.last_checked_slot),
      last_successful_tick_at_ms: parseOptionalBigInt(
        o.last_successful_tick_at_ms,
      ),
      last_error: parseOptionalString(o.last_error),
      created_at_ms: parseBigInt(o.created_at_ms) ?? BigInt(0),
      updated_at_ms: parseBigInt(o.updated_at_ms) ?? BigInt(0),
    },
  };
}

function parseLifecycleMap(
  raw: unknown,
):
  | { kind: "ok"; value: Record<string, Stage2RuleLifecycleEvent[]> }
  | { kind: "error"; reason: string } {
  if (raw === null || raw === undefined) {
    return { kind: "ok", value: {} };
  }
  if (typeof raw !== "object" || Array.isArray(raw)) {
    return {
      kind: "error",
      reason: "`lifecycle_by_rule_id` is not an object",
    };
  }
  const out: Record<string, Stage2RuleLifecycleEvent[]> = {};
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (!Array.isArray(v)) {
      return {
        kind: "error",
        reason: `lifecycle_by_rule_id["${k}"] is not an array`,
      };
    }
    const events: Stage2RuleLifecycleEvent[] = [];
    for (let i = 0; i < v.length; i++) {
      const e = v[i];
      if (e === null || typeof e !== "object") {
        return {
          kind: "error",
          reason: `lifecycle_by_rule_id["${k}"][${i}] is not an object`,
        };
      }
      const eo = e as Record<string, unknown>;
      if (typeof eo.label !== "string") {
        return {
          kind: "error",
          reason: `lifecycle_by_rule_id["${k}"][${i}].label is not a string`,
        };
      }
      events.push({
        at_ms: parseBigInt(eo.at_ms) ?? BigInt(0),
        label: eo.label,
        detail: parseOptionalString(eo.detail),
      });
    }
    out[k] = events;
  }
  return { kind: "ok", value: out };
}

function parseBigInt(v: unknown): bigint | null {
  if (typeof v === "bigint") return v;
  if (typeof v === "number" && Number.isFinite(v))
    return BigInt(Math.trunc(v));
  if (typeof v === "string" && /^-?\d+$/.test(v)) return BigInt(v);
  return null;
}

function parseOptionalBigInt(v: unknown): bigint | null {
  if (v === null || v === undefined) return null;
  return parseBigInt(v);
}

function parseNumber(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && /^-?\d+$/.test(v)) return Number(v);
  return null;
}

function parseOptionalString(v: unknown): string | null {
  if (v === null || v === undefined) return null;
  if (typeof v === "string") return v;
  return null;
}
