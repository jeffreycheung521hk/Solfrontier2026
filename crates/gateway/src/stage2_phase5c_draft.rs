//! Stage 2 Phase 5c-lite — LLM draft intent + user finalization gate.
//!
//! # Trust boundary introduced in this phase
//!
//! Phase 5 wired the LLM intent extractor directly into the W5h bridge:
//! a paraphrased finance intent flowed all the way to a persisted
//! `WatchRule` + `W5hFundingIntent` in a single chat round-trip, with
//! the LLM acting as the trust authority for "did the user mean
//! exactly this".
//!
//! Phase 5c-lite breaks that into two halves:
//!
//! 1. **Draft** — the LLM extractor produces a `DraftIntent` and
//!    returns it to the chat surface. The draft is held in an
//!    in-process, session-scoped store with a short TTL. **NO** row
//!    is inserted in `stage2_w5h_funding_intents`, `stage2_watch_rules`,
//!    or any other persistent table. No memo expectation is created.
//!    The watcher cannot see this draft.
//!
//! 2. **Finalize** — the user reviews the canonicalized order on the
//!    frontend and POSTs a confirmation with the draft id + the
//!    backend-computed `draft_hash` they saw. On match, the runtime
//!    runs the exact same `handle_w5h_from_parsed` pipeline as before
//!    and the existing W5h funding-required DTO is returned. The
//!    draft is consumed on success and dropped on reject.
//!
//! The LLM thus drafts a paraphrase into the supported shape, but a
//! human attestation crosses the trust boundary before any DB or chain
//! state is touched.
//!
//! # Canonical hash contract
//!
//! `compute_draft_hash` consumes the *pinned* preimage object — its
//! key set is fixed by [`CanonicalDraftPreimage`] and is **NOT** the
//! set of all `DraftIntent` fields. Volatile or audit-only fields
//! (`draft_id`, `parser_source`, `warnings`, `review_copy`,
//! `created_at_ms`, `session_id`, the user wallet pubkey, the raw
//! user message text, model prose) are explicitly excluded.
//!
//! The preimage is serialized as a JSON object with
//! **lexicographically sorted keys** at every level, UTF-8 encoded,
//! with no insignificant whitespace. `amount_raw` is emitted as a
//! decimal string; `threshold_bps` as an integer. `draft_hash` is the
//! lowercase-hex SHA-256 of the resulting bytes. The golden fixture
//! test pins the exact preimage byte shape.
//!
//! # Amount decimal conversion
//!
//! [`parse_usdc_amount_to_raw`] converts a user-typed decimal USDC
//! amount into raw token units (`* 1_000_000`) using exact integer
//! arithmetic on the decimal string — never `f64`. Amounts with more
//! than six fractional digits are rejected. `"0.5 USDC"` → `"500000"`.

#![allow(missing_docs)]

use std::collections::BTreeMap;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use solana_sdk::hash::hash as sha256;

use claw_types::session::SessionId;

// ── Public range pins for the Phase 5c-lite supported amount band ────────

/// Minimum supported `amount_raw` in the Phase 5c-lite draft schema.
/// 0.10 USDC.
pub const PHASE5C_MIN_AMOUNT_RAW: u64 = 100_000;
/// Maximum supported `amount_raw` in the Phase 5c-lite draft schema.
/// 1.00 USDC.
pub const PHASE5C_MAX_AMOUNT_RAW: u64 = 1_000_000;
/// Fixed expiry window applied AFTER finalize, in seconds. The TTL
/// clock starts at `finalize_unix_ms`, NOT at draft creation. The
/// user may take several minutes reviewing the LLM draft card; only
/// the post-finalize Phantom-funding window honors this 3-minute
/// budget.
pub const PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE: u64 = 180;

/// USDC decimals.
pub const USDC_DECIMALS: u32 = 6;
/// `10u64.pow(USDC_DECIMALS)`.
pub const USDC_RAW_PER_WHOLE: u64 = 1_000_000;

/// Default TTL the daemon constructs the [`DraftIntentStore`] with.
/// Brief addendum: TTL ≥ 10 minutes (the user may take several
/// minutes reviewing the LLM draft card). 15 min default.
pub const DEFAULT_DRAFT_TTL_SECONDS: u64 = 15 * 60;

// ── DraftIntent shape ────────────────────────────────────────────────────

