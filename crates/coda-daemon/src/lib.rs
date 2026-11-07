// SPDX-License-Identifier: Apache-2.0

//! The coda daemon: a Unix socket server that speaks the coda JSON-RPC protocol.

pub mod backends;
mod checkpoint;
mod checkpoint_diff;
mod checkpoint_prune;
mod checkpoints_rpc;
pub mod dispatch;
pub mod gate;
pub mod grants_rpc;
mod idle_pool;
pub mod lock;
pub mod model_session;
pub mod pidfile;
mod pr_watch;
mod pr_watch_gh;
mod pr_watches;
pub mod reaper;
mod receipts;
pub mod recover;
pub mod registry;
pub mod repair;
pub mod server;
pub mod session_rpc;
mod ship;
pub mod state;
pub mod subscribe;
pub mod supervisor;
pub mod trace;
pub mod turn;
pub mod usage_rpc;
pub mod watchdog;
pub mod worktree;
mod worktree_clean;
