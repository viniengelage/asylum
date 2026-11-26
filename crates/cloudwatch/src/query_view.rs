//! A `.insights` file from the project opened as a Logs Insights tab: the file is an ordinary
//! editor (saved like any other) with the log groups, the window and the limit above it and what
//! the query returned underneath.

mod assist;
mod insights;
mod results;

use std::{
    cell::RefCell,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, HighlightKey, MultiBufferOffset};
use gpui::{
    AnyElement, App, Context, DragMoveEvent, Empty, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, FontWeight, HighlightStyle, KeyContext, ListAlignment, ListState, Pixels,
    Subscription, Task, UnderlineStyle, WeakEntity, Window, px, relative,
};
use gpui_tokio::Tokio;
use i18n::t;
use language::{Buffer, CodeLabel, Language, LanguageConfig, LanguageMatcher, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Project,
    ProjectEntryId, ProjectPath, lsp_store::CompletionDocumentation,
};
use serde::{Deserialize, Serialize};
use ui::{
    ContextMenu, IconPosition, KeyBinding, PopoverMenu, PopoverTriggerArea, prelude::*,
    primary_button, secondary_button,
};
use util::ResultExt as _;
use workspace::{
    OpenOptions, Workspace,
    item::{Item, ItemEvent, ProjectItem, SaveOptions, TabContentParams},
};

use crate::{
    AcceptSuggestion, AcceptSuggestionAndRun, AskAgent, CancelQuery, CloudWatchError,
    CloudWatchStore, Connection, DiscardSuggestion, QUERY_EXTENSION, QueryResult, RunQuery,
    StoreEvent, SummarizeResults,
};

use insights::TokenKind;
use results::ResultSet;

const LANGUAGE_NAME: &str = "Logs Insights";
const MAX_HISTORY: usize = 50;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const SAMPLE_EVENTS: i32 = 20;
const SAMPLE_WINDOW: Duration = Duration::from_secs(15 * 60);
const MIN_RESULTS_HEIGHT: Pixels = px(180.);
const MIN_EDITOR_HEIGHT: Pixels = px(120.);
const WINDOW_PRESETS: [u32; 8] = [5, 15, 30, 60, 180, 720, 1440, 10_080];
const LIMIT_PRESETS: [u32; 6] = [50, 100, 200, 500, 1000, 10_000];
/// Keys of the editor's text highlights, one per token kind; the error underline comes after.
/// The editor has no key for third-party syntax, and these are only used by the debugger's
/// console editor, never by this one.
const ERROR_UNDERLINE_KEY: usize = 100;

struct DraggedResultsDivider;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TimeWindow {
    Last {
        minutes: u32,
    },
    /// Unix seconds, from a click on a histogram bar.
    Between {
        start: i64,
        end: i64,
    },
}

impl TimeWindow {
    pub(crate) fn range(self, now: SystemTime) -> (SystemTime, SystemTime) {
        match self {
            TimeWindow::Last { minutes } => {
                (now - Duration::from_secs(u64::from(minutes) * 60), now)
            }
            TimeWindow::Between { start, end } => (from_unix(start), from_unix(end)),
        }
    }

    pub(crate) fn seconds(self) -> u64 {
        match self {
            TimeWindow::Last { minutes } => u64::from(minutes) * 60,
            TimeWindow::Between { start, end } => (end - start).max(1) as u64,
        }
    }

    pub(crate) fn label(self) -> SharedString {
        match self {
            TimeWindow::Last { minutes } => minutes_label(minutes),
            TimeWindow::Between { start, end } => format!(
                "{}–{}",
                results::clock(from_unix(start)),
                results::clock(from_unix(end))
            )
            .into(),
        }
    }
}

pub(crate) fn minutes_label(minutes: u32) -> SharedString {
    if minutes.is_multiple_of(1440) {
        t!("cloudwatch_query.window.days", count = minutes / 1440)
    } else if minutes.is_multiple_of(60) {
        t!("cloudwatch_query.window.hours", count = minutes / 60)
    } else {
        t!("cloudwatch_query.window.minutes", count = minutes)
    }
}

pub(crate) fn from_unix(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}

pub(crate) fn unix(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HistoryKind {
    Run,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    pub kind: HistoryKind,
    pub query: String,
    /// The request for agent entries, the outcome for runs.
    pub summary: String,
    pub at: i64,
}

/// What the tab remembers per file, in the profile's key-value store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileState {
    groups: Vec<String>,
    window: TimeWindow,
    limit: u32,
    history: Vec<HistoryEntry>,
}

