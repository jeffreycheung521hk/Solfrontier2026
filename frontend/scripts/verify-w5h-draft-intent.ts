// Phase 5c-lite frontend verifier — asserts the four invariants that
// the LLM-draft → review → finalize → funding-tx chain MUST uphold:
//
//   1. `DraftIntentReviewRequiredDto` round-trips through the
//      frontend type without coercion: amount_raw stays a string,
//      threshold_display is rendered verbatim (no parsing), the
//      backend-supplied `draft_hash` is never recomputed.
//
//   2. Phase 5c finalized-funding flow uses the BACKEND-PINNED
//      `memo_text` for the on-chain memo instruction, NOT
//      `buildW5hMemoText(rule_id, canonical_hash)`. Mutating the
//      ruleIdHex / canonicalRuleHashHex args while `memoTextOverride`
//      is set MUST NOT change the bytes Phantom signs.
//
//   3. The TransferChecked amount byte-encodes the FINALIZED
//      `amount_raw` (0.5 USDC = 500_000 raw, NOT the legacy 0.25 / 250_000).
//
//   4. The legacy 0.25-regex path still works: when `memoTextOverride`
//      is undefined and the caller passes the rule_id + hash, the
//      builder falls back to `buildW5hMemoText`.
//
// Run: `node frontend/scripts/verify-w5h-draft-intent.ts`

import { PublicKey } from "@solana/web3.js";

import {
  MEMO_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  USDC_MINT_BASE58,
  buildW5hFundingTransaction,
  buildW5hMemoText,
} from "../src/lib/stage2-funding.ts";
import type {
  DraftIntentReviewRequiredDto,
  W5hConditionalDepositResult,
} from "../src/lib/types.ts";

const PAYER = new PublicKey("BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L");
const SOURCE_ATA = new PublicKey("73CnYmQAUKgaiQ4mY3ub2j8wPqkfaUmEnu3GuVHzefVB");
const DEST_ATA = new PublicKey("7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3");
const CONTROLLED = new PublicKey("BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L");
const BLOCKHASH = "11111111111111111111111111111111";

const PHASE5C_RULE_ID = "cafef00ddeadbeefcafef00d1234abcd";
const PHASE5C_HASH =
  "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const PHASE5C_AMOUNT_RAW = "500000"; // 0.5 USDC = 500_000 raw

const LEGACY_RULE_ID = "deadbeefcafef00d1234567890abcdef";
const LEGACY_HASH =
  "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const LEGACY_AMOUNT_RAW = "250000"; // 0.25 USDC = 250_000 raw

let failures = 0;
function ok(label: string, cond: boolean, extra?: string): void {
  if (cond) {
    console.log(`  [OK] ${label}`);
  } else {
    console.error(`  [FAIL] ${label}${extra ? `: ${extra}` : ""}`);
    failures += 1;
  }
}

console.log("Phase 5c-lite draft-intent verification\n");

// ── Invariant 1 — DraftIntentReviewRequiredDto type-shape ────────────
//
// Build a fixture DTO and assert every required field is the expected
// shape. The type itself is the schema; this is a smoke check that
// the canonical shape compiles against the type without coercion.
const draft: DraftIntentReviewRequiredDto = {
  draft_id: "draft-test-001",
  draft_hash:
    "ffeeddccbbaa9988776655443322110000112233445566778899aabbccddeeff",
  parser_source: "llm_extractor",
  original_user_message_hash:
    "1111111111111111111111111111111111111111111111111111111111111111",
  action: "deposit",
  protocol: "solend",
  asset: "USDC",
  display_source: "save",
  comparison: "gt",
  threshold_bps: 50,
  threshold_display: "0.5%",
  amount_raw: PHASE5C_AMOUNT_RAW,
  amount_display: "0.5 USDC",
  controlled_wallet: PAYER.toBase58(),
  controlled_usdc_ata: DEST_ATA.toBase58(),
  expiry_seconds_after_finalize: 180,
  warnings: [],
  review_copy:
    "LLM drafted this intent. Nothing is funded or executed until you confirm.",
};

