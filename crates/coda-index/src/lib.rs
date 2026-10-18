// SPDX-License-Identifier: AGPL-3.0-only

//! Workspace walker and Tree-sitter symbol extraction for Rust and TypeScript.

pub mod lang;
pub mod symbols;
pub mod update;
pub mod walk;

pub use lang::Language;
pub use symbols::{Symbol, SymbolError, SymbolKind, extract};
pub use update::{FailedFile, UpdateError, UpdateReport, update_index};
pub use walk::{
    IndexedFile, SkipReason, Skipped, WalkError, WalkLimits, WalkReport, index_workspace,
};
