// SPDX-License-Identifier: AGPL-3.0-only

//! Token economy: shrink large tool output and keep the original retrievable.

pub mod compress;

use coda_protocol::types::{OriginalId, SessionId};
use coda_store::{MAX_ORIGINAL_BYTES, SavingRecord, Store, StoreError};

/// Bytes kept from each end of an output too large to store.
const OVERSIZE_KEEP_BYTES: usize = 4 * 1024;

/// The shape of a tool output, which picks its compressor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Log,
    Diff,
    JsonArray,
}

/// Why shrinking an output failed.
#[derive(Debug, thiserror::Error)]
pub enum EconomyError {
    /// Storing the original or recording the saving failed.
    #[error("the token economy store failed: {0}")]
    Store(#[from] StoreError),
}

/// A tool output after shrinking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The text the model sees.
    pub text: String,
    /// Where the full output was kept, when the text is a shrunk version of it.
    pub original: Option<OriginalId>,
}

/// Picks the compressor that suits `text`.
#[must_use]
pub fn detect(text: &str) -> Kind {
    if is_diff(text) {
        Kind::Diff
    } else if text.trim_start().starts_with('[') {
        Kind::JsonArray
    } else {
        Kind::Log
    }
}

/// A diff names a file pair or has a hunk header; a lone leading `--- ` line
/// is more often a banner.
fn is_diff(text: &str) -> bool {
    if text.starts_with("diff --git") {
        return true;
    }
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("@@ ")
            || (line.starts_with("--- ") && lines.peek().is_some_and(|n| n.starts_with("+++ ")))
        {
            return true;
        }
    }
    false
}

fn shrink(raw: &[u8]) -> String {
    match detect(&String::from_utf8_lossy(raw)) {
        Kind::Diff => compress::compress_diff(raw),
        Kind::JsonArray => {
            compress::compress_json_array(raw).unwrap_or_else(|| compress::compress_log(raw))
        }
        Kind::Log => compress::compress_log(raw),
    }
}

/// Shrinks `raw`, keeps the original under `session` and records the saving.
///
/// Output over the store's size limit is capped to its head and tail and
/// has no retrieve id.
///
/// Output that does not get smaller, footer included, passes through
/// untouched and leaves no trace. Invalid UTF-8 is replaced with U+FFFD in
/// both the shrunk text and the stored original.
///
/// # Errors
///
/// Returns [`EconomyError::Store`] when the original or the saving cannot be stored.
pub fn compact(
    store: &mut Store,
    session: &SessionId,
    raw: &[u8],
) -> Result<Compacted, EconomyError> {
    let shrunk = shrink(raw);
    if raw.len() > MAX_ORIGINAL_BYTES {
        return record_oversize(store, session, raw.len(), &shrunk);
    }
    // The footer names the id, which is unknown until the original is stored,
    // so the size check uses the longest footer this output could get.
    let footer_bound = footer(raw.len(), &OriginalId::new(i64::MAX.to_string())).len();
    if shrunk.len() + footer_bound >= raw.len() {
        return Ok(Compacted {
            text: String::from_utf8_lossy(raw).into_owned(),
            original: None,
        });
    }
    let id = store.store_original(session, &String::from_utf8_lossy(raw))?;
    let mut text = shrunk;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&footer(raw.len(), &id));
    store.record_saving(&SavingRecord {
        session: session.clone(),
        original_bytes: byte_count(raw.len()),
        compressed_bytes: byte_count(text.len()),
    })?;
    Ok(Compacted {
        text,
        original: Some(id),
    })
}

/// Output too large to keep: cap what the model sees and give no retrieve id.
fn record_oversize(
    store: &mut Store,
    session: &SessionId,
    original_bytes: usize,
    shrunk: &str,
) -> Result<Compacted, EconomyError> {
    let text = format!(
        "{}\n[compressed from {original_bytes} bytes, original too large to keep]",
        cap_ends(shrunk)
    );
    store.record_saving(&SavingRecord {
        session: session.clone(),
        original_bytes: byte_count(original_bytes),
        compressed_bytes: byte_count(text.len()),
    })?;
    Ok(Compacted {
        text,
        original: None,
    })
}

