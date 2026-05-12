// W5h-Memo addendum — asserts that `buildW5hFundingTransaction` produces
// the instruction sequence required by the on-chain audit anchor:
//
//   0. SPL Memo v2 (programId === MEMO_PROGRAM_ID;
//                   data === buildW5hMemoText(rule_id, canonical_hash))
//   1. (optional) CreateIdempotent ATA  (ATA program)
//   2. TransferChecked                  (SPL Token program)
//
// Two cases are exercised: with `includeCreateAta=true` (3 ix) and
// `includeCreateAta=false` (2 ix). No RPC, no signing, no broadcast —
// pure offline assembly + introspection.
//
// Run: `node frontend/scripts/verify-w5h-tx-shape.ts` (Node 22.7+
// strips TS types natively; this repo runs on Node 24).

import { PublicKey } from "@solana/web3.js";

import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  MEMO_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  USDC_MINT_BASE58,
  buildW5hFundingTransaction,
  buildW5hMemoText,
} from "../src/lib/stage2-funding.ts";

// Dummy but valid base58 pubkeys.
const PAYER = new PublicKey(
  "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
);
const SOURCE_ATA = new PublicKey(
  "73CnYmQAUKgaiQ4mY3ub2j8wPqkfaUmEnu3GuVHzefVB",
);
const DEST_ATA = new PublicKey(
  "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3",
);
const CONTROLLED_WALLET = new PublicKey(
  "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
);

const RULE_ID_HEX = "deadbeefcafef00d1234567890abcdef";
const CANONICAL_HASH_HEX =
  "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const BLOCKHASH_STUB = "11111111111111111111111111111111";
const AMOUNT = BigInt(250_000); // 0.25 USDC

let failures = 0;
function ok(label: string, cond: boolean, extra?: string): void {
  if (cond) {
    console.log(`  [OK] ${label}`);
  } else {
    console.error(`  [FAIL] ${label}${extra ? `: ${extra}` : ""}`);
    failures += 1;
  }
}

console.log("W5h funding-tx shape verification\n");

// ── Case A — includeCreateAta = true ─────────────────────────────────
const txA = buildW5hFundingTransaction({
  payer: PAYER,
  sourceAta: SOURCE_ATA,
  destinationAta: DEST_ATA,
  includeCreateAta: true,
  amountBaseUnits: AMOUNT,
  controlledWallet: CONTROLLED_WALLET,
  ruleIdHex: RULE_ID_HEX,
  canonicalRuleHashHex: CANONICAL_HASH_HEX,
  recentBlockhash: BLOCKHASH_STUB,
});

ok(
  "Case A: instruction count == 3",
  txA.instructions.length === 3,
  `actual=${txA.instructions.length}`,
);
ok(
  "Case A: instruction 0 programId === MEMO_PROGRAM_ID",
  txA.instructions[0]?.programId.equals(MEMO_PROGRAM_ID) ?? false,
);
ok(
  "Case A: instruction 0 has zero co-signer keys",
  (txA.instructions[0]?.keys ?? []).length === 0,
);
const memoBytesA = txA.instructions[0]?.data ?? Buffer.alloc(0);
const memoStrA = Buffer.from(memoBytesA).toString("utf8");
ok(
  "Case A: instruction 0 data === buildW5hMemoText(rule_id, hash)",
  memoStrA === buildW5hMemoText(RULE_ID_HEX, CANONICAL_HASH_HEX),
  `actual=${memoStrA}`,
);
ok(
  'Case A: instruction 0 data starts with "claw:w5h:"',
  memoStrA.startsWith("claw:w5h:"),
);
ok(
  "Case A: instruction 1 is the ATA program (CreateIdempotent)",
  txA.instructions[1]?.programId.equals(ASSOCIATED_TOKEN_PROGRAM_ID) ?? false,
);
ok(
  "Case A: instruction 2 is the SPL Token program (TransferChecked)",
  txA.instructions[2]?.programId.equals(TOKEN_PROGRAM_ID) ?? false,
);
// TransferChecked layout: tag(1) + amount(8 LE) + decimals(1) = 10 bytes
const xferData = txA.instructions[2]?.data ?? Buffer.alloc(0);
ok(
  "Case A: TransferChecked data length == 10",
  xferData.length === 10,
  `actual=${xferData.length}`,
);
ok(
  "Case A: TransferChecked first byte == 12 (tag)",
  xferData[0] === 12,
  `actual=${xferData[0]}`,
);
ok(
  "Case A: TransferChecked decimals byte == 6 (USDC)",
  xferData[9] === 6,
  `actual=${xferData[9]}`,
);
ok(
  "Case A: feePayer equals payer",
  txA.feePayer?.equals(PAYER) ?? false,
);
ok(
  "Case A: recentBlockhash is set",
  txA.recentBlockhash === BLOCKHASH_STUB,
);

// Sanity: amount bytes encode 250_000 in little-endian u64.
//   250_000 = 0x3D090 → bytes (LE) = [0x90, 0xD0, 0x03, 0, 0, 0, 0, 0]
const expectedAmountBytes = Buffer.from([
  0x90, 0xd0, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
]);
const actualAmountBytes = xferData.subarray(1, 9);
ok(
  "Case A: TransferChecked amount bytes == 0.25 USDC LE u64",
  Buffer.compare(actualAmountBytes, expectedAmountBytes) === 0,
  `actual=${actualAmountBytes.toString("hex")} expected=${expectedAmountBytes.toString("hex")}`,
);

// ── Case B — includeCreateAta = false ────────────────────────────────
const txB = buildW5hFundingTransaction({
  payer: PAYER,
  sourceAta: SOURCE_ATA,
  destinationAta: DEST_ATA,
  includeCreateAta: false,
  amountBaseUnits: AMOUNT,
  controlledWallet: CONTROLLED_WALLET,
  ruleIdHex: RULE_ID_HEX,
  canonicalRuleHashHex: CANONICAL_HASH_HEX,
  recentBlockhash: BLOCKHASH_STUB,
});

ok(
  "Case B: instruction count == 2 (no CreateIdempotent)",
  txB.instructions.length === 2,
  `actual=${txB.instructions.length}`,
);
ok(
  "Case B: instruction 0 programId === MEMO_PROGRAM_ID",
  txB.instructions[0]?.programId.equals(MEMO_PROGRAM_ID) ?? false,
);
ok(
  "Case B: instruction 1 is the SPL Token program",
  txB.instructions[1]?.programId.equals(TOKEN_PROGRAM_ID) ?? false,
);

// ── Sanity: pinned program IDs match the canonical mainnet strings ───
ok(
  "MEMO_PROGRAM_ID base58 == MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
  MEMO_PROGRAM_ID.toBase58() === "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
);
ok(
  "USDC_MINT_BASE58 == EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDC_MINT_BASE58 === "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
);

if (failures > 0) {
  console.error(`\nFAIL — ${failures} assertion(s) failed.`);
  process.exit(1);
}
console.log("\nW5h funding-tx shape verified.");
process.exit(0);
