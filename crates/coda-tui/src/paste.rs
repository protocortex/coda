// SPDX-License-Identifier: Apache-2.0

//! Cleans pasted text and keeps a large paste in a file the agent can read.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A paste of this many bytes or more goes to a file instead of the prompt.
pub const LARGE_PASTE_BYTES: usize = 32 * 1024;
/// Where the Coda state of a session's folder lives.
const CODA_DIR: &str = ".coda";
/// Where large pastes are kept, under [`CODA_DIR`].
const PASTE_SUBDIR: &str = "pastes";
const PASTE_PREFIX: &str = "paste-";
const PASTE_FILE_MODE: u32 = 0o600;
const PASTE_DIR_MODE: u32 = 0o700;
/// Keeps git from listing, adding or snapshotting the pastes.
const IGNORE_FILE: &str = ".gitignore";
const IGNORE_ALL: &str = "*\n";
/// How long a saved paste stays before the next save removes it.
const KEEP_PASTES: Duration = Duration::from_hours(7 * 24);
const TAB_AS_SPACES: &str = "    ";
const BYTES_PER_KIB: usize = 1024;
const ZERO_WIDTH_NON_JOINER: char = '\u{200c}';
const ZERO_WIDTH_JOINER: char = '\u{200d}';
static PASTE_COUNT: AtomicU64 = AtomicU64::new(0);

/// What a paste holds once it is safe to show and send.
#[derive(Debug, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    /// How many characters that draw nothing were taken out.
    pub invisible: usize,
}

/// Whether `ch` draws nothing but changes what a reader or a model sees: zero
/// width space and direction marks and overrides, the byte order mark, the soft
/// hyphen, the fillers and the tag characters. The joiners are judged by what
/// surrounds them, see [`is_joiner`].
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{ad}'
            | '\u{61c}'
            | '\u{115f}'
            | '\u{1160}'
            | '\u{180e}'
            | '\u{200b}'
            | '\u{200e}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{3164}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// The zero width joiner and non-joiner, which emoji sequences and some scripts need.
fn is_joiner(ch: char) -> bool {
    ch == ZERO_WIDTH_JOINER || ch == ZERO_WIDTH_NON_JOINER
}

/// Whether `ch` shows as itself, so a joiner next to it has something to join.
fn is_shown(ch: char) -> bool {
    !(ch.is_whitespace() || ch.is_control() || is_invisible(ch) || is_joiner(ch))
}

fn clean_with(pasted: &str, tab: &str) -> Cleaned {
    let unified = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let chars: Vec<char> = unified.chars().collect();
    let mut text = String::with_capacity(unified.len());
    let mut invisible = 0;
    for (at, &ch) in chars.iter().enumerate() {
        match ch {
            '\n' | '\u{2028}' | '\u{2029}' => text.push('\n'),
            '\t' => text.push_str(tab),
            ch if is_joiner(ch) => {
                let between = at > 0
                    && chars.get(at + 1).is_some_and(|next| is_shown(*next))
                    && is_shown(chars[at - 1]);
                if between {
                    text.push(ch);
                } else {
                    invisible += 1;
                }
            }
            ch if is_invisible(ch) => invisible += 1,
            ch if ch.is_control() => {}
            ch => text.push(ch),
        }
    }
    text.truncate(text.trim_end_matches('\n').len());
    Cleaned { text, invisible }
}

/// Makes pasted text fit the prompt: every line break becomes a newline, tabs
/// become spaces, other control characters and invisible ones are dropped, and
/// the newlines at the end go, so the paste cannot submit by itself. A joiner
/// stays when it sits between two shown characters, as in an emoji sequence or
/// a Persian word, and goes anywhere else.
#[must_use]
pub fn clean(pasted: &str) -> Cleaned {
    clean_with(pasted, TAB_AS_SPACES)
}

/// [`clean`] for the text of a file, where a tab stays a tab.
#[must_use]
pub fn clean_for_file(pasted: &str) -> Cleaned {
    clean_with(pasted, "\t")
}