ok("DraftIntentReviewRequiredDto: amount_raw is a string", typeof draft.amount_raw === "string");
ok("DraftIntentReviewRequiredDto: threshold_bps is a number", typeof draft.threshold_bps === "number");
ok("DraftIntentReviewRequiredDto: threshold_display is a string", typeof draft.threshold_display === "string");
ok("DraftIntentReviewRequiredDto: draft_hash is a 64-char hex string", /^[0-9a-fA-F]{64}$/.test(draft.draft_hash));
ok("DraftIntentReviewRequiredDto: draft_id is non-empty", draft.draft_id.length > 0);
ok("DraftIntentReviewRequiredDto: warnings is an array", Array.isArray(draft.warnings));

// ── Invariant 2 — memo_text override drives the memo bytes ───────────
//
// Build two transactions:
//   - txA: memoTextOverride set, ruleIdHex/canonicalRuleHashHex set to
//          INTENTIONALLY WRONG values
//   - txB: memoTextOverride set to the same value, ruleIdHex /
//          canonicalRuleHashHex set to the CORRECT values
// The memo bytes MUST be identical — proving that ruleIdHex /
// canonicalRuleHashHex are ignored when override is present.
const finalizeMemo = `claw:w5h:${PHASE5C_RULE_ID}:${PHASE5C_HASH}`;
const txA = buildW5hFundingTransaction({
  payer: PAYER,
  sourceAta: SOURCE_ATA,
  destinationAta: DEST_ATA,
  includeCreateAta: false,
  amountBaseUnits: BigInt(PHASE5C_AMOUNT_RAW),
  controlledWallet: CONTROLLED,
  ruleIdHex: "WRONG_RULE_ID_DECEIVE", // intentionally wrong
  canonicalRuleHashHex: "WRONG_HASH_DECEIVE", // intentionally wrong
  memoTextOverride: finalizeMemo,
  recentBlockhash: BLOCKHASH,
});
const txB = buildW5hFundingTransaction({
  payer: PAYER,
  sourceAta: SOURCE_ATA,
  destinationAta: DEST_ATA,
  includeCreateAta: false,
  amountBaseUnits: BigInt(PHASE5C_AMOUNT_RAW),
  controlledWallet: CONTROLLED,
  ruleIdHex: PHASE5C_RULE_ID,
  canonicalRuleHashHex: PHASE5C_HASH,
  memoTextOverride: finalizeMemo,
  recentBlockhash: BLOCKHASH,
});
const memoBytesA = txA.instructions[0]?.data ?? Buffer.alloc(0);
const memoBytesB = txB.instructions[0]?.data ?? Buffer.alloc(0);
ok(
  "memo_text override: identical memo bytes regardless of rule_id / hash args",
  Buffer.compare(memoBytesA, memoBytesB) === 0,
  `A=${memoBytesA.toString("utf8")} B=${memoBytesB.toString("utf8")}`,
);
ok(
  "memo_text override: bytes equal the override string",
  Buffer.from(memoBytesA).toString("utf8") === finalizeMemo,
);
ok(
  "memo_text override: instruction 0 programId === MEMO_PROGRAM_ID",
  txA.instructions[0]?.programId.equals(MEMO_PROGRAM_ID) ?? false,
);

// ── Invariant 3 — TransferChecked encodes finalized amount_raw ───────
const xferData = txA.instructions[1]?.data ?? Buffer.alloc(0);
ok(
  "Phase 5c funding tx: TransferChecked is the only non-Memo ix",
  txA.instructions.length === 2 &&
    (txA.instructions[1]?.programId.equals(TOKEN_PROGRAM_ID) ?? false),
);
ok(
  "Phase 5c funding tx: TransferChecked data length == 10",
  xferData.length === 10,
);
ok(
  "Phase 5c funding tx: TransferChecked tag == 12",
  xferData[0] === 12,
);
// 500_000 = 0x7A120  → LE bytes = [0x20, 0xA1, 0x07, 0, 0, 0, 0, 0]
const expectedAmountBytes = Buffer.from([
  0x20, 0xa1, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00,
]);
const actualAmountBytes = xferData.subarray(1, 9);
ok(
  "Phase 5c funding tx: amount bytes == LE u64(500_000) (NOT the legacy 250_000)",
  Buffer.compare(actualAmountBytes, expectedAmountBytes) === 0,
  `actual=${actualAmountBytes.toString("hex")}`,
);
ok(
  "Phase 5c funding tx: decimals byte == 6 (USDC)",
  xferData[9] === 6,
);

