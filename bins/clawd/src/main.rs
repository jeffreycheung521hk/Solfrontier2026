//! `clawd` — the ClawSolana daemon.
//!
//! Entry point for the always-on Solana agent control plane.
//!
//! # Startup sequence
//! 1. Parse CLI args
//! 2. Load config (TOML file, env var overrides)
//! 3. Initialise tracing (no `unsafe env::set_var` — filter passed directly)
//! 4. Run `GatewayDaemon::run()` which owns all further startup
//! 5. Block until SIGINT/SIGTERM
//!
//! # What belongs here
//! CLI argument parsing and the minimal wiring to `GatewayDaemon::run()`.
//!
//! # What does NOT belong here
//! Any business logic, Solana code, or agent routing.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing::info;

use claw_gateway::{ClawConfig, GatewayDaemon};
use claw_observability::{init_tracing_with_filter, init::LogFormat};

#[derive(Parser, Debug)]
#[command(
    name    = "clawd",
    about   = "ClawSolana daemon — Solana-native AI agent control plane",
    version,
    long_about = None
)]
struct Args {
    /// Path to the TOML config file.
    /// If not provided, uses the built-in dev defaults.
    #[arg(short, long, value_name = "FILE", env = "CLAW_CONFIG")]
    config: Option<PathBuf>,

    /// Override the log level (e.g. "debug", "info,claw_gateway=debug").
    /// If not provided, falls back to RUST_LOG env var, then to the config file,
    /// then to "info".
    #[arg(short, long, env = "RUST_LOG")]
    log_level: Option<String>,

    /// Use JSON log output (for production / log aggregators).
    #[arg(long, env = "CLAW_LOG_JSON")]
    log_json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load config first so we can use its log settings as defaults.
    let config = if let Some(path) = &args.config {
        ClawConfig::load(path)
            .with_context(|| format!("failed to load config from {}", path.display()))?
    } else {
        eprintln!("clawd: no --config provided, using dev defaults");
        ClawConfig::default_dev()
    };

    // CLI flag overrides config file for log format.
    let log_format = if args.log_json || config.logging.format == "json" {
        LogFormat::Json
    } else {
        LogFormat::Pretty
    };

    // Resolve the effective log filter — priority: CLI flag > RUST_LOG env > config > default.
    // We pass it directly to init_tracing_with_filter, avoiding unsafe { env::set_var }.
    //
    // NOTE: `--log-level` maps to `env = "RUST_LOG"` in clap, so if RUST_LOG is set in the
    // environment, it will appear in args.log_level. We don't need to separately read RUST_LOG.
    let filter_str: Option<String> = args.log_level
        .or_else(|| {
            let lvl = config.logging.level.clone();
            if lvl.is_empty() { None } else { Some(lvl) }
        });

    // Initialize tracing — no unsafe code, no environment mutation.
    init_tracing_with_filter(log_format, filter_str.as_deref());

    info!(
        version = env!("CARGO_PKG_VERSION"),
        network = %config.network.network,
        rpc     = %config.rpc.primary_url,
        "clawd starting"
    );

    GatewayDaemon::run(config)
        .await
        .context("gateway daemon exited with error")?;

    info!("clawd stopped cleanly");
    Ok(())
}
