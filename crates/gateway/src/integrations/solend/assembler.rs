//! Solend snapshot assembler — the ONLY Phase 4B-1 component that owns RPC.
//!
//! # What this module owns
//!
//! This module is the anti-corruption seam that turns read-only on-chain
//! Solana account bytes into the protocol-agnostic [`LendingSnapshot`]
//! plus the execution-planning facts Phase 4C will need to compose the
//! Slice 3G multi-tx sequence (`setup obligation → refresh → deposit`).
//!
//! Spec anchors:
//! - `docs/lending_policy_vocabulary.md` Parts 5A §40 – §42 (assembler
//!   seam: RPC is allowed here; it is NOT allowed in `lending::*`).
//! - Part 6B §62 – §63 (Solend-shaped types terminate at this subtree).
//! - Part 6B §66 (refresh / first-deposit precondition belongs to outer
//!   wiring — NOT to the evaluator).
//!
//! # What this module deliberately does NOT do (Phase 4B-1)
//!
//! - Does NOT construct transactions. No blockhash, no ALT, no
//!   `VersionedTransaction`, no `Transaction::new`.
//! - Does NOT call any Solend instruction builder (`refresh`, `deposit`,
//!   `init_obligation`, or the ATA-create ix). It derives the ATA
//!   *address* (pure math) but does NOT emit a create ix.
//! - Does NOT simulate. Does NOT broadcast. Does NOT sign.
//! - Does NOT import approval / park / signer / orchestrator / daemon
//!   state.
//! - Does NOT call [`crate::lending::evaluate_lending_policy`] — policy
//!   evaluation is the caller's (Phase 4B-2's) concern.
//!
//! # V1 supported target
//!
//! V1 only supports the mainnet USDC Solend target proven in Slice 3G.
//! Any other `reserve_mint` fails closed with
//! [`SolendAssemblyError::UnsupportedReserveMint`]. No broad reserve
//! scan is performed — the mint-to-reserve lookup is a small in-crate
//! allowlist.
//!
//! # Fetch abstraction
//!
//! All RPC goes through [`SolanaAccountFetcher`]. The trait is the only
//! surface this module uses for network IO; unit tests inject a stub
//! implementation, so no unit test needs network access. A production
//! adapter that wraps `claw_solana_core::rpc::client::ClawRpcClient` is
//! out of scope for this slice — it will land in Phase 4B-2 when the
//! tool path wires the assembler into the daemon.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

use crate::lending::{ChainSlot, LendingSnapshot};

use super::ata::derive_associated_token_address;
use super::deposit::SPL_TOKEN_PROGRAM_ID_BS58;
use super::mapping::{
    map_snapshot, map_snapshot_for_first_deposit, FirstDepositAssemblyInputs, MappingError,
    OracleAccountInfo, ReserveInput, SolendAssemblyInputs,
};
use super::oracle_decoder::extract_publish_freshness;
use super::raw::{
    decode_obligation, decode_reserve, is_null_oracle_sentinel, DecodeError,
    SOLEND_PROGRAM_ID_BS58,
};

// ── V1 supported target (Slice 3G / 3H proven) ──────────────────────────────

/// Mainnet USDC mint (6 decimals).
pub const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Solend Save Main Pool USDC reserve (V1 target; Slice 3G live-proven).
pub const USDC_RESERVE_BS58: &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";

/// Pyth Solana Receiver program id — owner of `PriceUpdateV2` accounts.
/// Matches the classification in
/// `integrations::solend::mapping::classify_oracle_provider`.
pub const PYTH_SOLANA_RECEIVER_PROGRAM_BS58: &str =
    "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

/// SPL classic Token Account layout size. Per the SPL Token program
/// `Account::LEN` constant. Parsed fields at fixed offsets:
///   0..32  mint   (Pubkey)
///  32..64  owner  (Pubkey)
///  64..72  amount (u64 little-endian)  — read only for future use; not validated here
const SPL_TOKEN_ACCOUNT_LEN: usize = 165;

// ── ATA validation surface ──────────────────────────────────────────────────

/// Which of the two ATAs an error or validation refers to. Embedded in
/// every `InvalidTokenAccount*` variant so the caller can distinguish a
/// source-ATA failure (user's underlying funds) from a collateral-ATA
/// failure (reserve cToken destination) without string-matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtaKind {
    /// Source liquidity ATA — holds the user's underlying (e.g. USDC).
    Source,
    /// User collateral ATA — holds the reserve's cToken (e.g. cUSDC).
    Collateral,
}

// ── Fetch abstraction ───────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("RPC fetch failed: {reason}")]
    RpcFailure { reason: String },
}

/// Minimal read-only Solana RPC surface the assembler depends on.
///
/// `get_account` returns `Ok(None)` when the account does not exist,
/// `Ok(Some(a))` when it exists. The distinction is load-bearing: the
/// assembler uses "obligation account does not exist" to select the
/// first-deposit mapping path. Collapsing absence into an error would
/// break that discrimination.
///
/// Implementations must be safe to `Arc`-share across tasks.
#[async_trait]
pub trait SolanaAccountFetcher: Send + Sync {
    async fn get_account(
        &self,
        pubkey: &Pubkey,
    ) -> Result<Option<Account>, FetchError>;

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, FetchError>;

    async fn get_slot(&self) -> Result<u64, FetchError>;
}

// ── Error surface ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SolendAssemblyError {
    #[error("reserve mint {mint} is not supported by the V1 Solend assembler \
             (only USDC {usdc} is supported)")]
    UnsupportedReserveMint { mint: Pubkey, usdc: Pubkey },

    #[error("reserve account {reserve} not found on chain")]
    ReserveAccountMissing { reserve: Pubkey },

    #[error("reserve account {reserve} decode failed: {source}")]
    ReserveDecodeFailed {
        reserve: Pubkey,
        #[source]
        source: DecodeError,
    },

    #[error("obligation account {obligation} decode failed: {source}")]
    ObligationDecodeFailed {
        obligation: Pubkey,
        #[source]
        source: DecodeError,
    },

    #[error("obligation account {obligation} exists but is owned by {actual_owner} \
             (expected Solend program {expected_owner})")]
    ObligationAccountOwnerMismatch {
        obligation: Pubkey,
        actual_owner: Pubkey,
        expected_owner: Pubkey,
    },

    #[error("oracle account {oracle} not found on chain")]
    OracleAccountMissing { oracle: Pubkey },

    #[error("account {account} is owned by {actual_owner} but the assembler \
             expected owner {expected_owner}")]
    InvalidAccountOwner {
        account: Pubkey,
        actual_owner: Pubkey,
        expected_owner: Pubkey,
    },

    #[error("RPC fetch failed while assembling Solend snapshot: {reason}")]
    RpcFetchFailed { reason: String },

    #[error("first-deposit snapshot mapping failed: {source}")]
    FirstDepositMappingFailed {
        #[source]
        source: MappingError,
    },

    #[error("snapshot mapping failed: {source}")]
    SnapshotMappingFailed {
        #[source]
        source: MappingError,
    },

    // ── ATA validation errors (4B-1a) ───────────────────────────────────────
    //
    // An account exists at the derived ATA pubkey but fails one of the
    // four SPL-Token-Account checks: owner program, data length, mint
    // field, or owner field. Each carries `ata_kind` so a caller can
    // tell source-vs-collateral without string-matching.
    #[error("{ata_kind:?} ATA account is owned by {actual_owner} but must be \
             owned by the classic SPL Token program {expected_owner}")]
    InvalidTokenAccountOwnerProgram {
        ata_kind: AtaKind,
        actual_owner: Pubkey,
        expected_owner: Pubkey,
    },

    #[error("{ata_kind:?} ATA account has data length {actual_len} but the \
             classic SPL Token Account layout requires {expected_len}")]
    InvalidTokenAccountDataLen {
        ata_kind: AtaKind,
        actual_len: usize,
        expected_len: usize,
    },

    #[error("{ata_kind:?} ATA account mint field is {actual_mint} but the \
             assembler expected {expected_mint}")]
    InvalidTokenAccountMint {
        ata_kind: AtaKind,
        actual_mint: Pubkey,
        expected_mint: Pubkey,
    },

    #[error("{ata_kind:?} ATA account owner field is {actual_owner} but the \
             assembler expected {expected_owner} (session-bound wallet)")]
    InvalidTokenAccountOwner {
        ata_kind: AtaKind,
        actual_owner: Pubkey,
        expected_owner: Pubkey,
    },

    // ── Pyth oracle owner mismatch (4B-1a) ──────────────────────────────────
    //
    // The reserve's Pyth oracle pubkey was non-sentinel, so we fetched it;
    // the account exists but is NOT owned by the Pyth Solana Receiver
    // program. Fail closed rather than passing the bytes to the Pyth
    // decoder (which would return `Unknown`) — an explicit typed error
    // gives a clearer diagnostic than silently downgrading freshness.
    #[error("Pyth oracle account {oracle} is owned by {actual_owner} but must \
             be owned by the Pyth Solana Receiver program {expected_owner}")]
    PythOracleOwnerMismatch {
        oracle: Pubkey,
        actual_owner: Pubkey,
        expected_owner: Pubkey,
    },
}

