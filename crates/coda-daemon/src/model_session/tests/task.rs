// SPDX-License-Identifier: AGPL-3.0-only

//! The `task` tool end to end: a model-API session dispatches a named agent as
//! a child conversation, against the faux provider. One script feeds both
//! loops in call order: the parent's turn, then the child's, then the parent's.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_agents::{Status, Tier, trust_project};
use coda_config::config::Config;
use coda_core::prompt::{Conversation, Prefix};
use coda_core::turn::{StopCondition, ToolSet, TurnConfig, TurnError, TurnEvent};
use coda_protocol::paths::repo_identity;
use coda_protocol::types::{BackendKind, Mode};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::registry::{ModelRegistry, RegistryError};
use coda_tools::task::MAX_DEPTH;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{HOME_DIR, Rig, dump, text_turn, tool_turn, user_message};
use crate::gate::BrokerGate;
use crate::model_session::task::{ChildEnv, ChildRunner, LoopParts, child_target, status_of};
use crate::model_session::{COMPACT_THRESHOLD_BYTES, Provider};
use crate::turn::Turns;

const BODY: &str = "You are a scout. Report what you find.\n";
const PROMPT: &str = "find the needle";
const TOUCH: &str = "touch ran.txt";
const AGENT: &str = "scout";
const DENY_TIMEOUT: Duration = Duration::from_millis(100);
/// A file that a build or hook runs, so an edit to it prompts even in accept-edits mode.
const SCRIPT: &str = "scripts/x.sh";
const EDIT_TEXT: &str = "edit scripts/x.sh";
const MODEL_DOWN: &str =
    r#"{"events": [], "error": {"class": "fatal", "message": "the model is down"}}"#;

fn agent_file(name: &str, tools: &str, body: &str) -> String {
    format!(
        "+++\nname = \"{name}\"\n\
         description = \"Does {name}. Use when asked. Do not use otherwise.\"\n\
         tier = \"fast\"\neffort = \"low\"\ntools = [{tools}]\n+++\n{body}"
    )
}

fn task_call(id: &str, agent: &str) -> Value {
    tool_turn(&[(id, "task", json!({"agent": agent, "prompt": PROMPT}))])
}

fn edit_call(id: &str, old: &str, new: &str) -> Value {
    let edit = json!({"path": SCRIPT, "old_string": old, "new_string": new});
    tool_turn(&[(id, "edit", edit)])
}

fn bash_call(id: &str) -> Value {
    tool_turn(&[(id, "bash", json!({ "command": TOUCH }))])
}

/// A rig in accept-edits mode whose daemon knows the user agent `scout`.
fn scout_rig(script: &[Value], tools: &str, tune: impl FnOnce(&mut Config)) -> Rig {
    let rig = Rig::custom(Mode::AcceptEdits, script, true, None, tune);
    let dir = rig.data.path().join(HOME_DIR).join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scout.md"), agent_file("scout", tools, BODY)).unwrap();
    rig
}

/// The text and asking agent of the first waiting prompt.
async fn waiting(rig: &Rig) -> (String, Option<String>) {
    let pending = rig.pending().await;
    (pending[0].command.clone(), pending[0].agent.clone())
}

/// What a prompt of the agent `scout` for `text` looks like.
fn scouts(text: &str) -> (String, Option<String>) {
    (text.to_owned(), Some(AGENT.to_owned()))
}

/// Waits for the `results`th turn to end.
async fn finish(rig: &Rig, results: usize) {
    rig.wait_for_results(results).await;
    rig.wait_until_idle().await;
}

/// The tool results among the messages of the faux request `request`.
fn tool_results(request: &Value) -> Vec<Value> {
    let messages = request["messages"].as_array().unwrap();
    let results = messages.iter().filter(|m| m["role"] == "tool");
    results.cloned().collect()
}

/// The tokens the usage events add up to.
fn spent(usage: &[Value]) -> u64 {
    let tokens = |e: &Value| {
        e["usage"]["input_tokens"].as_u64().unwrap() + e["usage"]["output_tokens"].as_u64().unwrap()
    };
    usage.iter().map(tokens).sum()
}

