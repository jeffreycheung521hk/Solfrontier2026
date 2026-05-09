//! Stage 2 Watcher Scheduler — lifecycle substrate over the W1
//! [`Stage2WatchRuleRepository`].
//!
//! This module is the W2 deliverable: it makes Stage 2 watch rules
//! observable and tickable. On each tick it:
//!
//! 1. Loads up to `max_rules_per_tick` active rules from the W1
//!    repository (bounded — never unbounded-load the whole table).
//! 2. Garbage-collects rules whose `expires_at_slot <= current_slot`
//!    via [`Stage2WatchRuleRepository::mark_expired_if_not_terminal`]
//!    *before* attempting evaluation.
//! 3. For each remaining active rule, acquires a per-rule in-flight
//!    guard, calls the injected
//!    [`Stage2ConditionEvaluator`], and (on `Ok(true)`) the injected
//!    [`Stage2ExecutionSimulator`] stub, updating lifecycle state
//!    via TOCTOU-safe repo helpers.
//!
//! # Hard scope boundaries (W2)
//!
//! This slice MUST NOT call live RPC, sign, broadcast, or build
//! Solend/Jupiter transactions. The simulator slot is a **stub** —
//! it returns `Ok(())` by default and is the seam where future
//! W3/I1 work attaches the real execution path. Forcing those calls
//! here would violate the "scheduler + lifecycle substrate only"
//! constraint of this prompt.
//!
//! # Concurrency / TOCTOU discipline
//!
//! - The in-flight set is a tiny `HashSet<[u8; 16]>` behind a
//!   `parking_lot::Mutex`. Critical sections only contain HashSet
//!   ops; no `.await` is held across the lock. The
//!   [`InFlightGuard`] removes the rule_id on drop, including drop
//!   on panic / error paths.
//! - Lifecycle transitions advance via the repo's `*_if_active` /
//!   `*_if_not_terminal` helpers (added in this slice). The
//!   `WHERE status = ...` clause means a row that was externally
//!   marked revoked / completed / expired between our load and our
//!   transition cannot be overwritten — the UPDATE matches zero rows
//!   and the watcher records that the race was lost.
//!
//! # What's deferred
//!
//! - HTTP route (`GET /stage2/watcher/health`,
//!   `POST /stage2/watcher/force-tick`) — A2 dependency; this slice
//!   exposes only the internal API. See [`Stage2Watcher::health`]
//!   and [`Stage2Watcher::force_tick`] / [`Stage2Watcher::force_tick_rule`].
//! - Backoff / jitter on transient retries — recorded in
//!   [`Stage2WatcherConfig`] notes and the W3 prompt; W2 records the
//!   transient error and lets the next tick retry.
//! - Real evaluator / simulator implementations — W3+ owns those.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tracing::{debug, info, warn};

use claw_state_store::stage2_watch_rules::{
    Stage2WatchRuleRepository, StoredWatchRule, WatchRuleStatus,
};
use claw_state_store::StoreError;
use claw_types::stage2_watch_rule::WatchRule;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors raised by the W2 watcher and its injected evaluator /
/// simulator.
///
/// Three classes per the prompt's "Error typology" section:
///
/// - **Transient** — leave the rule active, record `last_error`, no
///   condition_met, retry on next tick.
/// - **Terminal** — flip the rule to `failed` (only if it's still in
///   a non-terminal status), record `last_error`.
/// - **Internal** — bubble up in the tick report; the tick is NOT
///   recorded as successful.
#[derive(Debug, thiserror::Error)]
pub enum Stage2WatcherError {
    /// Caller-supplied evaluator / simulator returned a transient
    /// failure (RPC timeout, oracle unavailable, rate limited).
    /// The watcher records `last_error` and leaves the rule active.
    #[error("transient: {0}")]
    Transient(String),

    /// Caller-supplied evaluator / simulator returned a terminal
    /// failure (invalid rule shape, unsupported action, canonical
    /// hash mismatch). The watcher flips the rule to `failed`.
    #[error("terminal: {0}")]
    Terminal(String),

    /// Internal error inside the watcher itself (DB error,
    /// serialization, invariant violation). Tick report records
    /// this; the rule is NOT marked failed because the failure
    /// originates inside the watcher infrastructure, not the rule
    /// or its environment.
    #[error("internal: {0}")]
    Internal(String),
}

impl Stage2WatcherError {
    /// `true` if this error is a [`Self::Transient`] variant.
    /// Drives the "leave active + record last_error" branch.
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    /// `true` if this error is a [`Self::Terminal`] variant. Drives
    /// the "mark_failed_if_not_terminal" branch.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }

    /// `true` if this error is a [`Self::Internal`] variant.
    pub const fn is_internal(&self) -> bool {
        matches!(self, Self::Internal(_))
    }
}

impl From<StoreError> for Stage2WatcherError {
    fn from(value: StoreError) -> Self {
        Self::Internal(format!("state-store: {value}"))
    }
}

// ── Tick context ────────────────────────────────────────────────────────────

/// Tick context — kept as three explicit fields so callers cannot
/// accidentally conflate Solana slot, Pyth/Solend Unix-timestamp
/// freshness, and wall-clock-millis health. Per the prompt:
///
/// > Authorization expiry uses Solana slot (`expires_at_slot`)
/// > Pyth freshness uses Unix timestamp / publish_time
/// > health uses wall-clock milliseconds
///
/// Tests pin that expiry uses [`Self::current_slot`], not
/// [`Self::current_unix_timestamp`].
#[derive(Debug, Clone, Copy)]
pub struct Stage2TickContext {
    /// Solana cluster slot at tick start. Drives expiry, replay,
    /// and any on-chain ordering decisions.
    pub current_slot: u64,
    /// Solana cluster Unix timestamp (seconds). Drives Pyth /
    /// Solend freshness checks (W3+).
    pub current_unix_timestamp: i64,
    /// Wall-clock millis at tick start. Drives health surfacing
    /// (last_tick_*_at_ms, offline threshold).
    pub now_ms: i64,
}

impl Stage2TickContext {
    /// Convenience constructor.
    pub const fn new(
        current_slot: u64,
        current_unix_timestamp: i64,
        now_ms: i64,
    ) -> Self {
        Self {
            current_slot,
            current_unix_timestamp,
            now_ms,
        }
    }
}

// ── Config ──────────────────────────────────────────────────────────────────

/// Watcher configuration. All thresholds live here, not in code, so
/// operators can tune without recompiling — and tests can pin tight
/// thresholds without globally changing constants.
#[derive(Debug, Clone)]
pub struct Stage2WatcherConfig {
    /// Maximum rules processed per tick. Keeps the watcher
    /// bounded — a 100k-rule corpus does NOT trigger a 100k-rule
    /// tick. Default `500`.
    pub max_rules_per_tick: u32,

    /// Watcher tick interval (informational at this level — the
    /// outer driver decides cadence). Default `5_000` ms.
    pub tick_interval_ms: u64,

    /// Health "offline" threshold. If `now_ms - last_successful_tick
    /// > offline_threshold_ms`, [`Stage2Watcher::is_offline`] returns
    /// `true`. Default `300_000` ms (5 minutes), per spec § 9.2.
    pub offline_threshold_ms: i64,

    /// Per-rule "stale" threshold for health-surface counting. An
    /// active rule whose `last_successful_tick_at_ms` is older than
    /// this (or that has never been ticked AND was created longer
    /// ago than this) is counted in `stale_rule_count`. Default
    /// `120_000` ms (2 minutes).
    pub stale_threshold_ms: i64,

    /// Force-tick gating. `false` by default — production must not
    /// expose [`Stage2Watcher::force_tick`] /
    /// [`Stage2Watcher::force_tick_rule`] without an explicit opt-in
    /// (config flag, demo-tools feature, env). The watcher returns
    /// [`Stage2WatcherError::Terminal`] on a force-tick call when
    /// disabled.
    pub force_tick_enabled: bool,
}

impl Default for Stage2WatcherConfig {
    fn default() -> Self {
        Self {
            max_rules_per_tick: 500,
            tick_interval_ms: 5_000,
            offline_threshold_ms: 300_000,
            stale_threshold_ms: 120_000,
            force_tick_enabled: false,
        }
    }
}

// ── Clock injection ─────────────────────────────────────────────────────────

/// Wall-clock abstraction used for per-rule timestamp reads and
/// health bookkeeping. Tests inject [`MockClock`] so timestamps are
/// deterministic and per-rule timestamps are visibly distinct from
/// the tick-start `now_ms`.
pub trait Stage2Clock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> i64;
}

/// `chrono::Utc::now()`-backed clock. Production default.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Stage2Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

// ── Evaluator + Simulator traits ────────────────────────────────────────────

