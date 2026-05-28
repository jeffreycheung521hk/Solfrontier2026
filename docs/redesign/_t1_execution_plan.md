# T1 — Technical Post Execution Plan

> Working file (`_` prefix). Not in docs/ freeze scope. Source of truth for the
> agent pipeline that produces Jeff's first external Solana technical post.
> A fresh Claude Code session can read this + memory and execute end-to-end.

## Context

Read first (in memory, not this file):
- `user_jeff.md` — Jeff is 2022 FTX-survivor retail-turned-builder; bilingual; Solo Frontier 2026 participant; values honesty over polish
- `project_post_hackathon_pivot.md` — Frontier ROI redefined as network + ecosystem learning, NOT winning. T1 is the highest-leverage artifact in that pivot.
- `feedback_no_overclaim.md` — only mark mainnet-proven what is mainnet-proven; never frame failures as victories
- `project_frontier_judging_freeze.md` — `docs/` proper is frozen until 2026-06-23. `docs/redesign/_*` working files are NOT in freeze scope.
- `feedback_traditional_chinese.md` — Chinese chat = 繁體 always; but **T1 post itself is English-primary** (global Solana audience)

T1 is one external dev.to longform post (~3000-4500 words) recapping ClawSolana's 7 mainnet fail-closed defenses. It is **NOT** a pitch, NOT a victory lap, NOT marketing. It is a case-study writeup for other Solana builders who hit similar Solend/Pyth/processor-parity issues.

### Jeff's locked answers (2026-05-28)
- **Title direction: A** — "Seven fail-closed defenses on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity"
- **Publish platform: dev.to** (primary)
- **Public repo link: `https://github.com/jeffreycheung521hk/Solfrontier2026`**
- **X handle: TBD** — if undecided at Agent 2 time, write literal `[X handle TBD]`, do NOT guess

### LINK & CITATION STRATEGY (load-bearing — read carefully)

Agents READ from the LOCAL filesystem (branch `integrate-phase5c-lite`), which has the FULL
evidence trail: `docs/WEEKLY_UPDATES.md` and `docs/proofs/` (INDEX.md, PHASE5G, PHASE6I,
PHASE4C, etc). So Agent 1 can verify everything locally — no problem there.

BUT the chosen public repo `Solfrontier2026` is the slimmed phase5c-lite redesign. It
HAS: ARCHITECTURE.md, SECURITY_BOUNDARIES.md, DEMO_EVIDENCE.md, PHASE5C_LITE_MAINNET_PROOF.md,
REVIEWER_QUICK_PATH.md, ROADMAP.md. It does **NOT** have `docs/proofs/` or `docs/WEEKLY_UPDATES.md`.

Therefore the post's clickable links must follow these rules:
1. **Per-tx verification = Solscan links only.** These are repo-independent and the strongest
   evidence. Every one of the 7 defenses + every success tx cites its Solscan URL. This is the
   primary verification channel.
2. **"See the code/architecture" = link `Solfrontier2026`** — and ONLY deep-link into files
   that exist there: ARCHITECTURE.md, SECURITY_BOUNDARIES.md, DEMO_EVIDENCE.md.
3. **DO NOT** create clickable GitHub links to `docs/proofs/*` or `docs/WEEKLY_UPDATES.md`
   pointing at Solfrontier2026 — they 404. If referencing the weekly log or a proof doc by
   name, mention it in prose WITHOUT a hyperlink, OR (optional) add ONE line near the closing:
   "The original frozen Frontier submission, with the full weekly engineering log and proof
   index, lives at github.com/jeffreycheung521hk/testingcrypto2."
4. Net effect: every hyperlink in the published post must resolve. Solscan carries verification;
   Solfrontier2026 carries code-reading; testingcrypto2 is an optional single mention for the
   full archive.

---

## 3-Agent Pipeline (Sequential — DO NOT parallelize)

