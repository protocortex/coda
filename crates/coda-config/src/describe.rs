// SPDX-License-Identifier: Apache-2.0

//! The `[describe]` table: the labels `coda describe --labels` may suggest.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const DESCRIBE_KEY: &str = "describe";
/// Key of the label list inside `[describe]`.
pub const LABELS_KEY: &str = "labels";
/// Key of a label's name.
pub const LABEL_NAME_KEY: &str = "name";
/// Key of the line that says when a label fits.
pub const LABEL_USE_WHEN_KEY: &str = "use_when";

/// The `[describe]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DescribeConfig {
    /// The labels that may be suggested, each with the line that says when it fits. A label
    /// that is not here is never suggested, whatever the repository has.
    pub labels: Vec<LabelRule>,
}

/// One entry of `[[describe.labels]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelRule {
    /// The label's name, as the repository spells it. Must not be empty.
    pub name: String,
    /// When the label fits, in one line the model reads. Must not be empty.
    pub use_when: String,
}
