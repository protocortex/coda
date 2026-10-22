// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run --command`: a command file becomes the prompt of a headless run.

mod common;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use coda_agents::trust_project;
use coda_config::config::Dirs;
use coda_protocol::paths::repo_identity;
use coda_store::Store;
use common::{Outcome, StopDaemon, cli, context, short_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const REQUEST_LOG: &str = "requests.jsonl";
const REVIEW: &str = "+++\nname = \"review\"\ndescription = \"Reviews a change.\"\n+++\n\
    Review $ARGUMENTS and report problems only.\n";
const STATUS: &str = "+++\nname = \"status\"\ndescription = \"Summarizes.\"\n+++\n\
    Summarize the repository.\n";
const DEPLOY: &str =
    "+++\nname = \"deploy\"\ndescription = \"Ships it.\"\n+++\nDeploy $ARGUMENTS\n";

struct Rig {
    // First, so the daemon is stopped while its home directory still exists.
    _stop: StopDaemon,
    home: TempDir,
    work: TempDir,
    ctx: coda_cli::Context,
}

/// A home whose faux provider answers once and logs the requests it gets, and a
/// work directory that is a repository.
fn rig() -> Rig {
    let home = short_dir();
    let work = short_dir();
    fs::create_dir(work.path().join(".git")).unwrap();
    let script = json!([{"events": [
        {"type": "text_delta", "text": "all done"},
        {"type": "stop", "reason": "end_turn"},
    ]}]);
    fs::write(home.path().join("script.json"), script.to_string()).unwrap();
    let config = format!(
        "[providers.faux]\nenabled = true\nscript = {:?}\nrequest_log = {:?}\n",
        home.path().join("script.json").display().to_string(),
        home.path().join(REQUEST_LOG).display().to_string(),
    );
    fs::write(home.path().join("config.toml"), config).unwrap();
    let ctx = context(home.path());
    Rig {
        _stop: StopDaemon::new(home.path()),
        home,
        work,
        ctx,
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

impl Rig {
    async fn run(&self, flags: &[&str]) -> Outcome {
        let cwd = self.work.path().display().to_string();
        let mut args = vec!["coda", "run", "--cwd", &cwd];
        args.extend_from_slice(flags);
        cli(&self.ctx, &args, "").await
    }

    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.home.path().join(REQUEST_LOG))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn a_command_file_with_arguments_becomes_the_prompt_of_the_run() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);

    // Act
    let outcome = rig
        .run(&["--command", "review", "--args", "PR 42 $ARGUMENTS"])
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "all done\n");
    let requests = rig.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["messages"][0],
        json!({
            "role": "user",
            "text": "Review PR 42 $ARGUMENTS and report problems only."
        })
    );
}

#[tokio::test]
async fn an_unknown_command_fails_before_any_model_call() {
    // Arrange
    let rig = rig();

    // Act
    let outcome = rig.run(&["--command", "nope"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("no command named nope"),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn arguments_for_a_command_without_a_placeholder_fail_before_any_model_call() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/status.md", STATUS);

    // Act
    let outcome = rig
        .run(&["--command", "status", "--args", "extra words"])
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("takes no arguments"),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn a_pending_project_command_does_not_run_until_the_repository_is_trusted() {
    // Arrange
    let rig = rig();
    write(rig.work.path(), ".coda/commands/deploy.md", DEPLOY);

    // Act
    let pending = rig.run(&["--command", "deploy", "--args", "staging"]).await;
    let held_back = rig.requests();
    let mut store = Store::open(&Dirs::from_base(rig.home.path()).data_dir()).unwrap();
    let root = repo_identity(rig.work.path()).unwrap();
    trust_project(&mut store, &root).unwrap();
    drop(store);
    let trusted = rig.run(&["--command", "deploy", "--args", "staging"]).await;

    // Assert
    assert_eq!(pending.code, ExitCode::from(70), "{}", pending.err);
    assert!(pending.err.contains("not in use"), "{}", pending.err);
    assert!(pending.err.contains("coda agents trust"), "{}", pending.err);
    assert_eq!(held_back, Vec::<Value>::new());
    assert_eq!(trusted.code, ExitCode::SUCCESS, "{}", trusted.err);
    assert_eq!(
        rig.requests()[0]["messages"][0],
        json!({"role": "user", "text": "Deploy staging"})
    );
}

#[tokio::test]
async fn a_name_that_only_looks_like_a_command_file_is_refused_before_any_model_call() {
    for name in ["../x", "Deploy", "deploy/"] {
        // Arrange
        let rig = rig();
        write(rig.home.path(), "commands/deploy.md", DEPLOY);
        // What a path trick would reach: a file next to the commands directory.
        write(rig.home.path(), "x.md", DEPLOY);

        // Act
        let outcome = rig.run(&["--command", name, "--args", "now"]).await;

        // Assert
        assert_eq!(outcome.code, ExitCode::from(70), "{name}: {}", outcome.err);
        assert!(
            outcome.err.contains(&format!("no command named {name}")),
            "{name}: {}",
            outcome.err
        );
        assert_eq!(rig.requests(), Vec::<Value>::new(), "{name}");
    }
}

#[tokio::test]
async fn a_command_and_a_prompt_together_are_a_usage_error() {
    // Arrange
    let rig = rig();

    // Act
    let outcome = rig.run(&["--command", "review", "--prompt", "hello"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(64), "{}", outcome.err);
    assert_eq!(rig.requests(), Vec::<Value>::new());
}
