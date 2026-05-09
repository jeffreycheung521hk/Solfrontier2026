// Stage 2 — TypeScript canonical WatchRule + Borsh + SHA-256.
//
// Mirrors `crates/types/src/stage2_watch_rule.rs`. The hash bytes
// produced here MUST be byte-identical to what the Rust crate
// produces for the same logical rule — this is the tamper-evidence
// guarantee that ties together the rule the user authored, the
// rule the watcher watches, and the rule the on-chain comparator
// would eventually see (Stage 2 future slices).
//
// # Stage 1 Tail boundary
//
// This is NOT `CanonicalIntent`. Stage 1 Tail commits to a single
// human-approved propose-now action; Stage 2 commits to a longer-
// lived rule whose execution is delegated to a daemon when
// conditions are met. Different schemas, different version
// constants, different hash domains so a Stage 1 hash can never
// be reinterpreted as a Stage 2 rule and vice versa.
//
// # Borsh wire-shape rules
//
// Identical to the Stage 1 Tail rules; see
// `frontend/src/lib/canonical-intent.ts` header for full details.
// The shared `BorshWriter` class lives in that file and is
// re-used here so primitive encodings cannot drift between Stages.
//
// # What this module deliberately does NOT do
//
//   - Does NOT call any network or RPC.
//   - Does NOT build a Solana transaction.
//   - Does NOT sign, broadcast, or submit anything.
//   - Does NOT implement the executor, watcher, or Authorization PDA.
//   - Does NOT execute any conditional action. Stage 2 substrate
//     here is a tamper-evident hash + canonical preview only.

// Relative path (not the `@/...` alias) so Node's TS strip-types can
// resolve this import directly when running the verification script.
// Next.js / tsc resolve relative paths identically.
import {
  BorshWriter,
  bytesToHex,
  PubkeyBytes,
} from "./canonical-intent.ts";

/// Stage 2 schema version — first byte of every canonical rule.
export const STAGE2_WATCH_RULE_SCHEMA_VERSION = 1 as const;

export const MAX_CONDITIONS = 8;
export const MIN_CONDITIONS = 1;

// ── Discriminators ─────────────────────────────────────────────────────────

/// How `conditions[..]` are combined. Borsh tag: All=0, Any=1.
export type ConditionLogic = "all" | "any";
const CONDITION_LOGIC_TAGS: Record<ConditionLogic, number> = {
  all: 0,
  any: 1,
};

/// Comparison operator for numeric thresholds. Borsh tag:
/// Lt=0, Lte=1, Gt=2, Gte=3.
export type Comparison = "lt" | "lte" | "gt" | "gte";
const COMPARISON_TAGS: Record<Comparison, number> = {
  lt: 0,
  lte: 1,
  gt: 2,
  gte: 3,
};

/// Pyth required verification level. Borsh tag: Full=0. (V1 only
/// supports Full per Stage 2 spec § 7.1.)
export type VerificationLevel = "full";
const VERIFICATION_LEVEL_TAGS: Record<VerificationLevel, number> = {
  full: 0,
};

/// Adverse-bound discipline for Pyth price + confidence. Borsh tag:
/// AdverseLowerForGt=0, AdverseUpperForLt=1, Midpoint=2.
export type BoundMode =
  | "adverse_lower_for_gt"
  | "adverse_upper_for_lt"
  | "midpoint";
const BOUND_MODE_TAGS: Record<BoundMode, number> = {
  adverse_lower_for_gt: 0,
  adverse_upper_for_lt: 1,
  midpoint: 2,
};

/// Solend rate kind. Borsh tag: Apr=0, Apy=1.
export type RateKind = "apr" | "apy";
const RATE_KIND_TAGS: Record<RateKind, number> = {
  apr: 0,
  apy: 1,
};

/// Solend withdraw mode. Borsh tag: WithdrawAllDelegatedPosition=0.
export type WithdrawMode = "withdraw_all_delegated_position";
const WITHDRAW_MODE_TAGS: Record<WithdrawMode, number> = {
  withdraw_all_delegated_position: 0,
};

/// Jupiter API version pin. Borsh tag: V1=0, V2=1.
export type JupiterApiVersion = "v1" | "v2";
const JUPITER_API_VERSION_TAGS: Record<JupiterApiVersion, number> = {
  v1: 0,
  v2: 1,
};

