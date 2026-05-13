//! Watcher — scans `budget_reserved` intents, re-fetches the
//! pre-set condition, claims the execution lease via CAS.
//!
//! Phase 3 of the Bounded Intent Execution Loop. See
//! [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) §7–§8 and
//! [`docs/SECURITY_BOUNDARIES.md`](../../../docs/SECURITY_BOUNDARIES.md) §B4.
//!
//! # Status
//!
//! Scaffold. Behaviour lands in Phase 3.
//!
//! # Boundary
//!
//! - The watcher is a single-process `tokio::time::interval` loop
//!   (30 s in scaffold). It is **not a production scheduler**.
//! - No `std::thread::sleep` anywhere — async safety only.
//! - Each tick is bounded: list eligible intents → filter by pinned
//!   demo shape → re-fetch condition → attempt CAS.
//! - A failed tick must warn-log and continue. One bad tick does
//!   not crash the daemon.
//!
//! # CAS gate
//!
//! `lease_execution_if_budget_reserved(rule_id, now)` is the single
//! atomic transition `budget_reserved → executing`. Both the
//! autonomous watcher and a future manual-approval path call the
//! same SQL `UPDATE`; they cannot double-execute a single funded
//! budget.
//!
//! # Out of scope
//!
//! - Distributed coordination, leader election.
//! - Durable job queue.
//! - Multi-tenant isolation.
//! - LLM-driven condition evaluation (conditions are deterministic
//!   thresholds against on-chain or off-chain reads).
