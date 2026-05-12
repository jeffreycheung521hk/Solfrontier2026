// Stage 2 — Phantom controlled-wallet funding helpers.
//
// Pure-Solana primitives the demo page uses to build a single
// "fund 5 USDC into the controlled wallet" transaction. NO RPC,
// NO signing, NO sending — those happen in the page (RPC) or in
// the existing `@/lib/phantom` wrapper (signing).
//
// HARD SAFETY upheld in this file:
//   - No private-key material handled.
//   - No controlled-wallet keypair imported.
//   - No Solend / Jupiter / live-execution code.
//   - No JSON-RPC. No `fetch`. No `Connection`.
//   - Amounts are bigint / u64-only — no floating point math.
//
// Constants are pinned to mainnet (USDC mint, controlled wallet,
// SPL Token + ATA program ids). They are documented at the call
// site and surfaced to the UI so the operator can re-verify each
// pubkey before approving the Phantom popup.

import {
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
} from "@solana/web3.js";

// ── Pinned mainnet pubkeys ────────────────────────────────────────────────

/// USDC mainnet mint. 6 decimals. Per memory + every other Stage 2
/// fixture in the repo.
export const USDC_MINT_BASE58 = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
export const USDC_DECIMALS = 6;

/// Controlled wallet — Slice 3C controlled keypair pubkey. The
/// keypair lives ONLY on the operator's filesystem (per the
/// `slice3c-dryrun.json` memory pointer); this frontend never sees
/// the secret bytes.
export const CONTROLLED_WALLET_BASE58 =
  "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

/// SPL Token program (the legacy v3 program — Token-2022 not used).
export const TOKEN_PROGRAM_ID = new PublicKey(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
);

/// Associated Token Account (ATA) program — derives the canonical
/// token account for `(owner, mint)`.
export const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
);

/// Funding amount — exactly 5 USDC = 5 × 10^6 base units. u64.
///
/// Pinned to the smallest demo-meaningful amount on 2026-05-11 so
/// live-mainnet runs stay cheap. The demo's bounded-automation
/// framing — the controlled wallet only receives an explicit
/// one-shot transfer; the user main wallet retains self-custody and
/// is never delegated — is independent of the amount. The empirical
/// reference tx `5RWXZh…r6Px9` (see `LAST_SUCCESSFUL_FUNDING_SIGNATURE`
/// below) was a 5 USDC transfer with this exact instruction layout.
export const FUNDING_AMOUNT_BASE_UNITS = BigInt(5_000_000);

/// Funding amount UI label — produced by integer math, never by
/// floating-point arithmetic.
export const FUNDING_AMOUNT_UI_LABEL = "5.000000 USDC";

/// Empirical proof of a successful Phantom controlled-wallet funding
/// using THIS exact instruction layout (TransferChecked tag 12 +
/// optional CreateIdempotent ATA). Finalized cleanly on mainnet at
/// slot 418961171, err=None. Rendered read-only on the demo page so
/// the operator/audience can verify the wire-shape works before
/// signing a fresh tx in this session.
///
/// NOT a key — just a public tx signature. Anyone can resolve it
/// through Solscan or the JSON-RPC `getTransaction` endpoint.
export const LAST_SUCCESSFUL_FUNDING_SIGNATURE =
  "5RWXZhGeFWohJAdSv2iUrM1qiBFSNnaxkqq8dEBErauoLi2y8rGvqavSDKv4WVhgomDkhH55r7yZ5UiTmvGr6Px9";

/// Slot at which `LAST_SUCCESSFUL_FUNDING_SIGNATURE` finalized. Used
/// for the read-only proof panel's secondary label. Operator can
/// cross-check on Solscan / `solana confirm -v <sig>`.
export const LAST_SUCCESSFUL_FUNDING_SLOT = 418961171;

// ── ATA derivation (deterministic; no RPC) ────────────────────────────────

/// Derive the canonical Associated Token Account address for a
/// `(owner, mint)` pair. Pure: zero RPC, identical inputs always
/// give identical output. Mirrors the SPL `getAssociatedTokenAddressSync`
/// helper but built locally so this module does not add a new
/// dependency.
export function deriveAtaPubkey(
  owner: PublicKey,
  mint: PublicKey,
): PublicKey {
  const [ata] = PublicKey.findProgramAddressSync(
    [owner.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mint.toBuffer()],
    ASSOCIATED_TOKEN_PROGRAM_ID,
  );
  return ata;
}