```
Agent 1 (Evidence Miner, Explore, read-only)        [~5-10 min]
    └→ writes docs/redesign/_t1_evidence_pack.md
        ↓
Agent 2 (Technical Writer, general-purpose)         [~15-20 min]
    └→ reads _t1_evidence_pack.md
    └→ writes docs/redesign/_t1_draft_v1.md
        ↓
Agent 3 (Distribution Writer, general-purpose)      [~5-10 min]
    └→ reads _t1_draft_v1.md
    └→ writes docs/redesign/_t1_distribution_v1.md
        ↓
You (orchestrator) — relay all 3 outputs to Jeff for final edit pass
```

Parallel was rejected because Agent 3's 4 outputs all depend on Agent 2's draft.

---

## Agent 1 prompt — Evidence Miner

```
You are an evidence-verification agent for Jeff's external technical post on ClawSolana.
Your single job: verify and structure the factual basis for 7 fail-closed defense incidents
on Solana mainnet. Do NOT write prose. Do NOT speculate. Read-only.

Sources to read (in order of authority):
1. README.md (root) — claims index
2. docs/WEEKLY_UPDATES.md — dated chronological record with tx signatures
3. docs/proofs/INDEX.md — proof document index
4. docs/proofs/*.md — individual proof writeups
5. git log (focus on commits 2026-04-23 through 2026-05-12 mentioning Phase 5G or 6I-)
6. Anchor program source for actual error code references

For EACH of the 7 incidents below, produce:
- Phase ID + name
- Exact tx signature (or "no tx, caught pre-submission")
- Solscan link
- Slot number if Finalized
- On-chain error code (e.g. Custom(5) InvalidAccountOwner) or off-chain rejection reason
- Root cause (1-2 sentences, code-level)
- Fix (commit hash + file:line if possible)
- A 1-line lesson generalizable to other Solana builders
- "safe to publish" / "needs softening" / "do not publish" label with reason

The 7 incidents:

PHASE 5G — Deposit path (3 defenses, all caught BEFORE unsafe action)
  D1. find_program_address panic from 33-byte PDA seed
  D2. Zero-priority-fee tx dropped from cluster mempool
  D3. Pyth USDC oracle staleness — proposal rejected by lending policy evaluator

PHASE 6I-H/I/J/K — Withdraw processor parity (4 defenses, each reverted on-chain)
  D4. Custom(5) InvalidAccountOwner on RefreshObligation (Phase 6I-H fix)
  D5. NotEnoughAccountKeys on Withdraw (Phase 6I-I fix)
  D6. Custom(9) InvalidTokenProgram — slot 11 collision (Phase 6I-J fix)
  D7. ReadonlyDataModified — slot 4 lending_market writability (Phase 6I-K fix)

Final success tx for context (verify these are correct):
- Phase 5G success: 4M4ezLgm…Py3y at slot 415,571,964
- Phase 6I-K success: 3tMpTSEn…EZPJZ at slot 418,395,346

Also verify and include if found:
- Phase 4C baseline: 2QqSfDq…pxALs at slot 415,475,589
- W5i autonomous: takmz89b…jTv5 at slot 419,200,198

Output to: docs/redesign/_t1_evidence_pack.md

Tone: pure data. No prose. Table-heavy. Cite file:line when claiming code-level facts.

Hard rules:
- If memory disagrees with code/docs, code wins
- If no Solscan-verifiable tx, label "needs softening"
- If a fact is in memory but not in repo, label "memory only — verify before publish"
- DO NOT write to docs/ proper (frozen)
- DO NOT edit any source files
```

---

## Agent 2 prompt — Technical Writer (depends on Agent 1)

