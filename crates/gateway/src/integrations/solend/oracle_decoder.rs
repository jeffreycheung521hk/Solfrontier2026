//! Provider-specific oracle-account publish-freshness extraction.
//!
//! This module is the single protocol-specific seam for decoding oracle
//! account payloads into the normalized [`FeedPublishFreshness`] carried
//! by `crate::lending::snapshot`. It is INTERNAL to `integrations::solend`
//! — `lending::policy` MUST NOT import from here.
//!
//! ## Current decoder status (Slice 2C)
//!
//! ### Pyth Solana Receiver — **VERIFIED**
//!
//! Owner program `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` (134-byte
//! `PriceUpdateV2` accounts). Decoder returns
//! [`FeedPublishFreshness::KnownSlot`] from the account's `posted_slot`
//! field.
//!
//! Source of truth: `pyth-network/pyth-crosschain` repository,
//! `target_chains/solana/pyth_solana_receiver_sdk/src/price_update.rs`.
//! Verbatim `PriceUpdateV2::LEN` formula = `8 + 32 + 2 + 32 + 8 + 8 + 4 +
//! 8 + 8 + 8 + 8 + 8 = 134` bytes, matching the 134-byte mainnet account
//! size observed across Slice 1.5 live smoke and this slice's verification
//! fetch.
//!
//! Two subtleties forced branching on the `verification_level` Borsh tag
//! byte at offset 40:
//! - `VerificationLevel::Partial { num_signatures: u8 }` serializes as
//!   `[0, num_sigs]` — 2 bytes.
//! - `VerificationLevel::Full` serializes as `[1]` — 1 byte.
//! The `LEN` constant reserves 2 bytes so that Full-variant accounts have
//! one trailing zero byte. Reading `posted_slot` at a fixed offset without
//! branching would silently mis-align on Full-variant accounts.
//!
//! Note on naming: earlier revisions referred to this provider as
//! "Pyth Lazer (receiver)", inherited from the Slice 1 read-only spike
//! where `rec5EK…` was informally labeled that way. The owner program is
//! in fact the **Pyth Solana Receiver** (Pyth Pull Oracle) — a distinct
//! product from Pyth Lazer. The enum variant and decoder function were
//! renamed to `PythSolanaReceiver` / `decode_pyth_solana_receiver_publish_freshness`
//! in the Slice 2C closeout for accuracy.
//!
//! ### Switchboard On-Demand — **UNVERIFIED, still Unknown**
//!
//! Owner program `SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f` (3851-byte
//! `PullFeedAccountData` accounts). Decoder continues to return
//! [`FeedPublishFreshness::Unknown`].
//!
//! Evidence search during Slice 2C:
//! - `switchboard-xyz/sb-on-demand-solana` — 404 on every path tried.
//! - `switchboard-xyz/solana-sdk` — 404 for `rust/switchboard-on-demand/
//!   src/accounts/pull_feed.rs`.
//! - `docs.rs/switchboard-on-demand` — 404 on struct page.
//! - `switchboard-on-demand-rust-docs.web.app` — 404.
//! - Public search surfaced only call-site usage (`PullFeedAccountData::
//!   parse(account)`) without a pinned struct definition.
//!
//! Without a pinned upstream source committing to exact field offsets and
//! a discriminator, hardcoding Switchboard offsets would violate Part 6B
//! §65. Switchboard feeds therefore continue to report `Unknown`; at the
//! policy layer `MaxOracleStalenessMs` HardBlocks them. When an
//! authoritative source is located (pinned-commit URL to the
//! `PullFeedAccountData` definition, plus a round-trip fixture against a
//! real mainnet account), a follow-up slice can drop the decoder in
//! behind `decode_switchboard_on_demand_publish_freshness` without
//! touching any other module.
//!
//! ## Consequence at the policy layer
//!
//! `MaxOracleStalenessMs` treats `FeedPublishFreshness::Unknown` as
//! fail-closed (§65 / §18.2). For Solend reserves whose only configured
//! feed is Switchboard (the majority observed during Slice 1.5 — two of
//! three reserves in the spike target), evaluation will continue to
//! HardBlock until the Switchboard decoder lands. This is a correct V1
//! posture; Slice 3 (deposit builder + refresh precondition) is gated on
//! clearing this last decoder evidence gap OR on targeting a Solend
//! reserve whose Pyth slot is populated (not the sentinel).
//!
//! ## Invariants every provider decoder enforces
//!
//! 1. Length gate: account data length must equal the provider's
//!    pinned `LEN`. Mismatch ⇒ `Unknown`.
//! 2. Discriminator gate (for Anchor accounts): the first 8 bytes must
//!    match the expected `sha256("account:<TypeName>")[0..8]`. Mismatch
//!    ⇒ `Unknown`.
//! 3. Version / variant gate: any Borsh or Anchor enum whose variant
//!    shifts downstream offsets must be branched explicitly. Unrecognized
//!    tags ⇒ `Unknown`.
//! 4. Slot extraction at the branched offset.

