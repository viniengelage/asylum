//! What a run returned and how the results area shows it: the events table with the expanded
//! event, the histogram, the chart and fields tabs, and the footer with the exports.

use std::{
    collections::HashMap,
    path::Path,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use editor::Editor;
use gpui::{AnyElement, App, ClipboardItem, Entity, FontWeight, WeakEntity, Window, list, px};
use i18n::t;
use project::Project;
use regex::Regex;
use serde_json::Value;
use ui::{
    EmptyState, StatusTone, Tooltip, error_callout, prelude::*, secondary_button, segment_option,
    segmented_control, status_pill,
};
use util::ResultExt as _;
use workspace::Workspace;

use super::{QueryView, ResultTab, RunState, TimeWindow, from_unix, insights};
use crate::{
    LogEvent,
    agent_bridge::{self, AgentContext},
};

pub const TIMESTAMP: &str = "@timestamp";
pub const LOG_STREAM: &str = "@logStream";
const MESSAGE_FIELDS: [&str; 4] = ["msg", "message", "@message", "log"];
const LEVEL_FIELDS: [&str; 5] = ["level", "log.level", "severity", "levelname", "lvl"];
const ID_FIELDS: [&str; 6] = [
    "requestId",
    "request_id",
    "traceId",
    "trace_id",
    "correlationId",
    "@requestId",
];
const STACK_FIELDS: [&str; 5] = [
    "err.stack",
    "error.stack",
    "stack",
    "stack_trace",
    "err.stack_trace",
];
const HIDDEN_FIELDS: [&str; 3] = ["@ptr", "@log", "@message"];
/// Logs Insights bills $0.005 per GB scanned.
const DOLLARS_PER_GB: f64 = 0.005;
const ROW_HEIGHT: f32 = 29.;

pub struct ResultSet {
    pub rows: Vec<Vec<(String, String)>>,
    /// Every field any row has, in the order they first appear.
    pub columns: Vec<String>,
    levels: Vec<Option<String>>,
    message_field: Option<String>,
    id_field: Option<String>,
}

impl ResultSet {
    pub fn new(rows: Vec<Vec<(String, String)>>) -> Self {
        let mut columns: Vec<String> = Vec::new();
        for row in &rows {
            for (name, _) in row {
                if !columns.contains(name) {
                    columns.push(name.clone());
                }
            }
        }
        let message_field = MESSAGE_FIELDS
            .iter()
            .find(|field| columns.iter().any(|column| column == *field))
            .map(|field| field.to_string());
        let id_field = ID_FIELDS
            .iter()
            .find(|field| columns.iter().any(|column| column == *field))
            .map(|field| field.to_string());
        let levels = rows
            .iter()
            .map(|row| {
                LEVEL_FIELDS
                    .iter()
                    .find_map(|field| value(row, field))
                    .map(str::to_string)
                    .or_else(|| {
                        let json = parse_json(value(row, "@message")?)?;
                        LEVEL_FIELDS
                            .iter()
                            .find_map(|field| json.get(*field)?.as_str().map(str::to_string))
                    })
            })
            .collect();
        Self {
            rows,
            columns,
            levels,
            message_field,
            id_field,
        }
    }

    /// Log lines when the rows are events with a message; otherwise a plain table of whatever
    /// `stats` returned.
    pub fn is_log(&self) -> bool {
        self.columns.iter().any(|column| column == TIMESTAMP) && self.message_field.is_some()
    }

    pub fn value(&self, row: usize, name: &str) -> Option<&str> {
        value(self.rows.get(row)?, name)
    }

    pub fn level(&self, row: usize) -> Option<&str> {
        self.levels.get(row)?.as_deref()
    }

    pub fn message(&self, row: usize) -> Option<&str> {
        self.value(row, self.message_field.as_deref()?)
    }

    pub fn errors(&self) -> usize {
        self.levels
            .iter()
            .filter(|level| level.as_deref().is_some_and(is_error_level))
            .count()
    }

    pub fn table_columns(&self) -> Vec<String> {
        if !self.is_log() {
            return self.columns.iter().take(8).cloned().collect();
        }
        let mut columns = vec![TIMESTAMP.to_string(), "level".to_string()];
        columns.extend(self.message_field.clone());
        if self.columns.iter().any(|column| column == LOG_STREAM) {
            columns.push(LOG_STREAM.to_string());
        }
        columns.extend(self.id_field.clone());
        columns
    }

    /// The fields the expanded event lists: the id, what the query returned outside the table
    /// and what the JSON message carries.
    pub fn detail_fields(&self, row: usize) -> Vec<(String, String)> {
        let Some(values) = self.rows.get(row) else {
            return Vec::new();
        };
        let shown = self.table_columns();
        let mut fields: Vec<(String, String)> = Vec::new();
        if let Some(id_field) = &self.id_field
            && let Some(id) = value(values, id_field)
        {
            fields.push((id_field.clone(), id.to_string()));
        }
        for (name, field_value) in values {
            let skip = shown.contains(name)
                || HIDDEN_FIELDS.contains(&name.as_str())
                || LEVEL_FIELDS.contains(&name.as_str())
                || STACK_FIELDS.contains(&name.as_str());
            if !skip && !fields.iter().any(|(existing, _)| existing == name) {
                fields.push((name.clone(), field_value.clone()));
            }
        }
        if let Some(json) = value(values, "@message").and_then(parse_json) {
            for (name, field_value) in flatten(&json) {
                let skip = MESSAGE_FIELDS.contains(&name.as_str())
                    || LEVEL_FIELDS.contains(&name.as_str())
                    || STACK_FIELDS.contains(&name.as_str());
                if !skip && !fields.iter().any(|(existing, _)| *existing == name) {
                    fields.push((name, field_value));
                }
            }
        }
        fields.truncate(10);
        fields
    }

    pub fn stack(&self, row: usize) -> Option<String> {
        let values = self.rows.get(row)?;
        STACK_FIELDS
            .iter()
            .find_map(|field| value(values, field).map(str::to_string))
            .or_else(|| {
                let json = flatten(&parse_json(value(values, "@message")?)?);
                STACK_FIELDS.iter().find_map(|field| {
                    json.iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, stack)| stack.clone())
                })
            })
    }

    pub fn row_json(&self, row: usize) -> Value {
        let mut object = serde_json::Map::new();
        for (name, field_value) in self.rows.get(row).into_iter().flatten() {
            object.insert(name.clone(), Value::String(field_value.clone()));
        }
        Value::Object(object)
    }

    pub fn to_json(&self) -> String {
        let rows: Vec<Value> = (0..self.rows.len()).map(|row| self.row_json(row)).collect();
        serde_json::to_string_pretty(&rows).unwrap_or_default()
    }

    pub fn to_csv(&self) -> String {
        let mut csv = self
            .columns
            .iter()
            .map(|column| csv_field(column))
            .collect::<Vec<_>>()
            .join(",");
        csv.push('\n');
        for row in &self.rows {
            csv.push_str(
                &self
                    .columns
                    .iter()
                    .map(|column| csv_field(value(row, column).unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            csv.push('\n');
        }
        csv
    }
}

fn value<'a>(row: &'a [(String, String)], name: &str) -> Option<&'a str> {
    row.iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
}

