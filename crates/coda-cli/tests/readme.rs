// SPDX-License-Identifier: AGPL-3.0-only

//! The README quickstart and `docs/config.md` stay true: tagged code blocks run
//! against the real `coda` binary and the faux provider, the account-risk notice
//! is present, every `--flag` the docs name exists in its command's `--help`, and the
//! defaults tables match `Config::default()`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use coda_config::config::{Config, ProviderConfig};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_coda");
const BLOCK_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);
const MIN_TEST_BLOCKS: usize = 3;
const TEST_TAG: &str = "coda-test";
const OUTPUT_TAG: &str = "coda-output";
const CONFIG_TAG: &str = "coda-config";
const NOTICE_HEADING: &str = "## Account risk notice";
const NOTICE_SENTENCES: [&str; 4] = [
    "Coda has not verified that the Anthropic terms allow this use of the `claude` binary.",
    "Coda has not verified that the OpenAI terms allow this use of the `codex` binary.",
    "You must check the terms for your account yourself.",
    "The claude_code and codex backends stay disabled until you enable them.",
];
const PID_PREFIX: &str = "pid: ";
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const CANARY_KEY: &str = "sk-canary-must-not-reach-a-block";
const HELP_COMMANDS: [&[&str]; 9] = [
    &[],
    &["daemon"],
    &["daemon", "restart"],
    &["status"],
    &["ping"],
    &["run"],
    &["grant", "add"],
    &["usage"],
    &["doctor"],
];

struct Block {
    info: String,
    body: String,
}

fn repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

fn fenced_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for line in markdown.lines() {
        match (line.strip_prefix("```"), open.take()) {
            (Some(info), None) => {
                open = Some(Block {
                    info: info.trim().to_owned(),
                    body: String::new(),
                });
            }
            (Some(_), Some(block)) => blocks.push(block),
            (None, Some(mut block)) => {
                block.body.push_str(line);
                block.body.push('\n');
                open = Some(block);
            }
            (None, None) => {}
        }
    }
    blocks
}

fn has_tag(block: &Block, tag: &str) -> bool {
    block.info.split_whitespace().any(|word| word == tag)
}

/// macOS limits socket paths to 104 bytes, so keep them under /tmp.
fn short_dir() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("cc")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Runs `command` and returns its standard output, or `None` if it outlasts `limit`.
fn output_within(command: &mut Command, limit: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(POLL);
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

fn signal(name: &str, pid: &str) -> bool {
    Command::new("kill")
        .args([name, pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Stops any daemon the blocks left running, even when an assertion fails first,
/// and waits for it to exit so the directory it uses can be removed.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let mut status = Command::new(BIN);
        status.arg("status").env("CODA_HOME", &self.0);
        let Some(out) = output_within(&mut status, STOP_TIMEOUT) else {
            return;
        };
        let Some(pid) = out.lines().find_map(|line| line.strip_prefix(PID_PREFIX)) else {
            return;
        };
        signal("-TERM", pid);
        let deadline = Instant::now() + STOP_TIMEOUT;
        while signal("-0", pid) {
            if Instant::now() >= deadline {
                signal("-KILL", pid);
                return;
            }
            std::thread::sleep(POLL);
        }
    }
}

struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
}

/// Gives `command` an environment of its own: nothing inherited, so a block can
/// never read the caller's API keys, and a throwaway home for tools such as git.
fn isolate(command: &mut Command, coda_home: &Path, scratch: &Path) {
    let bin_dir = Path::new(BIN).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (user_home, tmp) = (scratch.join("home"), scratch.join("tmp"));
    fs::create_dir_all(&user_home).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", user_home)
        .env("TMPDIR", tmp)
        .env("CODA_HOME", coda_home);
}

/// Runs `script` with `sh`, output going to files so a daemon it starts cannot hold a pipe open.
fn run_block(script: &str, home: &Path, work: &Path, scratch: &Path) -> Ran {
    let (out_path, err_path) = (scratch.join("out"), scratch.join("err"));
    let mut command = Command::new("sh");
    command.arg("-ec").arg(script).current_dir(work);
    isolate(&mut command, home, scratch);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(fs::File::create(&out_path).unwrap())
        .stderr(fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + BLOCK_TIMEOUT;
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a README block ran longer than {BLOCK_TIMEOUT:?}:\n{script}");
        }
        std::thread::sleep(POLL);
    };
    Ran {
        code,
        out: fs::read_to_string(out_path).unwrap(),
        err: fs::read_to_string(err_path).unwrap(),
    }
}

