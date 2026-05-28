# T1 Distribution v1 — dev.to / X / peer outreach / Chainlink DM

Generated 2026-05-28 by Agent 3 (Distribution Writer). Source: `_t1_draft_v1.md`. Fact-checked against `_t1_evidence_pack.md` Decision lock.

Working file. Not in `docs/` freeze scope. No commit, no push, no publish from this artifact — these are drafts for Jeff to send manually.

---

## 1. dev.to publish metadata

### Final title — recommended

**Option 1 (locked working title — recommended, ship as-is):**

> Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity

**Rationale.** This is the Jeff-approved working title from the Decision lock. It carries both the personal frame ("60 days shipping solo") and the SEO core ("Solend processor parity"), which is the phrase any future Solana builder searching for the same drift will type. Option 2 trades the personal frame for a punchier code hook but loses the "60 days solo" credibility cue that anchors the post against overclaim. Option 3 is the safest but drops both SEO core and the framing that makes the post worth opening. Ship Option 1.

### Subtitle / cover description (≤200 chars)

> Sixty days. ~175 commits. Seven mainnet-finalized successes. Seven fail-closed incidents. Zero user funds lost. This is the long write-up of what the failures taught me about Solend's deployed processor.

### Tags (dev.to caps at 4 — pick top 4)

Recommended 4 in priority order:

1. `solana`
2. `rust`
3. `defi`
4. `security`

Fallback tags if any of the above is unavailable or already saturated:

5. `hackathon`
6. `llm`

`solana` is non-negotiable (the searcher we want is a Solana builder). `rust` brings the systems-language audience that will care about the typestate pipeline. `defi` is the domain match. `security` is load-bearing for the fail-closed framing. `hackathon` and `llm` are weaker on signal-to-noise but acceptable substitutes if needed.

### Canonical URL / cross-post note

- **Primary publication: dev.to.** Set canonical URL to the dev.to permalink itself (i.e. dev.to is the canonical source).
- **Cross-post candidates (after primary):** Hashnode, Mirror, personal GitHub Pages. Each cross-post must set `<link rel="canonical">` back to the dev.to URL so search ranking consolidates there and does not split.
- **X thread is not a canonical source.** The thread is a teaser pointing to the post, not a duplicate; no canonical handling needed.
- **Repo README link-back:** add a one-line pointer in `Solfrontier2026/README.md` (post-deadline, after 2026-06-23 unfreeze) pointing to the dev.to URL under a "Public writeup" section. Do not back-link before the Frontier judging freeze lifts.

---

## 2. X thread — 11 tweets, each ≤270 chars

Hard rules applied:
- No fake tx links. No Solscan link for D2 or D7.
- D2 framed as off-chain confirmation-tracker drop.
- D7 framed as code-supported, failed-tx signature truncated in our internal record.
- Failed-tx Solscan links exist only for D4 / D5 / D6 (per Decision lock), and are not inlined into tweets to keep each tweet self-contained — the full post carries the verifiable links.

---

**Tweet 1 — hook**

```
60 days solo on Solana mainnet.
~175 commits.
7 success transactions.
7 fail-closed incidents.
0 user funds lost.

This post is the long version of what the failures taught me — because the failures were more useful than the successes.

🧵
```

---

**Tweet 2 — architecture TL;DR**

```
Architecture in one breath:
LLM proposes. Control plane governs. Human signs.

The signing boundary lives in the Rust type system, not in a prompt. SignTransaction is not a capability the LLM-driven tool dispatcher carries. Ever.
```

---

**Tweet 3 — D1 (Phase 5G deposit path)**

```
D1 — PDA seed-length panic. Caught pre-submission.

A 33-byte seed slipped into find_program_address (MAX_SEED_LEN=32). The function panics. It does NOT return a Result.

Validate seed length at propose time, or your control plane crashes mid-flow.
```

---

**Tweet 4 — D2 (off-chain confirmation-tracker drop, no Solscan)**

```
D2 — zero-fee mempool drop. Off-chain confirmation-tracker catch (no Solscan).

Valid signature, RPC-accepted, never landed. getSignatureStatuses returned null across the block-validity window.

"Signature exists, never landed" = typed terminal failure, not a retry.
```

---

**Tweet 5 — D3**

```
D3 — Pyth USDC oracle staleness, rejected at the policy stage.

RuleKind::MaxOracleStalenessMs HardBlock fired. No approval, no signature.

If max_publish_age is a number in a prompt, prompt injection loosens your most safety-critical gate. Make it a compile-time enum.
```

---

**Tweet 6 — D4 (Phase 6I-K withdraw)**

```
D4 — Custom(5) InvalidAccountOwner on RefreshObligation.

We put sysvar::clock at slot 1. The deployed processor reads Clock via syscall and consumes [obligation, ...reserves]. The Clock shift made it dereference the wrong account as a reserve.
```

---

**Tweet 7 — D5**

```
D5 — NotEnoughAccountKeys on Withdraw.

SDK helper said 12 accounts. Deployed processor calls update_borrow_attribution_values, which iterates obligation.deposits via next_account_info() from slot 12+.

SDK lists known slots, not iterated ones.
```

---

**Tweet 8 — D6**

```
D6 — Custom(9) InvalidTokenProgram on Withdraw.

My "fix" put sysvar::clock at slot 11 because the SDK helper said so. The processor reads slot 11 as token_program_id. Got SysvarC1ock where it expected Tokenkeg.

SDK and processor disagreed on slot 11.
```

---

**Tweet 9 — D7 (code-supported, sig truncated, no Solscan)**

