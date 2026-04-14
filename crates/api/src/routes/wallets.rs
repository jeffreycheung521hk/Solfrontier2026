//! `GET /wallets` — configured wallets with today's spend.
//!
//! The daemon reports the wallet directory at startup; this endpoint surfaces
//! it together with each wallet's current UTC-day spend so the dashboard can
//! show spend-vs-cap progress.

use axum::{extract::State, response::{IntoResponse, Response}, Json};

use crate::state::{AppState, WalletSummaryDto};

#[derive(serde::Serialize)]
pub struct WalletsResponse {
    pub wallets: Vec<WalletSummaryDto>,
}

pub async fn list_wallets(State(state): State<AppState>) -> Response {
    let wallets = state.wallets.list().await;
    Json(WalletsResponse { wallets }).into_response()
}
