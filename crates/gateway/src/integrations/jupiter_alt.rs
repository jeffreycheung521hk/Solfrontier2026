//! Address Lookup Table (ALT) resolution for Jupiter V0 transactions.
//!
//! # Why this module exists
//!
//! The live Jupiter API (`POST /swap/v1/swap-instructions`) returns only the
//! *addresses* of the ALT accounts used by a swap route — the `addresses`
//! array inside each ALT must be fetched from Solana RPC by the caller.
//!
//! This module owns that resolution step. Given a list of ALT pubkeys and an
//! RPC-backed account fetcher, it returns a `Vec<AddressLookupTableAccount>`
//! ready to hand to `v0::Message::try_compile`.
//!
//! # Scope
//!
//! - Fetch ALT account data via a pluggable trait (`AltAccountFetcher`).
//! - Deserialize with `solana_program::address_lookup_table::state::AddressLookupTable`.
//! - Preserve deterministic ordering (sort by address) so repeated assembly
//!   over the same inputs yields identical byte output.
//!
//! # Not in scope
//!
//! - Caching resolved tables (each JIT build re-fetches; Jupiter's blockhash
//!   window is ~2 minutes and tables rarely change within that window, but
//!   staleness is not an invariant we maintain here).
//! - Handling deactivated / frozen tables specially. The resolver returns
//!   whatever the account contains; V0 message compilation will fail
//!   downstream if the table is unusable.

use async_trait::async_trait;
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    pubkey::Pubkey,
};
use std::str::FromStr;

/// Failure modes for ALT resolution.
#[derive(Debug, thiserror::Error)]
pub enum AltResolveError {
    #[error("invalid ALT address '{0}': {1}")]
    InvalidAddress(String, String),

    #[error("RPC failed for ALT '{0}': {1}")]
    RpcFailed(String, String),

    #[error("ALT '{0}' account data could not be deserialized: {1}")]
    DeserializeFailed(String, String),
}

/// Minimal seam for fetching an ALT account's raw bytes.
///
/// Production impl wraps `ClawRpcClient::get_account`. Tests inject a
/// recording mock. The trait deliberately does NOT leak `solana_client`
/// types so that test doubles stay trivial.
#[async_trait]
pub trait AltAccountFetcher: Send + Sync {
    /// Return the raw account data for an ALT pubkey, or an error string.
    async fn fetch_alt_data(&self, alt_address: &str) -> Result<Vec<u8>, String>;
}

