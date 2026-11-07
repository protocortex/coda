// SPDX-License-Identifier: Apache-2.0

//! Splits the reasoning that a local server prints inside `<think>` tags out of
//! the text of a reply.
//!
//! The split is a small state machine fed one chunk at a time, so a tag cut
//! across chunks is still found. Only a reply that begins with the tag is read
//! as having reasoning, which keeps text that merely mentions the tag as text.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// A piece of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Answer text.
    Text(String),
    /// Reasoning text, without the tags.
    Reasoning(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Nothing but white space seen; the reply may still open with the tag.
    #[default]
    Start,
    /// Inside the tags.
    Reasoning,
    /// Just past the closing tag: white space before the answer is dropped.
    AfterReasoning,
    /// Answer text, with no more tags to look for.
    Text,
}

/// Splits one reply's text chunks into [`Piece`]s.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    state: State,
    /// Text held back because it may be the start of a tag.
    held: String,
}

impl ThinkSplitter {
    /// Feeds the next chunk and returns what is now known to be text or reasoning.
    pub fn push(&mut self, chunk: &str) -> Vec<Piece> {
        self.held.push_str(chunk);
        let mut pieces = Vec::new();
        loop {
            let advanced = match self.state {
                State::Start => self.start(),
                State::Reasoning => self.reasoning(&mut pieces),
                State::AfterReasoning => self.after_reasoning(),
                State::Text => {
                    let text = std::mem::take(&mut self.held);
                    if !text.is_empty() {
                        pieces.push(Piece::Text(text));
                    }
                    false
                }
            };
            if !advanced {
                return pieces;
            }
        }
    }

    /// Ends the reply and returns what was held back. A reply that ends inside
    /// the tags keeps what it said as reasoning, and a half-written tag is text.
    pub fn finish(&mut self) -> Vec<Piece> {
        let held = std::mem::take(&mut self.held);
        let piece = match self.state {
            State::Reasoning => Piece::Reasoning(held),
            State::Start | State::AfterReasoning | State::Text => Piece::Text(held),
        };
        self.state = State::Text;
        match piece {
            Piece::Text(text) | Piece::Reasoning(text) if text.is_empty() => Vec::new(),
            piece => vec![piece],
        }
    }

    fn start(&mut self) -> bool {
        let trimmed = self.held.trim_start();
        if let Some(rest) = trimmed.strip_prefix(OPEN) {
            self.held = rest.to_owned();
            self.state = State::Reasoning;
            return true;
        }
        if trimmed.is_empty() || OPEN.starts_with(trimmed) {
            return false;
        }
        self.state = State::Text;
        true
    }

    fn reasoning(&mut self, pieces: &mut Vec<Piece>) -> bool {
        if let Some(at) = self.held.find(CLOSE) {
            let after = self.held.split_off(at + CLOSE.len());
            self.held.truncate(at);
            let reasoning = std::mem::replace(&mut self.held, after);
            if !reasoning.is_empty() {
                pieces.push(Piece::Reasoning(reasoning));
            }
            self.state = State::AfterReasoning;
            return true;
        }
        let keep = partial_suffix(&self.held, CLOSE);
        let ready = self.held.len() - keep;
        if ready > 0 {
            let rest = self.held.split_off(ready);
            let ready = std::mem::replace(&mut self.held, rest);
            pieces.push(Piece::Reasoning(ready));
        }
        false
    }

    fn after_reasoning(&mut self) -> bool {
        let trimmed = self.held.trim_start().len();
        self.held.drain(..self.held.len() - trimmed);
        if self.held.is_empty() {
            return false;
        }
        self.state = State::Text;
        true
    }
}

/// How many bytes at the end of `text` are the start of `tag`, and so cannot
/// be sent on before the next chunk shows whether the tag completes.
fn partial_suffix(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&len| {
            text.len() >= len
                && text.is_char_boundary(text.len() - len)
                && text.ends_with(&tag[..len])
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> (String, String) {
        let mut splitter = ThinkSplitter::default();
        let mut pieces = Vec::new();
        for chunk in chunks {
            pieces.extend(splitter.push(chunk));
        }
        pieces.extend(splitter.finish());
        let (mut text, mut reasoning) = (String::new(), String::new());
        for piece in pieces {
            match piece {
                Piece::Text(piece) => text.push_str(&piece),
                Piece::Reasoning(piece) => reasoning.push_str(&piece),
            }
        }
        (text, reasoning)
    }

    #[test]
    fn a_whole_block_is_split_from_the_answer() {
        // Act
        let (text, reasoning) = run(&["<think>step one\nstep two</think>\n\nThe answer."]);

        // Assert
        assert_eq!(reasoning, "step one\nstep two");
        assert_eq!(text, "The answer.");
    }

    #[test]
    fn a_tag_cut_across_three_chunks_is_removed() {
        // Act
        let opening = run(&["<th", "in", "k>why", "</think>done"]);
        let closing = run(&["<think>why</", "thi", "nk>", "done"]);

        // Assert
        assert_eq!(opening, ("done".to_owned(), "why".to_owned()));
        assert_eq!(closing, ("done".to_owned(), "why".to_owned()));
    }

    #[test]
    fn the_same_text_split_at_every_byte_gives_the_same_result() {
        // Arrange
        let whole = "  <think>a < b, </th and <thin</think>  Answer <think>kept</think>";
        let expected = run(&[whole]);
        let boundaries: Vec<usize> = whole.char_indices().map(|(at, _)| at).collect();

        for &first in &boundaries {
            for &second in boundaries.iter().filter(|&&at| at >= first) {
                // Act
                let result = run(&[&whole[..first], &whole[first..second], &whole[second..]]);

                // Assert
                assert_eq!(result, expected, "cuts at {first} and {second}");
            }
        }
    }

    #[test]
    fn text_that_only_looks_like_a_tag_is_kept() {
        for text in [
            "I will <think> about it",
            "<thinking>not a tag</thinking>",
            "<thinker>x</thinker>",
            "a </think> b",
            "<think",
            "< think>x</think>",
            "plain text",
        ] {
            // Act
            let (kept, reasoning) = run(&[text]);

            // Assert
            assert_eq!(kept, text, "{text}");
            assert_eq!(reasoning, "", "{text}");
        }
    }

    #[test]
    fn a_reply_that_ends_inside_the_tags_keeps_its_reasoning() {
        // Act
        let (text, reasoning) = run(&["<think>cut off mid", " thought"]);

        // Assert
        assert_eq!(text, "");
        assert_eq!(reasoning, "cut off mid thought");
    }

    #[test]
    fn a_reply_without_reasoning_passes_through_unchanged() {
        // Act
        let (text, reasoning) = run(&["Hello", ", ", "world"]);

        // Assert
        assert_eq!((text.as_str(), reasoning.as_str()), ("Hello, world", ""));
    }

    #[test]
    fn multibyte_text_next_to_a_partial_tag_is_not_split() {
        // Act
        let (text, reasoning) = run(&["<think>é</th", "é</think>ü"]);

        // Assert
        assert_eq!(reasoning, "é</thé");
        assert_eq!(text, "ü");
    }
}
