// SPDX-License-Identifier: Apache-2.0

//! The pull request watches a daemon runs, bounded in number, and the
//! `watch_pr` tool a model session calls to start one.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use coda_protocol::types::SessionId;
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::runtime::Handle;

use crate::pr_watch::{POLL_INTERVAL, run};
use crate::pr_watch_gh::{GhReader, URL_PREFIX, is_pull_request_reference};

/// The name the model uses to call the tool.
pub(crate) const TOOL_NAME: &str = "watch_pr";
/// Most watches one session runs at once.
pub(crate) const MAX_WATCHES_PER_SESSION: usize = 5;
/// Most watches the daemon runs at once.
pub(crate) const MAX_WATCHES: usize = 32;
const TOOL_DESCRIPTION: &str = "Watch a pull request and be woken with a message when a check \
fails, the required checks pass, someone else comments or reviews, or the branch conflicts. The \
watch reads the pull request every 2 minutes with the user's `gh`, never repeats news it already \
gave, and stops when the pull request is merged or closed. `pr` is the number or the web address.";

/// Passes a wake message to the session it is for.
pub(crate) type Waker = Arc<dyn Fn(SessionId, String) + Send + Sync>;

/// Why a watch could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WatchError {
    #[error("the pull request must be a number or an https web address")]
    BadReference,
    #[error("this session already watches that pull request")]
    Duplicate,
    #[error("a session may watch at most {MAX_WATCHES_PER_SESSION} pull requests at once")]
    SessionFull,
    #[error("the daemon watches at most {MAX_WATCHES} pull requests at once")]
    DaemonFull,
    #[error("the daemon cannot start a watch now")]
    NoRuntime,
}

/// The running watches, one task each.
pub(crate) struct PrWatches {
    waker: OnceLock<Waker>,
    active: Mutex<BTreeSet<(String, String)>>,
    handle: Option<Handle>,
    gh: OsString,
    interval: Duration,
}

impl PrWatches {
    /// Watches that read with the user's `gh` every [`POLL_INTERVAL`].
    pub(crate) fn new() -> Arc<Self> {
        Self::with(OsString::from("gh"), POLL_INTERVAL)
    }

    pub(crate) fn with(gh: OsString, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            waker: OnceLock::new(),
            active: Mutex::default(),
            handle: Handle::try_current().ok(),
            gh,
            interval,
        })
    }

    /// Sets where wake messages go. The first call wins.
    pub(crate) fn set_waker(&self, waker: Waker) {
        let _ = self.waker.set(waker);
    }

    /// The pull requests `session` watches now.
    pub(crate) fn watching(&self, session: &SessionId) -> Vec<String> {
        self.set()
            .iter()
            .filter(|(id, _)| id == session.as_str())
            .map(|(_, pr)| pr.clone())
            .collect()
    }

    fn set(&self) -> std::sync::MutexGuard<'_, BTreeSet<(String, String)>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a watch of `pr` for `session`, reading in `cwd`.
    ///
    /// # Errors
    /// Returns a [`WatchError`] when `pr` names no pull request, the session
    /// already watches it, a limit is reached, or no runtime is running.
    pub(crate) fn start(
        self: &Arc<Self>,
        session: &SessionId,
        cwd: PathBuf,
        pr: &str,
    ) -> Result<(), WatchError> {
        if !is_pull_request_reference(pr) {
            return Err(WatchError::BadReference);
        }
        let handle = self.handle.clone().ok_or(WatchError::NoRuntime)?;
        let key = (session.as_str().to_owned(), pr.to_owned());
        {
            let mut active = self.set();
            if active.contains(&key) {
                return Err(WatchError::Duplicate);
            }
            if active.iter().filter(|(id, _)| *id == key.0).count() >= MAX_WATCHES_PER_SESSION {
                return Err(WatchError::SessionFull);
            }
            if active.len() >= MAX_WATCHES {
                return Err(WatchError::DaemonFull);
            }
            active.insert(key.clone());
        }
        let watches = Arc::clone(self);
        let reader = Arc::new(GhReader::new(self.gh.clone(), cwd, pr.to_owned()));
        let (id, subject) = (session.clone(), subject_of(pr));
        let interval = self.interval;
        handle.spawn(async move {
            let waker = watches.waker.get().cloned();
            run(&reader, &subject, interval, |text| {
                if let Some(waker) = &waker {
                    waker(id.clone(), text);
                }
            })
            .await;
            watches.set().remove(&key);
        });
        Ok(())
    }
}

fn subject_of(pr: &str) -> String {
    if pr.starts_with(URL_PREFIX) {
        pr.to_owned()
    } else {
        format!("#{pr}")
    }
}

/// The `watch_pr` tool of one model session.
pub(crate) struct WatchPrTool {
    watches: Arc<PrWatches>,
    session: SessionId,
    cwd: PathBuf,
}

impl WatchPrTool {
    pub(crate) fn new(watches: Arc<PrWatches>, session: SessionId, cwd: PathBuf) -> Self {
        Self {
            watches,
            session,
            cwd,
        }
    }