fn text_of(result: &Value) -> &str {
    result["text"].as_str().unwrap()
}

#[tokio::test]
async fn a_child_runs_fresh_with_only_its_tools_and_cannot_call_task_or_others() {
    // Arrange
    let nested = json!({"agent": "scout", "prompt": "again"});
    let edit = json!({"path": "a.txt", "old_string": "needle", "new_string": "thread"});
    let script = [
        task_call("t1", "scout"),
        tool_turn(&[("k1", "task", nested), ("k2", "edit", edit)]),
        text_turn("it is in a.txt"),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\", \"grep\"", |_| {});

    // Act
    rig.send("please search").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 4);
    assert!(
        lines[0]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("task"))
    );
    let child = &lines[1];
    assert_eq!(child["system"], BODY);
    assert_eq!(child["tools"], json!(["grep", "read"]));
    assert_eq!(child["messages"], json!([user_message(PROMPT)]));
    let refused = tool_results(&lines[2]);
    assert!(refused.iter().all(|r| r["is_error"] == true), "{refused:?}");
    assert!(text_of(&refused[0]).contains("cannot start sub-agents"));
    assert!(text_of(&refused[1]).contains("unknown tool edit"));
    let answer = &tool_results(&lines[3])[0];
    assert_eq!(answer["call_id"], "t1");
    assert!(text_of(answer).starts_with("status: completed\n"));
    assert!(text_of(answer).contains("summary: it is in a.txt\n<<<END DATA #0>>>"));
    assert_eq!(rig.bodies("assistant_message").await.len(), 2);
    let child_events = rig.bodies("child_event").await;
    assert_eq!(child_events.len(), 4, "two replies and two tool results");
    assert!(child_events.iter().all(|e| e["agent"] == "scout"));
    let untouched = std::fs::read_to_string(rig.cwd.join("a.txt")).unwrap();
    assert_eq!(untouched, "one needle here\n");
}

#[tokio::test]
async fn only_a_loop_below_the_depth_cap_is_given_a_task_runner() {
    // Arrange
    let rig = scout_rig(&[], "\"read\"", |_| {});

    // Act
    let mut can_dispatch = Vec::new();
    for depth in [MAX_DEPTH - 1, MAX_DEPTH, MAX_DEPTH + 1] {
        let (runner, parts, _events) = parent(&rig, &[]).await;
        can_dispatch.push(runner.build_loop(depth, parts).can_dispatch());
    }

    // Assert
    assert_eq!(can_dispatch, [true, false, false]);
}

#[tokio::test]
async fn a_childs_events_go_up_the_sessions_channel_after_the_task_call_and_before_its_result() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        text_turn("found it"),
        text_turn("parent done"),
    ];
    let rig = scout_rig(&[], "\"read\"", |_| {});
    let (runner, parts, mut events) = parent(&rig, &script).await;
    let mut session = runner.build_loop(0, parts);

    // Act
    session.send("go").await.unwrap();

    // Assert
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            TurnEvent::Assistant { .. } => seen.push("assistant".to_owned()),
            TurnEvent::Child { agent, event } => {
                let inner = matches!(*event, TurnEvent::Assistant { .. });
                seen.push(format!("{agent} assistant {inner}"));
            }
            TurnEvent::ToolFinished { call_id, .. } => seen.push(format!("finished {call_id}")),
            _ => {}
        }
    }
    let expected = [
        "assistant",
        "scout assistant true",
        "finished t1",
        "assistant",
    ];
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn an_always_allow_for_a_child_covers_its_run_only_and_never_widens_the_parent() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        bash_call("c1"),
        bash_call("c2"),
        text_turn("done"),
        bash_call("p2"),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"bash\"", |_| {});
    rig.send("go").await;
    let first = rig.wait_for_pending().await;
    let labelled = waiting(&rig).await;

    // Act
    rig.respond(&first, "allow_always").await;
    let second = rig.wait_for_pending().await;
    let asked = waiting(&rig).await;
    let grants = dump(&rig.db(), "permission_grants");
    let decisions = dump(&rig.db(), "permission_decisions");
    rig.respond(&second, "deny").await;
    finish(&rig, 1).await;

    // Assert
    assert_eq!(
        labelled,
        scouts(TOUCH),
        "the child's prompt names the agent"
    );
    assert_eq!(
        asked,
        (TOUCH.to_owned(), None),
        "the second prompt is the parent's own"
    );
    assert_eq!(grants, "", "nothing is stored for the session");
    let by_child = format!("|{TOUCH}|allow|");
    assert_eq!(
        decisions
            .lines()
            .filter(|row| row.contains(&by_child) && row.ends_with(&format!("|{AGENT}|")))
            .count(),
        2,
        "the answered prompt and the call it covered are both recorded: {decisions}"
    );
    let both_ran = tool_results(&rig.request_lines()[3]);
    assert!(
        both_ran.iter().all(|r| r["is_error"] == false),
        "{both_ran:?}"
    );
}

