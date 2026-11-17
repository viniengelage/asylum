//! The agent's CloudWatch tools. They read through the connection of the project's CloudWatch
//! store and never write; what they return to the model has personal data masked first.
//! `cloudwatch_insights_query` reads at most `AGENT_FREE_WINDOW` without asking,
//! `cloudwatch_wide_insights_query` goes further and asks every time.

mod mask;
mod query_text;

pub(crate) use mask::mask;
pub(crate) use query_text::stack_locations;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, anyhow};
use gpui::{App, AppContext as _, Entity, EntityId, Global, Task, WeakEntity, Window};
use i18n::t;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use task_agents::{
    CloudWatchReadKind, CloudWatchReadResult, CloudWatchSource, LogMention, ToolAccess, Toolkit,
    ToolkitOutput, ToolkitTool,
};
use workspace::Workspace;

use crate::{
    AGENT_FREE_WINDOW, CloudWatch, CloudWatchError, CloudWatchStore, Connection, LogEvent,
    QueryResult,
};

const DEFAULT_WINDOW_MINUTES: u32 = 30;
const MAX_WIDE_WINDOW_MINUTES: u32 = 30 * 24 * 60;
const DEFAULT_ROW_LIMIT: u32 = 100;
const MAX_ROW_LIMIT: u32 = 1000;
const DEFAULT_EVENT_LIMIT: u32 = 50;
const MAX_EVENT_LIMIT: u32 = 200;
const DEFAULT_GROUP_LIMIT: usize = 200;
const MAX_CELL_CHARS: usize = 300;
const MAX_OUTPUT_CHARS: usize = 40_000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MENTION_TAIL_MINUTES: u64 = 15;
const MENTION_TAIL_EVENTS: i32 = 20;

const GROUP_PREFIX: &str = "cloudwatch:group:";
const QUERY_PREFIX: &str = "cloudwatch:query:";
const CONTEXT_PREFIX: &str = "cloudwatch:context:";

fn free_window_minutes() -> u32 {
    (AGENT_FREE_WINDOW.as_secs() / 60) as u32
}

#[derive(Deserialize, JsonSchema)]
struct InsightsInput {
    /// One Logs Insights query, e.g. `fields @timestamp, @message | filter level = "error" |
    /// stats count(*) by msg`. Don't filter on @timestamp for the window: `window_minutes` does
    /// it.
    query: String,
    /// Log group names. Defaults to the project's log groups.
    #[serde(default)]
    log_groups: Option<Vec<String>>,
    /// How far back from now to read, in minutes. Defaults to 30.
    #[serde(default)]
    window_minutes: Option<u32>,
    /// Maximum rows to return. Defaults to 100, at most 1000.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct RecentEventsInput {
    /// The log group to read. Defaults to the project's first log group.
    #[serde(default)]
    log_group: Option<String>,
    /// A CloudWatch filter pattern, e.g. `ERROR` or `{ $.level = "error" }`.
    #[serde(default)]
    filter_pattern: Option<String>,
    /// How far back from now, in minutes. Defaults to 30, at most 60.
    #[serde(default)]
    window_minutes: Option<u32>,
    /// Maximum events. Defaults to 50, at most 200.
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct ListLogGroupsInput {
    /// Only groups whose name starts with this, e.g. `/ecs/`.
    #[serde(default)]
    prefix: Option<String>,
}

/// The workspace of each project, so tools and mentions that only get the project can reach the
/// workspace's CloudWatch store.
#[derive(Default)]
struct ProjectWorkspaces(HashMap<EntityId, WeakEntity<Workspace>>);

impl Global for ProjectWorkspaces {}

fn workspace_for(project: &Entity<Project>, cx: &App) -> Option<WeakEntity<Workspace>> {
    cx.try_global::<ProjectWorkspaces>()?
        .0
        .get(&project.entity_id())
        .filter(|workspace| workspace.upgrade().is_some())
        .cloned()
}

fn existing_store(project: &Entity<Project>, cx: &App) -> Option<Entity<CloudWatchStore>> {
    CloudWatchStore::try_for_workspace(&workspace_for(project, cx)?, cx)
}

fn store_for(project: &Entity<Project>, cx: &mut App) -> anyhow::Result<Entity<CloudWatchStore>> {
    let workspace =
        workspace_for(project, cx).context(t!("cloudwatch_agent.error.no_workspace"))?;
    Ok(CloudWatchStore::for_workspace(
        workspace,
        project.clone(),
        cx,
    ))
}

/// The store's client, waiting for a connection that is still coming up.
fn connected_client(
    store: Entity<CloudWatchStore>,
    cx: &mut App,
) -> Task<anyhow::Result<CloudWatch>> {
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match store.read_with(cx, |store, _| store.connection().clone()) {
                Connection::Connected(client) => return Ok(client),
                Connection::SessionExpired { profile, .. } => {
                    let profile = profile.unwrap_or_else(|| "default".to_string());
                    return Err(anyhow!(t!(
                        "cloudwatch_agent.error.session_expired",
                        profile = profile
                    )));
                }
                Connection::Failed(message) => {
                    return Err(anyhow!(t!(
                        "cloudwatch_agent.error.not_connected",
                        message = message
                    )));
                }
                Connection::Idle => store.update(cx, |store, cx| store.connect(cx)),
                Connection::Connecting => {}
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(t!("cloudwatch_agent.error.connect_timeout")));
            }
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
        }
    })
}

