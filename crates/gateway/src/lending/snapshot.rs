//! Normalized `LendingSnapshot` shape — the policy-evaluator input seam.
//!
//! Spec anchors: `docs/lending_policy_vocabulary.md` §48 (snapshot shape),
//! §11.1 (oracle feed-set), §9 (obligation), §10 (reserve).
//!
//! All numeric fields carry unit tags through the newtypes in
//! [`crate::lending::types`]. Required-present fields are not `Option<T>`:
//! missing data is a pre-evaluation failure at the assembler boundary
//! (Part 5A §42), not a `None` the evaluator can observe (§48.1).

use solana_sdk::pubkey::Pubkey;

use super::types::{ChainSlot, CollateralTokenAmount, UnderlyingAmount, WadAmount};

/// Closed enum of V1-supported lending protocols.
///
/// This enum is protocol-agnostic at the type level — variants are labels,
/// not structures. A `ProtocolTag::Solend` appearing in a snapshot field
/// is not a seam violation (§63.4). A Solend-specific struct in the same
/// field would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolTag {
    Solend,
}

/// Protocol-native freshness marker. Surfaced verbatim from the protocol's
/// own `stale` bit equivalent. Interpretation lives in the rule evaluator
/// (Part 3A §18.1), not in the snapshot assembly layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleMarker {
    Fresh,
    Stale,
}

/// RPC-reported chain slot at snapshot assembly time. Acts as the "now"
/// reference for V1 staleness arithmetic per §51.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotFreshness {
    pub observed_slot: ChainSlot,
}

/// RPC-reported chain slot at a single component's fetch time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentFreshness {
    pub observed_slot: ChainSlot,
}

/// Oracle's self-reported publish freshness, normalized across providers.
///
/// `KnownSlot` carries a publish slot extracted by a verified provider
/// decoder. `Unknown` encodes "the provider payload could not be safely
/// decoded to a freshness value" — explicit fail-closed input for
/// `MaxOracleStalenessMs`. This is the explicit form hinted at in Part 6B
/// §65: an unverified provider layout must not silently pass; it reports
/// `Unknown` and the policy layer HardBlocks.
///
/// Future provider variants (e.g., `KnownWallTimeMs`) will be added here
/// when verified decoders land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPublishFreshness {
    KnownSlot(ChainSlot),
    Unknown,
}

/// Reserve lifecycle markers. V1 rules do not consume these; the struct
/// is carried for schema-completeness per Part 2 §10 and future rule
/// use. Slice 1 leaves it as an empty placeholder; decoding of
/// `paused` / `deposit_capped` / `borrow_capped` bits is deferred to a
/// future slice whose rules actually consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReserveLifecycleMarkers {}

/// Oracle provider label. `Unknown` is a legitimate value for feeds
/// whose owner program is not in V1's recognized set — the feed still
/// surfaces (freshness rules can evaluate it), but downstream code
/// that wants provider-specific handling sees `Unknown` as opt-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleProvider {
    /// Pyth Solana Receiver program (`rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`)
    /// — aka Pyth Pull Oracle. Publishes `PriceUpdateV2` accounts.
    PythSolanaReceiver,
    SwitchboardOnDemand,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleFeed {
    pub identifier: Pubkey,
    pub provider: OracleProvider,
    pub publish: FeedPublishFreshness,
    pub fetch: ComponentFreshness,
}

/// Per-priced-asset oracle feed set. An empty `feeds` Vec is a legal
/// representation of "no feed configured for this priced asset" per
/// §11.1; V1 rules fail-closed when they encounter one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleFeedSet {
    pub priced_asset: Pubkey,
    pub feeds: Vec<OracleFeed>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObligationDeposit {
    pub reserve: Pubkey,
    pub deposited: CollateralTokenAmount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObligationBorrow {
    pub reserve: Pubkey,
    pub borrowed_wads: WadAmount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub identifier: Pubkey,
    pub owner: Pubkey,
    pub protocol_last_updated_slot: ChainSlot,
    pub protocol_native_stale: StaleMarker,
    pub deposits: Vec<ObligationDeposit>,
    pub borrows: Vec<ObligationBorrow>,
    pub freshness: ComponentFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserve {
    pub identifier: Pubkey,
    pub mint: Pubkey,
    pub mint_decimals: u8,
    pub protocol_last_updated_slot: ChainSlot,
    pub protocol_native_stale: StaleMarker,
    pub lifecycle: ReserveLifecycleMarkers,
    pub freshness: ComponentFreshness,
    pub liquidity_supply_pubkey: Pubkey,
    pub collateral_mint_pubkey: Pubkey,
    pub collateral_supply_pubkey: Pubkey,
    pub available_underlying: UnderlyingAmount,
}

/// Frozen, freshness-aware read model the policy evaluator consumes.
///
/// Per §48, every required field here is required-present. The absence
/// of a required field is not representable; it is a pre-evaluation
/// failure at the assembler boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingSnapshot {
    pub protocol_tag: ProtocolTag,
    pub session_wallet: Pubkey,
    pub fetched_at: SnapshotFreshness,
    pub obligation: Obligation,
    pub reserves: Vec<Reserve>,
    pub oracles: Vec<OracleFeedSet>,
}
