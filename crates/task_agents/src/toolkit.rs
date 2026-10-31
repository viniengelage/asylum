use anyhow::Result;
use base64::Engine as _;
use collections::{HashMap, HashSet};
use gpui::{App, Entity, Global, SharedString, Task, Window};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use settings::{RegisterSetting, Settings, SettingsLocation};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::rel_path::RelPath;

/// The ids the toolkits register under, which `agent_toolkits` in the settings is keyed by.
pub mod toolkit_ids {
    pub const API: &str = "api";
    pub const BROWSER: &str = "browser";
    pub const CLICKUP: &str = "clickup";
    pub const DATABASE: &str = "database";
    pub const DEVICES: &str = "devices";
    pub const DOCKER: &str = "docker";
    pub const ELASTIC: &str = "elastic";
    pub const GITHUB_PROJECTS: &str = "github_projects";
    pub const REPO: &str = "repo";
}

/// `agent_toolkits` from the settings: which toolkits are on.
#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct AgentToolkitSettings {
    toolkits: HashMap<Arc<str>, bool>,
}

impl Settings for AgentToolkitSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            toolkits: content.project.agent_toolkits.clone(),
        }
    }
}

impl AgentToolkitSettings {
    pub fn is_enabled(&self, toolkit_id: &str) -> bool {
        self.toolkits.get(toolkit_id).copied().unwrap_or(true)
    }
}

/// Whether the toolkit is on in the profile's own settings, before any project weighs in.
pub fn toolkit_enabled_in_profile(toolkit_id: &str, cx: &App) -> bool {
    AgentToolkitSettings::get_global(cx).is_enabled(toolkit_id)
}

/// `Some` when a folder of `project` turns the toolkit on or off against the profile, through
/// its `.zed/settings.json`. With several folders, the first one that disagrees wins.
pub fn toolkit_project_override(
    toolkit_id: &str,
    project: &Entity<Project>,
    cx: &App,
) -> Option<bool> {
    let in_profile = toolkit_enabled_in_profile(toolkit_id, cx);
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| {
            let location = SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            };
            AgentToolkitSettings::get(Some(location), cx).is_enabled(toolkit_id)
        })
        .find(|enabled| *enabled != in_profile)
}

/// Whether the agent, the status bar and the docks of `project` get the toolkit.
pub fn toolkit_enabled(toolkit_id: &str, project: &Entity<Project>, cx: &App) -> bool {
    toolkit_project_override(toolkit_id, project, cx)
        .unwrap_or_else(|| toolkit_enabled_in_profile(toolkit_id, cx))
}

/// The registered toolkits that are off for `project`.
pub fn disabled_toolkit_ids(project: &Entity<Project>, cx: &App) -> HashSet<SharedString> {
    toolkits(cx)
        .into_iter()
        .filter(|toolkit| !toolkit_enabled(&toolkit.id, project, cx))
        .map(|toolkit| toolkit.id.clone())
        .collect()
}

/// The registered toolkits that are off for `project`, by id, with their names.
pub fn disabled_toolkit_names(
    project: &Entity<Project>,
    cx: &App,
) -> HashMap<SharedString, SharedString> {
    toolkits(cx)
        .into_iter()
        .filter(|toolkit| !toolkit_enabled(&toolkit.id, project, cx))
        .map(|toolkit| (toolkit.id.clone(), toolkit.name.clone()))
        .collect()
}

/// What to tell someone who reaches for a toolkit that is off, e.g. through its shortcut.
pub fn toolkit_disabled_message(name: &str) -> String {
    format!("{name} está desligado neste perfil. Ligue em Gerenciar perfis → Toolkit do agente.")
}

fn toolkit_disabled_error(toolkit_id: &str, cx: &App) -> anyhow::Error {
    let name = toolkits(cx)
        .into_iter()
        .find(|toolkit| toolkit.id == toolkit_id)
        .map(|toolkit| toolkit.name.to_string())
        .unwrap_or_else(|| toolkit_id.to_string());
    anyhow::anyhow!(toolkit_disabled_message(&name))
}

/// Whether a tool only looks at things or changes them. Acting tools go through the tool
/// permission prompt; reading tools run without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccess {
    Read,
    Act,
    /// Acts in a way that can't be undone, such as merging a pull request: it asks every time,
    /// even when a rule or the task agent would allow it.
    AlwaysConfirm,
}