/// Runs `operation` on Tokio with the store's client, reporting AWS errors to the store so the
/// dock shows an expired session too.
fn with_client<R, Fut>(
    store: Entity<CloudWatchStore>,
    cx: &mut App,
    operation: impl FnOnce(CloudWatch) -> Fut + 'static,
) -> Task<anyhow::Result<R>>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R, CloudWatchError>> + Send + 'static,
{
    let client = connected_client(store.clone(), cx);
    cx.spawn(async move |cx| {
        let client = client.await?;
        let result = gpui_tokio::Tokio::spawn(cx, operation(client)).await;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                let message = error.to_string();
                store.update(cx, |store, cx| store.report_error(&error, cx));
                Err(anyhow!(message))
            }
            Err(error) => Err(anyhow!(error)),
        }
    })
}

fn unix_millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn format_timestamp(milliseconds: Option<i64>) -> String {
    milliseconds
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn cell(value: &str) -> String {
    let text = mask(value).replace(['\n', '\r'], " ").replace('|', "\\|");
    query_text::truncate(&text, MAX_CELL_CHARS)
}

/// The rows as a markdown table, masked, for the model.
fn rows_as_table(rows: &[Vec<(String, String)>]) -> String {
    let mut columns: Vec<&str> = Vec::new();
    for row in rows {
        for (name, _) in row {
            if !columns.contains(&name.as_str()) {
                columns.push(name);
            }
        }
    }
    if columns.is_empty() {
        return String::new();
    }
    let mut text = format!(
        "| {} |\n|{}\n",
        columns
            .iter()
            .map(|column| column.replace('|', "\\|"))
            .collect::<Vec<_>>()
            .join(" | "),
        " --- |".repeat(columns.len())
    );
    for row in rows {
        let line = format!(
            "| {} |\n",
            columns
                .iter()
                .map(|column| {
                    row.iter()
                        .find(|(name, _)| name == column)
                        .map(|(_, value)| cell(value))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" | ")
        );
        if text.len() + line.len() > MAX_OUTPUT_CHARS {
            text.push_str("… (output cut)\n");
            break;
        }
        text.push_str(&line);
    }
    text
}

fn resolve_groups(
    requested: Option<Vec<String>>,
    store: &Entity<CloudWatchStore>,
    cx: &App,
) -> anyhow::Result<Vec<String>> {
    let groups = requested
        .unwrap_or_default()
        .into_iter()
        .map(|group| group.trim().to_string())
        .filter(|group| !group.is_empty())
        .collect::<Vec<_>>();
    if !groups.is_empty() {
        return Ok(groups);
    }
    let project_groups = store.read(cx).project_groups().to_vec();
    if project_groups.is_empty() {
        Err(anyhow!(t!("cloudwatch_agent.error.no_groups")))
    } else {
        Ok(project_groups)
    }
}

fn run_insights(
    project: Entity<Project>,
    input: InsightsInput,
    max_window: u32,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let window_minutes = input
        .window_minutes
        .unwrap_or(DEFAULT_WINDOW_MINUTES)
        .max(1);
    if window_minutes > max_window {
        let message = if max_window == free_window_minutes() {
            t!(
                "cloudwatch_agent.error.window_needs_wide",
                window = window_minutes,
                max = max_window
            )
        } else {
            t!(
                "cloudwatch_agent.error.window_too_wide",
                window = window_minutes,
                max = max_window
            )
        };
        return Task::ready(Err(anyhow!(message)));
    }
    let query = input.query.trim().to_string();
    if query_text::commands(&query).is_empty() {
        return Task::ready(Err(anyhow!(t!("cloudwatch_agent.error.empty_query"))));
    }
    let limit = input
        .limit
        .unwrap_or(DEFAULT_ROW_LIMIT)
        .clamp(1, MAX_ROW_LIMIT);
    let store = match store_for(&project, cx) {
        Ok(store) => store,
        Err(error) => return Task::ready(Err(error)),
    };
    let log_groups = match resolve_groups(input.log_groups, &store, cx) {
        Ok(groups) => groups,
        Err(error) => return Task::ready(Err(error)),
    };
    let end = SystemTime::now();
    let start = end - Duration::from_secs(u64::from(window_minutes) * 60);
    let run = with_client(store.clone(), cx, {
        let log_groups = log_groups.clone();
        let query = query.clone();
        move |client| async move {
            client
                .run_insights_query(&log_groups, &query, start, end, limit as i32, POLL_INTERVAL)
                .await
        }
    });
    cx.spawn(async move |cx| {
        let result = run.await?;
        let bytes_scanned = result.bytes_scanned.max(0.0) as u64;
        store.update(cx, |store, cx| store.record_query(bytes_scanned, cx));
        Ok(insights_output(
            &log_groups,
            &query,
            unix_millis(start),
            unix_millis(end),
            window_minutes,
            &result,
        ))
    })
}

fn insights_output(
    log_groups: &[String],
    query: &str,
    start_ms: i64,
    end_ms: i64,
    window_minutes: u32,
    result: &QueryResult,
) -> ToolkitOutput {
    let grouped_by = query_text::grouped_by(query);
    let records_matched = result.records_matched.max(0.0) as u64;
    let bytes_scanned = result.bytes_scanned.max(0.0) as u64;
    let mut text = format!(
        "CloudWatch Logs Insights · {} · last {window_minutes} min · {} row(s) · {records_matched} \
         matching events · {} scanned\n\n",
        log_groups.join(", "),
        result.rows.len(),
        megabytes(bytes_scanned),
    );
    if result.rows.is_empty() {
        text.push_str("No rows in the window.\n");
    } else {
        text.push_str(&rows_as_table(&result.rows));
    }
    text.push_str(
        "\nPersonal data was masked («email», «cpf»…). The user sees a card with the groups, \
         window and totals under this call: answer with the fact (how many, which, since when, \
         where in the code) without repeating the table.",
    );
    let card = CloudWatchReadResult {
        kind: CloudWatchReadKind::Insights,
        log_groups: log_groups.to_vec(),
        start_ms,
        end_ms,
        query: query.to_string(),
        summary: query_text::summary(query),
        finding: query_text::finding(&result.rows, grouped_by.as_deref()),
        grouped_by,
        rows: result.rows.len(),
        records_matched,
        bytes_scanned,
    };
    ToolkitOutput::text(text).with_raw_output(card)
}

fn recent_events(
    project: Entity<Project>,
    input: RecentEventsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let window_minutes = input
        .window_minutes
        .unwrap_or(DEFAULT_WINDOW_MINUTES)
        .clamp(1, free_window_minutes());
    let limit = input
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let store = match store_for(&project, cx) {
        Ok(store) => store,
        Err(error) => return Task::ready(Err(error)),
    };
    let log_group = match resolve_groups(input.log_group.map(|group| vec![group]), &store, cx)
        .map(|groups| groups.into_iter().next())
    {
        Ok(Some(group)) => group,
        Ok(None) => return Task::ready(Err(anyhow!(t!("cloudwatch_agent.error.no_groups")))),
        Err(error) => return Task::ready(Err(error)),
    };
    let filter_pattern = input
        .filter_pattern
        .map(|pattern| pattern.trim().to_string())
        .filter(|pattern| !pattern.is_empty());
    let end = SystemTime::now();
    let start = end - Duration::from_secs(u64::from(window_minutes) * 60);
    let run = with_client(store, cx, {
        let log_group = log_group.clone();
        let filter_pattern = filter_pattern.clone();
        move |client| async move {
            client
                .filter_log_events(&log_group, filter_pattern.as_deref(), start, limit as i32)
                .await
        }
    });
    cx.spawn(async move |_| {
        let events = run.await?;
        Ok(events_output(
            &log_group,
            filter_pattern.as_deref(),
            unix_millis(start),
            unix_millis(end),
            window_minutes,
            &events,
        ))
    })
}

fn events_as_text(events: &[LogEvent]) -> String {
    let mut text = String::new();
    for event in events {
        let line = format!(
            "{} [{}] {}\n",
            format_timestamp(event.timestamp_ms),
            event.log_stream.as_deref().unwrap_or("-"),
            query_text::truncate(&mask(event.message.trim_end()), MAX_CELL_CHARS * 4),
        );
        if text.len() + line.len() > MAX_OUTPUT_CHARS {
            text.push_str("… (output cut)\n");
            break;
        }
        text.push_str(&line);
    }
    text
}

fn events_output(
    log_group: &str,
    filter_pattern: Option<&str>,
    start_ms: i64,
    end_ms: i64,
    window_minutes: u32,
    events: &[LogEvent],
) -> ToolkitOutput {
    let mut text = format!(
        "CloudWatch events of {log_group} · last {window_minutes} min{} · {} event(s), oldest \
         first\n\n",
        filter_pattern
            .map(|pattern| format!(" · filter {pattern}"))
            .unwrap_or_default(),
        events.len(),
    );
    if events.is_empty() {
        text.push_str("No events in the window.\n");
    } else {
        text.push_str(&events_as_text(events));
    }
    text.push_str(
        "\nPersonal data was masked. For counts and grouping use cloudwatch_insights_query.",
    );
    let card = CloudWatchReadResult {
        kind: CloudWatchReadKind::Events,
        log_groups: vec![log_group.to_string()],
        start_ms,
        end_ms,
        query: filter_pattern.unwrap_or_default().to_string(),
        summary: filter_pattern.unwrap_or_default().to_string(),
        grouped_by: None,
        rows: events.len(),
        records_matched: events.len() as u64,
        bytes_scanned: 0,
        finding: None,
    };
    ToolkitOutput::text(text).with_raw_output(card)
}

fn list_log_groups(
    project: Entity<Project>,
    input: ListLogGroupsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let store = match store_for(&project, cx) {
        Ok(store) => store,
        Err(error) => return Task::ready(Err(error)),
    };
    let project_groups = store.read(cx).project_groups().to_vec();
    let prefix = input
        .prefix
        .map(|prefix| prefix.trim().to_string())
        .filter(|prefix| !prefix.is_empty());
    let run = with_client(store.clone(), cx, {
        let prefix = prefix.clone();
        move |client| async move {
            client
                .list_log_groups(prefix.as_deref(), DEFAULT_GROUP_LIMIT)
                .await
        }
    });
    cx.spawn(async move |cx| {
        let groups = run.await?;
        let (profile, region) = store.read_with(cx, |store, _| {
            (store.profile_name(), store.region().unwrap_or_default())
        });
        let mut text = format!(
            "{} CloudWatch log group(s) in profile {profile}, region {region}{}:\n",
            groups.len(),
            prefix
                .as_deref()
                .map(|prefix| format!(" starting with {prefix}"))
                .unwrap_or_default()
        );
        if !project_groups.is_empty() {
            text.push_str(&format!(
                "This project's groups: {}\n",
                project_groups.join(", ")
            ));
        }
        for group in &groups {
            let mut details = Vec::new();
            if let Some(bytes) = group.stored_bytes {
                details.push(megabytes(bytes.max(0) as u64));
            }
            if let Some(days) = group.retention_days {
                details.push(format!("retention {days} d"));
            }
            text.push_str(&format!(
                "- {}{}\n",
                group.name,
                if details.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", details.join(", "))
                }
            ));
        }
        if groups.len() >= DEFAULT_GROUP_LIMIT {
            text.push_str("… (more groups exist: narrow with `prefix`)\n");
        }
        let now = unix_millis(SystemTime::now());
        let card = CloudWatchReadResult {
            kind: CloudWatchReadKind::LogGroups,
            log_groups: Vec::new(),
            start_ms: now,
            end_ms: now,
            query: prefix.clone().unwrap_or_default(),
            summary: prefix.unwrap_or_default(),
            grouped_by: None,
            rows: groups.len(),
            records_matched: 0,
            bytes_scanned: 0,
            finding: None,
        };
        Ok(ToolkitOutput::text(text).with_raw_output(card))
    })
}

/// What the CloudWatch views handed to the agent, kept until the app quits so a mention can be
/// read again when the thread is sent.
#[derive(Default)]
struct Remembered {
    next: usize,
    entries: HashMap<String, String>,
}

impl Global for Remembered {}

pub(crate) fn remember(text: String, cx: &mut App) -> String {
    let remembered = cx.default_global::<Remembered>();
    remembered.next += 1;
    let id = format!("{CONTEXT_PREFIX}{}", remembered.next);
    remembered.entries.insert(id.clone(), text);
    id
}

pub(crate) fn group_mention_id(log_group: &str) -> String {
    format!("{GROUP_PREFIX}{log_group}")
}

fn query_mention_id(path: &Path) -> String {
    format!("{QUERY_PREFIX}{}", path.display())
}

fn list_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    let Some(store) = existing_store(project, cx) else {
        return Vec::new();
    };
    let store = store.read(cx);
    let detail = format!("CloudWatch · {}", store.profile_name());
    let groups = store.project_groups().iter().map(|group| LogMention {
        id: group_mention_id(group).into(),
        title: group.clone().into(),
        detail: detail.clone().into(),
    });
    let queries = store.saved_queries().iter().map(|query| LogMention {
        id: query_mention_id(&query.path).into(),
        title: query.name.clone().into(),
        detail: detail.clone().into(),
    });
    groups.chain(queries).collect()
}

