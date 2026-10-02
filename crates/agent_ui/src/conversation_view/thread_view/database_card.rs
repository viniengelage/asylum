use super::{ThreadView, ToolCallLayout};
use acp_thread::{ToolCall, ToolCallStatus};
use gpui::{AnyElement, ClipboardItem, Empty, relative};
use task_agents::{
    DATABASE_QUERY_TOOL, DATABASE_WRITE_QUERY_TOOL, DatabaseConnectionInfo, DatabaseQueryResult,
    DatabaseWriteResult,
};
use ui::{Headline, HeadlineSize, prelude::*};
use util::ResultExt as _;

const MAX_TABLE_COLUMNS: usize = 5;
const MAX_TABLE_ROWS: usize = 10;
const MAX_RECORD_FIELDS: usize = 12;
const MAX_SERIES_POINTS: usize = 31;
const MAX_BAR_HEIGHT: f32 = 56.;

pub(super) enum DatabaseCard {
    Query(DatabaseQueryResult),
    Write {
        result: DatabaseWriteResult,
        sql: String,
    },
    NotConnected {
        message: String,
    },
}

/// How a result reads best, decided by its shape so the model doesn't have to say.
enum Presentation {
    Empty,
    Number,
    Record,
    Series,
    Table,
}

pub(super) fn database_card(tool_call: &ToolCall) -> Option<DatabaseCard> {
    let tool_name = tool_call.tool_name.as_deref()?;
    let raw_output = tool_call.raw_output.clone()?;
    match (tool_call.status(), tool_name) {
        (ToolCallStatus::Completed, DATABASE_QUERY_TOOL) => serde_json::from_value(raw_output)
            .ok()
            .map(DatabaseCard::Query),
        (ToolCallStatus::Completed, DATABASE_WRITE_QUERY_TOOL) => {
            let result = serde_json::from_value(raw_output).ok()?;
            let sql = tool_call
                .raw_input
                .as_ref()?
                .get("sql")?
                .as_str()?
                .to_owned();
            Some(DatabaseCard::Write { result, sql })
        }
        (ToolCallStatus::Failed, DATABASE_QUERY_TOOL) => {
            let message = raw_output
                .as_array()?
                .iter()
                .find_map(|content| content.get("Text")?.as_str())?
                .to_owned();
            // Only the panel's own errors get a card, since the user has to act on them; a SQL
            // error is the model's to fix and it usually retries.
            message
                .contains("painel Banco")
                .then_some(DatabaseCard::NotConnected { message })
        }
        _ => None,
    }
}

fn presentation(result: &DatabaseQueryResult) -> Presentation {
    let series = result.columns.len() == 2
        && result
            .columns
            .get(1)
            .is_some_and(|column| is_numeric(column.type_name.as_deref()))
        && (2..=MAX_SERIES_POINTS).contains(&result.rows.len());
    match (result.rows.len(), result.columns.len()) {
        (0, _) => Presentation::Empty,
        (1, 1) => Presentation::Number,
        (1, _) => Presentation::Record,
        _ if series => Presentation::Series,
        _ => Presentation::Table,
    }
}

fn is_numeric(type_name: Option<&str>) -> bool {
    matches!(
        type_name,
        Some("int2" | "int4" | "int8" | "numeric" | "float4" | "float8" | "money" | "oid")
    )
}

/// Values as the server prints them, made readable: NULL spelled out, booleans as words and
/// timestamps without seconds and zone.
fn display_value(value: Option<&str>, type_name: Option<&str>) -> String {
    let Some(value) = value else {
        return "NULL".to_owned();
    };
    match (type_name, value) {
        (Some("bool"), "t") => "true".to_owned(),
        (Some("bool"), "f") => "false".to_owned(),
        (Some("timestamp" | "timestamptz"), value) => {
            format_timestamp(value).unwrap_or_else(|| value.to_owned())
        }
        (Some("date"), value) => format_date(value).unwrap_or_else(|| value.to_owned()),
        _ => value.replace(['\n', '\r'], " "),
    }
}

fn format_date(value: &str) -> Option<String> {
    let mut parts = value.get(..10)?.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    (year.len() == 4 && month.len() == 2 && day.len() == 2).then(|| format!("{day}/{month}/{year}"))
}

