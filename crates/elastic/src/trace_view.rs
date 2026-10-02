//! One APM trace as a waterfall: the transactions and spans of `traces-apm*` for a `trace.id`,
//! nested by `parent.id`, with the errors and the log lines that carry the same trace.

use crate::{
    esql,
    panel::{ElasticPanel, Session},
    results::{self, Rows},
};
use collections::HashMap;
use gpui::{
    AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    Task, WeakEntity, px,
};
use project::Project;
use std::sync::Arc;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

const NAME_WIDTH: f32 = 300.;
const SERVICE_WIDTH: f32 = 110.;
const DURATION_WIDTH: f32 = 72.;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Span {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub service: String,
    /// `transaction` or `span`.
    pub event: String,
    /// `db`, `external`, `messaging`, `cache`… for spans.
    pub kind: Option<String>,
    pub start_us: u64,
    pub duration_us: u64,
    pub failed: bool,
    pub status: Option<u64>,
    pub destination: Option<String>,
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TraceError {
    pub parent: Option<String>,
    pub message: String,
    pub culprit: Option<String>,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TraceLog {
    pub span: Option<String>,
    pub transaction: Option<String>,
    pub timestamp: String,
    pub level: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Trace {
    pub spans: Vec<Span>,
    pub errors: Vec<TraceError>,
    pub logs: Vec<TraceLog>,
}

impl Trace {
    pub fn start_us(&self) -> u64 {
        self.spans
            .iter()
            .map(|span| span.start_us)
            .min()
            .unwrap_or(0)
    }

    pub fn duration_us(&self) -> u64 {
        let start = self.start_us();
        self.spans
            .iter()
            .map(|span| span.start_us + span.duration_us)
            .max()
            .unwrap_or(start)
            .saturating_sub(start)
            .max(1)
    }

    pub fn root(&self) -> Option<&Span> {
        self.spans.first()
    }

    fn errors_for(&self, span: &Span) -> Vec<&TraceError> {
        self.errors
            .iter()
            .filter(|error| error.parent.as_deref() == Some(span.id.as_str()))
            .collect()
    }

    fn logs_for(&self, span: &Span) -> Vec<&TraceLog> {
        self.logs
            .iter()
            .filter(|log| {
                log.span.as_deref() == Some(span.id.as_str())
                    || (span.event == "transaction"
                        && log.span.is_none()
                        && log.transaction.as_deref() == Some(span.id.as_str()))
            })
            .collect()
    }

    /// The trace as text for the agent.
    pub fn describe(&self, trace_id: &str) -> String {
        let start = self.start_us();
        let mut text = format!(
            "Trace {trace_id}: {} spans, {} em {} serviço(s), {} erro(s), {} log(s).\n",
            self.spans.len(),
            results::format_micros(self.duration_us()),
            self.spans
                .iter()
                .map(|span| span.service.as_str())
                .collect::<collections::HashSet<_>>()
                .len(),
            self.errors.len(),
            self.logs.len()
        );
        for span in &self.spans {
            text.push_str(&format!(
                "{}- {} [{}{}] +{} · {}{}{}\n",
                "  ".repeat(span.depth),
                span.name,
                span.service,
                span.kind
                    .as_deref()
                    .map(|kind| format!(", {kind}"))
                    .unwrap_or_default(),
                results::format_micros(span.start_us.saturating_sub(start)),
                results::format_micros(span.duration_us),
                span.status
                    .map(|status| format!(" · HTTP {status}"))
                    .unwrap_or_default(),
                if span.failed { " · FALHOU" } else { "" }
            ));
        }
        for error in &self.errors {
            text.push_str(&format!(
                "erro {}: {}{}\n",
                error.timestamp,
                crate::mask::mask(&error.message),
                error
                    .culprit
                    .as_deref()
                    .map(|culprit| format!(" (em {culprit})"))
                    .unwrap_or_default()
            ));
        }
        for log in self.logs.iter().take(50) {
            text.push_str(&format!(
                "log {} {} {}\n",
                log.timestamp,
                log.level.as_deref().unwrap_or("-"),
                crate::mask::mask(&log.message)
            ));
        }
        text
    }
}

fn number(rows: &Rows, row: usize, name: &str) -> Option<u64> {
    let value = rows.value(row, name)?;
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|value| value as u64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn timestamp_us(rows: &Rows, row: usize) -> u64 {
    number(rows, row, "timestamp.us").unwrap_or_else(|| {
        rows.text(row, results::TIMESTAMP)
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(&timestamp).ok())
            .and_then(|time| u64::try_from(time.timestamp_micros()).ok())
            .unwrap_or(0)
    })
}

/// Builds the waterfall out of the raw rows: parents before children, siblings by start.
pub(crate) fn build_trace(spans: Rows, logs: Rows) -> Trace {
    let mut all = Vec::new();
    for row in 0..spans.values.len() {
        let event = spans.text(row, "processor.event").unwrap_or_default();
        let (id, name, duration) = match event.as_str() {
            "transaction" => (
                spans.text(row, "transaction.id"),
                spans.text(row, "transaction.name"),
                number(&spans, row, "transaction.duration.us"),
            ),
            "span" => (
                spans.text(row, "span.id"),
                spans.text(row, "span.name"),
                number(&spans, row, "span.duration.us"),
            ),
            _ => continue,
        };
        let Some(id) = id else {
            continue;
        };
        all.push(Span {
            id,
            parent: spans.text(row, "parent.id"),
            name: name.unwrap_or_else(|| "(sem nome)".to_string()),
            service: spans.text(row, "service.name").unwrap_or_default(),
            event,
            kind: spans.text(row, "span.type"),
            start_us: timestamp_us(&spans, row),
            duration_us: duration.unwrap_or(0),
            failed: spans.text(row, "event.outcome").as_deref() == Some("failure"),
            status: number(&spans, row, "http.response.status_code"),
            destination: spans.text(row, "span.destination.service.resource"),
            depth: 0,
        });
    }
    let ids: collections::HashSet<String> = all.iter().map(|span| span.id.clone()).collect();
    let mut children: HashMap<Option<String>, Vec<Span>> = HashMap::default();
    for span in all {
        let parent = span.parent.clone().filter(|parent| ids.contains(parent));
        children.entry(parent).or_default().push(span);
    }
    for list in children.values_mut() {
        list.sort_by_key(|span| span.start_us);
    }
    let mut ordered = Vec::new();
    let mut stack: Vec<(Span, usize)> = children
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|span| (span, 0))
        .collect();
    while let Some((mut span, depth)) = stack.pop() {
        span.depth = depth;
        if let Some(kids) = children.remove(&Some(span.id.clone())) {
            stack.extend(kids.into_iter().rev().map(|kid| (kid, depth + 1)));
        }
        ordered.push(span);
    }

    let mut errors = Vec::new();
    let mut trace_logs = Vec::new();
    for row in 0..logs.values.len() {
        let timestamp = logs.text(row, results::TIMESTAMP).unwrap_or_default();
        if logs.text(row, "processor.event").as_deref() == Some("error") {
            errors.push(TraceError {
                parent: logs.text(row, "parent.id"),
                message: logs
                    .text(row, "error.exception.message")
                    .or_else(|| logs.text(row, "error.log.message"))
                    .or_else(|| logs.text(row, "error.grouping_name"))
                    .or_else(|| logs.text(row, results::MESSAGE))
                    .unwrap_or_else(|| "erro".to_string()),
                culprit: logs.text(row, "error.culprit"),
                timestamp,
            });
        } else {
            trace_logs.push(TraceLog {
                span: logs.text(row, "span.id"),
                transaction: logs.text(row, "transaction.id"),
                level: logs.text(row, results::LEVEL),
                message: logs.text(row, results::MESSAGE).unwrap_or_default(),
                timestamp,
            });
        }
    }
    Trace {
        spans: ordered,
        errors,
        logs: trace_logs,
    }
}

pub(crate) async fn load_trace(session: &Session, trace_id: &str) -> anyhow::Result<Trace> {
    let literal = esql::string_literal(trace_id);
    let spans_query =
        format!("FROM traces-apm* | WHERE trace.id == {literal} | SORT @timestamp | LIMIT 1000");
    let logs_query =
        format!("FROM logs-* | WHERE trace.id == {literal} | SORT @timestamp | LIMIT 300");
    let spans = session.elastic.esql(&spans_query, None);
    let logs = session.elastic.esql(&logs_query, None);
    let (spans, logs) = futures::join!(spans, logs);
    let spans = spans?;
    // Without log data streams (or the privilege to read them) the waterfall still works.
    let logs = logs.log_err().map(Rows::from).unwrap_or_default();
    Ok(build_trace(spans.into(), logs))
}

enum State {
    Loading,
    Loaded(Trace),
    Failed(SharedString),
}

pub struct TraceView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    trace_id: String,
    state: State,
    selected: Option<usize>,
    similar: Option<SharedString>,
    focus_handle: FocusHandle,
    load_task: Task<()>,
    similar_task: Task<()>,
}

impl TraceView {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        panel: WeakEntity<ElasticPanel>,
        session: Arc<Session>,
        trace_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            workspace,
            project,
            panel,
            session,
            trace_id,
            state: State::Loading,
            selected: None,
            similar: None,
            focus_handle: cx.focus_handle(),
            load_task: Task::ready(()),
            similar_task: Task::ready(()),
        };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loading;
        cx.notify();
        let session = self.session.clone();
        let trace_id = self.trace_id.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            let result = load_trace(&session, &trace_id).await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(trace) if trace.spans.is_empty() => State::Failed(
                        format!(
                            "Nenhum span com trace.id {} em traces-apm*. O APM pode ter \
                             descartado a amostra, ou o trace é de outro cluster.",
                            this.trace_id
                        )
                        .into(),
                    ),
                    Ok(trace) => {
                        // The failed external call is what people open a trace for.
                        let selected = trace
                            .spans
                            .iter()
                            .position(|span| span.failed && span.event == "span")
                            .or_else(|| trace.spans.iter().position(|span| span.failed));
                        this.state = State::Loaded(trace);
                        this.select(selected, cx);
                        return;
                    }
                    Err(error) => State::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        self.selected = index;
        self.similar = None;
        cx.notify();
        let State::Loaded(trace) = &self.state else {
            return;
        };
        let Some(span) = index.and_then(|index| trace.spans.get(index)) else {
            return;
        };
        let (field, duration) = if span.event == "transaction" {
            ("transaction.name", "transaction.duration.us")
        } else {
            ("span.name", "span.duration.us")
        };
        let query = format!(
            "FROM traces-apm* | WHERE {field} == {} AND service.name == {} AND @timestamp >= NOW() - 30 minutes \
             | STATS total = COUNT(*), failed = COUNT(*) WHERE event.outcome == \"failure\", \
             p95 = PERCENTILE({duration}, 95)",
            esql::string_literal(&span.name),
            esql::string_literal(&span.service),
        );
        let elastic = self.session.elastic.clone();
        self.similar_task = cx.spawn(async move |this, cx| {
            let result = elastic.esql(&query, None).await;
            this.update(cx, |this, cx| {
                this.similar = result.log_err().and_then(|result| {
                    let rows = Rows::from(result);
                    let total = number(&rows, 0, "total")?;
                    let failed = number(&rows, 0, "failed").unwrap_or(0);
                    let p95 = number(&rows, 0, "p95").unwrap_or(0);
                    Some(
                        format!(
                            "{} vezes nos últimos 30 min · {failed} em erro · p95 {}",
                            results::format_count(total),
                            results::format_micros(p95)
                        )
                        .into(),
                    )
                });
                cx.notify();
            })
            .log_err();
        });
    }

    fn logs_query(&self) -> String {
        format!(
            "FROM logs-*\n| WHERE trace.id == {}\n| SORT @timestamp\n| LIMIT 500",
            esql::string_literal(&self.trace_id)
        )
    }

    fn ask_agent(&self, window: &mut Window, cx: &mut App) {
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id: format!("trace:{}", self.trace_id),
                title: format!("trace {}", short_id(&self.trace_id)),
                prompt: Some(
                    "Leia este trace e explique onde o tempo foi gasto, o que falhou e por quê. \
                     Aponte o código quando der."
                        .to_string(),
                ),
                submit: true,
            }),
            cx,
        );
    }

    fn kind_color(span: &Span, cx: &App) -> Hsla {
        if span.failed {
            return Color::Error.color(cx);
        }
        match (span.event.as_str(), span.kind.as_deref()) {
            ("transaction", _) => cx.theme().colors().text_accent,
            (_, Some("db" | "cache")) => Color::Info.color(cx),
            (_, Some("messaging")) => Color::Warning.color(cx),
            (_, Some("external")) => Color::Created.color(cx),
            _ => Color::Muted.color(cx),
        }
    }

    fn render_header(&self, trace: &Trace, cx: &mut Context<Self>) -> AnyElement {
        let root = trace.root();
        let status = root.and_then(|root| root.status);
        let services = trace
            .spans
            .iter()
            .map(|span| span.service.as_str())
            .collect::<collections::HashSet<_>>()
            .len();
        let failed = root.is_some_and(|root| root.failed) || !trace.errors.is_empty();
        v_flex()
            .px_4()
            .pt_3()
            .pb_2()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(
                            root.map(|root| root.name.clone())
                                .unwrap_or_else(|| "trace".to_string()),
                        )
                        .size(LabelSize::Large)
                        .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("elastic-trace-logs", "Logs deste trace")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Filter).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let body = this.logs_query();
                                this.panel
                                    .update(cx, |panel, cx| {
                                        panel.new_query(Some(body), true, window, cx)
                                    })
                                    .log_err();
                            })),
                    )
                    .child(
                        Button::new("elastic-trace-agent", "Perguntar ao Agent")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.ask_agent(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("elastic-trace-kibana", "Abrir no Kibana")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(url) = crate::kibana_trace_url(
                                    &this.session.connection,
                                    &this.trace_id,
                                ) {
                                    cx.open_url(&url);
                                }
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when_some(status, |this, status| {
                        let color = if status >= 500 {
                            Color::Error
                        } else if status >= 400 {
                            Color::Warning
                        } else {
                            Color::Success
                        };
                        this.child(
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .bg(color.color(cx).opacity(0.14))
                                .child(
                                    Label::new(status.to_string())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(color),
                                ),
                        )
                    })
                    .child(
                        Label::new(format!(
                            "{} · {} · {} spans em {services} {} · {} erro(s) do APM · {} log(s)",
                            results::format_micros(trace.duration_us()),
                            root.map(|root| root.service.clone()).unwrap_or_default(),
                            trace.spans.len(),
                            if services == 1 {
                                "serviço"
                            } else {
                                "serviços"
                            },
                            trace.errors.len(),
                            trace.logs.len(),
                        ))
                        .size(LabelSize::Small)
                        .color(if failed {
                            Color::Error
                        } else {
                            Color::Muted
                        }),
                    ),
            )
            .child(
                h_flex()
                    .id("elastic-trace-id")
                    .gap_1()
                    .cursor_pointer()
                    .tooltip(Tooltip::text("Copiar o trace.id"))
                    .on_click({
                        let trace_id = self.trace_id.clone();
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(trace_id.clone()))
                        }
                    })
                    .child(
                        Label::new(format!("trace.id {}", self.trace_id))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }

    fn render_waterfall(&self, trace: &Trace, cx: &mut Context<Self>) -> AnyElement {
        let start = trace.start_us();
        let total = trace.duration_us() as f32;
        let ticks = [0.0_f32, 0.25, 0.5, 0.75, 1.0];
        v_flex()
            .mx_4()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_3()
                    .h(px(26.))
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(section_label("SPAN").w(px(NAME_WIDTH)))
                    .child(section_label("SERVIÇO").w(px(SERVICE_WIDTH)))
                    .child(
                        h_flex()
                            .flex_1()
                            .justify_between()
                            .children(ticks.iter().map(|tick| {
                                Label::new(results::format_micros((total * tick) as u64))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Disabled)
                            })),
                    )
                    .child(section_label("DURAÇÃO").w(px(DURATION_WIDTH))),
            )
            .children(trace.spans.iter().enumerate().map(|(index, span)| {
                let offset = (span.start_us.saturating_sub(start)) as f32 / total;
                let width = (span.duration_us as f32 / total).clamp(0.004, 1.0 - offset.min(0.996));
                let color = Self::kind_color(span, cx);
                let selected = self.selected == Some(index);
                let has_errors = !trace.errors_for(span).is_empty();
                h_flex()
                    .id(("elastic-span", index))
                    .px_3()
                    .h(px(30.))
                    .gap_2()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().colors().element_selected)
                            .border_l_2()
                            .border_color(cx.theme().colors().text_accent)
                    })
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        div()
                            .w(px(NAME_WIDTH))
                            .flex_none()
                            .pl(px(span.depth as f32 * 14.))
                            .child(
                                Label::new(span.name.clone())
                                    .size(LabelSize::Small)
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
                        div().w(px(SERVICE_WIDTH)).flex_none().child(
                            Label::new(span.service.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().flex_1().h(px(10.)).relative().child(
                            div()
                                .absolute()
                                .top_0()
                                .left(gpui::relative(offset))
                                .w(gpui::relative(width))
                                .h(px(10.))
                                .rounded_sm()
                                .bg(if span.event == "transaction" {
                                    color.opacity(0.4)
                                } else {
                                    color.opacity(0.9)
                                })
                                .when(has_errors, |this| {
                                    this.border_r_2().border_color(Color::Error.color(cx))
                                }),
                        ),
                    )
                    .child(
                        div().w(px(DURATION_WIDTH)).flex_none().child(
                            Label::new(results::format_micros(span.duration_us))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(if span.failed {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                        ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if this.selected == Some(index) {
                            None
                        } else {
                            Some(index)
                        };
                        this.select(next, cx)
                    }))
            }))
            .into_any_element()
    }

    fn render_span(&self, trace: &Trace, span: &Span, cx: &mut Context<Self>) -> AnyElement {
        let errors = trace.errors_for(span);
        let logs = trace.logs_for(span);
        let fields = [
            ("processor.event", Some(span.event.clone())),
            ("span.type", span.kind.clone()),
            (
                "http.response.status_code",
                span.status.map(|status| status.to_string()),
            ),
            ("destination", span.destination.clone()),
            (
                "event.outcome",
                Some(if span.failed { "failure" } else { "success" }.to_string()),
            ),
            ("id", Some(span.id.clone())),
        ];
        v_flex()
            .id("elastic-span-detail")
            .m_4()
            .p_3()
            .gap_2p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .overflow_y_scroll()
            .child(
                Label::new(span.name.clone())
                    .weight(FontWeight::SEMIBOLD)
                    .buffer_font(cx),
            )
            .child(
                Label::new(format!(
                    "{} · {} · {:.0}% do trace",
                    span.service,
                    results::format_micros(span.duration_us),
                    span.duration_us as f32 / trace.duration_us() as f32 * 100.
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(h_flex().flex_wrap().gap_x_6().gap_y_1p5().children(
                fields.into_iter().filter_map(|(name, value)| {
                    let value = value?;
                    let failure = value == "failure"
                        || (name == "http.response.status_code"
                            && value.parse::<u64>().is_ok_and(|status| status >= 500));
                    Some(
                        v_flex()
                            .child(
                                Label::new(name)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(value)
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(if failure {
                                        Color::Error
                                    } else {
                                        Color::Default
                                    }),
                            ),
                    )
                }),
            ))
            .when(!errors.is_empty(), |this| {
                this.child(section_label("ERRO DO APM"))
                    .children(errors.into_iter().map(|error| {
                        v_flex()
                            .child(
                                Label::new(format!("Error: {}", error.message))
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(Color::Error),
                            )
                            .when_some(error.culprit.clone(), |this, culprit| {
                                let link = culprit_link(&culprit).filter(|link| {
                                    results::resolve_in_project(&self.project, &link.path, cx)
                                        .is_some()
                                });
                                let workspace = self.workspace.clone();
                                let project = self.project.clone();
                                this.child(
                                    h_flex()
                                        .id("elastic-error-culprit")
                                        .child(
                                            Label::new(format!("    em {culprit}"))
                                                .size(LabelSize::Small)
                                                .buffer_font(cx)
                                                .color(if link.is_some() {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                }),
                                        )
                                        .when_some(link, move |this, link| {
                                            this.cursor_pointer()
                                                .tooltip(Tooltip::text(format!(
                                                    "Abrir {}",
                                                    link.path
                                                )))
                                                .on_click(move |_, window, cx| {
                                                    results::open_stack_link(
                                                        workspace.clone(),
                                                        &project,
                                                        &link,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                        }),
                                )
                            })
                    }))
            })
            .when(!logs.is_empty(), |this| {
                this.child(section_label(if logs.len() == 1 {
                    "LOG NESTE SPAN".to_string()
                } else {
                    format!("LOGS NESTE SPAN · {}", logs.len())
                }))
                .children(logs.into_iter().take(20).map(|log| {
                    h_flex()
                        .gap_2()
                        .child(
                            div().w(px(92.)).flex_none().child(
                                Label::new(results::short_time(&log.timestamp))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            ),
                        )
                        .when_some(log.level.clone(), |this, level| {
                            this.child(results::level_chip(&level, cx))
                        })
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(log.message.replace('\n', " "))
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                        )
                }))
            })
            .child(section_label("PARECIDOS · ÚLTIMOS 30 MIN"))
            .child(
                Label::new(
                    self.similar
                        .clone()
                        .unwrap_or_else(|| "Contando no traces-apm*…".into()),
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }
}

fn section_label(text: impl Into<SharedString>) -> gpui::Div {
    div().child(
        Label::new(text)
            .size(LabelSize::XSmall)
            .weight(FontWeight::SEMIBOLD)
            .color(Color::Muted),
    )
}

/// `capturePayment (src/modules/payments/checkout.ts)` → that file, at its first line: the APM
/// culprit names the file of the top frame but not the line.
fn culprit_link(culprit: &str) -> Option<results::StackLink> {
    let start = culprit.rfind('(')? + 1;
    let end = culprit[start..].find(')')? + start;
    let path = culprit[start..end].trim();
    (path.contains('/') && path.contains('.')).then(|| results::StackLink {
        path: path.to_string(),
        line: 1,
        column: None,
        range: start..end,
    })
}

fn short_id(trace_id: &str) -> &str {
    trace_id.get(..8).unwrap_or(trace_id)
}

impl Render for TraceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            State::Loading => div()
                .p_4()
                .child(
                    Label::new("Lendo o trace em traces-apm* e os logs com o mesmo trace.id…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            State::Failed(error) => v_flex()
                .p_4()
                .gap_2()
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    Button::new("elastic-trace-retry", "Tentar de novo")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                )
                .into_any_element(),
            State::Loaded(trace) => {
                let trace = trace.clone();
                let selected = self
                    .selected
                    .and_then(|index| trace.spans.get(index))
                    .cloned();
                v_flex()
                    .id("elastic-trace")
                    .size_full()
                    .overflow_y_scroll()
                    .child(self.render_header(&trace, cx))
                    .child(self.render_waterfall(&trace, cx))
                    .when_some(selected, |this, span| {
                        this.child(self.render_span(&trace, &span, cx))
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("ElasticTraceView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

impl Focusable for TraceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TraceView {}

impl Item for TraceView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("trace · {}", short_id(&self.trace_id)).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(
            format!(
                "trace.id {} · {}",
                self.trace_id, self.session.connection.name
            )
            .into(),
        )
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens a trace tab, or focuses the one already showing it.
pub fn open(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    trace_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace_handle = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            let existing = workspace
                .items_of_type::<TraceView>(cx)
                .find(|view| view.read(cx).trace_id == trace_id);
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let view = cx
                .new(|cx| TraceView::new(workspace_handle, project, panel, session, trace_id, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .log_err();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EsqlColumn;
    use serde_json::json;

    fn rows(columns: &[&str], values: Vec<Vec<serde_json::Value>>) -> Rows {
        Rows {
            columns: columns
                .iter()
                .map(|name| EsqlColumn {
                    name: name.to_string(),
                    kind: "keyword".to_string(),
                })
                .collect(),
            values,
        }
    }

    #[test]
    fn spans_nest_under_their_parents_in_start_order() {
        let columns = [
            "processor.event",
            "transaction.id",
            "transaction.name",
            "transaction.duration.us",
            "span.id",
            "span.name",
            "span.duration.us",
            "parent.id",
            "timestamp.us",
            "service.name",
            "event.outcome",
        ];
        let null = serde_json::Value::Null;
        let spans = rows(
            &columns,
            vec![
                vec![
                    json!("span"),
                    null.clone(),
                    null.clone(),
                    null.clone(),
                    json!("s2"),
                    json!("POST adquirente"),
                    json!(10_000_000),
                    json!("t1"),
                    json!(1_060),
                    json!("trix-api"),
                    json!("failure"),
                ],
                vec![
                    json!("transaction"),
                    json!("t1"),
                    json!("POST /v1/checkout/pay"),
                    json!(10_412_000),
                    null.clone(),
                    null.clone(),
                    null.clone(),
                    null.clone(),
                    json!(1_000),
                    json!("trix-api"),
                    json!("failure"),
                ],
                vec![
                    json!("span"),
                    null.clone(),
                    null.clone(),
                    null,
                    json!("s1"),
                    json!("SELECT carts"),
                    json!(38_000),
                    json!("t1"),
                    json!(1_004),
                    json!("trix-api"),
                    json!("success"),
                ],
            ],
        );
        let logs = rows(
            &[
                "processor.event",
                "parent.id",
                "error.exception.message",
                "@timestamp",
            ],
            vec![vec![
                json!("error"),
                json!("s2"),
                json!("Adquirente respondeu 502"),
                json!("2026-09-30T13:41:47.553Z"),
            ]],
        );
        let trace = build_trace(spans, logs);
        let names: Vec<(&str, usize)> = trace
            .spans
            .iter()
            .map(|span| (span.name.as_str(), span.depth))
            .collect();
        assert_eq!(
            names,
            [
                ("POST /v1/checkout/pay", 0),
                ("SELECT carts", 1),
                ("POST adquirente", 1)
            ]
        );
        assert_eq!(trace.duration_us(), 10_412_000);
        assert_eq!(trace.errors_for(&trace.spans[2]).len(), 1);
        assert!(trace.describe("abc").contains("FALHOU"));
    }
}
