// SPDX-License-Identifier: AGPL-3.0-only

//! `session.set_model`: alias switches on idle and running sessions.

use std::sync::Arc;

use coda_config::config::{Config, ModelAlias};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, Store};
use serde_json::{Value, json};
use tempfile::TempDir;

use coda_providers::registry::RegistryError;

use super::{CONVERSATION_RESET_KIND, EnvReader, QUEUE, Rig, text_turn, tool_turn};
use crate::gate::Brokers;
use crate::model_session::{
    Limits, MODEL_SWITCHED_KIND, ModelError, ModelSessions, Provider, SwitchError, lock,
};
use crate::registry::{MODEL_UNAVAILABLE, UNKNOWN_MODEL};
use crate::subscribe::Hub;
use crate::turn::{TURN_RUNNING, TurnError};

const SECRET: &str = "sk-live-SECRET-VALUE-91c";
const API_CONFIG: &str = r#"
[providers.anthropic]
enabled = true
api_key_env = "MY_ANTHROPIC_KEY"

[providers.openai_compat]
enabled = true
api_key_env = "OTHER_SECRET_VAR"
base_url = "http://localhost:11434/v1"

[models.fast]
provider = "anthropic"
model = "claude-fast"
"#;

fn faux_alias(config: &mut Config) {
    config.models.insert(
        "scripted".to_owned(),
        ModelAlias {
            provider: "faux".to_owned(),
            model: "script-1".to_owned(),
        },
    );
}

async fn stored_model(hub: &Arc<Hub>, session: &SessionId) -> Option<String> {
    let id = session.clone();
    let record = hub.with_store(move |s| s.load_session(&id)).await.unwrap();
    record.model
}

fn set_model(session: &SessionId, model: &str) -> Value {
    json!({"session_id": session, "model": model})
}

fn error_code(response: &Value) -> i64 {
    response["error"]["code"].as_i64().unwrap()
}

#[tokio::test]
async fn an_idle_switch_stores_the_alias_drops_the_conversation_and_records_the_event() {
    // Arrange
    let script = [text_turn("one"), text_turn("two")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, faux_alias);
    rig.send("first").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;
    let kept_before = lock(&rig.turns.models.loops).len();

    // Act
    let params = set_model(&rig.session, "scripted");
    let response = rig.call("session.set_model", params).await;
    let kept_after = lock(&rig.turns.models.loops).len();
    rig.send("second").await;
    rig.wait_for_results(2).await;

    // Assert
    assert_eq!(
        response["result"]["session"]["model"], "script-1",
        "{response}"
    );
    assert_eq!((kept_before, kept_after), (1, 0));
    assert_eq!(
        rig.bodies(MODEL_SWITCHED_KIND).await,
        [json!({
            "alias": "scripted",
            "provider": "faux",
            "model": "script-1",
            "conversation": "reset",
        })]
    );
    assert_eq!(rig.bodies(CONVERSATION_RESET_KIND).await.len(), 1);
}

#[tokio::test]
async fn an_unknown_alias_is_rejected_and_changes_nothing() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("one")]);
    rig.send("first").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;

    // Act
    let params = set_model(&rig.session, "nope");
    let response = rig.call("session.set_model", params).await;

    // Assert
    assert_eq!(error_code(&response), UNKNOWN_MODEL, "{response}");
    assert_eq!(stored_model(&rig.hub, &rig.session).await, None);
    assert_eq!(lock(&rig.turns.models.loops).len(), 1);
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("unknown model alias `nope`"), "{message}");
    assert_eq!(rig.bodies(MODEL_SWITCHED_KIND).await.len(), 0);
}

#[tokio::test]
async fn a_switch_during_a_running_turn_is_rejected_and_the_turn_goes_on() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "true"}))]);
    let mut rig = Rig::custom(
        Mode::Default,
        &[calls, text_turn("done")],
        true,
        None,
        faux_alias,
    );
    rig.send("go").await;
    let request = rig.wait_for_pending().await;

    // Act
    let params = set_model(&rig.session, "scripted");
    let response = rig.call("session.set_model", params).await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for_results(1).await;

    // Assert
    assert_eq!(error_code(&response), TURN_RUNNING, "{response}");
    assert_eq!(rig.bodies(MODEL_SWITCHED_KIND).await.len(), 0);
}

#[tokio::test]
async fn a_switch_changes_only_the_target_session() {
    // Arrange
    let mut rig = Rig::custom(Mode::Default, &[], true, None, faux_alias);
    let other = rig.other_session().await;

    // Act
    let params = set_model(&rig.session, "scripted");
    rig.call("session.set_model", params).await;

    // Assert
    let id = other.clone();
    let stored = rig
        .hub
        .with_store(move |s| s.load_session(&id))
        .await
        .unwrap();
    let events = rig
        .hub
        .with_store(move |s| s.load_events(&other, 0, 100))
        .await
        .unwrap();
    assert_eq!(stored.model, None);
    assert_eq!(events.len(), 0);
}