fn describe_mention(
    project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    if let Some(log_group) = id.strip_prefix(GROUP_PREFIX) {
        return describe_group(project, log_group.to_string(), cx);
    }
    if let Some(path) = id.strip_prefix(QUERY_PREFIX) {
        return describe_query(project, PathBuf::from(path), cx);
    }
    let text = cx
        .try_global::<Remembered>()
        .and_then(|remembered| remembered.entries.get(id).cloned());
    Task::ready(
        text.map(|text| {
            format!(
                "{}\n\nPersonal data masked. To read more, use the cloudwatch_* tools.",
                mask(&text)
            )
        })
        .context(t!("cloudwatch_agent.error.mention_forgotten")),
    )
}

fn describe_group(
    project: &Entity<Project>,
    log_group: String,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let store = match store_for(project, cx) {
        Ok(store) => store,
        Err(error) => return Task::ready(Err(error)),
    };
    let (profile, region, client) = {
        let store = store.read(cx);
        (
            store.profile_name(),
            store.region().unwrap_or_default(),
            store.client(),
        )
    };
    let header = format!(
        "CloudWatch log group `{log_group}` (profile {profile}, region {region}). Query it with \
         cloudwatch_insights_query (log_groups: [\"{log_group}\"])."
    );
    let Some(client) = client else {
        return Task::ready(Ok(header));
    };
    let start = SystemTime::now() - Duration::from_secs(MENTION_TAIL_MINUTES * 60);
    let events = gpui_tokio::Tokio::spawn(cx, {
        async move {
            client
                .filter_log_events(&log_group, None, start, MENTION_TAIL_EVENTS)
                .await
        }
    });
    cx.spawn(async move |cx| {
        let events = match events.await {
            Ok(Ok(events)) => events,
            Ok(Err(error)) => {
                let message = error.to_string();
                store.update(cx, |store, cx| store.report_error(&error, cx));
                return Ok(format!(
                    "{header}\nThe latest events could not be read: {message}"
                ));
            }
            Err(error) => {
                log::error!("reading the latest CloudWatch events: {error}");
                return Ok(header);
            }
        };
        if events.is_empty() {
            return Ok(format!(
                "{header}\nNo events in the last {MENTION_TAIL_MINUTES} min."
            ));
        }
        Ok(format!(
            "{header}\nLatest events (last {MENTION_TAIL_MINUTES} min, personal data masked):\n```\n{}```",
            events_as_text(&events)
        ))
    })
}

