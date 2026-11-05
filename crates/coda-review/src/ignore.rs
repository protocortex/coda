// SPDX-License-Identifier: Apache-2.0

//! Globs for files a review leaves out.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::rules::RulesError;

/// Generated or machine-written files that are not worth a review. A glob with no `/`
/// matches the file name at any depth.
pub const BUILTIN_GLOBS: [&str; 17] = [
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "poetry.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.pb.go",
    "*_pb2.py",
    "*_pb2_grpc.py",
];

const ANY_DEPTH_PREFIX: &str = "**/";
const SEPARATOR: char = '/';
const CURRENT_DIR_PREFIX: &str = "./";

/// A compiled set of ignore globs.
#[derive(Debug, Clone)]
pub struct IgnoreSet {
    set: GlobSet,
}

impl IgnoreSet {
    /// Compiles `globs`, plus the built-in list when `builtin` is true.
    ///
    /// # Errors
    /// Returns [`RulesError::BadGlob`] for a glob that does not parse.
    pub fn new(globs: &[String], builtin: bool) -> Result<Self, RulesError> {
        let mut builder = GlobSetBuilder::new();
        let builtin_globs: &[&str] = if builtin { &BUILTIN_GLOBS } else { &[] };
        let all = globs
            .iter()
            .map(String::as_str)
            .chain(builtin_globs.iter().copied());
        for pattern in all {
            let rooted = pattern.strip_prefix(SEPARATOR);
            let body = rooted.unwrap_or(pattern);
            let anchored = if rooted.is_some() || body.contains(SEPARATOR) {
                body.to_owned()
            } else {
                format!("{ANY_DEPTH_PREFIX}{body}")
            };
            let glob = GlobBuilder::new(&anchored)
                .literal_separator(true)
                .build()
                .map_err(|e| RulesError::BadGlob {
                    pattern: pattern.to_owned(),
                    reason: e.to_string(),
                })?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|e| RulesError::BadGlob {
            pattern: String::new(),
            reason: e.to_string(),
        })?;
        Ok(Self { set })
    }

    /// True when `path`, relative to the repository root, matches a glob.
    #[must_use]
    pub fn is_ignored(&self, path: &str) -> bool {
        self.set
            .is_match(path.strip_prefix(CURRENT_DIR_PREFIX).unwrap_or(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(globs: &[&str], builtin: bool) -> IgnoreSet {
        let globs: Vec<String> = globs.iter().map(|g| (*g).to_owned()).collect();
        IgnoreSet::new(&globs, builtin).unwrap()
    }

    #[test]
    fn glob_matching_table() {
        // Arrange
        let cases = [
            ("**/generated/**", "generated/a.rs", true),
            ("**/generated/**", "src/x/generated/deep/a.rs", true),
            ("**/generated/**", "src/gen/a.rs", false),
            ("src/**/*.rs", "src/a.rs", true),
            ("src/**/*.rs", "src/a/b/c.rs", true),
            ("src/**/*.rs", "lib/a.rs", false),
            ("src/*.rs", "src/a/b.rs", false),
            ("src/*.rs", "src/b.rs", true),
            ("*.snap", "tests/out/a.snap", true),
            ("*.snap", "a.snap", true),
            ("docs/**", "docs/a/b.md", true),
            ("docs/**", "other/docs/a.md", false),
            ("a?.txt", "ab.txt", true),
            ("a?.txt", "a/b.txt", false),
            ("/docs/**", "docs/a.md", true),
            ("/docs/**", "x/docs/a.md", false),
            ("/build", "build", true),
            ("/build", "x/build", false),
            ("/*.md", "README.md", true),
            ("/*.md", "docs/README.md", false),
        ];

        for (glob, path, expected) in cases {
            // Act
            let got = set(&[glob], false).is_ignored(path);

            // Assert
            assert_eq!(got, expected, "{glob} vs {path}");
        }
    }

    #[test]
    fn builtin_list_covers_lockfiles_minified_maps_and_protobuf_at_any_depth() {
        // Arrange
        let ignore = set(&[], true);
        let ignored = [
            "Cargo.lock",
            "web/package-lock.json",
            "a/b/yarn.lock",
            "go.sum",
            "static/app.min.js",
            "static/app.min.css",
            "static/app.js.map",
            "api/v1/svc.pb.go",
            "py/svc_pb2.py",
            "py/svc_pb2_grpc.py",
        ];
        let kept = ["src/lib.rs", "Cargo.toml", "docs/lock.md", "min.js.txt"];

        // Act and Assert
        for path in ignored {
            assert!(ignore.is_ignored(path), "{path}");
        }
        for path in kept {
            assert!(!ignore.is_ignored(path), "{path}");
        }
    }

    #[test]
    fn builtin_can_be_turned_off() {
        // Arrange and Act
        let ignore = set(&[], false);

        // Assert
        assert!(!ignore.is_ignored("Cargo.lock"));
    }

    #[test]
    fn a_leading_dot_slash_is_ignored_and_a_bad_glob_is_an_error() {
        // Arrange
        let ignore = set(&["docs/**"], false);

        // Act and Assert
        assert!(ignore.is_ignored("./docs/a.md"));
        let err = IgnoreSet::new(&["[".to_owned()], false).unwrap_err();
        assert!(matches!(err, RulesError::BadGlob { .. }));
    }
}