/// Pluggable condition evaluator. The W2 watcher does NOT fetch
/// live oracles; it delegates the truth-check to this trait. W3+
/// will plug in real Pyth / Solend evaluators behind this seam.
///
/// # Contract
///
/// - `Ok(true)`  → conditions met; the watcher then runs the
///   [`Stage2ExecutionSimulator`] stub and (on simulator success)
///   transitions the rule to `condition_met`.
/// - `Ok(false)` → conditions not yet met; the rule remains
///   `active` and `mark_checked` is recorded.
/// - `Err(Transient)` → leave active, record `last_error`, retry
///   next tick.
/// - `Err(Terminal)` → flip to `failed` via
///   `mark_failed_if_not_terminal`.
/// - `Err(Internal)` → bubble up in the tick report; the rule is
///   not advanced.
#[async_trait]
pub trait Stage2ConditionEvaluator: Send + Sync + std::fmt::Debug {
    async fn evaluate(
        &self,
        rule: &WatchRule,
        ctx: &Stage2TickContext,
    ) -> Result<bool, Stage2WatcherError>;
}

/// Pluggable execution simulator. **No-op in W2** — the default
/// returns `Ok(())`. Future W3 / I1 work will replace this with a
/// real RPC simulation that builds the execute_action_v2 tx,
/// validates account-key budget, dry-runs against `simulateTransaction`,
/// and only then approves the transition to `condition_met`.
///
/// # W2 contract (deliberately weak)
///
/// - `Ok(())`         → simulator approves; watcher advances to
///   `condition_met`.
/// - `Err(Transient)` → leave active, record `last_error`.
/// - `Err(Terminal)`  → mark_failed_if_not_terminal.
/// - `Err(Internal)`  → bubble up; rule not advanced.
#[async_trait]
pub trait Stage2ExecutionSimulator: Send + Sync + std::fmt::Debug {
    async fn simulate(
        &self,
        rule: &WatchRule,
        ctx: &Stage2TickContext,
    ) -> Result<(), Stage2WatcherError>;
}

/// W2 default evaluator — returns `false` for every rule. Safe by
/// construction (no live calls; no condition ever appears met).
#[derive(Debug, Default)]
pub struct NoopConditionEvaluator;

#[async_trait]
impl Stage2ConditionEvaluator for NoopConditionEvaluator {
    async fn evaluate(
        &self,
        _rule: &WatchRule,
        _ctx: &Stage2TickContext,
    ) -> Result<bool, Stage2WatcherError> {
        Ok(false)
    }
}

/// W2 default simulator — returns `Ok(())`. The simulator slot is
/// where W3/I1 will plug in a real RPC `simulateTransaction` call;
/// W2 only proves the wiring around it.
#[derive(Debug, Default)]
pub struct NoopExecutionSimulator;

#[async_trait]
impl Stage2ExecutionSimulator for NoopExecutionSimulator {
    async fn simulate(
        &self,
        _rule: &WatchRule,
        _ctx: &Stage2TickContext,
    ) -> Result<(), Stage2WatcherError> {
        Ok(())
    }
}

// ── In-flight guard ─────────────────────────────────────────────────────────

/// Per-rule in-flight RAII guard. Removes its `rule_id` from the
/// shared in-flight set on drop — including the drop-on-panic and
/// drop-on-error paths. The guard MUST NOT outlive the tick frame
/// it was acquired in.
///
/// The shared in-flight set lives behind a `parking_lot::Mutex`;
/// the lock is only held briefly (HashSet ops, no awaits), so
/// using a non-async mutex is correct and avoids the
/// `MutexGuard across await` pitfall.
struct InFlightGuard {
    in_flight: Arc<Mutex<HashSet<[u8; 16]>>>,
    rule_id: [u8; 16],
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.rule_id);
    }
}

fn try_acquire_in_flight(
    in_flight: &Arc<Mutex<HashSet<[u8; 16]>>>,
    rule_id: [u8; 16],
) -> Option<InFlightGuard> {
    let mut set = in_flight.lock();
    if set.contains(&rule_id) {
        None
    } else {
        set.insert(rule_id);
        Some(InFlightGuard {
            in_flight: Arc::clone(in_flight),
            rule_id,
        })
    }
}

// ── Per-rule tick result ────────────────────────────────────────────────────

/// One entry in [`Stage2TickReport::per_rule`]. Carries enough
/// context to surface in tracing logs and tests; intentionally
/// no `Display` derive — call sites format the `rule_id` as hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage2RuleTickResult {
    /// Rule was loaded but its status was not `active` (e.g.
    /// `condition_met`, `executing`). Idempotency / re-tick safety.
    SkippedNonActive { rule_id: [u8; 16] },
    /// Rule_id already present in the in-flight set; another
    /// concurrent tick frame is processing it. Skipped this tick.
    SkippedInFlight { rule_id: [u8; 16] },
    /// Status guard rejected the transition (TOCTOU lost): the row
    /// was advanced externally (e.g. revoked) between our load and
    /// our transition. We do nothing.
    SkippedRaceLost { rule_id: [u8; 16] },
    /// `current_slot >= expires_at_slot`. Rule was flipped to
    /// `expired` (or was already non-terminal-protected) and not
    /// evaluated.
    Expired { rule_id: [u8; 16] },
    /// Evaluator returned `Ok(false)`. Rule stays active.
    ConditionFalse { rule_id: [u8; 16] },
    /// Evaluator returned `Ok(true)` AND simulator returned `Ok(())`.
    /// Rule was flipped to `condition_met`.
    ConditionMet { rule_id: [u8; 16] },
    /// Evaluator returned `Err(Transient)`. Rule stays active;
    /// `last_error` was recorded.
    EvaluatorTransientError { rule_id: [u8; 16], error: String },
    /// Evaluator returned `Err(Terminal)`. Rule flipped to
    /// `failed`; `last_error` was recorded.
    EvaluatorTerminalError { rule_id: [u8; 16], error: String },
    /// Simulator (after eval=true) returned `Err(Transient)`. Rule
    /// stays active; `last_error` was recorded.
    SimulatorTransientError { rule_id: [u8; 16], error: String },
    /// Simulator (after eval=true) returned `Err(Terminal)`. Rule
    /// flipped to `failed`; `last_error` was recorded.
    SimulatorTerminalError { rule_id: [u8; 16], error: String },
}

impl Stage2RuleTickResult {
    pub fn rule_id(&self) -> [u8; 16] {
        match self {
            Self::SkippedNonActive { rule_id }
            | Self::SkippedInFlight { rule_id }
            | Self::SkippedRaceLost { rule_id }
            | Self::Expired { rule_id }
            | Self::ConditionFalse { rule_id }
            | Self::ConditionMet { rule_id }
            | Self::EvaluatorTransientError { rule_id, .. }
            | Self::EvaluatorTerminalError { rule_id, .. }
            | Self::SimulatorTransientError { rule_id, .. }
            | Self::SimulatorTerminalError { rule_id, .. } => *rule_id,
        }
    }
}

// ── Tick report ─────────────────────────────────────────────────────────────

/// Per-tick result. Aggregates counters across all per-rule
/// outcomes plus internal-error notes that don't belong to a
/// specific rule (e.g. the bulk `list_active` query failing).
#[derive(Debug, Clone, Default)]
pub struct Stage2TickReport {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub duration_ms: i64,
    /// Count of rows returned by `list_active_limit`. Bounded by
    /// [`Stage2WatcherConfig::max_rules_per_tick`].
    pub rules_loaded: u32,
    /// Subset of `rules_loaded` for which we acquired an in-flight
    /// guard and called `mark_checked`.
    pub rules_processed: u32,
    /// `rules_loaded == max_rules_per_tick` is a strong hint there
    /// MAY be more active rules than this tick processed. Surfaced
    /// for operators / health.
    pub rules_loaded_at_limit: bool,
    pub expired_count: u32,
    pub condition_met_count: u32,
    pub transient_error_count: u32,
    pub terminal_error_count: u32,
    pub failed_count: u32,
    pub skipped_non_active_count: u32,
    pub in_flight_skip_count: u32,
    pub race_lost_count: u32,
    pub internal_errors: Vec<String>,
    pub per_rule: Vec<Stage2RuleTickResult>,
    /// `true` if this report was produced by a force-tick path,
    /// not the normal scheduler tick.
    pub force_tick: bool,
    /// `Some(rule_id)` if this was a targeted force-tick.
    pub force_tick_target: Option<[u8; 16]>,
    /// `true` if a targeted force-tick was requested but the
    /// rule_id wasn't present in the DB (or had a terminal status).
    /// Drives the "clean not-found report" requirement.
    pub force_tick_target_not_found: bool,
}

impl Stage2TickReport {
    fn started(now_ms: i64) -> Self {
        Self {
            started_at_ms: now_ms,
            ..Self::default()
        }
    }

    fn finish(&mut self, now_ms: i64) {
        self.finished_at_ms = now_ms;
        self.duration_ms = now_ms.saturating_sub(self.started_at_ms);
    }

