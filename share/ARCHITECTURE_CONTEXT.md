# ClawSolana — Architecture Context
# (Shareable summary for external review — current as of V1.5 STEP 1)

---

## What Is This Project?

ClawSolana is a production-grade, Rust-first, Solana-native AI agent control plane.
It runs as a local daemon (`clawd`) that connects an LLM (Anthropic Claude) to the
Solana blockchain via a typed tool system and a policy-gated transaction pipeline.

Operators interact via a CLI client (`claw`) or the HTTP API.

**Key properties:**
- Simulation-first: every transaction is simulated before policy evaluation — enforced at compile time via typestate
- Fail-closed: if no policy rule matches, human approval is required
- Local-only API: binds 127.0.0.1, never exposed to the network
- Per-session least-privilege: capability set is narrowed to each session's `AgentRole` at construction time
- Full audit trail: every tool invocation start/finish is persisted to SQLite
- No stringly-typed patterns: every domain boundary is a named Rust type

---

## Crate Dependency Graph (13 crates)

```
claw-types          ← zero deps (domain vocabulary only)
    ↑
claw-observability  ← tracing, health checks, metrics
claw-state-store    ← SQLite via sqlx (runtime queries, no DATABASE_URL needed)
claw-solana-core    ← RpcPool, ClawRpcClient, BlockhashManager, SimulationClient
claw-wallet-engine  ← SecretKeystore, signing pipeline, typestate invariants
claw-risk-engine    ← PolicySet, SpendTracker, rule evaluation
claw-tool-system    ← ToolRegistry, ToolDispatcher, CapabilitySet, 6 tools
claw-agent-runtime  ← ReAct loop, AnthropicClient, personas, ToolTraceRepository wiring
claw-channels       ← Channel trait, CliChannel
claw-api            ← axum HTTP server, auth middleware, route handlers
claw-gateway        ← ClawConfig, EventBus, SessionManager, GatewayDaemon
    ↑
bins/clawd          ← daemon binary (clap CLI → GatewayDaemon::run)
bins/claw           ← client binary (clap CLI → HTTP API calls)
```

Dependency rules:
- `claw-api` does NOT depend on `claw-gateway` (avoids circular dep)
- `claw-types` has zero intra-workspace deps (enforced permanently)
- `claw-gateway` depends on `claw-api` via the `SessionOps` trait pattern
- `claw-agent-runtime` depends on `claw-state-store` for tool trace persistence

---

## Startup Sequence (GatewayDaemon::run)

1. Parse CLI args (clap) → load `ClawConfig` from TOML
2. Initialize tracing subscriber (JSON or pretty format, RUST_LOG filter, no unsafe)
3. Open SQLite database, run migrations
4. Build `RpcPool` with circuit breaker (failure_threshold=3, recovery=30s)
5. Wrap pool in `ClawRpcClient` (commitment=Confirmed)
6. Spawn `BlockhashManager::run_refresh_loop` as supervised background task
7. Build `SimulationClient` from rpc_client
8. Build `ToolRegistry` via `default_registry(rpc_client, sim_client)` — shared Arc-backed registry
9. Build `AnthropicClient` LLM from config.llm.api_key + config.llm.model
10. Create `Agent` (LLM only — NO dispatcher stored on Agent)
11. Create `AgentRouter`
12. Create `EventBus` (tokio::sync::broadcast, capacity 1024)
13. Create `SessionManager` (DashMap-backed, publishes lifecycle events to EventBus)
14. Build `PolicySet` (mainnet_safe_default or permissive_default based on network)
15. Wire `AuditRepository` + `ToolTraceRepository` into `GatewayMessageHandler`
16. Load/generate API token (CLAW_API_TOKEN env or ephemeral UUID)
17. Start axum HTTP server on 127.0.0.1:7070
18. Await SIGINT/SIGTERM → graceful shutdown

---

## Per-Session Capability Narrowing (V1.5)

Every session is born with the narrowest capability set that still lets it do its job.

```
AgentRole::Research  → ReadWallet, ReadChain, SimulateTransaction
AgentRole::Execution → + BuildTransaction
AgentRole::Risk      → ReadChain, ReadWallet, ManageSubscriptions
AgentRole::Ops       → All except SignTransaction, SendTransaction
```

`SignTransaction` and `SendTransaction` are NEVER granted at session construction.
They are not even in `CapabilitySet::default_agent()`.

At each `handle_inner` call in `GatewayMessageHandler`:
1. `AgentRouter::session_role(session_id)` → `AgentRole`
2. `CapabilitySet::for_role(role)` → narrowed capability set
3. `ToolDispatcher::with_capabilities(registry.clone(), caps)` → per-call dispatcher
4. Dispatcher passed to `AgentRouter::dispatch()` → `Agent::run()`

`Agent` no longer owns a `ToolDispatcher` — it receives one per `run()` call.

---

## Tool Trace Persistence (V1.5 Audit Closure)

Every tool invocation is persisted to the `tool_traces` SQLite table.

In `Agent::run()`, for each LLM tool call:
1. `tool_traces.start(trace_id, session_id, correlation_id, tool_name, input)` — before execution
2. `dispatcher.dispatch(tool_input)` — actual execution with capability check + timeout
3. `tool_traces.finish(trace_id, status, output, error, duration_ms)` — after execution

