// SPDX-License-Identifier: Apache-2.0

//! Completes an `@path` word in the prompt against the session's folder.

use std::path::Path;

/// The marker that starts a path word.
pub const MARKER: char = '@';
/// How many names a list of choices shows.
pub const MAX_SHOWN: usize = 8;
const SEPARATOR: char = '/';
const PARENT: &str = "..";
const GIT_DIR: &str = ".git";
const HIDDEN: char = '.';
const SPACE_AFTER_FILE: &str = " ";
/// The most entries of a folder read for one completion, so a huge folder
/// cannot hold the UI.
pub const MAX_ENTRIES_READ: usize = 5000;

/// What Tab does to a path word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The word to put in place of the typed one, marker included.
    pub replacement: String,
    /// Up to [`MAX_SHOWN`] names to choose from; empty when the word is complete.
    pub choices: Vec<String>,
    /// How many names match.
    pub total: usize,
}

/// Completes `word`, which starts with [`MARKER`], against the entries of
/// `folder`. Not offered: a path that leaves the folder (also through a symbolic
/// link), symbolic links, `.git`, hidden entries unless the word starts with a
/// dot, and names with white space, control characters or bytes that are not
/// UTF-8. Matching is case sensitive. Directories end in a slash. `None` means
/// nothing matches.
#[must_use]
pub fn complete(folder: &Path, word: &str) -> Option<Completion> {
    let path = word.strip_prefix(MARKER)?;
    if path.starts_with(SEPARATOR)
        || path
            .split(SEPARATOR)
            .any(|part| part == PARENT || part == GIT_DIR)
    {
        return None;
    }
    let split = path.rfind(SEPARATOR).map_or(0, |at| at + 1);
    let (directory, prefix) = path.split_at(split);
    let listed = folder.join(directory);
    if !listed
        .canonicalize()
        .ok()?
        .starts_with(folder.canonicalize().ok()?)
    {
        return None;
    }
    let mut names: Vec<String> = std::fs::read_dir(listed)
        .ok()?
        .take(MAX_ENTRIES_READ)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let kind = entry.file_type().ok()?;
            let name = entry.file_name().into_string().ok()?;
            let plain =
                !kind.is_symlink() && !name.chars().any(|ch| ch.is_whitespace() || ch.is_control());
            let offered = plain
                && name.starts_with(prefix)
                && name != GIT_DIR
                && (prefix.starts_with(HIDDEN) || !name.starts_with(HIDDEN));
            offered.then(|| {
                if kind.is_dir() {
                    format!("{name}{SEPARATOR}")
                } else {
                    name
                }
            })
        })
        .collect();
    names.sort();
    let total = names.len();
    let common = common_prefix(&names)?;
    let complete_word = |tail: &str| format!("{MARKER}{directory}{tail}");
    if let [only] = names.as_slice() {
        let space = if only.ends_with(SEPARATOR) {
            ""
        } else {
            SPACE_AFTER_FILE
        };
        return Some(Completion {
            replacement: complete_word(&format!("{only}{space}")),
            choices: Vec::new(),
            total,
        });
    }
    names.truncate(MAX_SHOWN);
    Some(Completion {
        replacement: complete_word(&common),
        choices: names,
        total,
    })
}

