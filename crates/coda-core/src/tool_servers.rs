// SPDX-License-Identifier: Apache-2.0

//! Starts the external tool servers the user listed and offers their tools to
//! a session. A server comes only from the user's own `config.toml`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coda_config::tool_servers::ToolServerConfig;
use coda_tools::Tool;
use coda_tools::tool_server::{ExternalTool, ServerSpec, Started, ToolServer};
use tracing::warn;

use crate::prompt::ToolSchema;
use crate::tools::BaseTools;

/// One listed server: its settings and the only variables it is given.
#[derive(Clone)]
struct Listed {
    name: String,
    config: ToolServerConfig,
    env: Vec<(OsString, OsString)>,
}

/// The servers the user listed, ready to start for a session.
#[derive(Default, Clone)]
pub struct ToolServers {
    listed: Vec<Listed>,
}

impl ToolServers {
    /// The servers of `config`, each with the variables it named, copied from `env`.
    #[must_use]
    pub fn new(config: &BTreeMap<String, ToolServerConfig>, env: &[(OsString, OsString)]) -> Self {
        let listed = config
            .iter()
            .map(|(name, server)| Listed {
                name: name.clone(),
                config: server.clone(),
                env: env
                    .iter()
                    .filter(|(key, _)| server.env.iter().any(|wanted| key == wanted.as_str()))
                    .cloned()
                    .collect(),
            })
            .collect();
        Self { listed }
    }

    /// Whether the user listed no server.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.listed.is_empty()
    }

    /// Starts every listed server in `cwd`. A server that does not start is left
    /// out and logged. `reserved` holds the names of the tools the model already has.
    /// This blocks until each server has answered or timed out.
    #[must_use]
    pub fn start(&self, cwd: &Path, reserved: &[&str]) -> Vec<Started> {
        let mut started = Vec::new();
        for listed in &self.listed {
            let spec = ServerSpec {
                name: listed.name.clone(),
                argv: listed.config.command.clone(),
                env: listed.env.clone(),
                cwd: cwd.to_owned(),
                timeout: Duration::from_millis(listed.config.timeout_ms),
            };
            match ToolServer::start(&spec, reserved) {
                Ok(one) => {
                    for refused in &one.refused {
                        warn!(server = %listed.name, tool = %refused.name, reason = %refused.reason, "a tool server tool was refused");
                    }
                    started.push(one);
                }
                Err(error) => {
                    warn!(server = %listed.name, %error, "a tool server did not start");
                }
            }
        }
        started
    }
}

/// `tools` with the tools of `started` added. With none started they come back
/// unchanged.
#[must_use]
pub fn with_tool_servers(mut tools: BaseTools, started: &[Started]) -> BaseTools {
    for one in started {
        for def in &one.tools {
            tools.tools = tools
                .tools
                .with(Arc::new(ExternalTool::new(Arc::clone(&one.server), def)) as Arc<dyn Tool>);
            tools.schemas.push(ToolSchema {
                name: def.name.clone(),
                description: def.description.clone(),
                input_schema: def.input_schema.clone(),
            });
        }
    }
    tools.schemas.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// Whether `started` offers any tool.
#[must_use]
pub fn offers_tools(started: &[Started]) -> bool {
    started.iter().any(|one| !one.tools.is_empty())
}
