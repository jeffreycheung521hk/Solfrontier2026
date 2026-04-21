# Lending Policy Vocabulary — Design Spec (Part 1: Semantics & Risk Boundaries)

**Status:** draft — Part 1 of N
**Scope of Part 1:** design philosophy, V1 scope, action classification, freshness semantics, failure semantics.
**Out of scope for Part 1 (explicit):** snapshot field schema, complete rule taxonomy, implementation roadmap, abstraction triggers, protocol choice.

This document is a **design spec**, not a brainstorm. Each section below makes a decision that later work MUST respect. Parts 2+ will build on these decisions; they will not re-open them without explicit prompt authorization.

---

## 1. Design Philosophy

### 1.1 Why lending is a different category of risk than transfers or swaps

Every existing Claw policy (transfer amount caps, swap mint allowlists, slippage caps) is **transaction-local**: the policy looks at a single proposed transaction and decides allow / require-approval / reject based on inputs visible in that transaction alone.

Lending is not transaction-local. To decide whether a proposed `Borrow` or `Withdraw` is safe, Claw must know:

- what the user already holds as collateral in the obligation
- what the user already owes
- the current oracle prices feeding LTV and health-factor calculations
- how recently any of the above was observed on chain

None of this is contained in the transaction bytes. It must be read from chain state and normalized before a policy decision is even possible.

This makes lending policy the **first state-aware control-plane surface** Claw owns. It is a new *category* of system state — not one more rule in the existing `PolicyCondition` enum.

### 1.2 Why V1 is fail-closed

Every unknown in the state-aware path becomes a potential mis-judgment of risk. If Claw cannot fetch the obligation, or cannot parse a reserve, or sees a stale oracle, then any allow-decision at that moment is a decision made on incomplete information.

A mis-judged `Borrow` or `Withdraw` under bad collateral conditions is not an inconvenience — it can push the user into a liquidatable state they did not choose. The asymmetry between "wrongly permitting a risk-increasing action" and "wrongly rejecting a risk-reducing one" is large and one-sided.

**V1 therefore treats every uncertainty as a Hard Block, with no degradation paths.** No recent-enough-last-known fallback, no best-effort, no partial snapshot inference. If the state-aware path cannot produce a fully-grounded decision, the action does not proceed.

This is a **defaulted** position, not a permanent one. V2+ may add narrowly-scoped fail-open exceptions behind explicit opt-in. V1 does not have those exceptions.

### 1.3 Why V1 allows only risk-reducing actions

`Deposit` and `Repay` are risk-reducing: in the worst case (wrong amount, wrong mint routing, bad parameter), the bug's effect is bounded — it does not cross the liquidation threshold by construction. These actions are the right surface on which to build, debug, and stabilise the new state-aware framework: snapshot fetch, metrics derivation, rule vocabulary, failure handling, cache policy, and the local-read-model-plus-simulate composition.

`Withdraw` and `Borrow` are risk-increasing: a bug in oracle-freshness handling, an off-by-one in LTV derivation, a stale reserve parameter, a mis-set interest-accrual timestamp — any single one of these can move the user from safe to liquidatable on chain.

V1 therefore ships risk-reducing actions only. The state-aware machinery is exercised and hardened under Deposit/Repay before any risk-increasing action is enabled. This is a risk-management ordering, not merely a convenience ordering.

### 1.4 Why B main + A backstop (not one or the other)

Two implementation stances were considered:

- **A alone** — rely on RPC `simulateTransaction` as the single gate. The network actually executes the proposed transaction against current state and reports the post-state.
- **B alone** — rely on a local read model plus derived metrics and policy rules.

Each has a blind spot.

A alone treats Claw as a pass-through that defers every judgment to the chain's own simulation. That violates Claw's reason for existing: we are a control plane whose value is making policy decisions **earlier and more explicitly** than the protocol itself does. A control plane that decides nothing is not a control plane.

B alone risks missing real protocol behavior. Our local metrics are an approximation; the protocol's on-chain math, fee schedules, interest accrual, reserve state updates, and atomicity behaviors are the authoritative source of what will actually happen if the transaction lands.

V1 therefore composes both, in a fixed order:

1. **Local state-aware policy gate** is primary. It runs first and can reject alone. A rejection here never reaches simulate.
2. **RPC `simulateTransaction`** is the final safety backstop. It runs only after the local gate accepts. A failure here is still a rejection.

The order matters: local-first avoids round-trip waste when Claw can already decide, and preserves the control-plane invariant that policy decisions are Claw-authored, not network-authored. Simulate is the line of defense against the cases where our local model diverges from protocol reality.

This composition is a V1 invariant. Variants that drop either leg — "skip simulate for speed", "skip local gate because simulate will catch it anyway" — are explicitly not V1.

---

## 2. V1 Scope

### 2.1 In-scope

- **State-aware policy language for lending** — a vocabulary of rules that can reference both transaction inputs AND position-level state (LTV, health factor, oracle freshness, etc.). Part 2+ of this spec will enumerate the rule types.
- **Risk-reducing actions only**: `Deposit`, `Repay`.
- **Local read model + local metrics** as the primary implementation direction for state-aware policy evaluation. Exact schema and fetch shape are deferred to Part 2+.
- **RPC `simulateTransaction` as final backstop**, invoked after the local gate accepts.
- **Single-protocol integration for V1.** Protocol choice is not decided in this document and is deferred.

### 2.2 Out-of-scope (explicit)

- **`Withdraw` actions.** V1 blocks unconditionally until V2+ has the post-action risk machinery.
- **`Borrow` actions.** Same.
- **Cross-protocol abstractions** (generic `LendingProtocol` trait, multi-protocol router, shared obligation abstractions). V1 builds one concrete protocol. Any abstraction will be forced by the *second* protocol, not speculated before the first.
- **Frontend / UX.** Not in this spec; not in V1.
- **Detailed implementation roadmap.** Step-by-step execution plan (what file, what test, what order) is the subject of a later prompt — not Part 1.
- **Fail-open exceptions.** V1 does not define any. Reasons a narrow fail-open might be considered later must be argued in V2+ with specific threat modeling.
- **Liquidation participation, leverage loops, recursive borrows, flash loans.** All V2+ or beyond, if ever.
- **Historical position tracking, PnL attribution, reporting.** Not a V1 concern.

### 2.3 What "V1" means operationally

"V1" in this document refers to the first shipped state-aware lending policy surface that is allowed to run against mainnet with real funds. It is not an internal milestone name; it is the scope boundary that determines what behaviours reach the user.

---

## 3. Action Classification

V1 recognizes exactly two classes of lending actions.

### 3.1 `RiskReducingAction`

An action whose worst-case on-chain effect cannot move the user from a not-liquidatable state to a liquidatable one, assuming the action's parameters are bounded and the protocol executes as documented.

Members in V1:

- **`Deposit`** — supply additional collateral to the obligation.
- **`Repay`** — reduce outstanding debt on the obligation.

Properties:

- Worst-case bug effect is **bounded and reversible-by-user**. A deposit that mis-routes can be withdrawn later; a repayment that overpays will typically be handled by the protocol.
- Does not cross a liquidation threshold by construction: reducing debt or increasing collateral does not reduce the health factor.
- Appropriate surface on which to stabilise the state-aware framework before risk-increasing actions are introduced.

Handling in V1:

- Subject to the full state-aware policy gate (local read model + metrics + rules) AND to the RPC simulate backstop.
- Not auto-approved purely because they are risk-reducing. They still pass through the composition in §1.4 — the classification determines *eligibility*, not *verdict*.

### 3.2 `RiskIncreasingAction`

An action whose on-chain effect can move the user from a not-liquidatable state to a liquidatable one, depending on oracle prices, collateral composition, and obligation state at execution time.

Members in V1:

- **`Withdraw`** — remove collateral from the obligation.
- **`Borrow`** — add new debt to the obligation.

Handling in V1:

- **Hard Block**, unconditionally. V1 does not implement post-action predicted-metric derivation, and therefore has no sound basis for allowing a risk-increasing action.
- No policy rule in V1 can override this block. It is not expressible as a rule; it is a scope boundary.
- V2+ will introduce post-action simulation, predicted LTV / health-factor derivation, and the policy vocabulary needed to judge risk-increasing actions. Until that work lands and is proven, the block stands.

### 3.3 Why this is a scope boundary, not a rule

The reason `Withdraw` / `Borrow` are blocked in V1 is not that a policy rule has been set to reject them. It is that **V1 has not implemented the machinery required to evaluate them safely**. Expressing the block as a policy rule would suggest a user could flip a config flag to enable them. The block lives at the scope boundary of V1 specifically to prevent that.

Parts 2+ of this spec will define the vocabulary, metrics, and simulation surfaces that V2+ needs. None of that work will retroactively permit risk-increasing actions under V1.

---

## 4. Freshness / Staleness Semantics

State-aware policy is only as sound as the state it operates on. Freshness is therefore a **first-class input to the policy decision**, not a hidden implementation detail.

### 4.1 Definitions

- **Snapshot freshness** — how recently the obligation, reserve, and oracle accounts that make up the position read model were observed on chain. Measured relative to the slot or wall-clock timestamp of last successful fetch.
- **Oracle staleness** — the gap between the oracle's last on-chain publish time and the moment the policy decision is made. This is a property of the oracle account itself, not of Claw's cache.
- **State staleness** — the gap between the read-model fetch time and the current chain head at decision time. Distinct from oracle staleness: the chain may have advanced many slots since Claw's last fetch, even if the underlying oracle has not republished.

Each of these is independently observable, and each can fail independently. V1 treats them as independently enforceable conditions.

### 4.2 Freshness is a policy decision input

Freshness does not sit alongside the policy engine; it is consumed *by* the policy engine. Concretely:

- Every policy evaluation that depends on the read model MUST be given, or MUST derive, an explicit freshness descriptor for each input (oracle publish time, fetch time, chain head at decision time).
- Policy rules can reference freshness. V2+ will include rule types like `MaxOracleStalenessMs`. V1 does not yet need to enumerate these; it only needs to make room for them semantically and make sure implementation honours them.
- A policy decision produced without freshness metadata is a bug. V1 implementations MUST make this representable in types (or equivalent compile-time enforcement) so that "we evaluated a rule without knowing the freshness of its inputs" cannot happen silently.

### 4.3 Staleness default handling in V1

V1's default stance on any form of unacceptable staleness is:

**Stale ⇒ Hard Block.**

Specifically, in V1:

- Oracle staleness exceeding a defined threshold is a Hard Block. The threshold itself is a configuration concern deferred to later spec parts; the *semantic rule* is fixed here.
- Snapshot staleness — the read model was fetched too long ago relative to chain head — is a Hard Block. A V1 implementation is free to refresh transparently to avoid this; it is not free to permit a decision on stale data.
- Partially-stale snapshots (one oracle fresh, another stale) are a Hard Block. Staleness is not averaged. The worst stale component dominates.
- Deactivated or in-process-of-deactivating state components (e.g. an ALT mid-deactivation, a reserve marked for removal) are a Hard Block for V1. V1 does not attempt to reason about transient states.

### 4.4 Thresholds are deferred; the semantic is not

The specific numeric thresholds (how many milliseconds of oracle lag is "too stale"? how many slots of chain advance?) are not decided in Part 1. They belong to Part 2+ when rule vocabulary is enumerated and tuned to a specific protocol.

What Part 1 fixes is the *semantic*: once a threshold is established, exceeding it is Hard Block, not Warn, not Approve-with-caveat. Whenever later parts introduce a threshold, that threshold inherits this semantic without restating it.

### 4.5 What this spec does not decide about freshness

- Exact numeric thresholds (see §4.4).
- Cache/refresh strategy (push-based subscriptions vs. pull-on-evaluate vs. TTL).
- Which accounts need re-fetching per decision vs. reusable across decisions.
- Per-protocol adaptations (some protocols publish oracle freshness differently).

These are Part 2+ concerns. The semantic in §4.3 applies across all of them.

---

## 5. Failure Semantics

This section defines, exhaustively for V1, what happens when anything in the state-aware path does not produce a clean, confidently-interpreted result. It is the most load-bearing section in Part 1.

### 5.1 Failure model

The state-aware path has a chain of steps: RPC fetch → account parse → snapshot assembly → freshness check → metrics derivation → rule evaluation → verdict. Each step has well-defined input requirements. Failure is any condition where a step cannot produce outputs that meet its contract with the next step.

Failures in V1 are **not distinguished by cause at the verdict layer**. RPC timeout, malformed account data, missing oracle, uncomputable metric — all produce the same verdict: **Hard Block**. The cause is logged and audited; it does not change the decision.

This is a deliberate simplification. Distinguishing failure causes at decision time invites fail-open drift ("we can treat this particular failure as benign"). V1 does not allow that drift.

### 5.2 Enumerated failure conditions and handling

Each of the following is a V1 Hard Block. This list is not exhaustive of *all* possible failures; it is exhaustive of the *categories* V1 must account for.

- **RPC / network failure** (timeout, connection refused, rate-limited, circuit-broken endpoint) — Hard Block. Do not retry silently to the point of hiding the failure from the decision.
- **Obligation account fetch failure** (not found, account does not exist for the bound wallet, transient fetch error) — Hard Block. V1 does not attempt to derive from prior knowledge; a missing obligation is an unknown position, and a decision on an unknown position is not a V1 decision.
- **Reserve account fetch failure** — Hard Block. Reserves are essential inputs to LTV / health factor derivation.
- **Oracle account fetch failure** — Hard Block. Without current prices, no V1 policy decision about a position is sound.
- **Account parse / deserialization failure** — Hard Block. Parse bugs, version drift, or corrupt data all fall under this. V1 does not attempt to use partial fields.
- **Snapshot assembly incomplete** (any required component missing for the protocol being evaluated) — Hard Block.
- **Oracle stale beyond threshold** — Hard Block. See §4.3.
- **State stale beyond threshold** — Hard Block. See §4.3.
- **Metric uncomputable** (e.g. LTV would require division by zero collateral; interest accrual timestamp invalid; arithmetic overflow in health-factor derivation) — Hard Block.
- **Rule expression references data not in the snapshot** (a V2+ rule deployed against a V1 snapshot schema, for example) — Hard Block. V1 evaluator must refuse, not silently return `false`.

### 5.3 No fail-open exceptions in V1

V1 has **zero** fail-open paths. This is a deliberately absolute statement.

- There is no "old-cache fallback" path.
- There is no "assume last-known-good" path.
- There is no "oracle recently fresh, allow" path.
- There is no "the protocol will likely catch this anyway so let it through" path.

If a later version wants to introduce a fail-open path, it must:

1. State the specific failure class it applies to.
2. State the bounded behaviour allowed under fail-open (e.g. "Deposit only", "up to X amount").
3. Justify why the blast radius of allowing that fail-open is small relative to the availability cost of fail-closed.
4. Gate itself behind an explicit opt-in flag, not a default.

V1 does not perform any of this analysis, so V1 does not ship any fail-open paths. The default answer is "Hard Block."

### 5.4 Observability of failures

Hard-Blocking on a failure is not sufficient; V1 must make every failure **observable to the operator and auditable after the fact**. Concretely:

- Every Hard Block caused by a failure MUST produce an audit event naming the failure category from §5.2.
- Every such audit event MUST carry enough context to diagnose the failure (which account, which protocol, which session, which rule was evaluating — the granularity depends on where in the chain the failure occurred).
- Operator-facing error messages MUST be concrete about *what* failed, without exposing secret material. "Oracle stale" is concrete; "state-aware policy failed" is not.
- The audit event taxonomy is subject to later spec parts, but it must exist.

### 5.5 Retry is not a V1 policy decision

V1 does not decide on behalf of the caller whether to retry. A Hard Block is terminal for the specific proposed action. The caller (user, agent, or operator) is responsible for deciding whether to resubmit the action later when conditions may have changed.

V2+ may introduce a "retry when fresh" signal back to the caller, comparable to the `rebuild_required` signal used by the Jupiter path. V1 does not introduce it. Absence of retry guidance is a deliberate conservative default, not an oversight.

---

## Closing note for Part 1

The five sections above lock a single posture: **V1 lending is state-aware, fail-closed, risk-reducing-only, composed of local policy primary and RPC simulate backstop, with stale-data and failure producing Hard Block and no exceptions.** Every subsequent part of this spec — snapshot schema, rule vocabulary, metrics derivation, implementation roadmap, protocol choice — builds on this posture without re-opening it.

Subsequent parts, when authored, will extend (not contradict) the above. If any later part needs to contradict a decision made here, that contradiction must be explicit, argued, and gated behind a new prompt that specifically authorizes it.

---

# Part 2 — `LendingSnapshot` Schema and Freshness Metadata Semantics

**Status:** draft — Part 2 of N
**Scope of Part 2:** the abstract shape of the read model that Parts 3+ will evaluate policy over, and the freshness / staleness metadata that must be structurally present on it.
**Out of scope for Part 2 (explicit):** rule vocabulary (Part 3), metrics derivation (Part 4), implementation / fetch strategy (Part 5), protocol-specific account layout mapping (Part 6). Part 2 stays at the spec layer: it describes what the policy engine *sees*, not how it was produced.

## 6. Purpose of `LendingSnapshot`

`LendingSnapshot` is the **single read-model input** to the lending policy engine and to the metrics derivation layer. It is the materialization of Part 1's "Obligation / Reserve / Oracle" concept: a fully-populated, normalized, time-stamped view of one wallet's position on one lending protocol at one observation moment.

It has a narrow job:

- Be the **only** thing Parts 3+ are allowed to reason over when evaluating a proposed lending action.
- Carry **enough freshness metadata** that every policy decision can justify its staleness bound without an additional fetch or lookup.
- Refuse to exist in an incomplete state — see Part 1 §5.

Things `LendingSnapshot` is deliberately **not**:

- Not a cache. The snapshot is the artifact of one assembly; cache / refresh / TTL are Part 5 concerns.
- Not a derived-metrics carrier. LTV, health factor, collateral value, predicted post-action metrics — none of these live in the snapshot. Part 4 derives them *from* the snapshot.
- Not a protocol-specific account dump. It is a normalized abstract view; the raw chain bytes do not survive normalization. See §7.
- Not a plan or a proposal. It is a past-tense observation: "this is what we saw at that slot / timestamp." It commits to nothing about future or proposed actions.

## 7. Normalization Philosophy

`LendingSnapshot` describes **field semantics in Claw's policy language**, not a specific protocol's account layout. This is a deliberate choice:

- Part 1 forbade speculative multi-protocol abstractions (no generic `LendingProtocol` trait before a second protocol forces one).
- Part 1 equally forbade burying protocol-specific logic inside executors / policy paths.
- Part 6 will decide *which* protocol's accounts get mapped into this snapshot, and *how*.

Part 2 sits between those two boundaries. It commits only to the abstract field semantics: "an Obligation has a list of deposits, each with a mint, a base-unit amount, and a reference to a Reserve". It does **not** commit to:

- A Rust type layout (no `struct LendingSnapshot { ... }`).
- A trait hierarchy of protocols.
- A specific protocol's account field names or sizes.
- An enum of protocols.
- Any per-protocol match / dispatch.

The goal is that when Part 6 chooses a protocol, the mapping from that protocol's on-chain accounts to this schema is an implementation exercise, not an architectural redesign. Conversely, the schema must be expressive enough that the policy vocabulary of Part 3 can be expressed over it without needing extra fields.

If during Part 6 implementation a protocol demands a field this schema does not have, that is a spec event: the schema is revised, explicitly, in a new Part 2 revision. Snapshot fields are not added silently at implementation time.

## 8. Top-level snapshot shape (conceptual)

A `LendingSnapshot` conceptually consists of:

- **Session attribution** — which session, which wallet pubkey this snapshot is about. Snapshots are not session-free; there is no "global" snapshot.
- **Protocol attribution** — which lending protocol the view is for. A snapshot describes one wallet on one protocol; cross-protocol aggregation is not a V1 concept.
- **Obligation** — the wallet's single obligation on that protocol (see §9).
- **Reserves** — the set of Reserves referenced by that obligation (see §10). Reserves not referenced by the obligation are not in the snapshot.
- **Oracles** — the set of Oracles that price the reserves in this snapshot (see §11).
- **Freshness descriptor (snapshot-level)** — when the assembly completed and what chain head was observed (see §12).

An important structural note: the oracles, reserves, and obligation have their own *component-level* freshness descriptors (because each was fetched independently); the snapshot-level descriptor sits above them and records the moment the policy engine was handed a completed, consistent view. Both levels are required; see §12.

Cardinality:

- Exactly one Obligation per snapshot. A wallet with no obligation on the protocol yields *no snapshot* (not an empty one); the policy engine does not proceed without an obligation to evaluate.
- One or more Reserves. The set is determined by the obligation's references, not by the protocol's full reserve list.
- One Oracle **per priced asset** referenced by the included Reserves. If a Reserve uses a composite or multi-feed oracle, it is still represented at the snapshot level as the one authoritative Oracle entry the Reserve depends on; decomposition of multi-feed oracles is a Part 6 implementation concern.

## 9. Obligation — abstract field semantics

The Obligation component carries the user's position state. Field semantics:

