// SPDX-License-Identifier: AGPL-3.0-only

//! JSON Schema of the wire protocol, generated from the Rust types.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{Map, Value, json};

use crate::events::Event;
use crate::jsonrpc::{ErrorObject, Id, Notification, Request};
use crate::methods::{self as m, PROTOCOL_VERSION};
use crate::types::OriginalId;

const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
const SCHEMA_TITLE: &str = "coda protocol";

/// Adds the params and result schemas of one method to the methods map.
fn add_method<P: JsonSchema, R: JsonSchema>(
    generator: &mut SchemaGenerator,
    methods: &mut Map<String, Value>,
    name: &str,
) {
    debug_assert!(m::METHODS.contains(&name), "{name} is missing from METHODS");
    let params = generator.subschema_for::<P>().to_value();
    let result = generator.subschema_for::<R>().to_value();
    methods.insert(name.to_owned(), json!({"params": params, "result": result}));
}

/// Builds the schema document: `$defs` for every public wire type, a
/// `methods` map keyed by method name with its params and result, and the
/// `event` schema.
#[must_use]
pub fn protocol_schema() -> Value {
    let mut generator = SchemaGenerator::new(SchemaSettings::draft2020_12());
    let mut methods = Map::new();
    add_method::<m::InitializeParams, m::InitializeResult>(
        &mut generator,
        &mut methods,
        m::INITIALIZE,
    );
    add_method::<m::SessionCreateParams, m::SessionCreateResult>(
        &mut generator,
        &mut methods,
        m::SESSION_CREATE,
    );
    add_method::<m::SessionSendParams, m::SessionSendResult>(
        &mut generator,
        &mut methods,
        m::SESSION_SEND,
    );
    add_method::<m::SessionCancelParams, m::SessionCancelResult>(
        &mut generator,
        &mut methods,
        m::SESSION_CANCEL,
    );
    add_method::<m::SessionSubscribeParams, m::SessionSubscribeResult>(
        &mut generator,
        &mut methods,
        m::SESSION_SUBSCRIBE,
    );
    add_method::<m::SessionResumeParams, m::SessionResumeResult>(
        &mut generator,
        &mut methods,
        m::SESSION_RESUME,
    );
    add_method::<m::SessionListParams, m::SessionListResult>(
        &mut generator,
        &mut methods,
        m::SESSION_LIST,
    );
    add_method::<m::SessionCloseParams, m::SessionCloseResult>(
        &mut generator,
        &mut methods,
        m::SESSION_CLOSE,
    );
    add_method::<m::PermissionRespondParams, m::PermissionRespondResult>(
        &mut generator,
        &mut methods,
        m::PERMISSION_RESPOND,
    );
    add_method::<m::GrantAddParams, m::GrantAddResult>(&mut generator, &mut methods, m::GRANT_ADD);
    add_method::<m::UsageSummaryParams, m::UsageSummaryResult>(
        &mut generator,
        &mut methods,
        m::USAGE_SUMMARY,
    );
    add_method::<m::SessionSetModelParams, m::SessionSetModelResult>(
        &mut generator,
        &mut methods,
        m::SESSION_SET_MODEL,
    );
    add_method::<m::DaemonBackendsParams, m::DaemonBackendsResult>(
        &mut generator,
        &mut methods,
        m::DAEMON_BACKENDS,
    );
    add_method::<m::DaemonStatusParams, m::DaemonStatusResult>(
        &mut generator,
        &mut methods,
        m::DAEMON_STATUS,
    );
    add_method::<m::DaemonShutdownParams, m::DaemonShutdownResult>(
        &mut generator,
        &mut methods,
        m::DAEMON_SHUTDOWN,
    );
    add_method::<m::PingParams, m::PingResult>(&mut generator, &mut methods, m::DAEMON_PING);

    let event = generator.subschema_for::<Event>().to_value();
    // Types no method or event reaches still belong to the contract.
    generator.subschema_for::<Request>();
    generator.subschema_for::<Notification>();
    generator.subschema_for::<ErrorObject>();
    generator.subschema_for::<Id>();
    generator.subschema_for::<OriginalId>();

    json!({
        "$schema": SCHEMA_DIALECT,
        "title": SCHEMA_TITLE,
        "x-protocol-version": PROTOCOL_VERSION,
        "$defs": generator.take_definitions(true),
        "methods": methods,
        "event": event,
    })
}

/// Top-level keys whose entries each get their own line.
const EXPANDED_KEYS: [&str; 2] = ["$defs", "methods"];

/// Deterministic JSON of [`protocol_schema`] with sorted keys, one line per
/// `$defs` and `methods` entry, and a trailing newline.
///
/// # Panics
/// Never in practice: a `Value` always serializes.
#[must_use]
pub fn render() -> String {
    let Value::Object(root) = protocol_schema() else {
        unreachable!("the schema document is an object");
    };
    let compact = |v: &Value| serde_json::to_string(v).expect("a Value serializes");
    let mut lines = Vec::new();
    for (key, value) in &root {
        let key_json = compact(&Value::String(key.clone()));
        match value {
            Value::Object(entries) if EXPANDED_KEYS.contains(&key.as_str()) => {
                let body: Vec<String> = entries
                    .iter()
                    .map(|(name, entry)| {
                        format!(
                            "    {}: {}",
                            compact(&Value::String(name.clone())),
                            compact(entry)
                        )
                    })
                    .collect();
                lines.push(format!("  {key_json}: {{\n{}\n  }}", body.join(",\n")));
            }
            other => lines.push(format!("  {key_json}: {}", compact(other))),
        }
    }
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}
