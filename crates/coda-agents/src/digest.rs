// SPDX-License-Identifier: AGPL-3.0-only

//! A digest of the project's definition files, recorded with the trust
//! decision so a later change to any of them withdraws the trust.

use std::fmt::Write;

use sha2::{Digest, Sha256};

use crate::catalog::{DefinitionFile, LoadProblem};

/// SHA-256, in lowercase hex, over the sorted kind folder, file name and
/// content hash of the files read. A file that could not be read adds a marker
/// instead, so its presence still changes the digest.
pub(crate) fn digest_of(files: &[DefinitionFile], problems: &[LoadProblem]) -> String {
    let mut lines: Vec<String> = files
        .iter()
        .map(|file| {
            let name = file
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            format!(
                "{}/{name}\0{}",
                file.kind.folder(),
                hex(&Sha256::digest(file.text.as_bytes()))
            )
        })
        .collect();
    lines.extend(problems.iter().map(|problem| {
        let name = problem
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        format!("{name}\0unreadable:{}", problem.field)
    }));
    lines.sort();
    hex(&Sha256::digest(lines.join("\n").as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
