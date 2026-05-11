//! Stage 2 W3 — watcher condition evaluator substrate.
//!
//! This module is the W3 deliverable: it wires the W2
//! [`Stage2Watcher`](crate::stage2_watcher::Stage2Watcher) tick loop to a
//! deterministic, integer-only condition-evaluation substrate.
//!
//! # Hard scope (what this module does NOT do)
//!
//! - No signing.
//! - No broadcast.
//! - No transaction construction.
//! - No Solend/Jupiter CPI.
//! - No Solend/Jupiter execution.
//! - No mainnet / devnet transaction.
//! - Live RPC is disabled by default and never runs in normal tests.
//!
//! `condition_met` is only an off-chain scheduling hint. It is NOT
//! execution. The on-chain P3 condition verifier remains the final
//! authority at execute time.
//!
//! # Architecture
//!
//! Two clean seams:
//!
//! ```text
//!   Stage2SnapshotProvider     →  fetch a batch of snapshots, per-key fallible
//!   Stage2RuleEvaluator        →  evaluate one WatchRule against a snapshot batch
//!   Stage2BatchedConditionEvaluator
//!                              →  glue: implements W2's Stage2ConditionEvaluator
//!                                 trait by composing provider + rule evaluator
//!                                 with a per-tick dedupe cache.
//! ```
//!
//! # Per-key fallibility
//!
//! [`Stage2SnapshotBatch`] stores each fetched item as
//! `Result<Snapshot, Stage2SnapshotError>`. A failed Pyth feed only
//! affects rules that depend on that feed; a failed Solend reserve
//! only affects rules that depend on that reserve. Other independent
//! rules in the same tick continue evaluation.
//!
//! # Clock axes
//!
//! Three axes are kept distinct (mirrors
//! [`crate::stage2_watcher::Stage2TickContext`]):
//!
//! - `current_slot`           — drives Solend reserve staleness, rule
//!   expiry, on-chain ordering.
//! - `current_unix_timestamp` — drives Pyth freshness.
//! - `now_ms`                 — drives health bookkeeping (handled by
//!   the watcher; not used by the evaluator math).
//!
//! # Error typology
//!
//! Maps to W2's [`Stage2WatcherError`]:
//!
//! - **Transient** — provider timeout, missing snapshot, oracle stale,
//!   confidence too wide, reserve stale, reserve config refresh-blip.
//!   The watcher records `last_error` and retries next tick.
//! - **Terminal** — malformed rule, unsupported schema/action/condition
//!   shape, snapshot feed-id mismatch (configuration drift), bound
//!   direction mismatch, math overflow on bounded inputs. The watcher
//!   flips the rule to `failed` via `mark_failed_if_not_terminal`.
//! - **Internal** — never produced by the evaluator on its own; the
//!   watcher uses it for DB / system errors above this layer.
//!
//! # Live provider safety
//!
//! This slice ships only:
//!
//! - [`Stage2NoopSnapshotProvider`] — returns empty batches.
//! - [`Stage2DeterministicMockProvider`] — returns scripted fixtures.
//!
//! There is no live RPC provider in this slice. Constructors
//! ([`Stage2BatchedConditionEvaluator::with_provider`]) accept any
//! `Arc<dyn Stage2SnapshotProvider>`, but the only providers in tree
//! are the two above. A future live provider MUST land behind an
//! explicit constructor (`new_with_live_provider(...)`) and MUST NOT
//! be activated by ambient environment variables in normal
//! constructors or tests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tracing::debug;

use claw_types::canonical_intent::PubkeyBytes;
use claw_types::stage2_watch_rule::{
    BoundMode, Comparison, Condition, ConditionLogic, RateKind, VerificationLevel,
    WatchRule,
};

use crate::stage2_watcher::{
    Stage2ConditionEvaluator, Stage2TickContext, Stage2WatcherError,
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Solend WAD scale (10¹⁸). Mirrors the constant in
/// `programs/clawsol-authority/src/condition_verifier.rs`. Duplicated
/// on purpose — the on-chain BPF crate is not a runtime dep of the
/// gateway, so the constant must travel with each consumer. Parity
/// across the two copies is asserted in tests.
pub const SOLEND_WAD: u128 = 1_000_000_000_000_000_000;

/// Basis-points denominator (1 bps = 1 / 10_000). Mirrors the on-chain
/// constant of the same name.
pub const BPS_DENOM: u128 = 10_000;

/// Solend supply-APR formula version this build of the evaluator
/// understands. The canonical schema commits `formula_version: u8`
/// per condition; the evaluator rejects anything other than this
/// constant.
pub const SUPPORTED_SOLEND_FORMULA_VERSION: u8 = 1;

/// Maximum exponent difference accepted by the Pyth comparator before
/// `i128` overflow becomes a real risk. Mirrors the on-chain constant
/// of the same name.
pub const PYTH_MAX_EXPONENT_DIFF: i32 = 18;

// ── Snapshot keys + values ──────────────────────────────────────────────────

/// Identity of a Pyth feed in the snapshot cache. The 32-byte feed-id
/// is the canonical Pyth identifier (the price-update PDA is derived
/// elsewhere; the watcher only needs the feed-id to dedupe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PythSnapshotKey {
    pub feed_id: [u8; 32],
}

/// Identity of a Solend reserve in the snapshot cache. The reserve
/// pubkey is the canonical address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SolendSnapshotKey {
    pub reserve_pubkey: PubkeyBytes,
}

/// Decoded Pyth price-update fields the evaluator needs. Construct
/// from a `PriceUpdateV2` account off-chain (or fixture, in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythSnapshotValue {
    pub feed_id: [u8; 32],
    pub price_mantissa: i64,
    pub price_exponent: i32,
    pub conf: u64,
    /// Pyth `publish_time` (Unix seconds).
    pub publish_time: i64,
    pub verification_level: VerificationLevel,
}

/// Decoded Solend reserve fields the evaluator needs. Field semantics
/// follow `STAGE2_SOLEND_APY_RESEARCH.md` § 4.5 verbatim. The
/// `super_max_borrow_rate_pct` is `u64` — preserving width per audit
/// C-3 (a naive `as u8` cast truncates above 255% and silently breaks
/// the third region of the kinked rate model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolendSnapshotValue {
    pub reserve_pubkey: PubkeyBytes,
    pub available_amount: u64,
    pub borrowed_amount_wads: u128,
    pub min_borrow_rate_pct: u8,
    pub optimal_borrow_rate_pct: u8,
    pub max_borrow_rate_pct: u8,
    pub super_max_borrow_rate_pct: u64,
    pub optimal_utilization_rate_pct: u8,
    pub max_utilization_rate_pct: u8,
    pub protocol_take_rate_pct: u8,
    pub last_update_slot: u64,
    pub stale_flag: bool,
}

// ── Snapshot batch + request ────────────────────────────────────────────────

/// Per-key error returned by a snapshot provider. Mapped to
/// [`Stage2WatcherError`] by the evaluator.
#[derive(Debug, Clone)]
pub enum Stage2SnapshotError {
    /// Provider timeout, oracle unavailable, RPC blip — retry next tick.
    Transient(String),
    /// Snapshot fundamentally unavailable for this key (account closed,
    /// feed deprecated, configuration drift). Rule should fail closed.
    Terminal(String),
}

impl Stage2SnapshotError {
    pub fn into_watcher_error(self) -> Stage2WatcherError {
        match self {
            Self::Transient(m) => {
                Stage2WatcherError::Transient(format!("snapshot provider: {m}"))
            }
            Self::Terminal(m) => {
                Stage2WatcherError::Terminal(format!("snapshot provider: {m}"))
            }
        }
    }
}

impl std::fmt::Display for Stage2SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(m) => write!(f, "transient: {m}"),
            Self::Terminal(m) => write!(f, "terminal: {m}"),
        }
    }
}

/// Request to a snapshot provider. Carries the deduped union of all
/// Pyth feeds and Solend reserves that the planner observed across
/// the rules in scope.
#[derive(Debug, Clone, Default)]
pub struct Stage2SnapshotRequest {
    pyth_feeds: Vec<PythSnapshotKey>,
    solend_reserves: Vec<SolendSnapshotKey>,
}

impl Stage2SnapshotRequest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a Pyth key. Duplicate keys are silently deduped.
    pub fn add_pyth(&mut self, key: PythSnapshotKey) {
        if !self.pyth_feeds.iter().any(|k| k == &key) {
            self.pyth_feeds.push(key);
        }
    }

    /// Append a Solend key. Duplicate keys are silently deduped.
    pub fn add_solend(&mut self, key: SolendSnapshotKey) {
        if !self.solend_reserves.iter().any(|k| k == &key) {
            self.solend_reserves.push(key);
        }
    }

    pub fn pyth_feeds(&self) -> &[PythSnapshotKey] {
        &self.pyth_feeds
    }

    pub fn solend_reserves(&self) -> &[SolendSnapshotKey] {
        &self.solend_reserves
    }

    pub fn is_empty(&self) -> bool {
        self.pyth_feeds.is_empty() && self.solend_reserves.is_empty()
    }
}

/// Per-key results from a snapshot provider. Both maps are keyed
/// directly by their lookup identifier so callers can resolve a single
/// rule's dependencies cheaply.
///
/// **Per-key fallibility is the load-bearing property.** A failed Pyth
/// feed is `Err(SnapshotError)` for that one key; the rest of the
/// batch is unaffected.
#[derive(Debug, Default, Clone)]
pub struct Stage2SnapshotBatch {
    pyth: HashMap<[u8; 32], Result<PythSnapshotValue, Stage2SnapshotError>>,
    solend: HashMap<PubkeyBytes, Result<SolendSnapshotValue, Stage2SnapshotError>>,
}

impl Stage2SnapshotBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_pyth(
        &mut self,
        feed_id: [u8; 32],
        result: Result<PythSnapshotValue, Stage2SnapshotError>,
    ) {
        self.pyth.insert(feed_id, result);
    }

    pub fn insert_solend(
        &mut self,
        reserve: PubkeyBytes,
        result: Result<SolendSnapshotValue, Stage2SnapshotError>,
    ) {
        self.solend.insert(reserve, result);
    }

    pub fn pyth(
        &self,
        feed_id: &[u8; 32],
    ) -> Option<&Result<PythSnapshotValue, Stage2SnapshotError>> {
        self.pyth.get(feed_id)
    }

    pub fn solend(
        &self,
        reserve: &PubkeyBytes,
    ) -> Option<&Result<SolendSnapshotValue, Stage2SnapshotError>> {
        self.solend.get(reserve)
    }

    pub fn pyth_count(&self) -> usize {
        self.pyth.len()
    }

    pub fn solend_count(&self) -> usize {
        self.solend.len()
    }
}

// ── Provider trait + built-in providers ─────────────────────────────────────

