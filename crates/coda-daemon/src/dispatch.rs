// SPDX-License-Identifier: AGPL-3.0-only

//! Routes JSON-RPC requests to handlers by method name.
//!
//! Handlers are async so they can await I/O. Anything blocking, such as the
//! synchronous SQLite store, must run under `spawn_blocking` inside the handler
//! so it never stalls the runtime threads.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use coda_protocol::jsonrpc::{
    ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Message,
    PARSE_ERROR, Request, Response,
};
use coda_protocol::methods::{
    DAEMON_PING, INITIALIZE, InitializeParams, InitializeResult, PROTOCOL_VERSION, PingResult,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// `initialize` was called with a protocol version the daemon does not speak.
pub const PROTOCOL_VERSION_MISMATCH: i64 = -32001;

type HandlerResult = Result<Value, ErrorObject>;
type HandlerFuture = Pin<Box<dyn Future<Output = HandlerResult> + Send>>;
type Handler = fn(Option<Value>) -> HandlerFuture;

/// Method table that turns raw frame payloads into responses.
pub struct Dispatcher {
    handlers: HashMap<&'static str, Handler>,
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
        Self { handlers }
    }

    /// Answers one frame payload; returns `None` for messages that expect no reply.
    pub async fn handle(&self, payload: &[u8]) -> Option<Response> {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => return Some(failure(Id::Null, PARSE_ERROR, "Parse error")),
        };
        match serde_json::from_value::<Message>(value.clone()) {
            Ok(Message::Request(request)) => Some(self.route(request).await),
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

    async fn route(&self, request: Request) -> Response {
        let Some(handler) = self.handlers.get(request.method.as_str()) else {
            return failure(request.id, METHOD_NOT_FOUND, "Method not found");
        };
        match handler(request.params).await {
            Ok(result) => Response::success(request.id, result),
            Err(error) => Response::failure(request.id, error),
        }
    }
}

fn failure(id: Id, code: i64, message: &str) -> Response {
    Response::failure(id, error(code, message))
}

fn error(code: i64, message: &str) -> ErrorObject {
    ErrorObject {
        code,
        message: message.to_owned(),
        data: None,
    }
}

fn parse_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| error(INVALID_PARAMS, "Invalid params"))
}

fn to_value<T: serde::Serialize>(result: &T) -> HandlerResult {
    serde_json::to_value(result).map_err(|_| error(INTERNAL_ERROR, "Internal error"))
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
    use serde_json::{Value, json};

    async fn call(payload: &str) -> Option<Response> {
        Dispatcher::new().handle(payload.as_bytes()).await
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
