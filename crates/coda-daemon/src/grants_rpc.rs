// SPDX-License-Identifier: AGPL-3.0-only

//! The `grant.add` request: stores a standing permission grant through the store API.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use coda_protocol::methods::{GrantAddParams, GrantAddResult};
use serde_json::Value;

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::Hub;

/// Longest accepted value for any grant field, in bytes.
pub(crate) const MAX_FIELD_BYTES: usize = 4096;

/// Saves the grant in `params` and answers with when it was first stored.
///
/// Repeating a request changes nothing, so a retry is safe. Without a hub the
/// daemon has no store and the method is unavailable.
pub(crate) async fn add_grant(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let Some(hub) = hub else {
        return Err(error(METHOD_NOT_FOUND, "Method not found"));
    };
    let params: GrantAddParams = parse_params(params)?;
    validate(&params)?;
    let saved = hub
        .add_grant(params.repo_path, params.tool, params.cwd, params.pattern)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "grant.add failed");
            error(INTERNAL_ERROR, "Internal error")
        })?;
    let Some(grant) = saved else {
        tracing::error!("grant.add stored a grant that could not be read back");
        return Err(error(INTERNAL_ERROR, "Internal error"));
    };
    to_value(&GrantAddResult {
        granted_at: grant.granted_at,
    })
}

fn validate(params: &GrantAddParams) -> Result<(), ErrorObject> {
    let fields = [
        ("repo_path", &params.repo_path),
        ("tool", &params.tool),
        ("cwd", &params.cwd),
        ("pattern", &params.pattern),
    ];
    for (name, value) in fields {
        if value.is_empty() {
            return Err(invalid(name, "must not be empty"));
        }
        if value.len() > MAX_FIELD_BYTES {
            return Err(invalid(name, "is too long"));
        }
    }
    for (name, value) in [("repo_path", &params.repo_path), ("cwd", &params.cwd)] {
        if !Path::new(value).is_absolute() {
            return Err(invalid(name, "must be an absolute path"));
        }
    }
    Ok(())
}

fn invalid(field: &str, problem: &str) -> ErrorObject {
    error(
        INVALID_PARAMS,
        &format!("Invalid params: {field} {problem}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use coda_protocol::jsonrpc::{INVALID_PARAMS, METHOD_NOT_FOUND};
    use coda_store::{GrantKey, Store};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{MAX_FIELD_BYTES, add_grant};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

    /// The store creates the data directory itself with private permissions.
    const DATA: &str = "data";

    fn hub_in(dir: &TempDir) -> Arc<Hub> {
        let store = Store::open(&dir.path().join(DATA)).unwrap();
        Hub::new(store, DEFAULT_QUEUE_CAPACITY)
    }

    fn params() -> serde_json::Value {
        json!({"repo_path": "/work/repo", "tool": "bash", "cwd": "/work/repo/sub", "pattern": "cargo test"})
    }

    const KEY: GrantKey<'static> = GrantKey {
        repo_path: "/work/repo",
        tool: "bash",
        cwd: "/work/repo/sub",
        pattern: "cargo test",
    };

    #[tokio::test]
    async fn add_grant_stores_the_exact_key_through_the_store() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);

        // Act
        let result = add_grant(Some(&hub), Some(params())).await.unwrap();

        // Assert
        let stored = Store::open(&dir.path().join(DATA))
            .unwrap()
            .lookup_grant(&KEY)
            .unwrap();
        assert_eq!(result["granted_at"], json!(stored.unwrap().granted_at));
    }

    #[tokio::test]
    async fn add_grant_twice_keeps_the_first_grant_time() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);
        let first = add_grant(Some(&hub), Some(params())).await.unwrap();

        // Act
        let second = add_grant(Some(&hub), Some(params())).await.unwrap();

        // Assert
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn add_grant_with_missing_fields_is_invalid_params() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);

        // Act
        let error = add_grant(Some(&hub), Some(json!({"tool": "bash"})))
            .await
            .unwrap_err();

        // Assert
        assert_eq!(error.code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn add_grant_without_a_store_is_method_not_found() {
        // Act
        let error = add_grant(None, Some(params())).await.unwrap_err();

        // Assert
        assert_eq!(error.code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn add_grant_matches_only_the_exact_key() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);
        add_grant(Some(&hub), Some(params())).await.unwrap();
        let store = Store::open(&dir.path().join(DATA)).unwrap();
        let misses = [
            GrantKey {
                repo_path: "/work/other",
                ..KEY
            },
            GrantKey {
                tool: "edit",
                ..KEY
            },
            GrantKey {
                cwd: "/work/repo",
                ..KEY
            },
            GrantKey {
                pattern: "cargo build",
                ..KEY
            },
        ];

        // Act
        let found: Vec<bool> = misses
            .iter()
            .map(|key| store.lookup_grant(key).unwrap().is_some())
            .collect();

        // Assert
        assert_eq!(found, [false; 4]);
    }

    #[tokio::test]
    async fn add_grant_rejects_unusable_fields() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);
        let too_long = "x".repeat(MAX_FIELD_BYTES + 1);
        let cases = [
            ("repo_path", json!("")),
            ("tool", json!("")),
            ("cwd", json!("")),
            ("pattern", json!("")),
            ("repo_path", json!("relative/repo")),
            ("cwd", json!("relative/cwd")),
            ("tool", json!(too_long)),
            ("pattern", json!(too_long)),
            ("repo_path", json!(format!("/{too_long}"))),
            ("cwd", json!(format!("/{too_long}"))),
        ];

        for (field, value) in cases {
            // Act
            let mut request = params();
            request[field] = value;
            let error = add_grant(Some(&hub), Some(request)).await.unwrap_err();

            // Assert
            assert_eq!(error.code, INVALID_PARAMS, "field {field}");
        }
    }

    #[tokio::test]
    async fn add_grant_accepts_fields_at_the_length_limit() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(&dir);
        let mut request = params();
        request["pattern"] = json!("x".repeat(MAX_FIELD_BYTES));

        // Act
        let result = add_grant(Some(&hub), Some(request)).await;

        // Assert
        assert!(result.is_ok());
    }
}
