# ClawSolana — Roadmap

**North Star:** Evolve ClawSolana from a secure, typestate-enforced control plane into an **Autonomous & Intent-Based DeFi Orchestrator** on Solana.

**Created:** 2026-03-19
**Baseline:** Session 12 complete (108 tests, zero failures, V1 control plane operational)
**Companion:** [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) — system invariants, plane model, failure modes, phase gates

---

## Target Scenarios

The roadmap is structured to incrementally unlock these 4 core scenarios:

| # | Scenario | Unlocked At |
|---|----------|-------------|
| S1 | **Dynamic CLMM Liquidity Management** — AI reads pool state, analyzes toxicity/volatility, proposes defensive LP range adjustments as single-tx proposals | Phase 2 |
| S2 | **Cross-Protocol Position Restructuring** — AI detects risk (Solend Health Factor drop), calculates optimal cross-protocol path, compiles into single Transaction Proposal | Phase 2–3 |
| S3 | **Intent-Based Operations via Channels** — Users send fuzzy text/voice via Telegram, LLM translates to precise Solana instructions, pushes to Phantom for signing | Phase 4 |
| S4 | **On-Chain Emergency Escape Pod** — External listener detects protocol hack/panic, AI constructs emergency withdraw proposal, pushes to user for immediate signing | Phase 3 |

---

## Phase 0 — Production Readiness & V1 Foundation

**Goal:** Stabilize the existing control plane for safe devnet/testnet operation. Fix all P0 bugs and critical P1 gaps before building new features on top.

**Duration estimate:** 2–3 sessions

### 0.1 — P0 Critical Fixes
- [ ] **P0-1:** Atomic `take_and_verify()` — eliminate concurrent double-accept race in `submit_signed_tx_inner`
- [ ] **P0-2:** Request expiry reaper — add `parked_at: Instant` to pending wallet signatures, implement Tokio reaper task (~120s TTL)
- [ ] **P0-3:** Session-request binding check — verify `entry.proposal.session_id == session_id` before accepting submission

### 0.2 — P1 Hardening
- [ ] **P1-1:** Persist pending state to SQLite — `PendingSigningStore`, `ApprovalStore`, `ExternalWalletStore` must survive restart
- [ ] **P1-2:** Rate limiting on submit endpoint (Tower rate-limit middleware)
- [ ] **P1-3:** `bind_wallet` challenge-response — require signed nonce to prove wallet ownership
- [ ] **P1-4:** Durable event log — persist events to SQLite, add replay endpoint

### 0.3 — Technical Debt
- [ ] **N12:** Context trimming preserves tool_use/tool_result pairs atomically
- [ ] **N13:** Two-pass compute budget optimization (simulate → tighten CU → re-simulate)
- [ ] Clean up compiler warnings (unused imports, missing docs on public items)

### 0.4 — On-Chain Submission
- [ ] Implement `sendTransaction` after verification passes (with retry + confirmation tracking)
- [ ] Add `TransactionStatus` tracking (Pending → Confirmed → Finalized / Failed)
- [ ] Wire confirmation status into SSE events and audit trail

**Exit Criteria:** All P0 fixed, P1-1 through P1-4 resolved, transactions actually land on devnet, 120+ tests passing.
**Phase Gate:** See [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) §5 Gate 0→1 for mandatory criteria.

---

## Phase 1 — Single-Protocol DeFi Execution (Orca Whirlpools)

**Goal:** Graduate from read-only to full DeFi execution against a single protocol (Orca). Establish the `DeFiAction` trait pattern that all future protocols will implement.

**Duration estimate:** 3–4 sessions

### 1.1 — DeFi Action Abstraction
- [ ] Define `DeFiAction` trait in `claw-types`:
  ```
  trait DeFiAction: Send + Sync {
      fn protocol(&self) -> ProtocolId;
      fn describe(&self) -> String;           // human-readable summary
      fn build_instructions(&self) -> Result<Vec<Instruction>>;
      fn required_accounts(&self) -> Vec<Pubkey>;
      fn estimated_compute_units(&self) -> u32;
  }
  ```
- [ ] Define `ProtocolAdapter` trait — registry-compatible protocol interface
- [ ] Create `claw-defi-core` crate for shared DeFi types (`PoolState`, `Position`, `SwapParams`, `LiquidityParams`)