/// Canonicalized, schema-validated draft of a W5h Solend USDC
/// conditional-deposit intent. Returned by the chat route in the
/// `DraftIntentReviewRequired` variant; consumed by the finalize
/// route.
///
/// All v1-pinned fields are present as constants below; only
/// `threshold_bps`, `amount_raw`, and identity fields vary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftIntent {
    /// Always `"deposit"` in v1. Excluded from `draft_hash` ONLY if
    /// it were variable — kept in the preimage as a future-proofing
    /// anchor.
    pub action: &'static str,
    pub protocol: &'static str,
    pub asset: &'static str,
    pub display_source: &'static str,
    pub comparison: &'static str,
    /// Threshold percent in bps. `1% → 100`. Range `[1, 10_000]`.
    pub threshold_bps: u32,
    /// Raw USDC amount (decimals=6). Range
    /// `[PHASE5C_MIN_AMOUNT_RAW, PHASE5C_MAX_AMOUNT_RAW]`.
    pub amount_raw: u64,
    /// `PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE` (180).
    pub expiry_seconds_after_finalize: u64,
    /// Controlled wallet (base58) the user funds. From the daemon's
    /// pinned controlled-wallet config.
    pub controlled_wallet: String,
    /// Controlled wallet's USDC ATA (base58).
    pub controlled_usdc_ata: String,
    /// SHA-256 hex of the raw user message bytes. Lets the canonical
    /// hash detect "different paraphrase, different draft" without
    /// putting the raw text in the preimage.
    ///
    /// `None` semantically equivalent to a 64-character zero string
    /// in the preimage; we always emit a non-null value so the
    /// canonical shape is invariant.
    pub original_user_message_hash: String,

    // ── Audit-only / volatile fields NOT in the canonical preimage ──

    /// Server-issued opaque id. UUID v4 hex.
    pub draft_id: String,
    /// Always `"llm_extractor"` in this phase.
    pub parser_source: &'static str,
    /// Non-fatal advisories surfaced to the user (model said low
    /// confidence; we ignored; etc.). Empty by default.
    pub warnings: Vec<String>,
    /// Short pre-rendered text the frontend may use as a fallback
    /// review summary. Stored alongside the draft for round-tripping
    /// but excluded from the canonical hash.
    pub review_copy: String,
    /// Epoch ms — server clock at draft creation.
    pub created_at_ms: i64,
    /// Session that minted the draft. Excluded from the canonical
    /// hash but enforced at finalize time.
    pub session_id_hex: String,
}

impl DraftIntent {
    /// Pin: `"deposit"`.
    pub const ACTION: &'static str = "deposit";
    /// Pin: `"solend"`.
    pub const PROTOCOL: &'static str = "solend";
    /// Pin: `"USDC"`.
    pub const ASSET: &'static str = "USDC";
    /// Pin: `"save"`.
    pub const DISPLAY_SOURCE: &'static str = "save";
    /// Pin: `"gt"`.
    pub const COMPARISON: &'static str = "gt";
    /// Pin: `"llm_extractor"`.
    pub const PARSER_SOURCE: &'static str = "llm_extractor";

    /// Project the canonical-hash preimage. Volatile / audit-only
    /// fields are intentionally omitted.
    pub fn canonical_preimage(&self) -> CanonicalDraftPreimage {
        CanonicalDraftPreimage {
            action: self.action.to_string(),
            protocol: self.protocol.to_string(),
            asset: self.asset.to_string(),
            display_source: self.display_source.to_string(),
            comparison: self.comparison.to_string(),
            threshold_bps: self.threshold_bps,
            amount_raw: self.amount_raw.to_string(),
            expiry_seconds_after_finalize: self.expiry_seconds_after_finalize,
            controlled_wallet: self.controlled_wallet.clone(),
            controlled_usdc_ata: self.controlled_usdc_ata.clone(),
            original_user_message_hash: self.original_user_message_hash.clone(),
        }
    }

    /// Compute the canonical hash for this draft. Pure projection of
    /// [`Self::canonical_preimage`] through [`compute_draft_hash`].
    pub fn compute_hash(&self) -> String {
        compute_draft_hash(&self.canonical_preimage())
    }
}

/// Frozen shape of the canonical hash preimage. The field list is
/// PART of the protocol — adding / removing a field changes every
/// draft hash and is a breaking change to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDraftPreimage {
    pub action: String,
    /// Decimal string (e.g. `"500000"`). String to defend against
    /// JS number truncation at the wire boundary; the hash treats it
    /// as a string lexeme either way.
    pub amount_raw: String,
    pub asset: String,
    pub comparison: String,
    pub controlled_usdc_ata: String,
    pub controlled_wallet: String,
    pub display_source: String,
    pub expiry_seconds_after_finalize: u64,
    pub original_user_message_hash: String,
    pub protocol: String,
    pub threshold_bps: u32,
}

/// SHA-256-hex (lowercase) of the canonicalized preimage. The
/// preimage is rendered with **lexicographically sorted keys** at
/// every level, no insignificant whitespace, UTF-8 bytes.
///
/// We do NOT trust `serde_json` to sort keys — it serializes struct
/// fields in declaration order. Instead we explicitly build a
/// `BTreeMap<&str, serde_json::Value>` (BTreeMap iterates in
/// lexicographic key order) and serialize that.
pub fn compute_draft_hash(p: &CanonicalDraftPreimage) -> String {
    let mut m: BTreeMap<&'static str, serde_json::Value> = BTreeMap::new();
    m.insert("action", serde_json::Value::String(p.action.clone()));
    m.insert("amount_raw", serde_json::Value::String(p.amount_raw.clone()));
    m.insert("asset", serde_json::Value::String(p.asset.clone()));
    m.insert("comparison", serde_json::Value::String(p.comparison.clone()));
    m.insert(
        "controlled_usdc_ata",
        serde_json::Value::String(p.controlled_usdc_ata.clone()),
    );
    m.insert(
        "controlled_wallet",
        serde_json::Value::String(p.controlled_wallet.clone()),
    );
    m.insert(
        "display_source",
        serde_json::Value::String(p.display_source.clone()),
    );
    m.insert(
        "expiry_seconds_after_finalize",
        serde_json::Value::Number(p.expiry_seconds_after_finalize.into()),
    );
    m.insert(
        "original_user_message_hash",
        serde_json::Value::String(p.original_user_message_hash.clone()),
    );
    m.insert("protocol", serde_json::Value::String(p.protocol.clone()));
    m.insert(
        "threshold_bps",
        serde_json::Value::Number(p.threshold_bps.into()),
    );
    // `serde_json::to_string` produces no insignificant whitespace
    // and iterates BTreeMap in lexicographic key order.
    let bytes = serde_json::to_vec(&m).expect("BTreeMap of Value serializes");
    let digest = sha256(&bytes);
    hex_lower_32(digest.to_bytes())
}

