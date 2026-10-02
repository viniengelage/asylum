//! A `.esql` file from the project, open against a connection: the file is an ordinary editor
//! (saved like any other), with what the query under the cursor returned underneath. The time
//! window is a filter on `@timestamp` sent next to the query, so the text never has to say it.

use crate::{
    CancelQuery, RunQuery,
    client::{ElasticError, EsqlResult, TimeRange},
    completion::{EsqlCompletionProvider, FieldCache},
    esql,
    panel::{ElasticPanel, Session, environment_chip},
    results::{self, LOG_COLUMNS, MESSAGE, Rows, TIMESTAMP, TRACE_ID},
    trace_view,
};
use chrono::{DateTime, Utc};
use editor::{Editor, EditorEvent, HighlightKey, MultiBufferOffset, SelectionEffects};
use gpui::{
    AnyElement, AnyWindowHandle, ClipboardItem, DragMoveEvent, Empty, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, FontWeight, Global, Pixels, Subscription, Task,
    WeakEntity, px, relative, uniform_list,
};
use project::Project;
use std::{
    cell::RefCell,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, SaveOptions, TabContentParams},
};

const MAX_HISTORY: usize = 50;
const HISTOGRAM_BUCKETS: usize = 40;
const MIN_RESULTS_HEIGHT: Pixels = px(200.);
const MIN_EDITOR_HEIGHT: Pixels = px(80.);
const MAX_TABLE_COLUMNS: usize = 8;
/// How much of the editor and of the result goes to the agent when the tab is mentioned.
const MAX_AGENT_QUERY_CHARS: usize = 20_000;
const MAX_AGENT_ROWS: usize = 20;

struct DraggedResultsDivider;

/// Every open query tab, so the agent (which only knows its project) can list them and write
/// into one.
#[derive(Default)]
struct OpenQueryViews(Vec<WeakEntity<EsqlQueryView>>);

impl Global for OpenQueryViews {}

pub(crate) fn query_views(project: &Entity<Project>, cx: &App) -> Vec<Entity<EsqlQueryView>> {
    let Some(open) = cx.try_global::<OpenQueryViews>() else {
        return Vec::new();
    };
    open.0
        .iter()
        .rev()
        .filter_map(|view| view.upgrade())
        .filter(|view| view.read(cx).project_id == project.entity_id())
        .collect()
}

/// The windows the toolbar offers, from the most used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeWindow {
    Minutes(i64),
    Hours(i64),
    Days(i64),
}

impl TimeWindow {
    pub const PRESETS: [TimeWindow; 7] = [
        TimeWindow::Minutes(15),
        TimeWindow::Minutes(30),
        TimeWindow::Hours(1),
        TimeWindow::Hours(4),
        TimeWindow::Hours(12),
        TimeWindow::Hours(24),
        TimeWindow::Days(7),
    ];

    pub fn duration(self) -> chrono::Duration {
        match self {
            TimeWindow::Minutes(minutes) => chrono::Duration::minutes(minutes),
            TimeWindow::Hours(hours) => chrono::Duration::hours(hours),
            TimeWindow::Days(days) => chrono::Duration::days(days),
        }
    }

    pub fn label(self) -> String {
        match self {
            TimeWindow::Minutes(minutes) => format!("últimos {minutes} min"),
            TimeWindow::Hours(1) => "última hora".to_string(),
            TimeWindow::Hours(hours) => format!("últimas {hours} h"),
            TimeWindow::Days(days) => format!("últimos {days} dias"),
        }
    }

    /// The smallest preset that covers `minutes`, or the widest one.
    pub fn covering(minutes: u32) -> Self {
        let minutes = i64::from(minutes);
        Self::PRESETS
            .into_iter()
            .find(|preset| preset.duration().num_minutes() >= minutes)
            .unwrap_or(TimeWindow::Days(7))
    }