fn format_timestamp(value: &str) -> Option<String> {
    let date = format_date(value)?;
    let time = value.get(11..16)?;
    Some(format!("{date} {time}"))
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

/// Booleans and status-like columns read better as a colored tag than as text.
fn badge_color(column: &str, type_name: Option<&str>, value: &str) -> Option<Color> {
    if type_name == Some("bool") {
        return Some(if value == "true" {
            Color::Success
        } else {
            Color::Muted
        });
    }
    let column = column.to_lowercase();
    let is_status = ["status", "state", "situacao", "situação", "stage", "estado"]
        .iter()
        .any(|name| column == *name || column.ends_with(&format!("_{name}")));
    if !is_status || value.chars().count() > 24 {
        return None;
    }
    let value = value.to_lowercase();
    let has_any = |words: &[&str]| words.iter().any(|word| value.contains(word));
    Some(
        if has_any(&[
            "fail", "falh", "error", "erro", "recus", "reject", "rejeit", "cancel", "block",
            "bloque", "denied", "negad",
        ]) {
            Color::Error
        } else if has_any(&[
            "inativ", "inactive", "disabled", "desativ", "archiv", "arquiv",
        ]) {
            Color::Muted
        } else if has_any(&[
            "pend", "process", "timeout", "wait", "aguard", "expir", "review", "análise", "analise",
        ]) {
            Color::Warning
        } else if has_any(&[
            "activ", "ativ", "paid", "pago", "succe", "sucesso", "done", "conclu", "complet",
            "approv", "aprov", "confirm", "enabled", "ok",
        ]) {
            Color::Success
        } else {
            Color::Muted
        },
    )
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn to_csv(result: &DatabaseQueryResult) -> String {
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

fn one_line(sql: &str, max_chars: usize) -> String {
    let line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > max_chars {
        format!("{}…", line.chars().take(max_chars).collect::<String>())
    } else {
        line
    }
}

fn rows_label(count: usize) -> String {
    if count == 1 {
        "1 linha".to_owned()
    } else {
        format!("{} linhas", format_number(&count.to_string()))
    }
}

impl ThreadView {
    pub(super) fn render_database_card(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        card: DatabaseCard,
        layout: ToolCallLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let sql_expanded = self.expanded_database_sql.contains(&tool_call.id);
        let content = match card {
            DatabaseCard::Query(result) => {
                self.render_query_card(entry_ix, tool_call, result, sql_expanded, cx)
            }
            DatabaseCard::Write { result, sql } => {
                self.render_write_card(entry_ix, result, sql, cx)
            }
            DatabaseCard::NotConnected { message } => {
                self.render_not_connected_card(entry_ix, message)
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

    fn render_card_header(
        &self,
        icon: IconName,
        icon_color: Color,
        title: String,
        header_badge: Option<(String, Color)>,
        connection: &DatabaseConnectionInfo,
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

    fn render_query_card(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        result: DatabaseQueryResult,
        sql_expanded: bool,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let source = result
            .source
            .clone()
            .unwrap_or_else(|| result.connection.database.clone());
        let presentation = presentation(&result);
        let limit_badge = result
            .truncated
            .then(|| (format!("limite {}", result.limit), Color::Warning));
        let (icon, icon_color, title, badge_for_header) = match presentation {
            Presentation::Empty => (
                IconName::XCircle,
                Color::Error,
                format!("{source} · 0 linhas"),
                None,
            ),
            Presentation::Number => (IconName::Hash, Color::Accent, source, None),
            Presentation::Record => {
                let id = result
                    .columns
                    .iter()
                    .position(|column| column.name == "id")
                    .and_then(|index| result.rows.first()?.get(index)?.clone());
                let title = match id {
                    Some(id) => format!("{source} · id {id}"),
                    None => format!("{source} · 1 linha"),
                };
                (IconName::Check, Color::Success, title, None)
            }
            Presentation::Series | Presentation::Table => (
                IconName::Table,
                Color::Muted,
                format!("{source} · {}", rows_label(result.total_rows)),
                limit_badge,
            ),
        };
        let header = self.render_card_header(
            icon,
            icon_color,
            title,
            badge_for_header,
            &result.connection,
            cx,
        );
        let body = match presentation {
            Presentation::Empty => render_empty(&result, cx),
            Presentation::Number => render_number(&result),
            Presentation::Record => self.render_record(&result, cx),
            Presentation::Series => render_series(&result, cx),
            Presentation::Table => self.render_table(&result, cx),
        };
        let copy_label = match presentation {
            Presentation::Record => "Copiar",
            _ => "Copiar CSV",
        };
        let copy_text = match presentation {
            Presentation::Record => result
                .columns
                .iter()
                .zip(result.rows.first().into_iter().flatten())
                .map(|(column, value)| {
                    format!("{}: {}", column.name, value.as_deref().unwrap_or("NULL"))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => to_csv(&result),
        };
        let has_rows = !matches!(presentation, Presentation::Empty);
        let tool_call_id = tool_call.id.clone();
        let sql = result.sql.clone();
        let footer = h_flex()
            .w_full()
            .gap_1()
            .px_1()
            .py_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Button::new(
                    ("database-card-sql", entry_ix),
                    if sql_expanded {
                        "Esconder SQL"
                    } else {
                        "Ver SQL"
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
                    if !this.expanded_database_sql.remove(&tool_call_id) {
                        this.expanded_database_sql.insert(tool_call_id.clone());
                    }
                    cx.notify();
                })),
            )
            .child(
                Button::new(("database-card-open", entry_ix), "Abrir no Banco")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_query_in_database(sql.clone(), window, cx);
                    })),
            )
            .when(has_rows, |this| {
                this.child(
                    Button::new(("database-card-copy", entry_ix), copy_label)
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
            .child(
                Label::new(format!("{} ms", result.elapsed_ms))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .into_any_element();
        let mut elements = vec![header, body];
        if sql_expanded {
            elements.push(self.render_sql_block(&result.sql, cx));
        }
        elements.push(footer);
        elements
    }

    fn render_sql_block(&self, sql: &str, cx: &Context<Self>) -> AnyElement {
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
            .child(SharedString::from(sql.trim().to_owned()))
            .into_any_element()
    }

    fn render_record(&self, result: &DatabaseQueryResult, cx: &Context<Self>) -> AnyElement {
        let Some(row) = result.rows.first() else {
            return Empty.into_any_element();
        };
        let hidden = result.columns.len().saturating_sub(MAX_RECORD_FIELDS);
        v_flex()
            .w_full()
            .py_1()
            .children(result.columns.iter().zip(row).take(MAX_RECORD_FIELDS).map(
                |(column, value)| {
                    let type_name = column.type_name.as_deref();
                    let text = display_value(value.as_deref(), type_name);
                    h_flex()
                        .w_full()
                        .gap_2()
                        .px_2()
                        .py_0p5()
                        .child(
                            div().w(rems(8.)).flex_none().child(
                                Label::new(column.name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .child(div().flex_1().min_w_0().child(value_element(
                            &column.name,
                            type_name,
                            text,
                            value.is_none(),
                            cx,
                        )))
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

    fn render_table(&self, result: &DatabaseQueryResult, cx: &Context<Self>) -> AnyElement {
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
                let widest = rows
                    .iter()
                    .map(|row| {
                        display_value(
                            row.get(index).and_then(|value| value.as_deref()),
                            column.type_name.as_deref(),
                        )
                        .chars()
                        .count()
                    })
                    .chain([column.name.chars().count()])
                    .max()
                    .unwrap_or(1);
                widest.clamp(4, 28) as f32
            })
            .collect::<Vec<_>>();
        let total_weight = weights.iter().sum::<f32>().max(1.);
        let hidden_columns = result.columns.len().saturating_sub(MAX_TABLE_COLUMNS);
        let hidden_rows = result.total_rows.saturating_sub(rows.len());
        let border = self.tool_card_border_color(cx);
        let cell = |index: usize, content: AnyElement| {
            let numeric = columns
                .get(index)
                .is_some_and(|column| is_numeric(column.type_name.as_deref()));
            h_flex()
                .min_w_0()
                .w(relative(
                    weights.get(index).copied().unwrap_or(1.) / total_weight,
                ))
                .when(numeric, |this| this.justify_end())
                .child(content)
        };
        let header =
            h_flex()
                .w_full()
                .gap_2()
                .px_2()
                .py_1()
                .children(columns.iter().enumerate().map(|(index, column)| {
                    cell(
                        index,
                        Label::new(column.name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                            .into_any_element(),
                    )
                }));
        let body = rows.iter().map(|row| {
            h_flex()
                .w_full()
                .gap_2()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(border)
                .children(columns.iter().enumerate().map(|(index, column)| {
                    let value = row.get(index).and_then(|value| value.as_deref());
                    let type_name = column.type_name.as_deref();
                    let text = display_value(value, type_name);
                    cell(
                        index,
                        value_element(&column.name, type_name, text, value.is_none(), cx),
                    )
                }))
        });
        let mut notes = Vec::new();
        if hidden_rows > 0 {
            notes.push(format!("+ {} no resultado", rows_label(hidden_rows)));
        }
        if hidden_columns > 0 {
            notes.push(format!("+ {hidden_columns} colunas"));
        }
        if result.truncated {
            notes.push(format!("o limite de {} cortou o resto", result.limit));
        }
        v_flex()
            .w_full()
            .child(header)
            .children(body)
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

    fn render_write_card(
        &self,
        entry_ix: usize,
        result: DatabaseWriteResult,
        sql: String,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let file_name = result
            .abs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| result.abs_path.display().to_string());
        let header = self.render_card_header(
            IconName::FileCode,
            Color::Warning,
            file_name,
            Some(("não executado".to_owned(), Color::Warning)),
            &result.connection,
            cx,
        );
        let abs_path = result.abs_path.clone();
        let footer = h_flex()
            .w_full()
            .gap_1()
            .px_1()
            .py_1()
            .border_t_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                Button::new(("database-card-focus", entry_ix), "Abrir no editor")
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
                        if let Err(error) =
                            task_agents::focus_sql_editor(&project, &abs_path, window, cx)
                        {
                            this.show_workspace_error(error, cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .child(
                Label::new(format!("linha {} · roda com ⌘↵", result.line))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element();
        vec![header, self.render_sql_block(&sql, cx), footer]
    }

    fn render_not_connected_card(&self, entry_ix: usize, message: String) -> Vec<AnyElement> {
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
                        .child(Label::new("Sem banco para consultar").size(LabelSize::Small))
                        .child(
                            Label::new(message)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    Button::new(("database-card-panel", entry_ix), "Abrir painel Banco")
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Database)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .on_click(|_, window, cx| {
                            if let Some(action) = cx
                                .build_action("database_client::ToggleFocus", None)
                                .log_err()
                            {
                                window.dispatch_action(action, cx);
                            }
                        }),
                )
                .into_any_element(),
        ]
    }

    fn open_query_in_database(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        if let Err(error) = task_agents::open_query_in_database(&project, sql, window, cx) {
            self.show_workspace_error(error, cx);
        }
    }

    fn show_workspace_error(&self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| workspace.show_error(error, cx))
            .log_err();
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

fn value_element(
    column: &str,
    type_name: Option<&str>,
    text: String,
    is_null: bool,
    cx: &App,
) -> AnyElement {
    if !is_null && let Some(color) = badge_color(column, type_name, &text) {
        return badge(text, color, cx);
    }
    let label = Label::new(text)
        .size(LabelSize::Small)
        .color(if is_null {
            Color::Muted
        } else {
            Color::Default
        })
        .truncate();
    let is_text = matches!(
        type_name,
        Some("text" | "varchar" | "bpchar" | "name" | "citext") | None
    );
    if is_text {
        label.into_any_element()
    } else {
        label.buffer_font(cx).into_any_element()
    }
}

fn render_empty(result: &DatabaseQueryResult, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .px_2()
        .py_1p5()
        .child(Label::new("Nenhuma linha.").size(LabelSize::Small))
        .child(
            Label::new(one_line(&result.sql, 140))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate(),
        )
        .into_any_element()
}

fn render_number(result: &DatabaseQueryResult) -> AnyElement {
    let column = result.columns.first();
    let value = result
        .rows
        .first()
        .and_then(|row| row.first())
        .map(|value| {
            let text = display_value(
                value.as_deref(),
                column.and_then(|column| column.type_name.as_deref()),
            );
            format_number(&text)
        })
        .unwrap_or_else(|| "NULL".to_owned());
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

fn render_series(result: &DatabaseQueryResult, cx: &App) -> AnyElement {
    let label_type = result
        .columns
        .first()
        .and_then(|column| column.type_name.clone());
    let points = result
        .rows
        .iter()
        .map(|row| {
            let label = display_value(
                row.first().and_then(|value| value.as_deref()),
                label_type.as_deref(),
            );
            // Days read as dd/mm; the year and time only add noise under a bar.
            let label = label
                .get(..5)
                .filter(|_| label.len() >= 10 && label.as_bytes().get(2) == Some(&b'/'))
                .map(str::to_owned)
                .unwrap_or(label);
            let raw = row
                .get(1)
                .and_then(|value| value.clone())
                .unwrap_or_default();
            let value = raw.parse::<f64>().unwrap_or(0.).max(0.);
            (label, raw, value)
        })
        .collect::<Vec<_>>();
    let max = points.iter().map(|(_, _, value)| *value).fold(0., f64::max);
    let accent = cx.theme().colors().text_accent;
    h_flex()
        .w_full()
        .items_end()
        .gap_1()
        .px_3()
        .pt_3()
        .pb_2()
        .children(points.into_iter().map(|(label, raw, value)| {
            let height = if max > 0. {
                (value / max * f64::from(MAX_BAR_HEIGHT)) as f32
            } else {
                0.
            };
            let is_peak = max > 0. && value == max;
            v_flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_0p5()
                .child(
                    Label::new(format_number(&raw))
                        .size(LabelSize::XSmall)
                        .color(if is_peak { Color::Accent } else { Color::Muted })
                        .buffer_font(cx)
                        .truncate(),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(height.max(2.)))
                        .rounded_sm()
                        .bg(accent.opacity(if is_peak { 0.9 } else { 0.3 })),
                )
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use task_agents::DatabaseColumn;

    fn result(columns: &[(&str, &str)], rows: Vec<Vec<Option<&str>>>) -> DatabaseQueryResult {
        DatabaseQueryResult {
            columns: columns
                .iter()
                .map(|(name, type_name)| DatabaseColumn {
                    name: (*name).to_owned(),
                    type_name: Some((*type_name).to_owned()),
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
        let empty = result(&[("id", "int8")], vec![]);
        assert!(matches!(presentation(&empty), Presentation::Empty));
        let number = result(&[("count", "int8")], vec![vec![Some("1284")]]);
        assert!(matches!(presentation(&number), Presentation::Number));
        let record = result(
            &[("id", "int8"), ("email", "text")],
            vec![vec![Some("1"), Some("a@b.c")]],
        );
        assert!(matches!(presentation(&record), Presentation::Record));
        let series = result(
            &[("day", "date"), ("count", "int8")],
            vec![
                vec![Some("2026-09-22"), Some("142")],
                vec![Some("2026-09-23"), Some("168")],
            ],
        );
        assert!(matches!(presentation(&series), Presentation::Series));
        let table = result(
            &[("id", "int8"), ("email", "text")],
            vec![vec![Some("1"), Some("a")], vec![Some("2"), Some("b")]],
        );
        assert!(matches!(presentation(&table), Presentation::Table));
    }

    #[test]
    fn formats_values_for_reading() {
        assert_eq!(format_number("1284"), "1.284");
        assert_eq!(format_number("-1284567"), "-1.284.567");
        assert_eq!(format_number("12.5"), "12.5");
        assert_eq!(
            display_value(Some("2026-03-12 10:22:31.123+00"), Some("timestamptz")),
            "12/03/2026 10:22"
        );
        assert_eq!(display_value(Some("t"), Some("bool")), "true");
        assert_eq!(display_value(None, Some("text")), "NULL");
    }

    #[test]
    fn colors_status_values() {
        assert_eq!(
            badge_color("status", Some("text"), "failed"),
            Some(Color::Error)
        );
        assert_eq!(
            badge_color("payment_status", Some("text"), "paid"),
            Some(Color::Success)
        );
        assert_eq!(
            badge_color("status", Some("text"), "inativo"),
            Some(Color::Muted)
        );
        assert_eq!(badge_color("email", Some("text"), "failed"), None);
    }

    #[test]
    fn quotes_csv_fields() {
        let csv = to_csv(&result(
            &[("name", "text")],
            vec![vec![Some("a, \"b\"")], vec![None]],
        ));
        assert_eq!(csv, "name\n\"a, \"\"b\"\"\"\n");
    }
}
