// SPDX-License-Identifier: AGPL-3.0-only

//! Source language detection from file extensions.

use std::path::Path;

/// A language the indexer can extract symbols from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
}

impl Language {
    /// Detects the language from the file extension, or `None` if unsupported.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_extensions() {
        // Arrange
        let cases = [
            ("a/b.rs", Some(Language::Rust)),
            ("x.ts", Some(Language::TypeScript)),
            ("x.mts", Some(Language::TypeScript)),
            ("x.tsx", Some(Language::Tsx)),
            ("x.py", None),
            ("x.js", None),
            ("Makefile", None),
        ];
        for (path, expected) in cases {
            // Act
            let got = Language::from_path(Path::new(path));
            // Assert
            assert_eq!(got, expected, "{path}");
        }
    }
}
