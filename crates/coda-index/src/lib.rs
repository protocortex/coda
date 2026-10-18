// SPDX-License-Identifier: AGPL-3.0-only

//! Workspace walker and Tree-sitter symbol extraction for Rust and TypeScript.

pub mod lang;
pub mod symbols;
pub mod walk;

pub use lang::Language;
pub use symbols::{Symbol, SymbolError, SymbolKind, extract};
pub use walk::{
    IndexedFile, SkipReason, Skipped, WalkError, WalkLimits, WalkReport, index_workspace,
};