impl Default for FileState {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            window: TimeWindow::Last { minutes: 30 },
            limit: 200,
            history: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultTab {
    Events,
    Chart,
    Fields,
}

pub(crate) enum RunState {
    Idle,
    Running {
        started: Instant,
    },
    Done {
        elapsed: Duration,
        finished: SystemTime,
        bytes_scanned: f64,
        from: SystemTime,
        to: SystemTime,
    },
    Malformed {
        message: String,
    },
    SessionExpired,
    Failed(SharedString),
}

/// The events FilterLogEvents returned for the first group, for field discovery and as the
/// sample the ⌘K prompt shows the model.
#[derive(Default)]
pub(crate) struct Sample {
    pub group: Option<String>,
    pub events: Vec<Vec<(String, String)>>,
}

pub struct QueryView {
    workspace: Option<WeakEntity<Workspace>>,
    store: Option<Entity<CloudWatchStore>>,
    project: Entity<Project>,
    editor: Entity<Editor>,
    path: PathBuf,
    focus_handle: FocusHandle,
    file: FileState,
    diagnostic: Option<insights::Diagnostic>,
    state: RunState,
    last_query: Option<String>,
    results: Option<ResultSet>,
    histogram: Vec<(i64, u64)>,
    histogram_bin: u64,
    selected: Option<usize>,
    tab: ResultTab,
    list_state: ListState,
    visible_rows: Rc<RefCell<Option<usize>>>,
    results_height: Option<Pixels>,
    sample: Sample,
    fields: Rc<RefCell<Vec<String>>>,
    pending_rerun: bool,
    assist: assist::Assist,
    run_task: Task<()>,
    tick_task: Task<()>,
    sample_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl QueryView {
    fn new(
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        set_language(&project, &buffer, cx);
        let fields = Rc::new(RefCell::new(Vec::new()));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, Some(project.clone()), window, cx);
            editor.set_completion_provider(Some(Rc::new(InsightsCompletionProvider {
                fields: fields.clone(),
            })));
            editor
        });
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event, cx| match event {
            EditorEvent::DirtyChanged
            | EditorEvent::Saved
            | EditorEvent::TitleChanged
            | EditorEvent::FileHandleChanged => cx.emit(ItemEvent::UpdateTab),
            EditorEvent::BufferEdited => {
                this.refresh_syntax(cx);
                if this.assist.fix.is_some() || matches!(this.state, RunState::Malformed { .. }) {
                    this.assist.fix = None;
                    this.clear_error(cx);
                }
                cx.emit(ItemEvent::Edit);
            }
            _ => {}
        })];
        let file = read_json::<FileState>(&file_key(&path), cx).unwrap_or_default();
        let visible_rows = Rc::new(RefCell::new(None));
        let list_state = ListState::new(0, ListAlignment::Top, px(400.));
        list_state.set_scroll_handler({
            let visible_rows = visible_rows.clone();
            move |event, _, _| {
                *visible_rows.borrow_mut() = Some(event.visible_range.len());
            }
        });
        let mut this = Self {
            workspace: None,
            store: None,
            project,
            editor,
            path,
            focus_handle: cx.focus_handle(),
            file,
            diagnostic: None,
            state: RunState::Idle,
            last_query: None,
            results: None,
            histogram: Vec::new(),
            histogram_bin: 60,
            selected: None,
            tab: ResultTab::Events,
            list_state,
            visible_rows,
            results_height: None,
            sample: Sample::default(),
            fields,
            pending_rerun: false,
            assist: assist::Assist::default(),
            run_task: Task::ready(()),
            tick_task: Task::ready(()),
            sample_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.refresh_syntax(cx);
        this
    }

    /// Called from `added_to_workspace`, which hands over the workspace itself: the store must
    /// not read the workspace entity while it is being updated.
    fn attach(&mut self, workspace: &Workspace, cx: &mut Context<Self>) {
        if self.store.is_some() {
            return;
        }
        let store = CloudWatchStore::for_workspace(
            workspace.weak_handle(),
            workspace.project().clone(),
            cx,
        );
        self.workspace = Some(workspace.weak_handle());
        self._subscriptions
            .push(cx.subscribe(&store, |this, _, event, cx| match event {
                StoreEvent::Connected => {
                    this.load_sample(cx);
                    if this.pending_rerun {
                        this.pending_rerun = false;
                        this.run(None, cx);
                    }
                }
                StoreEvent::SessionExpired => cx.notify(),
                StoreEvent::TargetChanged => {
                    this.sample = Sample::default();
                    cx.notify();
                }
            }));
        self._subscriptions
            .push(cx.observe(&store, |this, store, cx| {
                if this.file.groups.is_empty() && !store.read(cx).project_groups().is_empty() {
                    this.file.groups = store.read(cx).project_groups().to_vec();
                    this.load_sample(cx);
                }
                cx.notify();
            }));
        if self.file.groups.is_empty() {
            self.file.groups = store.read(cx).project_groups().to_vec();
        }
        self.store = Some(store);
        self.load_sample(cx);
    }

    fn store(&self) -> Option<&Entity<CloudWatchStore>> {
        self.store.as_ref()
    }

    fn query_text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running { .. })
    }

    fn save_file_state(&self, cx: &App) {
        write_json(file_key(&self.path), &self.file, cx);
    }

    fn push_history(&mut self, entry: HistoryEntry, cx: &App) {
        self.file.history.insert(0, entry);
        self.file.history.truncate(MAX_HISTORY);
        self.save_file_state(cx);
    }

    pub(crate) fn set_groups(&mut self, groups: Vec<String>, cx: &mut Context<Self>) {
        self.file.groups = groups;
        self.save_file_state(cx);
        self.load_sample(cx);
        cx.notify();
    }

    pub(crate) fn set_window(&mut self, window: TimeWindow, cx: &mut Context<Self>) {
        self.file.window = window;
        self.save_file_state(cx);
        cx.notify();
    }

    fn set_limit(&mut self, limit: u32, cx: &mut Context<Self>) {
        self.file.limit = limit;
        self.save_file_state(cx);
        cx.notify();
    }

    fn refresh_syntax(&mut self, cx: &mut Context<Self>) {
        let text = self.query_text(cx);
        self.diagnostic = insights::validate(&text);
        let tokens = insights::tokenize(&text);
        let syntax = cx.theme().syntax().clone();
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (index, kind) in TokenKind::HIGHLIGHTED.iter().enumerate() {
                let Some(style) = kind
                    .style_name()
                    .and_then(|name| syntax.style_for_name(name))
                else {
                    continue;
                };
                let ranges = tokens
                    .iter()
                    .filter(|token| token.kind == *kind)
                    .map(|token| {
                        snapshot.anchor_before(MultiBufferOffset(token.range.start))
                            ..snapshot.anchor_after(MultiBufferOffset(token.range.end))
                    })
                    .collect::<Vec<_>>();
                editor.highlight_text(HighlightKey::ConsoleAnsiHighlight(index), ranges, style, cx);
            }
        });
        cx.notify();
    }

    fn highlight_error(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let text = self.query_text(cx);
        let line_start = text[..range.start.min(text.len())]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |index| line_start + index);
        let error = Color::Error.color(cx);
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let line = [snapshot.anchor_before(MultiBufferOffset(line_start))
                ..snapshot.anchor_after(MultiBufferOffset(line_end))];
            editor.highlight_background(
                HighlightKey::DatabaseError,
                &line,
                |_, theme| theme.status().error_background,
                cx,
            );
            let symbol = vec![
                snapshot.anchor_before(MultiBufferOffset(range.start))
                    ..snapshot.anchor_after(MultiBufferOffset(range.end.min(line_end))),
            ];
            editor.highlight_text(
                HighlightKey::ConsoleAnsiHighlight(ERROR_UNDERLINE_KEY),
                symbol,
                HighlightStyle {
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(error),
                        wavy: true,
                    }),
                    ..HighlightStyle::default()
                },
                cx,
            );
        });
    }

    fn clear_error(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(HighlightKey::DatabaseError, cx);
            editor.clear_highlights(HighlightKey::ConsoleAnsiHighlight(ERROR_UNDERLINE_KEY), cx);
        });
    }

    /// Reads a few recent events of the first group with FilterLogEvents, which costs nothing
    /// per byte, to learn the field names before any query runs.
    fn load_sample(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.file.groups.first().cloned() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(client) = store.read(cx).client() else {
            return;
        };
        if self.sample.group.as_deref() == Some(group.as_str()) && !self.sample.events.is_empty() {
            return;
        }
        let start = SystemTime::now() - SAMPLE_WINDOW;
        let load = Tokio::spawn(cx, {
            let group = group.clone();
            async move {
                client
                    .filter_log_events(&group, None, start, SAMPLE_EVENTS)
                    .await
            }
        });
        self.sample_task = cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(result) => result,
                Err(error) => Err(CloudWatchError::Other(error.into())),
            };
            this.update(cx, |this, cx| match result {
                Ok(events) => {
                    this.sample = Sample {
                        group: Some(group),
                        events: events
                            .iter()
                            .map(|event| results::event_fields(event))
                            .collect(),
                    };
                    this.merge_fields(results::sample_field_names(&this.sample.events));
                    cx.notify();
                }
                Err(error) => {
                    store.update(cx, |store, cx| store.report_error(&error, cx));
                }
            })
            .log_err();
        });
    }

    fn merge_fields(&mut self, names: impl IntoIterator<Item = String>) {
        let mut fields = self.fields.borrow_mut();
        for name in names {
            if !fields.contains(&name) {
                fields.push(name);
            }
        }
    }

    /// `preview` runs another query over another window without touching the editor, for
    /// "Prévia de 15 min".
    pub(crate) fn run(&mut self, preview: Option<(String, TimeWindow)>, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let (query, time_window) =
            preview.unwrap_or_else(|| (self.query_text(cx), self.file.window));
        if query.trim().is_empty() {
            self.state = RunState::Failed(t!("cloudwatch_query.run.nothing_to_run"));
            cx.notify();
            return;
        }
        let groups = self.file.groups.clone();
        if groups.is_empty() {
            self.state = RunState::Failed(t!("cloudwatch_query.run.no_groups"));
            cx.notify();
            return;
        }
        let client = match store.read(cx).connection() {
            Connection::Connected(client) => client.clone(),
            Connection::SessionExpired { .. } => {
                self.state = RunState::SessionExpired;
                self.pending_rerun = true;
                cx.notify();
                return;
            }
            Connection::Failed(message) => {
                self.state = RunState::Failed(message.clone().into());
                cx.notify();
                return;
            }
            Connection::Idle | Connection::Connecting => {
                self.state = RunState::Failed(t!("cloudwatch_query.run.connecting"));
                self.pending_rerun = true;
                cx.notify();
                return;
            }
        };
        self.clear_error(cx);
        self.assist.fix = None;
        self.state = RunState::Running {
            started: Instant::now(),
        };
        self.tick_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
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
        cx.notify();

        let (from, to) = time_window.range(SystemTime::now());
        let (bin_seconds, bin) = insights::bin_for_window(time_window.seconds());
        let histogram = insights::histogram_query(&query, &bin);
        let bins = (time_window.seconds() / bin_seconds + 2) as i32;
        let limit = self.file.limit as i32;
        let started = Instant::now();
        let request = Tokio::spawn(cx, {
            let query = query.clone();
            async move {
                let main =
                    client.run_insights_query(&groups, &query, from, to, limit, POLL_INTERVAL);
                let bars = async {
                    match &histogram {
                        Some(histogram) => Some(
                            client
                                .run_insights_query(
                                    &groups,
                                    histogram,
                                    from,
                                    to,
                                    bins,
                                    POLL_INTERVAL,
                                )
                                .await,
                        ),
                        None => None,
                    }
                };
                futures::join!(main, bars)
            }
        });
        self.run_task = cx.spawn(async move |this, cx| {
            let (result, bars) = match request.await {
                Ok(result) => result,
                Err(error) => (Err(CloudWatchError::Other(error.into())), None),
            };
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.finish(query, result, bars, from, to, bin_seconds, elapsed, cx)
            })
            .log_err();
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        query: String,
        result: Result<QueryResult, CloudWatchError>,
        bars: Option<Result<QueryResult, CloudWatchError>>,
        from: SystemTime,
        to: SystemTime,
        bin_seconds: u64,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let mut bytes_scanned = 0.0;
        let bars = match bars {
            Some(Ok(bars)) => {
                bytes_scanned += bars.bytes_scanned;
                if let Some(store) = &store {
                    store.update(cx, |store, cx| {
                        store.record_query(bars.bytes_scanned as u64, cx)
                    });
                }
                Some(bars)
            }
            Some(Err(error)) => {
                log::warn!("CloudWatch histogram query failed: {error}");
                None
            }
            None => None,
        };
        match result {
            Ok(result) => {
                bytes_scanned += result.bytes_scanned;
                if let Some(store) = &store {
                    store.update(cx, |store, cx| {
                        store.record_query(result.bytes_scanned as u64, cx)
                    });
                }
                let results = ResultSet::new(result.rows);
                self.merge_fields(results.columns.iter().cloned());
                self.list_state.reset(results.rows.len());
                *self.visible_rows.borrow_mut() = None;
                self.histogram_bin = bin_seconds;
                self.histogram = bars
                    .map(|bars| results::histogram(&bars.rows, unix(from), unix(to), bin_seconds))
                    .unwrap_or_default();
                let count = results.rows.len();
                self.results = Some(results);
                self.selected = None;
                self.assist.summary = None;
                self.last_query = Some(query.clone());
                self.state = RunState::Done {
                    elapsed,
                    finished: SystemTime::now(),
                    bytes_scanned,
                    from,
                    to,
                };
                self.push_history(
                    HistoryEntry {
                        kind: HistoryKind::Run,
                        query,
                        summary: t!(
                            "cloudwatch_query.history.run_summary",
                            count = count,
                            bytes = results::format_bytes(bytes_scanned)
                        )
                        .to_string(),
                        at: unix(SystemTime::now()),
                    },
                    cx,
                );
            }
            Err(CloudWatchError::MalformedQuery { message }) => {
                self.state = RunState::Malformed {
                    message: message.clone(),
                };
                let text = self.query_text(cx);
                if let Some((line, column)) = insights::error_position(&message)
                    && let Some(offset) = insights::offset_of(&text, line, column)
                {
                    let length =
                        insights::error_symbol(&message).map_or(1, |symbol| symbol.len().max(1));
                    self.highlight_error(offset..(offset + length).min(text.len()), cx);
                }
                self.suggest_fix(&text, &message, cx);
            }
            Err(error) => {
                if let Some(store) = &store {
                    store.update(cx, |store, cx| store.report_error(&error, cx));
                }
                if matches!(error, CloudWatchError::SessionExpired { .. }) {
                    self.state = RunState::SessionExpired;
                    self.pending_rerun = true;
                } else {
                    self.state = RunState::Failed(error.to_string().into());
                }
            }
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_running() {
            return;
        }
        // Dropping the task drops the SDK future, which stops the query on the server.
        self.run_task = Task::ready(());
        self.state = RunState::Idle;
        cx.notify();
    }

    fn run_action(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        if self.assist.proposal.is_some() {
            self.accept_proposal(true, window, cx);
        } else if self.assist.fix.is_some() {
            self.apply_fix(true, window, cx);
        } else {
            self.run(None, cx);
        }
    }

    /// Replaces the whole text in one edit, keeping what didn't change out of it.
    pub(crate) fn replace_text(&mut self, new_text: &str, cx: &mut Context<Self>) {
        let old_text = self.query_text(cx);
        let (range, replacement) = insights::minimal_edit(&old_text, new_text);
        self.editor.update(cx, |editor, cx| {
            editor.edit(
                [(
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    replacement,
                )],
                cx,
            );
        });
    }

    pub(crate) fn open_in_console(&self, cx: &mut App) {
        let Some(region) = self.store().and_then(|store| store.read(cx).region()) else {
            return;
        };
        let query = self
            .last_query
            .clone()
            .unwrap_or_else(|| self.query_text(cx));
        let (from, to) = match self.state {
            RunState::Done { from, to, .. } => (from, to),
            _ => self.file.window.range(SystemTime::now()),
        };
        cx.open_url(&insights::console_url(
            &region,
            &self.file.groups,
            &query,
            unix(from) * 1000,
            unix(to) * 1000,
        ));
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("consulta.{QUERY_EXTENSION}"))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.weak_entity();
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let editor_focus = self.editor.focus_handle(cx);
        let first_group = self
            .file
            .groups
            .first()
            .cloned()
            .unwrap_or_else(|| t!("cloudwatch_query.toolbar.no_groups").to_string());
        let extra_groups = self.file.groups.len().saturating_sub(1);
        let group_select = h_flex()
            .id("cloudwatch-groups")
            .w(px(262.))
            .h(px(32.))
            .px(px(10.))
            .gap_2()
            .rounded(Radius::Md.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .hover(|this| this.bg(colors.element_hover))
            .child(
                Icon::new(IconName::CloudPulse)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(first_group)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .buffer_font(cx)
                        .truncate(),
                ),
            )
            .when(extra_groups > 0, |this| {
                this.child(
                    div()
                        .px_2()
                        .rounded(Radius::Sm.rems())
                        .bg(colors.element_selected)
                        .child(
                            Label::new(format!("+{extra_groups}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            );
        let proposed_window = self
            .assist
            .proposal
            .as_ref()
            .and_then(|proposal| proposal.window_minutes);
        let window_label: SharedString = match (proposed_window, self.file.window) {
            (Some(proposed), TimeWindow::Last { minutes }) if proposed != minutes => {
                t!(
                    "cloudwatch_query.toolbar.window_change",
                    from = minutes,
                    to = proposed
                )
            }
            _ => self.file.window.label(),
        };
        let window_chip = toolbar_chip(
            "cloudwatch-window",
            Some(IconName::Clock),
            window_label,
            true,
            cx,
        )
        .when(proposed_window.is_some(), |this| {
            this.border_color(accent).bg(colors.element_selected)
        });
        let limit_chip = toolbar_chip(
            "cloudwatch-limit",
            None,
            t!("cloudwatch_query.toolbar.limit", limit = self.file.limit),
            false,
            cx,
        );
        let ask_open = self.assist.prompt.is_some();
        let ask = secondary_button(
            "cloudwatch-ask-agent",
            Some((IconName::Sparkle, Color::Accent)),
            t!("cloudwatch_query.toolbar.ask_agent"),
            true,
            cx,
        )
        .h(px(32.))
        .border_color(accent)
        .bg(accent.opacity(if ask_open { 0.35 } else { 0.15 }))
        .child(KeyBinding::for_action_in(&AskAgent, &editor_focus, cx).size(rems_from_px(12_f32)))
        .on_click(cx.listener(|this, _, window, cx| this.open_prompt(window, cx)));
        let history = PopoverMenu::new("cloudwatch-history")
            .trigger(PopoverTriggerArea::new(
                secondary_button(
                    "cloudwatch-history-trigger",
                    Some((IconName::History, Color::Default)),
                    t!("cloudwatch_query.toolbar.history"),
                    true,
                    cx,
                )
                .h(px(32.)),
            ))
            .menu({
                let this = this.clone();
                move |window, cx| {
                    let entries = this
                        .upgrade()
                        .map(|view| view.read(cx).file.history.clone())
                        .unwrap_or_default();
                    let this = this.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        if entries.is_empty() {
                            return menu.label(t!("cloudwatch_query.history.empty"));
                        }
                        for entry in entries {
                            let first_line = entry
                                .query
                                .lines()
                                .find(|line| {
                                    !line.trim().is_empty() && !line.trim_start().starts_with('#')
                                })
                                .unwrap_or_default()
                                .chars()
                                .take(56)
                                .collect::<String>();
                            let label = match entry.kind {
                                HistoryKind::Run => format!(
                                    "{} · {first_line} · {}",
                                    results::ago_label(from_unix(entry.at)),
                                    entry.summary
                                ),
                                HistoryKind::Agent => t!(
                                    "cloudwatch_query.history.agent_entry",
                                    when = results::ago_label(from_unix(entry.at)),
                                    request = entry.summary.chars().take(60).collect::<String>()
                                )
                                .to_string(),
                            };
                            let this = this.clone();
                            let query = entry.query;
                            menu = menu.entry(label, None, move |_, cx| {
                                this.update(cx, |this, cx| this.replace_text(&query, cx))
                                    .log_err();
                            });
                        }
                        menu
                    }))
                }
            });
        let run = if self.is_running() {
            secondary_button(
                "cloudwatch-cancel",
                Some((IconName::Stop, Color::Error)),
                t!("cloudwatch_query.toolbar.cancel"),
                true,
                cx,
            )
            .h(px(32.))
            .on_click(cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)))
            .into_any_element()
        } else {
            primary_button(
                "cloudwatch-run",
                Some(IconName::PlayFilled),
                t!("cloudwatch_query.toolbar.run"),
                Some(KeyBinding::for_action_in(&RunQuery, &editor_focus, cx)),
                cx,
            )
            .h(px(32.))
            .on_click(cx.listener(|this, _, window, cx| this.run_action(&RunQuery, window, cx)))
            .into_any_element()
        };
        h_flex()
            .px_3()
            .py_2p5()
            .gap_2()
            .child(
                PopoverMenu::new("cloudwatch-groups-menu")
                    .trigger(PopoverTriggerArea::new(group_select))
                    .menu({
                        let this = this.clone();
                        move |window, cx| Some(groups_menu(this.clone(), window, cx))
                    }),
            )
            .child(
                PopoverMenu::new("cloudwatch-window-menu")
                    .trigger(PopoverTriggerArea::new(window_chip))
                    .menu({
                        let this = this.clone();
                        let current = self.file.window;
                        move |window, cx| {
                            let this = this.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for minutes in WINDOW_PRESETS {
                                    let this = this.clone();
                                    menu = menu.toggleable_entry(
                                        minutes_label(minutes),
                                        current == TimeWindow::Last { minutes },
                                        IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            this.update(cx, |this, cx| {
                                                this.set_window(TimeWindow::Last { minutes }, cx)
                                            })
                                            .log_err();
                                        },
                                    );
                                }
                                menu
                            }))
                        }
                    }),
            )
            .child(
                PopoverMenu::new("cloudwatch-limit-menu")
                    .trigger(PopoverTriggerArea::new(limit_chip))
                    .menu({
                        let current = self.file.limit;
                        move |window, cx| {
                            let this = this.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for limit in LIMIT_PRESETS {
                                    let this = this.clone();
                                    menu = menu.toggleable_entry(
                                        t!("cloudwatch_query.toolbar.limit", limit = limit),
                                        current == limit,
                                        IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            this.update(cx, |this, cx| this.set_limit(limit, cx))
                                                .log_err();
                                        },
                                    );
                                }
                                menu
                            }))
                        }
                    }),
            )
            .child(div().flex_1())
            .child(ask)
            .child(history)
            .child(run)
            .into_any_element()
    }

    fn render_status_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, color, text): (IconName, Color, SharedString) =
            if let Some(proposal) = &self.assist.proposal {
                match &proposal.diagnostic {
                    Some(diagnostic) => (
                        IconName::XCircle,
                        Color::Error,
                        diagnostic_label(diagnostic),
                    ),
                    None => (
                        IconName::Check,
                        Color::Success,
                        assist::proposal_validity(proposal, &self.sample),
                    ),
                }
            } else if let Some(diagnostic) = &self.diagnostic {
                (
                    IconName::XCircle,
                    Color::Error,
                    diagnostic_label(diagnostic),
                )
            } else if let RunState::Malformed { message } = &self.state {
                (IconName::XCircle, Color::Error, malformed_label(message))
            } else {
                let fields = self.fields.borrow().len();
                let group = self.file.groups.first();
                let text = match group {
                    Some(group) if fields > 0 => t!(
                        "cloudwatch_query.status.valid_with_fields",
                        count = fields,
                        group = group
                    ),
                    _ => t!("cloudwatch_query.status.valid"),
                };
                (IconName::Check, Color::Success, text)
            };
        h_flex()
            .h(px(32.))
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Icon::new(icon).size(IconSize::Small).color(color))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(if color == Color::Error {
                            Color::Error
                        } else {
                            Color::Muted
                        })
                        .truncate(),
                ),
            )
            .child(
                Label::new(t!("cloudwatch_query.status.hint"))
                    .size(LabelSize::Small)
                    .color(Color::Placeholder),
            )
            .into_any_element()
    }

    fn render_editor_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        v_flex()
            .flex_1()
            .min_h(MIN_EDITOR_HEIGHT)
            .mx_3()
            .rounded(Radius::Md.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .overflow_hidden()
            .children(self.render_prompt(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pt_2()
                    .map(|this| match &self.assist.proposal {
                        Some(proposal) => this.child(assist::render_diff(&proposal.diff, cx)),
                        None => this.child(self.editor.clone()),
                    }),
            )
            .child(self.render_status_line(cx))
            .into_any_element()
    }
}

