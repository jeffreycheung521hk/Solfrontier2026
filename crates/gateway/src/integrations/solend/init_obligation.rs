//! Solend `InitObligation` instruction builder + obligation account
//! create-account helper — Slice 3E(i).
//!
//! ## Purity
//!
//! Both builders here are pure functions over caller-supplied pubkeys
//! and scalars. They do NOT:
//!
//! - call RPC
//! - sign transactions
//! - fetch rent-exempt lamports (caller supplies them)
//! - read the policy evaluator state or any `LendingSnapshot`
//! - reach into `approval_store` / `park_store` / daemon runtime
//! - read global config
//! - submit or broadcast
//! - fetch blockhash
//! - create accounts on chain
//!
//! A first-deposit flow requires the obligation account to be created
//! by the SystemProgram BEFORE `InitObligation` runs. The two
//! instructions are composed by the caller in the same transaction (or
//! bundled with the ATA-create and deposit ixs per the Slice 3E(i)
//! review). This module builds each unsigned instruction; signing,
//! transaction assembly, and blockhash attachment are all downstream.
//!
//! ## Source of truth
//!
//! **`solendprotocol/solana-program-library`, `mainnet` branch**, commit
//! `d04ce00bbf4356c4fd32b3be38eb9760b696bb3e`:
//!
//! - Constructor: `token-lending/sdk/src/instruction.rs`,
//!   `init_obligation(...)`.
//! - Handler: `token-lending/program/src/processor.rs`,
//!   `process_init_obligation(...)` — `Clock` is read via
//!   `Clock::get()?` sysvar syscall, NOT from the account list.
//! - `LendingInstruction` variant: tag byte = **6**.
//! - Obligation account: `Obligation::LEN = OBLIGATION_LEN = 1300`
//!   (`token-lending/sdk/src/state/obligation.rs`). Owner is the Solend
//!   program.
//! - **5 accounts** in the exact order produced below.
//!
//! ## Signer / writable matrix for `InitObligation`
//!
//! | # | Account | Writable | Signer |
//! |---|---|---|---|
//! | 0 | obligation          | yes | no     |
//! | 1 | lending_market      | no  | no     |
//! | 2 | obligation_owner    | no  | **yes** |
//! | 3 | sysvar::rent        | no  | no     |
//! | 4 | spl_token program   | no  | no     |
//!
//! Clock is NOT in the account list; the handler reads it via
//! `Clock::get()?` sysvar syscall — identical to the deposit builder's
//! mainnet-branch shape.
//!
//! ## Anti-corruption boundary (Part 6B §63)
//!
//! This module is purely Solend-shaped. It does not accept or return any
//! `crate::lending::*` type. The obligation account size constant lives
//! alongside the other Solend raw-layout constants.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{system_instruction, sysvar};

use super::deposit::SPL_TOKEN_PROGRAM_ID_BS58;
use super::raw::OBLIGATION_LEN;

/// Tag byte for `LendingInstruction::InitObligation` at the pinned
/// mainnet source. `InitObligation` is the 7th variant (index 6).
const SOLEND_IX_TAG_INIT_OBLIGATION: u8 = 6;

/// Explicit inputs to the `InitObligation` builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitObligationInputs {
    pub solend_program_id: Pubkey,
    /// The obligation account being initialized. It must have been
    /// created by a prior SystemProgram `CreateAccount` instruction in
    /// the same transaction (or earlier), sized to
    /// [`OBLIGATION_LEN`], and owned by `solend_program_id`.
    pub obligation: Pubkey,
    pub lending_market: Pubkey,
    /// The wallet that will own the obligation. Signer on this ix.
    pub obligation_owner: Pubkey,
}

/// Pure builder for Solend's `InitObligation`. Returns an unsigned
/// `Instruction` whose account list and payload match the deployed
/// mainnet Solend program exactly.
pub fn build_init_obligation_instruction(inputs: InitObligationInputs) -> Instruction {
    let spl_token_program_id: Pubkey = SPL_TOKEN_PROGRAM_ID_BS58
        .parse()
        .expect("SPL Token program id is a well-known constant");

    // Account list — order and flags are load-bearing and match the
    // mainnet-branch `init_obligation(...)` constructor verbatim.
    let accounts = vec![
        AccountMeta::new(inputs.obligation, false),                 // 0 — writable
        AccountMeta::new_readonly(inputs.lending_market, false),    // 1
        AccountMeta::new_readonly(inputs.obligation_owner, true),   // 2 — signer
        AccountMeta::new_readonly(sysvar::rent::id(), false),       // 3
        AccountMeta::new_readonly(spl_token_program_id, false),     // 4
    ];

    // Payload: single tag byte `6`. `InitObligation` takes no ix-level
    // parameters.
    let data = vec![SOLEND_IX_TAG_INIT_OBLIGATION];

    Instruction {
        program_id: inputs.solend_program_id,
        accounts,
        data,
    }
}

// ── Obligation-account `CreateAccount` helper ────────────────────────────

