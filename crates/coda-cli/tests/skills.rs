// SPDX-License-Identifier: AGPL-3.0-only

//! Skills through the real daemon: offered by name and description, a body
//! only when the model asks for it. The faux provider logs every request.

mod common;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use coda_agents::trust_project;
use coda_config::config::Dirs;
use coda_protocol::paths::repo_identity;
use coda_store::Store;
use common::{FauxRig as Rig, Outcome, cli, faux_rig};
use serde_json::{Value, json};

const PDF_BODY: &str = "PDF-BODY-MARKER fill every field";
const DEPLOY_BODY: &str = "DEPLOY-BODY-MARKER push to staging";

fn skill(name: &str, description: &str, body: &str) -> String {
    format!("+++\nname = \"{name}\"\ndescription = {description:?}\n+++\n{body}\n")
}

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

fn skill_call_turn(name: &str) -> Value {
    json!({"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": "skill"},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": json!({"name": name}).to_string()},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

impl Rig {
    fn user_skills(&self) {
        write(
            self.home.path(),
            "skills/pdf.md",
            &skill("pdf", "Fills in PDF forms.", PDF_BODY),
        );
        write(
            self.home.path(),
            "skills/deploy.md",
            &skill("deploy", "Ships a release.", DEPLOY_BODY),
        );
    }

    async fn run(&self) -> Outcome {
        let cwd = self.work.path().display().to_string();
        cli(
            &self.ctx,
            &["coda", "run", "--prompt", "fill the form", "--cwd", &cwd],
            "",
        )
        .await
    }
}

/// Whether the model was offered the `skill` tool in `request`.
fn offers_skill(request: &Value) -> bool {
    request["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("skill"))
}

#[tokio::test]
async fn the_skill_tool_is_offered_and_no_body_reaches_the_provider_when_no_skill_is_used() {
    // Arrange
    let rig = faux_rig(&[text_turn("nothing to load")]);
    rig.user_skills();

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let requests = rig.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(offers_skill(&requests[0]), "{}", requests[0]);
    let sent = requests[0].to_string();
    assert!(!sent.contains("PDF-BODY-MARKER"), "{sent}");
    assert!(!sent.contains("DEPLOY-BODY-MARKER"), "{sent}");
}

#[tokio::test]
async fn a_body_enters_the_context_only_after_the_model_asks_for_that_skill() {
    // Arrange
    let rig = faux_rig(&[skill_call_turn("pdf"), text_turn("filled")]);
    rig.user_skills();

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "filled\n");
    let requests = rig.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(!requests[0].to_string().contains("PDF-BODY-MARKER"));
    let loaded = &requests[1]["messages"][2];
    assert_eq!(loaded["role"], "tool", "{loaded}");
    assert_eq!(loaded["is_error"], false, "{loaded}");
    assert!(
        loaded["text"].as_str().unwrap().contains(PDF_BODY),
        "{loaded}"
    );
    // The skill the model did not ask for stays out of both requests.
    for request in &requests {
        assert!(!request.to_string().contains("DEPLOY-BODY-MARKER"));
    }
}

#[tokio::test]
async fn with_no_skills_the_model_is_offered_no_skill_tool() {
    // Arrange
    let rig = faux_rig(&[text_turn("hello")]);

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let requests = rig.requests();
    assert_eq!(requests.len(), 1);
    assert!(!offers_skill(&requests[0]));
}

#[tokio::test]
async fn a_project_skill_is_offered_only_once_the_repository_is_trusted() {
    // Arrange
    let rig = faux_rig(&[text_turn("one"), text_turn("two")]);
    write(
        rig.work.path(),
        ".coda/skills/local.md",
        &skill("local", "A project skill.", "LOCAL-BODY-MARKER"),
    );

    // Act
    let before = rig.run().await;
    let mut store = Store::open(&Dirs::from_base(rig.home.path()).data_dir()).unwrap();
    trust_project(&mut store, &repo_identity(rig.work.path()).unwrap()).unwrap();
    drop(store);
    let after = rig.run().await;

    // Assert
    assert_eq!(before.code, ExitCode::SUCCESS, "{}", before.err);
    assert_eq!(after.code, ExitCode::SUCCESS, "{}", after.err);
    let requests = rig.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(!offers_skill(&requests[0]), "{}", requests[0]);
    assert!(offers_skill(&requests[1]), "{}", requests[1]);
    for request in &requests {
        assert!(!request.to_string().contains("LOCAL-BODY-MARKER"));
    }
}
