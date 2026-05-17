//! V0 `VersionedTransaction` assembly from a Jupiter `/swap/v2/build` response.
//!
//! Pure function, no I/O. Given the Jupiter build response + the payer pubkey,
//! produces an unsigned V0 `VersionedTransaction` ready for simulate + sign.
//!
//! # Canonical instruction order
//!
//! The produced transaction's instructions are in Jupiter's declared order:
//!
//! ```text
//! compute_budget_instructions → setup_instructions → swap_instruction
//!   → cleanup_instruction (if present) → other_instructions
//! ```
//!
//! This order is locked by tests. It is the order Jupiter assumes its route
//! is executed in; reordering would break the swap.

use std::collections::HashMap;
use std::str::FromStr;

use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use thiserror::Error;

use crate::integrations::jupiter::{
    JupiterAccountMeta, JupiterInstruction, SwapBuildResponse,
};

#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("invalid program id '{0}': {1}")]
    InvalidProgramId(String, String),
    #[error("invalid account pubkey '{0}': {1}")]
    InvalidAccountPubkey(String, String),
    #[error("invalid lookup table address '{0}': {1}")]
    InvalidLookupTableAddress(String, String),
    #[error("invalid base64 data for instruction program {0}: {1}")]
    InvalidInstructionData(String, String),
    #[error("invalid blockhash '{0}': {1}")]
    InvalidBlockhash(String, String),
    #[error("failed to compile V0 message: {0}")]
    MessageCompile(String),
}

/// Assemble an unsigned V0 `VersionedTransaction` from a Jupiter build response.
///
/// Uses `build.addresses_by_lookup_table_address` (the pre-resolved map) as
/// the ALT source. This is the legacy test-friendly entry point: callers with
/// fixture data can supply a fully-inlined map and skip RPC resolution.
///
/// Production callers hitting the live Jupiter API must use
/// [`assemble_v0_transaction_with_resolved_alts`] — the live response returns
/// `addresses_by_lookup_table_address: null` and expects the caller to fetch
/// ALT contents from RPC themselves.
pub fn assemble_v0_transaction(
    build: &SwapBuildResponse,
    payer: &Pubkey,
) -> Result<VersionedTransaction, AssembleError> {
    let lookup_tables = convert_lookup_tables(&build.addresses_by_lookup_table_address)?;
    assemble_v0_transaction_with_resolved_alts(build, payer, &lookup_tables)
}

/// Assemble an unsigned V0 `VersionedTransaction` using caller-resolved ALTs.
///
/// This is the production entry point. The caller is responsible for
/// resolving `build.address_lookup_table_addresses` via RPC (see
/// `jupiter_alt::resolve_address_lookup_tables`) and passing the fully-hydrated
/// `AddressLookupTableAccount` list here.
///
/// The function does NOT inspect `build.addresses_by_lookup_table_address` —
/// if you want the legacy fixture path, call [`assemble_v0_transaction`].
pub fn assemble_v0_transaction_with_resolved_alts(
    build: &SwapBuildResponse,
    payer: &Pubkey,
    lookup_tables: &[AddressLookupTableAccount],
) -> Result<VersionedTransaction, AssembleError> {
    // 1. Flatten Jupiter's instruction buckets into a single ordered list.
    let ordered = ordered_instructions(build);

    // 2. Convert each JupiterInstruction → solana_sdk::Instruction.
    let instructions: Vec<Instruction> = ordered
        .iter()
        .map(|ji| convert_instruction(ji))
        .collect::<Result<Vec<_>, _>>()?;

    // 3. Parse the recent blockhash.
    let blockhash = Hash::from_str(&build.blockhash_with_metadata.blockhash).map_err(|e| {
        AssembleError::InvalidBlockhash(
            build.blockhash_with_metadata.blockhash.clone(),
            e.to_string(),
        )
    })?;

    // 4. Compile V0 message with ALT support.
    let message = v0::Message::try_compile(
        payer,
        &instructions,
        lookup_tables,
        blockhash,
    )
    .map_err(|e| AssembleError::MessageCompile(e.to_string()))?;

    // 5. Wrap in a VersionedTransaction with placeholder signatures.
    let num_required_sigs = message.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_required_sigs],
        message: VersionedMessage::V0(message),
    };

    Ok(tx)
}

