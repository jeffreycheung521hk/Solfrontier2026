//! Local keypair signer implementation.
//!
//! Signs transactions using a keypair loaded into `SecretKeystore`.
//! This is the default signer type for non-mainnet use and development.
//! For mainnet, prefer a Ledger hardware signer.

use async_trait::async_trait;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Signature,
    signer::Signer as SolanaSignerTrait,
    transaction::{Transaction, VersionedTransaction},
};
use tracing::instrument;

use crate::errors::WalletError;
use crate::keystore::SecretKeystore;
use crate::signer::Signer;

/// A signer backed by a local keypair in the `SecretKeystore`.
#[derive(Clone, Debug)]
pub struct LocalKeypairSigner {
    pubkey: Pubkey,
    keystore: SecretKeystore,
}

impl LocalKeypairSigner {
    pub fn new(pubkey: Pubkey, keystore: SecretKeystore) -> Self {
        Self { pubkey, keystore }
    }
}

#[async_trait]
impl Signer for LocalKeypairSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    #[instrument(skip(self, tx), fields(pubkey = %self.pubkey))]
    async fn sign_transaction(&self, tx: &mut Transaction) -> Result<Signature, WalletError> {
        let keypair = self.keystore.get(&self.pubkey)?;
        // Sign using the Solana SDK's partial_sign method.
        // The transaction's recent_blockhash must already be set.
        tx.partial_sign(&[keypair.inner()], tx.message.recent_blockhash);
        // Return the signature for this signer's pubkey
        let sig_idx = tx
            .message
            .account_keys
            .iter()
            .position(|k| k == &self.pubkey)
            .ok_or_else(|| {
                WalletError::SigningFailed(format!(
                    "pubkey {} not found in transaction",
                    self.pubkey
                ))
            })?;

        Ok(tx.signatures[sig_idx])
    }

    #[instrument(skip(self, tx), fields(pubkey = %self.pubkey))]
    async fn sign_versioned(
        &self,
        tx: &mut VersionedTransaction,
    ) -> Result<Signature, WalletError> {
        let keypair = self.keystore.get(&self.pubkey)?;

        // Static account keys (the first-class accounts in the V0 message,
        // excluding lookup-table-resolved accounts). The signer MUST appear
        // in this section for its signature to be verifiable.
        let static_keys = tx.message.static_account_keys();
        let sig_idx = static_keys
            .iter()
            .position(|k| k == &self.pubkey)
            .ok_or_else(|| {
                WalletError::SigningFailed(format!(
                    "pubkey {} not found in static account keys",
                    self.pubkey
                ))
            })?;

        // Sign the message serialization. For VersionedMessage, this includes
        // the version prefix, so the signature binds to V0 semantics.
        let message_bytes = tx.message.serialize();
        let signature = keypair.inner().sign_message(&message_bytes);

        // Ensure signatures vector is sized correctly.
        let required_sigs = tx.message.header().num_required_signatures as usize;
        if tx.signatures.len() < required_sigs {
            tx.signatures.resize(required_sigs, Signature::default());
        }
        if sig_idx >= tx.signatures.len() {
            return Err(WalletError::SigningFailed(format!(
                "signer index {sig_idx} exceeds signatures vec length {}",
                tx.signatures.len()
            )));
        }
        tx.signatures[sig_idx] = signature;
        Ok(signature)
    }

    fn description(&self) -> String {
        format!("local-keypair:{}", self.pubkey)
    }

    fn is_automatic(&self) -> bool {
        true
    }
}
