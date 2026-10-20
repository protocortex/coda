// SPDX-License-Identifier: AGPL-3.0-only

//! The live acceptance run: up to a few attempts through the real `coda run`,
//! each in a fresh copy of the repository.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::attempt::{AttemptSpec, Provider, Transcript, run_attempt};
use crate::error::SelfhostError;
use crate::process::ProcessError;
use crate::scrub::scrub;
use crate::verify::{VerifyPlan, verify_workdir};
use crate::workspace::prepare_copy;

const TRANSCRIPT_PREFIX: &str = "attempt-";

/// Everything one live run needs. The provider is injected so the loop can be
/// exercised without the network.
pub struct LiveOptions<'a> {
    pub coda: &'a Path,
    /// The repository the temporary copies are made from.
    pub source: &'a Path,
    pub attempts: u32,
    /// The provider of attempt `n`, given that attempt's `CODA_HOME`.
    pub provider: &'a dyn Fn(u32, &Path) -> Provider,
    /// Environment whose values are scrubbed from the transcripts.
    pub env: Vec<(String, String)>,
    /// Arguments after `cargo test`; all tests when empty.
    pub cargo_test_args: Vec<String>,
    /// Build output directory shared by the verifications; one in the
    /// temporary root when `None`.
    pub target_dir: Option<&'a Path>,
    pub attempt_limit: Duration,
    pub cargo_limit: Duration,
    /// Where scrubbed transcripts go; none are written when `None`.
    pub transcripts: Option<&'a Path>,
}

/// Fails unless the environment variable `name` is set; its value is never read.
pub fn require_key(name: &str) -> Result<(), SelfhostError> {
    if std::env::var_os(name).is_some() {
        Ok(())
    } else {
        Err(SelfhostError::MissingKey(name.to_owned()))
    }
}

/// Runs up to `options.attempts` attempts and stops at the first that passes
/// the verifier. Returns its number, or `None` when every attempt was
/// rejected. `log` receives one scrubbed line per step.
///
/// A `coda run` or cargo step that outlives its limit rejects the attempt.
///
/// # Errors
/// A step that could not run, such as a missing `coda` binary or a failing
/// git command, ends the run: that is the harness failing, not the attempt.
/// So does an interrupt, after the running child is killed and the temporary
/// root removed.
pub fn run_live(
    options: &LiveOptions<'_>,
    log: &mut dyn FnMut(&str),
) -> Result<Option<u32>, SelfhostError> {
    let mut say = |line: &str| log(&scrub(line, &options.env));
    if let Some(dir) = options.transcripts {
        check_transcript_dir(dir)?;
    }
    // Under /tmp so the daemon socket path stays short.
    let root = tempfile::Builder::new()
        .prefix("sh")
        .tempdir_in("/tmp")
        .map_err(|e| io_error(Path::new("/tmp"), e))?;
    let root_path = root
        .path()
        .canonicalize()
        .map_err(|e| io_error(root.path(), e))?;
    let shared_target = root_path.join("target");
    let target = options.target_dir.unwrap_or(&shared_target);
    for number in 1..=options.attempts {
        let (repo, home) = (
            root_path.join(format!("a{number}")),
            root_path.join(format!("h{number}")),
        );
        say(&format!("attempt {number}: copying the repository"));
        fs::create_dir_all(&home).map_err(|e| io_error(&home, e))?;
        prepare_copy(options.source, &repo)?;
        say(&format!("attempt {number}: running coda"));
        let provider = (options.provider)(number, &home);
        let withheld_env = provider
            .withheld_env()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let ran = run_attempt(&AttemptSpec {
            coda: options.coda,
            home: &home,
            workdir: &repo,
            provider,
            limit: options.attempt_limit,
        });
        let transcript = match ran {
            Ok(transcript) => transcript,
            Err(SelfhostError::Process(timeout @ ProcessError::Timeout { .. })) => {
                say(&format!("attempt {number}: rejected: {timeout}"));
                continue;
            }
            Err(other) => return Err(other),
        };
        if let Some(dir) = options.transcripts {
            write_transcript(dir, number, &transcript, &options.env)?;
        }
        say(&format!("attempt {number}: verifying"));
        let verdict = verify_workdir(&VerifyPlan {
            workdir: &repo,
            cargo_test_args: options.cargo_test_args.clone(),
            target_dir: Some(target),
            limit: options.cargo_limit,
            withheld_env,
        });
        match verdict {
            Ok(()) => {
                say(&format!("attempt {number}: passed"));
                return Ok(Some(number));
            }
            Err(SelfhostError::Rejected(why)) => say(&format!("attempt {number}: rejected: {why}")),
            Err(other) => return Err(other),
        }
    }
    Ok(None)
}

/// Refuses a transcript directory that is a symlink or already holds
/// transcripts, so a file from an earlier run cannot pass for this run's.
fn check_transcript_dir(dir: &Path) -> Result<(), SelfhostError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(SelfhostError::TranscriptDirIsLink(dir.to_path_buf()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(dir, e)),
    }
    for entry in fs::read_dir(dir).map_err(|e| io_error(dir, e))? {
        let entry = entry.map_err(|e| io_error(dir, e))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(TRANSCRIPT_PREFIX)
        {
            return Err(SelfhostError::StaleTranscripts(dir.to_path_buf()));
        }
    }
    Ok(())
}

/// Writes the scrubbed transcript to a temporary file and renames it into
/// place. A rename replaces a symlink planted at the final name instead of
/// writing through it.
fn write_transcript(
    dir: &Path,
    number: u32,
    transcript: &Transcript,
    env: &[(String, String)],
) -> Result<(), SelfhostError> {
    let text = format!(
        "exit: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        transcript.exit_code, transcript.stdout, transcript.stderr
    );
    fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
    let file = dir.join(format!("{TRANSCRIPT_PREFIX}{number}.txt"));
    let mut staged = tempfile::NamedTempFile::new_in(dir).map_err(|e| io_error(dir, e))?;
    staged
        .write_all(scrub(&text, env).as_bytes())
        .map_err(|e| io_error(&file, e))?;
    staged
        .persist(&file)
        .map_err(|e| io_error(&file, e.error))?;
    Ok(())
}

fn io_error(path: &Path, source: std::io::Error) -> SelfhostError {
    SelfhostError::Io {
        path: path.to_path_buf(),
        source,
    }
}
