use super::{ThreadView, ToolCallLayout};
use acp_thread::{ToolCall, ToolCallStatus};
use gpui::{AnyElement, ClipboardItem, relative};
use task_agents::{
    ELASTIC_ESQL_TOOL, ELASTIC_RECENT_ERRORS_TOOL, ELASTIC_TRACE_TOOL, ELASTIC_WIDE_ESQL_TOOL,
    ElasticConnectionInfo, ElasticQueryResult, ElasticTraceResult,
};
use ui::{Headline, HeadlineSize, prelude::*};
use util::ResultExt as _;

const MAX_TABLE_COLUMNS: usize = 5;
const MAX_TABLE_ROWS: usize = 10;
const MAX_LOG_LINES: usize = 8;
const MAX_ERROR_ROWS: usize = 8;
const MAX_RECORD_FIELDS: usize = 12;
const MAX_SERIES_POINTS: usize = 60;
const MAX_BAR_HEIGHT: f32 = 56.;
const MAX_TRACE_SPANS: usize = 12;

pub(super) enum ElasticCard {
    Query(ElasticQueryResult),
    Errors(ElasticQueryResult),
    Trace(ElasticTraceResult),
    NotConnected { message: String },
}

/// How a result reads best, decided by its shape so the model doesn't have to say.
enum Presentation {
    Empty,
    Number,
    Series { time: usize, value: usize },
    Logs,
    Record,
    Table,
}

pub(super) fn elastic_card(tool_call: &ToolCall) -> Option<ElasticCard> {
    let tool_name = tool_call.tool_name.as_deref()?;
    let raw_output = tool_call.raw_output.clone()?;
    match (tool_call.status(), tool_name) {
        (ToolCallStatus::Completed, ELASTIC_ESQL_TOOL | ELASTIC_WIDE_ESQL_TOOL) => {
            serde_json::from_value(raw_output)
                .ok()
                .map(ElasticCard::Query)
        }
        (ToolCallStatus::Completed, ELASTIC_RECENT_ERRORS_TOOL) => {
            serde_json::from_value(raw_output)
                .ok()
                .map(ElasticCard::Errors)
        }
        (ToolCallStatus::Completed, ELASTIC_TRACE_TOOL) => serde_json::from_value(raw_output)
            .ok()
            .map(ElasticCard::Trace),
        (
            ToolCallStatus::Failed,
            ELASTIC_ESQL_TOOL
            | ELASTIC_WIDE_ESQL_TOOL
            | ELASTIC_RECENT_ERRORS_TOOL
            | ELASTIC_TRACE_TOOL,
        ) => {
            let message = raw_output
                .as_array()?
                .iter()
                .find_map(|content| content.get("Text")?.as_str())?
                .to_owned();
            // Only the dock's own errors get a card, since the user has to act on them; a
            // query error is the model's to fix and it usually retries.
            message
                .contains("dock Elastic")
                .then_some(ElasticCard::NotConnected { message })
        }
        _ => None,
    }
}

fn is_numeric(kind: &str) -> bool {
    matches!(
        kind,
        "long"
            | "integer"
            | "double"
            | "unsigned_long"
            | "float"
            | "counter_long"
            | "counter_integer"
            | "counter_double"
    )
}

fn is_date(kind: &str) -> bool {
    matches!(kind, "date" | "date_nanos" | "datetime")
}

fn column(result: &ElasticQueryResult, name: &str) -> Option<usize> {
    result.columns.iter().position(|column| column.name == name)
}

fn presentation(result: &ElasticQueryResult) -> Presentation {
    if result.rows.is_empty() {
        return Presentation::Empty;
    }
    if column(result, "@timestamp").is_some() && column(result, "message").is_some() {
        return Presentation::Logs;
    }
    if result.columns.len() == 2 && (2..=MAX_SERIES_POINTS).contains(&result.rows.len()) {
        let time = result
            .columns
            .iter()
            .position(|column| is_date(&column.kind));
        let value = result
            .columns
            .iter()
            .position(|column| is_numeric(&column.kind));
        if let (Some(time), Some(value)) = (time, value)
            && time != value
        {
            return Presentation::Series { time, value };
        }
    }
    match (result.rows.len(), result.columns.len()) {
        (1, 1) => Presentation::Number,
        (1, _) => Presentation::Record,
        _ => Presentation::Table,
    }
}