    /// The schema the model sees.
    pub(crate) fn schema() -> coda_core::prompt::ToolSchema {
        coda_core::prompt::ToolSchema {
            name: TOOL_NAME.to_owned(),
            description: TOOL_DESCRIPTION.to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pr": {"type": "string", "description": "The pull request number or web address."},
                },
                "required": ["pr"],
                "additionalProperties": false,
            }),
        }
    }
}

impl Tool for WatchPrTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let invalid = |reason: String| ToolError::InvalidInput {
            tool: TOOL_NAME,
            reason,
        };
        let pr = input
            .get("pr")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("`pr` is required".to_owned()))?;
        self.watches
            .start(&self.session, self.cwd.clone(), pr)
            .map_err(|e| invalid(e.to_string()))?;
        Ok(ToolOutput {
            text: format!("watching pull request {pr}; you will be woken when there is news"),
            truncated: false,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    const SESSION: &str = "1";

    fn fake_gh(dir: &TempDir, view: &str) -> OsString {
        let script = dir.path().join("gh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n api) echo '{{\"login\":\"me\"}}';;\n pr) case \"$2\" in view) echo '{view}';; checks) echo '[]';; esac;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.into_os_string()
    }

    fn collect(watches: &Arc<PrWatches>) -> std::sync::mpsc::Receiver<(SessionId, String)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        watches.set_waker(Arc::new(move |session, text| {
            let _ = tx.lock().unwrap().send((session, text));
        }));
        rx
    }

    #[tokio::test]
    async fn a_watch_wakes_its_session_when_the_pull_request_merges_and_then_ends() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let watches = PrWatches::with(fake_gh(&dir, r#"{"state":"MERGED"}"#), Duration::ZERO);
        let woken = collect(&watches);
        let session = SessionId::new(SESSION);

        // Act
        watches.start(&session, dir.path().to_owned(), "7").unwrap();
        let (who, text) = tokio::task::spawn_blocking(move || {
            woken.recv_timeout(Duration::from_secs(30)).unwrap()
        })
        .await
        .unwrap();

        // Assert
        assert_eq!(who, session);
        assert!(text.contains("merged"), "{text}");
    }

    #[tokio::test]
    async fn the_same_pull_request_is_not_watched_twice_by_one_session() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let watches = PrWatches::with(
            fake_gh(&dir, r#"{"state":"OPEN"}"#),
            Duration::from_secs(3600),
        );
        let session = SessionId::new(SESSION);
        watches.start(&session, dir.path().to_owned(), "7").unwrap();

        // Act
        let again = watches.start(&session, dir.path().to_owned(), "7");

        // Assert
        assert_eq!(again, Err(WatchError::Duplicate));
        assert_eq!(watches.watching(&session), ["7"]);
    }

    #[tokio::test]
    async fn a_session_may_watch_only_so_many_pull_requests() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let watches = PrWatches::with(
            fake_gh(&dir, r#"{"state":"OPEN"}"#),
            Duration::from_secs(3600),
        );
        let session = SessionId::new(SESSION);
        for n in 0..MAX_WATCHES_PER_SESSION {
            watches
                .start(&session, dir.path().to_owned(), &n.to_string())
                .unwrap();
        }

        // Act
        let over = watches.start(&session, dir.path().to_owned(), "99");

        // Assert
        assert_eq!(over, Err(WatchError::SessionFull));
    }

    #[tokio::test]
    async fn the_daemon_runs_only_so_many_watches() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let watches = PrWatches::with(
            fake_gh(&dir, r#"{"state":"OPEN"}"#),
            Duration::from_secs(3600),
        );
        for n in 0..MAX_WATCHES {
            let session = SessionId::new(format!("s{}", n / MAX_WATCHES_PER_SESSION));
            watches
                .start(&session, dir.path().to_owned(), &n.to_string())
                .unwrap();
        }

        // Act
        let over = watches.start(&SessionId::new("other"), dir.path().to_owned(), "1");

        // Assert
        assert_eq!(over, Err(WatchError::DaemonFull));
    }

    #[tokio::test]
    async fn a_reference_that_could_be_a_flag_is_refused() {
        // Arrange
        let watches = PrWatches::with(OsString::from("gh"), Duration::ZERO);

        // Act
        let refused = watches.start(&SessionId::new(SESSION), PathBuf::from("."), "--web");

        // Assert
        assert_eq!(refused, Err(WatchError::BadReference));
    }

    #[tokio::test]
    async fn the_tool_starts_a_watch_and_reports_a_refusal_as_invalid_input() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let watches = PrWatches::with(
            fake_gh(&dir, r#"{"state":"OPEN"}"#),
            Duration::from_secs(3600),
        );
        let tool = WatchPrTool::new(
            Arc::clone(&watches),
            SessionId::new(SESSION),
            dir.path().to_owned(),
        );

        // Act
        let started = tool.call(&json!({"pr": "7"})).unwrap();
        let again = tool.call(&json!({"pr": "7"}));
        let missing = tool.call(&json!({}));

        // Assert
        assert!(
            started.text.contains("watching pull request 7"),
            "{}",
            started.text
        );
        assert!(matches!(again, Err(ToolError::InvalidInput { .. })));
        assert!(matches!(missing, Err(ToolError::InvalidInput { .. })));
    }
}