fn parse_json(text: &str) -> Option<serde_json::Map<String, Value>> {
    let start = text.find('{')?;
    match serde_json::from_str::<Value>(&text[start..]).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

/// Nested objects become dotted names (`err.stack`), the way Logs Insights names them.
fn flatten(object: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    fn walk(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&name, value, out);
                }
            }
            Value::String(text) => out.push((prefix.to_string(), text.clone())),
            Value::Null => {}
            other => out.push((prefix.to_string(), other.to_string())),
        }
    }
    let mut out = Vec::new();
    for (key, value) in object {
        walk(key, value, &mut out);
    }
    out
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn is_error_level(level: &str) -> bool {
    matches!(
        level.to_ascii_lowercase().as_str(),
        "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emergency"
    )
}

fn level_color(level: &str) -> Color {
    if is_error_level(level) {
        Color::Error
    } else if matches!(level.to_ascii_lowercase().as_str(), "warn" | "warning") {
        Color::Warning
    } else {
        Color::Muted
    }
}

/// The fields of one sampled event, as Logs Insights would discover them.
pub fn event_fields(event: &LogEvent) -> Vec<(String, String)> {
    let mut fields = vec![("@message".to_string(), event.message.clone())];
    if let Some(stream) = &event.log_stream {
        fields.push((LOG_STREAM.to_string(), stream.clone()));
    }
    if let Some(json) = parse_json(&event.message) {
        fields.extend(flatten(&json));
    }
    fields
}

