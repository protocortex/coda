// SPDX-License-Identifier: AGPL-3.0-only

//! Helpers that draw onto a `TestBackend` and read the screen back as text.

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Cell;

/// Draws with `render` on a `width` by `height` screen and returns its rows joined by newlines.
pub fn screen(width: u16, height: u16, render: impl FnOnce(&mut Frame)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(render).unwrap();
    let buffer = terminal.backend().buffer().clone();
    buffer
        .content()
        .chunks(usize::from(width).max(1))
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
