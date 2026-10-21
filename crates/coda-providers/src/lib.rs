// SPDX-License-Identifier: AGPL-3.0-only

//! Provider traits, backend detection and the shared permission policy.

pub mod anthropic;
pub mod backend;
pub mod claude_code;
pub mod claude_session;
pub mod codex;
pub mod codex_app_server;
pub mod codex_exec;
pub mod detect;
pub mod faux;
pub mod gate;
mod http;
mod jsonl;
pub mod model;
pub mod models;
pub mod openai_compat;
pub mod policy;
pub mod registry;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tool_request;
mod websocket;
