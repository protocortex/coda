// SPDX-License-Identifier: AGPL-3.0-only

//! The `retrieve` tool: the full output behind a shrunk one.

use coda_protocol::readers::OriginalsReader;
use coda_protocol::types::OriginalId;
use serde::Deserialize;
use serde_json::Value;

use crate::{Tool, ToolError, ToolOutput, parse_args};

pub const NAME: &str = "retrieve";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetrieveArgs {
    id: String,
}

/// Fetches an original by id from a session-scoped reader.
pub struct RetrieveTool<R> {
    reader: R,
    max_output_bytes: usize,
}

impl<R> RetrieveTool<R> {
    /// Creates the tool, capping each result at `max_output_bytes`.
    #[must_use]
    pub fn new(reader: R, max_output_bytes: usize) -> Self {
        Self {
            reader,
            max_output_bytes,
        }
    }
}

impl<R: OriginalsReader + Send + Sync> Tool for RetrieveTool<R> {
    fn name(&self) -> &'static str {
        NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let args: RetrieveArgs = parse_args(NAME, input)?;
        let text = self.reader.fetch_original(&OriginalId::new(args.id))?;
        Ok(ToolOutput::capped(text, self.max_output_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::readers::ReaderError;
    use serde_json::json;

    struct Fake;

    impl OriginalsReader for Fake {
        fn fetch_original(&self, id: &OriginalId) -> Result<String, ReaderError> {
            if id.as_str() == "7" {
                Ok("full output".to_owned())
            } else {
                Err(ReaderError::NotFound(id.to_string()))
            }
        }
    }

    #[test]
    fn returns_the_original_stored_under_the_id() {
        // Arrange
        let tool = RetrieveTool::new(Fake, 64);

        // Act
        let out = tool.call(&json!({"id": "7"})).unwrap();

        // Assert
        assert_eq!(
            out,
            ToolOutput {
                text: "full output".to_owned(),
                truncated: false
            }
        );
    }

    #[test]
    fn cuts_a_long_original_to_the_cap() {
        // Arrange
        let tool = RetrieveTool::new(Fake, 4);

        // Act
        let out = tool.call(&json!({"id": "7"})).unwrap();

        // Assert
        assert_eq!(
            out,
            ToolOutput {
                text: "full".to_owned(),
                truncated: true
            }
        );
    }

    #[test]
    fn an_unknown_id_is_reported_by_name() {
        // Arrange
        let tool = RetrieveTool::new(Fake, 64);

        // Act
        let err = tool.call(&json!({"id": "8"})).unwrap_err();

        // Assert
        assert_eq!(err.to_string(), "original `8` was not found");
    }

    #[test]
    fn arguments_without_an_id_are_invalid() {
        // Arrange
        let tool = RetrieveTool::new(Fake, 64);

        // Act
        let err = tool.call(&json!({"nope": 1})).unwrap_err();

        // Assert
        assert!(matches!(
            err,
            ToolError::InvalidInput {
                tool: "retrieve",
                ..
            }
        ));
    }
}
