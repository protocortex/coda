// SPDX-License-Identifier: AGPL-3.0-only

//! How a session reads the definitions it offers: off the store, once per
//! send, quietly, and with the tokens it spent kept across a rebuild.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use coda_agents::{Catalog, StoredTrust, trust_project};
use coda_protocol::paths::repo_identity;
use coda_protocol::types::{Mode, SessionId};
use serde_json::{Value, json};
use tracing::Level;

use super::skills::{schema_of, skill_file, write};
use super::task::{agent_file, finish, task_call, text_of, tool_results};
use super::{MAX_IDLE_LOOPS, Rig, StoreProbe, TOKEN_CAP, read_turn, text_turn};
use crate::model_session::{Carried, Dropped, Limits, MAX_CARRIED, lock};
use crate::trace::capture::{EventRecord, Spans, install};

/// Skill files of about this size, the most a definition file may be.
const BULK_BYTES: usize = 60 * 1024;
const BULK_FILES: usize = 400;
const LINE: &str = "a line of instructions\n";
/// How long a thread is given to run into a lock held for it.
const HOLD: Duration = Duration::from_millis(100);

/// `count` large skill files under `root/dir`.
fn write_bulk_skills(root: &Path, dir: &str, count: usize) {
    let body = LINE.repeat(BULK_BYTES / LINE.len());
    for n in 0..count {
        let name = format!("bulk{n}");
        write(
            root,
            &format!("{dir}/{name}.md"),
            &skill_file(&name, "Bulk.", &body),
        );
    }
}

/// The time one read of the definitions takes on this machine.
async fn time_a_load(rig: &Rig) -> Duration {
    let root = repo_identity(&rig.cwd);
    let trust_root = root.clone();
    let trust = rig
        .hub
        .with_store(move |store| StoredTrust::read(store, trust_root.as_deref()))
        .await
        .unwrap();
    let started = Instant::now();
    let catalog = Catalog::load_with_trust(&rig.home(), root.as_deref(), &trust);
    let took = started.elapsed();
    assert!(catalog.problems().is_empty(), "{:?}", catalog.problems());
    took
}

#[tokio::test]
async fn a_store_user_is_not_blocked_while_the_definitions_are_read() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("hi")]);
    write_bulk_skills(&rig.home(), "skills", BULK_FILES);
    write_bulk_skills(&rig.cwd, ".coda/skills", BULK_FILES);
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| trust_project(store, &repo))
        .await
        .unwrap();
    let load = time_a_load(&rig).await;
    assert!(
        load > Duration::from_millis(100),
        "{load:?} is too short to measure"
    );
    let user = StoreProbe::start(&rig);

    // Act
    let started = Instant::now();
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let probed = user.stop().await;

    // Assert
    // A store call that finishes in the middle of the read was served while
    // the files were being read. Were the store held for the read, the first
    // one would finish only at its end. One call in a window of most of the
    // read is enough, so a pause of the runner cannot fail the test.
    let (from, to) = (started + load / 5, started + load * 4 / 5);
    let served = probed.finished.iter().any(|at| (from..=to).contains(at));
    assert!(
        served,
        "no store call finished while the definitions were read; the longest waited {:?} of a {load:?} read",
        probed.longest
    );
}

/// The `problems` counts of the "files were skipped" warnings logged so far.
fn skipped_warnings(logs: &Spans) -> Vec<String> {
    logs.events_at(Level::WARN)
        .into_iter()
        .filter(|event| event.fields["message"].contains("definition files were skipped"))
        .map(|event| event.fields["problems"].clone())
        .collect()
}

#[tokio::test]
async fn the_skipped_files_warning_is_logged_once_per_build_and_again_when_the_count_changes() {
    // Arrange
    let (logs, _guard) = install();
    let script = ["a", "b", "c", "d"].map(text_turn);
    let mut rig = Rig::new(Mode::Default, &script);
    write(&rig.home(), "skills/broken.md", "not a definition\n");

    // Act
    for text in ["one", "two", "three"] {
        rig.send(text).await;
        rig.wait_until_idle().await;
    }
    let after_three = skipped_warnings(&logs);
    write(&rig.home(), "skills/also-broken.md", "not one either\n");
    rig.send("four").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(after_three, ["1"], "three sends, one build");
    assert_eq!(skipped_warnings(&logs), ["1", "2"]);
    assert_eq!(rig.request_lines().len(), 4, "the loop was kept");
}

