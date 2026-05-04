//! Read-only `get_jupiter_quote` chat tool — Phase 6C.
//!
//! Fetches a Jupiter quote (`/swap/v1/quote`) so the LLM can reason
//! about expected output BEFORE deciding to call `submit_jupiter_swap`.
//! NEVER builds, signs, broadcasts, or creates an approval — the tool
//! only consumes the quote endpoint and returns a structured snapshot.
//!
//! # Scope (Phase 6C)
//!
//! - SOL ↔ USDC only — both `input_mint` and `output_mint` must be in
//!   the allowlist; mismatched mints return a `policy_blocked` status
//!   (the user-facing layer can render this without surprise).
//! - `slippage_bps ≤ 100` (1.00 %). Higher values are rejected without
//!   contacting Jupiter.
//! - The schema enforces the input shape; `execute` re-validates as
//!   defence-in-depth so a non-conforming dispatcher cannot smuggle a
//!   wider envelope through.
//!
//! # Capabilities
//!
//! `required_capabilities: vec![]`. The tool is pure read; the chat
//! handler's `ProposeSigning`-only capability set is more than enough.
//!
//! # Failure modes
//!
//! All non-success outcomes are returned as `Ok(ToolOutput { success:
//! false, data: { status: ... } })` so the chat handler can render a
//! consistent `tool_dispatched` shape and the LLM/UI can react. True
//! input-validation failures (missing field, non-numeric amount) still
//! return `Err(ToolError::InvalidInput)`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::integrations::jupiter::{
    JupiterClient, JupiterError, SwapQuoteRequest, SwapQuoteResponse,
};

/// Mints this slice allows for quoting. Mirrors the production Jupiter
/// swap policy envelope (`config/jupiter-mainnet-e2e.example.toml`).
pub const SOL_MINT_BS58: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Hard slippage cap for this slice (100 bps = 1.00 %).
pub const MAX_SLIPPAGE_BPS: u16 = 100;

/// Narrow read seam — `quote()` only. Lets test stubs avoid implementing
/// the full `JupiterClient::build` path (which is execution-side and
/// out of scope here).
#[async_trait]
pub trait JupiterQuoteSource: Send + Sync {
    async fn fetch_quote(
        &self,
        request: &SwapQuoteRequest,
    ) -> Result<SwapQuoteResponse, JupiterError>;
}

/// Adapter — wrap any `JupiterClient` into a `JupiterQuoteSource`. The
/// daemon plugs `HttpJupiterClient` here at wire time; tests inject
/// stubs directly.
pub struct JupiterClientQuoteSource {
    inner: Arc<dyn JupiterClient>,
}

