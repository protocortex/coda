// SPDX-License-Identifier: AGPL-3.0-only

//! The coda daemon: a Unix socket server that speaks the coda JSON-RPC protocol.

pub mod dispatch;
pub mod lock;
pub mod server;
pub mod subscribe;