struct ApiRig {
    _dir: TempDir,
    hub: Arc<Hub>,
    sessions: Arc<ModelSessions>,
    session: SessionId,
}

fn api_rig(reader: EnvReader) -> ApiRig {
    api_rig_with(API_CONFIG, BackendKind::Anthropic, reader)
}

fn api_rig_with(config: &str, backend: BackendKind, reader: EnvReader) -> ApiRig {
    let dir = tempfile::tempdir().unwrap();
    let mut store = coda_store::Store::open(&dir.path().join("store")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend,
            model: None,
            cwd: dir.path().to_string_lossy().into_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    let hub = Hub::new(store, QUEUE);
    let config = Config::from_toml_str(config).unwrap().config;
    let brokers = Brokers::new(Arc::clone(&hub), Vec::new());
    let sessions = ModelSessions::new(Arc::clone(&hub), brokers, config, Vec::new(), reader);
    ApiRig {
        _dir: dir,
        hub,
        sessions,
        session,
    }
}

#[tokio::test]
async fn a_missing_key_variable_is_a_clear_error_that_names_no_other_variable() {
    // Arrange
    let rig = api_rig(Arc::new(|_| None));

    // Act
    let error = rig
        .sessions
        .set_model(&rig.session, "fast")
        .await
        .unwrap_err();

    // Assert
    let message = error.to_string();
    assert!(matches!(error, SwitchError::Registry(_)));
    assert!(message.contains("MY_ANTHROPIC_KEY"), "{message}");
    assert!(!message.contains("OTHER_SECRET_VAR"), "{message}");
    let id = rig.session.clone();
    let stored = rig
        .hub
        .with_store(move |s| s.load_session(&id))
        .await
        .unwrap();
    assert_eq!(stored.model, None);
}

#[tokio::test]
async fn a_cancel_during_a_model_switch_reports_nothing_cancelled() {
    // Arrange
    let target = Arc::new(std::sync::OnceLock::<(Arc<ModelSessions>, SessionId)>::new());
    let cancelled = Arc::new(std::sync::Mutex::new(None));
    let reader: EnvReader = {
        let (target, cancelled) = (Arc::clone(&target), Arc::clone(&cancelled));
        // The key is read while the switch holds the session.
        Arc::new(move |_| {
            let (sessions, session) = target.get().unwrap();
            *lock(&cancelled) = Some(sessions.cancel(session));
            Some(SECRET.to_owned())
        })
    };
    let rig = api_rig(reader);
    assert!(
        target
            .set((Arc::clone(&rig.sessions), rig.session.clone()))
            .is_ok()
    );

    // Act
    rig.sessions.set_model(&rig.session, "fast").await.unwrap();

    // Assert
    assert_eq!(*lock(&cancelled), Some(false));
}

#[tokio::test]
async fn the_key_value_never_reaches_events_or_the_switched_session() {
    // Arrange
    let rig = api_rig(Arc::new(|_| Some(SECRET.to_owned())));

    // Act
    let record = rig.sessions.set_model(&rig.session, "fast").await.unwrap();

    // Assert
    let id = rig.session.clone();
    let events = rig
        .hub
        .with_store(move |s| s.load_events(&id, 0, 100))
        .await
        .unwrap();
    let everything = format!("{record:?}{events:?}");
    assert_eq!(record.model.as_deref(), Some("claude-fast"));
    assert!(!everything.contains(SECRET), "{everything}");
    assert!(!everything.contains("MY_ANTHROPIC_KEY"), "{everything}");
}

#[tokio::test]
async fn a_model_switch_leaves_no_turn_open() {
    // Arrange
    let rig = api_rig(Arc::new(|_| Some(SECRET.to_owned())));

    // Act
    rig.sessions.set_model(&rig.session, "fast").await.unwrap();

    // Assert
    let open = rig.hub.with_store(Store::unfinished_turns).await.unwrap();
    assert_eq!(open, vec![]);
}

#[test]
fn an_unavailable_provider_has_its_own_error_code() {
    // Arrange
    let error = SwitchError::Registry(coda_providers::registry::RegistryError::Disabled(
        "anthropic",
    ));

    // Act
    let rpc = crate::registry::into_rpc(error);

    // Assert
    assert_eq!(rpc.code, MODEL_UNAVAILABLE);
}

#[tokio::test]
async fn the_send_limits_survive_a_model_switch() {
    // Arrange
    let script = [
        tool_turn(&[("c1", "no_such_tool", json!({}))]),
        text_turn("x"),
    ];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, faux_alias);
    let params = json!({"session_id": rig.session, "text": "first", "max_turns": 1});
    rig.call("session.send", params).await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;

    // Act
    let params = set_model(&rig.session, "scripted");
    rig.call("session.set_model", params).await;
    rig.send("second").await;
    rig.wait_for_results(2).await;

    // Assert
    let stops = rig.bodies("turn_stopped").await;
    let conditions: Vec<_> = stops.iter().map(|b| b["condition"].clone()).collect();
    assert_eq!(conditions, ["max_turns", "max_turns"]);
}

