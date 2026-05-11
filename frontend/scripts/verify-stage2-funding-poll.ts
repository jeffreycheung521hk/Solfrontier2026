// Stage 2 — pure unit tests for `pollSignatureStatus`.
//
// Three mock-Connection branches:
//   1. finalized       — first poll returns null, then status with
//                        err=null + confirmationStatus="finalized".
//                        Expected: { kind: "finalized" }.
//   2. failed_on_chain — first poll returns status with err set.
//                        Expected: { kind: "failed_on_chain" }.
//   3. timeout         — every poll returns null.
//                        Expected: { kind: "timeout" }.
//
// Each test passes a mock that satisfies `SignatureStatusFetcher`
// (the narrow structural type the helper depends on) — no real
// JSON-RPC client, no network, no real signatures.
//
// Run: `node frontend/scripts/verify-stage2-funding-poll.ts`
// (Node 22.7+ strips TS types natively; this repo runs on Node 24.)

import {
  pollSignatureStatus,
  SignatureStatusNetworkError,
  type SignatureStatusFetcher,
} from "../src/lib/stage2-funding.ts";

type StatusEntry =
  | { err: unknown | null; confirmationStatus?: string | null }
  | null;

/// Build a mock that returns the supplied entries in order. After
/// the array is exhausted, returns the final entry indefinitely.
function makeMockConnection(entries: StatusEntry[]): SignatureStatusFetcher {
  let idx = 0;
  return {
    async getSignatureStatuses(_sigs, _opts) {
      const value = idx < entries.length ? entries[idx] : entries[entries.length - 1];
      idx += 1;
      return { value: [value] };
    },
  };
}

/// Mock that throws N times in a row, then returns the supplied entry.
function makeThrowingMockConnection(
  throwTimes: number,
  thenEntry: StatusEntry,
): SignatureStatusFetcher {
  let idx = 0;
  return {
    async getSignatureStatuses(_sigs, _opts) {
      idx += 1;
      if (idx <= throwTimes) {
        throw new Error(`mock RPC failure attempt ${idx}`);
      }
      return { value: [thenEntry] };
    },
  };
}

let failures = 0;
function expectEq(label: string, actual: unknown, expected: unknown): void {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a === e) {
    console.log(`  [OK] ${label}`);
  } else {
    console.error(`  [FAIL] ${label}`);
    console.error(`    expected: ${e}`);
    console.error(`    actual:   ${a}`);
    failures += 1;
  }
}

async function main(): Promise<void> {
  console.log("Stage 2 — pollSignatureStatus unit tests\n");

  // ── 1. finalized ────────────────────────────────────────────────────
  const finalizedResult = await pollSignatureStatus(
    makeMockConnection([
      null,
      null,
      { err: null, confirmationStatus: "finalized" },
    ]),
    "sig_finalized",
    { maxAttempts: 10, intervalMs: 0 },
  );
  expectEq("finalized after two null polls", finalizedResult, {
    kind: "finalized",
  });

  // ── 1b. confirmed counts as finalized (per helper contract) ────────
  const confirmedResult = await pollSignatureStatus(
    makeMockConnection([{ err: null, confirmationStatus: "confirmed" }]),
    "sig_confirmed",
    { maxAttempts: 10, intervalMs: 0 },
  );
  expectEq("confirmed status terminates as finalized", confirmedResult, {
    kind: "finalized",
  });

  // ── 1c. processed status keeps polling, then finalizes ─────────────
  const processedThenFinalizedResult = await pollSignatureStatus(
    makeMockConnection([
      { err: null, confirmationStatus: "processed" },
      { err: null, confirmationStatus: "finalized" },
    ]),
    "sig_processed_then_finalized",
    { maxAttempts: 10, intervalMs: 0 },
  );
  expectEq(
    "processed is NOT terminal; promotion to finalized observed",
    processedThenFinalizedResult,
    { kind: "finalized" },
  );

  // ── 2. failed_on_chain ──────────────────────────────────────────────
  const onChainErr = { InstructionError: [0, { Custom: 42 }] };
  const failedResult = await pollSignatureStatus(
    makeMockConnection([
      {
        err: onChainErr,
        confirmationStatus: "confirmed",
      },
    ]),
    "sig_failed",
    { maxAttempts: 10, intervalMs: 0 },
  );
  expectEq(
    "status.err set → failed_on_chain with err passed through",
    failedResult,
    { kind: "failed_on_chain", err: onChainErr },
  );

  // ── 3. timeout (null forever) ──────────────────────────────────────
  const timeoutResult = await pollSignatureStatus(
    makeMockConnection([null]),
    "sig_timeout",
    { maxAttempts: 3, intervalMs: 0 },
  );
  expectEq("null status for maxAttempts polls → timeout", timeoutResult, {
    kind: "timeout",
  });

  // ── 4. transient throws under threshold are tolerated ──────────────
  const recoveredResult = await pollSignatureStatus(
    makeThrowingMockConnection(2, {
      err: null,
      confirmationStatus: "finalized",
    }),
    "sig_recovered",
    { maxAttempts: 10, intervalMs: 0 },
  );
  expectEq(
    "≤3 consecutive throws are tolerated; finalized once chain returns",
    recoveredResult,
    { kind: "finalized" },
  );

  // ── 5. > 3 consecutive throws → SignatureStatusNetworkError ────────
  let threw = false;
  try {
    await pollSignatureStatus(
      makeThrowingMockConnection(99, null),
      "sig_dead_rpc",
      { maxAttempts: 10, intervalMs: 0 },
    );
  } catch (err) {
    threw = true;
    expectEq(
      "> 3 consecutive throws raise SignatureStatusNetworkError",
      err instanceof SignatureStatusNetworkError,
      true,
    );
  }
  if (!threw) {
    console.error("  [FAIL] dead RPC mock: helper did not throw");
    failures += 1;
  }

  if (failures > 0) {
    console.error(`\nFAIL — ${failures} assertion(s) failed.`);
    process.exit(1);
  }
  console.log("\nAll pollSignatureStatus branches verified.");
  process.exit(0);
}

main().catch((err) => {
  console.error("verify-stage2-funding-poll: uncaught error:", err);
  process.exit(2);
});
