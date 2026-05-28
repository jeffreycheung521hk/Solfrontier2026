# T1 Agent 2 — Cross-session handoff

Written 2026-05-28 by the previous Claude Code session, after Agent 1 completed and Jeff locked Option A. Agent 2's first dispatch in the prior session disconnected mid-read (Anthropic infra socket drop, no partial output written). This file packages everything a fresh session needs to dispatch Agent 2 cleanly.

## Read me first (one Read call)

If you are the fresh Claude Code session continuing T1: read this file in full, then read the two files referenced in §Authority order. That is sufficient context — you do not need to re-derive the pipeline plan.

## Pipeline state as of 2026-05-28

- **Agent 1 (Evidence Miner)** — DONE. Output at `docs/redesign/_t1_evidence_pack.md`. 5 of 7 defenses fully verified with Solscan links; D2 and D7 labeled "needs softening". 4 success-tx baseline fully verified.
- **Publishability patch** — DONE. Added by orchestrator (not Agent 1). Lives at the bottom of `_t1_evidence_pack.md` under headings `## Publishability Decision Before Agent 2` and `## Decision lock for Agent 2 (2026-05-28)`. Encodes Jeff's Option A choice, locked title, locked 3/2/2 disclosure, D7 sig recovery result.
- **D7 sig recovery attempt** — DONE, FAILED. Searched `data/clawd-phase6i-k-live.log` and full `data/` dir for `5W3PzXnR` and `ReadonlyDataModified` — no matches. D7 stays "code-supported, tx-link missing". Do NOT re-attempt recovery.
- **Agent 2 (Technical Writer)** — NOT STARTED in this session. Previous session's dispatch socket-dropped before any write. No `_t1_draft_v1.md` on disk.
- **Agent 3 (Distribution Writer)** — HOLD until Jeff confirms Agent 2 output.

## Jeff's confirmed scope (do not re-ask)

- **Option A confirmed**: keep 7-defense article with softened D2/D7
- **Title locked**: `Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity`
- **Disclosure counts locked**: 3 Solscan-verified failed tx (D4/D5/D6) / 2 pre-submission catches (D1/D3) / 2 softened (D2/D7). Non-negotiable.
- **Forbidden alt titles**: anything with "verified on-chain defenses"; "Almost broke Solend"-style framing.
- **X handle**: undecided — write literal `[X handle TBD]`, do NOT guess.
- **No commit, no push, no docs/ proper, no source edits.**

## Authority order

If anything below contradicts the live files, the live files win (they are authoritative; this handoff is just orientation):

1. `docs/redesign/_t1_evidence_pack.md` §Decision lock for Agent 2 (2026-05-28) — highest priority
2. `docs/redesign/_t1_evidence_pack.md` §Publishability Decision Before Agent 2
3. `docs/redesign/_t1_evidence_pack.md` per-defense blocks + cross-citation map
4. `docs/redesign/_t1_execution_plan.md` voice rules, structure, word counts
5. `README.md`, `docs/proofs/*`, `docs/WEEKLY_UPDATES.md` for factual lookup

## What to do now (orchestrator job)

1. Verify pipeline state — `ls docs/redesign/` should show `_t1_evidence_pack.md`, `_t1_execution_plan.md`, this file, no `_t1_draft_v1.md`.
2. Dispatch ONE general-purpose Agent with the prompt below (verbatim).
3. When Agent 2 returns, do NOT auto-dispatch Agent 3. Read `_t1_draft_v1.md`, run the FORBIDDEN-list self-audit, and surface a short summary to Jeff. Wait for Jeff to confirm before Agent 3.

## Agent 2 prompt — dispatch verbatim

