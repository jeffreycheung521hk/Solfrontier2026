//! V1 lending domain newtypes — compile-time unit tags.
//!
//! Spec anchor: `docs/lending_policy_vocabulary.md` §47.1.
//!
//! Each newtype wraps a private integer and exposes construction / raw-access
//! methods. The wrapper's purpose is to make cross-unit confusion
//! unrepresentable at function-signature level (§44). Arithmetic within a
//! single unit is permitted via the comparison traits derived below; bare
//! `u64` / `u128` arithmetic across units requires an explicit `raw()` call,
//! which is the single, auditable conversion site.

/// Base units of an SPL mint (e.g., USDC micro-units at 6 decimals,
/// SOL lamports at 9). Used for Deposit / Repay action inputs and for
/// reserve-level underlying quantities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnderlyingAmount(u64);

impl UnderlyingAmount {
    pub const ZERO: Self = Self(0);

    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Protocol collateral-token / c-token units. Not convertible to
/// `UnderlyingAmount` without a protocol-specific exchange rate (Part 6).
/// Separated at the type level specifically to make c-token ↔ underlying
/// confusion a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollateralTokenAmount(u64);

impl CollateralTokenAmount {
    pub const ZERO: Self = Self(0);

    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// High-precision fixed-point amount in the protocol's wad domain
/// (typically 10^18-scaled). 128-bit width because observed wad-scaled
/// borrow values on mainnet exceed 64 bits (spike evidence, §30.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WadAmount(u128);

impl WadAmount {
    pub const ZERO: Self = Self(0);

    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u128 {
        self.0
    }
}

/// Solana chain slot. Opaque integer identity of a slot — not a duration,
/// not a millisecond count, not an amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainSlot(u64);

impl ChainSlot {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Number of slots between two chain slots, saturating at zero if
    /// `earlier > self`. The caller chooses which slot is "later"; this
    /// method does not take a "now."
    pub const fn slots_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Millisecond-scale duration. Used for staleness thresholds
/// (`max_fetch_age`, `max_publish_age`). Paired with `ChainSlot` through
/// explicit conversion, not implicit arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMs(u64);

impl DurationMs {
    pub const ZERO: Self = Self(0);

    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}
