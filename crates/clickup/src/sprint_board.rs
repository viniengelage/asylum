use crate::{
    ClickUpPanel, ClickUpStore, Connection, api,
    panel::{
        day_and_month, parse_color, priority_pill, status_dot, sync_status_label, task_matches,
        timer_started_at,
    },
};
use anyhow::Result;
use chrono::{Datelike as _, Local, TimeZone as _};
use db::kvp::KeyValueStore;
use gpui::{
    Entity, EventEmitter, FocusHandle, Focusable, FontWeight, PromptLevel, Subscription, Task,
    WeakEntity, px,
};
use http_client::HttpClient;
use i18n::t;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use ui::{
    BoardColumn, BoardDragPreview, EmptyState, EmptyStateTone, HeaderBar, HeaderBarLevel,
    board_card, board_row, initial_tile, prelude::*, secondary_button,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    item::{Item as WorkspaceItem, ItemEvent},
    notifications::NotificationId,
};

/// Done and closed tasks are history; the board shows the most recent few.
const DONE_TASKS_SHOWN: usize = 4;

/// Whether moving a card into an in-progress status takes the timer from another task.
const REPLACE_RUNNING_TIMER_KEY: &str = "clickup_board_replace_running_timer";

/// ClickUp's status type for the in-progress statuses.
const IN_PROGRESS_KIND: &str = "custom";

/// Opens the sprint board, or brings the open one forward. `list_id` picks the list to show
/// instead of working out the current sprint.
pub fn open(
    workspace: &mut Workspace,
    list_id: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<SprintBoard>(cx).next();
    if let Some(board) = existing {
        board.update(cx, |board, cx| board.show_list(list_id, cx));
        workspace.activate_item(&board, true, true, window, cx);
        return;
    }
    let store = ClickUpStore::global(cx);
    let weak_workspace = workspace.weak_handle();
    let board = cx.new(|cx| SprintBoard::new(store, weak_workspace, list_id, window, cx));
    workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
}

#[derive(Clone)]
struct DraggedTask {
    id: String,
    name: SharedString,
}

enum BoardState {
    Loading,
    NoSprint,
    Failed(SharedString),
    Ready,
}

