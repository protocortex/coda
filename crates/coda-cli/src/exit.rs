// SPDX-License-Identifier: AGPL-3.0-only

//! The exit codes of `coda`.
//!
//! `coda run` reports why the turn stopped with the codes 0 to 5. Usage and
//! failure codes sit outside that range, so a script can tell a bad
//! invocation or a broken run from a turn that stopped. `coda doctor` has its
//! own pair of codes, 20 and 21, and `coda agents lint` has 22, for the same reason.

/// The turn ended normally.
pub const RUN_END_TURN: u8 = 0;
/// The turn used all of its model calls.
pub const RUN_MAX_TURNS: u8 = 1;
/// The session token budget ran out.
pub const RUN_BUDGET_EXHAUSTED: u8 = 2;
/// The request no longer fit the model context window.
pub const RUN_CONTEXT_OVERFLOW: u8 = 3;
/// The turn was cancelled.
pub const RUN_CANCELLED: u8 = 4;
/// The turn stopped for another reason, or failed outright. This includes a
/// compaction that could not summarize the conversation.
pub const RUN_OTHER: u8 = 5;
/// The command line was invalid.
pub const USAGE: u8 = 64;
/// The command could not run to its end: no prompt, no daemon, a lost connection.
pub const FAILURE: u8 = 70;
/// `coda doctor` found at least one failing check.
pub const DOCTOR_FAILURES: u8 = 20;
/// `coda doctor` found warnings but no failing check.
pub const DOCTOR_WARNINGS: u8 = 21;
/// `coda agents lint` found at least one problem.
pub const LINT_FAILURES: u8 = 22;
