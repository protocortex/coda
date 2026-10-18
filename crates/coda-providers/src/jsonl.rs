// SPDX-License-Identifier: AGPL-3.0-only

//! Newline framing and diagnostics retention shared by the JSONL stream parsers.

use crate::claude_code::Diagnostic;

const NEWLINE: u8 = b'\n';
/// Longest line buffered; a longer one is discarded.
pub(crate) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Diagnostics kept; further ones are only counted.
pub(crate) const MAX_DIAGNOSTICS: usize = 256;
/// Longest stream-supplied string kept in a diagnostic, in characters.
pub(crate) const MAX_KIND_CHARS: usize = 64;

/// One framed unit of the stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Framed {
    /// A complete line, without its newline.
    Line {
        /// 1-based line number in the stream.
        number: u64,
        bytes: Vec<u8>,
    },
    /// A line over the size limit; its content was dropped.
    TooLong {
        /// 1-based line number in the stream.
        number: u64,
    },
}

/// Splits raw output bytes into lines, holding a partial line between reads.
#[derive(Debug, Default)]
pub(crate) struct LineFramer {
    /// Start of the current incomplete line; empty between lines.
    pending: Vec<u8>,
    /// Skipping the rest of a line that exceeded the limit.
    discarding: bool,
    lines_seen: u64,
}

impl LineFramer {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<Framed> {
        let mut framed = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let newline = rest.iter().position(|byte| *byte == NEWLINE);
            let (segment, tail) = match newline {
                Some(end) => (&rest[..end], &rest[end + 1..]),
                None => (rest, &rest[rest.len()..]),
            };
            rest = tail;
            if self.discarding {
                if newline.is_some() {
                    self.discarding = false;
                    self.lines_seen += 1;
                }
                continue;
            }
            if self.pending.len() + segment.len() > MAX_LINE_BYTES {
                framed.push(Framed::TooLong {
                    number: self.lines_seen + 1,
                });
                self.pending.clear();
                self.discarding = newline.is_none();
                if newline.is_some() {
                    self.lines_seen += 1;
                }
                continue;
            }
            if newline.is_none() {
                self.pending.extend_from_slice(segment);
                continue;
            }
            self.lines_seen += 1;
            self.pending.extend_from_slice(segment);
            framed.push(Framed::Line {
                number: self.lines_seen,
                bytes: std::mem::take(&mut self.pending),
            });
        }
        framed
    }
}

/// Truncates a stream-supplied string before it enters a diagnostic.
pub(crate) fn bounded(text: &str) -> String {
    text.chars().take(MAX_KIND_CHARS).collect()
}

/// Keeps the first [`MAX_DIAGNOSTICS`] diagnostics and counts the rest.
#[derive(Debug, Default)]
pub(crate) struct DiagnosticLog {
    items: Vec<Diagnostic>,
    dropped: u64,
}

impl DiagnosticLog {
    pub(crate) fn record(&mut self, diagnostic: Diagnostic) {
        if self.items.len() < MAX_DIAGNOSTICS {
            self.items.push(diagnostic);
        } else {
            self.dropped += 1;
        }
    }

    pub(crate) fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}
