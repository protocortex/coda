// SPDX-License-Identifier: Apache-2.0

//! Which login a backend runs on, and how it is billed.
//!
//! Coda reads only what the backend binary prints. For Claude Code that is the
//! `apiKeySource` of the `init` frame of a session, and `claude auth status
//! --json` outside one. The initialize control response of the stream carries the
//! account's email and plan but neither `apiKeySource` nor `tokenSource`
//! (claude 2.1.296), and starting a session to ask runs the user's hooks, so the
//! probe uses `auth status`, which starts none. For Codex it is `account/read`.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use coda_protocol::events::AccountBilling;
use serde_json::Value;

use crate::claude_session::allowlisted;
use crate::detect::run_bounded;

/// `apiKeySource` of an `init` frame that has no API key behind the session.
const NO_API_KEY: &str = "none";
/// `apiKeySource` the Agent SDK documents for a subscription login.
const OAUTH_SOURCE: &str = "oauth";
const CLAUDE_AUTH_ARGS: &[&str] = &["auth", "status", "--json"];
const CLAUDE_SUBSCRIPTION_METHOD: &str = "claude.ai";
const CLAUDE_API_KEY_METHOD: &str = "api_key";
const CODEX_API_KEY_TYPE: &str = "apiKey";
const CODEX_SUBSCRIPTION_TYPE: &str = "chatgpt";

/// A login a backend is signed in with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Whether usage is billed to a subscription or at API rates.
    pub billing: AccountBilling,
    /// The subscription plan, as the backend names it.
    pub plan: Option<String>,
    /// The account's email, when the backend prints one. Shown to the user only.
    pub email: Option<String>,
    /// Where the API key came from, when one is in use.
    pub api_key_source: Option<String>,
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    /// The backend has a login.
    LoggedIn(Account),
    /// The backend says it has none.
    LoggedOut,
}