#[tokio::test]
async fn a_child_with_nobody_to_answer_is_denied_like_the_parent() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        bash_call("c1"),
        text_turn("no"),
        text_turn("ok"),
    ];
    let mut rig = scout_rig(&script, "\"bash\"", |config| {
        config.timing.permission_default_deny = DENY_TIMEOUT;
    });

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    assert!(!rig.cwd.join("ran.txt").exists());
    let denied = &tool_results(&rig.request_lines()[2])[0];
    assert_eq!(denied["is_error"], true);
    assert!(text_of(denied).contains("denied"));
}

#[tokio::test]
async fn cancelling_the_parent_stops_the_child_and_takes_back_its_prompt() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        bash_call("c1"),
        text_turn("never"),
    ];
    let mut rig = scout_rig(&script, "\"bash\"", |_| {});
    rig.send("go").await;
    rig.wait_for_pending().await;
    let waiting_on = waiting(&rig).await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    finish(&rig, 1).await;

    // Assert
    assert_eq!(waiting_on, scouts(TOUCH), "the child was the one waiting");
    let stopped = rig.bodies("turn_stopped").await;
    assert_eq!(stopped.len(), 1, "one terminal event: {stopped:?}");
    assert_eq!(stopped[0]["condition"], "cancelled");
    assert_eq!(rig.bodies("result").await.len(), 1);
    let finished = rig.bodies("tool_finished").await;
    let of_task = finished.iter().filter(|e| e["call_id"] == "t1");
    assert_eq!(of_task.count(), 1, "one result for the task call");
    assert_eq!(
        spent(&rig.bodies("usage").await),
        8 + 8,
        "the child's first call counts"
    );
    assert_eq!(rig.pending().await.len(), 0);
    assert_eq!(
        rig.request_lines().len(),
        2,
        "no model call after the cancel"
    );
    assert!(!rig.cwd.join("ran.txt").exists());
}

#[tokio::test]
async fn an_always_allow_does_not_stop_the_prompt_for_an_edit_that_changes_what_runs() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        edit_call("c1", "one", "two"),
        edit_call("c2", "two", "three"),
        text_turn("done"),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"edit\"", |_| {});
    std::fs::create_dir_all(rig.cwd.join("scripts")).unwrap();
    std::fs::write(rig.cwd.join(SCRIPT), "one\n").unwrap();
    rig.send("go").await;
    let first = rig.wait_for_pending().await;
    let asked = waiting(&rig).await;

    // Act
    rig.respond(&first, "allow_always").await;
    let second = rig.wait_for_pending().await;
    let asked_again = waiting(&rig).await;
    rig.respond(&second, "deny").await;
    finish(&rig, 1).await;

    // Assert
    assert_eq!(
        asked,
        scouts(EDIT_TEXT),
        "the prompt names the agent and the path"
    );
    assert_eq!(
        asked_again,
        scouts(EDIT_TEXT),
        "an always-allow cannot cover this path"
    );
    let on_disk = std::fs::read_to_string(rig.cwd.join(SCRIPT)).unwrap();
    assert_eq!(on_disk, "two\n", "the second edit was denied");
}