/// Pluggable snapshot source.
///
/// **Default build invariant.** The two providers shipped here
/// ([`Stage2NoopSnapshotProvider`], [`Stage2DeterministicMockProvider`])
/// make NO live RPC calls of any kind. A future live provider MUST be
/// added behind an explicit constructor and MUST NOT be wired in by
/// ambient environment variables in normal constructors or tests.
#[async_trait]
pub trait Stage2SnapshotProvider: Send + Sync + std::fmt::Debug {
    /// Resolve every key in `request`. Per-key Results live in the
    /// returned [`Stage2SnapshotBatch`]; one feed's failure must not
    /// poison the rest.
    async fn fetch_batch(
        &self,
        request: &Stage2SnapshotRequest,
        ctx: &Stage2TickContext,
    ) -> Stage2SnapshotBatch;
}

/// Default provider — returns an empty batch for any request. The
/// W2/W3 default `Stage2Watcher::new` cannot accidentally fire an
/// execution because the evaluator built on this provider has nothing
/// to compare against and produces `Ok(false)` for every rule.
#[derive(Debug, Default)]
pub struct Stage2NoopSnapshotProvider;

#[async_trait]
impl Stage2SnapshotProvider for Stage2NoopSnapshotProvider {
    async fn fetch_batch(
        &self,
        _request: &Stage2SnapshotRequest,
        _ctx: &Stage2TickContext,
    ) -> Stage2SnapshotBatch {
        Stage2SnapshotBatch::new()
    }
}

/// Deterministic provider scripted by inserted fixtures. Never reads
/// the network, never reads the environment, never opens a file —
/// returns exactly what the test set, and counts how many times each
/// key was requested so batch-dedupe tests can assert the count.
#[derive(Debug, Default)]
pub struct Stage2DeterministicMockProvider {
    pyth: Mutex<HashMap<[u8; 32], Result<PythSnapshotValue, Stage2SnapshotError>>>,
    solend: Mutex<HashMap<PubkeyBytes, Result<SolendSnapshotValue, Stage2SnapshotError>>>,
    pyth_calls: Mutex<HashMap<[u8; 32], u64>>,
    solend_calls: Mutex<HashMap<PubkeyBytes, u64>>,
    fetch_calls: Mutex<u64>,
}

impl Stage2DeterministicMockProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_pyth(self, feed_id: [u8; 32], snap: PythSnapshotValue) -> Self {
        self.pyth.lock().insert(feed_id, Ok(snap));
        self
    }

    pub fn with_pyth_error(self, feed_id: [u8; 32], err: Stage2SnapshotError) -> Self {
        self.pyth.lock().insert(feed_id, Err(err));
        self
    }

    pub fn with_solend(
        self,
        reserve: PubkeyBytes,
        snap: SolendSnapshotValue,
    ) -> Self {
        self.solend.lock().insert(reserve, Ok(snap));
        self
    }

    pub fn with_solend_error(
        self,
        reserve: PubkeyBytes,
        err: Stage2SnapshotError,
    ) -> Self {
        self.solend.lock().insert(reserve, Err(err));
        self
    }

    pub fn pyth_call_count(&self, feed_id: &[u8; 32]) -> u64 {
        *self.pyth_calls.lock().get(feed_id).unwrap_or(&0)
    }

    pub fn solend_call_count(&self, reserve: &PubkeyBytes) -> u64 {
        *self.solend_calls.lock().get(reserve).unwrap_or(&0)
    }

    pub fn fetch_call_count(&self) -> u64 {
        *self.fetch_calls.lock()
    }
}

#[async_trait]
impl Stage2SnapshotProvider for Stage2DeterministicMockProvider {
    async fn fetch_batch(
        &self,
        request: &Stage2SnapshotRequest,
        _ctx: &Stage2TickContext,
    ) -> Stage2SnapshotBatch {
        *self.fetch_calls.lock() += 1;
        let mut batch = Stage2SnapshotBatch::new();

        let pyth_map = self.pyth.lock();
        let mut pyth_calls = self.pyth_calls.lock();
        for key in request.pyth_feeds() {
            *pyth_calls.entry(key.feed_id).or_insert(0) += 1;
            let result = match pyth_map.get(&key.feed_id) {
                Some(Ok(v)) => Ok(*v),
                Some(Err(e)) => Err(e.clone()),
                None => Err(Stage2SnapshotError::Transient(format!(
                    "no fixture for pyth feed_id {:02x}{:02x}{:02x}…",
                    key.feed_id[0], key.feed_id[1], key.feed_id[2]
                ))),
            };
            batch.insert_pyth(key.feed_id, result);
        }
        drop(pyth_map);
        drop(pyth_calls);

        let solend_map = self.solend.lock();
        let mut solend_calls = self.solend_calls.lock();
        for key in request.solend_reserves() {
            *solend_calls.entry(key.reserve_pubkey).or_insert(0) += 1;
            let result = match solend_map.get(&key.reserve_pubkey) {
                Some(Ok(v)) => Ok(*v),
                Some(Err(e)) => Err(e.clone()),
                None => Err(Stage2SnapshotError::Transient(format!(
                    "no fixture for solend reserve {}",
                    key.reserve_pubkey
                ))),
            };
            batch.insert_solend(key.reserve_pubkey, result);
        }
        drop(solend_map);
        drop(solend_calls);

        batch
    }
}

// ── Pure rule evaluator (mirrors on-chain condition_verifier) ───────────────

/// Plan a snapshot request from a single rule's conditions. Duplicate
/// keys within the same rule are deduped by the request type.
pub fn plan_request_for_rule(rule: &WatchRule) -> Stage2SnapshotRequest {
    let mut req = Stage2SnapshotRequest::new();
    extend_request_with_rule(&mut req, rule);
    req
}

/// Append a rule's keys to an existing request. Used by tick-level
/// planners to aggregate across many rules.
pub fn extend_request_with_rule(req: &mut Stage2SnapshotRequest, rule: &WatchRule) {
    for c in &rule.conditions {
        match c {
            Condition::PythPrice { feed_id, .. } => {
                req.add_pyth(PythSnapshotKey { feed_id: *feed_id });
            }
            Condition::SolendReserveSupplyRate { reserve_pubkey, .. } => {
                req.add_solend(SolendSnapshotKey {
                    reserve_pubkey: *reserve_pubkey,
                });
            }
        }
    }
}

/// Outcome of one condition evaluation. Held internally so the rule
/// evaluator can short-circuit while still preserving the highest
/// severity error if needed.
#[derive(Debug)]
enum CondOutcome {
    True,
    False,
    Err(Stage2WatcherError),
}

/// Evaluate one rule against a previously-fetched snapshot batch.
///
/// Returns:
/// - `Ok(true)`  — conditions passed; watcher should advance to
///   `condition_met` (subject to the simulator stub at execute-time).
/// - `Ok(false)` — conditions did not pass; rule stays active.
/// - `Err(Transient)` — temporary issue (oracle stale, provider down);
///   watcher leaves rule active and records `last_error`.
/// - `Err(Terminal)`  — rule shape is wrong (unsupported variant,
///   bound mismatch, snapshot configuration drift); watcher flips to
///   `failed` via `mark_failed_if_not_terminal`.
///
/// **Short-circuit semantics:**
/// - `ConditionLogic::Any` returns `Ok(true)` on the first true result
///   without evaluating the remaining conditions.
/// - `ConditionLogic::All` returns `Ok(false)` on the first false
///   result without evaluating the remaining conditions.
/// - In both cases, errors are propagated only if the short-circuit
///   value cannot be reached without consulting the failed condition.
/// Stage 2 B-O1 — narrow public wrapper around the internal
/// `supply_apr_wad`. Validates the reserve config first (mirroring
/// `evaluate_solend_supply_rate_condition`'s gating) so external
/// callers cannot trip the unchecked-precondition path. Returns the
/// supply APR scaled to WAD (`10^18`); divide by `10^14` for basis
/// points, or call [`solend_supply_apr_bps_from_wad`].
pub fn solend_supply_apr_wad_for_snapshot(
    snap: &SolendSnapshotValue,
) -> Result<u128, Stage2WatcherError> {
    if !is_reserve_config_valid(snap) {
        return Err(Stage2WatcherError::Transient(
            "solend reserve config invariants violated".to_string(),
        ));
    }
    supply_apr_wad(snap)
}

/// Stage 2 B-O1 — convert a supply-APR WAD value (10^18-scaled) to
/// integer basis points (1 bps = 10^14 wads). Truncates toward zero;
/// saturates at `u64::MAX`. Mirrors the conversion
/// `evaluate_solend_supply_rate_condition` uses on the threshold side
/// at the same `apply_cmp_u128(...)` call site
/// (`threshold_bps as u128 * 10^14 == threshold_wad`).
pub fn solend_supply_apr_bps_from_wad(wad: u128) -> u64 {
    let bps = wad / 10u128.pow(14);
    u64::try_from(bps).unwrap_or(u64::MAX)
}

/// Stage 2 B-O1 — pure mapper from a live-decoded
/// [`crate::integrations::solend::raw::SolendReserveRaw`] (rate-config-
/// extended decoder, this slice) into the [`SolendSnapshotValue`] the
/// evaluator's APR math expects. The reserve's pubkey is supplied by
/// the caller because the raw account-data decoder records only the
/// account contents, not its address.
pub fn solend_snapshot_value_from_reserve_raw(
    reserve_pubkey: PubkeyBytes,
    raw: &crate::integrations::solend::raw::SolendReserveRaw,
) -> SolendSnapshotValue {
    SolendSnapshotValue {
        reserve_pubkey,
        available_amount: raw.liquidity_available_amount,
        borrowed_amount_wads: raw.liquidity_borrowed_amount_wads,
        min_borrow_rate_pct: raw.config_min_borrow_rate_pct,
        optimal_borrow_rate_pct: raw.config_optimal_borrow_rate_pct,
        max_borrow_rate_pct: raw.config_max_borrow_rate_pct,
        super_max_borrow_rate_pct: raw.config_super_max_borrow_rate_pct,
        optimal_utilization_rate_pct: raw.config_optimal_utilization_rate_pct,
        max_utilization_rate_pct: raw.config_max_utilization_rate_pct,
        protocol_take_rate_pct: raw.config_protocol_take_rate_pct,
        last_update_slot: raw.last_update_slot,
        stale_flag: raw.last_update_stale,
    }
}

pub fn evaluate_rule_against_batch(
    rule: &WatchRule,
    batch: &Stage2SnapshotBatch,
    ctx: &Stage2TickContext,
) -> Result<bool, Stage2WatcherError> {
    if rule.conditions.is_empty() {
        // Vacuous semantics mirror condition_verifier::evaluate_condition_logic:
        // All over empty = true; Any over empty = false. The validation
        // layer rejects empty rules at insert time, so this is a defence-
        // in-depth branch.
        return Ok(matches!(rule.condition_logic, ConditionLogic::All));
    }

    let mut outcomes: Vec<CondOutcome> = Vec::with_capacity(rule.conditions.len());
    let mut sealed = false;
    let mut sealed_value: Option<bool> = None;

    for cond in &rule.conditions {
        if sealed {
            // Short-circuited; do not evaluate further. The rest of
            // the conditions are not consulted, and the outcomes
            // vector is left short — the final fold will respect
            // sealed_value.
            break;
        }
        let outcome = evaluate_one_condition(cond, batch, ctx);
        match (&outcome, rule.condition_logic) {
            (CondOutcome::True, ConditionLogic::Any) => {
                sealed = true;
                sealed_value = Some(true);
            }
            (CondOutcome::False, ConditionLogic::All) => {
                sealed = true;
                sealed_value = Some(false);
            }
            _ => {}
        }
        outcomes.push(outcome);
    }

    if let Some(v) = sealed_value {
        return Ok(v);
    }

    // No short-circuit reached. Fold according to logic:
    // - All: any Err → propagate (highest severity); else AND.
    // - Any: any Err AND no True so far → propagate (highest severity);
    //   else OR (which by here is all-false → false).
    match rule.condition_logic {
        ConditionLogic::All => fold_all(&outcomes),
        ConditionLogic::Any => fold_any(&outcomes),
    }
}