```
You are the technical writer for Jeff's first external Solana post. You write ONE 3000-4500 word English draft based on the evidence pack Agent 1 produced.

REQUIRED INPUTS (read in order, fully):
1. docs/redesign/_t1_evidence_pack.md — Agent 1's output PLUS the "Publishability Decision Before Agent 2" and "Decision lock for Agent 2 (2026-05-28)" sections that Jeff approved. The Decision lock is the highest-priority authority in any conflict.
2. docs/redesign/_t1_execution_plan.md — voice rules, structure, word counts, link/citation strategy
3. README.md — for architecture facts: typestate, capability boundary, audit
4. docs/WEEKLY_UPDATES.md — for narrative context (read but cite by name only, no hyperlink)
5. C:\Users\jeffr\.claude\projects\c--Users-jeffr-SolFrontier\memory\user_jeff.md — author voice: 2022 FTX-survivor retail-to-builder, bilingual, prior Chainlink hackathon builder, no prior Solana builder network

OUTPUT TO: docs/redesign/_t1_draft_v1.md

WORKING DIRECTORY: c:\Users\jeffr\SolFrontier
CURRENT BRANCH: integrate-phase5c-lite

═══════════════════════════════════════════════════════════════════
HARD CONSTRAINTS — VIOLATING ANY = REDO
═══════════════════════════════════════════════════════════════════

- Output ONLY to docs/redesign/_t1_draft_v1.md. Do NOT write any other file.
- READ-ONLY everywhere else. Do NOT edit any source code, do NOT touch docs/ proper (frozen until 2026-06-23), do NOT modify the evidence pack.
- Do NOT run git commit, git push, or any state-changing git command.
- The Decision lock section in _t1_evidence_pack.md is authoritative. If it conflicts with the execution plan, Decision lock wins.

═══════════════════════════════════════════════════════════════════
LOCKED ELEMENTS — DO NOT RE-LITIGATE
═══════════════════════════════════════════════════════════════════

**Title (working title — use verbatim as H1):**
> Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity

You MAY offer 2 tightened title variants at the very bottom of the draft for Jeff to pick from, but the working title above is the default and is NOT up for re-litigation.

**FORBIDDEN title alternatives:**
- Anything with "verified on-chain defenses" or similar (the D2/D7 evidence won't support it)
- "Almost broke Solend" framings — violates voice rule

**Front-loaded disclosure (within first ~200 words — MANDATORY):**

Within the hook section, you MUST include an honest evidence-strength paragraph in roughly this shape:

> Of the 7 incidents, 3 left full on-chain failed-tx traces you can verify on Solscan, 2 were caught pre-submission by code-level guardrails before any tx existed, and 2 are code-supported with the failed-tx signatures truncated in our internal record. All 7 share one outcome: zero user funds lost.

You may polish the wording. You may NOT change the counts. The mapping is fixed:
- 3 Solscan-verified failed tx: D4, D5, D6
- 2 pre-submission catches: D1, D3
- 2 softened: D2 (off-chain mempool drop), D7 (code-supported, tx-link truncated)

**Author identity (closing section):**
- Contact: write the LITERAL string `[X handle TBD]` — do NOT guess or invent an X/Twitter handle. The X handle is undecided as of 2026-05-28.
- Repo: `https://github.com/jeffreycheung521hk/Solfrontier2026`
- Optional single line allowed: "The original frozen Frontier submission, with the full weekly engineering log and proof index, lives at github.com/jeffreycheung521hk/testingcrypto2."

═══════════════════════════════════════════════════════════════════
STRUCTURE — word counts are targets, not hard caps
═══════════════════════════════════════════════════════════════════

1. **Hook (~250 words)**
   - Open on ONE specific moment — e.g. Phase 6I-K finalization at slot 418,395,346 (full tx `3tMpTSEn…EZPJZ`)
   - Establish credentials: "60 days, ~175 commits, 7 mainnet finalized success txs across the build, 7 fail-closed incidents, 0 funds lost"
   - Establish person: "2022 FTX survivor; retail-to-builder; solo; first Solana hackathon"
   - **The front-loaded 3/2/2 disclosure paragraph MUST land here**
   - Thesis: "I'm publishing the failures because they're more useful than the successes"

2. **30-second architecture (~300 words)**
   - Typestate pipeline (ASCII art is fine)
   - Capability boundary (LLM never holds SignTransaction)
   - Append-only audit
   - Why pre-submission enforcement matters on 400ms slots
   - Deep-link to ARCHITECTURE.md at `https://github.com/jeffreycheung521hk/Solfrontier2026/blob/main/ARCHITECTURE.md` (confirmed present on staging2026/main per evidence pack)

3. **Defense Sequence 1 — Phase 5G Deposit (~750 words, 3 sub-sections ~250 each)**
   - D1, D2, D3, in that order
   - Per defense: symptom / root cause / fix / lesson
   - For D2: APPLY THE D2 REFRAME from Publishability Decision. Frame as "off-chain confirmation-tracker / mempool-drop diagnostic". Do NOT cite Solscan. Do NOT cite a failed tx signature. Cite the priority-fee code path (`solend_tx_plan.rs` priority-fee constant) and the confirmation tracker.
   - For D1 and D3: cite the pre-submission catch + code path; no tx needed.