// ── Conditions ─────────────────────────────────────────────────────────────

export type Condition =
  | {
      kind: "pyth_price";
      feed_id: Uint8Array; // 32 bytes
      price_update_account: PubkeyBytes;
      comparison: Comparison;
      threshold_mantissa: bigint; // i64
      threshold_exponent: number; // i32
      max_age_seconds: number; // u32
      max_confidence_bps: number; // u16
      verification_level_required: VerificationLevel;
      bound_mode: BoundMode;
    }
  | {
      kind: "solend_reserve_supply_rate";
      reserve_pubkey: PubkeyBytes;
      lending_market: PubkeyBytes;
      solend_program_id: PubkeyBytes;
      comparison: Comparison;
      threshold_bps: number; // u32
      rate_kind: RateKind;
      formula_version: number; // u8
      max_reserve_staleness_slots: number; // u32
      required_refresh_same_tx: boolean;
    };

const CONDITION_TAGS: Record<Condition["kind"], number> = {
  pyth_price: 0,
  solend_reserve_supply_rate: 1,
};

// ── Action ─────────────────────────────────────────────────────────────────

export type ActionSpec =
  | {
      kind: "solend_withdraw_all_delegated";
      target_obligation: PubkeyBytes;
      reserve_pubkey: PubkeyBytes;
      lending_market: PubkeyBytes;
      destination_wallet: PubkeyBytes;
      withdraw_mode: WithdrawMode;
    }
  | {
      kind: "jupiter_buy_sol_with_usdc";
      input_mint: PubkeyBytes;
      output_mint: PubkeyBytes;
      input_amount_raw: bigint;
      /** `Some(n)` pins the floor exactly; `null` (None) defers to
       *  `slippage_bps`. Borsh `Option<u64>` = 1-byte tag + 8 bytes. */
      min_output_amount_raw: bigint | null;
      jupiter_api_version: JupiterApiVersion;
      max_accounts_hint: number; // u8
      require_pre_post_bracket: boolean;
    };

const ACTION_TAGS: Record<ActionSpec["kind"], number> = {
  solend_withdraw_all_delegated: 0,
  jupiter_buy_sol_with_usdc: 1,
};

// ── WatchRule ──────────────────────────────────────────────────────────────

export interface WatchRule {
  schema_version: number;
  rule_id: Uint8Array; // 16 bytes
  user: PubkeyBytes;
  executor: PubkeyBytes;
  delegated_wallet: PubkeyBytes;
  created_at_slot: bigint;
  expires_at_slot: bigint;
  one_shot: boolean;
  condition_logic: ConditionLogic;
  conditions: Condition[];
  action: ActionSpec;
  max_input_amount_raw: bigint;
  used_amount_raw: bigint;
  destination: PubkeyBytes;
  slippage_bps: number; // u16
}

// ── Borsh encoder ──────────────────────────────────────────────────────────

function encodePubkey(w: BorshWriter, pk: PubkeyBytes): void {
  w.writeFixedBytes(pk.bytes, 32);
}

function encodeCondition(w: BorshWriter, c: Condition): void {
  w.writeU8(CONDITION_TAGS[c.kind]);
  switch (c.kind) {
    case "pyth_price":
      w.writeFixedBytes(c.feed_id, 32);
      encodePubkey(w, c.price_update_account);
      w.writeU8(COMPARISON_TAGS[c.comparison]);
      w.writeI64(c.threshold_mantissa);
      w.writeI32(c.threshold_exponent);
      w.writeU32(c.max_age_seconds);
      w.writeU16(c.max_confidence_bps);
      w.writeU8(VERIFICATION_LEVEL_TAGS[c.verification_level_required]);
      w.writeU8(BOUND_MODE_TAGS[c.bound_mode]);
      return;
    case "solend_reserve_supply_rate":
      encodePubkey(w, c.reserve_pubkey);
      encodePubkey(w, c.lending_market);
      encodePubkey(w, c.solend_program_id);
      w.writeU8(COMPARISON_TAGS[c.comparison]);
      w.writeU32(c.threshold_bps);
      w.writeU8(RATE_KIND_TAGS[c.rate_kind]);
      w.writeU8(c.formula_version);
      w.writeU32(c.max_reserve_staleness_slots);
      w.writeBool(c.required_refresh_same_tx);
      return;
    default: {
      const _exhaustive: never = c;
      return _exhaustive;
    }
  }
}

