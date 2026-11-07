// SPDX-License-Identifier: Apache-2.0

//! Model fallback chains in a model-API session: the switch is announced and
//! stored once, and the send finishes on the next model of the chain.

use coda_config::config::{Config, ModelAlias};
use coda_protocol::events::{MODEL_FALLBACK_KIND, ModelFallbackBody};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use super::{Rig, text_turn};

const PRIMARY: &str = "scripted";
const BACKUP: &str = "backup";

fn chain(config: &mut Config) {
    for (name, model, fallback) in [
        (PRIMARY, "script-1", vec![BACKUP.to_owned()]),
        (BACKUP, "script-2", Vec::new()),
    ] {
        config.models.insert(
            name.to_owned(),
            ModelAlias {
                provider: "faux".to_owned(),
                model: model.to_owned(),
                context_window: None,
                fallback,
            },
        );
    }
}

fn rate_limited() -> Value {
    json!({"events": [], "error": {"class": "retryable", "message": "HTTP 429"}})
}

#[tokio::test]
async fn a_rate_limited_model_hands_the_send_to_its_fallback_and_the_switch_is_stored_once() {
    // Arrange
    let script = [rate_limited(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, Some("script-1"), chain);

    // Act
    rig.send("go").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;

    // Assert
    let stored = rig.bodies(MODEL_FALLBACK_KIND).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let body: ModelFallbackBody = serde_json::from_value(stored[0].clone()).unwrap();
    assert_eq!((body.from.as_str(), body.to.as_str()), (PRIMARY, BACKUP));
    assert!(body.reason.contains("429"), "{body:?}");
    assert_eq!(rig.requests().len(), 2);
    assert_eq!(rig.requests()[0], rig.requests()[1]);
}

#[tokio::test]
async fn a_model_without_a_chain_fails_the_send_as_before() {
    // Arrange
    let script = [rate_limited(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, Some("script-1"), |config| {
        chain(config);
        config.models.get_mut(PRIMARY).unwrap().fallback.clear();
    });

    // Act
    rig.send("go").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.bodies(MODEL_FALLBACK_KIND).await.len(), 0);
    assert_eq!(rig.requests().len(), 1);
}