/// Resolve a list of ALT addresses into `AddressLookupTableAccount`s.
///
/// Returns an empty vec if `addresses` is empty. Preserves sorted order on
/// the ALT `key` for reproducible assembly.
pub async fn resolve_address_lookup_tables(
    addresses: &[String],
    fetcher: &dyn AltAccountFetcher,
) -> Result<Vec<AddressLookupTableAccount>, AltResolveError> {
    if addresses.is_empty() {
        return Ok(Vec::new());
    }

    let mut resolved: Vec<AddressLookupTableAccount> = Vec::with_capacity(addresses.len());

    for addr in addresses {
        let key = Pubkey::from_str(addr).map_err(|e| {
            AltResolveError::InvalidAddress(addr.clone(), e.to_string())
        })?;

        let data = fetcher
            .fetch_alt_data(addr)
            .await
            .map_err(|e| AltResolveError::RpcFailed(addr.clone(), e))?;

        let table = AddressLookupTable::deserialize(&data)
            .map_err(|e| AltResolveError::DeserializeFailed(addr.clone(), e.to_string()))?;

        resolved.push(AddressLookupTableAccount {
            key,
            addresses: table.addresses.to_vec(),
        });
    }

    resolved.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test fetcher backed by a canned address → bytes map. Each fetch
    /// increments a counter so tests can assert call counts.
    struct CannedFetcher {
        data: HashMap<String, Vec<u8>>,
        fetch_count: Mutex<usize>,
    }

    impl CannedFetcher {
        fn new(entries: Vec<(&str, Vec<u8>)>) -> Self {
            let data = entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            Self {
                data,
                fetch_count: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl AltAccountFetcher for CannedFetcher {
        async fn fetch_alt_data(&self, alt_address: &str) -> Result<Vec<u8>, String> {
            *self.fetch_count.lock().unwrap() += 1;
            self.data
                .get(alt_address)
                .cloned()
                .ok_or_else(|| format!("no canned data for {alt_address}"))
        }
    }

    /// Build an in-memory ALT account whose on-wire serialization is bit-identical
    /// to what `AddressLookupTable::deserialize` expects.
    fn serialize_test_alt(addresses: Vec<Pubkey>) -> Vec<u8> {
        use solana_sdk::address_lookup_table::state::LookupTableMeta;
        use std::borrow::Cow;

        let table = AddressLookupTable {
            meta: LookupTableMeta::default(),
            addresses: Cow::Owned(addresses),
        };
        AddressLookupTable::serialize_for_tests(table)
            .expect("serialize_for_tests must succeed for a well-formed table")
    }

    #[tokio::test]
    async fn empty_input_returns_empty_vec_without_fetching() {
        let fetcher = CannedFetcher::new(vec![]);
        let out = resolve_address_lookup_tables(&[], &fetcher).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(*fetcher.fetch_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn single_alt_resolves_with_correct_key_and_addresses() {
        let alt_addr = "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5";
        let inner1 = Pubkey::from_str("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4").unwrap();
        let inner2 = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let data = serialize_test_alt(vec![inner1, inner2]);

        let fetcher = CannedFetcher::new(vec![(alt_addr, data)]);
        let out = resolve_address_lookup_tables(&[alt_addr.to_string()], &fetcher)
            .await
            .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, Pubkey::from_str(alt_addr).unwrap());
        assert_eq!(out[0].addresses, vec![inner1, inner2]);
    }

    #[tokio::test]
    async fn multiple_alts_sorted_by_key_for_determinism() {
        let a = "8rUvfYVNMRhkxY4nRr5xrFa8eq3vWk8vLe8EEsCxmbZQ"; // smaller
        let z = "J4JVQWjJkxr4hW7QKhDC4gC5uEHxCZRBcSNkXUBRTCNk"; // larger
        let inner = Pubkey::new_unique();
        let data = serialize_test_alt(vec![inner]);

        let fetcher = CannedFetcher::new(vec![
            (a, data.clone()),
            (z, data.clone()),
        ]);

        // Intentionally pass in reverse order; resolver must re-sort.
        let out = resolve_address_lookup_tables(
            &[z.to_string(), a.to_string()],
            &fetcher,
        )
        .await
        .unwrap();

        assert_eq!(out.len(), 2);
        assert!(out[0].key < out[1].key, "resolver must sort output by key");
    }

    #[tokio::test]
    async fn invalid_address_is_reported_before_rpc_call() {
        let fetcher = CannedFetcher::new(vec![]);
        let err = resolve_address_lookup_tables(
            &["not-a-valid-pubkey".to_string()],
            &fetcher,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AltResolveError::InvalidAddress(..)));
        assert_eq!(
            *fetcher.fetch_count.lock().unwrap(),
            0,
            "no RPC should fire for a malformed address"
        );
    }

    #[tokio::test]
    async fn rpc_failure_propagates_with_address_context() {
        // Syntactically valid address but no canned data → RPC returns err.
        let addr = "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5";
        let fetcher = CannedFetcher::new(vec![]);
        let err = resolve_address_lookup_tables(&[addr.to_string()], &fetcher)
            .await
            .unwrap_err();
        match err {
            AltResolveError::RpcFailed(a, _) => assert_eq!(a, addr),
            other => panic!("expected RpcFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_account_data_returns_deserialize_error() {
        let addr = "4yTMfaDAUMeaoBnhXnshdbw2Z35DX9CU5HBsPufxeqp5";
        let fetcher = CannedFetcher::new(vec![(addr, vec![0u8; 4])]);
        let err = resolve_address_lookup_tables(&[addr.to_string()], &fetcher)
            .await
            .unwrap_err();
        assert!(matches!(err, AltResolveError::DeserializeFailed(..)));
    }
}