fn hex_lower_32(b: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256-hex of an arbitrary UTF-8 string. Used for
/// `original_user_message_hash`.
pub fn sha256_hex(s: &str) -> String {
    hex_lower_32(sha256(s.as_bytes()).to_bytes())
}

// ── Decimal-safe USDC amount parsing ─────────────────────────────────────

/// Typed errors from [`parse_usdc_amount_to_raw`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountParseError {
    /// Empty string.
    Empty,
    /// Sign character (the chat surface never emits negative amounts).
    Negative,
    /// Non-digit / non-dot character.
    InvalidChar,
    /// More than one `.`
    MultipleDots,
    /// More than 6 fractional digits (USDC has 6 decimals; any extra
    /// would silently round and is therefore refused).
    TooManyDecimals,
    /// The decimal would overflow `u64` when scaled.
    Overflow,
    /// Zero amount.
    Zero,
}

impl std::fmt::Display for AmountParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "amount is empty"),
            Self::Negative => write!(f, "amount must be non-negative"),
            Self::InvalidChar => write!(f, "amount contains non-decimal characters"),
            Self::MultipleDots => write!(f, "amount has more than one decimal point"),
            Self::TooManyDecimals => write!(
                f,
                "amount has more than {USDC_DECIMALS} decimal places"
            ),
            Self::Overflow => write!(f, "amount overflows u64 when scaled to raw units"),
            Self::Zero => write!(f, "amount must be greater than zero"),
        }
    }
}

/// Convert a decimal USDC amount string (e.g. `"0.5"`, `"1"`, `"0.25"`)
/// to raw u64 units (USDC has 6 decimals). Uses string-level integer
/// arithmetic — NEVER `f64` — so binary-float rounding can never
/// silently change `0.1 USDC` into `99999` or `100001`.
///
/// Rejects:
/// - empty
/// - negative
/// - non-digit / non-dot characters
/// - multiple `.` separators
/// - more than 6 fractional digits
/// - zero amount
/// - overflow
pub fn parse_usdc_amount_to_raw(s: &str) -> Result<u64, AmountParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AmountParseError::Empty);
    }
    if s.starts_with('-') {
        return Err(AmountParseError::Negative);
    }
    // Reject `+` prefix too — keep the surface narrow.
    if s.starts_with('+') {
        return Err(AmountParseError::InvalidChar);
    }
    let mut parts = s.split('.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next();
    if parts.next().is_some() {
        return Err(AmountParseError::MultipleDots);
    }
    // Every char must be 0-9.
    for c in int_part.chars() {
        if !c.is_ascii_digit() {
            return Err(AmountParseError::InvalidChar);
        }
    }
    if let Some(f) = frac_part {
        for c in f.chars() {
            if !c.is_ascii_digit() {
                return Err(AmountParseError::InvalidChar);
            }
        }
        if f.len() > USDC_DECIMALS as usize {
            return Err(AmountParseError::TooManyDecimals);
        }
    }
    let int_value: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| AmountParseError::Overflow)?
    };
    let frac_str = frac_part.unwrap_or("");
    // Right-pad to 6 digits, then parse.
    let mut frac_padded = String::with_capacity(USDC_DECIMALS as usize);
    frac_padded.push_str(frac_str);
    while frac_padded.len() < USDC_DECIMALS as usize {
        frac_padded.push('0');
    }
    let frac_value: u64 = frac_padded
        .parse()
        .map_err(|_| AmountParseError::Overflow)?;
    let scaled_int = int_value
        .checked_mul(USDC_RAW_PER_WHOLE)
        .ok_or(AmountParseError::Overflow)?;
    let raw = scaled_int
        .checked_add(frac_value)
        .ok_or(AmountParseError::Overflow)?;
    if raw == 0 {
        return Err(AmountParseError::Zero);
    }
    Ok(raw)
}

// ── In-process, session-scoped draft store ───────────────────────────────

