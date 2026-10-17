// SPDX-License-Identifier: AGPL-3.0-only

//! Routes JSON-RPC requests to handlers by method name.
//!
//! Handlers are async so they can await I/O. Anything blocking, such as the
//! synchronous SQLite store, must run under `spawn_blocking` inside the handler
//! so it never stalls the runtime threads.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use coda_protocol::codec::encode_message;
use coda_protocol::jsonrpc::{
    ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Message,
    PARSE_ERROR, Request, Response,
};
use coda_protocol::methods::{
    DAEMON_PING, DAEMON_SHUTDOWN, DAEMON_STATUS, DaemonShutdownResult, GRANT_ADD, INITIALIZE,
    InitializeParams, InitializeResult, PERMISSION_RESPOND, PROTOCOL_VERSION, PingResult,
    SESSION_CANCEL, SESSION_CREATE, SESSION_LIST, SESSION_RESUME, SESSION_SEND, SESSION_SUBSCRIBE,
    SessionSubscribeParams, SessionSubscribeResult, USAGE_SUMMARY,
};
use coda_protocol::types::SessionId;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::{Instrument, error};

use crate::grants_rpc;
use crate::session_rpc;
use crate::state::{ClientGuard, DaemonState};
use crate::subscribe::{Connection, Hub, HubError, Outbox};
use crate::trace::{request_span, session_span};
use crate::turn::{self, Turns};
use crate::usage_rpc;

/// `initialize` was called with a protocol version the daemon does not speak.
pub const PROTOCOL_VERSION_MISMATCH: i64 = -32001;

pub(crate) type HandlerResult = Result<Value, ErrorObject>;
type HandlerFuture = Pin<Box<dyn Future<Output = HandlerResult> + Send>>;
type Handler = fn(Option<Value>) -> HandlerFuture;

/// Method table that turns raw frame payloads into responses.
pub struct Dispatcher {
    handlers: HashMap<&'static str, Handler>,
    hub: Option<Arc<Hub>>,
    state: Option<Arc<DaemonState>>,
    turns: Option<Arc<Turns>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    /// Builds the table of methods the daemon answers.
    #[must_use]
    pub fn new() -> Self {
        let handlers = HashMap::from([
            (DAEMON_PING, ping as Handler),
            (INITIALIZE, initialize as Handler),
        ]);
        Self {
            handlers,
            hub: None,
            state: None,
            turns: None,
        }
    }

    /// Also answers `daemon.status` and `daemon.shutdown` from `state`.
    #[must_use]
    pub fn with_state(mut self, state: Arc<DaemonState>) -> Self {
        self.state = Some(state);
        self
    }

    /// Counts one open connection while the returned guard lives, if state is attached.
    #[must_use]
    pub fn track_client(&self) -> Option<ClientGuard> {
        self.state.as_ref().map(DaemonState::track_client)
    }

    /// Also answers `session.create`, `session.resume`, `session.subscribe`,
    /// `grant.add` and `usage.summary` from `hub`.
    #[must_use]
    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// Also answers `session.send`, `session.cancel` and `permission.respond` from `turns`.
    #[must_use]
    pub fn with_turns(mut self, turns: Arc<Turns>) -> Self {
        self.turns = Some(turns);
        self
    }

    /// Opens the per-connection state that subscriptions need.
    #[must_use]
    pub fn connect(&self) -> (Connection, Outbox) {
        match &self.hub {
            Some(hub) => hub.connect(),
            None => Connection::detached(),
        }
    }

