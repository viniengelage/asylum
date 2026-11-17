//! Live tail of CloudWatch log groups over `StartLiveTail`. The session is billed per minute, so
//! it closes on its own after a while with the tab out of sight; pausing only stops the view from
//! moving while events keep arriving.

mod patterns;

use std::{
    collections::VecDeque,
    ops::Range,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use chrono::TimeZone as _;
use editor::{Editor, EditorEvent};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, FollowMode, FontWeight,
    HighlightStyle, Hsla, InteractiveText, ListAlignment, ListState, StyledText, Subscription,
    Task, WeakEntity, actions, combine_highlights, list, px,
};
use gpui_tokio::Tokio;
use i18n::t;
use project::{Project, ProjectPath};
use ui::{
    ContextMenu, EmptyState, EmptyStateTone, KeyBinding, PopoverMenu, PopoverTriggerArea,
    StatusTone, Tooltip, prelude::*, primary_button, secondary_button, status_chip, status_pill,
    text_field,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
    notifications::DetachAndPromptErr as _,
};

use crate::{
    CloudWatchError, CloudWatchStore, Connection, LogEvent, QUERY_EXTENSION, StoreEvent,
    TailUpdate, TogglePause,
    agent_bridge::{self, AgentContext},
    aws_login, llm, query_view,
};
use patterns::{FrameLink, Level, NewPattern, PatternDetector};

actions!(
    cloudwatch_tail,
    [
        /// Restarts the live tail with the filter pattern being edited.
        ApplyFilter,
        /// Goes to the next match of the search in the live tail.
        NextMatch,
        /// Goes to the previous match of the search in the live tail.
        PreviousMatch,
    ]
);

const MAX_IN_MEMORY: usize = 5_000;
const IDLE_CLOSE: Duration = Duration::from_secs(10 * 60);
const REOPEN_DELAY: Duration = Duration::from_secs(2);
const RATE_WINDOW: Duration = Duration::from_secs(10);
const AGENT_LINES: usize = 200;
const PATTERN_CONTEXT_LINES: usize = 40;
const BASELINE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// `FilterLogEvents` reads forward from a start time, so the baseline samples several points of
/// the 24 h instead of only its first minutes.
const BASELINE_SLICES: u32 = 8;
const BASELINE_EVENTS_PER_SLICE: i32 = 400;

