// SPDX-License-Identifier: Apache-2.0

//! The status line: backend, model and tokens used.

use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, ContextFill, Mode, TokenUsage};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::limits::Limits;

const NO_BACKEND: &str = "no backend";
const NO_MODEL: &str = "no model";
/// For a backend added to the protocol after this client was built.
const UNKNOWN_BACKEND: &str = "unknown backend";
/// For a mode added to the protocol after this client was built.
const UNKNOWN_MODE: &str = "unknown";
const PLAN_MODE: &str = "plan mode";
const PLAN_READY: &str = "plan ready";
const PERCENT: u128 = 100;
const THOUSAND: u32 = 1_000;
const MILLION: u32 = 1_000_000;

#[derive(Debug, Default)]
pub struct Status {
    backend: Option<BackendKind>,
    model: Option<String>,
    usage: TokenUsage,
    context: Option<ContextFill>,
    mode: Mode,
    plan_ready: bool,
    limits: Limits,
}

impl Status {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_session(&mut self, backend: BackendKind, model: Option<String>) {
        self.backend = Some(backend);
        self.model = model;
        self.usage = TokenUsage::default();
        self.context = None;
        self.mode = Mode::Default;
        self.plan_ready = false;
    }

    /// Shows the mode the session is in. Leaving plan mode drops the ready plan.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if mode != Mode::Plan {
            self.plan_ready = false;
        }
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Shows whether a finished plan is waiting for `/implement`.
    pub fn set_plan_ready(&mut self, ready: bool) {
        self.plan_ready = ready;
    }

    /// Shows `model` while keeping the backend and the tokens used so far.
    pub fn switch_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Adds the tokens of a usage report to the running total and keeps how
    /// full its context was. A report that says nothing about it, such as a
    /// sub-agent's or a compaction's, leaves the meter as it was.
    pub fn apply(&mut self, event: &Event) {
        self.limits.apply(event);
        if let Event::Usage { usage, context, .. } = event {
            self.usage.cache_read_tokens = self
                .usage
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            self.usage.cache_write_tokens = self
                .usage
                .cache_write_tokens
                .saturating_add(usage.cache_write_tokens);
            self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
            self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
            if context.is_some() {
                self.context = *context;
            }
        }
    }

    #[must_use]
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// The plan limit lines `/limits` shows, one per window.
    #[must_use]
    pub fn limit_lines(&self) -> Vec<String> {
        self.limits.report(unix_now())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        self.render_with(frame, area, None);
    }

    /// Draws the status line with what the running turn is doing after it.
    pub fn render_with(&self, frame: &mut Frame, area: Rect, activity: Option<&str>) {
        self.render_at(frame, area, activity, unix_now());
    }

    fn render_at(&self, frame: &mut Frame, area: Rect, activity: Option<&str>, now: u64) {
        let backend = self.backend.map_or(NO_BACKEND, backend_name);
        let model = self.model.as_deref().unwrap_or(NO_MODEL);
        let mut line = format!(
            "{backend} | {model} | in {} out {}",
            self.usage.input_tokens, self.usage.output_tokens
        );
        if self.usage.cache_read_tokens > 0 || self.usage.cache_write_tokens > 0 {
            let _ = write!(
                line,
                " cache r {} w {}",
                self.usage.cache_read_tokens, self.usage.cache_write_tokens
            );
        }
        if let Some(part) = self.limits.status_part(now) {
            line.push_str(" | ");
            line.push_str(&part);
        }
        if let Some(meter) = self.context.and_then(meter) {
            line.push_str(" | ");
            line.push_str(&meter);
        }
        if self.mode == Mode::Plan {
            line.push_str(" | ");
            line.push_str(if self.plan_ready {
                PLAN_READY
            } else {
                PLAN_MODE
            });
        }
        if let Some(activity) = activity {
            line.push_str(" | ");
            line.push_str(activity);
        }
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `ctx 41% of 200k`; `None` when the window is zero, which has no share.
fn meter(fill: ContextFill) -> Option<String> {
    if fill.window_tokens == 0 {
        return None;
    }
    let window = u128::from(fill.window_tokens);
    let share = u128::from(fill.used_tokens) * PERCENT / window;
    Some(format!(
        "ctx {}% of {}",
        share.min(PERCENT),
        window_text(fill.window_tokens)
    ))
}

fn window_text(tokens: u32) -> String {
    if tokens >= MILLION {
        format!("{}M", tokens / MILLION)
    } else if tokens >= THOUSAND {
        format!("{}k", tokens / THOUSAND)
    } else {
        tokens.to_string()
    }
}

/// The mode as the user reads it.
pub(crate) fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Default => "default",
        Mode::Plan => "plan",
        Mode::AcceptEdits => "accept-edits",
        _ => UNKNOWN_MODE,
    }
}

