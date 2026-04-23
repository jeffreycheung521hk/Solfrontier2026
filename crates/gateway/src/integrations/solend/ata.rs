//! SPL Associated Token Account helper — Slice 3D(i).
//!
//! ## What this module is
//!
//! A thin, pure wrapper over `spl_associated_token_account` for the two
//! operations the future Solend deposit path needs:
//!
//! 1. Deterministically **derive** an ATA address from
//!    `(wallet, mint, token_program)`.
//! 2. Build an **unsigned** `CreateIdempotent` ATA instruction.
//!
//! ## Not Solend protocol logic
//!
//! The Associated Token Account program is a standard SPL program used
//! across every Solana token flow, not something Solend-specific. This
//! module lives under `integrations::solend` for now because Solend is
//! currently the only consumer; if a second integration needs ATA
//! tooling, the cleanest move is to lift this module into a shared
//! `integrations::spl` location. Part 5A §40.1 explicitly allows this —
//! don't invent a repo-wide taxonomy ahead of a second consumer.
//!
//! ## Purity
//!
//! Every function here is pure. No RPC, no signer, no blockhash, no
//! transaction submission, no policy, no `LendingSnapshot`, no daemon
//! runtime, no `approval_store` / `park_store`. The caller composes
//! these instructions with others and signs downstream.
//!
//! ## Authoritative source
//!
//! `spl-associated-token-account` v6 (workspace-pinned). The crate's
//! own `instruction::create_associated_token_account_idempotent` is
//! used directly so we do not hand-roll account metas or PDA derivation
//! curve math.

use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

pub use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};

/// Derive the ATA for `(wallet, mint, token_program)` deterministically
/// via the SPL Associated Token Account crate. The derivation is
/// `find_program_address([wallet, token_program, mint], ATA_PROGRAM)`
/// per the SPL ATA program — we do not re-implement the curve math.
pub fn derive_associated_token_address(
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, token_program)
}