/// The longest start every name shares, or `None` for no names.
fn common_prefix(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let mut length = first.len();
    for name in &names[1..] {
        length = first
            .char_indices()
            .zip(name.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((at, a), _)| at + a.len_utf8())
            .min(length);
    }
    Some(first[..length].to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        for file in [
            "README.md",
            "Cargo.toml",
            "src/main.rs",
            "src/lib.rs",
            "src/limits.rs",
            ".hidden",
        ] {
            fs::write(dir.path().join(file), "x").unwrap();
        }
        dir
    }

    #[test]
    fn one_match_completes_the_word_and_adds_a_space_after_a_file() {
        // Arrange
        let dir = folder();

        // Act
        let completion = complete(dir.path(), "@READ").unwrap();

        // Assert
        assert_eq!(completion.replacement, "@README.md ");
        assert_eq!(completion.choices, Vec::<String>::new());
        assert_eq!(completion.total, 1);
    }

    #[test]
    fn a_directory_completes_with_a_slash_and_no_space_so_typing_can_go_on() {
        // Arrange
        let dir = folder();

        // Act
        let completion = complete(dir.path(), "@sr").unwrap();

        // Assert
        assert_eq!(completion.replacement, "@src/");
    }

    #[test]
    fn several_matches_complete_to_their_common_start_and_list_the_choices() {
        // Arrange
        let dir = folder();

        // Act
        let completion = complete(dir.path(), "@src/l").unwrap();

        // Assert
        assert_eq!(completion.replacement, "@src/li");
        assert_eq!(completion.choices, ["lib.rs", "limits.rs"]);
        assert_eq!(completion.total, 2);
    }

    #[test]
    fn an_empty_name_lists_the_folder_without_hidden_entries_or_git() {
        // Arrange
        let dir = folder();

        // Act
        let completion = complete(dir.path(), "@").unwrap();

        // Assert
        assert_eq!(completion.choices, ["Cargo.toml", "README.md", "src/"]);
        assert_eq!(completion.replacement, "@");
    }

    #[test]
    fn a_dot_asks_for_hidden_entries_but_never_the_git_folder() {
        // Arrange
        let dir = folder();

        // Act
        let completion = complete(dir.path(), "@.").unwrap();

        // Assert
        assert_eq!(completion.replacement, "@.hidden ");
    }

    #[test]
    fn no_match_and_a_path_outside_the_folder_give_nothing() {
        // Arrange
        let dir = folder();

        // Act / Assert
        assert_eq!(complete(dir.path(), "@zzz"), None);
        assert_eq!(complete(dir.path(), "@../"), None);
        assert_eq!(complete(dir.path(), "@src/../"), None);
        assert_eq!(complete(dir.path(), "@/etc/"), None);
        assert_eq!(complete(dir.path(), "@missing/x"), None);
        assert_eq!(complete(dir.path(), "src"), None);
    }

    #[test]
    fn a_symlink_is_never_offered_and_a_path_through_one_to_outside_gives_nothing() {
        // Arrange
        let dir = folder();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("inside")).unwrap();

        // Act / Assert
        assert_eq!(complete(dir.path(), "@li"), None);
        assert_eq!(complete(dir.path(), "@link/"), None);
        assert_eq!(complete(dir.path(), "@link/sec"), None);
        assert_eq!(complete(dir.path(), "@ins"), None);
        let through_inside = complete(dir.path(), "@inside/ma").unwrap();
        assert_eq!(through_inside.replacement, "@inside/main.rs ");
    }

    #[test]
    fn the_git_folder_is_not_entered_even_when_typed() {
        // Arrange
        let dir = folder();

        // Act / Assert
        assert_eq!(complete(dir.path(), "@.git/"), None);
        assert_eq!(complete(dir.path(), "@.git/HE"), None);
        assert_eq!(complete(dir.path(), "@.g"), None);
        assert_eq!(complete(dir.path(), "@src/.git/"), None);
    }

    #[test]
    fn a_name_with_a_space_or_a_control_character_is_not_offered() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("my notes.md"), "x").unwrap();
        fs::write(dir.path().join("my\u{1b}[31mnotes"), "x").unwrap();
        fs::write(dir.path().join("my-notes.md"), "x").unwrap();

        // Act
        let completion = complete(dir.path(), "@my").unwrap();

        // Assert
        assert_eq!(completion.replacement, "@my-notes.md ");
        assert_eq!(completion.total, 1);
    }

    #[test]
    fn a_folder_with_more_entries_than_the_read_limit_is_only_partly_read() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        for number in 0..(MAX_ENTRIES_READ + 20) {
            fs::write(dir.path().join(format!("f{number:05}")), "x").unwrap();
        }

        // Act
        let completion = complete(dir.path(), "@f").unwrap();

        // Assert
        assert_eq!(completion.total, MAX_ENTRIES_READ);
    }

    #[test]
    fn matching_is_case_sensitive() {
        // Arrange
        let dir = folder();

        // Act / Assert
        assert_eq!(complete(dir.path(), "@readme"), None);
    }

    #[test]
    fn a_long_list_is_cut_to_the_most_it_shows_but_counts_every_match() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        for number in 0..(MAX_SHOWN + 4) {
            fs::write(dir.path().join(format!("f{number:02}")), "x").unwrap();
        }

        // Act
        let completion = complete(dir.path(), "@f").unwrap();

        // Assert
        assert_eq!(completion.choices.len(), MAX_SHOWN);
        assert_eq!(completion.total, MAX_SHOWN + 4);
        assert_eq!(completion.replacement, "@f");
    }

    #[test]
    fn the_common_start_respects_character_boundaries() {
        // Arrange
        let names = ["caf\u{e9}a".to_owned(), "caf\u{e8}b".to_owned()];

        // Act
        let common = common_prefix(&names);

        // Assert
        assert_eq!(common.as_deref(), Some("caf"));
        assert_eq!(common_prefix(&[]), None);
    }
}
