// SPDX-License-Identifier: Apache-2.0

//! The same fixtures against a real model. It spends money, so it never runs in
//! CI: the test is `#[ignore]`, and it also needs `CODA_LIVE_EVALS=1` and a
//! variable holding an API key. Run it with
//! `CODA_LIVE_EVALS=1 ANTHROPIC_API_KEY=... cargo test -p coda-core --test evals -- --ignored live`.

use std::fmt::Write as _;

use coda_config::config::Config;
use coda_protocol::types::BackendKind;
use coda_providers::registry::ModelRegistry;

use crate::fixtures;
use crate::runner;

const FLAG_VAR: &str = "CODA_LIVE_EVALS";
const KEY_VAR_NAME_VAR: &str = "CODA_EVAL_KEY_ENV";
const DEFAULT_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const MODEL_VAR: &str = "CODA_EVAL_MODEL";

/// The key variable the run reads, when the flag is set and the variable holds a key.
fn key_variable() -> Result<String, String> {
    if std::env::var(FLAG_VAR).as_deref() != Ok("1") {
        return Err(format!("set {FLAG_VAR}=1 to run the live evals"));
    }
    let name = std::env::var(KEY_VAR_NAME_VAR).unwrap_or_else(|_| DEFAULT_KEY_VAR.to_owned());
    match std::env::var(&name) {
        Ok(value) if !value.is_empty() => Ok(name),
        _ => Err(format!("{name} must hold an API key for the live evals")),
    }
}

#[tokio::test]
#[ignore = "spends API credits; needs CODA_LIVE_EVALS=1 and an API key variable"]
async fn live_fixtures_pass_with_a_real_model() {
    let key_var = match key_variable() {
        Ok(name) => name,
        Err(reason) => panic!("the live evals were asked for but cannot run: {reason}"),
    };
    let mut toml = format!("[providers.anthropic]\nenabled = true\napi_key_env = \"{key_var}\"\n");
    if let Ok(model) = std::env::var(MODEL_VAR) {
        writeln!(toml, "model = \"{model}\"").unwrap();
    }
    let registry = ModelRegistry::from_config(&Config::from_toml_str(&toml).unwrap().config);
    let target = registry.select(BackendKind::Anthropic, None).unwrap();

    let mut outcomes = Vec::new();
    for fixture in fixtures::all() {
        let client = registry
            .build(&target, |name| std::env::var(name).ok())
            .unwrap();
        outcomes.push(runner::run(&fixture, client).await);
    }

    println!("{}", runner::report(&outcomes));
    let failed: Vec<_> = outcomes.iter().filter(|o| !o.passed).collect();
    assert!(failed.is_empty(), "{failed:#?}");
}