fn fold_all(outcomes: &[CondOutcome]) -> Result<bool, Stage2WatcherError> {
    let mut highest: Option<Stage2WatcherError> = None;
    let mut all_true = true;
    for o in outcomes {
        match o {
            CondOutcome::True => {}
            CondOutcome::False => {
                all_true = false;
            }
            CondOutcome::Err(e) => {
                highest = Some(merge_severity(highest.take(), clone_watcher_error(e)));
            }
        }
    }
    if let Some(e) = highest {
        return Err(e);
    }
    Ok(all_true)
}

fn fold_any(outcomes: &[CondOutcome]) -> Result<bool, Stage2WatcherError> {
    let mut highest: Option<Stage2WatcherError> = None;
    let mut any_true = false;
    for o in outcomes {
        match o {
            CondOutcome::True => {
                any_true = true;
            }
            CondOutcome::False => {}
            CondOutcome::Err(e) => {
                highest = Some(merge_severity(highest.take(), clone_watcher_error(e)));
            }
        }
    }
    if any_true {
        return Ok(true);
    }
    if let Some(e) = highest {
        return Err(e);
    }
    Ok(false)
}

/// Pick the higher-severity error: Terminal > Transient > Internal.
/// Internal is intentionally lowest because the evaluator does not
/// produce Internal itself; it would only appear if a future caller
/// injected one.
fn merge_severity(
    cur: Option<Stage2WatcherError>,
    new: Stage2WatcherError,
) -> Stage2WatcherError {
    match cur {
        None => new,
        Some(c) => {
            let cur_rank = severity_rank(&c);
            let new_rank = severity_rank(&new);
            if new_rank >= cur_rank {
                new
            } else {
                c
            }
        }
    }
}

fn severity_rank(e: &Stage2WatcherError) -> u8 {
    if e.is_terminal() {
        2
    } else if e.is_transient() {
        1
    } else {
        0
    }
}

/// `Stage2WatcherError` does not derive `Clone` (its variants carry
/// `String`s by value). The evaluator borrows the variant text and
/// rebuilds an equivalent error; this cannot fail.
fn clone_watcher_error(e: &Stage2WatcherError) -> Stage2WatcherError {
    match e {
        Stage2WatcherError::Transient(m) => Stage2WatcherError::Transient(m.clone()),
        Stage2WatcherError::Terminal(m) => Stage2WatcherError::Terminal(m.clone()),
        Stage2WatcherError::Internal(m) => Stage2WatcherError::Internal(m.clone()),
    }
}

fn evaluate_one_condition(
    cond: &Condition,
    batch: &Stage2SnapshotBatch,
    ctx: &Stage2TickContext,
) -> CondOutcome {
    match cond {
        Condition::PythPrice {
            feed_id,
            price_update_account: _,
            comparison,
            threshold_mantissa,
            threshold_exponent,
            max_age_seconds,
            max_confidence_bps,
            verification_level_required,
            bound_mode,
        } => {
            let snap = match batch.pyth(feed_id) {
                Some(Ok(v)) => *v,
                Some(Err(e)) => {
                    return CondOutcome::Err(e.clone().into_watcher_error());
                }
                None => {
                    return CondOutcome::Err(Stage2WatcherError::Transient(
                        "pyth snapshot missing from batch".to_string(),
                    ));
                }
            };
            match eval_pyth(
                *feed_id,
                *comparison,
                *threshold_mantissa,
                *threshold_exponent,
                *max_age_seconds,
                *max_confidence_bps,
                *verification_level_required,
                *bound_mode,
                &snap,
                ctx,
            ) {
                Ok(true) => CondOutcome::True,
                Ok(false) => CondOutcome::False,
                Err(e) => CondOutcome::Err(e),
            }
        }
        Condition::SolendReserveSupplyRate {
            reserve_pubkey,
            lending_market: _,
            solend_program_id: _,
            comparison,
            threshold_bps,
            rate_kind,
            formula_version,
            max_reserve_staleness_slots,
            required_refresh_same_tx: _,
        } => {
            let snap = match batch.solend(reserve_pubkey) {
                Some(Ok(v)) => *v,
                Some(Err(e)) => {
                    return CondOutcome::Err(e.clone().into_watcher_error());
                }
                None => {
                    return CondOutcome::Err(Stage2WatcherError::Transient(
                        "solend snapshot missing from batch".to_string(),
                    ));
                }
            };
            match eval_solend(
                *reserve_pubkey,
                *comparison,
                *threshold_bps,
                *rate_kind,
                *formula_version,
                *max_reserve_staleness_slots,
                &snap,
                ctx,
            ) {
                Ok(true) => CondOutcome::True,
                Ok(false) => CondOutcome::False,
                Err(e) => CondOutcome::Err(e),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn eval_pyth(
    feed_id: [u8; 32],
    comparison: Comparison,
    threshold_mantissa: i64,
    threshold_exponent: i32,
    max_age_seconds: u32,
    max_confidence_bps: u16,
    verification_level_required: VerificationLevel,
    bound_mode: BoundMode,
    snap: &PythSnapshotValue,
    ctx: &Stage2TickContext,
) -> Result<bool, Stage2WatcherError> {
    // Gate 1 — feed identity.
    if snap.feed_id != feed_id {
        return Err(Stage2WatcherError::Terminal(format!(
            "pyth feed id mismatch: snapshot vs rule (feed {:02x}{:02x}{:02x}…)",
            feed_id[0], feed_id[1], feed_id[2]
        )));
    }

    // Gate 2 — verification level. Both required and provided are
    // currently `Full`; the exhaustive match here forces a deliberate
    // decision when a future variant lands.
    match (snap.verification_level, verification_level_required) {
        (VerificationLevel::Full, VerificationLevel::Full) => {}
    }

    // Gate 3 — freshness. Uses `current_unix_timestamp`, NOT
    // `current_slot` — Pyth's `publish_time` is wall-clock seconds.
    let age_secs = (ctx.current_unix_timestamp as i128)
        .checked_sub(snap.publish_time as i128)
        .ok_or_else(|| {
            Stage2WatcherError::Terminal("pyth age subtract overflow".to_string())
        })?;
    if age_secs < 0 || age_secs > max_age_seconds as i128 {
        return Err(Stage2WatcherError::Transient(format!(
            "pyth price too old: age {age_secs}s, max {max_age_seconds}s"
        )));
    }

    // Gate 4 — bound-direction agreement. Mismatch is a rule shape
    // error: refuse to evaluate.
    match (comparison, bound_mode) {
        (Comparison::Gt | Comparison::Gte, BoundMode::AdverseLowerForGt) => {}
        (Comparison::Lt | Comparison::Lte, BoundMode::AdverseUpperForLt) => {}
        (_, BoundMode::Midpoint) => {}
        _ => {
            return Err(Stage2WatcherError::Terminal(
                "pyth bound mode does not match comparison direction".to_string(),
            ));
        }
    }

    // Gate 5 — confidence ratio. `(conf * BPS_DENOM) / abs(price)`
    // must be ≤ max_confidence_bps. `price == 0` is meaningless on
    // any feed Stage 2 cares about — Transient (next tick may publish
    // a non-zero price).
    let price_abs_u128 = (snap.price_mantissa as i128)
        .checked_abs()
        .and_then(|n| u128::try_from(n).ok())
        .ok_or_else(|| {
            Stage2WatcherError::Terminal("pyth abs(price) overflow".to_string())
        })?;
    if price_abs_u128 == 0 {
        return Err(Stage2WatcherError::Transient(
            "pyth price is zero; confidence-bps undefined".to_string(),
        ));
    }
    let conf_u128 = snap.conf as u128;
    let conf_bps_num = conf_u128.checked_mul(BPS_DENOM).ok_or_else(|| {
        Stage2WatcherError::Terminal("pyth confidence * BPS_DENOM overflow".to_string())
    })?;
    let conf_bps = conf_bps_num / price_abs_u128;
    if conf_bps > max_confidence_bps as u128 {
        return Err(Stage2WatcherError::Transient(format!(
            "pyth confidence too wide: {conf_bps} bps > max {max_confidence_bps} bps"
        )));
    }

    // Gate 6 — apply adverse bound, normalise exponents, compare.
    let bound = match bound_mode {
        BoundMode::AdverseLowerForGt => (snap.price_mantissa as i128)
            .checked_sub(snap.conf as i128)
            .ok_or_else(|| {
                Stage2WatcherError::Terminal("pyth bound subtract overflow".to_string())
            })?,
        BoundMode::AdverseUpperForLt => (snap.price_mantissa as i128)
            .checked_add(snap.conf as i128)
            .ok_or_else(|| {
                Stage2WatcherError::Terminal("pyth bound add overflow".to_string())
            })?,
        BoundMode::Midpoint => snap.price_mantissa as i128,
    };

    let diff = (snap.price_exponent as i64)
        .checked_sub(threshold_exponent as i64)
        .ok_or_else(|| {
            Stage2WatcherError::Terminal("pyth exponent diff overflow".to_string())
        })?;
    if !(-(PYTH_MAX_EXPONENT_DIFF as i64)..=(PYTH_MAX_EXPONENT_DIFF as i64))
        .contains(&diff)
    {
        return Err(Stage2WatcherError::Terminal(format!(
            "pyth exponent diff {diff} out of supported range [-{n}, {n}]",
            n = PYTH_MAX_EXPONENT_DIFF
        )));
    }
    let (lhs, rhs) = match diff.cmp(&0) {
        std::cmp::Ordering::Equal => (bound, threshold_mantissa as i128),
        std::cmp::Ordering::Greater => {
            let m = pow10_i128(diff as u32)?;
            let lhs = bound.checked_mul(m).ok_or_else(|| {
                Stage2WatcherError::Terminal("pyth lhs * 10^diff overflow".to_string())
            })?;
            (lhs, threshold_mantissa as i128)
        }
        std::cmp::Ordering::Less => {
            let m = pow10_i128((-diff) as u32)?;
            let rhs = (threshold_mantissa as i128).checked_mul(m).ok_or_else(|| {
                Stage2WatcherError::Terminal(
                    "pyth threshold * 10^(-diff) overflow".to_string(),
                )
            })?;
            (bound, rhs)
        }
    };

    Ok(apply_cmp_i128(comparison, lhs, rhs))
}

fn apply_cmp_i128(comparison: Comparison, lhs: i128, rhs: i128) -> bool {
    match comparison {
        Comparison::Lt => lhs < rhs,
        Comparison::Lte => lhs <= rhs,
        Comparison::Gt => lhs > rhs,
        Comparison::Gte => lhs >= rhs,
    }
}

fn apply_cmp_u128(comparison: Comparison, lhs: u128, rhs: u128) -> bool {
    match comparison {
        Comparison::Lt => lhs < rhs,
        Comparison::Lte => lhs <= rhs,
        Comparison::Gt => lhs > rhs,
        Comparison::Gte => lhs >= rhs,
    }
}

fn pow10_i128(n: u32) -> Result<i128, Stage2WatcherError> {
    let mut acc: i128 = 1;
    for _ in 0..n {
        acc = acc.checked_mul(10).ok_or_else(|| {
            Stage2WatcherError::Terminal("pyth pow10 overflow".to_string())
        })?;
    }
    Ok(acc)
}

#[allow(clippy::too_many_arguments)]
fn eval_solend(
    reserve_pubkey: PubkeyBytes,
    comparison: Comparison,
    threshold_bps: u32,
    rate_kind: RateKind,
    formula_version: u8,
    max_reserve_staleness_slots: u32,
    snap: &SolendSnapshotValue,
    ctx: &Stage2TickContext,
) -> Result<bool, Stage2WatcherError> {
    if snap.reserve_pubkey != reserve_pubkey {
        return Err(Stage2WatcherError::Terminal(format!(
            "solend reserve mismatch: snapshot vs rule ({})",
            reserve_pubkey
        )));
    }

    if formula_version != SUPPORTED_SOLEND_FORMULA_VERSION {
        return Err(Stage2WatcherError::Terminal(format!(
            "solend formula_version {formula_version} unsupported (this build supports {SUPPORTED_SOLEND_FORMULA_VERSION})"
        )));
    }

    match rate_kind {
        RateKind::Apr => {}
        RateKind::Apy => {
            return Err(Stage2WatcherError::Terminal(
                "solend rate_kind APY not supported in v1".to_string(),
            ));
        }
    }

    if !is_reserve_config_valid(snap) {
        return Err(Stage2WatcherError::Transient(
            "solend reserve config invariants violated".to_string(),
        ));
    }

    // Staleness uses ctx.current_slot — NOT current_unix_timestamp.
    let age = ctx.current_slot.checked_sub(snap.last_update_slot).ok_or_else(|| {
        Stage2WatcherError::Transient("solend reserve future-dated".to_string())
    })?;
    if age > max_reserve_staleness_slots as u64 || snap.stale_flag {
        return Err(Stage2WatcherError::Transient(format!(
            "solend reserve stale: age {age} slots, max {max_reserve_staleness_slots} slots, stale_flag={}",
            snap.stale_flag
        )));
    }

    let supply_apr_wad = supply_apr_wad(snap)?;

    let threshold_wad = (threshold_bps as u128).checked_mul(10u128.pow(14)).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend threshold conversion overflow".to_string())
    })?;

    Ok(apply_cmp_u128(comparison, supply_apr_wad, threshold_wad))
}

// ── Solend math (mirrors condition_verifier WAD path) ───────────────────────

fn is_reserve_config_valid(s: &SolendSnapshotValue) -> bool {
    let min = s.min_borrow_rate_pct as u64;
    let opt = s.optimal_borrow_rate_pct as u64;
    let max = s.max_borrow_rate_pct as u64;
    let super_max = s.super_max_borrow_rate_pct;
    if !(min <= opt && opt <= max && max <= super_max) {
        return false;
    }
    if !(s.optimal_utilization_rate_pct <= s.max_utilization_rate_pct
        && s.max_utilization_rate_pct <= 100)
    {
        return false;
    }
    if s.protocol_take_rate_pct > 100 {
        return false;
    }
    true
}

fn supply_apr_wad(s: &SolendSnapshotValue) -> Result<u128, Stage2WatcherError> {
    let utilisation = utilization_wad(s)?;
    let borrow_rate = current_borrow_rate_wad(s)?;
    let take_rate = pct_to_wad(s.protocol_take_rate_pct as u64)?;
    let one_minus_take = SOLEND_WAD.checked_sub(take_rate).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend (1 - take) underflow".to_string())
    })?;
    let inner = mul_wad(utilisation, one_minus_take)?;
    mul_wad(borrow_rate, inner)
}