impl From<FetchError> for SolendAssemblyError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::RpcFailure { reason } => SolendAssemblyError::RpcFetchFailed { reason },
        }
    }
}

// ── Output type ─────────────────────────────────────────────────────────────

/// Assembled read-model + execution-planning facts for a Solend deposit.
///
/// The `snapshot` is ready to be fed to
/// [`crate::lending::evaluate_lending_policy`] by the caller (Phase 4B-2);
/// the `*_exists` booleans and ATA / reserve / obligation pubkeys are the
/// extra facts Phase 4C will need to compose the Slice 3G multi-tx
/// sequence. This slice does NOT emit any instruction — the builder seam
/// at `integrations::solend::{refresh, deposit, init_obligation, ata}`
/// is not touched here.
#[derive(Debug, Clone)]
pub struct AssembledSolendDepositSnapshot {
    pub snapshot: LendingSnapshot,
    pub obligation_exists: bool,
    pub source_ata_exists: bool,
    pub collateral_ata_exists: bool,
    pub reserve_pubkey: Pubkey,
    pub obligation_pubkey: Pubkey,
    pub source_liquidity_ata: Pubkey,
    pub user_collateral_ata: Pubkey,
    /// Solend `lending_market` pubkey, decoded from the target reserve's
    /// raw bytes at assembly time. Surfaced here (not on
    /// [`crate::lending::LendingSnapshot`]) because the market pubkey is a
    /// Solend-specific execution-planning fact: Phase 4C-2's transaction
    /// plan assembler needs it for the refresh/init-obligation/deposit
    /// builders, but `LendingSnapshot` deliberately stays
    /// protocol-neutral (Part 6B §63 anti-corruption seam).
    pub lending_market: Pubkey,
}

// ── Assembler ───────────────────────────────────────────────────────────────

/// Stateless Solend snapshot assembler.
///
/// The only stored state is the fetcher Arc — no cache, no mutable
/// state, no ambient context, no approval / park / daemon handle.
pub struct SolendSnapshotAssembler {
    fetcher: Arc<dyn SolanaAccountFetcher>,
}

impl SolendSnapshotAssembler {
    pub fn new(fetcher: Arc<dyn SolanaAccountFetcher>) -> Self {
        Self { fetcher }
    }

