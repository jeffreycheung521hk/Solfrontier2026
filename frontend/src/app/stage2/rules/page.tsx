"use client";

// Stage 2 — `/stage2/rules` mock route.
//
// Frontend-only WatchRule creation mock. Lets the operator pick
// between Scenario A (Solend APR < 10% → withdraw delegated
// position) and Scenario B (BTC > 75000 AND ETH > 2300 AND SOL < 90
// → buy SOL with 5 USDC via Jupiter), and renders the canonical
// preview + locally-computed hash for the selected fixture.
//
// IMPORTANT — preview only:
//   - No signing, no wallet transaction.
//   - No backend mutation.
//   - No watcher, no live RPC, no Jupiter / Solend call.
//   - No "Sign" / "Execute" / "Submit" affordance anywhere.

import { useMemo, useState } from "react";

import { Stage2WatchRulePreview } from "@/components/stage2-watch-rule-preview";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
  EXPECTED_FIXTURE_RULE_HASHES,
  fixtureScenarioASolendAprBelow10,
  fixtureScenarioBBasketBuySol,
} from "@/lib/stage2-watch-rule";

type ScenarioKey = "a" | "b";

export default function Stage2RulesPage() {
  const [scenario, setScenario] = useState<ScenarioKey>("a");

  // Recompute the fixture once per scenario change; the preview hashes
  // it asynchronously inside `<Stage2WatchRulePreview>`.
  const ruleA = useMemo(() => fixtureScenarioASolendAprBelow10(), []);
  const ruleB = useMemo(() => fixtureScenarioBBasketBuySol(), []);

  return (
    <div className="space-y-6">
      <header className="space-y-1">
        <div className="text-xs uppercase tracking-wider text-muted-foreground">
          Stage 2 mock
        </div>
        <h1 className="text-2xl font-semibold tracking-tight">
          Watch rule preview
        </h1>
        <p className="text-sm text-muted-foreground">
          Frontend-only canonical preview of a Stage 2 WatchRule. No
          authorization, signing, watcher, or executor wiring — those land in
          downstream slices.
        </p>
      </header>

      <Alert>
        <AlertTitle className="text-sm">Preview only</AlertTitle>
        <AlertDescription className="text-xs">
          The hash computed below comes from the same Borsh layout the Rust
          crate uses. The two pinned hashes match
          <code className="mx-1">crates/types/src/stage2_watch_rule.rs</code>
          fixtures byte-for-byte. There is no Sign / Execute / Submit
          affordance on this page — and there will not be one until a
          downstream Stage 2 slice wires in the Authorization PDA.
        </AlertDescription>
      </Alert>

      <Tabs
        value={scenario}
        onValueChange={(v) => setScenario(v as ScenarioKey)}
      >
        <TabsList>
          <TabsTrigger value="a">Solend APR rule</TabsTrigger>
          <TabsTrigger value="b">Jupiter basket rule</TabsTrigger>
        </TabsList>

        <TabsContent value="a" className="pt-4">
          <Stage2WatchRulePreview
            rule={ruleA}
            expectedHashHex={
              EXPECTED_FIXTURE_RULE_HASHES.scenario_a_solend_apr_below_10
            }
          />
        </TabsContent>

        <TabsContent value="b" className="pt-4">
          <Stage2WatchRulePreview
            rule={ruleB}
            expectedHashHex={
              EXPECTED_FIXTURE_RULE_HASHES.scenario_b_basket_buy_sol
            }
          />
        </TabsContent>
      </Tabs>
    </div>
  );
}
