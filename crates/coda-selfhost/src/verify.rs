// SPDX-License-Identifier: AGPL-3.0-only

//! The acceptance checks on a finished attempt: the diff, the tests and the
//! printed version.

use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::NESTED_ENV;
use crate::error::SelfhostError;
use crate::process::run_bounded;
use crate::workspace::git;

/// Attributes that mark a test function.
const TEST_ATTRIBUTES: [&str; 2] = ["#[test]", "#[tokio::test"];

/// How much of a failed command's error output is kept.
const FAILURE_TAIL_CHARS: usize = 4000;

/// The package whose metadata `coda version --json` must report.
pub const PACKAGE: &str = "coda-cli";
/// The file the pinned change must touch.
pub const MAIN_RS: &str = "crates/coda-cli/src/main.rs";

/// Name and version of a crate as `cargo metadata` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub version: String,
}

/// Why an attempt was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("the attempt changed nothing")]
    NoDiff,
    #[error("the attempt did not touch {MAIN_RS}")]
    MainNotTouched,
    #[error("the attempt added no test")]
    NoTestAdded,
    #[error("{what} failed: {stderr}")]
    StepFailed { what: String, stderr: String },
    #[error("the metadata has no package {0:?}")]
    UnknownPackage(String),
    #[error("the metadata is not valid cargo metadata: {0}")]
    BadMetadata(String),
    #[error("`coda version --json` did not print a JSON object with string name and version: {0}")]
    NotJson(String),
    #[error("`coda version --json` printed {field} {found:?}, expected {expected:?}")]
    Mismatch {
        field: &'static str,
        found: String,
        expected: String,
    },
}

/// What to run against a finished attempt.
#[derive(Debug, Clone)]
pub struct VerifyPlan<'a> {
    pub workdir: &'a Path,
    /// Arguments after `cargo test`, for example `-p coda-cli`.
    pub cargo_test_args: Vec<String>,
    /// Build output directory, so cargo does not rebuild every dependency.
    pub target_dir: Option<&'a Path>,
    /// Bound for each cargo invocation.
    pub limit: Duration,
}

/// Runs every acceptance check on `plan.workdir`.
///
/// # Errors
/// Returns [`SelfhostError::Rejected`] for the first failed check, or the
/// error of a command that could not run.
pub fn verify_workdir(plan: &VerifyPlan<'_>) -> Result<(), SelfhostError> {
    // Intent-to-add makes files the attempt created show up in the diff.
    git(plan.workdir, &["add", "-A", "--intent-to-add"])?;
    let names = git(plan.workdir, &["diff", "HEAD", "--name-only"])?;
    let diff = git(plan.workdir, &["diff", "HEAD"])?;
    check_diff(&names.lines().collect::<Vec<_>>(), &diff)?;

    mark_sources_modified(plan.workdir)?;
    cargo(plan, "cargo test", &["test"], &plan.cargo_test_args).map_err(attempt_failed)?;
    let metadata = cargo(
        plan,
        "cargo metadata",
        &["metadata", "--no-deps", "--format-version", "1"],
        &[],
    )?;
    let expected = identity_from_metadata(&metadata, PACKAGE)?;
    let printed = cargo(
        plan,
        "coda version --json",
        &[
            "run", "--quiet", "-p", PACKAGE, "--bin", "coda", "--", "version", "--json",
        ],
        &[],
    )
    .map_err(attempt_failed)?;
    check_version_output(&printed, &expected)?;
    Ok(())
}

/// A command of the attempt's own making that exits non-zero rejects the
/// attempt; any other error is the verifier's, not the attempt's.
fn attempt_failed(error: SelfhostError) -> SelfhostError {
    match error {
        SelfhostError::CommandFailed { what, stderr, .. } => {
            VerifyError::StepFailed { what, stderr }.into()
        }
        other => other,
    }
}

/// Cargo decides what to rebuild by modification time, and a target directory
/// shared between copies holds outputs built from other sources. A fresh mtime
/// on every source file forces this copy's crates to be rebuilt.
fn mark_sources_modified(workdir: &Path) -> Result<(), SelfhostError> {
    let now = SystemTime::now();
    let listing = git(workdir, &["ls-files", "-z", "--cached"])?;
    for relative in listing.split('\0').filter(|name| !name.is_empty()) {
        let path = workdir.join(relative);
        File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(now))
            .map_err(|source| SelfhostError::Io { path, source })?;
    }
    Ok(())
}