/// Keeps the first and last [`OVERSIZE_KEEP_BYTES`] of `text`, cut on char boundaries.
pub(crate) fn cap_ends(text: &str) -> String {
    if text.len() <= 2 * OVERSIZE_KEEP_BYTES {
        return text.to_owned();
    }
    let mut head = OVERSIZE_KEEP_BYTES;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - OVERSIZE_KEEP_BYTES;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let omitted = tail - head;
    format!(
        "{}\n{}\n{}",
        &text[..head],
        compress::marker(omitted, "bytes"),
        &text[tail..]
    )
}

fn byte_count(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn footer(original_bytes: usize, id: &OriginalId) -> String {
    format!("[compressed from {original_bytes} bytes, full output: retrieve id {id}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::readers::{OriginalsReader, ReaderError};
    use coda_protocol::types::{BackendKind, Mode};
    use coda_store::NewSession;

    const LOG_220: &str = "aaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\n\
aaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\n\
aaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\n\
aaaaaaaaaa\n";
    const LOG_220_SHRUNK: &str = "aaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\n[... 14 lines omitted]\n\
aaaaaaaaaa\naaaaaaaaaa\naaaaaaaaaa\n[compressed from 220 bytes, full output: retrieve id 1]";

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("d")).unwrap();
        (dir, store)
    }

    fn session(store: &mut Store) -> SessionId {
        store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd: "/r".to_owned(),
                mode: Mode::Default,
            })
            .unwrap()
    }

    #[test]
    fn detect_picks_the_compressor_from_the_text() {
        // Arrange
        let cases = [
            ("diff --git a/f b/f\n", Kind::Diff),
            ("--- a/f\n+++ b/f\n", Kind::Diff),
            ("noise\n@@ -1 +1 @@\n-a\n+b\n", Kind::Diff),
            ("--- test run ---\nok\nok\n", Kind::Log),
            ("--- only a dash line\n", Kind::Log),
            ("  [1, 2]", Kind::JsonArray),
            ("{\"a\": 1}", Kind::Log),
            ("plain output", Kind::Log),
            ("", Kind::Log),
        ];

        // Act
        let found: Vec<Kind> = cases.iter().map(|(text, _)| detect(text)).collect();

        // Assert
        let expected: Vec<Kind> = cases.iter().map(|(_, kind)| *kind).collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_large_output_is_shrunk_stored_and_counted() {
        // Arrange
        assert_eq!(LOG_220.len(), 220);
        let (_dir, mut store) = open();
        let one = session(&mut store);

        // Act
        let done = compact(&mut store, &one, LOG_220.as_bytes()).unwrap();

        // Assert
        assert_eq!(done.text, LOG_220_SHRUNK);
        let id = done.original.unwrap();
        let kept = store.originals_reader(&one).fetch_original(&id).unwrap();
        assert_eq!(kept, LOG_220);
        // 220 original bytes minus 144 shown (89 of log, 55 of footer).
        assert_eq!(store.saved_bytes(Some(&one)).unwrap(), 76);
    }

    #[test]
    fn an_original_cannot_be_read_from_another_session() {
        // Arrange
        let (_dir, mut store) = open();
        let one = session(&mut store);
        let two = session(&mut store);
        let id = compact(&mut store, &one, LOG_220.as_bytes())
            .unwrap()
            .original
            .unwrap();

        // Act
        let err = store
            .originals_reader(&two)
            .fetch_original(&id)
            .unwrap_err();

        // Assert
        assert!(matches!(err, ReaderError::NotFound(_)));
        assert_eq!(store.saved_bytes(Some(&two)).unwrap(), 0);
    }

    #[test]
    fn a_small_output_passes_through_without_a_trace() {
        // Arrange
        let (_dir, mut store) = open();
        let one = session(&mut store);

        // Act
        let done = compact(&mut store, &one, b"short\n").unwrap();

        // Assert
        assert_eq!(
            done,
            Compacted {
                text: "short\n".to_owned(),
                original: None
            }
        );
        assert_eq!(store.saved_bytes(None).unwrap(), 0);
    }

    #[test]
    fn an_output_too_large_to_keep_is_capped_and_not_retrievable() {
        // Arrange
        let (_dir, mut store) = open();
        let one = session(&mut store);
        let raw = "y".repeat(coda_store::MAX_ORIGINAL_BYTES + 1);

        // Act
        let done = compact(&mut store, &one, raw.as_bytes()).unwrap();

        // Assert
        assert_eq!(done.original, None);
        assert!(done.text.len() < raw.len() / 100);
        assert!(done.text.contains("too large to keep"));
        assert!(done.text.starts_with("yyyy"));
        assert!(done.text.contains("[... "));
    }
}
