// Stage 2 — server-safe deterministic dashboard payload builder.
//
// Produces a JSON-serializable object accepted byte-for-byte by A3's
// `validateApiResponse()` in `frontend/src/lib/stage2-watch-api.ts`.
// Server-only by convention: no React, no client-globals, no
// browser-only APIs. The Next.js App Router route at
// `frontend/src/app/api/stage2/dashboard/route.ts` is the only
// caller in this slice.
//
// SAFETY rules upheld here:
//   - No Solana RPC.
//   - No outbound HTTP / fetch / axios.
//   - No wallet, no signing, no broadcast.
//   - No on-chain program code import.
//   - Pure deterministic data derived from the request wall-clock.
//
// SERIALIZATION rules upheld here:
//   - All u64 / i64 fields (slots, ms-since-epoch, lamports-shaped
//     counts) are emitted as DECIMAL STRINGS so the value cannot
//     drift through `Number.MAX_SAFE_INTEGER`. The A3 validator's
//     `parseBigInt` accepts string | number | bigint, so this is
//     forward-compatible.
//   - u32-shaped per-tick counts are emitted as JSON numbers (their
//     range comfortably fits Number).
//   - Booleans and strings are emitted natively.
//
// DATA SOURCE choice for this slice:
//   - Deterministic demo fallback. There is no in-process state-store
//     accessible from the Next route in this slice; calling out to a
//     hypothetical Rust daemon URL was rejected for security and
//     simplicity. The route's response shape is identical in surface
//     to A3's `getMockDashboard("healthy")`, so the dashboard renders
//     in API mode exactly as it would in mock mode (minus the full
//     canonical preview which requires a deeper deserialiser).

/// Stable rule ids that match A3's mock fixtures, so the dashboard's
/// detail panel selection-by-id continues to work across mode switches.
const RULE_IDS = {
  active_solend: "52554c45000000000000000000000001", // matches A1 Scenario A
  condition_met_basket: "52554c45000000000000000000000002",
  expired_solend: "455850440000000000000000000000a1",
  revoked_basket: "5256504b00000000000000000000000b",
  failed_solend: "464149440000000000000000000000ff",
  executing_solend: "455845430000000000000000000000c0",
} as const;

/// Pinned canonical rule hash for Scenario A — the only A1-pinned
/// hash we surface from the API path. Other rules' hashes are
/// rendered as "(computed)" since their canonical bytes are not
/// ferried over the JSON wire in this slice.
const A1_SCENARIO_A_HASH =
  "5fd3a3b8cfeab17e9985826be642acdd44d726b5395bc6752f58076b97329adc";

/// JSON shape of `Stage2WatcherHealth`. Field names mirror A3's
/// validator exactly; bigint-shaped fields are stringified.
interface HealthJson {
  enabled: boolean;
  last_successful_tick_at_ms: string | null;
  last_tick_started_at_ms: string | null;
  last_tick_finished_at_ms: string | null;
  last_tick_duration_ms: string | null;
  last_error: string | null;
  active_rule_count: string;
  stale_rule_count: string;
  in_flight_rule_count: string;
  expired_count_last_tick: number;
  condition_met_count_last_tick: number;
  transient_error_count_last_tick: number;
  terminal_error_count_last_tick: number;
  failed_count_last_tick: number;
  offline_threshold_ms: string;
  stale_threshold_ms: string;
  is_offline: boolean;
  /// Telemetry-only field. A3's `parseHealth` ignores unknown keys, so
  /// this round-trips into the JSON the operator can inspect via raw
  /// network tab but does NOT widen the typed `Stage2WatcherHealth`.
  /// The user-visible "fallback" signal is encoded in `last_error`.
  daemon_status: "fallback";
}

interface RuleSummaryJson {
  rule_id_hex: string;
  canonical_rule_hash_hex: string;
  status:
    | "active"
    | "condition_met"
    | "executing"
    | "completed"
    | "expired"
    | "revoked"
    | "failed";
  action_type:
    | "solend_withdraw_all_delegated"
    | "jupiter_buy_sol_with_usdc";
  expires_at_slot: string;
  last_checked_slot: string | null;
  last_successful_tick_at_ms: string | null;
  last_error: string | null;
  created_at_ms: string;
  updated_at_ms: string;
}

