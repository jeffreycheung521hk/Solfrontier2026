//! Tracing subscriber initialization.
//!
//! Call `init_tracing` exactly once at daemon startup.
//! The format and level are controlled by environment variables:
//!   RUST_LOG  — standard tracing log filter (e.g. "info,claw_gateway=debug")
//!   CLAW_LOG_FORMAT — "json" (default for production) or "pretty" (for dev)

use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Newline-delimited JSON. Use in production / container environments.
    Json,
    /// Human-readable colored output. Use in development.
    Pretty,
}

impl LogFormat {
    /// Reads `CLAW_LOG_FORMAT` from the environment.
    /// Defaults to `Pretty` for development; production configs should set
    /// this explicitly.
    pub fn from_env() -> Self {
        match std::env::var("CLAW_LOG_FORMAT")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "json" => LogFormat::Json,
            _      => LogFormat::Pretty,
        }
    }
}

/// Initialize the global tracing subscriber with an explicit filter string.
///
/// Prefer this over `init_tracing` when the filter level comes from config
/// or CLI args, because it avoids the need for `unsafe { std::env::set_var }`.
///
/// `filter_override`: if `Some`, used as the log filter (e.g. "info,claw_gateway=debug").
///                    If `None`, falls back to RUST_LOG env var or "info".
pub fn init_tracing_with_filter(format: LogFormat, filter_override: Option<&str>) {
    let filter = if let Some(f) = filter_override {
        EnvFilter::new(f)
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };
    apply_tracing(format, filter);
}

/// Initialize the global tracing subscriber.
///
/// Must be called before any `tracing::info!` or similar macros.
/// Calling this more than once in the same process will panic — use
/// `try_init_tracing` in test code.
pub fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    apply_tracing(format, filter);
}

fn apply_tracing(format: LogFormat, filter: EnvFilter) {
    match format {
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .json()
                        .with_span_events(FmtSpan::CLOSE)
                        .with_current_span(true)
                        .with_span_list(true),
                )
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .pretty()
                        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                        .with_target(true)
                        .with_thread_ids(false),
                )
                .init();
        }
    }
}

/// Like `init_tracing` but does not panic if a subscriber is already set.
/// Suitable for use in test modules.
pub fn try_init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));

    let _ = match format {
        LogFormat::Json => tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json())
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().pretty())
            .try_init(),
    };
}