/// Runs cargo in the attempt's directory and returns its standard output.
fn cargo(
    plan: &VerifyPlan<'_>,
    what: &str,
    args: &[&str],
    more: &[String],
) -> Result<String, SelfhostError> {
    let mut command = Command::new("cargo");
    command
        .args(args)
        .args(more)
        .current_dir(plan.workdir)
        .env(NESTED_ENV, "1");
    if let Some(target) = plan.target_dir {
        command.env("CARGO_TARGET_DIR", target);
    }
    let done = run_bounded(&mut command, plan.limit)?;
    if !done.status.success() {
        return Err(SelfhostError::CommandFailed {
            what: what.to_owned(),
            status: done.status.to_string(),
            stderr: tail(&done.stderr),
        });
    }
    Ok(done.stdout)
}

/// The end of `text`, where a build or test failure says why.
fn tail(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(FAILURE_TAIL_CHARS);
    text.chars().skip(skip).collect()
}

/// Checks the changed paths and the diff text of an attempt.
///
/// # Errors
/// Returns the first of [`VerifyError::NoDiff`], [`VerifyError::MainNotTouched`]
/// and [`VerifyError::NoTestAdded`] that applies.
pub fn check_diff(changed: &[&str], diff: &str) -> Result<(), VerifyError> {
    if changed.is_empty() {
        return Err(VerifyError::NoDiff);
    }
    if !changed.contains(&MAIN_RS) {
        return Err(VerifyError::MainNotTouched);
    }
    if !adds_test(diff) {
        return Err(VerifyError::NoTestAdded);
    }
    Ok(())
}

/// Whether `diff` adds more test attributes than it removes, so re-indenting
/// or moving an existing test does not count.
fn adds_test(diff: &str) -> bool {
    let attributes = |sign: char, header: &str| {
        diff.lines()
            .filter(|line| line.starts_with(sign) && !line.starts_with(header))
            .map(|line| line[1..].trim_start())
            .filter(|line| TEST_ATTRIBUTES.iter().any(|attr| line.starts_with(attr)))
            .count()
    };
    attributes('+', "+++") > attributes('-', "---")
}

/// Reads the identity of `package` from `cargo metadata --format-version 1` output.
///
/// # Errors
/// Returns [`VerifyError::BadMetadata`] or [`VerifyError::UnknownPackage`].
pub fn identity_from_metadata(metadata: &str, package: &str) -> Result<Identity, VerifyError> {
    let parsed: Value =
        serde_json::from_str(metadata).map_err(|e| VerifyError::BadMetadata(e.to_string()))?;
    let packages = parsed["packages"]
        .as_array()
        .ok_or_else(|| VerifyError::BadMetadata("no packages array".to_owned()))?;
    packages
        .iter()
        .find(|p| p["name"] == package)
        .and_then(|p| {
            Some(Identity {
                name: package.to_owned(),
                version: p["version"].as_str()?.to_owned(),
            })
        })
        .ok_or_else(|| VerifyError::UnknownPackage(package.to_owned()))
}

