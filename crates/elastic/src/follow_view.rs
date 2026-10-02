//! Follows a data stream. Elasticsearch doesn't push new documents, so this asks every two
//! seconds for what arrived since the last one, re-reading 30 s back because shippers and the
//! index refresh deliver late; `_id` keeps a document from showing twice.

use crate::{
    ApplyFollowFilter, TogglePause,
    client::TimeRange,
    esql,
    panel::{ElasticPanel, Session},
    query_view::{DocumentDetail, document_json, render_document_detail},
    results::{self, LEVEL, MESSAGE, Rows, SERVICE, TIMESTAMP, TRACE_ID},
};
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ScrollStrategy,
    Subscription, Task, UniformListScrollHandle, WeakEntity, px, uniform_list,
};
use project::Project;
use std::{
    collections::VecDeque,
    ops::Range,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use ui::{Tooltip, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How far back each poll re-reads, for documents indexed after later ones.
const INGEST_LAG: chrono::Duration = chrono::Duration::seconds(30);
const BATCH: usize = 500;
const MAX_LINES: usize = 5_000;
/// Stops on its own after this long without the tab in front.
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// A template seen this many times, and not in the last 24 h, is announced as new.
const NEW_PATTERN_HITS: usize = 3;

#[derive(Clone)]
struct Line {
    id: String,
    timestamp: String,
    level: Option<String>,
    service: Option<String>,
    message: String,
    trace_id: Option<String>,
    document: serde_json::Value,
}

enum State {
    Starting,
    Following,
    Paused { reason: Option<SharedString> },
    Failed(SharedString),
}

struct NewPattern {
    template: String,
    example: String,
    since: String,
    hits: usize,
}

pub struct FollowView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    source: String,
    filter_input: Entity<InputField>,
    applied_filter: Option<String>,
    lines: VecDeque<Line>,
    seen: HashSet<String>,
    total: usize,
    /// Arrival times of the last minute, for the rate.
    arrivals: VecDeque<Instant>,
    newest: Option<DateTime<Utc>>,
    state: State,
    selected: Option<usize>,
    started: Instant,
    last_seen: Instant,
    last_poll: Instant,
    baseline: Option<HashSet<String>>,
    candidates: HashMap<String, (usize, String, String)>,
    silenced: HashSet<String>,
    new_pattern: Option<NewPattern>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    poll_task: Task<()>,
    baseline_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl FollowView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        panel: WeakEntity<ElasticPanel>,
        session: Arc<Session>,
        source: String,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "WHERE log.level IN (\"error\", \"warn\")")
                .start_icon(IconName::Filter)
        });
        if let Some(filter) = &filter {
            filter_input.update(cx, |input, cx| input.set_text(filter, window, cx));
        }
        let mut subscriptions = Vec::new();
        {
            let editor = filter_input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::Blurred {
                        this.update(cx, |this, cx| this.apply_filter(cx)).log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        let mut this = Self {
            workspace,
            project,
            panel,
            session,
            source,
            applied_filter: filter,
            filter_input,
            lines: VecDeque::new(),
            seen: HashSet::default(),
            total: 0,
            arrivals: VecDeque::new(),
            newest: None,
            state: State::Starting,
            selected: None,
            started: Instant::now(),
            last_seen: Instant::now(),
            last_poll: Instant::now(),
            baseline: None,
            candidates: HashMap::default(),
            silenced: HashSet::default(),
            new_pattern: None,
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            poll_task: Task::ready(()),
            baseline_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_baseline(cx);
        this.start(cx);
        this
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter(cx);
        if filter != self.applied_filter {
            self.applied_filter = filter;
            self.restart(cx);
        }
    }

    fn filter(&self, cx: &App) -> Option<String> {
        let text = self.filter_input.read(cx).text(cx);
        let text = text.trim();
        let text = text
            .strip_prefix("WHERE ")
            .or_else(|| text.strip_prefix("where "))
            .unwrap_or(text)
            .trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn query(&self, since: DateTime<Utc>) -> String {
        let mut query = format!(
            "FROM {} METADATA _id\n| WHERE @timestamp >= \"{}\"",
            self.source,
            esql::iso(since)
        );
        if let Some(filter) = &self.applied_filter {
            query.push_str(&format!("\n| WHERE {filter}"));
        }
        query.push_str(&format!("\n| SORT @timestamp DESC\n| LIMIT {BATCH}"));
        query
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.lines.clear();
        self.seen.clear();
        self.newest = None;
        self.selected = None;
        self.start(cx);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.state = State::Starting;
        self.last_seen = Instant::now();
        self.last_poll = Instant::now();
        cx.notify();
        self.poll_task = cx.spawn(async move |this, cx| {
            loop {
                let Ok((query, elastic)) = this.update(cx, |this, _| {
                    let since = match this.newest {
                        Some(newest) => newest - INGEST_LAG,
                        None => Utc::now() - chrono::Duration::minutes(2),
                    };
                    (this.query(since), this.session.elastic.clone())
                }) else {
                    return;
                };
                let result = elastic.esql(&query, None::<&TimeRange>).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        match result {
                            Ok(result) => this.receive(result.into(), cx),
                            Err(error) => {
                                this.state = State::Failed(error.to_string().into());
                                cx.notify();
                                return false;
                            }
                        }
                        let elapsed = this.last_poll.elapsed();
                        this.last_poll = Instant::now();
                        this.panel
                            .update(cx, |panel, cx| panel.record_following(elapsed, cx))
                            .log_err();
                        if this.last_seen.elapsed() > IDLE_STOP {
                            this.state = State::Paused {
                                reason: Some(
                                    "parou sozinho depois de 10 min com a aba fora de foco".into(),
                                ),
                            };
                            cx.notify();
                            return false;
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        });
    }

    fn toggle_pause(&mut self, _: &TogglePause, _window: &mut Window, cx: &mut Context<Self>) {
        match self.state {
            State::Following | State::Starting => {
                self.poll_task = Task::ready(());
                self.state = State::Paused { reason: None };
                cx.notify();
            }
            State::Paused { .. } | State::Failed(_) => self.start(cx),
        }
    }

    fn receive(&mut self, rows: Rows, cx: &mut Context<Self>) {
        self.state = State::Following;
        let mut fresh = Vec::new();
        // The batch comes newest first, so a full batch keeps the latest documents.
        for row in (0..rows.values.len()).rev() {
            let Some(id) = rows.text(row, "_id") else {
                continue;
            };
            if !self.seen.insert(id.clone()) {
                continue;
            }
            let timestamp = rows.text(row, TIMESTAMP).unwrap_or_default();
            if let Ok(time) = DateTime::parse_from_rfc3339(&timestamp) {
                let time = time.with_timezone(&Utc);
                if self.newest.is_none_or(|newest| time > newest) {
                    self.newest = Some(time);
                }
            }
            fresh.push(Line {
                id,
                timestamp,
                level: rows.text(row, LEVEL),
                service: rows.text(row, SERVICE),
                message: rows
                    .text(row, MESSAGE)
                    .or_else(|| rows.text(row, "error.message"))
                    .unwrap_or_default(),
                trace_id: rows.text(row, TRACE_ID),
                document: document_json(&rows, row),
            });
        }
        fresh.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let now = Instant::now();
        for line in fresh {
            self.total += 1;
            self.arrivals.push_back(now);
            self.watch_pattern(&line);
            self.lines.push_back(line);
        }
        while self.lines.len() > MAX_LINES {
            if let Some(line) = self.lines.pop_front() {
                self.seen.remove(&line.id);
            }
            self.selected = self.selected.and_then(|selected| selected.checked_sub(1));
        }
        while self
            .arrivals
            .front()
            .is_some_and(|arrival| arrival.elapsed() > Duration::from_secs(60))
        {
            self.arrivals.pop_front();
        }
        if self.selected.is_none() && !self.lines.is_empty() {
            self.scroll_handle
                .scroll_to_item(self.lines.len() - 1, ScrollStrategy::Bottom);
        }
        cx.notify();
    }

    /// Reads which message templates the last 24 h already had, so only really new ones are
    /// announced.
    fn load_baseline(&mut self, cx: &mut Context<Self>) {
        let query = format!(
            "FROM {} | WHERE @timestamp >= NOW() - 24 hours AND @timestamp < NOW() - 10 minutes \
             | STATS docs = COUNT(*) BY message | SORT docs DESC | LIMIT 5000",
            self.source
        );
        let elastic = self.session.elastic.clone();
        self.baseline_task = cx.spawn(async move |this, cx| {
            let result = elastic.esql(&query, None).await;
            this.update(cx, |this, _| {
                let Some(result) = result.log_err() else {
                    return;
                };
                let rows = Rows::from(result);
                this.baseline = Some(
                    (0..rows.values.len())
                        .filter_map(|row| rows.text(row, MESSAGE))
                        .map(|message| template(&message))
                        .collect(),
                );
            })
            .log_err();
        });
    }

    fn watch_pattern(&mut self, line: &Line) {
        let Some(baseline) = &self.baseline else {
            return;
        };
        if line.message.is_empty() {
            return;
        }
        let template = template(&line.message);
        if baseline.contains(&template) || self.silenced.contains(&template) {
            return;
        }
        let entry = self.candidates.entry(template.clone()).or_insert_with(|| {
            (
                0,
                line.message.clone(),
                results::short_time(&line.timestamp),
            )
        });
        entry.0 += 1;
        if entry.0 >= NEW_PATTERN_HITS
            && self
                .new_pattern
                .as_ref()
                .is_none_or(|pattern| pattern.template != template)
        {
            self.new_pattern = Some(NewPattern {
                template,
                example: entry.1.clone(),
                since: entry.2.clone(),
                hits: entry.0,
            });
        } else if let Some(pattern) = self
            .new_pattern
            .as_mut()
            .filter(|pattern| pattern.template == template)
        {
            pattern.hits = entry.0;
        }
    }

    fn lines_as_text(&self, count: usize) -> String {
        self.lines
            .iter()
            .rev()
            .take(count)
            .rev()
            .map(|line| {
                format!(
                    "{} {} {} {}",
                    line.timestamp,
                    line.level.as_deref().unwrap_or("-"),
                    line.service.as_deref().unwrap_or("-"),
                    line.message.replace('\n', " ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn send_to_agent(&self, prompt: &str, window: &mut Window, cx: &mut App) {
        let text = format!(
            "Últimas linhas seguidas em {} ({}):\n{}",
            self.source,
            self.session.describe(),
            self.lines_as_text(200)
        );
        let id = crate::agent_toolkit::remember_text(text, cx);
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id,
                title: format!("seguindo {}", self.source),
                prompt: Some(prompt.to_string()),
                submit: true,
            }),
            cx,
        );
    }

    fn save_as_log(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self
            .panel
            .upgrade()
            .and_then(|panel| panel.read(cx).root().map(|root| root.to_path_buf()))
        else {
            return;
        };
        let fs = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().fs.clone());
        let Some(fs) = fs else {
            return;
        };
        let text = self.lines_as_text(MAX_LINES);
        let name = format!(
            "{}-{}.log",
            self.source.replace(['*', ',', ' '], "_"),
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let directory = root.join(".asylum/elastic/tails");
            fs.create_dir(&directory).await?;
            let path = directory.join(name);
            fs.atomic_write(path.clone(), text).await?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(path, Default::default(), window, cx)
                })?
                .await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_line(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(line) = self.lines.get(index) else {
            return div().into_any_element();
        };
        let error = line
            .level
            .as_deref()
            .is_some_and(|level| results::level_color(level) == Color::Error);
        let selected = self.selected == Some(index);
        h_flex()
            .id(("elastic-follow-line", index))
            .h(px(22.))
            .px_3()
            .gap_2()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .when(!selected && error, |this| {
                this.bg(Color::Error.color(cx).opacity(0.06))
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                div().w(px(92.)).flex_none().child(
                    Label::new(results::short_time(&line.timestamp))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Disabled),
                ),
            )
            .when_some(line.service.clone(), |this, service| {
                this.child(
                    div()
                        .w(px(96.))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(service)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Accent)
                                .truncate(),
                        ),
                )
            })
            .child(
                div().w(px(48.)).flex_none().child(
                    Label::new(line.level.clone().unwrap_or_default().to_ascii_uppercase())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(
                            line.level
                                .as_deref()
                                .map_or(Color::Muted, results::level_color),
                        ),
                ),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(line.message.replace('\n', " "))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .truncate(),
                ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = if this.selected == Some(index) {
                    None
                } else {
                    Some(index)
                };
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_new_pattern(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pattern = self.new_pattern.as_ref()?;
        let template = pattern.template.clone();
        let example = pattern.example.clone();
        Some(
            v_flex()
                .mx_3()
                .mb_2()
                .p_2p5()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(Color::Accent.color(cx).opacity(0.5))
                .bg(Color::Accent.color(cx).opacity(0.06))
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Sparkle)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(format!("Padrão novo desde {}", pattern.since))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                )
                .child(
                    Label::new(format!(
                        "\"{}\" não apareceu nas últimas 24 h e já veio {} vezes nesta sessão.",
                        example.chars().take(160).collect::<String>(),
                        pattern.hits
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("elastic-pattern-investigate", "Investigar no chat")
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Chat).size(IconSize::Small))
                                .on_click(cx.listener({
                                    let example = example.clone();
                                    move |this, _, window, cx| {
                                        this.send_to_agent(
                                            &format!(
                                                "Apareceu um padrão novo nos logs: \"{example}\". \
                                                 Descubra de onde vem, desde quando e se tem a ver \
                                                 com os erros ao redor."
                                            ),
                                            window,
                                            cx,
                                        )
                                    }
                                })),
                        )
                        .child(
                            Button::new("elastic-pattern-query", "Montar consulta")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::FileCode).size(IconSize::Small))
                                .on_click(cx.listener({
                                    move |this, _, window, cx| {
                                        let words: Vec<&str> = example
                                            .split_whitespace()
                                            .filter(|word| {
                                                word.chars().all(|character| {
                                                    character.is_alphabetic()
                                                        || matches!(character, ':' | '.')
                                                })
                                            })
                                            .take(4)
                                            .collect();
                                        let like = format!("*{}*", words.join("*"));
                                        let body = format!(
                                            "FROM {}\n| WHERE message LIKE {}\n| SORT @timestamp DESC\n| LIMIT 200",
                                            this.source,
                                            esql::string_literal(&like)
                                        );
                                        this.panel
                                            .update(cx, |panel, cx| {
                                                panel.new_query(Some(body), true, window, cx)
                                            })
                                            .log_err();
                                    }
                                })),
                        )
                        .child(
                            Button::new("elastic-pattern-silence", "Silenciar este padrão")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.silenced.insert(template.clone());
                                    this.new_pattern = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

static TEMPLATE_PARTS: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
    [
        (
            r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            "<uuid>",
        ),
        (r"\b[0-9a-fA-F]{12,}\b", "<hex>"),
        (r"\b[A-Za-z]+_[A-Za-z0-9]{4,}\b", "<id>"),
        (r"\d+([.,]\d+)?", "<n>"),
    ]
    .into_iter()
    .filter_map(|(pattern, replacement)| {
        regex::Regex::new(pattern)
            .log_err()
            .map(|pattern| (pattern, replacement))
    })
    .collect()
});

/// The shape of a message with ids and numbers taken out, so repeats of one event group.
fn template(message: &str) -> String {
    let mut template = message.lines().next().unwrap_or_default().to_string();
    for (pattern, replacement) in TEMPLATE_PARTS.iter() {
        template = pattern.replace_all(&template, *replacement).into_owned();
    }
    template
}

impl Render for FollowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_handle.contains_focused(window, cx) {
            self.last_seen = Instant::now();
        }
        let (dot, status) = match &self.state {
            State::Starting => (Color::Muted, "conectando…".to_string()),
            State::Following => (
                Color::Error,
                format!(
                    "a cada {} s · {} docs/min",
                    POLL_INTERVAL.as_secs(),
                    self.arrivals.len()
                ),
            ),
            State::Paused { .. } => (Color::Warning, "pausado".to_string()),
            State::Failed(_) => (Color::Error, "falhou".to_string()),
        };
        let paused = matches!(self.state, State::Paused { .. } | State::Failed(_));
        let line_count = self.lines.len();
        let running_for = self.started.elapsed().as_secs();
        let subtitle = match &self.state {
            State::Paused {
                reason: Some(reason),
            } => reason.to_string(),
            State::Failed(error) => error.to_string(),
            _ => format!(
                "Seguindo há {} min {} s · consulta a cada {} s, relendo os últimos 30 s por \
                 atraso de ingestão · para sozinho após 10 min com a aba fora de foco",
                running_for / 60,
                running_for % 60,
                POLL_INTERVAL.as_secs()
            ),
        };
        let selected = self
            .selected
            .and_then(|index| self.lines.get(index))
            .map(|line| {
                let fields = line
                    .document
                    .as_object()
                    .map(|object| {
                        object
                            .iter()
                            .filter(|(name, _)| {
                                ![TIMESTAMP, LEVEL, MESSAGE, "_id"].contains(&name.as_str())
                                    && !results::STACK_FIELDS.contains(&name.as_str())
                            })
                            .take(12)
                            .map(|(name, value)| (name.clone(), results::display(value)))
                            .collect()
                    })
                    .unwrap_or_default();
                let stack = results::STACK_FIELDS.iter().find_map(|field| {
                    line.document
                        .get(*field)
                        .filter(|value| !value.is_null())
                        .map(results::display)
                });
                let first_link = stack
                    .as_deref()
                    .and_then(|stack| stack.lines().find_map(results::stack_link));
                DocumentDetail {
                    id: "elastic-follow-document",
                    fields,
                    stack,
                    first_link,
                    trace_id: line.trace_id.clone(),
                    message: Some(line.message.clone()),
                    document: line.document.clone(),
                }
            });
        v_flex()
            .key_context("ElasticFollowView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_pause))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(IconName::CloudPulse)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(self.source.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div()
                            .key_context("ElasticFollowFilter")
                            .on_action(cx.listener(|this, _: &ApplyFollowFilter, _, cx| {
                                this.apply_filter(cx)
                            }))
                            .w(px(360.))
                            .child(self.filter_input.clone()),
                    )
                    .child(
                        h_flex()
                            .h(px(22.))
                            .px_2()
                            .gap_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .child(ui::Indicator::dot().color(dot))
                            .child(Label::new(status).size(LabelSize::XSmall).color(dot)),
                    )
                    .when(self.baseline.is_some(), |this| {
                        this.child(
                            h_flex()
                                .id("elastic-watching")
                                .h(px(22.))
                                .px_2()
                                .gap_1()
                                .rounded_md()
                                .bg(Color::Accent.color(cx).opacity(0.1))
                                .child(
                                    Icon::new(IconName::Sparkle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new("vigiando padrões")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .tooltip(Tooltip::text(
                                    "Compara o que chega com as mensagens das últimas 24 h e avisa \
                                     quando aparece um padrão novo. Roda aqui, sem o modelo.",
                                )),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new(
                            "elastic-follow-pause",
                            if paused { "Retomar" } else { "Pausar" },
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(if paused {
                                IconName::PlayFilled
                            } else {
                                IconName::DebugPause
                            })
                            .size(IconSize::Small),
                        )
                        .key_binding(ui::KeyBinding::for_action_in(
                            &TogglePause,
                            &self.focus_handle,
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_pause(&TogglePause, window, cx)
                        })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .child(Label::new(subtitle).size(LabelSize::XSmall).color(
                        if matches!(self.state, State::Failed(_)) {
                            Color::Error
                        } else {
                            Color::Disabled
                        },
                    )),
            )
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list(
                        "elastic-follow-lines",
                        line_count,
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            range.map(|index| this.render_line(index, cx)).collect()
                        }),
                    )
                    .track_scroll(&self.scroll_handle)
                    .size_full(),
                ),
            )
            .when_some(selected, |this, detail| {
                this.child(render_document_detail(
                    detail,
                    self.workspace.clone(),
                    self.project.clone(),
                    self.panel.clone(),
                    cx,
                ))
            })
            .children(self.render_new_pattern(cx))
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(ui::Indicator::dot().color(dot))
                    .child(
                        Label::new(format!(
                            "{} · {} docs desde o início, {} na memória (máx. {})",
                            if paused { "Pausado" } else { "Seguindo o fim" },
                            results::format_count(self.total as u64),
                            results::format_count(line_count as u64),
                            results::format_count(MAX_LINES as u64),
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("elastic-follow-save", "Salvar como .log")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Download).size(IconSize::Small))
                            .disabled(line_count == 0)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_as_log(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("elastic-follow-agent", "Mandar para o Agent")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                            .disabled(line_count == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_to_agent(
                                    "Resuma o que está acontecendo nestes logs e aponte o que \
                                     merece atenção.",
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
    }
}

impl Focusable for FollowView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for FollowView {}

impl Item for FollowView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match self.applied_filter.as_deref().and_then(filtered_app) {
            Some(app) => format!("seguindo · {app}").into(),
            None => format!("seguindo · {}", short_source(&self.source)).into(),
        }
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} · {}", self.source, self.session.connection.name).into())
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

/// The app a follow filter narrows to, when it is just `service.name == "…"`.
fn filtered_app(filter: &str) -> Option<&str> {
    filter
        .strip_prefix("service.name == \"")?
        .strip_suffix('"')
        .filter(|app| !app.contains('"'))
}

/// `logs-trix-api-prod` → `trix-api-prod`, for the tab.
fn short_source(source: &str) -> &str {
    source
        .split_once('-')
        .map(|(_, rest)| rest)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(source)
}

/// Opens a follow tab, or focuses the one already following the same source.
#[allow(clippy::too_many_arguments)]
pub fn open(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    source: String,
    filter: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace_handle = workspace.clone();
    workspace
        .update(cx, |workspace, cx| {
            let existing = workspace.items_of_type::<FollowView>(cx).find(|view| {
                let view = view.read(cx);
                view.source == source && view.applied_filter == filter
            });
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let view = cx.new(|cx| {
                FollowView::new(
                    workspace_handle,
                    project,
                    panel,
                    session,
                    source,
                    filter,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .log_err();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_group_repeats_of_one_event() {
        assert_eq!(
            template("Retentativa 1/3 no adquirente: ECONNRESET req_T81sLm"),
            template("Retentativa 2/3 no adquirente: ECONNRESET req_Vn30Ka")
        );
        assert_ne!(
            template("Webhook fora de ordem: pagamento.estornado antes de capturado pay_0Rk2"),
            template("Adquirente respondeu 502 ao capturar o pagamento req_T81sLm")
        );
        assert_eq!(short_source("logs-trix-api-prod"), "trix-api-prod");
    }
}
