// SPDX-License-Identifier: Apache-2.0

//! `coda hooks`: writes and removes the git hooks that start a review. The only hook is the
//! pre-push review. The script does nothing until `[review.hook] enabled` is set in the
//! global config, so installing it changes no push by itself.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::cli::{HookName, HooksCommand};
use crate::context::{Context, Io};
use crate::error::CliError;

const GIT: &str = "git";
const PRE_PUSH_PATH: &str = "hooks/pre-push";
const HOOK_MODE: u32 = 0o755;
/// The line that marks a hook as written by coda, so `remove` never deletes another one.
const MARKER: &str = "# coda-review-hook";
const SCRIPT: &str = "#!/bin/sh\n\
# coda-review-hook: written by `coda hooks install review`, removed by `coda hooks remove review`.\n\
# It does nothing until `[review.hook] enabled = true` is set in the coda config.\n\
command -v coda >/dev/null 2>&1 || exit 0\n\
coda review --hook\n\
code=$?\n\
# Only a finding that the config says to block stops the push, not a failure of coda itself.\n\
[ \"$code\" -eq 23 ] && exit 23\n\
exit 0\n";

/// Runs a `coda hooks` command.
///
/// # Errors
/// Returns [`CliError::Hooks`] when the directory is not a git repository, or a pre-push
/// hook that coda did not write is in the way.
pub fn run(ctx: &Context, io: &mut Io<'_>, command: &HooksCommand) -> Result<ExitCode, CliError> {
    let (HooksCommand::Install(args) | HooksCommand::Remove(args)) = command;
    let HookName::Review = args.name;
    let path = hook_path(&ctx.cwd)?;
    match command {
        HooksCommand::Install(_) => install(io, &path)?,
        HooksCommand::Remove(_) => remove(io, &path)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn hook_path(cwd: &Path) -> Result<PathBuf, CliError> {
    let out = Command::new(GIT)
        .args(["rev-parse", "--git-path", PRE_PUSH_PATH])
        .current_dir(cwd)
        .output()
        .map_err(|e| CliError::Hooks(format!("could not run git: {e}")))?;
    if !out.status.success() {
        return Err(CliError::Hooks(
            "this directory is not in a git repository".to_owned(),
        ));
    }
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(cwd.join(printed))
}

fn is_ours(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.lines().any(|line| line.starts_with(MARKER)))
}

fn refuse(path: &Path) -> CliError {
    CliError::Hooks(format!(
        "{} exists and coda did not write it; move it away first",
        path.display()
    ))
}

fn install(io: &mut Io<'_>, path: &Path) -> Result<(), CliError> {
    if path.exists() && !is_ours(path) {
        return Err(refuse(path));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| CliError::Path {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let io_error = |source| CliError::Path {
        path: path.to_path_buf(),
        source,
    };
    fs::write(path, SCRIPT).map_err(io_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(HOOK_MODE)).map_err(io_error)?;
    writeln!(
        io.out,
        "installed {}\nit reviews nothing until `[review.hook] enabled = true` is set in the config",
        path.display()
    )?;
    Ok(())
}

fn remove(io: &mut Io<'_>, path: &Path) -> Result<(), CliError> {
    if !path.exists() {
        writeln!(io.out, "no pre-push hook is installed")?;
        return Ok(());
    }
    if !is_ours(path) {
        return Err(refuse(path));
    }
    fs::remove_file(path).map_err(|source| CliError::Path {
        path: path.to_path_buf(),
        source,
    })?;
    writeln!(io.out, "removed {}", path.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tempfile::TempDir;

    use super::*;
    use crate::cli::HookArgs;

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let status = Command::new(GIT)
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        // A global core.hooksPath on the machine running the tests must not decide where the
        // hook goes.
        let status = Command::new(GIT)
            .args(["config", "core.hooksPath", ".git/hooks"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn command(install: bool) -> HooksCommand {
        let args = HookArgs {
            name: HookName::Review,
        };
        if install {
            HooksCommand::Install(args)
        } else {
            HooksCommand::Remove(args)
        }
    }

    fn go(dir: &TempDir, install: bool) -> Result<String, CliError> {
        let mut ctx = Context::new(
            coda_config::config::Dirs::from_base(dir.path().join("coda")),
            PathBuf::from("coda"),
            dir.path().to_path_buf(),
        );
        ctx.is_tty = false;
        let (mut out, mut err, mut input) = (Vec::new(), Vec::new(), Cursor::new(Vec::new()));
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        run(&ctx, &mut io, &command(install))?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn pre_push(dir: &TempDir) -> PathBuf {
        dir.path().join(".git/hooks/pre-push")
    }

    #[test]
    fn install_writes_an_executable_script_that_calls_the_review_hook() {
        // Arrange
        let dir = repo();

        // Act
        let said = go(&dir, true).unwrap();

        // Assert
        let text = fs::read_to_string(pre_push(&dir)).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains("coda review --hook"));
        assert!(text.contains(MARKER));
        let mode = fs::metadata(pre_push(&dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert!(said.contains("[review.hook] enabled = true"));
    }

    fn run_script(dir: &TempDir, coda_exit: i32) -> Option<i32> {
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("coda");
        fs::write(&fake, format!("#!/bin/sh\nexit {coda_exit}\n")).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(HOOK_MODE)).unwrap();
        let script = dir.path().join("hook.sh");
        fs::write(&script, SCRIPT).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        Command::new("/bin/sh")
            .arg(&script)
            .env("PATH", path)
            .status()
            .unwrap()
            .code()
    }

    #[test]
    fn the_script_stops_a_push_only_for_exit_23_and_lets_every_other_outcome_through() {
        // Arrange
        let dir = TempDir::new().unwrap();

        // Act and Assert
        assert_eq!(run_script(&dir, 23), Some(23));
        assert_eq!(run_script(&dir, 0), Some(0));
        assert_eq!(run_script(&dir, 64), Some(0));
        assert_eq!(run_script(&dir, 70), Some(0));
    }

    #[test]
    fn the_script_lets_the_push_through_when_coda_is_not_installed() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("hook.sh");
        fs::write(&script, SCRIPT).unwrap();

        // Act
        let code = Command::new("/bin/sh")
            .arg(&script)
            .env("PATH", dir.path())
            .status()
            .unwrap()
            .code();

        // Assert
        assert_eq!(code, Some(0));
    }

    #[test]
    fn a_hook_that_only_mentions_the_marker_is_not_ours() {
        // Arrange
        let dir = repo();
        fs::create_dir_all(pre_push(&dir).parent().unwrap()).unwrap();
        let mine = "#!/bin/sh\necho 'see # coda-review-hook in the docs'\n";
        fs::write(pre_push(&dir), mine).unwrap();

        // Act
        let install = go(&dir, true);

        // Assert
        assert!(matches!(install, Err(CliError::Hooks(_))));
        assert_eq!(fs::read_to_string(pre_push(&dir)).unwrap(), mine);
    }

    #[test]
    fn install_twice_is_fine_and_remove_deletes_the_hook() {
        // Arrange
        let dir = repo();
        go(&dir, true).unwrap();

        // Act
        go(&dir, true).unwrap();
        let said = go(&dir, false).unwrap();
        let again = go(&dir, false).unwrap();

        // Assert
        assert!(!pre_push(&dir).exists());
        assert!(said.starts_with("removed "));
        assert!(again.contains("no pre-push hook"));
    }

    #[test]
    fn a_hook_coda_did_not_write_is_left_alone() {
        // Arrange
        let dir = repo();
        fs::create_dir_all(pre_push(&dir).parent().unwrap()).unwrap();
        fs::write(pre_push(&dir), "#!/bin/sh\necho mine\n").unwrap();

        // Act
        let install = go(&dir, true);
        let remove = go(&dir, false);

        // Assert
        assert!(matches!(install, Err(CliError::Hooks(_))));
        assert!(matches!(remove, Err(CliError::Hooks(_))));
        assert_eq!(
            fs::read_to_string(pre_push(&dir)).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
    }

    #[test]
    fn outside_a_repository_it_says_so() {
        // Arrange
        let dir = TempDir::new().unwrap();

        // Act
        let result = go(&dir, true);

        // Assert
        assert!(matches!(result, Err(CliError::Hooks(why)) if why.contains("git repository")));
    }
}