// ── Idempotent create-ATA instruction ─────────────────────────────────────
//
// Layout: ATA program instruction tag 1 = `CreateIdempotent`. The
// canonical SPL `createAssociatedTokenAccountIdempotentInstruction`
// builds the same instruction — we hand-roll it so this module
// stays @solana/web3.js-only.

export function createIdempotentAtaInstruction({
  payer,
  ata,
  owner,
  mint,
}: {
  payer: PublicKey;
  ata: PublicKey;
  owner: PublicKey;
  mint: PublicKey;
}): TransactionInstruction {
  return new TransactionInstruction({
    programId: ASSOCIATED_TOKEN_PROGRAM_ID,
    keys: [
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: ata, isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: false, isWritable: false },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    ],
    // Tag 1 = CreateIdempotent (succeeds silently if the ATA
    // already exists). Mirrors the SPL helper.
    data: Buffer.from([1]),
  });
}

// ── SPL Token `TransferChecked` instruction ───────────────────────────────
//
// Per the project's Slice 3G memory + policy, we use `TransferChecked`
// (instruction tag 12), NOT the legacy `Transfer`. TransferChecked
// includes the mint as an account and asserts the source token
// account's decimals — preventing the legacy-Transfer bug class
// where amounts on the wrong mint slip through.
//
// Wire data layout: tag(u8=12) + amount(u64 LE) + decimals(u8).

function u64LeBytes(amount: bigint): Buffer {
  if (amount < BigInt(0)) {
    throw new Error(`amount must be non-negative; got ${amount}`);
  }
  const MAX_U64 = BigInt("18446744073709551615");
  if (amount > MAX_U64) {
    throw new Error(`amount exceeds u64 range; got ${amount}`);
  }
  const out = Buffer.alloc(8);
  const MASK = BigInt(0xff);
  let v = amount;
  for (let i = 0; i < 8; i++) {
    out[i] = Number(v & MASK);
    v >>= BigInt(8);
  }
  return out;
}

export function createTransferCheckedInstruction({
  source,
  mint,
  destination,
  owner,
  amount,
  decimals,
}: {
  source: PublicKey;
  mint: PublicKey;
  destination: PublicKey;
  owner: PublicKey;
  amount: bigint;
  decimals: number;
}): TransactionInstruction {
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > 255) {
    throw new Error(`decimals must be u8 (0..255); got ${decimals}`);
  }
  const data = Buffer.concat([
    Buffer.from([12]), // TransferChecked tag
    u64LeBytes(amount),
    Buffer.from([decimals]),
  ]);
  return new TransactionInstruction({
    programId: TOKEN_PROGRAM_ID,
    keys: [
      { pubkey: source, isSigner: false, isWritable: true },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: destination, isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: true, isWritable: false },
    ],
    data,
  });
}

// ── Full funding-transaction builder ──────────────────────────────────────

export interface BuildFundingTxArgs {
  /** User's connected Phantom wallet pubkey (= signer / payer). */
  payer: PublicKey;
  /** User's source USDC ATA. The page derives this from
   *  `(payer, USDC_MINT)` before calling. */
  sourceAta: PublicKey;
  /** Controlled wallet's destination USDC ATA. The page derives this
   *  from `(CONTROLLED_WALLET, USDC_MINT)` before calling. */
  destinationAta: PublicKey;
  /** Required even when the ATA already exists — the idempotent
   *  create variant succeeds silently on an existing account, so
   *  we include it unconditionally for safety. */
  includeCreateAta: boolean;
  /** From a fresh `Connection.getLatestBlockhash()` — set on the tx
   *  before signing. */
  recentBlockhash: string;
}

/// Build the funding transaction (unsigned).
///
/// Always:
///   1. (optional) `CreateIdempotent` ATA for the controlled wallet
///   2. `TransferChecked` 5 USDC from `sourceAta` → `destinationAta`
///
/// Returns the assembled `Transaction`; the caller signs via Phantom
/// (`signTransaction`) and submits via the JSON-RPC connection.
export function buildFundingTransaction(
  args: BuildFundingTxArgs,
): Transaction {
  const usdcMint = new PublicKey(USDC_MINT_BASE58);
  const tx = new Transaction();
  if (args.includeCreateAta) {
    tx.add(
      createIdempotentAtaInstruction({
        payer: args.payer,
        ata: args.destinationAta,
        owner: new PublicKey(CONTROLLED_WALLET_BASE58),
        mint: usdcMint,
      }),
    );
  }
  tx.add(
    createTransferCheckedInstruction({
      source: args.sourceAta,
      mint: usdcMint,
      destination: args.destinationAta,
      owner: args.payer,
      amount: FUNDING_AMOUNT_BASE_UNITS,
      decimals: USDC_DECIMALS,
    }),
  );
  tx.feePayer = args.payer;
  tx.recentBlockhash = args.recentBlockhash;
  return tx;
}

