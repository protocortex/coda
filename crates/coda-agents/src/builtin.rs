// SPDX-License-Identifier: AGPL-3.0-only

//! The agents that ship in the binary. They are ordinary agent files, parsed
//! and linted like any other, and a user or project file of the same name
//! replaces them under the usual rules.

use std::path::PathBuf;

use crate::catalog::{DefinitionFile, Kind, Snapshot};

/// File stem and text of each built-in agent.
const AGENTS: [(&str, &str); 4] = [
    ("explore", include_str!("../builtin/explore.md")),
    ("implementer", include_str!("../builtin/implementer.md")),
    ("reviewer", include_str!("../builtin/reviewer.md")),
    ("tester", include_str!("../builtin/tester.md")),
];

/// The built-in files, as if they had been read from a directory.
pub(crate) fn snapshot() -> Snapshot {
    let files = AGENTS
        .iter()
        .map(|(stem, text)| DefinitionFile {
            kind: Kind::Agent,
            stem: (*stem).to_owned(),
            path: PathBuf::from(format!("builtin/{stem}.md")),
            text: (*text).to_owned(),
        })
        .collect();
    Snapshot {
        files,
        ..Snapshot::default()
    }
}