```
You are the technical writer for Jeff's first external Solana post. You write ONE
3000-4500 word English draft based on the evidence pack Agent 1 produced.

Required inputs:
- docs/redesign/_t1_evidence_pack.md (Agent 1's output — read first, fully)
- README.md (for architecture facts — typestate, capability, audit)
- docs/WEEKLY_UPDATES.md (for narrative context)
- user_jeff memory (for author voice — retail-to-builder, FTX 2022 survivor)

Output to: docs/redesign/_t1_draft_v1.md

STRUCTURE (word-counts are targets, not hard caps):

1. Hook (~250 words)
   - Open on ONE specific moment, e.g. Phase 6I-K finalization at slot 418,395,346
   - Establish credentials: "60 days, 175 commits, 7 mainnet finalized tx, 7 fail-closed defenses, 0 funds lost"
   - Establish person: "2022 FTX survivor; retail-to-builder; solo"
   - Thesis: "I documented each failure publicly because they're more useful than the successes"

2. 30-second architecture (~300 words)
   - Typestate pipeline (ASCII art is fine)
   - Capability boundary (LLM never holds SignTransaction)
   - Append-only audit
   - Why pre-submission enforcement matters on 400ms slots
   - Link to ARCHITECTURE.md

3. Defense Sequence 1 — Phase 5G Deposit (~750 words, 3 sub-sections of ~250 each)
   - Each defense: symptom / root cause / fix / lesson
   - Cite tx, file:line, error codes from evidence pack
   - Lessons: typestate pre-submission catch, no-double-submit semantics, policy-evaluator-before-processor

4. Defense Sequence 2 — Phase 6I-K Withdraw Processor Parity (~1000 words, 4 sub-sections of ~250 each)
   - This is the SEO core. The "processor.rs is source of truth, SDK helpers are advisory" lesson lives here.
   - Each of D4-D7: full symptom / root cause / fix / lesson
   - Include withdraw account-ordering table (before/after for each iteration)
   - End with: "processor.rs is the source of truth. SDK helpers are advisory."

5. Generalized lessons (~450 words)
   5 numbered takeaways, each 80-90 words:
   - processor.rs > SDK docs
   - pre-submission enforcement is the only viable layer on 400ms slots
   - compile-time safety > runtime check > prompt-engineered guardrail
   - failed-closed is more instructive than succeeded-once
   - honest scope (this is hackathon, not production)

6. What I'd do differently (~250 words)
   - Integration tests against deployed program ID, not localnet
   - Builder constraints derived from processor.rs reverse-engineering, not SDK
   - Open question to community: how do we standardize "processor parity" as an integration test layer?

7. Closing (~200 words)
   - Brief personal note: 2022 FTX, 60 days, solo
   - What I'm looking for: builders who hit similar parity issues (NOT funding, NOT a job pitch)
   - Frontier 2026 submission frozen until 2026-06-23
   - Contact: `[X handle TBD]` (literal, do not guess), repo link github.com/jeffreycheung521hk/Solfrontier2026
   - Optional single line: original frozen submission with full weekly log + proof index at github.com/jeffreycheung521hk/testingcrypto2

VOICE — LOCKED RULES (violating any = redo)

REQUIRED:
- First-person "I" / "we" voice — Jeff is the author
- Retail-to-builder framing in hook and closing
- Every claim with a tx, slot, or commit gets a verifiable link (Solscan or GitHub commit URL)
- Honest scope disclaimer surfaces at least once: "This is hackathon scope, not production-deployed, not audited."
- Every defense framed as "we hit X / learned Y" — NEVER "the system gracefully caught X"
- Solend / Pyth / Jupiter / processor authors framed with respect; learnings credited to their published code as source of truth

FORBIDDEN:
- Promotional adjectives: "revolutionary", "unprecedented", "novel", "first-of-its-kind", "game-changing", "industry-leading"
- Victory framing on the defenses — they were bugs in OUR builder that the system caught, not features of ClawSolana
- Implying Solend / Pyth have bugs — frame it as "deployed processor and SDK helpers can drift; processor.rs is the source of truth"
- Asking for funding, employment, or partnership in the post
- Comparing to other Frontier projects by name
- Em-dashes used as decorative pacing (1 max per section)
- Bullet-list dumps without prose connective tissue
- 繁中 mixed in — English-only

WORD COUNT: 3000-4500. Over 4500 = trim. Under 3000 = expand examples.

Title — Jeff locked direction A: "Seven fail-closed defenses on Solana mainnet — what 60
days shipping solo taught me about Solend processor parity". Use it as the working title;
you MAY offer 2 tightened variants at the bottom, but A is the default — do not re-litigate.

Citation strategy — obey the LINK & CITATION STRATEGY block in Context: Solscan for every tx,
Solfrontier2026 for code/architecture deep-links (ARCHITECTURE.md / SECURITY_BOUNDARIES.md /
DEMO_EVIDENCE.md only), NO clickable links to docs/proofs/* or WEEKLY_UPDATES.md.

Hard rules:
- Output to docs/redesign/_t1_draft_v1.md only
- DO NOT touch docs/ proper
- DO NOT push to git
- DO NOT modify any code
```

