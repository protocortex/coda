// SPDX-License-Identifier: AGPL-3.0-only

//! The base tools every model-API session offers: read, grep, edit, bash and
//! retrieve, with the schemas the model sees.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use coda_protocol::readers::{OriginalsReader, ReaderError};
use coda_protocol::types::{OriginalId, SessionId};
use coda_store::Store;
use coda_tools::DEFAULT_MAX_OUTPUT_BYTES;
use coda_tools::bash::{BashConfig, BashTool};
use coda_tools::edit::{DEFAULT_MAX_FILE_BYTES, EditTool};
use coda_tools::grep::GrepTool;
use coda_tools::read::ReadTool;
use coda_tools::retrieve::RetrieveTool;
use coda_tools::sandbox::{Sandbox, SandboxError};
use serde_json::json;

use crate::prompt::ToolSchema;
use crate::turn::ToolSet;

/// Where the base tools run and what their shell may see.
#[derive(Debug, Clone)]
pub struct BaseToolsConfig {
    /// The workspace the file tools are confined to and bash runs in.
    pub workdir: PathBuf,
    /// The longest one shell command may run.
    pub bash_timeout: Duration,
    /// The only environment variables the shell sees.
    pub env: Vec<(OsString, OsString)>,
}

/// The tools of a session and the schemas that describe them to the model.
pub struct BaseTools {
    /// The tools, ready for a turn loop.
    pub tools: ToolSet,
    /// One schema per tool, ordered by name.
    pub schemas: Vec<ToolSchema>,
}

/// Why the base tools could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    /// The workspace directory cannot be used as a sandbox root.
    #[error("the workspace cannot be used for tools: {0}")]
    Workspace(#[from] SandboxError),
}

/// Reads the originals of one session out of a shared store.
pub struct StoreOriginals {
    store: Arc<Mutex<Store>>,
    session: SessionId,
}

impl StoreOriginals {
    /// A reader for `session`; ids of other sessions are not found.
    #[must_use]
    pub fn new(store: Arc<Mutex<Store>>, session: SessionId) -> Self {
        Self { store, session }
    }
}

impl OriginalsReader for StoreOriginals {
    fn fetch_original(&self, id: &OriginalId) -> Result<String, ReaderError> {
        // SQLite rolls an interrupted transaction back, so a poisoned guard is safe to reuse.
        let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        store.originals_reader(&self.session).fetch_original(id)
    }
}

/// Builds the five base tools over `config.workdir`.
///
/// # Errors
///
/// Returns [`ToolsError::Workspace`] when the workspace directory cannot be resolved.
pub fn base_tools(
    config: &BaseToolsConfig,
    originals: impl OriginalsReader + Send + Sync + 'static,
) -> Result<BaseTools, ToolsError> {
    let sandbox = || Sandbox::new(&config.workdir);
    let bash = BashConfig {
        timeout: config.bash_timeout,
        max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        env: config.env.clone(),
    };
    let tools = ToolSet::new()
        .with(Arc::new(ReadTool::new(
            sandbox()?,
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(GrepTool::new(
            sandbox()?,
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(EditTool::new(sandbox()?, DEFAULT_MAX_FILE_BYTES)))
        .with(Arc::new(BashTool::new(config.workdir.clone(), bash)))
        .with(Arc::new(RetrieveTool::new(
            originals,
            DEFAULT_MAX_OUTPUT_BYTES,
        )));
    Ok(BaseTools {
        tools,
        schemas: schemas(),
    })
}

fn schema(
    name: &str,
    description: &str,
    properties: &serde_json::Value,
    required: &[&str],
) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn schemas() -> Vec<ToolSchema> {
    vec![
        schema(
            "bash",
            "Run a shell command in the workspace and return its output.",
            &json!({
                "command": {"type": "string", "description": "The command to run."},
                "timeout_ms": {"type": "integer", "description": "Stop the command after this many milliseconds."},
            }),
            &["command"],
        ),
        schema(
            "edit",
            "Replace exactly one occurrence of a string in a workspace file.",
            &json!({
                "path": {"type": "string", "description": "File path relative to the workspace."},
                "old_string": {"type": "string", "description": "The text to replace; it must occur once."},
                "new_string": {"type": "string", "description": "The replacement text."},
            }),
            &["path", "old_string", "new_string"],
        ),
        schema(
            "grep",
            "Search workspace files for lines matching a regular expression.",
            &json!({
                "pattern": {"type": "string", "description": "The regular expression."},
                "path": {"type": "string", "description": "Directory to search, relative to the workspace."},
                "include_dirs": {"type": "array", "items": {"type": "string"}, "description": "Directories to search inside the path."},
            }),
            &["pattern"],
        ),
        schema(
            "read",
            "Read a byte range of a workspace file.",
            &json!({
                "path": {"type": "string", "description": "File path relative to the workspace."},
                "offset": {"type": "integer", "description": "First byte to read."},
                "length": {"type": "integer", "description": "How many bytes to read."},
            }),
            &["path"],
        ),
        schema(
            "retrieve",
            "Fetch the full output behind a shortened tool result.",
            &json!({"id": {"type": "string", "description": "The id named in the shortened result."}}),
            &["id"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use coda_protocol::readers::{OriginalsReader, ReaderError};
    use coda_protocol::types::{BackendKind, Mode, SessionId};
    use coda_store::{NewSession, Store};
    use tempfile::TempDir;

    use super::{BaseToolsConfig, StoreOriginals, base_tools};

    const NAMES: [&str; 5] = ["bash", "edit", "grep", "read", "retrieve"];

    fn new_session(store: &mut Store) -> SessionId {
        store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd: "/ws".to_owned(),
                mode: Mode::default(),
            })
            .unwrap()
    }

    fn open_store(dir: &TempDir) -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open(&dir.path().join("data")).unwrap()))
    }

    #[test]
    fn the_base_tools_are_registered_with_object_schemas() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir);
        let session = new_session(&mut store.lock().unwrap());
        let config = BaseToolsConfig {
            workdir: dir.path().to_path_buf(),
            bash_timeout: Duration::from_secs(5),
            env: Vec::new(),
        };

        // Act
        let tools = base_tools(&config, StoreOriginals::new(store, session)).unwrap();

        // Assert
        assert_eq!(tools.tools.names(), NAMES);
        let schema_names: Vec<&str> = tools.schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(schema_names, NAMES);
        for schema in &tools.schemas {
            assert_eq!(schema.input_schema["type"], "object", "{}", schema.name);
            assert!(!schema.description.is_empty(), "{}", schema.name);
            assert!(
                schema.input_schema["required"].is_array(),
                "{}",
                schema.name
            );
        }
    }

    #[test]
    fn store_originals_serve_only_the_owning_session() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(&dir);
        let (owner, other, id) = {
            let mut guard = store.lock().unwrap();
            let owner = new_session(&mut guard);
            let other = new_session(&mut guard);
            let id = guard.store_original(&owner, "full output").unwrap();
            (owner, other, id)
        };

        // Act
        let own = StoreOriginals::new(Arc::clone(&store), owner).fetch_original(&id);
        let foreign = StoreOriginals::new(store, other).fetch_original(&id);

        // Assert
        assert_eq!(own.unwrap(), "full output");
        assert!(matches!(foreign, Err(ReaderError::NotFound(_))));
    }
}