pub fn sample_field_names(events: &[Vec<(String, String)>]) -> Vec<String> {
    let mut names = vec![TIMESTAMP.to_string()];
    for event in events {
        for (name, _) in event {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// Counts per bin over the whole window, with the empty bins the query leaves out.
pub fn histogram(
    rows: &[Vec<(String, String)>],
    from: i64,
    to: i64,
    bin_seconds: u64,
) -> Vec<(i64, u64)> {
    let bin = bin_seconds.max(1) as i64;
    let mut counts: HashMap<i64, u64> = HashMap::new();
    for row in rows {
        let time = row
            .iter()
            .find(|(name, _)| name.starts_with("bin("))
            .and_then(|(_, time)| parse_timestamp(time));
        let count = row
            .iter()
            .find(|(name, _)| name == "events")
            .and_then(|(_, count)| count.parse::<f64>().ok());
        if let (Some(time), Some(count)) = (time, count) {
            *counts.entry(time.div_euclid(bin) * bin).or_default() += count as u64;
        }
    }
    let first = from.div_euclid(bin) * bin;
    (0..)
        .map(|index| first + index * bin)
        .take_while(|start| *start <= to)
        .map(|start| (start, counts.get(&start).copied().unwrap_or_default()))
        .collect()
}

/// Where the histogram peaks: the bins next to the highest one that reach half of it.
pub fn peak(bins: &[(i64, u64)]) -> Option<Range> {
    let (index, max) = bins
        .iter()
        .enumerate()
        .max_by_key(|(_, (_, count))| *count)
        .map(|(index, (_, count))| (index, *count))?;
    if max == 0 {
        return None;
    }
    let threshold = max.div_ceil(2);
    let mut start = index;
    while start > 0 && bins[start - 1].1 >= threshold {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < bins.len() && bins[end + 1].1 >= threshold {
        end += 1;
    }
    Some(Range {
        bins: start..end + 1,
        count: bins[start..=end].iter().map(|(_, count)| count).sum(),
    })
}

pub struct Range {
    pub bins: std::ops::Range<usize>,
    pub count: u64,
}

/// `2026-11-17 13:41:52.318` (UTC, as Insights returns it) to Unix seconds.
pub fn parse_timestamp(text: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|time| time.and_utc().timestamp())
}

/// The event's local time, `13:41:52.318`.
pub fn local_time(text: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .map(|time| {
            time.and_utc()
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|_| text.to_string())
}

pub fn clock(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%H:%M")
        .to_string()
}

fn portuguese() -> bool {
    i18n::current_language() == i18n::Language::PortugueseBrazil
}

pub fn localized_decimal(text: String) -> String {
    if portuguese() {
        text.replace('.', ",")
    } else {
        text
    }
}

pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut amount = bytes.max(0.0);
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit >= 3 && amount < 100.0 {
        localized_decimal(format!("{amount:.1} {}", UNITS[unit]))
    } else {
        format!("{amount:.0} {}", UNITS[unit])
    }
}

pub fn format_cost(bytes: f64) -> String {
    let dollars = bytes / (1024.0 * 1024.0 * 1024.0) * DOLLARS_PER_GB;
    localized_decimal(format!("{dollars:.3}"))
}

pub fn format_seconds(duration: Duration) -> String {
    localized_decimal(format!("{:.1} s", duration.as_secs_f64()))
}

pub fn ago_label(time: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs();
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h", seconds / 3600)
    } else {
        format!("{} d", seconds / 86_400)
    }
}

/// A `file:line[:column]` inside a stack frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackLink {
    pub path: String,
    pub line: u32,
    pub column: Option<u32>,
    pub range: std::ops::Range<usize>,
}

static FRAME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?:file://|webpack://[^/]*/)?((?:[\w@.-]+/)*[\w@.-]+\.(?:tsx?|jsx?|mjs|cjs|rs|py|go|java|kt|rb|php|ex|exs|cs|swift|dart)):(\d+)(?::(\d+))?",
    )
    .log_err()
});

pub fn stack_link(frame: &str) -> Option<StackLink> {
    let captures = FRAME.as_ref()?.captures(frame)?;
    let path = captures.get(1)?;
    if path.as_str().contains("node_modules") || frame.contains("node:internal") {
        return None;
    }
    let whole = captures.get(0)?;
    Some(StackLink {
        path: path.as_str().to_string(),
        line: captures.get(2)?.as_str().parse().ok()?,
        column: captures
            .get(3)
            .and_then(|column| column.as_str().parse().ok()),
        range: whole.start()..whole.end(),
    })
}

/// Containers prefix paths (`/app/src/…`), so leading components go until a project file
/// matches.
pub fn resolve_in_project(
    project: &Entity<Project>,
    path: &str,
    cx: &App,
) -> Option<project::ProjectPath> {
    let mut candidate = path.trim_start_matches('/');
    loop {
        if let Some(found) = project.read(cx).find_project_path(Path::new(candidate), cx)
            && project.read(cx).entry_for_path(&found, cx).is_some()
        {
            return Some(found);
        }
        let (_, rest) = candidate.split_once('/')?;
        candidate = rest;
    }
}