function encodeAction(w: BorshWriter, a: ActionSpec): void {
  w.writeU8(ACTION_TAGS[a.kind]);
  switch (a.kind) {
    case "solend_withdraw_all_delegated":
      encodePubkey(w, a.target_obligation);
      encodePubkey(w, a.reserve_pubkey);
      encodePubkey(w, a.lending_market);
      encodePubkey(w, a.destination_wallet);
      w.writeU8(WITHDRAW_MODE_TAGS[a.withdraw_mode]);
      return;
    case "jupiter_buy_sol_with_usdc":
      encodePubkey(w, a.input_mint);
      encodePubkey(w, a.output_mint);
      w.writeU64(a.input_amount_raw);
      // Option<u64> = 1-byte tag (0=None, 1=Some) + value if Some.
      if (a.min_output_amount_raw === null) {
        w.writeU8(0);
      } else {
        w.writeU8(1);
        w.writeU64(a.min_output_amount_raw);
      }
      w.writeU8(JUPITER_API_VERSION_TAGS[a.jupiter_api_version]);
      w.writeU8(a.max_accounts_hint);
      w.writeBool(a.require_pre_post_bracket);
      return;
    default: {
      const _exhaustive: never = a;
      return _exhaustive;
    }
  }
}

/// Borsh-encode the rule into its canonical byte form. Field order
/// matches the Rust source (the only authoritative declaration order
/// for the Borsh layout).
export function canonicalRuleBytes(rule: WatchRule): Uint8Array {
  if (rule.rule_id.length !== 16) {
    throw new Error(
      `canonicalRuleBytes: rule_id must be exactly 16 bytes, got ${rule.rule_id.length}`,
    );
  }
  if (rule.conditions.length < MIN_CONDITIONS) {
    throw new Error(
      `canonicalRuleBytes: conditions empty (need ${MIN_CONDITIONS}+)`,
    );
  }
  if (rule.conditions.length > MAX_CONDITIONS) {
    throw new Error(
      `canonicalRuleBytes: conditions over cap (${rule.conditions.length} > ${MAX_CONDITIONS})`,
    );
  }
  const w = new BorshWriter();
  w.writeU8(rule.schema_version);
  w.writeFixedBytes(rule.rule_id, 16);
  encodePubkey(w, rule.user);
  encodePubkey(w, rule.executor);
  encodePubkey(w, rule.delegated_wallet);
  w.writeU64(rule.created_at_slot);
  w.writeU64(rule.expires_at_slot);
  w.writeBool(rule.one_shot);
  w.writeU8(CONDITION_LOGIC_TAGS[rule.condition_logic]);
  // Vec<T> = u32 LE length + elements in order.
  w.writeU32(rule.conditions.length);
  for (const c of rule.conditions) {
    encodeCondition(w, c);
  }
  encodeAction(w, rule.action);
  w.writeU64(rule.max_input_amount_raw);
  w.writeU64(rule.used_amount_raw);
  encodePubkey(w, rule.destination);
  w.writeU16(rule.slippage_bps);
  return w.toBytes();
}

// ── SHA-256 ────────────────────────────────────────────────────────────────

function getSubtle(): SubtleCrypto {
  const c = (globalThis as unknown as { crypto?: { subtle?: SubtleCrypto } }).crypto;
  if (!c?.subtle) {
    throw new Error(
      "Web Crypto API (crypto.subtle) not available — required for Stage 2 SHA-256",
    );
  }
  return c.subtle;
}

/// SHA-256 of the canonical Borsh bytes. Lowercase hex.
export async function canonicalRuleHash(rule: WatchRule): Promise<string> {
  const bytes = canonicalRuleBytes(rule);
  const digest = await getSubtle().digest(
    "SHA-256",
    bytes as unknown as BufferSource,
  );
  return bytesToHex(new Uint8Array(digest));
}

// ── Hash mismatch verification ─────────────────────────────────────────────