#[tokio::test]
async fn a_store_error_during_a_send_keeps_the_idle_conversation_as_it_is() {
    // Arrange
    let (logs, _guard) = install();
    let script = [text_turn("a"), text_turn("b")];
    let mut rig = Rig::new(Mode::Default, &script);
    let agent = agent_file("local", "\"read\"", "Local.\n");
    write(&rig.cwd, ".coda/agents/local.md", &agent);
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| trust_project(store, &repo))
        .await
        .unwrap();
    rig.send("first").await;
    rig.wait_until_idle().await;
    rig.db().execute("DROP TABLE project_trust", []).unwrap();

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    // The built-in agents alone differ from what the idle conversation offers,
    // but a failed read says nothing about the files, so it must not rebuild it.
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 2, "the kept loop went on with the script");
    let listed = schema_of(&lines[1], "task")["description"].to_string();
    assert!(listed.contains("- local: "), "{listed}");
    let warned = logs.events_at(Level::WARN);
    let unread = |event: &EventRecord| event.fields["message"].contains("could not be read");
    assert!(warned.iter().any(unread), "{warned:?}");
}

#[tokio::test]
async fn a_loop_rebuilt_after_a_restart_keeps_the_tokens_the_session_spent() {
    // Arrange
    // A read turn and a reply leave 4 of the 30 tokens, and a rebuilt loop
    // starts the script again with a read turn.
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.token_cap = TOKEN_CAP;
    });
    rig.send("first").await;
    rig.wait_until_idle().await;
    // What a daemon restart leaves: no loop in memory, the usage in the store.
    rig.turns.models.loops.clear();

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let stops = rig.bodies("turn_stopped").await;
    let conditions: Vec<_> = stops.iter().map(|stop| &stop["condition"]).collect();
    assert_eq!(conditions, ["end_turn", "budget_exhausted"]);
    assert_eq!(
        rig.request_lines().len(),
        3,
        "the second send makes one call"
    );
}

/// Sends to the rig's session under a token cap of 30 and spends 26 of it, then
/// fills the idle pool with other sessions until its conversation is evicted.
async fn evicted_under_a_cap(rig: &mut Rig) -> SessionId {
    let first = rig.session.clone();
    let params = json!({"session_id": first, "text": "first", "token_cap": TOKEN_CAP});
    rig.call("session.send", params).await;
    rig.wait_idle(&first).await;
    for _ in 0..MAX_IDLE_LOOPS {
        let other = rig.other_session().await;
        rig.send_as(&other, "filler").await;
        rig.wait_idle(&other).await;
    }
    let evicted = !rig.turns.models.loops.holds(&first);
    assert!(evicted, "the first conversation should be out of the cache");
    first
}

/// How the turns of the rig's session stopped, oldest first.
async fn conditions(rig: &Rig) -> Vec<Value> {
    let stops = rig.bodies("turn_stopped").await;
    stops
        .into_iter()
        .map(|stop| stop["condition"].clone())
        .collect()
}

#[tokio::test]
async fn a_conversation_evicted_from_the_idle_cache_keeps_its_limits_and_its_spent_tokens() {
    // Arrange
    // The next send names no cap, so only what the dropped loop carried can stop it.
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::new(Mode::Default, &script);
    let first = evicted_under_a_cap(&mut rig).await;

    // Act
    rig.send_as(&first, "second").await;
    rig.wait_idle(&first).await;

    // Assert
    assert_eq!(conditions(&rig).await, ["end_turn", "budget_exhausted"]);
}

#[tokio::test]
async fn closing_a_session_forgets_the_limits_kept_for_its_evicted_conversation() {
    // Arrange
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::new(Mode::Default, &script);
    let first = evicted_under_a_cap(&mut rig).await;

    // Act
    rig.call("session.close", json!({"session_id": first}))
        .await;
    rig.send_as(&first, "second").await;
    rig.wait_idle(&first).await;

    // Assert
    // The cap of the first send went with the close: the second send runs under
    // the configured one and the 26 tokens already stored are far below it.
    assert_eq!(conditions(&rig).await, ["end_turn", "end_turn"]);
}

#[tokio::test]
async fn an_evicted_conversation_is_never_out_of_the_pool_without_its_state_kept() {
    // Arrange
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::new(Mode::Default, &script);
    let first = rig.session.clone();
    rig.send_as(&first, "first").await;
    rig.wait_idle(&first).await;
    // A pool that is full, with a spare conversation in hand.
    let mut others = Vec::new();
    for _ in 1..MAX_IDLE_LOOPS {
        let other = rig.other_session().await;
        rig.send_as(&other, "filler").await;
        rig.wait_idle(&other).await;
        others.push(other);
    }
    let models = Arc::clone(&rig.turns.models);
    let spare_id = others.pop().unwrap();
    let spare = models.take_loop(&spare_id).unwrap();
    let last = rig.other_session().await;
    rig.send_as(&last, "filler").await;
    rig.wait_idle(&last).await;
    assert!(models.loops.holds(&first), "the pool is full and holds it");

    // Act
    // Another thread keeps the spare conversation, which pushes the oldest one
    // out, while this one holds the table of kept state.
    let held = lock(&models.carried);
    let keeping = {
        let models = Arc::clone(&models);
        std::thread::spawn(move || models.keep_loop(spare_id, spare))
    };
    std::thread::sleep(HOLD);
    let still_pooled = models.loops.holds(&first);
    drop(held);
    keeping.join().unwrap();

    // Assert
    assert!(
        still_pooled,
        "the conversation left the pool before its state was kept"
    );
    assert!(!models.loops.holds(&first));
    assert!(lock(&models.carried).get(&first).is_some());
}