    fn add_internal_error(&mut self, msg: String) {
        warn!(error = %msg, "stage2 watcher internal error");
        self.internal_errors.push(msg);
    }

    /// `true` if this tick had no internal errors. Drives the
    /// `last_successful_tick_at_ms` health update — a tick with
    /// internal errors is NOT a successful tick.
    pub fn was_successful(&self) -> bool {
        self.internal_errors.is_empty()
    }
}

// ── Health surface ──────────────────────────────────────────────────────────

/// Cached watcher state. Mutated under [`Stage2Watcher::state`].
#[derive(Debug, Clone, Default)]
struct WatcherCachedState {
    last_successful_tick_at_ms: Option<i64>,
    last_tick_started_at_ms: Option<i64>,
    last_tick_finished_at_ms: Option<i64>,
    last_tick_duration_ms: Option<i64>,
    last_error: Option<String>,
    expired_count_last_tick: u32,
    condition_met_count_last_tick: u32,
    transient_error_count_last_tick: u32,
    terminal_error_count_last_tick: u32,
    failed_count_last_tick: u32,
}

/// Read-only snapshot of watcher health. The route layer (A2)
/// serialises this; this slice does NOT add a route handler.
#[derive(Debug, Clone)]
pub struct Stage2WatcherHealth {
    pub enabled: bool,
    pub last_successful_tick_at_ms: Option<i64>,
    pub last_tick_started_at_ms: Option<i64>,
    pub last_tick_finished_at_ms: Option<i64>,
    pub last_tick_duration_ms: Option<i64>,
    pub last_error: Option<String>,
    pub active_rule_count: u64,
    pub stale_rule_count: u64,
    pub in_flight_rule_count: u64,
    pub expired_count_last_tick: u32,
    pub condition_met_count_last_tick: u32,
    pub transient_error_count_last_tick: u32,
    pub terminal_error_count_last_tick: u32,
    pub failed_count_last_tick: u32,
    pub offline_threshold_ms: i64,
    pub stale_threshold_ms: i64,
    /// `true` if `now_ms - last_successful_tick_at_ms > offline_threshold_ms`
    /// at the moment this snapshot was assembled.
    pub is_offline: bool,
}

// ── Watcher ────────────────────────────────────────────────────────────────

/// Stage 2 watcher scheduler. Constructed via [`Self::new`] /
/// [`Self::with_components`] in the daemon wiring; ticks are
/// driven externally (this slice does NOT spawn a background
/// task — the daemon owns that orchestration).
#[derive(Debug)]
pub struct Stage2Watcher {
    repo: Stage2WatchRuleRepository,
    evaluator: Arc<dyn Stage2ConditionEvaluator>,
    simulator: Arc<dyn Stage2ExecutionSimulator>,
    config: Stage2WatcherConfig,
    clock: Arc<dyn Stage2Clock>,
    in_flight: Arc<Mutex<HashSet<[u8; 16]>>>,
    state: Arc<RwLock<WatcherCachedState>>,
    enabled: bool,
}

impl Stage2Watcher {
    /// Construct with the W2 default no-op evaluator + simulator
    /// and the system clock. Suitable for the daemon's first wire-up
    /// (watcher exists, ticks safely, never fires execution).
    pub fn new(repo: Stage2WatchRuleRepository, config: Stage2WatcherConfig) -> Self {
        Self::with_components(
            repo,
            Arc::new(NoopConditionEvaluator),
            Arc::new(NoopExecutionSimulator),
            Arc::new(SystemClock),
            config,
        )
    }

    /// Construct with caller-supplied components. Used by W3+ to
    /// inject a real evaluator / simulator, and by tests to inject
    /// scripted evaluators / a [`MockClock`].
    pub fn with_components(
        repo: Stage2WatchRuleRepository,
        evaluator: Arc<dyn Stage2ConditionEvaluator>,
        simulator: Arc<dyn Stage2ExecutionSimulator>,
        clock: Arc<dyn Stage2Clock>,
        config: Stage2WatcherConfig,
    ) -> Self {
        Self {
            repo,
            evaluator,
            simulator,
            config,
            clock,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            state: Arc::new(RwLock::new(WatcherCachedState::default())),
            enabled: true,
        }
    }

