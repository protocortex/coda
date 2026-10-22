// SPDX-License-Identifier: AGPL-3.0-only

//! The skill tool: an index of names and descriptions, bodies only on request.

use std::fs;
use std::path::Path;

use coda_agents::{
    Catalog, MAX_INDEX_BYTES, MAX_INDEX_DESCRIPTION_CHARS, MAX_SKILL_BODY_BYTES, SkillTool,
    trust_project,
};
use coda_store::Store;
use coda_tools::{Tool, ToolError};
use serde_json::json;
use tempfile::TempDir;

fn skill(name: &str, description: &str, body: &str) -> String {
    format!("+++\nname = \"{name}\"\ndescription = {description:?}\n+++\n{body}\n")
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

struct World {
    user: TempDir,
    repo: TempDir,
    data: TempDir,
}

fn world() -> World {
    World {
        user: TempDir::new().unwrap(),
        repo: TempDir::new().unwrap(),
        data: TempDir::new().unwrap(),
    }
}

impl World {
    fn user_skill(&self, name: &str, description: &str, body: &str) {
        let text = skill(name, description, body);
        write(self.user.path(), &format!("skills/{name}.md"), &text);
    }

    fn project_skill(&self, name: &str, description: &str, body: &str) {
        let text = skill(name, description, body);
        write(self.repo.path(), &format!(".coda/skills/{name}.md"), &text);
    }

    fn untrusted(&self) -> Option<SkillTool> {
        let catalog = Catalog::load(self.user.path(), Some(self.repo.path()));
        SkillTool::from_catalog(&catalog)
    }

    fn trusted(&self) -> Option<SkillTool> {
        let mut store = Store::open(&self.data.path().join("data")).unwrap();
        trust_project(&mut store, self.repo.path()).unwrap();
        let catalog =
            Catalog::load_trusted(&store, self.user.path(), Some(self.repo.path())).unwrap();
        SkillTool::from_catalog(&catalog)
    }
}

fn load(tool: &SkillTool, name: &str) -> Result<String, ToolError> {
    tool.call(&json!({ "name": name })).map(|out| out.text)
}

#[test]
fn no_skills_means_no_tool() {
    // Arrange
    let w = world();

    // Act
    let tool = w.untrusted();

    // Assert
    assert!(tool.is_none());
}

#[test]
fn the_index_lists_each_skill_by_name_and_description_and_holds_no_body() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "SECRET-BODY-PDF steps");
    w.user_skill("deploy", "Ships a release.", "SECRET-BODY-DEPLOY steps");

    // Act
    let tool = w.untrusted().unwrap();

    // Assert
    let index = tool.description();
    assert!(index.contains("- deploy: Ships a release."), "{index}");
    assert!(index.contains("- pdf: Fills in PDF forms."), "{index}");
    assert!(!index.contains("SECRET-BODY"), "{index}");
    assert_eq!(tool.names(), ["deploy", "pdf"]);
}

#[test]
fn calling_a_skill_returns_its_body_and_only_that_skills_body() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "Open the form, then fill it.");
    w.user_skill("deploy", "Ships a release.", "SECRET-BODY-DEPLOY steps");
    let tool = w.untrusted().unwrap();

    // Act
    let text = load(&tool, "pdf").unwrap();

    // Assert
    assert!(text.contains("Open the form, then fill it."), "{text}");
    assert!(text.contains("pdf"), "{text}");
    assert!(!text.contains("SECRET-BODY-DEPLOY"), "{text}");
}

#[test]
fn an_unknown_skill_is_an_error_that_lists_the_known_ones() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "body");
    let tool = w.untrusted().unwrap();

    // Act
    let result = load(&tool, "nope");

    // Assert
    let message = result.unwrap_err().to_string();
    assert!(message.contains("unknown skill `nope`"), "{message}");
    assert!(message.contains("pdf"), "{message}");
}

#[test]
fn arguments_other_than_a_name_are_refused() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "body");
    let tool = w.untrusted().unwrap();

    // Act
    let missing = tool.call(&json!({}));
    let extra = tool.call(&json!({"name": "pdf", "path": "/etc/passwd"}));

    // Assert
    assert!(matches!(missing, Err(ToolError::InvalidInput { .. })));
    assert!(matches!(extra, Err(ToolError::InvalidInput { .. })));
}

#[test]
fn a_body_over_the_cap_is_cut_and_says_so() {
    // Arrange
    let w = world();
    // Over the body cap but under the 64 KiB a definition file may be.
    let body = "x".repeat(MAX_SKILL_BODY_BYTES + 4 * 1024);
    w.user_skill("big", "A big one.", &body);
    let tool = w.untrusted().unwrap();

    // Act
    let output = tool.call(&json!({"name": "big"})).unwrap();

    // Assert
    assert!(output.truncated);
    assert!(output.text.contains("cut at"), "{}", output.text);
    assert!(output.text.len() < MAX_SKILL_BODY_BYTES + 512);
}

#[test]
fn a_description_over_the_cap_is_cut_in_the_index_and_kept_on_one_line() {
    // Arrange
    let w = world();
    let long = format!(
        "{}\nforged: a second entry",
        "d".repeat(MAX_INDEX_DESCRIPTION_CHARS)
    );
    w.user_skill("wordy", &long, "body");

    // Act
    let tool = w.untrusted().unwrap();

    // Assert
    let entries: Vec<&str> = tool
        .description()
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    assert_eq!(entries.len(), 1, "{}", tool.description());
    assert!(entries[0].chars().count() <= "- wordy: ".len() + MAX_INDEX_DESCRIPTION_CHARS + 3);
    assert!(
        !tool.description().contains("forged"),
        "{}",
        tool.description()
    );
}