use solana_sdk::pubkey::Pubkey;

use crate::lending::{ChainSlot, FeedPublishFreshness};

use super::mapping::classify_oracle_provider;
use crate::lending::OracleProvider;

// ── Pyth Solana Receiver (`PriceUpdateV2`) ─────────────────────────────────

/// Pinned total account size for `PriceUpdateV2`.
///
/// Source: `pyth-network/pyth-crosschain`,
/// `target_chains/solana/pyth_solana_receiver_sdk/src/price_update.rs`,
/// `PriceUpdateV2::LEN = 8 + 32 + 2 + 32 + 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8`.
pub const PYTH_PRICE_UPDATE_V2_LEN: usize = 134;

/// Anchor discriminator for `PriceUpdateV2` = first 8 bytes of
/// `sha256("account:PriceUpdateV2")`. Verified byte-for-byte against a
/// real mainnet `PriceUpdateV2` account during Slice 2C; see the
/// round-trip fixture test in this file for the verified payload.
pub const PYTH_PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] =
    [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];

/// Decode `posted_slot` from a Pyth Solana Receiver `PriceUpdateV2`
/// account into [`FeedPublishFreshness::KnownSlot`].
///
/// Layout (cited from `PriceUpdateV2` definition above):
///
/// ```text
///   0..8     Anchor discriminator (8 bytes)
///   8..40    write_authority: Pubkey (32 bytes)
///   40       verification_level: Borsh enum tag
///     tag=0  Partial { num_signatures: u8 }  — 2 bytes total (tag + u8)
///     tag=1  Full                            — 1 byte total (tag only)
///   {41|42}..+84   price_message: PriceFeedMessage
///                    feed_id(32) + price(i64) + conf(u64) + exponent(i32)
///                    + publish_time(i64) + prev_publish_time(i64)
///                    + ema_price(i64) + ema_conf(u64)
///   {125|126}..+8  posted_slot: u64   ← returned as ChainSlot
///   trailing       zero-padding (Full variant reserves 2 bytes for enum;
///                  1 byte unused and zero)
/// ```
///
/// Fails closed (`Unknown`) on any of:
/// - wrong length,
/// - wrong discriminator,
/// - unrecognized `verification_level` tag.
pub fn decode_pyth_solana_receiver_publish_freshness(
    account_data: &[u8],
) -> FeedPublishFreshness {
    if account_data.len() != PYTH_PRICE_UPDATE_V2_LEN {
        return FeedPublishFreshness::Unknown;
    }
    if account_data[0..8] != PYTH_PRICE_UPDATE_V2_DISCRIMINATOR {
        return FeedPublishFreshness::Unknown;
    }
    // Branch on the verification_level Borsh tag at offset 40.
    let price_message_start = match account_data[40] {
        0 => 42,           // Partial { num_signatures: u8 } — 2-byte enum
        1 => 41,           // Full                           — 1-byte enum
        _ => return FeedPublishFreshness::Unknown, // unknown variant
    };
    const PRICE_MESSAGE_LEN: usize = 84;
    let posted_slot_start = price_message_start + PRICE_MESSAGE_LEN;
    // Defensive check — if our branching is ever wrong, bail before read.
    if posted_slot_start + 8 > account_data.len() {
        return FeedPublishFreshness::Unknown;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&account_data[posted_slot_start..posted_slot_start + 8]);
    let posted_slot = u64::from_le_bytes(buf);
    FeedPublishFreshness::KnownSlot(ChainSlot::new(posted_slot))
}

