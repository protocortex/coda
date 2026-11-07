// SPDX-License-Identifier: Apache-2.0

//! The structured helper call, driven against shell scripts that stand in for `claude`.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use coda_providers::claude_helper::{DEFAULT_TIMEOUT, HelperError, HelperRequest, run_structured};
use serde_json::{Value, json};

const PROMPT: &str = "Title this session: add a retry to the client";
const GOOD_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"structured_output":{"title":"Add retry"}}"#;
const SHORT: Duration = Duration::from_millis(300);
/// Long enough for the script to start its background child before the stop.
const GROUP_TIMEOUT: Duration = Duration::from_secs(3);
const KILL_BOUND: Duration = Duration::from_secs(10);
const GONE_POLL: Duration = Duration::from_millis(50);
const GONE_BOUND: Duration = Duration::from_secs(5);
const BACKGROUND_CHILD: &str = "sleep 60 &\necho $! > '{dir}/child.pid'\n";

struct Fake {
    dir: tempfile::TempDir,
    script: PathBuf,
}

impl Fake {
    /// A `claude` that records what it was given, then runs `tail`.
    fn new(tail: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("claude");
        let log = dir.path().join("log");
        let log = log.to_str().unwrap();
        let tail = tail.replace("{dir}", dir.path().to_str().unwrap());
        let body = format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{log}.args'; done\n\
             pwd > '{log}.cwd'\n\
             ls -A | wc -l > '{log}.cwdfiles'\n\
             cat > '{log}.stdin'\n\
             env | grep -c '^ANTHROPIC_API_KEY=' > '{log}.key'\n\
             {tail}\n"
        );
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, script }
    }

    fn logged(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(format!("log.{name}"))).unwrap()
    }

    fn args(&self) -> Vec<String> {
        self.logged("args").lines().map(str::to_owned).collect()
    }
}

fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

fn schema() -> Value {
    json!({"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]})
}

async fn ask(
    binary: &Path,
    parent_env: &[(OsString, OsString)],
    passthrough: &[String],
    model: Option<&str>,
    timeout: Duration,
) -> Result<Value, HelperError> {
    let schema = schema();
    run_structured(&HelperRequest {
        binary,
        parent_env,
        env_passthrough: passthrough,
        prompt: PROMPT,
        schema: &schema,
        model,
        timeout,
    })
    .await
}

#[tokio::test]
async fn the_structured_output_comes_back() {
    // Arrange
    let fake = Fake::new(&format!("echo '{GOOD_RESULT}'"));

    // Act
    let answer = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT).await;

    // Assert
    assert_eq!(answer.unwrap(), json!({"title": "Add retry"}));
}

#[tokio::test]
async fn the_call_has_no_tools_servers_settings_skills_or_saved_session() {
    // Arrange
    let fake = Fake::new(&format!("echo '{GOOD_RESULT}'"));

    // Act
    ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap();

    // Assert
    let args = fake.args();
    let value_of = |flag: &str| {
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1].clone()
    };
    assert!(args.contains(&"-p".to_owned()), "{args:?}");
    assert_eq!(value_of("--output-format"), "json");
    assert_eq!(value_of("--tools"), "");
    assert_eq!(value_of("--setting-sources"), "");
    assert_ne!(value_of("--system-prompt"), "");
    assert_eq!(
        serde_json::from_str::<Value>(&value_of("--json-schema")).unwrap(),
        schema()
    );
    for flag in [
        "--no-session-persistence",
        "--strict-mcp-config",
        "--disable-slash-commands",
    ] {
        assert!(args.contains(&flag.to_owned()), "{flag} in {args:?}");
    }
    assert!(!args.contains(&"--model".to_owned()));
}

#[tokio::test]
async fn a_model_is_passed_only_when_given() {
    // Arrange
    let fake = Fake::new(&format!("echo '{GOOD_RESULT}'"));

    // Act
    ask(&fake.script, &[], &[], Some("haiku"), DEFAULT_TIMEOUT)
        .await
        .unwrap();

    // Assert
    let args = fake.args();
    let at = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[at + 1], "haiku");
}

#[tokio::test]
async fn the_prompt_goes_on_stdin_from_an_empty_folder_that_is_removed_afterwards() {
    // Arrange
    let fake = Fake::new(&format!("echo '{GOOD_RESULT}'"));

    // Act
    ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap();

    // Assert
    assert_eq!(fake.logged("stdin"), PROMPT);
    assert_eq!(fake.logged("cwdfiles").trim(), "0");
    let cwd = PathBuf::from(fake.logged("cwd").trim());
    assert!(!cwd.exists(), "{} is left behind", cwd.display());
}