/// Build an unsigned SPL `CreateIdempotent` ATA instruction. Idempotent
/// variant is preferred because it is a no-op when the ATA already
/// exists — safer to compose into a batched transaction that may be
/// submitted more than once or where the ATA state on chain is
/// uncertain at tx-assembly time.
///
/// `funder` pays rent for the new ATA and signs the transaction.
/// `wallet` is the ATA's owner.
/// `mint` is the token mint the ATA is for.
/// `token_program` is the SPL Token program (classic or Token-2022)
///   — Solend's AMM path uses classic (`TokenkegQfe…`).
pub fn build_create_ata_idempotent_instruction(
    funder: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    create_associated_token_account_idempotent(funder, wallet, mint, token_program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Controlled wallet pubkey from Slice 3C (not a secret — public key).
    const OWNER_BS58: &str = "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L";

    /// Target reserve's underlying mint (AMM Pyth-only reserve).
    const UNDERLYING_MINT_BS58: &str =
        "E5ndSkaB17Dm7CsD22dvcjfrYSDLCxFcMd6z8ddCk5wp";

    /// Target reserve's c-token / collateral mint, decoded from the
    /// on-chain reserve bytes in Slice 3C.
    const COLLATERAL_MINT_BS58: &str =
        "7i7dsv8srRcERzd8EtUb6sZJoE4zVG49mH675QkAFJdX";

    /// SPL Token program (classic).
    const CLASSIC_SPL_TOKEN_BS58: &str =
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    /// Expected ATA for `(OWNER, UNDERLYING_MINT, classic SPL Token)`.
    /// Value computed out-of-band during Slice 3C via an independent
    /// Python ed25519 implementation, then confirmed by this test.
    const EXPECTED_SOURCE_LIQUIDITY_ATA_BS58: &str =
        "DSVdfKv8zHQ34riJDra8UNHZynkpk92cavFFLEGGgTt6";

    /// Expected ATA for `(OWNER, COLLATERAL_MINT, classic SPL Token)`.
    const EXPECTED_USER_COLLATERAL_ATA_BS58: &str =
        "FErpwhDZsitwzvgJUqMjiNdKCfes3wJRFZXVjkGR2CVt";

    fn owner() -> Pubkey {
        Pubkey::from_str(OWNER_BS58).unwrap()
    }
    fn underlying_mint() -> Pubkey {
        Pubkey::from_str(UNDERLYING_MINT_BS58).unwrap()
    }
    fn collateral_mint() -> Pubkey {
        Pubkey::from_str(COLLATERAL_MINT_BS58).unwrap()
    }
    fn classic_spl_token() -> Pubkey {
        Pubkey::from_str(CLASSIC_SPL_TOKEN_BS58).unwrap()
    }

    #[test]
    fn derive_ata_for_underlying_matches_independent_computation() {
        let derived = derive_associated_token_address(
            &owner(),
            &underlying_mint(),
            &classic_spl_token(),
        );
        assert_eq!(
            derived.to_string(),
            EXPECTED_SOURCE_LIQUIDITY_ATA_BS58,
            "SPL-ATA-crate derivation must agree with independent ed25519 computation"
        );
    }

    #[test]
    fn derive_ata_for_collateral_matches_independent_computation() {
        let derived = derive_associated_token_address(
            &owner(),
            &collateral_mint(),
            &classic_spl_token(),
        );
        assert_eq!(
            derived.to_string(),
            EXPECTED_USER_COLLATERAL_ATA_BS58,
        );
    }

    #[test]
    fn create_idempotent_ix_program_id_is_ata_program() {
        let ix = build_create_ata_idempotent_instruction(
            &owner(),
            &owner(),
            &underlying_mint(),
            &classic_spl_token(),
        );
        assert_eq!(ix.program_id, spl_associated_token_account::id());
    }

    #[test]
    fn create_idempotent_ix_data_is_single_byte_tag_one() {
        // SPL ATA instruction enum: Create = 0, CreateIdempotent = 1,
        // RecoverNested = 2. Ix data is a single discriminator byte.
        let ix = build_create_ata_idempotent_instruction(
            &owner(),
            &owner(),
            &underlying_mint(),
            &classic_spl_token(),
        );
        assert_eq!(ix.data, vec![1u8], "CreateIdempotent tag is 1");
    }

    /// Confirms the account meta layout the crate produces. The SPL ATA
    /// program expects, in order:
    ///   0. funder (writable, signer)
    ///   1. associated_token_account (writable)
    ///   2. wallet (readonly)
    ///   3. mint (readonly)
    ///   4. system_program (readonly)
    ///   5. token_program (readonly)
    ///
    /// We don't hand-roll these; the crate produces them. The test locks
    /// the layout so a future dep bump that reorders accounts is caught.
    #[test]
    fn create_idempotent_ix_account_metas_order_and_flags() {
        use solana_sdk::system_program;

        let funder = owner();
        let wallet = Pubkey::from_str("11111111111111111111111111111112").unwrap();
        let mint = underlying_mint();
        let ix = build_create_ata_idempotent_instruction(
            &funder,
            &wallet,
            &mint,
            &classic_spl_token(),
        );
        let expected_ata = derive_associated_token_address(&wallet, &mint, &classic_spl_token());

        assert_eq!(ix.accounts.len(), 6);

        // 0: funder — writable, signer
        assert_eq!(ix.accounts[0].pubkey, funder);
        assert!(ix.accounts[0].is_writable);
        assert!(ix.accounts[0].is_signer);

        // 1: ATA — writable, not signer
        assert_eq!(ix.accounts[1].pubkey, expected_ata);
        assert!(ix.accounts[1].is_writable);
        assert!(!ix.accounts[1].is_signer);

        // 2: wallet — readonly
        assert_eq!(ix.accounts[2].pubkey, wallet);
        assert!(!ix.accounts[2].is_writable);
        assert!(!ix.accounts[2].is_signer);

        // 3: mint — readonly
        assert_eq!(ix.accounts[3].pubkey, mint);
        assert!(!ix.accounts[3].is_writable);
        assert!(!ix.accounts[3].is_signer);

        // 4: system program — readonly
        assert_eq!(ix.accounts[4].pubkey, system_program::id());
        assert!(!ix.accounts[4].is_writable);
        assert!(!ix.accounts[4].is_signer);

        // 5: token program — readonly
        assert_eq!(ix.accounts[5].pubkey, classic_spl_token());
        assert!(!ix.accounts[5].is_writable);
        assert!(!ix.accounts[5].is_signer);
    }

    /// Different `token_program` values must produce different ATAs.
    /// This catches a drift where a future change silently hardcodes
    /// one token program.
    #[test]
    fn different_token_programs_produce_different_atas() {
        let classic = classic_spl_token();
        let token_2022: Pubkey =
            Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();
        let ata_classic = derive_associated_token_address(&owner(), &underlying_mint(), &classic);
        let ata_2022 = derive_associated_token_address(&owner(), &underlying_mint(), &token_2022);
        assert_ne!(ata_classic, ata_2022);
    }

    /// Compile-time evidence: no `lending::*` type appears in this module's
    /// public API. Function pointers below compile iff that holds.
    #[test]
    fn ata_helpers_do_not_depend_on_lending_types() {
        fn _derive_fn(_: &Pubkey, _: &Pubkey, _: &Pubkey) -> Pubkey {
            derive_associated_token_address(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
            )
        }
        fn _create_fn(
            _: &Pubkey,
            _: &Pubkey,
            _: &Pubkey,
            _: &Pubkey,
        ) -> Instruction {
            build_create_ata_idempotent_instruction(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
            )
        }
        let _ = _derive_fn as fn(&Pubkey, &Pubkey, &Pubkey) -> Pubkey;
        let _ = _create_fn as fn(&Pubkey, &Pubkey, &Pubkey, &Pubkey) -> Instruction;
    }
}
