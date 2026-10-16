// SPDX-License-Identifier: AGPL-3.0-only

//! Span attribute names: OpenTelemetry `GenAI` semantic conventions and coda's own.
//!
//! The `gen_ai.*` names follow
//! <https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/>.

/// Name of the `GenAI` provider serving the request.
pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
/// The operation being performed, such as `chat`.
pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
/// Name of the model the request targets.
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
/// Number of tokens in the prompt.
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// Number of tokens in the response.
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";

/// Identifier of the coda session.
pub const CODA_SESSION_ID: &str = "coda.session.id";
/// Identifier of a turn within a session.
pub const CODA_TURN_ID: &str = "coda.turn.id";
/// Name of the tool being invoked.
pub const CODA_TOOL_NAME: &str = "coda.tool.name";
/// Outcome of the permission check for a tool call.
pub const CODA_PERMISSION_DECISION: &str = "coda.permission.decision";
/// Kind of model backend in use.
pub const CODA_BACKEND_KIND: &str = "coda.backend.kind";

/// Every content-free attribute name, for uniqueness checks.
pub const ALL: &[&str] = &[
    GEN_AI_PROVIDER_NAME,
    GEN_AI_OPERATION_NAME,
    GEN_AI_REQUEST_MODEL,
    GEN_AI_USAGE_INPUT_TOKENS,
    GEN_AI_USAGE_OUTPUT_TOKENS,
    CODA_SESSION_ID,
    CODA_TURN_ID,
    CODA_TOOL_NAME,
    CODA_PERMISSION_DECISION,
    CODA_BACKEND_KIND,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_constants_are_unique_and_non_empty() {
        // Arrange
        let names = ALL;

        // Act
        let unique: HashSet<&str> = names.iter().copied().collect();

        // Assert
        assert!(names.iter().all(|n| !n.is_empty()));
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn provider_name_uses_current_semconv_key() {
        // Arrange / Act
        let name = GEN_AI_PROVIDER_NAME;

        // Assert
        assert_eq!(name, "gen_ai.provider.name");
    }
}
