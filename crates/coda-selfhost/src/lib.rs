// SPDX-License-Identifier: AGPL-3.0-only

//! Self-hosting acceptance: runs `coda run` on a pinned task in a temporary
//! copy of the repository, verifies the result and scrubs the transcript.

pub mod attempt;
pub mod error;
pub mod process;
pub mod scrub;
pub mod task;
pub mod verify;
pub mod workspace;

/// Set in the environment of the verifier's `cargo test`, so the acceptance
/// regression test inside it does not start another acceptance run.
pub const NESTED_ENV: &str = "CODA_SELFHOST_NESTED";