impl ToolAccess {
    pub fn acts(self) -> bool {
        self != Self::Read
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolkitContent {
    Text(String),
    Image { base64_png: String },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolkitOutput {
    pub content: Vec<ToolkitContent>,
    /// Kept with the tool call instead of the text, so the thread can draw the result itself
    /// (live and when the thread is reopened). The model only sees `content`.
    pub raw_output: Option<serde_json::Value>,
}

impl ToolkitOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolkitContent::Text(text.into())],
            raw_output: None,
        }
    }

    pub fn with_raw_output(mut self, raw_output: impl Serialize) -> Self {
        match serde_json::to_value(raw_output) {
            Ok(value) => self.raw_output = Some(value),
            Err(error) => log::error!("toolkit raw output could not be serialized: {error}"),
        }
        self
    }

    pub fn push_text(&mut self, text: impl Into<String>) {
        self.content.push(ToolkitContent::Text(text.into()));
    }

    pub fn push_png(&mut self, png: &[u8]) {
        self.content.push(ToolkitContent::Image {
            base64_png: base64::engine::general_purpose::STANDARD.encode(png),
        });
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                ToolkitContent::Text(text) => Some(text.as_str()),
                ToolkitContent::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A screenshot scaled down so its longest side is at most `max_side`, with the factor that maps
/// its pixels back to the source. Tools that take coordinates use the same factor, so the model
/// can point at what it saw.
pub struct ScaledScreenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

pub const SCREENSHOT_MAX_SIDE: u32 = 1280;

pub fn screenshot_scale(width: u32, height: u32) -> f64 {
    let longest = width.max(height).max(1);
    if longest <= SCREENSHOT_MAX_SIDE {
        1.0
    } else {
        f64::from(SCREENSHOT_MAX_SIDE) / f64::from(longest)
    }
}

pub fn scale_screenshot(image: image::DynamicImage) -> Result<ScaledScreenshot> {
    let scale = screenshot_scale(image.width(), image.height());
    let image = if scale < 1.0 {
        let width = (f64::from(image.width()) * scale).round().max(1.0) as u32;
        let height = (f64::from(image.height()) * scale).round().max(1.0) as u32;
        image.resize_exact(width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(ScaledScreenshot {
        png,
        width: image.width(),
        height: image.height(),
        scale,
    })
}

pub struct ToolkitCall {
    pub project: Entity<Project>,
    pub input: serde_json::Value,
}

type RunTool = Arc<dyn Fn(ToolkitCall, &mut App) -> Task<Result<ToolkitOutput>> + Send + Sync>;

#[derive(Clone)]
pub struct ToolkitTool {
    pub name: SharedString,
    /// Short Portuguese label for the tool call header, e.g. "Tocou na tela".
    pub title: SharedString,
    pub description: SharedString,
    pub input_schema: serde_json::Value,
    pub access: ToolAccess,
    run: RunTool,
}

impl ToolkitTool {
    pub fn new<I>(
        name: impl Into<SharedString>,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        access: ToolAccess,
        run: impl Fn(Entity<Project>, I, &mut App) -> Task<Result<ToolkitOutput>>
        + Send
        + Sync
        + 'static,
    ) -> Self
    where
        I: JsonSchema + DeserializeOwned + 'static,
    {
        let input_schema = serde_json::to_value(schemars::schema_for!(I))
            .unwrap_or_else(|_| serde_json::json!({ "type": "object", "properties": {} }));
        Self {
            name: name.into(),
            title: title.into(),
            description: description.into(),
            input_schema,
            access,
            run: Arc::new(
                move |call, cx| match serde_json::from_value::<I>(call.input) {
                    Ok(input) => run(call.project, input, cx),
                    Err(error) => Task::ready(Err(anyhow::anyhow!("invalid input: {error}"))),
                },
            ),
        }
    }

    pub fn run(&self, call: ToolkitCall, cx: &mut App) -> Task<Result<ToolkitOutput>> {
        (self.run)(call, cx)
    }

    /// The input values tool permission patterns are matched against: every string in the
    /// input's top level, in order.
    pub fn permission_inputs(input: &serde_json::Value) -> Vec<String> {
        match input {
            serde_json::Value::Object(map) => map
                .values()
                .filter_map(|value| match value {
                    serde_json::Value::String(text) => Some(text.clone()),
                    serde_json::Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

pub struct Toolkit {
    pub id: SharedString,
    pub name: SharedString,
    pub description: SharedString,
    /// An `IconName` in snake case, resolved by the UI.
    pub icon: SharedString,
    /// The `@` mention the toolkit offers in the composer, e.g. "@PR", if any.
    pub mention: Option<SharedString>,
    pub tools: Vec<ToolkitTool>,
}

#[derive(Default)]
pub struct ToolkitRegistry {
    toolkits: Vec<Arc<Toolkit>>,
}

impl Global for ToolkitRegistry {}

/// Makes a toolkit's tools available to agent threads. Registering the same id again replaces
/// the earlier toolkit.
pub fn register_toolkit(toolkit: Toolkit, cx: &mut App) {
    let registry = cx.default_global::<ToolkitRegistry>();
    registry
        .toolkits
        .retain(|existing| existing.id != toolkit.id);
    registry.toolkits.push(Arc::new(toolkit));
}

pub fn toolkits(cx: &App) -> Vec<Arc<Toolkit>> {
    cx.try_global::<ToolkitRegistry>()
        .map(|registry| registry.toolkits.clone())
        .unwrap_or_default()
}

pub fn toolkit_for_tool(tool_name: &str, cx: &App) -> Option<Arc<Toolkit>> {
    toolkits(cx)
        .into_iter()
        .find(|toolkit| toolkit.tools.iter().any(|tool| tool.name == tool_name))
}

/// A SQL editor tab open against a database, which the user can point the agent at with `@`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SqlEditorTab {
    pub abs_path: PathBuf,
    /// The connection the tab runs against, e.g. "Homologação · dev".
    pub connection: SharedString,
}

type ListSqlEditors = Arc<dyn Fn(&Entity<Project>, &App) -> Vec<SqlEditorTab>>;
type DescribeSqlEditor = Arc<dyn Fn(&Entity<Project>, &Path, &App) -> Result<String>>;
type OpenQuery = Arc<dyn Fn(&Entity<Project>, String, &mut Window, &mut App) -> Result<()>>;
type FocusSqlEditor = Arc<dyn Fn(&Entity<Project>, &Path, &mut Window, &mut App) -> Result<()>>;

/// Installed by the database client, so the agent's `@` menu can offer its editors without
/// depending on it.
#[derive(Clone)]
pub struct SqlEditorSource {
    /// The project's SQL editors, most recently opened first.
    pub list: ListSqlEditors,
    /// What the agent receives when a tab is mentioned: the connection, the SQL in it, and how
    /// to write into it.
    pub describe: DescribeSqlEditor,
    /// Opens the SQL in a new query tab against the dock's connection and runs it.
    pub open_query: OpenQuery,
    /// Brings an open SQL editor tab to the front.
    pub focus: FocusSqlEditor,
}

impl Global for SqlEditorSource {}

pub fn register_sql_editor_source(source: SqlEditorSource, cx: &mut App) {
    cx.set_global(source);
}

pub fn sql_editor_tabs(project: &Entity<Project>, cx: &App) -> Vec<SqlEditorTab> {
    if !toolkit_enabled(toolkit_ids::DATABASE, project, cx) {
        return Vec::new();
    }
    cx.try_global::<SqlEditorSource>()
        .map(|source| (source.list)(project, cx))
        .unwrap_or_default()
}

pub fn describe_sql_editor(project: &Entity<Project>, abs_path: &Path, cx: &App) -> Result<String> {
    if !toolkit_enabled(toolkit_ids::DATABASE, project, cx) {
        return Err(toolkit_disabled_error(toolkit_ids::DATABASE, cx));
    }
    let source = cx
        .try_global::<SqlEditorSource>()
        .ok_or_else(|| anyhow::anyhow!("This build has no database client."))?;
    (source.describe)(project, abs_path, cx)
}

pub fn open_query_in_database(
    project: &Entity<Project>,
    sql: String,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let open_query = cx
        .try_global::<SqlEditorSource>()
        .map(|source| source.open_query.clone())
        .ok_or_else(|| anyhow::anyhow!("This build has no database client."))?;
    open_query(project, sql, window, cx)
}

pub fn focus_sql_editor(
    project: &Entity<Project>,
    abs_path: &Path,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let focus = cx
        .try_global::<SqlEditorSource>()
        .map(|source| source.focus.clone())
        .ok_or_else(|| anyhow::anyhow!("This build has no database client."))?;
    focus(project, abs_path, window, cx)
}

/// Something from the Elastic dock the user can point the agent at with `@`: an ES|QL tab, a
/// log document, a trace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogMention {
    /// Opaque to the agent: `esql:<path>`, `trace:<id>`, `doc:<n>`…
    pub id: SharedString,
    pub title: SharedString,
    /// The connection, e.g. "trix-logs".
    pub detail: SharedString,
}

type ListLogMentions = Arc<dyn Fn(&Entity<Project>, &App) -> Vec<LogMention>>;
type DescribeLogMention = Arc<dyn Fn(&Entity<Project>, &str, &mut App) -> Task<Result<String>>>;
type OpenLogMention = Arc<dyn Fn(&Entity<Project>, &str, &mut Window, &mut App) -> Result<()>>;
type OpenLogQuery = Arc<dyn Fn(&Entity<Project>, String, u32, &mut Window, &mut App) -> Result<()>>;
type FollowLogs = Arc<dyn Fn(&Entity<Project>, String, &mut Window, &mut App) -> Result<()>>;

/// Installed by the Elastic dock, so the agent can offer and read its logs without depending on
/// it.
#[derive(Clone)]
pub struct LogSource {
    /// The open ES|QL tabs, most recently opened first.
    pub list: ListLogMentions,
    /// What the agent receives when the mention is sent, with personal data masked.
    pub describe: DescribeLogMention,
    /// Brings what the mention points at to the front.
    pub open: OpenLogMention,
    /// Opens an ES|QL query in a new tab of the Elastic dock, with a window in minutes, and
    /// runs it.
    pub open_query: OpenLogQuery,
    /// Follows a data stream in a tab of the Elastic dock.
    pub follow: FollowLogs,
}

impl Global for LogSource {}

pub fn register_log_source(source: LogSource, cx: &mut App) {
    cx.set_global(source);
}

pub fn log_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    if !toolkit_enabled(toolkit_ids::ELASTIC, project, cx) {
        return Vec::new();
    }
    cx.try_global::<LogSource>()
        .map(|source| (source.list)(project, cx))
        .unwrap_or_default()
}

pub fn describe_log_mention(
    project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<Result<String>> {
    if id.starts_with(CONTAINER_MENTION_PREFIX) {
        return describe_container_mention(project, id, cx);
    }
    if !toolkit_enabled(toolkit_ids::ELASTIC, project, cx) {
        return Task::ready(Err(toolkit_disabled_error(toolkit_ids::ELASTIC, cx)));
    }
    match cx.try_global::<LogSource>() {
        Some(source) => {
            let describe = source.describe.clone();
            describe(project, id, cx)
        }
        None => Task::ready(Err(anyhow::anyhow!("This build has no Elastic dock."))),
    }
}

pub fn open_log_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    if id.starts_with(CONTAINER_MENTION_PREFIX) {
        let open = cx
            .try_global::<ContainerSource>()
            .map(|source| source.open.clone())
            .ok_or_else(|| anyhow::anyhow!("This build has no Docker dock."))?;
        return open(project, id, window, cx);
    }
    let open = cx
        .try_global::<LogSource>()
        .map(|source| source.open.clone())
        .ok_or_else(|| anyhow::anyhow!("This build has no Elastic dock."))?;
    open(project, id, window, cx)
}

pub fn open_log_query(
    project: &Entity<Project>,
    query: String,
    window_minutes: u32,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let open_query = cx
        .try_global::<LogSource>()
        .map(|source| source.open_query.clone())
        .ok_or_else(|| anyhow::anyhow!("This build has no Elastic dock."))?;
    open_query(project, query, window_minutes, window, cx)
}

pub fn follow_logs(
    project: &Entity<Project>,
    source: String,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let follow = cx
        .try_global::<LogSource>()
        .map(|source| source.follow.clone())
        .ok_or_else(|| anyhow::anyhow!("This build has no Elastic dock."))?;
    follow(project, source, window, cx)
}

/// Container mentions travel as log mentions whose id starts with this, so the thread keeps one
/// kind of mention for "things the docks resolve".
pub const CONTAINER_MENTION_PREFIX: &str = "container:";

/// Installed by the Docker dock, so the agent can offer and read containers without depending
/// on it.
#[derive(Clone)]
pub struct ContainerSource {
    /// The machine's containers, the project's compose services first.
    pub list: ListLogMentions,
    /// State, image, ports and the last log lines, with env values masked.
    pub describe: DescribeLogMention,
    /// Focuses the container in the Docker dock.
    pub open: OpenLogMention,
    /// The thread card's buttons: `logs`, `terminal` or `inspect` for a container id.
    pub act: ContainerAct,
}

pub type ContainerAct = Arc<dyn Fn(&Entity<Project>, &str, ContainerAction, &mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerAction {
    Logs,
    Terminal,
    Inspect,
}

impl Global for ContainerSource {}

pub fn register_container_source(source: ContainerSource, cx: &mut App) {
    cx.set_global(source);
}

pub fn container_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    if !toolkit_enabled(toolkit_ids::DOCKER, project, cx) {
        return Vec::new();
    }
    cx.try_global::<ContainerSource>()
        .map(|source| (source.list)(project, cx))
        .unwrap_or_default()
}

fn describe_container_mention(
    project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<Result<String>> {
    if !toolkit_enabled(toolkit_ids::DOCKER, project, cx) {
        return Task::ready(Err(toolkit_disabled_error(toolkit_ids::DOCKER, cx)));
    }
    match cx.try_global::<ContainerSource>() {
        Some(source) => {
            let describe = source.describe.clone();
            describe(project, id, cx)
        }
        None => Task::ready(Err(anyhow::anyhow!("This build has no Docker dock."))),
    }
}

pub fn act_on_container(
    project: &Entity<Project>,
    container_id: &str,
    action: ContainerAction,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(source) = cx.try_global::<ContainerSource>() {
        let act = source.act.clone();
        act(project, container_id, action, window, cx);
    }
}

pub const DATABASE_QUERY_TOOL: &str = "database_query";
pub const DATABASE_WRITE_QUERY_TOOL: &str = "database_write_query";

/// The connection a database tool ran against, as the thread shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DatabaseConnectionInfo {
    pub name: String,
    pub database: String,
    /// `local`, `dev`, `staging` or `prod`.
    pub environment: String,
}

impl DatabaseConnectionInfo {
    pub fn is_production(&self) -> bool {
        self.environment == "prod"
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DatabaseColumn {
    pub name: String,
    /// The Postgres type name (`int8`, `text`, `timestamptz`…), when the server said.
    pub type_name: Option<String>,
}

/// What `database_query` keeps for the thread: the rows the model got, so the card draws them
/// instead of the model retyping them as a table.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DatabaseQueryResult {
    pub connection: DatabaseConnectionInfo,
    pub sql: String,
    /// The relation after the first FROM, when the query reads a single one.
    pub source: Option<String>,
    pub columns: Vec<DatabaseColumn>,
    /// Every value as the server prints it; `None` is SQL NULL. Capped, see `total_rows`.
    pub rows: Vec<Vec<Option<String>>>,
    /// How many rows the query returned, which can be more than `rows` holds.
    pub total_rows: usize,
    /// The query had more rows than the limit.
    pub truncated: bool,
    pub limit: usize,
    pub elapsed_ms: u64,
}

/// What `database_write_query` keeps for the thread: where the SQL went. The SQL itself is in
/// the tool input.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DatabaseWriteResult {
    pub connection: DatabaseConnectionInfo,
    pub abs_path: PathBuf,
    pub line: usize,
}

pub const ELASTIC_ESQL_TOOL: &str = "elastic_esql";
pub const ELASTIC_WIDE_ESQL_TOOL: &str = "elastic_wide_esql";
pub const ELASTIC_TRACE_TOOL: &str = "elastic_trace";
pub const ELASTIC_RECENT_ERRORS_TOOL: &str = "elastic_recent_errors";

/// The Elasticsearch connection a tool read through, as the thread shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ElasticConnectionInfo {
    pub name: String,
    /// `dev`, `staging` or `prod`.
    pub environment: String,
}

impl ElasticConnectionInfo {
    pub fn is_production(&self) -> bool {
        self.environment == "prod"
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ElasticColumn {
    pub name: String,
    /// The ES|QL type (`long`, `keyword`, `date`…).
    pub kind: String,
}

pub const DOCKER_LOGS_TOOL: &str = "docker_logs";
pub const DOCKER_INSPECT_TOOL: &str = "docker_inspect";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DockerLogLine {
    /// `14:09:41`, local time.
    pub time: String,
    pub text: String,
    pub error: bool,
}

/// What `docker_logs` and `docker_inspect` keep for the thread: the container as the dock shows
/// its row, and the last log lines for the card's excerpt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DockerContainerResult {
    pub id: String,
    pub name: String,
    pub image: String,
    /// `running`, `paused`, `restarting`, `exited`…
    pub state: String,
    /// The state as the dock writes it: "reiniciando · 14×", "0,4% · 182 MB".
    pub state_label: String,
    pub log_lines: Vec<DockerLogLine>,
}

/// What `elastic_esql` and `elastic_recent_errors` keep for the thread: the rows as the dock
/// shows them, unmasked, since only the user sees the card; the model got a masked copy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ElasticQueryResult {
    pub connection: ElasticConnectionInfo,
    pub query: String,
    pub window_minutes: u32,
    /// The first index pattern after FROM, for the title and "Seguir".
    pub source: Option<String>,
    pub columns: Vec<ElasticColumn>,
    /// Values as text; `None` is null. Capped, see `total_rows`.
    pub rows: Vec<Vec<Option<String>>>,
    pub total_rows: usize,
    pub limit: usize,
    pub took_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ElasticTraceSpan {
    pub name: String,
    pub service: String,
    /// `transaction` or `span`.
    pub event: String,
    pub kind: Option<String>,
    pub depth: usize,
    /// From the start of the trace.
    pub offset_us: u64,
    pub duration_us: u64,
    pub failed: bool,
    pub status: Option<u64>,
}

/// What `elastic_trace` keeps for the thread: the waterfall, capped to its first spans.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ElasticTraceResult {
    pub connection: ElasticConnectionInfo,
    pub trace_id: String,
    pub duration_us: u64,
    pub spans: Vec<ElasticTraceSpan>,
    pub total_spans: usize,
    pub errors: Vec<String>,
    pub log_count: usize,
}

pub const REPO_PULL_REQUEST_TOOL: &str = "repo_pull_request";
pub const REPO_MERGE_TOOL: &str = "repo_pr_merge";

/// A pull request of the project's repository, as the agent's `@` menu offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestEntry {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub source_branch: String,
    pub destination_branch: String,
    /// Opened from the checked-out branch.
    pub is_current_branch: bool,
}

/// What a mention points at: the pull request itself, one of its comments or one of its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestReference {
    pub number: u64,
    pub comment_id: Option<u64>,
    pub file_path: Option<String>,
}

type ListPullRequests = Arc<dyn Fn(&Entity<Project>, &App) -> Vec<PullRequestEntry>>;
type DescribePullRequest =
    Arc<dyn Fn(&Entity<Project>, PullRequestReference, &mut App) -> Task<Result<String>>>;

/// Installed by the Repo dock, so the agent can offer and read its pull requests without
/// depending on it.
#[derive(Clone)]
pub struct PullRequestSource {
    /// The open pull requests from the dock's last sync, the checked-out branch's first.
    pub list: ListPullRequests,
    /// What the agent receives when the pull request is mentioned.
    pub describe: DescribePullRequest,
}

impl Global for PullRequestSource {}

pub fn register_pull_request_source(source: PullRequestSource, cx: &mut App) {
    cx.set_global(source);
}

pub fn pull_requests(project: &Entity<Project>, cx: &App) -> Vec<PullRequestEntry> {
    if !toolkit_enabled(toolkit_ids::REPO, project, cx) {
        return Vec::new();
    }
    cx.try_global::<PullRequestSource>()
        .map(|source| (source.list)(project, cx))
        .unwrap_or_default()
}

pub fn describe_pull_request(
    project: &Entity<Project>,
    reference: PullRequestReference,
    cx: &mut App,
) -> Task<Result<String>> {
    if !toolkit_enabled(toolkit_ids::REPO, project, cx) {
        return Task::ready(Err(toolkit_disabled_error(toolkit_ids::REPO, cx)));
    }
    match cx.try_global::<PullRequestSource>() {
        Some(source) => {
            let describe = source.describe.clone();
            describe(project, reference, cx)
        }
        None => Task::ready(Err(anyhow::anyhow!("This build has no Repo dock."))),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestCheckState {
    Passed,
    Failed,
    #[default]
    Running,
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequestCheckCard {
    pub name: String,
    pub state: PullRequestCheckState,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewerCard {
    pub name: String,
    pub approved: bool,
    pub requested_changes: bool,
}

/// What `repo_pull_request` keeps for the thread, so the card draws the pull request instead of
/// the model retyping its checks and reviews.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequestCard {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `open`, `merged`, `declined` or `superseded`.
    pub state: String,
    pub draft: bool,
    pub author: String,
    pub source_branch: String,
    pub destination_branch: String,
    pub is_current_branch: bool,
    pub checks: Vec<PullRequestCheckCard>,
    pub reviewers: Vec<PullRequestReviewerCard>,
    pub file_count: usize,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub conflict_count: usize,
    pub comment_count: u64,
    pub open_task_count: u64,
}

/// What `repo_pr_merge` keeps for the thread.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequestMergeCard {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub destination_branch: String,
    pub strategy: String,
}

pub const PROJECT_ITEMS_TOOL: &str = "project_items";
pub const PROJECT_ITEM_TOOL: &str = "project_item";

/// An item of the GitHub project, as the agent's `@` menu offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectItemEntry {
    pub project_id: String,
    pub item_id: String,
    /// `asylum#198`, or `rascunho`.
    pub reference: String,
    pub title: String,
    pub status: Option<String>,
    /// The item of the checked-out branch.
    pub is_current_branch: bool,
    pub is_assigned_to_you: bool,
}

type ListProjectItems = Arc<dyn Fn(&Entity<Project>, &App) -> Vec<ProjectItemEntry>>;
type DescribeProjectItem =
    Arc<dyn Fn(&Entity<Project>, String, String, &mut App) -> Task<Result<String>>>;

/// Installed by the Tarefas dock, so the agent can offer and read GitHub project items without
/// depending on it.
#[derive(Clone)]
pub struct ProjectItemSource {
    /// The items of the selected project as of the dock's last sync.
    pub list: ListProjectItems,
    /// What the agent receives when the item is mentioned, by project and item id.
    pub describe: DescribeProjectItem,
}

impl Global for ProjectItemSource {}

pub fn register_project_item_source(source: ProjectItemSource, cx: &mut App) {
    cx.set_global(source);
}

pub fn project_items(project: &Entity<Project>, cx: &App) -> Vec<ProjectItemEntry> {
    if !toolkit_enabled(toolkit_ids::GITHUB_PROJECTS, project, cx) {
        return Vec::new();
    }
    cx.try_global::<ProjectItemSource>()
        .map(|source| (source.list)(project, cx))
        .unwrap_or_default()
}

pub fn describe_project_item(
    project: &Entity<Project>,
    project_id: String,
    item_id: String,
    cx: &mut App,
) -> Task<Result<String>> {
    if !toolkit_enabled(toolkit_ids::GITHUB_PROJECTS, project, cx) {
        return Task::ready(Err(toolkit_disabled_error(
            toolkit_ids::GITHUB_PROJECTS,
            cx,
        )));
    }
    match cx.try_global::<ProjectItemSource>() {
        Some(source) => {
            let describe = source.describe.clone();
            describe(project, project_id, item_id, cx)
        }
        None => Task::ready(Err(anyhow::anyhow!("This build has no Tarefas dock."))),
    }
}

/// One row of a [`ProjectItemsCard`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectItemRow {
    pub item_id: String,
    pub reference: String,
    pub title: String,
    pub status: Option<String>,
    /// GitHub's color name of the status option, such as `YELLOW`.
    pub status_color: Option<String>,
    /// `P1`, `em andamento`: what the row shows next to the title.
    pub detail: Option<String>,
    /// Issues without a branch yet can be started from the card.
    pub can_start: bool,
    pub url: Option<String>,
}

/// What `project_items` keeps for the thread, so the card lists the items instead of the model
/// retyping them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectItemsCard {
    pub project_id: String,
    pub project_title: String,
    pub project_url: String,
    /// `Sprint 42 · seus itens`.
    pub title: String,
    pub items: Vec<ProjectItemRow>,
}

/// What an esteira's GitHub step asks the Tarefas dock to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitHubStepRequest {
    /// `start_item`, `open_pr`, `wait_checks`, `move_item` or `comment_item`.
    pub action: String,
    pub draft: bool,
    pub timeout_minutes: u32,
    /// The column `move_item` moves to.
    pub status: String,
    /// What `comment_item` posts; empty posts `summary`.
    pub comment: String,
    /// The run's input, where `#204` names the item when there is one; otherwise the item is
    /// the one of the checked-out branch.
    pub request: String,
    /// What the run's steps delivered so far, for the pull request and comments.
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitHubStepOutcome {
    pub passed: bool,
    /// What the run's log says about the step, and the reason when it failed.
    pub evidence: String,
    /// `branch`, `pr`, `checks`, `log do que falhou`.
    pub outputs: std::collections::BTreeMap<String, String>,
}

type GitHubStepsRepository = Arc<dyn Fn(&Entity<Project>, &App) -> Option<String>>;
type RunGitHubStep =
    Arc<dyn Fn(&Entity<Project>, GitHubStepRequest, &mut App) -> Task<Result<GitHubStepOutcome>>>;

/// Installed by the Tarefas dock, so esteiras can have GitHub steps without depending on it.
#[derive(Clone)]
pub struct GitHubStepRunner {
    /// `owner/name` when the project is on GitHub, so the editor offers the steps.
    pub repository: GitHubStepsRepository,
    pub run: RunGitHubStep,
}

impl Global for GitHubStepRunner {}

pub fn register_github_step_runner(runner: GitHubStepRunner, cx: &mut App) {
    cx.set_global(runner);
}

/// The GitHub repository esteiras of `project` can have GitHub steps for.
pub fn github_steps_repository(project: &Entity<Project>, cx: &App) -> Option<String> {
    cx.try_global::<GitHubStepRunner>()
        .and_then(|runner| (runner.repository)(project, cx))
}

pub fn run_github_step(
    project: &Entity<Project>,
    request: GitHubStepRequest,
    cx: &mut App,
) -> Task<Result<GitHubStepOutcome>> {
    match cx.try_global::<GitHubStepRunner>() {
        Some(runner) => {
            let run = runner.run.clone();
            run(project, request, cx)
        }
        None => Task::ready(Err(anyhow::anyhow!("Este build não tem o dock Tarefas."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use settings::SettingsStore;
    use util::path;

    #[gpui::test]
    async fn test_project_settings_override_the_profile(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/app"),
            serde_json::json!({
                ".zed": { "settings.json": r#"{ "agent_toolkits": { "elastic": true } }"# },
                "main.rs": "",
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/app").as_ref()], cx).await;
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(toolkit_enabled(toolkit_ids::CLICKUP, &project, cx));
            assert_eq!(
                toolkit_project_override(toolkit_ids::ELASTIC, &project, cx),
                None
            );

            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    let toolkits = &mut content.project.agent_toolkits;
                    toolkits.insert(toolkit_ids::CLICKUP.into(), false);
                    toolkits.insert(toolkit_ids::ELASTIC.into(), false);
                });
            });

            assert!(!toolkit_enabled_in_profile(toolkit_ids::CLICKUP, cx));
            assert!(!toolkit_enabled(toolkit_ids::CLICKUP, &project, cx));
            assert!(!toolkit_enabled_in_profile(toolkit_ids::ELASTIC, cx));
            assert_eq!(
                toolkit_project_override(toolkit_ids::ELASTIC, &project, cx),
                Some(true)
            );
            assert!(toolkit_enabled(toolkit_ids::ELASTIC, &project, cx));
        });
    }

    #[test]
    fn test_screenshot_scale() {
        assert_eq!(screenshot_scale(800, 600), 1.0);
        let scale = screenshot_scale(1080, 2400);
        assert!((scale - 1280.0 / 2400.0).abs() < 1e-9);
    }

    #[test]
    fn test_scale_screenshot_keeps_aspect() {
        let image = image::DynamicImage::new_rgba8(1080, 2400);
        let scaled = scale_screenshot(image).expect("scaled");
        assert_eq!((scaled.width, scaled.height), (576, 1280));
        assert!(!scaled.png.is_empty());
    }

    #[test]
    fn test_permission_inputs() {
        let input = serde_json::json!({ "url": "http://localhost", "x": 10, "full": true });
        let mut inputs = ToolkitTool::permission_inputs(&input);
        inputs.sort();
        assert_eq!(
            inputs,
            vec!["10".to_string(), "http://localhost".to_string()]
        );
    }
}
