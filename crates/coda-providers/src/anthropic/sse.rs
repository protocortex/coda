// SPDX-License-Identifier: AGPL-3.0-only

//! Server-sent event framing.

/// Largest event accepted, in bytes.
pub const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// Why the byte stream is not valid SSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    /// An event grew past [`MAX_EVENT_BYTES`].
    #[error("an event is larger than {MAX_EVENT_BYTES} bytes")]
    EventTooLarge,
    /// An event is not UTF-8.
    #[error("an event is not valid UTF-8")]
    InvalidUtf8,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Splits a byte stream into event data payloads.
///
/// Lines end in LF, CRLF or a lone CR. They are decoded only once complete,
/// so a multi-byte character split across chunks is never cut.
#[derive(Debug, Default)]
pub struct SseParser {
    pending: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    bom_checked: bool,
}

impl SseParser {
    /// Feeds bytes and returns the payloads of the events they complete.
    ///
    /// # Errors
    /// Returns an [`SseError`] when the stream is malformed.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseError> {
        self.pending.extend_from_slice(bytes);
        let events = self.drain_lines(false)?;
        if self.pending.len() + self.data_bytes > MAX_EVENT_BYTES {
            return Err(SseError::EventTooLarge);
        }
        Ok(events)
    }

    /// Ends the stream and returns the event an unterminated tail completes.
    ///
    /// # Errors
    /// Returns an [`SseError`] when the stream is malformed.
    pub fn finish(&mut self) -> Result<Vec<String>, SseError> {
        let mut events = self.drain_lines(true)?;
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.read_line(&line, &mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn drain_lines(&mut self, at_end: bool) -> Result<Vec<String>, SseError> {
        let mut events = Vec::new();
        if !self.bom_checked {
            if !at_end && self.pending.len() < BOM.len() && BOM.starts_with(&self.pending) {
                return Ok(events);
            }
            if self.pending.starts_with(BOM) {
                self.pending.drain(..BOM.len());
            }
            self.bom_checked = true;
        }
        while let Some(end) = self
            .pending
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
        {
            let last = end + 1 == self.pending.len();
            // A trailing CR may be the first half of a CRLF; wait for more.
            if self.pending[end] == b'\r' && last && !at_end {
                break;
            }
            let crlf = self.pending[end] == b'\r' && self.pending.get(end + 1) == Some(&b'\n');
            let line: Vec<u8> = self.pending.drain(..end + 1 + usize::from(crlf)).collect();
            self.read_line(&line[..end], &mut events)?;
        }
        Ok(events)
    }

    fn read_line(&mut self, line: &[u8], events: &mut Vec<String>) -> Result<(), SseError> {
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        let line = std::str::from_utf8(line).map_err(|_| SseError::InvalidUtf8)?;
        let Some((field, value)) = line.split_once(':') else {
            return Ok(());
        };
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.data_bytes += value.len();
            self.data.push(value.to_owned());
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
            self.data_bytes = 0;
        }
    }
}