// ── W5h chat-driven funding-transaction builder ──────────────────────────
//
// The /stage2/live-demo page pins amount + destination at module level
// (`FUNDING_AMOUNT_BASE_UNITS`, `CONTROLLED_WALLET_BASE58`). The W5h
// chat flow CAN'T do that — both amount and destination ATA come from
// the backend's `W5hConditionalDepositResult` DTO at chat time, so the
// builder must be parameterised. This helper is the same primitive
// pipeline as `buildFundingTransaction` above, just open to any amount
// and any destination ATA. Hard safety:
//   - Source ATA stays user-owned (caller's responsibility to derive
//     it from the user's pubkey).
//   - Mint stays pinned to USDC (`USDC_MINT_BASE58`) — there's no
//     parameter to override the mint, by design.
//   - Decimals stays pinned to 6 (`USDC_DECIMALS`) — TransferChecked
//     hard-asserts this against the on-chain mint, so even if a
//     hostile DTO swapped in a non-USDC mint pubkey for `destinationAta`,
//     the instruction would fail before the tx landed.

// ── SPL Memo (v2) instruction ────────────────────────────────────────────
//
// The canonical on-chain Memo program. Memo v2 takes UTF-8 bytes as the
// entire instruction data — no varint length prefix, no instruction tag.
// Optional signer accounts in `keys` co-sign the memo; for the W5h
// audit-anchor use case we don't co-sign (Phantom already signs the
// payer slot of the tx as a whole), so `keys` is empty.

/// Canonical SPL Memo v2 program. Pinned to a string + parsed once.
/// Pubkey shipped with the @solana/web3.js standard library, but we
/// pin it here so we don't depend on the optional @solana/spl-memo
/// package.
export const MEMO_PROGRAM_ID = new PublicKey(
  "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
);

/// Build the W5h on-chain memo string. Canonical format:
///   `claw:w5h:<rule_id_hex>:<canonical_rule_hash_hex>`
///
/// Used in two places — the actual `MemoSq4gqAB…` instruction data,
/// and the W5h card's "Instruction hash memo" UI row. Keeping both
/// behind a single helper ensures the value the user sees matches the
/// bytes Phantom signs.
///
/// NEVER includes the natural-language chat command. The two hex
/// anchors are deterministic and bound to the persisted rule; that's
/// the audit signal we want on-chain. Free-form text would balloon
/// the memo size and risk leaking commands.
export function buildW5hMemoText(
  ruleIdHex: string,
  canonicalRuleHashHex: string,
): string {
  return `claw:w5h:${ruleIdHex}:${canonicalRuleHashHex}`;
}

/// Build a single-instruction Memo entry from a UTF-8 string. No
/// co-signers; the memo is anchored by the outer transaction's
/// payer signature alone.
export function createMemoInstruction(memoText: string): TransactionInstruction {
  return new TransactionInstruction({
    programId: MEMO_PROGRAM_ID,
    keys: [],
    data: Buffer.from(memoText, "utf8"),
  });
}

/// Args for `buildW5hFundingTransaction`. Same shape as
/// `BuildFundingTxArgs` plus an explicit `amountBaseUnits` parameter
/// plus the two memo-anchor hex strings.
export interface BuildW5hFundingTxArgs {
  payer: PublicKey;
  sourceAta: PublicKey;
  destinationAta: PublicKey;
  /// `CreateIdempotent` is included unconditionally when true. The
  /// idempotent variant is a no-op when the ATA already exists, so
  /// it's safe to always include — costs ~1.5k lamports of rent if
  /// the account doesn't exist, zero otherwise.
  includeCreateAta: boolean;
  /// USDC base units, u64. Validate at the call site that the value
  /// fits in u64 — the underlying `createTransferCheckedInstruction`
  /// throws on overflow but does NOT clamp.
  amountBaseUnits: bigint;
  /// Controlled wallet pubkey (= owner of `destinationAta`). The
  /// idempotent CreateAta needs this as the `owner` field. The W5h
  /// DTO carries `controlled_wallet` so the caller threads it
  /// through; we never default to `CONTROLLED_WALLET_BASE58` here
  /// because that would silently mask a backend DTO drift.
  controlledWallet: PublicKey;
  /// 32-char hex of the persisted `WatchRule.rule_id`. Anchored in
  /// the on-chain memo as the first half of the audit string.
  ruleIdHex: string;
  /// 64-char hex of the rule's canonical Borsh hash. Anchored in
  /// the on-chain memo as the second half — this is the rule's
  /// integrity signature, distinct from the rule id.
  canonicalRuleHashHex: string;
  recentBlockhash: string;
}

