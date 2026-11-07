// SPDX-License-Identifier: Apache-2.0

//! Messages the user adds to a send that is already running.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

/// Holds the messages a client steered into a running send until the loop
/// reaches its next step boundary and adds them to the conversation.
#[derive(Debug, Clone, Default)]
pub struct SteerQueue {
    waiting: Arc<Mutex<VecDeque<String>>>,
}

impl SteerQueue {
    /// Adds `text` behind the messages already waiting.
    pub fn push(&self, text: String) {
        self.lock().push_back(text);
    }

    /// Whether a message waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Takes every waiting message, oldest first. A send that ends takes what
    /// its loop never reached, so the caller can start it as a turn of its own.
    #[must_use]
    pub fn take(&self) -> Vec<String> {
        self.lock().drain(..).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