- **Owner pubkey** — the wallet the obligation is attributed to. MUST match the session-bound external wallet (see Part 1 and A2-Execute's session-binding invariant). A snapshot whose Obligation owner does not match the session-bound wallet is a bug and a Hard Block.
- **Protocol-scoped obligation identifier** — the on-chain account pubkey that represents this obligation on the chosen protocol. Opaque at the policy layer; policy rules do not reason about it except to log it.
- **Deposits** — an ordered list of `(mint, base_unit_amount, reserve_reference, account_pubkey_of_deposit_position)`. Each entry represents one collateral position. "base_unit_amount" means smallest integer unit (lamports for SOL, 10^decimals for SPL tokens); the decimals live on the referenced Reserve, not duplicated here.
- **Borrows** — an ordered list mirroring Deposits. Each entry represents one debt position: `(mint, base_unit_amount, reserve_reference, accrued_interest_marker_if_protocol_exposes_one)`. Interest accrual is represented as a marker or timestamp; its interpretation belongs to Part 4 metrics.
- **Protocol-reported last-updated slot** — the slot at which the protocol itself believes this obligation was most recently modified. Distinct from our fetch freshness (§12): this is *the chain's* view of the obligation's age.
- **Lifecycle markers** — any state the protocol exposes that indicates the obligation is in-transition: mid-liquidation, closing, frozen, locked, deactivating. See §12 / §13; in V1, any such marker is a Hard Block condition.

What the Obligation does **not** carry:

- No derived `current_ltv`, `health_factor`, `total_collateral_value`, `total_debt_value`. Those are metrics.
- No predicted post-action state. Snapshots describe observed past state only.
- No protocol-specific bit-packed flags left unnormalized. If the protocol packs several booleans into one word, they are unpacked into the schema's explicit boolean fields.
- No rule evaluation results. A snapshot is not a verdict carrier.

## 10. Reserve — abstract field semantics

Reserves carry the protocol's declared risk parameters for each asset the obligation touches. The snapshot exposes reserves **verbatim at the protocol-declared level**, translated only into the schema's types. Policy rules and metrics interpret these; the snapshot does not.

Field semantics:

- **Protocol-scoped reserve identifier** — the on-chain account pubkey.
- **Mint** — the SPL mint pubkey this reserve is for.
- **Decimals** — the token's decimal count. The snapshot stores this explicitly so metrics do not need a separate mint-registry lookup.
- **Risk parameters (as declared)** — at least: maximum LTV, liquidation threshold, liquidation bonus / penalty, optimal utilization, reserve factor. These are reported using the protocol's own declarations (e.g., basis points, or ratios scaled to some denominator); the snapshot preserves the protocol's denomination and labels it, it does not convert units silently. Part 4 metrics derivation is responsible for consistent unit handling.
- **Interest model parameters (as declared)** — the reserve's base rate, slope(s), utilization optimum, and any other parameters the protocol exposes for computing borrow/supply APY. These are verbatim.
- **Current utilization and totals (as most-recently reported on chain)** — total deposits, total borrows, available liquidity, or whatever pair of figures the protocol exposes to derive utilization. These are verbatim.
- **Protocol-reported last-updated slot** — the reserve's own `last_updated_slot`-equivalent as reported by the protocol. Distinct from the snapshot's per-component fetch freshness.
- **Lifecycle markers** — paused, disabled, deprecating, supply-capped, borrow-capped. Any of these active MUST be surfaced to the policy layer; in V1, their interaction with specific actions is a Part 3 rule concern.

What the Reserve does **not** carry:

- No derived APY / effective rate / implied risk premium. Metrics.
- No "normalized" risk parameter where the protocol's native unit has been silently remapped into a Claw-internal unit. If units need to align, Part 4 does it explicitly with audit trail; the snapshot is faithful to source.
- No per-user fields. The Reserve is protocol-global state; the user-specific positions live in the Obligation's Deposits/Borrows.

### 10.1 Unit tags are a schema-level invariant

Every numeric quantity in the snapshot — in Reserve, in Obligation's deposits / borrows, in Oracle price fields — MUST carry an explicit **unit tag** that identifies which numeric domain the value is reported in.

The V1 snapshot necessarily mixes at least three numeric domains that coexist within a single component and across components. Evidence for the need: the read-only spike (§30.5) observed, in one obligation + its referenced reserves, all three domains live simultaneously — deposits in collateral-token units, borrows in wad-scaled fixed-point, reserve available-liquidity in underlying base units.

At minimum, a unit tag MUST be able to distinguish:

- **Underlying** — the SPL mint's native base unit (e.g., USDC micro-units at 6 decimals, SOL lamports at 9 decimals). Interpretable given the Reserve's `decimals` field.
- **Collateral token (c-token / derivative)** — the reserve's own receipt-token unit, which is not convertible to underlying without the reserve's current exchange rate. Reserve fields expressed here MUST be tagged as such so Part 4 metrics cannot silently treat them as underlying.
- **Wad / scaled fixed-point** — the protocol's scaled-integer representation (typically `10^18`-scaled `Decimal`). Used for per-position borrow amounts, per-reserve cumulative rates, and market-value accumulators. Wad values MUST NOT be confused with raw integer amounts.

Protocols MAY use other domains (basis points, percentage-scaled integers, provider-native fixed-point on oracle feeds). Each such domain gets its own tag; the minimum three above are universal across Solana lending protocols observed so far.

This is a **schema-level invariant**, not a downstream convention:

- Part 4 metrics are forbidden from inferring the unit from the field name alone — the tag is load-bearing.
- Part 5 types MUST make "untagged numeric" unrepresentable (e.g., distinct newtype wrappers per domain, or a `(value, unit)` pair where the unit is an enum).
- A snapshot that fails to tag a numeric field is not a valid snapshot; it fails the Part 2 §8 / §14 completeness check and the policy layer HardBlocks per Part 1 §5.

Why this belongs in §10 rather than a downstream part: the unit diversity is a property of the protocol's raw state, and §10 is where raw state first enters the schema. Recording the invariant at the point of first contact is cheaper than chasing every numeric field downstream.

## 11. Oracle — abstract field semantics

Oracles carry the price inputs to any metric that crosses asset boundaries (LTV, health factor, collateral-vs-debt valuation). The snapshot exposes them in the oracle's native format; translation into a policy-time unit is a Part 4 metrics concern.

### 11.1 The unit of oracle pricing is a feed *set* per priced asset

A priced asset is not priced by "the" oracle. It is priced by a **set of oracle feeds**, each with its own provider, its own on-chain account, its own publish cadence, and its own staleness. This is a Part 2 schema commitment, forced by real protocol behavior observed in the read-only spike (§30.5) where a single Solend reserve simultaneously carried a Pyth Lazer feed and a Switchboard On-Demand feed, and other reserves carried only one of the two with the unused slot filled by a sentinel pubkey.

Rules the schema therefore MUST encode:

- A priced asset MAY have one, several, or zero oracle feeds configured at snapshot-assembly time.
- Feeds for the same priced asset MAY come from different providers (e.g., Pyth + Switchboard simultaneously), MAY have different on-chain account layouts, and MAY have different staleness windows.
- A feed slot on a protocol reserve MAY be populated with a sentinel / null pubkey meaning "no feed configured for this slot." Sentinels MUST be recognized by the protocol adapter and excluded at assembly time; they MUST NOT surface as pseudo-feeds that policy then has to special-case. *Configured-but-unreachable* (fetch failure on a real pubkey) is a different failure mode from *unconfigured slot* (sentinel); both are fail-closed inputs under Part 1 §5, but they are distinguishable at the adapter boundary.

The schema represents oracle pricing as a two-level structure:

- **`OracleFeedSet` per priced asset** — the set of feeds the protocol has configured for this asset, sentinel slots removed. An empty set is a valid schema-representable state; it is a fail-closed input for any rule that needs oracle pricing on that asset.
- **`OracleFeed`** — each individual feed, carrying the per-feed fields listed in §11.2.

How policy combines a multi-feed set (disjunction, conjunction, weighted, preferred-provider) is a Part 3 / Part 4 decision; the snapshot only provides the set.

### 11.2 Per-feed field semantics

Each `OracleFeed` carries:

- **Oracle identifier** — the on-chain account pubkey, plus the oracle provider label (e.g., "Pyth Lazer", "Switchboard On-Demand") and the feed name (e.g., "SOL/USD"). The provider and feed label are surfaced so policy rules can reason about them (e.g., "allowed oracle providers" — deferred to Part 3).
- **Priced asset** — the mint this feed prices, and the quote asset (typically USD, but never assumed; the quote is explicit). Within a single `OracleFeedSet`, all feeds share the same priced asset and quote.
- **Price value** — the numeric price as published, in the oracle's native encoding (integer magnitude + exponent / decimals, or fixed-point — whatever the oracle exposes). The snapshot does not silently normalize to floating point or to a common base.
- **Confidence interval** — if the oracle publishes one. If the oracle does not publish a confidence interval, the field is absent (*not* zero — absent is semantically different).
- **Publish timestamp / slot** — when the oracle itself claims the price was produced. This is distinct from when Claw fetched the oracle (§12). The former is the oracle's staleness truth; the latter is Claw's view staleness.
- **Oracle status markers** — any published indicator that the feed is currently unreliable, paused, disabled, or in a degraded state. Surfaced verbatim.

What the Oracle does **not** carry:

- No "effective price" that combines Claw's view of the feed with risk adjustments. Metrics.
- No converted / normalized price in USD base units ready for LTV math. Metrics.
- No derived "oracle is too stale for this operation" boolean. That is a policy evaluation result, not a snapshot fact.
- No collapsing of a multi-feed `OracleFeedSet` into a single "winning" feed. Selection and combination are Part 3 / Part 4.

## 12. Freshness Metadata Semantics

Freshness in Part 1 was established as a **first-class input to policy decisions**, not a hidden implementation detail. Part 2 lands that principle on the schema: every component and every snapshot carries explicit freshness metadata that the policy layer cannot ignore or reconstruct.

### 12.1 Two levels of freshness

- **Component-level freshness** — each Obligation, Reserve, and Oracle instance in the snapshot carries its own freshness descriptor. This captures *when Claw fetched this particular account* and *what slot the chain was at, from the RPC's view, at that moment*. Components are fetched in some order, possibly in parallel; their freshness timestamps differ.
- **Snapshot-level freshness** — the top-level descriptor captures when the assembly of all components completed and what chain head was observed at assembly completion. This is the *composite* freshness the policy engine sees.

Both levels are required for every snapshot. A snapshot with only component-level freshness is incomplete; one with only snapshot-level freshness cannot meet §12.4's staleness-is-worst-component rule.

### 12.2 What a freshness descriptor contains

At each level, a freshness descriptor carries at minimum:

- **`fetched_at` wall-clock moment** — when Claw-side the fetch (or assembly, at snapshot level) completed. UTC, millisecond resolution or better.
- **`observed_slot`** — the chain slot the RPC reported as current at the moment of fetch, if available. Solana RPC exposes this on most read endpoints; if an endpoint does not, the field is absent and policy MUST treat the component as "slot-unknown".
- **`source_context`** — enough information to trace the freshness back to its origin: the RPC endpoint used, any cache layer involved, whether the fetch was synchronous or drawn from a subscription stream. This is audit / observability material; it exists on the descriptor so the policy layer's evaluation can be reconstructed after the fact.

An oracle's descriptor also carries:

- **Oracle's own `publish_timestamp` / `publish_slot`** — distinct from fetch time. This is the oracle's self-reported age; staleness rules (Part 1 §4.1) distinguish *oracle staleness* (publish-vs-now) from *state staleness* (fetch-vs-chain-head), and both lean on fields here.

### 12.3 The composite rule: worst-component dominates

When the policy layer asks "how stale is this snapshot?", the answer is determined by the **most stale component**, not an average, not the snapshot-level descriptor alone. If one oracle is 30 seconds old and another is 3 seconds old, the snapshot's effective freshness for policy purposes is 30 seconds.

This restates Part 1 §4.3 at schema level: partial staleness is total staleness. The schema carries both levels precisely so this rule can be evaluated deterministically rather than approximated.

### 12.4 Types MUST make "unknown freshness" unrepresentable

From Part 1 §4.2:

> A policy decision produced without freshness metadata is a bug. V1 implementations MUST make this representable in types (or equivalent compile-time enforcement) so that "we evaluated a rule without knowing the freshness of its inputs" cannot happen silently.

At Part 2's schema layer, this becomes:

- There is no representable `Obligation` / `Reserve` / `Oracle` that lacks a freshness descriptor.
- There is no representable `LendingSnapshot` that lacks a snapshot-level freshness descriptor.
- There is no representable "freshness descriptor is `Option`al" — it is required by construction.

How that is enforced (Rust types, `#[serde(deny_missing)]`, or any other mechanism) is Part 5. The schema's commitment is that no path into the policy layer can pass a snapshot whose freshness is absent or partial.

### 12.5 Freshness is immutable at snapshot level

Once a snapshot is assembled, its freshness descriptors are frozen. A policy run that takes longer than expected does NOT get to "re-freshen" the snapshot mid-evaluation; if more recent data is needed, a new snapshot is assembled (new descriptors, new component fetches). Policy evaluation is always against a fixed point-in-time view.

This matters for reasoning: if a rule says "reject if oracle is stale beyond 10 seconds," the rule evaluates against the descriptor at snapshot-assembly time, not at rule-evaluation time. Descriptors are what was observed, not what is true right now.

### 12.6 Interaction with Part 1's Hard Block defaults

Freshness descriptors exist so that staleness can be evaluated; they do not themselves define staleness thresholds (that's Part 3 / Part 6). But the fail-closed semantic from Part 1 §5 applies at the schema layer as well:

- A component whose freshness descriptor is missing a required field (`fetched_at`, or `publish_timestamp` on an oracle that should have one) **is not a valid component**. A snapshot containing it cannot be assembled; the snapshot does not exist; the policy layer Hard Blocks per §5.2.
- This is enforced by the schema, not by the policy rules. Rules never see a snapshot that failed to assemble.

## 13. What is deliberately NOT in `LendingSnapshot`

Exhaustive list of things the snapshot MUST NOT carry. These are captured here so later parts cannot silently slip them in.

- **Derived metrics**: `current_ltv`, `health_factor`, `borrow_headroom`, `collateral_value_usd`, `debt_value_usd`, `utilization_ratio`, any APY. These live in the `LendingMetrics` type Part 4 introduces, which is *computed from* a snapshot.
- **Predicted / post-action state**: no "what would LTV be if we borrowed X". That is either a Part 4 metric (if derivable locally) or a simulate result (Part 1 §1.4 backstop). Not in the snapshot.
- **Rule evaluation results**: no `rule_name`, `verdict`, `reason` fields. The snapshot is input, not output.
- **Raw on-chain account bytes** alongside normalized fields. If raw bytes need to be preserved for audit, they are carried in an *audit* artifact (not in scope here), not in the snapshot.
- **Session or agent metadata beyond the session-wallet attribution** (§8). The snapshot is about chain state, not about who proposed the action.
- **Proposed transaction details**: no input / output mints / amounts / slippage for the action being evaluated. The snapshot is independent of which action is currently under evaluation; Part 3's rules combine a snapshot with a proposed action input separately.
- **Future / speculative fields** reserved for V2+. If V2 needs a field, V2's spec adds it explicitly.

## 14. Invariants at the snapshot layer

Any valid `LendingSnapshot` satisfies:

- **Complete or absent.** A snapshot either contains all required components with valid freshness descriptors, or it does not exist. There is no partial-snapshot representation. Follows from Part 1 §5.
- **Deterministic assembly.** Given the same raw RPC responses with the same reported slots and timestamps, the same snapshot is produced. Assembly is a pure function of (raw reads + fetch timestamps). Implementation may add tie-breaking rules for concurrent fetch ordering but those rules are deterministic.
- **Self-contained evaluation.** Once a snapshot is handed to the policy layer, no further RPC is needed to evaluate any V1 rule. If Part 3's rules ever demand data outside the snapshot, the snapshot schema is extended (spec event), not patched at evaluation time.
- **Session-wallet consistent.** The Obligation's owner matches the snapshot's session-attributed wallet. A mismatch is a bug, Hard Block, not a rule rejection.
- **Temporally consistent within a tolerance.** All component freshness timestamps fall within a bounded window (defined in Part 3 / Part 6). Wildly divergent component freshness (e.g. obligation fetched 2s ago, oracle fetched 5 minutes ago) is a Hard Block at assembly time.
- **Frozen at assembly.** Once assembled, the snapshot is read-only for policy purposes. It is not mutated during rule evaluation.

## 15. Scope boundary between Part 2 and later parts

To preempt drift:

- **Part 2 (this):** what fields and freshness metadata the schema exposes, conceptually. No specific protocol layout. No Rust type. No code.
- **Part 3:** the vocabulary of rules that operate over a snapshot (`MaxCurrentLtv`, `MaxOracleStalenessMs`, etc.), their input/output contracts.
- **Part 4:** pure metrics derivation from a snapshot — how LTV, health factor, etc. are computed from the schema.
- **Part 5:** implementation roadmap — fetch strategy, cache, types, where modules live.
- **Part 6:** protocol choice and the mapping from chosen-protocol accounts to this schema.

If a reviewer of Part 2 finds themselves wanting to write "LTV is computed as …" or "we fetch the obligation via getProgramAccounts using …" or "`struct LendingSnapshot { ... }`", they are crossing a boundary. Those belong to Parts 4, 5, and 5, respectively.

---

# Part 3A — Rule Vocabulary and Scope Boundary

**Status:** draft — Part 3A of N.
**Scope of Part 3A:** what a "rule" is and is not in this policy language; which rules are V1-mandatory vs. deferred; how scope boundaries sit above rules; why the V1 rule set must stay minimal.
**Out of scope for Part 3A (explicit):** the pass / hard-block verdict contract and its fields; short-circuit / fail-fast evaluation order; evaluator API / inputs / dependency diagram; metrics derivation formulas; module layout; per-protocol specifics. These are Part 3B, Part 4, Part 5, Part 6.

Part 3A fixes **what the policy language says**. Part 3B will later fix **how an evaluation resolves**. Keeping these separate prevents the rule list from being warped by implementation decisions that have not been taken yet.

## 16. Rule Vocabulary Overview

A **rule** in this language is a named, declarative constraint over a pair of inputs:

- a `LendingSnapshot` (Part 2) — the observed on-chain state.
- a proposed action (which category it is, which mints, which amounts).

A rule **decides**; it does not **act**. Its only job is to say whether the proposed action is permitted by this rule given this snapshot. The evaluation layer (Part 3B) combines rule verdicts into a final outcome.

Specifically, a rule in V1:

- Does NOT fetch state. The snapshot is already assembled by the time a rule is evaluated. A rule that needs data not in the snapshot is a design error, not a fetch opportunity.
- Does NOT repair state. Missing or stale snapshot components are already handled at the snapshot layer per Part 1 §5 and Part 2 §12.6; by the time a rule sees a snapshot, the snapshot is complete by construction. Rules never have to check "is the snapshot valid?" — they only see valid snapshots.
- Does NOT fall back, retry, or warm caches. Those belong to the implementation layer (Part 5). Rules are pure functions over their inputs.
- Is NOT a scope boundary. A rule can reject an action, but a rule cannot express the concept "in V1 we do not do this action class at all." That concept lives above the rule layer — see §17.
- Is NOT a carrier for implementation details. No rule implicitly assumes a specific protocol, a specific account layout, a specific fetch cadence, or a specific metric formula. If a rule needs one of those, either the need is lifted to the snapshot schema (Part 2 revision) or the rule moves to future (§19).

**The purpose of Part 3A is not to enumerate every possible risk. It is to define the minimum rule language V1 actually needs.** Rules that sound reasonable but require machinery V1 is not shipping are explicitly deferred to §19 — that is not a backlog, it is a scope boundary.

## 17. Scope Boundary vs Rule Boundary

This section distinguishes two very different kinds of "block":

- **Rule boundary** — the action reached the rule evaluator and some rule rejected it. The proposed action is expressible in the policy language; there is some configuration of rules under which it could have been accepted; this particular session's rules rejected it.
- **Scope boundary** — the action never reaches the rule evaluator at all. The action is outside what V1 implements. No configuration of rules can change this; the question is not which rules are set but what software has been written.

Blurring these two kinds of block is the single most common way state-aware policy designs go wrong. Treating a scope boundary as a rule invites an operator to "flip a flag" to disable it — which is the opposite of a scope boundary. Treating a rule as a scope boundary invites the code to short-circuit the rule engine — which is the opposite of a rule.

### 17.1 V1 scope boundaries (not rules)

The following are **V1 scope boundaries**, not expressible as rules and not evaluated by the rule engine:

- **`Withdraw` actions are blocked.** Part 1 §3.2. There is no rule named "`NoWithdraw`" or "`AllowWithdraw=false`". V1 does not include the post-action machinery needed to evaluate `Withdraw`, so `Withdraw` does not reach rule evaluation. If the rule layer ever sees a `Withdraw` action, that is a bug in the gate above the rule layer, not a configuration choice.
- **`Borrow` actions are blocked.** Same reasoning as above.
- **Cross-protocol aggregation is not implemented.** There is no rule "across protocols"; the concept does not exist in V1.
- **Multi-obligation-per-wallet on the chosen protocol.** V1 assumes one obligation per wallet per protocol; alternative shapes are a scope boundary, not a rule.
- **Off-chain state inputs** (user reputation databases, external watchlists, time-of-day schedules, geolocation). V1 policy inputs are the snapshot and the proposed action. Anything else is scope.

### 17.2 V1 system invariants / hard preconditions (also not rules)

The following are enforced at layers *below* the rule evaluator. By the time a rule runs, these are already known to hold. Rules do not re-check them; rules do not depend on them being re-checkable:

- **Snapshot is complete and internally consistent.** Part 2 §14.
- **Obligation owner matches session-bound wallet.** Part 2 §9.
- **Every snapshot component has valid freshness metadata.** Part 2 §12.4.
- **Snapshot was assembled within a bounded divergence window across components.** Part 2 §14.
- **RPC fetch, parse, and deserialization succeeded** for all required components. Failure to do so yields a Hard Block *before* the rule evaluator runs — see Part 1 §5 and Part 2 §12.6.

Rules do not have conditions like "only apply if snapshot is complete." By construction, rules only see complete snapshots.

### 17.3 Implication for Part 3B and Part 5

Because scope boundaries and system invariants sit above the rule layer:

- **Part 3B's verdict contract** only needs to cover the rule-boundary case. Scope boundaries use a different, explicitly-outside-rules mechanism (Part 5 concern).
- **Part 5's implementation** must wire the rule layer downstream of scope-boundary and system-invariant checks. Rules are never the first line of defense; they are the fine-grained layer after the structural gates pass.
- **Operators configuring rules** can never, by configuration alone, enable a scope-boundary-blocked action. If V2+ wants to unblock `Withdraw`, that is a scope decision that requires code, spec revision, and new rule types — not a config edit.

## 18. V1 Mandatory Rules

The V1 mandatory set is deliberately small. Each rule below is justified, constrained in what it does, and tied back to what the V1 product actually ships (Deposit / Repay under state-aware fail-closed policy).

### 18.1 `RequireFreshState`

- **Semantic intent — V1 contract.** The rule rejects an action unless **every** non-oracle snapshot component (Obligation, Reserve, and the snapshot-level descriptor) passes BOTH of the following checks:
  1. **Claw-fetch freshness** — the component's `fetched_at` is within the configured maximum staleness window relative to the snapshot-level assembly time, per Part 2 §12.3's "worst-component dominates."
  2. **Protocol-native freshness** — where the protocol exposes a self-declared freshness marker on that component (e.g., a Solend-style `last_update.stale` bit, a protocol-declared `last_updated_slot` that lags chain head beyond a protocol-defined threshold), the marker MUST read "fresh." A protocol-native `stale = true` is treated as "not fresh," regardless of how recently Claw fetched the account.

  The rule rejects on the first component that fails either check, consistent with Part 3B §27's fail-fast ordering.

- **Inputs it depends on.** The snapshot's per-component and snapshot-level freshness descriptors (Part 2 §12.2), AND the protocol-declared freshness markers surfaced on Obligation (§9, `protocol_last_updated_slot` and any `stale`-equivalent lifecycle marker) and Reserve (§10, `protocol_last_updated_slot` and lifecycle markers). No proposed-action fields are required.

- **Relationship with §18.2 `MaxOracleStalenessMs`.** The two rules are **disjoint by component** and together define "fresh enough to evaluate":
  - **Oracle publish-time age** → §18.2 only.
  - **Obligation fetch age + obligation protocol-native stale marker** → §18.1 only.
  - **Reserve fetch age + reserve protocol-native stale marker** → §18.1 only.
  - **Oracle fetch age** is NOT in §18.1's scope; the oracle's Claw-fetch freshness is covered by Part 2 §12.3's worst-component rule composition at the snapshot level, and any policy beyond publish-age is future rule scope (§19).

  Neither rule evaluates the other's component set. Both are mandatory; the action passes the freshness precondition only when both rules pass.

- **Handling of protocol-native `stale = true`.** Some protocols return `stale = true` as the **default** state on a bare `getAccountInfo` read, because their on-chain account is only refreshed by an explicit protocol instruction (e.g., Solend's `RefreshObligation` / `RefreshReserve`). This was observed directly in the read-only spike (§30.5): a real Solend obligation and all its referenced reserves returned `stale = true` on a naïve read, even when the account bytes were freshly fetched. V1's fail-closed stance takes the protocol at its word: a protocol-native `stale = true` bit is a HardBlock, regardless of Claw's fetch recency.

  The work of issuing a protocol-native refresh before assembling the snapshot — whether by bundling a `RefreshObligation` instruction, waiting for a refresh cycle, or marking the snapshot un-assemblable — belongs to the **snapshot assembly layer** (Part 5 / Part 6 responsibility), not to the rule engine. §18.1 evaluates the bit it was given; it does not attempt repair.

- **Why it belongs in V1.** Part 1 §4 made freshness a first-class policy input. Part 2 §12.4 made freshness structurally unavoidable. Without §18.1's concrete contract, "freshness" remains abstract — the read-only spike demonstrated that an abstract rule statement cannot decide V1's behavior against Solend's real `stale = true` signal. This rule is the user-visible enforcement knob that turns the freshness machinery into actual policy.

- **What it does NOT do.**
  - Does not validate oracle publish-time staleness — that is §18.2.
  - Does not apply per-component thresholds differently — V1 uses a single threshold applied per worst-component; per-component thresholds are a future refinement (§19).
  - Does not repair stale snapshots, trigger protocol refresh instructions, or retry — those are snapshot-assembly-layer concerns, and retry is not a V1 policy decision (Part 1 §5.5).
  - Does not interpret protocol-native stale markers as transient or retry-able signals. Fail-closed means HardBlock; the caller's responsibility is to assemble a fresh snapshot before retrying the evaluation.

- **Mandatory by default in V1:** yes. The rule must be evaluated; both the Claw-fetch threshold and the protocol-native-marker check are structural. The threshold value is configurable; whether the checks run at all is not.

### 18.2 `MaxOracleStalenessMs`

- **Semantic intent:** rejects an action if any oracle in the snapshot has a `publish_timestamp` older than the configured bound. This is the oracle's own view of its age, distinct from Claw's fetch freshness (Part 2 §11 and §12.2).
- **Inputs it depends on:** each Oracle component's `publish_timestamp` (and, where applicable, status markers). The proposed action is not required as input for this rule's evaluation; it is a snapshot-only precondition.
- **Why it belongs in V1:** oracle staleness is a distinct risk from Claw's fetch staleness. A freshly fetched oracle that has not republished for minutes is dangerous even though Claw's fetch is seconds old. Part 1 §4.1 made oracle staleness a distinct concept; this rule is where that distinction becomes enforceable.
- **What it does NOT do:** does not cross oracles and attempt any weighting scheme. Each oracle is independently bounded. Does not attempt to reason about missing oracles (a snapshot without its required oracles does not exist, per §17.2). Does not convert staleness into a metric; it is a threshold rule against a published timestamp.
- **Mandatory by default in V1:** yes.

### 18.3 `AllowedLendingProtocols`

- **Semantic intent:** rejects an action whose protocol identity is not in an operator-configured allowlist. For V1 with a single supported protocol, the allowlist is effectively `{ that_protocol }`; the rule still runs, and an action tagged with any other protocol identity is rejected.
- **Inputs it depends on:** the snapshot's protocol attribution (Part 2 §8) and the proposed action's protocol tag. No other snapshot fields are required.
- **Why it belongs in V1:** establishes the pattern for future multi-protocol expansion without committing to multi-protocol abstractions today. Also gives operators a clean way to express "this session is allowed to interact with this specific lending protocol only," consistent with how existing Claw policies express allowlists for mints or destinations.
- **What it does NOT do:** does not make multi-protocol policy decisions (reject based on protocol combinations, prefer one protocol over another). Each action is evaluated against one protocol; cross-protocol reasoning is §17.1 scope.
- **Mandatory by default in V1:** yes.

### 18.4 `AllowedMints`

- **Semantic intent:** rejects an action whose input / target mint is not on an operator-configured allowlist. Applies across `Deposit` (collateral mint being supplied) and `Repay` (debt mint being reduced).
- **Inputs it depends on:** the proposed action's mint(s) and, for cross-referencing, the Obligation's deposit / borrow mints in the snapshot so the rule can validate the mint exists as a legitimate target for the proposed action (e.g., a `Repay` against a mint the obligation has no debt in is rejected by this rule's cross-reference).
- **Why it belongs in V1:** directly analogous to the transfer-level `MintNotInAllowlist` rule that already exists in Claw. Lending does not weaken the operator's ability to express mint-level policy; this rule carries that expression into the lending path.
- **What it does NOT do:** does not distinguish between "allowed as collateral" vs. "allowed as borrow source." That split is useful once `Borrow` is unblocked (see §19.3) and is deferred until then. In V1, there is a single allowlist applied to every lending action's mints.
- **Mandatory by default in V1:** yes.

### 18.5 `MaxActionInputAmount`

- **Semantic intent:** rejects an action whose `input_amount` (in the input token's smallest base unit) exceeds a configured cap. For `Deposit`, caps the collateral amount being supplied; for `Repay`, caps the debt amount being repaid per action.
- **Inputs it depends on:** the proposed action's input amount and the Reserve's `decimals` (from the snapshot) for denomination clarity. No oracle, no metric derivation.
- **Why it belongs in V1:** value caps are the most basic unit of "don't spend more than X per action." A native-denominated cap (lamports for SOL, base units for SPL) is derivable from the snapshot without any metric machinery. This keeps V1 rule evaluation fully local and decoupled from Part 4 metrics.
- **What it does NOT do:** does not express a USD-denominated cap. A USD cap would require oracle pricing and metric derivation, which couples V1 rule evaluation to Part 4 formulas and to oracle-freshness-dependent valuation. V1 instead takes the conservative stance of native-unit caps; USD-denominated caps are listed in §19.5 as a future rule class.
- **Mandatory by default in V1:** yes.

### 18.6 Note on `RiskReducingActionOnly`

`RiskReducingActionOnly` was listed as a V1 mandatory candidate. Part 3A does **not** carry it as a rule.

Part 1 §3.2 stated that `Withdraw` and `Borrow` are blocked by **scope boundary**, not by **rule**. §17.1 above restates that position at the rule-vocabulary layer: the Withdraw/Borrow block is not expressible as a rule, not configurable via a rule, and not evaluated by the rule engine.

If `RiskReducingActionOnly` were a rule:

- An operator could disable it by flipping a config flag, which contradicts Part 1 §3.2's "no policy rule in V1 can override this block."
- It would appear in rule evaluation output with a verdict like "rejected by `RiskReducingActionOnly`," framing the block as a rules-level decision that could be relaxed.
- Part 3B's verdict contract would need to model this case, which would erode the scope-boundary vs. rule-boundary distinction §17 fixes.

Part 3A's decision: the `RiskReducingActionOnly` constraint is **already enforced as a V1 scope boundary** (Part 1 §3). It is not added as a rule in §18. This is the intended relationship between the two concepts, and the reason §17 exists before §18.

## 19. Future / Not-Yet-Enabled Rule Classes

The rules below are coherent and useful, but none are V1 mandatory. Each is listed with the specific prerequisite it waits on. None may be implemented in V1 even if the prerequisite looks close — adding them ahead of time imports machinery V1 has not yet stabilised.

### 19.1 `MaxCurrentLtv`

- **Semantic intent:** rejects an action if the wallet's current loan-to-value ratio (before the proposed action executes) exceeds a configured threshold.
- **Why it is not in V1:** requires LTV to be derived from the snapshot, which is a Part 4 metric. Part 4 formulas have not been written. More importantly, for V1's allowed actions (Deposit, Repay), current LTV is a risk-reducing-action input that provides no safety boost — Deposit can only lower current LTV; Repay can only lower current LTV. A rule that caps current LTV has no risk-blocking effect on these actions.
- **Prerequisite for enabling:** Part 4's LTV metric defined, tested, cross-validated against the chosen protocol's own LTV calculation. Additionally, `Withdraw` must be unblocked (since `Withdraw` can raise current LTV), otherwise the rule still has no V1 effect.

### 19.2 `MaxPredictedLtv`

- **Semantic intent:** rejects an action if the LTV that would result from the proposed action's execution exceeds a configured threshold.
- **Why it is not in V1:** requires post-action metric derivation, which requires either (a) local predicted-metric formulas or (b) RPC simulate result parsing, both of which are Part 4 / Part 5. Also only meaningful once `Borrow` or `Withdraw` is in scope — for `Deposit` / `Repay`, predicted LTV is always less than or equal to current LTV, so the rule cannot be violated.
- **Prerequisite for enabling:** post-action metric derivation (Part 4 or simulate parsing), AND at least one risk-increasing action type in scope.

### 19.3 `MinHealthFactor`

- **Semantic intent:** rejects an action if the wallet's health factor is below a configured minimum (either current or predicted, depending on rule variant).
- **Why it is not in V1:** same category as `MaxCurrentLtv` / `MaxPredictedLtv` — depends on Part 4 metrics. Health factor and LTV are different renderings of the same underlying math; treating them as two rule classes is reasonable so operators can express policy in the language that matches their mental model, but both wait on the same prerequisite.
- **Prerequisite for enabling:** Part 4 health-factor metric defined and cross-validated.

### 19.4 `AllowedCollateralMints` / `AllowedBorrowMints`

- **Semantic intent:** refines §18.4's `AllowedMints` by splitting collateral-side and borrow-side allowlists. Lets an operator say "wallet may deposit USDC and SOL as collateral, but may only borrow USDC."
- **Why it is not in V1:** `AllowedBorrowMints` has no V1 effect because `Borrow` is scope-boundary blocked. `AllowedCollateralMints` is meaningful for `Deposit` in V1 but is a strict subset of `AllowedMints`'s expressive power — adding it now doubles the operator's configuration surface with no V1-unique benefit. Wait until `Borrow` is unblocked in V2+ and the asymmetry between collateral and borrow mints becomes real.
- **Prerequisite for enabling:** `Borrow` in scope, which requires most of §19.1 / §19.2 / §19.3 to be stable first.

### 19.5 `MaxBorrowHeadroomUsage`

- **Semantic intent:** rejects an action that would consume more than a configured fraction of the wallet's remaining borrow headroom.
- **Why it is not in V1:** only meaningful for `Borrow` actions; scope-boundary blocked in V1.
- **Prerequisite for enabling:** `Borrow` in scope + borrow-headroom metric defined in Part 4.

### 19.6 `MaxActionValueUsd`

- **Semantic intent:** rejects an action whose input amount, priced in USD at current oracle, exceeds a configured USD cap. The natural-language-feel version of §18.5.
- **Why it is not in V1:** requires conversion from native amount to USD, which requires oracle price + metric derivation. That coupling means this rule would depend on:
  1. Part 4 USD valuation formula being defined and audited.
  2. The oracle feeding the input mint being fresh (per §18.2) AND being able to fail closed cleanly if not.
  3. A stable policy for which quote currency is "USD" (different oracles publish against slightly different USD references).
  All three are reasonable future work. None is V1. §18.5 gives operators a conservative substitute that ships without this coupling.
- **Prerequisite for enabling:** Part 4 USD valuation formula + oracle-quote-asset policy + audit of the USD derivation for the chosen protocol's supported mints.

## 20. Rule Set Minimalism

Rule vocabulary should be a function of V1 product scope, not a function of what rules "might eventually make sense." This principle is load-bearing for Parts 4 and 5 — if the V1 rule set is inflated now, Part 4 metrics must grow to support the extras, and Part 5 must implement, test, and cross-validate every extension before V1 can ship.

Three design rules follow:

### 20.1 The V1 rule set is driven by the V1 action set

V1 implements `Deposit` and `Repay` (Part 1 §3). Every V1 mandatory rule must have a meaningful effect on at least one of those actions. Rules that only matter for `Borrow` or `Withdraw` are deferred until those actions are in scope — they are not "laid in early" as preparation. A rule with no V1-observable effect is not V1 rule vocabulary; it is V2 design speculation.

### 20.2 Every V1 mandatory rule must be answerable from the Part 2 snapshot alone

No V1 rule introduces a snapshot field that Part 2 does not already expose. If a candidate rule requires data outside the snapshot, the candidate is deferred; the snapshot schema is not silently expanded in Part 3A to fit a rule. Schema changes are Part 2 revisions, explicitly.

(`MaxCurrentLtv` and family are deferred largely for this reason — they require `LendingMetrics` which Part 4 has not defined. Adding them to V1 would force Part 4 to be co-drafted, which is exactly the cross-coupling Part 3A is designed to avoid.)

### 20.3 Part 4 must serve Part 3A's V1 rules, not the other way around

When Part 4 is drafted, it should define only the metrics required to evaluate the V1 mandatory rules in §18. Metrics that would support §19 future rules are designed later when those rules are promoted. This keeps Part 4 tightly scoped and delays the need for cross-protocol LTV / health-factor reconciliation until a second protocol actually forces it (consistent with Part 2 §7).

A rule of thumb for future reviewers: if Part 4 finds itself defining LTV math, a spec event has occurred — either a §19 rule has been promoted to V1, or Part 4 is expanding its scope beyond what V1 rules require. Both cases deserve explicit argument in a new prompt.

## 21. Scope boundary between Part 3A and Part 3B / Parts 4–6

- **Part 3A (this):** rule vocabulary — which rules exist, which are mandatory, which are deferred. Each rule has a semantic intent, input dependencies, and a scope for what it does NOT do. No verdict contract, no evaluation semantics, no evaluator API.
- **Part 3B (next):** verdict contract (pass / hard-block shape), short-circuit / fail-fast ordering within the rule engine, rule-evaluator inputs, evaluator composition with the system-invariant and scope-boundary gates that sit above it. Part 3B builds on Part 3A's vocabulary without re-opening it.
- **Part 4:** metrics derivation for the V1 rule set (and only that set). Defined only after Part 3A's rule list is fixed.
- **Part 5:** implementation roadmap, including fetch strategy, caching, module placement, and where rule vocabulary types live.
- **Part 6:** protocol choice and per-protocol mapping from chosen accounts into the Part 2 schema.

If a reviewer of Part 3A finds themselves wanting to specify what `Pass` / `HardBlock` look like as data, or whether `RequireFreshState` evaluates before `MaxOracleStalenessMs`, or how a rule expresses its `Config` — they are in Part 3B territory. If they find themselves writing "LTV = collateral_value / debt_value" — they are in Part 4 territory.

---

# Part 3B — Verdict Contract and Evaluation Semantics

**Status:** draft — Part 3B of N.
**Scope of Part 3B:** the shape of a policy verdict, the three-layer gate model (system invariant, scope boundary, rule evaluation), fail-fast evaluation semantics, verdict reason categories, and the evaluator's input / dependency boundary.
**Out of scope for Part 3B (explicit):** concrete Rust types / enums / traits / APIs; metrics formulas (Part 4); implementation roadmap / module placement (Part 5); protocol choice and per-protocol mappings (Part 6); expanded reason taxonomy beyond the V1-required categories; retry / rebuild / refresh semantics at the verdict layer.

Part 3A defined **what the policy language says**. Part 3B defines **what a policy evaluation produces**, and in what order the layers that produce it run.

## 22. Verdict Contract Overview

Every policy evaluation under the state-aware lending path produces a single, explicit **verdict** that is consumed by the control plane (audit layer, event bus, caller response). The verdict is not a side effect of a boolean `true / false`; it is the first-class artifact the decision exists to produce.

V1 does not model the verdict as a simple pass / fail flag. Reasons:

- **Audit needs the "why", not just the "what."** A rejection that carries "which layer rejected and for what reason" is diagnosable after the fact; a rejection that carries only "blocked" is not.
- **Callers need categorical information, not free-form text.** The agent runtime, the HTTP response shape, and any future frontend must be able to branch on the reason category without pattern-matching a human-readable string.
- **The three evaluation layers defined in §24 – §26 produce categorically distinct rejection causes.** Those distinctions are lost in a boolean; Part 3A §17's entire point is that the kinds of block are not fungible and must stay distinguishable at the decision surface.

At the same time, the verdict contract is deliberately **not** an exhaustive error taxonomy:

- The verdict is a **control-plane decision surface**, not an error taxonomy.
- Audit, telemetry, and log output may carry finer granularity than the verdict does.
- V1 locks a minimal verdict shape; later parts may extend reason categories, but only behind explicit spec revision.

The goal of Part 3B is **semantic convergence**, not implementation design. If a reviewer is tempted to write `enum PolicyVerdict { ... }`, they are in Part 5 territory. Part 3B describes what the future representation MUST carry, not how it is spelled.

## 23. Pass and HardBlock

V1 has exactly two verdict variants:

- **`Pass`** — the proposed action is permitted to proceed past the policy layer. All three gates (§24 – §26) positively accepted it.
- **`HardBlock(reason)`** — the proposed action is rejected. The rejection is terminal for this action and this evaluation.

### 23.1 `HardBlock` is terminal

`HardBlock` means the action does not proceed. It is not a warning, not a suggestion, not a deferred outcome. Specifically in V1:

- **No soft-fail variant.** An evaluation either clears every gate or it rejects. There is no intermediate "approved conditionally" verdict.
- **No allow-with-warning.** If something is risky enough to emit a warning, V1's fail-closed posture says it is risky enough to reject. Warnings are a later design decision, not a V1 concern.
- **No partial pass.** An action is not partially approved over part of its scope. Verdict applies to the action as a whole.
- **No `Deferred` / `Retry` / `RebuildRequired` verdict.** The rebuild-required signal used in the Jupiter external-wallet completion path (`WalletSignatureOutcome.rebuild_required`) operates at a different layer — outside the state-aware policy verdict. V1 lending policy does not produce retry guidance. A blocked action's caller decides whether to resubmit later; the verdict makes no promise about when or whether to retry.

### 23.2 Every fail-closed outcome maps to `HardBlock`

Part 1 §5 established that V1 has zero fail-open paths. At the verdict layer:

- Every system invariant failure → `HardBlock`.
- Every scope boundary rejection → `HardBlock`.
- Every rule rejection → `HardBlock`.
- Every evaluator-internal error (if any is reachable given V1's fail-fast design — see §27) → `HardBlock`.

The evaluator never returns `Pass` on uncertainty. It returns `Pass` only when all three gates have positively accepted. This is the fail-closed default, restated at the decision surface.

### 23.3 Source layer stays distinguishable inside the `reason`

Although `HardBlock` is a single verdict variant, the `reason` it carries preserves **which layer produced the block**. A `HardBlock` originating from the system invariant gate, the scope boundary gate, and rule evaluation are three distinguishable cases — not mergeable. §28 defines this.

This matters because the appropriate downstream behaviour differs by layer:

- A `SystemInvariantFailed` tells the operator "this evaluation never reached the policy layer because state was unsound." No policy reconfiguration changes this.
- A `ScopeBoundary` tells the operator "this action is not evaluated by V1 at all." No rule configuration changes this.
- A `RuleRejected` tells the operator "a configurable rule blocked this." Changing rule config MIGHT change this outcome.

Collapsing these into a single opaque "blocked" erases that distinction and would let downstream code treat all three the same way, which is wrong.

### 23.4 Why not more variants

A tempting V1 design adds `SoftWarn`, `Review`, `AwaitingManualApproval`, or `RetryAfter(duration)`. V1 rejects all of them for the same reasons:

- Each extra variant is a decision surface downstream code (agent, audit, operator UI) must handle correctly. Adding surface before needing it invites partial-handling bugs.
- Each variant is a policy commitment — V1 does not ship the underlying machinery (there is no lending human-approval queue, there is no retry-scheduling machinery).
- The minimal `Pass` / `HardBlock` shape is sufficient for V1's action set (Deposit / Repay). Adding variants later is a spec event a future prompt can authorize.

## 24. System Invariant Gate

The System Invariant Gate enforces the **structural preconditions** established in Part 1 §5 and Part 2 §14. It answers a single question: *is the state on which this decision would be made actually sound?*

### 24.1 What this gate checks

A non-exhaustive list of checks this gate owns:

- **Snapshot existence.** A `LendingSnapshot` for this session and this target protocol has been successfully assembled (Part 2 §8, §14).
- **Snapshot completeness.** Every required component is present — Obligation, the Reserves referenced by that Obligation, the Oracles feeding those Reserves (Part 2 §8, §10, §11).
- **Owner correspondence.** The Obligation's owner pubkey matches the session-bound external wallet (Part 2 §9). Session-wallet binding is the authoritative source of signer identity (Part 1 / A2-Execute invariant); a snapshot whose Obligation owner does not match is a structural failure, not a policy rule's concern.
- **Freshness metadata completeness.** Every component carries a well-formed freshness descriptor; the snapshot-level freshness descriptor is present (Part 2 §12.2, §12.4).
- **Component temporal consistency.** All component freshness timestamps fall within the bounded divergence window (Part 2 §14).
- **Assembly integrity.** RPC fetches, account parses, and snapshot assembly all succeeded without silent degradation (Part 1 §5.2).
- **Immutability.** The snapshot handed to evaluation is the frozen artifact from assembly; it has not been mutated since (Part 2 §12.5).

This list is illustrative, not exhaustive. Anything Part 1 §5 or Part 2 §14 declares as a structural precondition lives here.

### 24.2 Properties

- **Not a rule.** No operator configuration can relax any of these checks. They are structural invariants, not policy choices.
- **Not a rule engine concern.** The rule engine never runs if this gate fails. Rules must not defensively re-check these conditions; the evaluator layering contract (§27) says the rule engine can assume they all hold by the time it runs.
- **Precedes everything.** If this gate rejects, neither the scope boundary gate (§25) nor rule evaluation (§26) runs. The subsequent layers are entitled to assume all their preconditions hold.
- **Fail-closed by construction.** Every uncertainty at this gate produces a `HardBlock`. This is Part 1 §5 restated at the evaluator layer.

### 24.3 Classification in the verdict

Failures here are reported under the `SystemInvariantFailed` reason category (§28.1). They are NOT classified as `RuleRejected`. Misclassifying a structural failure as a rule rejection would suggest the operator could "turn the rule off," which is precisely what this gate exists to prevent.

## 25. Scope Boundary Gate

The Scope Boundary Gate enforces the **V1 product scope** established in Part 1 §3 and restated in Part 3A §17. It answers a single question: *is the action class something V1 evaluates at all?*

### 25.1 What this gate checks

- **Action class membership.** The action is `Deposit` or `Repay` (V1 allowlist per Part 1 §3.1).
- **Action class exclusion.** The action is not `Withdraw` or `Borrow` (V1 scope boundary per Part 1 §3.2).
- **Cross-protocol aggregation is rejected.** An action that implies aggregation across multiple lending protocols is outside V1 (Part 3A §17.1).
- **Multi-obligation ambiguity is rejected.** V1 assumes one obligation per wallet per protocol; alternative shapes are outside V1 scope.
- **Non-on-chain policy inputs are rejected.** Actions whose permissibility would depend on off-chain state (watchlists, geolocation, time-of-day schedules) are outside V1 policy-language scope (Part 3A §17.1).

### 25.2 Properties

- **This is not rule evaluation.** An operator cannot enable `Withdraw` or `Borrow` by adjusting rule configuration. `RiskReducingActionOnly` is not and will not be an ordinary rule (Part 3A §18.6).
- **This is not a system invariant.** This gate assumes a valid snapshot already exists; it does not check snapshot integrity. It asks only about the action's kind and scope, not about state correctness.
- **Represents a product boundary.** The V1 software does not implement the post-action machinery required to judge `Withdraw` / `Borrow`. That is a scope boundary — a software fact, not a policy decision.
- **Follows the system invariant gate.** It does not make sense to ask "is this action in V1 scope?" of an unassembled or invalid snapshot.

### 25.3 Classification in the verdict

Failures here are reported under the `ScopeBoundary` reason category (§28.1). They are NOT `RuleRejected` (they are not rule verdicts) and NOT `SystemInvariantFailed` (the structural state is fine; the action itself is out of scope).

## 26. Rule Evaluation

Rule Evaluation runs only after both the System Invariant Gate and the Scope Boundary Gate have positively accepted.

### 26.1 Scope of this layer

- Ordinary policy rules from Part 3A §18 (and any future rules that may be added via explicit spec revision) run here.
- Rules are evaluated against `(snapshot, proposed_action)` as defined in §29.
- Rule configuration (thresholds, allowlists) is supplied as part of the rule's own configured state; it is operator-facing.

### 26.2 Preconditions the rule engine may assume

By contract, the rule engine never sees:

- A missing, partial, or structurally inconsistent snapshot. The System Invariant Gate (§24) has already rejected those.
- An action class outside V1 scope. The Scope Boundary Gate (§25) has already rejected those — specifically, no `Withdraw` or `Borrow` action ever reaches rule evaluation.
- A snapshot whose Obligation owner does not match the session-bound wallet — that is a §24 failure.
- A snapshot whose components' freshness is missing — that is a §24 failure.

Rules MUST NOT defensively re-check any of the above. Doing so is a contract violation and must be caught at code review; the evaluator is not a place to re-litigate the preceding gates' decisions.

### 26.3 Scope-of-responsibility boundary

The rule evaluator:

- Does **not** decide whether an action class is supported. That is §25's job.
- Does **not** decide whether a snapshot is trustworthy. That is §24's job.
- Consumes only an already-validated, in-scope `(snapshot, proposed_action)` pair and runs the configured rules over it.

### 26.4 Classification in the verdict

A rule-driven rejection is reported under the `RuleRejected` reason category (§28.1), naming the specific rule that rejected and carrying enough context to describe the failing condition (which threshold was breached, which mint was not in the allowlist, etc.).

## 27. Evaluation Order and Fail-Fast Semantics

V1 evaluation is **fail-fast / short-circuit**. The first `HardBlock` produced by any layer terminates the evaluation immediately.

### 27.1 Fixed ordering

1. **System Invariant Gate** (§24). If it rejects, return `HardBlock(SystemInvariantFailed(...))`. The scope boundary gate and rule evaluation do NOT run.
2. **Scope Boundary Gate** (§25). If it rejects, return `HardBlock(ScopeBoundary(...))`. Rule evaluation does NOT run.
3. **Rule Evaluation** (§26). Rules run in a deterministic order (the exact order is not fixed by Part 3B; it is a Part 5 / Part 6 concern). The first rule that rejects returns `HardBlock(RuleRejected { rule, context })`. Subsequent rules do NOT run.

If all three layers positively accept, return `Pass`.

### 27.2 The ordering is an invariant, not a convenience

The three-layer ordering is a **policy invariant**. Any implementation that short-circuits the ordering (runs rules before invariants, folds scope into rules, or uses rules to cover for invariant gaps) violates the semantics Parts 1–3A established.

Concretely:

- **An operator cannot disable the System Invariant Gate.** There is no rule named "allow stale snapshots." Stale is `HardBlock` by Part 1 §4.
- **An operator cannot enable `Withdraw` via rule configuration.** Withdraw is a scope boundary (Part 1 §3.2), not a rule (Part 3A §18.6).
- **A rule cannot repair a broken snapshot.** Rules see valid snapshots or they do not run. There is no fallback rule that "kicks in if the snapshot is incomplete."

These are the guarantees that make the three-layer model load-bearing. If any slips at implementation time, the safety argument built in Parts 1–3A does not hold.

### 27.3 Why V1 is fail-fast (not collect-all)

- **Safety first.** Once the evaluator has one sufficient reason to reject, the decision is final. Continuing evaluation to "collect more failure context" spends effort on an already-settled decision.
- **Control-plane clarity.** Audit and caller response carry one reason. This is easier to act on (retry after condition X changes) than a set of reasons, most of which may be downstream consequences of the first.
- **Evaluator simplicity.** Fail-fast evaluators are easier to reason about, simpler to test, simpler to prove correct. Collect-all evaluators invite edge cases around rule interaction and partial-state diagnostics.
- **Diagnostic completeness is not a policy-layer concern.** If operators want full diagnostics for a rejected action, audit logs carry what they need (§28.3); the verdict does not carry a bag of every reason that would have rejected.
- **Fail-closed interaction.** In a fail-closed world, the first rejection is decisive by design. Collect-all implies some rejections are "less authoritative" than others, which contradicts fail-closed posture.
- **Architecture complexity avoidance.** Committing to collect-all semantics forces every gate layer to return a list rather than a verdict, and forces the evaluator to handle partial-state diagnostics across gates. That is significant extra surface for a speculative benefit. V1 keeps the shape minimal.

### 27.4 Side effects and reproducibility

- **No RPC calls during evaluation.** All state is in the snapshot.
- **No cache refresh during evaluation.** The snapshot is immutable per Part 2 §12.5.
- **No state mutation during evaluation.**
- **Audit event emission is post-evaluation.** Rules do not directly emit audit events; the evaluator's output drives audit downstream (Part 5 concern).
- **Determinism.** Given the same `(snapshot, proposed_action)` pair, evaluation yields the same verdict, every time. Two evaluators with the same inputs agree.

## 28. Reason Categories

A `HardBlock` carries a **reason** that is categorical, not free-form. V1 locks three top-level categories, mirroring the three evaluation layers.

### 28.1 The three V1 categories

- **`SystemInvariantFailed`** — produced by §24. The snapshot / structural preconditions were not met.
- **`ScopeBoundary`** — produced by §25. The action class is not within V1's product scope.
- **`RuleRejected`** — produced by §26. A configurable policy rule rejected the action.

These three categories are **the entire V1 reason space**. Nothing else is a public reason category in V1.

### 28.2 What each category must carry

The verdict reason must be machine-processable — callers branch on the category without pattern-matching prose. Each category carries at minimum:

- **`SystemInvariantFailed`**: a subcategory identifying which invariant failed (for example: snapshot-incomplete, owner-mismatch, freshness-missing, RPC-failure). Part 3B does not fix the exact subcategory taxonomy; it requires that one exists and is categorical.
- **`ScopeBoundary`**: the action class that was rejected (e.g., `Withdraw`, `Borrow`), and a rationale tag linking back to a V1 scope boundary (e.g., "V1 risk-reducing-only").
- **`RuleRejected`**: the rule identifier that rejected, and rule-specific context describing the failing condition (which threshold, which mint, etc.).

### 28.3 Verdict reason is a public contract; audit carries more

The verdict reason is the caller-facing output. Audit and telemetry can and should carry additional context that does NOT belong in the verdict:

- Fine-grained failure origin (file / line / internal error code).
- Correlation IDs, timing data, retry statistics.
- Multi-line diagnostic dumps useful for post-mortem analysis.
- Developer-facing identifiers of specific code paths exercised.

None of these are verdict reason fields. The verdict reason is the minimum structured information the control plane needs to make a routing or response decision. Audit is where rich context lives. Keeping them separate prevents the verdict reason from growing into an error-taxonomy god-object that ties V1's public contract to every internal detail.

### 28.4 Subcategory enumeration is deferred

Part 3B does not enumerate every possible `SystemInvariantFailed` subcategory or every `ScopeBoundary` rationale tag. What it requires:

- Subcategories must be categorical (machine-processable labels), not free text.
- The set of subcategories is bounded and known to the evaluator implementation.
- Extensions to the subcategory set are spec events (Part 5 / Part 6), not runtime invention.

The actual subcategory enumeration is a Part 5 concern, pinned to real code paths at implementation time.

### 28.5 Reason is not a retry instruction

Verdict reason does not tell the caller "try again in N seconds" or "resubmit after condition X." V1 keeps retry semantics out of the verdict contract. If a future version adds retry guidance, it is a new field authorized by a new prompt, not an implicit reinterpretation of an existing category.

### 28.6 Operator-facing diagnostic richness is future scope

V1 requires reason to be machine-processable and minimally structured. It does not require reason to be rich enough to drive full operator-facing diagnostic UI. That richness — if needed — is a future augmentation, delivered through additional context fields or through the audit layer, not through extra verdict variants.

## 29. Inputs and Dependency Boundaries

The evaluator is a pure function from a known pair of inputs to a verdict. Part 3B fixes the input boundary so later parts cannot silently widen it.

### 29.1 Allowed inputs

The evaluator consumes exactly two inputs:

- **`LendingSnapshot`** (Part 2) — observed on-chain state, already pre-validated by the System Invariant Gate. Rules and gates read only fields defined in Part 2 §8 – §12.
- **`proposed_action`** — a declarative description of what the caller wants to do. At minimum this carries:
  - Action class (`Deposit` or `Repay` in V1; no other classes survive §25).
  - Mint(s) referenced by the action.
  - Amount(s), in the input token's smallest base unit.
  - Protocol tag identifying which lending protocol the action targets.
  - Any future action-specific parameters added by explicit spec revision.

The `proposed_action` input does NOT carry:

- The signer wallet pubkey. The signer is an invariant from session binding (Part 1 / A2-Execute), not a free parameter of the action. The evaluator reads the signer from the snapshot's session attribution.
- Derived or predicted metrics.
- Simulation results. Simulate is a separate safety backstop (Part 1 §1.4); its role as a rule input would be a new spec event.

### 29.2 Disallowed dependencies

The evaluator does not:

- **Fetch state.** No RPC client, no DB handle, no network capability. Any rule that needs state reads it from the snapshot.
- **Refresh state.** The snapshot is frozen (Part 2 §12.5). If a rule finds a stale component, that is already a §24 or §26 rejection; the evaluator does not trigger re-fetch.
- **Consult caches, files, env vars, or any ambient process state.** Its only inputs are the two defined in §29.1.
- **Produce side effects during evaluation.** Logging, metric emission, audit writes are post-evaluation, not during.
- **Assume protocol-specific raw account layouts.** Part 6 provides the mapping into the Part 2 schema; the evaluator consumes only the normalized schema.
- **Accept implicit defaults from elsewhere.** Rule configuration (thresholds, allowlists) is explicit via the rule's own configured state, not drawn from process globals.

### 29.3 Rules may not retroactively widen the input set

If a rule wants data not present in the snapshot or the proposed action, Part 3B forbids the evaluator from growing side channels to reach it. The correct path is explicit:

- Needed data is a Part 2 schema extension → revise Part 2 in a new spec prompt, then add the rule in a Part 3A / 3B revision.
- Needed data is derived (computed from the snapshot) → defined in Part 4 when drafted.
- Needed data is protocol-specific → belongs to Part 6's mapping, which produces Part 2-shaped snapshot fields. The evaluator still consumes only the schema.

Part 4 is driven by the V1 rule set defined in Part 3A §18, not the other way around. If Part 4 finds itself defining metrics that no V1 rule consumes, Part 4 has drifted out of scope.

### 29.4 Determinism follows from the input boundary

Because `(snapshot, proposed_action)` fully captures all inputs and the snapshot is frozen, two evaluators with identical inputs produce the same verdict. This restates the determinism Part 2 §14 relied on.

## 30. Deferred Decisions and Freeze Boundary

Part 3B is the **final V1 control-plane semantic surface** for lending policy. After Part 3B, the spec deliberately pauses. The next planned work is a read-only spike against a chosen protocol (see §30.3) — not Part 4, not Part 5, not Part 6.

This section exists to break two specific failure modes:

- **The deferral chain.** Each part pushing decisions to the next part, forever, without anything ever settling. §30.2 lists what is now frozen.
- **Zero-feedback drift.** Writing more spec in the absence of concrete evidence from a real protocol. §30.3 makes the spike the next step by default.

### 30.1 What is frozen for V1

As of Part 3B, the following decisions are **Frozen for V1**. They may only change through:

- **(a)** a new prompt that explicitly reopens the decision, OR
- **(b)** concrete evidence from the upcoming read-only spike (§30.3) that invalidates an underlying assumption.

No other path reopens them. In particular, Parts 4 / 5 / 6, when drafted, **must** treat these as fixed constraints and work within them.

Frozen list:

| Decision | Origin | Short form |
|---|---|---|
| Fail-closed default, zero fail-open paths | Part 1 §5 | Every unknown is a Hard Block. |
| V1 action scope | Part 1 §3 | Only `Deposit` and `Repay` are evaluated. `Withdraw` / `Borrow` are blocked at the scope boundary, not by rule. |
| State-aware as a new system category | Part 1 §1.1 | Lending is not "more rules." It is a new category with freshness, staleness, failure semantics. |
| Stale / unreadable / incomplete state = Hard Block | Part 1 §4 / Part 2 §12.6 | No fallback, no degraded mode. |
| Snapshot-first, metrics-later ordering | Part 2 §6 / Part 3A §20 | The read model is defined before derived metrics. |
| Snapshot completeness and freshness invariants | Part 2 §8 – §14 | Complete-or-absent, frozen at assembly, worst-component-staleness dominates. |
| V1 mandatory rule set | Part 3A §18 | Five rules: `RequireFreshState`, `MaxOracleStalenessMs`, `AllowedLendingProtocols`, `AllowedMints`, `MaxActionInputAmount`. |
| Future rules list (not V1) | Part 3A §19 | Everything metric-dependent or risk-increasing-action-dependent is deferred; Part 4 is NOT permitted to drag these into V1. |
| Rule set minimalism | Part 3A §20 | V1 rule set is driven by V1 actions, not by what might "eventually be useful." |
| Two-variant verdict contract | Part 3B §23 | `Pass` / `HardBlock(reason)` and nothing else. No soft-fail, no warn, no retry signal. |
| Three-layer gate model | Part 3B §24 – §26 | System Invariant → Scope Boundary → Rule Evaluation, in that order. |
| Fail-fast / short-circuit evaluation | Part 3B §27 | First `HardBlock` terminates. No collect-all. |
| Three reason categories | Part 3B §28 | `SystemInvariantFailed` / `ScopeBoundary` / `RuleRejected`. No additional top-level categories in V1. |
| Evaluator input boundary | Part 3B §29 | Only `(snapshot, proposed_action)`. No fetching, no refreshing, no ambient state. |

Anything in Parts 1–3B that is not explicitly labeled deferred is implicitly frozen under this list.

### 30.2 What remains deferred

The following are **NOT frozen** — they are genuinely deferred to later parts because they depend on work that has not happened yet.

- **Metrics derivation formulas (LTV, health factor, USD valuation, borrow headroom)** — Part 4. A notable observation: the V1 rule set in Part 3A §18 does not require any derived metric. Part 4, when drafted, may legitimately conclude that V1 needs no metrics at all; that is itself a valid spec outcome and MUST be recorded explicitly to prevent Part 5 from reintroducing them.
- **Concrete Rust types / enums / trait shapes** — Part 5. Part 3B described what the verdict, reason, and gate model MUST represent, not how they are spelled.
- **Module placement** (where the evaluator, snapshot, rules live in the crate graph) — Part 5. This will trigger the `ARCHITECTURE.md §7.1` builders/executors test. The pure-function shape specified in §29 was chosen precisely to keep the "produce a valid artifact from pure inputs" test passable.
- **Evaluator API surface** (invocation shape, configuration passing) — Part 5.
- **Fetch strategy, cache policy, refresh cadence, subscription vs pull** — Part 5.
- **Reason subcategory enumeration** — Part 5, tied to real code paths at implementation time.
- **Rule execution order within §27.1 step 3** (which rule evaluates first) — Part 5, possibly per-protocol tie-breakers in Part 6.
- **Protocol choice** — Part 6. Deciding which lending protocol is V1's first integration is a Part 6 concern; Parts 1–3B deliberately made no choice.
- **Per-protocol account-layout mapping into Part 2 schema** — Part 6.
- **Per-protocol rule default values** (what `MaxOracleStalenessMs` is for Kamino's pyth feed, etc.) — Part 6.

### 30.3 Spike-reopen boundary

The next planned work after Part 3B is a **read-only spike** against a chosen protocol. The spike is not Part 6 and not Part 5 implementation — it is a time-boxed read-only investigation whose explicit purpose is to falsify or confirm Part 2 assumptions against real on-chain data.

The spike's goals:

- Validate that Part 2's snapshot schema captures the fields the chosen protocol actually exposes for risk-relevant decisions.
- Validate that Part 2's freshness-descriptor model matches real RPC fetch behavior (slot visibility, timestamp resolution, publish-time availability).
- Validate that Part 3A §18's V1 mandatory rules can be evaluated from the schema alone, without new inputs.
- Surface protocol-specific edge cases (partial obligations, lifecycle markers, oracle composition) that Part 2's abstract language may have glossed over.

**Decisions the spike MAY cause to reopen (with evidence):**

- Part 2 schema fields — if the protocol exposes risk-relevant data in a form the current schema does not capture.
- Part 2 §14's bounded-divergence-window threshold assumption — if typical fetch timing across components does not fit the assumed window shape.
- Narrow adjustments to individual V1 rules in §18 — if their input shape does not cleanly read from a real snapshot.

**Decisions the spike CANNOT cause to reopen** (a new prompt is required):

- V1 action scope (`Withdraw` / `Borrow` stay blocked; adding them is a scope decision, not a schema correction).
- The fail-closed default (requires explicit threat modeling).
- The three-layer gate model (structural).
- The two-variant verdict shape (structural).
- Rule set minimalism as a design principle (changing it would require re-litigating Part 3A §20).

If the spike surfaces something that feels like it should change one of these non-reopenable items, the correct response is to pause and write a new prompt naming the specific decision and its threat argument — not to patch it under spike authority.

### 30.4 After Part 3B, the next step is the spike — not Part 4

This is the load-bearing operational rule of §30.

Writing Parts 4 / 5 / 6 in the absence of spike feedback runs both failure modes this section exists to prevent:

- **Infinite deferral.** Part 4 defers some decisions to Part 5, Part 5 defers some to Part 6, Part 6 defers protocol-specific choices back into Part 5 implementation, and nothing ever commits to reality.
- **Zero-feedback drift.** Spec becomes internally elegant but possibly incompatible with what real protocols expose. The longer this continues, the larger the eventual correction.

Part 3B's freeze therefore includes a directive: **stop writing spec; go run the spike.** The spike's output — concrete field mappings and a yes/no on "is the schema sufficient" — is what legitimises subsequent Part 4 / Part 5 / Part 6 work. Until the spike produces evidence, those parts remain paused.

This is a deliberately uncomfortable posture for a spec. It is more comfortable to keep refining the spec. The posture is chosen precisely because the discomfort is the point: the spec's value degrades quickly without real feedback, and Part 3B is the last place where further refinement is cheap.

### 30.5 Spike-triggered reopens — log

The read-only spike ran on 2026-04-19 against a Solend Coin98-pool obligation on mainnet (evidence: `spikes/solend_read/spike-report.md`). Per §30.3's authorization envelope, three narrow spec reopens were applied as evidence-driven patches:

- **§11 (Oracle)** — reshaped from a single feed per priced asset into an `OracleFeedSet` per priced asset, with explicit handling of sentinel-pubkey slots and intra-asset provider heterogeneity. Evidence: observed reserve with Pyth Lazer + Switchboard On-Demand feeds coexisting for the same priced asset; two other reserves with the Pyth slot populated by the `nu11…` sentinel pubkey (human-readable "null" marker, account not present on chain).
- **§10 (Reserve)** — numeric quantities across Reserve, Obligation deposits / borrows, and Oracle prices MUST carry an explicit unit tag (underlying / collateral-token / wad at minimum). Schema-level invariant, enforced at the point of first contact. Evidence: a single obligation + its three referenced reserves simultaneously exposed collateral-token units (deposits), `10^18`-scaled wads (borrows, cumulative rates, market values), and underlying base units (reserve available liquidity) — three domains in one snapshot.
- **§18.1 `RequireFreshState`** — contract concretized. Evaluates **both** Claw-fetch age (per §12.3's worst-component rule) AND any protocol-native stale markers (e.g., Solend's `last_update.stale`) on non-oracle components. Disjoint-by-component from §18.2's oracle publish-age check; together the two rules define "fresh enough to evaluate." Evidence: Solend routinely returns `stale = true` by default on bare reads (observed `stale = true` on the obligation and all three reserves, with `last_update.slot` lagging the RPC-reported chain head by ~253M slots); an abstract rule statement could not decide V1's behavior against that signal.

**Explicitly NOT reopened** (all of §30.1's frozen list remains unchanged). The spike exercised Part 3B's gate model, verdict contract, fail-fast ordering, and reason categories against real state, and the three-layer model plus two-variant verdict survived unchanged. No evidence surfaced that would unlock §30.3's "non-reopenable" list either — V1 action scope, fail-closed default, three-layer gate, two-variant verdict, and rule-set minimalism all remain frozen.

With these three §30.3-authorized patches in place, the snapshot schema, Part 3A's V1 rule answerability, and Part 3B's evaluation shape all have an evidence-grounded contract. The operational rule in §30.4 ("after Part 3B, the next step is the spike — not Part 4") has now been satisfied by a concrete spike run; Part 4 becomes the next legitimate work item once these reopens are accepted.

---

# Part 4A — Metrics Layer Scope and V1 Metrics Set

**Status:** draft — Part 4A of N
**Scope of Part 4A:** what the metrics layer is for, whether V1 needs derived metrics at all, the minimum V1 metrics set, and the constraints the spike-triggered reopens (§10.1 unit tags, §11.1 oracle feed sets) impose on any metrics derivation.
**Out of scope for Part 4A (explicit):** a catalog of future metrics; metric formula definitions (LTV, health factor, USD valuation, borrow headroom); Rust types, traits, or function signatures; oracle binary decoders; protocol-specific mapping; implementation roadmap; protocol choice. These are Part 4B / Part 5 / Part 6.

Part 4A inherits every frozen decision from Parts 1 / 2 / 3A / 3B and the three §30.5 spike-triggered reopens. It does not reopen any of them.

## 31. Metrics Layer Overview

### 31.1 What the metrics layer is

The **metrics layer** is a deterministic view-transformation that sits between the snapshot (Part 2) and the rule evaluator (Part 3B). It consumes a frozen `LendingSnapshot` plus, where relevant, a `ProposedAction`, and produces a **`LendingMetrics`** view containing only the derived values that V1 rules actually need.

"Derived" here means a value that is computed from raw snapshot fields using arithmetic or logical combination across fields — as opposed to a value that is read directly out of the snapshot.

### 31.2 What the metrics layer is NOT

- **Not a fetcher.** The metrics layer reads only the snapshot and the proposed action. It does not touch RPC, cache, or any external state. Part 3B §29 already forbids ambient state at the evaluator input boundary; the metrics layer is inside that boundary.
- **Not a decoder.** The metrics layer does not decode oracle binaries, does not unpack protocol-specific account layouts, does not parse program instruction data. All such decoding is a prerequisite to snapshot assembly and belongs to Part 5 (schema-to-code) or Part 6 (protocol mapping).
- **Not a rule evaluator.** The metrics layer does not return `Pass` / `HardBlock`; it returns derived values. The rule engine, not the metrics layer, decides whether a value violates a threshold.
- **Not a normalizer-of-convenience.** The metrics layer does not exist to paper over unit mismatches, oracle-shape mismatches, or schema gaps. If a metric requires a conversion that Part 2 has not committed to supporting with a unit tag (§10.1) or an oracle feed set (§11.1), that metric is not V1-ready — full stop.

### 31.3 Where the metrics layer sits in the evaluation order

Within Part 3B's three-layer gate model (§24 – §26), metrics derivation runs **only between the Scope Boundary Gate (§25) and Rule Evaluation (§26)**, and only for rules that declare a metric dependency. Specifically:

1. System Invariant Gate — uses raw snapshot presence only. No metrics.
2. Scope Boundary Gate — uses the proposed action and protocol tag only. No metrics.
3. Metrics derivation (this layer) — produces derived values that Step 4 may consume.
4. Rule Evaluation — evaluates each rule. Rules that do not declare a metric dependency never see metrics.

This ordering means metrics derivation is **lazy by shape, not by implementation**: if the V1 rule set declares no metric dependencies, step 3 is a no-op. The metrics layer is still defined, but in V1 it produces nothing.

### 31.4 What Part 4A is actually for

Part 4A exists to answer one gating question: **does V1 require any derived metrics?** If yes, Part 4A fixes the minimum set. If no, Part 4A records that result and prevents future implementation work from reintroducing metrics under the banner of "V1 readiness."

Part 4A is deliberately not a complete metrics catalog. A catalog of lending metrics that might one day be useful belongs to Part 4B; it MUST NOT be conflated with V1 scope. Rule minimalism (Part 3A §20) applies symmetrically to metrics: every V1 metric must be justified by a V1 rule, not by "it will probably be needed later."

## 32. Does V1 Need Derived Metrics?

This section walks each of Part 3A §18's five V1 mandatory rules and asks, for each: does evaluation of this rule require a value that is not already present verbatim in `(snapshot, proposed_action)`?

### 32.1 `RequireFreshState` (Part 3A §18.1)

- **What the rule checks.** For every non-oracle snapshot component: (a) Claw-fetch freshness is within the configured staleness window per Part 2 §12.3, AND (b) any protocol-native stale marker (e.g., Solend `last_update.stale`) reads "fresh."
- **Inputs needed.** Per-component and snapshot-level freshness descriptors (`fetched_at`, `observed_slot`) — Part 2 §12.2. Obligation and Reserve `protocol_last_updated_slot` and lifecycle markers — Part 2 §9, §10. A configured staleness threshold (rule config, not a metric).
- **Derived value required?** **No.** The comparison `snapshot_level.fetched_at − component.fetched_at ≤ threshold` and the Boolean read of a protocol-native `stale` bit are direct field reads plus a numeric comparison inside the rule evaluator. Neither step computes a value across fields; both stay at the snapshot-read level.
- **Verdict.** Answerable from snapshot alone.

### 32.2 `MaxOracleStalenessMs` (Part 3A §18.2)

- **What the rule checks.** Each oracle feed's `publish_timestamp` (or equivalent `publish_slot` with a slot-to-time conversion) is within the configured staleness bound relative to the snapshot-level clock.
- **Inputs needed.** Per-feed `Publish timestamp / slot` — Part 2 §11.2. Snapshot-level `fetched_at` — Part 2 §12.2. A configured staleness threshold.
- **Derived value required?** **No.** The comparison is `snapshot_clock − feed.publish_timestamp ≤ threshold`. Where the oracle exposes only a slot and the threshold is in milliseconds, the slot-to-time conversion is a straight function of `observed_slot` and Solana's slot duration — a local arithmetic step inside rule evaluation, not a cross-field metric.
- **Note on prerequisites.** Extracting `publish_timestamp` from an oracle binary (Pyth Lazer payload, Switchboard On-Demand payload) is a **decoder** concern, not a metric concern. The spike observed (§30.5) that the oracle account owner program is distinguishable at the account layer, but the actual payload parse is Part 5 (schema-to-code) or Part 6 (protocol mapping). Once the decoder exists, the field is directly available; the metrics layer stays empty.
- **Verdict.** Answerable from snapshot alone, assuming Part 5/6 decoders populate the field. No derived metric is introduced by the metrics layer.

### 32.3 `AllowedLendingProtocols` (Part 3A §18.3)

- **What the rule checks.** `proposed_action.protocol_tag ∈ configured_allowlist`, and `snapshot.protocol_tag == proposed_action.protocol_tag` (so the action is not evaluated against the wrong snapshot).
- **Inputs needed.** `snapshot.protocol_tag` — Part 2 §8. `proposed_action.protocol_tag`. The configured allowlist.
- **Derived value required?** **No.** Two pubkey / enum equality checks.
- **Verdict.** Answerable from snapshot and action alone.

### 32.4 `AllowedMints` (Part 3A §18.4)

- **What the rule checks.** Every mint the proposed action touches (collateral mint for `Deposit`, debt mint for `Repay`) is in the operator-configured mint allowlist, and cross-references the corresponding Reserve / Obligation-position in the snapshot to confirm the mint is a legitimate target (e.g., a `Repay` against a mint the obligation has no debt in is rejected).
- **Inputs needed.** `proposed_action` mint fields. Snapshot `Reserve.liquidity.mint_pubkey` — Part 2 §10. Obligation deposit / borrow mint references — Part 2 §9. The configured allowlist.
- **Derived value required?** **No.** Membership checks and reference cross-checks; no arithmetic across fields.
- **Verdict.** Answerable from snapshot and action alone.

### 32.5 `MaxActionInputAmount` (Part 3A §18.5)

- **What the rule checks.** `proposed_action.input_amount ≤ configured_cap`, where both sides are in the input token's smallest base unit.
- **Inputs needed.** `proposed_action.input_amount` (already in native base units per §18.5). Reserve `decimals` for denomination clarity in rule configuration — Part 2 §10. The configured cap.
- **Derived value required?** **No.** A single integer comparison in the token's native unit. §18.5 explicitly forbids the V1 form from taking a USD-denominated cap precisely to avoid introducing a metric dependency; that decision is already frozen.
- **Verdict.** Answerable from action alone (with `decimals` as display / configuration context, not a derived value).

### 32.6 Conclusion for V1

**V1 requires zero derived metrics.**

Every V1 mandatory rule is answerable from `(snapshot, proposed_action)` via:

- direct field reads,
- equality / membership checks against configured allowlists,
- numeric comparisons against configured thresholds, where both sides of the comparison are already in the same unit by schema construction.

No cross-field arithmetic, no value conversion across units, no valuation against oracle prices, no ratio computation between asset classes is required by any V1 rule.

This is not a gap to be filled in later; it is the **intended outcome** of rule-set minimalism (Part 3A §20) meeting action-set minimalism (Part 1 §3). V1's scope is exactly `Deposit` and `Repay` — both risk-reducing — and the V1 rule set was chosen so that each rule is locally decidable from the snapshot's verbatim fields. The metrics layer's V1 obligation is therefore to produce nothing.

## 33. V1 Metrics Set

### 33.1 The V1 metrics set is empty

The V1 metrics set is **empty**. The metrics layer is defined as a concept in §31 because later versions will need it, but it produces no values in V1.

Operationally, this means:

- V1 implementations MAY omit the metrics-layer step entirely from the evaluation pipeline. The rule evaluator receives `(snapshot, proposed_action)` directly and consumes only those.
- V1 implementations that do include a metrics-layer step MUST have it return an empty `LendingMetrics` in all cases. No conditional derivation, no "compute this if an operator enables it," no feature flags that turn on derived values.
- Any proposed addition to the V1 metrics set — at any point before V1 ships — requires an explicit spec prompt that cites which V1 rule depends on the new metric. "Might be useful," "other protocols have it," and "we might need it for observability" are not sufficient justifications. The bar is the same as Part 3A §18: every V1 element traces to a V1 product requirement.

### 33.2 What "empty" closes off

Stating that V1 metrics set is empty is load-bearing because it prevents three failure modes that otherwise recur when a spec defers metric decisions:

- **Metrics creep.** A contributor adds `collateral_value_usd` "because it will be useful for dashboards," then a later rule uses it as an input, then the V1 rule set has implicitly grown. §33.1's empty set denies the first step.
- **Back-door Withdraw/Borrow enablement.** Metrics like LTV and health factor only become policy-relevant once a risk-increasing action is in scope. Defining them in V1 quietly signals that V1 is edging toward `Withdraw` / `Borrow`, which is a Part 1 §3 scope boundary decision, not a Part 4 decision. The empty set keeps Part 4 from accidentally reopening Part 1.
- **Part 4 drifting out of its lane.** Part 3A §20.3 and Part 3B §29.3 already state that Part 4 serves Part 3A, not the other way around. §33.1's empty set is the operational expression of that rule.

### 33.3 Future metrics stay explicitly future-only

Every derived value whose natural home is Part 4 — LTV (current or predicted), health factor, borrow headroom, USD valuations (`collateral_value_usd`, `debt_value_usd`), utilization-based risk premiums — remains **future-only** under Part 4A. They may be catalogued in Part 4B; they may not be implemented in V1.

Part 3A §19 already lists the future rules that would consume these metrics (`MaxCurrentLtv` §19.1, `MaxPredictedLtv` §19.2, `MinHealthFactor` §19.3, `AllowedCollateralMints` / `AllowedBorrowMints` §19.4, `MaxBorrowHeadroomUsage` §19.5, `MaxActionValueUsd` §19.6) and records the prerequisite each is waiting on. Part 4A does not re-list these; it simply points at §19 and confirms that none of those prerequisites is cleared by Part 4A, and none may cross into V1 scope.

## 34. Unit and Oracle Constraints on Metrics Derivation

Even though the V1 metrics set is empty, the metrics layer's **future** behavior is constrained now by the spike-triggered reopens. Recording these constraints in Part 4A prevents later work from reinterpreting the reopens as Part 4 problems to be patched around.

### 34.1 Unit tags constrain what metrics may assume

Per §10.1, every numeric quantity in the snapshot carries a unit tag distinguishing at minimum underlying / collateral-token / wad. The metrics layer MUST treat this tag as structural, not documentary:

- **No inference from field names.** A field named `borrowed_amount` is not assumed to be in underlying units. The metrics layer reads the tag, or it does not use the value.
- **No cross-unit arithmetic without explicit conversion.** Adding a collateral-token amount to an underlying amount is forbidden. Comparing a wad-scaled market value to an underlying amount is forbidden. Conversions between units are a derived step and MUST be expressed as explicit metric operations that (a) consume tagged inputs, (b) produce tagged outputs, (c) declare the conversion's inputs (e.g., reserve exchange rate for c-token ↔ underlying, wad-unscale for wad ↔ underlying).
- **Conversion prerequisites live in the snapshot, not in the metrics layer.** The c-token-to-underlying exchange rate, the wad scale factor — these are Reserve-level facts the snapshot carries verbatim. The metrics layer consumes them; it does not fetch or assume them.

These constraints are dormant in V1 (empty metrics set, no derivation). They activate the moment Part 4B introduces a first derived metric.

### 34.2 Oracle feed-set shape constrains USD-denominated metrics

Per §11.1, oracle pricing is a feed *set* per priced asset, not a single feed. The metrics layer MUST treat this as a structural input, not a convenience:

- **No "the price of X."** Any metric that consumes oracle pricing takes an `OracleFeedSet` as input, not a scalar price. If the set is empty, the metric is undefined (fail-closed: the policy layer HardBlocks per Part 1 §5.2).
- **Combination semantics are explicit.** When multiple feeds price the same asset, the metric MUST declare which combination semantic it uses: disjunction ("any fresh feed is sufficient"), conjunction ("all feeds must be fresh and their prices must agree within tolerance"), preferred-provider ordering, or weighted. No silent defaulting. This choice is metric-specific and lives in the metric's definition, not in snapshot assembly.
- **Per-feed staleness is preserved through derivation.** A metric that derives a USD value from an oracle feed MUST carry the per-feed staleness into the output. The "worst-component dominates" rule (Part 2 §12.3) applies recursively: a derived value's freshness is the worst freshness among its inputs.

### 34.3 Why USD-denominated metrics cannot be V1

Even if Part 4B were drafted today, USD-denominated metrics (`collateral_value_usd`, `debt_value_usd`, and every downstream ratio that reduces to a USD comparison) require three independent pieces of groundwork that are **not** in place:

1. **Oracle binary decoders (Part 5 / Part 6).** The `price_value` field in Part 2 §11.2 is defined abstractly. Populating it for Pyth Lazer and Switchboard On-Demand requires format-specific decoders. The read-only spike (§30.5) identified the owner programs but did not decode the payloads; that decoding is not in V1 scope.
2. **Unit-tag-aware conversion formulas.** Translating a wad-scaled per-position borrow into a USD value requires: (a) wad-unscale to underlying, (b) underlying × oracle price per underlying → USD, (c) oracle price confidence-interval handling. Each step is a derivation governed by §34.1.
3. **Feed-set combination semantic for each USD metric.** See §34.2. The choice is a policy decision, not an implementation detail.

Until all three are committed and wired, `collateral_value_usd` and its derivatives (LTV, HF, borrow headroom) cannot be safely derived — not because the math is unclear, but because the inputs the math depends on are not in a state where a V1-grade fail-closed system can rely on them. Part 4A therefore holds the line: these remain future-only under §33.3, and Part 4A does not attempt to partially implement them.

### 34.4 What Part 4A commits to, operationally

- The V1 metrics set is empty (§33.1). This is the only substantive V1 commitment Part 4A makes.
- The metrics layer's future shape is constrained by unit tags (§34.1) and oracle feed sets (§34.2). These constraints are recorded now so later Part 4B work inherits them.
- USD-denominated metrics remain future-only (§34.3). Part 4A does not attempt to unlock them and does not list prerequisites beyond pointing at Part 5 / Part 6.

Nothing in Part 4A authorizes Part 4B, Part 5, or Part 6 to start; those remain gated by their own spec prompts. Part 4A closes the single question "does V1 need derived metrics?" with a clean "no," and stops.

---

# Part 4B — Future Metrics Catalog and Derivation Boundary

**Status:** draft — Part 4B of N
**Scope of Part 4B:** a controlled catalog of future-only metrics; the prerequisite layers each metric depends on before it can become implementation-ready; the derivation boundary that every metric — present or future — must respect; an explicit list of things Part 4 refuses to do; a short set of open questions deferred to later work.
**Out of scope for Part 4B (explicit):** metric formula definitions; Rust types, traits, enums, or function signatures; module layout for metrics; oracle provider or aggregation-policy design; predicted-state engine design; protocol-specific semantic mappings; implementation roadmap; protocol choice; any V1 metric addition. All of these belong to later spec prompts or to Part 5 / Part 6 implementation work.

Part 4B inherits every frozen decision from Parts 1 / 2 / 3A / 3B / 4A and the three §30.5 spike-triggered reopens. It does not reopen any of them. Part 4A's conclusion that **V1 requires zero derived metrics (§33.1)** remains in force; Part 4B does not weaken it.

The purpose of Part 4B is to make "future-only" a **controlled** state rather than a vague deferral. A metric listed here is catalogued, not committed to. Listing a metric does not authorize its implementation, does not imply it will ever ship, and does not obligate Part 5 or Part 6 to prepare for it.

## 35. Future Metrics Catalog

This catalog is conservative by construction. Each entry records what the metric means, which future rule or action would consume it, why it is not V1, and the prerequisite layers that must exist before it can move from future-only to implementation-ready. **No entry below is authorized for implementation.**

Each entry uses the same shape:

- **Semantic intent** — what this value represents, in words.
- **Question it answers** — the policy or advisory question the metric supports.
- **Consumer rules / actions** — which Part 3A §19 future rule (or future advisory path) would consume it.
- **Why it is not V1** — the specific V1-scope blocker.
- **Prerequisites** — the groundwork that must be in place before it is implementation-ready.

No entry contains a formula. Formulae are Part 5 or Part 6 decisions.

### 35.1 `current_ltv`

- **Semantic intent.** The loan-to-value ratio of an obligation at snapshot assembly time. Conceptually, the ratio of total borrow value to total collateral value, both expressed in a common valuation base.
- **Question it answers.** Is this wallet's position currently at an unsafe leverage level, before any proposed action runs?
- **Consumer rules / actions.** Part 3A §19.1 `MaxCurrentLtv`.
- **Why it is not V1.** Part 3A §19.1 already records that V1's allowed actions (`Deposit`, `Repay`) can only lower current LTV; a cap on pre-action LTV therefore has no risk-blocking effect on V1 actions. Additionally, the metric's prerequisites are not in place (below).
- **Prerequisites.** Unit-tag-aware conversion rules (§34.1) to move between c-token / underlying / wad domains; oracle binary decoders for each priced asset (§34.3); per-asset feed-set combination semantic (§34.2); protocol-specific reserve mapping (Part 6); cross-validation against the protocol's own LTV number so Claw's value is not silently divergent from the protocol UI.

### 35.2 `predicted_ltv`

- **Semantic intent.** LTV after the proposed action executes, using the same valuation base and same inputs as `current_ltv`, with the action's effect simulated on the position.
- **Question it answers.** Would this action cross an LTV threshold once executed?
- **Consumer rules / actions.** Part 3A §19.2 `MaxPredictedLtv`.
- **Why it is not V1.** Requires a committed predicted-state model (either local replay of the action's effect on snapshot state, or RPC `simulateTransaction` result parsing). Also meaningful only for risk-increasing actions; V1 has none.
- **Prerequisites.** All prerequisites of `current_ltv` PLUS a committed predicted-state model PLUS at least one risk-increasing action type inside V1 scope (the latter is a Part 1 §3 scope-boundary decision, not a Part 4 decision).

### 35.3 `health_factor`

- **Semantic intent.** A ratio between liquidation-threshold-weighted collateral value and borrow value, expressed in the protocol's conventional form. Typically `>= 1.0` is safe, `< 1.0` is liquidatable; the exact formula varies per protocol.
- **Question it answers.** How close is this position to a liquidation threshold?
- **Consumer rules / actions.** Part 3A §19.3 `MinHealthFactor`.
- **Why it is not V1.** Same category as `current_ltv` — requires valuation, combination, and decoders; provides no risk-blocking effect against V1's `Deposit` / `Repay`.
- **Prerequisites.** All prerequisites of `current_ltv` PLUS an explicit decision on **protocol parity** (see §39.1): must Claw's HF numerically match the protocol's own HF (display parity with protocol UI), or may it be a Claw-side conservative envelope that errs safer but can disagree with the protocol's displayed number?

### 35.4 `predicted_health_factor`

- **Semantic intent.** HF after the proposed action, using the same valuation base and parity choice as `health_factor`.
- **Question it answers.** Would this action cross a minimum HF threshold once executed?
- **Consumer rules / actions.** A predicted-variant extension of §19.3, not individually listed in Part 3A §19 today. Its addition is itself a future spec decision.
- **Why it is not V1.** Same as `predicted_ltv` — requires predicted-state model plus risk-increasing action in scope.
- **Prerequisites.** All prerequisites of `health_factor` PLUS predicted-state model PLUS scope unlock.

### 35.5 `collateral_value_usd`

- **Semantic intent.** Aggregate USD-denominated value of the obligation's collateral deposits, summed across reserves.
- **Question it answers.** What is the total USD value of what the user has posted as collateral?
- **Consumer rules / actions.** Not directly consumed by any single Part 3A §19 rule; used as an **intermediate** input to `current_ltv`, `health_factor`, `borrow_headroom`, and any future USD-denominated cap (e.g., a hypothetical `MaxActionValueUsd` at Part 3A §19.6).
- **Why it is not V1.** Part 4A §34.3 already documents this explicitly: three independent groundwork items (oracle decoders, unit-tag-aware conversion formulas, feed-set combination semantic) are missing.
- **Prerequisites.** §34.1 (unit conversion), §34.3 (oracle decoders), §34.2 (feed-set combination semantic), plus protocol-specific reserve mapping (Part 6).

### 35.6 `borrow_value_usd`

- **Semantic intent.** Aggregate USD-denominated value of the obligation's borrows, summed across reserves.
- **Question it answers.** What is the total USD value of what the user owes?
- **Consumer rules / actions.** Same shape as `collateral_value_usd` — intermediate input to LTV / HF / headroom / USD-cap rules.
- **Why it is not V1.** Same as §35.5.
- **Prerequisites.** Same as §35.5.

### 35.7 `borrow_headroom`

- **Semantic intent.** The additional borrowing a position could absorb before hitting the protocol's own per-obligation borrow limit. Often expressed in USD but may be expressed in a specific quote asset.
- **Question it answers.** Is the user close to their protocol-declared borrow ceiling?
- **Consumer rules / actions.** Part 3A §19.5 `MaxBorrowHeadroomUsage`.
- **Why it is not V1.** Derived from `current_ltv` or protocol-declared max-borrow-value fields; requires their prerequisites plus a mapping to each protocol's specific "allowed borrow value" representation (e.g., Solend exposes `allowed_borrow_value` as a per-obligation wad-scaled field — see spike evidence at §30.5).
- **Prerequisites.** All prerequisites of `current_ltv` PLUS protocol-specific max-borrow-value field mapping (Part 6).

### 35.8 `max_safe_withdraw_amount`

- **Semantic intent.** The largest collateral withdrawal that would leave the position within a configured safety threshold (LTV, HF, or headroom).
- **Question it answers.** How much collateral can the user safely remove?
- **Consumer rules / actions.** No V1 rule consumes this; no §19 rule lists it today. Plausible future home is an advisory path for `Withdraw` planning, not a rule.
- **Why it is not V1.** `Withdraw` is blocked at the V1 scope boundary (Part 1 §3). Absent a `Withdraw` action, there is no consumer.
- **Prerequisites.** All LTV / HF prerequisites PLUS predicted-state model PLUS a Part 1 §3 scope unlock for `Withdraw` (scope decision, not a Part 4 decision).

### 35.9 `max_safe_borrow_amount`

- **Semantic intent.** The largest additional borrow that would stay within configured safety thresholds.
- **Question it answers.** How much more can the user safely borrow?
- **Consumer rules / actions.** No V1 rule; plausible future home is advisory for `Borrow` planning.
- **Why it is not V1.** `Borrow` is blocked at the V1 scope boundary.
- **Prerequisites.** Same as `max_safe_withdraw_amount`, with scope unlock for `Borrow` instead of `Withdraw`.

### 35.10 Catalog discipline

The nine entries above are the set Part 4B commits to maintaining. Adding a new entry in future work requires:

- a clear future rule or advisory consumer that motivates the metric, and
- explicit prerequisite listing using the same shape as §35.1 – §35.9.

Speculative metrics, metrics that exist only because "other protocols have them," or metrics that have no consumer path are not catalog-worthy. The catalog is a scoping tool, not a wishlist.

## 36. Metric Dependency Matrix

The table below summarises each future metric's prerequisite layers. Columns are the layers; cells record whether the layer is **required** before the metric can move to implementation-ready, or **not applicable** to that metric.

Prerequisite columns:

- **Snapshot alone** — Is the metric answerable from the Part 2 schema verbatim, with no derivation?
- **Unit conv.** — Unit-tag-aware conversion rules between c-token / underlying / wad (§34.1).
- **Oracle dec.** — Oracle binary decoders that populate `publish_timestamp` and `price_value` on each `OracleFeed` (§34.3; Part 5 / Part 6).
- **Feed-set agg.** — Combination semantic for multi-feed `OracleFeedSet` per priced asset (§34.2; see §39.2).
- **Protocol map.** — Per-protocol mapping of raw reserve / obligation fields into the Part 2 schema, plus any protocol-specific auxiliary fields (e.g., Solend's `allowed_borrow_value`). Part 6.
- **Predicted state** — A committed post-action state model (local replay or RPC simulate-parse).
- **Cross-val.** — Cross-validation against the protocol's own number (display parity or bounded divergence — see §39.1).

Legend: **Req** = required before implementation-ready. **—** = not needed for this metric. **Opt** = useful but not gating for a V1-grade implementation.

| Metric | Snapshot alone | Unit conv. | Oracle dec. | Feed-set agg. | Protocol map. | Predicted state | Cross-val. |
|---|---|---|---|---|---|---|---|
| `current_ltv` | No | Req | Req | Req | Req | — | Req |
| `predicted_ltv` | No | Req | Req | Req | Req | Req | Req |
| `health_factor` | No | Req | Req | Req | Req | — | Req |
| `predicted_health_factor` | No | Req | Req | Req | Req | Req | Req |
| `collateral_value_usd` | No | Req | Req | Req | Req | — | Opt |
| `borrow_value_usd` | No | Req | Req | Req | Req | — | Opt |
| `borrow_headroom` | No | Req | Req | Req | Req | — | Req |
| `max_safe_withdraw_amount` | No | Req | Req | Req | Req | Req | Req |
| `max_safe_borrow_amount` | No | Req | Req | Req | Req | Req | Req |

**Orthogonal to the table: scope unlock.** Some metrics (`predicted_ltv`, `predicted_health_factor`, `max_safe_withdraw_amount`, `max_safe_borrow_amount`) additionally require a Part 1 §3 scope-boundary decision to unlock `Withdraw` and/or `Borrow`. That decision is not a metric prerequisite per se — it governs whether the metric has a consumer at all. It is recorded here so reviewers do not mistake a metric for "implementation-ready" simply because all table columns are cleared.

### 36.1 What the matrix does NOT decide

- Order of work. The matrix records dependencies, not a roadmap. Which prerequisites to clear first, or whether to clear them at all, is a future spec / Part 5 / Part 6 decision.
- Sharing of prerequisites. Several metrics share the same prerequisite (unit conversion, feed-set aggregation, protocol mapping). The matrix does not prescribe whether these are implemented once and reused, or separately per metric — that is an implementation-architecture call.
- Protocol selection. The matrix is protocol-agnostic; "Protocol map. — Req" applies to whichever protocol is eventually chosen (Part 6), and the choice of protocol is not a Part 4 decision.

## 37. Derivation Boundary

Every metric — V1 (none today) or future (any of §35) — MUST respect the boundary below. The boundary is a structural contract on the metrics layer; violating it turns the metrics layer into something else (a fetcher, a decoder, a rule engine), which is already ruled out by Part 4A §31.2.

### 37.1 The metrics layer is a pure-function layer

- **Inputs** — exactly the frozen `LendingSnapshot` (Part 2) and, where a metric depends on it, the `ProposedAction` already carried by Part 3B §29's evaluator input. Nothing else.
- **Outputs** — derived values, each carrying its own unit tag (§34.1) and its own freshness descriptor inherited from the snapshot components it was derived from (§34.2's "per-feed staleness is preserved through derivation"; by extension, per-component staleness is preserved for non-oracle inputs).
- **Determinism** — two derivations with the same `(snapshot, proposed_action)` produce bit-identical outputs. No wall-clock reads, no randomness, no iteration order dependence.

### 37.2 What the metrics layer MUST NOT do

- **No fetching.** No RPC calls, no cache reads, no subscription consumption. The snapshot is the only view of chain state available to this layer.
- **No refreshing.** If the snapshot says a component is stale, the metrics layer does not try to improve that state. Refresh is the snapshot assembly layer's job (Part 5 / Part 6), and only before the evaluator boundary.
- **No raw protocol-layout decoding.** The metrics layer reads schema fields, not raw bytes. Oracle binaries, protocol account layouts, instruction-data payloads — all decoded into the schema upstream.
- **No implicit feed-set aggregation.** A metric that consumes an `OracleFeedSet` MUST declare its combination semantic explicitly (§34.2). "The first feed," "the most recent feed," "the provider I prefer" are each explicit choices, recorded in the metric's own definition; none are default behavior.
- **No oracle-provider prioritisation.** Choosing between Pyth Lazer and Switchboard On-Demand when both price the same asset is a policy decision, not an implementation shortcut. The metrics layer does not pick; the metric's declared combination semantic expresses the policy, and the choice of that semantic is a Part 3 / Part 4 future-spec decision.
- **No verdict production.** The metrics layer does not emit `Pass` / `HardBlock`. It produces derived values; the rule engine (Part 3B §26) consumes those values and produces the verdict.
- **No snapshot mutation.** The snapshot is frozen per Part 2 §12.5. The metrics layer cannot re-freshen, re-assemble, or patch missing fields.

### 37.3 Promotion criterion: future-only → implementation-ready

A metric in the §35 catalog may be promoted from future-only to implementation-ready only when **all** of the following hold:

1. Every **Req** cell in its §36 row is cleared by a committed spec or implementation (not merely "planned").
2. Its consumer rule or action exists in V1 scope (or has been explicitly unlocked by a Part 1 §3 scope decision, for metrics whose consumer is a currently-blocked action).
3. Cross-validation procedure is defined for metrics whose §36 row lists `Cross-val. = Req` (see §39.1).
4. A spec prompt has been written that explicitly authorizes the metric's V1 inclusion, following the same bar as Part 3A §18 (each element traces to a concrete product requirement).

Absent any of these, the metric remains future-only. Part 4B does not authorize promotion; it only defines the promotion criterion.

## 38. What Part 4 Explicitly Refuses to Do

Parts 4A and 4B together close the metrics-layer spec at the semantic and boundary level. They deliberately do **not** do the following, and later readers must not misread them as "these things are ready to start":

- **No V1 derived metrics.** The V1 metrics set is empty (Part 4A §33.1). Part 4B does not add any, regardless of how future-facing it feels.
- **No formula definitions.** Part 4 does not write `LTV = ...`, `HF = ...`, `borrow_headroom = ...`, or any numeric formula. Formulae are Part 5 (schema-to-code) or Part 6 (protocol mapping) decisions.
- **No canonical LTV / HF algorithm.** Choosing one canonical formula for each of LTV and HF — especially across multiple future-supported protocols whose own formulae differ — is a policy decision that requires its own spec prompt. Part 4 does not make that choice.
- **No oracle provider or aggregation policy choice.** Whether Pyth Lazer is preferred over Switchboard On-Demand, whether feed-set aggregation is disjunction or conjunction, whether confidence-interval widths factor into derived USD values — all deferred. Part 4 only records that these choices must be made explicitly.
- **No protocol-specific semantics.** Part 4 is protocol-agnostic. Solend, Kamino, MarginFi, port.finance — none are chosen, none are favoured, and none of their protocol-specific conventions are smuggled into Part 4 vocabulary.
- **No predicted-state engine.** The choice between local post-action state replay and RPC `simulateTransaction` result parsing is a Part 5 / Part 6 decision. Part 4 lists "predicted-state model" as a prerequisite without committing to either path.
- **No `Withdraw` / `Borrow` unlock.** These remain Part 1 §3 scope-boundary hard blocks. Part 4 cataloguing `max_safe_withdraw_amount` or `max_safe_borrow_amount` does not signal intent to unlock the actions; those are a separate scope decision.
- **No implementation contract.** Part 4B does not commit to a schedule, a milestone, or an implementation order for any future metric.
- **No Rust types, traits, or module layout for future metrics.** Part 4 uses only abstract language (snake_case identifiers and prose). Typed artefacts are Part 5 work; module placement is an `ARCHITECTURE.md §7` test that runs when Part 5 actually lands.
- **No advisory UI surface.** Metrics like `max_safe_withdraw_amount` and `max_safe_borrow_amount` catalogue a plausible future advisory path; Part 4 does not design that path, and does not imply one exists in V1.

The refusal list is load-bearing. Without it, a future reader could interpret the §35 catalog as a to-do list — which is exactly what Part 4B exists to prevent.

## 39. Open Questions / Deferred Decisions

The questions below are **not** for Part 4B to answer. They are recorded here so later spec work knows which decisions remain outstanding when it takes up Part 4 material.

### 39.1 Protocol parity vs. Claw conservative envelope

For LTV, HF, and `borrow_headroom`: must Claw's derived value numerically match the protocol's own displayed value (strict parity), or may Claw produce a conservative envelope that errs safer but can disagree with the protocol UI?

- **Parity** makes the user experience consistent across the Claw dashboard and the protocol's own dashboard, at the cost of fully reproducing the protocol's formula — including protocol-specific quirks, oracle choices, and rounding conventions.
- **Conservative envelope** lets Claw enforce a stricter-than-protocol safety line, at the cost of occasional divergence from the protocol's own view that must be explained to users.

This choice affects cross-validation requirements (§36 `Cross-val.` column) and the shape of Part 5's metric definitions. It is not decidable by Part 4 alone; it is a product / risk decision.

### 39.2 Feed-set aggregation policy — standalone spec or per-protocol mapping?

The §34.2 / §37.2 requirement that every metric declare its `OracleFeedSet` combination semantic leaves open where that declaration lives:

- One option: a **single cross-protocol aggregation-policy spec** that defines a small set of named semantics (disjunction, conjunction, preferred-provider, weighted) and requires every metric to pick one.
- Another option: **per-protocol mapping** (Part 6) defines the combination semantic for that protocol's feeds, and metrics inherit whatever the protocol's adapter supplies.

The tradeoff: cross-protocol consistency vs. protocol-specific correctness. Neither is decidable by Part 4.

### 39.3 Predicted metrics — Part 4 extension or V2 chapter?

`predicted_ltv`, `predicted_health_factor`, `max_safe_withdraw_amount`, `max_safe_borrow_amount` all require a predicted-state model. The model is a significant piece of work (local replay of action effects, or RPC-simulate-parse). Open question: does predicted-state modelling belong as a future extension of Part 4 (a Part 4C, say), or as a separately-scoped V2 chapter alongside `Withdraw` / `Borrow` unlocking?

Recording the question here means a future spec author does not have to rediscover it.

### 39.4 Which metrics, if any, belong only to a specific protocol?

Some protocols expose values with no cross-protocol analogue (e.g., Solend's `super_unhealthy_borrow_value`, which exists alongside `unhealthy_borrow_value` to drive a graduated liquidation flow). Open question: should such values surface as metrics in the generic vocabulary at all, or should they stay inside the protocol adapter and only be consumed by protocol-specific rules?

This tension cuts against "minimalism in the common vocabulary" (Part 3A §20) and "faithful to source" (Part 2 §10). Part 4B does not resolve it; it notes that a future spec must.

### 39.5 Slot-time conversion policy

Several metrics (and Part 3A §18.2 `MaxOracleStalenessMs`) compare a slot-valued timestamp against a millisecond threshold. Solana slot duration is nominally 400 ms but empirically varies. Open question: which source (which RPC, which historical window) is authoritative for slot-to-time conversion, and is that conversion applied in the metrics layer, the rule evaluator, or upstream during snapshot assembly?

This is not urgent in V1 — §18.2 is the only consumer and can take a conservative upper bound — but any USD-denominated future metric that ties valuation to publish-time will hit this question.

### 39.6 Oracle confidence interval handling

When `OracleFeed.confidence_interval` is present, how (if at all) does it enter a derived USD value?

- Use midpoint only: ignores confidence.
- Use worst-edge (lower edge for collateral, upper edge for debt): conservative, reduces false negatives at the cost of user-visible divergence from protocol numbers.
- Reject if confidence exceeds a bound: different policy shape — handled as a rule input, not a derived metric.

Each choice has product consequences. Part 4B records the question; it does not answer it.

---

Part 4A and Part 4B together close the Part 4 spec surface. Parts 4A / 4B do not authorize any implementation work; that remains gated by later spec prompts and by the promotion criterion in §37.3. The metrics layer stands defined, bounded, and empty for V1.

---

# Part 5A — V1 Implementation Boundaries: Placement & Seam

**Status:** draft — Part 5A of N
**Scope of Part 5A:** module placement semantics for the V1 `Deposit` / `Repay` slice; the hard seam between Part 5 (contracts / boundaries) and Part 6 (protocol-specific implementation); the assembler–evaluator handoff contract; the unit-tagging principle at the boundary level.
**Out of scope for Part 5A (explicit):** concrete Rust types, traits, API signatures, module paths, crate names; protocol choice; raw account-layout mapping; oracle binary decoders; refresh / cache / TTL / retry policy; transaction builder shape; control-flow call graph; wire-format choices; test layout. All of these belong to Part 5B, Part 6, or later implementation work.

Part 5A inherits every frozen decision from Parts 1 / 2 / 3A / 3B / 4A / 4B and the three §30.5 spike-triggered reopens. It does not reopen any of them. In particular, it does not weaken:

- Part 4A §33.1 (V1 metrics set is empty).
- Part 4B §37 (metrics-layer derivation boundary).
- Part 3B §29 (evaluator input boundary: only `(snapshot, proposed_action)`).

Part 5A's purpose is to clear a narrow runway for the V1 `Deposit` / `Repay` vertical slice without letting implementation land in the wrong place. Nothing here authorizes implementation to start; it records the placement contract that implementation work, when separately authorized, must satisfy.

## 40. Module Placement & Taxonomy

### 40.1 `ARCHITECTURE.md §7.1` remains the only taxonomy

Part 5A does **not** introduce a repo-wide third component category. `ARCHITECTURE.md §7.1` keeps its two-way split:

| Test question | If yes |
|---|---|
| Can this module, removed from `claw-gateway`, still produce a valid artifact from pure inputs? | **Builder** — lives on the protocol / tool / policy side. |
| Does this module need `approval_store`, `park_store`, blockhash tracking, or JIT rebuild on resume? | **Executor** — lives in `claw-gateway`, private to the control plane. |

Three reasons Part 5A refuses to invent a third category:

1. A third category would be repo-wide, not lending-specific; creating one inside a lending spec would force a reorganisation well outside the V1 `Deposit` / `Repay` slice.
2. `DEBT.md` already tracks the current placement asymmetries under the two-category model (D-1, D-2). Introducing a third category would silently invalidate those trigger conditions.
3. The lending snapshot assembler's impurity is a protocol-integration concern, not a generic cross-cutting one. It fits inside Part 6's protocol-specific surface without needing a new global label.

Where a lending module falls in the §7.1 taxonomy is decided per module below (§40.2 – §40.4), not by inventing a taxonomy that matches the module.

### 40.2 Evaluator placement — builder side

The lending policy **evaluator** (the component that takes `(snapshot, proposed_action)` and produces a `Pass` / `HardBlock` verdict per Part 3B §22 – §29) is a pure function. It satisfies §7.1's builder test:

- Given a `LendingSnapshot` and a `ProposedAction`, it produces a `Verdict` from pure inputs.
- It does not read RPC, DB, cache, subscription streams, or any ambient process state.
- It does not touch `approval_store`, `park_store`, blockhash tracking, signer state, or the daemon runtime.
- It does not mutate the snapshot (Part 2 §12.5: snapshots are frozen).
- Determinism: two evaluations with the same inputs produce bit-identical verdicts (Part 3B §29.4, restated at the boundary level here).

Consequently, the evaluator's placement target is the **policy / risk side** — a module that can be lifted out of `claw-gateway` and still compile and evaluate. Concrete crate / module path is **deferred to Part 5B**; Part 5A commits only to the semantic placement, not its spelling.

The evaluator's placement commitment is single-direction: policy-side code may be consumed by the gateway, but the evaluator MUST NOT take an inbound dependency on any gateway-runtime type (`approval_store`, `park_store`, daemon state, signer). A single such dependency collapses the builder test.

### 40.3 Snapshot assembler placement — protocol-integration side, inside Part 6

The **snapshot assembler** is the component that reads on-chain state and produces a `LendingSnapshot` ready for evaluator consumption. It is impure by construction — it issues RPC reads, handles protocol-native refresh semantics (Part 3A §18.1), and applies protocol-specific raw-layout mapping (Part 6's territory). This impurity means it does not pass §7.1's builder test.

Part 5A's placement stance:

- The assembler is **not** an executor under §7.1. It does not touch `approval_store`, `park_store`, blockhash tracking, pending-state machinery, signer, or any control-plane runtime. It does not hold transaction bytes, it does not rebuild JIT artifacts, it does not participate in completion dispatch.
- The assembler is **not** a pure builder under §7.1. Its need for RPC reads is structural: snapshot freshness requires an actual chain read, not a pure transformation of cached data.
- The assembler's natural home is therefore **Part 6's protocol-specific read-model assembly surface**. Part 5A does not invent a repo-wide label for this ("adapter", "read-model assembler", or similar); it records the functional constraint and leaves the placement specifics to Part 6.

What this means operationally:

- Because the assembler's impurity lives on the protocol-integration side, per-protocol assemblers (Solend assembler, Kamino assembler, etc.) MAY exist as distinct Part 6 modules, each implementing the same boundary contract defined by Part 5 (§42).
- The evaluator MUST NOT import a protocol-specific assembler type. The seam §41 defines is a one-way information flow: the assembler produces a protocol-agnostic `LendingSnapshot`; the evaluator consumes it; no types cross the seam in the reverse direction.

### 40.4 Why the assembler must not land on the executor side

Precisely because the assembler is impure, it would be tempting to place it alongside `claw-gateway`'s other impure components (daemon, orchestrator, signer, completion dispatch). Part 5A explicitly rules this out:

- **No approval / park / pending coupling.** An assembler that reaches into `approval_store` or `park_store` has conflated "read on-chain state" with "read Claw's pending-state machinery." Those are different concerns; mixing them hands the lending policy layer a hidden dependency on gateway runtime invariants.
- **No signer coupling.** The assembler reads; it does not sign. A module that reads on behalf of a signing flow inherits the signing flow's lifecycle (blockhash windows, typestate progression, JIT rebuild triggers). The assembler's lifecycle is bounded by a single snapshot, not by a transaction's multi-step signing path.
- **No JIT / rebuild coupling.** Part 2 §12.5 (snapshots frozen) combined with Part 3B §29 (evaluator input boundary) means the assembler's output is handed off once and never re-entered. It does not rebuild, it does not refresh-mid-evaluation, and it does not subscribe to changes during an evaluation. JIT semantics belong to transaction builders; they do not apply to the lending snapshot.

### 40.5 Module-boundary posture summary

Summary of the placement commitments Part 5A makes:

| Module | §7.1 classification | Allowed dependencies | Forbidden dependencies |
|---|---|---|---|
| Evaluator | Builder (pure) | Part 2 schema types; `ProposedAction`; configured thresholds | Any gateway runtime state (`approval_store`, `park_store`, signer, daemon, blockhash) |
| Snapshot assembler | Neither pure builder nor executor — lives in Part 6's protocol-integration surface | RPC client abstraction; protocol-specific layout decoders (Part 6); protocol-specific refresh semantics (Part 6) | `approval_store`, `park_store`, signer state, daemon runtime, transaction builders, JIT-rebuild machinery |

Both rows restate §7.1's boundary shape without adding a new category to the repo-wide taxonomy.

## 41. The Part 5 / Part 6 Seam

The seam between Part 5 and Part 6 is the single point where protocol-agnostic policy meets protocol-specific integration. Part 5A's responsibility is to make the seam hard — one-way in information flow, stable in contract shape, and unambiguous about which side owns which decision.

### 41.1 What Part 5 owns

- **Assembler contract** — what success and failure look like at the boundary (§42).
- **Evaluator input / output boundary** — what the evaluator accepts, what it returns, what it is forbidden to do (§43, plus Part 3B §29's existing commitments).
- **Handoff semantics** — the sequencing rule that the evaluator consumes a frozen snapshot once and cannot re-enter the assembler mid-evaluation (§43.2).
- **Unit-tagging principle** — boundary-level insistence that numeric values crossing the seam carry a compile-time-distinguishable unit (§44).

Everything Part 5 owns is protocol-agnostic. A Part 5 decision that only makes sense for Solend, or only for Kamino, is a seam leak.

### 41.2 What Part 6 owns

- **Protocol choice.** Which lending protocol V1 integrates with first is not a Part 5 decision.
- **Raw account-layout mapping.** How a Solend `Obligation` or a Kamino equivalent is decoded into Part 2 schema fields lives entirely on the Part 6 side.
- **Oracle binary decoding.** Pyth Lazer payload parse, Switchboard On-Demand payload parse, `publish_timestamp` / `price_value` extraction — all Part 6.
- **Protocol-native stale-marker interpretation.** How the assembler decides whether `Obligation.last_update.stale = true` should surface as "assembly failure" or "assemble-then-refresh-once-then-assemble" is Part 6 policy per protocol, constrained by Part 3A §18.1's fail-closed stance.
- **Protocol-specific refresh / preconditioning.** Issuing `RefreshObligation` / `RefreshReserve` (or each protocol's equivalent) before a read is Part 6. Part 5A does not commit to whether refresh happens, where, or how often; it only commits that if refresh happens, it happens **inside the assembler, before handoff**, never during evaluation (§43.2).
- **Per-protocol defaults.** What `MaxOracleStalenessMs` threshold is reasonable for a given protocol's oracle cadence is a Part 6 input into operator configuration, not a Part 5 contract.

Everything Part 6 owns is free to vary across protocols. A Part 6 decision that tries to set a cross-protocol rule belongs on the Part 5 side of the seam.

### 41.3 Seam discipline

Three rules preserve the seam:

1. **One-way information flow.** The assembler produces a `LendingSnapshot` (Part 2 schema, protocol-agnostic). The evaluator consumes it. No Part 6 types appear in the evaluator's signature, in its internal representations, or in the `Verdict` it produces. If a protocol-specific detail matters to policy, it is surfaced through an already-defined Part 2 schema field, not through a side channel.
2. **No Part 5 file imports a Part 6 module directly.** The evaluator and any Part 5 helper depend on Part 2 schema types. Part 6 modules depend on Part 2 schema types. Neither imports the other. Composition happens at the outermost call site (Part 5B / gateway wiring), not at the spec surface.
3. **Contract changes go through the seam definition.** Adding a field to the seam (e.g., a new `LendingSnapshot` field) is a Part 2 spec change. Changing how a protocol populates that field is a Part 6 change. The two cannot be collapsed into one PR without re-opening the part that was changed first.

### 41.4 Non-seams (intentional)

Two things that could look like seams are **not**:

- The **metrics layer** (Parts 4A / 4B) is not part of the Part 5 / Part 6 seam. It is a protocol-agnostic pure function inside Part 5's territory and, per Part 4A §33.1, produces nothing in V1. Future metrics (Part 4B §35) will cross the seam only as consumers of Part 2 schema fields, not as separate protocol-specific abstractions.
- The **snapshot's oracle feed set** (Part 2 §11.1) is not a Part 6 escape hatch. Protocol-specific oracle binaries are decoded on the Part 6 side into the protocol-agnostic `OracleFeed` / `OracleFeedSet` shape; policy rules evaluate against that shape, not against protocol-specific payload types.

## 42. Assembler Contract

The assembler contract fixes what success and failure look like at the boundary, without specifying how either is achieved.

### 42.1 Success output

On success, the assembler returns a **complete, freshness-aware, evaluable `LendingSnapshot`** as defined by Part 2:

- All required components populated (Part 2 §8, §14 completeness invariants).
- Per-component and snapshot-level freshness descriptors present (Part 2 §12.1, §12.2, §12.4: unknown freshness is unrepresentable).
- Numeric fields carry unit tags (§10.1 / §44).
- Oracle pricing expressed as `OracleFeedSet` per priced asset (§11.1); sentinel slots excluded at assembly time.
- Protocol-native stale markers surfaced verbatim in the schema where they exist; the decision about whether `stale = true` is acceptable at the rule evaluator is §18.1's call, not the assembler's.

The assembler does **not** pre-evaluate any rule, does **not** produce a `Verdict`, and does **not** compute derived metrics. Its output is a snapshot that is *capable of being evaluated*, not one that has been evaluated.

### 42.2 Failure output

On failure, the assembler returns a **pre-evaluation failure** — a single outcome representing "no valid snapshot was produced." The evaluator never sees this outcome; the caller at the outer boundary (Part 5B / gateway wiring) translates it into a `HardBlock` per Part 1 §5's fail-closed default.

Part 5A deliberately does **not**:

- Define the concrete shape of this failure value (is it an enum, a `Result::Err`, a variant?). Part 5B.
- Enumerate failure sub-categories (RPC unreachable, decoder mismatch, protocol-native stale marker TRUE, oracle binary parse error, etc.). Each protocol's Part 6 mapping may surface its own categories; Part 5A only commits that all such categories collapse, at the seam, into "no snapshot." Observability-grade diagnostic richness is explicitly future scope (Part 3B §28.6).
- Commit to whether the failure is retryable. Retry is not a V1 policy decision (Part 1 §5.5); the caller's response to a pre-evaluation failure is fail-closed.

### 42.3 What the assembler contract refuses to decide

- **No retry policy.** The assembler returns success or failure once per call. Multi-call retry sequencing, if it exists at all, is gateway-wiring concern (Part 5B).
- **No cache policy.** Whether assembler output is cached, for how long, and with what invalidation is a Part 5B / Part 6 decision; Part 5A does not commit to cache existence.
- **No TTL.** Snapshot freshness thresholds are Part 3A rule inputs (§18.1, §18.2); they are not the assembler's responsibility.
- **No refresh strategy.** Whether the assembler issues a `RefreshObligation` before reading, after a first read, or never, is Part 6 per-protocol policy — see §41.2.
- **No partial snapshots.** Part 2 §14 forbids them; the assembler cannot expose one as a "best effort" output.

### 42.4 Contract stability requirement

The assembler contract's success shape is the `LendingSnapshot` defined by Part 2. Widening the assembler contract (adding a new required field, adding a new schema component, adding a new freshness dimension) is a Part 2 change, not a Part 5A / Part 5B change. This keeps the seam from drifting as new protocols are added: each protocol's Part 6 assembler satisfies the same Part 2 schema, not a growing list of side-channel requirements.

## 43. Evaluator Handoff

The handoff is the single transition from "reading on-chain state" to "evaluating policy." Once it happens, nothing reads chain state again until a new snapshot is assembled.

### 43.1 What the evaluator receives

The evaluator receives:

- A **complete `LendingSnapshot`** that has already passed Part 2's structural completeness checks (the assembler would not have returned success otherwise).
- A **`ProposedAction`** for one of V1's allowed action types (`Deposit` or `Repay`; all other action types are rejected at Part 3B §25's Scope Boundary Gate before any snapshot-dependent work runs).
- Evaluator configuration (rule thresholds from Part 3A §18.1 – §18.5).

The snapshot has **already passed the Part 3B §24 System Invariant Gate** at or before handoff. Concretely: presence / size / owner-program checks on the snapshot's component accounts are a precondition for the assembler returning success, and the evaluator does not re-verify them. Where the System Invariant Gate should run physically (inside the assembler's final step, at the gateway-wiring call site between assembler and evaluator, or as a standalone check) is a Part 5B decision; Part 5A only commits that it runs **before** the evaluator sees the snapshot.

### 43.2 What the evaluator is forbidden to do

Restating and tightening Part 3B §29 at the boundary level:

- **No RPC.** No `get_account_info`, no `get_slot`, no subscription read, no out-of-band state fetch.
- **No cache read.** Even if a cache exists in the process (a Part 5B / Part 6 concern), the evaluator does not consult it.
- **No assembler re-invocation mid-evaluation.** If evaluation needs data not in the snapshot, evaluation fails closed — it does not ask the assembler for more. The only path to a fresh input is to discard the current attempt and assemble a new snapshot, then restart the full gate sequence.
- **No DB / store access.** No `approval_store`, `park_store`, persona state, session store, signer state. These do not cross the seam.
- **No clock reads beyond a single snapshot-level timestamp.** Where a rule compares "now" against a snapshot field (e.g., `MaxOracleStalenessMs`), "now" MUST be the snapshot-level freshness descriptor's `fetched_at`, not a fresh wall-clock read at evaluation time. Part 2 §12.5 (snapshots frozen) combined with Part 3B §29.4 (determinism) already requires this; §43.2 restates it for the placement boundary.
- **No mutation.** The evaluator does not write anything, does not update any state, does not emit side effects other than its `Verdict`.

### 43.3 What `Pass` means at the handoff level

A `Pass` verdict from the evaluator means **only**:

- All System Invariant, Scope Boundary, and Rule Evaluation checks have cleared for this `(snapshot, proposed_action)` pair.
- The action is eligible to proceed to the next pipeline stage.

A `Pass` does **not**:

- Build a transaction. Transaction building for a lending action is a Part 6 / Part 5B concern downstream of the evaluator; the evaluator itself never produces transaction bytes.
- Sign anything. Signing remains the existing Claw signing pipeline's responsibility.
- Guarantee the action will succeed on chain. Between the snapshot and the transaction's eventual execution, chain state may move; that is the existing gateway's concern (retry / blockhash / JIT), not the evaluator's.
- Extend the snapshot's freshness guarantee. If downstream wiring takes too long to build or submit a transaction, a new snapshot assembly + evaluator run may be required; policy for this is a Part 5B / gateway-wiring decision.

### 43.4 Handoff atomicity

The handoff is not a protocol step; it is a single call-site invariant. In the outermost pipeline:

1. Assembler returns `LendingSnapshot` or pre-evaluation failure.
2. On failure, outer wiring HardBlocks, no evaluator call.
3. On success, System Invariant Gate runs (location per §43.1 commitment).
4. On invariant failure, evaluator call is skipped and outer wiring produces the appropriate `HardBlock`.
5. On invariant pass, the evaluator is called exactly once with the snapshot and action.
6. The evaluator's `Verdict` is the caller's output; the caller does not loop, re-call, or refresh.

Whether steps 2–5 are implemented as a single function, a pipeline of small functions, or a typestate machine is a Part 5B choice. Part 5A only commits to the sequencing.

## 44. Unit-Tagging Principle

Part 4A §10.1 established unit tags as a schema invariant. Part 5A restates this at the implementation-boundary level as a **compile-time** requirement, without specifying Rust shape.

### 44.1 The principle

- Numeric values crossing the seam (both directions) MUST carry a compile-time-distinguishable unit.
- "Untagged numeric" MUST be unrepresentable in the types that appear in the assembler output, the evaluator input, and any rule-configuration surface consumed at evaluation time.
- A function signature that takes `u64` / `u128` / floating-point without a unit wrapper is a seam violation at the type level, caught at compile time.

### 44.2 What Part 5A does NOT do

- **No concrete newtypes listed.** Part 5A does not name `UnderlyingAmount`, `WadAmount`, `CollateralTokenAmount`, `ChainSlot`, `SlotOffset`, or any other concrete type. Naming and concrete shape are Part 5B decisions.
- **No choice between approaches.** Whether the unit is carried by distinct newtype wrappers, by a `(value, unit_enum)` pair, by `PhantomData` parameters, or by any other Rust-level encoding is a Part 5B decision. Part 5A only requires that whatever is chosen passes the compile-time-distinguishability test.
- **No unit catalogue.** The minimum set defined at §10.1 (underlying / collateral / wad) plus any protocol-specific domains are the schema's concern. Part 5A does not extend the catalogue; Part 5B will select which Rust-level encoding expresses each.

### 44.3 Why this belongs in Part 5A, not only in §10.1

§10.1 is a schema invariant — a property of the snapshot's abstract structure. Part 5A is the first place where the invariant becomes operational: when Rust types appear at the seam (in Part 5B), the seam MUST refuse to compile against an untagged numeric. Stating the requirement in Part 5A prevents Part 5B from selecting a Rust shape that satisfies the schema on paper (tagged fields inside structs) but fails at call-site safety (untagged arithmetic leaks across the seam).

## 45. What Part 5A Refuses to Do

Part 5A is a placement contract. It deliberately does **not**:

- **Write Rust types.** No newtypes, no enums, no trait signatures. Part 5B.
- **Pick concrete crate / module paths.** No commitment to "evaluator lives in `claw-policy`" or "assembler lives in `claw-lending-adapter`." Part 5B.
- **Select a protocol.** Solend, Kamino, MarginFi, port.finance — none are chosen by Part 5A. Part 6.
- **Write raw account-layout mapping.** How any protocol's bytes become `LendingSnapshot` fields is Part 6.
- **Write oracle binary decoders.** Pyth Lazer parse, Switchboard On-Demand parse — Part 6.
- **Design the evaluator API surface.** Invocation shape, configuration passing, error-propagation style, async vs sync — Part 5B.
- **Design the transaction builder.** Building a `Deposit` or `Repay` transaction is downstream of `Pass` and is a Part 6 / builder-side concern that follows the existing Jupiter-reference shape; Part 5A does not restate that pattern and does not commit to where the builder lives in the crate graph.
- **Write the V1 control-flow call graph in detail.** Part 5A records the sequencing invariants (§43.4) without describing the functions that implement them.
- **Invent placeholder modules, traits, or enums for future metrics or future rules.** A `LendingRule` trait with one implementor per V1 rule is premature; the five V1 rules may be expressed as plain functions and Part 5B will decide. No `FutureMetric` enum, no `ProtocolAdapter` trait hierarchy, no `OracleAggregator` trait — none of these are authorised by Part 5A, and Part 5B should not introduce them either unless a concrete second implementor exists.
- **Re-open the builder / executor taxonomy.** §7.1 stands. §40.1 restates this at the lending-spec level.
- **Authorize implementation to start.** Part 5A closes the placement question; a separate spec prompt (Part 5B) authorizes Rust types, and a separate spec prompt (Part 6) authorizes protocol-specific work. Neither is authorized by this part.

The refusal list is load-bearing: without it, a future reader could read Part 5A as permission to begin implementation. It is not. It is permission to begin *writing Part 5B* without fear that implementation will land in the wrong place.

---

Part 5A closes placement and seam. It does not close implementation. Parts 5B (V1 Rust shape), 6 (protocol-specific implementation), and any future Part 5C (cache / refresh / wiring) remain gated by their own spec prompts. The runway is narrow, but it is now clear.

---

# Part 5B — V1 Types and Control Flow

**Status:** draft — Part 5B of N
**Scope of Part 5B:** conceptual Rust-like shapes for V1 `LendingSnapshot`, `ProposedAction`, rule config, evaluator API, and the `Deposit` / `Repay` control flow; the minimum newtype set required to make §44's compile-time unit invariant enforceable; the physical placement of the System Invariant Gate between assembler success and evaluator entry.
**Out of scope for Part 5B (explicit):** actual Rust source; serde / database / wire / IPC formats; final crate names and module paths; protocol choice; raw account-layout mapping; oracle binary decoding; transaction-builder implementation; cache / TTL / retry / refresh policy; future metrics / rules / action types; error-propagation spelling (enum vs typestate vs `Result<…, E>`); async vs sync choice.

Part 5B inherits every frozen decision from Parts 1 / 2 / 3A / 3B / 4A / 4B / 5A. It does not reopen any of them. Code blocks below that resemble Rust are **conceptual only** — they fix the *shape* of the seam, not the Rust spelling. A vertical-slice implementation MAY deviate on names, visibility, module placement, derive lists, and error-propagation style, provided it satisfies the shape and invariants recorded here.

## 46. V1 Type Shape Overview

Part 5B's purpose is to make the V1 `Deposit` / `Repay` slice implementation-shaped without making it implementation-started. It does this by fixing four things:

- The minimum newtype set that satisfies §44's compile-time unit invariant (§47).
- The abstract shape of the three seam types — `LendingSnapshot`, `ProposedAction`, rule config (§48, §49, §50).
- The physical location of the System Invariant Gate between assembler and evaluator (§51).
- The evaluator's API surface as a pure function over those three types plus a non-ambient evaluation context (§52).

Part 5B does NOT fix:

- **Final Rust crate / module paths.** Part 5A §40.2 fixed the semantic placement; Part 5B does not choose the literal `mod` name, the crate split, or the `use` path.
- **Serialization.** The evaluator operates on in-memory values. Any serde / database / wire-format choice belongs to outer wiring and is not part of the seam.
- **Error-propagation style.** Conceptual signatures below use `Result<…, HardBlockReason>` or `Verdict` return types for readability; the vertical slice picks the Rust spelling (`anyhow` vs concrete enum vs typestate).
- **Async vs sync.** Whichever is simpler for the vertical slice is acceptable; the evaluator is pure and thus trivially `Send + Sync` regardless.
- **Types whose sole consumer is a future rule or future metric.** Part 4A §33.1's empty V1 metrics set applies to types too.

Code-shaped fragments in §47 – §53 are fenced as `conceptual`. A reviewer should read them as "this is what the seam requires," not "this is what the Rust file will say on day one."

## 47. Numeric and Freshness Newtypes

V1 introduces exactly the newtypes required to make the five mandatory rules (Part 3A §18) and the assembler / evaluator seam (Part 5A §44) type-safe. No more.

### 47.1 The V1 newtype set

```rust,conceptual
/// Base units of the SPL mint (e.g., USDC at 6 decimals, SOL lamports at 9).
/// Used for: Deposit/Repay action inputs, reserve `available_amount`,
/// `MaxActionInputAmount` threshold configuration.
pub struct UnderlyingAmount(u64);

/// Protocol collateral-token / c-token units. Not convertible to
/// `UnderlyingAmount` without a protocol-specific exchange rate (Part 6).
/// Exists specifically to make c-token ↔ underlying confusion unrepresentable
/// at the seam. Used for: Obligation deposit entries.
pub struct CollateralTokenAmount(u64);

/// High-precision fixed-point amount in the protocol's wad domain
/// (typically 10^18-scaled Decimal — Solend's borrow math uses this).
/// 128-bit width because observed wad-scaled borrow values on mainnet
/// (see §30.5 spike evidence) exceed 64 bits. Used for: Obligation borrow
/// entries, reserve-level accumulators carried verbatim per §10.
pub struct WadAmount(u128);

/// Solana chain slot. Opaque integer identity of a slot; not a duration,
/// not a millisecond count, not an amount.
pub struct ChainSlot(u64);

/// Millisecond-scale duration. Used for staleness thresholds
/// (`max_fetch_age`, `max_publish_age`) and for any duration arithmetic
/// that crosses the seam. Paired with `ChainSlot` through an explicit
/// slot-to-duration conversion (§51.4) — there is no implicit conversion.
pub struct DurationMs(u64);
```

Five newtypes. That is the entire V1 set.

### 47.2 Explicitly refused newtypes in V1

- **`UsdValue`** — would require oracle decoders + feed-set aggregation (§34.3). V1 has no USD-denominated rule (Part 4A §32.6). Deferred to Part 4B / Part 6 prerequisites.
- **`BasisPoints`** — no V1 rule takes a bp-scaled input. Reserve risk parameters (loan-to-value ratio, liquidation threshold) are carried verbatim per §10 but are not consumed by V1 rules; adding a newtype for them now is premature.
- **`Amount<U>` or any generic-over-unit container** — explicitly refused per §46. The V1 set is five specific newtypes, not a family.
- **`Price`, `PriceExponent`, `ConfidenceInterval`** — oracle price representation is protocol-specific binary-format territory (Part 6). V1 does not consume prices.
- **Ratio / percentage newtypes** — no V1 rule consumer.

### 47.3 What the newtypes enforce at compile time

- An `UnderlyingAmount` cannot be added to a `WadAmount`, compared to a `CollateralTokenAmount`, or assigned from a bare `u64` without an explicit constructor call. The constructor is the single conversion site; silent conversions are not permitted.
- A `DurationMs` cannot be compared directly to a `ChainSlot`. Rules that compare an age in milliseconds against a slot difference do so through a single named conversion function whose signature declares both domains (§51.4).
- A function signature that takes `u64` or `u128` for a quantity that should be one of the five newtypes is a seam violation at the type level.

### 47.4 What the newtypes do NOT commit to

- **Arithmetic trait derivation** (`Add`, `Sub`, `Mul`, `Div`). V1 rules use `==`, `<=`, and subtraction within a single unit. Which traits the vertical slice derives is an implementation call; Part 5B records only the non-confusion requirement.
- **Serde / wire format.** Evaluator inputs are in-memory; outer wiring decides if and how it serializes.
- **Overflow semantics** (`checked_add` vs saturating vs panic). V1 arithmetic operates on snapshot-derived values that fit in their declared widths by construction; the vertical slice picks a policy.
- **Zero-value handling.** Where "zero" has policy meaning (an empty obligation, an empty feed set), that meaning lives in the snapshot shape (§48), not in the newtype.

### 47.5 Wall-clock representation is deferred

Part 2 §12.2 requires freshness descriptors to carry a `fetched_at` wall-clock moment (UTC, millisecond resolution or better). V1's rule evaluator does **not** compute with wall-clock values — all V1 staleness arithmetic runs in `ChainSlot` + `DurationMs` via §51.4's conservative slot-duration constant. The wall-clock field is therefore:

- Present in the snapshot (per Part 2 §12.2), carried for audit / trace.
- Not consumed by any V1 rule.
- Its Rust-level spelling (a sixth newtype, an opaque `u64`, an `Instant`) is deferred to vertical-slice implementation, because no V1 comparison exercises it.

A vertical slice that wants to introduce a `WallClockMs` newtype to hold this field is permitted; a vertical slice that wants to hold it as a tagged wrapper inside `FreshnessContext` is permitted; neither decision is Part 5B's to make.

## 48. Snapshot Shape

`LendingSnapshot` is the frozen, freshness-aware read model the evaluator consumes. The shape below is conceptual — it records what is and is not representable, not the final Rust spelling.

### 48.1 Top-level shape

```rust,conceptual
pub struct LendingSnapshot {
    pub protocol_tag: ProtocolTag,          // Part 2 §8 attribution
    pub session_wallet: Pubkey,             // owner the snapshot was assembled for
    pub fetched_at: SnapshotFreshness,      // §48.5 — required-present
    pub obligation: Obligation,             // §48.2 — required-present
    pub reserves: ReserveSet,               // §48.3 — may be empty-but-valid
    pub oracles: OracleFeedSetMap,          // §48.4 — may be empty-but-valid
}
```

**No field is `Option<T>`.** Missing required data is not representable as `None`; it is a pre-evaluation failure produced by the assembler (Part 5A §42.2), and the evaluator never sees it. This is §48.6's test, restated in one sentence.

### 48.2 Obligation

```rust,conceptual
pub struct Obligation {
    pub identifier: Pubkey,                    // obligation account pubkey
    pub owner: Pubkey,                         // Part 2 §9 obligation owner
    pub protocol_last_updated_slot: ChainSlot,
    pub protocol_native_stale: StaleMarker,    // Fresh | Stale — verbatim from protocol
    pub deposits: Vec<ObligationDeposit>,
    pub borrows: Vec<ObligationBorrow>,
    pub freshness: ComponentFreshness,
}

pub struct ObligationDeposit {
    pub reserve: Pubkey,                       // reference into ReserveSet
    pub deposited: CollateralTokenAmount,      // §47.1 — c-token units
}

pub struct ObligationBorrow {
    pub reserve: Pubkey,
    pub borrowed_wads: WadAmount,              // §47.1 — wad-scaled
}
```

`deposits: Vec<…>` and `borrows: Vec<…>` MAY be empty; empty-but-valid is distinct from missing. An obligation with zero deposits is a legal input — a `Deposit` rule evaluates against it without special-casing. `Obligation` itself is always present; the snapshot has no "no obligation" state.

### 48.3 Reserves

```rust,conceptual
pub struct ReserveSet { … }    // conceptually keyed by Pubkey; exact collection choice is vertical-slice

pub struct Reserve {
    pub identifier: Pubkey,
    pub mint: Pubkey,
    pub mint_decimals: u8,                     // part of the mint's identity; not a unit-bearing quantity
    pub protocol_last_updated_slot: ChainSlot,
    pub protocol_native_stale: StaleMarker,
    pub lifecycle: ReserveLifecycleMarkers,    // paused / disabled / supply_capped / borrow_capped
    pub freshness: ComponentFreshness,
    // Part 2 §10 verbatim fields (interest model, utilization, risk parameters)
    // are carried here; V1 rules do not consume them, so Part 5B does not
    // enumerate their shape beyond "each numeric field is a newtype from §47
    // or a protocol-carried tagged variant."
}
```

`ReserveSet` MAY be empty — valid only when the obligation has neither deposits nor borrows and the action does not reference a reserve. Where a V1 rule requires a specific reserve (e.g., `AllowedMints` cross-referencing the action's `reserve_mint` against the snapshot), absence is resolved by the rule as a HardBlock, not by the snapshot shape.

### 48.4 Oracles

```rust,conceptual
pub struct OracleFeedSetMap { … }    // keyed by priced-asset mint; conceptual collection

pub struct OracleFeedSet {
    pub priced_asset: Pubkey,
    pub quote: QuoteAsset,                     // explicit; typically USD, never assumed — Part 2 §11
    pub feeds: Vec<OracleFeed>,                // sentinel slots excluded at assembly time — §11.1
}

pub struct OracleFeed {
    pub identifier: Pubkey,
    pub provider: OracleProvider,              // PythLazer | SwitchboardOnDemand | … (closed enum)
    pub publish: FeedPublishFreshness,         // §48.5
    pub fetch: ComponentFreshness,
    pub status: OracleStatusMarkers,           // surfaced verbatim per §11.2; not interpreted at Part 5
    // `price_value` and `confidence_interval` are intentionally absent:
    // V1 has no rule consumer (Part 4A §32.2 verdict), and extracting them
    // requires Part 6 decoders. Pulling them across the seam now would
    // violate §41's one-way information flow.
}
```

An `OracleFeedSet` with `feeds: []` is a legal representation of "no feed configured for this priced asset." V1 responds only when a rule that consumes oracle fields actually runs (only `MaxOracleStalenessMs`); the response is a rule-level HardBlock (§52), not a snapshot-shape failure.

### 48.5 Freshness descriptors

```rust,conceptual
pub struct SnapshotFreshness {
    pub observed_slot: ChainSlot,              // RPC-reported current slot at snapshot assembly
    pub source_context: FreshnessContext,      // audit / trace — opaque to rules
    // Wall-clock `fetched_at` per Part 2 §12.2 is carried for audit; its
    // Rust spelling is deferred per §47.5. V1 rules do not compute with it.
}

pub struct ComponentFreshness {
    pub observed_slot: ChainSlot,              // RPC-reported current slot at this component's fetch
    pub source_context: FreshnessContext,
    // Wall-clock as above.
}

pub struct FeedPublishFreshness {
    pub publish_slot: Option<ChainSlot>,       // oracle-self-reported publish slot (if available)
    // Wall-clock publish_timestamp per Part 2 §12.2 is a Part 2-level concept
    // but requires oracle binary decoding (Part 4A §32.2 / Part 6). V1 rule
    // arithmetic consumes publish_slot only.
}
```

`SnapshotFreshness` and `ComponentFreshness` are required-present (no `Option`). `FeedPublishFreshness` uses `Option<ChainSlot>` because some oracle binary formats expose `publish_slot` while others may expose only wall-clock; this is the single place `Option` appears in the snapshot, and it encodes "this particular dimension was not published," not "missing data." A V1 rule (specifically §18.2 `MaxOracleStalenessMs`) responds to `None` by HardBlocking per fail-closed default.

### 48.6 Valid-empty vs invalid-missing

| Field | Valid empty? | Invalid-missing behaviour |
|---|---|---|
| `LendingSnapshot.protocol_tag` | No | Assembler fails — pre-evaluation HardBlock |
| `LendingSnapshot.session_wallet` | No | Assembler fails |
| `LendingSnapshot.fetched_at` | No | Assembler fails |
| `LendingSnapshot.obligation` | No | Assembler fails |
| `Obligation.deposits` | Yes (empty `Vec` OK) | N/A |
| `Obligation.borrows` | Yes (empty `Vec` OK) | N/A |
| `LendingSnapshot.reserves` | Yes (empty `ReserveSet` OK) | N/A |
| `LendingSnapshot.oracles` | Yes (empty `OracleFeedSetMap` OK) | N/A |
| `OracleFeedSet.feeds` | Yes (empty `Vec` OK — rule may HardBlock) | N/A |
| `ComponentFreshness.observed_slot` | No | Assembler fails |
| `FeedPublishFreshness.publish_slot` | N/A (`Option`) | Rule-level HardBlock if a rule consumes it |

The table is the reviewer's test: any "invalid-missing" row means the field must NOT be `Option<T>` in the shape; any "valid empty" row means an empty collection is a legal value the evaluator handles without special-case branching.

## 49. ProposedAction Shape

### 49.1 V1 action shape

```rust,conceptual
pub enum ProposedAction {
    Deposit {
        protocol: ProtocolTag,
        reserve_mint: Pubkey,                  // mint of the asset being supplied
        amount: UnderlyingAmount,
    },
    Repay {
        protocol: ProtocolTag,
        reserve_mint: Pubkey,                  // mint of the debt being reduced
        amount: UnderlyingAmount,
    },
}
```

Two variants, both carrying the minimum V1 rule inputs (Part 3A §18.3, §18.4, §18.5). Neither variant carries a signer pubkey or wallet pubkey — the signer identity comes from the session-bound wallet seam (§49.3).

### 49.2 Absent variants — enforced at the type level

`Withdraw` and `Borrow` are **absent** from the enum. Part 1 §3.2 and Part 3B §25's Scope Boundary Gate make them unreachable at the semantic level; Part 5B makes them unreachable at the type level by not representing them as variants.

**Documentation of future variants goes in a future spec prompt, not in the V1 enum.** A `#[doc(hidden)]` or otherwise-disabled variant in the V1 type is not acceptable: a variant that exists in the type — even disabled — leaks the possibility that some call site could construct it. If a user / LLM expresses intent to `Withdraw` or `Borrow`, resolution happens upstream (intent → action translation) and yields a Scope Boundary HardBlock **before** a `ProposedAction` is ever constructed.

### 49.3 No free-form signer input

The action does not take a `signer_pubkey` or `wallet_pubkey` field. V1's A2-Execute evidence (§30.5, project memory `project_a2_execute_proof`) established that the signer identity must come from the `SessionBoundWallet` seam to block LLM-supplied wallet override. Part 5B locks this at the type level: a `ProposedAction` variant carrying a free-form signer field is a seam violation caught at compile time.

The `session_wallet` ends up on the snapshot (`LendingSnapshot.session_wallet`) and is cross-referenced by the System Invariant Gate (§51.2) against the session binding passed separately through the evaluation context — this closes the loop without giving the action a signer field.

### 49.4 `ProtocolTag` agreement with the snapshot

Both `ProposedAction` and `LendingSnapshot` carry a `ProtocolTag`. The System Invariant Gate checks equality; mismatch is a HardBlock before the evaluator runs. `ProtocolTag` is a closed enum whose V1 variants enumerate only protocols V1 supports — not a free-form string, and not pre-populated with future protocols.

## 50. V1 Rule Config Shape

### 50.1 Closed config, no rule trait

V1 rule config is a single closed struct, not a registry of trait-object rule handlers.

```rust,conceptual
pub struct LendingRuleConfig {
    pub require_fresh_state: RequireFreshStateConfig,
    pub max_oracle_staleness: MaxOracleStalenessConfig,
    pub allowed_lending_protocols: AllowedLendingProtocolsConfig,
    pub allowed_mints: AllowedMintsConfig,
    pub max_action_input_amount: MaxActionInputAmountConfig,
}

pub struct RequireFreshStateConfig {
    pub max_fetch_age: DurationMs,
    // Protocol-native stale-marker handling is not a knob — per §18.1, any
    // `StaleMarker::Stale` is a HardBlock regardless of this threshold.
}

pub struct MaxOracleStalenessConfig {
    pub max_publish_age: DurationMs,
}

pub struct AllowedLendingProtocolsConfig {
    pub allowlist: Vec<ProtocolTag>,           // closed enum — no strings
}

pub struct AllowedMintsConfig {
    pub allowlist: Vec<Pubkey>,
}

pub struct MaxActionInputAmountConfig {
    pub per_mint_caps: Vec<(Pubkey, UnderlyingAmount)>,
}
```

### 50.2 No `LendingRule` trait

A `LendingRule` trait that implementations plug into a dispatcher is refused for V1. The five rules are called as plain functions inside a deterministic dispatcher (§52.3). Trait-object dispatch is appropriate when many implementations are swapped at runtime; V1 has five fixed rules known at compile time.

When a sixth rule eventually lands, the decision "add a sixth field + sixth call site" vs. "introduce a trait" is the decision point — not today, with five.

### 50.3 Each rule's inputs

| Rule | Snapshot inputs | Action inputs | Config inputs |
|---|---|---|---|
| `RequireFreshState` | `fetched_at.observed_slot`; every `ComponentFreshness.observed_slot` (obligation, each reserve); every `protocol_native_stale` | — | `max_fetch_age: DurationMs` |
| `MaxOracleStalenessMs` | every `OracleFeed.publish.publish_slot`; `fetched_at.observed_slot` | — | `max_publish_age: DurationMs` |
| `AllowedLendingProtocols` | `protocol_tag` | `protocol` | `allowlist: Vec<ProtocolTag>` |
| `AllowedMints` | `obligation.deposits[].reserve`; `obligation.borrows[].reserve`; `reserves[].mint` | `reserve_mint` | `allowlist: Vec<Pubkey>` |
| `MaxActionInputAmount` | — | `reserve_mint`, `amount` | `per_mint_caps: Vec<(Pubkey, UnderlyingAmount)>` |

Every input either appears verbatim in the snapshot or in the action. No rule consumes a derived metric. This restates Part 4A §32.6 at the type level: the V1 metrics set is empty because the table closes without a "derived" column.

## 51. System Invariant Gate Placement

### 51.1 Physical location

The System Invariant Gate runs **after the assembler returns success** and **before the evaluator is invoked**. It is NOT part of the assembler (whose contract stops at "complete, evaluable `LendingSnapshot`" per Part 5A §42.1) and NOT part of the evaluator (which assumes its checks have already passed — Part 3B §24 / Part 5A §43.1).

```rust,conceptual
pub fn system_invariant_gate(
    snapshot: &LendingSnapshot,
    session_wallet: Pubkey,
    action: &ProposedAction,
) -> Result<(), HardBlockReason>;
```

The gate is pure: no I/O, no RPC, no clock, no ambient state. It operates entirely on values already carried by the snapshot and the two non-ambient inputs (session binding, action).

### 51.2 What the gate checks

- **Owner check.** `snapshot.session_wallet == session_wallet`, where `session_wallet` is bound by the `SessionBoundWallet` seam and passed through evaluation context — not supplied by user / LLM. Mismatch ⇒ HardBlock.
- **Protocol-tag agreement.** `snapshot.protocol_tag == action.protocol_tag()`. Mismatch ⇒ HardBlock.
- **Snapshot completeness.** Every field Part 2 §14 / §48.6 marks required-present is in fact present (tautological at the type level given §48.1's no-`Option` rule, but restated here as a structural precondition downstream rules depend on).
- **Freshness-metadata presence.** Every `SnapshotFreshness` / `ComponentFreshness` slot is populated (also tautological at the type level; restated for completeness).

Explicitly NOT in this gate:

- **Freshness-threshold comparisons.** Those belong to `RequireFreshState` (§18.1) and run inside the rule evaluator. The gate checks *presence*, not *age*.
- **Protocol-native stale-marker interpretation.** The bit is consumed by `RequireFreshState`, not by the gate.
- **Oracle publish-age.** Consumed by `MaxOracleStalenessMs`, not the gate.

### 51.3 Where the gate physically lives

Part 5A §43.1 deferred this placement. Part 5B commits:

- The gate is a standalone pure function (shape above).
- It lives on the **same side of the seam as the evaluator** (policy/risk builder side per §7.1) because it is a pure function over `(snapshot, session_wallet, action)` with no protocol-specific knowledge.
- Outer wiring (the call site that owns the vertical slice's top-level flow — a gateway-adjacent module) composes `assembler → gate → evaluator` sequentially.

This placement satisfies §7.1's builder test for the gate.

### 51.4 Freshness comparisons need explicit, non-ambient inputs

Part 5B ruling: **the evaluator and the gate MUST NOT read wall-clock or slot from RPC or ambient state.** Where `RequireFreshState` or `MaxOracleStalenessMs` need a "now," the "now" is the **snapshot-level field** (`snapshot.fetched_at.observed_slot`), not an ambient read.

Concretely, staleness comparisons are slot-based:

```rust,conceptual
// Component age relative to the snapshot-level clock:
let age_slots = snapshot.fetched_at.observed_slot
    .saturating_sub(component.freshness.observed_slot);
let age_ms = slots_to_duration_ms_conservative(age_slots);
if age_ms > config.max_fetch_age { return Err(HardBlock(...)); }
```

`slots_to_duration_ms_conservative` is a single named conversion function that uses a conservative slot-duration constant chosen at V1 time. §39.5 records this as an open question; V1's conservative constant suffices. Where the constant lives (inside the evaluator, inside a small utility module) is vertical-slice decision.

**No `EvaluationContext` may carry a fresh wall-clock handle, `tokio::time::Instant`, RPC client, or cache.** If a rule wanted a clock not already in the snapshot, that would be a seam violation, caught at the `evaluate(…)` signature.

## 52. Evaluator API Surface

### 52.1 Signature (conceptual)

```rust,conceptual
pub fn evaluate(
    snapshot: &LendingSnapshot,
    action: &ProposedAction,
    config: &LendingRuleConfig,
    context: &EvaluationContext,
) -> Verdict;

pub enum Verdict {
    Pass,
    HardBlock(HardBlockReason),
}

pub enum HardBlockReason {
    SystemInvariantFailed(/* subcategory — Part 5B does not enumerate */),
    ScopeBoundary(/* subcategory — Part 5B does not enumerate */),
    RuleRejected(/* which rule + rule-level reason — Part 5B does not enumerate */),
}
```

The three `HardBlockReason` variants match Part 3B §28.1 exactly. Subcategory enumeration is deferred per §28.4 and §30.2; the vertical slice fills them in when it first needs them.

### 52.2 `EvaluationContext` — named, non-extensible input boundary

```rust,conceptual
pub struct EvaluationContext {
    pub session_wallet: Pubkey,    // cross-reference target for §51.2's owner check
    // NOTHING ELSE. Explicitly forbidden:
    //   - RpcClient / any fetch handle
    //   - Clock / tokio::time::Instant / std::time::Instant / SystemTime
    //   - Database handle, cache handle, subscription stream
    //   - signer / approval_store / park_store / daemon state
    //   - Arc<…> / Weak<…> / channel to any ambient singleton
    //   - Logger / metrics sink (use return value for audit; no side-effect emission)
}
```

`EvaluationContext` is a named struct, not a trait bag. Every field documented with a justification for why it cannot live in `LendingSnapshot` or `ProposedAction` instead. In V1, `session_wallet` is the only such field — the System Invariant Gate consumes it, and the evaluator receives it for cross-reference completeness.

**Adding a field to `EvaluationContext` is a spec change**, not an implementation refinement. Any new field requires justification against Part 3B §29's "only `(snapshot, proposed_action)`" rule; the justification must explain why the value cannot live in the snapshot.

### 52.3 Dispatcher shape

```rust,conceptual
pub fn evaluate(
    snapshot: &LendingSnapshot,
    action: &ProposedAction,
    config: &LendingRuleConfig,
    context: &EvaluationContext,
) -> Verdict {
    // Step 1: Scope Boundary Gate.
    //   - Action variant is Deposit | Repay (tautological at type level —
    //     the enum excludes Withdraw/Borrow — but the function still exists
    //     at the flow level).
    //   - AllowedLendingProtocols' allowlist check against action.protocol_tag.
    if let Err(reason) = scope_boundary(action, config) {
        return Verdict::HardBlock(reason);
    }

    // Step 2: Rule Evaluation, fail-fast per Part 3B §27.1.
    if let Err(reason) = require_fresh_state(snapshot, config)        { return Verdict::HardBlock(reason); }
    if let Err(reason) = max_oracle_staleness(snapshot, config)       { return Verdict::HardBlock(reason); }
    if let Err(reason) = allowed_mints(snapshot, action, config)      { return Verdict::HardBlock(reason); }
    if let Err(reason) = max_action_input_amount(action, config)      { return Verdict::HardBlock(reason); }

    Verdict::Pass
}
```

Five rules, five direct function calls, fail-fast short-circuit. No dispatcher state, no iteration over a rule list, no trait dispatch. This is deliberately the shape that forces a decision when a sixth rule lands; five rules do not justify the abstraction.

Order follows Part 3B §27.1: Scope Boundary before any rule. `RequireFreshState` first among rules because it is the precondition every other rule's meaningful interpretation relies on — a stale snapshot's oracle-publish-age check is meaningless. Within-step final ordering (e.g., whether `allowed_mints` or `max_action_input_amount` runs first) is a §30.2-permitted Part 5 implementation refinement; the order chosen above is the default.

### 52.4 System Invariant Gate sits BEFORE `evaluate`

Per §51.1, the gate runs upstream of `evaluate(…)` at the outer wiring call site. A failed gate produces `Verdict::HardBlock(SystemInvariantFailed(...))` directly; the evaluator is not invoked. This preserves Part 3B's three-layer ordering at the physical flow level — the evaluator concerns itself only with Steps 2 (Scope Boundary) and 3 (Rule Evaluation) of Part 3B §27.1.

## 53. Deposit / Repay Control Flow

### 53.1 Top-level sequence (conceptual)

For a single action attempt:

1. **Intent resolution.** User / LLM intent is resolved into a concrete `ProposedAction::Deposit { … }` or `ProposedAction::Repay { … }`. Upstream of the lending slice (existing session / persona / tool pipeline). If the resolved intent is `Withdraw` or `Borrow`, it does not become a `ProposedAction` at all — the intent-translation step yields a Scope Boundary HardBlock immediately (the V1 `ProposedAction` enum cannot represent these actions per §49.2).
2. **Session wallet binding.** `session_wallet` is fixed from the `SessionBoundWallet` seam. It is NOT part of the action; it is an input to the gate and to `EvaluationContext`.
3. **Snapshot assembly.** The Part 6 protocol-specific assembler reads on-chain state, applies any protocol-native refresh / preconditioning (Part 6 decision), and returns either a complete `LendingSnapshot` or a pre-evaluation failure (Part 5A §42). Failure ⇒ HardBlock, stop.
4. **System Invariant Gate.** Pure validation of the snapshot + action + session wallet per §51. Failure ⇒ HardBlock, stop.
5. **Evaluator.** `evaluate(snapshot, action, config, context)` runs Scope Boundary + five rules, fail-fast (§52.3). Returns `Pass` or `HardBlock`.
6. **On `Pass`:** the action is eligible to proceed to the next pipeline stage (protocol-specific transaction build — §53.3).
7. **On any `HardBlock`:** outer wiring produces the HardBlock verdict per Part 3B §23.2. No retry, no refresh, no re-evaluation (Part 1 §5.5).

### 53.2 What `Pass` means at the flow level

A `Pass` verdict from `evaluate(…)` authorises the pipeline to proceed but does **not**:

- **Build transaction bytes.** The `Deposit` / `Repay` transaction is constructed downstream by a protocol-specific builder (Part 6 — shape follows §7.1's builder/executor split; placement not decided by Part 5B).
- **Sign anything.** Signing remains the existing Claw signing pipeline's responsibility (external-wallet / Phantom V0 per the A1/A2 proofs in §30.5).
- **Extend snapshot freshness.** If downstream steps take longer than `RequireFreshState`'s `max_fetch_age`, a fresh snapshot + fresh gate + fresh evaluator run is required. Whether and when to re-run is an outer-wiring policy Part 5B does not decide.
- **Guarantee on-chain success.** Chain state may move between snapshot assembly and transaction submission; that is the existing gateway's JIT / retry / blockhash concern, not the evaluator's.

### 53.3 Where transaction build lives

Part 5B does not commit to transaction-build placement. What Part 5B does commit to:

- The builder is **protocol-specific** (Part 6).
- The builder passes §7.1's builder test (pure inputs → unsigned transaction bytes); Jupiter's intent-first + JIT flow is the reference executor shape that consumes a builder output.
- The builder is NOT the evaluator. An evaluator that builds a transaction fails both the pure-function test (it would need reserve-layout decoders) and the seam discipline (§41.2 locates raw-layout mapping on the Part 6 side).
- A `Pass` verdict is consumed by the builder at the outer-wiring layer; the evaluator does not invoke the builder.

### 53.4 Flow diagram (conceptual)

```
Intent (upstream)
     │
     ▼
ProposedAction (Deposit | Repay)   ◄── session_wallet bound here (not on action)
     │
     ▼
Assembler (Part 6, protocol-specific)
     ├── success ──► LendingSnapshot
     └── failure ──► HardBlock(pre-eval) ──► STOP
            │
            ▼
System Invariant Gate (Part 5, pure)
     ├── ok ──► continue
     └── fail ──► HardBlock(SystemInvariantFailed) ──► STOP
            │
            ▼
evaluate(snapshot, action, config, context)   (Part 5, pure)
     ├── Pass ──► action eligible to proceed
     └── HardBlock(ScopeBoundary | RuleRejected) ──► STOP
            │
            ▼
Transaction builder (Part 6; NOT inside evaluator)
     │
     ▼
Existing signing pipeline (A1/A2 path — external wallet / Phantom V0)
```

Every arrow crossing the Part 5 / Part 6 seam runs in the one direction Part 5A §41.3 requires. The evaluator is invoked at one call site; the builder is invoked at a separate call site downstream of `Pass`.

## 54. What Part 5B Refuses to Do

Part 5B is a type-shape and control-flow contract. It deliberately does **not**:

- **Write production code.** No `.rs` files created, no existing files modified, no Rust compiled. Vertical-slice implementation is separately authorised.
- **Commit to final module paths or crate names.** §40.2 / §46 defer these.
- **Choose a protocol.** Solend, Kamino, MarginFi — none selected (§41.2, §53.3).
- **Write raw account-layout mapping.** Part 6.
- **Write oracle binary decoders.** Part 6.
- **Commit to cache / TTL / retry / refresh policy.** §42.3 defers these; Part 5B restates the deferral at the type level by forbidding `EvaluationContext` from carrying cache or retry state.
- **Commit to transaction-builder placement.** §53.3.
- **Represent `Withdraw` / `Borrow` at the type level** — §49.2.
- **Represent future metrics types.** Part 4A §33.1's empty V1 set applies here: no `LendingMetrics`, no metric newtypes.
- **Introduce a generic `LendingRule` trait or rule plugin system.** §50.2.
- **Introduce `Amount<U>` or any unit-indexed generic family.** §47.2.
- **Introduce a `ProtocolAdapter` trait hierarchy.** Per-protocol assemblers are Part 6-side; each is its own type composed at outer wiring, not plugged through a V1 trait.
- **Commit to error-propagation spelling** (enum vs typestate vs `Result<…, E>`). The conceptual signatures above use `Result<…, HardBlockReason>` and `Verdict` for readability; the vertical slice picks the Rust spelling.
- **Commit to serde / database / wire format.** Evaluator operates on in-memory values; persistence and transport belong to outer wiring.
- **Authorize implementation to start.** Part 5B closes the type and flow shape; a separate spec prompt authorizes vertical-slice implementation.

---

Parts 5A (placement / seam) and 5B (types / control flow) together close the Part 5 spec surface for V1 `Deposit` / `Repay`. Part 6 (protocol-specific implementation) and any future Part 5C (cache / refresh / wiring specifics) remain gated by their own spec prompts. The vertical-slice runway is now clear, narrow, and type-shaped; it is still unpaved — no Rust code has been written.

---

# Part 6A — Protocol Choice and V1 Deposit-Only Vertical Slice Scope

**Status:** draft — Part 6A of N
**Scope of Part 6A:** select the first lending protocol for V1; scope the first vertical slice to `Deposit` only; list the minimum protocol-specific work that slice requires; restate the Part 5 seam invariants the slice must uphold; define acceptance criteria for completion.
**Out of scope for Part 6A (explicit):** Rust source; Solend raw-layout parser code; transaction-builder implementation; Repay / Withdraw / Borrow support; predicted metrics; LTV / HF / borrow headroom; cross-protocol abstraction; generic `LendingProtocol` trait; cache / TTL / retry policy beyond a single deposit attempt; frontend polish; multi-protocol routing.

Part 6A inherits every frozen decision from Parts 1 / 2 / 3A / 3B / 4A / 4B / 5A / 5B and the three §30.5 spike-triggered reopens. It does not reopen any of them.

## 55. Protocol Choice

### 55.1 Engineering / risk comparison

| Criterion | Solend | Kamino |
|---|---|---|
| Account-layout stability | SPL-style fixed-size `Pack` structs (1300-byte `Obligation`, 619-byte `Reserve`). Public source at `solendprotocol/solana-program-library` (`token-lending/program/src/state/*`). | Anchor / zero-copy layouts. More versioning surface; per-reserve schema evolves faster. |
| Existing spike evidence in this repo | Yes — `spikes/solend_read/spike.py` + `spike-report.md`. Real non-empty obligation `6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR` (Coin98 pool, 3 deposits + 1 borrow) decoded end-to-end. Three §30.5 reopens already applied to close schema gaps. | None on this repo. A Kamino spike would have to be run before Part 6B could begin. |
| Obligation / Reserve / Oracle mapping verifiability | Byte-offset decode validated against live mainnet state during spike. Dual-oracle + sentinel-slot pattern already absorbed into §11.1. Pyth Lazer (`rec5EKMGg6…`, 134 bytes) and Switchboard On-Demand (`SW1TCH7qEPTdLs…`, 3851 bytes) owner programs identified. | Requires new spike-level investigation. |
| Deposit-path live-verifiability at small scale | Composite ix `DepositReserveLiquidityAndObligationCollateral` is a single-signer transaction pattern observed on mainnet during spike recon. Small-amount deposit (< $5) feasible through existing Phantom / A1-A2 signing path. | Achievable but unverified from this repo's position. |
| Part 5 seam compliance fit | Packed-struct + documented offsets produce a clean mapping into Part 2 `LendingSnapshot` fields without leaking protocol types across the seam. §11.1 / §10.1 reopens were already designed against Solend's shape. | Likely achievable; requires a second spike cycle to confirm the seam holds. |
| Protocol-native refresh semantics | `RefreshObligation` / `RefreshReserve` ixs, documented. Spike directly observed `last_update.stale = true` as default and confirmed refresh-before-use is the real-world pattern. | Similar category of refresh semantics, unverified locally. |

Market-size, user-count, TVL, and yield comparisons are deliberately excluded. V1's first protocol choice is an engineering / risk decision, not a product-market decision.

### 55.2 Decision — Solend

**V1 selects Solend as the first integration protocol.**

Reasons:

- Solend is the only protocol with committed spike evidence in this repo (§30.5). A protocol choice that inherits concrete byte-level evidence is strictly cheaper than one that requires re-running spike-equivalent work.
- Solend's packed-struct layout is directly cited from public source; decode cost is discovery-done, not discovery-pending.
- The dual-oracle + sentinel-slot shape that forced the §11.1 reopen is already absorbed into Part 2. Any Solend reserve the V1 slice touches will fit that reopened shape without a fresh schema change.
- Deposit-path live verifiability (single composite ix, single signer, small-amount feasible) is consistent with the A1/A2 proof posture the repo has already established.
- Switching to Kamino would require a new spike cycle before Part 6B could begin; no engineering gain justifies the delay at V1 scale.

### 55.3 What this choice is NOT

- **Not a product preference.** Part 6A does not claim Solend is a better product, more liquid, or more user-friendly than alternatives. The choice is an engineering sequencing call bounded to V1's first vertical slice.
- **Not a multi-protocol commitment.** V1 integrates one protocol. Part 6C and beyond (or a future multi-protocol spec) MAY add Kamino or others; the Part 5 seam was designed precisely to make additional protocols cost seam-compliant Part 6 modules, not rewrites.
- **Not a specific-market commitment beyond V1.** V1 narrows to ONE Solend lending market (vertical slice picks one of Main / Coin98 / AMM / etc.). Adding a second market is an extension, not a V1 requirement.
- **Not permission to write Solend-specific code.** Part 6A selects; Part 6B authorizes implementation.

## 56. Why Deposit-Only First

### 56.1 Risk posture

Deposit is a **risk-reducing action** per Part 1 §3.1. It cannot raise the obligation's current LTV, cannot increase the obligation's debt, cannot trigger a liquidation path. The worst-case outcome of a bad Deposit is that the user has posted collateral that sits earning supply yield — a reversible state.

Repay is also risk-reducing, but Repay requires a pre-existing debt position. V1's first proof does not need a debt to already exist; Deposit works from a clean wallet.

### 56.2 Metric independence

Deposit does NOT require any derived metric to evaluate under V1 rules (Part 4A §32.6): the five V1 mandatory rules (`RequireFreshState`, `MaxOracleStalenessMs`, `AllowedLendingProtocols`, `AllowedMints`, `MaxActionInputAmount`) are all answerable from `(snapshot, proposed_action)` alone.

- No LTV computation.
- No health-factor computation.
- No borrow-headroom computation.
- No USD valuation.

This keeps the V1 metrics set empty (§33.1) through the first vertical slice unchanged.

### 56.3 Full-stack seam exercise

A single Deposit action exercises every layer Parts 1 – 5B defined:

- Part 1: risk-reducing action classification.
- Part 2: snapshot assembly with the schema + unit tags + oracle feed set.
- Part 3A: five V1 rules.
- Part 3B: three-layer gate + fail-fast + `Pass` / `HardBlock(reason)` verdict.
- Part 4A: empty metrics set in practice.
- Part 5A: Part 5 / Part 6 seam compliance.
- Part 5B: evaluator API + System Invariant Gate + control flow + newtypes.

Plus the existing A1/A2 signing pipeline (`SessionBoundWallet`, Phantom V0 signing, daemon completion, `send_raw_v0_transaction`).

A successful Deposit slice closes "does the seam hold end-to-end on a real protocol?" with concrete evidence. Adding Repay after that is a local extension; adding future risk-increasing actions is a separate scope decision.

### 56.4 Sequencing

- **V1 Deposit slice** (this scope): first live proof. Sealed by the acceptance criteria in §60.
- **Repay slice** (follow-up, not authorized by Part 6A): added after Deposit is sealed. No Part reopening required — Repay reuses the same five rules, the same assembler, the same seam.
- **Withdraw / Borrow**: remain hard-blocked at scope boundary per Part 1 §3.2. Unlocking them is a future V2 scope decision (see §30.2 "Part 4 Open Questions" and §35.8 / §35.9 prerequisites); Part 6A does not touch this.

## 57. Required Protocol-Specific Work

Deposit-only. Work needed ONLY for other actions (Repay, Withdraw, Borrow) is excluded from this list.

### 57.1 Account identification and fetch

- **Obligation pubkey resolution** for the `(session_wallet, lending_market)` pair. Solend's obligation account is a PDA derived from the lending market + seed; V1 Part 6B decides between user-supplied obligation pubkey or derived-then-verified. If the obligation account does not exist on chain, V1 surfaces a clear `HardBlock(SystemInvariantFailed)` with reason "obligation not initialized" — auto-init is NOT required for V1 (see §59).
- **Reserve pubkey for the deposit target mint.** V1 narrows to one Solend lending market. The reserve registry for that market is a Part 6B concern — either fetched via `getProgramAccounts(dataSize=619)` + market filter, or carried as configuration. Decision is vertical-slice; either is seam-compliant.
- **Oracle pubkey extraction.** Each Reserve byte-layout exposes two oracle slot pubkeys (pyth + switchboard). Sentinel pubkey `nu11…` recognized at assembly time and excluded from the `OracleFeedSet`. Accounts for real oracle pubkeys are fetched to identify their owner program (Pyth Lazer receiver vs Switchboard On-Demand) — this much was already done in the spike.

### 57.2 Raw account-layout mapping

- **Obligation decode (1300 bytes)** — the subset Part 2 / Part 5B consumes: version, last_update slot / stale, lending_market, owner, padding-presence-check, `deposits_len`, `borrows_len`, and the deposits / borrows arrays. Offsets per cited source (`token-lending/program/src/state/obligation.rs`).
- **Reserve decode (619 bytes)** — the subset needed for V1:
  - `version`, `last_update.slot`, `last_update.stale`
  - `lending_market`
  - `liquidity.mint`, `liquidity.mint_decimals`, `liquidity.supply` (ATA destination)
  - `liquidity.pyth_oracle`, `liquidity.switchboard_oracle`
  - `liquidity.available_amount` (underlying units; for display and for future caps — NOT a V1 rule input)
  - `collateral.mint` (c-token mint; destination for deposit collateral)
  - Lifecycle markers (paused / disabled / supply_capped)

  Wad-scaled interest-model / risk-parameter fields are carried verbatim per §10 but are not consumed by V1 rules.
- **Reserve → Part 2 `Reserve` mapping.** Numeric fields carry unit tags per §47.1 at the seam. Non-decoded fields that §10 requires to be present can be represented as opaque-but-typed in V1 (vertical-slice choice).

### 57.3 Oracle decode — minimum surface

V1 consumes only `publish_slot` per feed, plus owner-program identification. That is the minimum `MaxOracleStalenessMs` needs.

- **Pyth Lazer receiver account (134 bytes, owner `rec5EKMGg6…`)** — extract `publish_slot` from the payload. Payload layout is protocol-binary-format work; cost is bounded because only one field is needed.
- **Switchboard On-Demand account (3851 bytes, owner `SW1TCH7qEPTdLs…`)** — extract `publish_slot` (or `publish_timestamp` if that is the only dimension the format exposes — in which case V1's conservative slot-to-ms conversion per §51.4 applies in reverse).
- **Price value / confidence interval / price exponent: NOT DECODED.** Part 4A §32.2, Part 5B §48.4.

### 57.4 Protocol-native stale-marker interpretation

- `Obligation.last_update.stale` and each `Reserve.last_update.stale` surface verbatim into Part 2's `protocol_native_stale: StaleMarker`.
- Per §18.1: `StaleMarker::Stale` on any component is a HardBlock unconditionally. No threshold, no override.
- V1 Part 6B implementation chooses ONE of three patterns to handle the default `stale = true` that Solend returns on bare reads:
  - **(a) Assembler issues standalone `RefreshObligation` / `RefreshReserve` transaction BEFORE the read.** Higher latency, cleaner separation.
  - **(b) Deposit transaction bundles refresh ixs as leading instructions.** Cheapest on-chain; policy evaluates against the post-refresh state predicted by the bundled ixs (this requires care: the snapshot must reflect the post-refresh state, not the pre-refresh state).
  - **(c) Assembler fails-closed on `stale = true`, caller retries after the user-facing UI triggers refresh separately.** Simplest, worst UX.

  All three are Part 5 seam compliant because refresh happens inside the assembler or outer-wiring layer — never inside the evaluator. V1 picks one in Part 6B; this is not a Part 6A decision.

### 57.5 Deposit instruction builder (minimum shape)

Solend's deposit on V1 uses `DepositReserveLiquidityAndObligationCollateral` — a single composite instruction that (i) transfers underlying from the user's source ATA into the reserve's liquidity supply, (ii) mints collateral tokens to the user's collateral ATA, (iii) deposits the collateral into the user's obligation.

Required accounts (Part 6B produces the exact list; this is the spec-level minimum):

- User's source ATA for the reserve's underlying mint (must exist; missing = HardBlock).
- User's destination ATA for the reserve's collateral mint (may not exist; vertical-slice decides whether to bundle `create_associated_token_account` or HardBlock).
- Reserve pubkey.
- Reserve liquidity supply pubkey (from decoded Reserve.liquidity.supply).
- Reserve collateral mint pubkey (from decoded Reserve.collateral.mint).
- Obligation pubkey.
- Lending market pubkey.
- Lending market authority PDA.
- Transfer authority (signer — the session-bound wallet).
- Token program, clock sysvar (as required by the ix).

Builder semantics:

- Pure inputs → unsigned V0 transaction bytes. Passes §7.1's builder test.
- The builder is protocol-specific and lives on the Part 6 side of the seam.
- The builder does NOT re-evaluate policy. It consumes a `Pass` verdict and produces transaction bytes; any failure at build time (missing accounts, math check) is a clean upstream surface, not a silent policy override.

### 57.6 Associated token accounts

- **Source underlying ATA**: assumed to exist (user already holds the asset to deposit). If missing → HardBlock with reason "source ATA missing."
- **Destination collateral (c-token) ATA**: may or may not exist for a first-time depositor. Part 6B decides: (a) bundle `create_associated_token_account` as a leading ix, or (b) HardBlock with reason "collateral ATA not initialized." Default suggestion: bundle the create ix — the cost is negligible and the UX gain is real.
- **Obligation account init**: NOT required for V1 per §59. If the user has no obligation, V1 HardBlocks cleanly.

### 57.7 Transaction-assembly pattern

Follows the existing Jupiter intent-first + JIT reference shape (`ARCHITECTURE.md §7.1` executor side). Builder = pure inputs → `VersionedTransaction`. Executor = existing gateway completion / rebroadcast / blockhash-window machinery. No new executor semantics introduced for Deposit.

## 58. Part 5 Seam Compliance

The Deposit slice MUST uphold every invariant Parts 5A / 5B fix. Restated here because implementation-proximity is where seams are most likely to leak.

- **Assembler returns complete `LendingSnapshot` or fails.** Any Solend RPC failure, decode failure, sentinel-ambiguity, missing-oracle, or `stale = true` bit surfaces as a single pre-evaluation failure at the seam. The evaluator never sees partial state, never sees an `Option<Obligation>`, never sees a "best-effort" snapshot.
- **Evaluator signature is fixed.** `evaluate(snapshot, action, config, context) -> Verdict` per §52.1. Solend-specific types MUST NOT appear in its inputs or outputs. Part 6B may reference Solend types only inside the assembler and the transaction builder; they do not cross the seam.
- **No chain-state read after evaluator entry.** Evaluator and gate use `snapshot.fetched_at.observed_slot` as "now" per §51.4. If the transaction takes long enough to build or queue that the snapshot stales, outer wiring MUST reassemble from scratch and restart the full gate sequence — NOT refresh in place, NOT patch a field, NOT extend freshness.
- **Transaction builder runs only after `Pass`.** Outer wiring composes `assembler → invariant_gate → evaluator → (on Pass) deposit_builder → signing pipeline`. No skip, no parallel run, no speculative builds before `Pass`.
- **Evaluator does not build; builder does not re-policy.** The builder trusts the `Pass` verdict. If the builder encounters unexpected state (e.g., an account pubkey that would produce an invalid ix), it surfaces failure to the caller — it does NOT re-enter the evaluator, re-fetch the snapshot, or silently patch.
- **Protocol-level on-chain rejection flows back cleanly.** If Solend's runtime rejects the deposit (reserve deposit limit reached, reserve paused mid-flight, simulate-time ix rejection), the failure surfaces through the existing gateway error path as a visible, actionable error — without partial writes to `approval_store` / `park_store`, without orphaned pending state, and without leaving the daemon in a state that would retry indefinitely.
- **Stale mid-flight → full restart.** If between `Pass` and on-chain submission the snapshot's `RequireFreshState` threshold is exceeded, the outer wiring re-assembles a fresh `LendingSnapshot`, re-runs the full three-layer gate, and only then builds and submits.
- **No Part 6 → Part 5 back-imports.** Per §41.3: Part 6B's Solend modules depend on Part 2 schema types and their own protocol-specific internal types. Part 5 modules (evaluator, gate, newtypes) do not depend on Part 6 Solend modules. Outer wiring at the gateway edge is the only composition site.

## 59. Out of Scope

Explicit non-goals for the Deposit-only slice:

- **Repay.** Follow-up slice, separately authorized. V1 scope includes Repay; V1's *first slice* does not.
- **Withdraw.** Part 1 §3.2 scope boundary. Not representable in `ProposedAction` (§49.2).
- **Borrow.** Same as Withdraw.
- **Predicted metrics.** Part 4B §35.2, §35.4 — no predicted-state model introduced.
- **LTV / health factor / borrow headroom.** No V1 rule consumes them (§32.1 – §32.5); they do not get computed for the slice.
- **USD valuation.** No oracle price decode beyond `publish_slot`.
- **Cross-protocol abstraction / `LendingProtocol` trait / adapter hierarchy.** Per §50.2, deferred until a second protocol exists.
- **Cache / TTL / retry / refresh policy beyond a single deposit attempt.** If a single attempt stales mid-flight the slice reassembles once and re-evaluates; repeated retries under a policy are out of scope for V1's first proof.
- **Frontend polish.** Minimum UI necessary to trigger a Deposit action and display the verdict is slice-scope; error-UX enrichment, multi-step approval flow, success-animation, and comparable polish are NOT.
- **Multi-protocol routing or selection UX.** V1 integrates one protocol; one-protocol UI is sufficient.
- **Multiple Solend markets in one slice.** V1 narrows to one Solend lending market. Adding a second market to the same slice is unnecessary scope expansion; it is a follow-up extension.
- **Obligation auto-init.** If the user has no obligation account, the slice HardBlocks with a clear reason. Auto-creating the obligation is a UX improvement, not V1's first-proof requirement.
- **Refresh-ix bundling vs. stand-alone ix — decided but not prescribed here.** Part 6B picks one of §57.4's three patterns; Part 6A does not.

## 60. Acceptance Criteria

The Deposit-only vertical slice is complete when all of the following hold.

### 60.1 Deterministic tests

Implemented as standard Rust tests (`cargo test`), no network required.

- **Assembler success shape.** Given fixture Obligation bytes + fixture Reserve bytes + fixture oracle account-infos, the assembler produces a valid `LendingSnapshot` whose every required-present field is populated, whose unit tags are correct, and whose oracle `OracleFeedSet` excludes sentinel slots.
- **Assembler failure shapes.** Separate tests for each failure mode:
  - Missing obligation (RPC returns null) → pre-evaluation failure.
  - Wrong-owner obligation (account owner ≠ Solend program) → pre-evaluation failure.
  - Wrong-size obligation (not 1300 bytes) → pre-evaluation failure.
  - Missing Reserve referenced by obligation → pre-evaluation failure.
  - Oracle account at a non-sentinel pubkey is unreadable → pre-evaluation failure.
  - All-sentinel oracle slots on a reserve the deposit touches → (may pass assembly with empty `OracleFeedSet`; rule-layer HardBlocks).
- **Unit-tag correctness at compile time.** A test that attempts to construct a `ProposedAction::Deposit { amount: WadAmount(_) }` fails to compile. (This is a compile-fail test fixture, not a runtime test.)
- **Owner-mismatch HardBlock.** Fixture snapshot whose `obligation.owner` ≠ `context.session_wallet` → `Verdict::HardBlock(SystemInvariantFailed(_))`.
- **Protocol-tag mismatch HardBlock.** Fixture snapshot where `snapshot.protocol_tag ≠ action.protocol_tag()` → `SystemInvariantFailed`.
- **Protocol-native stale HardBlock.** Fixture where `obligation.protocol_native_stale = Stale` (or any reserve's) → `RuleRejected(RequireFreshState)`.
- **Fetch-age HardBlock.** Fixture where a component's `fetched_at.observed_slot` is so far behind `snapshot.fetched_at.observed_slot` that the converted age exceeds `max_fetch_age` → `RuleRejected(RequireFreshState)`.
- **Oracle publish-age HardBlock.** Fixture where oldest feed's `publish_slot` converted to ms exceeds `max_publish_age` → `RuleRejected(MaxOracleStalenessMs)`.
- **Oracle-missing HardBlock.** Fixture where the reserve the action touches has an empty `OracleFeedSet` → `RuleRejected(MaxOracleStalenessMs)` (fail-closed per §18.2 interpretation).
- **Mint-not-in-allowlist HardBlock.** Fixture whose action `reserve_mint ∉ AllowedMints.allowlist` → `RuleRejected(AllowedMints)`.
- **Amount-over-cap HardBlock.** Fixture whose action `amount > per_mint_caps[reserve_mint]` → `RuleRejected(MaxActionInputAmount)`.
- **Deposit happy path → `Pass`.** Fixture with: fresh snapshot, obligation owner matches session wallet, protocol-tag matches, mint in allowlist, amount under cap, all oracles fresh → `Verdict::Pass`.
- **`Withdraw` / `Borrow` non-representability.** A test attempting to construct `ProposedAction::Borrow { … }` fails to compile. A test that feeds an externally-deserialized action whose tag is "Withdraw" into the pipeline is rejected at the Scope Boundary Gate with `ScopeBoundary` reason.
- **Graceful protocol-level rejection.** Fixture (or in-process harness) where the Solend deposit ix would fail at simulate-time (reserve supply-cap reached, reserve paused bit active mid-flight, invalid account combination) → the deposit builder / executor surfaces a clean error upstream. Post-conditions asserted: no panic, no partial write to `approval_store` or `park_store`, no orphaned pending state, daemon does not loop on retry.

### 60.2 Opt-in live proof

- A single small mainnet Solend deposit (< $5 USD equivalent).
- Signed via the existing Phantom external-wallet V0 path (A1/A2 proof pattern).
- Completion dispatched through the daemon (`project_a1_execute_proof` reference pattern: `send_raw_v0_transaction` → mainnet finalize).
- Transaction signature observable on Solana Explorer at finalized commitment.
- Test is gated behind an opt-in env flag (e.g., `CLAW_LIVE_SOLEND_DEPOSIT=1`). NOT enabled in CI.

### 60.3 Proof artifact

- Written under `docs/proofs/` (aligns with D-4 target shape for frozen proof material).
- Content: tx signature, finalized slot, explorer URL, signer pubkey, obligation pubkey, reserve pubkey, reserve mint, deposit `UnderlyingAmount` (in base units + decimals), the `LendingSnapshot` hash evaluated, the verdict = `Pass`, the rule configuration used.
- Referenced from a new project-memory entry following the pattern of `project_a1_execute_proof` / `project_a2_execute_proof`.

### 60.4 Non-dependence on spike scripts

- The production path MUST NOT `use`, import, invoke, or depend on anything under `spikes/solend_read/`.
- Any code reuse from the spike is a copy-with-intent into a production module with its own tests. Spike remains disposable per its own README; deleting `spikes/solend_read/` MUST NOT break the production build.

### 60.5 Seam-compliance inspection

A final acceptance pass verifies, by reading the touched modules, that:

- No Part 6 module is imported by any Part 5 module (evaluator, gate, newtypes).
- The evaluator's signature does not mention any Solend-specific type.
- The snapshot-assembler's output type is protocol-agnostic `LendingSnapshot`.
- The transaction builder consumes only snapshot + action + configuration, with no back-edge into the policy evaluator.

## 61. What Part 6A Does Not Authorize

Part 6A selects a protocol and scopes a slice. It deliberately does NOT:

- **Write production code.** No `.rs` files created, no existing files modified.
- **Authorize full Solend integration.** Assembler + deposit builder + Repay + all five rules wired end-to-end is Part 6B scope at the earliest; it is not unlocked by Part 6A.
- **Authorize writing the Solend raw-layout parser.** Part 6B.
- **Authorize writing the deposit transaction builder.** Part 6B.
- **Authorize adding a new crate or top-level subdirectory to the workspace.** Any such addition is a Part 6B decision that must also run the PC-1 path-reference audit per `DEBT.md`.
- **Authorize Repay, Withdraw, or Borrow.** Repay is deferred post-Deposit; Withdraw / Borrow remain scope-boundary blocked.
- **Introduce a `LendingProtocol` trait or any cross-protocol abstraction.** Per §50.2 and §41.3.
- **Commit to a specific Solend lending market.** Part 6B picks one of Main / Coin98 / AMM / etc.; Part 6A fixes only "one market."
- **Commit to a specific refresh pattern.** Part 6B chooses from §57.4's three options.
- **Commit to whether the destination c-token ATA is auto-created.** Part 6B chooses; see §57.6.

What Part 6A does authorize:

- **Writing a Deposit-only implementation prompt (Part 6B).** That prompt treats §55 – §61 as the scope contract, picks the deferred choices above, and authorizes its own implementation work against the acceptance criteria in §60.

---

Part 6A closes protocol choice and slice scope. Part 6B (Deposit-only implementation) remains gated by its own spec prompt. No Rust code has been written for lending integration; the vertical-slice runway is now both type-shaped (Part 5B) and protocol-targeted (Part 6A), still unpaved.

---

# Part 6B — Solend Deposit-only Implementation Spec

**Status:** draft — Part 6B of N
**Scope of Part 6B:** fix the implementation boundaries for the V1 Solend `Deposit`-only vertical slice — module placement, anti-corruption boundary between Solend raw types and Part 2 / Part 5B seam types, minimum raw-account decoding for Deposit, minimum oracle freshness decoding, the standalone-refresh precondition strategy, deposit transaction builder scope, ATA handling, transaction-version / signing path, deterministic test categories, and opt-in live-proof acceptance criteria.
**Out of scope for Part 6B (explicit):** Rust source files, existing-module edits, test fixtures, CI changes, new workspace members, frontend work, multiple Solend markets in one slice, Repay / Withdraw / Borrow, any cross-protocol abstraction, cache / TTL / retry policy beyond a single deposit attempt.

Part 6B inherits every frozen decision from Parts 1 / 2 / 3A / 3B / 4A / 4B / 5A / 5B / 6A and the §30.5 spike-triggered reopens. It does not reopen any of them.

## 62. Placement Decision

### 62.1 Comparison

| Option | Shape | Cost | V1 fit |
|---|---|---|---|
| **New crate** (`claw-solend` / `claw-protocol-solend`) | Own `Cargo.toml`, own workspace member, own public API. | `Cargo.toml` workspace edit; CI awareness; rust-analyzer config; PC-1 path-reference audit required. | Premature: no cross-protocol abstraction yet justifies a crate boundary; forces early taxonomy. |
| **Gateway integration** (`crates/gateway/src/integrations/solend/`) | Lives inside existing `claw-gateway`, alongside other integrations. Aligns with the existing `crates/gateway/src/integrations/` directory already present in the tree, and with the Jupiter integration at `crates/gateway/src/tools/jupiter_swap.rs`. | One new subdirectory inside an existing crate. No workspace or CI changes. | Consistent with current repo shape; satisfies §7.1 builder test without inventing a crate taxonomy from a single implementor. |

### 62.2 Decision — gateway integration

**V1 Solend Deposit slice lives under `crates/gateway/src/integrations/solend/`.**

Reasons:

- Aligns with existing DeFi-integration placement in this repo (Jupiter). No new taxonomy invented for a single implementor.
- Avoids a workspace-level edit and the associated PC-1 path-reference audit cost until the second lending protocol arrives.
- Satisfies `ARCHITECTURE.md §7.1`: the Solend assembler + decoder + builder are builder-test-passable modules (pure inputs → artifacts), provided the module-boundary disciplines in §62.3 hold.
- When a second lending protocol arrives and genuinely justifies a shared crate, extraction is a natural D-1-adjacent refactor — cheaper to perform once both implementors exist than to pre-design an empty abstraction now.

### 62.3 Module-boundary discipline

Every new `mod.rs` under `crates/gateway/src/integrations/solend/` MUST carry a module doc (`//! …`) satisfying `ARCHITECTURE.md §7.3`. At minimum, the doc states:

- **What this module owns.** Solend-specific raw-account decoding; the `SolendObligationRaw` / `SolendReserveRaw` internal shapes; the Solend Deposit transaction builder; the Solend standalone-refresh transaction builder.
- **What the module MUST NOT import.** Explicit forbidden list:
  - `approval_store`, `park_store`, pending-state machinery.
  - Signer internals, daemon runtime state, completion-dispatch loops, blockhash-tracking machinery beyond what the existing A1/A2 path already exposes as a public input.
  - Any type from the policy / evaluator / gate side of the seam (§41.3 one-way flow).
- **What the module's evaluator-facing output is.** Part 2 `LendingSnapshot` + Part 5B newtypes ONLY. Solend-prefixed types MUST NOT appear in any public function signature whose callers live outside the `integrations/solend/` subtree.

Part 6B does not choose the leaf file names (`mod.rs`, `obligation.rs`, `reserve.rs`, `oracle.rs`, `deposit.rs`, `refresh.rs`) — those are implementation choices. It fixes the subtree and the boundary posture.

## 63. Anti-Corruption Boundary

### 63.1 Allowed Solend-specific types (internal only)

The integration MAY define and use:

- `SolendObligationRaw` — the 1300-byte Solend Obligation decoded into a Solend-shaped intermediate struct (version, last_update, lending_market, owner, deposited_value / borrowed_value wad fields, deposits[], borrows[], protocol-native stale marker, padding marker).
- `SolendReserveRaw` — the 619-byte Solend Reserve decoded into a Solend-shaped intermediate struct with the subset of fields §64 enumerates.
- `SolendDepositAccounts` — the fixed bundle of accounts the Deposit instruction requires.
- `SolendRefreshAccounts` — the account bundle required for `RefreshObligation` / `RefreshReserve`.
- `SolendInstructionTag` or equivalent discriminator enum if needed.
- `SolendOracleRaw` — minimal struct carrying only the freshness fields §65 requires.

These are **integration-internal** types. They live inside `integrations/solend/` and are never re-exported outside that subtree.

### 63.2 Forbidden boundary crossings

A Solend-prefixed type appearing in any of the following is a **spec violation**:

- The `evaluate(…)` signature (Part 5B §52.1) or any function it calls.
- Any function in a `rules::*` module, in the System Invariant Gate, in the Scope Boundary Gate, or in rule-config types (Part 5B §50.1).
- The public API of the snapshot-assembly seam — what crosses is `Part2::LendingSnapshot` / `Part5B` newtypes only.
- The proof artifact's stable schema written to `docs/proofs/` (§71.3).
- Any wire format, API request / response, or persisted state.

### 63.3 The mapping is the seam's last line

`SolendObligationRaw → Part2::Obligation`, `SolendReserveRaw → Part2::Reserve`, `SolendOracleRaw → Part2::OracleFeed` — each mapping function lives in `integrations/solend/` and is the **only** place where Solend vocabulary terminates and Part 2 vocabulary begins. A reviewer's test: grep for `Solend` in files outside `crates/gateway/src/integrations/solend/` — every hit that is not a string literal, a comment, or a `ProtocolTag::Solend` enum variant is a candidate seam violation.

### 63.4 `ProtocolTag::Solend`

The Part 2 / Part 5B `ProtocolTag` enum gains a `Solend` variant. This is **not** a Solend-prefixed type crossing the seam — `ProtocolTag` itself is protocol-agnostic; variants are just labels. The variant's *value* crosses (e.g., in `snapshot.protocol_tag`), not any Solend-specific structure.

## 64. Minimal Raw Decoding

Only the fields the Deposit-only slice actually needs. No interest-rate / APY / accumulator decoding unless the field is required to populate a Part 2 shape that V1 rules (or the System Invariant Gate) consume.

### 64.1 Obligation decode (1300 bytes)

Offsets cited from `solendprotocol/solana-program-library`, `token-lending/program/src/state/obligation.rs`, `Pack` impl.

Required fields for V1 Deposit:

| Offset | Size | Field | V1 use |
|---|---|---|---|
| 0 | 1 | `version` | decoder sanity — fixture-check expected version |
| 1 | 8 | `last_update.slot` | Part 2 `Obligation.protocol_last_updated_slot` |
| 9 | 1 | `last_update.stale` | Part 2 `Obligation.protocol_native_stale` → `RequireFreshState` input |
| 10 | 32 | `lending_market` | §51.2 protocol-market consistency check; confirms snapshot matches the market the action targets |
| 42 | 32 | `owner` | §51.2 owner check against `context.session_wallet` |
| 138 | 64 | `_padding` | integrity check — all zeros (fixture asserts) |
| 202 | 1 | `deposits_len` | bound the deposits array read |
| 203 | 1 | `borrows_len` | bound the borrows array read (included for completeness — Part 2 requires borrows present even if V1 rules don't read them) |
| 204 | 88 × deposits_len | `deposits[]` | each entry: `deposit_reserve` (32) + `deposited_amount` (8, `CollateralTokenAmount`) + `market_value` (16, wad — NOT consumed by V1) + padding |
| 204 + 88·deposits_len | 112 × borrows_len | `borrows[]` | each entry: `borrow_reserve` (32) + `cumulative_borrow_rate_wads` (16) + `borrowed_amount_wads` (16, `WadAmount`) + `market_value` (16) + padding |

Fields explicitly NOT decoded for V1: `deposited_value`, `borrowed_value`, `allowed_borrow_value`, `unhealthy_borrow_value` (all at offsets 74–138). These are wad-scaled policy / liquidation fields feeding `borrow_headroom` / HF metrics, which V1 does not compute (Part 4A §33.1). The bytes are skipped or carried opaquely; V1 rules never read them.

Fixture / sanity-check requirements:

- One golden obligation fixture with a known real-world layout (spike target `6rFb29ZA…` or a stable mainnet alternative snapshotted to bytes).
- Per-field decode test: decoded value matches the expected value asserted verbatim in the test.
- Size assertion: `data.len() == 1300`; any other size returns pre-evaluation failure.
- `_padding` assertion: bytes 138..202 are all zero.

### 64.2 Reserve decode (619 bytes)

Offsets cited from `token-lending/program/src/state/reserve.rs`, `Pack` impl.

Required fields for V1 Deposit:

| Field | V1 use |
|---|---|
| `version` | decoder sanity |
| `last_update.slot` | Part 2 `Reserve.protocol_last_updated_slot` |
| `last_update.stale` | Part 2 `Reserve.protocol_native_stale` → `RequireFreshState` input |
| `lending_market` | market consistency check |
| `liquidity.mint_pubkey` | Part 2 `Reserve.mint`; `AllowedMints` input; Deposit builder input |
| `liquidity.mint_decimals` | Part 2 `Reserve.mint_decimals`; display only (no rule consumes) |
| `liquidity.supply_pubkey` | Deposit builder input (destination underlying supply) |
| `liquidity.pyth_oracle` | oracle pubkey; sentinel recognition |
| `liquidity.switchboard_oracle` | oracle pubkey; sentinel recognition |
| `liquidity.available_amount` | Part 2 `Reserve.available_amount` (`UnderlyingAmount`); carried; not a V1 rule input |
| `collateral.mint_pubkey` | Deposit builder input (c-token mint, destination ATA target) |
| `collateral.supply_pubkey` | included in Part 2 Reserve for completeness |
| Lifecycle markers (paused / disabled / supply-capped / borrow-capped) | Part 2 `Reserve.lifecycle`; surfaced to policy (rule consumers are future) |

Fields explicitly NOT decoded for V1: interest-model parameters (base rate, slopes, optimal utilization), fee parameters (borrow fee wad, flash-loan fee wad, host fee), accumulators (cumulative borrow rate wads, accumulated protocol fees wads), rate limiter fields. V1 rules do not consume these; carrying them as tagged opaque bytes or eliding them from the Rust shape are both acceptable (§10's "verbatim fields" remain a Part 2 *conceptual* commitment; Rust-level elision is a §33.1-consistent choice).

Fixture / sanity-check requirements:

- Golden reserve fixture (e.g., one of the reserves the spike decoded: `46Lh1P2X…`, `5twXA9pw…`, or `GdJd6a8Z…` from Coin98 pool).
- Size assertion: `data.len() == 619`.
- Per-field decode test as for obligation.

### 64.3 What the raw decoders MUST NOT produce

- **Snapshot-facing output.** The decoders return `SolendObligationRaw` / `SolendReserveRaw`. These are consumed by the Part 2 mapping step (§63.3) which is the only site that produces `Part2::Obligation` / `Part2::Reserve`.
- **No derived values.** No c-token-to-underlying conversion, no utilization, no APY.
- **No interpretation.** Stale bit is surfaced verbatim as `StaleMarker::Fresh | StaleMarker::Stale`. Threshold comparisons live in rules, not the decoder.

## 65. Oracle Freshness Minimum

### 65.1 What to decode per feed

V1's only oracle consumer is `MaxOracleStalenessMs` (Part 3A §18.2). The per-feed minimum:

- **Owner program** (trivial — from `getAccountInfo` result, not payload parse).
- **Feed identity** (the account pubkey — trivial).
- **Publish freshness: `publish_slot` OR `publish_wall_time`.** Whichever the format exposes is acceptable. The assembler normalizes into `Part2::FeedPublishFreshness` which carries an `Option<ChainSlot>` for `publish_slot`; when only wall-time is available, the mapping converts using the conservative slot-duration constant (§51.4) to populate `publish_slot` as a derived value AND records the wall-time input in `FreshnessContext` for audit.
- **Status markers**, if the format exposes interpretable flags — surfaced verbatim into `OracleStatusMarkers`. Unparseable flags are elided.

### 65.2 What NOT to decode in V1

- **Price value.** No rule consumes it.
- **Confidence interval.** No rule consumes it.
- **Price exponent.** No rule consumes it.
- **EMA / TWAP / smoothed price.** No rule consumes it.
- **Cross-feed aggregation.** Part 2 §11.1 preserves the feed set; V1 evaluator reads every feed in the set; any failing → HardBlock. No silent winner selection.

### 65.3 Freshness extraction per owner program

Two known programs from the spike:

- **Pyth Lazer receiver** (owner `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`, 134-byte account). Extract `publish_slot` or `publish_wall_time` per the receiver format.
- **Switchboard On-Demand** (owner `SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f`, 3851-byte account). Extract `publish_slot` or `publish_wall_time` per the On-Demand format.

For either format: if neither `publish_slot` nor `publish_wall_time` can be extracted (unknown sub-format, schema drift), the assembler fails-closed for that feed; the feed does not surface in the `OracleFeedSet`. If every feed for a priced asset fails extraction, the asset's `OracleFeedSet` is empty, and `MaxOracleStalenessMs` HardBlocks when evaluated (§60.1).

### 65.4 Sentinel handling

Per §11.1: the literal `nu11111111111111111111111111111111111111111` pubkey in a reserve's oracle slot means "slot not configured." The assembler recognizes it at source (the reserve decode step, not the oracle-fetch step) and excludes the slot from the feed set. The assembler does NOT fetch the sentinel account from RPC. *Unconfigured slot* and *configured-but-unreachable feed* are distinguished at the adapter boundary per §11.1.

## 66. Refresh Strategy — Standalone Precondition

V1 adopts **option (b)** from the design discussion: a standalone refresh transaction as a precondition, never bundled into the Deposit transaction.

### 66.1 The rule

- If any component's `protocol_native_stale == Stale` on bare read, outer wiring MAY issue a standalone Solend refresh transaction.
- The refresh transaction contains one or more `RefreshReserve` ixs (one per reserve the Deposit touches) followed by a `RefreshObligation` ix. It contains NO Deposit ix.
- The refresh transaction is submitted, signed via the existing Phantom / daemon path, and confirmed (finalized) **before** the assembler re-fetches.
- After finalization, the assembler re-fetches the obligation, reserves, and oracles, and builds a new `LendingSnapshot`.
- The full gate sequence restarts from scratch (System Invariant Gate → Scope Boundary Gate → Rule Evaluation). If the new snapshot also shows `stale = true` (e.g., the refresh itself was evicted or the state drifted again), the attempt fails-closed with a clean HardBlock.

### 66.2 Seam invariants the strategy preserves

- **Evaluator unawareness.** The evaluator sees a fresh `LendingSnapshot`. It has no input field, no context field, and no channel that reveals whether a refresh happened. Evaluator determinism per Part 3B §29.4 holds.
- **Deposit builder unawareness.** The Deposit tx builder receives the Part 2 snapshot and the approved action. It does not include refresh ixs. Its output is a pure Deposit transaction.
- **No mid-flight refresh.** Between `Pass` and on-chain submission of the Deposit tx, no refresh happens. If the snapshot stales during this window, outer wiring reassembles from scratch (which MAY re-trigger a refresh) — it does NOT patch or extend the existing snapshot.
- **No chain read after evaluator entry.** Unchanged.

### 66.3 Failure modes and outcomes

| Condition | Outcome |
|---|---|
| Bare read: all components fresh | Proceed directly to gate sequence. No refresh tx. |
| Bare read: any component stale; refresh tx succeeds (finalized); re-assembled snapshot fresh | Proceed to gate sequence with the fresh snapshot. Refresh tx signature recorded separately from Deposit tx signature. |
| Bare read stale; refresh tx broadcast fails (RPC error, signer reject, blockhash expiry) | Attempt fail-closed with HardBlock; refresh tx signature (if any) recorded as failed. |
| Bare read stale; refresh tx finalizes; re-assembled snapshot still stale | Attempt fail-closed; no second-round refresh is attempted within one Deposit attempt. |
| Deposit tx builds; on-chain Solend returns `ReserveStale` or `ObligationStale` at simulate / submit time | Gateway surfaces the error upstream; outer wiring MAY treat this as a new attempt and restart from bare read (with refresh as needed). |

### 66.4 Why this over the alternatives

- **vs. "no-refresh fail-closed only":** The spike observed that Solend's bare reads return `stale = true` as the default state. A no-refresh posture makes the live proof depend on a window of chance (some other user's refresh happening in the same slot). That's an unreliable acceptance condition for §71 live proof and reduces the slice's evidence value.
- **vs. "bundled refresh + deposit":** Bundling refresh ixs into the Deposit transaction mixes read-model assembly with transaction preconditioning. The evaluator would evaluate against either pre-refresh state (wrong — the Deposit ix won't succeed against pre-refresh) or post-refresh state predicted by locally simulating the refresh effect (significant new code, and a write-path / policy-boundary entanglement the seam was designed to avoid).

Standalone refresh as a separate tx keeps the seam clean: refresh is a write operation, but it is a **user-session-owned precondition** that completes before any policy-relevant work begins, not a policy-layer concern.

### 66.5 Refresh transaction builder

A parallel, smaller builder to the Deposit builder. Conceptual:

- Input: `SolendRefreshAccounts` (derived from the stale reserves + obligation), session-bound wallet.
- Output: unsigned V0 transaction containing refresh ixs only.
- Lives in `crates/gateway/src/integrations/solend/` alongside the Deposit builder (§62.2).
- Not invoked from within the Deposit builder. Not invoked from the evaluator. Not invoked from any rule.
- Invoked only from outer wiring when the assembler reports `stale = true` on bare read.

## 67. Deposit Transaction Builder

### 67.1 Contract

- **Runs only after `Verdict::Pass`.** Outer wiring is the only call site; the builder has no capability to run earlier.
- **Inputs.**
  - The approved `ProposedAction::Deposit { protocol, reserve_mint, amount }`.
  - The `Part2::LendingSnapshot` the evaluator Passed on (for account derivation — the reserve's supply pubkey, collateral mint, lending market, obligation identity all come from the snapshot).
  - The session-bound wallet (signer authority; from the `SessionBoundWallet` seam).
  - Solend-specific integration-internal account mappings (constants / PDAs derivable from the snapshot + program id).
- **Output.** An unsigned V0 `VersionedTransaction` (or a self-contained instruction bundle ready for the existing signing pipeline). No side effects.

### 67.2 What the builder MUST NOT do

- **Fetch chain state.** Zero RPC. Zero account reads.
- **Run policy.** The builder does not import or invoke any rule, gate, or evaluator entrypoint.
- **Refresh accounts.** Refresh is a separate tx and builder (§66.5). The Deposit tx is a Deposit tx only.
- **Mutate the snapshot.** Snapshot is frozen (Part 2 §12.5).
- **Infer signer from action payload.** The action has no signer field (§49.3); signer identity comes from the session-bound wallet passed in separately.
- **Create hidden dependencies on daemon runtime.** No `approval_store`, `park_store`, or pending-state touches. The builder produces a transaction object; the existing daemon completion path consumes it downstream.

### 67.3 Required Solend accounts for `DepositReserveLiquidityAndObligationCollateral`

The minimum account set (exact ordering per Solend's instruction definition; Part 6B commits only to the set):

- **Source underlying ATA** — the user's associated token account for the reserve's underlying mint (§68).
- **Destination collateral ATA** — the user's associated token account for the reserve's collateral (c-token) mint (§68).
- **Reserve pubkey** — from the snapshot.
- **Reserve liquidity supply pubkey** — decoded from the reserve (§64.2).
- **Reserve collateral mint** — decoded from the reserve.
- **Lending market pubkey** — decoded from the reserve.
- **Lending market authority PDA** — derived from `(lending_market, Solend program id)`.
- **Obligation pubkey** — from the snapshot.
- **Obligation owner / transfer authority** — the session-bound wallet; signer.
- **Clock sysvar**.
- **Token program id**.
- **Solend program id**.

Additional accounts (per Solend's exact instruction signature, which Part 6B does not reproduce verbatim): resolved by the implementation against the cited Solend source.

### 67.4 Precondition: the reserve must be fresh when the Deposit tx reaches the chain

Solend's Deposit instruction checks `reserve.last_update.stale == false` at execution time. If stale, it errors `ReserveStale` (or the equivalent instruction error). V1's flow handles this:

- Fresh snapshot at Pass time + fast handoff to builder + immediate signing + immediate submission = the usual path.
- Mid-flight stale (between Pass and submit): outer wiring reassembles + re-evaluates per §58; the Deposit tx is never submitted with a stale reserve precondition.
- Chain returns `ReserveStale` at simulate / submit time despite the above (rare): gateway error path surfaces cleanly; outer wiring MAY restart the attempt (which would trigger §66's refresh path again).

## 68. ATA Handling

### 68.1 Source underlying ATA

The user's ATA for the reserve's underlying mint MUST exist and MUST hold at least `amount` base units.

- If the ATA account does not exist on chain → **pre-execution HardBlock** with reason "source ATA missing."
- If the ATA exists but balance < `amount` → **pre-execution HardBlock** with reason "insufficient balance."
- The builder does NOT fund the ATA, does NOT transfer from elsewhere, does NOT attempt to derive funds.

Rationale: V1 does not hold user balances. The user must already possess the asset; the policy / builder layer does not synthesize it.

### 68.2 Destination collateral (c-token) ATA

The user's ATA for the reserve's collateral (c-token) mint MAY NOT exist for a first-time depositor.

- **V1 default behaviour: the Deposit transaction bundles a leading `create_associated_token_account` ix if the destination ATA does not exist at build time.**
- ATA creation is a low-risk, idempotent-in-effect operation that does not alter lending risk semantics.
- The create-ATA ix is visible in the transaction's ix list and MUST be recorded in the proof artifact (§71.3).
- ATA creation does NOT constitute a policy bypass — rule evaluation has already `Pass`ed before builder invocation; the builder is merely producing the transaction the Pass authorised.

### 68.3 What ATA handling MUST NOT do

- Silent ATA funding from an unrelated wallet.
- Auto-close of ATAs after the transaction.
- Treatment of ATA creation as a policy input (it is not).

## 69. Transaction Version and Signing Path

### 69.1 V0 preference

The Deposit transaction and the standalone refresh transaction are both built as V0 `VersionedTransaction`. Rationale:

- The Jupiter integration already uses V0 end-to-end (A1-Execute / A2-Execute proofs, §30.5).
- The existing `wallet-signatures` → `complete_v0` → `send_raw_v0_transaction` path is V0-shaped.
- V0 tolerates Phantom's canonicalization behaviour (the resolved-account-identity comparison in `ExternalWalletAdapter::complete_v0` — established during A1-Execute).

Legacy transactions are not introduced for the Solend slice. If a future requirement forces legacy support, that is a separate spec decision.

### 69.2 Signing path — reuse, don't duplicate

- Phantom external-wallet signing.
- Daemon `complete_v0` path at the existing wallet-signatures endpoint.
- Daemon `send_raw_v0_transaction` broadcast.
- No new signer model, no new executor, no new typestate stage.

### 69.3 Separate tx handling for refresh and deposit

When a refresh precondition is required (§66), both the refresh tx and the Deposit tx go through the same signing pipeline — but as **two separate Phantom-signed transactions**, with the refresh tx finalized before the Deposit tx is constructed. Outer wiring sequences them; the signing pipeline does not batch them.

## 70. Deterministic Tests

All tests described below are CI-default, offline (no mainnet / devnet / Phantom required), and deterministic. Fixtures are committed bytes, not live RPC results.

### 70.1 Raw decoder fixture tests

- Obligation size assertion (`len == 1300`). Non-1300 inputs produce pre-evaluation failure.
- Obligation per-field decode against a committed golden obligation fixture (e.g., a snapshot of the spike target's bytes at a known slot). Version, last_update, lending_market, owner, padding-all-zero, deposits_len, borrows_len, each deposit's decoded `(reserve, deposited_amount: CollateralTokenAmount)`, each borrow's decoded `(reserve, borrowed_amount_wads: WadAmount)` match expected values exactly.
- Reserve size assertion (`len == 619`).
- Reserve per-field decode against a committed golden reserve fixture.
- Stale-marker parsing: fixture with `last_update.stale = 1` decodes to `StaleMarker::Stale`; fixture with `last_update.stale = 0` decodes to `StaleMarker::Fresh`.
- Unit-tag domain correctness: attempting to construct `ObligationDeposit { deposited: UnderlyingAmount(…) }` fails to compile (compile-fail test fixture).

### 70.2 Snapshot mapping tests

- `SolendRaw → Part2::LendingSnapshot` happy path: fixture raw decodes map to a well-formed Part 2 snapshot with correct unit tags, a non-empty / empty `OracleFeedSet` per reserve (matching the fixture's oracle configuration), and freshness descriptors populated.
- Missing required field → pre-evaluation failure (e.g., obligation owner all-zero would be anomalous).
- Owner-mismatch HardBlock: mapping produces a valid snapshot; System Invariant Gate run with `context.session_wallet ≠ snapshot.obligation.owner` returns `HardBlock(SystemInvariantFailed(...))`.
- Protocol-tag mismatch HardBlock.

### 70.3 Evaluator tests (against mapped snapshots)

- `Deposit` happy path → `Verdict::Pass`.
- Protocol-native stale → `HardBlock(RuleRejected(RequireFreshState))` (tests both obligation and reserve stale cases).
- `fetched_at` too old vs. snapshot-level clock → `HardBlock(RuleRejected(RequireFreshState))`.
- Oracle publish too old → `HardBlock(RuleRejected(MaxOracleStalenessMs))`.
- Empty `OracleFeedSet` on the acted-on reserve → `HardBlock(RuleRejected(MaxOracleStalenessMs))`.
- `reserve_mint` not in `AllowedMints.allowlist` → `HardBlock(RuleRejected(AllowedMints))`.
- `action.amount > per_mint_caps[reserve_mint]` → `HardBlock(RuleRejected(MaxActionInputAmount))`.
- Disallowed protocol tag in action → `HardBlock(RuleRejected(AllowedLendingProtocols))`.
- `ProposedAction::Borrow` does not compile (compile-fail test).
- `ProposedAction::Withdraw` does not compile (compile-fail test).

### 70.4 Refresh-path tests

- Stale bare read triggers the standalone refresh path (harness asserts the refresh tx is constructed, not bundled into the Deposit tx).
- Refresh success leads to reassemble + gate restart (harness verifies a second assembler call is made post-refresh-confirm).
- Refresh failure (simulated broadcast error) → fail-closed for the attempt (no Deposit tx constructed; clean HardBlock surfaced).
- Refresh tx contains only `RefreshReserve` + `RefreshObligation` ixs; does NOT contain `DepositReserveLiquidityAndObligationCollateral`.
- Deposit tx contains the Deposit ix and (if applicable) the leading `create_associated_token_account` ix; does NOT contain any `RefreshReserve` / `RefreshObligation` ix.

### 70.5 Builder tests

- Builder cannot be invoked before `Verdict::Pass` (type-level or runtime assertion).
- Builder does not fetch chain state (harness with a forbidden RPC mock that panics on any RPC call; builder run against it succeeds).
- Destination collateral ATA missing at build time → builder includes leading `create_associated_token_account` ix.
- Source underlying ATA missing at build time → builder returns a build-level failure (which surfaces as HardBlock upstream).
- Protocol-level rejection handling: fixture or in-process harness simulating Solend ix rejection (supply-cap reached, reserve paused, invalid account combo) at simulate / build time → surfaces cleanly. Post-conditions asserted: no panic, no partial write to `approval_store` / `park_store`, no orphaned pending state, no daemon retry loop.

## 71. Opt-in Live Proof

### 71.1 Run conditions

- Gated behind an env flag (e.g., `CLAW_LIVE_SOLEND_DEPOSIT=1`). NOT CI-default.
- Targets Solana mainnet.
- Small deposit: < $5 USD equivalent (exact cap chosen at run-time, recorded in proof artifact).
- Uses a session-bound Phantom-signed wallet following the A1/A2 pattern.
- Single Solend lending market, chosen by the implementer at run-time from the V1 narrow-scope options (§59).

### 71.2 Path exercised

- Snapshot assembly against live mainnet accounts.
- Standalone refresh tx if `stale = true` on bare read (per §66). Refresh tx signed via Phantom V0, submitted via daemon `send_raw_v0_transaction`, awaited to finalized commitment.
- Re-assembly of snapshot post-refresh.
- Full gate sequence run: System Invariant Gate → Scope Boundary Gate → Rule Evaluation.
- `Verdict::Pass` obtained.
- Deposit tx built (including leading `create_associated_token_account` ix if needed), signed via Phantom V0, submitted via daemon, awaited to finalized commitment.
- Tx signature observable on Solana Explorer.

### 71.3 Proof artifact

Written under `docs/proofs/` (aligns with D-4 target shape). Content:

- Network: `mainnet`; date (UTC).
- Session wallet pubkey.
- Obligation pubkey; reserve pubkey; reserve mint; collateral mint.
- Underlying amount deposited (`UnderlyingAmount` base units + decimals).
- Rule configuration used (snapshot of `LendingRuleConfig`).
- **Refresh tx signature(s)** — listed separately (and marked "refresh" with its own finalized slot), or explicitly marked "no refresh required — bare read was fresh."
- **Deposit tx signature** — listed separately, with finalized slot and Explorer URL.
- Before / after evidence where feasible: source wallet balance delta; destination ATA c-token balance (pre/post); obligation `deposits` array (pre/post) showing the new deposit entry.
- The verdict obtained (`Pass`) and the rule config at evaluation time.
- Referenced from a new project-memory entry following the pattern of `project_a1_execute_proof` / `project_a2_execute_proof`.

### 71.4 Non-dependencies and isolation

- Production path MUST NOT import or invoke anything under `spikes/solend_read/`.
- CI MUST NOT require the env flag; unset = the live test is skipped silently.
- The live proof MUST NOT rely on any non-public RPC endpoint; a publicly-reachable mainnet RPC is sufficient (with reasonable rate-limit tolerance).
- The live proof MUST NOT depend on specific wallet funding beyond the small deposit amount + estimated transaction fees.

## 72. Out of Scope for Part 6B

Explicitly deferred to later spec prompts or marked permanently out of V1:

- **Repay** — follow-up slice after Deposit sealed; Part 6 reopens with a narrow Repay-only prompt.
- **Withdraw, Borrow** — remain Part 1 §3.2 scope-boundary blocked.
- **LTV / health factor / borrow headroom** — Part 4B future; not V1.
- **USD valuation** — Part 4B future; not V1.
- **Oracle price / confidence decoding** — not V1 (§65.2).
- **Feed aggregation beyond freshness preservation** — §11.1 / §34.2 / §39.2.
- **Cross-protocol abstraction / generic `LendingProtocol` trait** — Part 5B §50.2.
- **Cache / TTL / retry policy beyond a single Deposit attempt** — Part 5A §42.3.
- **Relayer-paid refresh optimization** — a plausible future UX improvement (third party pays for the refresh tx to reduce user friction); not V1.
- **Frontend integration** — minimum UI for Deposit is slice-scope; polish, multi-step wizard, error-UX enrichment are NOT.
- **Multiple Solend markets in one slice** — §59.
- **Obligation auto-init** — §57.6; HardBlock with clear reason is V1 behaviour.
- **Alternative protocols** — §55 choice is Solend-only for V1.
- **Any bundled refresh+deposit variant** — §66.4.

## 73. What Part 6B Does Not Authorize

Part 6B is an implementation-spec contract. It deliberately does **not**:

- **Write production code.** No `.rs` files created, no existing files modified, no Rust compiled.
- **Create CI changes, fixtures, or test runners.** Fixtures are designed here (§70), not committed.
- **Modify `Cargo.toml`.** The gateway integration lives inside an existing crate; no workspace or dependency changes are authorized by Part 6B.
- **Perform the live proof.** §71 defines the criteria; running the proof requires a separate authorization + wallet funding + observer discipline.
- **Reopen any frozen decision from Parts 1 – 6A.**
- **Pick the specific Solend lending market.** Implementation chooses from V1-scope options at implementation time.
- **Commit to leaf file names inside `integrations/solend/`.** Implementation chooses; §62.3's module-doc discipline applies to whatever is created.
- **Authorize Repay / Withdraw / Borrow work.** Full stop.

What Part 6B does authorize:

- **Writing a Deposit-only implementation PR** (by a separate, explicit prompt that treats §62 – §72 as the implementation contract), targeting the gateway-integration placement and passing the §70 deterministic tests.
- **Running the §71 live proof** once the deterministic tests pass, producing a proof artifact under `docs/proofs/`.

---

Part 6B closes the Solend Deposit-only implementation-spec surface. Actual implementation work (Rust code, tests, proof run) remains gated by a separate prompt that cites this part as its scope contract. The vertical-slice runway is now type-shaped (Part 5B), protocol-targeted (Part 6A), and implementation-bounded (Part 6B) — still unpaved.