    /// Number of rules currently in the in-flight set. Surfaced in
    /// health and asserted in tests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.lock().len()
    }

    /// Watcher config (read-only handle for callers).
    pub fn config(&self) -> &Stage2WatcherConfig {
        &self.config
    }

    /// Run one tick over the full active-rule batch. The driver
    /// (gateway daemon, future tick loop) calls this on its tick
    /// schedule.
    pub async fn tick(&self, ctx: Stage2TickContext) -> Stage2TickReport {
        debug!(
            current_slot = ctx.current_slot,
            current_unix_timestamp = ctx.current_unix_timestamp,
            now_ms = ctx.now_ms,
            "stage2 watcher tick start"
        );
        let mut report = Stage2TickReport::started(ctx.now_ms);
        self.state.write().last_tick_started_at_ms = Some(ctx.now_ms);

        // NB: `list_pending_lifecycle_limit` includes rows whose
        // `expires_at_slot` has already passed but whose status is
        // still non-terminal — that's the watcher's garbage-collect
        // backlog. Using the slot-filtered `list_active_limit` here
        // would silently leave expired rows stuck in `active`
        // status forever.
        let rules = match self
            .repo
            .list_pending_lifecycle_limit(self.config.max_rules_per_tick)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                report.add_internal_error(format!(
                    "list_pending_lifecycle_limit: {e}"
                ));
                self.finalise_report(&mut report);
                return report;
            }
        };

        report.rules_loaded = rules.len() as u32;
        report.rules_loaded_at_limit =
            rules.len() as u32 == self.config.max_rules_per_tick
                && self.config.max_rules_per_tick > 0;

        for stored in rules {
            self.process_rule(stored, &ctx, &mut report).await;
        }

        self.finalise_report(&mut report);
        info!(
            rules_loaded = report.rules_loaded,
            processed = report.rules_processed,
            expired = report.expired_count,
            condition_met = report.condition_met_count,
            transient = report.transient_error_count,
            terminal = report.terminal_error_count,
            duration_ms = report.duration_ms,
            "stage2 watcher tick finished"
        );
        report
    }

    /// Run one tick over the entire active-rule batch, ignoring the
    /// scheduler's normal cadence. The route handler (A2) bridges
    /// to this; the watcher disables the call when
    /// [`Stage2WatcherConfig::force_tick_enabled`] is `false`.
    ///
    /// Force-tick is dry-run only — it goes through the same
    /// evaluator and simulator paths as the normal tick. No
    /// signing, no broadcast, no live RPC introduced here.
    pub async fn force_tick(
        &self,
        ctx: Stage2TickContext,
    ) -> Result<Stage2TickReport, Stage2WatcherError> {
        if !self.config.force_tick_enabled {
            return Err(Stage2WatcherError::Terminal(
                "force_tick disabled: enable Stage2WatcherConfig::force_tick_enabled \
                 (demo/dev only)"
                    .to_string(),
            ));
        }
        let mut report = self.tick(ctx).await;
        report.force_tick = true;
        Ok(report)
    }

    /// Targeted force tick — only processes the rule with the given
    /// `rule_id`. Other active rules are NOT loaded, NOT evaluated,
    /// NOT mutated.
    ///
    /// Returns a report whose `force_tick_target_not_found` is
    /// `true` if the rule_id is unknown OR is in a terminal status.
    pub async fn force_tick_rule(
        &self,
        rule_id: [u8; 16],
        ctx: Stage2TickContext,
    ) -> Result<Stage2TickReport, Stage2WatcherError> {
        if !self.config.force_tick_enabled {
            return Err(Stage2WatcherError::Terminal(
                "force_tick_rule disabled: enable Stage2WatcherConfig::force_tick_enabled \
                 (demo/dev only)"
                    .to_string(),
            ));
        }

        debug!(
            rule_id = %hex_id(&rule_id),
            "stage2 watcher force_tick_rule start"
        );
        let mut report = Stage2TickReport::started(ctx.now_ms);
        report.force_tick = true;
        report.force_tick_target = Some(rule_id);
        self.state.write().last_tick_started_at_ms = Some(ctx.now_ms);

        let stored = match self.repo.get(&rule_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                report.force_tick_target_not_found = true;
                self.finalise_report(&mut report);
                return Ok(report);
            }
            Err(e) => {
                report.add_internal_error(format!("repo.get: {e}"));
                self.finalise_report(&mut report);
                return Ok(report);
            }
        };

        // Treat any terminal status as "not found" for force-tick
        // purposes — we don't resurrect terminal rules.
        if matches!(
            stored.status,
            WatchRuleStatus::Completed
                | WatchRuleStatus::Expired
                | WatchRuleStatus::Revoked
                | WatchRuleStatus::Failed
        ) {
            report.force_tick_target_not_found = true;
            self.finalise_report(&mut report);
            return Ok(report);
        }

        // Mirror the normal-tick gating: rules_loaded counts the
        // single targeted row (regardless of whether we ultimately
        // process or skip it).
        report.rules_loaded = 1;
        self.process_rule(stored, &ctx, &mut report).await;
        self.finalise_report(&mut report);
        Ok(report)
    }

    /// Snapshot of the watcher's health. The DB-backed counts
    /// (`active_rule_count`, `stale_rule_count`) are computed
    /// against the supplied `current_slot` / `now_ms`; the rest of
    /// the snapshot reads the cached last-tick state.
    pub async fn health(
        &self,
        current_slot: u64,
        now_ms: i64,
    ) -> Result<Stage2WatcherHealth, Stage2WatcherError> {
        let state = self.state.read().clone();
        let in_flight_rule_count = self.in_flight.lock().len() as u64;

        let actives = self
            .repo
            .list_active_limit(current_slot, u32::MAX)
            .await?;
        let active_rule_count = actives.len() as u64;
        let stale_rule_count = actives
            .iter()
            .filter(|r| {
                if r.status != WatchRuleStatus::Active {
                    return false;
                }
                self.is_rule_stale(r, now_ms)
            })
            .count() as u64;

        let is_offline = match state.last_successful_tick_at_ms {
            Some(t) => now_ms - t > self.config.offline_threshold_ms,
            None => true,
        };

        Ok(Stage2WatcherHealth {
            enabled: self.enabled,
            last_successful_tick_at_ms: state.last_successful_tick_at_ms,
            last_tick_started_at_ms: state.last_tick_started_at_ms,
            last_tick_finished_at_ms: state.last_tick_finished_at_ms,
            last_tick_duration_ms: state.last_tick_duration_ms,
            last_error: state.last_error,
            active_rule_count,
            stale_rule_count,
            in_flight_rule_count,
            expired_count_last_tick: state.expired_count_last_tick,
            condition_met_count_last_tick: state.condition_met_count_last_tick,
            transient_error_count_last_tick: state.transient_error_count_last_tick,
            terminal_error_count_last_tick: state.terminal_error_count_last_tick,
            failed_count_last_tick: state.failed_count_last_tick,
            offline_threshold_ms: self.config.offline_threshold_ms,
            stale_threshold_ms: self.config.stale_threshold_ms,
            is_offline,
        })
    }

    fn is_rule_stale(&self, rule: &StoredWatchRule, now_ms: i64) -> bool {
        let threshold = self.config.stale_threshold_ms;
        match rule.last_successful_tick_at_ms {
            Some(t) => now_ms - t > threshold,
            None => now_ms - rule.created_at_ms > threshold,
        }
    }

    async fn process_rule(
        &self,
        stored: StoredWatchRule,
        ctx: &Stage2TickContext,
        report: &mut Stage2TickReport,
    ) {
        let rule_id = stored.rule.rule_id;

        // 1. Skip non-active rows (condition_met, executing). They
        //    are intentionally returned by `list_active_limit` (which
        //    excludes only terminal states), so we filter here.
        //    Terminal rows (completed/expired/revoked/failed) never
        //    appear; this branch is the idempotency guard for
        //    condition_met / executing.
        if stored.status != WatchRuleStatus::Active {
            debug!(
                rule_id = %hex_id(&rule_id),
                status = ?stored.status,
                "skipped: non-active status"
            );
            report.skipped_non_active_count += 1;
            report
                .per_rule
                .push(Stage2RuleTickResult::SkippedNonActive { rule_id });
            return;
        }

        // 2. Slot-based expiry. Use ctx.current_slot — NOT
        //    current_unix_timestamp. The expiry guard fires before
        //    the in-flight acquire so we cannot pointlessly contend
        //    with a parallel evaluator on a rule we're about to GC.
        if ctx.current_slot >= stored.rule.expires_at_slot {
            match self.repo.mark_expired_if_not_terminal(&rule_id).await {
                Ok(_n) => {
                    debug!(
                        rule_id = %hex_id(&rule_id),
                        current_slot = ctx.current_slot,
                        expires_at_slot = stored.rule.expires_at_slot,
                        "expired: slot >= expires_at_slot"
                    );
                    report.expired_count += 1;
                    report
                        .per_rule
                        .push(Stage2RuleTickResult::Expired { rule_id });
                }
                Err(e) => {
                    report.add_internal_error(format!(
                        "mark_expired_if_not_terminal {}: {e}",
                        hex_id(&rule_id)
                    ));
                }
            }
            return;
        }

        // 3. In-flight guard. If another concurrent tick frame
        //    already inserted this rule_id, skip; do NOT mutate.
        let _guard = match try_acquire_in_flight(&self.in_flight, rule_id) {
            Some(g) => g,
            None => {
                debug!(
                    rule_id = %hex_id(&rule_id),
                    "skipped: rule already in-flight"
                );
                report.in_flight_skip_count += 1;
                report
                    .per_rule
                    .push(Stage2RuleTickResult::SkippedInFlight { rule_id });
                return;
            }
        };

        // 4. Per-rule clock read. Each rule gets its own timestamp
        //    so a long batch tick does not stamp every row with the
        //    tick-start `now_ms`.
        let per_rule_now_ms = self.clock.now_ms();
        if let Err(e) = self
            .repo
            .mark_checked(&rule_id, ctx.current_slot, per_rule_now_ms)
            .await
        {
            report.add_internal_error(format!(
                "mark_checked {}: {e}",
                hex_id(&rule_id)
            ));
            return;
        }
        report.rules_processed += 1;

        // 5. Evaluator.
        let eval_result = self.evaluator.evaluate(&stored.rule, ctx).await;
        match eval_result {
            Ok(false) => {
                debug!(rule_id = %hex_id(&rule_id), "evaluator: false");
                report
                    .per_rule
                    .push(Stage2RuleTickResult::ConditionFalse { rule_id });
            }
            Ok(true) => {
                self.handle_condition_true(&stored.rule, ctx, report).await;
            }
            Err(e) if e.is_transient() => {
                let msg = e.to_string();
                self.record_transient(&rule_id, &msg, report).await;
                warn!(
                    rule_id = %hex_id(&rule_id),
                    error = %msg,
                    "evaluator transient error"
                );
                report.per_rule.push(
                    Stage2RuleTickResult::EvaluatorTransientError {
                        rule_id,
                        error: msg,
                    },
                );
            }
            Err(e) if e.is_terminal() => {
                let msg = e.to_string();
                self.record_terminal(&rule_id, &msg, report).await;
                warn!(
                    rule_id = %hex_id(&rule_id),
                    error = %msg,
                    "evaluator terminal error -> failed"
                );
                report.per_rule.push(
                    Stage2RuleTickResult::EvaluatorTerminalError {
                        rule_id,
                        error: msg,
                    },
                );
            }
            Err(e) => {
                report.add_internal_error(format!(
                    "evaluator internal error for {}: {e}",
                    hex_id(&rule_id)
                ));
            }
        }
    }

    async fn handle_condition_true(
        &self,
        rule: &WatchRule,
        ctx: &Stage2TickContext,
        report: &mut Stage2TickReport,
    ) {
        let rule_id = rule.rule_id;
        let sim_result = self.simulator.simulate(rule, ctx).await;
        match sim_result {
            Ok(()) => {
                match self.repo.mark_condition_met_if_active(&rule_id).await {
                    Ok(1) => {
                        info!(rule_id = %hex_id(&rule_id), "condition met");
                        report.condition_met_count += 1;
                        report
                            .per_rule
                            .push(Stage2RuleTickResult::ConditionMet { rule_id });
                    }
                    Ok(_) => {
                        debug!(
                            rule_id = %hex_id(&rule_id),
                            "TOCTOU race: rule advanced before condition_met transition"
                        );
                        report.race_lost_count += 1;
                        report
                            .per_rule
                            .push(Stage2RuleTickResult::SkippedRaceLost { rule_id });
                    }
                    Err(e) => {
                        report.add_internal_error(format!(
                            "mark_condition_met_if_active {}: {e}",
                            hex_id(&rule_id)
                        ));
                    }
                }
            }
            Err(e) if e.is_transient() => {
                let msg = e.to_string();
                self.record_transient(&rule_id, &msg, report).await;
                warn!(
                    rule_id = %hex_id(&rule_id),
                    error = %msg,
                    "simulator transient error"
                );
                report.per_rule.push(
                    Stage2RuleTickResult::SimulatorTransientError {
                        rule_id,
                        error: msg,
                    },
                );
            }
            Err(e) if e.is_terminal() => {
                let msg = e.to_string();
                self.record_terminal(&rule_id, &msg, report).await;
                warn!(
                    rule_id = %hex_id(&rule_id),
                    error = %msg,
                    "simulator terminal error -> failed"
                );
                report.per_rule.push(
                    Stage2RuleTickResult::SimulatorTerminalError {
                        rule_id,
                        error: msg,
                    },
                );
            }
            Err(e) => {
                report.add_internal_error(format!(
                    "simulator internal error for {}: {e}",
                    hex_id(&rule_id)
                ));
            }
        }
    }

    async fn record_transient(
        &self,
        rule_id: &[u8; 16],
        msg: &str,
        report: &mut Stage2TickReport,
    ) {
        if let Err(e) = self.repo.record_last_error(rule_id, msg).await {
            report.add_internal_error(format!(
                "record_last_error {}: {e}",
                hex_id(rule_id)
            ));
        }
        report.transient_error_count += 1;
    }

    async fn record_terminal(
        &self,
        rule_id: &[u8; 16],
        msg: &str,
        report: &mut Stage2TickReport,
    ) {
        match self.repo.mark_failed_if_not_terminal(rule_id, msg).await {
            Ok(1) => {
                report.terminal_error_count += 1;
                report.failed_count += 1;
            }
            Ok(_) => {
                // Race lost: external actor advanced lifecycle.
                report.race_lost_count += 1;
            }
            Err(e) => {
                report.add_internal_error(format!(
                    "mark_failed_if_not_terminal {}: {e}",
                    hex_id(rule_id)
                ));
            }
        }
    }

    fn finalise_report(&self, report: &mut Stage2TickReport) {
        let now_ms = self.clock.now_ms();
        report.finish(now_ms);

        let mut state = self.state.write();
        state.last_tick_finished_at_ms = Some(now_ms);
        state.last_tick_duration_ms = Some(report.duration_ms);
        state.expired_count_last_tick = report.expired_count;
        state.condition_met_count_last_tick = report.condition_met_count;
        state.transient_error_count_last_tick = report.transient_error_count;
        state.terminal_error_count_last_tick = report.terminal_error_count;
        state.failed_count_last_tick = report.failed_count;

        if report.was_successful() {
            state.last_successful_tick_at_ms = Some(now_ms);
            state.last_error = None;
        } else {
            // Pin the most recent internal-error message for the
            // health surface; preserves a useful breadcrumb without
            // pretending the tick succeeded.
            state.last_error = report.internal_errors.last().cloned();
        }
    }
}

