use crate::{AgentToolOutput, AnyAgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use collections::HashSet;
use context_server::ContextServerId;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use parking_lot::Mutex;
use project::context_server_store::{ContextServerStatus, ContextServerStore};
use serde::Deserialize;
use std::{fmt::Write as _, sync::Arc, time::Duration};
use util::markdown::MarkdownInlineCode;

/// MCP servers loaded in a thread, shared between the thread and its `load_mcp_server` tool.
pub type LoadedContextServers = Arc<Mutex<HashSet<String>>>;

/// npx-based servers download their package on the first start, which easily takes a minute.
const START_TIMEOUT: Duration = Duration::from_secs(120);
const START_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why a configured server's tools aren't in the thread yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithheldContextServer {
    /// Listed in `on_demand_context_servers`.
    OnDemand,
    /// The thread's task agent lists other servers, and not this one.
    NotListedByAgent { agent: String },
    /// The profile turns all of its tools off.
    OffInProfile,
    /// Enabled in settings, but not running: loading it starts it.
    Stopped,
    /// Enabled in settings, but it failed: loading it starts it again.
    Failed { error: Arc<str> },
}

/// A server the user can let the thread load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadableContextServer {
    pub id: String,
    /// `None` while the server isn't running, since its tools are only known once it is.
    pub tool_count: Option<usize>,
    pub withheld: WithheldContextServer,
}

/// A configured server the model can't load, listed so it can tell the user what to do instead
/// of concluding the server doesn't exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableContextServer {
    Disabled { id: String },
    NeedsSignIn { id: String },
    Starting { id: String },
}

#[derive(Debug, Deserialize)]
struct LoadContextServerToolInput {
    server: String,
    #[serde(default)]
    reason: String,
}

/// Stands in for MCP servers whose tools the thread doesn't get up front: servers kept out of
/// the context to save tokens, servers the task agent or the profile leave out, and servers that
/// stopped or failed. The model sees one small tool that names them, and the user approves each
/// load, so a server that is configured is never invisible to the agent.
pub struct LoadContextServerTool {
    servers: Vec<LoadableContextServer>,
    unavailable: Vec<UnavailableContextServer>,
    loaded: LoadedContextServers,
    server_store: Entity<ContextServerStore>,
}

impl LoadContextServerTool {
    pub const NAME: &'static str = "load_mcp_server";

    pub fn new(
        servers: Vec<LoadableContextServer>,
        unavailable: Vec<UnavailableContextServer>,
        loaded: LoadedContextServers,
        server_store: Entity<ContextServerStore>,
    ) -> Self {
        Self {
            servers,
            unavailable,
            loaded,
            server_store,
        }
    }
}

impl AnyAgentTool for LoadContextServerTool {
    fn name(&self) -> SharedString {
        Self::NAME.into()
    }

