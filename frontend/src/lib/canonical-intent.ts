// Stage 1 Tail — TypeScript canonical intent + Borsh + SHA-256.
//
// Mirrors the Rust types in `crates/types/src/canonical_intent.rs`. The
// hash bytes produced here MUST be byte-identical to what the Rust
// crate produces for the same logical intent — this is the
// tamper-evidence guarantee that ties together chat → approval page →
// Phantom signing → daemon submit.
//
// # Borsh wire-shape rules (source: https://borsh.io/)
//
//   - `u8`         → 1 raw byte
//   - `u16`        → 2 bytes, little-endian
//   - `u64`        → 8 bytes, little-endian
//   - `[u8; N]`    → N raw bytes (no length prefix)
//   - `struct`     → fields concatenated in declaration order
//   - `enum`       → 1 raw byte (u8 ordinal, 0-indexed by declaration
//                    order) followed by the variant's fields in
//                    declaration order
//
// Borsh is deterministic by construction: no map iteration order, no
// floats, no enum discriminant ambiguity. Two callers running the
// same schema on the same intent always produce byte-identical output.
//
// # Hash construction
//
//   canonical_bytes = borsh_encode(intent)
//   canonical_hash  = SHA-256(canonical_bytes)        // hex-lowercase
//
// SHA-256 is computed via the Web Crypto API (`crypto.subtle.digest`)
// — available in modern browsers and in Node 16+. Anything that wants
// to verify the hash MUST hash the canonical bytes, NOT JSON.
//
// # What this module deliberately does NOT do
//
//   - Does NOT call any network or RPC.
//   - Does NOT build a Solana transaction.
//   - Does NOT sign, broadcast, or submit anything.
//   - Does NOT introduce conditions, scheduling, or execute-later
//     semantics. Stage 1 Tail is "user instructs now, user signs now".

import { PublicKey } from "@solana/web3.js";

/// Schema version embedded as the FIRST byte of every canonical intent's
/// Borsh layout (so a version bump produces a distinct hash domain and
/// downstream consumers fail closed on mismatch). Mirrors
/// `STAGE1_TAIL_SCHEMA_VERSION` in the Rust crate.
export const STAGE1_TAIL_SCHEMA_VERSION = 1 as const;

// ── PubkeyBytes ────────────────────────────────────────────────────────────

/// 32-byte Solana pubkey, deterministic Borsh layout (32 raw bytes).
/// Stored internally as base58 + a memoised raw-bytes cache so the UI
/// can render `wallet.toBase58()` without going through the byte form.
export class PubkeyBytes {
  /** Base58 form for human display. */
  readonly base58: string;
  /** 32 raw bytes for Borsh emission and on-chain comparison. */
  readonly bytes: Uint8Array;

  private constructor(base58: string, bytes: Uint8Array) {
    if (bytes.length !== 32) {
      throw new Error(`PubkeyBytes must be exactly 32 bytes, got ${bytes.length}`);
    }
    this.base58 = base58;
    this.bytes = bytes;
  }

  /** Parse a base58 pubkey string. Throws on invalid base58 or wrong length. */
  static fromBase58(s: string): PubkeyBytes {
    // `@solana/web3.js`'s PublicKey constructor handles base58 validation
    // and length enforcement via the bs58 + bn dependencies it already
    // pulls in. We do not re-implement bs58 here.
    const pk = new PublicKey(s);
    return new PubkeyBytes(pk.toBase58(), pk.toBytes());
  }

  /** Construct from raw 32 bytes. */
  static fromBytes(bytes: Uint8Array): PubkeyBytes {
    if (bytes.length !== 32) {
      throw new Error(`PubkeyBytes must be exactly 32 bytes, got ${bytes.length}`);
    }
    const pk = new PublicKey(bytes);
    return new PubkeyBytes(pk.toBase58(), new Uint8Array(bytes));
  }

  toString(): string {
    return this.base58;
  }
}

// ── Action variants ─────────────────────────────────────────────────────────
//
// IntentAction maps to a Borsh enum. The `kind` discriminant in TS is
// the variant name; the Borsh on-the-wire ordinal is fixed by the
// declaration order in the Rust source (NOT alphabetical, NOT
// JSON `tag` order):
//
//   0 → SolendDeposit
//   1 → SolendWithdrawAll
//   2 → JupiterSwap
//
// `INTENT_ACTION_TAGS` is the explicit table the encoder reads so a
// future re-ordering or addition is a one-line change at the table,
// not a fragile lookup-by-string.

