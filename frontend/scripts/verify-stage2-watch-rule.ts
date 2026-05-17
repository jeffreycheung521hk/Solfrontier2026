// Stage 2 — fixture parity verification.
//
// Computes the canonical rule hash for each scenario fixture using
// the same `stage2-watch-rule.ts` Borsh + SHA-256 path the UI uses,
// and compares against the hashes pinned in both the Rust crate
// (crates/types/src/stage2_watch_rule.rs) and the frontend's
// EXPECTED_FIXTURE_RULE_HASHES table.
//
// Exit code 0 = all hashes match; exit code 1 = mismatch.
//
// Run: `node frontend/scripts/verify-stage2-watch-rule.ts`
// (Node 22.7+ strips TS types natively; this repo runs on Node 24.)

import {
  EXPECTED_FIXTURE_RULE_HASHES,
  runRuleFixtureParityCheck,
} from "../src/lib/stage2-watch-rule.ts";

async function main(): Promise<void> {
  const report = await runRuleFixtureParityCheck();

  console.log("Stage 2 — canonical WatchRule fixture parity\n");
  for (const r of report.results) {
    const sigil = r.match ? "OK" : "MISMATCH";
    console.log(`  [${sigil}] ${r.name}`);
    console.log(`    computed: ${r.computed}`);
    console.log(`    expected: ${r.expected}`);
  }

  if (report.all_match) {
    console.log(
      `\nAll ${report.results.length} Stage 2 fixtures match the Rust-committed hashes.`,
    );
    process.exit(0);
  }

  console.error("\nFAIL — at least one Stage 2 fixture hash does not match.");
  console.error("Expected hashes (from Rust crate):");
  for (const [k, v] of Object.entries(EXPECTED_FIXTURE_RULE_HASHES)) {
    console.error(`  ${k}: ${v}`);
  }
  process.exit(1);
}

main().catch((err) => {
  console.error("verify-stage2-watch-rule: uncaught error:", err);
  process.exit(2);
});