    /// Assemble a read-model snapshot + execution-planning facts for a
    /// deposit by `session_wallet` of the given `reserve_mint`.
    ///
    /// `obligation_pubkey` is passed in by the caller (Phase 4B-2 will
    /// source it from config / session binding). The assembler does not
    /// invent obligation identity — Slice 3G's proven model is that the
    /// obligation pubkey is a caller-supplied keypair-backed address,
    /// not a PDA. The assembler simply reads it.
    ///
    /// All Solend-shaped raw types terminate inside this function. The
    /// returned [`LendingSnapshot`] and pubkeys are provider-agnostic.
    pub async fn assemble_for_deposit(
        &self,
        session_wallet: Pubkey,
        reserve_mint: Pubkey,
        obligation_pubkey: Pubkey,
    ) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
        // ── 1. V1 mint allowlist ────────────────────────────────────────────
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58)
            .expect("compile-time USDC mint constant is valid");
        if reserve_mint != usdc_mint {
            return Err(SolendAssemblyError::UnsupportedReserveMint {
                mint: reserve_mint,
                usdc: usdc_mint,
            });
        }
        let reserve_pubkey = Pubkey::from_str(USDC_RESERVE_BS58)
            .expect("compile-time USDC reserve constant is valid");
        let solend_program_id = Pubkey::from_str(SOLEND_PROGRAM_ID_BS58)
            .expect("compile-time Solend program id constant is valid");
        let spl_token_program = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58)
            .expect("compile-time SPL Token program id constant is valid");

        // ── 2. Source liquidity ATA (pure derivation — no RPC) ──────────────
        let source_liquidity_ata =
            derive_associated_token_address(&session_wallet, &reserve_mint, &spl_token_program);

        // ── 3. Snapshot-observed slot (single fetch, shared by all components) ─
        let snapshot_slot_raw = self.fetcher.get_slot().await?;
        let snapshot_observed_slot = ChainSlot::new(snapshot_slot_raw);

        // ── 4. Reserve fetch + ownership check + decode ─────────────────────
        let reserve_account = self
            .fetcher
            .get_account(&reserve_pubkey)
            .await?
            .ok_or(SolendAssemblyError::ReserveAccountMissing {
                reserve: reserve_pubkey,
            })?;

        if reserve_account.owner != solend_program_id {
            return Err(SolendAssemblyError::InvalidAccountOwner {
                account: reserve_pubkey,
                actual_owner: reserve_account.owner,
                expected_owner: solend_program_id,
            });
        }

        let reserve_raw =
            decode_reserve(&reserve_account.data).map_err(|source| {
                SolendAssemblyError::ReserveDecodeFailed {
                    reserve: reserve_pubkey,
                    source,
                }
            })?;

        // Derive collateral ATA now that we know the cToken mint.
        let user_collateral_ata = derive_associated_token_address(
            &session_wallet,
            &reserve_raw.collateral_mint,
            &spl_token_program,
        );

        // ── 5. Batch-fetch downstream accounts ──────────────────────────────
        //
        // Batch layout:
        //   [0] obligation
        //   [1] source_liquidity_ata
        //   [2] user_collateral_ata
        //   [pyth_idx?]  pyth_oracle          — only if non-sentinel
        //   [sb_idx?]    switchboard_oracle   — only if non-sentinel
        //
        // Both oracle slots are sentinel-gated. The stub fetcher in unit
        // tests panics if a sentinel pubkey is ever queried (tests G and L).
        let pyth_is_sentinel = is_null_oracle_sentinel(&reserve_raw.pyth_oracle);
        let switchboard_is_sentinel =
            is_null_oracle_sentinel(&reserve_raw.switchboard_oracle);

        let mut batch_pks: Vec<Pubkey> = vec![
            obligation_pubkey,
            source_liquidity_ata,
            user_collateral_ata,
        ];
        let pyth_idx = if !pyth_is_sentinel {
            let i = batch_pks.len();
            batch_pks.push(reserve_raw.pyth_oracle);
            Some(i)
        } else {
            None
        };
        let switchboard_idx = if !switchboard_is_sentinel {
            let i = batch_pks.len();
            batch_pks.push(reserve_raw.switchboard_oracle);
            Some(i)
        } else {
            None
        };

        let batch_accounts = self.fetcher.get_multiple_accounts(&batch_pks).await?;
        if batch_accounts.len() != batch_pks.len() {
            return Err(SolendAssemblyError::RpcFetchFailed {
                reason: format!(
                    "get_multiple_accounts returned {} items for {} pubkeys",
                    batch_accounts.len(),
                    batch_pks.len()
                ),
            });
        }
        let obligation_account_opt = batch_accounts[0].clone();
        let source_ata_account_opt = batch_accounts[1].clone();
        let collateral_ata_account_opt = batch_accounts[2].clone();
        let pyth_account_opt = pyth_idx.and_then(|i| batch_accounts[i].clone());
        let switchboard_account_opt = switchboard_idx.and_then(|i| batch_accounts[i].clone());

        // ── 6. Pyth oracle — required when non-sentinel; validate owner ────
        //
        // Null/sentinel Pyth: skip fetching, skip validation, do not
        // surface a feed. The mapping layer's per-slot sentinel guard
        // (see `build_reserves_and_oracles`) also drops it, and policy
        // HardBlocks on zero feeds. No assembler bypass.
        //
        // Non-sentinel Pyth: account must exist AND be owned by the Pyth
        // Solana Receiver program. Ownership is validated BEFORE
        // `extract_publish_freshness`, so a foreign-owned account cannot
        // silently downgrade to `Unknown` — it produces a typed
        // `PythOracleOwnerMismatch` error instead.
        let pyth_receiver_program = Pubkey::from_str(PYTH_SOLANA_RECEIVER_PROGRAM_BS58)
            .expect("compile-time Pyth Receiver constant is valid");

        let mut oracles: Vec<OracleAccountInfo> = Vec::new();
        if !pyth_is_sentinel {
            let pyth_account = pyth_account_opt.ok_or(
                SolendAssemblyError::OracleAccountMissing {
                    oracle: reserve_raw.pyth_oracle,
                },
            )?;
            if pyth_account.owner != pyth_receiver_program {
                return Err(SolendAssemblyError::PythOracleOwnerMismatch {
                    oracle: reserve_raw.pyth_oracle,
                    actual_owner: pyth_account.owner,
                    expected_owner: pyth_receiver_program,
                });
            }
            let pyth_freshness =
                extract_publish_freshness(&pyth_account.owner, &pyth_account.data);
            oracles.push(OracleAccountInfo {
                pubkey: reserve_raw.pyth_oracle,
                owner_program: pyth_account.owner,
                fetched_at_slot: snapshot_observed_slot,
                publish: pyth_freshness,
            });
        }

        // ── 7. Switchboard oracle — only for non-sentinel targets ───────────
        //
        // V1 USDC's Switchboard is sentinel, so this branch is never
        // taken in the happy path. For future non-sentinel Switchboard
        // targets, `extract_publish_freshness` returns `Unknown` (the
        // current Switchboard decoder is unverified, per Part 6B §65),
        // and `MaxOracleStalenessMs` HardBlocks `Unknown`. No bypass.
        if !switchboard_is_sentinel {
            let sb_account = switchboard_account_opt.ok_or(
                SolendAssemblyError::OracleAccountMissing {
                    oracle: reserve_raw.switchboard_oracle,
                },
            )?;
            let sb_freshness =
                extract_publish_freshness(&sb_account.owner, &sb_account.data);
            oracles.push(OracleAccountInfo {
                pubkey: reserve_raw.switchboard_oracle,
                owner_program: sb_account.owner,
                fetched_at_slot: snapshot_observed_slot,
                publish: sb_freshness,
            });
        }

        // ── 8. Obligation handling ──────────────────────────────────────────
        let reserve_input = ReserveInput {
            pubkey: reserve_pubkey,
            raw: reserve_raw.clone(),
            fetched_at_slot: snapshot_observed_slot,
        };

        let (snapshot, obligation_exists) = match obligation_account_opt {
            // A. Obligation does not exist — first-deposit path.
            None => {
                let snapshot = map_snapshot_for_first_deposit(FirstDepositAssemblyInputs {
                    session_wallet,
                    obligation_pubkey,
                    lending_market: reserve_raw.lending_market,
                    target_reserves: vec![reserve_input],
                    oracles,
                    snapshot_observed_slot,
                })
                .map_err(|source| SolendAssemblyError::FirstDepositMappingFailed { source })?;
                (snapshot, false)
            }
            Some(account) => {
                // B. Obligation exists but wrong owner — fail closed.
                if account.owner != solend_program_id {
                    return Err(SolendAssemblyError::ObligationAccountOwnerMismatch {
                        obligation: obligation_pubkey,
                        actual_owner: account.owner,
                        expected_owner: solend_program_id,
                    });
                }
                // C. Obligation exists and is Solend-owned — decode + map.
                let obligation_raw = decode_obligation(&account.data).map_err(|source| {
                    SolendAssemblyError::ObligationDecodeFailed {
                        obligation: obligation_pubkey,
                        source,
                    }
                })?;
                let snapshot = map_snapshot(SolendAssemblyInputs {
                    session_wallet,
                    obligation_pubkey,
                    obligation_raw,
                    obligation_fetched_at_slot: snapshot_observed_slot,
                    reserves: vec![reserve_input],
                    oracles,
                    snapshot_observed_slot,
                })
                .map_err(|source| SolendAssemblyError::SnapshotMappingFailed { source })?;
                (snapshot, true)
            }
        };

        // ── 9. ATA validation (4B-1a) ───────────────────────────────────────
        //
        // A missing ATA is NOT an error — first-deposit / create-idempotent
        // paths expect it to be absent. But if an account is present at
        // the derived ATA pubkey, it must be a real, correctly-shaped SPL
        // Token Account for the right mint, owned by the session wallet.
        // Otherwise we fail closed with a typed `InvalidTokenAccount*`
        // variant that names `ata_kind` so callers can distinguish
        // source-vs-collateral without string matching.
        //
        // `ata_kind` + `expected_mint` + `expected_owner` differ per ATA:
        //   source:      mint = USDC (reserve liquidity mint),      owner = session_wallet
        //   collateral:  mint = reserve.collateral_mint (cToken),   owner = session_wallet
        let source_ata_exists = match source_ata_account_opt {
            None => false,
            Some(account) => {
                validate_spl_token_account(
                    &account,
                    AtaKind::Source,
                    reserve_raw.liquidity_mint,
                    session_wallet,
                    spl_token_program,
                )?;
                true
            }
        };
        let collateral_ata_exists = match collateral_ata_account_opt {
            None => false,
            Some(account) => {
                validate_spl_token_account(
                    &account,
                    AtaKind::Collateral,
                    reserve_raw.collateral_mint,
                    session_wallet,
                    spl_token_program,
                )?;
                true
            }
        };

        Ok(AssembledSolendDepositSnapshot {
            snapshot,
            obligation_exists,
            source_ata_exists,
            collateral_ata_exists,
            reserve_pubkey,
            obligation_pubkey,
            source_liquidity_ata,
            user_collateral_ata,
            lending_market: reserve_raw.lending_market,
        })
    }
}

