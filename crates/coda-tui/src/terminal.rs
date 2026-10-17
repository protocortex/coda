// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal setup and a guard that restores it however the UI ends.

use std::io::{self, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use tracing::warn;

/// Switches a terminal into and out of full screen UI mode.
pub trait TerminalMode {
    /// # Errors
    /// Returns the I/O error of the terminal.
    fn enter(&mut self) -> io::Result<()>;

    /// Must be safe to call after a partial or failed [`Self::enter`].
    ///
    /// # Errors
    /// Returns the I/O error of the terminal.
    fn leave(&mut self) -> io::Result<()>;
}

/// Restores the terminal when dropped, including while a panic unwinds.
pub struct TerminalGuard<M: TerminalMode> {
    mode: M,
    active: bool,
}

impl<M: TerminalMode> TerminalGuard<M> {
    /// # Errors
    /// Returns the error of [`TerminalMode::enter`], after trying to restore.
    pub fn new(mut mode: M) -> io::Result<Self> {
        if let Err(error) = mode.enter() {
            // Entry may have half applied, so undo it before reporting.
            let _ = mode.leave();
            return Err(error);
        }
        Ok(Self { mode, active: true })
    }

    /// Restores the terminal now and reports a failure, unlike the silent drop.
    ///
    /// # Errors
    /// Returns the error of [`TerminalMode::leave`].
    pub fn restore(mut self) -> io::Result<()> {
        self.leave_once()
    }

    fn leave_once(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.mode.leave()
    }
}

impl<M: TerminalMode> Drop for TerminalGuard<M> {
    fn drop(&mut self) {
        if let Err(error) = self.leave_once() {
            warn!(%error, "could not restore the terminal");
        }
    }
}

/// Restores the real terminal through stdout, for the panic hook.
pub fn restore_stdout() {
    // A panic is already under way, so a failure here has nowhere to go.
    let _ = AnsiMode::new(io::stdout(), SystemRawMode).leave();
}

/// Runs `restore` before the previously installed panic hook, so the panic
/// message prints on the normal screen instead of vanishing with the alternate one.
pub fn install_panic_hook(restore: impl Fn() + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Raw mode is process wide, so it sits behind its own seam.
pub trait RawMode {
    /// # Errors
    /// Returns the I/O error of the terminal.
    fn enable(&mut self) -> io::Result<()>;

    /// # Errors
    /// Returns the I/O error of the terminal.
    fn disable(&mut self) -> io::Result<()>;
}

/// Switches the real terminal into raw mode.
pub struct SystemRawMode;

impl RawMode for SystemRawMode {
    fn enable(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

/// [`TerminalMode`] that writes the alternate screen and cursor escapes to `W`.
pub struct AnsiMode<W: Write, R: RawMode> {
    out: W,
    raw: R,
}

impl<W: Write, R: RawMode> AnsiMode<W, R> {
    pub fn new(out: W, raw: R) -> Self {
        Self { out, raw }
    }
}

impl<W: Write, R: RawMode> TerminalMode for AnsiMode<W, R> {
    fn enter(&mut self) -> io::Result<()> {
        self.raw.enable()?;
        execute!(self.out, EnterAlternateScreen, EnableBracketedPaste, Hide)
    }

    fn leave(&mut self) -> io::Result<()> {
        // Always attempt both steps so one failure cannot leave the terminal raw.
        let screen = execute!(self.out, Show, DisableBracketedPaste, LeaveAlternateScreen);
        let raw = self.raw.disable();
        screen.and(raw)
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    use super::*;

    const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
    const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";
    const HIDE_CURSOR: &str = "\x1b[?25l";
    const SHOW_CURSOR: &str = "\x1b[?25h";
    const ENABLE_PASTE: &str = "\x1b[?2004h";
    const DISABLE_PASTE: &str = "\x1b[?2004l";

    #[derive(Clone, Default)]
    struct Recording(Arc<Mutex<Vec<String>>>);

    impl Recording {
        fn push(&self, entry: &str) {
            self.0.lock().unwrap().push(entry.to_owned());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Records every write and raw mode switch in one ordered log.
    struct RecordingWriter(Recording);

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.push(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct RecordingRaw(Recording);

    impl RawMode for RecordingRaw {
        fn enable(&mut self) -> io::Result<()> {
            self.0.push("raw:on");
            Ok(())
        }

        fn disable(&mut self) -> io::Result<()> {
            self.0.push("raw:off");
            Ok(())
        }
    }

    fn guard(log: &Recording) -> TerminalGuard<AnsiMode<RecordingWriter, RecordingRaw>> {
        let mode = AnsiMode::new(RecordingWriter(log.clone()), RecordingRaw(log.clone()));
        TerminalGuard::new(mode).unwrap()
    }

    fn joined(log: &Recording) -> String {
        log.entries().concat()
    }

    #[test]
    fn creating_the_guard_enters_raw_mode_and_the_alternate_screen() {
        // Arrange
        let log = Recording::default();

        // Act
        let _guard = guard(&log);

        // Assert
        let entries = log.entries();
        assert_eq!(entries.first().map(String::as_str), Some("raw:on"));
        let written = joined(&log);
        assert!(written.contains(ENTER_ALT_SCREEN), "{written:?}");
        assert!(written.contains(HIDE_CURSOR), "{written:?}");
        assert!(!written.contains(LEAVE_ALT_SCREEN), "{written:?}");
    }

    #[test]
    fn the_guard_turns_bracketed_paste_on_and_restores_it_on_exit() {
        // Arrange
        let log = Recording::default();
        let guard = guard(&log);
        let entered = joined(&log);

        // Act
        guard.restore().unwrap();

        // Assert
        let written = joined(&log);
        assert!(entered.contains(ENABLE_PASTE), "{entered:?}");
        assert!(!entered.contains(DISABLE_PASTE), "{entered:?}");
        assert_eq!(written.matches(DISABLE_PASTE).count(), 1, "{written:?}");
    }

    #[test]
    fn dropping_the_guard_restores_the_terminal() {
        // Arrange
        let log = Recording::default();
        let guard = guard(&log);

        // Act
        drop(guard);

        // Assert
        let written = joined(&log);
        assert!(written.ends_with("raw:off"), "{written:?}");
        assert!(written.contains(SHOW_CURSOR), "{written:?}");
        assert!(written.contains(LEAVE_ALT_SCREEN), "{written:?}");
    }

    #[test]
    fn restoring_explicitly_restores_exactly_once() {
        // Arrange
        let log = Recording::default();
        let guard = guard(&log);

        // Act
        guard.restore().unwrap();

        // Assert
        let written = joined(&log);
        assert_eq!(written.matches(LEAVE_ALT_SCREEN).count(), 1, "{written:?}");
        assert_eq!(written.matches("raw:off").count(), 1, "{written:?}");
    }

    #[test]
    fn a_panic_while_the_guard_is_alive_still_restores_the_terminal() {
        // Arrange
        let log = Recording::default();

        // Act
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = guard(&log);
            panic!("ui crashed");
        }));

        // Assert
        assert!(outcome.is_err());
        let written = joined(&log);
        assert!(written.contains(LEAVE_ALT_SCREEN), "{written:?}");
        assert!(written.ends_with("raw:off"), "{written:?}");
    }

    #[test]
    fn the_panic_hook_restores_the_terminal_before_the_previous_hook_prints() {
        // Arrange
        let log = Recording::default();
        let original = std::panic::take_hook();
        let previous_log = log.clone();
        std::panic::set_hook(Box::new(move |_| previous_log.push("previous hook")));
        let restore_log = log.clone();
        install_panic_hook(move || {
            let mut mode = AnsiMode::new(
                RecordingWriter(restore_log.clone()),
                RecordingRaw(restore_log.clone()),
            );
            mode.leave().unwrap();
        });

        // Act
        let outcome = catch_unwind(|| panic!("ui crashed"));
        std::panic::set_hook(original);

        // Assert
        assert!(outcome.is_err());
        let entries = log.entries();
        assert_eq!(entries.last().map(String::as_str), Some("previous hook"));
        let written = joined(&log);
        assert!(written.contains(LEAVE_ALT_SCREEN), "{written:?}");
        assert!(written.contains("raw:off"), "{written:?}");
    }

    struct FailingEnter(Recording);

    impl TerminalMode for FailingEnter {
        fn enter(&mut self) -> io::Result<()> {
            Err(io::Error::other("no tty"))
        }

        fn leave(&mut self) -> io::Result<()> {
            self.0.push("left");
            Ok(())
        }
    }

    #[test]
    fn a_failed_entry_restores_before_reporting_the_error() {
        // Arrange
        let log = Recording::default();

        // Act
        let result = TerminalGuard::new(FailingEnter(log.clone()));

        // Assert
        assert!(result.is_err());
        assert_eq!(log.entries(), ["left"]);
    }
}