fn hex_id(rule_id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in rule_id {
        std::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}")).unwrap();
    }
    s
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    use claw_state_store::db::Database;
    use claw_state_store::stage2_watch_rules::{
        Stage2WatchRuleRepository, WatchRuleStatus,
    };
    use claw_types::canonical_intent::PubkeyBytes;
    use claw_types::stage2_watch_rule::{
        ActionSpec, BoundMode, Comparison, Condition, ConditionLogic,
        JupiterApiVersion, RateKind, VerificationLevel, WatchRule, WithdrawMode,
        STAGE2_WATCH_RULE_SCHEMA_VERSION,
    };

    // ── Test fixtures ───────────────────────────────────────────────────

    const TEST_USER: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
    const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
    const FIXTURE_CREATED_AT_SLOT: u64 = 415_500_000;
    const FIXTURE_EXPIRES_AT_SLOT: u64 = 415_700_000;

    fn pk(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn pk_from_str(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test pubkey parses")
    }

    fn fixture_solend_rule(rule_id: [u8; 16]) -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT_SLOT,
            expires_at_slot: FIXTURE_EXPIRES_AT_SLOT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                solend_program_id: pk_from_str(SOLEND_PROGRAM_ID),
                comparison: Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: 1,
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            action: ActionSpec::SolendWithdrawAllDelegated {
                target_obligation: pk(0x04),
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                destination_wallet: pk_from_str(TEST_USER),
                withdraw_mode: WithdrawMode::WithdrawAllDelegatedPosition,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 0,
        }
    }

    fn fixture_jupiter_rule(rule_id: [u8; 16]) -> WatchRule {
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT_SLOT,
            expires_at_slot: FIXTURE_EXPIRES_AT_SLOT,
            one_shot: true,
            condition_logic: ConditionLogic::All,
            conditions: vec![Condition::PythPrice {
                feed_id: [0xAB; 32],
                price_update_account: pk(0x10),
                comparison: Comparison::Gt,
                threshold_mantissa: 1_000_000,
                threshold_exponent: -2,
                max_age_seconds: 30,
                max_confidence_bps: 50,
                verification_level_required: VerificationLevel::Full,
                bound_mode: BoundMode::AdverseLowerForGt,
            }],
            action: ActionSpec::JupiterBuySolWithUsdc {
                input_mint: pk(0x21),
                output_mint: pk(0x22),
                input_amount_raw: 5_000_000,
                min_output_amount_raw: None,
                jupiter_api_version: JupiterApiVersion::V2,
                max_accounts_hint: 48,
                require_pre_post_bracket: true,
            },
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk_from_str(TEST_USER),
            slippage_bps: 50,
        }
    }

    // ── Mock clock ──────────────────────────────────────────────────────

    /// Advancing-by-default mock clock — every read returns
    /// `start_ms + step_ms * call_count`. Lets tests prove the
    /// per-rule timestamp is distinct from the tick-start `now_ms`
    /// without orchestrating an explicit advance call.
    #[derive(Debug)]
    struct MockClock {
        start_ms: i64,
        step_ms: i64,
        calls: AtomicU64,
        override_value: AtomicI64,
        override_set: AtomicU64, // 0 = unset, 1 = set
    }

    impl MockClock {
        fn new(start_ms: i64, step_ms: i64) -> Self {
            Self {
                start_ms,
                step_ms,
                calls: AtomicU64::new(0),
                override_value: AtomicI64::new(0),
                override_set: AtomicU64::new(0),
            }
        }

        fn pinned(at_ms: i64) -> Self {
            let c = Self::new(at_ms, 0);
            c.set_override(at_ms);
            c
        }

        fn set_override(&self, value: i64) {
            self.override_value.store(value, Ordering::SeqCst);
            self.override_set.store(1, Ordering::SeqCst);
        }
    }

    impl Stage2Clock for MockClock {
        fn now_ms(&self) -> i64 {
            if self.override_set.load(Ordering::SeqCst) == 1 {
                return self.override_value.load(Ordering::SeqCst);
            }
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.start_ms + self.step_ms * (n as i64)
        }
    }

    // ── Scripted evaluator + simulator ──────────────────────────────────

    type EvalScript =
        dyn Fn(&WatchRule, &Stage2TickContext) -> Result<bool, Stage2WatcherError>
            + Send
            + Sync;
    type SimScript =
        dyn Fn(&WatchRule, &Stage2TickContext) -> Result<(), Stage2WatcherError>
            + Send
            + Sync;

    #[derive(Clone)]
    struct ScriptedEvaluator {
        f: Arc<EvalScript>,
        calls: Arc<AtomicU64>,
    }

    impl std::fmt::Debug for ScriptedEvaluator {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ScriptedEvaluator")
                .field("calls", &self.calls.load(Ordering::SeqCst))
                .finish()
        }
    }

    impl ScriptedEvaluator {
        fn new<F>(f: F) -> Self
        where
            F: Fn(&WatchRule, &Stage2TickContext)
                    -> Result<bool, Stage2WatcherError>
                + Send
                + Sync
                + 'static,
        {
            Self {
                f: Arc::new(f),
                calls: Arc::new(AtomicU64::new(0)),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Stage2ConditionEvaluator for ScriptedEvaluator {
        async fn evaluate(
            &self,
            rule: &WatchRule,
            ctx: &Stage2TickContext,
        ) -> Result<bool, Stage2WatcherError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.f)(rule, ctx)
        }
    }

    #[derive(Clone)]
    struct ScriptedSimulator {
        f: Arc<SimScript>,
        calls: Arc<AtomicU64>,
    }

    impl std::fmt::Debug for ScriptedSimulator {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ScriptedSimulator")
                .field("calls", &self.calls.load(Ordering::SeqCst))
                .finish()
        }
    }

    impl ScriptedSimulator {
        fn new<F>(f: F) -> Self
        where
            F: Fn(&WatchRule, &Stage2TickContext)
                    -> Result<(), Stage2WatcherError>
                + Send
                + Sync
                + 'static,
        {
            Self {
                f: Arc::new(f),
                calls: Arc::new(AtomicU64::new(0)),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Stage2ExecutionSimulator for ScriptedSimulator {
        async fn simulate(
            &self,
            rule: &WatchRule,
            ctx: &Stage2TickContext,
        ) -> Result<(), Stage2WatcherError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.f)(rule, ctx)
        }
    }

    fn always_false() -> ScriptedEvaluator {
        ScriptedEvaluator::new(|_, _| Ok(false))
    }

    fn always_true() -> ScriptedEvaluator {
        ScriptedEvaluator::new(|_, _| Ok(true))
    }

    fn evaluator_transient(msg: &'static str) -> ScriptedEvaluator {
        ScriptedEvaluator::new(move |_, _| Err(Stage2WatcherError::Transient(msg.to_string())))
    }

    fn evaluator_terminal(msg: &'static str) -> ScriptedEvaluator {
        ScriptedEvaluator::new(move |_, _| Err(Stage2WatcherError::Terminal(msg.to_string())))
    }

    fn simulator_ok() -> ScriptedSimulator {
        ScriptedSimulator::new(|_, _| Ok(()))
    }

    fn simulator_transient(msg: &'static str) -> ScriptedSimulator {
        ScriptedSimulator::new(move |_, _| Err(Stage2WatcherError::Transient(msg.to_string())))
    }

    fn simulator_terminal(msg: &'static str) -> ScriptedSimulator {
        ScriptedSimulator::new(move |_, _| Err(Stage2WatcherError::Terminal(msg.to_string())))
    }

    // ── Test harness ────────────────────────────────────────────────────

    struct Harness {
        _db: Database,
        repo: Stage2WatchRuleRepository,
    }

    async fn harness() -> Harness {
        let db = Database::open_in_memory()
            .await
            .expect("in-memory DB opens");
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        Harness { _db: db, repo }
    }

    fn watcher_with(
        repo: Stage2WatchRuleRepository,
        evaluator: Arc<dyn Stage2ConditionEvaluator>,
        simulator: Arc<dyn Stage2ExecutionSimulator>,
        clock: Arc<dyn Stage2Clock>,
        config: Stage2WatcherConfig,
    ) -> Stage2Watcher {
        Stage2Watcher::with_components(repo, evaluator, simulator, clock, config)
    }

    fn ctx_at(slot: u64, now_ms: i64) -> Stage2TickContext {
        Stage2TickContext::new(slot, 1_700_000_000, now_ms)
    }

    fn force_tick_config() -> Stage2WatcherConfig {
        Stage2WatcherConfig {
            force_tick_enabled: true,
            ..Stage2WatcherConfig::default()
        }
    }

    // ── Empty-batch tick ────────────────────────────────────────────────

    #[tokio::test]
    async fn tick_with_no_rules_returns_empty_report() {
        let h = harness().await;
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(1_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher.tick(ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 1_000)).await;
        assert_eq!(report.rules_loaded, 0);
        assert_eq!(report.rules_processed, 0);
        assert_eq!(report.expired_count, 0);
        assert_eq!(report.condition_met_count, 0);
        assert!(report.per_rule.is_empty());
        assert!(report.was_successful());
    }

    // ── Expiry uses current_slot, not unix timestamp ────────────────────

    #[tokio::test]
    async fn expired_rule_is_marked_expired_and_not_evaluated() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xA1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let evaluator = Arc::new(always_true());
        let simulator = Arc::new(simulator_ok());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            simulator.clone(),
            Arc::new(MockClock::pinned(2_000)),
            Stage2WatcherConfig::default(),
        );

        // Slot well past expiry.
        let ctx = ctx_at(rule.expires_at_slot + 100, 2_000);
        let report = watcher.tick(ctx).await;

        assert_eq!(report.expired_count, 1);
        assert_eq!(report.rules_processed, 0);
        assert_eq!(evaluator.call_count(), 0, "expired rules must not be evaluated");
        assert_eq!(simulator.call_count(), 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Expired);
    }

    #[tokio::test]
    async fn expiry_uses_current_slot_not_unix_timestamp() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xA2; 16]);
        h.repo.insert(&rule).await.unwrap();

        let evaluator = Arc::new(always_true());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(3_000)),
            Stage2WatcherConfig::default(),
        );

        // Unix timestamp is "in the future" (huge), but slot is
        // well below expires_at_slot. The rule must NOT expire.
        let ctx = Stage2TickContext::new(
            rule.expires_at_slot - 10,
            i64::MAX, // huge unix timestamp
            3_000,
        );
        let report = watcher.tick(ctx).await;

        assert_eq!(report.expired_count, 0);
        assert_eq!(report.rules_processed, 1);
        assert_eq!(evaluator.call_count(), 1);

        // Inverse — slot far ahead, unix timestamp small. MUST expire.
        let rule_b = fixture_solend_rule([0xA3; 16]);
        h.repo.insert(&rule_b).await.unwrap();
        let ctx = Stage2TickContext::new(
            rule_b.expires_at_slot + 1,
            0, // zero unix timestamp
            3_000,
        );
        let report = watcher.tick(ctx).await;

        assert_eq!(report.expired_count, 1);
        let loaded = h.repo.get(&rule_b.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Expired);
    }

    // ── False / true / mark_checked ─────────────────────────────────────

    #[tokio::test]
    async fn false_condition_keeps_rule_active_and_marks_checked() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xB1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(4_000)),
            Stage2WatcherConfig::default(),
        );

        let ctx = ctx_at(rule.expires_at_slot - 1, 4_000);
        let report = watcher.tick(ctx).await;

        assert_eq!(report.rules_processed, 1);
        assert_eq!(report.condition_met_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert_eq!(loaded.last_checked_slot, Some(rule.expires_at_slot - 1));
        assert!(loaded.last_successful_tick_at_ms.is_some());
    }

    #[tokio::test]
    async fn true_condition_with_simulator_ok_marks_condition_met() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xB2; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(5_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 5_000))
            .await;
        assert_eq!(report.condition_met_count, 1);
        assert_eq!(report.failed_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    // ── Simulator branches ──────────────────────────────────────────────

    #[tokio::test]
    async fn condition_true_simulator_transient_keeps_active_and_records_error() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xB3; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_transient("simulator down")),
            Arc::new(MockClock::pinned(6_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 6_000))
            .await;
        assert_eq!(report.transient_error_count, 1);
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.failed_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert!(
            loaded
                .last_error
                .as_deref()
                .unwrap()
                .contains("simulator down"),
            "expected last_error recorded, got {:?}",
            loaded.last_error
        );
    }

    #[tokio::test]
    async fn condition_true_simulator_terminal_marks_failed_and_records_error() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xB4; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_terminal("unsupported action")),
            Arc::new(MockClock::pinned(7_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 7_000))
            .await;
        assert_eq!(report.terminal_error_count, 1);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.condition_met_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert!(
            loaded
                .last_error
                .as_deref()
                .unwrap()
                .contains("unsupported action"),
            "expected last_error recorded, got {:?}",
            loaded.last_error
        );
    }

    // ── Evaluator transient / terminal ─────────────────────────────────

    #[tokio::test]
    async fn evaluator_transient_keeps_active_and_records_error() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xC1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator_transient("rate limited")),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(8_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 8_000))
            .await;
        assert_eq!(report.transient_error_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert!(loaded.last_error.as_deref().unwrap().contains("rate limited"));
    }

    #[tokio::test]
    async fn evaluator_terminal_marks_failed_and_records_error() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xC2; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator_terminal("invalid rule shape")),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(9_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 9_000))
            .await;
        assert_eq!(report.terminal_error_count, 1);
        assert_eq!(report.failed_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert!(
            loaded
                .last_error
                .as_deref()
                .unwrap()
                .contains("invalid rule shape"),
        );
    }

    // ── Skipped non-active / terminal ──────────────────────────────────

    #[tokio::test]
    async fn revoked_completed_failed_rules_are_skipped() {
        let h = harness().await;

        let rule_revoked = fixture_solend_rule([0xD1; 16]);
        let rule_completed = fixture_solend_rule([0xD2; 16]);
        let rule_failed = fixture_solend_rule([0xD3; 16]);
        let rule_active = fixture_solend_rule([0xD4; 16]);

        for r in [&rule_revoked, &rule_completed, &rule_failed, &rule_active] {
            h.repo.insert(r).await.unwrap();
        }
        h.repo.mark_revoked(&rule_revoked.rule_id).await.unwrap();
        h.repo
            .mark_completed(&rule_completed.rule_id, 5_000_000, FIXTURE_CREATED_AT_SLOT + 5)
            .await
            .unwrap();
        h.repo.mark_failed(&rule_failed.rule_id, "x").await.unwrap();

        let evaluator = Arc::new(always_true());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(10_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule_active.expires_at_slot - 1, 10_000))
            .await;

        // Only the active rule was loaded — list_active_limit
        // already filters out terminal statuses.
        assert_eq!(report.rules_loaded, 1);
        assert_eq!(report.rules_processed, 1);
        assert_eq!(report.condition_met_count, 1);
        assert_eq!(evaluator.call_count(), 1);

        // Terminal rules unchanged.
        assert_eq!(
            h.repo.get(&rule_revoked.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Revoked
        );
        assert_eq!(
            h.repo.get(&rule_completed.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Completed
        );
        assert_eq!(
            h.repo.get(&rule_failed.rule_id).await.unwrap().unwrap().status,
            WatchRuleStatus::Failed
        );
    }

    // ── Idempotency for condition_met / executing ──────────────────────

    #[tokio::test]
    async fn repeat_tick_is_idempotent_for_condition_met() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xE1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let evaluator = Arc::new(always_true());
        let simulator = Arc::new(simulator_ok());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            simulator.clone(),
            Arc::new(MockClock::pinned(11_000)),
            Stage2WatcherConfig::default(),
        );

        // First tick — flips to condition_met.
        let r1 = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 11_000))
            .await;
        assert_eq!(r1.condition_met_count, 1);
        let after_first = evaluator.call_count();

        // Second tick — rule loaded but skipped due to non-active
        // status. Evaluator must NOT be called again.
        let r2 = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 11_000))
            .await;
        assert_eq!(r2.condition_met_count, 0);
        assert_eq!(r2.skipped_non_active_count, 1);
        assert_eq!(
            evaluator.call_count(),
            after_first,
            "evaluator must not be re-called for condition_met rules"
        );

        // Status unchanged.
        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    // ── Per-rule timestamp distinct from tick-start now_ms ─────────────

    #[tokio::test]
    async fn per_rule_timestamp_uses_clock_read_not_tick_start() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF1; 16]);
        h.repo.insert(&rule).await.unwrap();

        // Advancing clock — first read returns 100_000, second 100_500, etc.
        let clock = Arc::new(MockClock::new(100_000, 500));
        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            clock.clone(),
            Stage2WatcherConfig::default(),
        );

        // Tick passes a tiny tick-start now_ms (1) — proving the
        // per-rule mark_checked timestamp comes from the clock,
        // not from ctx.now_ms.
        let _r = watcher
            .tick(Stage2TickContext::new(rule.expires_at_slot - 1, 0, 1))
            .await;
        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        let stamped = loaded.last_successful_tick_at_ms.unwrap();
        assert!(
            stamped >= 100_000,
            "per-rule timestamp should come from the clock, got {stamped}"
        );
    }

    // ── In-flight: overlap protection, release, skip-no-mutate ─────────

    #[tokio::test]
    async fn overlapping_ticks_cannot_evaluate_same_rule_twice() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF2; 16]);
        h.repo.insert(&rule).await.unwrap();

        // The evaluator yields once so two concurrent ticks can
        // race on the in-flight set.
        let yielding_eval = ScriptedEvaluator::new(|_, _| Ok(false));
        let watcher = Arc::new(watcher_with(
            h.repo.clone(),
            Arc::new(yielding_eval.clone()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(12_000)),
            Stage2WatcherConfig::default(),
        ));

        // Pre-occupy the in-flight set so the second concurrent
        // call observes the contention deterministically.
        let captured_id = rule.rule_id;
        watcher.in_flight.lock().insert(captured_id);

        // Tick now — should observe in-flight skip.
        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 12_000))
            .await;
        assert_eq!(report.in_flight_skip_count, 1);
        assert_eq!(report.rules_processed, 0);
        assert_eq!(yielding_eval.call_count(), 0);

        // Manually release the pre-occupied marker.
        watcher.in_flight.lock().remove(&captured_id);

        // Now a fresh tick proceeds normally.
        let report2 = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 12_000))
            .await;
        assert_eq!(report2.rules_processed, 1);
        assert_eq!(report2.in_flight_skip_count, 0);
    }

    #[tokio::test]
    async fn in_flight_marker_released_after_evaluator_success() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF3; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(13_000)),
            Stage2WatcherConfig::default(),
        );

        assert_eq!(watcher.in_flight_count(), 0);
        let _r = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 13_000))
            .await;
        assert_eq!(
            watcher.in_flight_count(),
            0,
            "in-flight set must be empty after a successful tick"
        );
    }

    #[tokio::test]
    async fn in_flight_marker_released_after_evaluator_error() {
        let h = harness().await;
        let rule_t = fixture_solend_rule([0xF4; 16]);
        let rule_e = fixture_solend_rule([0xF5; 16]);
        h.repo.insert(&rule_t).await.unwrap();
        h.repo.insert(&rule_e).await.unwrap();

        let watcher_t = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator_transient("blip")),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(14_000)),
            Stage2WatcherConfig::default(),
        );
        let _r = watcher_t
            .tick(ctx_at(rule_t.expires_at_slot - 1, 14_000))
            .await;
        assert_eq!(
            watcher_t.in_flight_count(),
            0,
            "in-flight must release on transient error"
        );

        let watcher_e = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator_terminal("bad")),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(14_500)),
            Stage2WatcherConfig::default(),
        );
        let _r = watcher_e
            .tick(ctx_at(rule_e.expires_at_slot - 1, 14_500))
            .await;
        assert_eq!(
            watcher_e.in_flight_count(),
            0,
            "in-flight must release on terminal error"
        );
    }

    #[tokio::test]
    async fn in_flight_marker_released_after_simulator_error() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF6; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_terminal("nope")),
            Arc::new(MockClock::pinned(15_000)),
            Stage2WatcherConfig::default(),
        );
        let _r = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 15_000))
            .await;
        assert_eq!(
            watcher.in_flight_count(),
            0,
            "in-flight must release on simulator terminal error"
        );
    }

    #[tokio::test]
    async fn in_flight_skip_does_not_mutate_db_lifecycle() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF7; 16]);
        h.repo.insert(&rule).await.unwrap();

        let evaluator = Arc::new(always_true());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(16_000)),
            Stage2WatcherConfig::default(),
        );

        // Pre-mark rule_id as in-flight from outside.
        watcher.in_flight.lock().insert(rule.rule_id);

        let before = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 16_000))
            .await;
        assert_eq!(report.in_flight_skip_count, 1);
        assert_eq!(evaluator.call_count(), 0);

        let after = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_checked_slot, before.last_checked_slot);
        assert_eq!(after.last_error, before.last_error);
    }

    // ── TOCTOU: revoked between load and condition_met transition ─────

    /// Evaluator that flips the rule to `revoked` BEFORE returning
    /// `Ok(true)`. Models an external actor (user revoke ix,
    /// concurrent admin sweep) advancing the lifecycle between the
    /// watcher's `list_active_limit` and the watcher's
    /// `mark_condition_met_if_active`. The status-guarded UPDATE
    /// must reject the would-be transition.
    #[derive(Debug)]
    struct RevokingEvaluator {
        repo: Stage2WatchRuleRepository,
        target: [u8; 16],
    }

    #[async_trait]
    impl Stage2ConditionEvaluator for RevokingEvaluator {
        async fn evaluate(
            &self,
            _rule: &WatchRule,
            _ctx: &Stage2TickContext,
        ) -> Result<bool, Stage2WatcherError> {
            self.repo
                .mark_revoked(&self.target)
                .await
                .map_err(|e| Stage2WatcherError::Internal(e.to_string()))?;
            Ok(true)
        }
    }

    #[tokio::test]
    async fn race_revoked_after_load_does_not_overwrite_with_condition_met() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xF8; 16]);
        h.repo.insert(&rule).await.unwrap();

        let racing_eval = RevokingEvaluator {
            repo: h.repo.clone(),
            target: rule.rule_id,
        };

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(racing_eval),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(17_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 17_000))
            .await;
        // The watcher saw the race lost and recorded it.
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.race_lost_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(
            loaded.status,
            WatchRuleStatus::Revoked,
            "revoked state must NOT be overwritten by condition_met"
        );
        assert!(loaded.revoked);
    }

    // ── Tick report counts are accurate ───────────────────────────────

    #[tokio::test]
    async fn tick_report_counts_are_accurate() {
        let h = harness().await;
        let active_id = [0x01_u8; 16];
        let expired_id = [0x02_u8; 16];

        h.repo.insert(&fixture_solend_rule(active_id)).await.unwrap();

        let mut expired_rule = fixture_solend_rule(expired_id);
        expired_rule.expires_at_slot = FIXTURE_CREATED_AT_SLOT + 50;
        h.repo.insert(&expired_rule).await.unwrap();

        // Tick at a slot past the expired rule but before the
        // active rule's expiry.
        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(18_000)),
            Stage2WatcherConfig::default(),
        );
        let ctx = ctx_at(FIXTURE_CREATED_AT_SLOT + 100, 18_000);
        let report = watcher.tick(ctx).await;

        assert_eq!(report.rules_loaded, 2);
        assert_eq!(report.expired_count, 1);
        assert_eq!(report.rules_processed, 1);
        assert_eq!(report.condition_met_count, 1);
        assert_eq!(report.per_rule.len(), 2);
    }

    // ── Force tick: gating, ctx, targeted ──────────────────────────────

    #[tokio::test]
    async fn force_tick_disabled_by_default() {
        let h = harness().await;
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(19_000)),
            Stage2WatcherConfig::default(),
        );

        let result = watcher
            .force_tick(ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 19_000))
            .await;
        assert!(matches!(result, Err(Stage2WatcherError::Terminal(_))));

        let result_one = watcher
            .force_tick_rule([0; 16], ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 19_000))
            .await;
        assert!(matches!(result_one, Err(Stage2WatcherError::Terminal(_))));
    }

    #[tokio::test]
    async fn force_tick_uses_provided_ctx() {
        let h = harness().await;
        let rule = fixture_solend_rule([0x21; 16]);
        h.repo.insert(&rule).await.unwrap();

        let evaluator = ScriptedEvaluator::new(|_, ctx| {
            // Pin: force_tick must pass our chosen slot through.
            assert!(ctx.current_slot >= 415_500_000);
            Ok(false)
        });
        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(20_000)),
            force_tick_config(),
        );

        let report = watcher
            .force_tick(ctx_at(rule.expires_at_slot - 1, 20_000))
            .await
            .unwrap();
        assert!(report.force_tick);
        assert_eq!(report.rules_processed, 1);
    }

    #[tokio::test]
    async fn force_tick_rule_only_evaluates_target() {
        let h = harness().await;
        let target = fixture_solend_rule([0x31; 16]);
        let other = fixture_solend_rule([0x32; 16]);
        h.repo.insert(&target).await.unwrap();
        h.repo.insert(&other).await.unwrap();

        // Track which rule_ids the evaluator saw.
        let target_id = target.rule_id;
        let evaluator = ScriptedEvaluator::new(move |rule, _| {
            assert_eq!(rule.rule_id, target_id, "force_tick_rule must not call evaluator on other rules");
            Ok(true)
        });
        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(evaluator.clone()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(21_000)),
            force_tick_config(),
        );

        let report = watcher
            .force_tick_rule(target.rule_id, ctx_at(target.expires_at_slot - 1, 21_000))
            .await
            .unwrap();
        assert_eq!(report.condition_met_count, 1);
        assert!(report.force_tick);
        assert_eq!(report.force_tick_target, Some(target.rule_id));
        assert_eq!(evaluator.call_count(), 1);

        // Other rule untouched.
        let other_loaded = h.repo.get(&other.rule_id).await.unwrap().unwrap();
        assert_eq!(other_loaded.status, WatchRuleStatus::Active);
        assert!(other_loaded.last_checked_slot.is_none());
        assert!(other_loaded.last_successful_tick_at_ms.is_none());
    }

    #[tokio::test]
    async fn force_tick_rule_missing_returns_clean_not_found() {
        let h = harness().await;
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_true()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(22_000)),
            force_tick_config(),
        );

        let report = watcher
            .force_tick_rule([0xCC; 16], ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 22_000))
            .await
            .unwrap();
        assert!(report.force_tick);
        assert!(report.force_tick_target_not_found);
        assert_eq!(report.rules_loaded, 0);
        assert_eq!(report.rules_processed, 0);
        assert_eq!(report.condition_met_count, 0);
    }

    #[tokio::test]
    async fn force_tick_rule_terminal_status_returns_not_found() {
        let h = harness().await;
        let rule = fixture_solend_rule([0x41; 16]);
        h.repo.insert(&rule).await.unwrap();
        h.repo.mark_revoked(&rule.rule_id).await.unwrap();

        let evaluator = Arc::new(always_true());
        let watcher = watcher_with(
            h.repo.clone(),
            evaluator.clone(),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(23_000)),
            force_tick_config(),
        );

        let report = watcher
            .force_tick_rule(rule.rule_id, ctx_at(rule.expires_at_slot - 1, 23_000))
            .await
            .unwrap();
        assert!(report.force_tick_target_not_found);
        assert_eq!(evaluator.call_count(), 0);
    }

    // ── Health surface ────────────────────────────────────────────────

    #[tokio::test]
    async fn health_offline_when_no_successful_tick() {
        let h = harness().await;
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(50_000)),
            Stage2WatcherConfig::default(),
        );

        let health = watcher.health(FIXTURE_CREATED_AT_SLOT + 1, 50_000).await.unwrap();
        assert!(health.is_offline);
        assert!(health.last_successful_tick_at_ms.is_none());
    }

    #[tokio::test]
    async fn health_online_after_successful_tick() {
        let h = harness().await;
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(60_000)),
            Stage2WatcherConfig::default(),
        );
        let _r = watcher.tick(ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 60_000)).await;

        let health = watcher
            .health(FIXTURE_CREATED_AT_SLOT + 1, 60_001)
            .await
            .unwrap();
        assert!(!health.is_offline);
        assert_eq!(health.last_successful_tick_at_ms, Some(60_000));
    }

    #[tokio::test]
    async fn health_offline_when_last_tick_older_than_threshold() {
        let h = harness().await;
        let clock = Arc::new(MockClock::pinned(70_000));
        let watcher = watcher_with(
            h.repo,
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            clock.clone(),
            Stage2WatcherConfig {
                offline_threshold_ms: 1_000,
                ..Stage2WatcherConfig::default()
            },
        );
        let _r = watcher.tick(ctx_at(FIXTURE_CREATED_AT_SLOT + 1, 70_000)).await;

        // Now check health far in the future.
        let health = watcher
            .health(FIXTURE_CREATED_AT_SLOT + 1, 80_000)
            .await
            .unwrap();
        assert!(health.is_offline, "should be offline after threshold elapsed");
    }

    #[tokio::test]
    async fn health_stale_rule_count_works() {
        let h = harness().await;

        // Two rules: one will be ticked recently, one will have a
        // very old created_at_ms (treated as "never ticked, old").
        let rule_fresh = fixture_solend_rule([0x71; 16]);
        let rule_old = fixture_solend_rule([0x72; 16]);
        h.repo.insert(&rule_fresh).await.unwrap();
        h.repo.insert(&rule_old).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(100_000)),
            Stage2WatcherConfig {
                stale_threshold_ms: 1_000,
                ..Stage2WatcherConfig::default()
            },
        );

        // Tick to mark the fresh rule.
        let _r = watcher.tick(ctx_at(rule_fresh.expires_at_slot - 1, 100_000)).await;

        // Force-update the OLD rule's created_at_ms backwards in
        // time so it counts as stale (never ticked + old). Also
        // null out last_successful_tick_at_ms / last_checked_slot
        // — those got populated during tick(), but we want the
        // "never ticked AND old" branch of is_rule_stale to fire.
        sqlx::query(
            "UPDATE stage2_watch_rules
             SET created_at_ms = 0,
                 last_successful_tick_at_ms = NULL,
                 last_checked_slot = NULL
             WHERE rule_id = ?",
        )
        .bind(hex_id(&rule_old.rule_id))
        .execute(h.repo.pool())
        .await
        .unwrap();

        let health = watcher
            .health(rule_fresh.expires_at_slot - 1, 100_500)
            .await
            .unwrap();
        // rule_fresh: ticked at 100_000, now=100_500, threshold 1000 → fresh.
        // rule_old: never ticked, created_at_ms=0, now=100_500 → stale.
        assert_eq!(health.stale_rule_count, 1);
        assert_eq!(health.active_rule_count, 2);
    }

    // ── Bounded tick / max_rules_per_tick ─────────────────────────────

    #[tokio::test]
    async fn tick_respects_max_rules_per_tick() {
        let h = harness().await;
        for i in 0..5_u8 {
            let r = fixture_solend_rule([0x80 + i; 16]);
            h.repo.insert(&r).await.unwrap();
        }

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_false()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(110_000)),
            Stage2WatcherConfig {
                max_rules_per_tick: 2,
                ..Stage2WatcherConfig::default()
            },
        );

        let report = watcher
            .tick(ctx_at(FIXTURE_EXPIRES_AT_SLOT - 1, 110_000))
            .await;
        assert_eq!(report.rules_loaded, 2);
        assert!(report.rules_loaded_at_limit);
    }

    // ── Action coverage: Jupiter rule processed identically ────────────

    #[tokio::test]
    async fn jupiter_action_rule_is_processed() {
        let h = harness().await;
        let rule = fixture_jupiter_rule([0x90; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = watcher_with(
            h.repo.clone(),
            Arc::new(always_true()),
            Arc::new(simulator_ok()),
            Arc::new(MockClock::pinned(120_000)),
            Stage2WatcherConfig::default(),
        );
        let report = watcher
            .tick(ctx_at(rule.expires_at_slot - 1, 120_000))
            .await;
        assert_eq!(report.condition_met_count, 1);
    }

    // ── No-op evaluator + simulator ────────────────────────────────────

    #[tokio::test]
    async fn default_noop_evaluator_never_advances() {
        let h = harness().await;
        let rule = fixture_solend_rule([0xA0; 16]);
        h.repo.insert(&rule).await.unwrap();

        let watcher = Stage2Watcher::new(h.repo.clone(), Stage2WatcherConfig::default());
        let report = watcher.tick(ctx_at(rule.expires_at_slot - 1, 130_000)).await;
        assert_eq!(report.condition_met_count, 0);
        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
    }
}
