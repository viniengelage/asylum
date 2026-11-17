use super::ThreadView;
use acp_thread::{ToolCall, ToolCallStatus};
use chrono::{DateTime, Local};
use gpui::{AnyElement, FontWeight};
use i18n::t;
use task_agents::{
    CLOUDWATCH_INSIGHTS_QUERY_TOOL, CLOUDWATCH_LIST_LOG_GROUPS_TOOL, CLOUDWATCH_RECENT_EVENTS_TOOL,
    CLOUDWATCH_WIDE_INSIGHTS_QUERY_TOOL, CloudWatchReadKind, CloudWatchReadResult,
};
use ui::prelude::*;

const LABEL_COLUMN_WIDTH: f32 = 78.;
const ROW_HEIGHT: f32 = 24.;

pub(super) fn is_cloudwatch_tool(tool_call: &ToolCall) -> bool {
    matches!(
        tool_call.tool_name.as_deref(),
        Some(
            CLOUDWATCH_INSIGHTS_QUERY_TOOL
                | CLOUDWATCH_WIDE_INSIGHTS_QUERY_TOOL
                | CLOUDWATCH_RECENT_EVENTS_TOOL
                | CLOUDWATCH_LIST_LOG_GROUPS_TOOL
        )
    )
}

fn read_result(tool_call: &ToolCall) -> Option<CloudWatchReadResult> {
    if !is_cloudwatch_tool(tool_call) || !matches!(tool_call.status(), ToolCallStatus::Completed) {
        return None;
    }
    serde_json::from_value(tool_call.raw_output.clone()?).ok()
}

/// The full card, for queries and events; the log group listing only gets its one-line header.
pub(super) fn cloudwatch_card(tool_call: &ToolCall) -> Option<CloudWatchReadResult> {
    read_result(tool_call).filter(|result| result.kind != CloudWatchReadKind::LogGroups)
}

