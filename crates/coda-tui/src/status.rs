// SPDX-License-Identifier: AGPL-3.0-only

//! The status line: backend, model and tokens used.

use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, TokenUsage};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

const NO_BACKEND: &str = "no backend";
const NO_MODEL: &str = "no model";
/// For a backend added to the protocol after this client was built.
const UNKNOWN_BACKEND: &str = "unknown backend";

#[derive(Debug, Default)]
pub struct Status {
    backend: Option<BackendKind>,
    model: Option<String>,
    usage: TokenUsage,
}

impl Status {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_session(&mut self, backend: BackendKind, model: Option<String>) {
        self.backend = Some(backend);
        self.model = model;
    }

    /// Adds the tokens of a usage report to the running total.
    pub fn apply(&mut self, event: &Event) {
        if let Event::Usage { usage, .. } = event {
            self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
            self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        }
    }

    #[must_use]
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let backend = self.backend.map_or(NO_BACKEND, backend_name);
        let model = self.model.as_deref().unwrap_or(NO_MODEL);
        let line = format!(
            "{backend} | {model} | in {} out {}",
            self.usage.input_tokens, self.usage.output_tokens
        );
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::ClaudeCode => "claude_code",
        BackendKind::CodexAppServer => "codex_app_server",
        BackendKind::CodexExec => "codex_exec",
        BackendKind::Anthropic => "anthropic",
        BackendKind::OpenaiCompat => "openai_compat",
        BackendKind::Faux => "faux",
        _ => UNKNOWN_BACKEND,
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::{SessionId, TurnId};

    use super::*;
    use crate::test_support::screen;

    fn usage_event(input: u64, output: u64) -> Event {
        Event::Usage {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                ..TokenUsage::default()
            },
        }
    }

    fn draw(status: &Status, width: u16) -> String {
        screen(width, 1, |frame| status.render(frame, frame.area()))
    }

    #[test]
    fn a_fresh_status_shows_placeholders() {
        // Arrange
        let status = Status::new();

        // Act
        let shown = draw(&status, 40);

        // Assert
        assert_eq!(shown.trim_end(), "no backend | no model | in 0 out 0");
    }

    #[test]
    fn the_status_line_shows_backend_model_and_tokens() {
        // Arrange
        let mut status = Status::new();

        // Act
        status.set_session(BackendKind::ClaudeCode, Some("sonnet".to_owned()));
        status.apply(&usage_event(120, 30));

        // Assert
        assert_eq!(
            draw(&status, 50).trim_end(),
            "claude_code | sonnet | in 120 out 30"
        );
    }

    #[test]
    fn usage_reports_add_up() {
        // Arrange
        let mut status = Status::new();

        // Act
        status.apply(&usage_event(10, 1));
        status.apply(&usage_event(5, 2));

        // Assert
        assert_eq!(status.usage().input_tokens, 15);
        assert_eq!(status.usage().output_tokens, 3);
    }

    #[test]
    fn every_backend_has_a_name_and_a_missing_model_reads_as_none() {
        for (kind, name) in [
            (BackendKind::ClaudeCode, "claude_code"),
            (BackendKind::CodexAppServer, "codex_app_server"),
            (BackendKind::CodexExec, "codex_exec"),
            (BackendKind::Anthropic, "anthropic"),
            (BackendKind::OpenaiCompat, "openai_compat"),
            (BackendKind::Faux, "faux"),
        ] {
            // Arrange
            let mut status = Status::new();

            // Act
            status.set_session(kind, None);

            // Assert
            let expected = format!("{name} | no model | in 0 out 0");
            assert_eq!(draw(&status, 60).trim_end(), expected);
        }
    }

    #[test]
    fn other_events_leave_the_totals_alone_and_tiny_areas_do_not_panic() {
        // Arrange
        let mut status = Status::new();
        status.apply(&usage_event(1, 1));

        // Act
        status.apply(&Event::TurnInterrupted {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
        });

        // Assert
        assert_eq!(status.usage().input_tokens, 1);
        for width in [0, 1, 5] {
            let _ = draw(&status, width);
        }
    }
}