// ── SPL Token Account validator (private) ───────────────────────────────────

/// Validate that `account` is a genuine classic SPL Token Account for
/// `expected_mint` owned by `expected_owner`. Performed only when an
/// account was found at the derived ATA pubkey; a missing ATA is not
/// validated here (the caller treats absence as `exists = false`).
///
/// Check order (fail-closed; earliest failure short-circuits):
///   1. Account's Solana owner program == classic SPL Token program.
///   2. Account data length == `SPL_TOKEN_ACCOUNT_LEN` (165).
///   3. Mint field at bytes 0..32 == `expected_mint`.
///   4. Token-account owner field at bytes 32..64 == `expected_owner`.
///
/// The length check (step 2) always runs BEFORE any byte slicing
/// (steps 3–4). Once step 2 succeeds, `data[0..32]` and `data[32..64]`
/// cannot panic: 165 > 64 covers both slices.
///
/// Amount (bytes 64..72) is intentionally NOT read or validated here —
/// balance checks belong to policy / execution preflight, not to this
/// slice.
fn validate_spl_token_account(
    account: &Account,
    ata_kind: AtaKind,
    expected_mint: Pubkey,
    expected_owner: Pubkey,
    expected_owner_program: Pubkey,
) -> Result<(), SolendAssemblyError> {
    // 1. Owner program
    if account.owner != expected_owner_program {
        return Err(SolendAssemblyError::InvalidTokenAccountOwnerProgram {
            ata_kind,
            actual_owner: account.owner,
            expected_owner: expected_owner_program,
        });
    }

    // 2. Data length — MUST precede any byte slicing.
    if account.data.len() != SPL_TOKEN_ACCOUNT_LEN {
        return Err(SolendAssemblyError::InvalidTokenAccountDataLen {
            ata_kind,
            actual_len: account.data.len(),
            expected_len: SPL_TOKEN_ACCOUNT_LEN,
        });
    }

    // 3. Mint field — safe because length is proven 165 (> 32).
    let mut mint_buf = [0u8; 32];
    mint_buf.copy_from_slice(&account.data[0..32]);
    let actual_mint = Pubkey::new_from_array(mint_buf);
    if actual_mint != expected_mint {
        return Err(SolendAssemblyError::InvalidTokenAccountMint {
            ata_kind,
            actual_mint,
            expected_mint,
        });
    }

    // 4. Owner field — safe because length is proven 165 (> 64).
    let mut owner_buf = [0u8; 32];
    owner_buf.copy_from_slice(&account.data[32..64]);
    let actual_owner = Pubkey::new_from_array(owner_buf);
    if actual_owner != expected_owner {
        return Err(SolendAssemblyError::InvalidTokenAccountOwner {
            ata_kind,
            actual_owner,
            expected_owner,
        });
    }

    Ok(())
}

// ── Production RPC adapter (Phase 4B-2) ─────────────────────────────────────
//
// [`ClawRpcPoolAccountFetcher`] is the production [`SolanaAccountFetcher`]
// implementation that backs the Phase 4B-2 tool path. It is read-only and
// exposes ONLY the three methods the assembler needs: `get_slot`,
// `get_account`, `get_multiple_accounts`. No transaction / signer / write
// methods are present — callers cannot reach sendTransaction through this
// adapter, keeping the Phase 4B-1a seam (no propose-stage chain side
// effects) intact.
//
// The adapter wraps an [`RpcPool`] directly (not a `ClawRpcClient`) because
// Phase 4B-1/1a's fetch coherence requires a single batched
// `getMultipleAccounts` call for the post-reserve topology, and
// `ClawRpcClient` does not currently expose batched reads. Going through
// the pool's `read_client()` preserves the existing circuit-breaker /
// retry behaviour.

use claw_types::solana::CommitmentLevel;

use claw_solana_core::rpc::RpcPool;

/// Read-only Solana RPC adapter for the Phase 4B-2 Solend deposit tool.
///
/// Implements [`SolanaAccountFetcher`] against [`RpcPool`]. Cheaply cloneable;
/// the inner pool is `Arc`-backed.
#[derive(Clone)]
pub struct ClawRpcPoolAccountFetcher {
    pool: RpcPool,
    commitment: CommitmentLevel,
}

impl ClawRpcPoolAccountFetcher {
    pub fn new(pool: RpcPool, commitment: CommitmentLevel) -> Self {
        Self { pool, commitment }
    }

    fn sdk_commitment(&self) -> solana_sdk::commitment_config::CommitmentConfig {
        match self.commitment {
            CommitmentLevel::Processed => {
                solana_sdk::commitment_config::CommitmentConfig::processed()
            }
            CommitmentLevel::Confirmed => {
                solana_sdk::commitment_config::CommitmentConfig::confirmed()
            }
            CommitmentLevel::Finalized => {
                solana_sdk::commitment_config::CommitmentConfig::finalized()
            }
        }
    }
}