/// `1284` → `1.284`, the way pt-BR writes it. Anything that isn't a plain integer stays as is.
fn format_number(value: &str) -> String {
    let (sign, digits) = match value.strip_prefix('-') {
        Some(digits) => ("-", digits),
        None => ("", value),
    };
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return value.to_owned();
    }
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(digit);
    }
    format!("{sign}{grouped}")
}

/// `2026-09-30T13:41:52.318Z` → `13:41:52` in local time.
fn short_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// `10412000` µs → `10,4 s`, `38000` → `38 ms`.
fn format_micros(micros: u64) -> String {
    if micros >= 1_000_000 {
        format!("{:.1} s", micros as f64 / 1_000_000.0).replace('.', ",")
    } else if micros >= 1_000 {
        format!("{} ms", micros / 1_000)
    } else {
        format!("{micros} µs")
    }
}

fn window_label(minutes: u32) -> String {
    match minutes {
        minutes if minutes >= 24 * 60 && minutes % (24 * 60) == 0 => {
            let days = minutes / (24 * 60);
            if days == 1 {
                "últimas 24 h".to_owned()
            } else {
                format!("últimos {days} dias")
            }
        }
        minutes if minutes >= 60 && minutes % 60 == 0 => format!("últimas {} h", minutes / 60),
        minutes => format!("últimos {minutes} min"),
    }
}

fn level_color(level: &str) -> Color {
    match level.to_ascii_lowercase().as_str() {
        "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emergency" => Color::Error,
        "warn" | "warning" => Color::Warning,
        _ => Color::Muted,
    }
}

fn display(value: &Option<String>) -> String {
    value
        .as_deref()
        .map(|value| value.replace(['\n', '\r'], " "))
        .unwrap_or_else(|| "null".to_owned())
}