#[tokio::test]
async fn a_send_that_began_before_a_switch_builds_on_the_switched_model() {
    // Arrange: the model "panic" makes the faux provider fail the task, which
    // shows which model the loop was built on.
    let script = [text_turn("one")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.models.insert(
            "boom".to_owned(),
            ModelAlias {
                provider: "faux".to_owned(),
                model: "panic".to_owned(),
            },
        );
    });
    let id = rig.session.clone();
    let stale = rig.hub.with_store(move |s| s.load_session(&id)).await;
    let params = set_model(&rig.session, "boom");
    rig.call("session.set_model", params).await;

    // Act
    let sent = rig
        .turns
        .models
        .send(stale.unwrap(), "go".to_owned(), Limits::default())
        .await;
    rig.wait_for_results(1).await;

    // Assert
    sent.unwrap();
    let results = rig.bodies("result").await;
    assert_eq!(results[0]["is_error"], true, "{results:?}");
}

const REASSIGNED_CONFIG: &str = r#"
[providers.anthropic]
enabled = true
api_key_env = "MY_ANTHROPIC_KEY"

[providers.openai_compat]
enabled = true
api_key_env = "OTHER_SECRET_VAR"
base_url = "http://localhost:11434/v1"

[models.fast]
provider = "openai_compat"
model = "local-model"

[models.claude-fast]
provider = "openai_compat"
model = "local-model"
"#;

#[tokio::test]
async fn a_stored_model_stays_a_model_id_when_the_aliases_change_after_a_restart() {
    // Arrange
    let configs = [
        ("[providers.anthropic]\nenabled = true\napi_key_env = \"K\"\n"),
        REASSIGNED_CONFIG,
    ];
    for restarted in configs {
        let rig = api_rig(Arc::new(|_| Some(SECRET.to_owned())));
        let record = rig.sessions.set_model(&rig.session, "fast").await.unwrap();
        let config = Config::from_toml_str(restarted).unwrap().config;
        let brokers = Brokers::new(Arc::clone(&rig.hub), Vec::new());
        let reader: EnvReader = Arc::new(|_| Some(SECRET.to_owned()));
        let after = ModelSessions::new(Arc::clone(&rig.hub), brokers, config, Vec::new(), reader);

        // Act
        let provider = after.provider(&record);

        // Assert
        assert!(matches!(provider, Ok(Provider::Api(_))));
    }
}

fn registry_failures() -> Vec<(RegistryError, i64)> {
    vec![
        (RegistryError::NoModel("openai_compat"), MODEL_UNAVAILABLE),
        (RegistryError::NoBaseUrl("openai_compat"), MODEL_UNAVAILABLE),
        (
            RegistryError::WrongBackend {
                alias: "fast".to_owned(),
                provider: "openai_compat",
            },
            UNKNOWN_MODEL,
        ),
        (
            RegistryError::UnknownAlias("nope".to_owned()),
            UNKNOWN_MODEL,
        ),
    ]
}

#[test]
fn send_time_registry_errors_keep_their_clear_text_and_a_model_code() {
    for (failure, code) in registry_failures() {
        // Arrange
        let text = failure.to_string();

        // Act
        let rpc = TurnError::Model(ModelError::Registry(failure)).into_rpc();

        // Assert
        assert_eq!((rpc.code, rpc.message), (code, text));
    }
}

#[tokio::test]
async fn a_send_without_a_key_variable_or_a_model_reports_why() {
    // Arrange
    let anthropic = api_rig(Arc::new(|_| None));
    let compat = api_rig_with(API_CONFIG, BackendKind::OpenaiCompat, Arc::new(|_| None));

    for (rig, expected) in [(anthropic, "MY_ANTHROPIC_KEY"), (compat, "no model chosen")] {
        // Act
        let id = rig.session.clone();
        let record = rig.hub.with_store(move |s| s.load_session(&id)).await;
        let failed = rig
            .sessions
            .send(record.unwrap(), "hi".to_owned(), Limits::default())
            .await
            .unwrap_err();
        let rpc = failed.into_rpc();

        // Assert
        assert_eq!(rpc.code, MODEL_UNAVAILABLE, "{}", rpc.message);
        assert!(rpc.message.contains(expected), "{}", rpc.message);
    }
}
