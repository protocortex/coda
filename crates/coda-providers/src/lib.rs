// SPDX-License-Identifier: AGPL-3.0-only

//! Provider traits, backend detection and the shared permission policy.

pub mod anthropic;
pub mod backend;
pub mod claude_code;
pub mod claude_session;
pub mod detect;
pub mod faux;
pub mod gate;
pub mod model;
pub mod openai_compat;
pub mod policy;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tool_request;