export interface RuleHashVerificationResult {
  computed_hash_hex: string;
  expected_hash_hex: string;
  matches: boolean;
}

export async function verifyRuleHash(
  rule: WatchRule,
  expectedHashHex: string,
): Promise<RuleHashVerificationResult> {
  const computed = await canonicalRuleHash(rule);
  const expected = expectedHashHex.toLowerCase();
  return {
    computed_hash_hex: computed,
    expected_hash_hex: expected,
    matches: computed === expected,
  };
}

// ── Fixtures (mirror `crates/types/src/stage2_watch_rule.rs` tests) ───────

const FIXTURE_TEST_USER = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
const FIXTURE_USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const FIXTURE_WSOL_MINT = "So11111111111111111111111111111111111111112";
const FIXTURE_SOLEND_USDC_RESERVE =
  "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const FIXTURE_SOLEND_LENDING_MARKET =
  "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const FIXTURE_SOLEND_PROGRAM_ID =
  "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

/// Bytes "RULE" + 11 zeros + 0x01 — same as Rust `FIXTURE_RULE_ID`.
const FIXTURE_RULE_ID = new Uint8Array([
  0x52, 0x55, 0x4c, 0x45, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
]);

const FIXTURE_CREATED_AT = BigInt(415_500_000);
const FIXTURE_EXPIRES_AT = BigInt(415_700_000);

/// 32-byte filler pubkey — mirrors the Rust test helper `pk(b: u8)
/// → PubkeyBytes::new([b; 32])`. Used for the synthetic delegated
/// executor / delegated wallet / target obligation / Pyth price-
/// update accounts in the fixtures.
function pkFill(byte: number): PubkeyBytes {
  return PubkeyBytes.fromBytes(new Uint8Array(32).fill(byte));
}

/// Stable Pyth feed ids (mainnet, public chain data — committing
/// them to source is fine; matches the Rust fixture file exactly).
const BTC_USD_FEED_ID = new Uint8Array([
  0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1,
  0x2d, 0xe5, 0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe,
  0xdf, 0x0f, 0x4a, 0x41, 0x5b, 0x43,
]);
const ETH_USD_FEED_ID = new Uint8Array([
  0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd,
  0x1b, 0x64, 0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80,
  0x87, 0x46, 0x34, 0xfd, 0x0a, 0xce,
]);
const SOL_USD_FEED_ID = new Uint8Array([
  0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95,
  0xd1, 0xda, 0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c,
  0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
]);

/// Expected canonical rule hashes (hex, lowercase) committed alongside
/// the Rust crate fixtures. Frontend MUST reproduce these byte-for-byte.
export const EXPECTED_FIXTURE_RULE_HASHES = {
  scenario_a_solend_apr_below_10:
    "5fd3a3b8cfeab17e9985826be642acdd44d726b5395bc6752f58076b97329adc",
  scenario_b_basket_buy_sol:
    "ee8a1edb91a6c7b3d3c023adbdfa9df48901ccb71b1445a630a2d1038b48b7bb",
} as const;

export function fixtureScenarioASolendAprBelow10(): WatchRule {
  return {
    schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
    rule_id: FIXTURE_RULE_ID,
    user: PubkeyBytes.fromBase58(FIXTURE_TEST_USER),
    executor: pkFill(0x02),
    delegated_wallet: pkFill(0x03),
    created_at_slot: FIXTURE_CREATED_AT,
    expires_at_slot: FIXTURE_EXPIRES_AT,
    one_shot: true,
    condition_logic: "all",
    conditions: [
      {
        kind: "solend_reserve_supply_rate",
        reserve_pubkey: PubkeyBytes.fromBase58(FIXTURE_SOLEND_USDC_RESERVE),
        lending_market: PubkeyBytes.fromBase58(FIXTURE_SOLEND_LENDING_MARKET),
        solend_program_id: PubkeyBytes.fromBase58(FIXTURE_SOLEND_PROGRAM_ID),
        comparison: "lt",
        threshold_bps: 1_000, // 10.00%
        rate_kind: "apr",
        formula_version: 1,
        max_reserve_staleness_slots: 16,
        required_refresh_same_tx: true,
      },
    ],
    action: {
      kind: "solend_withdraw_all_delegated",
      target_obligation: pkFill(0x04),
      reserve_pubkey: PubkeyBytes.fromBase58(FIXTURE_SOLEND_USDC_RESERVE),
      lending_market: PubkeyBytes.fromBase58(FIXTURE_SOLEND_LENDING_MARKET),
      destination_wallet: PubkeyBytes.fromBase58(FIXTURE_TEST_USER),
      withdraw_mode: "withdraw_all_delegated_position",
    },
    max_input_amount_raw: BigInt(5_000_000),
    used_amount_raw: BigInt(0),
    destination: PubkeyBytes.fromBase58(FIXTURE_TEST_USER),
    slippage_bps: 0,
  };
}