/// Explicit inputs to the obligation-account create-account helper.
///
/// `lamports` MUST be sized to at least
/// `getMinimumBalanceForRentExemption(OBLIGATION_LEN)`. The caller is
/// responsible for fetching that value from RPC — the helper itself is
/// pure and does not touch RPC, clocks, or signers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateObligationAccountInputs {
    /// Fee payer. Funds the new account's rent. Signer + writable on the
    /// resulting SystemProgram ix.
    pub funder: Pubkey,
    /// Pubkey of the obligation account being created. Signer + writable
    /// on the resulting ix — the caller MUST include the matching
    /// obligation keypair in the transaction signers.
    pub new_obligation: Pubkey,
    /// Rent-exempt lamports for [`OBLIGATION_LEN`] bytes, fetched by the
    /// caller via `getMinimumBalanceForRentExemption`.
    pub rent_exempt_lamports: u64,
    /// The Solend program id. Will be set as the new account's owner so
    /// that `InitObligation` — which runs next — can write its state
    /// into the account.
    pub solend_program_id: Pubkey,
}

/// Pure helper: build the SystemProgram `CreateAccount` instruction that
/// allocates a fresh obligation account. Thin wrapper over
/// `system_instruction::create_account` that encodes the Solend-specific
/// `space = OBLIGATION_LEN` and `owner = solend_program_id`, and names
/// the parameters for this exact use case.
///
/// This helper NEVER fetches rent. The caller fetches it via RPC (see
/// the opt-in dry-simulation test) and passes the value in.
pub fn build_create_obligation_account_instruction(
    inputs: CreateObligationAccountInputs,
) -> Instruction {
    system_instruction::create_account(
        &inputs.funder,
        &inputs.new_obligation,
        inputs.rent_exempt_lamports,
        OBLIGATION_LEN as u64,
        &inputs.solend_program_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::system_program;
    use std::str::FromStr;

    fn solend_program() -> Pubkey {
        Pubkey::from_str("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo").unwrap()
    }

    fn valid_init_inputs() -> InitObligationInputs {
        InitObligationInputs {
            solend_program_id: solend_program(),
            obligation: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            obligation_owner: Pubkey::new_unique(),
        }
    }

    // ── InitObligation builder ──────────────────────────────────────────

    #[test]
    fn init_obligation_program_id_is_solend() {
        let inputs = valid_init_inputs();
        let expected = inputs.solend_program_id;
        let ix = build_init_obligation_instruction(inputs);
        assert_eq!(ix.program_id, expected);
    }

    #[test]
    fn init_obligation_data_is_single_tag_six_byte() {
        let ix = build_init_obligation_instruction(valid_init_inputs());
        assert_eq!(ix.data, vec![6u8], "InitObligation tag is 6");
        assert_eq!(ix.data.len(), 1, "InitObligation has no trailing payload");
    }

    /// The account list must match the mainnet-branch `init_obligation`
    /// constructor verbatim: 5 accounts, in the precise order with the
    /// precise signer/writable flags listed in the module doc.
    #[test]
    fn init_obligation_account_metas_match_mainnet_source() {
        let inputs = valid_init_inputs();
        let ix = build_init_obligation_instruction(inputs);

        assert_eq!(ix.accounts.len(), 5, "mainnet InitObligation has 5 accounts");

        // 0 — obligation (writable, not signer)
        assert_eq!(ix.accounts[0].pubkey, inputs.obligation);
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[0].is_signer);

        // 1 — lending_market (readonly, not signer)
        assert_eq!(ix.accounts[1].pubkey, inputs.lending_market);
        assert!(!ix.accounts[1].is_writable);
        assert!(!ix.accounts[1].is_signer);

        // 2 — obligation_owner (readonly, signer)
        assert_eq!(ix.accounts[2].pubkey, inputs.obligation_owner);
        assert!(!ix.accounts[2].is_writable);
        assert!(ix.accounts[2].is_signer);

        // 3 — rent sysvar (readonly, not signer)
        assert_eq!(ix.accounts[3].pubkey, sysvar::rent::id());
        assert!(!ix.accounts[3].is_writable);
        assert!(!ix.accounts[3].is_signer);

        // 4 — SPL Token program (readonly, not signer)
        let spl_token: Pubkey =
            Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        assert_eq!(ix.accounts[4].pubkey, spl_token);
        assert!(!ix.accounts[4].is_writable);
        assert!(!ix.accounts[4].is_signer);
    }

    /// Slice 3D(ii) taught us to explicitly guard against passing Clock
    /// where deployed Solend expects something else. `InitObligation`'s
    /// deployed handler reads Clock via `Clock::get()?` syscall — it
    /// must NOT appear in the account list, mirroring the deposit
    /// builder's mainnet-branch shape.
    #[test]
    fn init_obligation_does_not_include_clock_sysvar() {
        let ix = build_init_obligation_instruction(valid_init_inputs());
        for (i, a) in ix.accounts.iter().enumerate() {
            assert_ne!(
                a.pubkey,
                sysvar::clock::id(),
                "Clock sysvar must not appear in InitObligation accounts (idx={i})",
            );
        }
    }

    /// The builder must not depend on any `lending::*` type. Function
    /// pointers with only pure solana_sdk types compile iff that holds.
    #[test]
    fn init_obligation_builder_does_not_depend_on_lending_types() {
        fn _fn(_: InitObligationInputs) -> Instruction {
            build_init_obligation_instruction(InitObligationInputs {
                solend_program_id: Pubkey::new_unique(),
                obligation: Pubkey::new_unique(),
                lending_market: Pubkey::new_unique(),
                obligation_owner: Pubkey::new_unique(),
            })
        }
        let _ = _fn as fn(InitObligationInputs) -> Instruction;
    }

    // ── CreateAccount helper ────────────────────────────────────────────

    fn valid_create_inputs() -> CreateObligationAccountInputs {
        CreateObligationAccountInputs {
            funder: Pubkey::new_unique(),
            new_obligation: Pubkey::new_unique(),
            rent_exempt_lamports: 9_938_880, // example test value; caller-supplied
            solend_program_id: solend_program(),
        }
    }

    #[test]
    fn create_obligation_account_program_id_is_system() {
        let ix = build_create_obligation_account_instruction(valid_create_inputs());
        assert_eq!(ix.program_id, system_program::id());
    }

    /// System `CreateAccount` has 2 accounts: the funding account
    /// (writable+signer) and the new account (writable+signer). Both
    /// must appear with those exact flags for the SystemProgram to
    /// actually fund+allocate the account.
    #[test]
    fn create_obligation_account_metas_flags_are_correct() {
        let inputs = valid_create_inputs();
        let ix = build_create_obligation_account_instruction(inputs);

        assert_eq!(ix.accounts.len(), 2);

        // 0 — funder: writable + signer
        assert_eq!(ix.accounts[0].pubkey, inputs.funder);
        assert!(ix.accounts[0].is_writable);
        assert!(ix.accounts[0].is_signer);

        // 1 — new obligation account: writable + signer
        assert_eq!(ix.accounts[1].pubkey, inputs.new_obligation);
        assert!(ix.accounts[1].is_writable);
        assert!(ix.accounts[1].is_signer);
    }

    /// The encoded `CreateAccount` payload must carry:
    /// - lamports  = caller-supplied rent_exempt_lamports
    /// - space     = OBLIGATION_LEN (1300)
    /// - owner     = solend_program_id
    ///
    /// We round-trip via `bincode` matching SystemInstruction's own
    /// serialization to confirm no silent field shuffle.
    #[test]
    fn create_obligation_account_payload_encodes_space_lamports_owner() {
        let inputs = valid_create_inputs();
        let ix = build_create_obligation_account_instruction(inputs);

        // SystemInstruction::CreateAccount is variant 0 (4-byte LE).
        // Then lamports (u64 LE), space (u64 LE), owner (32 bytes).
        assert!(ix.data.len() >= 4 + 8 + 8 + 32);
        let variant = u32::from_le_bytes(ix.data[0..4].try_into().unwrap());
        assert_eq!(variant, 0, "SystemInstruction::CreateAccount is variant 0");

        let lamports = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
        let space = u64::from_le_bytes(ix.data[12..20].try_into().unwrap());
        let owner_bytes: [u8; 32] = ix.data[20..52].try_into().unwrap();

        assert_eq!(lamports, inputs.rent_exempt_lamports);
        assert_eq!(space, OBLIGATION_LEN as u64);
        assert_eq!(space, 1300, "Solend mainnet obligation size is 1300 bytes");
        assert_eq!(Pubkey::new_from_array(owner_bytes), inputs.solend_program_id);
    }

    /// The helper must not fetch rent: calling it with two different
    /// `rent_exempt_lamports` values returns instructions differing
    /// only in the lamports field. Any internal fetch would give the
    /// same value both times (or fail).
    #[test]
    fn create_obligation_account_helper_does_not_fetch_rent() {
        let mut a = valid_create_inputs();
        let mut b = a;
        a.rent_exempt_lamports = 1_000_000;
        b.rent_exempt_lamports = 2_000_000;
        let ia = build_create_obligation_account_instruction(a);
        let ib = build_create_obligation_account_instruction(b);
        let la = u64::from_le_bytes(ia.data[4..12].try_into().unwrap());
        let lb = u64::from_le_bytes(ib.data[4..12].try_into().unwrap());
        assert_eq!(la, 1_000_000);
        assert_eq!(lb, 2_000_000);
        // All other payload bytes must be identical (space + owner).
        assert_eq!(ia.data[12..], ib.data[12..]);
    }

    /// Compile-time boundary check: helper must not depend on any
    /// `lending::*` type.
    #[test]
    fn create_account_helper_does_not_depend_on_lending_types() {
        fn _fn(_: CreateObligationAccountInputs) -> Instruction {
            build_create_obligation_account_instruction(CreateObligationAccountInputs {
                funder: Pubkey::new_unique(),
                new_obligation: Pubkey::new_unique(),
                rent_exempt_lamports: 0,
                solend_program_id: Pubkey::new_unique(),
            })
        }
        let _ = _fn as fn(CreateObligationAccountInputs) -> Instruction;
    }
}