fn describe_query(
    project: &Entity<Project>,
    path: PathBuf,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let fs = project.read(cx).fs().clone();
    let project_groups = existing_store(project, cx)
        .map(|store| store.read(cx).project_groups().to_vec())
        .unwrap_or_default();
    cx.background_spawn(async move {
        let text = fs.load(&path).await.with_context(|| {
            t!(
                "cloudwatch_agent.error.query_unreadable",
                path = path.display()
            )
        })?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(query_context(&name, &text, &project_groups))
    })
}

pub(crate) fn query_context(name: &str, query: &str, log_groups: &[String]) -> String {
    let mut text = format!(
        "Logs Insights query `{name}`:\n```\n{}\n```\n",
        query.trim_end()
    );
    if !log_groups.is_empty() {
        text.push_str(&format!("Log groups: {}\n", log_groups.join(", ")));
    }
    text.push_str("Run it or a variation with cloudwatch_insights_query.");
    text
}

fn open_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    if let Some(path) = id.strip_prefix(QUERY_PREFIX) {
        let workspace = workspace_for(project, cx)
            .and_then(|workspace| workspace.upgrade())
            .context(t!("cloudwatch_agent.error.no_workspace"))?;
        let path = PathBuf::from(path);
        workspace.update(cx, |workspace, cx| {
            crate::open_query(workspace, path, window, cx)
        });
        return Ok(());
    }
    if id.starts_with(GROUP_PREFIX) {
        window.dispatch_action(Box::new(crate::ToggleFocus), cx);
    }
    Ok(())
}