    /// Answers one frame payload; returns `None` for messages that expect no reply.
    pub async fn handle(&self, payload: &[u8], connection: &Connection) -> Option<Response> {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => return Some(failure(Id::Null, PARSE_ERROR, "Parse error")),
        };
        match serde_json::from_value::<Message>(value.clone()) {
            Ok(Message::Request(request)) => self.route(request, connection).await,
            Ok(_) => None,
            Err(_) => {
                let id = value
                    .get("id")
                    .and_then(|id| serde_json::from_value(id.clone()).ok())
                    .unwrap_or(Id::Null);
                Some(failure(id, INVALID_REQUEST, "Invalid request"))
            }
        }
    }

    async fn route(&self, request: Request, connection: &Connection) -> Option<Response> {
        let span = request_span(&request.method);
        self.dispatch(request, connection).instrument(span).await
    }

    async fn dispatch(&self, request: Request, connection: &Connection) -> Option<Response> {
        if request.method == SESSION_SUBSCRIBE {
            return subscribe(request, connection).await;
        }
        let Request {
            id, method, params, ..
        } = request;
        let outcome = match method.as_str() {
            GRANT_ADD => grants_rpc::add_grant(self.hub.as_ref(), params).await,
            USAGE_SUMMARY => usage_rpc::summarize(self.hub.as_ref(), params).await,
            SESSION_CREATE => session_rpc::create(self.hub.as_ref(), params).await,
            SESSION_RESUME => session_rpc::resume(self.hub.as_ref(), params).await,
            SESSION_LIST => {
                let turns = self.turns.as_ref();
                let running = |id: &SessionId| turns.is_some_and(|t| t.is_running(id));
                session_rpc::list(self.hub.as_ref(), running, params).await
            }
            SESSION_SEND => turn::send(self.turns.as_ref(), params).await,
            SESSION_CANCEL => turn::cancel(self.turns.as_ref(), params),
            PERMISSION_RESPOND => turn::respond(self.turns.as_ref(), params).await,
            DAEMON_STATUS => self.status(),
            DAEMON_SHUTDOWN => self.shutdown(),
            other => match self.handlers.get(other) {
                Some(handler) => handler(params).await,
                None => return Some(failure(id, METHOD_NOT_FOUND, "Method not found")),
            },
        };
        Some(match outcome {
            Ok(result) => Response::success(id, result),
            Err(e) => Response::failure(id, e),
        })
    }
}

impl Dispatcher {
    fn status(&self) -> HandlerResult {
        let state = self.state.as_ref().ok_or_else(not_found)?;
        let active = self.turns.as_ref().map_or(0, |turns| turns.active());
        to_value(&state.status(active))
    }

    fn shutdown(&self) -> HandlerResult {
        let state = self.state.as_ref().ok_or_else(not_found)?;
        state.request_shutdown();
        to_value(&DaemonShutdownResult {})
    }
}

fn not_found() -> ErrorObject {
    error(METHOD_NOT_FOUND, "Method not found")
}

fn failure(id: Id, code: i64, message: &str) -> Response {
    Response::failure(id, error(code, message))
}

pub(crate) fn error(code: i64, message: &str) -> ErrorObject {
    ErrorObject {
        code,
        message: message.to_owned(),
        data: None,
    }
}

pub(crate) fn parse_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| error(INVALID_PARAMS, "Invalid params"))
}

pub(crate) fn to_value<T: serde::Serialize>(result: &T) -> HandlerResult {
    serde_json::to_value(result).map_err(|_| error(INTERNAL_ERROR, "Internal error"))
}

/// Answers through the connection's event queue so the response precedes the
/// replayed events; returns `None` once the subscription took the reply over.
async fn subscribe(request: Request, connection: &Connection) -> Option<Response> {
    let params: SessionSubscribeParams = match parse_params(request.params) {
        Ok(params) => params,
        Err(e) => return Some(Response::failure(request.id, e)),
    };
    let ack = to_value(&SessionSubscribeResult { subscribed: true })
        .map(|result| Response::success(request.id.clone(), result))
        .ok()
        .and_then(|response| encode_message(&Message::Response(response)).ok());
    let Some(ack) = ack else {
        return Some(failure(request.id, INTERNAL_ERROR, "Internal error"));
    };
    let subscribed = connection
        .subscribe(&params.session_id, params.since_seq, Some(ack))
        .instrument(session_span(&params.session_id))
        .await;
    match subscribed {
        Ok(()) => None,
        Err(HubError::InvalidCursor) => Some(failure(request.id, INVALID_PARAMS, "Invalid cursor")),
        Err(HubError::Store(coda_store::StoreError::SessionNotFound { .. })) => {
            Some(failure(request.id, INVALID_PARAMS, "Unknown session"))
        }
        Err(HubError::Unavailable) => {
            Some(failure(request.id, METHOD_NOT_FOUND, "Method not found"))
        }
        Err(e) => {
            error!(error = %e, "session.subscribe failed");
            Some(failure(request.id, INTERNAL_ERROR, "Internal error"))
        }
    }
}