#[test]
fn the_readme_quickstart_blocks_run_and_print_what_they_claim() {
    // Arrange
    let blocks = fenced_blocks(&repo_file("README.md"));
    let home = short_dir();
    let work = short_dir();
    let scratch = short_dir();
    let _stop = StopDaemon(home.path().to_path_buf());
    let tested: Vec<usize> = (0..blocks.len())
        .filter(|&i| has_tag(&blocks[i], TEST_TAG))
        .collect();

    // Act
    let results: Vec<(usize, Ran)> = tested
        .iter()
        .map(|&i| {
            (
                i,
                run_block(&blocks[i].body, home.path(), work.path(), scratch.path()),
            )
        })
        .collect();

    // Assert
    assert!(
        tested.len() >= MIN_TEST_BLOCKS,
        "the README has {} `{TEST_TAG}` blocks, need {MIN_TEST_BLOCKS}",
        tested.len()
    );
    for (i, ran) in &results {
        assert_eq!(
            ran.code,
            Some(0),
            "block failed:\n{}\nstdout: {}\nstderr: {}",
            blocks[*i].body,
            ran.out,
            ran.err
        );
        if let Some(expected) = blocks.get(i + 1).filter(|b| has_tag(b, OUTPUT_TAG)) {
            assert_eq!(
                ran.out.trim_end(),
                expected.body.trim_end(),
                "output of:\n{}",
                blocks[*i].body
            );
        }
    }
}

#[test]
fn the_readme_has_the_account_risk_notice() {
    // Arrange
    let readme = repo_file("README.md");

    // Act
    let notice = readme
        .split_once(NOTICE_HEADING)
        .map(|(_, rest)| rest.split("\n## ").next().unwrap_or_default());

    // Assert
    let notice = notice.unwrap_or_else(|| panic!("README.md has no `{NOTICE_HEADING}` heading"));
    let flat = notice.split_whitespace().collect::<Vec<_>>().join(" ");
    for sentence in NOTICE_SENTENCES {
        assert!(flat.contains(sentence), "the notice lacks: {sentence}");
    }
}

fn flags_in(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|line| !line.contains("cargo "))
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')))
        .filter(|word| {
            word.starts_with("--")
                && word.len() > 2
                && word[2..].starts_with(|c: char| c.is_ascii_lowercase())
        })
        .map(str::to_owned)
        .collect()
}

/// The commands a line names, such as `coda grant add --repo .`, longest match each.
fn owners(line: &str) -> BTreeSet<&'static [&'static str]> {
    line.match_indices("coda ")
        .filter_map(|(at, mention)| {
            let mut words = Vec::new();
            for word in line[at + mention.len()..].split_whitespace() {
                let name = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
                if name.is_empty() || name.starts_with('-') {
                    break;
                }
                words.push(name);
                if name.len() != word.len() {
                    break;
                }
            }
            HELP_COMMANDS
                .into_iter()
                .filter(|command| !command.is_empty() && words.starts_with(command))
                .max_by_key(|command| command.len())
        })
        .collect()
}

