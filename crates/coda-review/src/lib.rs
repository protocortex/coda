// SPDX-License-Identifier: Apache-2.0

//! Pure building blocks of `coda review`: the finding schema, the parser for model
//! output, and the renderers. Nothing here touches the network or a model.

pub mod parse;
pub mod render;
pub mod schema;