export type IntentAction =
  | {
      kind: "solend_deposit";
      wallet_pubkey: PubkeyBytes;
      input_mint: PubkeyBytes;
      reserve_pubkey: PubkeyBytes;
      lending_market: PubkeyBytes;
      amount_raw: bigint;
      amount_decimals: number;
    }
  | {
      kind: "solend_withdraw_all";
      wallet_pubkey: PubkeyBytes;
      obligation_pubkey: PubkeyBytes;
      reserve_pubkey: PubkeyBytes;
      lending_market: PubkeyBytes;
    }
  | {
      kind: "jupiter_swap";
      wallet_pubkey: PubkeyBytes;
      input_mint: PubkeyBytes;
      output_mint: PubkeyBytes;
      input_amount_raw: bigint;
      slippage_bps: number;
    };

const INTENT_ACTION_TAGS: Record<IntentAction["kind"], number> = {
  solend_deposit: 0,
  solend_withdraw_all: 1,
  jupiter_swap: 2,
};

// ── CanonicalIntent ─────────────────────────────────────────────────────────

export interface CanonicalIntent {
  /** Currently always `STAGE1_TAIL_SCHEMA_VERSION` (1). */
  schema_version: number;
  /** Exactly 16 bytes. */
  intent_id: Uint8Array;
  /** Wallet the action will target. */
  user: PubkeyBytes;
  action: IntentAction;
  /** Solana slot at/after which this intent is considered expired. */
  expires_at_slot: bigint;
}

// ── Borsh encoder (minimal, hand-rolled) ────────────────────────────────────
//
// We implement only the four primitives + struct + enum patterns this
// schema requires. A new dependency would be overkill for this surface
// area; the canonical-intent.rs schema is intentionally closed.

export class BorshWriter {
  private chunks: number[] = [];

  private push(byte: number): void {
    this.chunks.push(byte & 0xff);
  }

  /** u8 — 1 raw byte. */
  writeU8(n: number): void {
    if (!Number.isInteger(n) || n < 0 || n > 0xff) {
      throw new Error(`writeU8: out-of-range value ${n}`);
    }
    this.push(n);
  }

  /** Borsh `bool` — 1 raw byte (`0x00` for false, `0x01` for true). */
  writeBool(b: boolean): void {
    this.push(b ? 1 : 0);
  }

  /** u16 — 2 bytes, little-endian. */
  writeU16(n: number): void {
    if (!Number.isInteger(n) || n < 0 || n > 0xffff) {
      throw new Error(`writeU16: out-of-range value ${n}`);
    }
    this.push(n & 0xff);
    this.push((n >> 8) & 0xff);
  }

  /** u32 — 4 bytes, little-endian. */
  writeU32(n: number): void {
    if (!Number.isInteger(n) || n < 0 || n > 0xffffffff) {
      throw new Error(`writeU32: out-of-range value ${n}`);
    }
    this.push(n & 0xff);
    this.push((n >>> 8) & 0xff);
    this.push((n >>> 16) & 0xff);
    this.push((n >>> 24) & 0xff);
  }

  /** i32 — 4 bytes, little-endian, two's complement. */
  writeI32(n: number): void {
    if (!Number.isInteger(n) || n < -0x80000000 || n > 0x7fffffff) {
      throw new Error(`writeI32: out-of-range value ${n}`);
    }
    // Two's complement = unsigned wraparound for the same bit pattern.
    const u = n < 0 ? n + 0x100000000 : n;
    this.push(u & 0xff);
    this.push((u >>> 8) & 0xff);
    this.push((u >>> 16) & 0xff);
    this.push((u >>> 24) & 0xff);
  }

  /** u64 — 8 bytes, little-endian. Accepts bigint OR safe-integer Number. */
  writeU64(n: bigint | number): void {
    let v: bigint;
    if (typeof n === "number") {
      if (!Number.isInteger(n) || n < 0) {
        throw new Error(`writeU64: out-of-range value ${n}`);
      }
      v = BigInt(n);
    } else {
      v = n;
    }
    const MAX_U64 = BigInt("18446744073709551615"); // 2^64 - 1
    if (v < BigInt(0) || v > MAX_U64) {
      throw new Error(`writeU64: out-of-range value ${v}`);
    }
    const MASK = BigInt(0xff);
    for (let i = 0; i < 8; i++) {
      this.push(Number(v & MASK));
      v >>= BigInt(8);
    }
  }

