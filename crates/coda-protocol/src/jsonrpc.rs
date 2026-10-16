// SPDX-License-Identifier: AGPL-3.0-only

//! JSON-RPC 2.0 message types.

use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// The payload is not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist or is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The method parameters are invalid.
pub const INVALID_PARAMS: i64 = -32602;
/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i64 = -32603;

const VERSION_STR: &str = "2.0";

/// The `jsonrpc` member: always `"2.0"`, anything else fails to deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(VERSION_STR)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw == VERSION_STR {
            Ok(Self)
        } else {
            Err(de::Error::custom(format!(
                "unsupported jsonrpc version `{raw}`, expected `{VERSION_STR}`"
            )))
        }
    }
}

/// A request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric identifier.
    Number(i64),
    /// String identifier.
    String(String),
    /// Null identifier, used when the request id could not be determined.
    Null,
}

/// A call that expects a [`Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version marker.
    pub jsonrpc: Version,
    /// Correlates the response with this request.
    pub id: Id,
    /// Name of the method to invoke.
    pub method: String,
    /// Method parameters, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A call that expects no response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Protocol version marker.
    pub jsonrpc: Version,
    /// Name of the method to invoke.
    pub method: String,
    /// Method parameters, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// The error member of a failed [`Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Error code, see the named constants in this module.
    pub code: i64,
    /// Short description of the error.
    pub message: String,
    /// Additional error information, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The outcome carried by a [`Response`]: exactly one of result or error.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The call succeeded.
    Result(Value),
    /// The call failed.
    Error(ErrorObject),
}

/// The reply to a [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// Protocol version marker.
    pub jsonrpc: Version,
    /// Identifier of the request being answered.
    pub id: Id,
    /// Success value or error.
    pub outcome: Outcome,
}

impl Response {
    /// Builds a successful response.
    #[must_use]
    pub fn success(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: Outcome::Result(result),
        }
    }

    /// Builds an error response.
    #[must_use]
    pub fn failure(id: Id, error: ErrorObject) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: Outcome::Error(error),
        }
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("jsonrpc", &self.jsonrpc)?;
        map.serialize_entry("id", &self.id)?;
        match &self.outcome {
            Outcome::Result(result) => map.serialize_entry("result", result)?,
            Outcome::Error(error) => map.serialize_entry("error", error)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        let jsonrpc = take(&mut object, "jsonrpc")?;
        let id = take(&mut object, "id")?;
        let outcome = match (object.remove("result"), object.remove("error")) {
            (Some(result), None) => Outcome::Result(result),
            (None, Some(error)) => Outcome::Error(from_value(error)?),
            _ => {
                return Err(de::Error::custom(
                    "a response must contain exactly one of `result` or `error`",
                ));
            }
        };
        Ok(Self {
            jsonrpc,
            id,
            outcome,
        })
    }
}

fn from_value<T: DeserializeOwned, E: de::Error>(value: Value) -> Result<T, E> {
    serde_json::from_value(value).map_err(E::custom)
}

fn take<T: DeserializeOwned, E: de::Error>(
    object: &mut Map<String, Value>,
    key: &'static str,
) -> Result<T, E> {
    from_value(object.remove(key).ok_or_else(|| E::missing_field(key))?)
}

/// Any JSON-RPC message, classified by shape.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Message {
    /// A call expecting a response.
    Request(Request),
    /// A call expecting no response.
    Notification(Notification),
    /// A reply to a request.
    Response(Response),
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Request(m) => m.serialize(serializer),
            Self::Notification(m) => m.serialize(serializer),
            Self::Response(m) => m.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Value::Object(object) = &value else {
            return Err(de::Error::custom("a message must be a JSON object"));
        };
        if !object.contains_key("method") {
            from_value(value).map(Self::Response)
        } else if object.contains_key("id") {
            from_value(value).map(Self::Request)
        } else {
            from_value(value).map(Self::Notification)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn to_json<T: serde::Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn request_round_trips_and_matches_golden() {
        // Arrange
        let golden = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"a":1}}"#;
        let request = Request {
            jsonrpc: Version,
            id: Id::Number(1),
            method: "ping".into(),
            params: Some(json!({"a": 1})),
        };

        // Act
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: Message = serde_json::from_str(golden).unwrap();

        // Assert
        assert_eq!(encoded, golden);
        assert_eq!(decoded, Message::Request(request));
    }

    #[test]
    fn notification_round_trips_and_matches_golden() {
        // Arrange
        let golden = r#"{"jsonrpc":"2.0","method":"note"}"#;
        let notification = Notification {
            jsonrpc: Version,
            method: "note".into(),
            params: None,
        };

        // Act
        let encoded = serde_json::to_string(&notification).unwrap();
        let decoded: Message = serde_json::from_str(golden).unwrap();

        // Assert
        assert_eq!(encoded, golden);
        assert_eq!(decoded, Message::Notification(notification));
    }

    #[test]
    fn success_response_round_trips_and_matches_golden() {
        // Arrange
        let golden = r#"{"jsonrpc":"2.0","id":"abc","result":{"ok":true}}"#;
        let response = Response::success(Id::String("abc".into()), json!({"ok": true}));

        // Act
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: Message = serde_json::from_str(golden).unwrap();

        // Assert
        assert_eq!(encoded, golden);
        assert_eq!(decoded, Message::Response(response));
    }

    #[test]
    fn error_response_round_trips_and_matches_golden() {
        // Arrange
        let golden = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"Method not found","data":"x"}}"#;
        let response = Response::failure(
            Id::Null,
            ErrorObject {
                code: METHOD_NOT_FOUND,
                message: "Method not found".into(),
                data: Some(json!("x")),
            },
        );

        // Act
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: Message = serde_json::from_str(golden).unwrap();

        // Assert
        assert_eq!(encoded, golden);
        assert_eq!(decoded, Message::Response(response));
    }

    #[test]
    fn version_serializes_as_2_0() {
        // Act
        let value = to_json(&Version);

        // Assert
        assert_eq!(value, json!("2.0"));
    }

    #[test]
    fn wrong_version_is_rejected() {
        // Arrange
        let raw = r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#;

        // Act
        let result = serde_json::from_str::<Message>(raw);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn response_with_both_result_and_error_is_rejected() {
        // Arrange
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"m"}}"#;

        // Act
        let result = serde_json::from_str::<Message>(raw);

        // Assert
        assert!(result.is_err());
    }
}
