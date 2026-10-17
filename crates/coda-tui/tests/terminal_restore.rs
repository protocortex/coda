// SPDX-License-Identifier: AGPL-3.0-only

//! The terminal is restored after a full UI run, checked on an in-memory pipe
//! and on a real pseudo terminal.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use coda_tui::app::App;
use coda_tui::terminal::{AnsiMode, RawMode, TerminalGuard};
use coda_tui::{KeySource, UserInput, drive};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};
use tokio::sync::mpsc as tokio_mpsc;

const ENTER_ALTERNATE_SCREEN: &str = "\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const PASTE_OFF: &str = "\x1b[?2004l";
const SCREEN: Rect = Rect::new(0, 0, 80, 24);
/// The modes raw mode switches off. Other flags, such as the kernel's own
/// pending-input marker, change by themselves.
const LINE_EDITING: LocalModes = LocalModes::ICANON
    .union(LocalModes::ECHO)
    .union(LocalModes::ISIG)
    .union(LocalModes::IEXTEN);
/// Longest wait for the pty reader once the slave side is closed.
const READ_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
struct Pipe(Arc<Mutex<Vec<u8>>>);

impl Pipe {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Raw mode needs a real terminal, so the pipe test leaves it out.
struct NoRaw;

impl RawMode for NoRaw {
    fn enable(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn disable(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Raw mode on one terminal device instead of the process's own terminal.
struct DeviceRaw {
    device: File,
    original: Option<Termios>,
}

impl RawMode for DeviceRaw {
    fn enable(&mut self) -> io::Result<()> {
        let original = tcgetattr(&self.device)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&self.device, OptionalActions::Now, &raw)?;
        self.original = Some(original);
        Ok(())
    }

    fn disable(&mut self) -> io::Result<()> {
        match self.original.take() {
            Some(original) => Ok(tcsetattr(&self.device, OptionalActions::Now, &original)?),
            None => Ok(()),
        }
    }
}

fn ctrl_c() -> UserInput {
    UserInput::Key(KeyEvent {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// Sends Ctrl-C on the first poll and runs `during` just before it.
struct CtrlC<F: FnMut()> {
    pending: bool,
    during: F,
}

impl<F: FnMut()> KeySource for CtrlC<F> {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        if !std::mem::take(&mut self.pending) {
            return Ok(None);
        }
        (self.during)();
        Ok(Some(ctrl_c()))
    }
}

fn fixed_terminal<W: Write>(out: W) -> Terminal<CrosstermBackend<W>> {
    // A fixed viewport spares the backend a size query, which would need the
    // process's own terminal.
    let options = TerminalOptions {
        viewport: Viewport::Fixed(SCREEN),
    };
    Terminal::with_options(CrosstermBackend::new(out), options).unwrap()
}

/// Asserts the escapes of a full run: entered before the first draw, and the
/// cursor, paste mode and screen all restored after the last one.
fn assert_restored_after_the_last_draw(written: &str) {
    let at = |needle: &str| {
        written
            .rfind(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}"))
    };
    let drawn = written.find("coda").expect("the UI was never drawn");
    assert!(written.find(ENTER_ALTERNATE_SCREEN).unwrap() < drawn);
    for restore in [SHOW_CURSOR, PASTE_OFF, LEAVE_ALTERNATE_SCREEN] {
        assert!(at(restore) > drawn, "{restore:?} came before the last draw");
    }
}

#[test]
fn a_ctrl_c_exit_leaves_the_terminal_as_it_was_found() {
    // Arrange
    let pipe = Pipe::default();
    let guard = TerminalGuard::new(AnsiMode::new(pipe.clone(), NoRaw)).unwrap();
    let mut terminal = fixed_terminal(pipe.clone());
    let mut app = App::new();
    let (_sender, mut events) = tokio_mpsc::unbounded_channel();
    let mut keys = CtrlC {
        pending: true,
        during: || {},
    };

    // Act
    drive(&mut terminal, &mut app, &mut keys, &mut events).unwrap();
    guard.restore().unwrap();

    // Assert
    assert_restored_after_the_last_draw(&pipe.text());
}

/// A pseudo terminal pair: the master side the test reads, the slave the UI uses.
fn open_pty() -> (File, File) {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    (File::from(master), slave)
}

/// Drains the master on its own thread, because a full pty buffer would block the UI.
fn drain(mut master: File) -> mpsc::Receiver<Vec<u8>> {
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        // The read ends with an error or zero once every slave handle is closed.
        while let Ok(n @ 1..) = master.read(&mut chunk) {
            bytes.extend_from_slice(&chunk[..n]);
        }
        let _ = sender.send(bytes);
    });
    received
}

#[test]
fn a_pty_is_raw_while_the_ui_runs_and_back_to_its_modes_afterwards() {
    // Arrange
    let (master, slave) = open_pty();
    let received = drain(master);
    let probe = slave.try_clone().unwrap();
    let before = tcgetattr(&probe).unwrap();
    let raw = DeviceRaw {
        device: slave.try_clone().unwrap(),
        original: None,
    };
    let guard = TerminalGuard::new(AnsiMode::new(slave.try_clone().unwrap(), raw)).unwrap();
    let mut terminal = fixed_terminal(slave);
    let mut app = App::new();
    let (_sender, mut events) = tokio_mpsc::unbounded_channel();
    let running = Arc::new(Mutex::new(None));
    let mut keys = CtrlC {
        pending: true,
        during: {
            let (probe, running) = (probe.try_clone().unwrap(), Arc::clone(&running));
            move || *running.lock().unwrap() = Some(tcgetattr(&probe).unwrap())
        },
    };

    // Act
    drive(&mut terminal, &mut app, &mut keys, &mut events).unwrap();
    guard.restore().unwrap();
    let after = tcgetattr(&probe).unwrap();
    drop((terminal, keys, probe));
    let written = received
        .recv_timeout(READ_DEADLINE)
        .expect("the pty reader did not finish");

    // Assert
    let running = running.lock().unwrap().take().expect("the UI never polled");
    assert!(!running.local_modes.contains(LocalModes::ICANON));
    assert!(!running.local_modes.contains(LocalModes::ECHO));
    assert_eq!(
        after.local_modes & LINE_EDITING,
        before.local_modes & LINE_EDITING
    );
    assert_eq!(after.input_modes, before.input_modes);
    assert_eq!(after.output_modes, before.output_modes);
    assert_restored_after_the_last_draw(&String::from_utf8_lossy(&written));
}