  /** i64 — 8 bytes, little-endian, two's complement. Accepts bigint
   *  OR safe-integer Number. */
  writeI64(n: bigint | number): void {
    let v: bigint = typeof n === "number" ? BigInt(n) : n;
    const MIN_I64 = BigInt("-9223372036854775808");
    const MAX_I64 = BigInt("9223372036854775807");
    if (v < MIN_I64 || v > MAX_I64) {
      throw new Error(`writeI64: out-of-range value ${v}`);
    }
    // Two's complement: negative numbers wrap to (2^64 + v).
    if (v < BigInt(0)) {
      v = BigInt("18446744073709551616") + v;
    }
    const MASK = BigInt(0xff);
    for (let i = 0; i < 8; i++) {
      this.push(Number(v & MASK));
      v >>= BigInt(8);
    }
  }

  /** Fixed-size byte array — N raw bytes, no length prefix. */
  writeFixedBytes(bytes: Uint8Array, expectedLen: number): void {
    if (bytes.length !== expectedLen) {
      throw new Error(
        `writeFixedBytes: expected ${expectedLen} bytes, got ${bytes.length}`,
      );
    }
    for (let i = 0; i < bytes.length; i++) {
      this.push(bytes[i]);
    }
  }

  toBytes(): Uint8Array {
    return new Uint8Array(this.chunks);
  }
}

function encodePubkey(w: BorshWriter, pk: PubkeyBytes): void {
  w.writeFixedBytes(pk.bytes, 32);
}

function encodeAction(w: BorshWriter, action: IntentAction): void {
  // Enum discriminant — u8 ordinal in Rust declaration order.
  w.writeU8(INTENT_ACTION_TAGS[action.kind]);
  switch (action.kind) {
    case "solend_deposit":
      encodePubkey(w, action.wallet_pubkey);
      encodePubkey(w, action.input_mint);
      encodePubkey(w, action.reserve_pubkey);
      encodePubkey(w, action.lending_market);
      w.writeU64(action.amount_raw);
      w.writeU8(action.amount_decimals);
      return;
    case "solend_withdraw_all":
      encodePubkey(w, action.wallet_pubkey);
      encodePubkey(w, action.obligation_pubkey);
      encodePubkey(w, action.reserve_pubkey);
      encodePubkey(w, action.lending_market);
      return;
    case "jupiter_swap":
      encodePubkey(w, action.wallet_pubkey);
      encodePubkey(w, action.input_mint);
      encodePubkey(w, action.output_mint);
      w.writeU64(action.input_amount_raw);
      w.writeU16(action.slippage_bps);
      return;
    default: {
      // Exhaustiveness check: a new variant must be added to
      // INTENT_ACTION_TAGS *and* this switch, or compilation fails.
      const _exhaustive: never = action;
      return _exhaustive;
    }
  }
}

/// Borsh-encode a `CanonicalIntent` into its canonical byte form.
/// Field order is fixed by the Rust source (and by Borsh's
/// declaration-order rule): schema_version, intent_id, user, action,
/// expires_at_slot.
export function canonicalBytes(intent: CanonicalIntent): Uint8Array {
  if (intent.intent_id.length !== 16) {
    throw new Error(
      `canonicalBytes: intent_id must be exactly 16 bytes, got ${intent.intent_id.length}`,
    );
  }
  const w = new BorshWriter();
  w.writeU8(intent.schema_version);
  w.writeFixedBytes(intent.intent_id, 16);
  encodePubkey(w, intent.user);
  encodeAction(w, intent.action);
  w.writeU64(intent.expires_at_slot);
  return w.toBytes();
}

// ── SHA-256 hash ────────────────────────────────────────────────────────────

function hexEncode(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i++) {
    s += bytes[i].toString(16).padStart(2, "0");
  }
  return s;
}

