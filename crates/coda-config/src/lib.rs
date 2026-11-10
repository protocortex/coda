// SPDX-License-Identifier: Apache-2.0

//! Parsing and defaults for coda's `config.toml`, plus injectable timing.

pub mod checkpoints;
pub mod checks;
pub mod config;
pub mod daemon;
pub mod describe;
pub mod fetch;
pub mod hooks;
pub mod loop_guard;
pub mod recall;
pub mod repo_map;
pub mod review;
pub mod routing;
pub mod timing;
pub mod tool_servers;
pub mod tui;
pub mod worktree;