fn help_flags(command: &[&str]) -> BTreeSet<String> {
    let out = Command::new(BIN)
        .args(command)
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    flags_in(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn every_flag_the_docs_name_exists_in_its_commands_help() {
    // Arrange
    let helps: BTreeMap<&[&str], BTreeSet<String>> = HELP_COMMANDS
        .into_iter()
        .map(|command| (command, help_flags(command)))
        .collect();
    let docs = format!(
        "{}\n{}",
        repo_file("README.md"),
        repo_file("docs/config.md")
    );

    // Act
    let mut named = 0;
    let mut missing = Vec::new();
    for line in docs.lines() {
        let owners = owners(line);
        for flag in flags_in(line) {
            named += 1;
            // A flag on a line naming no command is global, so every command must list it.
            let found = if owners.is_empty() {
                helps.values().all(|help| help.contains(&flag))
            } else {
                owners.iter().any(|command| helps[command].contains(&flag))
            };
            if !found {
                missing.push(format!("{flag} in: {line}"));
            }
        }
    }

    // Assert
    assert!(named > 0, "the docs name no flags");
    assert!(
        missing.is_empty(),
        "flags not in the owning --help: {missing:#?}"
    );
}

#[test]
fn every_config_example_in_the_docs_parses_without_unknown_keys() {
    // Arrange
    let blocks = fenced_blocks(&repo_file("docs/config.md"));

    // Act
    let parsed: Vec<_> = blocks
        .iter()
        .filter(|b| has_tag(b, CONFIG_TAG))
        .map(|b| Config::from_toml_str(&b.body))
        .collect();

    // Assert
    assert!(
        !parsed.is_empty(),
        "docs/config.md has no `{CONFIG_TAG}` block"
    );
    for result in parsed {
        let parsed = result.expect("a documented example must be a valid config");
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    }
}

#[test]
fn a_readme_block_never_inherits_api_keys() {
    // Arrange
    let scratch = short_dir();
    let mut command = Command::new("env");
    command.env("ANTHROPIC_API_KEY", CANARY_KEY);

    // Act
    isolate(&mut command, scratch.path(), scratch.path());
    let dump = command.stdin(Stdio::null()).output().unwrap();
    let vars = String::from_utf8_lossy(&dump.stdout);

    // Assert
    assert!(dump.status.success());
    assert!(
        !vars.contains(CANARY_KEY),
        "a block saw the canary key:\n{vars}"
    );
    assert!(
        vars.contains(&format!("HOME={}", scratch.path().join("home").display())),
        "a block did not get the throwaway home:\n{vars}"
    );
}

/// The `Key | ... | Default | ...` rows of `docs/config.md` as (section, key, default cell).
/// The section is empty for the top level.
fn default_rows(markdown: &str) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    let mut section = None;
    let mut default_col = None;
    for line in markdown.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            section = match heading.split('`').nth(1) {
                _ if heading == "Top level" => Some(String::new()),
                Some(table) => Some(
                    table
                        .trim_matches(['[', ']'])
                        .replace("<name>", "x")
                        .replace("<alias>", "x"),
                ),
                None => None,
            };
            default_col = None;
        } else if line.starts_with('|') {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells[0] == "Key" {
                default_col = cells.iter().position(|cell| *cell == "Default");
            } else if let (Some(section), Some(col)) = (&section, default_col)
                && !cells[0].starts_with("---")
            {
                rows.push((
                    section.clone(),
                    cells[0].trim_matches('`').to_owned(),
                    cells[col].to_owned(),
                ));
            }
        }
    }
    rows
}

#[test]
fn the_defaults_tables_match_the_default_config() {
    // Arrange
    let rows = default_rows(&repo_file("docs/config.md"));
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // Act
    for (section, key, cell) in &rows {
        match cell.strip_prefix('`').and_then(|c| c.strip_suffix('`')) {
            Some(literal) => sections
                .entry(section.clone())
                .or_default()
                .push(format!("{key} = {literal}")),
            None => assert!(
                cell.starts_with("unset") || cell == "none",
                "`{key}` has the default `{cell}`, which is neither a literal nor unset"
            ),
        }
    }
    let mut toml = String::new();
    for (section, lines) in &sections {
        if !section.is_empty() {
            toml.push_str(&["[", section, "]\n"].concat());
        }
        toml.push_str(&lines.join("\n"));
        toml.push('\n');
    }
    let parsed = Config::from_toml_str(&toml).expect("the documented defaults must parse");

    // Assert
    for section in ["", "permissions", "providers.x", "telemetry", "timing"] {
        assert!(
            sections.contains_key(section),
            "no defaults for [{section}]"
        );
    }
    let mut expected = Config::default();
    expected
        .providers
        .insert("x".to_owned(), ProviderConfig::default());
    assert_eq!(parsed.unknown_keys, Vec::<String>::new());
    assert_eq!(parsed.config, expected, "documented defaults:\n{toml}");
}