/// Build a W5h funding transaction (unsigned).
///
/// Always (in this exact order — Memo MUST be index 0):
///   0. SPL Memo v2 — anchors `claw:w5h:<rule_id_hex>:<hash_hex>`
///      on-chain so the audit trail of "this signature funded that
///      rule" is recoverable without backend state.
///   1. (optional) `CreateIdempotent` ATA for the controlled-wallet
///      USDC ATA — safe to include unconditionally.
///   2. `TransferChecked` `amountBaseUnits` USDC from `sourceAta`
///      → `destinationAta`.
///
/// Returns the assembled `Transaction`; the caller signs via Phantom
/// (ONE signature on the payer slot, NO new co-signers) and submits
/// via JSON-RPC.
///
/// NOTE: amount is bigint. The caller MUST parse the DTO's
/// `amount_raw` (string) into a bigint at the call site, with whatever
/// guardrails fit the context (e.g. reject negatives, cap at the
/// pinned demo amount of 250_000).
export function buildW5hFundingTransaction(
  args: BuildW5hFundingTxArgs,
): Transaction {
  const usdcMint = new PublicKey(USDC_MINT_BASE58);
  const tx = new Transaction();
  // Index 0 — Memo (audit anchor). MUST stay first per the W5h-Memo
  // addendum contract so the on-chain layout is grep-stable.
  tx.add(
    createMemoInstruction(
      buildW5hMemoText(args.ruleIdHex, args.canonicalRuleHashHex),
    ),
  );
  if (args.includeCreateAta) {
    tx.add(
      createIdempotentAtaInstruction({
        payer: args.payer,
        ata: args.destinationAta,
        owner: args.controlledWallet,
        mint: usdcMint,
      }),
    );
  }
  tx.add(
    createTransferCheckedInstruction({
      source: args.sourceAta,
      mint: usdcMint,
      destination: args.destinationAta,
      owner: args.payer,
      amount: args.amountBaseUnits,
      decimals: USDC_DECIMALS,
    }),
  );
  tx.feePayer = args.payer;
  tx.recentBlockhash = args.recentBlockhash;
  return tx;
}

// ── Display helpers ───────────────────────────────────────────────────────

/// Convert base-units bigint to a `"X.YYYYYY USDC"` display string
/// using exact integer math (no floats). Returns `"—"` for null.
export function formatUsdcBaseUnits(
  baseUnits: bigint | null | undefined,
): string {
  if (baseUnits === null || baseUnits === undefined) return "—";
  if (baseUnits < BigInt(0)) return "—";
  const padded = baseUnits.toString().padStart(USDC_DECIMALS + 1, "0");
  const intPart = padded.slice(0, -USDC_DECIMALS);
  const fracPart = padded.slice(-USDC_DECIMALS);
  return `${intPart}.${fracPart} USDC`;
}

/// Standard Solscan explorer URL for a mainnet signature.
export function solscanTxUrl(signature: string): string {
  return `https://solscan.io/tx/${signature}`;
}

// ── Confirmation watcher (getSignatureStatuses polling) ───────────────────
//
// Replaces the previous `Connection.confirmTransaction({ blockhash,
// lastValidBlockHeight }, ...)` path that gave up at the block-height
// window even when the tx finalized after that window. The new helper
// polls `getSignatureStatuses` with `searchTransactionHistory: true`
// so a tx that landed late (after the block-height window) is still
// observed and reported as `finalized`.
//
// Empirical reference: tx
// 5RWXZhGeFWohJAdSv2iUrM1qiBFSNnaxkqq8dEBErauoLi2y8rGvqavSDKv4WVhgomDkhH55r7yZ5UiTmvGr6Px9
// (slot 418961171, err=None, fee=80000 lamports) finalized cleanly on
// mainnet but the OLD `confirmTransaction` path raised "block height
// exceeded" before the tx landed. The poller below would have
// reported `kind: "finalized"` for the same signature.
//
// SAFETY invariants upheld here:
//   - Bounded loop. `MAX_POLL_ATTEMPTS_DEFAULT = 60` × 2 s = 120 s
//     ceiling; the helper never loops indefinitely.
//   - Read-only. Calls `getSignatureStatuses` only. Never re-sends,
//     never re-signs, never broadcasts.
//   - Treats `status.err !== null` as a real failure — never reports
//     `finalized` for a tx whose on-chain error is set.
//   - Reports a `timeout` outcome when the chain is silent past the
//     cap; the caller is expected to surface "the tx may still
//     land — verify on Solscan" copy, NOT a destructive failure.