pub fn open_stack_link(
    workspace: Option<WeakEntity<Workspace>>,
    project: &Entity<Project>,
    link: &StackLink,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace else {
        return;
    };
    let Some(project_path) = resolve_in_project(project, &link.path, cx) else {
        return;
    };
    let point = language::Point::new(
        link.line.saturating_sub(1),
        link.column.unwrap_or(1).saturating_sub(1),
    );
    let Some(open) = workspace
        .update(cx, |workspace, cx| {
            workspace.open_path(project_path, None, true, window, cx)
        })
        .log_err()
    else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(point, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

pub fn link_label(link: &StackLink) -> String {
    let file = Path::new(&link.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| link.path.clone());
    format!("{file}:{}", link.line)
}

fn column_cell(name: &str, log: bool) -> gpui::Div {
    let cell = div().min_w_0().overflow_hidden();
    if !log {
        return cell.flex_1();
    }
    match name {
        TIMESTAMP => cell.w(px(104.)).flex_none(),
        "level" => cell.w(px(56.)).flex_none(),
        LOG_STREAM => cell.w(px(176.)).flex_none(),
        name if ID_FIELDS.contains(&name) => cell.w(px(112.)).flex_none(),
        _ => cell.flex_1(),
    }
}

impl QueryView {
    pub(super) fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let content: AnyElement = match &self.state {
            RunState::Malformed { message } => self.render_fix_card(message, cx),
            RunState::SessionExpired => self.render_session_card(cx),
            _ if self.assist.proposal.is_some() => self.render_proposal_card(cx),
            RunState::Failed(message) => v_flex()
                .p_3()
                .child(error_callout(
                    t!("cloudwatch_query.results.failed"),
                    message.clone(),
                    cx,
                ))
                .into_any_element(),
            _ => match &self.results {
                Some(results) => self.render_result_tabs(results, cx),
                None if self.is_running() => EmptyState::new(
                    IconName::LoadCircle,
                    t!("cloudwatch_query.results.running_title"),
                )
                .spinning(true)
                .into_any_element(),
                None => EmptyState::new(
                    IconName::CloudPulse,
                    t!("cloudwatch_query.results.empty_title"),
                )
                .description(t!("cloudwatch_query.results.empty_description"))
                .into_any_element(),
            },
        };
        v_flex()
            .size_full()
            .child(self.render_results_header(cx))
            .child(div().flex_1().min_h_0().child(content))
            .child(self.render_footer(cx))
            .into_any_element()
    }

    fn render_results_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let proposal = self.assist.proposal.is_some()
            && !matches!(
                self.state,
                RunState::Malformed { .. } | RunState::SessionExpired
            );
        let title = if proposal {
            t!("cloudwatch_query.proposal.title")
        } else {
            t!("cloudwatch_query.results.title")
        };
        let (pill, detail): (Option<(SharedString, StatusTone)>, Option<SharedString>) =
            match &self.state {
                _ if proposal => (
                    Some((t!("cloudwatch_query.proposal.not_run"), StatusTone::Neutral)),
                    None,
                ),
                RunState::Malformed { .. } => (
                    Some((t!("cloudwatch_query.results.not_run"), StatusTone::Error)),
                    Some(t!("cloudwatch_query.results.rejected")),
                ),
                RunState::SessionExpired => (
                    Some((t!("cloudwatch_query.results.not_run"), StatusTone::Error)),
                    Some(t!("cloudwatch_query.results.reruns_after_login")),
                ),
                RunState::Running { started } => (
                    Some((t!("cloudwatch_query.results.running"), StatusTone::Info)),
                    Some(format_seconds(started.elapsed()).into()),
                ),
                RunState::Done {
                    elapsed,
                    finished,
                    bytes_scanned,
                    ..
                } => {
                    let results = self.results.as_ref();
                    let count = results.map_or(0, |results| results.rows.len());
                    let errors = results.map_or(0, ResultSet::errors);
                    (
                        Some((
                            t!("cloudwatch_query.results.events", count = count),
                            if errors > 0 && errors * 2 >= count {
                                StatusTone::Error
                            } else {
                                StatusTone::Info
                            },
                        )),
                        Some(t!(
                            "cloudwatch_query.results.stats",
                            seconds = format_seconds(*elapsed),
                            bytes = format_bytes(*bytes_scanned),
                            cost = format_cost(*bytes_scanned),
                            ago = ago_label(*finished)
                        )),
                    )
                }
                _ => (None, None),
            };
        let has_rows = self
            .results
            .as_ref()
            .is_some_and(|results| !results.rows.is_empty());
        let show_tabs = !proposal
            && !matches!(
                self.state,
                RunState::Malformed { .. } | RunState::SessionExpired
            );
        let tab =
            |id: &'static str, label: SharedString, tab: ResultTab, cx: &mut Context<Self>| {
                segment_option(id, label, self.tab == tab, true, cx)
                    .px_3()
                    .flex_none()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
                    .into_any_element()
            };
        h_flex()
            .h(px(44.))
            .px_3()
            .gap_3()
            .child(
                Label::new(title)
                    .size(LabelSize::Default)
                    .weight(FontWeight::BOLD),
            )
            .children(pill.map(|(label, tone)| status_pill(label, tone, cx)))
            .child(div().flex_1().min_w_0().children(detail.map(|detail| {
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate()
            })))
            .when(
                show_tabs && has_rows && self.results.as_ref().is_some_and(ResultSet::is_log),
                |this| {
                    this.child(
                        secondary_button(
                            "cloudwatch-summarize",
                            Some((IconName::Sparkle, Color::Accent)),
                            if self.assist.summarizing {
                                t!("cloudwatch_query.summary.working")
                            } else {
                                t!("cloudwatch_query.summary.button")
                            },
                            !self.assist.summarizing,
                            cx,
                        )
                        .h(px(24.))
                        .border_color(cx.theme().colors().border_focused)
                        .on_click(cx.listener(|this, _, _, cx| this.summarize(cx))),
                    )
                },
            )
            .when(show_tabs, |this| {
                this.child(segmented_control(cx).flex_none().children([
                    tab(
                        "cloudwatch-tab-events",
                        t!("cloudwatch_query.tabs.events"),
                        ResultTab::Events,
                        cx,
                    ),
                    tab(
                        "cloudwatch-tab-chart",
                        t!("cloudwatch_query.tabs.chart"),
                        ResultTab::Chart,
                        cx,
                    ),
                    tab(
                        "cloudwatch-tab-fields",
                        t!("cloudwatch_query.tabs.fields"),
                        ResultTab::Fields,
                        cx,
                    ),
                ]))
                .child(
                    IconButton::new("cloudwatch-copy-results", IconName::Copy)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .disabled(!has_rows)
                        .tooltip(Tooltip::text(t!("cloudwatch_query.results.copy_json")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(results) = &this.results {
                                cx.write_to_clipboard(ClipboardItem::new_string(results.to_json()));
                            }
                        })),
                )
                .child(
                    IconButton::new("cloudwatch-download-results", IconName::Download)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .disabled(!has_rows)
                        .tooltip(Tooltip::text(t!("cloudwatch_query.results.save_csv")))
                        .on_click(cx.listener(|this, _, _, cx| this.export(false, cx))),
                )
            })
            .into_any_element()
    }

    fn render_result_tabs(&self, results: &ResultSet, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.tab {
            ResultTab::Events => v_flex()
                .size_full()
                .gap_2()
                .px_3()
                .pb_2()
                .when(self.is_running(), |this| this.opacity(0.5))
                .map(|this| match &self.assist.summary {
                    Some(summary) => this.child(self.render_summary_card(summary, cx)),
                    None => this.children(self.render_histogram(results, px(48.), cx)),
                })
                .child(self.render_table(results, cx))
                .into_any_element(),
            ResultTab::Chart => v_flex()
                .size_full()
                .px_3()
                .pb_2()
                .child(match self.render_histogram(results, px(160.), cx) {
                    Some(histogram)
                        if !insights::has_stats(self.last_query.as_deref().unwrap_or_default()) =>
                    {
                        histogram
                    }
                    _ => render_stats_chart(results, cx),
                })
                .into_any_element(),
            ResultTab::Fields => render_fields(results, cx),
        };
        body
    }

    fn render_histogram(
        &self,
        results: &ResultSet,
        height: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.histogram.is_empty() {
            return None;
        }
        let max = self
            .histogram
            .iter()
            .map(|(_, count)| *count)
            .max()
            .unwrap_or(1)
            .max(1);
        let errors = results.errors() > 0 && results.errors() * 2 >= results.rows.len();
        let peak = peak(&self.histogram);
        let accent = cx.theme().colors().border_focused;
        let error = Color::Error.color(cx);
        let bin = self.histogram_bin as i64;
        let first = self.histogram.first().map(|(start, _)| *start)?;
        let last = self.histogram.last().map(|(start, _)| *start + bin)?;
        let middle = first + (last - first) / 2;
        let peak_label = peak.as_ref().map(|peak| {
            let start = self.histogram[peak.bins.start].0;
            let end = self.histogram[peak.bins.end - 1].0 + bin;
            t!(
                "cloudwatch_query.histogram.peak",
                from = clock(from_unix(start)),
                to = clock(from_unix(end)),
                count = peak.count,
                unit = if errors {
                    t!("cloudwatch_query.histogram.errors")
                } else {
                    t!("cloudwatch_query.histogram.events")
                }
            )
        });
        let axis = |time: i64| {
            Label::new(clock(from_unix(time)))
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Placeholder)
        };
        Some(
            v_flex()
                .flex_none()
                .px_3()
                .pt_2()
                .pb_1()
                .gap_1()
                .rounded(Radius::Md.rems())
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().editor_background)
                .child(
                    h_flex().h(height).items_end().gap(px(3.)).children(
                        self.histogram
                            .iter()
                            .enumerate()
                            .map(|(index, (start, count))| {
                                let start = *start;
                                let in_peak =
                                    peak.as_ref().is_some_and(|peak| peak.bins.contains(&index));
                                let fraction = *count as f32 / max as f32;
                                let color = if *count == 0 {
                                    accent.opacity(0.35)
                                } else if in_peak && errors {
                                    error.opacity(0.85)
                                } else {
                                    accent.opacity(0.85)
                                };
                                div()
                                    .id(("cloudwatch-bin", index))
                                    .flex_1()
                                    .h(px((f32::from(height) * fraction).max(1.)))
                                    .rounded_t(px(2.))
                                    .bg(color)
                                    .cursor_pointer()
                                    .tooltip(Tooltip::text(t!(
                                        "cloudwatch_query.histogram.bar",
                                        time = clock(from_unix(start)),
                                        count = count
                                    )))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_window(
                                            TimeWindow::Between {
                                                start,
                                                end: start + bin,
                                            },
                                            cx,
                                        );
                                        this.run(None, cx);
                                    }))
                            }),
                    ),
                )
                .child(
                    h_flex()
                        .child(axis(first))
                        .child(div().flex_1())
                        .child(axis(middle))
                        .child(div().flex_1())
                        .children(peak_label.map(|label| {
                            Label::new(label).size(LabelSize::XSmall).color(if errors {
                                Color::Error
                            } else {
                                Color::Muted
                            })
                        }))
                        .child(div().flex_1())
                        .child(axis(last)),
                )
                .into_any_element(),
        )
    }

    fn render_table(&self, results: &ResultSet, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let columns = results.table_columns();
        let log = results.is_log();
        v_flex()
            .flex_1()
            .min_h_0()
            .rounded(Radius::Md.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(30.))
                    .px_3()
                    .gap_3()
                    .flex_none()
                    .bg(colors.panel_background)
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .children(columns.iter().map(|name| {
                        column_cell(name, log).child(
                            Label::new(name.clone())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })),
            )
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, row: usize, window, cx| this.render_row(row, window, cx)),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn render_row(
        &mut self,
        row: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(results) = &self.results else {
            return gpui::Empty.into_any_element();
        };
        let colors = cx.theme().colors();
        let columns = results.table_columns();
        let log = results.is_log();
        let selected = self.selected == Some(row);
        let cells = columns.iter().map(|name| {
            let cell = column_cell(name, log);
            let text = if name == "level" {
                results.level(row).unwrap_or_default().to_string()
            } else {
                results.value(row, name).unwrap_or_default().to_string()
            };
            match name.as_str() {
                TIMESTAMP if log => cell.child(
                    Label::new(local_time(&text))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Muted),
                ),
                "level" if log => cell.child(level_badge(&text, cx)),
                LOG_STREAM => cell.child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                ),
                name if ID_FIELDS.contains(&name) => cell.child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Accent)
                        .truncate(),
                ),
                _ => cell.child(
                    Label::new(text.replace('\n', " "))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .truncate(),
                ),
            }
        });
        v_flex()
            .border_b_1()
            .border_color(colors.border_variant.opacity(0.5))
            .child(
                h_flex()
                    .id(("cloudwatch-row", row))
                    .h(px(ROW_HEIGHT - 1.))
                    .px_3()
                    .gap_3()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.element_selected))
                    .hover(|style| style.bg(colors.element_hover))
                    .children(cells)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let previous = this.selected;
                        this.selected = if previous == Some(row) {
                            None
                        } else {
                            Some(row)
                        };
                        for changed in [previous, Some(row)].into_iter().flatten() {
                            this.list_state.remeasure_items(changed..changed + 1);
                        }
                        cx.notify();
                    })),
            )
            .when(selected && log, |this| {
                this.child(self.render_event(row, cx))
            })
            .into_any_element()
    }

    fn render_event(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(results) = &self.results else {
            return gpui::Empty.into_any_element();
        };
        let colors = cx.theme().colors();
        let fields = results.detail_fields(row);
        let stack = results.stack(row);
        let links: Vec<Option<StackLink>> = stack
            .as_deref()
            .map(|stack| {
                stack
                    .lines()
                    .map(|frame| {
                        stack_link(frame).filter(|link| {
                            resolve_in_project(&self.project, &link.path, cx).is_some()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let first_link = links.iter().flatten().next().cloned();
        let id_field = results.id_field.clone();
        let id_value = id_field
            .as_deref()
            .and_then(|field| results.value(row, field))
            .map(str::to_string);
        let message = results.message(row).unwrap_or_default().to_string();
        let event_json = results.row_json(row);
        let log_group = results
            .value(row, "@log")
            .map(|log| {
                log.split_once(':')
                    .map_or(log, |(_, name)| name)
                    .to_string()
            })
            .or_else(|| self.file.groups.first().cloned());
        let event_fields = results.rows.get(row).cloned().unwrap_or_default();
        let label = |text: String| {
            Label::new(text)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Placeholder)
        };
        h_flex()
            .bg(colors.panel_background)
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .flex_none()
                    .bg(colors.border_focused),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .pl(px(170.))
                    .pr_3()
                    .py_3()
                    .gap_3()
                    .when(!fields.is_empty(), |this| {
                        this.child(h_flex().flex_wrap().gap_x_8().gap_y_2().children(
                            fields.into_iter().map(|(name, field_value)| {
                                v_flex().max_w(px(280.)).gap_0p5().child(label(name)).child(
                                    Label::new(field_value)
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .truncate(),
                                )
                            }),
                        ))
                    })
                    .when_some(stack, |this, stack| {
                        let workspace = self.workspace.clone();
                        let project = self.project.clone();
                        this.child(
                            v_flex()
                                .gap_0p5()
                                .child(label(t!("cloudwatch_query.event.stack_label").to_string()))
                                .children(stack.lines().take(14).enumerate().map(
                                    |(index, frame)| {
                                        let link = links.get(index).cloned().flatten();
                                        let workspace = workspace.clone();
                                        let project = project.clone();
                                        h_flex()
                                            .id(("cloudwatch-frame", index))
                                            .child(
                                                Label::new(frame.to_string())
                                                    .size(LabelSize::Small)
                                                    .buffer_font(cx)
                                                    .color(if index == 0 {
                                                        Color::Error
                                                    } else if link.is_some() {
                                                        Color::Accent
                                                    } else {
                                                        Color::Placeholder
                                                    }),
                                            )
                                            .when_some(link, move |this, link| {
                                                this.cursor_pointer()
                                                    .tooltip(Tooltip::text(t!(
                                                        "cloudwatch_query.event.open",
                                                        location = link_label(&link)
                                                    )))
                                                    .on_click(move |_, window, cx| {
                                                        open_stack_link(
                                                            workspace.clone(),
                                                            &project,
                                                            &link,
                                                            window,
                                                            cx,
                                                        )
                                                    })
                                            })
                                    },
                                )),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .when_some(first_link, |this, link| {
                                let workspace = self.workspace.clone();
                                let project = self.project.clone();
                                this.child(
                                    accent_button(
                                        "cloudwatch-event-open",
                                        IconName::FileCode,
                                        t!(
                                            "cloudwatch_query.event.open",
                                            location = link_label(&link)
                                        ),
                                        cx,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            open_stack_link(
                                                workspace.clone(),
                                                &project,
                                                &link,
                                                window,
                                                cx,
                                            )
                                        },
                                    ),
                                )
                            })
                            .child(
                                secondary_button(
                                    "cloudwatch-event-ask",
                                    Some((IconName::Sparkle, Color::Accent)),
                                    t!("cloudwatch_query.event.ask_agent"),
                                    true,
                                    cx,
                                )
                                .on_click({
                                    move |_, window, cx| {
                                        agent_bridge::send_to_agent(
                                            AgentContext::Event {
                                                log_group: log_group.clone(),
                                                message: message.clone(),
                                                fields: event_fields.clone(),
                                            },
                                            message.chars().take(60).collect::<String>().into(),
                                            Some(
                                                t!("cloudwatch_query.event.ask_prompt").to_string(),
                                            ),
                                            true,
                                            window,
                                            cx,
                                        )
                                    }
                                }),
                            )
                            .when_some(id_field.zip(id_value), |this, (field, id)| {
                                this.child(
                                    secondary_button(
                                        "cloudwatch-event-filter",
                                        Some((IconName::Filter, Color::Muted)),
                                        t!("cloudwatch_query.event.filter_by", field = field),
                                        true,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            let query = insights::insert_filter(
                                                &this.query_text(cx),
                                                &field,
                                                &id,
                                            );
                                            this.replace_text(&query, cx);
                                            this.run(None, cx);
                                        },
                                    )),
                                )
                            })
                            .child(
                                secondary_button(
                                    "cloudwatch-event-copy",
                                    Some((IconName::Copy, Color::Muted)),
                                    t!("cloudwatch_query.event.copy"),
                                    true,
                                    cx,
                                )
                                .on_click(move |_, _, cx| {
                                    if let Ok(json) = serde_json::to_string_pretty(&event_json) {
                                        cx.write_to_clipboard(ClipboardItem::new_string(json));
                                    }
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let hint: Option<SharedString> = match &self.state {
            RunState::Malformed { .. } => Some(t!("cloudwatch_query.fix.footer")),
            _ if self.assist.proposal.is_some() => Some(t!("cloudwatch_query.proposal.footer")),
            _ => None,
        };
        if let Some(hint) = hint {
            return h_flex()
                .h(px(30.))
                .px_3()
                .child(
                    Label::new(hint)
                        .size(LabelSize::Small)
                        .color(Color::Placeholder)
                        .truncate(),
                )
                .into_any_element();
        }
        let Some(results) = &self.results else {
            return gpui::Empty.into_any_element();
        };
        let total = results.rows.len();
        let visible = self.visible_rows.borrow().unwrap_or_else(|| {
            let height = f32::from(self.list_state.viewport_bounds().size.height);
            ((height / ROW_HEIGHT).floor() as usize).min(total)
        });
        let (from, to) = match &self.state {
            RunState::Done { from, to, .. } => (*from, *to),
            _ => self.file.window.range(SystemTime::now()),
        };
        let order = self.last_query.as_deref().and_then(|query| {
            let lower = query.to_ascii_lowercase();
            if lower.contains("sort @timestamp desc") {
                Some(t!("cloudwatch_query.footer.newest_first"))
            } else if lower.contains("sort @timestamp asc") || lower.contains("sort @timestamp\n") {
                Some(t!("cloudwatch_query.footer.oldest_first"))
            } else {
                None
            }
        });
        let mut summary = t!(
            "cloudwatch_query.footer.visible",
            visible = visible.min(total),
            total = total,
            from = clock(from),
            to = clock(to)
        )
        .to_string();
        if let Some(order) = order {
            summary.push_str(" · ");
            summary.push_str(&order);
        }
        let link = |id: &'static str, label: SharedString| {
            Button::new(id, label)
                .style(ButtonStyle::Transparent)
                .label_size(LabelSize::Small)
                .color(Color::Accent)
        };
        h_flex()
            .h(px(30.))
            .px_3()
            .gap_1()
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(summary)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                link(
                    "cloudwatch-export-json",
                    t!("cloudwatch_query.footer.export_json"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.export(true, cx))),
            )
            .child(Label::new("·").size(LabelSize::Small).color(Color::Accent))
            .child(
                link("cloudwatch-export-csv", t!("cloudwatch_query.footer.csv"))
                    .on_click(cx.listener(|this, _, _, cx| this.export(false, cx))),
            )
            .child(Label::new("·").size(LabelSize::Small).color(Color::Accent))
            .child(
                link(
                    "cloudwatch-open-console",
                    t!("cloudwatch_query.footer.console"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_in_console(cx))),
            )
            .into_any_element()
    }

    /// Saves the rows next to the query file, wherever the user picks.
    fn export(&self, json: bool, cx: &mut Context<Self>) {
        let Some(results) = &self.results else {
            return;
        };
        let contents = if json {
            results.to_json()
        } else {
            results.to_csv()
        };
        let directory = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let stem = self
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "resultados".to_string());
        let name = format!("{stem}.{}", if json { "json" } else { "csv" });
        let prompt = cx.prompt_for_new_path(&directory, Some(&name));
        let fs = self.project.read(cx).fs().clone();
        cx.spawn(async move |_, _| {
            let Some(path) = prompt.await?? else {
                return anyhow::Ok(());
            };
            fs.atomic_write(path, contents).await
        })
        .detach_and_log_err(cx);
    }
}

/// The boards' accent-tinted action (Abrir checkout.ts:142): the bordered button in the accent.
pub(super) fn accent_button(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let accent = cx.theme().colors().border_focused;
    secondary_button(id, Some((icon, Color::Default)), label, true, cx)
        .border_color(accent)
        .bg(accent.opacity(0.2))
        .hover(move |this| this.bg(accent.opacity(0.3)))
}

fn level_badge(level: &str, cx: &App) -> impl IntoElement {
    let color = level_color(level);
    div()
        .flex_none()
        .w(px(48.))
        .h(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(Radius::Xs.rems())
        .when(color != Color::Muted, |this| {
            this.bg(color.color(cx).opacity(0.14))
        })
        .child(
            Label::new(level.to_ascii_uppercase())
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(color),
        )
}

/// Bars for a `stats` result: the first numeric column by the first other one.
fn render_stats_chart(results: &ResultSet, cx: &App) -> AnyElement {
    let numeric = results.columns.iter().find(|column| {
        results
            .rows
            .iter()
            .all(|row| value(row, column).is_none_or(|value| value.parse::<f64>().is_ok()))
    });
    let label_column = results
        .columns
        .iter()
        .find(|column| Some(*column) != numeric);
    let (Some(numeric), Some(label_column)) = (numeric, label_column) else {
        return EmptyState::new(IconName::Sparkle, t!("cloudwatch_query.chart.empty"))
            .into_any_element();
    };
    let points: Vec<(String, f64)> = results
        .rows
        .iter()
        .take(40)
        .map(|row| {
            (
                value(row, label_column).unwrap_or_default().to_string(),
                value(row, numeric)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
            )
        })
        .collect();
    let max = points
        .iter()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
        .max(f64::EPSILON);
    let accent = cx.theme().colors().border_focused;
    v_flex()
        .id("cloudwatch-chart")
        .size_full()
        .overflow_y_scroll()
        .gap_1()
        .child(
            Label::new(format!("{numeric} · {label_column}"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .children(points.into_iter().map(|(label, amount)| {
            h_flex()
                .gap_2()
                .child(
                    div().w(px(220.)).flex_none().child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .child(
                    div().flex_1().child(
                        div()
                            .h(px(10.))
                            .rounded(px(2.))
                            .bg(accent.opacity(0.85))
                            .w(gpui::relative((amount / max) as f32)),
                    ),
                )
                .child(
                    div().w(px(80.)).flex_none().child(
                        Label::new(localized_decimal(format!("{amount}")))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    ),
                )
        }))
        .into_any_element()
}

fn render_fields(results: &ResultSet, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    v_flex()
        .id("cloudwatch-fields")
        .size_full()
        .px_3()
        .overflow_y_scroll()
        .children(results.columns.iter().map(|column| {
            let present = results
                .rows
                .iter()
                .filter(|row| value(row, column).is_some_and(|value| !value.is_empty()))
                .count();
            let example = results
                .rows
                .iter()
                .find_map(|row| value(row, column).filter(|value| !value.is_empty()))
                .unwrap_or_default()
                .replace('\n', " ");
            h_flex()
                .h(px(30.))
                .gap_3()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    div().w(px(200.)).flex_none().child(
                        Label::new(column.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .child(
                    div().w(px(140.)).flex_none().child(
                        Label::new(t!(
                            "cloudwatch_query.fields.present",
                            present = present,
                            total = results.rows.len()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(example)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(fields: &[(&str, &str)]) -> Vec<(String, String)> {
        fields
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn reads_log_rows_like_the_board() {
        let results = ResultSet::new(vec![row(&[
            ("@timestamp", "2026-11-17 13:41:47.902"),
            ("@logStream", "api/web/7f3c2a91"),
            ("level", "error"),
            ("msg", "Adquirente respondeu 502 ao capturar o pagamento"),
            ("requestId", "req_9HX1c2"),
            ("userId", "48213"),
            (
                "@message",
                r#"{"route":"POST /v1/checkout/pay","durationMs":10412,"err":{"stack":"Error: Adquirente respondeu 502\n    at capturePayment (src/modules/payments/checkout.ts:142:11)"}}"#,
            ),
        ])]);
        assert!(results.is_log());
        assert_eq!(
            results.table_columns(),
            ["@timestamp", "level", "msg", "@logStream", "requestId"]
        );
        assert_eq!(
            results
                .detail_fields(0)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["requestId", "userId", "route", "durationMs"]
        );
        let stack = results.stack(0).expect("stack from the JSON message");
        let link = stack.lines().find_map(stack_link).expect("link");
        assert_eq!(link_label(&link), "checkout.ts:142");
        assert_eq!(results.errors(), 1);
    }

    #[test]
    fn histogram_fills_empty_bins_and_finds_the_peak() {
        let from = parse_timestamp("2026-11-17 13:30:00.000").unwrap();
        let rows = vec![
            row(&[("bin(1m)", "2026-11-17 13:32:00.000"), ("events", "10")]),
            row(&[("bin(1m)", "2026-11-17 13:33:00.000"), ("events", "12")]),
            row(&[("bin(1m)", "2026-11-17 13:31:00.000"), ("events", "1")]),
        ];
        let bins = histogram(&rows, from, from + 5 * 60, 60);
        assert_eq!(
            bins.iter().map(|(_, count)| *count).collect::<Vec<_>>(),
            [0, 1, 10, 12, 0, 0]
        );
        let peak = peak(&bins).unwrap();
        assert_eq!((peak.bins, peak.count), (2..4, 22));
    }

    #[test]
    fn formats_like_the_board() {
        assert_eq!(format_bytes(412.0 * 1024.0 * 1024.0), "412 MB");
        assert_eq!(format_cost(412.0 * 1024.0 * 1024.0), "0.002");
        assert_eq!(format_seconds(Duration::from_millis(1800)), "1.8 s");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
    }
}
