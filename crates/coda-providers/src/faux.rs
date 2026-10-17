// SPDX-License-Identifier: AGPL-3.0-only

//! A scripted, deterministic model provider for tests and offline runs.

use std::collections::VecDeque;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use coda_config::config::Config;
use serde::Deserialize;

use crate::model::{ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError};

const CONFIG_KEY: &str = "faux";

/// Largest script file accepted, in bytes.
pub const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

/// One scripted response: events to stream, then an optional failure.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// Events delivered in order.
    pub events: Vec<ModelEvent>,
    /// The failure delivered after the events, if any.
    #[serde(default)]
    pub error: Option<ProviderError>,
}

/// Why a faux provider could not be built.
#[derive(Debug, thiserror::Error)]
pub enum FauxError {
    /// `providers.faux.script` is not configured.
    #[error("providers.faux.script is not set; point it at a script file")]
    NoScript,
    /// The script file could not be read.
    #[error("cannot read faux script {}: {source}", .path.display())]
    Read {
        /// The script path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The script file is larger than [`MAX_SCRIPT_BYTES`].
    #[error("faux script {} is larger than {MAX_SCRIPT_BYTES} bytes", .path.display())]
    TooLarge {
        /// The script path.
        path: PathBuf,
    },
    /// The script file is not a valid list of turns.
    #[error("faux script {} is invalid: {source}", .path.display())]
    Parse {
        /// The script path.
        path: PathBuf,
        /// The underlying error.
        source: serde_json::Error,
    },
}

/// Replays scripted turns, one per [`stream`](ModelProvider::stream) call.
#[derive(Debug)]
pub struct FauxProvider {
    turns: Mutex<VecDeque<Turn>>,
}

impl FauxProvider {
    /// Builds a provider that replays `turns` in order.
    #[must_use]
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }

    /// Builds a provider from the script named by `providers.faux.script`.
    ///
    /// # Errors
    /// Returns [`FauxError::NoScript`] when no script is configured, or the
    /// errors of [`FauxProvider::from_script_file`].
    pub fn from_config(config: &Config) -> Result<Self, FauxError> {
        let script = config
            .providers
            .get(CONFIG_KEY)
            .and_then(|section| section.script.as_deref())
            .ok_or(FauxError::NoScript)?;
        Self::from_script_file(script)
    }

    /// Builds a provider from a JSON file holding a list of turns.
    ///
    /// # Errors
    /// Returns [`FauxError::Read`], [`FauxError::TooLarge`] or
    /// [`FauxError::Parse`].
    pub fn from_script_file(path: &Path) -> Result<Self, FauxError> {
        let read_error = |source| FauxError::Read {
            path: path.to_owned(),
            source,
        };
        let mut text = String::new();
        std::fs::File::open(path)
            .map_err(read_error)?
            .take(MAX_SCRIPT_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(read_error)?;
        if text.len() as u64 > MAX_SCRIPT_BYTES {
            return Err(FauxError::TooLarge {
                path: path.to_owned(),
            });
        }
        let turns = serde_json::from_str(&text).map_err(|source| FauxError::Parse {
            path: path.to_owned(),
            source,
        })?;
        Ok(Self::new(turns))
    }

    fn next_turn(&self) -> Result<Turn, ProviderError> {
        let fatal = |message: &str| ProviderError::Fatal {
            message: message.to_owned(),
        };
        self.turns
            .lock()
            .map_err(|_| fatal("the faux script state is poisoned by an earlier panic"))?
            .pop_front()
            .ok_or_else(|| fatal("the faux script has no turn left"))
    }
}

impl FauxProvider {
    fn open(&self) -> Result<ModelStream, ProviderError> {
        let turn = self.next_turn()?;
        let (sender, stream) = ModelStream::channel(turn.events.len() + 1);
        let items = turn.events.into_iter().map(Ok).chain(turn.error.map(Err));
        for item in items {
            // The buffer holds every item, so this only fails if the stream
            // was dropped, which no caller can observe yet.
            if sender.try_send(item).is_err() {
                break;
            }
        }
        Ok(stream)
    }
}

impl ModelProvider for FauxProvider {
    fn stream(
        &self,
        _request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        std::future::ready(self.open())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poisoned_script_state_is_reported_as_poisoned_not_exhausted() {
        // Arrange
        let provider = FauxProvider::new(vec![Turn {
            events: Vec::new(),
            error: None,
        }]);
        let poisoned = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = provider.turns.lock().expect("lock");
                    panic!("poison the script state");
                })
                .join()
        });
        assert!(poisoned.is_err());

        // Act
        let result = provider
            .stream(ModelRequest {
                prompt: String::new(),
            })
            .await;

        // Assert
        let Err(ProviderError::Fatal { message }) = result else {
            panic!("expected a fatal error");
        };
        assert!(message.contains("poisoned"), "{message}");
    }
}
