// SPDX-License-Identifier: Apache-2.0

//! Pure building blocks of `coda review`: the finding schema, the parser for model
//! output, the renderers and the ignore and skip rules. Nothing here touches the network
//! or a model.

pub mod ignore;
pub mod parse;
pub mod render;
pub mod rules;
pub mod schema;
pub mod skip;
