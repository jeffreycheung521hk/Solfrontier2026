//! Solend raw → normalized `LendingSnapshot` mapping.
//!
//! Spec anchor: `docs/lending_policy_vocabulary.md` Part 6B §63
//! (anti-corruption boundary) and §64 (minimal decoding).
//!
//! This module is the single site where Solend-shaped types terminate
//! and protocol-agnostic `crate::lending` types begin. The seam invariant
//! (Part 5A §41.3) is: no `Solend*` type appears in the return values
//! exposed here.

use solana_sdk::pubkey::Pubkey;

use crate::lending::{
    ChainSlot, CollateralTokenAmount, ComponentFreshness, FeedPublishFreshness,
    LendingSnapshot, Obligation, ObligationBorrow, ObligationDeposit, OracleFeed,
    OracleFeedSet, OracleProvider, ProtocolTag, Reserve, ReserveLifecycleMarkers,
    SnapshotFreshness, StaleMarker, UnderlyingAmount, WadAmount,
};

use super::raw::{self, SolendObligationRaw, SolendReserveRaw};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    #[error(
        "obligation references reserve {reserve} which is not present in the input set"
    )]
    ObligationReserveMissing { reserve: Pubkey },
    #[error("reserve {reserve} belongs to a different lending market than the obligation")]
    ReserveMarketMismatch { reserve: Pubkey },
    #[error(
        "freshness input slot for component {component:?} is older than the per-component \
         observed slot (would violate worst-component-dominates ordering)"
    )]
    FreshnessOrdering { component: FreshnessKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessKind {
    Snapshot,
    Obligation,
    Reserve,
    Oracle,
}

/// Oracle account-level metadata the assembler gathers from `getAccountInfo`
/// before handing inputs to the mapping step. Only the dimensions needed
/// to classify provider + carry fetch-freshness are present here; price /
/// confidence decoding is a future slice's concern (Part 4A §32.2).
///
/// `publish` carries the provider's self-reported publish freshness in the
/// normalized [`FeedPublishFreshness`] form. For providers whose payload
/// layout has not been verified against committed upstream source, the
/// caller passes `FeedPublishFreshness::Unknown` — `MaxOracleStalenessMs`
/// treats `Unknown` as fail-closed per Part 6B §65.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleAccountInfo {
    pub pubkey: Pubkey,
    pub owner_program: Pubkey,
    pub fetched_at_slot: ChainSlot,
    pub publish: FeedPublishFreshness,
}

/// Full set of inputs the mapping step takes. The caller (outer wiring)
/// is responsible for assembling these from RPC; this function is pure
/// over them.
#[derive(Debug, Clone)]
pub struct SolendAssemblyInputs {
    pub session_wallet: Pubkey,
    pub obligation_pubkey: Pubkey,
    pub obligation_raw: SolendObligationRaw,
    pub obligation_fetched_at_slot: ChainSlot,
    pub reserves: Vec<ReserveInput>,
    /// Non-sentinel oracle accounts fetched from RPC. Sentinel slots are
    /// filtered OUT before this list is constructed; the mapping step does
    /// not re-filter. See [`classify_oracle_provider`].
    pub oracles: Vec<OracleAccountInfo>,
    /// RPC-context slot observed at snapshot assembly completion.
    pub snapshot_observed_slot: ChainSlot,
}

