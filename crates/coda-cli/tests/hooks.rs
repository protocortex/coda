// SPDX-License-Identifier: AGPL-3.0-only

//! Hooks through `coda run` against the real daemon binary: they come from the
//! user's `config.toml` and from nowhere else.

mod common;

use std::path::Path;
use std::process::ExitCode;

use coda_cli::Context;
use common::{Outcome, StopDaemon, cli, context, short_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Far longer than a hook needs: the first run of a program can be slow on a busy machine.
const PATIENT_MS: u64 = 20_000;

/// A `[[hooks]]` entry running `script` through `/bin/sh` with `argument` as `$1`.
fn hook_entry(event: &str, script: &str, argument: &Path, blocking: bool) -> String {
    let command = json!([
        "/bin/sh",
        "-c",
        script,
        "hook",
        argument.display().to_string()
    ]);
    format!(
        "[[hooks]]\nevent = {event:?}\ncommand = {command}\nblocking = {blocking}\n\
         timeout_ms = {PATIENT_MS}\n\n"
    )
}

struct Rig {
    // First, so the daemon is stopped while its home directory still exists.
    _stop: StopDaemon,
    _home: TempDir,
    work: TempDir,
    ctx: Context,
}

/// A home whose config has `hooks` and a faux provider that answers once.
fn rig(hooks: &str) -> Rig {
    let home = short_dir();
    let reply = json!([{"events": [
        {"type": "text_delta", "text": "all done"},
        {"type": "usage", "input_tokens": 11, "output_tokens": 7},
        {"type": "stop", "reason": "end_turn"},
    ]}]);
    let script_path = home.path().join("script.json");
    std::fs::write(&script_path, reply.to_string()).unwrap();
    let faux = format!(
        "[providers.faux]\nenabled = true\nscript = {:?}\n",
        script_path.display().to_string()
    );
    std::fs::write(home.path().join("config.toml"), format!("{hooks}{faux}")).unwrap();
    let ctx = context(home.path());
    Rig {
        _stop: StopDaemon::new(home.path()),
        _home: home,
        work: short_dir(),
        ctx,
    }
}

impl Rig {
    async fn run(&self) -> Outcome {
        let cwd = self.work.path().display().to_string();
        let args = ["coda", "run", "--prompt", "hello", "--cwd", &cwd];
        cli(&self.ctx, &args, "").await
    }
}

#[tokio::test]
async fn a_session_start_hook_in_the_user_config_gets_the_session_as_json() {
    // Arrange
    let out = short_dir();
    let seen = out.path().join("start.json");
    let rig = rig(&hook_entry("session_start", r#"cat > "$1""#, &seen, false));

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "all done\n");
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(payload["event"], "session_start");
    assert_eq!(payload["resumed"], false);
    let cwd = std::fs::canonicalize(rig.work.path()).unwrap();
    assert_eq!(
        std::fs::canonicalize(payload["cwd"].as_str().unwrap()).unwrap(),
        cwd
    );
}

#[tokio::test]
async fn a_blocking_session_start_hook_stops_the_run_and_its_reason_goes_to_standard_error() {
    // Arrange
    let out = short_dir();
    let script = r#"echo "not on this network" >&2; exit 9"#;
    let rig = rig(&hook_entry("session_start", script, out.path(), true));

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(5), "{}", outcome.err);
    assert_eq!(outcome.out, "", "the reason is not model text");
    assert!(
        outcome.err.contains("not on this network"),
        "{}",
        outcome.err
    );
    assert!(
        outcome.err.contains("before the model was called"),
        "{}",
        outcome.err
    );
}

#[tokio::test]
async fn a_hook_in_a_project_file_never_runs() {
    // Arrange
    let out = short_dir();
    let (user, project) = (out.path().join("user.txt"), out.path().join("project.txt"));
    let rig = rig(&hook_entry(
        "session_start",
        r#"echo user > "$1""#,
        &user,
        false,
    ));
    let project_dir = rig.work.path().join(".coda");
    std::fs::create_dir_all(project_dir.join("commands")).unwrap();
    // The same hook as a project config file and as a key in a definition file.
    let entry = hook_entry("session_start", r#"echo project > "$1""#, &project, true);
    std::fs::write(project_dir.join("config.toml"), entry).unwrap();
    let command = format!(
        "+++\nname = \"ping\"\ndescription = \"d\"\nhooks = [{:?}]\n+++\nbody\n",
        project.display().to_string()
    );
    std::fs::write(project_dir.join("commands/ping.md"), command).unwrap();

    // Act
    let outcome = rig.run().await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(user.exists(), "the user's own hook did not run");
    assert!(!project.exists(), "a project file started a hook");
}