```
D7 — ReadonlyDataModified on Withdraw. Code-supported; failed-tx signature is truncated in our internal record, no Solscan link.

Slot 4 (lending_market) was new_readonly. Processor calls LendingMarket::pack(.., data.borrow_mut()) — needs writable. Flag flipped. Fix in tree.
```

---

**Tweet 10 — processor.rs punchline**

```
Sequence 2 in one line:

processor.rs is the source of truth. SDK helpers are advisory.

When they disagree on layout, writability, or signer flags, the deployed code is what executes. Build for the handler, not the helper.
```

---

**Tweet 11 — link + CTA**

```
Full post — 7 incidents, slot-by-slot withdraw parity table, what I'd do differently next time:

[T1 link]

Looking for Solana builders who've debugged similar processor-parity issues against Solend, Save, Kamino. Compare notes?
```

---

### Tweet-length self-audit

All 11 tweets checked against the 270-char ceiling (X allows 280; we keep 10 chars of safety margin for URL expansion and the eventual `[T1 link]` substitution):

| # | Char count (approx) | Status |
|---|---|---|
| 1 | ~240 | ✅ |
| 2 | ~225 | ✅ |
| 3 | ~250 | ✅ |
| 4 | ~265 | ✅ |
| 5 | ~265 | ✅ |
| 6 | ~245 | ✅ |
| 7 | ~245 | ✅ |
| 8 | ~250 | ✅ |
| 9 | ~260 | ✅ |
| 10 | ~225 | ✅ |
| 11 | ~225 (assuming `[T1 link]` expands to a t.co-shortened ~23 char URL) | ✅ |

Recount with the actual dev.to URL once it exists. None of the tweets reference a failed-tx Solscan link; the only verifiable on-chain anchors live in the long post.

---

## 3. Peer outreach follow-up messages (≤500 chars each)

Each message leads with the post link placeholder `[T1 link]`. Each ask is substantive technical, not networking-first. Replace `[T1 link]` and any `[intro context — to fill]` cues before sending. Tighten per-recipient based on the actual prior conversation; the versions below are first drafts.

---

### Peer outreach — Enclz

> [T1 link]
>
> Followed up after [intro context — to fill]. Wrote up 7 fail-closed incidents from a 60-day Solana solo build, including 4 consecutive on-chain reverts where Solend's deployed processor.rs and the published SDK helper disagreed on the withdraw account layout.
>
> Have you hit the same SDK-vs-processor drift on any third-party protocol you integrate against? The slot-by-slot parity table in the post is the artifact I wish I'd produced on day one — curious whether you keep an equivalent.

Length: ≈430 chars.

---

### Peer outreach — Veclabs

> [T1 link]
>
> 60 days solo on Solana mainnet — 7 fail-closed incidents, 0 user funds lost. Of the 7, the one I'd most want a second opinion on is D3: structural oracle staleness as a closed compile-time enum rather than an LLM-tunable parameter.
>
> If you've built oracle-guarded execution paths, would value pressure-testing on whether HardBlock-at-policy-stage is the right enforcement layer, or whether you'd push it earlier (schema) or later (simulator) in the pipeline.

Length: ≈430 chars.

---

### Peer outreach — wienerlabs

> [T1 link]
>
> Long-form on 7 fail-closed incidents from a 60-day Solana solo build. Technical core is Sequence 2: 4 consecutive Solend withdraw reverts on May 8, each from a different account-meta drift between the deployed processor and the published SDK helper.
>
> Question I'd put to you: have you adopted a publishable processor-parity test layer for any third-party Solana protocol you integrate against? If so, what shape — fork, devnet mirror, replay harness against deployed program ID?

Length: ≈475 chars.

---

## 4. Chainlink DM rewrites

### Type alpha — technical oracle / staleness hook

> [T1 link]
>
> 7 fail-closed incidents from 60 days solo on Solana mainnet. D3 is in your wheelhouse: Pyth USDC staleness HardBlock at the policy stage — before any approval object, before any signature.
>
> The call was to make max_publish_age a closed compile-time enum + deterministic evaluator, not an LLM-tunable parameter — on the theory that prompt injection loosens any threshold the model can argue with.
>
> Prior Chainlink hackathon work was on oracle-guard themes. Would value a critique: is closed-enum-at-policy the right shape, or should confidence-band + publish-age compose differently at the gate?

Length: ≈580 chars. Slightly over a typical DM-comfortable 500 — tighten per-channel if X DM limit applies (X DM cap is 10,000 chars so this is fine; LinkedIn InMail cap is 2,000 chars so fine; Telegram fine). The length is justified because the ask is specifically technical and a one-line cold DM with no oracle hook is the failure mode this type-alpha framing is meant to avoid.

---

## Notes for Jeff before sending

- **[X handle TBD]** appears in the draft's Closing — fill in before publishing the dev.to post and before any cross-post.
- **[T1 link]** placeholder appears in Tweet 11 and in all four outreach messages. Replace with the actual dev.to URL after publish.
- **[intro context — to fill]** appears in the Enclz outreach. If there's no prior conversation, delete that opening sentence rather than fabricating context.
- **Send order suggestion:** dev.to publish → wait ~1 hour for indexing → X thread with link → peer outreach within 24h while the post is still surfaceable on the recipient's feed → Chainlink DM separately, not bundled with the Solana-builder outreach burst.
- **Frontier judging freeze:** every recipient gets the public `Solfrontier2026` link, never the frozen `testingcrypto2` link. The post itself already handles this correctly.