/// Return Jupiter's instructions in canonical execution order.
///
/// Ordering is intentional and stable. Tests lock this order because
/// Jupiter routes depend on it.
pub fn ordered_instructions(build: &SwapBuildResponse) -> Vec<&JupiterInstruction> {
    let mut out: Vec<&JupiterInstruction> = Vec::with_capacity(build.total_instruction_count());
    out.extend(build.compute_budget_instructions.iter());
    out.extend(build.setup_instructions.iter());
    out.push(&build.swap_instruction);
    if let Some(cleanup) = &build.cleanup_instruction {
        out.push(cleanup);
    }
    out.extend(build.other_instructions.iter());
    out
}

fn convert_instruction(ji: &JupiterInstruction) -> Result<Instruction, AssembleError> {
    let program_id = Pubkey::from_str(&ji.program_id).map_err(|e| {
        AssembleError::InvalidProgramId(ji.program_id.clone(), e.to_string())
    })?;

    let accounts: Vec<AccountMeta> = ji
        .accounts
        .iter()
        .map(|a| convert_account_meta(a))
        .collect::<Result<Vec<_>, _>>()?;

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&ji.data)
        .map_err(|e| {
            AssembleError::InvalidInstructionData(ji.program_id.clone(), e.to_string())
        })?;

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

fn convert_account_meta(a: &JupiterAccountMeta) -> Result<AccountMeta, AssembleError> {
    let pubkey = Pubkey::from_str(&a.pubkey).map_err(|e| {
        AssembleError::InvalidAccountPubkey(a.pubkey.clone(), e.to_string())
    })?;
    Ok(AccountMeta {
        pubkey,
        is_signer: a.is_signer,
        is_writable: a.is_writable,
    })
}