// ── Switchboard On-Demand — fail-closed pending verified source ────────────

/// Stub: Switchboard On-Demand publish-freshness extraction. Returns
/// [`FeedPublishFreshness::Unknown`] unconditionally pending a pinned
/// upstream source reference. See the module-level doc for the evidence
/// trail.
pub fn decode_switchboard_on_demand_publish_freshness(
    _account_data: &[u8],
) -> FeedPublishFreshness {
    FeedPublishFreshness::Unknown
}

// ── Dispatcher ─────────────────────────────────────────────────────────────

/// Dispatcher: classify the oracle by owner-program identity and hand off
/// to the provider-specific decoder. Unknown provider ⇒ `Unknown`.
pub fn extract_publish_freshness(
    owner_program: &Pubkey,
    account_data: &[u8],
) -> FeedPublishFreshness {
    match classify_oracle_provider(owner_program) {
        OracleProvider::PythSolanaReceiver => decode_pyth_solana_receiver_publish_freshness(account_data),
        OracleProvider::SwitchboardOnDemand => {
            decode_switchboard_on_demand_publish_freshness(account_data)
        }
        OracleProvider::Unknown => FeedPublishFreshness::Unknown,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::str::FromStr;

    /// Committed mainnet `PriceUpdateV2` payload, fetched once from
    /// `https://api.mainnet-beta.solana.com` for the Pyth feed
    /// `Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX` (referenced by a
    /// Solend Coin98-pool reserve during the Slice 1.5 smoke test). This
    /// is public on-chain data; committing it here makes the decoder
    /// round-trip offline and CI-friendly.
    ///
    /// At fetch time:
    /// - RPC-reported context slot: 414,496,713
    /// - `verification_level` tag byte: 1 (Full variant)
    /// - `posted_slot`: 414,496,622
    /// - `publish_time` (Unix seconds, i64): 1,776,696,877
    /// - `exponent` (i32): −8
    /// - trailing byte[133]: 0 (reserved padding)
    const PYTH_PRICE_UPDATE_V2_FIXTURE_BASE64: &str =
        "IvEjY51+9M2+k5qDCfVkBxh//zCsVLFpSYvpn22OG/1CRGgM1PfR4gHqoCDGHMR5cSgTRhzhU4lKlqbACyHtDPwnmNH5qenJSu6Y9QUAAAAAK9cAAAAAAAD4////LT7maQAAAAAsPuZpAAAAANaa9QUAAAAAE/0AAAAAAABut7QYAAAAAAA=";

    const PYTH_FIXTURE_EXPECTED_POSTED_SLOT: u64 = 414_496_622;

    fn load_pyth_fixture() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(PYTH_PRICE_UPDATE_V2_FIXTURE_BASE64)
            .expect("fixture base64 decodes")
    }

    #[test]
    fn pyth_fixture_size_matches_pinned_len() {
        let bytes = load_pyth_fixture();
        assert_eq!(bytes.len(), PYTH_PRICE_UPDATE_V2_LEN);
    }

    #[test]
    fn pyth_fixture_round_trip_decodes_known_slot() {
        let bytes = load_pyth_fixture();
        let result = decode_pyth_solana_receiver_publish_freshness(&bytes);
        assert_eq!(
            result,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(
                PYTH_FIXTURE_EXPECTED_POSTED_SLOT
            )),
            "mainnet Pyth PriceUpdateV2 fixture must decode to the expected \
             posted_slot — drift here means the decoder is mis-reading"
        );
    }

    #[test]
    fn pyth_fixture_discriminator_matches_pinned_constant() {
        let bytes = load_pyth_fixture();
        assert_eq!(bytes[0..8], PYTH_PRICE_UPDATE_V2_DISCRIMINATOR);
    }

    #[test]
    fn pyth_fixture_verification_level_is_full() {
        // Documents the specific mainnet account's variant; Partial is also
        // structurally supported by the decoder's branch but is not
        // exercised by this particular fixture.
        let bytes = load_pyth_fixture();
        assert_eq!(bytes[40], 1);
        assert_eq!(bytes[133], 0, "Full variant reserves 1 zero byte trailer");
    }

    #[test]
    fn pyth_wrong_length_returns_unknown() {
        let too_short = vec![0u8; PYTH_PRICE_UPDATE_V2_LEN - 1];
        assert_eq!(
            decode_pyth_solana_receiver_publish_freshness(&too_short),
            FeedPublishFreshness::Unknown
        );
        let too_long = vec![0u8; PYTH_PRICE_UPDATE_V2_LEN + 1];
        assert_eq!(
            decode_pyth_solana_receiver_publish_freshness(&too_long),
            FeedPublishFreshness::Unknown
        );
    }

    #[test]
    fn pyth_wrong_discriminator_returns_unknown() {
        let mut bytes = load_pyth_fixture();
        bytes[0] ^= 0xFF; // flip first discriminator byte
        assert_eq!(
            decode_pyth_solana_receiver_publish_freshness(&bytes),
            FeedPublishFreshness::Unknown
        );
    }

    #[test]
    fn pyth_unknown_verification_level_returns_unknown() {
        let mut bytes = load_pyth_fixture();
        bytes[40] = 42; // neither 0 (Partial) nor 1 (Full)
        assert_eq!(
            decode_pyth_solana_receiver_publish_freshness(&bytes),
            FeedPublishFreshness::Unknown
        );
    }

    #[test]
    fn pyth_partial_variant_offsets_are_branched() {
        // Synthesize a valid-discriminator, Partial-variant account: shift
        // the posted_slot location by 1 byte relative to the Full variant
        // and confirm the decoder reads from 126..134.
        let mut bytes = vec![0u8; PYTH_PRICE_UPDATE_V2_LEN];
        bytes[0..8].copy_from_slice(&PYTH_PRICE_UPDATE_V2_DISCRIMINATOR);
        bytes[40] = 0; // Partial
        bytes[41] = 2; // num_signatures (arbitrary)
        let expected_slot: u64 = 999_000_111;
        bytes[126..134].copy_from_slice(&expected_slot.to_le_bytes());
        assert_eq!(
            decode_pyth_solana_receiver_publish_freshness(&bytes),
            FeedPublishFreshness::KnownSlot(ChainSlot::new(expected_slot))
        );
    }

    #[test]
    fn extract_dispatches_to_pyth_for_pyth_owner() {
        let pyth_owner: Pubkey =
            Pubkey::from_str("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ").unwrap();
        let bytes = load_pyth_fixture();
        let f = extract_publish_freshness(&pyth_owner, &bytes);
        assert_eq!(
            f,
            FeedPublishFreshness::KnownSlot(ChainSlot::new(
                PYTH_FIXTURE_EXPECTED_POSTED_SLOT
            ))
        );
    }

    #[test]
    fn switchboard_on_demand_reports_unknown_pending_verified_source() {
        let swb_owner: Pubkey =
            Pubkey::from_str("SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f").unwrap();
        let dummy = vec![0u8; 3851];
        let f = extract_publish_freshness(&swb_owner, &dummy);
        assert_eq!(f, FeedPublishFreshness::Unknown);
    }

    #[test]
    fn unknown_provider_reports_unknown() {
        let other = Pubkey::new_unique();
        let f = extract_publish_freshness(&other, &[]);
        assert_eq!(f, FeedPublishFreshness::Unknown);
    }
}
