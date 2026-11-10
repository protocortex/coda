// SPDX-License-Identifier: Apache-2.0

//! Listed tool servers become tools of a session.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::atomic::AtomicBool;

use coda_config::tool_servers::{DEFAULT_SERVER_TIMEOUT_MS, ToolServerConfig};
use coda_core::tool_servers::{ToolServers, offers_tools, with_tool_servers};
use coda_core::tools::BaseTools;
use coda_core::turn::ToolSet;
use coda_testkit::paths::fake_tool_server_path;
use serde_json::json;
use tempfile::TempDir;

fn listed(command: Vec<String>, env: &[&str]) -> BTreeMap<String, ToolServerConfig> {
    let server = ToolServerConfig {
        command,
        env: env.iter().map(|name| (*name).to_owned()).collect(),
        timeout_ms: DEFAULT_SERVER_TIMEOUT_MS,
    };
    BTreeMap::from([("srv".to_owned(), server)])
}

fn fake() -> Vec<String> {
    vec![
        fake_tool_server_path().unwrap().display().to_string(),
        "default".to_owned(),
    ]
}

fn daemon_env() -> Vec<(OsString, OsString)> {
    [("LISTED", "yes"), ("SECRET", "hunter2")]
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

#[test]
fn a_server_gets_only_the_variables_it_named_from_the_daemon_environment() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let servers = ToolServers::new(&listed(fake(), &["LISTED"]), &daemon_env());

    // Act
    let started = servers.start(dir.path(), &[]);
    let (seen, _) = started[0]
        .server
        .call("show_env", &json!({}), &AtomicBool::new(false))
        .unwrap();

    // Assert
    assert_eq!(seen, "LISTED");
}

#[test]
fn the_tools_of_a_started_server_are_added_to_the_session_with_their_schemas() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let started = ToolServers::new(&listed(fake(), &[]), &[]).start(dir.path(), &["bash"]);
    let base = BaseTools {
        tools: ToolSet::new(),
        schemas: Vec::new(),
    };

    // Act
    let tools = with_tool_servers(base, &started);

    // Assert
    let names: Vec<&str> = tools.schemas.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"srv__echo"), "{names:?}");
    assert!(names.is_sorted(), "{names:?}");
    assert_eq!(tools.tools.names().len(), names.len());
    assert!(offers_tools(&started));
}

#[test]
fn a_server_that_does_not_start_is_left_out() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let servers = ToolServers::new(&listed(vec!["/no/such/program".to_owned()], &[]), &[]);

    // Act
    let started = servers.start(dir.path(), &[]);

    // Assert
    assert!(started.is_empty());
    assert!(!offers_tools(&started));
}

#[test]
fn nothing_is_listed_by_default() {
    // Act
    let servers = ToolServers::new(&BTreeMap::new(), &daemon_env());

    // Assert
    assert!(servers.is_empty());
}