/// The mono text after "Consultou o CloudWatch" in the tool call's one-line header:
/// `bin(1m) · 60 min · 820 MB` for a grouped query, `status >= 500 · 37 eventos · 412 MB` for a
/// filter.
pub(super) fn header_target(tool_call: &ToolCall) -> Option<SharedString> {
    if !is_cloudwatch_tool(tool_call) {
        return None;
    }
    if let Some(result) = read_result(tool_call) {
        let text = match result.kind {
            CloudWatchReadKind::Insights => {
                let mut parts = Vec::new();
                match &result.grouped_by {
                    Some(field) => {
                        parts.push(field.clone());
                        parts.push(duration_label(result.end_ms - result.start_ms));
                    }
                    None => {
                        parts.push(result.summary.clone());
                        parts.push(events_label(result.records_matched).to_string());
                    }
                }
                parts.push(bytes_label(result.bytes_scanned));
                parts
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
            CloudWatchReadKind::Events => [
                result.log_groups.join(", "),
                result.summary.clone(),
                events_label(result.records_matched).to_string(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
            CloudWatchReadKind::LogGroups => {
                t!("agent_ui.cloudwatch_card.log_groups", count = result.rows).to_string()
            }
        };
        return Some(text.into());
    }
    let input = tool_call.raw_input.as_ref()?;
    let mut parts = Vec::new();
    if let Some(groups) = input.get("log_groups").and_then(|groups| groups.as_array()) {
        let groups = groups
            .iter()
            .filter_map(|group| group.as_str())
            .collect::<Vec<_>>();
        if !groups.is_empty() {
            parts.push(groups.join(", "));
        }
    }
    for field in ["log_group", "filter_pattern", "prefix"] {
        if let Some(value) = input
            .get(field)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            parts.push(value.to_string());
        }
    }
    if let Some(minutes) = input
        .get("window_minutes")
        .and_then(|minutes| minutes.as_i64())
    {
        parts.push(duration_label(minutes * 60_000));
    }
    (!parts.is_empty()).then(|| parts.join(" · ").into())
}

fn events_label(count: u64) -> SharedString {
    if count == 1 {
        t!("agent_ui.cloudwatch_card.events.one")
    } else {
        t!("agent_ui.cloudwatch_card.events.other", count = count)
    }
}

fn duration_label(milliseconds: i64) -> String {
    let minutes = (milliseconds / 60_000).max(1);
    if minutes < 120 {
        t!("agent_ui.cloudwatch_card.duration.minutes", count = minutes).to_string()
    } else if minutes < 48 * 60 {
        t!(
            "agent_ui.cloudwatch_card.duration.hours",
            count = minutes / 60
        )
        .to_string()
    } else {
        t!(
            "agent_ui.cloudwatch_card.duration.days",
            count = minutes / (24 * 60)
        )
        .to_string()
    }
}

fn bytes_label(bytes: u64) -> String {
    const MEGABYTE: f64 = 1_000_000.;
    const GIGABYTE: f64 = 1_000_000_000.;
    let bytes = bytes as f64;
    let (value, unit) = if bytes >= GIGABYTE {
        (bytes / GIGABYTE, "GB")
    } else if bytes >= MEGABYTE {
        (bytes / MEGABYTE, "MB")
    } else {
        (bytes / 1000., "KB")
    };
    let number = if value >= 10. || value == value.round() {
        format!("{value:.0}")
    } else {
        format!("{value:.1}").replace('.', &t!("agent_ui.cloudwatch_card.decimal_separator"))
    };
    format!("{number} {unit}")
}

fn local_time(milliseconds: i64, with_date: bool) -> String {
    DateTime::from_timestamp_millis(milliseconds)
        .map(|time| {
            let time = time.with_timezone(&Local);
            if with_date {
                time.format("%d/%m %H:%M").to_string()
            } else {
                time.format("%H:%M").to_string()
            }
        })
        .unwrap_or_default()
}

fn window_label(result: &CloudWatchReadResult) -> String {
    let span = result.end_ms - result.start_ms;
    let with_date = span >= 24 * 60 * 60_000;
    format!(
        "{}–{} ({})",
        local_time(result.start_ms, with_date),
        local_time(result.end_ms, with_date),
        duration_label(span)
    )
}

fn is_message_field(field: &str) -> bool {
    matches!(
        field.trim_start_matches('@').to_ascii_lowercase().as_str(),
        "msg" | "message"
    )
}

fn result_label(result: &CloudWatchReadResult) -> String {
    let events = events_label(result.records_matched);
    let mut parts = Vec::new();
    if result.kind == CloudWatchReadKind::Insights
        && let Some(field) = &result.grouped_by
    {
        let rows = result.rows;
        parts.push(match (is_message_field(field), rows == 1) {
            (true, true) => t!("agent_ui.cloudwatch_card.messages.one"),
            (true, false) => t!("agent_ui.cloudwatch_card.messages.other", count = rows),
            (false, true) => t!("agent_ui.cloudwatch_card.rows.one"),
            (false, false) => t!("agent_ui.cloudwatch_card.rows.other", count = rows),
        });
    }
    parts.push(events);
    if result.kind == CloudWatchReadKind::Insights {
        parts.push(t!(
            "agent_ui.cloudwatch_card.scanned",
            bytes = bytes_label(result.bytes_scanned)
        ));
    }
    parts
        .iter()
        .map(|part| part.as_ref())
        .collect::<Vec<&str>>()
        .join(" · ")
}

impl ThreadView {
    /// "Consultou o CloudWatch": the groups, window, query and totals of a read, and the most
    /// frequent value when the query counted by one field.
    pub(super) fn render_cloudwatch_card(
        &self,
        result: CloudWatchReadResult,
        cx: &Context<Self>,
    ) -> AnyElement {
        let border = self.tool_card_border_color(cx);
        let row = |label: SharedString, value: String, strong: bool| {
            h_flex()
                .h(px(ROW_HEIGHT))
                .min_w_0()
                .child(
                    div().w(px(LABEL_COLUMN_WIDTH)).flex_none().child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Placeholder),
                    ),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(value)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(if strong { Color::Default } else { Color::Muted })
                            .single_line()
                            .truncate(),
                    ),
                )
        };
        let query_label = match result.kind {
            CloudWatchReadKind::Events => t!("agent_ui.cloudwatch_card.filter"),
            _ => t!("agent_ui.cloudwatch_card.query"),
        };
        let finding = result.finding.as_ref().map(|finding| {
            t!(
                "agent_ui.cloudwatch_card.finding",
                count = finding.count,
                total = finding.total,
                value = finding.value
            )
        });
        v_flex()
            .mt_1()
            .mb_2()
            .rounded(px(10.))
            .border_1()
            .border_color(border)
            .bg(cx.theme().colors().editor_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        Icon::new(IconName::CloudPulse)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(t!("agent_ui.cloudwatch_card.title"))
                                .size(LabelSize::Small)
                                .weight(FontWeight::BOLD)
                                .truncate(),
                        ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded(px(6.))
                            .bg(cx.theme().status().success_background)
                            .child(
                                Label::new(t!("agent_ui.cloudwatch_card.read_only"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Success),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_1p5()
                    .child(row(
                        t!("agent_ui.cloudwatch_card.groups"),
                        result.log_groups.join(", "),
                        false,
                    ))
                    .child(row(
                        t!("agent_ui.cloudwatch_card.window"),
                        window_label(&result),
                        false,
                    ))
                    .when(!result.summary.is_empty(), |this| {
                        this.child(row(query_label, result.summary.clone(), false))
                    })
                    .child(row(
                        t!("agent_ui.cloudwatch_card.result"),
                        result_label(&result),
                        true,
                    )),
            )
            .when_some(finding, |this, finding| {
                this.child(
                    div()
                        .px_3()
                        .pt_2p5()
                        .pb_3()
                        .border_t_1()
                        .border_color(border)
                        .child(Label::new(finding).size(LabelSize::Small)),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_scanned_bytes() {
        assert_eq!(bytes_label(412_000_000), "412 MB");
        assert_eq!(bytes_label(820_400_000), "820 MB");
        assert_eq!(bytes_label(3_000_000), "3 MB");
        assert_eq!(bytes_label(12_000), "12 KB");
        assert!(bytes_label(1_800_000_000).starts_with("1"));
        assert!(bytes_label(1_800_000_000).ends_with("8 GB"));
    }

    #[test]
    fn spells_out_the_result() {
        let result = CloudWatchReadResult {
            kind: CloudWatchReadKind::Insights,
            grouped_by: Some("msg".into()),
            rows: 4,
            records_matched: 37,
            bytes_scanned: 412_000_000,
            ..CloudWatchReadResult::default()
        };
        let label = result_label(&result);
        assert!(label.contains("37"));
        assert!(label.contains("412 MB"));
        assert_eq!(label.matches(" · ").count(), 2);
        assert_eq!(duration_label(30 * 60_000), duration_label(30 * 60_000 + 1));
    }
}