/// Browser/Node-compatible SubtleCrypto handle. The Web Crypto API is
/// available unconditionally on `globalThis.crypto.subtle` in modern
/// browsers and in Node ≥ 16.
function getSubtle(): SubtleCrypto {
  const c = (globalThis as unknown as { crypto?: { subtle?: SubtleCrypto } }).crypto;
  if (!c?.subtle) {
    throw new Error(
      "Web Crypto API (crypto.subtle) not available — required for canonical-intent SHA-256",
    );
  }
  return c.subtle;
}

/// SHA-256 of the canonical Borsh bytes. Returns lowercase hex.
export async function canonicalHash(intent: CanonicalIntent): Promise<string> {
  const bytes = canonicalBytes(intent);
  // Cast to BufferSource explicitly: TS lib types tightened the
  // SubtleCrypto.digest signature, so a plain `Uint8Array` is not
  // assignable directly even though every browser/Node accepts it.
  const digest = await getSubtle().digest(
    "SHA-256",
    bytes as unknown as BufferSource,
  );
  return hexEncode(new Uint8Array(digest));
}

// ── Expiry validation ──────────────────────────────────────────────────────

export type IntentExpiryStatus =
  | { state: "valid"; current_slot: bigint; expires_at_slot: bigint }
  | { state: "expired"; current_slot: bigint; expires_at_slot: bigint }
  | { state: "unknown"; expires_at_slot: bigint };

/// Mirrors `validate_not_expired` in the Rust crate. The boundary is
/// exclusive: an intent at `expires_at_slot` is already expired.
export function checkIntentExpiry(
  intent: CanonicalIntent,
  current_slot: bigint | null | undefined,
): IntentExpiryStatus {
  if (current_slot === null || current_slot === undefined) {
    return { state: "unknown", expires_at_slot: intent.expires_at_slot };
  }
  if (current_slot < intent.expires_at_slot) {
    return {
      state: "valid",
      current_slot,
      expires_at_slot: intent.expires_at_slot,
    };
  }
  return {
    state: "expired",
    current_slot,
    expires_at_slot: intent.expires_at_slot,
  };
}

// ── Hash mismatch refusal ──────────────────────────────────────────────────

export interface HashVerificationResult {
  /** The hash the frontend recomputed locally, hex-lowercase. */
  computed_hash_hex: string;
  /** The hash the backend / pre-existing artifact claims, hex-lowercase
   *  (caller should normalise to lowercase before passing in). */
  expected_hash_hex: string;
  /** True iff the two strings are byte-identical when both lowercased. */
  matches: boolean;
}

/// Compute the canonical hash of `intent` and compare it byte-for-byte
/// with `expectedHashHex`. Caller is responsible for refusing to sign
/// when `result.matches === false`.
export async function verifyIntentHash(
  intent: CanonicalIntent,
  expectedHashHex: string,
): Promise<HashVerificationResult> {
  const computed = await canonicalHash(intent);
  const expected = expectedHashHex.toLowerCase();
  return {
    computed_hash_hex: computed,
    expected_hash_hex: expected,
    matches: computed === expected,
  };
}

// ── Fixtures (mirror Rust test fixtures byte-for-byte) ─────────────────────
//
// These match `crates/types/src/canonical_intent.rs` exactly. Any
// change here MUST be paired with a fixture change in the Rust crate
// AND a refresh of `EXPECTED_FIXTURE_HASHES` below — and the parity
// check MUST still pass.

const FIXTURE_USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const FIXTURE_SOLEND_USDC_RESERVE = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const FIXTURE_SOLEND_LENDING_MARKET = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const FIXTURE_HCKRV_OBLIGATION = "HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV";
const FIXTURE_TEST_WALLET = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
const FIXTURE_WSOL_MINT = "So11111111111111111111111111111111111111112";

/// Bytes "WALL" + 11 zeros + 0x01 — same as Rust `FIXTURE_INTENT_ID`.
const FIXTURE_INTENT_ID = new Uint8Array([
  0x57, 0x41, 0x4c, 0x4c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
]);

/// Stable expiry slot used by all three fixtures. Mainnet ~Apr 2026.
const FIXTURE_EXPIRES_AT = BigInt(415_500_000);