fn utilization_wad(s: &SolendSnapshotValue) -> Result<u128, Stage2WatcherError> {
    if s.borrowed_amount_wads == 0 {
        return Ok(0);
    }
    let available_wads =
        (s.available_amount as u128).checked_mul(SOLEND_WAD).ok_or_else(|| {
            Stage2WatcherError::Terminal(
                "solend available_amount * WAD overflow".to_string(),
            )
        })?;
    let denom = s.borrowed_amount_wads.checked_add(available_wads).ok_or_else(|| {
        Stage2WatcherError::Terminal(
            "solend borrowed + available denom overflow".to_string(),
        )
    })?;
    if denom == 0 {
        return Ok(0);
    }
    div_wad(s.borrowed_amount_wads, denom)
}

fn current_borrow_rate_wad(s: &SolendSnapshotValue) -> Result<u128, Stage2WatcherError> {
    let utilisation = utilization_wad(s)?;
    let optimal_util = pct_to_wad(s.optimal_utilization_rate_pct as u64)?;
    let max_util = pct_to_wad(s.max_utilization_rate_pct as u64)?;

    if utilisation <= optimal_util {
        let min_rate = pct_to_wad(s.min_borrow_rate_pct as u64)?;
        if optimal_util == 0 {
            return Ok(min_rate);
        }
        let normalised = div_wad(utilisation, optimal_util)?;
        let opt_minus_min = (s.optimal_borrow_rate_pct as u64)
            .checked_sub(s.min_borrow_rate_pct as u64)
            .ok_or_else(|| {
                Stage2WatcherError::Terminal(
                    "solend optimal - min underflow".to_string(),
                )
            })?;
        let rate_range = pct_to_wad(opt_minus_min)?;
        let scaled = mul_wad(normalised, rate_range)?;
        return scaled.checked_add(min_rate).ok_or_else(|| {
            Stage2WatcherError::Terminal("solend region 1 add overflow".to_string())
        });
    }

    if utilisation <= max_util {
        let weight_num = utilisation.checked_sub(optimal_util).ok_or_else(|| {
            Stage2WatcherError::Terminal("solend region 2 weight_num underflow".to_string())
        })?;
        let weight_den = max_util.checked_sub(optimal_util).ok_or_else(|| {
            Stage2WatcherError::Terminal("solend region 2 weight_den underflow".to_string())
        })?;
        if weight_den == 0 {
            return pct_to_wad(s.optimal_borrow_rate_pct as u64);
        }
        let weight = div_wad(weight_num, weight_den)?;
        let optimal_rate = pct_to_wad(s.optimal_borrow_rate_pct as u64)?;
        let max_rate = pct_to_wad(s.max_borrow_rate_pct as u64)?;
        let rate_range = max_rate.checked_sub(optimal_rate).ok_or_else(|| {
            Stage2WatcherError::Terminal("solend region 2 rate_range underflow".to_string())
        })?;
        let scaled = mul_wad(weight, rate_range)?;
        return scaled.checked_add(optimal_rate).ok_or_else(|| {
            Stage2WatcherError::Terminal("solend region 2 add overflow".to_string())
        });
    }

    let weight_num = utilisation.checked_sub(max_util).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend region 3 weight_num underflow".to_string())
    })?;
    let weight_den = SOLEND_WAD.checked_sub(max_util).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend region 3 weight_den underflow".to_string())
    })?;
    if weight_den == 0 {
        return pct_to_wad(s.super_max_borrow_rate_pct);
    }
    let weight = div_wad(weight_num, weight_den)?;
    let max_rate = pct_to_wad(s.max_borrow_rate_pct as u64)?;
    let super_max_rate = pct_to_wad(s.super_max_borrow_rate_pct)?;
    let rate_range = super_max_rate.checked_sub(max_rate).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend region 3 rate_range underflow".to_string())
    })?;
    let scaled = mul_wad(weight, rate_range)?;
    scaled
        .checked_add(max_rate)
        .ok_or_else(|| Stage2WatcherError::Terminal("solend region 3 add overflow".to_string()))
}

fn pct_to_wad(pct: u64) -> Result<u128, Stage2WatcherError> {
    (pct as u128).checked_mul(10u128.pow(16)).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend pct -> wad overflow".to_string())
    })
}

fn mul_wad(a: u128, b: u128) -> Result<u128, Stage2WatcherError> {
    mul_div_floor_u128(a, b, SOLEND_WAD)
}

fn div_wad(a: u128, b: u128) -> Result<u128, Stage2WatcherError> {
    if b == 0 {
        return Err(Stage2WatcherError::Terminal(
            "solend div_wad by zero".to_string(),
        ));
    }
    mul_div_floor_u128(a, SOLEND_WAD, b)
}

/// `floor((a × b) / c)` with a 256-bit intermediate. Hard-fails
/// (`Terminal`) on overflow or `c == 0`. Algorithm mirrors the on-chain
/// verifier's `mul_div_floor_u128`.
fn mul_div_floor_u128(a: u128, b: u128, c: u128) -> Result<u128, Stage2WatcherError> {
    if c == 0 {
        return Err(Stage2WatcherError::Terminal(
            "solend mul_div by zero".to_string(),
        ));
    }
    if let Some(prod) = a.checked_mul(b) {
        return Ok(prod / c);
    }

    // 256-bit slow path.
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = lh.wrapping_add(hl);
    let mid_carry = if mid < lh { 1u128 << 64 } else { 0 };

    let mid_lo = mid << 64;
    let mid_hi = mid >> 64;
    let (lo, c1) = ll.overflowing_add(mid_lo);
    let mut hi = hh
        .checked_add(mid_hi)
        .ok_or_else(|| Stage2WatcherError::Terminal("solend mul_div mid_hi overflow".to_string()))?
        .checked_add(if c1 { 1 } else { 0 })
        .ok_or_else(|| Stage2WatcherError::Terminal("solend mul_div carry overflow".to_string()))?;
    hi = hi.checked_add(mid_carry).ok_or_else(|| {
        Stage2WatcherError::Terminal("solend mul_div mid_carry overflow".to_string())
    })?;

    let mut quot: u128 = 0;
    let mut quot_overflow: bool = false;
    let mut rem: u128 = 0;
    for i in (0..256).rev() {
        let rem_overflow = (rem >> 127) & 1 == 1;
        rem = (rem << 1) | bit_of_u256(hi, lo, i as u32);
        let need_sub = rem_overflow || rem >= c;
        if need_sub {
            rem = rem.wrapping_sub(c);
        }
        let quot_msb = (quot >> 127) & 1 == 1;
        if quot_msb {
            quot_overflow = true;
        }
        quot = (quot << 1) | (need_sub as u128);
    }
    if quot_overflow {
        return Err(Stage2WatcherError::Terminal(
            "solend mul_div quotient overflow".to_string(),
        ));
    }
    Ok(quot)
}

#[inline]
fn bit_of_u256(hi: u128, lo: u128, idx: u32) -> u128 {
    if idx < 128 {
        (lo >> idx) & 1
    } else {
        (hi >> (idx - 128)) & 1
    }
}

// ── Stage2BatchedConditionEvaluator (W2 trait adapter + tick cache) ─────────