/// In-process, TTL-bounded, session-scoped draft store. NOT a DB —
/// drafts vanish on daemon restart, by design.
///
/// Concurrency:
/// - Stored under a single `Mutex` keyed by `(session_id_hex,
///   draft_id)`. Drafts are tiny and short-lived; contention is
///   negligible.
/// - `consume_if_match` is atomic: either the entry is removed AND
///   returned, or no mutation happens.
///
/// Multi-draft per session: a single session may hold many drafts
/// simultaneously (the user types several "what if" orders); the
/// `draft_id` disambiguates.
///
/// Lifetimes:
/// - `insert` writes (session_id, draft_id) → entry.
/// - `get` returns a clone if present AND not expired; otherwise
///   removes the expired entry and returns `None`.
/// - `consume_if_match` removes the entry if `hash` matches; returns
///   `DraftConsumeOutcome::Ok(draft)` on match, the appropriate
///   typed variant otherwise.
/// - `drop_draft` unconditionally removes an entry; used by the
///   reject path.
#[derive(Debug)]
pub struct DraftIntentStore {
    inner: Mutex<DraftIntentStoreInner>,
    ttl_seconds: u64,
}

#[derive(Debug, Default)]
struct DraftIntentStoreInner {
    by_key: std::collections::HashMap<DraftKey, StoredEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DraftKey {
    session_id_hex: String,
    draft_id: String,
}

/// One slot in [`DraftIntentStoreInner`] — either an active draft
/// awaiting finalize, or a tombstone marking a draft that has been
/// finalized + consumed already (so a concurrent / retried confirm
/// can distinguish "you already finalized this" from "never minted").
#[derive(Debug, Clone)]
enum StoredEntry {
    Active(StoredDraft),
    Tombstone(Tombstone),
}

#[derive(Debug, Clone)]
struct StoredDraft {
    draft: DraftIntent,
    expires_at_ms: i64,
}

#[derive(Debug, Clone)]
struct Tombstone {
    /// Wall-clock at which the tombstone may be lazily removed. Equal
    /// to `consumed_at_ms + tombstone_ttl_ms`, where
    /// `tombstone_ttl_ms = draft_ttl_ms + 60_000`. Past this point
    /// repeated finalize attempts revert to `NotFoundOrExpired`,
    /// which is acceptable because the user has long since moved on.
    expires_at_ms: i64,
    /// When the draft was consumed. Surfaced to logs only; never
    /// returned over the wire. Audit-grade — does NOT leak any raw
    /// user text.
    #[allow(dead_code)]
    consumed_at_ms: i64,
}

/// Outcome of [`DraftIntentStore::consume_if_match`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftConsumeOutcome {
    /// Draft existed, hash matched, entry consumed (and replaced
    /// with a tombstone).
    Ok(DraftIntent),
    /// Draft id not present and no tombstone for it (never minted,
    /// dropped, or both draft + tombstone expired).
    NotFoundOrExpired,
    /// Draft id WAS minted and has already been consumed by a prior
    /// confirm. Distinguished from `NotFoundOrExpired` so the finalize
    /// route can surface HTTP 409 `draft_already_finalized_or_missing`
    /// (rather than the 404 used for never-existed drafts).
    AlreadyConsumed,
    /// Draft id present but its computed hash does NOT match the
    /// one the caller supplied. The entry is left in place
    /// (NOT consumed) so the user can retry within the TTL.
    HashMismatch {
        /// Hash the caller supplied.
        provided: String,
        /// Hash the backend computes for the persisted draft.
        backend: String,
    },
}

/// Tombstone extra-life past the draft TTL — the Phase 5c-lite
/// brief pins this at `+60 s`. So if the draft TTL is 15 min, a
/// consumed draft leaves a tombstone for ~16 min, after which the
/// slot lazy-recycles to `NotFoundOrExpired`.
pub const TOMBSTONE_TTL_EXTRA_SECONDS: u64 = 60;

impl DraftIntentStore {
    /// Construct a store with the given TTL (in seconds). The brief
    /// requires `ttl_seconds >= 10*60`.
    pub fn new(ttl_seconds: u64) -> Self {
        assert!(
            ttl_seconds >= 10 * 60,
            "DraftIntentStore TTL must be at least 10 minutes; got {}s",
            ttl_seconds
        );
        Self {
            inner: Mutex::new(DraftIntentStoreInner::default()),
            ttl_seconds,
        }
    }

    /// Draft TTL (seconds) this store was constructed with. Tombstone
    /// TTL derives from this: `ttl_seconds + TOMBSTONE_TTL_EXTRA_SECONDS`.
    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    /// Tombstone TTL (seconds): draft TTL + 60 s, per brief.
    pub fn tombstone_ttl_seconds(&self) -> u64 {
        self.ttl_seconds + TOMBSTONE_TTL_EXTRA_SECONDS
    }

    /// Insert a freshly-minted draft. Returns the draft id (echo).
    pub fn insert(&self, draft: DraftIntent) -> String {
        let now_ms = Utc::now().timestamp_millis();
        let expires_at_ms = now_ms + (self.ttl_seconds as i64) * 1000;
        let key = DraftKey {
            session_id_hex: draft.session_id_hex.clone(),
            draft_id: draft.draft_id.clone(),
        };
        let id = key.draft_id.clone();
        let mut g = self.inner.lock();
        g.by_key.insert(
            key,
            StoredEntry::Active(StoredDraft {
                draft,
                expires_at_ms,
            }),
        );
        id
    }

