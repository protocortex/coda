// SPDX-License-Identifier: Apache-2.0

//! Pure building blocks of `coda review`: the finding schema, the parser for model
//! output, the renderers and the ignore and skip rules. Nothing here touches the network
//! or a model.

pub mod chunk;
pub mod diff;
pub mod guidance;
pub mod ignore;
pub mod issues;
pub mod memory;
pub mod pack;
pub mod parse;
pub mod post;
pub mod render;
pub mod request;
pub mod rules;
pub mod schema;
pub mod skip;
pub mod verify;