#[async_trait]
impl SolanaAccountFetcher for ClawRpcPoolAccountFetcher {
    async fn get_account(
        &self,
        pubkey: &Pubkey,
    ) -> Result<Option<Account>, FetchError> {
        let client = self.pool.read_client().map_err(|e| FetchError::RpcFailure {
            reason: format!("rpc pool has no available endpoint: {e}"),
        })?;
        let commitment = self.sdk_commitment();
        let resp = client
            .get_account_with_commitment(pubkey, commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                FetchError::RpcFailure {
                    reason: format!("get_account_with_commitment failed: {e}"),
                }
            })?;
        self.pool.record_success(&client.url());
        Ok(resp.value)
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, FetchError> {
        if pubkeys.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.pool.read_client().map_err(|e| FetchError::RpcFailure {
            reason: format!("rpc pool has no available endpoint: {e}"),
        })?;
        let commitment = self.sdk_commitment();
        let resp = client
            .get_multiple_accounts_with_commitment(pubkeys, commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                FetchError::RpcFailure {
                    reason: format!("get_multiple_accounts_with_commitment failed: {e}"),
                }
            })?;
        self.pool.record_success(&client.url());
        Ok(resp.value)
    }

    async fn get_slot(&self) -> Result<u64, FetchError> {
        let client = self.pool.read_client().map_err(|e| FetchError::RpcFailure {
            reason: format!("rpc pool has no available endpoint: {e}"),
        })?;
        let commitment = self.sdk_commitment();
        let slot = client
            .get_slot_with_commitment(commitment)
            .await
            .map_err(|e| {
                self.pool.record_failure(&client.url());
                FetchError::RpcFailure {
                    reason: format!("get_slot_with_commitment failed: {e}"),
                }
            })?;
        self.pool.record_success(&client.url());
        Ok(slot)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lending::{FeedPublishFreshness, ProtocolTag};
    use solana_sdk::account::Account as SolAccount;
    use std::sync::Mutex;

    use super::super::raw::{SOLEND_NULL_ORACLE_SENTINEL_BS58, synth_obligation, synth_reserve};

    /// Slice 3G controlled wallet (public key, not a secret).
    const SLICE_3G_OWNER_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";
    /// Slice 3G source USDC ATA — published in docs/proofs/SOLEND_USDC_DEPOSIT_SLICE3G.md.
    const SLICE_3G_SOURCE_USDC_ATA_BS58: &str =
        "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3";
    /// Pyth Solana Receiver program — owner of `PriceUpdateV2` accounts.
    const PYTH_RECEIVER_PROGRAM_BS58: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

    // ── Stub fetcher ────────────────────────────────────────────────────────

    /// HashMap-backed stub fetcher. `None` in the map means "account does
    /// not exist on chain"; `Some(account)` means it does. Pubkeys placed
    /// in `panic_pubkeys` cause the stub to panic if ever requested —
    /// this is how test G asserts the sentinel switchboard pubkey is
    /// never fetched.
    struct StubFetcher {
        accounts: HashMap<Pubkey, Option<SolAccount>>,
        slot: u64,
        panic_pubkeys: HashMap<Pubkey, &'static str>,
        calls: Mutex<Vec<Pubkey>>,
    }

    impl StubFetcher {
        fn new(slot: u64) -> Self {
            Self {
                accounts: HashMap::new(),
                slot,
                panic_pubkeys: HashMap::new(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn with_account(mut self, pk: Pubkey, account: SolAccount) -> Self {
            self.accounts.insert(pk, Some(account));
            self
        }
        fn with_missing(mut self, pk: Pubkey) -> Self {
            self.accounts.insert(pk, None);
            self
        }
        fn forbid(mut self, pk: Pubkey, reason: &'static str) -> Self {
            self.panic_pubkeys.insert(pk, reason);
            self
        }
        fn calls_for(&self, pk: &Pubkey) -> usize {
            self.calls.lock().unwrap().iter().filter(|c| *c == pk).count()
        }
        fn lookup(&self, pk: &Pubkey) -> Option<SolAccount> {
            if let Some(reason) = self.panic_pubkeys.get(pk) {
                panic!("StubFetcher: forbidden pubkey {pk} was queried — {reason}");
            }
            self.calls.lock().unwrap().push(*pk);
            // Not in map = treat as missing. Explicit entries override.
            self.accounts.get(pk).cloned().unwrap_or(None)
        }
    }

    #[async_trait]
    impl SolanaAccountFetcher for StubFetcher {
        async fn get_account(
            &self,
            pubkey: &Pubkey,
        ) -> Result<Option<SolAccount>, FetchError> {
            Ok(self.lookup(pubkey))
        }
        async fn get_multiple_accounts(
            &self,
            pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<SolAccount>>, FetchError> {
            Ok(pubkeys.iter().map(|pk| self.lookup(pk)).collect())
        }
        async fn get_slot(&self) -> Result<u64, FetchError> {
            Ok(self.slot)
        }
    }

    // ── Account builders ────────────────────────────────────────────────────

    fn solend_owned_account(data: Vec<u8>) -> SolAccount {
        SolAccount {
            lamports: 1_000_000_000,
            data,
            owner: Pubkey::from_str(SOLEND_PROGRAM_ID_BS58).unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    fn pyth_receiver_account(data: Vec<u8>) -> SolAccount {
        SolAccount {
            lamports: 1_000_000,
            data,
            owner: Pubkey::from_str(PYTH_RECEIVER_PROGRAM_BS58).unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Construct a real, correctly-shaped classic SPL Token Account buffer.
    ///
    /// Byte layout (matches SPL Token program `Account::pack`):
    ///   0..32   mint             (Pubkey)
    ///  32..64   owner            (Pubkey, token-account owner, i.e. wallet)
    ///  64..72   amount           (u64 little-endian)
    ///  remaining bytes are zero — unread by this assembler slice.
    ///
    /// Deliberately constructed by hand rather than via `spl-token` — the
    /// assembler's validator is a manual byte parser and the fixture must
    /// exercise that parser. Importing `spl-token` here would be a new
    /// dependency for no gain.
    fn real_spl_token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> SolAccount {
        let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        data[0..32].copy_from_slice(&mint.to_bytes());
        data[32..64].copy_from_slice(&owner.to_bytes());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        SolAccount {
            lamports: 2_039_280,
            data,
            owner: Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    /// System-Program-owned raw account for "wrong owner program" tests.
    /// Data length is still 165 to isolate the owner-program check from
    /// the data-length check.
    fn system_owned_account_of_len(len: usize) -> SolAccount {
        SolAccount {
            lamports: 2_039_280,
            data: vec![0u8; len],
            owner: solana_sdk::system_program::ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// SPL-Token-owned buffer with a chosen length. Used to isolate the
    /// data-length check from all other checks.
    fn spl_owned_buffer_of_len(len: usize) -> SolAccount {
        SolAccount {
            lamports: 2_039_280,
            data: vec![0u8; len],
            owner: Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    /// Synthesize a Pyth `PriceUpdateV2` with Full verification_level and
    /// a known `posted_slot`. Matches the layout expected by
    /// `decode_pyth_solana_receiver_publish_freshness`.
    fn synth_pyth_price_update_v2(posted_slot: u64) -> Vec<u8> {
        use super::super::oracle_decoder::{
            PYTH_PRICE_UPDATE_V2_DISCRIMINATOR, PYTH_PRICE_UPDATE_V2_LEN,
        };
        let mut buf = vec![0u8; PYTH_PRICE_UPDATE_V2_LEN];
        buf[0..8].copy_from_slice(&PYTH_PRICE_UPDATE_V2_DISCRIMINATOR);
        // verification_level tag at offset 40: 1 = Full (1-byte enum).
        buf[40] = 1;
        // Full variant: price_message_start = 41, posted_slot_start = 41 + 84 = 125.
        let posted_slot_off = 41 + 84;
        buf[posted_slot_off..posted_slot_off + 8].copy_from_slice(&posted_slot.to_le_bytes());
        buf
    }

    fn usdc_reserve_raw_bytes(
        lending_market: Pubkey,
        collateral_mint: Pubkey,
        pyth_oracle: Pubkey,
        switchboard_oracle: Pubkey,
    ) -> Vec<u8> {
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        synth_reserve(
            lending_market,
            usdc_mint,
            6,                              // USDC decimals
            Pubkey::new_unique(),           // liquidity_supply — opaque here
            pyth_oracle,
            switchboard_oracle,
            32_000_000_000_000,             // available liquidity
            collateral_mint,
            Pubkey::new_unique(),           // collateral_supply — opaque here
            414_000_000,                    // reserve last_update_slot
            false,                          // not stale
        )
    }

    fn sentinel_pubkey() -> Pubkey {
        Pubkey::from_str(SOLEND_NULL_ORACLE_SENTINEL_BS58).unwrap()
    }

    /// Shared fixture: a fully wired stub fetcher for the happy-path flow
    /// where the obligation may or may not exist, Pyth is present and
    /// Pyth-Receiver-owned, Switchboard = sentinel, source ATA is a valid
    /// SPL Token Account for (USDC, session_wallet), and collateral ATA
    /// is absent. `fetcher` is returned UNWRAPPED so tests can
    /// `accounts.insert(...)` overrides before wrapping in `Arc`.
    #[allow(dead_code)] // lending_market exposed for future test shaping; not all tests use it
    struct Scene {
        fetcher: StubFetcher,
        session_wallet: Pubkey,
        obligation_pubkey: Pubkey,
        reserve_pubkey: Pubkey,
        collateral_mint: Pubkey,
        source_ata: Pubkey,
        collateral_ata: Pubkey,
        pyth_oracle: Pubkey,
        lending_market: Pubkey,
    }

    fn scene(existing_obligation: bool, slice3g_owner: bool) -> Scene {
        scene_with_collateral_ata_present(existing_obligation, slice3g_owner, false)
    }

    /// Variant that lets the test decide whether a valid collateral ATA
    /// is pre-placed on-chain. Default `false` matches the Slice 3G
    /// "collateral ATA transient / not present pre-deposit" reality.
    fn scene_with_collateral_ata_present(
        existing_obligation: bool,
        slice3g_owner: bool,
        collateral_ata_present: bool,
    ) -> Scene {
        let session_wallet = if slice3g_owner {
            Pubkey::from_str(SLICE_3G_OWNER_BS58).unwrap()
        } else {
            Pubkey::new_unique()
        };
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        let reserve_pubkey = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();
        let collateral_mint = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let pyth_oracle = Pubkey::new_unique();
        let sentinel = sentinel_pubkey();
        let obligation_pubkey = Pubkey::new_unique();

        let source_ata = derive_associated_token_address(&session_wallet, &usdc_mint, &spl_token);
        let collateral_ata =
            derive_associated_token_address(&session_wallet, &collateral_mint, &spl_token);

        let reserve_bytes = usdc_reserve_raw_bytes(
            lending_market,
            collateral_mint,
            pyth_oracle,
            sentinel,
        );
        let reserve_account = solend_owned_account(reserve_bytes);
        let pyth_account = pyth_receiver_account(synth_pyth_price_update_v2(414_000_050));
        // VALID source ATA: classic SPL Token owner, 165 bytes, mint = USDC,
        // token-account owner = session_wallet.
        let source_ata_account = real_spl_token_account(usdc_mint, session_wallet, 0);

        let mut fetcher = StubFetcher::new(414_000_100)
            .with_account(reserve_pubkey, reserve_account)
            .with_account(pyth_oracle, pyth_account)
            .with_account(source_ata, source_ata_account)
            .forbid(sentinel, "Switchboard sentinel must not be fetched");
        if existing_obligation {
            let obligation_bytes =
                synth_obligation(session_wallet, lending_market, 414_000_000, false, &[], &[]);
            fetcher = fetcher.with_account(obligation_pubkey, solend_owned_account(obligation_bytes));
        } else {
            fetcher = fetcher.with_missing(obligation_pubkey);
        }
        // collateral ATA: default missing; explicit opt-in to place a valid one.
        if collateral_ata_present {
            fetcher = fetcher.with_account(
                collateral_ata,
                real_spl_token_account(collateral_mint, session_wallet, 0),
            );
        } else {
            fetcher = fetcher.with_missing(collateral_ata);
        }

        Scene {
            fetcher,
            session_wallet,
            obligation_pubkey,
            reserve_pubkey,
            collateral_mint,
            source_ata,
            collateral_ata,
            pyth_oracle,
            lending_market,
        }
    }

    // ── (A) Unsupported mint fails closed ──────────────────────────────────

    #[tokio::test]
    async fn unsupported_mint_fails_closed_without_broad_scan() {
        // Any non-USDC mint must fail immediately. The stub fetcher has
        // no accounts registered — if the assembler attempted a broad
        // scan it would make an RPC call and we'd see a recorded call.
        let fetcher = Arc::new(StubFetcher::new(1));
        let assembler =
            SolendSnapshotAssembler::new(fetcher.clone() as Arc<dyn SolanaAccountFetcher>);
        let not_usdc = Pubkey::new_unique();
        let err = assembler
            .assemble_for_deposit(Pubkey::new_unique(), not_usdc, Pubkey::new_unique())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::UnsupportedReserveMint { mint, .. } if mint == not_usdc
            ),
            "unexpected err: {err:?}"
        );
        // No scan: zero calls recorded.
        assert_eq!(fetcher.calls.lock().unwrap().len(), 0);
    }

    // ── (B) First-deposit path assembles snapshot ──────────────────────────

    #[tokio::test]
    async fn first_deposit_path_assembles_snapshot() {
        let s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let expected_reserve = s.reserve_pubkey;
        let expected_obligation = s.obligation_pubkey;
        let expected_source_ata = s.source_ata;
        let expected_collateral_ata = s.collateral_ata;
        let expected_session_wallet = s.session_wallet;
        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        let out = assembler
            .assemble_for_deposit(expected_session_wallet, usdc_mint, expected_obligation)
            .await
            .expect("assembly succeeds on first-deposit path");

        assert!(!out.obligation_exists);
        assert_eq!(out.reserve_pubkey, expected_reserve);
        assert_eq!(out.obligation_pubkey, expected_obligation);
        assert_eq!(out.source_liquidity_ata, expected_source_ata);
        assert_eq!(out.user_collateral_ata, expected_collateral_ata);
        assert!(out.source_ata_exists);
        assert!(!out.collateral_ata_exists);

        // Snapshot shape
        assert_eq!(out.snapshot.protocol_tag, ProtocolTag::Solend);
        assert_eq!(out.snapshot.session_wallet, expected_session_wallet);
        assert_eq!(out.snapshot.obligation.owner, expected_session_wallet);
        assert!(out.snapshot.obligation.deposits.is_empty());
        assert!(out.snapshot.obligation.borrows.is_empty());
        assert_eq!(out.snapshot.reserves.len(), 1);
        assert_eq!(out.snapshot.reserves[0].identifier, expected_reserve);
        assert_eq!(out.snapshot.reserves[0].mint, usdc_mint);
        // Only Pyth feed — switchboard sentinel excluded.
        assert_eq!(out.snapshot.oracles.len(), 1);
        assert!(matches!(
            out.snapshot.oracles[0].feeds[0].publish,
            FeedPublishFreshness::KnownSlot(_)
        ));
    }

    // ── (C) Existing obligation path assembles snapshot ────────────────────

    #[tokio::test]
    async fn existing_obligation_path_assembles_snapshot() {
        let s = scene(/*existing_obligation=*/ true, /*slice3g_owner=*/ false);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;
        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        let out = assembler
            .assemble_for_deposit(sw, usdc_mint, ob)
            .await
            .expect("assembly succeeds for existing obligation");

        assert!(out.obligation_exists);
        assert_eq!(out.snapshot.obligation.owner, sw);
        // Slice 3H-decoded obligation with no deposits — the synthetic
        // we placed on chain matches this.
        assert!(out.snapshot.obligation.deposits.is_empty());
        assert_eq!(out.snapshot.reserves.len(), 1);
    }

    // ── (D) Wrong-owner obligation fails closed ────────────────────────────

    #[tokio::test]
    async fn obligation_wrong_owner_fails_closed() {
        let mut s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;

        // Replace the obligation slot with a System-Program-owned account.
        let sysprog = solana_sdk::system_program::ID;
        s.fetcher.accounts.insert(
            ob,
            Some(SolAccount {
                lamports: 0,
                data: vec![0u8; 1300],
                owner: sysprog,
                executable: false,
                rent_epoch: 0,
            }),
        );

        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        let err = assembler
            .assemble_for_deposit(sw, usdc_mint, ob)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::ObligationAccountOwnerMismatch {
                    actual_owner, ..
                } if actual_owner == sysprog
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (E) Missing reserve fails closed ───────────────────────────────────

    #[tokio::test]
    async fn missing_reserve_fails_closed() {
        // Register reserve as explicitly missing.
        let reserve_pubkey = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();
        let fetcher = Arc::new(StubFetcher::new(1).with_missing(reserve_pubkey));
        let assembler =
            SolendSnapshotAssembler::new(fetcher as Arc<dyn SolanaAccountFetcher>);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let err = assembler
            .assemble_for_deposit(Pubkey::new_unique(), usdc_mint, Pubkey::new_unique())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::ReserveAccountMissing { reserve } if reserve == reserve_pubkey
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (F) Missing Pyth oracle fails closed ───────────────────────────────

    #[tokio::test]
    async fn missing_pyth_oracle_fails_closed() {
        let mut s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;
        let pyth = s.pyth_oracle;

        // Overwrite the Pyth entry to None (missing).
        s.fetcher.accounts.insert(pyth, None);

        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        let err = assembler
            .assemble_for_deposit(sw, usdc_mint, ob)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::OracleAccountMissing { oracle } if oracle == pyth
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (G) Switchboard sentinel is never fetched ─────────────────────────

    #[tokio::test]
    async fn switchboard_sentinel_is_never_fetched() {
        // The stub's `forbid(sentinel, ...)` panics if sentinel is ever
        // queried. If this test returns without panicking, the assembler
        // correctly skipped the sentinel slot.
        let s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;
        let fetcher_arc = Arc::new(s.fetcher);
        let assembler = SolendSnapshotAssembler::new(
            fetcher_arc.clone() as Arc<dyn SolanaAccountFetcher>,
        );
        let out = assembler
            .assemble_for_deposit(sw, usdc_mint, ob)
            .await
            .expect("assembly succeeds without fetching switchboard sentinel");

        assert_eq!(
            fetcher_arc.calls_for(&sentinel_pubkey()),
            0,
            "sentinel pubkey must never be queried"
        );
        // And the assembled snapshot has exactly one feed set (Pyth only).
        assert_eq!(out.snapshot.oracles.len(), 1);
    }

    // ── (H) ATA derivation matches Slice 3G published facts ────────────────

    #[test]
    fn slice_3g_source_usdc_ata_matches_published_fact() {
        // Pure derivation check: the assembler uses
        // `derive_associated_token_address(wallet, mint, spl_token)` for
        // the source liquidity ATA. Given Slice 3G's published owner and
        // USDC mint, this must equal Slice 3G's published source ATA.
        let owner = Pubkey::from_str(SLICE_3G_OWNER_BS58).unwrap();
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        let derived = derive_associated_token_address(&owner, &usdc_mint, &spl_token);
        assert_eq!(
            derived.to_string(),
            SLICE_3G_SOURCE_USDC_ATA_BS58,
            "Slice 3G published source USDC ATA must match the \
             assembler's derivation for that wallet"
        );
    }

    #[tokio::test]
    async fn assembler_collateral_ata_equals_derivation_of_reserve_collateral_mint() {
        // End-to-end through the assembler: given a synthetic reserve
        // whose collateral_mint we chose, the assembler's returned
        // `user_collateral_ata` must equal the direct derivation.
        let s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ true);
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;
        let collateral_mint = s.collateral_mint;
        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        let out = assembler
            .assemble_for_deposit(sw, usdc_mint, ob)
            .await
            .unwrap();

        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        let expected =
            derive_associated_token_address(&sw, &collateral_mint, &spl_token);
        assert_eq!(out.user_collateral_ata, expected);
        // And the source ATA for the Slice 3G owner must match the
        // published constant.
        assert_eq!(
            out.source_liquidity_ata.to_string(),
            SLICE_3G_SOURCE_USDC_ATA_BS58
        );
    }

    // ── (I) No-execution / no-policy seam guard ────────────────────────────

    /// Compile-time structural assertion: the assembler's only fetch
    /// dependency is `Arc<dyn SolanaAccountFetcher>`. If a future change
    /// grew an approval / park / signer / orchestrator dependency, the
    /// constructor signature would change and this test would stop
    /// compiling. Combined with the file-level imports discipline
    /// (no `crate::lending::evaluate_lending_policy`, no Solend ix
    /// builders imported, no signer / approval / orchestrator imports),
    /// this is the Phase 4B-1 seam guard.
    #[tokio::test]
    async fn assembler_has_only_fetcher_dependency() {
        let fetcher: Arc<dyn SolanaAccountFetcher> = Arc::new(StubFetcher::new(1));
        let _ = SolendSnapshotAssembler::new(fetcher);
    }

    // ────────────────────────────────────────────────────────────────────────
    // Phase 4B-1a · Account validation hardening
    //
    // These tests lock down the four-check SPL-Token-Account validator
    // (owner program, data length, mint, token-account owner field), plus
    // Pyth owner validation and null-Pyth skip behavior. Every failure
    // case asserts a typed `SolendAssemblyError` variant with an
    // `ata_kind` that distinguishes source from collateral.
    // ────────────────────────────────────────────────────────────────────────

    /// Helper: run the assembler against a scene and return the result.
    async fn run_assembler(s: Scene) -> Result<AssembledSolendDepositSnapshot, SolendAssemblyError> {
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let sw = s.session_wallet;
        let ob = s.obligation_pubkey;
        let assembler = SolendSnapshotAssembler::new(
            Arc::new(s.fetcher) as Arc<dyn SolanaAccountFetcher>,
        );
        assembler.assemble_for_deposit(sw, usdc_mint, ob).await
    }

    // ── (4B-1a-A) Missing ATAs are NOT errors and report exists=false ───────

    #[tokio::test]
    async fn missing_source_and_collateral_ata_return_exists_false_not_error() {
        let mut s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let src = s.source_ata;
        let col = s.collateral_ata;
        // Both ATAs explicitly absent.
        s.fetcher.accounts.insert(src, None);
        s.fetcher.accounts.insert(col, None);

        let out = run_assembler(s).await.expect("absent ATAs must not fail assembly");
        assert!(!out.source_ata_exists);
        assert!(!out.collateral_ata_exists);
    }

    // ── (4B-1a-B) Valid ATAs report exists=true ─────────────────────────────

    #[tokio::test]
    async fn valid_source_and_collateral_ata_report_exists_true() {
        // `scene_with_collateral_ata_present(..., true)` pre-places a
        // valid collateral ATA; the default scene already places a valid
        // source ATA.
        let s = scene_with_collateral_ata_present(
            /*existing_obligation=*/ false,
            /*slice3g_owner=*/ false,
            /*collateral_ata_present=*/ true,
        );
        let out = run_assembler(s).await.expect("valid ATAs must pass validation");
        assert!(out.source_ata_exists);
        assert!(out.collateral_ata_exists);
    }

    // ── (4B-1a-C/D) Wrong SPL owner program fails with ata_kind ─────────────

    #[tokio::test]
    async fn source_ata_wrong_owner_program_is_rejected() {
        let mut s = scene(/*existing_obligation=*/ false, /*slice3g_owner=*/ false);
        let src = s.source_ata;
        // System-Program-owned at source_ata pubkey — 165-byte payload
        // isolates the owner-program check from the length check.
        s.fetcher
            .accounts
            .insert(src, Some(system_owned_account_of_len(SPL_TOKEN_ACCOUNT_LEN)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountOwnerProgram {
                    ata_kind: AtaKind::Source,
                    ..
                }
            ),
            "unexpected err: {err:?}"
        );
    }

    #[tokio::test]
    async fn collateral_ata_wrong_owner_program_is_rejected() {
        let mut s = scene_with_collateral_ata_present(false, false, true);
        let col = s.collateral_ata;
        s.fetcher
            .accounts
            .insert(col, Some(system_owned_account_of_len(SPL_TOKEN_ACCOUNT_LEN)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountOwnerProgram {
                    ata_kind: AtaKind::Collateral,
                    ..
                }
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (4B-1a-E/F) Wrong data length fails BEFORE any byte slicing ─────────

    #[tokio::test]
    async fn source_ata_wrong_data_length_fails_before_slicing() {
        let mut s = scene(false, false);
        let src = s.source_ata;
        // SPL-Token-owned but only 64 bytes — too short to even contain
        // the mint field. The validator must reject on length (step 2)
        // BEFORE attempting `data[0..32]` (step 3).
        s.fetcher.accounts.insert(src, Some(spl_owned_buffer_of_len(64)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountDataLen {
                    ata_kind: AtaKind::Source,
                    actual_len: 64,
                    expected_len: 165,
                }
            ),
            "unexpected err: {err:?}"
        );
    }

    #[tokio::test]
    async fn collateral_ata_wrong_data_length_fails_before_slicing() {
        let mut s = scene_with_collateral_ata_present(false, false, true);
        let col = s.collateral_ata;
        // Zero-length buffer: the most aggressive "would-panic-on-slice"
        // case. Length check must reject without ever touching data[0..32].
        s.fetcher.accounts.insert(col, Some(spl_owned_buffer_of_len(0)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountDataLen {
                    ata_kind: AtaKind::Collateral,
                    actual_len: 0,
                    expected_len: 165,
                }
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (4B-1a-G/H) Wrong mint field fails with explicit ata_kind ───────────

    #[tokio::test]
    async fn source_ata_wrong_mint_is_rejected() {
        let mut s = scene(false, false);
        let src = s.source_ata;
        let sw = s.session_wallet;
        // Valid 165-byte SPL Token Account, but with WRONG mint (not USDC).
        let wrong_mint = Pubkey::new_unique();
        s.fetcher
            .accounts
            .insert(src, Some(real_spl_token_account(wrong_mint, sw, 0)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountMint {
                    ata_kind: AtaKind::Source,
                    actual_mint,
                    ..
                } if actual_mint == wrong_mint
            ),
            "unexpected err: {err:?}"
        );
    }

    #[tokio::test]
    async fn collateral_ata_wrong_mint_is_rejected() {
        let mut s = scene_with_collateral_ata_present(false, false, true);
        let col = s.collateral_ata;
        let sw = s.session_wallet;
        let wrong_mint = Pubkey::new_unique();
        s.fetcher
            .accounts
            .insert(col, Some(real_spl_token_account(wrong_mint, sw, 0)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountMint {
                    ata_kind: AtaKind::Collateral,
                    actual_mint,
                    ..
                } if actual_mint == wrong_mint
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (4B-1a-I/J) Wrong token-account owner field fails ──────────────────

    #[tokio::test]
    async fn source_ata_wrong_token_account_owner_is_rejected() {
        let mut s = scene(false, false);
        let src = s.source_ata;
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        // Right mint, wrong owner-field (belongs to someone else).
        let attacker = Pubkey::new_unique();
        s.fetcher
            .accounts
            .insert(src, Some(real_spl_token_account(usdc_mint, attacker, 0)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountOwner {
                    ata_kind: AtaKind::Source,
                    actual_owner,
                    ..
                } if actual_owner == attacker
            ),
            "unexpected err: {err:?}"
        );
    }

    #[tokio::test]
    async fn collateral_ata_wrong_token_account_owner_is_rejected() {
        let mut s = scene_with_collateral_ata_present(false, false, true);
        let col = s.collateral_ata;
        let collateral_mint = s.collateral_mint;
        let attacker = Pubkey::new_unique();
        s.fetcher
            .accounts
            .insert(col, Some(real_spl_token_account(collateral_mint, attacker, 0)));
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::InvalidTokenAccountOwner {
                    ata_kind: AtaKind::Collateral,
                    actual_owner,
                    ..
                } if actual_owner == attacker
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (4B-1a-K) Pyth oracle wrong owner program is rejected ──────────────

    #[tokio::test]
    async fn pyth_oracle_wrong_owner_program_is_rejected() {
        let mut s = scene(false, false);
        let pyth = s.pyth_oracle;
        // The account exists at the pyth pubkey, but it is owned by the
        // System Program instead of the Pyth Solana Receiver. Validation
        // must fail-closed BEFORE calling `extract_publish_freshness`.
        let sysprog = solana_sdk::system_program::ID;
        s.fetcher.accounts.insert(
            pyth,
            Some(SolAccount {
                lamports: 1_000_000,
                data: vec![0u8; 134], // whatever size — owner check runs first
                owner: sysprog,
                executable: false,
                rent_epoch: 0,
            }),
        );
        let err = run_assembler(s).await.unwrap_err();
        assert!(
            matches!(
                err,
                SolendAssemblyError::PythOracleOwnerMismatch {
                    actual_owner, ..
                } if actual_owner == sysprog
            ),
            "unexpected err: {err:?}"
        );
    }

    // ── (4B-1a-L) Null/default Pyth pubkey is not fetched ──────────────────

    #[tokio::test]
    async fn null_default_pyth_oracle_pubkey_is_not_fetched() {
        // Rebuild a scene where the reserve's pyth_oracle IS the Solend
        // null sentinel (both oracle slots sentinel). The stub forbids
        // the sentinel pubkey — any fetch attempt panics the test.
        let session_wallet = Pubkey::new_unique();
        let usdc_mint = Pubkey::from_str(USDC_MINT_BS58).unwrap();
        let spl_token = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID_BS58).unwrap();
        let reserve_pubkey = Pubkey::from_str(USDC_RESERVE_BS58).unwrap();
        let collateral_mint = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sentinel = sentinel_pubkey();
        let obligation_pubkey = Pubkey::new_unique();

        let source_ata = derive_associated_token_address(&session_wallet, &usdc_mint, &spl_token);
        let collateral_ata =
            derive_associated_token_address(&session_wallet, &collateral_mint, &spl_token);

        let reserve_bytes = usdc_reserve_raw_bytes(
            lending_market,
            collateral_mint,
            /*pyth_oracle=*/ sentinel,
            /*switchboard_oracle=*/ sentinel,
        );
        let reserve_account = solend_owned_account(reserve_bytes);
        let source_ata_account = real_spl_token_account(usdc_mint, session_wallet, 0);

        let fetcher = StubFetcher::new(414_000_100)
            .with_account(reserve_pubkey, reserve_account)
            .with_account(source_ata, source_ata_account)
            .with_missing(obligation_pubkey)
            .with_missing(collateral_ata)
            .forbid(sentinel, "Pyth/Switchboard sentinel must not be fetched");

        let fetcher_arc = Arc::new(fetcher);
        let assembler = SolendSnapshotAssembler::new(
            fetcher_arc.clone() as Arc<dyn SolanaAccountFetcher>,
        );
        let out = assembler
            .assemble_for_deposit(session_wallet, usdc_mint, obligation_pubkey)
            .await
            .expect("null-pyth + sentinel-switchboard must assemble without fetching sentinel");

        assert_eq!(
            fetcher_arc.calls_for(&sentinel),
            0,
            "sentinel pubkey must never be queried when Pyth is null/default"
        );

        // Mapping layer emits one OracleFeedSet per reserve. Because both
        // oracle slots are sentinels, the set's `feeds` list is empty.
        // The policy evaluator's `MaxOracleStalenessMs` will HardBlock on
        // the empty feed set — the assembler itself does NOT mark
        // freshness as pass.
        assert_eq!(out.snapshot.oracles.len(), 1);
        assert!(
            out.snapshot.oracles[0].feeds.is_empty(),
            "null Pyth + sentinel Switchboard must produce zero feeds (policy HardBlocks later)"
        );
    }
}