#[test]
fn a_character_that_straddles_the_body_cap_is_dropped_whole() {
    // Arrange
    let w = world();
    // The euro sign is three bytes and starts one byte before the cap.
    let body = format!("{}\u{20ac}tail", "x".repeat(MAX_SKILL_BODY_BYTES - 1));
    w.user_skill("edge", "Ends on a multibyte character.", &body);
    let tool = w.untrusted().unwrap();

    // Act
    let output = tool.call(&json!({"name": "edge"})).unwrap();

    // Assert
    assert!(output.truncated);
    assert!(!output.text.contains('\u{20ac}'), "the character was cut");
    assert!(!output.text.contains("tail"));
    assert!(output.text.contains("cut at"), "{}", output.text);
}

#[test]
fn a_multibyte_description_is_cut_on_characters_at_the_cap() {
    // Arrange
    let w = world();
    let exact = "\u{e9}".repeat(MAX_INDEX_DESCRIPTION_CHARS);
    w.user_skill("exact", &exact, "body");
    w.user_skill("over", &format!("{exact}\u{e9}\u{e9}"), "body");

    // Act
    let tool = w.untrusted().unwrap();

    // Assert
    let text = tool.description();
    assert!(text.contains(&format!("- exact: {exact}\n")), "{text}");
    assert!(text.contains(&format!("- over: {exact}...\n")), "{text}");
}

#[test]
fn a_skill_file_overwritten_after_the_tool_was_built_does_not_change_what_it_returns() {
    // Arrange
    let w = world();
    w.project_skill("local", "A project skill.", "TRUSTED-BODY");
    let tool = w.trusted().unwrap();
    w.project_skill("local", "A project skill.", "SWAPPED-BODY");

    // Act
    let text = load(&tool, "local").unwrap();

    // Assert
    assert!(text.contains("TRUSTED-BODY"), "{text}");
    assert!(!text.contains("SWAPPED-BODY"), "{text}");
}

#[test]
fn tools_from_the_same_files_are_equal_and_an_edit_makes_them_differ() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "body one");
    let first = w.untrusted().unwrap();
    let same = w.untrusted().unwrap();
    w.user_skill("pdf", "Fills in PDF forms.", "body two");

    // Act
    let edited = w.untrusted().unwrap();

    // Assert
    assert_eq!(first, same);
    assert_ne!(first, edited);
}

#[test]
fn skills_past_the_index_cap_are_left_out_and_counted() {
    // Arrange
    let w = world();
    let total = 200;
    for n in 0..total {
        w.user_skill(&format!("skill-{n:03}"), &"d".repeat(100), "body");
    }

    // Act
    let tool = w.untrusted().unwrap();

    // Assert
    let listed = tool.names().len();
    assert!(listed > 0 && listed < total, "{listed}");
    let list_bytes: usize = tool
        .description()
        .lines()
        .filter(|line| line.starts_with("- "))
        .map(|line| line.len() + 1)
        .sum();
    assert!(list_bytes <= MAX_INDEX_BYTES, "{list_bytes}");
    let left_out = total - listed;
    assert!(
        tool.description()
            .contains(&format!("left out of this list: {left_out}")),
        "{}",
        tool.description()
    );
    // A skill that is not in the index is not loadable either.
    let unlisted = format!("skill-{:03}", total - 1);
    assert!(load(&tool, &unlisted).is_err());
}

#[test]
fn a_project_skill_waiting_for_trust_is_neither_offered_nor_loadable() {
    // Arrange
    let w = world();
    w.user_skill("pdf", "Fills in PDF forms.", "body");
    w.project_skill("local", "A project skill.", "PROJECT-BODY");

    // Act
    let tool = w.untrusted().unwrap();

    // Assert
    assert!(
        !tool.description().contains("local"),
        "{}",
        tool.description()
    );
    assert!(load(&tool, "local").is_err());
}

#[test]
fn a_trusted_project_skill_is_offered_and_loads_as_instructions() {
    // Arrange
    let w = world();
    w.project_skill("local", "A project skill.", "PROJECT-BODY");

    // Act
    let tool = w.trusted().unwrap();

    // Assert
    assert!(tool.description().contains("- local: A project skill."));
    let text = load(&tool, "local").unwrap();
    assert!(text.contains("PROJECT-BODY"), "{text}");
    assert!(text.contains("project"), "{text}");
}

#[test]
fn a_project_skill_edited_after_it_was_trusted_is_not_offered() {
    // Arrange
    let w = world();
    w.project_skill("local", "A project skill.", "PROJECT-BODY");
    let mut store = Store::open(&w.data.path().join("data")).unwrap();
    trust_project(&mut store, w.repo.path()).unwrap();
    w.project_skill("local", "A project skill.", "SWAPPED-BODY");

    // Act
    let catalog = Catalog::load_trusted(&store, w.user.path(), Some(w.repo.path())).unwrap();

    // Assert
    assert!(SkillTool::from_catalog(&catalog).is_none());
}