fn cell<'a>(
    result: &'a ElasticQueryResult,
    row: &'a [Option<String>],
    name: &str,
) -> Option<&'a str> {
    row.get(column(result, name)?)?.as_deref()
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn to_csv(result: &ElasticQueryResult) -> String {
    let mut csv = result
        .columns
        .iter()
        .map(|column| csv_field(&column.name))
        .collect::<Vec<_>>()
        .join(",");
    for row in &result.rows {
        csv.push('\n');
        csv.push_str(
            &row.iter()
                .map(|value| csv_field(value.as_deref().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    csv
}

fn rows_label(count: usize, logs: bool) -> String {
    match (count, logs) {
        (1, true) => "1 doc".to_owned(),
        (1, false) => "1 linha".to_owned(),
        (count, true) => format!("{} docs", format_number(&count.to_string())),
        (count, false) => format!("{} linhas", format_number(&count.to_string())),
    }
}

fn badge(text: String, color: Color, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(color.color(cx).opacity(0.12))
        .child(Label::new(text).size(LabelSize::XSmall).color(color))
        .into_any_element()
}

impl ThreadView {
    pub(super) fn render_elastic_card(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        card: ElasticCard,
        layout: ToolCallLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let content = match card {
            ElasticCard::Query(result) => {
                let expanded = self.expanded_elastic_queries.contains(&tool_call.id);
                self.render_elastic_query_card(entry_ix, tool_call, result, expanded, cx)
            }
            ElasticCard::Errors(result) => self.render_elastic_errors_card(entry_ix, result, cx),
            ElasticCard::Trace(result) => self.render_elastic_trace_card(entry_ix, result, cx),
            ElasticCard::NotConnected { message } => {
                self.render_elastic_not_connected(entry_ix, message)
            }
        };
        v_flex()
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.ml_5().mr_5()
            })
            .mt_1()
            .mb_2()
            .rounded_md()
            .border_1()
            .border_color(self.tool_card_border_color(cx))
            .bg(cx.theme().colors().editor_background)
            .overflow_hidden()
            .children(content)
            .into_any_element()
    }

    fn render_elastic_header(
        &self,
        icon: IconName,
        icon_color: Color,
        title: String,
        header_badge: Option<(String, Color)>,
        connection: &ElasticConnectionInfo,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .gap_2()
            .px_2()
            .py_1p5()
            .bg(self.tool_card_header_bg(cx))
            .border_b_1()
            .border_color(self.tool_card_border_color(cx))
            .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .truncate(),
                ),
            )
            .when_some(header_badge, |this, (text, color)| {
                this.child(badge(text, color, cx))
            })
            .child(
                Label::new(format!("{} · {}", connection.name, connection.environment))
                    .size(LabelSize::XSmall)
                    .color(if connection.is_production() {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
            )
            .into_any_element()
    }

    fn render_elastic_query_card(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        result: ElasticQueryResult,
        expanded: bool,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let presentation = presentation(&result);
        let source = result
            .source
            .clone()
            .unwrap_or_else(|| "consulta".to_owned());
        let logs = matches!(presentation, Presentation::Logs);
        let (icon, icon_color, title) = match presentation {
            Presentation::Empty => (
                IconName::XCircle,
                Color::Muted,
                format!("{source} · nada na janela"),
            ),
            Presentation::Number => (IconName::Hash, Color::Accent, source),
            _ => (
                IconName::CloudPulse,
                Color::Accent,
                format!("{source} · {}", rows_label(result.total_rows, logs)),
            ),
        };
        let header = self.render_elastic_header(
            icon,
            icon_color,
            title,
            Some((window_label(result.window_minutes), Color::Muted)),
            &result.connection,
            cx,
        );
        let body = match presentation {
            Presentation::Empty => div()
                .px_2()
                .py_1p5()
                .child(
                    Label::new(format!(
                        "Nenhum documento {}.",
                        window_label(result.window_minutes).replace("últim", "nos últim")
                    ))
                    .size(LabelSize::Small),
                )
                .into_any_element(),
            Presentation::Number => render_number(&result),
            Presentation::Series { time, value } => render_series(&result, time, value, cx),
            Presentation::Logs => self.render_log_lines(&result, cx),
            Presentation::Record => self.render_elastic_record(&result, cx),
            Presentation::Table => self.render_elastic_table(&result, cx),
        };
        let tool_call_id = tool_call.id.clone();
        let query = result.query.clone();
        let window_minutes = result.window_minutes;
        let follow_source = result.source.clone();
        let copy_text = to_csv(&result);
        let has_rows = !result.rows.is_empty();
        let footer = h_flex()
            .w_full()
            .gap_1()
            .px_1()
            .py_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Button::new(
                    ("elastic-card-query", entry_ix),
                    if expanded {
                        "Esconder ES|QL"
                    } else {
                        "Ver ES|QL"
                    },
                )
                .label_size(LabelSize::Small)
                .color(Color::Muted)
                .start_icon(
                    Icon::new(IconName::Code)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if !this.expanded_elastic_queries.remove(&tool_call_id) {
                        this.expanded_elastic_queries.insert(tool_call_id.clone());
                    }
                    cx.notify();
                })),
            )
            .child(
                Button::new(("elastic-card-open", entry_ix), "Abrir no Elastic")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_elastic_query(query.clone(), window_minutes, window, cx);
                    })),
            )
            .when_some(follow_source.filter(|_| logs), |this, source| {
                this.child(
                    Button::new(("elastic-card-follow", entry_ix), "Seguir")
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .start_icon(
                            Icon::new(IconName::ListTree)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let Some(project) = this.project.upgrade() else {
                                return;
                            };
                            if let Err(error) =
                                task_agents::follow_logs(&project, source.clone(), window, cx)
                            {
                                this.show_elastic_error(error, cx);
                            }
                        })),
                )
            })
            .when(has_rows, |this| {
                this.child(
                    Button::new(("elastic-card-copy", entry_ix), "Copiar CSV")
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .start_icon(
                            Icon::new(IconName::Copy)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(move |_, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                        }),
                )
            })
            .child(div().flex_1())
            .when_some(result.took_ms, |this, took| {
                this.child(
                    Label::new(format!("took {took} ms"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
            })
            .into_any_element();
        let mut elements = vec![header, body];
        if expanded {
            elements.push(self.render_elastic_query_block(&result.query, cx));
        }
        elements.push(footer);
        elements
    }

    fn render_elastic_query_block(&self, query: &str, cx: &Context<Self>) -> AnyElement {
        div()
            .w_full()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .bg(cx.theme().colors().element_background)
            .font_buffer(cx)
            .text_ui_xs(cx)
            .text_color(cx.theme().colors().text_muted)
            .child(SharedString::from(query.trim().to_owned()))
            .into_any_element()
    }

    fn render_log_lines(&self, result: &ElasticQueryResult, cx: &Context<Self>) -> AnyElement {
        let border = self.tool_card_border_color(cx);
        let hidden = result.total_rows.saturating_sub(MAX_LOG_LINES);
        v_flex()
            .w_full()
            .children(result.rows.iter().take(MAX_LOG_LINES).map(|row| {
                let level = cell(result, row, "log.level")
                    .unwrap_or_default()
                    .to_owned();
                let color = level_color(&level);
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .when(color == Color::Error, |this| {
                        this.bg(Color::Error.color(cx).opacity(0.05))
                    })
                    .child(
                        div().w(rems(4.)).flex_none().child(
                            Label::new(short_time(
                                cell(result, row, "@timestamp").unwrap_or_default(),
                            ))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                        ),
                    )
                    .when(!level.is_empty(), |this| {
                        this.child(badge(level.to_ascii_uppercase(), color, cx))
                    })
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(
                                cell(result, row, "message")
                                    .unwrap_or_default()
                                    .replace(['\n', '\r'], " "),
                            )
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                        ),
                    )
                    .when_some(cell(result, row, "service.name"), |this, service| {
                        this.child(
                            Label::new(service.to_owned())
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
            }))
            .when(hidden > 0, |this| {
                this.child(
                    div().px_2().py_1().bg(self.tool_card_header_bg(cx)).child(
                        Label::new(format!("+ {} no resultado", rows_label(hidden, true)))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_elastic_record(&self, result: &ElasticQueryResult, cx: &Context<Self>) -> AnyElement {
        let Some(row) = result.rows.first() else {
            return div().into_any_element();
        };
        let hidden = result.columns.len().saturating_sub(MAX_RECORD_FIELDS);
        v_flex()
            .w_full()
            .py_1()
            .children(result.columns.iter().zip(row).take(MAX_RECORD_FIELDS).map(
                |(column, value)| {
                    h_flex()
                        .w_full()
                        .gap_2()
                        .px_2()
                        .py_0p5()
                        .child(
                            div().w(rems(10.)).flex_none().child(
                                Label::new(column.name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(display(value))
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                        )
                },
            ))
            .when(hidden > 0, |this| {
                this.child(
                    div().px_2().py_0p5().child(
                        Label::new(format!("+ {hidden} colunas"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_elastic_table(&self, result: &ElasticQueryResult, cx: &Context<Self>) -> AnyElement {
        let columns = result
            .columns
            .iter()
            .take(MAX_TABLE_COLUMNS)
            .collect::<Vec<_>>();
        let rows = result.rows.iter().take(MAX_TABLE_ROWS).collect::<Vec<_>>();
        let weights = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                rows.iter()
                    .map(|row| display(row.get(index).unwrap_or(&None)).chars().count())
                    .chain([column.name.chars().count()])
                    .max()
                    .unwrap_or(1)
                    .clamp(4, 36) as f32
            })
            .collect::<Vec<_>>();
        let total_weight = weights.iter().sum::<f32>().max(1.);
        let border = self.tool_card_border_color(cx);
        let cell = |index: usize, content: AnyElement| {
            let numeric = columns
                .get(index)
                .is_some_and(|column| is_numeric(&column.kind));
            h_flex()
                .min_w_0()
                .w(relative(
                    weights.get(index).copied().unwrap_or(1.) / total_weight,
                ))
                .when(numeric, |this| this.justify_end())
                .child(content)
        };
        let hidden_rows = result.total_rows.saturating_sub(rows.len());
        let hidden_columns = result.columns.len().saturating_sub(MAX_TABLE_COLUMNS);
        let mut notes = Vec::new();
        if hidden_rows > 0 {
            notes.push(format!("+ {} no resultado", rows_label(hidden_rows, false)));
        }
        if hidden_columns > 0 {
            notes.push(format!("+ {hidden_columns} colunas"));
        }
        v_flex()
            .w_full()
            .child(h_flex().w_full().gap_2().px_2().py_1().children(
                columns.iter().enumerate().map(|(index, column)| {
                    cell(
                        index,
                        Label::new(column.name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                            .into_any_element(),
                    )
                }),
            ))
            .children(rows.iter().map(|row| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(border)
                    .children(columns.iter().enumerate().map(|(index, column)| {
                        let value = row.get(index).unwrap_or(&None);
                        let text = display(value);
                        let text = if is_numeric(&column.kind) {
                            format_number(&text)
                        } else if is_date(&column.kind) {
                            short_time(&text)
                        } else {
                            text
                        };
                        cell(
                            index,
                            Label::new(text)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(if value.is_none() {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .truncate()
                                .into_any_element(),
                        )
                    }))
            }))
            .when(!notes.is_empty(), |this| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .border_t_1()
                        .border_color(border)
                        .bg(self.tool_card_header_bg(cx))
                        .child(
                            Label::new(notes.join(" · "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_elastic_errors_card(
        &self,
        entry_ix: usize,
        result: ElasticQueryResult,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let text_column = column(&result, "text").or_else(|| column(&result, "message"));
        let count_column = column(&result, "vezes");
        let last_column = column(&result, "ultima");
        let service_column = column(&result, "service.name");
        let total = result
            .rows
            .iter()
            .filter_map(|row| row.get(count_column?)?.as_deref()?.parse::<u64>().ok())
            .sum::<u64>();
        let header = self.render_elastic_header(
            if result.rows.is_empty() {
                IconName::Check
            } else {
                IconName::Warning
            },
            if result.rows.is_empty() {
                Color::Success
            } else {
                Color::Error
            },
            if result.rows.is_empty() {
                "Nenhum erro".to_owned()
            } else {
                format!(
                    "{} erros em {} mensagens",
                    format_number(&total.to_string()),
                    result.rows.len()
                )
            },
            Some((window_label(result.window_minutes), Color::Muted)),
            &result.connection,
            cx,
        );
        let border = self.tool_card_border_color(cx);
        let value = |row: &[Option<String>], index: Option<usize>| -> Option<String> {
            row.get(index?)?.clone()
        };
        let body = v_flex()
            .w_full()
            .children(result.rows.iter().take(MAX_ERROR_ROWS).map(|row| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .child(div().w(rems(3.5)).flex_none().child(badge(
                        format!(
                            "{}×",
                            format_number(&value(row, count_column).unwrap_or_default())
                        ),
                        Color::Error,
                        cx,
                    )))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(
                                value(row, text_column)
                                    .unwrap_or_else(|| "(sem mensagem)".to_owned())
                                    .replace(['\n', '\r'], " "),
                            )
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                        ),
                    )
                    .when_some(value(row, service_column), |this, service| {
                        this.child(
                            Label::new(service)
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .when_some(value(row, last_column), |this, last| {
                        this.child(
                            Label::new(format!("última {}", short_time(&last)))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
            }))
            .into_any_element();
        let query = result.query.clone();
        let window_minutes = result.window_minutes;
        let copy_text = to_csv(&result);
        let footer = h_flex()
            .w_full()
            .gap_1()
            .px_1()
            .py_1()
            .child(
                Button::new(("elastic-errors-open", entry_ix), "Abrir no Elastic")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_elastic_query(query.clone(), window_minutes, window, cx);
                    })),
            )
            .child(
                Button::new(("elastic-errors-copy", entry_ix), "Copiar CSV")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::Copy)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(move |_, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    }),
            )
            .into_any_element();
        vec![header, body, footer]
    }

    fn render_elastic_trace_card(
        &self,
        entry_ix: usize,
        result: ElasticTraceResult,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let failed = result.spans.iter().any(|span| span.failed) || !result.errors.is_empty();
        let root = result.spans.first();
        let short_id = result
            .trace_id
            .get(..8)
            .unwrap_or(&result.trace_id)
            .to_owned();
        let title = format!(
            "trace {short_id} · {} · {}",
            root.map(|root| root.name.clone()).unwrap_or_default(),
            format_micros(result.duration_us)
        );
        let header = self.render_elastic_header(
            IconName::ListTree,
            if failed { Color::Error } else { Color::Accent },
            title,
            failed.then(|| ("falhou".to_owned(), Color::Error)),
            &result.connection,
            cx,
        );
        let total = result.duration_us.max(1) as f32;
        let accent = cx.theme().colors().text_accent;
        let error = Color::Error.color(cx);
        let spans = v_flex()
            .w_full()
            .py_1()
            .children(result.spans.iter().take(MAX_TRACE_SPANS).map(|span| {
                let offset = span.offset_us as f32 / total;
                let width = (span.duration_us as f32 / total).clamp(0.01, 1.0 - offset.min(0.99));
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .child(
                        div()
                            .w(relative(0.4))
                            .flex_none()
                            .pl(rems(span.depth as f32 * 0.6))
                            .child(
                                Label::new(span.name.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(if span.failed {
                                        Color::Error
                                    } else {
                                        Color::Default
                                    })
                                    .truncate(),
                            ),
                    )
                    .child(
                        div().flex_1().h(px(8.)).relative().child(
                            div()
                                .absolute()
                                .top_0()
                                .left(relative(offset))
                                .w(relative(width))
                                .h(px(8.))
                                .rounded_sm()
                                .bg(if span.failed {
                                    error.opacity(0.85)
                                } else if span.event == "transaction" {
                                    accent.opacity(0.35)
                                } else {
                                    accent.opacity(0.75)
                                }),
                        ),
                    )
                    .child(
                        div().w(rems(3.5)).flex_none().child(
                            Label::new(format_micros(span.duration_us))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(if span.failed {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                        ),
                    )
            }))
            .when(result.total_spans > MAX_TRACE_SPANS, |this| {
                this.child(
                    div().px_2().child(
                        Label::new(format!("+ {} spans", result.total_spans - MAX_TRACE_SPANS))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .into_any_element();
        let errors = (!result.errors.is_empty()).then(|| {
            v_flex()
                .w_full()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(self.tool_card_border_color(cx))
                .children(result.errors.iter().take(3).map(|message| {
                    Label::new(format!("Error: {message}"))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Error)
                        .truncate()
                }))
                .into_any_element()
        });
        let trace_id = result.trace_id.clone();
        let logs_query = format!(
            "FROM logs-*\n| WHERE trace.id == \"{}\"\n| SORT @timestamp\n| LIMIT 500",
            result.trace_id.replace('"', "\\\"")
        );
        let footer = h_flex()
            .w_full()
            .gap_1()
            .px_1()
            .py_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Button::new(("elastic-trace-open", entry_ix), "Abrir trace")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let Some(project) = this.project.upgrade() else {
                            return;
                        };
                        if let Err(error) = task_agents::open_log_mention(
                            &project,
                            &format!("trace:{trace_id}"),
                            window,
                            cx,
                        ) {
                            this.show_elastic_error(error, cx);
                        }
                    })),
            )
            .when(result.log_count > 0, |this| {
                this.child(
                    Button::new(("elastic-trace-logs", entry_ix), "Logs do trace")
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .start_icon(
                            Icon::new(IconName::Filter)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_elastic_query(logs_query.clone(), 7 * 24 * 60, window, cx);
                        })),
                )
            })
            .child(div().flex_1())
            .child(
                Label::new(format!(
                    "{} spans · {} logs",
                    result.total_spans, result.log_count
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .into_any_element();
        let mut elements = vec![header, spans];
        elements.extend(errors);
        elements.push(footer);
        elements
    }

    fn render_elastic_not_connected(&self, entry_ix: usize, message: String) -> Vec<AnyElement> {
        vec![
            h_flex()
                .w_full()
                .gap_2()
                .px_2()
                .py_1p5()
                .child(
                    Icon::new(IconName::Power)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new("Sem Elastic para consultar").size(LabelSize::Small))
                        .child(
                            Label::new(message)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    Button::new(("elastic-card-panel", entry_ix), "Abrir Elastic")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::CloudPulse)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .on_click(|_, window, cx| {
                            if let Some(action) =
                                cx.build_action("elastic::ToggleFocus", None).log_err()
                            {
                                window.dispatch_action(action, cx);
                            }
                        }),
                )
                .into_any_element(),
        ]
    }

    fn open_elastic_query(
        &mut self,
        query: String,
        window_minutes: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        if let Err(error) = task_agents::open_log_query(&project, query, window_minutes, window, cx)
        {
            self.show_elastic_error(error, cx);
        }
    }

    fn show_elastic_error(&self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| workspace.show_error(error, cx))
            .log_err();
    }
}

fn render_number(result: &ElasticQueryResult) -> AnyElement {
    let column = result.columns.first();
    let value = result
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|value| format_number(&display(value)))
        .unwrap_or_else(|| "null".to_owned());
    v_flex()
        .w_full()
        .px_3()
        .py_2()
        .when_some(column, |this, column| {
            this.child(
                Label::new(column.name.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .child(Headline::new(value).size(HeadlineSize::Large))
        .into_any_element()
}

fn render_series(result: &ElasticQueryResult, time: usize, value: usize, cx: &App) -> AnyElement {
    let mut points = result
        .rows
        .iter()
        .map(|row| {
            let label = row
                .get(time)
                .and_then(|value| value.as_deref())
                .map(short_time)
                .map(|label| label.get(..5).map(str::to_owned).unwrap_or(label))
                .unwrap_or_default();
            let raw = row
                .get(value)
                .and_then(|value| value.clone())
                .unwrap_or_default();
            let number = raw.parse::<f64>().unwrap_or(0.).max(0.);
            (label, raw, number)
        })
        .collect::<Vec<_>>();
    // Buckets come in the order the query sorted them; time reads left to right.
    points.sort_by(|a, b| a.0.cmp(&b.0));
    let max = points.iter().map(|(_, _, value)| *value).fold(0., f64::max);
    let accent = cx.theme().colors().text_accent;
    let show_labels = points.len() <= 16;
    h_flex()
        .w_full()
        .items_end()
        .gap_px()
        .px_3()
        .pt_3()
        .pb_2()
        .children(points.into_iter().map(|(label, raw, number)| {
            let height = if max > 0. {
                (number / max * f64::from(MAX_BAR_HEIGHT)) as f32
            } else {
                0.
            };
            let is_peak = max > 0. && number == max;
            v_flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_0p5()
                .when(show_labels || is_peak, |this| {
                    this.child(
                        Label::new(format_number(&raw))
                            .size(LabelSize::XSmall)
                            .color(if is_peak { Color::Accent } else { Color::Muted })
                            .buffer_font(cx)
                            .truncate(),
                    )
                })
                .child(
                    div()
                        .w_full()
                        .h(px(height.max(2.)))
                        .rounded_sm()
                        .bg(accent.opacity(if is_peak { 0.9 } else { 0.35 })),
                )
                .when(show_labels, |this| {
                    this.child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                })
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_agents::ElasticColumn;

    fn result(columns: &[(&str, &str)], rows: Vec<Vec<Option<&str>>>) -> ElasticQueryResult {
        ElasticQueryResult {
            columns: columns
                .iter()
                .map(|(name, kind)| ElasticColumn {
                    name: (*name).to_owned(),
                    kind: (*kind).to_owned(),
                })
                .collect(),
            total_rows: rows.len(),
            rows: rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| value.map(str::to_owned))
                        .collect()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn picks_the_presentation_from_the_shape() {
        assert!(matches!(
            presentation(&result(&[("c", "long")], vec![])),
            Presentation::Empty
        ));
        assert!(matches!(
            presentation(&result(&[("c", "long")], vec![vec![Some("37")]])),
            Presentation::Number
        ));
        let series = result(
            &[("erros", "long"), ("bucket", "date")],
            vec![
                vec![Some("3"), Some("2026-09-30T13:00:00.000Z")],
                vec![Some("9"), Some("2026-09-30T13:01:00.000Z")],
            ],
        );
        assert!(matches!(
            presentation(&series),
            Presentation::Series { time: 1, value: 0 }
        ));
        let logs = result(
            &[("@timestamp", "date"), ("message", "text")],
            vec![vec![Some("2026-09-30T13:00:00.000Z"), Some("boom")]],
        );
        assert!(matches!(presentation(&logs), Presentation::Logs));
        let table = result(
            &[("service.name", "keyword"), ("c", "long")],
            vec![vec![Some("a"), Some("1")], vec![Some("b"), Some("2")]],
        );
        assert!(matches!(presentation(&table), Presentation::Table));
    }

    #[test]
    fn labels_read_like_the_dock() {
        assert_eq!(window_label(60), "últimas 1 h");
        assert_eq!(window_label(24 * 60), "últimas 24 h");
        assert_eq!(window_label(30), "últimos 30 min");
        assert_eq!(format_micros(10_412_000), "10,4 s");
        assert_eq!(format_number("67083474"), "67.083.474");
    }
}