fn spent(used: u64) -> Carried {
    Carried {
        limits: Limits::default(),
        used,
    }
}

fn numbered(n: usize) -> SessionId {
    SessionId::new(format!("s{n}"))
}

#[test]
fn the_dropped_state_is_bounded_and_forgets_the_oldest_first() {
    // Arrange
    let mut dropped = Dropped::default();

    // Act
    for n in 0..=MAX_CARRIED {
        dropped.insert(numbered(n), spent(n as u64));
    }

    // Assert
    assert_eq!(dropped.len(), MAX_CARRIED);
    assert!(dropped.get(&numbered(0)).is_none());
    assert_eq!(dropped.get(&numbered(1)).unwrap().used, 1);
}

#[test]
fn a_session_dropped_again_becomes_the_newest_entry_with_its_latest_state() {
    // Arrange
    let mut dropped = Dropped::default();
    for n in 0..MAX_CARRIED {
        dropped.insert(numbered(n), spent(n as u64));
    }

    // Act
    dropped.insert(numbered(0), spent(99));
    dropped.insert(numbered(MAX_CARRIED), spent(7));

    // Assert
    assert_eq!(dropped.len(), MAX_CARRIED);
    assert_eq!(dropped.get(&numbered(0)).unwrap().used, 99);
    assert!(dropped.get(&numbered(1)).is_none(), "the oldest went");
}

#[tokio::test]
async fn a_broken_override_of_a_built_in_agent_refuses_the_task_and_starts_no_child() {
    // Arrange
    let script = [task_call("t1", "explore"), text_turn("parent done")];
    let mut rig = Rig::new(Mode::Default, &script);
    write(&rig.home(), "agents/explore.md", "not a definition\n");

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 2, "no child asked the model");
    let refusal = &tool_results(&lines[1])[0];
    assert_eq!(refusal["is_error"], true);
    let text = text_of(refusal);
    assert!(text.contains("explore.md"), "{text}");
    assert!(text.contains("coda agents lint"), "{text}");
    assert_eq!(rig.bodies("child_event").await, Vec::<Value>::new());
}

#[tokio::test]
async fn a_held_back_project_file_that_shadows_the_agent_that_ran_is_noted_in_the_result() {
    // Arrange
    let script = [
        task_call("t1", "explore"),
        text_turn("found it"),
        text_turn("parent done"),
    ];
    let mut rig = Rig::new(Mode::Default, &script);
    let project = agent_file("explore", "\"read\"", "Project explore.\n");
    write(&rig.cwd, ".coda/agents/explore.md", &project);

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert!(
        lines[1]["system"]
            .as_str()
            .unwrap()
            .starts_with("You are a code search agent."),
        "the built-in agent ran"
    );
    let answer = &tool_results(&lines[2])[0];
    assert_eq!(answer["is_error"], false);
    let text = text_of(answer);
    assert!(text.starts_with("status: completed\n"), "{text}");
    let note = "note: the project agent `explore` is not in use: the repository is not trusted; \
                the user must review it and run `coda agents trust`. The built-in agent ran instead.";
    assert!(text.ends_with(note), "{text}");
}

#[tokio::test]
async fn a_store_that_cannot_give_the_trust_decision_leaves_only_the_built_in_agents() {
    // Arrange
    let (logs, _guard) = install();
    let script = [task_call("t1", "scout"), text_turn("parent done")];
    let mut rig = Rig::new(Mode::Default, &script);
    let scout = agent_file("scout", "\"read\"", "You are a scout.\n");
    write(&rig.home(), "agents/scout.md", &scout);
    let pdf = skill_file("pdf", "Fills in PDF forms.", "Fill every field.");
    write(&rig.home(), "skills/pdf.md", &pdf);
    rig.db().execute("DROP TABLE project_trust", []).unwrap();

    // Act
    rig.send("go").await;
    finish(&rig, 1).await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 2, "no child asked the model");
    let listed = schema_of(&lines[0], "task")["description"].to_string();
    assert!(listed.contains("- explore: "), "{listed}");
    assert!(!listed.contains("- scout: "), "{listed}");
    assert!(!lines[0]["tools"].to_string().contains("skill"));
    let refusal = &tool_results(&lines[1])[0];
    assert_eq!(refusal["is_error"], true);
    assert!(
        text_of(refusal).contains("no agent named `scout`"),
        "{refusal}"
    );
    let warned = logs.events_at(Level::WARN);
    assert!(
        warned
            .iter()
            .any(|event| event.fields["message"].contains("the definitions could not be read")),
        "{warned:?}"
    );
}