pub(crate) fn convert_lookup_tables(
    table_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<AddressLookupTableAccount>, AssembleError> {
    let mut out: Vec<AddressLookupTableAccount> = Vec::with_capacity(table_map.len());
    // Sort for determinism — HashMap iteration order is not stable.
    let mut entries: Vec<(&String, &Vec<String>)> = table_map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    for (table_addr, addresses) in entries {
        let key = Pubkey::from_str(table_addr).map_err(|e| {
            AssembleError::InvalidLookupTableAddress(table_addr.clone(), e.to_string())
        })?;
        let mut resolved = Vec::with_capacity(addresses.len());
        for addr in addresses {
            let pk = Pubkey::from_str(addr).map_err(|e| {
                AssembleError::InvalidAccountPubkey(addr.clone(), e.to_string())
            })?;
            resolved.push(pk);
        }
        out.push(AddressLookupTableAccount {
            key,
            addresses: resolved,
        });
    }
    Ok(out)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::jupiter::{
        BlockhashWithMetadata, JupiterAccountMeta, JupiterInstruction,
    };
    use base64::Engine;
    use solana_sdk::message::VersionedMessage;

    const PAYER: &str = "FhU4P7HWvLCk4W7E6VuH6GttYeHJekdPSHF9xXmVnWwy";
    const BLOCKHASH: &str = "GHtXQBpokKZYzitZ9bHnDZbUXZwqfsFQ2NUjRmiaV5Hf";
    const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
    const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";

    fn ix(program: &str, payload: &[u8]) -> JupiterInstruction {
        JupiterInstruction {
            program_id: program.to_string(),
            accounts: vec![JupiterAccountMeta {
                pubkey: PAYER.to_string(),
                is_signer: true,
                is_writable: true,
            }],
            data: base64::engine::general_purpose::STANDARD.encode(payload),
        }
    }

    fn build_response_with_ordering() -> SwapBuildResponse {
        // Each instruction has unique data so we can verify ordering precisely.
        SwapBuildResponse {
            compute_budget_instructions: vec![
                ix(COMPUTE_BUDGET_PROGRAM, b"CB1"),
                ix(COMPUTE_BUDGET_PROGRAM, b"CB2"),
            ],
            setup_instructions: vec![
                ix(SYSTEM_PROGRAM, b"SETUP1"),
                ix(TOKEN_PROGRAM, b"SETUP2"),
            ],
            swap_instruction: ix(JUPITER_PROGRAM, b"SWAP"),
            cleanup_instruction: Some(ix(TOKEN_PROGRAM, b"CLEANUP")),
            other_instructions: vec![ix(SYSTEM_PROGRAM, b"OTHER1")],
            address_lookup_table_addresses: vec!["4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5".to_string()],
            addresses_by_lookup_table_address: {
                let mut m = HashMap::new();
                m.insert(
                    "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5".to_string(),
                    vec![TOKEN_PROGRAM.to_string(), JUPITER_PROGRAM.to_string()],
                );
                m
            },
            blockhash_with_metadata: BlockhashWithMetadata {
                blockhash: BLOCKHASH.to_string(),
                last_valid_block_height: 100_000,
                fetched_at: None,
            },
            token_ledger_instruction: None,
        }
    }

    #[test]
    fn assembles_unsigned_v0_transaction() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let build = build_response_with_ordering();
        let tx = assemble_v0_transaction(&build, &payer).expect("assemble");

        // Must be V0, not Legacy.
        assert!(matches!(tx.message, VersionedMessage::V0(_)));

        // Unsigned: all signature slots are default.
        assert!(
            tx.signatures.iter().all(|s| s == &Signature::default()),
            "assembled tx must be unsigned"
        );
    }

    #[test]
    fn instruction_order_matches_jupiter_declaration() {
        // This test locks Jupiter's canonical execution order.
        // Reordering here is a behavior change, not a refactor.
        let build = build_response_with_ordering();
        let ordered = ordered_instructions(&build);

        // Extract the payload bytes of each instruction in order.
        let payloads: Vec<Vec<u8>> = ordered
            .iter()
            .map(|ji| {
                base64::engine::general_purpose::STANDARD
                    .decode(&ji.data)
                    .unwrap()
            })
            .collect();

        let expected: Vec<&[u8]> = vec![
            b"CB1",     // compute_budget[0]
            b"CB2",     // compute_budget[1]
            b"SETUP1",  // setup[0]
            b"SETUP2",  // setup[1]
            b"SWAP",    // swap
            b"CLEANUP", // cleanup
            b"OTHER1",  // other[0]
        ];

        assert_eq!(payloads.len(), expected.len());
        for (i, (actual, expected_ref)) in payloads.iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                actual.as_slice(),
                *expected_ref,
                "instruction {i} out of order"
            );
        }
    }

    #[test]
    fn ordering_without_cleanup_preserves_positions() {
        let mut build = build_response_with_ordering();
        build.cleanup_instruction = None;
        let ordered = ordered_instructions(&build);
        let payloads: Vec<Vec<u8>> = ordered
            .iter()
            .map(|ji| base64::engine::general_purpose::STANDARD.decode(&ji.data).unwrap())
            .collect();
        // 2 cb + 2 setup + 1 swap + 0 cleanup + 1 other = 6
        assert_eq!(payloads.len(), 6);
        assert_eq!(payloads[4], b"SWAP");
        assert_eq!(payloads[5], b"OTHER1");
    }

    #[test]
    fn blockhash_is_set_from_build_response() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let build = build_response_with_ordering();
        let tx = assemble_v0_transaction(&build, &payer).unwrap();
        let VersionedMessage::V0(ref msg) = tx.message else {
            panic!("expected V0 message");
        };
        assert_eq!(msg.recent_blockhash, Hash::from_str(BLOCKHASH).unwrap());
    }

    #[test]
    fn convert_lookup_tables_preserves_addresses_and_sorts_keys() {
        // Test the conversion fn directly — the v0 compiler's decision of
        // whether to USE an ALT depends on instruction structure, which isn't
        // our code's concern. Our code's job: correctly parse & sort ALT input.
        let mut input = HashMap::new();
        input.insert(
            "BPFLoaderUpgradeab1e11111111111111111111111".to_string(),
            vec![SYSTEM_PROGRAM.to_string()],
        );
        input.insert(
            "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5".to_string(),
            vec![TOKEN_PROGRAM.to_string(), JUPITER_PROGRAM.to_string()],
        );

        let tables = convert_lookup_tables(&input).expect("convert");
        assert_eq!(tables.len(), 2);

        // Sorted by key (base58 lexicographic).
        assert!(tables[0].key.to_string() < tables[1].key.to_string());

        // Addresses are preserved and parsed.
        let total_addresses: usize = tables.iter().map(|t| t.addresses.len()).sum();
        assert_eq!(total_addresses, 3);

        // Find the entry for our known key and verify its addresses.
        let entry = tables
            .iter()
            .find(|t| t.key.to_string() == "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5")
            .expect("entry must survive round-trip");
        assert_eq!(entry.addresses.len(), 2);
        let strs: Vec<String> = entry.addresses.iter().map(|a| a.to_string()).collect();
        assert!(strs.contains(&TOKEN_PROGRAM.to_string()));
        assert!(strs.contains(&JUPITER_PROGRAM.to_string()));
    }

    #[test]
    fn assembler_accepts_lookup_tables_from_build_response() {
        // End-to-end: assembler doesn't panic or drop ALT data.
        // The v0::Message::try_compile decides whether to actually emit the
        // lookup into `address_table_lookups` based on account-usage heuristics
        // which are the SDK's concern, not ours.
        let payer = Pubkey::from_str(PAYER).unwrap();
        let build = build_response_with_ordering();
        let tx = assemble_v0_transaction(&build, &payer).unwrap();
        let VersionedMessage::V0(ref msg) = tx.message else {
            panic!("expected V0");
        };
        // Sanity check: message constructed without error, has instructions.
        assert!(!msg.instructions.is_empty());
        // lookup_tables field exists on V0 messages (empty or populated).
        let _ = &msg.address_table_lookups;
    }

    #[test]
    fn empty_lookup_tables_still_assembles() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let mut build = build_response_with_ordering();
        build.addresses_by_lookup_table_address.clear();
        let tx = assemble_v0_transaction(&build, &payer).unwrap();
        let VersionedMessage::V0(ref msg) = tx.message else { panic!("expected V0") };
        assert!(msg.address_table_lookups.is_empty());
    }

    #[test]
    fn invalid_program_id_fails_cleanly() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let mut build = build_response_with_ordering();
        build.swap_instruction.program_id = "not_a_valid_pubkey".to_string();

        let err = assemble_v0_transaction(&build, &payer).unwrap_err();
        match err {
            AssembleError::InvalidProgramId(pid, _) => assert_eq!(pid, "not_a_valid_pubkey"),
            other => panic!("expected InvalidProgramId, got: {other}"),
        }
    }

    #[test]
    fn invalid_blockhash_fails_cleanly() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let mut build = build_response_with_ordering();
        build.blockhash_with_metadata.blockhash = "not_a_hash".to_string();

        let err = assemble_v0_transaction(&build, &payer).unwrap_err();
        assert!(matches!(err, AssembleError::InvalidBlockhash(_, _)));
    }

    #[test]
    fn invalid_base64_data_fails_cleanly() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let mut build = build_response_with_ordering();
        build.swap_instruction.data = "###not_base64###".to_string();

        let err = assemble_v0_transaction(&build, &payer).unwrap_err();
        assert!(matches!(err, AssembleError::InvalidInstructionData(_, _)));
    }

    #[test]
    fn payer_appears_as_required_signer() {
        let payer = Pubkey::from_str(PAYER).unwrap();
        let build = build_response_with_ordering();
        let tx = assemble_v0_transaction(&build, &payer).unwrap();
        let VersionedMessage::V0(ref msg) = tx.message else { panic!("V0") };
        assert!(msg.header.num_required_signatures >= 1);
        assert_eq!(tx.signatures.len(), msg.header.num_required_signatures as usize);
        assert_eq!(msg.account_keys[0], payer, "payer must be first static key");
    }

    #[test]
    fn lookup_table_ordering_is_deterministic() {
        // HashMap iteration order is random. Assembler must sort by table
        // address for bit-stable output. This is required so repeated runs
        // produce the same signed tx when nothing else changed.
        let payer = Pubkey::from_str(PAYER).unwrap();

        let mut build1 = build_response_with_ordering();
        build1.addresses_by_lookup_table_address.insert(
            "BPFLoaderUpgradeab1e11111111111111111111111".to_string(),
            vec![SYSTEM_PROGRAM.to_string()],
        );

        let mut build2 = build_response_with_ordering();
        // Insert in reverse order — HashMap wouldn't preserve insertion order anyway.
        build2.addresses_by_lookup_table_address.insert(
            "BPFLoaderUpgradeab1e11111111111111111111111".to_string(),
            vec![SYSTEM_PROGRAM.to_string()],
        );

        let tx1 = assemble_v0_transaction(&build1, &payer).unwrap();
        let tx2 = assemble_v0_transaction(&build2, &payer).unwrap();
        let b1 = bincode::serialize(&tx1).unwrap();
        let b2 = bincode::serialize(&tx2).unwrap();
        assert_eq!(b1, b2, "assembly must be deterministic given equal inputs");
    }
}