---

## Agent 3 prompt — Distribution Writer (depends on Agent 2)

```
You produce the distribution layer for T1. The draft is at docs/redesign/_t1_draft_v1.md.

Output to: docs/redesign/_t1_distribution_v1.md

Sections required:

1. dev.to publish metadata
   - Final title (pick one from Agent 2's 3 candidates, justify in 1 sentence)
   - Subtitle / cover description (1-2 sentences)
   - 4-6 tags from dev.to's vocabulary (e.g. solana, rust, defi, hackathon, ai)
   - Canonical URL plan if cross-posting

2. X thread (11 tweets)
   - Tweet 1: hook (mirror post hook, ~270 chars including a tx Solscan link)
   - Tweet 2: system TL;DR
   - Tweets 3-5: Phase 5G defenses (one each, with Solscan)
   - Tweets 6-9: Phase 6I-K defenses (one each, with Solscan)
   - Tweet 10: "processor.rs is source of truth" punchline
   - Tweet 11: link to dev.to + CTA "if you've hit similar parity issues, DM"
   - Each tweet ≤270 chars to allow attribution
   - No emojis except where natural (1-2 max in whole thread)

3. Peer outreach follow-up (3 messages)
   - Re-write Jeff's existing 3 Frontier peer DM targets (Enclz / Veclabs / wienerlabs)
     to now lead with "I just published this — your X work seemed relevant" + T1 link
   - Tighter than originals; each ≤500 chars

4. Chainlink DM rewrites (2 messages)
   - Type α (tech hook) and Type β (community ask), updated to reference T1 as conversation evidence
   - Each ≤500 chars

5. Publish-day operational checklist
   - Best HK time to post dev.to (target: Tuesday 09:00 HKT)
   - X thread timing (2 hours after dev.to)
   - Where to cross-post (Solana Discord channels, Reddit r/solana judgement, Hashnode mirror)
   - First-24h monitoring suggestions

Hard rules:
- Output to docs/redesign/_t1_distribution_v1.md only
- DO NOT publish anything — Jeff handles all external posting
- DO NOT add new technical claims not in the draft
- Match the draft's voice (no promotional adjectives)
```

---

## Final synthesis (orchestrator job)

After all 3 agents complete:
1. Read all three output files
2. Verify Agent 2 followed voice rules (run through FORBIDDEN list)
3. Verify Agent 3's X thread reflects Agent 2's actual content (no drift)
4. Surface to Jeff with one-screen summary:
   - Title chosen
   - Word count of draft
   - Any flagged "needs softening" claims from Agent 1 still in Agent 2's draft
   - 3 questions for Jeff's final review before publish

Do NOT auto-publish. Jeff publishes manually.

---

## Failure modes to watch

- **Voice drift**: Agent 2 lapsing into Medium-style "the system gracefully handles..." passive. Reject + re-prompt with example fix.
- **Fact drift**: Agent 2 introducing a claim not in evidence pack. Reject specific claim, demand citation.
- **Scope creep**: Agent 2 expanding beyond 7 defenses (e.g., adding W5i autonomous as "defense"). W5i is a SUCCESS, not a defense. Keep separate.
- **Tone drift**: Agent 2 or 3 implying Solend has bugs. Reject + re-prompt: "frame as parity, not bug".

---

## What this plan deliberately does NOT include

- MCP scaffold (T4) — deferred to Week 2 per Jeff's call
- Chinese version — deferred; English-primary first
- Visual diagrams beyond ASCII — keep complexity low for v1
- Cross-posting to Reddit / Hashnode — operational, not content; in Agent 3 checklist only
- Solend-specific outreach post-publish — separate decision after seeing post traction