    fn description(&self) -> SharedString {
        let mut description = String::from(
            "Loads the tools of a configured MCP server that isn't in your tool list. Before you \
             tell the user an MCP server isn't available, check the lists below. The user is \
             asked to approve every load, so only load a server when the task needs one of its \
             tools, and explain why in `reason`. Once loaded, the server's tools are available \
             from your next step on, for the rest of this thread.",
        );
        if !self.servers.is_empty() {
            description.push_str("\n\nServers you can load:");
        }
        for server in &self.servers {
            write!(description, "\n- `{}`", server.id).ok();
            if let Some(tool_count) = server.tool_count {
                write!(description, " ({tool_count} tools)").ok();
            }
            match &server.withheld {
                WithheldContextServer::OnDemand => {
                    description.push_str(": kept out of the context until needed")
                }
                WithheldContextServer::NotListedByAgent { agent } => {
                    write!(
                        description,
                        ": not among the servers the `{agent}` agent lists"
                    )
                    .ok();
                }
                WithheldContextServer::OffInProfile => {
                    description.push_str(": turned off in the agent profile")
                }
                WithheldContextServer::Stopped => {
                    description.push_str(": not running, loading it starts it")
                }
                WithheldContextServer::Failed { error } => {
                    write!(
                        description,
                        ": failed to start ({error}), loading it tries again"
                    )
                    .ok();
                }
            }
        }
        if !self.unavailable.is_empty() {
            description.push_str(
                "\n\nConfigured servers you can't load (tell the user what is needed if the task \
                 needs one):",
            );
        }
        for server in &self.unavailable {
            match server {
                UnavailableContextServer::Disabled { id } => write!(
                    description,
                    "\n- `{id}`: turned off in the settings, the user has to turn it on in \
                     Settings > MCP"
                ),
                UnavailableContextServer::NeedsSignIn { id } => write!(
                    description,
                    "\n- `{id}`: needs the user to sign in, from Settings > MCP"
                ),
                UnavailableContextServer::Starting { id } => write!(
                    description,
                    "\n- `{id}`: still starting, its tools show up on their own"
                ),
            }
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
            let needs_start = matches!(
                server.withheld,
                WithheldContextServer::Stopped | WithheldContextServer::Failed { .. }
            );
            let prompt = match server.tool_count {
                Some(count) => i18n::t!(
                    "agent.load_mcp_server_tool.authorize",
                    server = MarkdownInlineCode(&server.id),
                    count = count
                ),
                None => i18n::t!(
                    "agent.load_mcp_server_tool.authorize_start",
                    server = MarkdownInlineCode(&server.id)
                ),
            };
            let authorize = cx.update(|cx| {
                event_stream.authorize_always_prompt(
                    prompt,
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

            if needs_start {
                let started = start_server(&self.server_store, &server.id, cx);
                futures::select! {
                    result = started.fuse() => result?,
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err(anyhow::anyhow!("Starting the MCP server was cancelled by the user").into());
                    }
                }
            }

            self.loaded.lock().insert(server.id.clone());
            let message = match server.tool_count {
                Some(count) => format!(
                    "Loaded the `{}` MCP server: its {count} tools are available from your next \
                     step on.",
                    server.id
                ),
                None => format!(
                    "Started the `{}` MCP server: its tools are available from your next step on.",
                    server.id
                ),
            };
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

async fn start_server(
    server_store: &Entity<ContextServerStore>,
    server_id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let id = ContextServerId(server_id.into());
    server_store.update(cx, |store, cx| {
        let server = store.get_server(&id).ok_or_else(|| {
            anyhow::anyhow!("The MCP server `{server_id}` is no longer configured")
        })?;
        store.start_server(server, cx);
        anyhow::Ok(())
    })?;

    let timer = cx.background_executor().timer(START_TIMEOUT).fuse();
    futures::pin_mut!(timer);
    // `start_server` resolves the configuration before the state turns to `Starting`, so the old
    // `Error` or `Stopped` is still there on the first polls and only counts after `Starting`.
    let mut seen_starting = false;
    loop {
        futures::select! {
            _ = cx.background_executor().timer(START_POLL_INTERVAL).fuse() => {}
            _ = timer => {
                anyhow::bail!(
                    "The MCP server `{server_id}` didn't start within {} seconds",
                    START_TIMEOUT.as_secs()
                );
            }
        }
        let status = server_store.read_with(cx, |store, _| store.status_for_server(&id));
        match status {
            Some(ContextServerStatus::Running) => return Ok(()),
            Some(
                ContextServerStatus::AuthRequired
                | ContextServerStatus::ClientSecretRequired { .. }
                | ContextServerStatus::Authenticating,
            ) => anyhow::bail!(
                "The MCP server `{server_id}` needs the user to sign in, from Settings > MCP"
            ),
            Some(ContextServerStatus::Starting) => seen_starting = true,
            Some(ContextServerStatus::Error(error)) if seen_starting => {
                anyhow::bail!("The MCP server `{server_id}` failed to start: {error}")
            }
            Some(ContextServerStatus::Stopped) if seen_starting => {
                anyhow::bail!("The MCP server `{server_id}` stopped while starting")
            }
            Some(ContextServerStatus::Error(_) | ContextServerStatus::Stopped) | None => {}
        }
    }
}