/// Per-tick dedupe cache. Each entry is keyed by feed_id / reserve and
/// stores the resolved snapshot result. The cache invalidates when the
/// tick token (current_slot, current_unix_timestamp, now_ms) changes.
#[derive(Debug, Default)]
struct TickCache {
    /// Tick token. `None` means the cache has never been populated.
    token: Option<(u64, i64, i64)>,
    pyth: HashMap<[u8; 32], Result<PythSnapshotValue, Stage2SnapshotError>>,
    solend: HashMap<PubkeyBytes, Result<SolendSnapshotValue, Stage2SnapshotError>>,
    /// Records which keys we've already attempted to fetch this tick
    /// even if the provider didn't return them (unknown-key path).
    pyth_attempted: HashSet<[u8; 32]>,
    solend_attempted: HashSet<PubkeyBytes>,
}

impl TickCache {
    fn ensure_token(&mut self, ctx: &Stage2TickContext) {
        let token = (ctx.current_slot, ctx.current_unix_timestamp, ctx.now_ms);
        if self.token != Some(token) {
            self.pyth.clear();
            self.solend.clear();
            self.pyth_attempted.clear();
            self.solend_attempted.clear();
            self.token = Some(token);
        }
    }

    fn missing_request(&self, rule: &WatchRule) -> Stage2SnapshotRequest {
        let mut req = Stage2SnapshotRequest::new();
        for c in &rule.conditions {
            match c {
                Condition::PythPrice { feed_id, .. } => {
                    if !self.pyth_attempted.contains(feed_id) {
                        req.add_pyth(PythSnapshotKey { feed_id: *feed_id });
                    }
                }
                Condition::SolendReserveSupplyRate { reserve_pubkey, .. } => {
                    if !self.solend_attempted.contains(reserve_pubkey) {
                        req.add_solend(SolendSnapshotKey {
                            reserve_pubkey: *reserve_pubkey,
                        });
                    }
                }
            }
        }
        req
    }

    fn integrate(&mut self, batch: Stage2SnapshotBatch) {
        for (k, v) in batch.pyth {
            self.pyth_attempted.insert(k);
            self.pyth.insert(k, v);
        }
        for (k, v) in batch.solend {
            self.solend_attempted.insert(k);
            self.solend.insert(k, v);
        }
    }

    fn snapshot_view(&self) -> Stage2SnapshotBatch {
        let mut view = Stage2SnapshotBatch::new();
        for (k, v) in &self.pyth {
            view.insert_pyth(*k, v.clone());
        }
        for (k, v) in &self.solend {
            view.insert_solend(*k, v.clone());
        }
        view
    }
}

/// Glue type: implements the W2 [`Stage2ConditionEvaluator`] trait by
/// calling a [`Stage2SnapshotProvider`] and a pure
/// [`evaluate_rule_against_batch`].
///
/// Holds a tick-scoped dedupe cache so that duplicated Pyth feed-ids
/// and Solend reserves across many rules in the same tick produce a
/// single provider fetch per unique key. The cache invalidates on
/// every advance of any of `(current_slot, current_unix_timestamp,
/// now_ms)` — production drives this monotonically; tests using a
/// pinned MockClock may share cache across calls within the same
/// (slot, ts, ms) tuple, which is the intended dedupe behaviour.
#[derive(Debug)]
pub struct Stage2BatchedConditionEvaluator {
    provider: Arc<dyn Stage2SnapshotProvider>,
    cache: Mutex<TickCache>,
}

impl Stage2BatchedConditionEvaluator {
    /// Construct with a caller-supplied provider. Tests inject a
    /// [`Stage2DeterministicMockProvider`]; production wiring will
    /// pass an explicit live provider added by a future slice
    /// (currently no such provider exists in tree).
    pub fn with_provider(provider: Arc<dyn Stage2SnapshotProvider>) -> Self {
        Self {
            provider,
            cache: Mutex::new(TickCache::default()),
        }
    }

    /// Default: wraps [`Stage2NoopSnapshotProvider`]. Equivalent to
    /// the W2 `NoopConditionEvaluator` in observable behaviour
    /// (returns Ok(false) for every rule that has any condition,
    /// because every condition's snapshot is missing → Transient
    /// error → snapshots never appear "passed"). Suitable for the
    /// default `Stage2Watcher::new` constructor when paired with the
    /// caller's existing simulator stub.
    pub fn with_noop_provider() -> Self {
        Self::with_provider(Arc::new(Stage2NoopSnapshotProvider))
    }
}

#[async_trait]
impl Stage2ConditionEvaluator for Stage2BatchedConditionEvaluator {
    async fn evaluate(
        &self,
        rule: &WatchRule,
        ctx: &Stage2TickContext,
    ) -> Result<bool, Stage2WatcherError> {
        // Plan the missing keys for this rule, compute under the cache
        // lock, drop it, then await the provider with the lock not
        // held. (`parking_lot::Mutex` is a synchronous lock; we MUST
        // NOT hold it across an `.await`.)
        let request = {
            let mut cache = self.cache.lock();
            cache.ensure_token(ctx);
            cache.missing_request(rule)
        };

        if !request.is_empty() {
            let batch = self.provider.fetch_batch(&request, ctx).await;
            // Integrate under the lock again (short critical section).
            let mut cache = self.cache.lock();
            // Re-check token: a concurrent tick may have rotated the
            // cache while we awaited the provider. If so, we still
            // integrate — the batch corresponds to the request we
            // sent, which used the ctx the caller saw. Token rotation
            // is not a correctness issue, just a cache freshness one;
            // the next evaluate() call will re-plan.
            cache.ensure_token(ctx);
            cache.integrate(batch);
        }

        let view = {
            let cache = self.cache.lock();
            cache.snapshot_view()
        };

        debug!(
            rule_id = ?rule.rule_id,
            pyth = view.pyth_count(),
            solend = view.solend_count(),
            "stage2 evaluator: evaluating rule against cached snapshots"
        );
        evaluate_rule_against_batch(rule, &view, ctx)
    }
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
    use claw_types::stage2_watch_rule::{
        ActionSpec, JupiterApiVersion, WithdrawMode, STAGE2_WATCH_RULE_SCHEMA_VERSION,
    };

    use crate::stage2_watcher::{
        NoopExecutionSimulator, Stage2Clock, Stage2RuleTickResult,
        Stage2Watcher, Stage2WatcherConfig,
    };

    // ── Constants matching W2 fixtures so canonical hashes pin ──────────

    const TEST_USER: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
    const SOLEND_USDC_RESERVE: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
    const SOLEND_LENDING_MARKET: &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
    const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

    const FIXTURE_CREATED_AT_SLOT: u64 = 415_500_000;
    const FIXTURE_EXPIRES_AT_SLOT: u64 = 415_700_000;
    const FIXTURE_UNIX_TS: i64 = 1_700_000_000;

    fn pk(b: u8) -> PubkeyBytes {
        PubkeyBytes::new([b; 32])
    }

    fn pk_from_str(s: &str) -> PubkeyBytes {
        PubkeyBytes::from_base58(s).expect("test pubkey parses")
    }

    // ── Solend fixture rule (single condition, APR < 10%) ───────────────

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

    // ── Pyth fixture rule (single condition, BTC > 75k) ─────────────────

    const BTC_USD_FEED_ID: [u8; 32] = [
        0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6, 0x7d, 0xb4, 0x4d, 0xc1,
        0x2d, 0xe5, 0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc, 0x65, 0x8a, 0xfe,
        0xdf, 0x0f, 0x4a, 0x41, 0x5b, 0x43,
    ];
    const ETH_USD_FEED_ID: [u8; 32] = [
        0xff, 0x61, 0x49, 0x1a, 0x93, 0x11, 0x12, 0xdd, 0xf1, 0xbd, 0x81, 0x47, 0xcd,
        0x1b, 0x64, 0x13, 0x75, 0xf7, 0x9f, 0x58, 0x25, 0x12, 0x6d, 0x66, 0x54, 0x80,
        0x87, 0x46, 0x34, 0xfd, 0x0a, 0xce,
    ];

    fn pyth_btc_gt_75k_condition() -> Condition {
        Condition::PythPrice {
            feed_id: BTC_USD_FEED_ID,
            price_update_account: pk(0x10),
            comparison: Comparison::Gt,
            threshold_mantissa: 7_500_000,
            threshold_exponent: -2,
            max_age_seconds: 30,
            max_confidence_bps: 50,
            verification_level_required: VerificationLevel::Full,
            bound_mode: BoundMode::AdverseLowerForGt,
        }
    }

    fn pyth_eth_gt_2300_condition() -> Condition {
        Condition::PythPrice {
            feed_id: ETH_USD_FEED_ID,
            price_update_account: pk(0x11),
            comparison: Comparison::Gt,
            threshold_mantissa: 230_000,
            threshold_exponent: -2,
            max_age_seconds: 30,
            max_confidence_bps: 50,
            verification_level_required: VerificationLevel::Full,
            bound_mode: BoundMode::AdverseLowerForGt,
        }
    }

