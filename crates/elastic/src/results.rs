//! What the views share about ES|QL results: reading documents out of columns and values, how a
//! value and a log level look, and turning stack frames into links to project files.

use std::{path::Path, sync::LazyLock};

use editor::Editor;
use gpui::{App, Entity, WeakEntity, Window};
use project::Project;
use regex::Regex;
use serde_json::Value;
use ui::{Color, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::client::{EsqlColumn, EsqlResult};

pub const TIMESTAMP: &str = "@timestamp";
pub const MESSAGE: &str = "message";
pub const LEVEL: &str = "log.level";
pub const SERVICE: &str = "service.name";
pub const TRACE_ID: &str = "trace.id";

/// The columns a log line shows, in order, when the result has them.
pub const LOG_COLUMNS: [&str; 5] = [TIMESTAMP, LEVEL, MESSAGE, SERVICE, TRACE_ID];

/// Fields that hold a stack trace, as ECS and the APM agents name them.
pub const STACK_FIELDS: [&str; 4] = [
    "error.stack_trace",
    "error.stack",
    "error.exception.stacktrace",
    "err.stack",
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    pub columns: Vec<EsqlColumn>,
    pub values: Vec<Vec<Value>>,
}

impl From<EsqlResult> for Rows {
    fn from(result: EsqlResult) -> Self {
        Self {
            columns: result.columns,
            values: result.values,
        }
    }
}

impl Rows {
    pub fn index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub fn value<'a>(&'a self, row: usize, name: &str) -> Option<&'a Value> {
        let index = self.index(name)?;
        self.values
            .get(row)?
            .get(index)
            .filter(|value| !value.is_null())
    }

    pub fn text(&self, row: usize, name: &str) -> Option<String> {
        self.value(row, name).map(display)
    }

    /// Log lines, when the result is documents with a message; otherwise a plain table.
    pub fn is_log(&self) -> bool {
        self.index(TIMESTAMP).is_some() && self.index(MESSAGE).is_some()
    }

    /// The fields of one row that have a value, for the document detail.
    pub fn fields(&self, row: usize) -> Vec<(String, String)> {
        let Some(values) = self.values.get(row) else {
            return Vec::new();
        };
        self.columns
            .iter()
            .zip(values)
            .filter(|(_, value)| !value.is_null())
            .map(|(column, value)| (column.name.clone(), display(value)))
            .collect()
    }

    pub fn stack_trace(&self, row: usize) -> Option<String> {
        STACK_FIELDS.iter().find_map(|field| self.text(row, field))
    }
}

/// One line of text for a cell; arrays (multi-valued fields) join with a comma.
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(values) => values.iter().map(display).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// `2026-09-30T13:41:52.318Z` → `13:41:52.318` in local time.
pub fn short_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

pub fn level_color(level: &str) -> Color {
    match level.to_ascii_lowercase().as_str() {
        "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emergency" => Color::Error,
        "warn" | "warning" => Color::Warning,
        "debug" | "trace" => Color::Muted,
        _ => Color::Default,
    }
}

pub fn level_chip(level: &str, cx: &App) -> impl IntoElement {
    let color = level_color(level);
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .when(color != Color::Default, |this| {
            this.bg(color.color(cx).opacity(0.14))
        })
        .child(
            Label::new(level.to_ascii_uppercase())
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(if color == Color::Default {
                    Color::Muted
                } else {
                    color
                }),
        )
}

/// A `file:line[:column]` inside a stack frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackLink {
    pub path: String,
    /// 1-based.
    pub line: u32,
    pub column: Option<u32>,
    /// Where the link sits in the frame's text.
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
    // Frames inside dependencies and the runtime don't point at the project.
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

/// The project file a stack frame path names. Containers often prefix it (`/app/src/…`), so
/// leading components are dropped until a file matches.
pub fn resolve_in_project(
    project: &Entity<Project>,
    path: &str,
    cx: &App,
) -> Option<project::ProjectPath> {
    let trimmed = path.trim_start_matches('/');
    let mut candidate = trimmed;
    loop {
        if let Some(found) = project.read(cx).find_project_path(Path::new(candidate), cx) {
            return Some(found);
        }
        let (_, rest) = candidate.split_once('/')?;
        candidate = rest;
    }
}

pub fn open_stack_link(
    workspace: WeakEntity<Workspace>,
    project: &Entity<Project>,
    link: &StackLink,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(project_path) = resolve_in_project(project, &link.path, cx) else {
        return false;
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
        return false;
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
    true
}

/// `48213` → `48.213`, `1234567` → `1,2 M`.
pub fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1} M", count as f64 / 1_000_000.0).replace('.', ",")
    } else {
        let digits = count.to_string();
        let mut result = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                result.push('.');
            }
            result.push(digit);
        }
        result
    }
}

/// `10412345` µs → `10,4 s`, `38000` → `38 ms`.
pub fn format_micros(micros: u64) -> String {
    if micros >= 1_000_000 {
        format!("{:.1} s", micros as f64 / 1_000_000.0).replace('.', ",")
    } else if micros >= 1_000 {
        format!("{} ms", micros / 1_000)
    } else {
        format!("{micros} µs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_links_point_at_project_frames() {
        let link = stack_link("    at capturePayment (src/modules/payments/checkout.ts:142:11)")
            .expect("link");
        assert_eq!(link.path, "src/modules/payments/checkout.ts");
        assert_eq!((link.line, link.column), (142, Some(11)));
        assert!(stack_link("    at async node:internal/process/task_queues:95:5").is_none());
        assert!(
            stack_link("at Object.<anonymous> (/app/node_modules/express/lib/router.js:10:3)")
                .is_none()
        );
        let link = stack_link("File \"/srv/app/billing.py\", line 3").is_none();
        assert!(link);
    }

    #[test]
    fn values_read_like_the_dock() {
        assert_eq!(format_count(48213), "48.213");
        assert_eq!(format_count(1_234_567), "1,2 M");
        assert_eq!(format_micros(10_412_000), "10,4 s");
        assert_eq!(format_micros(38_000), "38 ms");
        assert_eq!(display(&serde_json::json!(["a", "b"])), "a, b");
    }
}