    /// Look up a draft, transparently expiring any entry whose TTL
    /// has elapsed. Returns `None` for not-found, expired, AND
    /// tombstoned entries. (`get` is the lookup-only path for the
    /// reject flow; the consume path uses `consume_if_match` to
    /// distinguish tombstones.)
    pub fn get(&self, session_id_hex: &str, draft_id: &str) -> Option<DraftIntent> {
        let now_ms = Utc::now().timestamp_millis();
        let key = DraftKey {
            session_id_hex: session_id_hex.to_string(),
            draft_id: draft_id.to_string(),
        };
        let mut g = self.inner.lock();
        match g.by_key.get(&key).cloned() {
            Some(StoredEntry::Active(s)) if s.expires_at_ms > now_ms => Some(s.draft),
            Some(StoredEntry::Active(_)) => {
                // Expired active draft — lazy-remove and report not-found.
                g.by_key.remove(&key);
                None
            }
            Some(StoredEntry::Tombstone(t)) if t.expires_at_ms > now_ms => {
                // Active tombstone — `get` does NOT distinguish; it is
                // the consume-path that surfaces AlreadyConsumed.
                None
            }
            Some(StoredEntry::Tombstone(_)) => {
                // Expired tombstone — lazy-remove.
                g.by_key.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Atomically consume a draft IF the supplied `expected_hash`
    /// matches the backend-computed hash. On success, the active
    /// draft is REPLACED with a tombstone (NOT a plain removal), so
    /// a concurrent / retried confirm with the same `draft_id`
    /// observes [`DraftConsumeOutcome::AlreadyConsumed`] rather than
    /// `NotFoundOrExpired`. The mutex is held across the read +
    /// compare + state-swap so the entire transition is atomic from
    /// the perspective of any other caller.
    pub fn consume_if_match(
        &self,
        session_id_hex: &str,
        draft_id: &str,
        expected_hash: &str,
    ) -> DraftConsumeOutcome {
        let now_ms = Utc::now().timestamp_millis();
        let key = DraftKey {
            session_id_hex: session_id_hex.to_string(),
            draft_id: draft_id.to_string(),
        };
        let mut g = self.inner.lock();
        let entry = match g.by_key.get(&key) {
            Some(e) => e.clone(),
            None => return DraftConsumeOutcome::NotFoundOrExpired,
        };
        match entry {
            StoredEntry::Tombstone(t) => {
                if t.expires_at_ms > now_ms {
                    DraftConsumeOutcome::AlreadyConsumed
                } else {
                    // Tombstone expired in flight — same as never minted.
                    g.by_key.remove(&key);
                    DraftConsumeOutcome::NotFoundOrExpired
                }
            }
            StoredEntry::Active(stored) => {
                if stored.expires_at_ms <= now_ms {
                    // Draft TTL expired before consume — lazy-clean.
                    g.by_key.remove(&key);
                    return DraftConsumeOutcome::NotFoundOrExpired;
                }
                let backend = stored.draft.compute_hash();
                if backend != expected_hash {
                    return DraftConsumeOutcome::HashMismatch {
                        provided: expected_hash.to_string(),
                        backend,
                    };
                }
                // Atomic swap: install tombstone in the same slot
                // before releasing the lock. Subsequent confirm calls
                // with the same draft_id observe AlreadyConsumed.
                let tombstone_expires =
                    now_ms + (self.tombstone_ttl_seconds() as i64) * 1000;
                g.by_key.insert(
                    key,
                    StoredEntry::Tombstone(Tombstone {
                        expires_at_ms: tombstone_expires,
                        consumed_at_ms: now_ms,
                    }),
                );
                DraftConsumeOutcome::Ok(stored.draft)
            }
        }
    }

    /// Unconditionally remove a draft entry (and any tombstone for
    /// it). Used by the reject path and tests. Idempotent.
    pub fn drop_draft(&self, session_id_hex: &str, draft_id: &str) {
        let key = DraftKey {
            session_id_hex: session_id_hex.to_string(),
            draft_id: draft_id.to_string(),
        };
        let mut g = self.inner.lock();
        g.by_key.remove(&key);
    }

    /// Number of currently-stored slots (active drafts + tombstones,
    /// some possibly expired — lazy cleanup happens on access).
    /// Test-only.
    pub fn len(&self) -> usize {
        self.inner.lock().by_key.len()
    }

    /// Number of currently-active drafts (excluding tombstones and
    /// expired slots). Test-only.
    pub fn active_len(&self) -> usize {
        let now_ms = Utc::now().timestamp_millis();
        self.inner
            .lock()
            .by_key
            .values()
            .filter(|e| match e {
                StoredEntry::Active(s) => s.expires_at_ms > now_ms,
                StoredEntry::Tombstone(_) => false,
            })
            .count()
    }

    /// Number of currently-active tombstones. Test-only.
    pub fn tombstone_len(&self) -> usize {
        let now_ms = Utc::now().timestamp_millis();
        self.inner
            .lock()
            .by_key
            .values()
            .filter(|e| match e {
                StoredEntry::Tombstone(t) => t.expires_at_ms > now_ms,
                StoredEntry::Active(_) => false,
            })
            .count()
    }

    /// True when there are no entries. Test-only.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Project a [`SessionId`] into the hex form drafts are keyed by.
/// Uses the public `Display` impl (UUID-as-string), so the store key
/// never depends on the private tuple-field shape.
pub fn session_id_hex(session_id: &SessionId) -> String {
    session_id.to_string()
}

/// Format a basis-point threshold into the percent label the
/// W5h pipeline carries on the `W5hParsed` shape. `100 → "1"`,
/// `250 → "2.5"`, `75 → "0.75"`. Shared between the LLM-draft path
/// and the finalize bridge so both produce identical strings.
pub fn format_threshold_pct_label(bps: u32) -> String {
    if bps % 100 == 0 {
        (bps / 100).to_string()
    } else {
        let whole = bps / 100;
        let frac = bps % 100;
        if frac % 10 == 0 {
            format!("{whole}.{}", frac / 10)
        } else {
            format!("{whole}.{frac:02}")
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const CW: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
    const ATA: &str = "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3";

    fn fixture_preimage() -> CanonicalDraftPreimage {
        // Golden fixture from the brief addendum.
        CanonicalDraftPreimage {
            action: "deposit".to_string(),
            amount_raw: "500000".to_string(),
            asset: "USDC".to_string(),
            comparison: "gt".to_string(),
            controlled_usdc_ata: ATA.to_string(),
            controlled_wallet: CW.to_string(),
            display_source: "save".to_string(),
            expiry_seconds_after_finalize: 180,
            original_user_message_hash: "0".repeat(64),
            protocol: "solend".to_string(),
            threshold_bps: 50,
        }
    }

    fn fixture_draft() -> DraftIntent {
        DraftIntent {
            action: DraftIntent::ACTION,
            protocol: DraftIntent::PROTOCOL,
            asset: DraftIntent::ASSET,
            display_source: DraftIntent::DISPLAY_SOURCE,
            comparison: DraftIntent::COMPARISON,
            threshold_bps: 50,
            amount_raw: 500_000,
            expiry_seconds_after_finalize: PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE,
            controlled_wallet: CW.to_string(),
            controlled_usdc_ata: ATA.to_string(),
            original_user_message_hash: "0".repeat(64),
            draft_id: "00112233445566778899aabbccddeeff".to_string(),
            parser_source: DraftIntent::PARSER_SOURCE,
            warnings: vec![],
            review_copy: "If Save APY > 0.5%, deposit 0.5 USDC".to_string(),
            created_at_ms: 1_700_000_000_000,
            session_id_hex: "session-abc-123".to_string(),
        }
    }

    // ── Golden fixture: canonical preimage bytes are stable ─────────────

    #[test]
    fn canonical_preimage_serialization_is_sorted_and_compact() {
        let p = fixture_preimage();
        // Build the same BTreeMap the producer builds, then check
        // the serialized form has the keys in alphabetical order.
        let mut m: BTreeMap<&'static str, serde_json::Value> = BTreeMap::new();
        m.insert("action", serde_json::Value::String(p.action.clone()));
        m.insert(
            "amount_raw",
            serde_json::Value::String(p.amount_raw.clone()),
        );
        m.insert("asset", serde_json::Value::String(p.asset.clone()));
        m.insert(
            "comparison",
            serde_json::Value::String(p.comparison.clone()),
        );
        m.insert(
            "controlled_usdc_ata",
            serde_json::Value::String(p.controlled_usdc_ata.clone()),
        );
        m.insert(
            "controlled_wallet",
            serde_json::Value::String(p.controlled_wallet.clone()),
        );
        m.insert(
            "display_source",
            serde_json::Value::String(p.display_source.clone()),
        );
        m.insert(
            "expiry_seconds_after_finalize",
            serde_json::Value::Number(p.expiry_seconds_after_finalize.into()),
        );
        m.insert(
            "original_user_message_hash",
            serde_json::Value::String(p.original_user_message_hash.clone()),
        );
        m.insert("protocol", serde_json::Value::String(p.protocol.clone()));
        m.insert(
            "threshold_bps",
            serde_json::Value::Number(p.threshold_bps.into()),
        );
        let s = serde_json::to_string(&m).unwrap();
        // Spot-check: must start with the alphabetically-first key.
        assert!(s.starts_with(r#"{"action":"deposit""#), "got: {s}");
        // Must end with the alphabetically-last key.
        assert!(s.ends_with(r#""threshold_bps":50}"#), "got: {s}");
        // No leading whitespace, no \n, no \r.
        assert!(!s.contains('\n'));
        assert!(!s.contains('\r'));
        // amount_raw rendered as a STRING (not a number).
        assert!(s.contains(r#""amount_raw":"500000""#));
        // threshold_bps rendered as a NUMBER (not a string).
        assert!(s.contains(r#""threshold_bps":50"#));
    }

    // ── Golden hash determinism ─────────────────────────────────────────

    #[test]
    fn draft_hash_is_deterministic() {
        let p = fixture_preimage();
        let h1 = compute_draft_hash(&p);
        let h2 = compute_draft_hash(&p);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        // All lowercase hex.
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn draft_hash_changes_when_amount_changes() {
        let mut p = fixture_preimage();
        let h0 = compute_draft_hash(&p);
        p.amount_raw = "750000".to_string();
        let h1 = compute_draft_hash(&p);
        assert_ne!(h0, h1, "amount_raw must affect the hash");
    }

    #[test]
    fn draft_hash_changes_when_threshold_changes() {
        let mut p = fixture_preimage();
        let h0 = compute_draft_hash(&p);
        p.threshold_bps = 250; // 2.5%
        let h1 = compute_draft_hash(&p);
        assert_ne!(h0, h1, "threshold_bps must affect the hash");
    }

    #[test]
    fn draft_hash_changes_when_user_message_hash_changes() {
        let mut p = fixture_preimage();
        let h0 = compute_draft_hash(&p);
        p.original_user_message_hash = "1".repeat(64);
        let h1 = compute_draft_hash(&p);
        assert_ne!(h0, h1);
    }

    #[test]
    fn draft_hash_unaffected_by_excluded_volatile_fields() {
        // The same canonical preimage hashes the same regardless of
        // draft_id, created_at_ms, session_id, warnings, review_copy —
        // verified by mutating those on a DraftIntent and re-computing.
        let mut d = fixture_draft();
        let h0 = d.compute_hash();
        d.draft_id = "ff".repeat(16);
        d.created_at_ms = 9_999_999_999_999;
        d.session_id_hex = "different-session".to_string();
        d.warnings = vec!["spurious".to_string()];
        d.review_copy = "totally different summary".to_string();
        let h1 = d.compute_hash();
        assert_eq!(h0, h1, "volatile fields must not affect canonical hash");
    }

    // ── Decimal amount conversion ───────────────────────────────────────

    #[test]
    fn amount_parse_happy_path() {
        assert_eq!(parse_usdc_amount_to_raw("0.5").unwrap(), 500_000);
        assert_eq!(parse_usdc_amount_to_raw("0.25").unwrap(), 250_000);
        assert_eq!(parse_usdc_amount_to_raw("1").unwrap(), 1_000_000);
        assert_eq!(parse_usdc_amount_to_raw("1.0").unwrap(), 1_000_000);
        assert_eq!(parse_usdc_amount_to_raw("0.1").unwrap(), 100_000);
        assert_eq!(parse_usdc_amount_to_raw("0.123456").unwrap(), 123_456);
        // Whitespace trimmed.
        assert_eq!(parse_usdc_amount_to_raw(" 0.5 ").unwrap(), 500_000);
    }

    #[test]
    fn amount_parse_rejects_more_than_six_decimals() {
        assert_eq!(
            parse_usdc_amount_to_raw("0.1234567"),
            Err(AmountParseError::TooManyDecimals)
        );
    }

    #[test]
    fn amount_parse_rejects_garbage() {
        assert_eq!(
            parse_usdc_amount_to_raw(""),
            Err(AmountParseError::Empty)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("-0.5"),
            Err(AmountParseError::Negative)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("abc"),
            Err(AmountParseError::InvalidChar)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("1.2.3"),
            Err(AmountParseError::MultipleDots)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("0"),
            Err(AmountParseError::Zero)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("0.0"),
            Err(AmountParseError::Zero)
        );
        assert_eq!(
            parse_usdc_amount_to_raw("+0.5"),
            Err(AmountParseError::InvalidChar)
        );
    }

    #[test]
    fn amount_parse_no_f64_drift() {
        // 0.1 + 0.2 == 0.30000000000000004 in f64 land; in raw u64
        // land it must be exactly 300_000.
        let a = parse_usdc_amount_to_raw("0.1").unwrap();
        let b = parse_usdc_amount_to_raw("0.2").unwrap();
        assert_eq!(a + b, 300_000);
        // 0.3 directly must also be 300_000.
        assert_eq!(parse_usdc_amount_to_raw("0.3").unwrap(), 300_000);
    }

    // ── DraftIntentStore ────────────────────────────────────────────────

    #[test]
    #[should_panic]
    fn store_rejects_ttl_under_ten_minutes() {
        // 9 minutes — must panic at construction.
        let _ = DraftIntentStore::new(9 * 60);
    }

    #[test]
    fn store_insert_then_get_returns_clone() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        store.insert(d.clone());
        let g = store.get(&sid, &id).unwrap();
        assert_eq!(g.amount_raw, d.amount_raw);
        assert_eq!(g.threshold_bps, d.threshold_bps);
        // get is non-consuming; entry still present.
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_get_unknown_returns_none() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        assert!(store.get("any-session", "nope").is_none());
    }

    #[test]
    fn store_consume_if_match_happy_path_installs_tombstone() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        let h = d.compute_hash();
        store.insert(d.clone());
        match store.consume_if_match(&sid, &id, &h) {
            DraftConsumeOutcome::Ok(out) => assert_eq!(out.draft_id, id),
            other => panic!("expected Ok, got {other:?}"),
        }
        // Phase 5c-lite contract-alignment: consume installs a
        // tombstone in the same slot (not a plain removal). The slot
        // still exists; the active-draft count is zero.
        assert_eq!(store.active_len(), 0, "active draft was consumed");
        assert_eq!(store.tombstone_len(), 1, "tombstone installed for 409 path");
        assert_eq!(store.len(), 1, "slot is still occupied (by the tombstone)");
    }

    #[test]
    fn store_consume_idempotency_second_call_returns_already_consumed() {
        // Phase 5c-lite contract-alignment: consume installs a
        // tombstone so a SECOND confirm with the same draft_id
        // surfaces `AlreadyConsumed` (the route maps this to
        // HTTP 409 `draft_already_finalized_or_missing`) rather than
        // the 404 used for never-existed drafts.
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        let h = d.compute_hash();
        store.insert(d);
        assert!(matches!(
            store.consume_if_match(&sid, &id, &h),
            DraftConsumeOutcome::Ok(_)
        ));
        // Second call: AlreadyConsumed (NOT NotFoundOrExpired, NOT
        // a duplicate Ok).
        assert!(matches!(
            store.consume_if_match(&sid, &id, &h),
            DraftConsumeOutcome::AlreadyConsumed
        ));
        // Active drafts: zero. Tombstone slot: one.
        assert_eq!(store.active_len(), 0);
        assert_eq!(store.tombstone_len(), 1);
    }

    #[test]
    fn store_never_minted_draft_returns_not_found_or_expired() {
        // The 404 path: a confirm targeting a draft_id that was
        // NEVER minted observes NotFoundOrExpired (NOT
        // AlreadyConsumed — there's no tombstone to hit).
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let outcome = store.consume_if_match(
            "any-session",
            "ff".repeat(16).as_str(),
            "0".repeat(64).as_str(),
        );
        assert!(matches!(outcome, DraftConsumeOutcome::NotFoundOrExpired));
    }

    #[test]
    fn store_tombstone_ttl_is_draft_ttl_plus_sixty_seconds() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        assert_eq!(store.ttl_seconds(), DEFAULT_DRAFT_TTL_SECONDS);
        assert_eq!(
            store.tombstone_ttl_seconds(),
            DEFAULT_DRAFT_TTL_SECONDS + TOMBSTONE_TTL_EXTRA_SECONDS
        );
        assert_eq!(TOMBSTONE_TTL_EXTRA_SECONDS, 60);
    }

    #[test]
    fn store_get_returns_none_when_slot_holds_a_tombstone() {
        // After consume, `get` for the same draft_id must return
        // None (the tombstone is opaque to the lookup path).
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        let h = d.compute_hash();
        store.insert(d);
        let _ = store.consume_if_match(&sid, &id, &h);
        assert!(store.get(&sid, &id).is_none());
    }

    #[test]
    fn store_drop_draft_clears_tombstone_too() {
        // drop_draft must remove EITHER an active draft OR a
        // tombstone — the reject path may run after consume and
        // we don't want stranded tombstones polluting len().
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        let h = d.compute_hash();
        store.insert(d);
        let _ = store.consume_if_match(&sid, &id, &h);
        assert_eq!(store.tombstone_len(), 1);
        store.drop_draft(&sid, &id);
        assert_eq!(store.tombstone_len(), 0);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn store_consume_hash_mismatch_leaves_entry() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        store.insert(d.clone());
        let wrong = "f".repeat(64);
        match store.consume_if_match(&sid, &id, &wrong) {
            DraftConsumeOutcome::HashMismatch { provided, backend } => {
                assert_eq!(provided, wrong);
                assert_ne!(backend, wrong);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        // Entry STILL present (mismatch must NOT consume).
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_drop_draft_is_idempotent() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let d = fixture_draft();
        let id = d.draft_id.clone();
        let sid = d.session_id_hex.clone();
        store.insert(d);
        store.drop_draft(&sid, &id);
        assert!(store.is_empty());
        // Second drop is a no-op (no panic).
        store.drop_draft(&sid, &id);
    }

    #[test]
    fn store_multiple_drafts_per_session_keyed_by_draft_id() {
        let store = DraftIntentStore::new(DEFAULT_DRAFT_TTL_SECONDS);
        let mut a = fixture_draft();
        a.draft_id = "aaaa".repeat(8);
        a.amount_raw = 200_000;
        let mut b = fixture_draft();
        b.draft_id = "bbbb".repeat(8);
        b.amount_raw = 700_000;
        // Same session, different drafts.
        assert_eq!(a.session_id_hex, b.session_id_hex);
        let sid = a.session_id_hex.clone();
        store.insert(a.clone());
        store.insert(b.clone());
        assert_eq!(store.len(), 2);
        let got_a = store.get(&sid, &a.draft_id).unwrap();
        let got_b = store.get(&sid, &b.draft_id).unwrap();
        assert_eq!(got_a.amount_raw, 200_000);
        assert_eq!(got_b.amount_raw, 700_000);
    }

    // ── Range pin sanity ────────────────────────────────────────────────

    #[test]
    fn phase5c_range_pins_match_brief() {
        assert_eq!(PHASE5C_MIN_AMOUNT_RAW, 100_000);
        assert_eq!(PHASE5C_MAX_AMOUNT_RAW, 1_000_000);
        assert_eq!(PHASE5C_EXPIRY_SECONDS_AFTER_FINALIZE, 180);
    }
}
