// SPDX-License-Identifier: Apache-2.0

//! A rewind keeps the turns it undid in the store and out of the conversation.

use coda_core::history::{ASSISTANT_MESSAGE_KIND, Rebuild, Transcript, USER_MESSAGE_KIND};
use coda_core::prompt::{Message, Prefix};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{EventRecord, NewSession, Store};
use serde_json::json;
use tempfile::TempDir;

const BUDGET: usize = 10_000;

fn said(store: &mut Store, session: &SessionId, turn: &coda_protocol::types::TurnId, text: &str) {
    for (kind, body) in [
        (USER_MESSAGE_KIND, json!({ "text": text })),
        (
            ASSISTANT_MESSAGE_KIND,
            json!({ "text": format!("re {text}") }),
        ),
    ] {
        store
            .append_event(
                session,
                turn,
                &EventRecord {
                    kind: kind.to_owned(),
                    body: body.to_string(),
                },
            )
            .unwrap();
    }
}

fn two_turns() -> (TempDir, Store, SessionId) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/ws".to_owned(),
            mode: Mode::default(),
        })
        .unwrap();
    let first = store.begin_turn(&session).unwrap();
    said(&mut store, &session, &first, "one");
    let second = store.begin_turn(&session).unwrap();
    said(&mut store, &session, &second, "two");
    store.roll_back_turns_after(&session, Some(&first)).unwrap();
    (dir, store, session)
}

#[test]
fn a_rebuilt_conversation_leaves_out_a_rolled_back_turn() {
    // Arrange
    let (_dir, store, session) = two_turns();
    let mut replay = Rebuild::new(Prefix::new("You are Coda.", Vec::new(), "").unwrap());

    // Act
    replay.apply_page(&store, &session, 0).unwrap();
    let messages = replay.finish();

    // Assert
    let users: Vec<_> = messages
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["one"]);
}

#[test]
fn a_transcript_leaves_out_a_rolled_back_turn() {
    // Arrange
    let (_dir, store, session) = two_turns();
    let mut transcript = Transcript::new(BUDGET);

    // Act
    transcript.apply_page(&store, &session, 0).unwrap();

    // Assert
    let text = transcript.render().unwrap();
    assert!(text.contains("one"), "{text}");
    assert!(!text.contains("two"), "{text}");
}
