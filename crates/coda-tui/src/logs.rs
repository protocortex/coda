// SPDX-License-Identifier: AGPL-3.0-only

//! Holds log output in memory while the terminal is in raw mode.
//!
//! Writing log lines onto the alternate screen corrupts the UI, so the UI
//! captures them here and prints them once the terminal is restored.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::{Dispatch, Level};

/// Log output past this size is dropped rather than growing without bound.
const MAX_BUFFERED_BYTES: usize = 256 * 1024;

#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    /// A dispatcher that writes plain log lines into this buffer.
    #[must_use]
    pub fn dispatch(&self) -> Dispatch {
        let buffer = self.clone();
        Dispatch::new(
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_ansi(false)
                .with_writer(move || buffer.clone())
                .finish(),
        )
    }

    /// Takes everything captured so far.
    #[must_use]
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut captured = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let room = MAX_BUFFERED_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buf[..buf.len().min(room)]);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_captured_instead_of_printed() {
        // Arrange
        let buffer = LogBuffer::default();

        // Act
        tracing::dispatcher::with_default(&buffer.dispatch(), || {
            tracing::warn!("link flickered");
        });

        // Assert
        let captured = String::from_utf8(buffer.take()).unwrap();
        assert!(captured.contains("link flickered"), "{captured}");
        assert_eq!(buffer.take().len(), 0);
    }

    #[test]
    fn output_beyond_the_cap_is_dropped() {
        // Arrange
        let mut buffer = LogBuffer::default();
        let chunk = vec![b'x'; MAX_BUFFERED_BYTES];

        // Act
        buffer.write_all(&chunk).unwrap();
        buffer.write_all(&chunk).unwrap();

        // Assert
        assert_eq!(buffer.take().len(), MAX_BUFFERED_BYTES);
    }
}
