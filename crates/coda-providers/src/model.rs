// SPDX-License-Identifier: AGPL-3.0-only

//! The streaming model-provider trait and the clock and sleep seams that
//! idle-timeout and retry logic share.

use std::future::Future;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// One request to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    /// The user prompt.
    pub prompt: String,
}

/// Token counts reported by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Tokens read from the request.
    pub input_tokens: u64,
    /// Tokens generated in the response.
    pub output_tokens: u64,
}

/// Why a model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model wants its tool calls run.
    ToolUse,
    /// The output limit was reached.
    MaxTokens,
}

/// One item of a model response stream.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelEvent {
    /// A piece of answer text.
    TextDelta {
        /// The text piece.
        text: String,
    },
    /// A tool call began; its arguments follow as fragments.
    ToolCallStart {
        /// Identifies the call across its fragments.
        call_id: String,
        /// The tool being called.
        name: String,
    },
    /// A piece of a tool call's JSON arguments.
    ToolCallArgumentsDelta {
        /// The call the fragment belongs to.
        call_id: String,
        /// The raw JSON text piece; only the concatenation is valid JSON.
        fragment: String,
    },
    /// Token usage so far.
    Usage(Usage),
    /// The last event of a successful stream.
    Stop {
        /// Why generation ended.
        reason: StopReason,
    },
}

/// A failed model call, classified by what the caller should do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderError {
    /// The request does not fit the model's context window; compact and retry.
    #[error("the request exceeds the model context window")]
    ContextWindowExceeded,
    /// A transient failure; retrying later may succeed.
    #[error("retryable provider error: {message}")]
    Retryable {
        /// What went wrong.
        message: String,
    },
    /// A failure that retrying cannot fix.
    #[error("fatal provider error: {message}")]
    Fatal {
        /// What went wrong.
        message: String,
    },
}

impl ProviderError {
    /// Whether the same request may succeed if sent again unchanged.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }
}

/// The result of one stream item.
pub type StreamItem = Result<ModelEvent, ProviderError>;

/// The receiving end of a response stream. Dropping it cancels the request:
/// a task started with [`ModelStream::spawn`] is aborted.
#[derive(Debug)]
pub struct ModelStream {
    receiver: mpsc::Receiver<StreamItem>,
    task: Option<JoinHandle<()>>,
}

impl Drop for ModelStream {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl ModelStream {
    /// Creates a stream and the sender that feeds it. The stream ends once
    /// the sender is dropped and the buffer is drained.
    #[must_use]
    pub fn channel(capacity: usize) -> (mpsc::Sender<StreamItem>, Self) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            sender,
            Self {
                receiver,
                task: None,
            },
        )
    }

    /// Creates a stream fed by a background task running `producer`. The task
    /// is aborted when the stream is dropped, so abandoning a stream stops the
    /// work behind it.
    #[must_use]
    pub fn spawn<F, Fut>(capacity: usize, producer: F) -> Self
    where
        F: FnOnce(mpsc::Sender<StreamItem>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (sender, mut stream) = Self::channel(capacity);
        stream.task = Some(tokio::spawn(producer(sender)));
        stream
    }

    /// The next item, or `None` when the stream has ended.
    pub async fn next(&mut self) -> Option<StreamItem> {
        self.receiver.recv().await
    }
}

/// A model that answers a request as a stream of events.
pub trait ModelProvider: Send + Sync {
    /// Starts a response stream.
    ///
    /// A successful stream yields events, exactly one [`ModelEvent::Usage`],
    /// and ends after exactly one [`ModelEvent::Stop`]. A failure is either returned here or delivered
    /// as the last stream item.
    ///
    /// Dropping the returned stream cancels the request: any background work
    /// feeding it must stop. [`ModelStream::spawn`] gives that for free.
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send;
}

/// Source of the current instant, injectable so tests control time.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// Waits for a duration, injectable so tests do not really wait.
pub trait Sleeper: Send + Sync {
    /// Resolves after `duration`.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The real sleeper, backed by the tokio timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_retryable_errors_are_retryable() {
        // Arrange
        let cases = [
            (ProviderError::ContextWindowExceeded, false),
            (
                ProviderError::Fatal {
                    message: "x".into(),
                },
                false,
            ),
            (
                ProviderError::Retryable {
                    message: "x".into(),
                },
                true,
            ),
        ];

        for (error, expected) in cases {
            // Act
            let retryable = error.is_retryable();

            // Assert
            assert_eq!(retryable, expected, "{error}");
        }
    }

    #[test]
    fn system_clock_never_goes_backwards() {
        // Arrange
        let clock = SystemClock;

        // Act
        let first = clock.now();
        let second = clock.now();

        // Assert
        assert!(second >= first);
    }

    #[tokio::test]
    async fn tokio_sleeper_waits_at_least_the_duration() {
        // Arrange
        let started = Instant::now();
        let pause = Duration::from_millis(10);

        // Act
        TokioSleeper.sleep(pause).await;

        // Assert
        assert!(started.elapsed() >= pause);
    }
}