interface LifecycleEventJson {
  at_ms: string;
  label: string;
  detail: string | null;
}

export interface Stage2DashboardWireShape {
  health: HealthJson;
  rules: RuleSummaryJson[];
  lifecycle_by_rule_id: Record<string, LifecycleEventJson[]>;
  /// Canonical `WatchRule` deep-parse is deferred to a future slice;
  /// emit empty so A3's `validateApiResponse` accepts the payload
  /// (its `rule_by_rule_id` policy is "always {}").
  rule_by_rule_id: Record<string, never>;
}

const FALLBACK_LAST_ERROR =
  "Read-only fallback: backend state-store not yet wired to this API route — serving deterministic demo data. (No on-chain action is triggered from this page.)";

/// Build a deterministic dashboard payload anchored to the supplied
/// wall-clock. Pure: identical inputs → identical bytes.
export function buildFallbackDashboardPayload(
  nowMs: number,
): Stage2DashboardWireShape {
  // ── Slot anchors (mirror A1 Scenario A's pinned slots so the
  //    canonical hash stays valid for the active_solend rule) ────────
  const ACTIVE_EXPIRES_AT_SLOT = "415700000";
  const ACTIVE_LAST_CHECKED_SLOT = "415512000";
  const EXPIRED_AT_SLOT = "415499000";
  const RECENT_LAST_CHECKED_SLOT = "415590000";

  // ── Time deltas (in ms) ────────────────────────────────────────────
  const ms = (n: number) => String(nowMs - n);

  return {
    health: {
      enabled: true,
      last_successful_tick_at_ms: ms(2_000),
      last_tick_started_at_ms: ms(2_000),
      last_tick_finished_at_ms: ms(1_860),
      last_tick_duration_ms: "140",
      last_error: FALLBACK_LAST_ERROR,
      // Counts are u64 in the Rust shape; keep as strings even when
      // small to honour the prompt's "any u64 / quantity → string"
      // rule and to forward-compat with growth.
      active_rule_count: "1",
      stale_rule_count: "0",
      in_flight_rule_count: "1",
      // Per-tick counts are u32 in the Rust shape; stay as numbers.
      expired_count_last_tick: 0,
      condition_met_count_last_tick: 1,
      transient_error_count_last_tick: 0,
      terminal_error_count_last_tick: 0,
      failed_count_last_tick: 0,
      offline_threshold_ms: "60000",
      stale_threshold_ms: "20000",
      is_offline: false,
      daemon_status: "fallback",
    },
    // Six representative rules covering every Stage2WatchRuleStatus
    // mentioned in the brief (active / condition_met / executing /
    // expired / revoked / failed). `completed` is omitted from the
    // demo set because the brief only requires representative
    // statuses; the validator and dashboard handle `completed`
    // unchanged when wired backends emit it.
    rules: [
      {
        rule_id_hex: RULE_IDS.active_solend,
        canonical_rule_hash_hex: A1_SCENARIO_A_HASH,
        status: "active",
        action_type: "solend_withdraw_all_delegated",
        expires_at_slot: ACTIVE_EXPIRES_AT_SLOT,
        last_checked_slot: ACTIVE_LAST_CHECKED_SLOT,
        last_successful_tick_at_ms: ms(2_000),
        last_error: null,
        created_at_ms: ms(8 * 60 * 60 * 1_000),
        updated_at_ms: ms(2_000),
      },
      {
        rule_id_hex: RULE_IDS.condition_met_basket,
        canonical_rule_hash_hex: "(computed)",
        status: "condition_met",
        action_type: "jupiter_buy_sol_with_usdc",
        expires_at_slot: ACTIVE_EXPIRES_AT_SLOT,
        last_checked_slot: "415580000",
        last_successful_tick_at_ms: ms(15_000),
        last_error: null,
        created_at_ms: ms(2 * 60 * 60 * 1_000),
        updated_at_ms: ms(15_000),
      },
      {
        rule_id_hex: RULE_IDS.executing_solend,
        canonical_rule_hash_hex: "(computed)",
        status: "executing",
        action_type: "solend_withdraw_all_delegated",
        expires_at_slot: ACTIVE_EXPIRES_AT_SLOT,
        last_checked_slot: RECENT_LAST_CHECKED_SLOT,
        last_successful_tick_at_ms: ms(8_000),
        last_error: null,
        created_at_ms: ms(20 * 60 * 1_000),
        updated_at_ms: ms(8_000),
      },
      {
        rule_id_hex: RULE_IDS.expired_solend,
        canonical_rule_hash_hex: "(computed)",
        status: "expired",
        action_type: "solend_withdraw_all_delegated",
        expires_at_slot: EXPIRED_AT_SLOT,
        last_checked_slot: "415499500",
        last_successful_tick_at_ms: ms(6 * 60 * 60 * 1_000),
        last_error: null,
        created_at_ms: ms(24 * 60 * 60 * 1_000),
        updated_at_ms: ms(6 * 60 * 60 * 1_000),
      },
      {
        rule_id_hex: RULE_IDS.revoked_basket,
        canonical_rule_hash_hex: "(computed)",
        status: "revoked",
        action_type: "jupiter_buy_sol_with_usdc",
        expires_at_slot: ACTIVE_EXPIRES_AT_SLOT,
        last_checked_slot: "415530000",
        last_successful_tick_at_ms: ms(40 * 60 * 1_000),
        last_error: null,
        created_at_ms: ms(3 * 60 * 60 * 1_000),
        updated_at_ms: ms(40 * 60 * 1_000),
      },
      {
        rule_id_hex: RULE_IDS.failed_solend,
        canonical_rule_hash_hex: "(computed)",
        status: "failed",
        action_type: "solend_withdraw_all_delegated",
        expires_at_slot: ACTIVE_EXPIRES_AT_SLOT,
        last_checked_slot: "415545000",
        last_successful_tick_at_ms: ms(60 * 60 * 1_000),
        last_error: "evaluator terminal: reserve account decode failed",
        created_at_ms: ms(4 * 60 * 60 * 1_000),
        updated_at_ms: ms(60 * 60 * 1_000),
      },
    ],
    lifecycle_by_rule_id: {
      [RULE_IDS.active_solend]: [
        {
          at_ms: ms(8 * 60 * 60 * 1_000),
          label: "created",
          detail: "Rule registered with watcher",
        },
        {
          at_ms: ms(8 * 60 * 60 * 1_000 - 2_000),
          label: "checked_condition_false",
          detail: "Solend APR 11.42% — above threshold 10.00%",
        },
        {
          at_ms: ms(2_000),
          label: "checked_condition_false",
          detail: "Solend APR 10.18% — above threshold",
        },
      ],
      [RULE_IDS.condition_met_basket]: [
        {
          at_ms: ms(2 * 60 * 60 * 1_000),
          label: "created",
          detail: "Rule registered with watcher",
        },
        {
          at_ms: ms(15_000),
          label: "condition_met",
          detail:
            "All three Pyth bands cleared. Execution simulated only — no on-chain transaction has been built.",
        },
      ],
      [RULE_IDS.executing_solend]: [
        {
          at_ms: ms(20 * 60 * 1_000),
          label: "created",
          detail: null,
        },
        {
          at_ms: ms(20_000),
          label: "condition_met",
          detail: "Solend APR 9.81% — gate reached",
        },
        {
          at_ms: ms(8_000),
          label: "executing",
          detail:
            "Executor slice picked up the rule. Stage 2 substrate — no on-chain transaction has been built.",
        },
      ],
      [RULE_IDS.expired_solend]: [
        {
          at_ms: ms(24 * 60 * 60 * 1_000),
          label: "created",
          detail: null,
        },
        {
          at_ms: ms(6 * 60 * 60 * 1_000),
          label: "expired",
          detail: "current_slot >= expires_at_slot",
        },
      ],
      [RULE_IDS.revoked_basket]: [
        {
          at_ms: ms(3 * 60 * 60 * 1_000),
          label: "created",
          detail: null,
        },
        {
          at_ms: ms(40 * 60 * 1_000),
          label: "revoked",
          detail:
            "Operator revoked rule. Stage 2 substrate — no on-chain authorization existed yet.",
        },
      ],
      [RULE_IDS.failed_solend]: [
        {
          at_ms: ms(4 * 60 * 60 * 1_000),
          label: "created",
          detail: null,
        },
        {
          at_ms: ms(60 * 60 * 1_000),
          label: "failed_terminal",
          detail: "evaluator terminal: reserve account decode failed",
        },
      ],
    },
    rule_by_rule_id: {},
  };
}