/// Inputs for the first-deposit mapping path — the obligation account
/// does not yet exist on chain. Slice 3A.
///
/// Spec anchors:
/// - Part 6B §66 — refresh / first-deposit precondition belongs to outer
///   wiring, not to the policy evaluator.
/// - Pre-Slice-3 recon empty-obligation caveat: do NOT treat absence of
///   feeds as a pass. The caller MUST inject the intended deposit target
///   reserve into [`Self::target_reserves`] so the policy evaluator sees
///   an oracle feed set for the priced asset the action will touch.
///
/// Why a separate input shape rather than weakening the existing
/// [`map_snapshot`]: the strict [`raw::decode_obligation`] decoder
/// continues to require Solend-owned 1300-byte data. Uninitialized /
/// System-Program-owned obligation accounts cannot be passed through
/// `decode_obligation` and MUST NOT silently flow into the normal
/// mapping path. This input enum makes the first-deposit case explicit
/// at the integration seam.
#[derive(Debug, Clone)]
pub struct FirstDepositAssemblyInputs {
    pub session_wallet: Pubkey,
    /// The obligation account's pubkey — typically a PDA that has not yet
    /// been initialized on chain. The deposit ix will create it.
    pub obligation_pubkey: Pubkey,
    /// The Solend lending market the deposit targets. All reserves in
    /// `target_reserves` must report this same `lending_market`.
    pub lending_market: Pubkey,
    /// Reserves the policy must evaluate. For a first-deposit slice this
    /// MUST contain the intended deposit target reserve, even though the
    /// (empty) obligation does not yet reference it. Additional reserves
    /// MAY be included if outer wiring chooses to evaluate a wider set.
    pub target_reserves: Vec<ReserveInput>,
    /// Non-sentinel oracle accounts the caller fetched. Same rule as
    /// [`SolendAssemblyInputs::oracles`]: sentinels are pre-filtered.
    pub oracles: Vec<OracleAccountInfo>,
    pub snapshot_observed_slot: ChainSlot,
}

#[derive(Debug, Clone)]
pub struct ReserveInput {
    pub pubkey: Pubkey,
    pub raw: SolendReserveRaw,
    pub fetched_at_slot: ChainSlot,
}

/// Raw → normalized snapshot mapping. Pure function.
///
/// Responsibilities:
/// 1. Wrap Solend-raw amounts in the Part 5B newtypes (unit tagging).
/// 2. Convert stale bits to [`StaleMarker`].
/// 3. Build per-priced-asset [`OracleFeedSet`]s from the oracle account
///    list provided by the caller (sentinels already excluded upstream).
/// 4. Verify every reserve referenced by an obligation deposit/borrow is
///    present in `reserves` — missing = `MappingError`.
/// 5. Verify every reserve's `lending_market` matches the obligation's
///    `lending_market`.
///
/// Explicitly NOT responsibilities of this function:
/// - Fetching any account. The caller does RPC.
/// - Refreshing stale accounts. Part 6B §66 refresh strategy lives in
///   outer wiring.
/// - Owner / protocol-tag checks. Those are the System Invariant Gate's
///   (`crate::lending::gate::check_system_invariants`).
/// - Any freshness-threshold comparison. That is the rule evaluator's.
pub fn map_snapshot(inputs: SolendAssemblyInputs) -> Result<LendingSnapshot, MappingError> {
    let SolendAssemblyInputs {
        session_wallet,
        obligation_pubkey,
        obligation_raw,
        obligation_fetched_at_slot,
        reserves,
        oracles,
        snapshot_observed_slot,
    } = inputs;

    // Obligation must reference only reserves present in the input set,
    // and all referenced reserves must share its lending market.
    for dep in &obligation_raw.deposits {
        find_reserve(&reserves, &dep.deposit_reserve)?;
    }
    for bor in &obligation_raw.borrows {
        find_reserve(&reserves, &bor.borrow_reserve)?;
    }
    for r in &reserves {
        if r.raw.lending_market != obligation_raw.lending_market {
            return Err(MappingError::ReserveMarketMismatch { reserve: r.pubkey });
        }
    }

    let obligation = Obligation {
        identifier: obligation_pubkey,
        owner: obligation_raw.owner,
        protocol_last_updated_slot: ChainSlot::new(obligation_raw.last_update_slot),
        protocol_native_stale: stale_to_marker(obligation_raw.last_update_stale),
        deposits: obligation_raw
            .deposits
            .iter()
            .map(|d| ObligationDeposit {
                reserve: d.deposit_reserve,
                deposited: CollateralTokenAmount::new(d.deposited_amount),
            })
            .collect(),
        borrows: obligation_raw
            .borrows
            .iter()
            .map(|b| ObligationBorrow {
                reserve: b.borrow_reserve,
                borrowed_wads: WadAmount::new(b.borrowed_amount_wads),
            })
            .collect(),
        freshness: ComponentFreshness {
            observed_slot: obligation_fetched_at_slot,
        },
    };

    let (reserves_out, oracles_out) = build_reserves_and_oracles(&reserves, &oracles);

    Ok(LendingSnapshot {
        protocol_tag: ProtocolTag::Solend,
        session_wallet,
        fetched_at: SnapshotFreshness {
            observed_slot: snapshot_observed_slot,
        },
        obligation,
        reserves: reserves_out,
        oracles: oracles_out,
    })
}

