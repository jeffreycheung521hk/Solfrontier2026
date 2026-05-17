//! Task supervisor — manages background task lifetimes.
//!
//! V1: simple restart-on-panic wrapper.
//! V2: exponential backoff + max restart count + alerting on repeated failure.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Spawns a supervised background task that restarts on panic.
/// The task factory `f` is called to create a new future each time.
pub fn spawn_supervised<F, Fut>(name: &'static str, f: F) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restarts = 0u32;
        loop {
            info!(task = name, restarts, "supervised task starting");
            let handle = tokio::spawn(f());
            match handle.await {
                Ok(()) => {
                    // Task completed normally (shouldn't happen for daemons)
                    warn!(task = name, "supervised task exited normally");
                    break;
                }
                Err(e) if e.is_panic() => {
                    restarts += 1;
                    error!(task = name, restarts, "supervised task panicked — restarting");
                    // Exponential backoff: 1s, 2s, 4s, max 30s
                    let delay = Duration::from_secs(1 << restarts.min(5));
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    error!(task = name, error = %e, "supervised task failed");
                    break;
                }
            }
        }
    })
}