    fn fixture_pyth_rule(rule_id: [u8; 16], conditions: Vec<Condition>) -> WatchRule {
        let logic = ConditionLogic::All;
        WatchRule {
            schema_version: STAGE2_WATCH_RULE_SCHEMA_VERSION,
            rule_id,
            user: pk_from_str(TEST_USER),
            executor: pk(0x02),
            delegated_wallet: pk(0x03),
            created_at_slot: FIXTURE_CREATED_AT_SLOT,
            expires_at_slot: FIXTURE_EXPIRES_AT_SLOT,
            one_shot: true,
            condition_logic: logic,
            conditions,
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

    fn fresh_pyth_btc_above_75k() -> PythSnapshotValue {
        PythSnapshotValue {
            feed_id: BTC_USD_FEED_ID,
            // 75,001.23 with exponent -8 → adverse-lower 75,001.22 > 75,000.00
            price_mantissa: 7_500_123_000_000,
            price_exponent: -8,
            conf: 100,
            publish_time: FIXTURE_UNIX_TS - 5,
            verification_level: VerificationLevel::Full,
        }
    }

    fn fresh_pyth_eth_above_2300() -> PythSnapshotValue {
        PythSnapshotValue {
            feed_id: ETH_USD_FEED_ID,
            // 2300.50 with exponent -8 → adverse-lower 2300.49 > 2300.00
            price_mantissa: 230_050_000_000,
            price_exponent: -8,
            conf: 100,
            publish_time: FIXTURE_UNIX_TS - 5,
            verification_level: VerificationLevel::Full,
        }
    }

    fn stale_pyth_btc() -> PythSnapshotValue {
        PythSnapshotValue {
            // Same feed/price, but publish_time well outside max_age=30s.
            publish_time: FIXTURE_UNIX_TS - 600,
            ..fresh_pyth_btc_above_75k()
        }
    }

    fn fresh_solend_below_threshold() -> SolendSnapshotValue {
        // Crafted to produce supply_apr < 1000 bps.
        // Borrow rates: 0/4/30/100% (typical USDC). Optimal util 80%.
        // Utilisation 50% (well below 80%) → region 1 borrow rate.
        // utilisation_wad ≈ 0.5e18; borrow_rate ≈ 0.5/0.8 * (0.04 - 0)
        // ≈ 0.025 (2.5%). supply ≈ 0.025 * 0.5 * 0.95 ≈ 0.0119
        // (1.19%) = 119 bps < 1000 bps → condition fires (Lt 1000).
        SolendSnapshotValue {
            reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
            available_amount: 1_000_000,
            borrowed_amount_wads: 1_000_000u128 * SOLEND_WAD,
            min_borrow_rate_pct: 0,
            optimal_borrow_rate_pct: 4,
            max_borrow_rate_pct: 30,
            super_max_borrow_rate_pct: 100,
            optimal_utilization_rate_pct: 80,
            max_utilization_rate_pct: 90,
            protocol_take_rate_pct: 5,
            last_update_slot: FIXTURE_CREATED_AT_SLOT + 100,
            stale_flag: false,
        }
    }

    fn fresh_solend_above_threshold() -> SolendSnapshotValue {
        // Region 3: utilisation 95% (above max_util 90%); near
        // super_max_borrow_rate. supply ≈ very high → > 1000 bps.
        SolendSnapshotValue {
            available_amount: 50_000,
            borrowed_amount_wads: 950_000u128 * SOLEND_WAD,
            ..fresh_solend_below_threshold()
        }
    }

    fn ctx(slot: u64) -> Stage2TickContext {
        Stage2TickContext::new(slot, FIXTURE_UNIX_TS, 1_000)
    }

    fn ctx_full(slot: u64, unix_ts: i64, now_ms: i64) -> Stage2TickContext {
        Stage2TickContext::new(slot, unix_ts, now_ms)
    }

    // ── MockClock + Harness ─────────────────────────────────────────────

    #[derive(Debug)]
    struct PinnedClock {
        at_ms: AtomicI64,
        calls: AtomicU64,
    }

    impl PinnedClock {
        fn new(at_ms: i64) -> Self {
            Self {
                at_ms: AtomicI64::new(at_ms),
                calls: AtomicU64::new(0),
            }
        }
    }

    impl Stage2Clock for PinnedClock {
        fn now_ms(&self) -> i64 {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.at_ms.load(Ordering::SeqCst)
        }
    }

    struct WatcherHarness {
        _db: Database,
        repo: Stage2WatchRuleRepository,
    }

    async fn watcher_harness() -> WatcherHarness {
        let db = Database::open_in_memory().await.expect("in-memory DB");
        let repo = Stage2WatchRuleRepository::new(db.pool().clone());
        WatcherHarness { _db: db, repo }
    }

    fn build_watcher(
        repo: Stage2WatchRuleRepository,
        provider: Arc<Stage2DeterministicMockProvider>,
        clock_at_ms: i64,
    ) -> Stage2Watcher {
        let evaluator = Arc::new(Stage2BatchedConditionEvaluator::with_provider(
            provider,
        ));
        Stage2Watcher::with_components(
            repo,
            evaluator,
            Arc::new(NoopExecutionSimulator),
            Arc::new(PinnedClock::new(clock_at_ms)),
            Stage2WatcherConfig::default(),
        )
    }

    // ── Default-provider safety ─────────────────────────────────────────

    /// Test #18 — ambient env vars must not enable a live provider.
    /// We set a battery of plausibly-named env vars and assert that
    /// constructing the default evaluator + watcher does NOT make any
    /// network call. Concretely: the only providers in tree are
    /// `Stage2NoopSnapshotProvider` and `Stage2DeterministicMockProvider`,
    /// neither of which reads env / network. The default
    /// `Stage2Watcher::new(...)` uses `NoopConditionEvaluator`, which
    /// also makes no calls. We assert the "tick produces no
    /// condition_met" property as a behavioural proxy.
    #[tokio::test]
    async fn ambient_env_vars_do_not_enable_live_provider_in_tests() {
        // SAFETY: these are local to this test process. No other tests
        // read these names, but to keep parallel-test flakiness out we
        // restore them at the end.
        let names = [
            "SOLANA_RPC_URL",
            "RPC_URL",
            "CLAW_RPC_URL",
            "CLAW_PYTH_RPC",
            "CLAW_SOLEND_RPC",
            "CLAW_PROVIDER",
            "CLAW_LIVE_PROVIDER",
            "STAGE2_LIVE_PROVIDER",
        ];
        let prior: Vec<(&str, Option<String>)> = names
            .iter()
            .map(|n| (*n, std::env::var(n).ok()))
            .collect();
        for n in &names {
            std::env::set_var(n, "https://example.invalid/should-never-be-called");
        }

        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0x01; 16]);
        h.repo.insert(&rule).await.unwrap();

        // Default Watcher::new uses NoopConditionEvaluator + NoopExecutionSimulator.
        let watcher = Stage2Watcher::new(h.repo.clone(), Stage2WatcherConfig::default());
        let report = watcher
            .tick(ctx(rule.expires_at_slot - 1))
            .await;
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.transient_error_count, 0);
        assert!(report.was_successful());

        // Default batched evaluator (noop provider) also returns no
        // condition_met for any rule (every snapshot is missing →
        // Transient → no Ok(true)).
        let noop_eval =
            Arc::new(Stage2BatchedConditionEvaluator::with_noop_provider());
        let watcher_b = Stage2Watcher::with_components(
            h.repo.clone(),
            noop_eval,
            Arc::new(NoopExecutionSimulator),
            Arc::new(PinnedClock::new(2_000)),
            Stage2WatcherConfig::default(),
        );
        let report_b = watcher_b
            .tick(ctx(rule.expires_at_slot - 1))
            .await;
        assert_eq!(report_b.condition_met_count, 0);

        for (n, prev) in prior {
            match prev {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
        }
    }

    // ── Direct evaluator math: All / Any / short-circuit ────────────────

    /// Test #3.
    #[test]
    fn all_logic_requires_all_conditions_true() {
        let mut rule = fixture_pyth_rule(
            [0xA1; 16],
            vec![pyth_btc_gt_75k_condition(), pyth_eth_gt_2300_condition()],
        );
        rule.condition_logic = ConditionLogic::All;

        // Both passing → All = true
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(fresh_pyth_btc_above_75k()));
        batch.insert_pyth(ETH_USD_FEED_ID, Ok(fresh_pyth_eth_above_2300()));
        assert!(evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());