fn find_reserve<'a>(
    reserves: &'a [ReserveInput],
    target: &Pubkey,
) -> Result<&'a ReserveInput, MappingError> {
    reserves
        .iter()
        .find(|r| r.pubkey == *target)
        .ok_or(MappingError::ObligationReserveMissing { reserve: *target })
}

fn stale_to_marker(stale: bool) -> StaleMarker {
    if stale {
        StaleMarker::Stale
    } else {
        StaleMarker::Fresh
    }
}

/// Pure helper: build the normalized `Reserve` + `OracleFeedSet` lists
/// from the caller-supplied `ReserveInput`s and `OracleAccountInfo`s.
/// Shared by `map_snapshot` and `map_snapshot_for_first_deposit`.
fn build_reserves_and_oracles(
    reserves: &[ReserveInput],
    oracles: &[OracleAccountInfo],
) -> (Vec<Reserve>, Vec<OracleFeedSet>) {
    let reserves_out: Vec<Reserve> = reserves
        .iter()
        .map(|r| Reserve {
            identifier: r.pubkey,
            mint: r.raw.liquidity_mint,
            mint_decimals: r.raw.liquidity_mint_decimals,
            protocol_last_updated_slot: ChainSlot::new(r.raw.last_update_slot),
            protocol_native_stale: stale_to_marker(r.raw.last_update_stale),
            lifecycle: ReserveLifecycleMarkers::default(),
            freshness: ComponentFreshness {
                observed_slot: r.fetched_at_slot,
            },
            liquidity_supply_pubkey: r.raw.liquidity_supply,
            collateral_mint_pubkey: r.raw.collateral_mint,
            collateral_supply_pubkey: r.raw.collateral_supply,
            available_underlying: UnderlyingAmount::new(r.raw.liquidity_available_amount),
        })
        .collect();

    let oracles_out: Vec<OracleFeedSet> = reserves
        .iter()
        .map(|r| {
            let mut feeds: Vec<OracleFeed> = Vec::new();
            for slot_pubkey in [r.raw.pyth_oracle, r.raw.switchboard_oracle] {
                if raw::is_null_oracle_sentinel(&slot_pubkey) {
                    continue;
                }
                if let Some(probe) = oracles.iter().find(|o| o.pubkey == slot_pubkey) {
                    feeds.push(OracleFeed {
                        identifier: probe.pubkey,
                        provider: classify_oracle_provider(&probe.owner_program),
                        publish: probe.publish,
                        fetch: ComponentFreshness {
                            observed_slot: probe.fetched_at_slot,
                        },
                    });
                }
                // If the slot is a non-sentinel pubkey but not present in
                // `oracles`, the caller did not fetch it. The feed simply
                // does not surface — `MaxOracleStalenessMs` will HardBlock
                // when the priced asset's feed set ends up empty. This is
                // fail-closed by construction.
            }
            OracleFeedSet {
                priced_asset: r.raw.liquidity_mint,
                feeds,
            }
        })
        .collect();

    (reserves_out, oracles_out)
}