/// The warning shown when `count` invisible characters were taken out of a paste.
#[must_use]
pub fn invisible_warning(count: usize) -> String {
    let noun = if count == 1 {
        "character"
    } else {
        "characters"
    };
    format!("removed {count} invisible {noun} from the paste")
}

fn refuse_link(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symbolic link", path.display()),
        )),
        _ => Ok(()),
    }
}

/// Removes saved pastes older than [`KEEP_PASTES`]. A failure leaves the file.
fn prune(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_PASTES);
        if old
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(PASTE_PREFIX)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Writes `text` under `folder` in a new file and returns its path relative to
/// `folder`. The paste folder is private, ignores its own contents in git and
/// loses pastes older than a week. A symbolic link in its path is refused.
///
/// # Errors
/// Returns the I/O error when the folder or the file cannot be made.
pub fn save(folder: &Path, text: &str) -> io::Result<PathBuf> {
    let coda = folder.join(CODA_DIR);
    let pastes = coda.join(PASTE_SUBDIR);
    refuse_link(&coda)?;
    refuse_link(&pastes)?;
    DirBuilder::new()
        .recursive(true)
        .mode(PASTE_DIR_MODE)
        .create(&pastes)?;
    let ignore = pastes.join(IGNORE_FILE);
    if !ignore.exists() {
        fs::write(&ignore, IGNORE_ALL)?;
    }
    prune(&pastes);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let count = PASTE_COUNT.fetch_add(1, Ordering::Relaxed);
    let name = format!("{PASTE_PREFIX}{millis}-{count}.txt");
    let full = pastes.join(&name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PASTE_FILE_MODE)
        .open(&full)?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        let _ = fs::remove_file(&full);
        return Err(error);
    }
    Ok(Path::new(CODA_DIR).join(PASTE_SUBDIR).join(name))
}