fn ping(_params: Option<Value>) -> HandlerFuture {
    Box::pin(async { to_value(&PingResult {}) })
}

fn initialize(params: Option<Value>) -> HandlerFuture {
    Box::pin(async move {
        let params: InitializeParams = parse_params(params)?;
        if params.protocol_version != PROTOCOL_VERSION {
            return Err(error(
                PROTOCOL_VERSION_MISMATCH,
                &format!(
                    "Protocol version {} is not supported, the daemon speaks {PROTOCOL_VERSION}",
                    params.protocol_version
                ),
            ));
        }
        to_value(&InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::jsonrpc::{
        ErrorObject, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Outcome, PARSE_ERROR,
        Response,
    };
    use coda_protocol::types::SessionId;
    use serde_json::{Value, json};

    async fn call(payload: &str) -> Option<Response> {
        let (connection, _outbox) = Connection::detached();
        Dispatcher::new()
            .handle(payload.as_bytes(), &connection)
            .await
    }

    fn error_of(response: Response) -> (Id, ErrorObject) {
        match response.outcome {
            Outcome::Error(error) => (response.id, error),
            Outcome::Result(value) => panic!("expected an error, got result {value}"),
        }
    }

    fn result_of(response: Response) -> Value {
        match response.outcome {
            Outcome::Result(value) => value,
            Outcome::Error(error) => panic!("expected a result, got error {error:?}"),
        }
    }

    #[tokio::test]
    async fn ping_answers_with_an_empty_result() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"daemon.ping"}"#;

        // Act
        let response = call(request).await.unwrap();

        // Assert
        assert_eq!(response.id, Id::Number(1));
        assert_eq!(result_of(response), json!({}));
    }

    #[tokio::test]
    async fn initialize_with_matching_version_reports_daemon_version() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocol_version":1,"client_name":"t"}}"#;

        // Act
        let response = call(request).await.unwrap();

        // Assert
        let result = result_of(response);
        assert_eq!(result["protocol_version"], json!(1));
        assert!(result["daemon_version"].is_string());
    }

    #[tokio::test]
    async fn initialize_with_other_version_gets_the_mismatch_code() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocol_version":99,"client_name":"t"}}"#;

        // Act
        let (id, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(id, Id::Number(2));
        assert_eq!(error.code, PROTOCOL_VERSION_MISMATCH);
    }

    #[tokio::test]
    async fn initialize_with_bad_params_gets_invalid_params() {
        // Arrange
        let request =
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocol_version":"x"}}"#;

        // Act
        let (_, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(error.code, INVALID_PARAMS);
    }

    fn dispatcher_with_state() -> (Dispatcher, Arc<crate::state::DaemonState>) {
        let state = crate::state::DaemonState::new(std::path::PathBuf::from("/data"));
        (Dispatcher::new().with_state(Arc::clone(&state)), state)
    }

    async fn call_on(dispatcher: &Dispatcher, payload: &str) -> Response {
        let (connection, _outbox) = Connection::detached();
        dispatcher
            .handle(payload.as_bytes(), &connection)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn status_answers_with_daemon_facts() {
        // Arrange
        let (dispatcher, _state) = dispatcher_with_state();

        // Act
        let response = call_on(
            &dispatcher,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.status"}"#,
        )
        .await;

        // Assert
        let result = result_of(response);
        assert_eq!(result["pid"], json!(std::process::id()));
        assert_eq!(result["protocol_version"], json!(1));
        assert_eq!(result["data_dir"], json!("/data"));
    }

    #[tokio::test]
    async fn status_without_state_is_method_not_found() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"daemon.status"}"#;

        // Act
        let (_, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn shutdown_answers_then_flags_the_state() {
        // Arrange
        let (dispatcher, state) = dispatcher_with_state();

        // Act
        let response = call_on(
            &dispatcher,
            r#"{"jsonrpc":"2.0","id":1,"method":"daemon.shutdown"}"#,
        )
        .await;

        // Assert
        assert_eq!(result_of(response), json!({}));
        let woke = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.shutdown_requested(),
        )
        .await;
        assert!(woke.is_ok());
    }

    #[tokio::test]
    async fn grant_add_is_registered_when_a_hub_is_present() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let store = coda_store::Store::open(&dir.path().join("data")).unwrap();
        let hub = Hub::new(store, crate::subscribe::DEFAULT_QUEUE_CAPACITY);
        let dispatcher = Dispatcher::new().with_hub(hub);
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"grant.add","params":{"repo_path":"/r","tool":"bash","cwd":"/r","pattern":"ls"}}"#;

        // Act
        let response = call_on(&dispatcher, request).await;

        // Assert
        assert!(result_of(response)["granted_at"].is_i64());
    }

    #[tokio::test]
    async fn grant_add_without_a_hub_is_method_not_found() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"grant.add","params":{"repo_path":"/r","tool":"bash","cwd":"/r","pattern":"ls"}}"#;

        // Act
        let (_, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    /// A hub over a store holding one session with two usage records.
    fn hub_with_usage(dir: &tempfile::TempDir) -> (Arc<Hub>, SessionId) {
        use coda_protocol::types::{BackendKind, Mode};
        use coda_store::{NewSession, Store, UsageRecord};

        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd: "/r".to_owned(),
                mode: Mode::Default,
            })
            .unwrap();
        let turn = store.begin_turn(&session).unwrap();
        for (input, output) in [(100, 20), (7, 3)] {
            store
                .record_usage(&UsageRecord {
                    session: session.clone(),
                    turn: turn.clone(),
                    input_tokens: input,
                    output_tokens: output,
                })
                .unwrap();
        }
        (
            Hub::new(store, crate::subscribe::DEFAULT_QUEUE_CAPACITY),
            session,
        )
    }

    #[tokio::test]
    async fn usage_summary_sums_one_session_from_the_store() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (hub, session) = hub_with_usage(&dir);
        let dispatcher = Dispatcher::new().with_hub(hub);
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"usage.summary","params":{{"session_id":"{}"}}}}"#,
            session.as_str()
        );

        // Act
        let response = call_on(&dispatcher, &request).await;

        // Assert
        let result = result_of(response);
        assert_eq!(result["usage"]["input_tokens"], json!(107));
        assert_eq!(result["usage"]["output_tokens"], json!(23));
        assert_eq!(result["saved_bytes"], json!(0));
    }

    #[tokio::test]
    async fn usage_summary_without_a_session_sums_every_session() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (hub, _session) = hub_with_usage(&dir);
        let dispatcher = Dispatcher::new().with_hub(hub);
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"usage.summary","params":{}}"#;

        // Act
        let response = call_on(&dispatcher, request).await;

        // Assert
        assert_eq!(result_of(response)["usage"]["input_tokens"], json!(107));
    }

    #[tokio::test]
    async fn usage_summary_for_an_unknown_session_is_invalid_params() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (hub, _session) = hub_with_usage(&dir);
        let dispatcher = Dispatcher::new().with_hub(hub);
        let request =
            r#"{"jsonrpc":"2.0","id":1,"method":"usage.summary","params":{"session_id":"999"}}"#;

        // Act
        let (_, error) = error_of(call_on(&dispatcher, request).await);

        // Assert
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn usage_summary_without_a_hub_is_method_not_found() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"usage.summary","params":{}}"#;

        // Act
        let (_, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    fn dispatcher_with_empty_store(dir: &tempfile::TempDir) -> Dispatcher {
        let store = coda_store::Store::open(&dir.path().join("data")).unwrap();
        Dispatcher::new().with_hub(Hub::new(store, crate::subscribe::DEFAULT_QUEUE_CAPACITY))
    }

    const CREATE_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"backend":"claude_code","cwd":"/","model":"m-1","mode":"plan"}}"#;

    #[tokio::test]
    async fn session_create_stores_the_session_and_returns_it() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let dispatcher = dispatcher_with_empty_store(&dir);

        // Act
        let response = call_on(&dispatcher, CREATE_REQUEST).await;

        // Assert
        let session = &result_of(response)["session"];
        assert_eq!(session["backend"], json!("claude_code"));
        assert_eq!(session["cwd"], json!("/"));
        assert_eq!(session["model"], json!("m-1"));
        assert_eq!(session["mode"], json!("plan"));
        assert!(session["id"].is_string());
    }

    #[tokio::test]
    async fn session_resume_returns_the_stored_session() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let dispatcher = dispatcher_with_empty_store(&dir);
        let created = result_of(call_on(&dispatcher, CREATE_REQUEST).await);
        let request = json!({"jsonrpc": "2.0", "id": 2, "method": "session.resume",
            "params": {"session_id": created["session"]["id"]}});

        // Act
        let response = call_on(&dispatcher, &request.to_string()).await;

        // Assert
        assert_eq!(result_of(response), created);
    }

    #[tokio::test]
    async fn session_resume_of_an_unknown_session_is_session_not_found() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let dispatcher = dispatcher_with_empty_store(&dir);
        let request =
            r#"{"jsonrpc":"2.0","id":1,"method":"session.resume","params":{"session_id":"999"}}"#;

        // Act
        let (_, error) = error_of(call_on(&dispatcher, request).await);

        // Assert
        assert_eq!(error.code, crate::turn::SESSION_NOT_FOUND);
    }

    #[tokio::test]
    async fn session_create_without_a_hub_is_method_not_found() {
        // Act
        let (_, error) = error_of(call(CREATE_REQUEST).await.unwrap());

        // Assert
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn every_request_runs_inside_a_request_span() {
        // Arrange
        let (spans, _guard) = crate::trace::capture::install();

        // Act
        call(r#"{"jsonrpc":"2.0","id":1,"method":"daemon.ping"}"#).await;

        // Assert
        let recorded = spans.named(crate::trace::REQUEST_SPAN);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].fields["rpc.method"], "daemon.ping");
    }

    #[tokio::test]
    async fn a_session_request_also_opens_a_session_span() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (hub, session) = hub_with_usage(&dir);
        let dispatcher = Dispatcher::new().with_hub(hub);
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"usage.summary","params":{{"session_id":"{}"}}}}"#,
            session.as_str()
        );
        let (spans, _guard) = crate::trace::capture::install();

        // Act
        call_on(&dispatcher, &request).await;

        // Assert
        let recorded = spans.named(crate::trace::SESSION_SPAN);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].fields["coda.session.id"], session.as_str());
    }

    #[tokio::test]
    async fn malformed_json_gets_parse_error_with_null_id() {
        // Act
        let (id, error) = error_of(call("{not json").await.unwrap());

        // Assert
        assert_eq!(id, Id::Null);
        assert_eq!(error.code, PARSE_ERROR);
    }

    #[tokio::test]
    async fn empty_payload_gets_parse_error() {
        // Act
        let (_, error) = error_of(call("").await.unwrap());

        // Assert
        assert_eq!(error.code, PARSE_ERROR);
    }

    #[tokio::test]
    async fn valid_json_that_is_not_a_message_gets_invalid_request() {
        // Act
        let (id, error) = error_of(
            call(r#"{"jsonrpc":"1.0","id":4,"method":"x"}"#)
                .await
                .unwrap(),
        );

        // Assert
        assert_eq!(id, Id::Number(4));
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[tokio::test]
    async fn unknown_method_gets_method_not_found() {
        // Arrange
        let request = r#"{"jsonrpc":"2.0","id":5,"method":"nope"}"#;

        // Act
        let (id, error) = error_of(call(request).await.unwrap());

        // Assert
        assert_eq!(id, Id::Number(5));
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notification_gets_no_response() {
        // Arrange
        let notification = r#"{"jsonrpc":"2.0","method":"daemon.ping"}"#;

        // Act
        let response = call(notification).await;

        // Assert
        assert_eq!(response, None);
    }
}
