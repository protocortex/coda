// SPDX-License-Identifier: AGPL-3.0-only

//! The coda daemon: a Unix socket server that speaks the coda JSON-RPC protocol.

pub mod backends;
pub mod dispatch;
pub mod gate;
pub mod grants_rpc;
pub mod lock;
pub mod pidfile;
pub mod reaper;
pub mod server;
pub mod session_rpc;
pub mod state;
pub mod subscribe;
pub mod trace;
pub mod turn;
pub mod usage_rpc;
pub mod watchdog;