impl JupiterClientQuoteSource {
    pub fn new(inner: Arc<dyn JupiterClient>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl JupiterQuoteSource for JupiterClientQuoteSource {
    async fn fetch_quote(
        &self,
        request: &SwapQuoteRequest,
    ) -> Result<SwapQuoteResponse, JupiterError> {
        self.inner.quote(request).await
    }
}

/// Read-only Jupiter quote tool. Wired into the chat allowlist as
/// `get_jupiter_quote`.
pub struct GetJupiterQuoteTool {
    source: Arc<dyn JupiterQuoteSource>,
}

impl GetJupiterQuoteTool {
    pub fn new(source: Arc<dyn JupiterQuoteSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl Tool for GetJupiterQuoteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_jupiter_quote".to_string(),
            description:
                "Read-only Jupiter quote. Fetches a route plan and expected output for \
                 a swap intent without building, signing, broadcasting, or creating an \
                 approval. Use BEFORE submit_jupiter_swap when the user asks \
                 \"how much X would I get for Y?\" or makes a conditional swap \
                 request. \
                 \n\nScope (Phase 6C): SOL <-> USDC only; slippage_bps in [0, 100]. \
                 Higher slippage or other mints are rejected without contacting \
                 Jupiter."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["input_mint", "output_mint", "input_amount", "slippage_bps"],
                "properties": {
                    "input_mint":   { "type": "string",  "description": "Base58 mint pubkey of the input token" },
                    "output_mint":  { "type": "string",  "description": "Base58 mint pubkey of the output token" },
                    "input_amount": { "type": "integer", "minimum": 1,    "description": "Raw base units of the input token" },
                    "slippage_bps": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Maximum slippage in basis points (100 = 1.00%)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status":                  { "type": "string", "enum": ["ok", "policy_blocked", "quote_unavailable", "provider_error"] },
                    "input_mint":              { "type": ["string", "null"] },
                    "output_mint":             { "type": ["string", "null"] },
                    "input_amount":            { "type": ["integer", "null"] },
                    "out_amount":              { "type": ["integer", "null"] },
                    "other_amount_threshold":  { "type": ["integer", "null"] },
                    "price_impact_pct":        { "type": ["string", "null"] },
                    "route_summary":           { "type": ["array", "null"], "items": { "type": "string" } },
                    "slippage_bps":            { "type": ["integer", "null"] },
                    "policy_rule_name":        { "type": ["string", "null"] },
                    "error":                   { "type": ["string", "null"] }
                }
            }),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // ── Input validation ────────────────────────────────────────────────
        let input_mint = require_string(&input, "input_mint")?;
        let output_mint = require_string(&input, "output_mint")?;
        let input_amount = input.parameters["input_amount"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "input_amount must be a positive integer".to_string(),
            })?;
        if input_amount == 0 {
            return Err(ToolError::InvalidInput {
                reason: "input_amount must be > 0".to_string(),
            });
        }
        let slippage_bps_raw = input.parameters["slippage_bps"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "slippage_bps must be an integer in [0, 100]".to_string(),
            })?;
        if slippage_bps_raw > MAX_SLIPPAGE_BPS as u64 {
            return Ok(policy_blocked_output(
                "slippage-exceeds-quote-cap",
                &format!(
                    "slippage_bps {slippage_bps_raw} exceeds Phase 6C cap {MAX_SLIPPAGE_BPS}"
                ),
                Some(slippage_bps_raw),
            ));
        }
        let slippage_bps = slippage_bps_raw as u16;

        // ── Mint allowlist (SOL ↔ USDC only) ───────────────────────────────
        if !is_allowed_mint(&input_mint) {
            return Ok(policy_blocked_output(
                "input-mint-not-allowed",
                &format!("input_mint {input_mint} is not in the SOL/USDC allowlist"),
                None,
            ));
        }
        if !is_allowed_mint(&output_mint) {
            return Ok(policy_blocked_output(
                "output-mint-not-allowed",
                &format!("output_mint {output_mint} is not in the SOL/USDC allowlist"),
                None,
            ));
        }
        if input_mint == output_mint {
            return Ok(policy_blocked_output(
                "input-output-mint-equal",
                "input_mint and output_mint must differ",
                None,
            ));
        }

        // ── Fetch quote ─────────────────────────────────────────────────────
        let request = SwapQuoteRequest {
            input_mint: input_mint.clone(),
            output_mint: output_mint.clone(),
            amount: input_amount,
            slippage_bps: Some(slippage_bps),
            swap_mode: None,
            only_direct_routes: None,
            max_accounts: None,
        };

        let response = match self.source.fetch_quote(&request).await {
            Ok(r) => r,
            Err(JupiterError::Network(msg)) => {
                return Ok(provider_error_output(
                    "quote_unavailable",
                    &format!("network: {msg}"),
                ));
            }
            Err(JupiterError::Api { status, message }) => {
                return Ok(provider_error_output(
                    "provider_error",
                    &format!("HTTP {status}: {message}"),
                ));
            }
            Err(JupiterError::Deserialize(msg)) => {
                return Ok(provider_error_output(
                    "provider_error",
                    &format!("malformed quote: {msg}"),
                ));
            }
        };

        let out_amount = response.out_amount_u64().ok();
        let other_amount_threshold = response.threshold_u64().ok();
        let route_summary: Vec<String> = response
            .route_labels()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        Ok(ToolOutput {
            tool_name: "get_jupiter_quote".to_string(),
            success: true,
            data: Some(json!({
                "status": "ok",
                "input_mint": input_mint,
                "output_mint": output_mint,
                "input_amount": input_amount,
                "out_amount": out_amount,
                "other_amount_threshold": other_amount_threshold,
                "price_impact_pct": response.price_impact_pct,
                "route_summary": route_summary,
                "slippage_bps": slippage_bps,
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

fn is_allowed_mint(mint: &str) -> bool {
    mint == SOL_MINT_BS58 || mint == USDC_MINT_BS58
}

fn require_string(input: &ToolInput, key: &str) -> Result<String, ToolError> {
    input.parameters[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::InvalidInput {
            reason: format!("{key} is required and must be a non-empty string"),
        })
}

fn policy_blocked_output(
    rule_name: &str,
    reason: &str,
    slippage_bps: Option<u64>,
) -> ToolOutput {
    let mut data = json!({
        "status": "policy_blocked",
        "policy_rule_name": rule_name,
        "error": reason,
    });
    if let Some(bps) = slippage_bps {
        data["slippage_bps"] = json!(bps);
    }
    ToolOutput {
        tool_name: "get_jupiter_quote".to_string(),
        success: false,
        data: Some(data),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

fn provider_error_output(status: &str, error: &str) -> ToolOutput {
    ToolOutput {
        tool_name: "get_jupiter_quote".to_string(),
        success: false,
        data: Some(json!({
            "status": status,
            "error": error,
        })),
        error: Some(error.to_string()),
        duration_ms: 0,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::jupiter::{RoutePlanStep, SwapInfo, SwapMode};
    use claw_types::session::SessionId;
    use uuid::Uuid;

    struct StubQuote {
        result: Result<SwapQuoteResponse, JupiterError>,
    }

    impl StubQuote {
        fn ok(out_amount: &str, threshold: &str, route: Vec<&str>) -> Self {
            Self {
                result: Ok(SwapQuoteResponse {
                    input_mint: SOL_MINT_BS58.to_string(),
                    in_amount: "1000000".to_string(),
                    output_mint: USDC_MINT_BS58.to_string(),
                    out_amount: out_amount.to_string(),
                    other_amount_threshold: threshold.to_string(),
                    swap_mode: SwapMode::ExactIn,
                    slippage_bps: 100,
                    price_impact_pct: Some("0.0123".to_string()),
                    route_plan: route
                        .into_iter()
                        .map(|label| RoutePlanStep {
                            swap_info: SwapInfo {
                                amm_key: "amm".to_string(),
                                label: Some(label.to_string()),
                                input_mint: SOL_MINT_BS58.to_string(),
                                output_mint: USDC_MINT_BS58.to_string(),
                                in_amount: "1000000".to_string(),
                                out_amount: out_amount.to_string(),
                                fee_amount: None,
                                fee_mint: None,
                            },
                            percent: 100,
                        })
                        .collect(),
                    context_slot: None,
                }),
            }
        }
        fn err(e: JupiterError) -> Self {
            Self { result: Err(e) }
        }
    }

    #[async_trait]
    impl JupiterQuoteSource for StubQuote {
        async fn fetch_quote(
            &self,
            _: &SwapQuoteRequest,
        ) -> Result<SwapQuoteResponse, JupiterError> {
            match &self.result {
                Ok(r) => Ok(r.clone()),
                Err(JupiterError::Network(s)) => Err(JupiterError::Network(s.clone())),
                Err(JupiterError::Api { status, message }) => Err(JupiterError::Api {
                    status: *status,
                    message: message.clone(),
                }),
                Err(JupiterError::Deserialize(s)) => Err(JupiterError::Deserialize(s.clone())),
            }
        }
    }

    fn input_for(input_mint: &str, output_mint: &str, amount: u64, slippage: u64) -> ToolInput {
        ToolInput {
            tool_name: "get_jupiter_quote".to_string(),
            parameters: json!({
                "input_mint":   input_mint,
                "output_mint":  output_mint,
                "input_amount": amount,
                "slippage_bps": slippage,
            }),
            session_id: SessionId::from(Uuid::new_v4()),
            correlation_id: Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn happy_path_returns_ok_with_route_summary() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok(
            "150000",
            "148500",
            vec!["Orca", "Raydium"],
        )));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1_000_000, 100))
            .await
            .unwrap();
        assert!(out.success);
        let data = out.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["input_mint"], SOL_MINT_BS58);
        assert_eq!(data["output_mint"], USDC_MINT_BS58);
        assert_eq!(data["input_amount"], 1_000_000);
        assert_eq!(data["out_amount"], 150_000);
        assert_eq!(data["other_amount_threshold"], 148_500);
        assert_eq!(data["price_impact_pct"], "0.0123");
        assert_eq!(data["slippage_bps"], 100);
        assert_eq!(
            data["route_summary"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            vec!["Orca".to_string(), "Raydium".to_string()]
        );
    }

    #[tokio::test]
    async fn slippage_above_cap_returns_policy_blocked() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("150000", "148500", vec![])));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1_000_000, 200))
            .await
            .unwrap();
        assert!(!out.success);
        let data = out.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "slippage-exceeds-quote-cap");
        assert_eq!(data["slippage_bps"], 200);
    }

    #[tokio::test]
    async fn non_allowlisted_input_mint_returns_policy_blocked() {
        const BONK: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let out = tool
            .execute(input_for(BONK, USDC_MINT_BS58, 1_000, 50))
            .await
            .unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "input-mint-not-allowed");
    }

    #[tokio::test]
    async fn non_allowlisted_output_mint_returns_policy_blocked() {
        const BONK: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, BONK, 1_000, 50))
            .await
            .unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "output-mint-not-allowed");
    }

    #[tokio::test]
    async fn same_input_and_output_mint_returns_policy_blocked() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, SOL_MINT_BS58, 1_000, 50))
            .await
            .unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "policy_blocked");
        assert_eq!(data["policy_rule_name"], "input-output-mint-equal");
    }

    #[tokio::test]
    async fn network_error_maps_to_quote_unavailable() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::err(JupiterError::Network(
            "dns failed".to_string(),
        ))));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1_000_000, 100))
            .await
            .unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "quote_unavailable");
        assert!(data["error"].as_str().unwrap().contains("dns failed"));
    }

    #[tokio::test]
    async fn api_error_maps_to_provider_error() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::err(JupiterError::Api {
            status: 503,
            message: "service unavailable".to_string(),
        })));
        let out = tool
            .execute(input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1_000_000, 100))
            .await
            .unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "provider_error");
        assert!(data["error"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn missing_input_mint_returns_invalid_input() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let mut input = input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1_000, 50);
        input.parameters["input_mint"] = json!("");
        let err = tool.execute(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn zero_amount_returns_invalid_input() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let mut input = input_for(SOL_MINT_BS58, USDC_MINT_BS58, 1, 50);
        input.parameters["input_amount"] = json!(0);
        let err = tool.execute(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[test]
    fn schema_required_set_and_no_secret_fields() {
        let tool = GetJupiterQuoteTool::new(Arc::new(StubQuote::ok("0", "0", vec![])));
        let spec = tool.spec();
        assert_eq!(spec.name, "get_jupiter_quote");
        assert!(spec.required_capabilities.is_empty());
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        let required: Vec<String> = spec.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let expected: std::collections::HashSet<&str> =
            ["input_mint", "output_mint", "input_amount", "slippage_bps"]
                .into_iter()
                .collect();
        let actual: std::collections::HashSet<&str> = required.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual, expected);
        // Slippage must be schema-capped at 100.
        assert_eq!(
            spec.input_schema["properties"]["slippage_bps"]["maximum"],
            json!(100)
        );

        // No secret-shaped tokens in the spec text.
        let raw = serde_json::to_string(&spec.input_schema).unwrap()
            + &serde_json::to_string(&spec.output_schema).unwrap();
        for forbidden in [
            "tx_bytes",
            "transaction_base64",
            "signed_tx",
            "private_key",
            "keypair",
            "Authorization",
        ] {
            assert!(
                !raw.contains(forbidden),
                "get_jupiter_quote schema must not mention `{forbidden}`; got {raw}"
            );
        }
    }
}
