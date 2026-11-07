// SPDX-License-Identifier: Apache-2.0

//! Provider traits, backend detection and the shared permission policy.

pub mod account;
pub mod anthropic;
pub mod backend;
pub mod claude_code;
pub mod claude_compact;
pub mod claude_helper;
pub mod claude_session;
pub mod codex;
pub mod codex_app_server;
pub mod codex_exec;
mod codex_limits;
pub mod detect;
pub mod emit;
pub mod faux;
pub mod gate;
mod http;
pub mod init_check;
mod jsonl;
pub mod model;
pub mod models;
pub mod openai_compat;
pub mod policy;
pub mod registry;
pub mod steer;
#[cfg(feature = "testing")]
pub mod testing;
pub mod think;
mod tool_call;
pub mod tool_request;
pub mod transcripts;
mod websocket;
