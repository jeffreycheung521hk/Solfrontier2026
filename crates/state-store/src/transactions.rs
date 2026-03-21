//! Transaction lifecycle repository.

use chrono::Utc;
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};
use tracing::instrument;

use claw_types::{
    policy::PolicyVerdict,
    transaction::{SimulationResult, TransactionProposal, TransactionStatus},
};

use crate::errors::StoreError;

/// Data-access object for transaction lifecycle records.
#[derive(Clone, Debug)]
pub struct TransactionRepository {
    pool: SqlitePool,
}

impl TransactionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Creates a new transaction record in `proposed` state.
    #[instrument(skip(self, proposal))]
    pub async fn create(&self, proposal: &TransactionProposal) -> Result<(), StoreError> {
        let id           = proposal.id.to_string();
        let session_id   = proposal.session_id.to_string();
        let proposal_json = serde_json::to_string(proposal)?;
        let network      = proposal.network.to_string();
        let now          = Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO transaction_records
                 (id, session_id, wallet_pubkey, network, status, description, proposal, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'proposed', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(&proposal.wallet_pubkey)
        .bind(network)
        .bind(&proposal.description)
        .bind(proposal_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Updates the status and optionally attaches simulation or policy data.
    pub async fn update_status(
        &self,
        id:                &str,
        status:            TransactionStatus,
        simulation_result: Option<&SimulationResult>,
        policy_verdict:    Option<&PolicyVerdict>,
        signature:         Option<&str>,
    ) -> Result<(), StoreError> {
        let status_str  = status.to_string();
        let sim_json    = simulation_result.map(|s| serde_json::to_string(s)).transpose()?;
        let policy_json = policy_verdict.map(|p| serde_json::to_string(p)).transpose()?;
        let now         = Utc::now().timestamp_millis();

        sqlx::query(
            "UPDATE transaction_records
             SET status            = ?,
                 simulation_result = COALESCE(?, simulation_result),
                 policy_verdict    = COALESCE(?, policy_verdict),
                 signature         = COALESCE(?, signature),
                 updated_at        = ?
             WHERE id = ?",
        )
        .bind(status_str)
        .bind(sim_json)
        .bind(policy_json)
        .bind(signature)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Returns all transaction records for a given wallet.
    pub async fn list_for_wallet(
        &self,
        wallet_pubkey: &str,
        limit:         i64,
    ) -> Result<Vec<TransactionRow>, StoreError> {
        let rows = sqlx::query_as::<_, TransactionRow>(
            "SELECT id, session_id, wallet_pubkey, network, status, description,
                    proposal, simulation_result, policy_verdict, signature,
                    created_at, updated_at
             FROM transaction_records
             WHERE wallet_pubkey = ?
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(wallet_pubkey)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

/// Raw row from the transaction_records table.
#[derive(Debug, FromRow)]
pub struct TransactionRow {
    pub id:                String,
    pub session_id:        String,
    pub wallet_pubkey:     String,
    pub network:           String,
    pub status:            String,
    pub description:       String,
    pub proposal:          String,
    pub simulation_result: Option<String>,
    pub policy_verdict:    Option<String>,
    pub signature:         Option<String>,
    pub created_at:        i64,
    pub updated_at:        i64,
}
