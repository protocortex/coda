// SPDX-License-Identifier: AGPL-3.0-only

//! The committed JSON Schema must match what the Rust types generate, and the
//! golden payloads must validate against it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use coda_protocol::methods::METHODS;
use coda_protocol::schema::{protocol_schema, render};
use serde_json::{Value, json};

const MAX_SCHEMA_LINES: usize = 150;
const REGENERATE_COMMAND: &str = "cargo run -p coda-protocol --example gen-schema";

fn workspace_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read_json(path: &PathBuf) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn committed_schema_matches_generated_schema() {
    // Arrange
    let path = workspace_path("../../schema/coda-protocol.schema.json");
    let committed = fs::read_to_string(&path).unwrap_or_default();

    // Act
    let generated = render();

    // Assert
    assert!(
        committed == generated,
        "schema/coda-protocol.schema.json is out of date, regenerate it with `{REGENERATE_COMMAND}` and commit the result"
    );
}

#[test]
fn rendered_schema_is_compact_valid_json() {
    // Arrange
    let expected = protocol_schema();

    // Act
    let rendered = render();

    // Assert
    let parsed: Value = serde_json::from_str(&rendered).expect("render emits valid JSON");
    assert_eq!(parsed, expected);
    assert!(rendered.ends_with('\n'));
    let lines = rendered.lines().count();
    assert!(
        lines < MAX_SCHEMA_LINES,
        "rendered schema has {lines} lines, the limit is {MAX_SCHEMA_LINES}"
    );
}

/// Keys that make a schema entry a real schema rather than an empty stub.
const SCHEMA_KEYS: [&str; 7] = [
    "$ref",
    "type",
    "properties",
    "oneOf",
    "anyOf",
    "allOf",
    "enum",
];

/// Wraps a `$ref` or inline schema entry with the document's `$defs`, or says
/// why the entry is not a schema.
fn standalone_schema(entry: &Value, defs: &Value) -> Result<Value, String> {
    let Some(object) = entry.as_object() else {
        return Err(format!("entry is not a schema object: {entry}"));
    };
    if !SCHEMA_KEYS.iter().any(|key| object.contains_key(*key)) {
        return Err(format!(
            "entry is neither a $ref nor an inline schema: {entry}"
        ));
    }
    let mut schema = object.clone();
    schema.insert("$defs".to_owned(), defs.clone());
    Ok(Value::Object(schema))
}

#[test]
fn schema_methods_equal_the_method_list() {
    // Arrange
    let schema = protocol_schema();
    let in_schema: BTreeSet<&str> = schema["methods"]
        .as_object()
        .expect("methods map")
        .keys()
        .map(String::as_str)
        .collect();

    // Act
    let listed: BTreeSet<&str> = METHODS.iter().copied().collect();

    // Assert
    assert_eq!(in_schema, listed);
}

#[test]
fn entries_that_are_not_schemas_are_rejected() {
    // Arrange
    let defs = json!({});
    let bad = [
        json!(null),
        json!({}),
        json!(true),
        json!({"description": "x"}),
    ];

    for entry in bad {
        // Act
        let result = standalone_schema(&entry, &defs);

        // Assert
        assert!(result.is_err(), "{entry} should be rejected");
    }
}

#[test]
fn ref_and_inline_entries_are_accepted() {
    // Arrange
    let defs = json!({});

    for entry in [json!({"$ref": "#/$defs/A"}), json!({"type": "object"})] {
        // Act
        let result = standalone_schema(&entry, &defs);

        // Assert
        assert!(result.is_ok(), "{entry} should be accepted");
    }
}

#[test]
fn golden_payloads_validate_against_the_schema() {
    // Arrange
    let schema = protocol_schema();
    let defs = schema["$defs"].clone();
    let methods = schema["methods"].as_object().expect("methods map");
    let mut cases: Vec<(String, Value)> = Vec::new();
    for (method, entry) in methods {
        for part in ["params", "result"] {
            cases.push((format!("{method}.{part}"), entry[part].clone()));
        }
    }
    cases.push(("event".to_owned(), schema["event"].clone()));
    assert!(cases.len() > 2, "no methods found in the schema");

    for (name, entry) in cases {
        let standalone = standalone_schema(&entry, &defs).unwrap_or_else(|e| panic!("{name}: {e}"));
        let validator = jsonschema::validator_for(&standalone).expect("schema");
        let goldens: Vec<PathBuf> = if name == "event" {
            fs::read_dir(workspace_path("tests/golden"))
                .expect("golden dir")
                .map(|e| e.expect("entry").path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("event."))
                })
                .collect()
        } else {
            vec![workspace_path(&format!("tests/golden/{name}.json"))]
        };
        assert!(!goldens.is_empty(), "no golden payload for {name}");

        for path in goldens {
            // Act
            let payload = read_json(&path);
            let errors: Vec<String> = validator
                .iter_errors(&payload)
                .map(|e| e.to_string())
                .collect();

            // Assert
            assert!(
                errors.is_empty(),
                "{} fails the schema: {errors:?}",
                path.display()
            );
        }
    }
}