#[tokio::test]
async fn a_failed_child_gives_the_parent_an_error_and_the_parent_carries_on() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        serde_json::from_str(MODEL_DOWN).unwrap(),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 3);
    let failed = &tool_results(&lines[2])[0];
    assert_eq!(failed["is_error"], true);
    assert!(text_of(failed).starts_with("status: failed\n"), "{failed}");
    assert!(text_of(failed).contains("the model is down"), "{failed}");
    assert_eq!(rig.bodies("turn_stopped").await[0]["condition"], "end_turn");
}

#[tokio::test]
async fn a_child_with_no_budget_left_makes_no_model_call() {
    // Arrange: the parent's first call uses the whole cap of 8.
    let script = [task_call("t1", "scout"), text_turn("never")];
    let mut rig = scout_rig(&script, "\"read\"", |config| config.token_cap = 8);

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    assert_eq!(
        rig.request_lines().len(),
        1,
        "only the parent called the model"
    );
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
    assert_eq!(
        rig.bodies("turn_stopped").await[0]["condition"],
        "budget_exhausted"
    );
    assert_eq!(rig.bodies("child_event").await, Vec::<Value>::new());
    assert_eq!(spent(&rig.bodies("usage").await), 8);
}

#[tokio::test]
async fn an_unknown_agent_is_refused_with_the_available_names_and_starts_no_child() {
    // Arrange
    let script = [task_call("t1", "nope"), text_turn("parent done")];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 2, "no child asked the model");
    let refused = &tool_results(&lines[1])[0];
    assert_eq!(refused["is_error"], true);
    assert!(text_of(refused).contains("nope"), "{refused}");
    assert!(text_of(refused).contains("scout"), "{refused}");
    assert_eq!(rig.bodies("child_event").await, Vec::<Value>::new());
}