4. **Defense Sequence 2 — Phase 6I-K Withdraw Processor Parity (~1000 words, 4 sub-sections ~250 each)**
   - This is the SEO core. The "processor.rs is source of truth, SDK helpers are advisory" lesson lives here.
   - D4, D5, D6, D7, in that order
   - For D4, D5, D6: full symptom / root cause / fix / lesson with Solscan tx links + commit hash + post-fix file:line
     - D4 Solscan: https://solscan.io/tx/4YnQFUTmPQEKpDoJRuUS6optFUs8pyxQuFknjFzRAUooWy88YjPQGeAFr9Ncv8JwZPV7xdp2cEjHm2Djk6Q7gnTv
     - D5 Solscan: https://solscan.io/tx/25REcnNpUTbqx5eLV2L7LcN39vEMP7RZUZ9sNFZcURrBr2S6B7LJVcxzAcRNhtbs9znEswb8Gtob8Bmrk5zmT3Hp
     - D6 Solscan: https://solscan.io/tx/2mLa5bQvTN5daWkC5ehiDDbQvGpyyTZ8qkphs9jUGx8b191Xq54XH1HxVWETr67FYJay6oc3NPnfqAo9tFPuCpfY
   - For D7: APPLY THE D7 REFRAME from Publishability Decision. Frame as "processor-parity debugging incident with strong code evidence but truncated failed-tx record". Cite commit `8d59697` and post-fix source `withdraw.rs:283-289`. Do NOT fabricate or partially quote a Solscan link. Do NOT use the truncated form `5W3PzXnR…NMNRVg` as if it were a clickable link. (You MAY mention "the failed-tx signature was truncated in our internal record and could not be recovered" if helpful — this is honest and not overclaim.)
   - Include the withdraw account-ordering table from evidence pack (Slot / Account / pre-6I-H through 6I-K / processor source-of-truth columns) — drop it directly into this section
   - End the section with the punchline: "processor.rs is the source of truth. SDK helpers are advisory."

5. **Generalized lessons (~450 words)**
   5 numbered takeaways, each 80–90 words:
   1. processor.rs > SDK docs
   2. pre-submission enforcement is the only viable layer on 400ms slots
   3. compile-time safety > runtime check > prompt-engineered guardrail
   4. failed-closed is more instructive than succeeded-once
   5. honest scope (this is hackathon, not production, not audited)

6. **What I'd do differently (~250 words)**
   - Integration tests against deployed program ID, not localnet
   - Builder constraints derived from processor.rs reverse-engineering, not SDK
   - Open question to community: how do we standardize "processor parity" as an integration test layer?

7. **Closing (~200 words)**
   - Brief personal note: 2022 FTX, 60 days solo, first Solana hackathon
   - What I'm looking for: builders who hit similar parity issues (NOT funding, NOT a job pitch)
   - Frontier 2026 submission frozen until 2026-06-23
   - Contact: `[X handle TBD]` (literal — do not guess), repo `github.com/jeffreycheung521hk/Solfrontier2026`
   - Optional single line: original frozen submission + weekly log + proof index at `github.com/jeffreycheung521hk/testingcrypto2`

8. **Title alternatives (bottom of draft, after main content)**
   - Header: `## Title alternatives for Jeff to choose from`
   - The working title (locked) as option 1
   - 2 tightened variants as option 2 and option 3
   - One sentence each on tradeoff

═══════════════════════════════════════════════════════════════════
VOICE — LOCKED RULES (violating any = redo)
═══════════════════════════════════════════════════════════════════

REQUIRED:
- First-person "I" / "we" voice — Jeff is the author
- Retail-to-builder framing in hook AND closing
- Every claim with a tx, slot, or commit gets a verifiable link (Solscan or GitHub commit URL on Solfrontier2026)
- Honest scope disclaimer surfaces at least once: "This is hackathon scope, not production-deployed, not audited."
- Every defense framed as "we hit X / learned Y" — NEVER "the system gracefully caught X"
- Solend / Pyth / Jupiter / processor authors framed with respect; learnings credited to their published code as source of truth

