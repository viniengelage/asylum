use crate::{AgentToolOutput, AnyAgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use collections::HashSet;
use futures::FutureExt as _;
use gpui::{App, SharedString, Task};
use parking_lot::Mutex;
use serde::Deserialize;
use std::{fmt::Write as _, sync::Arc};
use util::markdown::MarkdownInlineCode;

/// MCP servers loaded in a thread, shared between the thread and its `load_mcp_server` tool.
pub type LoadedContextServers = Arc<Mutex<HashSet<String>>>;

/// A server listed in `on_demand_context_servers` that this thread hasn't loaded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadableContextServer {
    pub id: String,
    pub tool_count: usize,
}

#[derive(Debug, Deserialize)]
struct LoadContextServerToolInput {
    server: String,
    #[serde(default)]
    reason: String,
}

/// Stands in for servers whose tools are too many to send on every request: the model sees one
/// small tool instead of hundreds of schemas, and the user decides whether loading them is worth
/// it.
pub struct LoadContextServerTool {
    servers: Vec<LoadableContextServer>,
    loaded: LoadedContextServers,
}

impl LoadContextServerTool {
    pub const NAME: &'static str = "load_mcp_server";

    pub fn new(servers: Vec<LoadableContextServer>, loaded: LoadedContextServers) -> Self {
        Self { servers, loaded }
    }
}

impl AnyAgentTool for LoadContextServerTool {
    fn name(&self) -> SharedString {
        Self::NAME.into()
    }

    fn description(&self) -> SharedString {
        let mut description = String::from(
            "Loads the tools of an MCP server that is kept out of your context until it is needed. \
             The user is asked to approve every load, so only load a server when the task clearly \
             needs one of its tools, and explain why in `reason`. Once loaded, the server's tools \
             are available from your next step on, for the rest of this thread.\n\n\
             Servers you can load:",
        );
        for server in &self.servers {
            write!(
                description,
                "\n- `{}` ({} tools)",
                server.id, server.tool_count
            )
            .ok();
        }
        description.into()
    }

    fn kind(&self) -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString {
        match serde_json::from_value::<LoadContextServerToolInput>(input) {
            Ok(input) => i18n::t!(
                "agent.load_mcp_server_tool.title_named",
                server = MarkdownInlineCode(&input.server)
            ),
            Err(_) => i18n::t!("agent.load_mcp_server_tool.title"),
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        let server_ids: Vec<&str> = self
            .servers
            .iter()
            .map(|server| server.id.as_str())
            .collect();
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "enum": server_ids,
                    "description": "The id of the server to load."
                },
                "reason": {
                    "type": "string",
                    "description": "One sentence, shown to the user, on what you need the server for."
                }
            },
            "required": ["server", "reason"],
            "additionalProperties": false
        })
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let input: LoadContextServerToolInput = serde_json::from_value(input)
                .map_err(|error| anyhow::anyhow!("Invalid input: {error}"))?;
            let Some(server) = self
                .servers
                .iter()
                .find(|server| server.id == input.server)
            else {
                let available = self
                    .servers
                    .iter()
                    .map(|server| format!("`{}`", server.id))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(anyhow::anyhow!(
                    "There is no MCP server `{}` to load. Servers you can load: {available}",
                    input.server
                )
                .into());
            };

            if !input.reason.trim().is_empty() {
                let reason = input.reason.trim().to_string();
                event_stream.update_fields(|update| update.content(vec![reason.into()]));
            }
            let authorize = cx.update(|cx| {
                event_stream.authorize_always_prompt(
                    i18n::t!(
                        "agent.load_mcp_server_tool.authorize",
                        server = MarkdownInlineCode(&server.id),
                        count = server.tool_count
                    ),
                    ToolPermissionContext::new(Self::NAME, vec![server.id.clone()]).once_only(),
                    cx,
                )
            });
            futures::select! {
                result = authorize.fuse() => result?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(anyhow::anyhow!("Loading the MCP server was cancelled by the user").into());
                }
            }

            self.loaded.lock().insert(server.id.clone());
            let message = format!(
                "Loaded the `{}` MCP server: its {} tools are available from your next step on.",
                server.id, server.tool_count
            );
            Ok(AgentToolOutput {
                raw_output: serde_json::Value::String(message.clone()),
                llm_output: vec![message.into()],
            })
        })
    }

    fn replay(
        &self,
        _input: serde_json::Value,
        _output: serde_json::Value,
        _event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        Ok(())
    }
}