fn diagnostic_label(diagnostic: &insights::Diagnostic) -> SharedString {
    use insights::Problem;
    let problem = match &diagnostic.problem {
        Problem::UnexpectedSymbol(symbol) => {
            t!("cloudwatch_query.status.unexpected_symbol", symbol = symbol)
        }
        Problem::UnterminatedString => t!("cloudwatch_query.status.unterminated_string"),
        Problem::UnterminatedRegex => t!("cloudwatch_query.status.unterminated_regex"),
        Problem::UnbalancedParenthesis => t!("cloudwatch_query.status.unbalanced_parenthesis"),
        Problem::EmptyCommand => t!("cloudwatch_query.status.empty_command"),
        Problem::MissingArgument(command) => {
            t!(
                "cloudwatch_query.status.missing_argument",
                command = command
            )
        }
        Problem::InvalidLimit(limit) => t!("cloudwatch_query.status.invalid_limit", limit = limit),
    };
    t!(
        "cloudwatch_query.status.at_position",
        problem = problem,
        line = diagnostic.line,
        column = diagnostic.column
    )
}

fn malformed_label(message: &str) -> SharedString {
    match insights::error_position(message) {
        Some((line, column)) => {
            let problem = message
                .to_ascii_lowercase()
                .find(" at line")
                .map_or(message, |index| &message[..index]);
            t!(
                "cloudwatch_query.status.at_position",
                problem = problem,
                line = line,
                column = column
            )
        }
        None => message.to_string().into(),
    }
}