/// The text that stands for a saved paste in the prompt.
#[must_use]
pub fn reference(relative: &Path, bytes: usize) -> String {
    format!(
        "[pasted text of {} KiB, saved at {}: read the whole file, in pieces if it is long]",
        bytes.div_ceil(BYTES_PER_KIB),
        relative.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_breaks_become_newlines_and_tabs_become_spaces() {
        // Arrange
        let pasted = "one\ntwo\r\nthree\rfour\tfive";

        // Act
        let cleaned = clean(pasted);

        // Assert
        assert_eq!(cleaned.text, "one\ntwo\nthree\nfour    five");
        assert_eq!(cleaned.invisible, 0);
    }

    #[test]
    fn newlines_at_the_end_are_dropped_so_a_paste_never_submits() {
        // Arrange / Act
        let cleaned = clean("first\nsecond\n\r\n");

        // Assert
        assert_eq!(cleaned.text, "first\nsecond");
    }

    #[test]
    fn control_characters_are_dropped() {
        // Arrange / Act
        let cleaned = clean("a\x1b[31mb\x07\u{9b}c\x7f");

        // Assert
        assert_eq!(cleaned.text, "a[31mbc");
        assert_eq!(cleaned.invisible, 0);
    }

    #[test]
    fn invisible_characters_are_dropped_and_counted() {
        // Arrange
        let pasted = "ig\u{200b}nore\u{202e} this\u{feff}\u{e0041}\u{ad}";

        // Act
        let cleaned = clean(pasted);

        // Assert
        assert_eq!(cleaned.text, "ignore this");
        assert_eq!(cleaned.invisible, 5);
    }

    #[test]
    fn visible_non_ascii_text_stays() {
        // Arrange
        let pasted = "caf\u{e9} \u{4e2d}\u{6587} \u{1f600}";

        // Act
        let cleaned = clean(pasted);

        // Assert
        assert_eq!(cleaned.text, pasted);
        assert_eq!(cleaned.invisible, 0);
    }

    #[test]
    fn a_joiner_between_letters_or_emoji_stays_and_a_hidden_run_goes() {
        // Arrange
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let persian = "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";
        let hidden = "a\u{200d}\u{200c}\u{200d}b";
        let edges = " \u{200d}x\u{200c} y";

        // Act
        let (family, persian, hidden, edges) =
            (clean(family), clean(persian), clean(hidden), clean(edges));

        // Assert
        assert_eq!(family.invisible, 0);
        assert_eq!(persian.invisible, 0);
        assert_eq!(
            persian.text,
            "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}"
        );
        assert_eq!((hidden.text.as_str(), hidden.invisible), ("ab", 3));
        assert_eq!((edges.text.as_str(), edges.invisible), (" x y", 2));
    }

    #[test]
    fn fillers_and_the_arabic_letter_mark_are_invisible_and_line_separators_are_newlines() {
        // Arrange
        let pasted = "a\u{61c}b\u{3164}c\u{ffa0}d\u{115f}e\u{1160}f\u{180e}g\u{2028}h\u{2029}i";

        // Act
        let cleaned = clean(pasted);

        // Assert
        assert_eq!(cleaned.text, "abcdefg\nh\ni");
        assert_eq!(cleaned.invisible, 6);
    }

    #[test]
    fn the_file_text_keeps_its_tabs() {
        // Arrange
        let pasted = "all:\r\n\tcc -o x\u{200b} x.c\r\n";

        // Act
        let cleaned = clean_for_file(pasted);

        // Assert
        assert_eq!(cleaned.text, "all:\n\tcc -o x x.c");
        assert_eq!(cleaned.invisible, 1);
    }

    #[test]
    fn the_warning_counts_what_was_removed() {
        // Arrange / Act / Assert
        assert_eq!(
            invisible_warning(1),
            "removed 1 invisible character from the paste"
        );
        assert_eq!(
            invisible_warning(3),
            "removed 3 invisible characters from the paste"
        );
    }

    #[test]
    fn a_saved_paste_is_a_private_file_under_the_folder_and_each_gets_its_own() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();

        // Act
        let first = save(dir.path(), "alpha").unwrap();
        let second = save(dir.path(), "beta").unwrap();

        // Assert
        assert_ne!(first, second);
        assert!(first.starts_with(".coda/pastes"));
        assert_eq!(
            fs::read_to_string(dir.path().join(&first)).unwrap(),
            "alpha"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(&second)).unwrap(),
            "beta"
        );
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(dir.path().join(&first))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode, PASTE_FILE_MODE);
    }

    #[test]
    fn the_paste_folder_is_private_and_ignores_everything_in_it() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();

        // Act
        let relative = save(dir.path(), "secret").unwrap();

        // Assert
        let folder = dir.path().join(&relative);
        let folder = folder.parent().unwrap();
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(folder).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode, PASTE_DIR_MODE);
        assert_eq!(
            fs::read_to_string(folder.join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn a_symlinked_paste_folder_is_refused() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".coda")).unwrap();

        // Act
        let saved = save(dir.path(), "text");

        // Assert
        assert!(saved.is_err());
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn saving_removes_pastes_older_than_the_keep_time_and_keeps_newer_ones() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let old = save(dir.path(), "old").unwrap();
        let recent = save(dir.path(), "recent").unwrap();
        let aged = SystemTime::now() - KEEP_PASTES - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(dir.path().join(&old))
            .unwrap()
            .set_modified(aged)
            .unwrap();

        // Act
        let newest = save(dir.path(), "newest").unwrap();

        // Assert
        assert!(!dir.path().join(&old).exists());
        assert!(dir.path().join(&recent).exists());
        assert!(dir.path().join(&newest).exists());
    }

    #[test]
    fn saving_into_a_missing_folder_is_an_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file");
        fs::write(&blocked, "x").unwrap();

        // Act
        let saved = save(&blocked, "text");

        // Assert
        assert!(saved.is_err());
    }

    #[test]
    fn the_reference_names_the_file_and_rounds_the_size_up() {
        // Arrange / Act
        let shown = reference(Path::new(".coda/pastes/p.txt"), 33 * 1024 + 1);

        // Assert
        assert_eq!(
            shown,
            "[pasted text of 34 KiB, saved at .coda/pastes/p.txt: read the whole file, in pieces if it is long]"
        );
    }
}