pub(crate) fn backend_name(kind: BackendKind) -> &'static str {
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
        usage_with_context(input, output, None)
    }

    fn usage_with_context(input: u64, output: u64, context: Option<ContextFill>) -> Event {
        Event::Usage {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                ..TokenUsage::default()
            },
            context,
        }
    }

    fn fill(used_tokens: u64, window_tokens: u32) -> ContextFill {
        ContextFill {
            used_tokens,
            window_tokens,
        }
    }

    fn draw(status: &Status, width: u16) -> String {
        screen(width, 1, |frame| status.render(frame, frame.area()))
    }

    fn limit_event(bps: u32, resets_at: u64) -> Event {
        Event::RateLimit {
            session_id: SessionId::new("s-1"),
            limit: coda_protocol::events::RateLimit {
                status: "allowed".to_owned(),
                window_type: None,
                resets_at: None,
                windows: vec![coda_protocol::events::RateLimitWindow {
                    name: "five_hour".to_owned(),
                    utilization_bps: bps,
                    resets_at: Some(resets_at),
                }],
                is_using_overage: false,
                overage_status: None,
                overage_resets_at: None,
                overage_disabled_reason: None,
            },
        }
    }

    #[test]
    fn cache_tokens_show_only_once_there_are_some() {
        // Arrange
        let mut status = Status::new();
        let report = Event::Usage {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 6,
                cache_read_tokens: 700,
                cache_write_tokens: 80,
            },
            context: None,
        };

        // Act
        let before = draw(&status, 60);
        status.apply(&report);
        status.apply(&report);
        let after = draw(&status, 60);

        // Assert
        assert_eq!(before.trim_end(), "no backend | no model | in 0 out 0");
        assert_eq!(
            after.trim_end(),
            "no backend | no model | in 10 out 12 cache r 1400 w 160"
        );
    }

    #[test]
    fn a_reported_limit_joins_the_status_line_until_it_resets() {
        // Arrange
        let mut status = Status::new();
        status.apply(&limit_event(3800, 10_000));
        let line = |now| {
            screen(80, 1, |frame| {
                status.render_at(frame, frame.area(), None, now);
            })
        };

        // Act
        let before = line(10_000 - 7_200);
        let after = line(10_000);

        // Assert
        assert_eq!(
            before.trim_end(),
            "no backend | no model | in 0 out 0 | 5h 62% left, resets in 2h 00m, under"
        );
        assert_eq!(after.trim_end(), "no backend | no model | in 0 out 0");
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
    fn a_plan_mode_session_says_so_and_the_default_mode_says_nothing() {
        // Arrange
        let mut status = Status::new();
        status.set_session(BackendKind::ClaudeCode, Some("sonnet".to_owned()));

        // Act
        status.set_mode(Mode::Plan);
        let planning = draw(&status, 60);
        status.set_mode(Mode::Default);
        let normal = draw(&status, 60);

        // Assert
        assert_eq!(
            planning.trim_end(),
            "claude_code | sonnet | in 0 out 0 | plan mode"
        );
        assert_eq!(normal.trim_end(), "claude_code | sonnet | in 0 out 0");
    }

    #[test]
    fn a_plan_that_is_ready_says_so_and_leaving_plan_mode_clears_it() {
        // Arrange
        let mut status = Status::new();
        status.set_session(BackendKind::ClaudeCode, Some("sonnet".to_owned()));
        status.set_mode(Mode::Plan);

        // Act
        status.set_plan_ready(true);
        let ready = draw(&status, 60);
        status.set_mode(Mode::Default);
        let left = draw(&status, 60);

        // Assert
        assert_eq!(
            ready.trim_end(),
            "claude_code | sonnet | in 0 out 0 | plan ready"
        );
        assert_eq!(left.trim_end(), "claude_code | sonnet | in 0 out 0");
    }

    #[test]
    fn a_new_session_starts_in_the_default_mode_until_it_reports_another() {
        // Arrange
        let mut status = Status::new();
        status.set_mode(Mode::Plan);

        // Act
        status.set_session(BackendKind::ClaudeCode, None);

        // Assert
        assert_eq!(
            draw(&status, 60).trim_end(),
            "claude_code | no model | in 0 out 0"
        );
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
    fn the_context_meter_shows_the_share_used_of_the_window() {
        // Arrange
        let mut status = Status::new();
        status.set_session(BackendKind::ClaudeCode, Some("sonnet".to_owned()));

        // Act
        status.apply(&usage_with_context(3, 4, Some(fill(82_000, 200_000))));

        // Assert
        assert_eq!(
            draw(&status, 60).trim_end(),
            "claude_code | sonnet | in 3 out 4 | ctx 41% of 200k"
        );
    }

    #[test]
    fn a_window_reads_in_thousands_and_millions() {
        for (window, text) in [
            (200_000, "200k"),
            (258_400, "258k"),
            (1_000_000, "1M"),
            (1_048_576, "1M"),
            (8_192, "8k"),
            (512, "512"),
        ] {
            // Arrange
            let mut status = Status::new();

            // Act
            status.apply(&usage_with_context(0, 0, Some(fill(0, window))));

            // Assert
            let expected = format!("in 0 out 0 | ctx 0% of {text}");
            assert!(draw(&status, 80).contains(&expected), "{window}");
        }
    }

    #[test]
    fn a_full_or_overfull_context_reads_one_hundred_percent() {
        // Arrange
        let mut status = Status::new();

        // Act
        status.apply(&usage_with_context(0, 0, Some(fill(250_000, 200_000))));

        // Assert
        assert!(draw(&status, 80).contains("ctx 100% of 200k"));
    }

    #[test]
    fn the_meter_follows_the_last_report_and_a_report_without_one_leaves_it() {
        // Arrange
        let mut status = Status::new();
        status.apply(&usage_with_context(0, 0, Some(fill(20_000, 200_000))));

        // Act
        status.apply(&usage_with_context(0, 0, Some(fill(100_000, 200_000))));
        let later = draw(&status, 80);
        status.apply(&usage_event(1, 1));

        // Assert
        assert!(later.contains("ctx 50% of 200k"), "{later}");
        assert!(draw(&status, 80).contains("ctx 50% of 200k"));
    }

    #[test]
    fn a_nearly_full_context_does_not_read_one_hundred_percent() {
        // Arrange
        let mut status = Status::new();

        // Act
        status.apply(&usage_with_context(0, 0, Some(fill(199_900, 200_000))));

        // Assert
        assert!(draw(&status, 80).contains("ctx 99% of 200k"));
    }

    #[test]
    fn a_new_session_starts_without_a_meter() {
        // Arrange
        let mut status = Status::new();
        status.apply(&usage_with_context(0, 0, Some(fill(20_000, 200_000))));

        // Act
        status.set_session(BackendKind::Faux, None);

        // Assert
        assert!(!draw(&status, 80).contains("ctx"));
    }

    #[test]
    fn a_window_of_zero_shows_no_meter() {
        // Arrange
        let mut status = Status::new();

        // Act
        status.apply(&usage_with_context(0, 0, Some(fill(5, 0))));

        // Assert
        assert!(!draw(&status, 80).contains("ctx"));
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
            cause: None,
        });

        // Assert
        assert_eq!(status.usage().input_tokens, 1);
        for width in [0, 1, 5] {
            let _ = draw(&status, width);
        }
    }
}