`ToolTraceStatus`: `Succeeded`, `Failed`, `TimedOut`, `Cancelled`, `Running`

In addition, `GatewayMessageHandler` persists agent-level events to `AuditRepository`:
- `agent_task_started` — includes `agent_role` field
- `agent_task_completed` — includes tool call count
- `agent_task_failed` — includes error string

---

## Transaction Pipeline (Typestate-enforced)

```
Agent builds TransactionProposal
    ↓
pipeline::simulate(proposal, sim_client) → SimulatedTransaction
    ↑ Only constructible if simulation.success == true
    ↓
pipeline::evaluate_policy(simulated, policy_set) → ApprovedTransaction
    ↑ Only constructible if verdict is not PolicyBlocked
    ↓
pipeline::sign(proposal, approved, signer) → PipelineResult
    ↑ Sign() signature requires ApprovedTransaction — compile error to pass raw Transaction
```

`PolicyVerdict` is NEVER a boolean. Every verdict carries `rule_name` and `reason`.

---

## ReAct Agent Loop (Agent::run)

```
receive AgentCommand (text + session context) + ToolDispatcher + ToolTraceRepository
    ↓
loop (max 10 iterations):
    session.context.push_user(command.text)       ← TRUSTED UserInstruction
    LlmClient.complete(system_prompt, context.messages(), tool_specs)
        ↓
    if tool_calls:
        for each tool:
            tool_traces.start(...)
            ToolDispatcher.dispatch(tool) ← capability check first, then timeout
            tool_traces.finish(...)
            context.push_tool_result(...)  ← UNTRUSTED, prefixed "[TOOL RESULT — UNTRUSTED DATA]"
        continue
    else:
        context.push_assistant(text)
        return AgentResponse
```

The tool/LLM boundary is the key prompt-injection defense:
- `push_user()` → `UserInstruction` — sent verbatim to LLM
- `push_tool_result()` → `ToolResult` — prefixed with untrusted-data warning

---

## HTTP API

```
GET    /health                    → HealthRegistry::check_all() (public)
GET    /sessions                  → list active sessions (bearer auth)
POST   /sessions                  → open session { role, channel }
DELETE /sessions/{id}             → close session
POST   /sessions/{id}/messages    → send command, get AgentResponse (bearer auth)
```

Auth: constant-time XOR-fold bearer token comparison (no short-circuit).
Binding: 127.0.0.1 only — never 0.0.0.0.
Token: if `CLAW_API_TOKEN` is set → loaded silently (preview only in trace log).
       if not set → ephemeral UUID printed to stderr ONCE on startup.

---

## Security Invariants

| Invariant | Enforcement |
|-----------|-------------|
| Simulation before sign | Typestate: `sign()` requires `ApprovedTransaction`, not `Transaction` |
| Policy before sign | Typestate: `ApprovedTransaction` only from non-blocked verdict |
| Capability check before tool exec | `ToolDispatcher::dispatch()` → `PermissionDenied` if missing |
| Sign/Send never default | Not in `default_agent()`, not in any `for_role()` output |
| Tool results tagged untrusted | `push_tool_result()` adds prefix; LLM sees warning |
| Audit log append-only | `AuditRepository` has no update/delete methods |
| No unsafe code | `#![forbid(unsafe_code)]` in all crates |
| API local-only | Hard-coded `127.0.0.1` bind; no configurable override |
| Bearer token constant-time | XOR fold, not `==` |
| Secret key redacted | `SecretKeypair::fmt()` → `[REDACTED]`; no Serialize |

---

## Known Remaining Gaps (V1.5 state)

| Item | Status |
|------|--------|
| Human approval round-trip | `AwaitingApproval` state exists, no `POST /sessions/{id}/approve` yet |
| SSE event stream | No `GET /sessions/{id}/events` |
| Wallet loading at startup | Config supports wallet entries; runtime loading not wired |
| DeFi read-only tools | No Orca/Solend protocol tools yet (STEP 2) |
| Ledger hardware wallet | Stub only |

---

## Config (config/default.toml summary)

```toml
[daemon]
db_path = "./data/claw.db"

[network]
network = "devnet"   # devnet | testnet | mainnet-beta | localnet

[rpc]
primary_url = "https://api.devnet.solana.com"
timeout_ms  = 15000

[policy]
mainnet_safe_defaults = true  # STRONGLY recommended

[llm]
api_key = ""   # Set via CLAW_LLM_API_KEY env var
model   = "claude-sonnet-4-6"

[api]
bind_addr = "127.0.0.1"  # Never change to 0.0.0.0
port      = 7070

[logging]
format = "pretty"   # or "json" for production
level  = "info,claw_gateway=debug,claw_agent_runtime=debug"
```

Environment variable overrides:
- `CLAW_LLM_API_KEY` — Anthropic API key
- `CLAW_RPC_URL` — override primary RPC endpoint
- `CLAW_API_TOKEN` — set a persistent token (if unset, ephemeral UUID printed to stderr)
- `RUST_LOG` — log filter