    /// Kibana's `now-30m`.
    pub fn kibana(self) -> String {
        match self {
            TimeWindow::Minutes(minutes) => format!("now-{minutes}m"),
            TimeWindow::Hours(hours) => format!("now-{hours}h"),
            TimeWindow::Days(days) => format!("now-{days}d"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultTab {
    Documents,
    History,
}

struct HistoryEntry {
    query: String,
    summary: String,
    failed: bool,
}

enum RunState {
    Idle,
    Running {
        started: Instant,
    },
    Done {
        took: Option<u64>,
        elapsed: Duration,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        finished: Instant,
    },
    Failed(Failure),
}

struct Failure {
    message: SharedString,
    kind: Option<String>,
    /// Buffer range the error points at.
    range: Option<Range<usize>>,
    line: Option<usize>,
    replacement: Option<String>,
}

pub struct EsqlQueryView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    project_id: EntityId,
    window: AnyWindowHandle,
    editor: Entity<Editor>,
    path: PathBuf,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    time_window: TimeWindow,
    state: RunState,
    rows: Rows,
    /// The query that produced `rows`, for "Abrir no Kibana" and the agent.
    last_query: Option<String>,
    histogram: Vec<(String, u64)>,
    selected: Option<usize>,
    tab: ResultTab,
    history: Vec<HistoryEntry>,
    results_height: Option<Pixels>,
    focus_handle: FocusHandle,
    run_task: Task<()>,
    tick_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EsqlQueryView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        buffer: Entity<language::Buffer>,
        path: PathBuf,
        panel: WeakEntity<ElasticPanel>,
        session: Arc<Session>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cache = Rc::new(RefCell::new(FieldCache::new(
            session
                .streams
                .iter()
                .map(|stream| stream.name.clone())
                .collect(),
        )));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, Some(project.clone()), window, cx);
            editor.set_completion_provider(Some(Rc::new(EsqlCompletionProvider {
                cache,
                elastic: session.elastic.clone(),
            })));
            editor
        });
        let this = cx.weak_entity();
        let open = cx.default_global::<OpenQueryViews>();
        open.0.retain(|view| view.upgrade().is_some());
        open.0.push(this);
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event, cx| match event {
            EditorEvent::DirtyChanged
            | EditorEvent::Saved
            | EditorEvent::TitleChanged
            | EditorEvent::FileHandleChanged => cx.emit(ItemEvent::UpdateTab),
            EditorEvent::Edited { .. } => {
                // The error's position no longer points at the same text.
                if matches!(this.state, RunState::Failed(_)) {
                    this.clear_highlight(HighlightKey::DatabaseError, cx);
                }
                cx.emit(ItemEvent::Edit);
            }
            _ => {}
        })];
        Self {
            workspace,
            project_id: project.entity_id(),
            project,
            window: window.window_handle(),
            editor,
            path,
            panel,
            session,
            time_window: TimeWindow::Minutes(30),
            state: RunState::Idle,
            rows: Rows::default(),
            last_query: None,
            histogram: Vec::new(),
            selected: None,
            tab: ResultTab::Documents,
            history: Vec::new(),
            results_height: None,
            focus_handle: cx.focus_handle(),
            run_task: Task::ready(()),
            tick_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub(crate) fn window(&self) -> AnyWindowHandle {
        self.window
    }

    fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running { .. })
    }

    pub(crate) fn set_time_window(&mut self, time_window: TimeWindow, cx: &mut Context<Self>) {
        self.time_window = time_window;
        cx.notify();
    }

    /// What the agent gets when this tab is mentioned: the file, the cursor and a masked
    /// sample of the last result.
    pub(crate) fn describe_for_agent(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let mut text = editor.text(cx);
        if text.chars().count() > MAX_AGENT_QUERY_CHARS {
            text = format!(
                "{}\n// … (cut)",
                text.chars().take(MAX_AGENT_QUERY_CHARS).collect::<String>()
            );
        }
        let path = self.path.display();
        let mut description = format!(
            "ES|QL editor `{path}`, open against the Elasticsearch connection {connection}. \
             The time window is {window}, sent as a filter on @timestamp (the query itself \
             doesn't need one).\n\n\
             When the user asks about the logs (how many, which errors, why), run the query \
             yourself with `elastic_esql` and answer the fact in the chat. When they ask for a \
             query in this editor (write, change, fix), write it with `elastic_write_query` \
             (path `{path}`) and end the turn without text. Use `elastic_fields` first when you \
             don't know the field names; the logs follow ECS (log.level, message, service.name, \
             trace.id).\n\nCurrent contents:\n```esql\n{text}\n```",
            connection = self.session.describe(),
            window = self.time_window.label(),
        );
        if let Some(query) = &self.last_query
            && !self.rows.values.is_empty()
        {
            description.push_str(&format!(
                "\n\nLast result ({} rows, first {} shown, personal data masked) of:\n```esql\n{query}\n```\n{}",
                self.rows.values.len(),
                self.rows.values.len().min(MAX_AGENT_ROWS),
                crate::agent_toolkit::rows_as_text(&self.rows, MAX_AGENT_ROWS)
            ));
        }
        description
    }

    /// Puts the agent's query into the editor as one undoable edit, in place of the query under
    /// the cursor or after the last one. Returns the line it starts on.
    pub(crate) fn write_from_agent(
        &mut self,
        query: &str,
        replace: bool,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let query = query.trim();
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let cursor = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
                .head()
                .0;
            let blocks = esql::blocks(&text);
            let (range, new_text, start) = match esql::block_at(&blocks, cursor).filter(|_| replace)
            {
                Some(block) => (block.range.clone(), query.to_string(), block.range.start),
                None if text.trim().is_empty() => (0..text.len(), format!("{query}\n"), 0),
                None => {
                    let separator = if text.ends_with("\n\n") {
                        ""
                    } else if text.ends_with('\n') {
                        "\n"
                    } else {
                        "\n\n"
                    };
                    (
                        text.len()..text.len(),
                        format!("{separator}{query}\n"),
                        text.len() + separator.len(),
                    )
                }
            };
            editor.edit(
                [(
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    new_text,
                )],
                cx,
            );
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(start)..MultiBufferOffset(start)])
            });
            editor.text(cx)[..start].matches('\n').count() + 1
        })
    }

    fn clear_highlight(&mut self, key: HighlightKey, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(key, cx);
        });
    }

    fn highlight(&mut self, key: HighlightKey, range: Range<usize>, cx: &mut Context<Self>) {
        let error = key == HighlightKey::DatabaseError;
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchors = [snapshot.anchor_before(MultiBufferOffset(range.start))
                ..snapshot.anchor_after(MultiBufferOffset(range.end))];
            editor.highlight_background(
                key,
                &anchors,
                move |_, theme| {
                    if error {
                        theme.status().error_background
                    } else {
                        theme.colors().editor_active_line_background
                    }
                },
                cx,
            );
        });
    }

    /// The query to run: the selection when there is one, else the block under the cursor.
    fn target(&self, cx: &mut Context<Self>) -> Option<(String, Range<usize>)> {
        let (text, start, end) = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let selection = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
            (text, selection.start.0, selection.end.0)
        });
        if start != end {
            let range = start.min(text.len())..end.min(text.len());
            return Some((text[range.clone()].to_string(), range));
        }
        let blocks = esql::blocks(&text);
        let block = esql::block_at(&blocks, start)?;
        Some((text[block.range.clone()].to_string(), block.range.clone()))
    }

    pub(crate) fn run(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let Some((query, range)) = self.target(cx) else {
            self.state = RunState::Failed(Failure {
                message: "Nada para rodar: escreva uma consulta começando por FROM.".into(),
                kind: None,
                range: None,
                line: None,
                replacement: None,
            });
            cx.notify();
            return;
        };
        window.focus(&self.editor.focus_handle(cx), cx);
        self.clear_highlight(HighlightKey::DatabaseError, cx);
        self.highlight(HighlightKey::DatabaseStatement, range.clone(), cx);
        self.state = RunState::Running {
            started: Instant::now(),
        };
        self.tab = ResultTab::Documents;
        cx.notify();
        self.tick_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.is_running()
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        });
        let to = Utc::now();
        let from = to - self.time_window.duration();
        let time_range = TimeRange {
            from: esql::iso(from),
            to: esql::iso(to),
        };
        let elastic = self.session.elastic.clone();
        let histogram = esql::histogram_query(&query, from, to, HISTOGRAM_BUCKETS);
        self.run_task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let main = elastic.esql(&query, Some(&time_range));
            let bars = async {
                match &histogram {
                    Some(histogram) => elastic.esql(histogram, Some(&time_range)).await.log_err(),
                    None => None,
                }
            };
            let (result, bars) = futures::join!(main, bars);
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.finish(query, range, result, bars, from, to, elapsed, cx)
            })
            .log_err();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        query: String,
        range: Range<usize>,
        result: Result<EsqlResult, ElasticError>,
        bars: Option<EsqlResult>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        self.clear_highlight(HighlightKey::DatabaseStatement, cx);
        match result {
            Ok(result) => {
                let took = result.took;
                let count = result.values.len();
                self.rows = result.into();
                self.selected = None;
                self.histogram = bars
                    .map(|bars| {
                        bars.values
                            .iter()
                            .filter_map(|row| {
                                let count = row.first()?.as_u64()?;
                                let bucket = row.get(1)?.as_str()?.to_string();
                                Some((bucket, count))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.last_query = Some(query.clone());
                self.history.insert(
                    0,
                    HistoryEntry {
                        query,
                        summary: format!("{count} linhas · {} ms", elapsed.as_millis()),
                        failed: false,
                    },
                );
                self.state = RunState::Done {
                    took,
                    elapsed,
                    from,
                    to,
                    finished: Instant::now(),
                };
                let documents = self.histogram.iter().map(|(_, count)| *count).sum::<u64>();
                self.panel
                    .update(cx, |panel, cx| {
                        panel.record_query(documents.max(count as u64), cx)
                    })
                    .log_err();
            }
            Err(error) => {
                let text = self.editor.read(cx).text(cx);
                let (kind, message) = match &error {
                    ElasticError::BadRequest { kind, message } => {
                        (Some(kind.clone()), message.clone())
                    }
                    other => (None, other.to_string()),
                };
                let location = esql::error_location(&message);
                let error_range = location.as_ref().and_then(|location| {
                    let query_text = text.get(range.clone())?;
                    let offset =
                        range.start + esql::offset_of(query_text, location.line, location.column)?;
                    let length = location
                        .replacement
                        .as_ref()
                        .map_or(1, |(unknown, _)| unknown.len());
                    Some(offset..(offset + length).min(text.len()))
                });
                let line = error_range
                    .as_ref()
                    .map(|range| text[..range.start].matches('\n').count() + 1);
                if let Some(error_range) = error_range.clone() {
                    self.highlight(HighlightKey::DatabaseError, error_range, cx);
                }
                let shown: SharedString = location
                    .as_ref()
                    .map(|location| location.message.clone())
                    .unwrap_or(message)
                    .into();
                self.history.insert(
                    0,
                    HistoryEntry {
                        query,
                        summary: shown.to_string(),
                        failed: true,
                    },
                );
                self.state = RunState::Failed(Failure {
                    message: shown,
                    kind,
                    range: error_range,
                    line,
                    replacement: location
                        .and_then(|location| location.replacement)
                        .map(|(_, suggestion)| suggestion),
                });
            }
        }
        self.history.truncate(MAX_HISTORY);
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelQuery, _window: &mut gpui::Window, cx: &mut Context<Self>) {
        if !self.is_running() {
            return;
        }
        // Dropping the request stops waiting for it; `_query` has no id to cancel on the server.
        self.run_task = Task::ready(());
        self.clear_highlight(HighlightKey::DatabaseStatement, cx);
        self.state = RunState::Idle;
        cx.notify();
    }

    fn apply_replacement(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let RunState::Failed(Failure {
            range: Some(range),
            replacement: Some(replacement),
            ..
        }) = &self.state
        else {
            return;
        };
        let (range, replacement) = (range.clone(), replacement.clone());
        self.editor.update(cx, |editor, cx| {
            editor.edit(
                [(
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    replacement,
                )],
                cx,
            );
        });
        self.state = RunState::Idle;
        self.clear_highlight(HighlightKey::DatabaseError, cx);
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn go_to_error(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let RunState::Failed(Failure {
            range: Some(range), ..
        }) = &self.state
        else {
            return;
        };
        let range = range.clone();
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections
                    .select_ranges([MultiBufferOffset(range.start)..MultiBufferOffset(range.end)])
            });
        });
        window.focus(&self.editor.focus_handle(cx), cx);
    }

    fn ask_agent(&self, prompt: &str, window: &mut gpui::Window, cx: &mut App) {
        let title = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "consulta".to_string());
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id: crate::agent_toolkit::query_mention_id(&self.path),
                title,
                prompt: Some(prompt.to_string()),
                submit: true,
            }),
            cx,
        );
    }

    fn open_in_kibana(&self, cx: &mut App) {
        let Some(query) = &self.last_query else {
            return;
        };
        if let Some(url) =
            crate::kibana_discover_url(&self.session.connection, query, &self.time_window.kibana())
        {
            cx.open_url(&url);
        }
    }

    fn copy_csv(&self, cx: &mut App) {
        let mut csv = self
            .rows
            .columns
            .iter()
            .map(|column| csv_field(&column.name))
            .collect::<Vec<_>>()
            .join(",");
        csv.push('\n');
        for row in &self.rows.values {
            csv.push_str(
                &row.iter()
                    .map(|value| csv_field(&results::display(value)))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            csv.push('\n');
        }
        cx.write_to_clipboard(ClipboardItem::new_string(csv));
    }

    fn copy_json(&self, cx: &mut App) {
        let documents: Vec<serde_json::Value> = (0..self.rows.values.len())
            .map(|row| document_json(&self.rows, row))
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&documents) {
            cx.write_to_clipboard(ClipboardItem::new_string(json));
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.is_running();
        let this = cx.weak_entity();
        let current = self.time_window;
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
                Label::new(self.session.connection.name.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(environment_chip(self.session.connection.environment, cx))
            .child(
                PopoverMenu::new("elastic-time-window")
                    .trigger(
                        Button::new("elastic-time-window-trigger", current.label())
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Clock).size(IconSize::Small))
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for preset in TimeWindow::PRESETS {
                                let this = this.clone();
                                menu = menu.toggleable_entry(
                                    preset.label(),
                                    preset == current,
                                    ui::IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        this.update(cx, |this, cx| {
                                            this.time_window = preset;
                                            cx.notify();
                                        })
                                        .log_err();
                                    },
                                );
                            }
                            menu
                        }))
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("elastic-ask-agent", "Pedir ao Agent")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.ask_agent(
                            "Olhe esta consulta e o último resultado. Explique o que está \
                             acontecendo e, se a consulta puder responder melhor, reescreva-a \
                             no editor.",
                            window,
                            cx,
                        )
                    })),
            )
            .child(if running {
                Button::new("elastic-cancel-query", "Cancelar")
                    .style(ButtonStyle::Tinted(ui::TintColor::Error))
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Stop).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &CancelQuery,
                        &self.editor.focus_handle(cx),
                        cx,
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                    .into_any_element()
            } else {
                Button::new("elastic-run", "Executar")
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &RunQuery,
                        &self.editor.focus_handle(cx),
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.run(window, cx)))
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_status(&self) -> AnyElement {
        let (icon, text, color): (Option<IconName>, String, Color) = match &self.state {
            RunState::Idle => (
                None,
                "⌘↵ roda a consulta sob o cursor (ou a seleção) · a janela de tempo vai à parte"
                    .to_string(),
                Color::Muted,
            ),
            RunState::Running { started } => (
                Some(IconName::LoadCircle),
                format!("rodando · {:.1} s", started.elapsed().as_secs_f32()),
                Color::Warning,
            ),
            RunState::Done {
                took,
                elapsed,
                finished,
                ..
            } => {
                let documents = self.histogram.iter().map(|(_, count)| *count).sum::<u64>();
                let ago = finished.elapsed().as_secs();
                (
                    Some(IconName::Check),
                    format!(
                        "{} linhas{} · {}{} · rodou há {}",
                        self.rows.values.len(),
                        if documents > 0 {
                            format!(" de {} docs na janela", results::format_count(documents))
                        } else {
                            String::new()
                        },
                        match took {
                            Some(took) => format!("took {took} ms"),
                            None => format!("{} ms", elapsed.as_millis()),
                        },
                        match self.session.connection.via {
                            crate::connection::Via::Kibana => " via Kibana",
                            crate::connection::Via::Direct => "",
                        },
                        if ago < 60 {
                            format!("{ago} s")
                        } else {
                            format!("{} min", ago / 60)
                        }
                    ),
                    Color::Success,
                )
            }
            RunState::Failed(failure) => (
                Some(IconName::XCircle),
                failure
                    .kind
                    .clone()
                    .unwrap_or_else(|| "a consulta falhou".to_string()),
                Color::Error,
            ),
        };
        h_flex()
            .gap_1p5()
            .min_w_0()
            .children(icon.map(|icon| Icon::new(icon).size(IconSize::Small).color(color)))
            .child(
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(color)
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_failure(&self, failure: &Failure, cx: &mut Context<Self>) -> AnyElement {
        let has_range = failure.range.is_some();
        v_flex()
            .m_3()
            .p_3()
            .gap_2()
            .rounded_md()
            .bg(Color::Error.color(cx).opacity(0.06))
            .border_1()
            .border_color(Color::Error.color(cx).opacity(0.3))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(failure.message.clone())
                                .buffer_font(cx)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                    ),
            )
            .when_some(failure.replacement.clone(), |this, replacement| {
                this.child(
                    Label::new(format!(
                        "O Elasticsearch sugere {replacement}. Nesses data streams os campos \
                         seguem o ECS; o resto da linha pode ficar igual."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .when_some(failure.replacement.clone(), |this, replacement| {
                        this.child(
                            Button::new(
                                "elastic-apply-replacement",
                                format!("Trocar por {replacement}"),
                            )
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.apply_replacement(window, cx)
                                }),
                            ),
                        )
                    })
                    .when(has_range, |this| {
                        this.child(
                            Button::new(
                                "elastic-go-to-error",
                                match failure.line {
                                    Some(line) => format!("Ir para a linha {line}"),
                                    None => "Ir para o erro".to_string(),
                                },
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.go_to_error(window, cx)),
                            ),
                        )
                    })
                    .child(
                        Button::new("elastic-explain-error", "Explicar a sintaxe")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let message = match &this.state {
                                    RunState::Failed(failure) => failure.message.to_string(),
                                    _ => String::new(),
                                };
                                this.ask_agent(
                                    &format!(
                                        "A consulta falhou com: {message}\nExplique o erro em \
                                         uma frase e corrija a consulta no editor."
                                    ),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_histogram(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
        let (peak_index, peak) = self
            .histogram
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, count))| *count)
            .map(|(index, (bucket, count))| (index, (bucket.clone(), *count)))?;
        let first = self
            .histogram
            .first()
            .map(|(bucket, _)| results::short_time(bucket));
        let last = self
            .histogram
            .last()
            .map(|(bucket, _)| results::short_time(bucket));
        let accent = cx.theme().colors().text_accent;
        let error = Color::Error.color(cx);
        let peaked = self.rows.is_log()
            && (0..self.rows.values.len()).any(|row| {
                self.rows
                    .text(row, results::LEVEL)
                    .is_some_and(|level| results::level_color(&level) == Color::Error)
            });
        Some(
            v_flex()
                .mx_3()
                .mb_2()
                .px_2()
                .pt_2()
                .pb_1()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex().h(px(40.)).items_end().gap_px().children(
                        self.histogram
                            .iter()
                            .enumerate()
                            .map(|(index, (bucket, count))| {
                                let height = (*count as f32 / max as f32 * 40.).max(2.);
                                div()
                                    .id(("elastic-bar", index))
                                    .flex_1()
                                    .h(px(height))
                                    .rounded_t_sm()
                                    .bg(if peaked && index == peak_index {
                                        error.opacity(0.8)
                                    } else {
                                        accent.opacity(0.55)
                                    })
                                    .tooltip(Tooltip::text(format!(
                                        "{} · {} docs",
                                        results::short_time(bucket),
                                        results::format_count(*count)
                                    )))
                            }),
                    ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(first.unwrap_or_default())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Disabled),
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(format!(
                                "pico · {} · {} docs",
                                results::short_time(&peak.0),
                                results::format_count(peak.1)
                            ))
                            .size(LabelSize::XSmall)
                            .color(if peaked {
                                Color::Error
                            } else {
                                Color::Muted
                            }),
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(last.unwrap_or_default())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Disabled),
                        ),
                )
                .into_any_element(),
        )
    }

    fn table_columns(&self) -> Vec<String> {
        if self.rows.is_log() {
            LOG_COLUMNS
                .iter()
                .filter(|name| self.rows.index(name).is_some())
                .map(|name| name.to_string())
                .collect()
        } else {
            self.rows
                .columns
                .iter()
                .take(MAX_TABLE_COLUMNS)
                .map(|column| column.name.clone())
                .collect()
        }
    }

    fn render_table(&self, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.table_columns();
        let log = self.rows.is_log();
        let header = h_flex()
            .px_3()
            .h(px(26.))
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .children(columns.iter().map(|name| {
                column_cell(name, log)
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .into_any_element()
            }));
        let row_count = self.rows.values.len();
        v_flex()
            .size_full()
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list(
                        "elastic-rows",
                        row_count,
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            let columns = this.table_columns();
                            range
                                .map(|row| this.render_table_row(row, &columns, cx))
                                .collect()
                        }),
                    )
                    .size_full(),
                ),
            )
            .into_any_element()
    }

    fn render_table_row(
        &self,
        row: usize,
        columns: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let log = self.rows.is_log();
        let selected = self.selected == Some(row);
        let level = self.rows.text(row, results::LEVEL);
        let error = level
            .as_deref()
            .is_some_and(|level| results::level_color(level) == Color::Error);
        h_flex()
            .id(("elastic-row", row))
            .px_3()
            .h(px(28.))
            .gap_3()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .when(!selected && error, |this| {
                this.bg(Color::Error.color(cx).opacity(0.05))
            })
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .children(columns.iter().map(|name| {
                let value = self.rows.text(row, name).unwrap_or_default();
                let cell = column_cell(name, log);
                match name.as_str() {
                    TIMESTAMP if log => cell
                        .child(
                            Label::new(results::short_time(&value))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                    results::LEVEL if log => cell
                        .child(results::level_chip(&value, cx))
                        .into_any_element(),
                    TRACE_ID => cell
                        .child(
                            Label::new(value)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(Color::Accent)
                                .truncate(),
                        )
                        .into_any_element(),
                    _ => cell
                        .child(
                            Label::new(value.replace('\n', " "))
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        )
                        .into_any_element(),
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = if this.selected == Some(row) {
                    None
                } else {
                    Some(row)
                };
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_document(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.table_columns();
        let fields: Vec<(String, String)> = self
            .rows
            .fields(row)
            .into_iter()
            .filter(|(name, _)| {
                !shown.contains(name) && !results::STACK_FIELDS.contains(&name.as_str())
            })
            .take(12)
            .collect();
        let stack = self.rows.stack_trace(row);
        let first_link = stack
            .as_deref()
            .and_then(|stack| stack.lines().find_map(results::stack_link));
        let trace_id = self.rows.text(row, TRACE_ID);
        let message = self.rows.text(row, MESSAGE);
        render_document_detail(
            DocumentDetail {
                id: "elastic-document",
                fields,
                stack,
                first_link,
                trace_id,
                message,
                document: document_json(&self.rows, row),
            },
            self.workspace.clone(),
            self.project.clone(),
            self.panel.clone(),
            cx,
        )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let content = match (self.tab, &self.state) {
            (ResultTab::Documents, RunState::Failed(failure)) => self.render_failure(failure, cx),
            (ResultTab::Documents, RunState::Done { .. } | RunState::Running { .. })
                if !self.rows.columns.is_empty() =>
            {
                v_flex()
                    .size_full()
                    .when(self.is_running(), |this| this.opacity(0.4))
                    .children(self.render_histogram(cx))
                    .child(div().flex_1().min_h_0().child(self.render_table(cx)))
                    .when_some(self.selected, |this, row| {
                        this.child(self.render_document(row, cx))
                    })
                    .into_any_element()
            }
            (ResultTab::Documents, RunState::Done { .. }) => div()
                .p_3()
                .child(
                    Label::new("Nenhum documento nessa janela.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            (ResultTab::Documents, _) => div()
                .p_3()
                .child(
                    Label::new(if self.is_running() {
                        "Aguardando o Elasticsearch…"
                    } else {
                        "Rode uma consulta para ver os documentos aqui."
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            (ResultTab::History, _) => v_flex()
                .id("elastic-history")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .children(self.history.iter().enumerate().map(|(index, entry)| {
                    let query = entry.query.clone();
                    v_flex()
                        .id(("elastic-history-entry", index))
                        .px_2()
                        .py_1p5()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .tooltip(Tooltip::text("Copiar a consulta"))
                        .child(
                            Label::new(
                                esql::commands(&query)
                                    .join(" | ")
                                    .chars()
                                    .take(160)
                                    .collect::<String>(),
                            )
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                        )
                        .child(
                            Label::new(entry.summary.clone())
                                .size(LabelSize::XSmall)
                                .color(if entry.failed {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                        )
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(query.clone()))
                        })
                }))
                .into_any_element(),
        };
        let has_rows = !self.rows.values.is_empty();
        let footer = match &self.state {
            RunState::Done { from, to, .. } if has_rows => Some(format!(
                "{} {} · {}–{} (horário local){}",
                self.rows.values.len(),
                if self.rows.is_log() { "docs" } else { "linhas" },
                results::short_time(&esql::iso(*from))
                    .get(..5)
                    .unwrap_or_default(),
                results::short_time(&esql::iso(*to))
                    .get(..5)
                    .unwrap_or_default(),
                if self.rows.is_log() {
                    " · clique numa linha para abrir o documento"
                } else {
                    ""
                }
            )),
            _ => None,
        };
        v_flex()
            .relative()
            .map(|this| match self.results_height {
                Some(height) => this.h(height),
                None => this.h(relative(0.62)),
            })
            .min_h(MIN_RESULTS_HEIGHT)
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .id("elastic-results-resize-handle")
                    .absolute()
                    .top(px(-3.))
                    .left_0()
                    .right_0()
                    .h(px(6.))
                    .cursor_row_resize()
                    .on_drag(DraggedResultsDivider, |_, _, _, cx| cx.new(|_| Empty))
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.results_height = None;
                            cx.notify();
                        }
                    })),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .child(Label::new("Resultados").weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1().min_w_0().child(self.render_status()))
                    .child(ToggleButtonGroupTabs { selected: self.tab }.render(cx))
                    .child(
                        IconButton::new("elastic-copy-csv", IconName::Copy)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(!has_rows)
                            .tooltip(Tooltip::text("Copiar como CSV"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_csv(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
            .when_some(footer, |this, footer| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new(footer)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("elastic-export-json", "Exportar JSON")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .on_click(cx.listener(|this, _, _, cx| this.copy_json(cx))),
                        )
                        .child(
                            Button::new("elastic-open-kibana", "Abrir no Kibana")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .on_click(cx.listener(|this, _, _, cx| this.open_in_kibana(cx))),
                        ),
                )
            })
            .into_any_element()
    }
}

/// The Docs/History switch; a struct so the listener can reach the view.
struct ToggleButtonGroupTabs {
    selected: ResultTab,
}

impl ToggleButtonGroupTabs {
    fn render(self, cx: &mut Context<EsqlQueryView>) -> AnyElement {
        let tabs = [
            ui::ToggleButtonSimple::new(
                "Docs",
                cx.listener(|this, _, _, cx| {
                    this.tab = ResultTab::Documents;
                    cx.notify();
                }),
            ),
            ui::ToggleButtonSimple::new(
                "Histórico",
                cx.listener(|this, _, _, cx| {
                    this.tab = ResultTab::History;
                    cx.notify();
                }),
            ),
        ];
        ui::ToggleButtonGroup::single_row("elastic-result-tabs", tabs)
            .style(ui::ToggleButtonGroupStyle::Outlined)
            .size(ui::ToggleButtonGroupSize::Custom(rems_from_px(26_f32)))
            .label_size(LabelSize::Small)
            .auto_width()
            .selected_index(match self.selected {
                ResultTab::Documents => 0,
                ResultTab::History => 1,
            })
            .into_any_element()
    }
}

fn column_cell(name: &str, log: bool) -> gpui::Div {
    let cell = div().min_w_0().overflow_hidden();
    if !log {
        return cell.flex_1();
    }
    match name {
        TIMESTAMP => cell.w(px(96.)).flex_none(),
        results::LEVEL => cell.w(px(56.)).flex_none(),
        MESSAGE => cell.flex_1(),
        results::SERVICE => cell.w(px(130.)).flex_none(),
        TRACE_ID => cell.w(px(120.)).flex_none(),
        _ => cell.flex_1(),
    }
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub(crate) fn document_json(rows: &Rows, row: usize) -> serde_json::Value {
    let mut document = serde_json::Map::new();
    if let Some(values) = rows.values.get(row) {
        for (column, value) in rows.columns.iter().zip(values) {
            if !value.is_null() {
                document.insert(column.name.clone(), value.clone());
            }
        }
    }
    serde_json::Value::Object(document)
}

/// What the document strip shows, shared by the query tab and the follow tab.
pub(crate) struct DocumentDetail {
    pub id: &'static str,
    pub fields: Vec<(String, String)>,
    pub stack: Option<String>,
    pub first_link: Option<results::StackLink>,
    pub trace_id: Option<String>,
    pub message: Option<String>,
    pub document: serde_json::Value,
}

pub(crate) fn render_document_detail<V: 'static>(
    detail: DocumentDetail,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    cx: &mut Context<V>,
) -> AnyElement {
    let DocumentDetail {
        id,
        fields,
        stack,
        first_link,
        trace_id,
        message,
        document,
    } = detail;
    v_flex()
        .id(id)
        .max_h(px(300.))
        .overflow_y_scroll()
        .px_4()
        .py_2p5()
        .gap_2()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().editor_background)
        .when(!fields.is_empty(), |this| {
            this.child(
                h_flex()
                    .flex_wrap()
                    .gap_x_6()
                    .gap_y_1p5()
                    .children(fields.into_iter().map(|(name, value)| {
                        v_flex()
                            .max_w(px(260.))
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
                                    .truncate(),
                            )
                    })),
            )
        })
        .when_some(stack, |this, stack| {
            this.child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("error.stack_trace")
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Disabled),
                    )
                    .children(stack.lines().take(12).enumerate().map(|(index, frame)| {
                        let link = results::stack_link(frame);
                        let frame = frame.to_string();
                        let workspace = workspace.clone();
                        let project = project.clone();
                        h_flex()
                            .id(("elastic-frame", index))
                            .child(
                                Label::new(frame)
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(if index == 0 {
                                        Color::Error
                                    } else if link.is_some() {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    }),
                            )
                            .when_some(link, move |this, link| {
                                this.cursor_pointer()
                                    .tooltip(Tooltip::text(format!(
                                        "Abrir {}:{}",
                                        link.path, link.line
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
                            })
                    })),
            )
        })
        .child(
            h_flex()
                .gap_2()
                .when_some(first_link, |this, link| {
                    let workspace = workspace.clone();
                    let project = project.clone();
                    let file = std::path::Path::new(&link.path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| link.path.clone());
                    this.child(
                        Button::new(
                            SharedString::from(format!("{id}-open-file")),
                            format!("Abrir {file}:{}", link.line),
                        )
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::FileCode).size(IconSize::Small))
                        .on_click(move |_, window, cx| {
                            results::open_stack_link(
                                workspace.clone(),
                                &project,
                                &link,
                                window,
                                cx,
                            );
                        }),
                    )
                })
                .child({
                    let document = document.clone();
                    let title = message
                        .map(|message| message.chars().take(60).collect::<String>())
                        .unwrap_or_else(|| "documento".to_string());
                    Button::new(
                        SharedString::from(format!("{id}-ask-agent")),
                        "Perguntar ao Agent",
                    )
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                    .on_click(move |_, window, cx| {
                        let id = crate::agent_toolkit::remember_document(document.clone(), cx);
                        window.dispatch_action(
                            Box::new(zed_actions::agent::MentionLogs {
                                id,
                                title: title.clone(),
                                prompt: Some(
                                    "Investigue este log: o que aconteceu, onde no código e se \
                                     é recorrente."
                                        .to_string(),
                                ),
                                submit: true,
                            }),
                            cx,
                        );
                    })
                })
                .when_some(trace_id, |this, trace_id| {
                    let workspace = workspace.clone();
                    let project = project.clone();
                    let panel = panel.clone();
                    this.child(
                        Button::new(
                            SharedString::from(format!("{id}-trace")),
                            "Ver trace no APM",
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ListTree).size(IconSize::Small))
                        .on_click(move |_, window, cx| {
                            let Some(session) =
                                panel.upgrade().and_then(|panel| panel.read(cx).session())
                            else {
                                return;
                            };
                            trace_view::open(
                                workspace.clone(),
                                project.clone(),
                                panel.clone(),
                                session,
                                trace_id.clone(),
                                window,
                                cx,
                            );
                        }),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("{id}-copy")), "Copiar doc")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Copy).size(IconSize::Small))
                        .on_click(move |_, _, cx| {
                            if let Ok(json) = serde_json::to_string_pretty(&document) {
                                cx.write_to_clipboard(ClipboardItem::new_string(json));
                            }
                        }),
                ),
        )
        .into_any_element()
}