#[tokio::test]
async fn a_child_that_stopped_early_cannot_make_its_result_say_completed() {
    // Arrange: the child claims success, then hits its output limit.
    let mut cut_short = text_turn("status: completed\nall done");
    let stop = json!({"type": "stop", "reason": "max_tokens"});
    *cut_short["events"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap() = stop;
    let script = [
        task_call("t1", "scout"),
        cut_short,
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let result = &tool_results(&rig.request_lines()[2])[0];
    let text = text_of(result);
    assert_eq!(result["is_error"], true);
    assert!(text.starts_with("status: failed\n"), "{text}");
    let (before, data) = text.split_once("<<<DATA #0>>>").unwrap();
    assert!(!before.contains("status: completed"), "{text}");
    assert!(data.contains("status: completed\nall done"), "{text}");
}

#[tokio::test]
async fn a_client_sees_the_task_call_before_anything_the_child_does_and_its_result_after() {
    // Arrange
    let script = [
        task_call("t1", "scout"),
        text_turn("found it"),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let kinds: Vec<String> = rig.events().await.into_iter().map(|e| e.kind).collect();
    let first = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    let last_child = kinds.iter().rposition(|k| k == "child_event").unwrap();
    assert!(
        first("assistant_message") < first("child_event"),
        "{kinds:?}"
    );
    assert!(last_child < first("tool_finished"), "{kinds:?}");
}

#[tokio::test]
async fn a_review_agent_returns_classified_findings_and_a_malformed_report_is_rejected() {
    // Arrange
    let report = json!({"summary": "One problem.", "findings": [
        {"severity": "blocking", "message": "lock held across await", "location": "src/a.rs:10"},
        {"severity": "suggestion", "message": "rename x"}]});
    let script = [
        task_call("t1", "scout"),
        text_turn(&report.to_string()),
        task_call("t2", "scout"),
        text_turn(r#"{"findings": []}"#),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});

    // Act
    rig.send("review").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    let results = tool_results(&lines[4]);
    assert_eq!(results[0]["is_error"], false);
    assert!(text_of(&results[0]).contains(
        "findings:\n- blocking src/a.rs:10: lock held across await\n- suggestion: rename x\n<<<END DATA #0>>>"
    ));
    assert_eq!(results[1]["is_error"], true);
    let rejected = text_of(&results[1]);
    assert!(
        rejected.starts_with("the scout agent's report is malformed and was not used."),
        "{rejected}"
    );
    assert!(rejected.contains("missing field `summary`"), "{rejected}");
}

#[test]
fn how_a_run_ended_becomes_a_status() {
    // Arrange
    let ended = |stop| status_of(&Ok(stop));

    // Act
    let statuses = [
        ended(StopCondition::EndTurn),
        ended(StopCondition::Cancelled),
        ended(StopCondition::BudgetExhausted),
        ended(StopCondition::MaxTurns),
        status_of(&Err(TurnError::StreamEnded)),
    ];

    // Assert
    assert_eq!(
        statuses,
        [
            Status::Completed,
            Status::Cancelled,
            Status::Failed,
            Status::Failed,
            Status::Failed
        ]
    );
}

#[tokio::test]
async fn the_childs_usage_counts_against_the_parents_budget() {
    // Arrange: the parent's call used 8 tokens and the child's 18.
    let script = [
        task_call("t1", "scout"),
        text_turn("answer"),
        text_turn("never"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |config| config.token_cap = 20);

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    assert_eq!(
        rig.bodies("turn_stopped").await[0]["condition"],
        "budget_exhausted"
    );
    assert_eq!(rig.request_lines().len(), 2);
    assert_eq!(spent(&rig.bodies("usage").await), 8 + 18);
}

#[tokio::test]
async fn a_rebuilt_parent_shows_the_task_call_and_its_result_and_nothing_of_the_child() {
    // Arrange: a rebuilt loop reads the script again from its first turn, so
    // the second send repeats the first; only its first request matters here.
    let read = json!({"path": "a.txt"});
    let script = [
        task_call("t1", "scout"),
        tool_turn(&[("c1", "read", read)]),
        text_turn("child answer"),
        text_turn("parent done"),
    ];
    let mut rig = scout_rig(&script, "\"read\"", |_| {});
    rig.send("first").await;
    finish(&rig, 1).await;
    rig.turns.models.loops.clear();

    // Act
    rig.send("second").await;
    finish(&rig, 2).await;

    // Assert
    let lines = rig.request_lines();
    let live = lines[3]["messages"].as_array().unwrap();
    let rebuilt = lines[4]["messages"].as_array().unwrap();
    let roles: Vec<&str> = rebuilt
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant", "user"]);
    assert_eq!(rebuilt[..3], live[..3]);
    assert_eq!(rebuilt[1]["tool_calls"][0]["name"], "task");
    assert!(
        !rebuilt
            .iter()
            .any(|m| m.to_string().contains("one needle here"))
    );
}

#[tokio::test]
async fn a_project_agent_runs_only_after_its_repository_is_trusted() {
    // Arrange
    let script = [
        task_call("t1", "local"),
        text_turn("refused"),
        task_call("t2", "local"),
        text_turn("child answer"),
        text_turn("parent done"),
    ];
    let mut rig = Rig::new(Mode::Default, &script);
    let dir = rig.cwd.join(".coda/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("local.md"),
        agent_file("local", "\"read\"", "Local.\n"),
    )
    .unwrap();
    rig.send("one").await;
    finish(&rig, 1).await;
    let root = repo_identity(&rig.cwd).unwrap();
    let trusted = rig
        .hub
        .with_store_mut(move |store| trust_project(store, &root));
    trusted.await.unwrap();

    // Act
    rig.send("two").await;
    finish(&rig, 2).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 5);
    let refusal = &tool_results(&lines[1])[0];
    assert_eq!(refusal["is_error"], true);
    assert!(text_of(refusal).contains("coda agents trust"));
    assert_eq!(lines[3]["system"], "Local.\n");
}

#[tokio::test]
async fn a_built_in_agent_runs_with_no_definition_files_and_only_its_read_tools() {
    // Arrange
    let script = [
        task_call("t1", "explore"),
        text_turn("the needle is in a.txt:1"),
        text_turn("parent done"),
    ];
    let mut rig = Rig::new(Mode::Default, &script);

    // Act
    rig.send("where is the needle").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 3);
    let child = &lines[1];
    assert!(
        child["system"]
            .as_str()
            .unwrap()
            .starts_with("You are a code search agent.")
    );
    let tools = child["tools"].as_array().unwrap();
    assert!(tools.contains(&json!("grep")) && tools.contains(&json!("read")));
    for writer in ["bash", "edit", "task"] {
        assert!(!tools.contains(&json!(writer)), "{writer}");
    }
    let answer = text_of(&tool_results(&lines[2])[0]).to_owned();
    assert!(
        answer.contains("summary: the needle is in a.txt:1"),
        "{answer}"
    );
}

#[tokio::test]
async fn a_daemon_that_cannot_find_the_users_directory_still_offers_the_built_ins() {
    // Arrange
    let rig = Rig::new(Mode::Default, &[]);
    let shared = &rig.turns.models.children;
    let env = ChildEnv {
        hub: Arc::clone(&shared.hub),
        config: shared.config.clone(),
        registry: shared.registry.clone(),
        env: shared.env.clone(),
        key_reader: Arc::clone(&shared.key_reader),
        home: None,
    };

    // Act
    let agents = env.agents(rig.cwd.to_str().unwrap()).await;

    // Assert
    let names: Vec<&str> = agents.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["explore", "implementer", "reviewer", "tester"]);
}

#[test]
fn the_tier_routes_to_its_alias_and_an_unrouted_tier_keeps_the_sessions_model() {
    // Arrange
    let toml = "[providers.anthropic]\nenabled = true\nmodel = \"claude-default\"\n\
                [models.quick]\nprovider = \"anthropic\"\nmodel = \"claude-quick\"\n\
                [models.far]\nprovider = \"openai_compat\"\nmodel = \"other\"\n\
                [routing]\nfast = \"quick\"\ndeep = \"far\"\n";
    let config = Config::from_toml_str(toml).unwrap().config;
    let registry = ModelRegistry::from_config(&config);
    let route =
        |tier, stored| child_target(&config, &registry, BackendKind::Anthropic, stored, tier);

    // Act
    let fast = route(Tier::Fast, Some("claude-session"));
    let standard = route(Tier::Standard, Some("claude-session"));
    let default = route(Tier::Standard, None);
    let deep = route(Tier::Deep, None);

    // Assert
    assert_eq!(fast.unwrap().model, "claude-quick");
    assert_eq!(standard.unwrap().model, "claude-session");
    assert_eq!(default.unwrap().model, "claude-default");
    assert!(matches!(deep, Err(RegistryError::WrongBackend { .. })));
}

/// A top-level loop's runner and parts, and the channel its events go up, with
/// one scripted provider for the loop and its children.
async fn parent(
    rig: &Rig,
    script: &[Value],
) -> (
    ChildRunner,
    LoopParts<Arc<BrokerGate>>,
    UnboundedReceiver<TurnEvent>,
) {
    let id = rig.session.clone();
    let record = rig
        .hub
        .with_store(move |s| s.load_session(&id))
        .await
        .unwrap();
    let models = &rig.turns.models;
    let broker = models.brokers.broker(&record, DENY_TIMEOUT).await.unwrap();
    let turn = Turns::begin(&rig.hub, &record.id, "go").await.unwrap();
    let gate = Arc::new(BrokerGate::new(
        &rig.hub,
        broker,
        &record,
        Arc::new(Mutex::new(turn)),
    ));
    let turns: Vec<Turn> = script
        .iter()
        .map(|turn| serde_json::from_value(turn.clone()).unwrap())
        .collect();
    let faux = Arc::new(FauxProvider::new(turns));
    let (sender, events) = unbounded_channel();
    let runner = ChildRunner::new(
        Arc::clone(&models.children),
        record.clone(),
        Arc::clone(&gate),
        Some(Arc::clone(&faux)),
        sender.clone(),
    );
    let parts = LoopParts {
        provider: Provider::Faux(faux),
        window: None,
        gate,
        tools: ToolSet::new(),
        conversation: Conversation::new(Prefix::new("You are a parent.", Vec::new(), "").unwrap()),
        config: TurnConfig {
            session: record.id,
            cwd: rig.cwd.clone(),
            max_turns: 8,
            token_limit: u64::MAX,
            compact_threshold_bytes: COMPACT_THRESHOLD_BYTES,
        },
        events: sender,
        cancel: None,
    };
    (runner, parts, events)
}