/// Structural projection of the web3.js `Connection` surface this
/// helper depends on. Keeps the helper testable with a tiny mock and
/// avoids importing the full `Connection` type into pure-logic land.
export interface SignatureStatusFetcher {
  getSignatureStatuses(
    signatures: string[],
    opts: { searchTransactionHistory: boolean },
  ): Promise<{
    value: Array<{
      err: unknown | null;
      confirmationStatus?: string | null;
    } | null>;
  }>;
}

export type SignatureStatusPollResult =
  | { kind: "finalized" }
  | { kind: "failed_on_chain"; err: unknown }
  | { kind: "timeout" };

/// Thrown by `pollSignatureStatus` when `getSignatureStatuses` raises
/// more than 3 times in a row. The caller catches and reports
/// "Network error polling status; verify on Solscan: <signature>"
/// without losing the signature.
export class SignatureStatusNetworkError extends Error {
  public readonly consecutiveErrors: number;
  public readonly lastError: unknown;
  constructor(consecutiveErrors: number, lastError: unknown) {
    super(
      `getSignatureStatuses failed ${consecutiveErrors} times in a row` +
        (lastError instanceof Error ? `: ${lastError.message}` : ""),
    );
    this.name = "SignatureStatusNetworkError";
    this.consecutiveErrors = consecutiveErrors;
    this.lastError = lastError;
  }
}

export interface PollSignatureStatusOptions {
  /** Hard upper bound on poll iterations. Default 60 (≈120 s with
   *  `intervalMs = 2000`). */
  maxAttempts: number;
  /** Sleep between successful polls, in milliseconds. */
  intervalMs: number;
  /** Optional progress callback — fired with the 1-indexed attempt
   *  number every iteration. UI uses this to update the "attempt
   *  N of M" copy. */
  onAttempt?: (n: number) => void;
}

export const MAX_POLL_ATTEMPTS_DEFAULT = 60;
export const POLL_INTERVAL_MS_DEFAULT = 2_000;

/// Poll `getSignatureStatuses([signature])` with
/// `searchTransactionHistory: true` until one of:
///   - the entry's `err === null` AND `confirmationStatus` ∈
///     {"confirmed","finalized"}     → `{ kind: "finalized" }`
///   - the entry's `err !== null`     → `{ kind: "failed_on_chain", err }`
///   - we hit `maxAttempts` with the entry still `null`
///                                    → `{ kind: "timeout" }`
///   - the fetch itself throws > 3 consecutive times
///                                    → `throw SignatureStatusNetworkError`
///
/// Note: a `"processed"` confirmation status is NOT terminal here —
/// we keep polling until the cluster promotes it to `"confirmed"` or
/// `"finalized"` (or we time out). This matches the prompt's spec.
export async function pollSignatureStatus(
  connection: SignatureStatusFetcher,
  signature: string,
  opts: PollSignatureStatusOptions,
): Promise<SignatureStatusPollResult> {
  let consecutiveNetworkErrors = 0;
  let lastNetworkError: unknown = null;
  for (let attempt = 1; attempt <= opts.maxAttempts; attempt++) {
    opts.onAttempt?.(attempt);
    let statuses;
    try {
      statuses = await connection.getSignatureStatuses([signature], {
        searchTransactionHistory: true,
      });
      consecutiveNetworkErrors = 0;
    } catch (err) {
      consecutiveNetworkErrors += 1;
      lastNetworkError = err;
      if (consecutiveNetworkErrors > 3) {
        throw new SignatureStatusNetworkError(
          consecutiveNetworkErrors,
          lastNetworkError,
        );
      }
      // Treat transient throw as a no-op poll: don't increment the
      // logical attempt counter further (we already did), but DO
      // sleep before retrying so we don't hammer the RPC.
      await sleep(opts.intervalMs);
      continue;
    }
    const status = statuses.value[0] ?? null;
    if (status !== null) {
      if (status.err !== null && status.err !== undefined) {
        return { kind: "failed_on_chain", err: status.err };
      }
      const cs = status.confirmationStatus;
      if (cs === "confirmed" || cs === "finalized") {
        return { kind: "finalized" };
      }
      // "processed" or unknown — keep polling.
    }
    if (attempt < opts.maxAttempts) {
      await sleep(opts.intervalMs);
    }
  }
  return { kind: "timeout" };
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
