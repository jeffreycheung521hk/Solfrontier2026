// Stage 1 Tail — fixture parity verification.
//
// Computes the canonical hash for each of the three committed fixtures
// (solend_deposit, solend_withdraw_all, jupiter_swap) using the same
// `canonical-intent.ts` Borsh + SHA-256 implementation the UI uses, and
// compares against the expected hashes baked into both the Rust crate
// (crates/types/src/canonical_intent.rs) and the frontend lib's
// EXPECTED_FIXTURE_HASHES table.
//
// Exit code 0 = all three hashes match; exit code 1 = mismatch (and
// the diffs are printed to stderr). Run with:
//
//   node frontend/scripts/verify-canonical-intent.ts
//
// Node 22.7+ strips TS types natively (Node 24 in this repo's
// development environment); no separate transpile step required.

import {
  EXPECTED_FIXTURE_HASHES,
  runFixtureParityCheck,
} from "../src/lib/canonical-intent.ts";

async function main(): Promise<void> {
  const report = await runFixtureParityCheck();

  console.log("Stage 1 Tail — canonical-intent fixture parity\n");
  for (const r of report.results) {
    const sigil = r.match ? "OK" : "MISMATCH";
    console.log(`  [${sigil}] ${r.name}`);
    console.log(`    computed: ${r.computed}`);
    console.log(`    expected: ${r.expected}`);
  }

  if (report.all_match) {
    console.log(
      `\nAll ${report.results.length} fixtures match the Rust-committed hashes.`,
    );
    process.exit(0);
  }

  console.error("\nFAIL — at least one fixture hash does not match.");
  console.error("Expected hashes (from Rust crate):");
  for (const [k, v] of Object.entries(EXPECTED_FIXTURE_HASHES)) {
    console.error(`  ${k}: ${v}`);
  }
  process.exit(1);
}

main().catch((err) => {
  console.error("verify-canonical-intent: uncaught error:", err);
  process.exit(2);
});
