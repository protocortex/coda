// SPDX-License-Identifier: AGPL-3.0-only

//! Command line the self-hosting verifier script calls.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use coda_selfhost::attempt::{ANTHROPIC_KEY_ENV, Provider};
use coda_selfhost::error::SelfhostError;
use coda_selfhost::live::{LiveOptions, require_key, run_live};
use coda_selfhost::process::{ProcessError, handle_interrupts};
use coda_selfhost::scrub::scrub;
use coda_selfhost::verify::{VerifyPlan, verify_workdir};

const ATTEMPT_LIMIT: Duration = Duration::from_mins(30);
/// Where `--write-transcript` puts the scrubbed transcripts, under the source.
const TRANSCRIPT_DIR: &str = "docs/selfhost";
const CARGO_LIMIT: Duration = Duration::from_mins(30);
/// Exit codes: 0 accepted, 1 rejected, 2 usage (clap), 3 harness failure,
/// 130 interrupted by SIGINT or SIGTERM.
const REJECTED: u8 = 1;
const INFRASTRUCTURE: u8 = 3;
const INTERRUPTED: u8 = 130;

/// What serves the live run's model calls.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Backend {
    /// The Anthropic API, with a key from an environment variable.
    Anthropic,
    /// The locally installed Claude Code, under the user's own login.
    ClaudeCode,
    /// The locally installed Codex CLI, under the user's own login.
    Codex,
}

#[derive(Debug, Parser)]
#[command(name = "coda-selfhost")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Accept or reject a finished attempt (exit 0 or 1).
    Verify {
        /// The attempt's copy of the repository.
        #[arg(long)]
        workdir: PathBuf,
        /// Build output directory.
        #[arg(long)]
        target_dir: Option<PathBuf>,
        /// Arguments after `cargo test`; all tests when absent.
        #[arg(last = true)]
        cargo_test_args: Vec<String>,
    },
    /// Run up to 3 live attempts through `coda run` (spends tokens or subscription
    /// usage).
    /// Exit 0 when one passes the verifier, 1 when all are rejected (a timeout
    /// rejects an attempt), 3 when the run itself failed, including a missing
    /// API key variable, 130 when interrupted.
    Live {
        /// The `coda` binary to run.
        #[arg(long)]
        coda: PathBuf,
        /// The repository the temporary copies are made from.
        #[arg(long, default_value = ".")]
        source: PathBuf,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=3))]
        attempts: u32,
        /// What serves the model calls; `claude-code` and `codex` need no API key.
        #[arg(long, value_enum, default_value_t = Backend::Anthropic)]
        backend: Backend,
        /// Name of the environment variable holding the API key (anthropic only).
        #[arg(long, default_value = ANTHROPIC_KEY_ENV)]
        api_key_env: String,
        /// Write the scrubbed transcripts to docs/selfhost under the source;
        /// without it nothing is written.
        #[arg(long)]
        write_transcript: bool,
    },
    /// Print a transcript file without environment values, keys and absolute paths.
    Scrub { file: PathBuf },
}

fn main() -> ExitCode {
    let command = Cli::parse().command;
    let env: Vec<(String, String)> = std::env::vars().collect();
    if let Err(e) = handle_interrupts() {
        eprintln!("coda-selfhost: could not handle interrupts: {e}");
        return ExitCode::from(INFRASTRUCTURE);
    }
    match execute(command, &env) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(REJECTED),
        Err(e) => {
            eprintln!("coda-selfhost: {}", scrub(&e.to_string(), &env));
            let interrupted = matches!(e, SelfhostError::Process(ProcessError::Interrupted { .. }));
            ExitCode::from(if interrupted {
                INTERRUPTED
            } else {
                INFRASTRUCTURE
            })
        }
    }
}

/// `true` when the command's check passed. Everything it prints is scrubbed.
fn execute(command: Command, env: &[(String, String)]) -> Result<bool, SelfhostError> {
    match command {
        Command::Verify {
            workdir,
            target_dir,
            cargo_test_args,
        } => {
            let say = |line: &str| eprintln!("[selfhost] {}", scrub(line, env));
            say(&format!("verifying {}", workdir.display()));
            let verdict = verify_workdir(&VerifyPlan {
                workdir: &workdir,
                cargo_test_args,
                target_dir: target_dir.as_deref(),
                limit: CARGO_LIMIT,
                withheld_env: vec![ANTHROPIC_KEY_ENV.to_owned()],
            });
            match verdict {
                Ok(()) => Ok(true),
                Err(SelfhostError::Rejected(why)) => {
                    say(&format!("rejected: {why}"));
                    Ok(false)
                }
                Err(other) => Err(other),
            }
        }
        Command::Live {
            coda,
            source,
            attempts,
            backend,
            api_key_env,
            write_transcript,
        } => {
            if matches!(backend, Backend::Anthropic) {
                require_key(&api_key_env)?;
            }
            let out = source.join(TRANSCRIPT_DIR);
            let provider = |_: u32, _: &Path| match backend {
                Backend::Anthropic => Provider::Anthropic {
                    api_key_env: api_key_env.clone(),
                },
                Backend::ClaudeCode => Provider::ClaudeCode,
                Backend::Codex => Provider::Codex,
            };
            let passed = run_live(
                &LiveOptions {
                    coda: &coda,
                    source: &source,
                    attempts,
                    provider: &provider,
                    env: env.to_vec(),
                    cargo_test_args: Vec::new(),
                    target_dir: None,
                    attempt_limit: ATTEMPT_LIMIT,
                    cargo_limit: CARGO_LIMIT,
                    transcripts: write_transcript.then_some(out.as_path()),
                },
                &mut |line| eprintln!("[selfhost] {line}"),
            )?;
            Ok(passed.is_some())
        }
        Command::Scrub { file } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|source| SelfhostError::Io { path: file, source })?;
            print!("{}", scrub(&text, env));
            Ok(true)
        }
    }
}