### 1.2 — Orca Whirlpool Execution Tools
- [ ] `orca_swap` tool — build swap instruction via Whirlpool SDK, with slippage parameter
- [ ] `orca_open_position` tool — open CLMM position with tick range
- [ ] `orca_close_position` tool — withdraw liquidity + collect fees + close position
- [ ] `orca_increase_liquidity` / `orca_decrease_liquidity` tools
- [ ] Full CL tick-array-aware quote (replace constant product approximation)

### 1.3 — Transaction Composer
- [ ] `TransactionComposer` — combines multiple `DeFiAction`s into a single `TransactionProposal`
  - Deduplicates accounts, sequences instructions, respects compute limits
  - Supports "Withdraw → Swap → Provide Liquidity" as atomic bundle
- [ ] Integrate composer into the existing typestate pipeline (Compose → Propose → Simulate → Policy → Approve → Sign)
- [ ] Add `ComposedTransaction` audit event type

### 1.4 — Phantom Bridge E2E
- [ ] Browser extension / web app for Phantom deep-link signing flow
- [ ] QR code fallback for mobile Phantom
- [ ] SSE-driven UI: show pending proposals, approval status, tx confirmation
- [ ] E2E test: agent proposes Orca swap → user signs via Phantom → tx lands on devnet

### 1.5 — Pool State Analysis Tools
- [ ] `orca_analyze_pool` — current tick, liquidity distribution, recent volume, fee tier
- [ ] `orca_position_pnl` — calculate IL, fees earned, net PnL for an open position
- [ ] Token price feed integration (Pyth/Switchboard oracle reads)

**Exit Criteria:** Agent can propose and execute Orca swap + LP operations via Phantom signing on devnet. Transaction Composer bundles multi-step DeFi operations atomically. 150+ tests.
**Phase Gate:** See [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) §5 Gate 1→2 for mandatory criteria.

---

## Phase 2 — Multi-Protocol & Risk Engine Expansion

**Goal:** Add Solend (lending) integration, build a cross-protocol risk engine, and enable the AI to act as a Risk Manager that restructures positions across protocols.

**Duration estimate:** 4–5 sessions

### 2.1 — Protocol Adapter Registry
- [ ] `ProtocolRegistry` — dynamic registration of `ProtocolAdapter` implementations
- [ ] Plugin-style loading: each protocol is a separate crate (`claw-proto-orca`, `claw-proto-solend`, `claw-proto-jupiter`)
- [ ] Unified tool generation: `ProtocolAdapter` auto-registers tools into `ToolRegistry`
- [ ] Protocol health checks (RPC reachability, program deployment verification)

### 2.2 — Solend Integration
- [ ] `solend_deposit` / `solend_withdraw` / `solend_borrow` / `solend_repay` tools
- [ ] `solend_get_obligation` — read user's borrow/lend positions, Health Factor
- [ ] `solend_get_reserve` — read pool utilization, borrow rate, supply rate
- [ ] Health Factor monitoring: `solend_check_health` tool with configurable thresholds

### 2.3 — Jupiter Aggregator Integration
- [ ] `jupiter_quote` — fetch best swap route across all Solana DEXs
- [ ] `jupiter_swap` — build swap instruction from Jupiter route
- [ ] Use Jupiter as default swap router (better execution than single-pool Orca swap)

### 2.4 — Cross-Protocol Risk Engine
- [ ] `RiskAssessor` trait — protocol-agnostic position risk evaluation
- [ ] Health Factor simulator: "If SOL drops 20%, what happens to my Solend position?"
- [ ] Impermanent Loss calculator for CLMM positions
- [ ] Cross-protocol net exposure calculator (sum positions across Orca, Solend, etc.)
- [ ] `PositionRestructuringPlanner` — given risk threshold violation, compute optimal rebalancing path
  - E.g., "Withdraw 20% USDC from Orca → Swap to SOL via Jupiter → Repay Solend debt"
  - Output: ordered list of `DeFiAction`s → feed to `TransactionComposer`

### 2.5 — Policy Engine V2
- [ ] Protocol-specific policy rules (e.g., "max 30% of portfolio in any single pool")
- [ ] Cross-protocol exposure limits
- [ ] Configurable risk thresholds per agent session
- [ ] Policy rule DSL or config-driven rule engine

