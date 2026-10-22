// SPDX-License-Identifier: AGPL-3.0-only

//! The seam between the turn loop and whatever runs a sub-agent for the
//! `task` tool.
//!
//! The loop only knows how to ask: it parses the call, says how much of the
//! session's token budget is left and hands over its cancel signal. The
//! daemon, which owns providers, permission prompts and the event log,
//! implements the running.

use std::future::Future;
use std::pin::Pin;

use coda_providers::model::Usage;

use crate::turn::CancelToken;

/// One `task` call, ready to run.
pub struct TaskRequest {
    /// The agent to run, by name.
    pub agent: String,
    /// What it is asked to do.
    pub prompt: String,
    /// Tokens the session may still spend. The child may spend at most this
    /// many, and what it spends counts against the session.
    pub budget: u64,
    /// The most model calls the child may make.
    pub max_turns: u32,
    /// The session's cancel signal. The child shares it, so cancelling the
    /// session cancels the child.
    pub cancel: CancelToken,
}

/// What a finished `task` call hands back.
pub struct TaskOutcome {
    /// The text the parent model gets as the tool result.
    pub text: String,
    /// Whether the parent model gets an error.
    pub is_error: bool,
    /// Tokens the child used.
    pub usage: Usage,
}

/// The future a [`TaskRunner`] returns.
pub type TaskFuture<'a> = Pin<Box<dyn Future<Output = TaskOutcome> + Send + 'a>>;

/// Runs the sub-agent a `task` call names.
pub trait TaskRunner: Send + Sync {
    /// Runs the call to the end. A failure is an outcome with `is_error` set,
    /// never a panic, so the parent model can read why.
    fn run(&self, request: TaskRequest) -> TaskFuture<'_>;
}