FORBIDDEN:
- Promotional adjectives: "revolutionary", "unprecedented", "novel", "first-of-its-kind", "game-changing", "industry-leading"
- Victory framing on the defenses — they were bugs in OUR builder that the system caught, not features of ClawSolana
- Implying Solend / Pyth have bugs — frame as "deployed processor and SDK helpers can drift; processor.rs is the source of truth"
- "Almost broke Solend" or similar
- Asking for funding, employment, or partnership in the post
- Comparing to other Frontier projects by name
- Em-dashes used as decorative pacing (1 max per section)
- Bullet-list dumps without prose connective tissue
- 繁中 mixed in — English-only
- Inventing tx signatures, commit hashes, slot numbers, or file paths not in the evidence pack
- Treating W5i autonomous as a "defense" — W5i is a SUCCESS proof, not in scope here

═══════════════════════════════════════════════════════════════════
CITATION STRATEGY (load-bearing — follow exactly)
═══════════════════════════════════════════════════════════════════

Per evidence pack Cross-citation map:

- **Solscan tx links** — always resolve. Use freely.
- **Safe deep-links to Solfrontier2026**: `ARCHITECTURE.md` and `ROADMAP.md` (confirmed present on staging2026/main). The other four redesign docs (`SECURITY_BOUNDARIES.md`, `DEMO_EVIDENCE.md`, `PHASE5C_LITE_MAINNET_PROOF.md`, `REVIEWER_QUICK_PATH.md`) are NOT yet present — mention by name in prose only, no hyperlink.
- **Code file:line links** (e.g. `crates/gateway/src/integrations/solend/withdraw.rs:283-289`) — safe to deep-link to Solfrontier2026 (these files exist on staging2026/integrate-phase5c-lite). Use the form: `https://github.com/jeffreycheung521hk/Solfrontier2026/blob/main/crates/gateway/src/integrations/solend/withdraw.rs#L283-L289`
- **Commit hashes** (`57f135f`, `d7d960d`, `e4195fa`, `8d59697`) — exist on Solfrontier2026 staging branches. Safest form: cite short hash + commit subject in prose, NO hyperlink (so the post doesn't break if Jeff hasn't pushed yet at publish time).
- **`docs/proofs/*` and `docs/WEEKLY_UPDATES.md`** — mention by name in prose only, NO hyperlink (per execution plan policy).
- **README.md / PITCH.md root** — exist on Solfrontier2026; safe to deep-link if needed.

WORD COUNT: 3000–4500 total. Over 4500 = trim. Under 3000 = expand examples.

═══════════════════════════════════════════════════════════════════
RETURN MESSAGE WHEN DONE
═══════════════════════════════════════════════════════════════════

After writing the file, return a short summary (under 300 words):
- Final word count
- Which 3 title options you placed at the bottom (just the titles, one line each)
- Confirmation that the 3/2/2 disclosure landed in the hook
- Confirmation that D2 and D7 followed the forbidden-framing rules (no Solscan claims, no fabricated sigs)
- Any FORBIDDEN-list items you noticed you almost wrote and corrected (self-audit)
- Any open question for Jeff before he reviews

Do NOT paste the full draft in your reply. Jeff will read it from disk.
```

## Suggested first message for Jeff in the new window

If Jeff wants the new session to dispatch immediately, the shortest viable opening is:

> 讀 `docs/redesign/_t1_agent2_session_handoff.md` 然後跟住入面嘅指示 dispatch Agent 2。完成後唔好自動開 Agent 3 — surface summary 俾我睇。

The handoff file gives the new session everything: pipeline state, locked decisions, the verbatim Agent 2 prompt, and the after-Agent-2 protocol (no auto-Agent-3, summary first).

## Failure-recovery note

If Agent 2 socket-drops again before writing `_t1_draft_v1.md`:
- No partial state to recover (file either exists or doesn't)
- Re-dispatch with the same prompt — it's self-contained and the inputs on disk are stable
- Do not modify the evidence pack between attempts — its lock is authoritative

If Agent 2 writes a partial file then drops:
- Inspect the file. If it covers through section 4 (Defense Sequence 2) coherently, surface to Jeff for review and ask whether to (a) keep + manually finish, (b) re-dispatch and overwrite. Do not silently overwrite a partial file.