**Exit Criteria:** Agent can monitor Solend Health Factor, detect risk, propose cross-protocol rebalancing as a composed transaction. Scenarios S1 (CLMM management) and S2 (cross-protocol restructuring) are functional on devnet. 200+ tests.
**Phase Gate:** See [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) §5 Gate 2→3 for mandatory criteria.

---

## Phase 3 — Autonomous Observers & Triggers

**Goal:** The AI no longer waits for user messages. It wakes up on schedules, reacts to on-chain events, and detects emergencies autonomously.

**Duration estimate:** 3–4 sessions

### 3.1 — Cron/Scheduler Subsystem
- [ ] `claw-scheduler` crate — Tokio-based periodic task runner
- [ ] Configurable schedules per agent session (e.g., "check Orca positions every 15 minutes")
- [ ] Schedule persistence in SQLite (survive restarts)
- [ ] Schedule → Agent wakeup → ReAct loop → Transaction Proposal (if action needed)
- [ ] Audit trail for autonomous wakeups

### 3.2 — On-Chain Event Listeners
- [ ] `claw-observers` crate — pluggable event source framework
- [ ] Solana WebSocket subscription: account changes, program logs
- [ ] Filtered observers: "Watch this Solend obligation account for Health Factor < 1.2"
- [ ] Observer → Event → Agent wakeup pipeline

### 3.3 — External Signal Adapters
- [ ] Webhook receiver endpoint — accept alerts from external monitoring systems
- [ ] Twitter/social sentiment listener (via external API) — panic keyword detection
- [ ] Protocol TVL monitor — detect massive outflows from specific protocols
- [ ] News feed adapter — macro event detection (rate decisions, regulatory news)

### 3.4 — Emergency Escape Pod (Scenario S4)
- [ ] `EmergencyDetector` — aggregates signals (TVL drop > threshold, social panic score, Health Factor critical)
- [ ] `EmergencyWithdrawPlanner` — constructs "withdraw everything from protocol X" transaction
- [ ] Priority notification pipeline — bypass normal SSE, push directly to mobile/desktop
- [ ] Configurable emergency thresholds and enabled protocols
- [ ] Dead man's switch: if user doesn't respond within N minutes, auto-escalate notification
- [ ] Audit trail: full reasoning chain logged for every emergency trigger

### 3.5 — Flow Toxicity & Volatility Analysis
- [ ] Historical trade flow analysis for Orca pools (MEV, toxic flow detection)
- [ ] Realized/implied volatility estimation from on-chain data
- [ ] Feed analysis results into CLMM range recommendation engine
- [ ] "Defensive mode" — auto-narrow LP range during high volatility

**Exit Criteria:** Agent autonomously monitors positions, wakes up on schedule, detects emergencies, and proposes protective actions. Scenario S4 (Emergency Escape Pod) is functional. 250+ tests.
**Phase Gate:** See [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) §5 Gate 3→4 for mandatory criteria.

---

## Phase 4 — Intent-Based UX & Channels

**Goal:** Users interact with ClawSolana through natural language on any channel. The LLM translates fuzzy intents into precise DeFi operations.

**Duration estimate:** 3–4 sessions

### 4.1 — Intent Parsing Engine
- [ ] `DeFiIntent` type hierarchy — structured representation of user goals:
  ```
  IntentKind::Swap { from, to, amount_or_pct, urgency }
  IntentKind::ProvideLiquidity { pool, range_hint, amount }
  IntentKind::WithdrawAll { protocol, asset_filter }
  IntentKind::Rebalance { target_allocation }
  IntentKind::EmergencyExit { protocol }
  IntentKind::YieldOptimize { asset, risk_tolerance }
  ```
- [ ] LLM-powered intent extraction — fuzzy text → `DeFiIntent`
- [ ] Intent validation and disambiguation ("Did you mean USDC or USDT?")
- [ ] Intent → `DeFiAction` compilation pipeline
- [ ] Slippage/compute budget inference from intent urgency

### 4.2 — Telegram Channel Adapter
- [ ] `claw-channel-telegram` crate — Telegram Bot API integration
- [ ] Message receiving: text, voice (→ speech-to-text → intent)
- [ ] Rich response formatting: transaction proposal cards with Approve/Reject buttons
- [ ] Deep-link to Phantom mobile for signing
- [ ] Per-user authentication and session management
- [ ] Rate limiting and abuse prevention

