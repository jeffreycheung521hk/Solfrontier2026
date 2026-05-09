//! Stage 2 — Jupiter V2 sizing measurement harness (default-skip).
//!
//! # Purpose
//!
//! Answer the J-prep feasibility report's measurement gates by hitting the
//! live Jupiter `/swap/v2/build` endpoint, wrapping the returned instruction
//! list with a synthetic Stage 2 bracket (3 Pyth feeds + instructions sysvar
//! + Authorization PDA + delegated wallet + token-program / ATA-program /
//! system-program / compute-budget / clawsol-authority pre/post checks) and
//! computing serialized v0 transaction bytes. Iterates over the
//! `maxAccounts` matrix (64, 58, 55, 52, 50, 48) so the report can pin which
//! values keep the bracket-wrapped tx under the 1232-byte cap.
//!
//! # Safety contract
//!
//! - **NO_SIGNING.** No keypair is ever loaded.
//! - **NO_BROADCAST.** No `sendTransaction` / `sendRawTransaction` is ever
//!   called.
//! - **NO_TRANSACTION_SUBMISSION.** No mainnet/devnet write of any kind.
//! - **FETCH_BUILD_ONLY.** Live calls are limited to `GET /swap/v2/build`.
//!   Even `simulateTransaction` is excluded from this slice.
//!
//! # Default behavior (no env)
//!
//! The test self-skips and returns `Ok`. CI does NOT set
//! `CLAW_STAGE2_JUPITER_SIZING`; deterministic pipelines are unaffected.
//!
//! # To run the live measurement
//!
//! ```bash
//! CLAW_STAGE2_JUPITER_SIZING=1 \
//!   CLAW_JUPITER_BASE_URL=https://api.jup.ag \
//!   cargo test -p claw-gateway --test live_jupiter_stage2_sizing -- --nocapture
//! ```
//!
//! Optional overrides:
//! - `CLAW_STAGE2_JUPITER_INPUT_MINT` (default USDC).
//! - `CLAW_STAGE2_JUPITER_OUTPUT_MINT` (default wSOL).
//! - `CLAW_STAGE2_JUPITER_AMOUNT_RAW` (default 5_000_000 = 5 USDC).
//! - `CLAW_STAGE2_JUPITER_SLIPPAGE_BPS` (default 50).
//! - `CLAW_JUPITER_API_KEY` (forwarded as `x-api-key`; required in some
//!   tiers, optional otherwise).
//!
//! # Output artifact
//!
//! Writes `docs/proofs/stage2_jupiter_sizing_fixtures.json` with one row per
//! `maxAccounts` value plus scenario / env / safety metadata. The companion
//! markdown `docs/proofs/STAGE2_JUPITER_SIZING_MEASUREMENT.md` documents
//! usage and (when measurement runs) summarizes the matrix.
//!
//! # Why a separate response struct
//!
//! `claw_gateway::integrations::jupiter::SwapBuildResponse` is the production
//! v1 wire-shape and does not carry `tipInstruction`. The harness defines a
//! local `V2BuildResponse` so the v2 `tipInstruction` and the optional
//! `routeLabel` field are observable in the JSON fixture without modifying
//! the production type.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize};
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};

use claw_gateway::integrations::jupiter::{
    BlockhashWithMetadata, JupiterAccountMeta, JupiterInstruction,
};

// ── Env keys ────────────────────────────────────────────────────────────────

const ENV_GATE: &str = "CLAW_STAGE2_JUPITER_SIZING";
const ENV_BASE_URL: &str = "CLAW_JUPITER_BASE_URL";
const ENV_API_KEY: &str = "CLAW_JUPITER_API_KEY";
const ENV_INPUT_MINT: &str = "CLAW_STAGE2_JUPITER_INPUT_MINT";
const ENV_OUTPUT_MINT: &str = "CLAW_STAGE2_JUPITER_OUTPUT_MINT";
const ENV_AMOUNT_RAW: &str = "CLAW_STAGE2_JUPITER_AMOUNT_RAW";
const ENV_SLIPPAGE_BPS: &str = "CLAW_STAGE2_JUPITER_SLIPPAGE_BPS";

// ── Defaults ────────────────────────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://api.jup.ag";
const DEFAULT_INPUT_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // USDC
const DEFAULT_OUTPUT_MINT: &str = "So11111111111111111111111111111111111111112"; // wSOL
const DEFAULT_AMOUNT_RAW: u64 = 5_000_000; // 5 USDC at 1e6
const DEFAULT_SLIPPAGE_BPS: u16 = 50;
const DEFAULT_RESTRICT_INTERMEDIATE_TOKENS: bool = true;

/// `maxAccounts` matrix from prompt § 2 — descending probe of route width.
const MAX_ACCOUNTS_MATRIX: &[u16] = &[64, 58, 55, 52, 50, 48];

/// Solana wire packet cap per `solana.com/docs/core/transactions` (2026-05-09).
const PACKET_DATA_SIZE: usize = 1232;

/// Date the harness re-confirmed Jupiter / Solana docs.
const OFFICIAL_DOCS_CHECKED_AT: &str = "2026-05-09";