/// Slice 3A first-deposit mapping. Pure function over caller-supplied,
/// already-fetched / already-decoded inputs.
///
/// Produces a regular [`LendingSnapshot`] with:
/// - `obligation.deposits` empty
/// - `obligation.borrows` empty
/// - `obligation.protocol_native_stale = Fresh` (no on-chain obligation
///   state exists yet — there is nothing to be stale about; the deposit
///   ix downstream creates the obligation)
/// - `obligation.protocol_last_updated_slot = ChainSlot::new(0)`
///   (no protocol-side update has happened)
/// - `reserves` = caller's `target_reserves`
/// - `oracles` = one [`OracleFeedSet`] per target reserve, sentinels
///   excluded
///
/// Per-reserve `protocol_native_stale` markers are taken verbatim from
/// the decoded reserve raw — the reserve IS on chain and may be stale.
/// `RequireFreshState` will HardBlock those reserves the same way it
/// would for a non-first-deposit path. The caller is responsible for
/// the §66 refresh precondition before re-fetching reserves.
///
/// Caller invariants (validated by this function):
/// - Every reserve in `target_reserves` reports a `lending_market`
///   matching `lending_market`. Mismatch ⇒
///   [`MappingError::ReserveMarketMismatch`].
///
/// Caller invariants (NOT validated; caller's responsibility):
/// - `obligation_pubkey` is the correct PDA / target obligation key
///   for the (`session_wallet`, `lending_market`) pair, since this
///   function does not derive PDAs.
pub fn map_snapshot_for_first_deposit(
    inputs: FirstDepositAssemblyInputs,
) -> Result<LendingSnapshot, MappingError> {
    let FirstDepositAssemblyInputs {
        session_wallet,
        obligation_pubkey,
        lending_market,
        target_reserves,
        oracles,
        snapshot_observed_slot,
    } = inputs;

    for r in &target_reserves {
        if r.raw.lending_market != lending_market {
            return Err(MappingError::ReserveMarketMismatch { reserve: r.pubkey });
        }
    }

    let obligation = Obligation {
        identifier: obligation_pubkey,
        owner: session_wallet,
        protocol_last_updated_slot: ChainSlot::new(0),
        // No on-chain obligation state exists yet → no protocol-native
        // stale state to surface. Per-reserve stale markers below still
        // apply normally and are NOT touched by this synthesis.
        protocol_native_stale: StaleMarker::Fresh,
        deposits: Vec::new(),
        borrows: Vec::new(),
        freshness: ComponentFreshness {
            observed_slot: snapshot_observed_slot,
        },
    };

    let (reserves_out, oracles_out) =
        build_reserves_and_oracles(&target_reserves, &oracles);

    Ok(LendingSnapshot {
        protocol_tag: ProtocolTag::Solend,
        session_wallet,
        fetched_at: SnapshotFreshness {
            observed_slot: snapshot_observed_slot,
        },
        obligation,
        reserves: reserves_out,
        oracles: oracles_out,
    })
}