/// Expected canonical hashes (hex, lowercase) committed alongside the
/// Rust fixtures. Frontend MUST reproduce these byte-for-byte.
export const EXPECTED_FIXTURE_HASHES = {
  solend_deposit:
    "0e2cbf3f57400e84a548c5a142d9e246214c1b26b85a75e9cca65cbfe8965bf1",
  solend_withdraw_all:
    "05a840f284b9c7b2163a1862806fc83a5b05127ac2e1833d5394c60b3b2ecf7a",
  jupiter_swap:
    "a9de08671a9d6811a142723043a43223a3e7a8bd567d6476d7c5821b1286ca6d",
} as const;

export function fixtureSolendDeposit(): CanonicalIntent {
  return {
    schema_version: STAGE1_TAIL_SCHEMA_VERSION,
    intent_id: FIXTURE_INTENT_ID,
    user: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
    action: {
      kind: "solend_deposit",
      wallet_pubkey: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
      input_mint: PubkeyBytes.fromBase58(FIXTURE_USDC_MINT),
      reserve_pubkey: PubkeyBytes.fromBase58(FIXTURE_SOLEND_USDC_RESERVE),
      lending_market: PubkeyBytes.fromBase58(FIXTURE_SOLEND_LENDING_MARKET),
      amount_raw: BigInt(5_000_000),
      amount_decimals: 6,
    },
    expires_at_slot: FIXTURE_EXPIRES_AT,
  };
}

export function fixtureSolendWithdrawAll(): CanonicalIntent {
  return {
    schema_version: STAGE1_TAIL_SCHEMA_VERSION,
    intent_id: FIXTURE_INTENT_ID,
    user: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
    action: {
      kind: "solend_withdraw_all",
      wallet_pubkey: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
      obligation_pubkey: PubkeyBytes.fromBase58(FIXTURE_HCKRV_OBLIGATION),
      reserve_pubkey: PubkeyBytes.fromBase58(FIXTURE_SOLEND_USDC_RESERVE),
      lending_market: PubkeyBytes.fromBase58(FIXTURE_SOLEND_LENDING_MARKET),
    },
    expires_at_slot: FIXTURE_EXPIRES_AT,
  };
}

export function fixtureJupiterSwap(): CanonicalIntent {
  return {
    schema_version: STAGE1_TAIL_SCHEMA_VERSION,
    intent_id: FIXTURE_INTENT_ID,
    user: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
    action: {
      kind: "jupiter_swap",
      wallet_pubkey: PubkeyBytes.fromBase58(FIXTURE_TEST_WALLET),
      input_mint: PubkeyBytes.fromBase58(FIXTURE_WSOL_MINT),
      output_mint: PubkeyBytes.fromBase58(FIXTURE_USDC_MINT),
      input_amount_raw: BigInt(1_000_000),
      slippage_bps: 50,
    },
    expires_at_slot: FIXTURE_EXPIRES_AT,
  };
}

/// Run all three fixture parity checks. Returns an object reporting
/// computed vs expected hashes per fixture and an `all_match` flag.
/// Used by `frontend/scripts/verify-canonical-intent.ts` and by any
/// future browser-side smoke check.
export interface FixtureParityReport {
  results: Array<{
    name: keyof typeof EXPECTED_FIXTURE_HASHES;
    computed: string;
    expected: string;
    match: boolean;
  }>;
  all_match: boolean;
}

export async function runFixtureParityCheck(): Promise<FixtureParityReport> {
  const cases: Array<[
    keyof typeof EXPECTED_FIXTURE_HASHES,
    () => CanonicalIntent,
  ]> = [
    ["solend_deposit", fixtureSolendDeposit],
    ["solend_withdraw_all", fixtureSolendWithdrawAll],
    ["jupiter_swap", fixtureJupiterSwap],
  ];

  const results = await Promise.all(
    cases.map(async ([name, build]) => {
      const computed = await canonicalHash(build());
      const expected = EXPECTED_FIXTURE_HASHES[name];
      return {
        name,
        computed,
        expected,
        match: computed === expected,
      };
    }),
  );

  return {
    results,
    all_match: results.every((r) => r.match),
  };
}

// ── Convenience: hex helpers (exported for the preview UI) ─────────────────

/// Hex-encode an arbitrary byte buffer. Lowercase, no `0x` prefix.
export function bytesToHex(bytes: Uint8Array): string {
  return hexEncode(bytes);
}
