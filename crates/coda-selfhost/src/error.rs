// SPDX-License-Identifier: AGPL-3.0-only

use std::path::PathBuf;

use crate::process::ProcessError;
use crate::verify::VerifyError;

/// Why a self-hosting step failed.
#[derive(Debug, thiserror::Error)]
pub enum SelfhostError {
    #[error("the attempt was rejected: {0}")]
    Rejected(#[from] VerifyError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("{what} failed ({status}): {stderr}")]
    CommandFailed {
        what: String,
        status: String,
        stderr: String,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}
