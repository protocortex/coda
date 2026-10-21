// SPDX-License-Identifier: AGPL-3.0-only

//! Facts about the models Coda knows by name.

/// Default context window of the Claude models listed below, in tokens. These
/// are defaults Coda assumes, not published figures; the larger beta windows
/// are not assumed, and a config override can change one.
const CLAUDE_CONTEXT_WINDOW: u32 = 200_000;
/// Default context window of `gpt-4o`, in tokens.
const GPT_4O_CONTEXT_WINDOW: u32 = 128_000;

/// Context windows by model id.
const CONTEXT_WINDOWS: &[(&str, u32)] = &[
    ("claude-sonnet-5-5", CLAUDE_CONTEXT_WINDOW),
    ("claude-sonnet-4-5", CLAUDE_CONTEXT_WINDOW),
    ("gpt-4o", GPT_4O_CONTEXT_WINDOW),
];

/// The context window of `model` in tokens, or `None` for a model not in the
/// table.
#[must_use]
pub fn context_window(model: &str) -> Option<u32> {
    CONTEXT_WINDOWS
        .iter()
        .find(|(known, _)| *known == model)
        .map(|&(_, window)| window)
}