/// The 22 px bordered chip of the toolbar (período, limite); there is no shared control this
/// small with an optional icon and chevron.
fn toolbar_chip(
    id: &'static str,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    chevron: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .h(px(22.))
        .px_2()
        .gap_1p5()
        .rounded(Radius::Sm.rems())
        .border_1()
        .border_color(colors.border_variant)
        .bg(colors.element_background)
        .hover(|this| this.bg(colors.element_hover))
        .children(icon.map(|icon| Icon::new(icon).size(IconSize::Small).color(Color::Muted)))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .single_line(),
        )
        .when(chevron, |this| {
            this.child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

fn groups_menu(
    view: WeakEntity<QueryView>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build_persistent(window, cx, move |mut menu, _, cx| {
        let Some(entity) = view.upgrade() else {
            return menu;
        };
        let selected = entity.read(cx).file.groups.clone();
        let Some(store) = entity.read(cx).store.clone() else {
            return menu.label(t!("cloudwatch_query.groups.not_connected"));
        };
        let store = store.read(cx);
        let project_groups = store.project_groups().to_vec();
        let others: Vec<String> = store
            .log_groups()
            .iter()
            .map(|group| group.name.clone())
            .filter(|name| !project_groups.contains(name))
            .take(300)
            .collect();
        let add = |menu: ContextMenu, name: String| {
            let view = view.clone();
            let checked = selected.contains(&name);
            menu.toggleable_entry(
                name.clone(),
                checked,
                IconPosition::Start,
                None,
                move |_, cx| {
                    view.update(cx, |view, cx| {
                        let mut groups = view.file.groups.clone();
                        if let Some(index) = groups.iter().position(|group| *group == name) {
                            groups.remove(index);
                        } else {
                            groups.push(name.clone());
                        }
                        view.set_groups(groups, cx);
                    })
                    .log_err();
                },
            )
        };
        if !project_groups.is_empty() {
            menu = menu.header(t!("cloudwatch_query.groups.project"));
            for name in project_groups {
                menu = add(menu, name);
            }
        }
        if !others.is_empty() {
            menu = menu.header(t!("cloudwatch_query.groups.all"));
            for name in others {
                menu = add(menu, name);
            }
        }
        menu.keep_open_on_confirm(true)
    })
}

impl Render for QueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut context = KeyContext::default();
        context.add("CloudWatchQueryView");
        if self.assist.proposal.is_some() || self.assist.fix.is_some() {
            context.add("suggestion");
        }
        v_flex()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_action))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &AskAgent, window, cx| this.open_prompt(window, cx)))
            .on_action(cx.listener(|this, _: &AcceptSuggestion, window, cx| {
                this.accept_suggestion(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &AcceptSuggestionAndRun, window, cx| {
                this.accept_suggestion(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &DiscardSuggestion, window, cx| this.discard(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SummarizeResults, _, cx| this.summarize(cx)))
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
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(self.render_editor_area(cx))
            .child(
                v_flex()
                    .relative()
                    .flex_none()
                    .map(|this| match self.results_height {
                        Some(height) => this.h(height),
                        None => this.h(relative(0.62)),
                    })
                    .min_h(MIN_RESULTS_HEIGHT)
                    .mt_3()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        div()
                            .id("cloudwatch-results-resize")
                            .absolute()
                            .top(px(-4.))
                            .left_0()
                            .right_0()
                            .h(px(8.))
                            .flex()
                            .justify_center()
                            .cursor_row_resize()
                            .child(
                                div()
                                    .mt(px(1.))
                                    .w(px(40.))
                                    .h(px(6.))
                                    .rounded(px(3.))
                                    .bg(cx.theme().colors().border),
                            )
                            .on_drag(DraggedResultsDivider, |_, _, _, cx| cx.new(|_| Empty))
                            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                                if event.click_count() >= 2 {
                                    this.results_height = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(self.render_results(cx)),
            )
    }
}

impl Focusable for QueryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(prompt) = &self.assist.prompt {
            return prompt.focus_handle(cx);
        }
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for QueryView {}

impl Item for QueryView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.file_name().into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.path.display().to_string().into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::CloudPulse).color(Color::Accent))
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach(workspace, cx);
    }
}