/// Classify an oracle by its owner program. Known programs are labeled;
/// unknown programs surface as [`OracleProvider::Unknown`] — rules can
/// still evaluate freshness, but any provider-specific handling sees the
/// feed as opt-out.
///
/// Known owner programs (from spike §30.5):
/// - Pyth Solana Receiver: `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`
/// - Switchboard On-Demand: `SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f`
pub fn classify_oracle_provider(owner: &Pubkey) -> OracleProvider {
    match owner.to_string().as_str() {
        "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ" => OracleProvider::PythSolanaReceiver,
        "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f" => OracleProvider::SwitchboardOnDemand,
        _ => OracleProvider::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::super::raw::{synth_obligation, synth_reserve, SOLEND_NULL_ORACLE_SENTINEL_BS58};
    use super::*;
    use crate::lending::check_system_invariants;
    use crate::lending::SystemInvariantError;

    fn parse_sentinel() -> Pubkey {
        SOLEND_NULL_ORACLE_SENTINEL_BS58
            .parse()
            .expect("sentinel parses")
    }

    #[test]
    fn happy_path_deposit_only_snapshot() {
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let c_mint = Pubkey::new_unique();
        let c_supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let swb_owner_program: Pubkey = "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f"
            .parse()
            .unwrap();
        let pyth_owner_program: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();
        let swb = Pubkey::new_unique();

        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            6,
            supply,
            pyth,
            swb,
            1_000_000_000,
            c_mint,
            c_supply,
            200,
            false,
        );

        let obligation_raw = raw::decode_obligation(&obligation_bytes).unwrap();
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();

        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw,
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(301),
            }],
            oracles: vec![
                OracleAccountInfo {
                    pubkey: pyth,
                    owner_program: pyth_owner_program,
                    fetched_at_slot: ChainSlot::new(302),
                    publish: FeedPublishFreshness::Unknown,
                },
                OracleAccountInfo {
                    pubkey: swb,
                    owner_program: swb_owner_program,
                    fetched_at_slot: ChainSlot::new(303),
                    publish: FeedPublishFreshness::Unknown,
                },
            ],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        let snap = map_snapshot(inputs).expect("mapping succeeds");

        assert_eq!(snap.protocol_tag, ProtocolTag::Solend);
        assert_eq!(snap.obligation.owner, owner);
        assert_eq!(snap.obligation.protocol_native_stale, StaleMarker::Fresh);
        assert_eq!(snap.reserves.len(), 1);
        assert_eq!(snap.reserves[0].mint, mint);
        assert_eq!(snap.reserves[0].available_underlying.raw(), 1_000_000_000);
        assert_eq!(snap.oracles.len(), 1);
        assert_eq!(snap.oracles[0].feeds.len(), 2);
        assert_eq!(
            snap.oracles[0].feeds[0].provider,
            OracleProvider::PythSolanaReceiver
        );
        assert_eq!(
            snap.oracles[0].feeds[1].provider,
            OracleProvider::SwitchboardOnDemand
        );

        // Gate check: owner matches session wallet.
        assert!(
            check_system_invariants(&snap, &owner, ProtocolTag::Solend).is_ok()
        );
    }

    #[test]
    fn sentinel_pyth_slot_excluded_from_feed_set() {
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let sentinel = parse_sentinel();
        let swb = Pubkey::new_unique();
        let swb_owner: Pubkey = "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f"
            .parse()
            .unwrap();

        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            sentinel, // pyth slot sentinel
            swb,
            123_456,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );

        let obligation_raw = raw::decode_obligation(&obligation_bytes).unwrap();
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();

        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw,
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(301),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: swb,
                owner_program: swb_owner,
                fetched_at_slot: ChainSlot::new(302),
                publish: FeedPublishFreshness::Unknown,
            }],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        let snap = map_snapshot(inputs).expect("mapping succeeds");
        assert_eq!(snap.oracles.len(), 1);
        // Only the switchboard feed surfaces; the sentinel pyth slot is excluded.
        assert_eq!(snap.oracles[0].feeds.len(), 1);
        assert_eq!(
            snap.oracles[0].feeds[0].provider,
            OracleProvider::SwitchboardOnDemand
        );
    }

    #[test]
    fn both_oracle_slots_sentinel_produces_empty_feed_set() {
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let sentinel = parse_sentinel();
        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let reserve_bytes = synth_reserve(
            mkt,
            Pubkey::new_unique(),
            6,
            Pubkey::new_unique(),
            sentinel,
            sentinel,
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );

        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![ReserveInput {
                pubkey: Pubkey::new_unique(),
                raw: raw::decode_reserve(&reserve_bytes).unwrap(),
                fetched_at_slot: ChainSlot::new(301),
            }],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        let snap = map_snapshot(inputs).expect("mapping succeeds");
        assert_eq!(snap.oracles.len(), 1);
        assert!(snap.oracles[0].feeds.is_empty());
    }

    #[test]
    fn obligation_referencing_missing_reserve_fails() {
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let missing_reserve = Pubkey::new_unique();
        let deposits = vec![
            super::super::raw::SolendObligationCollateralRaw {
                deposit_reserve: missing_reserve,
                deposited_amount: 1,
            },
        ];
        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &deposits, &[]);
        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![], // reserve not provided
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        assert_eq!(
            map_snapshot(inputs).unwrap_err(),
            MappingError::ObligationReserveMissing {
                reserve: missing_reserve
            }
        );
    }

    #[test]
    fn reserve_market_mismatch_fails() {
        let obligation_market = Pubkey::new_unique();
        let other_market = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();

        let obligation_bytes = synth_obligation(owner, obligation_market, 100, false, &[], &[]);
        // reserve claims a different lending_market:
        let reserve_bytes = synth_reserve(
            other_market,
            Pubkey::new_unique(),
            6,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );

        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&reserve_bytes).unwrap(),
                fetched_at_slot: ChainSlot::new(301),
            }],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        assert_eq!(
            map_snapshot(inputs).unwrap_err(),
            MappingError::ReserveMarketMismatch { reserve: reserve_pk }
        );
    }

    #[test]
    fn owner_mismatch_caught_by_gate() {
        let mkt = Pubkey::new_unique();
        let obligation_owner = Pubkey::new_unique();
        let wrong_session_wallet = Pubkey::new_unique();
        let obligation_bytes =
            synth_obligation(obligation_owner, mkt, 100, false, &[], &[]);
        let inputs = SolendAssemblyInputs {
            session_wallet: wrong_session_wallet,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };
        let snap = map_snapshot(inputs).expect("mapping succeeds");
        let err = check_system_invariants(&snap, &wrong_session_wallet, ProtocolTag::Solend)
            .unwrap_err();
        match err {
            SystemInvariantError::OwnerMismatch {
                snapshot_owner,
                session_wallet,
            } => {
                assert_eq!(snapshot_owner, obligation_owner);
                assert_eq!(session_wallet, wrong_session_wallet);
            }
            other => panic!("expected OwnerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn unit_tags_are_carried_end_to_end() {
        // Build an obligation with one deposit and one borrow; confirm
        // the mapping wraps amounts in the correct newtypes.
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let sentinel = parse_sentinel();

        let deposits = vec![super::super::raw::SolendObligationCollateralRaw {
            deposit_reserve: reserve_pk,
            deposited_amount: 12_457,
        }];
        let borrows = vec![super::super::raw::SolendObligationLiquidityRaw {
            borrow_reserve: reserve_pk,
            borrowed_amount_wads: 56_641_326_427_023_044_032_452u128,
        }];
        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &deposits, &borrows);
        let reserve_bytes = synth_reserve(
            mkt,
            mint,
            6,
            Pubkey::new_unique(),
            sentinel,
            sentinel,
            9_999_999_999,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );

        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: raw::decode_reserve(&reserve_bytes).unwrap(),
                fetched_at_slot: ChainSlot::new(301),
            }],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };

        let snap = map_snapshot(inputs).unwrap();
        let dep: CollateralTokenAmount = snap.obligation.deposits[0].deposited;
        let bor: WadAmount = snap.obligation.borrows[0].borrowed_wads;
        let avail: UnderlyingAmount = snap.reserves[0].available_underlying;
        assert_eq!(dep.raw(), 12_457);
        assert_eq!(bor.raw(), 56_641_326_427_023_044_032_452u128);
        assert_eq!(avail.raw(), 9_999_999_999);
    }

    #[test]
    fn protocol_tag_mismatch_in_gate_is_possible() {
        // There is only one V1 ProtocolTag variant today (Solend), so this
        // test documents the gate's shape — if a future variant is added
        // without updating gate call-sites, this test pattern catches it.
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let obligation_bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        let inputs = SolendAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            obligation_raw: raw::decode_obligation(&obligation_bytes).unwrap(),
            obligation_fetched_at_slot: ChainSlot::new(300),
            reserves: vec![],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(305),
        };
        let snap = map_snapshot(inputs).unwrap();
        // Positive-shape assertion: Solend tag passes.
        assert!(check_system_invariants(&snap, &owner, ProtocolTag::Solend).is_ok());
    }

    // ── Slice 3A: first-deposit mapping tests ─────────────────────────────

    #[test]
    fn first_deposit_snapshot_includes_target_reserve_even_without_obligation_ref() {
        let mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let target_reserve_pk = Pubkey::new_unique();
        let target_mint = Pubkey::new_unique();
        let sentinel = parse_sentinel();
        let pyth = Pubkey::new_unique();
        let pyth_owner: Pubkey = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
            .parse()
            .unwrap();

        let reserve_bytes = synth_reserve(
            mkt,
            target_mint,
            9,
            Pubkey::new_unique(),
            pyth,
            sentinel,
            1_000_000_000,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();

        let inputs = FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![ReserveInput {
                pubkey: target_reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(300),
            }],
            oracles: vec![OracleAccountInfo {
                pubkey: pyth,
                owner_program: pyth_owner,
                fetched_at_slot: ChainSlot::new(300),
                publish: FeedPublishFreshness::KnownSlot(ChainSlot::new(300)),
            }],
            snapshot_observed_slot: ChainSlot::new(300),
        };
        let snap = map_snapshot_for_first_deposit(inputs).expect("first-deposit mapping succeeds");
        // The target reserve is in the snapshot even though the obligation
        // is empty.
        assert_eq!(snap.reserves.len(), 1);
        assert_eq!(snap.reserves[0].mint, target_mint);
        // Obligation is empty but the shape is a regular Obligation.
        assert!(snap.obligation.deposits.is_empty());
        assert!(snap.obligation.borrows.is_empty());
        assert_eq!(snap.obligation.owner, owner);
        assert_eq!(snap.obligation.protocol_native_stale, StaleMarker::Fresh);
        // Oracle feed set for the target mint is present with the Pyth feed
        // — sentinel excluded.
        assert_eq!(snap.oracles.len(), 1);
        assert_eq!(snap.oracles[0].priced_asset, target_mint);
        assert_eq!(snap.oracles[0].feeds.len(), 1);
        assert_eq!(
            snap.oracles[0].feeds[0].provider,
            OracleProvider::PythSolanaReceiver
        );
    }

    #[test]
    fn first_deposit_snapshot_without_target_reserve_has_empty_reserves_and_oracles() {
        // Negative test: an empty first-deposit snapshot that does NOT
        // include the intended deposit target reserve is not oracle-passable
        // by construction. The mapping produces empty `reserves` and empty
        // `oracles`, and when the action tries to evaluate
        // `MaxOracleStalenessMs` for a mint that has no feed set, the rule
        // HardBlocks on `OracleFeedSetEmpty`. This test documents the
        // mapping shape; the policy HardBlock is covered in the policy
        // tests.
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let inputs = FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![], // caller did NOT include target reserve
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(300),
        };
        let snap = map_snapshot_for_first_deposit(inputs).unwrap();
        assert!(snap.reserves.is_empty());
        assert!(snap.oracles.is_empty());
        // Obligation is still well-formed and empty.
        assert!(snap.obligation.deposits.is_empty());
        assert!(snap.obligation.borrows.is_empty());
    }

    #[test]
    fn first_deposit_reserve_market_mismatch_fails() {
        let mkt = Pubkey::new_unique();
        let wrong_mkt = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let reserve_pk = Pubkey::new_unique();
        let reserve_bytes = synth_reserve(
            wrong_mkt, // reserve is for a different lending market
            Pubkey::new_unique(),
            9,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            0,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            200,
            false,
        );
        let reserve_raw = raw::decode_reserve(&reserve_bytes).unwrap();
        let inputs = FirstDepositAssemblyInputs {
            session_wallet: owner,
            obligation_pubkey: Pubkey::new_unique(),
            lending_market: mkt,
            target_reserves: vec![ReserveInput {
                pubkey: reserve_pk,
                raw: reserve_raw,
                fetched_at_slot: ChainSlot::new(300),
            }],
            oracles: vec![],
            snapshot_observed_slot: ChainSlot::new(300),
        };
        assert_eq!(
            map_snapshot_for_first_deposit(inputs).unwrap_err(),
            MappingError::ReserveMarketMismatch { reserve: reserve_pk }
        );
    }

    #[test]
    fn first_deposit_path_does_not_weaken_raw_decode_obligation() {
        // The strict raw decoder rejects empty / wrong-size data. This
        // test documents that the first-deposit path does NOT route
        // uninitialized bytes through `decode_obligation` — it uses the
        // `map_snapshot_for_first_deposit` seam instead.
        let empty: &[u8] = &[];
        assert!(raw::decode_obligation(empty).is_err());
        let wrong_size: Vec<u8> = vec![0u8; 10];
        assert!(raw::decode_obligation(&wrong_size).is_err());
        // The strict decoder also rejects valid-size bytes with non-zero
        // padding (existing test `obligation_padding_nonzero_rejected`
        // covers this); this test is concerned only with the seam
        // discipline.
    }
}
