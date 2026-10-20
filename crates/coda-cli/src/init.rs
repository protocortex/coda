// SPDX-License-Identifier: AGPL-3.0-only

//! `coda init`: writes a starter `config.toml` for the first run.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};

use crate::context::{Context, Io};
use crate::error::CliError;

/// Every provider section, commented out so nothing is enabled until the user decides.
const STARTER_CONFIG: &str = r#"# Coda configuration. See docs/config.md for every key.
# Every provider is off until you uncomment its section and set enabled to true.

# The claude and codex backends may run under a subscription. Read the account
# risk notice in the README before enabling them.

# [providers.claude_code]
# enabled = true

# [providers.codex]
# enabled = true

# [providers.anthropic]
# enabled = true
# api_key_env = "ANTHROPIC_API_KEY"
# model = "claude-sonnet-4-5"

# [providers.openai_compat]
# enabled = true
# base_url = "https://api.example.com/v1"
# api_key_env = "OPENAI_API_KEY"
# model = "gpt-4o"

# [providers.faux]
# enabled = true
# script = "/path/to/script.json"
"#;

/// Writes the starter config under the config directory.
///
/// # Errors
/// Returns [`CliError::ConfigExists`] when the file is already there, and
/// [`CliError::Path`] when it cannot be written.
pub fn run(ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let path = ctx.dirs.config_file();
    let path_error = |source| CliError::Path {
        path: path.clone(),
        source,
    };
    fs::create_dir_all(ctx.dirs.base()).map_err(path_error)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::AlreadyExists => CliError::ConfigExists { path: path.clone() },
            _ => path_error(source),
        })?;
    file.write_all(STARTER_CONFIG.as_bytes())
        .map_err(path_error)?;
    writeln!(io.out, "wrote {}", path.display())?;
    writeln!(
        io.out,
        "enable a provider in it, then run `coda daemon restart`; a running daemon reads the config once"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use coda_config::config::Config;

    use super::*;

    /// Strips the leading `# ` from the lines that hold sections and keys.
    fn uncommented(text: &str) -> String {
        text.lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| line.starts_with('[') || line.contains(" = "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_uncommented_starter_config_parses_without_unknown_keys() {
        // Arrange
        let text = uncommented(STARTER_CONFIG);

        // Act
        let parsed = Config::from_toml_str(&text).unwrap();

        // Assert
        assert_eq!(parsed.unknown_keys, Vec::<String>::new());
        assert_eq!(parsed.config.providers.len(), 5);
    }
}