### 4.3 — Notification & Push System
- [ ] `claw-notify` crate — unified notification dispatch
- [ ] Channels: Telegram, push notification (APNs/FCM), email, webhook
- [ ] Priority levels: Info, Warning, Urgent, Emergency
- [ ] User notification preferences (which channels for which priority)
- [ ] Delivery confirmation and retry

### 4.4 — Voice Intent Processing
- [ ] Speech-to-text integration (Whisper API or similar)
- [ ] Voice → text → intent pipeline
- [ ] Confirmation flow: "I understood you want to [action]. Shall I proceed?"

### 4.5 — Portfolio Dashboard API
- [ ] `GET /portfolio` — aggregated cross-protocol position summary
- [ ] `GET /portfolio/history` — historical PnL, fee income, IL
- [ ] `GET /portfolio/risk` — current risk metrics across all protocols
- [ ] WebSocket feed for real-time portfolio updates

**Exit Criteria:** Users can send "Liquidate all my shitcoins down >20% into USDC and deposit into the highest yield pool" via Telegram and receive a Phantom signing request. Scenario S3 (Intent-Based Operations) is fully functional. 300+ tests.
**Phase Gate:** See [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) §5 Gate 4→v1.0 for mandatory criteria.

---

## Cross-Cutting Concerns (All Phases)

These capabilities evolve continuously across all phases:

### Security & Audit
- Every new capability gated by `CapabilitySet` — no tool runs without explicit grant
- Every autonomous action logged to append-only audit trail with full reasoning chain
- Human-in-the-loop remains mandatory for all transaction signing (no auto-sign, ever)
- Emergency actions still require user signature — the AI proposes, never executes unilaterally

### Observability
- Prometheus metrics for all subsystems (Phase 0+)
- Grafana dashboards for autonomous agent behavior (Phase 3+)
- Structured tracing with correlation IDs across agent wakeups
- Alert rules for anomalous agent behavior

### Testing Strategy
- Unit tests for all new types and logic
- Integration tests with simulated on-chain state (solana-test-validator)
- E2E tests per scenario with mock external services
- Chaos tests for observer/trigger subsystems (delayed signals, duplicate events)
- Target: 300+ tests by Phase 4 completion

### Configuration
- All thresholds, schedules, and protocol parameters configurable via `config/*.toml`
- Per-session overrides via API
- Hot-reload for non-critical config changes

---

## Dependency Graph

```
Phase 0 ─── Production Readiness
   │
   ▼
Phase 1 ─── Single-Protocol Execution (Orca)
   │         ├── DeFiAction trait
   │         ├── Transaction Composer
   │         └── Phantom E2E
   │
   ▼
Phase 2 ─── Multi-Protocol & Risk Engine
   │         ├── Protocol Registry
   │         ├── Solend + Jupiter
   │         └── Cross-Protocol Risk Engine
   │
   ├────────────────────┐
   ▼                    ▼
Phase 3              Phase 4 (can start 4.1–4.2 in parallel with Phase 3)
Autonomous           Intent-Based UX
Observers            ├── Intent Parser
├── Scheduler        ├── Telegram Adapter
├── Event Listeners  └── Voice Processing
└── Emergency Pod
```

**Note:** Phase 3 and Phase 4 have partial parallelism. Intent parsing (4.1) and Telegram adapter (4.2) can begin once Phase 2 DeFi execution is solid, even while Phase 3 observers are still being built. The full S3 scenario requires both phases complete.

---

## Versioning

| Milestone | Version | Key Deliverable |
|-----------|---------|-----------------|
| Phase 0 complete | v0.2.0 | Production-safe V1 control plane |
| Phase 1 complete | v0.3.0 | Single-protocol DeFi execution |
| Phase 2 complete | v0.5.0 | Multi-protocol risk management |
| Phase 3 complete | v0.7.0 | Autonomous operation |
| Phase 4 complete | v1.0.0 | Intent-based DeFi orchestrator |

---

## Principles

1. **Human signs, AI proposes.** No phase ever introduces auto-signing. The user always holds the keys.
2. **Compile-time safety first.** Every new pipeline stage uses typestate enforcement. If it compiles, it's safe.
3. **Fail-closed on ambiguity.** If the AI isn't sure, it asks. If a policy check fails, the action is blocked.
4. **Audit everything.** Every autonomous wakeup, every emergency trigger, every intent parse — logged with full reasoning.
5. **One protocol at a time.** We master Orca before adding Solend. We master two protocols before adding intent parsing.
