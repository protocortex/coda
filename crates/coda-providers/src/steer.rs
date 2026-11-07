// SPDX-License-Identifier: Apache-2.0

//! Adds a user message to a turn that is already running.

use tokio::sync::mpsc;

/// Hands a message to the running turn of one session from another task. The
/// session reads it between its own steps and passes it to the backend.
#[derive(Debug, Clone)]
pub struct SteerHandle(mpsc::UnboundedSender<String>);

/// The session's end of a [`SteerHandle`].
pub(crate) type SteerInbox = mpsc::UnboundedReceiver<String>;

impl SteerHandle {
    /// Queues `text` for the running turn. Returns `false` when the session is gone.
    #[must_use]
    pub fn steer(&self, text: &str) -> bool {
        self.0.send(text.to_owned()).is_ok()
    }
}

pub(crate) fn channel() -> (SteerHandle, SteerInbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    (SteerHandle(tx), rx)
}

/// Drops what was queued before a turn started: a steer is for the turn that
/// was running when it was sent.
pub(crate) fn clear(inbox: &mut SteerInbox) {
    while inbox.try_recv().is_ok() {}
}