impl ProjectItem for QueryView {
    type Item = InsightsFile;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<InsightsFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (buffer, path) = {
            let item = item.read(cx);
            (item.buffer.clone(), item.abs_path.clone())
        };
        Self::new(project, buffer, path, window, cx)
    }
}

/// A `.insights` file of the project; the tab edits its buffer like any editor would.
pub struct InsightsFile {
    buffer: Entity<Buffer>,
    abs_path: PathBuf,
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
}

impl project::ProjectItem for InsightsFile {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        if !path
            .path
            .as_unix_str()
            .ends_with(&format!(".{QUERY_EXTENSION}"))
        {
            return None;
        }
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let open = project.update(cx, |project, cx| project.open_buffer(path.clone(), cx));
        let project_path = path.clone();
        Some(cx.spawn(async move |cx| {
            let buffer = open.await?;
            Ok(cx.new(|_| InsightsFile {
                buffer,
                abs_path,
                project_path,
                entry_id,
            }))
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

/// A grammar-less language so `⌘/` comments with `#`; the colors come from the tab's own
/// tokenizer.
fn set_language(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) {
    let languages = project.read(cx).languages().clone();
    let name = language::LanguageName::new_static(LANGUAGE_NAME);
    let language = match languages
        .language_for_name_or_extension(LANGUAGE_NAME)
        .now_or_never()
    {
        Some(Ok(language)) if language.name() == name => language,
        _ => {
            let language = Arc::new(Language::new(
                LanguageConfig {
                    name,
                    matcher: Arc::new(LanguageMatcher {
                        path_suffixes: vec![QUERY_EXTENSION.into()],
                        ..LanguageMatcher::default()
                    }),
                    line_comments: vec!["# ".into()],
                    ..LanguageConfig::default()
                },
                None,
            ));
            languages.add(language.clone());
            language
        }
    };
    buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
}

use futures::FutureExt as _;

pub fn open_query(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<QueryView>(cx)
        .find(|view| view.read(cx).path == path);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    workspace
        .open_abs_path(
            path,
            OpenOptions {
                focus: Some(true),
                ..OpenOptions::default()
            },
            window,
            cx,
        )
        .detach_and_log_err(cx);
}

pub(crate) fn init(cx: &mut App) {
    workspace::register_project_item::<QueryView>(cx);
}

fn file_key(path: &std::path::Path) -> String {
    format!("cloudwatch.query.{}", path.display())
}

fn read_json<T: for<'de> Deserialize<'de>>(key: &str, cx: &App) -> Option<T> {
    KeyValueStore::global(cx)
        .read_kvp(key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
}

fn write_json<T: Serialize>(key: String, value: &T, cx: &App) {
    let Some(json) = serde_json::to_string(value).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move { store.write_kvp(key, json).await })
        .detach_and_log_err(cx);
}

/// Completion without a language server: the discovered fields, then commands and functions.
struct InsightsCompletionProvider {
    fields: Rc<RefCell<Vec<String>>>,
}

const FUNCTIONS: [&str; 16] = [
    "count(*)",
    "count_distinct()",
    "avg()",
    "sum()",
    "min()",
    "max()",
    "pct(, 95)",
    "stddev()",
    "earliest()",
    "latest()",
    "bin(1m)",
    "ispresent()",
    "isempty()",
    "strlen()",
    "concat()",
    "datefloor(@timestamp, 1h)",
];

impl editor::CompletionProvider for InsightsCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let (offset, text) = {
            let buffer = buffer.read(cx);
            (buffer_position.to_offset(buffer), buffer.text())
        };
        let word_start = text[..offset]
            .char_indices()
            .rev()
            .take_while(|(_, character)| {
                character.is_alphanumeric() || matches!(character, '_' | '.' | '@' | '$')
            })
            .last()
            .map_or(offset, |(index, _)| index);
        let replace_range = buffer.read(cx).anchor_before(word_start)..buffer_position;
        let mut suggestions: Vec<(String, Option<SharedString>)> = self
            .fields
            .borrow()
            .iter()
            .map(|field| (field.clone(), Some(t!("cloudwatch_query.completion.field"))))
            .collect();
        suggestions.extend(insights::COMMANDS.iter().map(|command| {
            (
                command.to_string(),
                Some(t!("cloudwatch_query.completion.command")),
            )
        }));
        suggestions.extend(FUNCTIONS.iter().map(|function| {
            (
                function.to_string(),
                Some(t!("cloudwatch_query.completion.function")),
            )
        }));
        let completions = suggestions
            .into_iter()
            .map(|(text, detail)| Completion {
                replace_range: replace_range.clone(),
                label: CodeLabel::plain(text.clone(), Some(&text)),
                new_text: text,
                documentation: detail.map(CompletionDocumentation::SingleLine),
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars()
            .last()
            .is_some_and(|character| character.is_alphanumeric() || matches!(character, '@' | '.'))
    }
}