fn remember_workspaces(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        let project_id = workspace.project().entity_id();
        let handle = cx.entity().downgrade();
        let workspaces = cx.default_global::<ProjectWorkspaces>();
        workspaces
            .0
            .retain(|_, workspace| workspace.upgrade().is_some());
        workspaces.0.insert(project_id, handle);
    })
    .detach();
}

pub fn register_toolkit(cx: &mut App) {
    remember_workspaces(cx);
    let free_window = free_window_minutes();
    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::CLOUDWATCH.into(),
            name: "CloudWatch".into(),
            description: t!("cloudwatch_agent.toolkit.description"),
            icon: "cloud_pulse".into(),
            mention: Some("@Logs".into()),
            tools: vec![
                ToolkitTool::new(
                    task_agents::CLOUDWATCH_LIST_LOG_GROUPS_TOOL,
                    t!("cloudwatch_agent.toolkit.tool_list_log_groups"),
                    "Lists the CloudWatch log groups of the AWS profile and region set in the \
                     CloudWatch dock, with stored size and retention, and says which ones belong \
                     to this project. Call it first when the user names a service whose group you \
                     don't know.",
                    ToolAccess::Read,
                    list_log_groups,
                ),
                ToolkitTool::new(
                    task_agents::CLOUDWATCH_INSIGHTS_QUERY_TOOL,
                    t!("cloudwatch_agent.toolkit.tool_insights_query"),
                    format!(
                        "Runs one CloudWatch Logs Insights query over the last `window_minutes` \
                         (default {DEFAULT_WINDOW_MINUTES}, at most {free_window}) of \
                         `log_groups` (default: the project's groups) and returns the rows as a \
                         table, with personal data masked, plus how many events matched and how \
                         much was scanned. Insights bills by bytes scanned: keep the window \
                         short, filter early and aggregate with stats (e.g. `filter level = \
                         \"error\" | stats count(*) by msg`, or `stats count(*) by bin(1m)` for a \
                         timeline). Answer with the fact."
                    ),
                    ToolAccess::Read,
                    move |project, input: InsightsInput, cx| {
                        run_insights(project, input, free_window, cx)
                    },
                ),
                ToolkitTool::new(
                    task_agents::CLOUDWATCH_WIDE_INSIGHTS_QUERY_TOOL,
                    t!("cloudwatch_agent.toolkit.tool_wide_insights_query"),
                    format!(
                        "Same as cloudwatch_insights_query for windows longer than {free_window} \
                         min (up to 30 days). It asks the user first, showing the window and the \
                         groups, since it scans and bills much more. Prefer \
                         cloudwatch_insights_query and narrow the question when you can."
                    ),
                    ToolAccess::Act,
                    |project, input: InsightsInput, cx| {
                        run_insights(project, input, MAX_WIDE_WINDOW_MINUTES, cx)
                    },
                ),
                ToolkitTool::new(
                    task_agents::CLOUDWATCH_RECENT_EVENTS_TOOL,
                    t!("cloudwatch_agent.toolkit.tool_recent_events"),
                    format!(
                        "Reads the raw events of one log group from the last `window_minutes` \
                         (default {DEFAULT_WINDOW_MINUTES}, at most {free_window}), optionally \
                         matching a CloudWatch filter pattern, oldest first and masked. Use it to \
                         read what happened around an event; for counts use \
                         cloudwatch_insights_query."
                    ),
                    ToolAccess::Read,
                    recent_events,
                ),
            ],
        },
        cx,
    );
    task_agents::register_cloudwatch_source(
        CloudWatchSource {
            list: Arc::new(list_mentions),
            describe: Arc::new(describe_mention),
            open: Arc::new(open_mention),
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_ids_route_to_cloudwatch() {
        for prefix in [GROUP_PREFIX, QUERY_PREFIX, CONTEXT_PREFIX] {
            assert!(prefix.starts_with(task_agents::CLOUDWATCH_MENTION_PREFIX));
        }
    }

    #[test]
    fn tables_mask_and_flatten_cells() {
        let rows = vec![
            vec![
                ("msg".to_string(), "falhou para ana@trix.com.br".to_string()),
                ("count(*)".to_string(), "3".to_string()),
            ],
            vec![("msg".to_string(), "linha\ncom | barra".to_string())],
        ];
        assert_eq!(
            rows_as_table(&rows),
            "| msg | count(*) |\n| --- | --- |\n| falhou para «email» | 3 |\n| linha com \\| barra |  |\n"
        );
    }

    #[test]
    fn insights_output_keeps_the_card_numbers() {
        let result = QueryResult {
            rows: vec![
                vec![
                    ("msg".to_string(), "Adquirente respondeu 502".to_string()),
                    ("count(*)".to_string(), "22".to_string()),
                ],
                vec![
                    ("msg".to_string(), "timeout".to_string()),
                    ("count(*)".to_string(), "15".to_string()),
                ],
            ],
            records_matched: 37.0,
            records_scanned: 1000.0,
            bytes_scanned: 412_000_000.0,
        };
        let output = insights_output(
            &["/ecs/trix-api-prod".to_string()],
            "filter level = \"error\" | stats count(*) by msg",
            0,
            1_800_000,
            30,
            &result,
        );
        let card: CloudWatchReadResult =
            serde_json::from_value(output.raw_output.expect("raw output")).expect("card");
        assert_eq!(card.summary, "level = \"error\" · count(*) by msg");
        assert_eq!(card.grouped_by.as_deref(), Some("msg"));
        assert_eq!(card.rows, 2);
        assert_eq!(card.records_matched, 37);
        assert_eq!(card.bytes_scanned, 412_000_000);
        assert_eq!(card.finding.map(|finding| finding.count), Some(22));
    }
}
