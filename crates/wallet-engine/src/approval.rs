//! Approval mode and approval handle types.
//!
//! The approval step is the critical human-in-the-loop gate in the
//! transaction pipeline. The `ApprovalMode` determines whether signing
//! is automatic or requires explicit operator confirmation.
//!
//! `ApprovalHandle` is a one-shot channel: the pipeline sends the handle to
//! the channel layer, which presents it to the operator, and the operator
//! responds via `ApprovalHandle::respond`. The pipeline awaits the response.

use std::time::Duration;

use tokio::sync::oneshot;
use tracing::instrument;

use crate::errors::WalletError;

/// How the approval step should behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// No human confirmation required. Use only on devnet/testnet or when
    /// the policy engine has explicitly approved.
    Automatic,

    /// The operator must explicitly confirm or reject.
    /// Blocks until confirmation or timeout.
    RequireHuman { timeout: Duration },

    /// Always dry-run; never actually sign. Useful for testing and audit.
    DryRun,

    /// The operator has already approved via the out-of-band API (POST /approve).
    /// Sign unconditionally — this is the resume path after a parked AwaitingApproval.
    /// ONLY use this in `resume_signing_after_approval()` after verifying the operator
    /// approved. Using this anywhere else bypasses the human gate.
    HumanGranted,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        // Default to DryRun — the operator must explicitly configure
        // a non-DryRun mode to enable real signing.
        ApprovalMode::DryRun
    }
}

/// A one-shot approval handle. Created by the pipeline and sent to the
/// operator's channel for confirmation.
pub struct ApprovalHandle {
    tx: oneshot::Sender<bool>,
}

impl ApprovalHandle {
    pub(crate) fn new() -> (Self, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, rx)
    }

    /// The operator calls this to approve or reject the transaction.
    pub fn respond(self, approved: bool) {
        // If the receiver is gone (pipeline timed out), this silently drops.
        let _ = self.tx.send(approved);
    }
}
