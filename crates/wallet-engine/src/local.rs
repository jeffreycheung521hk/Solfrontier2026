//! Local keypair signer implementation.
//!
//! Signs transactions using a keypair loaded into `SecretKeystore`.
//! This is the default signer type for non-mainnet use and development.
//! For mainnet, prefer a Ledger hardware signer.

use async_trait::async_trait;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Signature,
    transaction::Transaction,
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

    fn description(&self) -> String {
        format!("local-keypair:{}", self.pubkey)
    }

    fn is_automatic(&self) -> bool {
        true
    }
}