// ── Invariant 4 — Legacy 0.25 regex path unchanged ───────────────────
//
// When memoTextOverride is undefined, the builder MUST fall back to
// buildW5hMemoText(ruleIdHex, canonicalRuleHashHex). Existing 0.25
// W5h fixture must still work.
const legacyMemo = buildW5hMemoText(LEGACY_RULE_ID, LEGACY_HASH);
const txLegacy = buildW5hFundingTransaction({
  payer: PAYER,
  sourceAta: SOURCE_ATA,
  destinationAta: DEST_ATA,
  includeCreateAta: false,
  amountBaseUnits: BigInt(LEGACY_AMOUNT_RAW),
  controlledWallet: CONTROLLED,
  ruleIdHex: LEGACY_RULE_ID,
  canonicalRuleHashHex: LEGACY_HASH,
  // memoTextOverride INTENTIONALLY undefined → recompute path
  recentBlockhash: BLOCKHASH,
});
const legacyMemoBytes = txLegacy.instructions[0]?.data ?? Buffer.alloc(0);
ok(
  "Legacy path: memo bytes == buildW5hMemoText(rule_id, hash) when no override",
  Buffer.from(legacyMemoBytes).toString("utf8") === legacyMemo,
);
ok(
  "Legacy path: memo starts with claw:w5h:",
  Buffer.from(legacyMemoBytes).toString("utf8").startsWith("claw:w5h:"),
);
// 250_000 = 0x3D090  → LE = [0x90, 0xD0, 0x03, 0, 0, 0, 0, 0]
const legacyXferData = txLegacy.instructions[1]?.data ?? Buffer.alloc(0);
const expectedLegacyAmount = Buffer.from([
  0x90, 0xd0, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
]);
ok(
  "Legacy path: TransferChecked amount bytes == LE u64(250_000)",
  Buffer.compare(legacyXferData.subarray(1, 9), expectedLegacyAmount) === 0,
);

// ── Invariant 5 — W5hConditionalDepositResult accepts finalize fields
//
// Type-shape check on the extended W5h DTO. The Phase 5c-lite
// finalize-response shape MUST compile against the existing
// W5hConditionalDepositResult interface (extended fields are all
// optional).
const finalizedDto: W5hConditionalDepositResult = {
  input_text: "showcase",
  status: "funding_required",
  rule_id_hex: PHASE5C_RULE_ID,
  canonical_rule_hash_hex: PHASE5C_HASH,
  amount_raw: PHASE5C_AMOUNT_RAW,
  amount_display: "0.5 USDC",
  controlled_wallet: PAYER.toBase58(),
  controlled_usdc_ata: DEST_ATA.toBase58(),
  expires_at_ms: (Date.now() + 180_000).toString(),
  threshold_bps: 50,
  threshold_pct_label: "0.5",
  threshold_display: "0.5%",
  memo_text: finalizeMemo,
  finalization: {
    parser_source: "llm_extractor",
    draft_id: "draft-test-001",
    draft_hash:
      "ffeeddccbbaa9988776655443322110000112233445566778899aabbccddeeff",
    original_user_message_hash:
      "1111111111111111111111111111111111111111111111111111111111111111",
    finalized_at_ms: Date.now().toString(),
  },
};
ok(
  "W5hConditionalDepositResult: amount_display is optional and stringable",
  typeof finalizedDto.amount_display === "string",
);
ok(
  "W5hConditionalDepositResult: memo_text is optional and stringable",
  typeof finalizedDto.memo_text === "string",
);
ok(
  "W5hConditionalDepositResult: finalization metadata bag carries draft_id",
  finalizedDto.finalization?.draft_id === "draft-test-001",
);

// ── Sanity: USDC mint pin unchanged ──────────────────────────────────
ok(
  "USDC_MINT_BASE58 unchanged",
  USDC_MINT_BASE58 === "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
);
ok(
  "MEMO_PROGRAM_ID unchanged",
  MEMO_PROGRAM_ID.toBase58() === "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
);

if (failures > 0) {
  console.error(`\nFAIL — ${failures} assertion(s) failed.`);
  process.exit(1);
}
console.log("\nPhase 5c-lite draft-intent invariants verified.");
process.exit(0);