impl Render for EsqlQueryView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("EsqlQueryView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| this.run(window, cx)))
            .on_action(cx.listener(Self::cancel))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedResultsDivider>, _, cx| {
                    let bounds = event.bounds;
                    let max_height =
                        (bounds.size.height - MIN_EDITOR_HEIGHT).max(MIN_RESULTS_HEIGHT);
                    let height = (bounds.bottom() - event.event.position.y)
                        .clamp(MIN_RESULTS_HEIGHT, max_height);
                    this.results_height = Some(height);
                    cx.notify();
                },
            ))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            .child(self.render_results(cx))
    }
}

impl Focusable for EsqlQueryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for EsqlQueryView {}

impl Item for EsqlQueryView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "consulta.esql".to_string())
            .into()
    }

    fn tab_content(
        &self,
        params: TabContentParams,
        _window: &gpui::Window,
        cx: &App,
    ) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} · {}", self.path.display(), self.session.connection.name).into())
    }

    fn tab_icon(&self, _window: &gpui::Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::CloudPulse).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

/// Opens the `.esql` file against the dock's connection, or focuses the tab that already has it.
pub fn open(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: WeakEntity<ElasticPanel>,
    session: Arc<Session>,
    path: PathBuf,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Task<anyhow::Result<Entity<EsqlQueryView>>> {
    let existing = workspace
        .update(cx, |workspace, cx| {
            let existing = workspace
                .items_of_type::<EsqlQueryView>(cx)
                .find(|view| view.read(cx).path == path);
            if let Some(existing) = &existing {
                workspace.activate_item(existing, true, true, window, cx);
            }
            existing
        })
        .ok()
        .flatten();
    if let Some(existing) = existing {
        return Task::ready(Ok(existing));
    }
    let buffer = project.update(cx, |project, cx| project.open_local_buffer(&path, cx));
    window.spawn(cx, async move |cx| {
        let buffer = buffer.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            let view = cx.new(|cx| {
                EsqlQueryView::new(
                    workspace_handle,
                    project,
                    buffer,
                    path,
                    panel,
                    session,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        })
    })
}

/// Brings the tab to the front of its pane.
pub(crate) fn activate(
    view: &Entity<EsqlQueryView>,
    window: &mut gpui::Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let workspace = view.read(cx).workspace.clone();
    workspace.update(cx, |workspace, cx| {
        workspace.activate_item(view, true, true, window, cx);
    })
}

/// A new file in `.asylum/elastic/queries`, named `consulta-N.esql`.
pub async fn create_query_file(
    fs: &dyn fs::Fs,
    root: &Path,
    contents: &str,
) -> anyhow::Result<PathBuf> {
    let directory = root.join(crate::connection::QUERIES_DIR);
    fs.create_dir(&directory).await?;
    let mut number = 1;
    let path = loop {
        let candidate = directory.join(format!("consulta-{number}.esql"));
        if !fs.is_file(&candidate).await {
            break candidate;
        }
        number += 1;
    };
    fs.atomic_write(path.clone(), contents.to_string()).await?;
    Ok(path)
}
