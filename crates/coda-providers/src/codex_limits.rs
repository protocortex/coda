// SPDX-License-Identifier: Apache-2.0

//! Reads the plan limits that the Codex app-server reports.
//!
//! The shapes come from the `RateLimitSnapshot` of the schema that codex-cli
//! 0.163.0 prints with `codex app-server generate-json-schema`. They were not
//! seen live, so every field is read defensively.

use coda_protocol::events::{RateLimit, RateLimitWindow};
use serde_json::Value;

/// The request sent on the first turn of a session.
pub(crate) const READ_METHOD: &str = "account/rateLimits/read";
/// The notification that carries a changed snapshot.
pub(crate) const UPDATED_METHOD: &str = "account/rateLimits/updated";
/// `CodexErrorInfo` value of a turn that ended at the plan limit.
const USAGE_LIMIT_ERROR: &str = "usageLimitExceeded";
/// The status the Claude adapter uses for a window that ran out.
pub(crate) const REJECTED: &str = "rejected";

const ALLOWED: &str = "allowed";
const PRIMARY: &str = "primary";
const SECONDARY: &str = "secondary";
const FIVE_HOUR_MINUTES: u64 = 300;
const SEVEN_DAY_MINUTES: u64 = 10_080;
const FIVE_HOUR: &str = "five_hour";
const SEVEN_DAY: &str = "seven_day";
const BASIS_POINTS_PER_PERCENT: u32 = 100;
const FULL_BASIS_POINTS: u32 = 10_000;

/// The limits of a snapshot, or `None` when it names no window and no stop.
///
/// A window is named by its length, so a five-hour window is `five_hour`
/// whichever slot Codex put it in; a length coda has no name for keeps the slot
/// name. The status is `rejected` when Codex says a limit was reached or a
/// window is full.
pub(crate) fn rate_limit(snapshot: &Value) -> Option<RateLimit> {
    let windows: Vec<RateLimitWindow> = [PRIMARY, SECONDARY]
        .into_iter()
        .filter_map(|slot| window(slot, &snapshot[slot]))
        .collect();
    let reached = snapshot["rateLimitReachedType"].is_string();
    if windows.is_empty() && !reached {
        return None;
    }
    let full = windows
        .iter()
        .find(|window| window.utilization_bps >= FULL_BASIS_POINTS);
    let rejected = reached || full.is_some();
    Some(RateLimit {
        status: if rejected { REJECTED } else { ALLOWED }.to_owned(),
        window_type: full.map(|window| window.name.clone()),
        resets_at: full.and_then(|window| window.resets_at),
        windows,
        is_using_overage: false,
        overage_status: None,
        overage_resets_at: None,
        overage_disabled_reason: None,
    })
}

fn window(slot: &str, value: &Value) -> Option<RateLimitWindow> {
    let percent = u32::try_from(value["usedPercent"].as_u64()?).ok()?;
    Some(RateLimitWindow {
        name: match value["windowDurationMins"].as_u64() {
            Some(FIVE_HOUR_MINUTES) => FIVE_HOUR.to_owned(),
            Some(SEVEN_DAY_MINUTES) => SEVEN_DAY.to_owned(),
            _ => slot.to_owned(),
        },
        utilization_bps: percent.saturating_mul(BASIS_POINTS_PER_PERCENT),
        resets_at: value["resetsAt"].as_u64(),
    })
}

/// True when a turn's error says the plan limit ended it.
pub(crate) fn is_usage_limit_error(error: &Value) -> bool {
    error["codexErrorInfo"] == USAGE_LIMIT_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn windows_are_named_by_their_length_and_scaled_to_basis_points() {
        // Arrange
        let snapshot = json!({
            "primary": {"usedPercent": 62, "windowDurationMins": 300, "resetsAt": 1_794_000_000},
            "secondary": {"usedPercent": 9, "windowDurationMins": 10080, "resetsAt": 1_794_500_000},
        });

        // Act
        let limit = rate_limit(&snapshot).unwrap();

        // Assert
        assert_eq!(limit.status, "allowed");
        assert_eq!(limit.window_type, None);
        assert_eq!(
            limit.windows,
            vec![
                RateLimitWindow {
                    name: "five_hour".to_owned(),
                    utilization_bps: 6200,
                    resets_at: Some(1_794_000_000),
                },
                RateLimitWindow {
                    name: "seven_day".to_owned(),
                    utilization_bps: 900,
                    resets_at: Some(1_794_500_000),
                },
            ]
        );
    }

    #[test]
    fn a_full_window_makes_the_status_rejected_and_names_the_window() {
        // Arrange
        let snapshot = json!({
            "primary": {"usedPercent": 40, "windowDurationMins": 300, "resetsAt": 1},
            "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 2},
        });

        // Act
        let limit = rate_limit(&snapshot).unwrap();

        // Assert
        assert_eq!(limit.status, "rejected");
        assert_eq!(limit.window_type.as_deref(), Some("seven_day"));
        assert_eq!(limit.resets_at, Some(2));
    }

    #[test]
    fn a_reached_type_rejects_even_when_no_window_is_full() {
        // Arrange
        let snapshot = json!({
            "primary": {"usedPercent": 10, "windowDurationMins": 300},
            "rateLimitReachedType": "workspace_member_credits_depleted",
        });

        // Act
        let limit = rate_limit(&snapshot).unwrap();

        // Assert
        assert_eq!(limit.status, "rejected");
        assert_eq!(limit.window_type, None);
    }

    #[test]
    fn a_window_of_an_unknown_length_keeps_its_slot_name() {
        // Arrange
        let snapshot = json!({"primary": {"usedPercent": 5}});

        // Act
        let limit = rate_limit(&snapshot).unwrap();

        // Assert
        assert_eq!(limit.windows[0].name, "primary");
        assert_eq!(limit.windows[0].resets_at, None);
    }

    #[test]
    fn a_snapshot_with_no_window_and_no_stop_says_nothing() {
        // Arrange
        let empty = json!({"primary": null, "secondary": null, "credits": {"hasCredits": true}});
        let junk = json!({"primary": {"usedPercent": "lots"}});

        // Act and Assert
        assert_eq!(rate_limit(&empty), None);
        assert_eq!(rate_limit(&junk), None);
        assert_eq!(rate_limit(&Value::Null), None);
    }

    #[test]
    fn the_usage_limit_error_is_told_from_other_errors() {
        // Arrange
        let limit = json!({"message": "x", "codexErrorInfo": "usageLimitExceeded"});
        let other = json!({"message": "x", "codexErrorInfo": "serverOverloaded"});

        // Act and Assert
        assert!(is_usage_limit_error(&limit));
        assert!(!is_usage_limit_error(&other));
        assert!(!is_usage_limit_error(&Value::Null));
    }
}