/// Checks that `stdout` is JSON whose `name` and `version` equal `expected`.
///
/// # Errors
/// Returns [`VerifyError::NotJson`] or [`VerifyError::Mismatch`].
pub fn check_version_output(stdout: &str, expected: &Identity) -> Result<(), VerifyError> {
    let not_json = |why: &str| VerifyError::NotJson(why.to_owned());
    let parsed: Value = serde_json::from_str(stdout).map_err(|e| not_json(&e.to_string()))?;
    let field = |key: &str| {
        parsed[key]
            .as_str()
            .ok_or_else(|| not_json(&format!("no string {key}")))
    };
    for (key, want) in [("name", &expected.name), ("version", &expected.version)] {
        let found = field(key)?;
        if found != want {
            return Err(VerifyError::Mismatch {
                field: key,
                found: found.to_owned(),
                expected: want.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DIFF: &str = "\
diff --git a/crates/coda-cli/tests/version.rs b/crates/coda-cli/tests/version.rs
--- a/crates/coda-cli/tests/version.rs
+++ b/crates/coda-cli/tests/version.rs
@@ -1,3 +1,8 @@
+#[test]
+fn it_works() {}
";

    fn identity() -> Identity {
        Identity {
            name: "coda-cli".to_owned(),
            version: "0.0.0".to_owned(),
        }
    }

    #[test]
    fn an_attempt_is_rejected_for_no_changes_no_main_edit_or_no_added_test() {
        // Arrange
        let no_test = "+++ b/x\n+fn helper() {}\n-#[test]\n";
        let reindented = "+++ b/x\n-#[test]\n+    #[test]\n";

        // Act and Assert
        assert_eq!(check_diff(&[], ""), Err(VerifyError::NoDiff));
        assert_eq!(
            check_diff(&["crates/coda-cli/tests/version.rs"], TEST_DIFF),
            Err(VerifyError::MainNotTouched)
        );
        for diff in [no_test, reindented] {
            assert_eq!(check_diff(&[MAIN_RS], diff), Err(VerifyError::NoTestAdded));
        }
    }

    #[test]
    fn main_plus_an_added_test_attribute_passes() {
        for diff in [
            "+++ b/x\n+#[test]\n",
            "+++ b/x\n+    #[tokio::test]\n",
            "+++ b/x\n+#[tokio::test(flavor = \"x\")]\n",
            "+++ b/x\n-#[test]\n+    #[test]\n+#[test]\n",
        ] {
            // Act and Assert
            assert_eq!(check_diff(&[MAIN_RS], diff), Ok(()), "{diff}");
        }
    }

    #[test]
    fn a_failed_attempt_command_rejects_but_other_errors_pass_through() {
        // Arrange
        let failed = SelfhostError::CommandFailed {
            what: "cargo test".to_owned(),
            status: "1".to_owned(),
            stderr: "boom".to_owned(),
        };
        let io = SelfhostError::Io {
            path: "x".into(),
            source: std::io::Error::other("e"),
        };

        // Act
        let (rejected, infra) = (attempt_failed(failed), attempt_failed(io));

        // Assert
        assert!(matches!(
            rejected,
            SelfhostError::Rejected(VerifyError::StepFailed { .. })
        ));
        assert!(matches!(infra, SelfhostError::Io { .. }));
    }

    #[test]
    fn identity_is_read_for_the_named_package_only() {
        // Arrange
        let metadata = r#"{"packages":[
            {"name":"other","version":"9.9.9"},
            {"name":"coda-cli","version":"0.0.0"}]}"#;

        // Act
        let found = identity_from_metadata(metadata, "coda-cli");
        let missing = identity_from_metadata(metadata, "nope");
        let broken = identity_from_metadata("not json", "coda-cli");

        // Assert
        assert_eq!(found, Ok(identity()));
        assert_eq!(missing, Err(VerifyError::UnknownPackage("nope".to_owned())));
        assert!(matches!(broken, Err(VerifyError::BadMetadata(_))));
    }

    #[test]
    fn the_printed_json_must_match_the_metadata() {
        // Arrange
        let ok = " {\"name\":\"coda-cli\",\"version\":\"0.0.0\",\"x\":1}\n";
        let mismatches = [
            (r#"{"name":"coda","version":"0.0.0"}"#, "name"),
            (r#"{"name":"coda-cli","version":"1.0.0"}"#, "version"),
        ];
        let malformed = ["coda-cli 0.0.0", "", "[]", r#"{"name":"coda-cli"}"#];

        // Act and Assert
        assert_eq!(check_version_output(ok, &identity()), Ok(()));
        for (stdout, field) in mismatches {
            let result = check_version_output(stdout, &identity());
            assert!(
                matches!(result, Err(VerifyError::Mismatch { field: f, .. }) if f == field),
                "{stdout}: {result:?}"
            );
        }
        for stdout in malformed {
            let result = check_version_output(stdout, &identity());
            assert!(matches!(result, Err(VerifyError::NotJson(_))), "{stdout}");
        }
    }
}
