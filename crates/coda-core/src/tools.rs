// SPDX-License-Identifier: AGPL-3.0-only

//! The base tools every model-API session offers: read, grep, edit, bash,
//! retrieve and the code index tools, with the schemas the model sees.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use coda_index::{WalkLimits, apply_walk, index_workspace, list_sources};
use coda_protocol::readers::{
    IndexReader, OriginalsReader, ReaderError, SearchHit, SymbolLocation,
};
use coda_protocol::types::{OriginalId, SessionId};
use coda_store::Store;
use coda_tools::bash::{BashConfig, BashTool};
use coda_tools::code::{
    CodeIndex, CodeOutlineTool, CodeReadRangeTool, CodeSearchTool, CodeSymbolTool, Freshness,
};
use coda_tools::edit::{DEFAULT_MAX_FILE_BYTES, EditTool};
use coda_tools::grep::GrepTool;
use coda_tools::read::ReadTool;
use coda_tools::retrieve::RetrieveTool;
use coda_tools::sandbox::{Sandbox, SandboxError};
use coda_tools::{DEFAULT_MAX_OUTPUT_BYTES, Tool, ToolError, ToolOutput};
use serde_json::Value;
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

/// The workspace symbol index kept in a shared store.
pub struct StoreCodeIndex {
    store: Arc<Mutex<Store>>,
    root: PathBuf,
}

impl StoreCodeIndex {
    /// An index over the repo at `root`, keyed as [`refresh_index`] keys it.
    #[must_use]
    pub fn new(store: Arc<Mutex<Store>>, root: PathBuf) -> Self {
        Self { store, root }
    }