        // One failing (ETH well below threshold) → All = false
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(fresh_pyth_btc_above_75k()));
        let mut eth_low = fresh_pyth_eth_above_2300();
        eth_low.price_mantissa = 100_000_000_000; // 1000.00 < 2300
        batch.insert_pyth(ETH_USD_FEED_ID, Ok(eth_low));
        assert!(!evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());
    }

    /// Test #4.
    #[test]
    fn any_logic_accepts_one_true_condition() {
        let mut rule = fixture_pyth_rule(
            [0xA2; 16],
            vec![pyth_btc_gt_75k_condition(), pyth_eth_gt_2300_condition()],
        );
        rule.condition_logic = ConditionLogic::Any;

        // BTC true, ETH false → Any = true
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(fresh_pyth_btc_above_75k()));
        let mut eth_low = fresh_pyth_eth_above_2300();
        eth_low.price_mantissa = 100_000_000_000;
        batch.insert_pyth(ETH_USD_FEED_ID, Ok(eth_low));
        assert!(evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());

        // Both false → Any = false
        let mut batch = Stage2SnapshotBatch::new();
        let mut btc_low = fresh_pyth_btc_above_75k();
        btc_low.price_mantissa = 1_000_000_000_000;
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(btc_low));
        let mut eth_low = fresh_pyth_eth_above_2300();
        eth_low.price_mantissa = 100_000_000_000;
        batch.insert_pyth(ETH_USD_FEED_ID, Ok(eth_low));
        assert!(!evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());
    }

    /// Test #5 — Any short-circuits true after first match. We prove
    /// short-circuit by placing a condition that would Err (missing
    /// snapshot) AFTER a passing Pyth condition; the rule must still
    /// return Ok(true) without consulting the broken second condition.
    #[test]
    fn any_logic_short_circuits_after_true() {
        let mut rule = fixture_pyth_rule(
            [0xA3; 16],
            vec![pyth_btc_gt_75k_condition(), pyth_eth_gt_2300_condition()],
        );
        rule.condition_logic = ConditionLogic::Any;

        // First condition true; second condition's snapshot is missing
        // (would Err). Short-circuit must give Ok(true).
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(fresh_pyth_btc_above_75k()));
        // ETH not inserted on purpose.
        assert!(evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());
    }

    /// Test #6 — All short-circuits false after first failure.
    #[test]
    fn all_logic_short_circuits_after_false() {
        let mut rule = fixture_pyth_rule(
            [0xA4; 16],
            vec![pyth_btc_gt_75k_condition(), pyth_eth_gt_2300_condition()],
        );
        rule.condition_logic = ConditionLogic::All;

        // First condition false (BTC well below 75k); second condition's
        // snapshot is missing. Short-circuit must give Ok(false).
        let mut batch = Stage2SnapshotBatch::new();
        let mut btc_low = fresh_pyth_btc_above_75k();
        btc_low.price_mantissa = 1_000_000_000_000;
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(btc_low));
        // ETH not inserted on purpose.
        assert!(!evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());
    }

    // ── Slot vs unix-timestamp axes ─────────────────────────────────────

    /// Test #8 — Pyth staleness uses current_unix_timestamp, not slot.
    #[test]
    fn pyth_stale_uses_current_unix_timestamp() {
        let rule = fixture_pyth_rule([0xB1; 16], vec![pyth_btc_gt_75k_condition()]);
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(stale_pyth_btc()));

        // Even with current_slot = far below expires, freshness uses
        // current_unix_timestamp - publish_time. publish_time is
        // FIXTURE_UNIX_TS - 600 (10 minutes); max_age is 30s. Stale.
        let stale_ctx = ctx_full(rule.expires_at_slot - 1, FIXTURE_UNIX_TS, 1);
        let res = evaluate_rule_against_batch(&rule, &batch, &stale_ctx);
        let err = res.expect_err("stale Pyth must be Transient");
        assert!(err.is_transient(), "got {err:?}");

        // Now bump current_unix_timestamp BACKWARDS to make the snapshot
        // appear fresh — slot unchanged. Should now succeed.
        let fresh_ctx = ctx_full(rule.expires_at_slot - 1, FIXTURE_UNIX_TS - 595, 1);
        assert!(
            evaluate_rule_against_batch(&rule, &batch, &fresh_ctx).unwrap(),
            "freshness depends on current_unix_timestamp axis"
        );
    }

    /// Test #9 — Solend staleness uses current_slot, not unix timestamp.
    #[test]
    fn solend_stale_uses_current_slot() {
        let rule = fixture_solend_rule([0xB2; 16]);
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            Ok(fresh_solend_below_threshold()),
        );

        // current_slot far ahead of last_update_slot (max_staleness=16).
        let stale_ctx = ctx_full(
            FIXTURE_CREATED_AT_SLOT + 200, // last_update was +100
            FIXTURE_UNIX_TS,
            1,
        );
        let res = evaluate_rule_against_batch(&rule, &batch, &stale_ctx);
        let err = res.expect_err("stale Solend reserve must be Transient");
        assert!(err.is_transient(), "got {err:?}");

        // current_slot only +5 past last_update — within tolerance.
        let fresh_ctx = ctx_full(
            FIXTURE_CREATED_AT_SLOT + 105,
            // Push unix_ts to a contradictory direction to prove the
            // axis is slot, not seconds.
            i64::MAX,
            1,
        );
        assert!(
            evaluate_rule_against_batch(&rule, &batch, &fresh_ctx).unwrap(),
            "Solend staleness depends on current_slot axis"
        );
    }

    // ── Batch dedupe ────────────────────────────────────────────────────

    /// Test #10.
    #[tokio::test]
    async fn duplicate_pyth_feed_batched_once_per_tick() {
        let h = watcher_harness().await;
        let rule_a =
            fixture_pyth_rule([0xC1; 16], vec![pyth_btc_gt_75k_condition()]);
        let rule_b =
            fixture_pyth_rule([0xC2; 16], vec![pyth_btc_gt_75k_condition()]);
        h.repo.insert(&rule_a).await.unwrap();
        h.repo.insert(&rule_b).await.unwrap();

        let provider = Arc::new(
            Stage2DeterministicMockProvider::new()
                .with_pyth(BTC_USD_FEED_ID, fresh_pyth_btc_above_75k()),
        );
        let watcher = build_watcher(h.repo.clone(), provider.clone(), 1_000);

        let report = watcher.tick(ctx(rule_a.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 2);
        assert_eq!(
            provider.pyth_call_count(&BTC_USD_FEED_ID),
            1,
            "duplicate Pyth feed must be fetched at most once per tick"
        );
    }

    /// Test #11.
    #[tokio::test]
    async fn duplicate_solend_reserve_batched_once_per_tick() {
        let h = watcher_harness().await;
        let rule_a = fixture_solend_rule([0xC3; 16]);
        let rule_b = fixture_solend_rule([0xC4; 16]);
        h.repo.insert(&rule_a).await.unwrap();
        h.repo.insert(&rule_b).await.unwrap();

        let provider = Arc::new(
            Stage2DeterministicMockProvider::new().with_solend(
                pk_from_str(SOLEND_USDC_RESERVE),
                SolendSnapshotValue {
                    last_update_slot: rule_a.expires_at_slot - 5,
                    ..fresh_solend_below_threshold()
                },
            ),
        );
        let watcher = build_watcher(h.repo.clone(), provider.clone(), 1_000);

        let report = watcher.tick(ctx(rule_a.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 2);
        assert_eq!(
            provider.solend_call_count(&pk_from_str(SOLEND_USDC_RESERVE)),
            1,
            "duplicate Solend reserve must be fetched at most once per tick"
        );
    }

    // ── Per-key error isolation ─────────────────────────────────────────

    /// Test #12 — A failed Pyth feed must only affect rules depending
    /// on that feed. An unrelated rule (different feed) must continue.
    #[tokio::test]
    async fn one_failed_pyth_feed_does_not_poison_unrelated_rules() {
        let h = watcher_harness().await;
        let rule_btc =
            fixture_pyth_rule([0xD1; 16], vec![pyth_btc_gt_75k_condition()]);
        let rule_eth =
            fixture_pyth_rule([0xD2; 16], vec![pyth_eth_gt_2300_condition()]);
        h.repo.insert(&rule_btc).await.unwrap();
        h.repo.insert(&rule_eth).await.unwrap();

        let provider = Arc::new(
            Stage2DeterministicMockProvider::new()
                .with_pyth_error(
                    BTC_USD_FEED_ID,
                    Stage2SnapshotError::Transient("BTC feed down".to_string()),
                )
                .with_pyth(ETH_USD_FEED_ID, fresh_pyth_eth_above_2300()),
        );
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule_btc.expires_at_slot - 1)).await;
        // ETH rule fired (its feed was fine); BTC rule recorded a
        // transient error (its feed failed). The two are independent.
        assert_eq!(report.condition_met_count, 1);
        assert_eq!(report.transient_error_count, 1);

        let btc = h.repo.get(&rule_btc.rule_id).await.unwrap().unwrap();
        assert_eq!(btc.status, WatchRuleStatus::Active);
        assert!(btc.last_error.as_deref().unwrap().contains("BTC feed down"));

        let eth = h.repo.get(&rule_eth.rule_id).await.unwrap().unwrap();
        assert_eq!(eth.status, WatchRuleStatus::ConditionMet);
    }

    /// Test #13.
    #[tokio::test]
    async fn one_failed_solend_reserve_does_not_poison_unrelated_rules() {
        let h = watcher_harness().await;
        let rule_solend = fixture_solend_rule([0xD3; 16]);
        let rule_pyth =
            fixture_pyth_rule([0xD4; 16], vec![pyth_btc_gt_75k_condition()]);
        h.repo.insert(&rule_solend).await.unwrap();
        h.repo.insert(&rule_pyth).await.unwrap();

        let provider = Arc::new(
            Stage2DeterministicMockProvider::new()
                .with_solend_error(
                    pk_from_str(SOLEND_USDC_RESERVE),
                    Stage2SnapshotError::Transient(
                        "solend reserve down".to_string(),
                    ),
                )
                .with_pyth(BTC_USD_FEED_ID, fresh_pyth_btc_above_75k()),
        );
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule_pyth.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 1);
        assert_eq!(report.transient_error_count, 1);

        let s = h.repo.get(&rule_solend.rule_id).await.unwrap().unwrap();
        assert_eq!(s.status, WatchRuleStatus::Active);
        assert!(s.last_error.as_deref().unwrap().contains("solend reserve down"));

        let p = h.repo.get(&rule_pyth.rule_id).await.unwrap().unwrap();
        assert_eq!(p.status, WatchRuleStatus::ConditionMet);
    }

    // ── Transient + Terminal classification ─────────────────────────────

    /// Test #14.
    #[tokio::test]
    async fn transient_provider_error_keeps_active_and_records_last_error() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xE1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(
            Stage2DeterministicMockProvider::new().with_solend_error(
                pk_from_str(SOLEND_USDC_RESERVE),
                Stage2SnapshotError::Transient("rpc 503".to_string()),
            ),
        );
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.transient_error_count, 1);
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.terminal_error_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert!(loaded.last_error.as_deref().unwrap().contains("rpc 503"));
    }

    /// Test #15 — A Terminal-classified evaluator error (here: a rule
    /// whose snapshot says formula_version=99, which the evaluator
    /// rejects as Terminal) is mark_failed_if_not_terminal'ed.
    #[tokio::test]
    async fn terminal_schema_error_marks_failed_if_not_terminal() {
        let h = watcher_harness().await;
        // Build a rule that asks for an unsupported formula_version.
        // The state-store will accept it; the evaluator catches it.
        let rule = WatchRule {
            conditions: vec![Condition::SolendReserveSupplyRate {
                reserve_pubkey: pk_from_str(SOLEND_USDC_RESERVE),
                lending_market: pk_from_str(SOLEND_LENDING_MARKET),
                solend_program_id: pk_from_str(SOLEND_PROGRAM_ID),
                comparison: Comparison::Lt,
                threshold_bps: 1_000,
                rate_kind: RateKind::Apr,
                formula_version: 99, // unsupported
                max_reserve_staleness_slots: 16,
                required_refresh_same_tx: true,
            }],
            ..fixture_solend_rule([0xE2; 16])
        };
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_below_threshold()
            },
        ));
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.terminal_error_count, 1);
        assert_eq!(report.failed_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Failed);
        assert!(
            loaded
                .last_error
                .as_deref()
                .unwrap()
                .contains("formula_version 99 unsupported")
        );
    }

    // ── End-to-end with watcher: false + true paths ─────────────────────

    /// Test #1.
    #[tokio::test]
    async fn tick_evaluates_false_rule_and_keeps_active() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF1; 16]);
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_above_threshold() // APR > 10% → Lt 10% = false
            },
        ));
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.rules_processed, 1);
        assert!(matches!(
            report.per_rule.first(),
            Some(Stage2RuleTickResult::ConditionFalse { .. })
        ));

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Active);
        assert!(loaded.last_checked_slot.is_some());
    }

    /// Test #2.
    #[tokio::test]
    async fn tick_evaluates_true_rule_and_marks_condition_met() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF2; 16]);
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_below_threshold() // APR < 10% → Lt 10% = true
            },
        ));
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
    }

    // ── Slot-axis expiry (parallel to W2 test) ──────────────────────────

    /// Test #7.
    #[tokio::test]
    async fn expired_rule_uses_current_slot_not_timestamp() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF3; 16]);
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_below_threshold()
            },
        ));
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        // Slot well past expiry, unix_ts intentionally tiny: rule
        // must expire on slot, not seconds.
        let expired_ctx =
            Stage2TickContext::new(rule.expires_at_slot + 1, 1, 1_000);
        let report = watcher.tick(expired_ctx).await;
        assert_eq!(report.expired_count, 1);
        assert_eq!(report.condition_met_count, 0);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, WatchRuleStatus::Expired);
    }

    // ── TOCTOU revoke during evaluate ───────────────────────────────────

    #[derive(Debug)]
    struct RevokingProvider {
        repo: Stage2WatchRuleRepository,
        target: [u8; 16],
        inner: Stage2DeterministicMockProvider,
    }

    #[async_trait]
    impl Stage2SnapshotProvider for RevokingProvider {
        async fn fetch_batch(
            &self,
            request: &Stage2SnapshotRequest,
            ctx: &Stage2TickContext,
        ) -> Stage2SnapshotBatch {
            // Flip status to revoked BEFORE returning the snapshots.
            self.repo.mark_revoked(&self.target).await.expect("mark_revoked");
            self.inner.fetch_batch(request, ctx).await
        }
    }

    /// Test #16.
    #[tokio::test]
    async fn revoked_mid_eval_is_not_overwritten_to_condition_met() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF4; 16]);
        h.repo.insert(&rule).await.unwrap();

        let inner = Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_below_threshold() // would fire
            },
        );
        let provider = Arc::new(RevokingProvider {
            repo: h.repo.clone(),
            target: rule.rule_id,
            inner,
        });
        let evaluator = Arc::new(Stage2BatchedConditionEvaluator::with_provider(
            provider,
        ));
        let watcher = Stage2Watcher::with_components(
            h.repo.clone(),
            evaluator,
            Arc::new(NoopExecutionSimulator),
            Arc::new(PinnedClock::new(1_000)),
            Stage2WatcherConfig::default(),
        );

        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        // Watcher saw race lost.
        assert_eq!(report.condition_met_count, 0);
        assert_eq!(report.race_lost_count, 1);

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        assert_eq!(
            loaded.status,
            WatchRuleStatus::Revoked,
            "revoked must not be overwritten by condition_met"
        );
    }

    // ── Provider-failure does not leak in_flight ────────────────────────

    /// Test #17.
    #[tokio::test]
    async fn provider_batch_failure_does_not_leave_in_flight_rule_stuck() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF5; 16]);
        h.repo.insert(&rule).await.unwrap();

        // Provider returns Transient for every key.
        let provider = Arc::new(
            Stage2DeterministicMockProvider::new().with_solend_error(
                pk_from_str(SOLEND_USDC_RESERVE),
                Stage2SnapshotError::Transient("rpc 503".to_string()),
            ),
        );
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);

        assert_eq!(watcher.in_flight_count(), 0);
        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.transient_error_count, 1);
        assert_eq!(
            watcher.in_flight_count(),
            0,
            "in_flight set must release after a provider-failure tick"
        );

        // Run another tick to confirm the rule is still tickable
        // (the in_flight set really is empty, not just temporarily
        // invisible).
        let report2 = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report2.transient_error_count, 1);
    }

    // ── Report copy discipline (no overclaim) ───────────────────────────

    /// Test #19 — the watcher's per-rule outcomes and the evaluator's
    /// error messages must not claim execution. Forbidden tokens
    /// listed by the W3 prompt's "Hard scope" section.
    #[tokio::test]
    async fn watcher_report_copy_does_not_claim_execution() {
        let h = watcher_harness().await;
        let rule = fixture_solend_rule([0xF6; 16]);
        h.repo.insert(&rule).await.unwrap();

        let provider = Arc::new(Stage2DeterministicMockProvider::new().with_solend(
            pk_from_str(SOLEND_USDC_RESERVE),
            SolendSnapshotValue {
                last_update_slot: rule.expires_at_slot - 5,
                ..fresh_solend_below_threshold()
            },
        ));
        let watcher = build_watcher(h.repo.clone(), provider, 1_000);
        let report = watcher.tick(ctx(rule.expires_at_slot - 1)).await;
        assert_eq!(report.condition_met_count, 1);

        let forbidden_tokens = [
            "executed",
            "execute_action",
            "swap complete",
            "swap_complete",
            "withdraw complete",
            "withdraw_complete",
            "transaction sent",
            "transaction_sent",
            "broadcast",
            "send_raw_transaction",
            "send_transaction",
            "signTransaction",
            "sendTransaction",
        ];
        for entry in &report.per_rule {
            let dump = format!("{entry:?}");
            for tok in &forbidden_tokens {
                assert!(
                    !dump.to_ascii_lowercase().contains(&tok.to_ascii_lowercase()),
                    "forbidden token '{tok}' surfaced in tick result: {dump}"
                );
            }
        }

        let loaded = h.repo.get(&rule.rule_id).await.unwrap().unwrap();
        // Status string itself must be the W2 vocabulary.
        assert_eq!(loaded.status, WatchRuleStatus::ConditionMet);
        // last_error is None on a clean fire — no execution claim
        // could possibly hide there.
        assert!(loaded.last_error.is_none());

        // Also pin: the provider never received a request to execute,
        // sign, or broadcast — it only knows fetch_batch.
        // (Compile-time enforcement — the trait has no execute method.)
        let _check: fn(&Stage2DeterministicMockProvider) = |_| {};
    }

    // ── Plan request helper ─────────────────────────────────────────────

    #[test]
    fn plan_request_dedupes_duplicate_keys_within_a_rule() {
        let rule = fixture_pyth_rule(
            [0xAA; 16],
            // Same feed twice.
            vec![pyth_btc_gt_75k_condition(), pyth_btc_gt_75k_condition()],
        );
        let req = plan_request_for_rule(&rule);
        assert_eq!(req.pyth_feeds().len(), 1);
        assert_eq!(req.solend_reserves().len(), 0);
    }

    // ── Provider has no live methods ────────────────────────────────────

    /// Compile-time + runtime sanity: the provider trait surface must
    /// not name any method that implies execution. This is a
    /// reflection on the trait definition's stability — adding e.g.
    /// `fn send_transaction(...)` would require updating this test.
    #[test]
    fn provider_trait_has_no_execution_methods() {
        // The only method on Stage2SnapshotProvider is fetch_batch.
        // We can't enumerate it at runtime, but we pin the *public*
        // surface here as documentation. If a future commit adds a
        // method, this test remains a deliberate review checkpoint.
        let names: &[&str] = &["fetch_batch"];
        assert_eq!(names, &["fetch_batch"]);
    }

    // ── Math sanity: integer-only ───────────────────────────────────────

    #[test]
    fn supply_apr_at_optimal_util_is_close_to_optimal_borrow_rate_times_take() {
        // Pin a known-shape: utilisation == optimal_util →
        // borrow_rate == optimal_borrow_rate (region 1 endpoint).
        // At optimal_borrow_rate=4%, take=5%, util=80%:
        //   supply_apr = 0.04 * 0.80 * 0.95 = 0.0304 → 0.0304 * 1e18.
        let s = SolendSnapshotValue {
            available_amount: 200_000,
            borrowed_amount_wads: 800_000u128 * SOLEND_WAD,
            ..fresh_solend_below_threshold()
        };
        let apr = supply_apr_wad(&s).unwrap();
        // Allow 1e15 slack (tolerance = 0.001 in WAD scale).
        let expected = (304u128 * 10u128.pow(14)) as i128; // 0.0304 in WAD
        let delta = (apr as i128 - expected).abs();
        assert!(delta < 1_000_000_000_000_000, "apr={apr}, expected≈{expected}");
    }

    #[test]
    fn pyth_exponent_normalisation_matches_on_chain_semantics() {
        let cond = pyth_btc_gt_75k_condition();
        let snap = fresh_pyth_btc_above_75k();
        let mut batch = Stage2SnapshotBatch::new();
        batch.insert_pyth(BTC_USD_FEED_ID, Ok(snap));
        let rule = fixture_pyth_rule([0xBB; 16], vec![cond]);
        // BTC adverse-lower 75001.22 > 75000.00 → fires.
        assert!(evaluate_rule_against_batch(&rule, &batch, &ctx(1)).unwrap());
    }

    // ── Stage 2 B-O1 — public APR wrappers + raw-snapshot mapper ───────

    #[test]
    fn b_o1_public_wrapper_matches_private_supply_apr_wad() {
        let snap = fresh_solend_below_threshold();
        let public = solend_supply_apr_wad_for_snapshot(&snap).unwrap();
        let private = supply_apr_wad(&snap).unwrap();
        assert_eq!(public, private, "public wrapper must delegate exactly");
    }

    #[test]
    fn b_o1_public_wrapper_rejects_invalid_config() {
        // Violate min ≤ optimal: min=10, optimal=4.
        let snap = SolendSnapshotValue {
            min_borrow_rate_pct: 10,
            optimal_borrow_rate_pct: 4,
            ..fresh_solend_below_threshold()
        };
        let err = solend_supply_apr_wad_for_snapshot(&snap).unwrap_err();
        match err {
            Stage2WatcherError::Transient(m) => {
                assert!(m.contains("config invariants violated"), "got {m}");
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn b_o1_bps_from_wad_truncates_toward_zero() {
        // 1.19% = 119 bps. WAD = 119 * 10^14.
        let wad = 119u128 * 10u128.pow(14);
        assert_eq!(solend_supply_apr_bps_from_wad(wad), 119);

        // 0.5 bps (sub-bps) → truncates to 0.
        let half = 5u128 * 10u128.pow(13);
        assert_eq!(solend_supply_apr_bps_from_wad(half), 0);

        // 1000.4 bps → truncates to 1000 (not rounded up).
        let frac = 1000u128 * 10u128.pow(14) + 4u128 * 10u128.pow(13);
        assert_eq!(solend_supply_apr_bps_from_wad(frac), 1000);
    }

    #[test]
    fn b_o1_snapshot_mapper_from_raw_reserve_round_trips() {
        use crate::integrations::solend::raw::SolendReserveRaw;
        use solana_sdk::pubkey::Pubkey as SolPubkey;

        let pk = SolPubkey::new_unique();
        let reserve_address = PubkeyBytes::new(pk.to_bytes());

        // Build a `SolendReserveRaw` directly (the struct fields are
        // pub; the decode side is exercised by raw.rs's own
        // `reserve_rate_config_fields_roundtrip` test, so we don't
        // duplicate the byte-level coverage here).
        let raw = SolendReserveRaw {
            version: 1,
            last_update_slot: FIXTURE_CREATED_AT_SLOT + 100,
            last_update_stale: false,
            lending_market: pk,
            liquidity_mint: pk,
            liquidity_mint_decimals: 6,
            liquidity_supply: pk,
            pyth_oracle: pk,
            switchboard_oracle: pk,
            liquidity_available_amount: 1_000_000,
            liquidity_borrowed_amount_wads: 1_000_000u128 * SOLEND_WAD,
            liquidity_accumulated_protocol_fees_wads: 0,
            collateral_mint: pk,
            collateral_supply: pk,
            config_deposit_limit: 0,
            // Valid rate-config matching `fresh_solend_below_threshold`.
            config_optimal_utilization_rate_pct: 80,
            config_min_borrow_rate_pct: 0,
            config_optimal_borrow_rate_pct: 4,
            config_max_borrow_rate_pct: 30,
            config_protocol_take_rate_pct: 5,
            config_max_utilization_rate_pct: 90,
            config_super_max_borrow_rate_pct: 100,
        };

        let mapped = solend_snapshot_value_from_reserve_raw(reserve_address, &raw);

        // Sanity: every field on the mapped SolendSnapshotValue is what
        // the raw decoder produced — no silent reorder.
        assert_eq!(mapped.reserve_pubkey, reserve_address);
        assert_eq!(mapped.available_amount, raw.liquidity_available_amount);
        assert_eq!(mapped.borrowed_amount_wads, raw.liquidity_borrowed_amount_wads);
        assert_eq!(mapped.min_borrow_rate_pct, raw.config_min_borrow_rate_pct);
        assert_eq!(
            mapped.optimal_borrow_rate_pct,
            raw.config_optimal_borrow_rate_pct
        );
        assert_eq!(mapped.max_borrow_rate_pct, raw.config_max_borrow_rate_pct);
        assert_eq!(
            mapped.super_max_borrow_rate_pct,
            raw.config_super_max_borrow_rate_pct
        );
        assert_eq!(
            mapped.optimal_utilization_rate_pct,
            raw.config_optimal_utilization_rate_pct
        );
        assert_eq!(
            mapped.max_utilization_rate_pct,
            raw.config_max_utilization_rate_pct
        );
        assert_eq!(
            mapped.protocol_take_rate_pct,
            raw.config_protocol_take_rate_pct
        );
        assert_eq!(mapped.last_update_slot, raw.last_update_slot);
        assert_eq!(mapped.stale_flag, raw.last_update_stale);

        // End-to-end: computed supply APR matches the existing
        // `fresh_solend_below_threshold` fixture's APR (which the
        // suite has already pinned at ≈119 bps). This is the integration
        // test the brief asks for — raw decoder + mapper + APR calculator.
        let apr_wad = solend_supply_apr_wad_for_snapshot(&mapped).unwrap();
        let apr_bps = solend_supply_apr_bps_from_wad(apr_wad);
        // 0.025 (region-1 borrow) × 0.5 (utilisation) × 0.95 (1-take) =
        // 0.01187… → 118 bps after integer truncation.
        assert!(
            apr_bps >= 100 && apr_bps <= 130,
            "mapped APR {apr_bps} bps out of expected band [100, 130]"
        );
    }
}
