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
    #[error("the API key variable {0} is not set; export it before running the live attempt")]
    MissingKey(String),
    #[error(
        "transcript files already exist in {}; remove docs/selfhost/attempt-* first so \
         none survive from an earlier run",
        .0.display()
    )]
    StaleTranscripts(PathBuf),
    #[error("{} is a symbolic link; refusing to write transcripts through it", .0.display())]
    TranscriptDirIsLink(PathBuf),
    #[error("{option} {} does not exist", path.display())]
    MissingPath { option: &'static str, path: PathBuf },
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