pub struct SprintBoard {
    focus_handle: FocusHandle,
    store: Entity<ClickUpStore>,
    workspace: WeakEntity<Workspace>,
    /// The list asked for from the dock; without one the board looks for the current sprint.
    list_hint: Option<String>,
    state: BoardState,
    list: Option<api::List>,
    tasks: Vec<api::Task>,
    /// Tasks whose move is still on its way to ClickUp.
    pending: HashSet<String>,
    show_all_done: bool,
    filter_input: Entity<InputField>,
    /// The panel the last clicked card opened in, whose open task is the selected card.
    panel: Option<Entity<ClickUpPanel>>,
    /// The store sync the board last reloaded after.
    seen_sync: Option<Instant>,
    load_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SprintBoard {
    fn new(
        store: Entity<ClickUpStore>,
        workspace: WeakEntity<Workspace>,
        list_hint: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("clickup.tasks.filter_placeholder"))
                .start_icon(IconName::MagnifyingGlass)
        });
        let mut subscriptions = vec![cx.observe(&store, |this, store, cx| {
            let synced_at = store.read(cx).tasks().synced_at;
            if synced_at != this.seen_sync {
                this.reload(cx);
            }
            cx.notify();
        })];
        let editor = filter_input.read(cx).editor().clone();
        let this = cx.weak_entity();
        subscriptions.push(editor.subscribe(
            Box::new(move |event, _window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    this.update(cx, |_, cx| cx.notify()).log_err();
                }
            }),
            window,
            cx,
        ));
        let mut board = Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            list_hint,
            state: BoardState::Loading,
            list: None,
            tasks: Vec::new(),
            pending: HashSet::default(),
            show_all_done: false,
            filter_input,
            panel: None,
            seen_sync: None,
            load_task: None,
            _subscriptions: subscriptions,
        };
        board.reload(cx);
        board
    }

    fn show_list(&mut self, list_id: Option<String>, cx: &mut Context<Self>) {
        if list_id == self.list_hint {
            return;
        }
        self.list_hint = list_id;
        self.state = BoardState::Loading;
        self.list = None;
        self.tasks.clear();
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some((http_client, token, _)) = store.request_context() else {
            return;
        };
        let assigned = store.tasks();
        self.seen_sync = assigned.synced_at;
        let candidate = match &self.list_hint {
            Some(list_id) => Some((list_id.clone(), true)),
            None => assigned
                .open
                .iter()
                .find(|task| task.status.kind == IN_PROGRESS_KIND)
                .or_else(|| assigned.open.first())
                .map(|task| (task.list.id.clone(), false)),
        };
        let Some((list_id, explicit)) = candidate else {
            // Until the first sync there is no task to find the sprint from.
            if assigned.synced_at.is_some() {
                self.state = BoardState::NoSprint;
            }
            cx.notify();
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let list = api::get_list(&http_client, &token, &list_id).await?;
                    let list = if explicit {
                        list
                    } else {
                        current_sprint(&http_client, &token, list).await?
                    };
                    let tasks = api::get_list_tasks(&http_client, &token, &list.id).await?;
                    anyhow::Ok((list, tasks))
                })
                .await;
            this.update(cx, |this, cx| this.finish_load(result, cx))
                .log_err();
        }));
    }

    fn finish_load(&mut self, result: Result<(api::List, Vec<api::Task>)>, cx: &mut Context<Self>) {
        match result {
            Ok((list, mut tasks)) => {
                // A reload can land before ClickUp has the move; the card stays where it was
                // dropped until the move settles.
                for task in &mut tasks {
                    if self.pending.contains(&task.id)
                        && let Some(local) = self.tasks.iter().find(|local| local.id == task.id)
                    {
                        task.status = local.status.clone();
                    }
                }
                self.list = Some(list);
                self.tasks = tasks;
                self.state = BoardState::Ready;
            }
            Err(error) => {
                log::warn!("ClickUp: falha ao carregar o sprint: {error:#}");
                if self.list.is_none() {
                    self.state = BoardState::Failed(format!("{error:#}").into());
                }
            }
        }
        cx.notify();
    }

    fn move_task(
        &mut self,
        task_id: String,
        status: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(new_status) = self.list.as_ref().and_then(|list| {
            list.statuses
                .iter()
                .find(|candidate| candidate.status == status)
                .cloned()
        }) else {
            return;
        };
        let Some((http_client, token, _)) = self.store.read(cx).request_context() else {
            return;
        };
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) else {
            return;
        };
        if task.status.status.eq_ignore_ascii_case(&new_status.status) {
            return;
        }
        let previous = std::mem::replace(&mut task.status, new_status.clone());
        let task_name = task.name.clone();
        self.pending.insert(task_id.clone());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn({
                    let task_id = task_id.clone();
                    let status = new_status.status.clone();
                    async move { api::set_task_status(&http_client, &token, &task_id, &status).await }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.pending.remove(&task_id);
                match result {
                    Ok(()) => {
                        this.follow_with_timer(task_id, task_name, &previous, &new_status, window, cx)
                    }
                    Err(error) => {
                        if let Some(task) = this.tasks.iter_mut().find(|task| task.id == task_id) {
                            task.status = previous;
                        }
                        this.show_toast(
                            t!(
                                "clickup.board.move_failed",
                                task = task_name,
                                status = new_status.status,
                                error = format!("{error:#}")
                            ),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Entering an in-progress status starts the task's timer and leaving one stops it, so the
    /// time tracked follows the board.
    fn follow_with_timer(
        &mut self,
        task_id: String,
        task_name: String,
        previous: &api::TaskStatus,
        new_status: &api::TaskStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let starts = new_status.kind == IN_PROGRESS_KIND && previous.kind != IN_PROGRESS_KIND;
        let stops = previous.kind == IN_PROGRESS_KIND && new_status.kind != IN_PROGRESS_KIND;
        let store = self.store.read(cx);
        let timing_this_task = timer_started_at(store, &task_id).is_some();
        let running_elsewhere = store
            .running_timer()
            .and_then(|timer| timer.task.as_ref())
            .filter(|running| running.id != task_id)
            .map(|running| {
                self.tasks
                    .iter()
                    .chain(&store.tasks().open)
                    .find(|task| task.id == running.id)
                    .map_or_else(|| running.id.clone(), |task| task.name.clone())
            });
        if stops && timing_this_task {
            let stop = self.store.update(cx, |store, cx| store.stop_timer(cx));
            self.report_timer(stop, cx);
            return;
        }
        if !starts {
            self.store.update(cx, |store, cx| store.sync(cx));
            return;
        }
        let Some(running_name) = running_elsewhere else {
            self.start_timer(task_id, cx);
            return;
        };
        let remembered = KeyValueStore::global(cx)
            .read_kvp(REPLACE_RUNNING_TIMER_KEY)
            .log_err()
            .flatten();
        match remembered.as_deref() {
            Some("true") => self.start_timer(task_id, cx),
            Some(_) => self.store.update(cx, |store, cx| store.sync(cx)),
            None => {
                let detail = t!(
                    "clickup.board.timer_prompt_detail",
                    task = task_name,
                    status = new_status.status,
                    running = running_name
                );
                let replace_label = t!("clickup.board.timer_replace");
                let keep_label = t!("clickup.board.timer_keep");
                let answer = window.prompt(
                    PromptLevel::Info,
                    &t!("clickup.board.timer_prompt"),
                    Some(&*detail),
                    &[&*replace_label, &*keep_label],
                    cx,
                );
                let kvp = KeyValueStore::global(cx);
                cx.spawn(async move |this, cx| {
                    let replace = answer.await? == 0;
                    kvp.write_kvp(REPLACE_RUNNING_TIMER_KEY.to_string(), replace.to_string())
                        .await?;
                    this.update(cx, |this, cx| {
                        if replace {
                            this.start_timer(task_id, cx);
                        } else {
                            this.store.update(cx, |store, cx| store.sync(cx));
                        }
                    })
                })
                .detach_and_log_err(cx);
            }
        }
    }

    fn start_timer(&mut self, task_id: String, cx: &mut Context<Self>) {
        let start = self
            .store
            .update(cx, |store, cx| store.start_timer(task_id, cx));
        self.report_timer(start, cx);
    }

    fn report_timer(&mut self, task: Task<Result<()>>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.show_toast(
                        t!("clickup.task.timer_failed", error = format!("{error:#}")),
                        cx,
                    )
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_toast(&self, message: SharedString, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<SprintBoard>(), message.to_string())
                        .autohide(),
                    cx,
                )
            })
            .log_err();
    }

    fn inspect(&mut self, task: api::Task, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let board = cx.weak_entity();
        // Revealing the dock can touch the pane that holds this board, so it waits until the
        // board's update is over.
        window.defer(cx, move |window, cx| {
            let panel = workspace
                .update(cx, |workspace, cx| {
                    crate::open(workspace, window, cx);
                    workspace.panel::<ClickUpPanel>(cx)
                })
                .log_err()
                .flatten();
            let Some(panel) = panel else {
                return;
            };
            panel.update(cx, |panel, cx| panel.open_task(task, window, cx));
            board
                .update(cx, |board, cx| board.watch_panel(panel, cx))
                .log_err();
        });
    }

    fn watch_panel(&mut self, panel: Entity<ClickUpPanel>, cx: &mut Context<Self>) {
        if self.panel.as_ref() == Some(&panel) {
            return;
        }
        self._subscriptions
            .push(cx.observe(&panel, |_, _, cx| cx.notify()));
        self.panel = Some(panel);
        cx.notify();
    }

    fn render_toolbar(&self, list: &api::List, cx: &Context<Self>) -> impl IntoElement {
        let title: SharedString = match (list.start_date, list.due_date) {
            (Some(start), Some(due)) => match (day_month(start), day_month(due)) {
                (Some(start), Some(end)) => format!(
                    "{} · {}",
                    list.name,
                    t!("clickup.board.date_range", start = start, end = end)
                )
                .into(),
                _ => list.name.clone().into(),
            },
            _ => list.name.clone().into(),
        };
        let url =
            self.store.read(cx).selected_workspace().map(|workspace| {
                format!("https://app.clickup.com/{}/v/li/{}", workspace.id, list.id)
            });
        HeaderBar::new("clickup-board-bar")
            .level(HeaderBarLevel::Content)
            .wrapping(true)
            .start_child(
                Icon::new(IconName::History)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .truncate(),
            )
            .end_child(div().w(px(260.)).child(self.filter_input.clone()))
            .end_children(url.map(|url| {
                secondary_button(
                    "clickup-board-open-web",
                    Some((IconName::ArrowUpRight, Color::Muted)),
                    t!("clickup.board.open_in_clickup"),
                    true,
                    cx,
                )
                .on_click(move |_, _, cx| cx.open_url(&url))
            }))
    }

    fn render_card(
        &self,
        task: &api::Task,
        selected_id: Option<&str>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected = selected_id == Some(task.id.as_str());
        let pending = self.pending.contains(&task.id);
        let timing = timer_started_at(self.store.read(cx), &task.id).is_some();
        let dragged = DraggedTask {
            id: task.id.clone(),
            name: task.name.clone().into(),
        };
        let clicked = task.clone();
        board_card(
            SharedString::from(format!("clickup-board-card-{}", task.id)),
            selected,
            pending,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| this.inspect(clicked.clone(), window, cx)))
        .on_drag(dragged, |dragged, _offset, _window, cx| {
            cx.new(|_| BoardDragPreview::new(dragged.name.clone()))
        })
        .child(Label::new(task.name.clone()).size(LabelSize::Small))
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Label::new(task.display_id().to_string())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .children(priority_pill(task, cx))
                .when(timing, |this| {
                    this.child(
                        Icon::new(IconName::Clock)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                })
                .child(div().flex_1())
                .children(
                    task.assignees
                        .first()
                        .map(|assignee| initial_tile(assignee.display_name(), cx)),
                ),
        )
    }

    fn render_board(
        &self,
        list: &api::List,
        tasks: &[&api::Task],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected_id = self
            .panel
            .as_ref()
            .and_then(|panel| panel.read(cx).open_task_id());
        board_row().children(list.statuses.iter().map(|status| {
            let column_tasks: Vec<&api::Task> = tasks
                .iter()
                .copied()
                .filter(|task| task.status.status.eq_ignore_ascii_case(&status.status))
                .collect();
            let total = column_tasks.len();
            let is_done = status.is_closed();
            let shown = if is_done && !self.show_all_done {
                DONE_TASKS_SHOWN.min(total)
            } else {
                total
            };
            let status_name = status.status.clone();
            BoardColumn::new(
                status.status.clone(),
                status_dot(parse_color(&status.color), cx),
                status.status.clone(),
                total,
            )
            .muted(is_done)
            .on_drop(cx.listener(move |this, dragged: &DraggedTask, window, cx| {
                this.move_task(dragged.id.clone(), status_name.clone(), window, cx)
            }))
            .children(
                column_tasks
                    .iter()
                    .take(shown)
                    .map(|task| self.render_card(task, selected_id, cx)),
            )
            .show_more(
                total - shown,
                cx.listener(|this, _, _, cx| {
                    this.show_all_done = true;
                    cx.notify();
                }),
            )
        }))
    }

    fn render_ready(&self, list: &api::List, cx: &Context<Self>) -> AnyElement {
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let tasks: Vec<&api::Task> = self
            .tasks
            .iter()
            .filter(|task| task_matches(task, &filter))
            .collect();
        let task_count = if self.tasks.len() == 1 {
            t!("clickup.board.tasks.one")
        } else {
            t!("clickup.board.tasks.other", count = self.tasks.len())
        };
        v_flex()
            .size_full()
            .child(self.render_toolbar(list, cx))
            .child(self.render_board(list, &tasks, cx))
            .child(
                HeaderBar::footer("clickup-board-footer")
                    .level(HeaderBarLevel::Content)
                    .child(
                        Label::new(t!(
                            "clickup.board.footer",
                            list = list.name,
                            tasks = task_count
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                    )
                    .end_child(
                        Label::new(sync_status_label(self.store.read(cx).tasks()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }
}

/// The sprint running today in `list`'s folder, or `list` itself when its folder has no
/// dated list covering today.
async fn current_sprint(
    http_client: &Arc<dyn HttpClient>,
    token: &str,
    list: api::List,
) -> Result<api::List> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    if list.is_running_at(now) {
        return Ok(list);
    }
    let Some(folder) = list.folder.as_ref().filter(|folder| !folder.hidden) else {
        return Ok(list);
    };
    let Some(lists) = api::get_folder_lists(http_client, token, &folder.id)
        .await
        .log_err()
    else {
        return Ok(list);
    };
    match lists
        .into_iter()
        .find(|candidate| candidate.is_running_at(now))
    {
        Some(running) => api::get_list(http_client, token, &running.id).await,
        None => Ok(list),
    }
}

fn day_month(millis: i64) -> Option<SharedString> {
    let date = Local.timestamp_millis_opt(millis).single()?;
    Some(day_and_month(date.day(), date.month0()))
}

impl Render for SprintBoard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.store.read(cx).connection() {
            Connection::Loading => {
                EmptyState::new(IconName::LoadCircle, t!("clickup.board.loading"))
                    .spinning(true)
                    .into_any_element()
            }
            Connection::Connected(_) => match (&self.state, &self.list) {
                (BoardState::Ready, Some(list)) => self.render_ready(list, cx),
                (BoardState::NoSprint, _) => {
                    EmptyState::new(IconName::Blocks, t!("clickup.board.no_sprint_title"))
                        .description(t!("clickup.board.no_sprint_detail"))
                        .into_any_element()
                }
                (BoardState::Failed(error), _) => {
                    EmptyState::new(IconName::Warning, t!("clickup.board.failed_title"))
                        .tone(EmptyStateTone::Error)
                        .detail(error.clone())
                        .into_any_element()
                }
                _ => EmptyState::new(IconName::LoadCircle, t!("clickup.board.loading"))
                    .spinning(true)
                    .into_any_element(),
            },
            _ => EmptyState::new(IconName::Disconnected, t!("clickup.errors.not_connected"))
                .into_any_element(),
        };
        v_flex()
            .key_context("ClickUpSprintBoard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

impl Focusable for SprintBoard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for SprintBoard {}

impl WorkspaceItem for SprintBoard {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.list {
            Some(list) => list.name.clone().into(),
            None => t!("clickup.tasks.sprint_board"),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Blocks))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
