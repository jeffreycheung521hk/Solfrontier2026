//! `claw` — the ClawSolana CLI client.
//!
//! Communicates with the `clawd` daemon over its local HTTP API.
//! The daemon must be running before using this CLI.
//!
//! # Usage
//!
//! ```
//! claw health                        # check daemon health
//! claw session list                  # list active sessions
//! claw session open --role research  # open a new session
//! claw session close <id>            # close a session
//! ```
//!
//! The daemon URL defaults to `http://127.0.0.1:7070`.
//! Override with `--daemon-url` or `CLAW_DAEMON_URL`.
//!
//! The auth token is read from `CLAW_API_TOKEN` or `--token`.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7070";

#[derive(Parser, Debug)]
#[command(
    name    = "claw",
    about   = "ClawSolana CLI — control the clawd daemon",
    version,
    long_about = None
)]
struct Args {
    /// Daemon URL.
    #[arg(long, env = "CLAW_DAEMON_URL", default_value = DEFAULT_DAEMON_URL)]
    daemon_url: String,

    /// API bearer token (required for all commands except `health`).
    #[arg(long, env = "CLAW_API_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check daemon health.
    Health,

    /// Session management.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    /// List active sessions.
    List,
    /// Open a new session.
    Open {
        /// Agent role: research | execution | risk | ops
        #[arg(long, default_value = "research")]
        role: String,
    },
    /// Close a session by ID.
    Close {
        id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = Client::new();

    match args.command {
        Command::Health => {
            let url = format!("{}/health", args.daemon_url);
            let resp = client
                .get(&url)
                .send()
                .await
                .context("could not reach daemon — is clawd running?")?;

            let status = resp.status();
            let body: Value = resp.json().await.context("failed to parse health response")?;

            println!("{}", serde_json::to_string_pretty(&body)?);

            if status != StatusCode::OK {
                bail!("daemon reported unhealthy status: {}", status);
            }
        }

        Command::Session { action } => {
            let token = args
                .token
                .as_deref()
                .unwrap_or("")
                .to_string();

            match action {
                SessionAction::List => {
                    let url = format!("{}/sessions", args.daemon_url);
                    let resp = client
                        .get(&url)
                        .bearer_auth(&token)
                        .send()
                        .await
                        .context("request to daemon failed")?;

                    check_status(&resp)?;
                    let body: Value = resp.json().await?;
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }

                SessionAction::Open { role } => {
                    let url = format!("{}/sessions", args.daemon_url);
                    let resp = client
                        .post(&url)
                        .bearer_auth(&token)
                        .json(&json!({ "role": role }))
                        .send()
                        .await
                        .context("request to daemon failed")?;

                    check_status(&resp)?;
                    let body: Value = resp.json().await?;
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }

                SessionAction::Close { id } => {
                    let url = format!("{}/sessions/{}", args.daemon_url, id);
                    let resp = client
                        .delete(&url)
                        .bearer_auth(&token)
                        .send()
                        .await
                        .context("request to daemon failed")?;

                    check_status(&resp)?;
                    println!("session {} closed", id);
                }
            }
        }
    }

    Ok(())
}

fn check_status(resp: &reqwest::Response) -> anyhow::Result<()> {
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        bail!("unauthorized — set CLAW_API_TOKEN or use --token");
    }
    if !status.is_success() && status != StatusCode::NO_CONTENT {
        bail!("daemon returned error status: {}", status);
    }
    Ok(())
}