#[tokio::test]
async fn an_api_key_reaches_the_call_only_when_it_is_listed() {
    // Arrange
    let parent = env(&[("ANTHROPIC_API_KEY", "sk-test"), ("PATH", "/usr/bin:/bin")]);
    let listed = vec!["ANTHROPIC_API_KEY".to_owned()];
    let plain = Fake::new(&format!("echo '{GOOD_RESULT}'"));
    let opted_in = Fake::new(&format!("echo '{GOOD_RESULT}'"));

    // Act
    ask(&plain.script, &parent, &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    ask(&opted_in.script, &parent, &listed, None, DEFAULT_TIMEOUT)
        .await
        .unwrap();

    // Assert
    assert_eq!(plain.logged("key").trim(), "0");
    assert_eq!(opted_in.logged("key").trim(), "1");
}

#[tokio::test]
async fn a_result_that_reports_an_error_is_an_error() {
    // Arrange
    let fake = Fake::new(
        r#"echo '{"type":"result","subtype":"error_max_turns","is_error":true}'; exit 1"#,
    );

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(
        matches!(&err, HelperError::Reported { subtype, .. } if subtype == "error_max_turns"),
        "{err}"
    );
}

#[tokio::test]
async fn a_result_without_structured_output_is_an_error() {
    // Arrange
    let fake =
        Fake::new(r#"echo '{"type":"result","subtype":"success","is_error":false,"result":"hi"}'"#);

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::NoStructuredOutput), "{err}");
}

#[tokio::test]
async fn text_that_is_not_json_is_an_error() {
    // Arrange
    let fake = Fake::new("echo 'hello'");

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::NotJson), "{err}");
}

#[tokio::test]
async fn a_failed_exit_with_no_result_names_the_exit_code() {
    // Arrange
    let fake = Fake::new("exit 3");

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::Exited(Some(3))), "{err}");
}

#[tokio::test]
async fn output_over_the_limit_is_refused() {
    // Arrange
    let fake = Fake::new("head -c 3000000 /dev/zero | tr '\\0' 'a'");

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::TooLarge(_)), "{err}");
}

#[tokio::test]
async fn a_call_past_the_timeout_is_stopped() {
    // Arrange
    let fake = Fake::new("sleep 60");
    let started = Instant::now();

    // Act
    let err = ask(&fake.script, &[], &[], None, SHORT).await.unwrap_err();

    // Assert
    assert!(
        matches!(err, HelperError::TimedOut(d) if d == SHORT),
        "{err}"
    );
    assert!(started.elapsed() < KILL_BOUND, "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_missing_binary_names_its_path() {
    // Act
    let err = ask(
        Path::new("/nonexistent/claude"),
        &[],
        &[],
        None,
        DEFAULT_TIMEOUT,
    )
    .await
    .unwrap_err();

    // Assert
    assert!(err.to_string().contains("/nonexistent/claude"), "{err}");
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Waits for the pid in `<dir>/child.pid` to be gone, and says whether it went.
async fn background_child_is_gone(fake: &Fake) -> bool {
    let pid = fs::read_to_string(fake.dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .to_owned();
    let started = Instant::now();
    while alive(&pid) {
        if started.elapsed() > GONE_BOUND {
            let _ = Command::new("kill").args(["-9", &pid]).output();
            return false;
        }
        tokio::time::sleep(GONE_POLL).await;
    }
    true
}

#[tokio::test]
async fn a_timeout_stops_the_whole_process_group() {
    // Arrange
    let fake = Fake::new(&format!("{BACKGROUND_CHILD}wait"));

    // Act
    let err = ask(&fake.script, &[], &[], None, GROUP_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::TimedOut(_)), "{err}");
    assert!(background_child_is_gone(&fake).await);
}

#[tokio::test]
async fn a_finished_call_leaves_no_descendant_behind() {
    // Arrange
    let fake = Fake::new(&format!("{BACKGROUND_CHILD}echo '{GOOD_RESULT}'"));

    // Act
    ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap();

    // Assert
    assert!(background_child_is_gone(&fake).await);
}

#[tokio::test]
async fn too_much_output_is_refused_at_once_even_when_the_prompt_cannot_be_written() {
    // Arrange
    let fake = Fake::new(&format!(
        "{BACKGROUND_CHILD}head -c 3000000 /dev/zero | tr '\\0' 'a'; wait"
    ));
    let prompt = "x".repeat(1024 * 1024);
    let schema = schema();
    let started = Instant::now();

    // Act
    let err = run_structured(&HelperRequest {
        binary: &fake.script,
        parent_env: &[],
        env_passthrough: &[],
        prompt: &prompt,
        schema: &schema,
        model: None,
        timeout: DEFAULT_TIMEOUT,
    })
    .await
    .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::TooLarge(_)), "{err}");
    assert!(started.elapsed() < KILL_BOUND, "{:?}", started.elapsed());
    assert!(background_child_is_gone(&fake).await);
}

#[tokio::test]
async fn an_error_result_carries_the_message_claude_gave() {
    // Arrange
    let fake = Fake::new(
        r#"echo '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in"}'; exit 1"#,
    );

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("Not logged in"), "{err}");
}

#[tokio::test]
async fn a_failed_exit_beats_a_result_that_looks_fine() {
    // Arrange
    let fake = Fake::new(&format!("echo '{GOOD_RESULT}'; exit 4"));

    // Act
    let err = ask(&fake.script, &[], &[], None, DEFAULT_TIMEOUT)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, HelperError::Exited(Some(4))), "{err}");
}