/// Opens a live tail of `log_groups` (the project's groups when empty), or focuses the tab
/// already tailing them.
pub fn open_live_tail(
    workspace: &mut Workspace,
    log_groups: Vec<String>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    let store = CloudWatchStore::for_workspace(workspace.weak_handle(), project.clone(), cx);
    let mut log_groups = if log_groups.is_empty() {
        store.read(cx).project_groups().to_vec()
    } else {
        log_groups
    };
    log_groups.dedup();
    if log_groups.is_empty() {
        workspace.show_toast(
            workspace::Toast::new(
                workspace::notifications::NotificationId::unique::<LiveTailView>(),
                t!("cloudwatch_tail.no_groups").to_string(),
            ),
            cx,
        );
        return;
    }
    let existing = workspace
        .items_of_type::<LiveTailView>(cx)
        .find(|view| view.read(cx).log_groups == log_groups);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let workspace_handle = workspace.weak_handle();
    let view =
        cx.new(|cx| LiveTailView::new(workspace_handle, project, store, log_groups, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

struct TailLine {
    timestamp_ms: i64,
    time: SharedString,
    group: Option<String>,
    source: Option<usize>,
    level: Option<Level>,
    message: String,
    stack: Vec<StackFrame>,
    raw: String,
}

struct StackFrame {
    text: String,
    link: Option<ResolvedLink>,
}

#[derive(Clone)]
struct ResolvedLink {
    range: Range<usize>,
    project_path: ProjectPath,
    line: u32,
    column: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
enum SessionState {
    WaitingForConnection,
    Starting,
    Live,
    Reopening,
    ClosedIdle,
    Expired,
    Failed(SharedString),
}

impl SessionState {
    fn is_open(&self) -> bool {
        matches!(
            self,
            SessionState::Starting | SessionState::Live | SessionState::Reopening
        )
    }
}

enum TailMessage {
    Started,
    Events {
        events: Vec<LogEvent>,
        sampled: bool,
    },
    Ended(Result<(), CloudWatchError>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SearchMatch {
    line: usize,
    /// 0 is the message, `n` the stack frame `n - 1`.
    part: usize,
    start: usize,
    end: usize,
}

struct PatternWarning {
    pattern: NewPattern,
    detected_at: Instant,
    sentence: Option<String>,
    context: Vec<String>,
    _summary: Task<()>,
}

pub struct LiveTailView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<CloudWatchStore>,
    log_groups: Vec<String>,
    short_names: Vec<String>,
    filter_editor: Entity<Editor>,
    applied_filter: Option<String>,
    search_editor: Entity<Editor>,
    search_query: String,
    search_matches: Vec<SearchMatch>,
    search_dirty: bool,
    current_match: Option<usize>,
    lines: VecDeque<TailLine>,
    pending: VecDeque<TailLine>,
    paused: bool,
    total_events: usize,
    sampled: bool,
    arrivals: VecDeque<(Instant, usize)>,
    state: SessionState,
    opened_at: Option<Instant>,
    segment_started: Option<Instant>,
    last_seen: Instant,
    detector: PatternDetector,
    baseline_requested: bool,
    warning: Option<PatternWarning>,
    list_state: ListState,
    focus_handle: FocusHandle,
    tail_task: Task<()>,
    reopen_task: Task<()>,
    baseline_task: Task<()>,
    _ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl LiveTailView {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        store: Entity<CloudWatchStore>,
        log_groups: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("cloudwatch_tail.filter_placeholder"), window, cx);
            editor
        });
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("cloudwatch_tail.search_placeholder"), window, cx);
            editor
        });
        let list_state = ListState::new(0, ListAlignment::Top, px(400.));
        list_state.set_follow_mode(FollowMode::Tail);
        {
            let this = cx.weak_entity();
            list_state.set_scroll_handler(move |_, _, cx| {
                this.update(cx, |_, cx| cx.notify()).log_err();
            });
        }
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                this.handle_store_event(event, cx)
            }),
            cx.subscribe(&filter_editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Blurred) {
                    this.apply_filter(cx);
                }
            }),
            cx.subscribe(&search_editor, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.search_query = editor.read(cx).text(cx).trim().to_string();
                    this.search_dirty = true;
                    this.current_match = None;
                    cx.notify();
                }
            }),
            cx.on_release(|this, cx| this.end_segment(cx)),
        ];
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    return;
                }
            }
        });
        let short_names = patterns::short_names(&log_groups);
        let mut this = Self {
            workspace,
            project,
            store,
            log_groups,
            short_names,
            filter_editor,
            applied_filter: None,
            search_editor,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_dirty: false,
            current_match: None,
            lines: VecDeque::new(),
            pending: VecDeque::new(),
            paused: false,
            total_events: 0,
            sampled: false,
            arrivals: VecDeque::new(),
            state: SessionState::WaitingForConnection,
            opened_at: None,
            segment_started: None,
            last_seen: Instant::now(),
            detector: PatternDetector::default(),
            baseline_requested: false,
            warning: None,
            list_state,
            focus_handle: cx.focus_handle(),
            tail_task: Task::ready(()),
            reopen_task: Task::ready(()),
            baseline_task: Task::ready(()),
            _ticker: ticker,
            _subscriptions: subscriptions,
        };
        this.start_session(cx);
        this.load_baseline(cx);
        this
    }

    fn handle_store_event(&mut self, event: &StoreEvent, cx: &mut Context<Self>) {
        match event {
            StoreEvent::Connected => {
                if matches!(
                    self.state,
                    SessionState::WaitingForConnection | SessionState::Expired
                ) {
                    self.start_session(cx);
                }
                self.load_baseline(cx);
            }
            StoreEvent::SessionExpired => {
                if self.state == SessionState::WaitingForConnection {
                    self.state = SessionState::Expired;
                    cx.notify();
                }
            }
            StoreEvent::TargetChanged => {
                self.stop_session(SessionState::WaitingForConnection, cx);
                self.detector = PatternDetector::default();
                self.baseline_requested = false;
                self.warning = None;
            }
        }
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let text = self.filter_editor.read(cx).text(cx);
        let filter = Some(text.trim().to_string()).filter(|filter| !filter.is_empty());
        if filter != self.applied_filter {
            self.applied_filter = filter;
            self.restart(cx);
        }
    }

    fn set_log_groups(&mut self, log_groups: Vec<String>, cx: &mut Context<Self>) {
        if log_groups.is_empty() || log_groups == self.log_groups {
            return;
        }
        self.log_groups = log_groups;
        self.short_names = patterns::short_names(&self.log_groups);
        self.detector = PatternDetector::default();
        self.baseline_requested = false;
        self.warning = None;
        self.restart(cx);
        self.load_baseline(cx);
    }

    /// A new filter or new groups start a new session with an empty view.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.lines.clear();
        self.pending.clear();
        self.list_state.reset(0);
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.total_events = 0;
        self.arrivals.clear();
        self.search_dirty = true;
        self.current_match = None;
        self.opened_at = None;
        self.start_session(cx);
    }

    fn log_group_arns(&self, cx: &App) -> Result<Vec<String>, String> {
        let store = self.store.read(cx);
        self.log_groups
            .iter()
            .map(|name| {
                let known = store
                    .log_groups()
                    .iter()
                    .find(|group| &group.name == name)
                    .and_then(|group| group.arn.clone());
                if let Some(arn) = known {
                    return Ok(arn);
                }
                match (store.region(), store.account_id()) {
                    (Some(region), Some(account)) => {
                        Ok(format!("arn:aws:logs:{region}:{account}:log-group:{name}"))
                    }
                    _ => Err(name.clone()),
                }
            })
            .collect()
    }

    fn start_session(&mut self, cx: &mut Context<Self>) {
        self.end_segment(cx);
        self.reopen_task = Task::ready(());
        let store = self.store.read(cx);
        let Some(client) = store.client() else {
            self.state = if matches!(store.connection(), Connection::SessionExpired { .. }) {
                SessionState::Expired
            } else {
                SessionState::WaitingForConnection
            };
            self.tail_task = Task::ready(());
            cx.notify();
            return;
        };
        let arns = match self.log_group_arns(cx) {
            Ok(arns) => arns,
            Err(missing) => {
                self.state =
                    SessionState::Failed(t!("cloudwatch_tail.unknown_group", group = missing));
                self.tail_task = Task::ready(());
                cx.notify();
                return;
            }
        };
        let filter = self.applied_filter.clone();
        let (sender, mut receiver) = mpsc::unbounded();
        let tail = Tokio::spawn(cx, async move {
            let mut tail = match client.start_live_tail(&arns, filter.as_deref()).await {
                Ok(tail) => tail,
                Err(error) => {
                    if sender
                        .unbounded_send(TailMessage::Ended(Err(error)))
                        .is_err()
                    {
                        log::debug!("live tail closed before it started");
                    }
                    return;
                }
            };
            loop {
                let message = match tail.next_update().await {
                    Ok(Some(TailUpdate::Started { .. })) => TailMessage::Started,
                    Ok(Some(TailUpdate::Events { events, sampled })) => {
                        TailMessage::Events { events, sampled }
                    }
                    Ok(None) => TailMessage::Ended(Ok(())),
                    Err(error) => TailMessage::Ended(Err(error)),
                };
                let ended = matches!(message, TailMessage::Ended(_));
                if sender.unbounded_send(message).is_err() || ended {
                    return;
                }
            }
        });
        let now = Instant::now();
        self.state = SessionState::Starting;
        self.segment_started = Some(now);
        self.opened_at.get_or_insert(now);
        self.last_seen = now;
        self.tail_task = cx.spawn(async move |this, cx| {
            // Dropping the Tokio task aborts the session, so it lives as long as this one.
            let _tail = tail;
            while let Some(message) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.receive(message, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        cx.notify();
    }

    fn stop_session(&mut self, state: SessionState, cx: &mut Context<Self>) {
        self.end_segment(cx);
        self.tail_task = Task::ready(());
        self.reopen_task = Task::ready(());
        self.opened_at = None;
        self.state = state;
        cx.notify();
    }

    fn end_segment(&mut self, cx: &mut App) {
        if let Some(started) = self.segment_started.take() {
            let seconds = started.elapsed().as_secs();
            if seconds > 0 {
                self.store
                    .update(cx, |store, cx| store.record_tail(seconds, cx));
            }
        }
    }

    fn receive(&mut self, message: TailMessage, cx: &mut Context<Self>) {
        match message {
            TailMessage::Started => {
                self.state = SessionState::Live;
                cx.notify();
            }
            TailMessage::Events { events, sampled } => {
                self.state = SessionState::Live;
                self.sampled = sampled;
                self.push_events(events, cx);
            }
            TailMessage::Ended(result) => {
                self.end_segment(cx);
                match result {
                    Ok(()) | Err(CloudWatchError::TailSessionEnded { .. }) => {
                        self.state = SessionState::Reopening;
                        self.reopen_task = cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(REOPEN_DELAY).await;
                            this.update(cx, |this, cx| this.start_session(cx)).log_err();
                        });
                    }
                    Err(error) => {
                        self.store
                            .update(cx, |store, cx| store.report_error(&error, cx));
                        self.state = if matches!(error, CloudWatchError::SessionExpired { .. }) {
                            SessionState::Expired
                        } else {
                            SessionState::Failed(error.to_string().into())
                        };
                        self.opened_at = None;
                    }
                }
                cx.notify();
            }
        }
    }

    fn push_events(&mut self, events: Vec<LogEvent>, cx: &mut Context<Self>) {
        if events.is_empty() {
            return;
        }
        self.arrivals.push_back((Instant::now(), events.len()));
        let mut fresh = Vec::with_capacity(events.len());
        for event in events {
            let line = self.tail_line(event, cx);
            self.total_events += 1;
            if let Some(pattern) = self.detector.observe(&line.message, line.timestamp_ms) {
                self.announce(pattern, cx);
            }
            fresh.push(line);
        }
        fresh.sort_by_key(|line| line.timestamp_ms);
        if self.paused {
            self.pending.extend(fresh);
        } else {
            let start = self.lines.len();
            let count = fresh.len();
            self.lines.extend(fresh);
            self.list_state.splice(start..start, count);
        }
        self.enforce_memory_limit();
        self.search_dirty = true;
        cx.notify();
    }

    fn enforce_memory_limit(&mut self) {
        let mut dropped = 0;
        while self.lines.len() + self.pending.len() > MAX_IN_MEMORY {
            if self.lines.pop_front().is_some() {
                dropped += 1;
            } else {
                self.pending.pop_front();
            }
        }
        if dropped > 0 {
            self.list_state.splice(0..dropped, 0);
            self.current_match = None;
        }
    }

    fn tail_line(&self, event: LogEvent, cx: &App) -> TailLine {
        let parsed = patterns::parse_message(&event.message);
        let group = event
            .log_group
            .as_deref()
            .map(|identifier| patterns::group_name(identifier).to_string());
        let source = group.as_ref().and_then(|group| {
            self.log_groups
                .iter()
                .position(|candidate| candidate == group)
        });
        let timestamp_ms = event.timestamp_ms.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or_default()
        });
        let stack = parsed
            .stack
            .into_iter()
            .map(|text| {
                let link = patterns::frame_link(&text).and_then(|link| self.resolve_link(link, cx));
                StackFrame { text, link }
            })
            .collect();
        TailLine {
            timestamp_ms,
            time: local_time(timestamp_ms, "%H:%M:%S%.3f").into(),
            group,
            source,
            level: parsed.level,
            message: parsed.message,
            stack,
            raw: event.message,
        }
    }

    /// Containers often prefix paths (`/app/src/…`), so leading components are dropped until a
    /// project file matches.
    fn resolve_link(&self, link: FrameLink, cx: &App) -> Option<ResolvedLink> {
        let project = self.project.read(cx);
        let mut candidate = link.path.trim_start_matches('/');
        loop {
            if let Some(project_path) = project.find_project_path(Path::new(candidate), cx)
                && project
                    .entry_for_path(&project_path, cx)
                    .is_some_and(|entry| entry.is_file())
            {
                return Some(ResolvedLink {
                    range: link.range,
                    project_path,
                    line: link.line,
                    column: link.column,
                });
            }
            candidate = candidate.split_once('/')?.1;
        }
    }

    fn open_link(&self, link: &ResolvedLink, window: &mut Window, cx: &mut App) {
        let point = language::Point::new(
            link.line.saturating_sub(1),
            link.column.unwrap_or(1).saturating_sub(1),
        );
        let Some(open) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.open_path(link.project_path.clone(), None, true, window, cx)
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

    fn toggle_pause(&mut self, _: &TogglePause, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_open() {
            self.paused = false;
            self.flush_pending();
            self.start_session(cx);
            return;
        }
        self.paused = !self.paused;
        if !self.paused {
            self.flush_pending();
        }
        cx.notify();
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let start = self.lines.len();
        let count = self.pending.len();
        self.lines.extend(self.pending.drain(..));
        self.list_state.splice(start..start, count);
        self.search_dirty = true;
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_open() {
            return;
        }
        if self.last_seen.elapsed() >= IDLE_CLOSE {
            self.stop_session(SessionState::ClosedIdle, cx);
            return;
        }
        while self
            .arrivals
            .front()
            .is_some_and(|(arrival, _)| arrival.elapsed() > RATE_WINDOW)
        {
            self.arrivals.pop_front();
        }
        cx.notify();
    }

    fn rate(&self) -> usize {
        let count: usize = self
            .arrivals
            .iter()
            .filter(|(arrival, _)| arrival.elapsed() <= RATE_WINDOW)
            .map(|(_, count)| count)
            .sum();
        let window = self
            .opened_at
            .map_or(RATE_WINDOW, |opened| opened.elapsed().min(RATE_WINDOW))
            .as_secs_f64()
            .max(1.);
        (count as f64 / window).round() as usize
    }

    fn load_baseline(&mut self, cx: &mut Context<Self>) {
        if self.baseline_requested {
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        self.baseline_requested = true;
        let groups = self.log_groups.clone();
        let load = Tokio::spawn(cx, async move {
            let now = SystemTime::now();
            let mut messages = Vec::new();
            for group in &groups {
                for slice in 0..BASELINE_SLICES {
                    let start = now - BASELINE_WINDOW + BASELINE_WINDOW / BASELINE_SLICES * slice;
                    let events = client
                        .filter_log_events(group, None, start, BASELINE_EVENTS_PER_SLICE)
                        .await?;
                    messages.extend(
                        events
                            .into_iter()
                            .map(|event| patterns::parse_message(&event.message).message),
                    );
                }
            }
            Ok::<_, CloudWatchError>(messages)
        });
        self.baseline_task = cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(result) => result,
                Err(error) => Err(CloudWatchError::Other(error.into())),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(messages) => this
                        .detector
                        .set_baseline(messages.iter().map(String::as_str)),
                    Err(error) => {
                        log::warn!("live tail baseline failed: {error}");
                        this.baseline_requested = false;
                        this.store
                            .update(cx, |store, cx| store.report_error(&error, cx));
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn announce(&mut self, pattern: NewPattern, cx: &mut Context<Self>) {
        let context = self.recent_lines(PATTERN_CONTEXT_LINES);
        let language = match i18n::current_language() {
            i18n::Language::PortugueseBrazil => "Brazilian Portuguese",
            i18n::Language::English => "English",
        };
        let system = format!(
            "You watch a live tail of application logs. A message template that did not appear \
             in the last 24 hours just showed up several times. Write one or two short sentences \
             in {language} for a developer: quote the message without its ids, say it is new in \
             the last 24 h and how often it came, and point out what it seems related to in the \
             surrounding lines, if anything. No preamble, no markdown."
        );
        let prompt = format!(
            "New message: {}\nTemplate: {}\nOccurrences in the last 2 minutes: {}\nFirst seen at: {}\n\nSurrounding lines:\n{}",
            pattern.example,
            pattern.template,
            pattern.hits,
            local_time(pattern.first_seen_ms, "%H:%M:%S"),
            context.join("\n")
        );
        let template = pattern.template.clone();
        let completion = llm::complete(system, prompt, cx);
        let summary = cx.spawn(async move |this, cx| {
            let sentence = completion.await;
            this.update(cx, |this, cx| {
                match sentence {
                    Ok(sentence) if !sentence.trim().is_empty() => {
                        if let Some(warning) = this
                            .warning
                            .as_mut()
                            .filter(|warning| warning.pattern.template == template)
                        {
                            warning.sentence = Some(sentence.trim().to_string());
                        }
                    }
                    Ok(_) => {}
                    Err(error) => log::info!("new pattern sentence without a model: {error}"),
                }
                cx.notify();
            })
            .log_err();
        });
        self.warning = Some(PatternWarning {
            pattern,
            detected_at: Instant::now(),
            sentence: None,
            context,
            _summary: summary,
        });
    }

    fn line_text(&self, line: &TailLine) -> String {
        format!(
            "{} {} {} {}",
            line.time,
            line.group.as_deref().unwrap_or("-"),
            line.level.map_or("-", Level::label),
            line.raw.trim_end().replace('\n', "\n    ")
        )
    }

    fn recent_lines(&self, count: usize) -> Vec<String> {
        let skip = (self.lines.len() + self.pending.len()).saturating_sub(count);
        self.lines
            .iter()
            .chain(self.pending.iter())
            .skip(skip)
            .map(|line| self.line_text(line))
            .collect()
    }

    fn send_to_agent(
        &self,
        lines: Vec<String>,
        prompt: Option<String>,
        submit: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        agent_bridge::send_to_agent(
            AgentContext::TailWindow {
                log_groups: self.log_groups.clone(),
                lines,
            },
            t!("cloudwatch_tail.mention_title", group = self.title_name()),
            prompt,
            submit,
            window,
            cx,
        );
    }

    fn investigate(&self, window: &mut Window, cx: &mut App) {
        let Some(warning) = &self.warning else {
            return;
        };
        let prompt = t!(
            "cloudwatch_tail.investigate_prompt",
            message = warning.pattern.example,
            time = local_time(warning.pattern.first_seen_ms, "%H:%M:%S")
        );
        self.send_to_agent(
            warning.context.clone(),
            Some(prompt.to_string()),
            true,
            window,
            cx,
        );
    }

    fn build_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(warning) = &self.warning else {
            return;
        };
        let Some(directory) = self.store.read(cx).queries_dir(cx) else {
            return;
        };
        let fragment = patterns::literal_fragment(&warning.pattern.template);
        let since = local_time(warning.pattern.first_seen_ms, "%H:%M:%S");
        let body = format!(
            "# {}\n# {}\nfields @timestamp, @logStream, @message\n| filter @message like /{}/\n| sort @timestamp desc\n| limit 200\n",
            t!("cloudwatch_tail.query_comment", time = since),
            self.log_groups.join(", "),
            patterns::insights_regex(&fragment)
        );
        let name = format!(
            "{}_{}.{QUERY_EXTENSION}",
            t!("cloudwatch_tail.query_file_name"),
            local_time(warning.pattern.first_seen_ms, "%H%M%S")
        );
        let fs = self.project.read(cx).fs().clone();
        let workspace = self.workspace.clone();
        let store = self.store.clone();
        cx.spawn_in(window, async move |_, cx| {
            fs.create_dir(&directory).await?;
            let path = directory.join(name);
            fs.atomic_write(path.clone(), body).await?;
            store.update(cx, |store, cx| store.reload_project_files(cx));
            workspace.update_in(cx, |workspace, window, cx| {
                query_view::open_query(workspace, path, window, cx)
            })?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err(
            &t!("cloudwatch_tail.query_failed"),
            window,
            cx,
            |_, _, _| None,
        );
    }

    fn save_as_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .lines
            .iter()
            .chain(self.pending.iter())
            .map(|line| self.line_text(line))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let directory = self
            .store
            .read(cx)
            .project_root(cx)
            .unwrap_or_else(|| util::paths::home_dir().clone());
        let name = format!(
            "{}-{}.log",
            self.title_name(),
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        let fs = self.project.read(cx).fs().clone();
        cx.spawn_in(window, async move |_, _| {
            let Some(path) = chosen.await?? else {
                return anyhow::Ok(());
            };
            fs.atomic_write(path, text).await
        })
        .detach_and_prompt_err(
            &t!("cloudwatch_tail.save_failed"),
            window,
            cx,
            |_, _, _| None,
        );
    }

    fn refresh_matches(&mut self) {
        if !self.search_dirty {
            return;
        }
        self.search_dirty = false;
        self.search_matches.clear();
        if self.search_query.is_empty() {
            self.current_match = None;
            return;
        }
        for (index, line) in self.lines.iter().enumerate() {
            let parts = std::iter::once(line.message.as_str())
                .chain(line.stack.iter().map(|frame| frame.text.as_str()));
            for (part, text) in parts.enumerate() {
                for range in patterns::find_matches(text, &self.search_query) {
                    self.search_matches.push(SearchMatch {
                        line: index,
                        part,
                        start: range.start,
                        end: range.end,
                    });
                }
            }
        }
        if self
            .current_match
            .is_some_and(|current| current >= self.search_matches.len())
        {
            self.current_match = None;
        }
    }

    fn go_to_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.refresh_matches();
        let count = self.search_matches.len();
        if count == 0 {
            return;
        }
        let next = match (self.current_match, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(current), true) => (current + 1) % count,
            (Some(current), false) => (current + count - 1) % count,
        };
        self.current_match = Some(next);
        if let Some(found) = self.search_matches.get(next) {
            self.list_state.pause_following_tail();
            self.list_state.scroll_to_reveal_item(found.line);
        }
        cx.notify();
    }

    fn title_name(&self) -> String {
        self.short_names
            .first()
            .cloned()
            .unwrap_or_else(|| "cloudwatch".to_string())
    }

    fn source_color(&self, index: usize, cx: &App) -> Hsla {
        let accents = cx.theme().accents();
        if accents.0.is_empty() {
            cx.theme().colors().text_accent
        } else {
            accents.color_for_index(index as u32)
        }
    }

    fn matches_for(&self, line: usize, part: usize) -> Vec<(Range<usize>, bool)> {
        let first = self
            .search_matches
            .partition_point(|found| found.line < line);
        self.search_matches
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(_, found)| found.line == line)
            .filter(|(_, found)| found.part == part)
            .map(|(index, found)| (found.start..found.end, self.current_match == Some(index)))
            .collect()
    }

    fn search_highlights(
        &self,
        line: usize,
        part: usize,
        cx: &App,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let warning = Color::Warning.color(cx);
        self.matches_for(line, part)
            .into_iter()
            .map(|(range, current)| {
                (
                    range,
                    HighlightStyle {
                        background_color: Some(warning.opacity(if current { 0.6 } else { 0.28 })),
                        ..HighlightStyle::default()
                    },
                )
            })
            .collect()
    }

    fn render_line(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(line) = self.lines.get(index) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let level_color = match line.level {
            Some(Level::Error) => Color::Error,
            Some(Level::Warn) => Color::Warning,
            Some(Level::Info) => Color::Info,
            Some(Level::Debug) | Some(Level::Trace) | None => Color::Muted,
        };
        let emphasized = matches!(line.level, Some(Level::Error) | Some(Level::Warn));
        let message_color = if emphasized {
            colors.text
        } else {
            colors.text_muted
        };
        let source = line.source.and_then(|source| {
            self.short_names
                .get(source)
                .map(|name| (name.clone(), self.source_color(source, cx)))
        });
        let message = StyledText::new(line.message.clone())
            .with_highlights(self.search_highlights(index, 0, cx));
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .h(px(22.))
                    .px(px(14.))
                    .when(line.level == Some(Level::Error), |this| {
                        this.bg(Color::Error.color(cx).opacity(0.08))
                    })
                    .child(
                        div().w(px(102.)).flex_none().child(
                            Label::new(line.time.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Disabled),
                        ),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .flex_none()
                            .when_some(source, |this, (name, color)| {
                                this.child(
                                    div()
                                        .w(px(76.))
                                        .h(px(18.))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .rounded(px(4.))
                                        .bg(color.opacity(0.14))
                                        .child(
                                            Label::new(name)
                                                .size(LabelSize::XSmall)
                                                .buffer_font(cx)
                                                .color(Color::Custom(color))
                                                .truncate(),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w(px(52.))
                            .flex_none()
                            .when_some(line.level, |this, level| {
                                this.child(
                                    Label::new(level.label())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(level_color),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_buffer(cx)
                            .text_size(rems_from_px(11.5_f32))
                            .text_color(message_color)
                            .child(message),
                    ),
            )
            .children(
                line.stack
                    .iter()
                    .enumerate()
                    .map(|(frame_index, frame)| self.render_frame(index, frame_index, frame, cx)),
            )
            .into_any_element()
    }

    fn render_frame(
        &self,
        line: usize,
        frame_index: usize,
        frame: &StackFrame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let search = self.search_highlights(line, frame_index + 1, cx);
        let accent = cx.theme().colors().text_accent;
        let link_highlight = frame.link.as_ref().map(|link| {
            (
                link.range.clone(),
                HighlightStyle {
                    color: Some(accent),
                    ..HighlightStyle::default()
                },
            )
        });
        let highlights: Vec<_> = combine_highlights(link_highlight, search).collect();
        let text = StyledText::new(frame.text.clone()).with_highlights(highlights);
        let content = match frame.link.clone() {
            Some(link) => {
                let this = cx.weak_entity();
                InteractiveText::new(
                    ElementId::named_usize(format!("cloudwatch-tail-frame-{frame_index}"), line),
                    text,
                )
                .on_click(vec![link.range.clone()], move |_, window, cx| {
                    this.update(cx, |this, cx| this.open_link(&link, window, cx))
                        .log_err();
                })
                .into_any_element()
            }
            None => text.into_any_element(),
        };
        div()
            .h(px(20.))
            .pl(px(120.))
            .pr(px(14.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .font_buffer(cx)
            .text_size(rems_from_px(11.5_f32))
            .text_color(cx.theme().colors().text_muted)
            .child(content)
            .into_any_element()
    }

    fn render_group_select(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let first = self.log_groups.first().cloned().unwrap_or_default();
        let extra = self.log_groups.len().saturating_sub(1);
        let store = self.store.read(cx);
        let mut choices: Vec<String> = store.project_groups().to_vec();
        for group in &self.log_groups {
            if !choices.contains(group) {
                choices.push(group.clone());
            }
        }
        let selected = self.log_groups.clone();
        let this = cx.weak_entity();
        PopoverMenu::new("cloudwatch-tail-groups")
            .trigger(PopoverTriggerArea::new(
                text_field(false, cx)
                    .id("cloudwatch-tail-groups-trigger")
                    .w(px(262.))
                    .flex_none()
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        Icon::new(IconName::CloudPulse)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(first)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .when(extra > 0, |this| {
                        this.child(
                            div()
                                .px(px(6.))
                                .rounded(px(6.))
                                .bg(cx.theme().colors().element_selected)
                                .child(
                                    Label::new(format!("+{extra}"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            ))
            .menu(move |window, cx| {
                let choices = choices.clone();
                let selected = selected.clone();
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for group in choices {
                        let toggled = selected.contains(&group);
                        let this = this.clone();
                        let selected = selected.clone();
                        menu = menu.toggleable_entry(
                            group.clone(),
                            toggled,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                let mut groups = selected.clone();
                                if toggled {
                                    groups.retain(|existing| existing != &group);
                                } else {
                                    groups.push(group.clone());
                                }
                                this.update(cx, |this, cx| this.set_log_groups(groups, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_focused = self
            .filter_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        let paused_or_closed = self.paused || !self.state.is_open();
        let (tone, status) = match &self.state {
            SessionState::Live => (
                StatusTone::Error,
                t!("cloudwatch_tail.live", rate = format_count(self.rate())),
            ),
            SessionState::Starting | SessionState::WaitingForConnection => {
                (StatusTone::Neutral, t!("cloudwatch_tail.connecting"))
            }
            SessionState::Reopening => (StatusTone::Warning, t!("cloudwatch_tail.reopening")),
            SessionState::ClosedIdle => (StatusTone::Neutral, t!("cloudwatch_tail.closed")),
            SessionState::Expired => (StatusTone::Warning, t!("cloudwatch_tail.expired")),
            SessionState::Failed(_) => (StatusTone::Error, t!("cloudwatch_tail.failed")),
        };
        let accent = cx.theme().colors().border_focused;
        h_flex()
            .px(px(12.))
            .pt(px(10.))
            .gap_2()
            .child(self.render_group_select(cx))
            .child(
                div()
                    .key_context("CloudWatchTailFilter")
                    .on_action(cx.listener(|this, _: &ApplyFilter, _, cx| this.apply_filter(cx)))
                    .w(px(360.))
                    .min_w_0()
                    .child(
                        text_field(filter_focused, cx)
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Filter)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1().min_w_0().child(self.filter_editor.clone())),
                    ),
            )
            .child(div().flex_1())
            .when(self.detector.is_watching(), |this| {
                this.child(
                    div()
                        .id("cloudwatch-tail-watching")
                        .child(
                            status_chip(
                                Some(IconName::Sparkle),
                                t!("cloudwatch_tail.watching"),
                                Color::Accent,
                                cx,
                            )
                            .bg(accent.opacity(0.15))
                            .border_color(accent),
                        )
                        .tooltip(Tooltip::text(t!("cloudwatch_tail.watching_tooltip"))),
                )
            })
            .child(
                div()
                    .id("cloudwatch-tail-status")
                    .child(status_pill(status, tone, cx))
                    .when(self.sampled && self.state == SessionState::Live, |this| {
                        this.tooltip(Tooltip::text(t!("cloudwatch_tail.sampled_tooltip")))
                    }),
            )
            .child(
                secondary_button(
                    "cloudwatch-tail-pause",
                    Some((
                        if paused_or_closed {
                            IconName::PlayFilled
                        } else {
                            IconName::DebugPause
                        },
                        Color::Default,
                    )),
                    if paused_or_closed {
                        t!("cloudwatch_tail.resume")
                    } else {
                        t!("cloudwatch_tail.pause")
                    },
                    true,
                    cx,
                )
                .h(px(32.))
                .child(
                    KeyBinding::for_action_in(&TogglePause, &self.focus_handle, cx)
                        .size(rems_from_px(11_f32)),
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.toggle_pause(&TogglePause, window, cx)),
                ),
            )
    }

    fn render_info_line(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (text, color) = match &self.state {
            SessionState::Failed(error) => (error.clone(), Color::Error),
            SessionState::ClosedIdle => (t!("cloudwatch_tail.closed_idle"), Color::Disabled),
            SessionState::Expired => (t!("cloudwatch_tail.expired_info"), Color::Disabled),
            SessionState::WaitingForConnection => {
                (t!("cloudwatch_tail.waiting_info"), Color::Disabled)
            }
            _ => (
                t!(
                    "cloudwatch_tail.session_info",
                    duration = format_duration(
                        self.opened_at
                            .map_or(Duration::ZERO, |opened| opened.elapsed())
                    )
                ),
                Color::Disabled,
            ),
        };
        self.refresh_matches();
        let search_focused = self
            .search_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        let counter = (!self.search_query.is_empty()).then(|| {
            t!(
                "cloudwatch_tail.search_count",
                current = self.current_match.map_or(0, |current| current + 1),
                total = format_count(self.search_matches.len())
            )
        });
        h_flex()
            .px(px(14.))
            .py(px(6.))
            .gap_2()
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(text)
                        .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                        .color(color)
                        .truncate(),
                ),
            )
            .child(
                div()
                    .key_context("CloudWatchTailSearch")
                    .on_action(cx.listener(|this, _: &NextMatch, _, cx| this.go_to_match(true, cx)))
                    .on_action(
                        cx.listener(|this, _: &PreviousMatch, _, cx| this.go_to_match(false, cx)),
                    )
                    .w(px(180.))
                    .flex_none()
                    .child(
                        text_field(search_focused, cx)
                            .h(px(22.))
                            .px(px(7.))
                            .gap_1()
                            .rounded(px(6.))
                            .child(
                                Icon::new(IconName::MagnifyingGlass)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1().min_w_0().child(self.search_editor.clone()))
                            .when_some(counter, |this, counter| {
                                this.child(
                                    div()
                                        .id("cloudwatch-tail-search-count")
                                        .cursor_pointer()
                                        .child(
                                            Label::new(counter)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.go_to_match(true, cx)
                                            }),
                                        ),
                                )
                            }),
                    ),
            )
    }

    fn render_warning(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let warning = self.warning.as_ref()?;
        let accent = cx.theme().colors().border_focused;
        let since = local_time(warning.pattern.first_seen_ms, "%H:%M:%S");
        let age = warning.detected_at.elapsed().as_secs() / 60;
        let age = if age == 0 {
            t!("cloudwatch_tail.just_now")
        } else {
            t!("cloudwatch_tail.minutes_ago", minutes = age)
        };
        let sentence = warning.sentence.clone().unwrap_or_else(|| {
            t!(
                "cloudwatch_tail.pattern_fallback",
                message = warning
                    .pattern
                    .example
                    .chars()
                    .take(160)
                    .collect::<String>(),
                count = warning.pattern.hits
            )
            .to_string()
        });
        let template = warning.pattern.template.clone();
        Some(
            h_flex()
                .mx(px(12.))
                .mb(px(10.))
                .p(px(14.))
                .items_start()
                .gap(px(8.))
                .rounded(px(10.))
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.06))
                .child(
                    Icon::new(IconName::Sparkle)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.))
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    Label::new(t!("cloudwatch_tail.new_pattern", time = since))
                                        .weight(FontWeight::SEMIBOLD),
                                )
                                .child(
                                    Label::new(age)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                ),
                        )
                        .child(Label::new(sentence).size(LabelSize::Small))
                        .child(
                            h_flex()
                                .pt_1()
                                .gap_2()
                                .child(
                                    secondary_button(
                                        "cloudwatch-tail-investigate",
                                        Some((IconName::Chat, Color::Default)),
                                        t!("cloudwatch_tail.investigate"),
                                        true,
                                        cx,
                                    )
                                    .h(px(26.))
                                    .bg(accent.opacity(0.2))
                                    .border_color(accent)
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.investigate(window, cx),
                                    )),
                                )
                                .child(
                                    secondary_button(
                                        "cloudwatch-tail-build-query",
                                        Some((IconName::CloudPulse, Color::Default)),
                                        t!("cloudwatch_tail.build_query"),
                                        true,
                                        cx,
                                    )
                                    .h(px(26.))
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.build_query(window, cx),
                                    )),
                                )
                                .child(
                                    secondary_button(
                                        "cloudwatch-tail-silence",
                                        Some((IconName::EyeOff, Color::Default)),
                                        t!("cloudwatch_tail.silence"),
                                        true,
                                        cx,
                                    )
                                    .h(px(26.))
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.detector.silence(&template);
                                            this.warning = None;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let following = self.list_state.is_following_tail();
        let (state, hint) = if self.paused {
            (
                t!("cloudwatch_tail.state_paused"),
                t!(
                    "cloudwatch_tail.hint_paused",
                    count = format_count(self.pending.len())
                ),
            )
        } else if following {
            (
                t!("cloudwatch_tail.state_following"),
                t!("cloudwatch_tail.hint_following"),
            )
        } else {
            (
                t!("cloudwatch_tail.state_scrolled"),
                t!("cloudwatch_tail.hint_scrolled"),
            )
        };
        let tone = if self.state == SessionState::Live && !self.paused {
            StatusTone::Error
        } else {
            StatusTone::Neutral
        };
        let has_lines = !self.lines.is_empty() || !self.pending.is_empty();
        h_flex()
            .h(px(42.))
            .px(px(14.))
            .gap(px(8.))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(tone.dot(cx))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(t!(
                        "cloudwatch_tail.footer",
                        state = state,
                        total = format_count(self.total_events),
                        kept = format_count(self.lines.len() + self.pending.len()),
                        max = format_count(MAX_IN_MEMORY),
                        hint = hint
                    ))
                    .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                    .color(Color::Muted)
                    .truncate(),
                ),
            )
            .child(
                secondary_button(
                    "cloudwatch-tail-save",
                    Some((IconName::Download, Color::Default)),
                    t!("cloudwatch_tail.save_log"),
                    has_lines,
                    cx,
                )
                .h(px(28.))
                .when(has_lines, |this| {
                    this.on_click(cx.listener(|this, _, window, cx| this.save_as_log(window, cx)))
                }),
            )
            .child(
                secondary_button(
                    "cloudwatch-tail-agent",
                    Some((IconName::Sparkle, Color::Default)),
                    t!("cloudwatch_tail.send_to_agent"),
                    has_lines,
                    cx,
                )
                .h(px(28.))
                .when(has_lines, |this| {
                    this.on_click(cx.listener(|this, _, window, cx| {
                        this.send_to_agent(this.recent_lines(AGENT_LINES), None, false, window, cx)
                    }))
                }),
            )
    }

    fn render_expired(&self, cx: &mut Context<Self>) -> AnyElement {
        let profile = self.store.read(cx).profile_name();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(
                EmptyState::new(IconName::Warning, t!("cloudwatch_tail.expired_title"))
                    .tone(EmptyStateTone::Error)
                    .description(t!("cloudwatch_tail.expired_description", profile = profile))
                    .action(
                        primary_button(
                            "cloudwatch-tail-aws-login",
                            Some(IconName::Terminal),
                            t!("cloudwatch_tail.run_aws_login"),
                            None,
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            let store = store.clone();
                            workspace
                                .update(cx, |workspace, cx| {
                                    aws_login::run_aws_login(workspace, store, window, cx)
                                })
                                .log_err();
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for LiveTailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.is_window_active() {
            self.last_seen = Instant::now();
        }
        let panel_background = cx.theme().colors().panel_background;
        let border = cx.theme().colors().border_variant;
        let stream_background = cx.theme().colors().editor_background;
        let info_line = self.render_info_line(window, cx).into_any_element();
        let stream = if self.state == SessionState::Expired && self.lines.is_empty() {
            self.render_expired(cx)
        } else {
            div()
                .flex_1()
                .min_h_0()
                .pt(px(8.))
                .child(
                    list(
                        self.list_state.clone(),
                        cx.processor(|this, index, _window, cx| this.render_line(index, cx)),
                    )
                    .size_full(),
                )
                .into_any_element()
        };
        v_flex()
            .key_context("CloudWatchLiveTail")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_pause))
            .size_full()
            .bg(panel_background)
            .child(self.render_toolbar(window, cx))
            .child(info_line)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .mx(px(12.))
                    .mb(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .bg(stream_background)
                    .overflow_hidden()
                    .when(
                        self.state == SessionState::Expired && !self.lines.is_empty(),
                        |this| this.child(div().p(px(12.)).child(self.render_expired(cx))),
                    )
                    .child(stream)
                    .children(self.render_warning(cx))
                    .child(self.render_footer(cx)),
            )
    }
}

impl Focusable for LiveTailView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for LiveTailView {}

impl Item for LiveTailView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        t!("cloudwatch_tail.tab_title", group = self.title_name())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.log_groups.join(", ").into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::List).color(Color::Accent))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

fn local_time(timestamp_ms: i64, format: &str) -> String {
    chrono::Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|time| time.format(format).to_string())
        .unwrap_or_default()
}

fn portuguese_numbers() -> bool {
    i18n::current_language() == i18n::Language::PortugueseBrazil
}

/// `3112` → `3,112` (`3.112` in Portuguese).
fn format_count(count: usize) -> String {
    let separator = if portuguese_numbers() { '.' } else { ',' };
    let digits = count.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(separator);
        }
        result.push(digit);
    }
    result
}

/// `222 s` → `3 min 42 s`.
fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} s")
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42 s");
        assert_eq!(format_duration(Duration::from_secs(222)), "3 min 42 s");
    }
}