    fn repo(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    // SQLite rolls an interrupted transaction back, so a poisoned guard is safe to reuse.
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(error: impl std::fmt::Display) -> ReaderError {
    ReaderError::Unavailable(error.to_string())
}

impl IndexReader for StoreCodeIndex {
    fn symbols(&self, query: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
        lock(&self.store).index_reader(&self.repo()).symbols(query)
    }

    fn outline(&self, path: &str) -> Result<Vec<SymbolLocation>, ReaderError> {
        lock(&self.store).index_reader(&self.repo()).outline(path)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, ReaderError> {
        lock(&self.store)
            .index_reader(&self.repo())
            .search(query, limit)
    }
}

impl CodeIndex for StoreCodeIndex {
    /// An indexed file counts as changed when it is gone or was modified at or
    /// after the time it was indexed. A supported file that is not indexed and
    /// appeared since the last refresh counts as changed too.
    fn freshness(&self) -> Result<Freshness, ReaderError> {
        let repo = self.repo();
        let (status, files) = {
            let store = lock(&self.store);
            let status = store.index_status(&repo).map_err(unavailable)?;
            (status, store.indexed_files(&repo).map_err(unavailable)?)
        };
        if status.files == 0 {
            return Ok(Freshness::Empty);
        }
        let modified_since = |path: &str, since: i64| {
            modified_ms(&self.root.join(path)).is_none_or(|modified| modified >= since)
        };
        let mut changed = files
            .iter()
            .filter(|file| modified_since(&file.file_path, file.indexed_at))
            .count();
        let last_refresh = status.last_indexed_at.unwrap_or(i64::MIN);
        let indexed: HashSet<&str> = files.iter().map(|file| file.file_path.as_str()).collect();
        changed += list_sources(&self.root, WalkLimits::default())
            .iter()
            .map(|path| path.to_string_lossy())
            .filter(|path| !indexed.contains(path.as_ref()) && modified_since(path, last_refresh))
            .count();
        if changed == 0 {
            return Ok(Freshness::Fresh {
                files: status.files,
                symbols: status.symbols,
            });
        }
        Ok(Freshness::Stale {
            files: status.files,
            changed: changed as u64,
        })
    }
}

/// Milliseconds since the Unix epoch at which `path` last changed, or `None`
/// when it cannot be statted.
fn modified_ms(path: &Path) -> Option<i64> {
    let modified = path.metadata().and_then(|m| m.modified()).ok()?;
    let since_epoch = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).ok()
}

/// Brings the index of the repo at `root` in line with the files on disk.
///
/// The walk and hashing run without the store; it is locked only for the
/// writes. A failure is logged and otherwise ignored: a stale index only costs
/// accuracy, and the code tools report staleness themselves. Blocks for the
/// whole walk, so async callers run it under `spawn_blocking`.
pub fn refresh_index(store: &Mutex<Store>, root: &Path) {
    let walk = match index_workspace(root, WalkLimits::default()) {
        Ok(walk) => walk,
        Err(error) => {
            tracing::warn!(%error, "walking the workspace for the code index failed");
            return;
        }
    };
    match apply_walk(&mut lock(store), root, walk) {
        Ok(report) => tracing::debug!(
            indexed = report.indexed.len(),
            removed = report.removed.len(),
            failed = report.failed.len(),
            "refreshed the code index"
        ),
        Err(error) => tracing::warn!(%error, "refreshing the code index failed"),
    }
}

/// The edit tool, followed by an index refresh after every edit that applied.
pub struct RefreshAfterEdit {
    edit: EditTool,
    store: Arc<Mutex<Store>>,
    root: PathBuf,
}

impl RefreshAfterEdit {
    /// Wraps `edit`, refreshing the index of `root` in `store` after it succeeds.
    #[must_use]
    pub fn new(edit: EditTool, store: Arc<Mutex<Store>>, root: PathBuf) -> Self {
        Self { edit, store, root }
    }
}

impl Tool for RefreshAfterEdit {
    fn name(&self) -> &'static str {
        self.edit.name()
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let output = self.edit.call(input)?;
        refresh_index(&self.store, &self.root);
        Ok(output)
    }
}

/// Builds the base tools over `config.workdir`; the code tools query the index
/// in `store`, which [`refresh_index`] fills.
///
/// # Errors
///
/// Returns [`ToolsError::Workspace`] when the workspace directory cannot be resolved.
pub fn base_tools(
    config: &BaseToolsConfig,
    store: Arc<Mutex<Store>>,
    originals: impl OriginalsReader + Send + Sync + 'static,
) -> Result<BaseTools, ToolsError> {
    let sandbox = || Sandbox::new(&config.workdir);
    let index: Arc<dyn CodeIndex> = Arc::new(StoreCodeIndex::new(
        Arc::clone(&store),
        config.workdir.clone(),
    ));
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
        .with(Arc::new(RefreshAfterEdit::new(
            EditTool::new(sandbox()?, DEFAULT_MAX_FILE_BYTES),
            store,
            config.workdir.clone(),
        )))
        .with(Arc::new(BashTool::new(config.workdir.clone(), bash)))
        .with(Arc::new(RetrieveTool::new(
            originals,
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(CodeSearchTool::new(
            Arc::clone(&index),
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(CodeSymbolTool::new(
            Arc::clone(&index),
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(CodeOutlineTool::new(
            index,
            DEFAULT_MAX_OUTPUT_BYTES,
        )))
        .with(Arc::new(CodeReadRangeTool::new(
            sandbox()?,
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
            "code_outline",
            "List the indexed symbols of one source file in order. The first line reports whether the index is fresh or stale.",
            &json!({"path": {"type": "string", "description": "File path relative to the workspace."}}),
            &["path"],
        ),
        schema(
            "code_read_range",
            "Read an inclusive range of lines from a workspace file.",
            &json!({
                "path": {"type": "string", "description": "File path relative to the workspace."},
                "start_line": {"type": "integer", "description": "First line to read, counting from 1."},
                "end_line": {"type": "integer", "description": "Last line to read; a value past the end of the file is clamped."},
            }),
            &["path", "start_line", "end_line"],
        ),
        schema(
            "code_search",
            "Search indexed symbol names. The first line reports whether the index is fresh or stale.",
            &json!({
                "query": {"type": "string", "description": "Text to look for; it is matched literally as a name prefix."},
                "limit": {"type": "integer", "description": "Most hits to return."},
            }),
            &["query"],
        ),
        schema(
            "code_symbol",
            "Find where a symbol is defined. The first line reports whether the index is fresh or stale.",
            &json!({"name": {"type": "string", "description": "The symbol name."}}),
            &["name"],
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

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::UNIX_EPOCH;

    use coda_store::Clock;

    use coda_protocol::readers::IndexReader;
    use coda_tools::code::{CodeIndex, Freshness};
    use coda_tools::edit::{DEFAULT_MAX_FILE_BYTES, EditTool};
    use coda_tools::sandbox::Sandbox;
    use coda_tools::{Tool, ToolError};
    use serde_json::json;

    use super::{
        BaseToolsConfig, RefreshAfterEdit, StoreCodeIndex, StoreOriginals, base_tools,
        refresh_index,
    };

    const NAMES: [&str; 9] = [
        "bash",
        "code_outline",
        "code_read_range",
        "code_search",
        "code_symbol",
        "edit",
        "grep",
        "read",
        "retrieve",
    ];
    const LIB_SOURCE: &str = "pub fn alpha() {}\n";
    const INDEXED_AT: i64 = 10_000;
    const ONE_FRESH: Freshness = Freshness::Fresh {
        files: 1,
        symbols: 1,
    };
    const ONE_CHANGED: Freshness = Freshness::Stale {
        files: 1,
        changed: 1,
    };

    #[derive(Debug, Default)]
    struct TestClock(AtomicI64);

    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// A store on a manual clock and a workspace holding `src/lib.rs`, indexed
    /// at [`INDEXED_AT`] after the file was last modified.
    fn indexed_workspace() -> (TempDir, Arc<Mutex<Store>>, PathBuf, Arc<TestClock>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB_SOURCE).unwrap();
        set_modified(&root.join("src/lib.rs"), INDEXED_AT - 5_000);
        let clock = Arc::new(TestClock(AtomicI64::new(INDEXED_AT)));
        let store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
        let store = Arc::new(Mutex::new(store));
        refresh_index(&store, &root);
        (dir, store, root, clock)
    }

    fn set_modified(path: &Path, ms: i64) {
        let at = UNIX_EPOCH + Duration::from_millis(ms.unsigned_abs());
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }

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
        let tools = base_tools(
            &config,
            Arc::clone(&store),
            StoreOriginals::new(store, session),
        )
        .unwrap();

        // Assert
        assert_eq!(tools.tools.names(), NAMES);
        let schema_names: Vec<&str> = tools.schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(schema_names, NAMES);
        assert_eq!(schema_names, coda_tools::TOOL_NAMES);
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

    #[test]
    fn a_just_indexed_workspace_is_fresh() {
        // Arrange
        let (_dir, store, root, _clock) = indexed_workspace();
        let index = StoreCodeIndex::new(store, root);

        // Act
        let freshness = index.freshness().unwrap();

        // Assert
        assert_eq!(freshness, ONE_FRESH);
    }

    #[test]
    fn an_unindexed_workspace_is_empty() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let index = StoreCodeIndex::new(open_store(&dir), dir.path().join("ws"));

        // Act
        let freshness = index.freshness().unwrap();

        // Assert
        assert_eq!(freshness, Freshness::Empty);
    }

    #[test]
    fn an_out_of_band_edit_makes_the_index_stale_until_it_is_refreshed() {
        // Arrange
        let (_dir, store, root, clock) = indexed_workspace();
        let index = StoreCodeIndex::new(Arc::clone(&store), root.clone());
        let file = root.join("src/lib.rs");
        fs::write(&file, "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
        set_modified(&file, INDEXED_AT + 5_000);
        clock.0.store(INDEXED_AT + 10_000, Ordering::SeqCst);

        // Act
        let stale = index.freshness().unwrap();
        let before = index.symbols("beta").unwrap();
        refresh_index(&store, &root);
        let after = index.symbols("beta").unwrap();

        // Assert
        assert_eq!(stale, ONE_CHANGED);
        assert_eq!(before, []);
        assert_eq!(after.len(), 1);
        assert!(matches!(index.freshness(), Ok(Freshness::Fresh { .. })));
    }

    #[test]
    fn touching_a_file_without_changing_it_is_fresh_again_after_a_refresh() {
        // Arrange
        let (_dir, store, root, clock) = indexed_workspace();
        let index = StoreCodeIndex::new(Arc::clone(&store), root.clone());
        set_modified(&root.join("src/lib.rs"), INDEXED_AT + 5_000);
        clock.0.store(INDEXED_AT + 10_000, Ordering::SeqCst);
        let touched = index.freshness().unwrap();

        // Act
        refresh_index(&store, &root);
        let refreshed = index.freshness().unwrap();

        // Assert
        assert!(matches!(touched, Freshness::Stale { .. }));
        assert_eq!(refreshed, ONE_FRESH);
    }

    type Change = fn(&Path);

    fn modify_at_indexing_time(root: &Path) {
        set_modified(&root.join("src/lib.rs"), INDEXED_AT);
    }

    fn create_after_indexing(root: &Path) {
        let created = root.join("src/new.rs");
        fs::write(&created, "pub fn newcomer() {}\n").unwrap();
        set_modified(&created, INDEXED_AT + 5_000);
    }

    fn create_unindexable_before_indexing(root: &Path) {
        let skipped = root.join("src/binary.rs");
        fs::write(&skipped, b"\0\0").unwrap();
        set_modified(&skipped, INDEXED_AT - 5_000);
    }

    fn delete_indexed_file(root: &Path) {
        fs::remove_file(root.join("src/lib.rs")).unwrap();
    }

    #[test]
    fn freshness_reflects_changes_made_outside_the_tools() {
        // Arrange
        let cases: [(&str, Change, Freshness); 4] = [
            ("same millisecond", modify_at_indexing_time, ONE_CHANGED),
            ("new file", create_after_indexing, ONE_CHANGED),
            (
                "old skipped file",
                create_unindexable_before_indexing,
                ONE_FRESH,
            ),
            ("deleted file", delete_indexed_file, ONE_CHANGED),
        ];

        for (name, change, expected) in cases {
            let (_dir, store, root, _clock) = indexed_workspace();
            let index = StoreCodeIndex::new(store, root.clone());
            change(&root);

            // Act
            let freshness = index.freshness().unwrap();

            // Assert
            assert_eq!(freshness, expected, "{name}");
        }
    }

    #[test]
    fn search_survives_fts_hostile_input() {
        // Arrange
        let (_dir, store, root, _clock) = indexed_workspace();
        let index = StoreCodeIndex::new(store, root);
        let hostile = [
            "\"",
            "\"\"",
            "alpha OR",
            "NEAR(",
            "a AND",
            "*",
            "alpha*(",
            "-alpha",
            "col:alpha",
            "'; DROP TABLE symbols;--",
        ];

        for query in hostile {
            // Act
            let result = index.search(query, 10);

            // Assert
            assert!(result.is_ok(), "{query}: {result:?}");
        }
    }

    #[test]
    fn search_finds_a_symbol_by_prefix() {
        // Arrange
        let (_dir, store, root, _clock) = indexed_workspace();
        let index = StoreCodeIndex::new(store, root);

        // Act
        let hits = index.search("alp", 10).unwrap();

        // Assert
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/lib.rs");
    }

    #[test]
    fn a_successful_edit_refreshes_the_index() {
        // Arrange
        let (_dir, store, root, _clock) = indexed_workspace();
        let index = StoreCodeIndex::new(Arc::clone(&store), root.clone());
        let edit = EditTool::new(Sandbox::new(&root).unwrap(), DEFAULT_MAX_FILE_BYTES);
        let tool = RefreshAfterEdit::new(edit, Arc::clone(&store), root);

        // Act
        tool.call(&json!({
            "path": "src/lib.rs",
            "old_string": "alpha",
            "new_string": "gamma"
        }))
        .unwrap();

        // Assert
        assert_eq!(tool.name(), "edit");
        assert_eq!(index.symbols("gamma").unwrap().len(), 1);
        assert_eq!(index.symbols("alpha").unwrap(), []);
    }

    #[test]
    fn a_failed_edit_leaves_the_index_alone() {
        // Arrange
        let (_dir, store, root, _clock) = indexed_workspace();
        let index = StoreCodeIndex::new(Arc::clone(&store), root.clone());
        let edit = EditTool::new(Sandbox::new(&root).unwrap(), DEFAULT_MAX_FILE_BYTES);
        let tool = RefreshAfterEdit::new(edit, store, root);

        // Act
        let result = tool.call(&json!({
            "path": "src/lib.rs",
            "old_string": "absent",
            "new_string": "x"
        }));

        // Assert
        assert!(matches!(result, Err(ToolError::NoMatch { .. })));
        assert_eq!(index.symbols("alpha").unwrap().len(), 1);
    }
}