/// How a session is billed, from the `apiKeySource` of its `init` frame.
#[must_use]
pub fn billing_of_api_key_source(source: &str) -> AccountBilling {
    if source == NO_API_KEY || source == OAUTH_SOURCE {
        AccountBilling::Subscription
    } else {
        AccountBilling::ApiKey
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// Reads the output of `claude auth status --json`; `None` when it is not that JSON.
#[must_use]
pub fn parse_claude_status(stdout: &str) -> Option<AccountStatus> {
    let status: Value = serde_json::from_str(stdout).ok()?;
    if !status["loggedIn"].as_bool()? {
        return Some(AccountStatus::LoggedOut);
    }
    let api_key_source = text(&status["apiKeySource"]).filter(|source| source != NO_API_KEY);
    let billing = match status["authMethod"].as_str() {
        _ if api_key_source.is_some() => AccountBilling::ApiKey,
        Some(CLAUDE_API_KEY_METHOD) => AccountBilling::ApiKey,
        Some(CLAUDE_SUBSCRIPTION_METHOD) => AccountBilling::Subscription,
        _ => AccountBilling::Other,
    };
    Some(AccountStatus::LoggedIn(Account {
        billing,
        plan: text(&status["subscriptionType"]),
        email: text(&status["email"]),
        api_key_source,
    }))
}

/// Reads the result of the Codex `account/read` request.
#[must_use]
pub fn parse_codex_account(result: &Value) -> AccountStatus {
    let account = &result["account"];
    if account.is_null() && result["requiresOpenaiAuth"].as_bool().unwrap_or(true) {
        return AccountStatus::LoggedOut;
    }
    let (billing, plan, email) = match account["type"].as_str() {
        Some(CODEX_API_KEY_TYPE) => (AccountBilling::ApiKey, None, None),
        Some(CODEX_SUBSCRIPTION_TYPE) => (
            AccountBilling::Subscription,
            text(&account["planType"]),
            text(&account["email"]),
        ),
        _ => (AccountBilling::Other, None, None),
    };
    AccountStatus::LoggedIn(Account {
        billing,
        plan,
        email,
        api_key_source: None,
    })
}

/// Asks the Claude Code binary whether it is logged in, with the environment a
/// session would get, so an API key coda does not pass on shows as absent.
///
/// # Errors
/// Returns text saying why the binary could not be asked or answered nothing usable.
pub fn probe_claude(
    binary: &Path,
    parent_env: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<AccountStatus, String> {
    let env: Vec<(&OsStr, &OsStr)> = allowlisted(parent_env)
        .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
        .collect();
    // A logged out binary exits non-zero but still prints the JSON, so only the output counts.
    let ran = run_bounded(binary, CLAUDE_AUTH_ARGS, &env, timeout, "auth status")?;
    parse_claude_status(&ran.stdout)
        .ok_or_else(|| format!("auth status printed nothing usable (exit {})", ran.status))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const SUBSCRIPTION: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"me@example.com","orgId":"o","orgName":"n","subscriptionType":"max"}"#;
    const API_KEY: &str = r#"{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty","apiKeySource":"ANTHROPIC_API_KEY"}"#;
    const LOGGED_OUT: &str = r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#;

    #[test]
    fn no_api_key_source_means_a_subscription_and_any_other_means_an_api_key() {
        assert_eq!(
            billing_of_api_key_source("none"),
            AccountBilling::Subscription
        );
        assert_eq!(
            billing_of_api_key_source("ANTHROPIC_API_KEY"),
            AccountBilling::ApiKey
        );
        assert_eq!(
            billing_of_api_key_source("/login managed key"),
            AccountBilling::ApiKey
        );
    }

    #[test]
    fn an_oauth_source_is_a_subscription_too() {
        assert_eq!(
            billing_of_api_key_source("oauth"),
            AccountBilling::Subscription
        );
    }

    #[test]
    fn an_auth_status_that_names_no_key_source_is_not_an_api_key_login() {
        // Arrange
        let text = r#"{"loggedIn":true,"authMethod":"claude.ai","apiKeySource":"none"}"#;

        // Act
        let status = parse_claude_status(text);

        // Assert
        assert!(matches!(
            status,
            Some(AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Subscription,
                api_key_source: None,
                ..
            }))
        ));
    }

    #[test]
    fn a_subscription_login_has_a_plan_and_an_email_and_no_key() {
        // Act
        let status = parse_claude_status(SUBSCRIPTION);

        // Assert
        assert_eq!(
            status,
            Some(AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Subscription,
                plan: Some("max".to_owned()),
                email: Some("me@example.com".to_owned()),
                api_key_source: None,
            }))
        );
    }

    #[test]
    fn an_api_key_login_names_where_the_key_came_from() {
        // Act
        let status = parse_claude_status(API_KEY);

        // Assert
        assert_eq!(
            status,
            Some(AccountStatus::LoggedIn(Account {
                billing: AccountBilling::ApiKey,
                plan: None,
                email: None,
                api_key_source: Some("ANTHROPIC_API_KEY".to_owned()),
            }))
        );
    }

    #[test]
    fn a_logged_out_binary_and_text_that_is_not_json_are_told_apart() {
        assert_eq!(
            parse_claude_status(LOGGED_OUT),
            Some(AccountStatus::LoggedOut)
        );
        assert_eq!(parse_claude_status("Not logged in"), None);
        assert_eq!(parse_claude_status(r#"{"other":1}"#), None);
    }

    #[test]
    fn a_login_method_coda_does_not_know_is_other() {
        // Arrange
        let text = r#"{"loggedIn":true,"authMethod":"bedrock"}"#;

        // Act
        let status = parse_claude_status(text);

        // Assert
        assert!(matches!(
            status,
            Some(AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Other,
                ..
            }))
        ));
    }

    #[test]
    fn a_codex_chatgpt_account_is_a_subscription_with_its_plan() {
        // Arrange
        let result = json!({"account": {"type": "chatgpt", "email": "me@example.com", "planType": "pro"}, "requiresOpenaiAuth": true});

        // Act
        let status = parse_codex_account(&result);

        // Assert
        assert_eq!(
            status,
            AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Subscription,
                plan: Some("pro".to_owned()),
                email: Some("me@example.com".to_owned()),
                api_key_source: None,
            })
        );
    }

    #[test]
    fn a_codex_api_key_account_is_billed_at_api_rates() {
        // Arrange
        let result = json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true});

        // Act
        let status = parse_codex_account(&result);

        // Assert
        assert!(matches!(
            status,
            AccountStatus::LoggedIn(Account {
                billing: AccountBilling::ApiKey,
                ..
            })
        ));
    }

    #[test]
    fn a_codex_without_an_account_is_logged_out_only_when_it_needs_one() {
        assert_eq!(
            parse_codex_account(&json!({"account": null, "requiresOpenaiAuth": true})),
            AccountStatus::LoggedOut
        );
        assert!(matches!(
            parse_codex_account(&json!({"account": null, "requiresOpenaiAuth": false})),
            AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Other,
                ..
            })
        ));
        assert!(matches!(
            parse_codex_account(&json!({"account": {"type": "amazonBedrock"}})),
            AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Other,
                ..
            })
        ));
    }

    fn script(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    const PATIENT: Duration = Duration::from_secs(60);

    #[test]
    fn the_probe_runs_auth_status_and_reads_a_logged_out_exit() {
        // Arrange
        let (_dir, path) = script(&format!(
            "[ \"$1 $2 $3\" = \"auth status --json\" ] || exit 9\necho '{LOGGED_OUT}'\nexit 1"
        ));

        // Act
        let status = probe_claude(&path, &[], PATIENT);

        // Assert
        assert_eq!(status, Ok(AccountStatus::LoggedOut));
    }

    #[test]
    fn the_probe_gives_the_binary_only_the_session_environment() {
        // Arrange
        let (_dir, path) = script(&format!(
            "if [ -n \"$ANTHROPIC_API_KEY\" ]; then echo '{API_KEY}'; else echo '{SUBSCRIPTION}'; fi"
        ));
        let env = vec![
            (OsString::from("ANTHROPIC_API_KEY"), OsString::from("x")),
            (OsString::from("HOME"), OsString::from("/tmp")),
        ];

        // Act
        let status = probe_claude(&path, &env, PATIENT).unwrap();

        // Assert
        assert!(matches!(
            status,
            AccountStatus::LoggedIn(Account {
                billing: AccountBilling::Subscription,
                ..
            })
        ));
    }

    #[test]
    fn a_probe_that_prints_nothing_usable_is_an_error() {
        // Arrange
        let (_dir, path) = script("echo 'unknown command'; exit 1");

        // Act
        let status = probe_claude(&path, &[], PATIENT);

        // Assert
        assert!(status.unwrap_err().contains("auth status"));
    }
}