export function fixtureScenarioBBasketBuySol(): WatchRule {
  return {
    schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
    rule_id: FIXTURE_RULE_ID,
    user: PubkeyBytes.fromBase58(FIXTURE_TEST_USER),
    executor: pkFill(0x02),
    delegated_wallet: pkFill(0x03),
    created_at_slot: FIXTURE_CREATED_AT,
    expires_at_slot: FIXTURE_EXPIRES_AT,
    one_shot: true,
    condition_logic: "all",
    conditions: [
      {
        kind: "pyth_price",
        feed_id: BTC_USD_FEED_ID,
        price_update_account: pkFill(0x10),
        comparison: "gt",
        threshold_mantissa: BigInt(7_500_000), // 75000.00
        threshold_exponent: -2,
        max_age_seconds: 30,
        max_confidence_bps: 50,
        verification_level_required: "full",
        bound_mode: "adverse_lower_for_gt",
      },
      {
        kind: "pyth_price",
        feed_id: ETH_USD_FEED_ID,
        price_update_account: pkFill(0x11),
        comparison: "gt",
        threshold_mantissa: BigInt(230_000), // 2300.00
        threshold_exponent: -2,
        max_age_seconds: 30,
        max_confidence_bps: 50,
        verification_level_required: "full",
        bound_mode: "adverse_lower_for_gt",
      },
      {
        kind: "pyth_price",
        feed_id: SOL_USD_FEED_ID,
        price_update_account: pkFill(0x12),
        comparison: "lt",
        threshold_mantissa: BigInt(9_000), // 90.00
        threshold_exponent: -2,
        max_age_seconds: 30,
        max_confidence_bps: 50,
        verification_level_required: "full",
        bound_mode: "adverse_upper_for_lt",
      },
    ],
    action: {
      kind: "jupiter_buy_sol_with_usdc",
      input_mint: PubkeyBytes.fromBase58(FIXTURE_USDC_MINT),
      output_mint: PubkeyBytes.fromBase58(FIXTURE_WSOL_MINT),
      input_amount_raw: BigInt(5_000_000),
      min_output_amount_raw: null,
      jupiter_api_version: "v2",
      max_accounts_hint: 48,
      require_pre_post_bracket: true,
    },
    max_input_amount_raw: BigInt(5_000_000),
    used_amount_raw: BigInt(0),
    destination: PubkeyBytes.fromBase58(FIXTURE_TEST_USER),
    slippage_bps: 50,
  };
}

// ── Fixture parity check (used by verify-stage2-watch-rule.ts) ────────────

export interface RuleFixtureParityReport {
  results: Array<{
    name: keyof typeof EXPECTED_FIXTURE_RULE_HASHES;
    computed: string;
    expected: string;
    match: boolean;
  }>;
  all_match: boolean;
}

export async function runRuleFixtureParityCheck(): Promise<RuleFixtureParityReport> {
  const cases: Array<[
    keyof typeof EXPECTED_FIXTURE_RULE_HASHES,
    () => WatchRule,
  ]> = [
    ["scenario_a_solend_apr_below_10", fixtureScenarioASolendAprBelow10],
    ["scenario_b_basket_buy_sol", fixtureScenarioBBasketBuySol],
  ];

  const results = await Promise.all(
    cases.map(async ([name, build]) => {
      const computed = await canonicalRuleHash(build());
      const expected = EXPECTED_FIXTURE_RULE_HASHES[name];
      return { name, computed, expected, match: computed === expected };
    }),
  );

  return { results, all_match: results.every((r) => r.match) };
}