// ── Known program / sysvar pubkeys (per J-prep § 5.2 / § 6.1) ──────────────

const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSVAR_INSTRUCTIONS: &str = "Sysvar1nstructions1111111111111111111111111";
/// Pyth Solana receiver program id (Pyth Pull oracle on Solana mainnet).
const PYTH_RECEIVER_PROGRAM: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";

// ── Safety markers (echoed to stdout for human review) ─────────────────────

const SAFETY_NO_SIGNING: &str = "NO_SIGNING";
const SAFETY_NO_BROADCAST: &str = "NO_BROADCAST";
const SAFETY_NO_TX_SUBMIT: &str = "NO_TRANSACTION_SUBMISSION";
const SAFETY_FETCH_BUILD_ONLY: &str = "FETCH_BUILD_ONLY";

// ── Output artifact paths (relative to repo root, resolved via CARGO_MANIFEST_DIR)

const FIXTURE_JSON_RELPATH: &str = "../../docs/proofs/stage2_jupiter_sizing_fixtures.json";

// ───────────────────────────────────────────────────────────────────────────
// V2 build response — local mirror that captures tipInstruction
// ───────────────────────────────────────────────────────────────────────────

/// Accept either an absent / null / map for the lookup-table map field. The
/// live API returns `null`; some test fixtures populate a map. Lifted from
/// the production module so the harness stays self-contained.
fn null_or_map<'de, D>(deserializer: D) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<HashMap<String, Vec<String>>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct V2BuildResponse {
    #[serde(default)]
    compute_budget_instructions: Vec<JupiterInstruction>,
    #[serde(default)]
    setup_instructions: Vec<JupiterInstruction>,
    swap_instruction: JupiterInstruction,
    #[serde(default)]
    cleanup_instruction: Option<JupiterInstruction>,
    #[serde(default)]
    other_instructions: Vec<JupiterInstruction>,
    /// v2 addition: optional Jito tip transfer ix (only present when
    /// `tipAmount` is supplied in the request — this harness does NOT supply
    /// `tipAmount`, so this should generally be absent).
    #[serde(default)]
    tip_instruction: Option<JupiterInstruction>,
    #[serde(default)]
    address_lookup_table_addresses: Vec<String>,
    #[serde(default, deserialize_with = "null_or_map")]
    addresses_by_lookup_table_address: HashMap<String, Vec<String>>,
    /// Optional in the wire response when Jupiter declines to attach a
    /// blockhash (e.g. no-route paths). When present, this is forwarded into
    /// the synthetic v0 message; when absent, a dummy blockhash is used.
    #[serde(default)]
    blockhash_with_metadata: Option<BlockhashWithMetadata>,
    /// Some Jupiter responses carry a `routeLabel` summary string.
    #[serde(default, alias = "routeLabel")]
    route_label: Option<String>,
}

// ───────────────────────────────────────────────────────────────────────────
// JSON fixture schema
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Scenario {
    input_mint: String,
    output_mint: String,
    amount_raw: u64,
    slippage_bps: u16,
    restrict_intermediate_tokens: bool,
}

#[derive(Debug, Clone, Serialize)]
struct EnvUsed {
    sizing_enabled: bool,
    api_key_supplied: bool,
    base_url: String,
    input_mint_overridden: bool,
    output_mint_overridden: bool,
    amount_overridden: bool,
    slippage_overridden: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
enum Classification {
    Green,
    Yellow,
    Red,
    NoRoute,
    /// Reserved for future CU-gate slice — Jupiter returned a build but the
    /// CU estimate is unobtainable without simulate. Listed in the JSON
    /// schema (`STAGE2_JUPITER_SIZING_MEASUREMENT.md` § 7) so consumers can
    /// match exhaustively across slices; not produced by this slice.
    #[allow(dead_code)]
    Unknown,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct MaxAccountsRow {
    max_accounts: u16,
    restrict_intermediate_tokens: bool,
    /// Best-effort label from the response if Jupiter included one.
    route_label: Option<String>,
    setup_instructions_count: usize,
    compute_budget_instructions_count: usize,
    swap_instruction_present: bool,
    cleanup_instruction_present: bool,
    other_instructions_count: usize,
    tip_instruction_present: bool,
    blockhash_with_metadata_present: bool,
    /// Number of unique ALT addresses Jupiter returned in
    /// `addressLookupTableAddresses`.
    address_lookup_tables_count: usize,
    /// Number of addresses Jupiter inlined (only ever non-null in fixture
    /// data; live responses keep this null per `jupiter.rs` notes).
    alt_loaded_addresses_count: Option<usize>,
    /// Static account count of the unwrapped Jupiter ix list (no bracket).
    static_account_count_before_bracket: Option<usize>,
    /// Static account count after wrapping with synthetic pre/post checks.
    static_account_count_after_bracket: Option<usize>,
    /// Total resolved (static + ALT-indexed) account count after bracket.
    resolved_account_count_after_bracket: Option<usize>,
    /// `bincode::serialize(&v0_versioned_tx).len()` after bracket wrap.
    /// Compared against `PACKET_DATA_SIZE = 1232`.
    serialized_v0_tx_bytes_after_bracket: Option<usize>,
    /// CU pressure: Jupiter `/build` does not currently expose a CU hint
    /// without a separate simulate call. Always `None` in this slice (per
    /// FETCH_BUILD_ONLY); future slices may add an opt-in CU probe.
    cu_estimate: Option<u64>,
    classification: Classification,
    /// Surfaced http / parse / no-route message — empty when the row is
    /// classified Green/Yellow/Red (i.e. Jupiter returned a valid build).
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    generated_at: String,
    official_docs_checked_at: String,
    jupiter_base_url: String,
    scenario: Scenario,
    env_used: EnvUsed,
    rows: Vec<MaxAccountsRow>,
    safety_markers: Vec<&'static str>,
    /// Echoes `PACKET_DATA_SIZE` so downstream readers can confirm what cap
    /// the classifications were measured against.
    packet_data_size: usize,
    notes: Vec<String>,
}

// ───────────────────────────────────────────────────────────────────────────
// Helpers
// ───────────────────────────────────────────────────────────────────────────

fn print_safety_banner() {
    println!("══════════════════════════════════════════════════════════════");
    println!("  Stage 2 Jupiter V2 sizing harness — measurement-only");
    println!("══════════════════════════════════════════════════════════════");
    println!("  {SAFETY_NO_SIGNING}");
    println!("  {SAFETY_NO_BROADCAST}");
    println!("  {SAFETY_NO_TX_SUBMIT}");
    println!("  {SAFETY_FETCH_BUILD_ONLY}");
    println!("══════════════════════════════════════════════════════════════");
}

fn skip_unless_opted_in() -> bool {
    match env::var(ENV_GATE) {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => false,
        _ => {
            eprintln!(
                "SKIP: set {ENV_GATE}=1 to run the Stage 2 Jupiter V2 sizing \
                 harness. Default behaviour: self-skip and pass. The harness \
                 only calls GET /swap/v2/build (no signing, no broadcast, no \
                 transaction submission)."
            );
            true
        }
    }
}

/// Read an env var, falling back to `default`. Returns `(value, overridden)`.
fn env_or<S: Into<String>>(key: &str, default: S) -> (String, bool) {
    match env::var(key) {
        Ok(v) if !v.is_empty() => (v, true),
        _ => (default.into(), false),
    }
}

fn classify_bytes(bytes: usize) -> Classification {
    // Green = comfortable headroom (≥ ~10% under cap, i.e. ≤ 1100 bytes).
    // Yellow = within cap but tight headroom (1101..=1232).
    // Red = over cap.
    if bytes <= 1100 {
        Classification::Green
    } else if bytes <= PACKET_DATA_SIZE {
        Classification::Yellow
    } else {
        Classification::Red
    }
}

fn classification_label(c: &Classification) -> &'static str {
    match c {
        Classification::Green => "Green",
        Classification::Yellow => "Yellow",
        Classification::Red => "Red",
        Classification::NoRoute => "NoRoute",
        Classification::Unknown => "Unknown",
        Classification::Error => "Error",
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Bracket synthesis (per J-prep § 4.4 / § 6.1)
// ───────────────────────────────────────────────────────────────────────────

/// The synthetic Stage 2 bracket account set. Using one consistent value for
/// the delegated wallet across the bracket AND the Jupiter `taker` query
/// parameter keeps the message compiler from double-counting the same logical
/// signer slot.
struct BracketAccounts {
    delegated_wallet: Pubkey,
    authorization_pda: Pubkey,
    clawsol_authority_program: Pubkey,
    pyth_receiver_program: Pubkey,
    pyth_btc_price_update: Pubkey,
    pyth_eth_price_update: Pubkey,
    pyth_sol_price_update: Pubkey,
    delegated_input_token_account: Pubkey,
    delegated_output_token_account: Pubkey,
    destination_token_account: Pubkey,
    instructions_sysvar: Pubkey,
}

impl BracketAccounts {
    fn new() -> Self {
        Self {
            delegated_wallet: Pubkey::new_unique(),
            authorization_pda: Pubkey::new_unique(),
            clawsol_authority_program: Pubkey::new_unique(),
            pyth_receiver_program: Pubkey::from_str(PYTH_RECEIVER_PROGRAM)
                .expect("known pyth receiver program id"),
            pyth_btc_price_update: Pubkey::new_unique(),
            pyth_eth_price_update: Pubkey::new_unique(),
            pyth_sol_price_update: Pubkey::new_unique(),
            delegated_input_token_account: Pubkey::new_unique(),
            delegated_output_token_account: Pubkey::new_unique(),
            destination_token_account: Pubkey::new_unique(),
            instructions_sysvar: Pubkey::from_str(SYSVAR_INSTRUCTIONS)
                .expect("known instructions sysvar id"),
        }
    }
}

/// Build the `pre_check_v2` instruction (J-prep § 4.4). Account list and data
/// size are conservative estimates — the goal is wire-form sizing, not real
/// execution. The data is a 32-byte placeholder representing typical input
/// (rule_id hash + amount cap + 1 byte tag).
fn synth_pre_check_ix(b: &BracketAccounts) -> Instruction {
    Instruction {
        program_id: b.clawsol_authority_program,
        accounts: vec![
            AccountMeta::new_readonly(b.delegated_wallet, true),
            AccountMeta::new(b.authorization_pda, false),
            AccountMeta::new_readonly(b.instructions_sysvar, false),
            AccountMeta::new_readonly(b.pyth_btc_price_update, false),
            AccountMeta::new_readonly(b.pyth_eth_price_update, false),
            AccountMeta::new_readonly(b.pyth_sol_price_update, false),
            AccountMeta::new_readonly(b.delegated_input_token_account, false),
            AccountMeta::new_readonly(b.delegated_output_token_account, false),
            AccountMeta::new_readonly(b.pyth_receiver_program, false),
        ],
        // 32-byte placeholder; J-prep § 7.2 budgeted ~32 bytes for pre-check
        // data (rule_id seed + cap fields).
        data: vec![0u8; 32],
    }
}

/// Build the `post_check_v2` instruction (J-prep § 4.4). 24-byte placeholder
/// for delta-check arguments + tag.
fn synth_post_check_ix(b: &BracketAccounts) -> Instruction {
    Instruction {
        program_id: b.clawsol_authority_program,
        accounts: vec![
            AccountMeta::new_readonly(b.delegated_wallet, true),
            AccountMeta::new(b.authorization_pda, false),
            AccountMeta::new_readonly(b.instructions_sysvar, false),
            AccountMeta::new_readonly(b.delegated_input_token_account, false),
            AccountMeta::new(b.destination_token_account, false),
        ],
        data: vec![0u8; 24],
    }
}

/// Convert a Jupiter wire-shape ix into a `solana_sdk::Instruction`. Returns
/// `Err` with a descriptive message if any pubkey or base64 payload is
/// malformed — the harness records the error in the row instead of panicking.
fn convert_jupiter_instruction(ji: &JupiterInstruction) -> Result<Instruction, String> {
    let program_id = Pubkey::from_str(&ji.program_id)
        .map_err(|e| format!("invalid program_id {}: {e}", ji.program_id))?;
    let accounts = ji
        .accounts
        .iter()
        .map(|a| -> Result<AccountMeta, String> {
            let pubkey = Pubkey::from_str(&a.pubkey)
                .map_err(|e| format!("invalid account pubkey {}: {e}", a.pubkey))?;
            Ok(AccountMeta {
                pubkey,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&ji.data)
        .map_err(|e| format!("invalid base64 data on ix {}: {e}", ji.program_id))?;
    Ok(Instruction { program_id, accounts, data })
}

/// Flatten the V2 response into ordered Jupiter ixs:
///   compute_budget → setup → swap → cleanup → other → tip
fn ordered_jupiter_ixs(build: &V2BuildResponse) -> Vec<&JupiterInstruction> {
    let mut out = Vec::with_capacity(
        build.compute_budget_instructions.len()
            + build.setup_instructions.len()
            + 1
            + build.cleanup_instruction.is_some() as usize
            + build.other_instructions.len()
            + build.tip_instruction.is_some() as usize,
    );
    out.extend(build.compute_budget_instructions.iter());
    out.extend(build.setup_instructions.iter());
    out.push(&build.swap_instruction);
    if let Some(cleanup) = &build.cleanup_instruction {
        out.push(cleanup);
    }
    out.extend(build.other_instructions.iter());
    if let Some(tip) = &build.tip_instruction {
        out.push(tip);
    }
    out
}

/// Build a synthetic ALT containing every account pubkey referenced anywhere
/// in the Jupiter ix list. The v0 message compiler will independently decide
/// which accounts go in static vs. ALT-loaded based on writability/signers,
/// so an over-broad synthetic ALT is safe — unused entries cost nothing in
/// the wire form. Returns one ALT per real Jupiter ALT pubkey, with the same
/// over-broad address set, so the per-ALT header overhead in the wire form
/// is realistic.
fn synthesize_alts(build: &V2BuildResponse) -> Vec<AddressLookupTableAccount> {
    let mut all_jupiter_accounts: HashSet<Pubkey> = HashSet::new();
    for ji in ordered_jupiter_ixs(build) {
        for am in &ji.accounts {
            if let Ok(pk) = Pubkey::from_str(&am.pubkey) {
                all_jupiter_accounts.insert(pk);
            }
        }
    }
    let addresses: Vec<Pubkey> = all_jupiter_accounts.into_iter().collect();

    if build.address_lookup_table_addresses.is_empty() {
        return Vec::new();
    }

    build
        .address_lookup_table_addresses
        .iter()
        .filter_map(|a| Pubkey::from_str(a).ok())
        .map(|key| AddressLookupTableAccount {
            key,
            addresses: addresses.clone(),
        })
        .collect()
}

/// Stable dummy blockhash used when Jupiter does not supply one (or when the
/// supplied one fails to parse). The choice does not affect byte size.
fn fallback_blockhash() -> Hash {
    Hash::new_from_array([7u8; 32])
}

/// Compute the byte- and account-pressure metrics for a given V2 build
/// response wrapped in the synthetic Stage 2 bracket.
struct BracketMetrics {
    /// Static account count of the *unwrapped* Jupiter ixs (no bracket).
    static_before_bracket: usize,
    /// Static account count after wrapping with bracket pre/post.
    static_after_bracket: usize,
    /// Total resolved (static + ALT writable + ALT readonly) accounts.
    resolved_after_bracket: usize,
    /// `bincode::serialize` length of the unsigned VersionedTransaction.
    serialized_bytes: usize,
}

fn measure_bracket(
    build: &V2BuildResponse,
    bracket: &BracketAccounts,
) -> Result<BracketMetrics, String> {
    // 1. Convert Jupiter ixs.
    let mut jupiter_ixs: Vec<Instruction> = Vec::new();
    for ji in ordered_jupiter_ixs(build) {
        jupiter_ixs.push(convert_jupiter_instruction(ji)?);
    }

    // 2. Synthesize ALTs from the response's ALT addresses.
    let alts = synthesize_alts(build);

    // 3. Establish the blockhash (fallback if Jupiter gave none / unparseable).
    let blockhash = build
        .blockhash_with_metadata
        .as_ref()
        .and_then(|m| Hash::from_str(&m.blockhash).ok())
        .unwrap_or_else(fallback_blockhash);

    // 4. Measure UNWRAPPED Jupiter-only message (without bracket) to record the
    //    static-before-bracket count. We do NOT reuse this for byte sizing —
    //    the byte sizing always reports the bracket-wrapped tx.
    let unwrapped_msg = v0::Message::try_compile(
        &bracket.delegated_wallet,
        &jupiter_ixs,
        &alts,
        blockhash,
    )
    .map_err(|e| format!("unwrapped v0 compile: {e}"))?;
    let static_before_bracket = unwrapped_msg.account_keys.len();

    // 5. Wrap with synthetic pre/post checks per § 4.4 (Variant A: two ixs).
    let mut wrapped_ixs: Vec<Instruction> = Vec::with_capacity(jupiter_ixs.len() + 2);
    wrapped_ixs.push(synth_pre_check_ix(bracket));
    wrapped_ixs.extend(jupiter_ixs.into_iter());
    wrapped_ixs.push(synth_post_check_ix(bracket));

    let wrapped_msg = v0::Message::try_compile(
        &bracket.delegated_wallet,
        &wrapped_ixs,
        &alts,
        blockhash,
    )
    .map_err(|e| format!("wrapped v0 compile: {e}"))?;

    let static_after_bracket = wrapped_msg.account_keys.len();
    let alt_loaded: usize = wrapped_msg
        .address_table_lookups
        .iter()
        .map(|l| l.writable_indexes.len() + l.readonly_indexes.len())
        .sum();
    let resolved_after_bracket = static_after_bracket + alt_loaded;

    let num_required_sigs = wrapped_msg.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_required_sigs],
        message: VersionedMessage::V0(wrapped_msg),
    };
    let serialized_bytes = bincode::serialize(&tx)
        .map_err(|e| format!("bincode::serialize: {e}"))?
        .len();

    Ok(BracketMetrics {
        static_before_bracket,
        static_after_bracket,
        resolved_after_bracket,
        serialized_bytes,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// HTTP — GET /swap/v2/build
// ───────────────────────────────────────────────────────────────────────────

async fn fetch_v2_build(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    scenario: &Scenario,
    taker: &Pubkey,
    max_accounts: u16,
) -> Result<V2BuildResponse, String> {
    let url = format!("{base_url}/swap/v2/build");
    let amount_str = scenario.amount_raw.to_string();
    let max_accounts_str = max_accounts.to_string();
    let slippage_str = scenario.slippage_bps.to_string();
    let restrict_str = scenario.restrict_intermediate_tokens.to_string();
    let taker_str = taker.to_string();
    let query: Vec<(&str, &str)> = vec![
        ("inputMint", scenario.input_mint.as_str()),
        ("outputMint", scenario.output_mint.as_str()),
        ("amount", amount_str.as_str()),
        ("taker", taker_str.as_str()),
        ("maxAccounts", max_accounts_str.as_str()),
        ("slippageBps", slippage_str.as_str()),
        ("restrictIntermediateTokens", restrict_str.as_str()),
    ];

    let mut req = http.get(&url).query(&query);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("network error contacting {url}: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;

    if !status.is_success() {
        // Many Jupiter "no route found" responses come back as 4xx with a
        // JSON body — surface the body text rather than swallow it.
        return Err(format!("HTTP {} from {url}: {body}", status.as_u16()));
    }

    serde_json::from_str::<V2BuildResponse>(&body)
        .map_err(|e| format!("failed to parse v2/build response: {e} :: body={body}"))
}

// ───────────────────────────────────────────────────────────────────────────
// Test entry point
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn jupiter_v2_sizing_measurement_matrix() {
    print_safety_banner();
    if skip_unless_opted_in() {
        return;
    }

    let (base_url, _) = env_or(ENV_BASE_URL, DEFAULT_BASE_URL);
    let api_key = env::var(ENV_API_KEY).ok().filter(|k| !k.is_empty());
    let (input_mint, input_overridden) = env_or(ENV_INPUT_MINT, DEFAULT_INPUT_MINT);
    let (output_mint, output_overridden) = env_or(ENV_OUTPUT_MINT, DEFAULT_OUTPUT_MINT);
    let (amount_raw_str, amount_overridden) = env_or(ENV_AMOUNT_RAW, DEFAULT_AMOUNT_RAW.to_string());
    let amount_raw: u64 = amount_raw_str
        .parse()
        .expect("CLAW_STAGE2_JUPITER_AMOUNT_RAW must be a u64");
    let (slippage_bps_str, slippage_overridden) =
        env_or(ENV_SLIPPAGE_BPS, DEFAULT_SLIPPAGE_BPS.to_string());
    let slippage_bps: u16 = slippage_bps_str
        .parse()
        .expect("CLAW_STAGE2_JUPITER_SLIPPAGE_BPS must be a u16");

    let scenario = Scenario {
        input_mint: input_mint.clone(),
        output_mint: output_mint.clone(),
        amount_raw,
        slippage_bps,
        restrict_intermediate_tokens: DEFAULT_RESTRICT_INTERMEDIATE_TOKENS,
    };
    let env_used = EnvUsed {
        sizing_enabled: true,
        api_key_supplied: api_key.is_some(),
        base_url: base_url.clone(),
        input_mint_overridden: input_overridden,
        output_mint_overridden: output_overridden,
        amount_overridden,
        slippage_overridden,
    };

    println!("scenario:");
    println!("  input_mint  : {}", scenario.input_mint);
    println!("  output_mint : {}", scenario.output_mint);
    println!("  amount_raw  : {}", scenario.amount_raw);
    println!("  slippage_bps: {}", scenario.slippage_bps);
    println!("  restrict_intermediate_tokens: {}", scenario.restrict_intermediate_tokens);
    println!("env:");
    println!("  base_url        : {}", env_used.base_url);
    println!("  api_key_supplied: {}", env_used.api_key_supplied);

    let bracket = BracketAccounts::new();
    let taker = bracket.delegated_wallet;
    println!("synthetic taker (bracket delegated_wallet): {taker}");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build reqwest client");

    let mut rows: Vec<MaxAccountsRow> = Vec::with_capacity(MAX_ACCOUNTS_MATRIX.len());
    for &max_accounts in MAX_ACCOUNTS_MATRIX {
        println!();
        println!("── maxAccounts = {max_accounts} ──");

        let row = match fetch_v2_build(
            &http,
            &base_url,
            api_key.as_deref(),
            &scenario,
            &taker,
            max_accounts,
        )
        .await
        {
            Ok(build) => match measure_bracket(&build, &bracket) {
                Ok(metrics) => {
                    let alt_loaded_addresses = if build.addresses_by_lookup_table_address.is_empty() {
                        None
                    } else {
                        Some(
                            build
                                .addresses_by_lookup_table_address
                                .values()
                                .map(|v| v.len())
                                .sum::<usize>(),
                        )
                    };
                    let classification = classify_bytes(metrics.serialized_bytes);
                    println!(
                        "  ixs: cb={} setup={} swap=1 cleanup={} other={} tip={}",
                        build.compute_budget_instructions.len(),
                        build.setup_instructions.len(),
                        build.cleanup_instruction.is_some() as u8,
                        build.other_instructions.len(),
                        build.tip_instruction.is_some() as u8,
                    );
                    println!(
                        "  ALTs(addressLookupTableAddresses)= {} | static_after_bracket = {} | resolved_after_bracket = {} | bytes_after_bracket = {} (cap {PACKET_DATA_SIZE}) | classification = {}",
                        build.address_lookup_table_addresses.len(),
                        metrics.static_after_bracket,
                        metrics.resolved_after_bracket,
                        metrics.serialized_bytes,
                        classification_label(&classification),
                    );

                    MaxAccountsRow {
                        max_accounts,
                        restrict_intermediate_tokens: scenario.restrict_intermediate_tokens,
                        route_label: build.route_label.clone(),
                        setup_instructions_count: build.setup_instructions.len(),
                        compute_budget_instructions_count: build
                            .compute_budget_instructions
                            .len(),
                        swap_instruction_present: true,
                        cleanup_instruction_present: build.cleanup_instruction.is_some(),
                        other_instructions_count: build.other_instructions.len(),
                        tip_instruction_present: build.tip_instruction.is_some(),
                        blockhash_with_metadata_present: build
                            .blockhash_with_metadata
                            .is_some(),
                        address_lookup_tables_count: build
                            .address_lookup_table_addresses
                            .len(),
                        alt_loaded_addresses_count: alt_loaded_addresses,
                        static_account_count_before_bracket: Some(metrics.static_before_bracket),
                        static_account_count_after_bracket: Some(metrics.static_after_bracket),
                        resolved_account_count_after_bracket: Some(
                            metrics.resolved_after_bracket,
                        ),
                        serialized_v0_tx_bytes_after_bracket: Some(metrics.serialized_bytes),
                        cu_estimate: None,
                        classification,
                        error: None,
                    }
                }
                Err(e) => {
                    println!("  measurement error: {e}");
                    MaxAccountsRow {
                        max_accounts,
                        restrict_intermediate_tokens: scenario.restrict_intermediate_tokens,
                        route_label: build.route_label.clone(),
                        setup_instructions_count: build.setup_instructions.len(),
                        compute_budget_instructions_count: build
                            .compute_budget_instructions
                            .len(),
                        swap_instruction_present: true,
                        cleanup_instruction_present: build.cleanup_instruction.is_some(),
                        other_instructions_count: build.other_instructions.len(),
                        tip_instruction_present: build.tip_instruction.is_some(),
                        blockhash_with_metadata_present: build
                            .blockhash_with_metadata
                            .is_some(),
                        address_lookup_tables_count: build
                            .address_lookup_table_addresses
                            .len(),
                        alt_loaded_addresses_count: None,
                        static_account_count_before_bracket: None,
                        static_account_count_after_bracket: None,
                        resolved_account_count_after_bracket: None,
                        serialized_v0_tx_bytes_after_bracket: None,
                        cu_estimate: None,
                        classification: Classification::Error,
                        error: Some(e),
                    }
                }
            },
            Err(e) => {
                let classification = if e.to_lowercase().contains("no route")
                    || e.to_lowercase().contains("noroute")
                {
                    Classification::NoRoute
                } else {
                    Classification::Error
                };
                println!("  fetch error: {e}");
                MaxAccountsRow {
                    max_accounts,
                    restrict_intermediate_tokens: scenario.restrict_intermediate_tokens,
                    route_label: None,
                    setup_instructions_count: 0,
                    compute_budget_instructions_count: 0,
                    swap_instruction_present: false,
                    cleanup_instruction_present: false,
                    other_instructions_count: 0,
                    tip_instruction_present: false,
                    blockhash_with_metadata_present: false,
                    address_lookup_tables_count: 0,
                    alt_loaded_addresses_count: None,
                    static_account_count_before_bracket: None,
                    static_account_count_after_bracket: None,
                    resolved_account_count_after_bracket: None,
                    serialized_v0_tx_bytes_after_bracket: None,
                    cu_estimate: None,
                    classification,
                    error: Some(e),
                }
            }
        };
        rows.push(row);
    }

    let report = Report {
        generated_at: chrono::Utc::now().to_rfc3339(),
        official_docs_checked_at: OFFICIAL_DOCS_CHECKED_AT.to_string(),
        jupiter_base_url: base_url.clone(),
        scenario,
        env_used,
        rows,
        safety_markers: vec![
            SAFETY_NO_SIGNING,
            SAFETY_NO_BROADCAST,
            SAFETY_NO_TX_SUBMIT,
            SAFETY_FETCH_BUILD_ONLY,
        ],
        packet_data_size: PACKET_DATA_SIZE,
        notes: vec![
            "Bracket model: Variant A two-ix (pre_check_v2 + post_check_v2) per J-prep \
             § 4.4. Synthetic clawsol-authority program id, 32-byte and 24-byte \
             placeholder data. Real bracket bytes may differ slightly once the \
             on-chain program is implemented."
                .to_string(),
            "ALT modeling: each Jupiter ALT pubkey gets a synthetic \
             AddressLookupTableAccount populated with every account referenced \
             anywhere in the Jupiter ix list. The compiler chooses static vs. \
             ALT-loaded; over-broad synthetic ALTs do not bias the wire bytes."
                .to_string(),
            "CU estimate is intentionally None in this slice (FETCH_BUILD_ONLY). \
             A future env-gated simulate probe can fill it in."
                .to_string(),
        ],
    };

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = manifest_dir.join(FIXTURE_JSON_RELPATH);
    if let Some(parent) = fixture_path.parent() {
        fs::create_dir_all(parent).expect("create parent dir for sizing fixture");
    }
    let json = serde_json::to_string_pretty(&report)
        .expect("serialize report");
    fs::write(&fixture_path, json + "\n").expect("write sizing fixture");
    println!();
    println!("wrote sizing fixture: {}", fixture_path.display());

    println!();
    println!("══════════════════════════════════════════════════════════════");
    println!("  Summary");
    println!("══════════════════════════════════════════════════════════════");
    for row in &report.rows {
        println!(
            "  maxAccounts={:>2} → {:<8} {}",
            row.max_accounts,
            classification_label(&row.classification),
            match (row.serialized_v0_tx_bytes_after_bracket, &row.error) {
                (Some(b), _) => format!("bytes={b} (cap {PACKET_DATA_SIZE})"),
                (None, Some(e)) => format!("error={e}"),
                (None, None) => "—".to_string(),
            }
        );
    }
    println!();
    println!("  {SAFETY_NO_SIGNING}");
    println!("  {SAFETY_NO_BROADCAST}");
    println!("  {SAFETY_NO_TX_SUBMIT}");
    println!("  {SAFETY_FETCH_BUILD_ONLY}");
}

// ───────────────────────────────────────────────────────────────────────────
// Pure helper unit tests — verify the bracket-synthesis primitives work
// without any live API. These run in default `cargo test` as part of the
// test binary, so the harness file always exercises *something* even when
// the live measurement is gated off.
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn synthetic_pre_check_has_expected_account_count() {
        let b = BracketAccounts::new();
        let ix = synth_pre_check_ix(&b);
        assert_eq!(
            ix.accounts.len(),
            9,
            "pre_check should reference 9 accounts (signer + PDA + sysvar + 3 pyth + 2 ATAs + receiver program id)"
        );
        assert_eq!(ix.data.len(), 32);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn synthetic_post_check_has_expected_account_count() {
        let b = BracketAccounts::new();
        let ix = synth_post_check_ix(&b);
        assert_eq!(
            ix.accounts.len(),
            5,
            "post_check should reference 5 accounts (signer + PDA + sysvar + input ATA + dest ATA)"
        );
        assert_eq!(ix.data.len(), 24);
    }

    #[test]
    fn classify_bytes_buckets() {
        assert!(matches!(classify_bytes(900), Classification::Green));
        assert!(matches!(classify_bytes(1100), Classification::Green));
        assert!(matches!(classify_bytes(1101), Classification::Yellow));
        assert!(matches!(classify_bytes(1232), Classification::Yellow));
        assert!(matches!(classify_bytes(1233), Classification::Red));
    }

    #[test]
    fn known_program_ids_parse() {
        for id in [
            COMPUTE_BUDGET_PROGRAM,
            SYSTEM_PROGRAM,
            TOKEN_PROGRAM,
            ASSOCIATED_TOKEN_PROGRAM,
            SYSVAR_INSTRUCTIONS,
            PYTH_RECEIVER_PROGRAM,
        ] {
            assert!(
                Pubkey::from_str(id).is_ok(),
                "constant pubkey must parse: {id}"
            );
        }
    }

    /// Verify the bracket-wrap measurement against a hand-built `V2BuildResponse`
    /// that mimics a small Jupiter route. The aim is to lock the harness's
    /// arithmetic, not to hit real numbers — those come from the live test.
    #[test]
    fn measure_bracket_on_synthetic_response_returns_metrics() {
        let bracket = BracketAccounts::new();

        // Use the same delegated wallet so the Jupiter ix's "user wallet" lands
        // in the same static slot as the bracket signer.
        let user = bracket.delegated_wallet.to_string();
        let mint_a = Pubkey::new_unique().to_string();
        let mint_b = Pubkey::new_unique().to_string();
        let alt_addr = Pubkey::new_unique().to_string();

        let cb_ix = JupiterInstruction {
            program_id: COMPUTE_BUDGET_PROGRAM.to_string(),
            accounts: vec![],
            data: base64::engine::general_purpose::STANDARD.encode([1u8, 0, 0, 0]),
        };
        let setup_ix = JupiterInstruction {
            program_id: ASSOCIATED_TOKEN_PROGRAM.to_string(),
            accounts: vec![
                JupiterAccountMeta {
                    pubkey: user.clone(),
                    is_signer: true,
                    is_writable: true,
                },
                JupiterAccountMeta {
                    pubkey: mint_a.clone(),
                    is_signer: false,
                    is_writable: false,
                },
            ],
            data: base64::engine::general_purpose::STANDARD.encode([0u8; 1]),
        };
        let swap_ix = JupiterInstruction {
            program_id: TOKEN_PROGRAM.to_string(),
            accounts: vec![
                JupiterAccountMeta {
                    pubkey: user.clone(),
                    is_signer: true,
                    is_writable: true,
                },
                JupiterAccountMeta {
                    pubkey: mint_b.clone(),
                    is_signer: false,
                    is_writable: false,
                },
            ],
            data: base64::engine::general_purpose::STANDARD.encode([3u8; 16]),
        };

        let build = V2BuildResponse {
            compute_budget_instructions: vec![cb_ix],
            setup_instructions: vec![setup_ix],
            swap_instruction: swap_ix,
            cleanup_instruction: None,
            other_instructions: vec![],
            tip_instruction: None,
            address_lookup_table_addresses: vec![alt_addr],
            addresses_by_lookup_table_address: HashMap::new(),
            blockhash_with_metadata: None,
            route_label: Some("Synthetic-1hop".to_string()),
        };

        let metrics = measure_bracket(&build, &bracket).expect("measurement must succeed");

        // Bracket adds 9 unique pre-check accounts + 1 destination ATA from
        // post-check (the rest dedup with pre-check). Compute-budget program id
        // also dedups with itself; ATA program from setup; token program from
        // swap; mint_a + mint_b from setup/swap.
        assert!(metrics.static_after_bracket > metrics.static_before_bracket);
        assert!(metrics.resolved_after_bracket >= metrics.static_after_bracket);
        assert!(metrics.serialized_bytes > 0);
        // Bracket-wrapped should fit comfortably for a 1-hop synthetic route.
        assert!(
            metrics.serialized_bytes < PACKET_DATA_SIZE,
            "synthetic bracket should fit; got {}",
            metrics.serialized_bytes
        );
    }
}

