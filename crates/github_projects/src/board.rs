//! Board D: the project as a board in a tab, grouped by status or by assignee. Cards are dragged
//! between the Status columns; the selected card is inspected in the Tarefas dock.

use crate::{
    Availability, RepositoryProjects, TasksPanel,
    api::{Item, Project},
    ensure_panel, github_repository, inspect_item,
    panel::{ago, avatar, item_matches, item_number, option_tone, priority_pill, today},
    repository_projects,
};
use chrono::{Datelike as _, NaiveDate};
use gpui::{
    AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, WeakEntity,
    actions, px,
};
use i18n::t;
use repo_hosting::api::RemoteRepository;
use ui::{
    BoardColumn, BoardDragPreview, EmptyState, EmptyStateTone, HeaderBar, HeaderBarLevel,
    KeyBinding, StatusTone, board_card, board_row, error_callout, prelude::*, primary_button,
    segment_option, segmented_control,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item as WorkspaceItem, ItemEvent},
};

actions!(
    github_projects,
    [
        /// Adds a draft item to the project.
        NewItem,
    ]
);

/// Done items are history; the board shows the most recent few.
const DONE_ITEMS_SHOWN: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Grouping {
    Status,
    Assignee,
}

#[derive(Clone)]
struct DraggedItem {
    id: String,
    title: SharedString,
}

struct Column<'a> {
    key: String,
    leading: AnyElement,
    title: String,
    items: Vec<&'a Item>,
    /// The Status option a card dropped here moves to; only status columns take drops.
    drop_option: Option<String>,
    is_done: bool,
}

/// Opens the board of the project's GitHub repository, or brings the open one forward, with
/// `item_id` inspected in the dock.
pub fn open(
    workspace: &mut Workspace,
    item_id: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some((repository, _, _)) = github_repository(&project, cx) else {
        return;
    };
    let panel = ensure_panel(workspace, window, cx);
    let existing = workspace
        .items_of_type::<ProjectBoard>(cx)
        .find(|board| board.read(cx).repository == repository);
    match existing {
        Some(board) => {
            workspace.activate_item(&board, true, true, window, cx);
        }
        None => {
            let weak_workspace = workspace.weak_handle();
            let board =
                cx.new(|cx| ProjectBoard::new(repository, panel, weak_workspace, window, cx));
            workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
        }
    }
    if item_id.is_some() {
        inspect_item(workspace, item_id, window, cx);
    }
}

pub struct ProjectBoard {
    focus_handle: FocusHandle,
    repository: RemoteRepository,
    workspace: WeakEntity<Workspace>,
    state: Entity<RepositoryProjects>,
    panel: Entity<TasksPanel>,
    grouping: Grouping,
    filter_input: Entity<InputField>,
    new_item_input: Entity<InputField>,
    adding_item: bool,
    show_all_done: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectBoard {
    fn new(
        repository: RemoteRepository,
        panel: Entity<TasksPanel>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = repository_projects(&repository, cx);
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "is:open sprint:@current").start_icon(IconName::Filter)
        });
        let new_item_input = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &t!("github_projects.board.new_item_placeholder"),
            )
        });
        let mut subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.observe(&panel, |_, _, cx| cx.notify()),
        ];
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
        Self {
            focus_handle: cx.focus_handle(),
            repository,
            workspace,
            state,
            panel,
            grouping: Grouping::Status,
            filter_input,
            new_item_input,
            adding_item: false,
            show_all_done: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn report(&mut self, task: gpui::Task<anyhow::Result<()>>, cx: &mut Context<Self>) {
        self.error = None;
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn inspect(&mut self, item_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Revealing the dock can touch the pane that holds this board, so it waits until the
        // board's update is over.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    inspect_item(workspace, Some(item_id), window, cx)
                })
                .log_err();
        });
    }

    fn new_item(&mut self, _: &NewItem, window: &mut Window, cx: &mut Context<Self>) {
        self.adding_item = true;
        window.focus(&self.new_item_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn submit_new_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.new_item_input.read(cx).text(cx).trim().to_string();
        if title.is_empty() {
            return;
        }
        self.new_item_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.adding_item = false;
        let task = self
            .state
            .update(cx, |state, cx| state.add_draft(title, cx));
        self.report(task, cx);
    }

    fn move_item(&mut self, item_id: String, option_id: String, cx: &mut Context<Self>) {
        let task = self
            .state
            .update(cx, |state, cx| state.move_item(item_id, option_id, cx));
        self.report(task, cx);
    }

    /// The items the filter keeps: plain words match titles, numbers and labels, and
    /// `sprint:@current`, `assignee:@me`, `label:x` and `is:open` narrow down.
    fn filtered<'a>(&self, project: &'a Project, login: &str, cx: &App) -> Vec<&'a Item> {
        let query = self.filter_input.read(cx).text(cx);
        let current = project
            .current_iteration(today())
            .map(|iteration| iteration.id.clone());
        let mut words = Vec::new();
        let mut only_open = false;
        let mut only_current = false;
        let mut only_mine = false;
        let mut labels = Vec::new();
        for token in query.split_whitespace() {
            match token.to_lowercase().as_str() {
                "is:open" => only_open = true,
                "sprint:@current" | "iteration:@current" => only_current = true,
                "assignee:@me" => only_mine = true,
                token if token.starts_with("label:") => {
                    labels.push(token.trim_start_matches("label:").to_string())
                }
                token => words.push(token.to_string()),
            }
        }
        let text = words.join(" ");
        project
            .items
            .iter()
            .filter(|item| !only_open || !item.closed)
            .filter(|item| {
                !only_current
                    || (current.is_some()
                        && project.iteration_of(item).map(|iteration| &iteration.id)
                            == current.as_ref())
            })
            .filter(|item| !only_mine || item.is_assigned_to(login))
            .filter(|item| {
                labels.iter().all(|wanted| {
                    item.labels
                        .iter()
                        .any(|label| label.name.to_lowercase() == *wanted)
                })
            })
            .filter(|item| text.is_empty() || item_matches(item, &text))
            .collect()
    }

    fn render_toolbar(&self, project: &Project, cx: &Context<Self>) -> impl IntoElement {
        HeaderBar::new("board-bar")
            .level(HeaderBarLevel::Content)
            .wrapping(true)
            .start_child(
                Icon::new(IconName::History)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(iteration_bar(project))
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .truncate(),
            )
            .end_child(div().w(px(260.)).child(self.filter_input.clone()))
            .end_child(
                div().w(px(240.)).child(
                    segmented_control(cx)
                        .child(
                            segment_option(
                                "board-group-status",
                                t!("github_projects.board.group_by_status"),
                                self.grouping == Grouping::Status,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.grouping = Grouping::Status;
                                cx.notify();
                            })),
                        )
                        .child(
                            segment_option(
                                "board-group-assignee",
                                t!("github_projects.board.group_by_assignee"),
                                self.grouping == Grouping::Assignee,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.grouping = Grouping::Assignee;
                                cx.notify();
                            })),
                        ),
                ),
            )
            .end_child(
                primary_button(
                    "board-new-item",
                    Some(IconName::Plus),
                    t!("github_projects.board.new_item"),
                    Some(KeyBinding::for_action_in(&NewItem, &self.focus_handle, cx)),
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.new_item(&NewItem, window, cx))),
            )
    }

    fn render_card(
        &self,
        project: &Project,
        item: &Item,
        selected_id: Option<&str>,
        draggable: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected = selected_id == Some(item.id.as_str());
        let item_id = item.id.clone();
        let dragged = DraggedItem {
            id: item.id.clone(),
            title: item.title.clone().into(),
        };
        let pending = self.state.read(cx).is_pending(&item.id);
        board_card(
            SharedString::from(format!("board-card-{}", item.id)),
            selected,
            pending,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| this.inspect(item_id.clone(), window, cx)))
        .when(draggable, |this| {
            this.on_drag(dragged, |dragged, _offset, _window, cx| {
                cx.new(|_| BoardDragPreview::new(dragged.title.clone()))
            })
        })
        .child(Label::new(item.title.clone()).size(LabelSize::Small))
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Label::new(item_number(item))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .children(priority_pill(project, item, cx))
                .child(div().flex_1())
                .children(item.assignees.first().map(|login| avatar(login, cx))),
        )
    }

    fn render_column(
        &self,
        project: &Project,
        column: Column<'_>,
        selected_id: Option<&str>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let total = column.items.len();
        let shown = if column.is_done && !self.show_all_done {
            DONE_ITEMS_SHOWN.min(total)
        } else {
            total
        };
        let draggable = self.grouping == Grouping::Status;
        BoardColumn::new(column.key, column.leading, column.title, total)
            .muted(column.is_done)
            .when_some(column.drop_option, |this, option_id| {
                this.on_drop(cx.listener(move |this, dragged: &DraggedItem, _, cx| {
                    this.move_item(dragged.id.clone(), option_id.clone(), cx)
                }))
            })
            .children(
                column
                    .items
                    .iter()
                    .take(shown)
                    .map(|item| self.render_card(project, item, selected_id, draggable, cx)),
            )
            .show_more(
                total - shown,
                cx.listener(|this, _, _, cx| {
                    this.show_all_done = true;
                    cx.notify();
                }),
            )
    }

    fn columns<'a>(&self, project: &'a Project, items: &[&'a Item], cx: &App) -> Vec<Column<'a>> {
        let mut columns = Vec::new();
        match self.grouping {
            Grouping::Status => {
                let without_status: Vec<&Item> = items
                    .iter()
                    .copied()
                    .filter(|item| project.status_of(item).is_none())
                    .collect();
                if !without_status.is_empty() {
                    columns.push(Column {
                        key: "no-status".to_string(),
                        leading: StatusTone::Neutral.dot(cx).into_any_element(),
                        title: t!("github_projects.board.no_status").to_string(),
                        items: without_status,
                        drop_option: None,
                        is_done: false,
                    });
                }
                let done_id = project.done_option().map(|option| option.id.as_str());
                for option in project.columns() {
                    columns.push(Column {
                        key: option.id.clone(),
                        leading: option_tone(option).dot(cx).into_any_element(),
                        title: option.name.clone(),
                        items: items
                            .iter()
                            .copied()
                            .filter(|item| {
                                project.status_of(item).map(|status| &status.id) == Some(&option.id)
                            })
                            .collect(),
                        drop_option: Some(option.id.clone()),
                        is_done: done_id == Some(option.id.as_str()),
                    });
                }
            }
            Grouping::Assignee => {
                let mut logins: Vec<&str> = Vec::new();
                for login in items.iter().flat_map(|item| item.assignees.iter()) {
                    if !logins.contains(&login.as_str()) {
                        logins.push(login);
                    }
                }
                for login in logins {
                    columns.push(Column {
                        key: format!("assignee-{login}"),
                        leading: avatar(login, cx).into_any_element(),
                        title: login.to_string(),
                        items: items
                            .iter()
                            .copied()
                            .filter(|item| item.is_assigned_to(login))
                            .collect(),
                        drop_option: None,
                        is_done: false,
                    });
                }
                let unassigned: Vec<&Item> = items
                    .iter()
                    .copied()
                    .filter(|item| item.assignees.is_empty())
                    .collect();
                if !unassigned.is_empty() {
                    columns.push(Column {
                        key: "no-assignee".to_string(),
                        leading: Icon::new(IconName::Person)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element(),
                        title: t!("github_projects.board.no_assignee").to_string(),
                        items: unassigned,
                        drop_option: None,
                        is_done: false,
                    });
                }
            }
        }
        columns
    }

    fn render_board(
        &self,
        project: &Project,
        items: &[&Item],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected_id = self.panel.read(cx).inspected_item();
        board_row().children(
            self.columns(project, items, cx)
                .into_iter()
                .map(|column| self.render_column(project, column, selected_id, cx)),
        )
    }

    fn render_ready(&self, project: &Project, cx: &Context<Self>) -> AnyElement {
        let state = self.state.read(cx);
        let login = state.viewer_login().unwrap_or_default().to_string();
        let items = self.filtered(project, &login, cx);
        let item_count = items_text(project.items.len());
        let footer = match state.synced_at().map(ago) {
            Some(ago) => t!(
                "github_projects.board.footer_synced",
                project = project.summary.title,
                items = item_count,
                ago = ago
            ),
            None => t!(
                "github_projects.board.footer",
                project = project.summary.title,
                items = item_count
            ),
        };
        let rate = t!(
            "github_projects.board.rate_limit",
            remaining = project.rate_limit.remaining,
            limit = project.rate_limit.limit
        );
        let border = cx.theme().colors().border_variant;
        v_flex()
            .size_full()
            .child(self.render_toolbar(project, cx))
            .when(self.adding_item, |this| {
                this.child(
                    h_flex()
                        .px_4()
                        .py_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(border)
                        .key_context("BoardNewItem")
                        .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                            this.submit_new_item(window, cx)
                        }))
                        .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                            this.adding_item = false;
                            cx.notify();
                        }))
                        .child(div().w(px(420.)).child(self.new_item_input.clone()))
                        .child(
                            Label::new(t!("github_projects.board.new_item_hint"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .children(self.error.clone().map(|error| {
                div().px_4().pt_2().child(error_callout(
                    t!("github_projects.panel.action_failed"),
                    error,
                    cx,
                ))
            }))
            .child(self.render_board(project, &items, cx))
            .child(
                HeaderBar::footer("board-footer")
                    .level(HeaderBarLevel::Content)
                    .child(
                        Label::new(footer)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .end_child(
                        Label::new(rate)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }
}

fn items_text(count: usize) -> SharedString {
    if count == 1 {
        t!("github_projects.board.footer_items.one")
    } else {
        t!("github_projects.board.footer_items.other", count = count)
    }
}

fn month_abbreviation(month0: u32) -> SharedString {
    match month0 {
        0 => t!("github_projects.date.month_jan"),
        1 => t!("github_projects.date.month_feb"),
        2 => t!("github_projects.date.month_mar"),
        3 => t!("github_projects.date.month_apr"),
        4 => t!("github_projects.date.month_may"),
        5 => t!("github_projects.date.month_jun"),
        6 => t!("github_projects.date.month_jul"),
        7 => t!("github_projects.date.month_aug"),
        8 => t!("github_projects.date.month_sep"),
        9 => t!("github_projects.date.month_oct"),
        10 => t!("github_projects.date.month_nov"),
        _ => t!("github_projects.date.month_dec"),
    }
}

fn day_month(date: NaiveDate) -> SharedString {
    t!(
        "github_projects.board.day_month",
        day = date.day(),
        month = month_abbreviation(date.month0())
    )
}

/// `Iteração 12 · 3 nov – 14 nov · 14 itens`, or the project and its size without an
/// iteration running.
fn iteration_bar(project: &Project) -> SharedString {
    let Some(iteration) = project.current_iteration(today()) else {
        return t!(
            "github_projects.board.footer",
            project = project.summary.title,
            items = items_text(project.items.len())
        );
    };
    let count = project
        .items
        .iter()
        .filter(|item| project.iteration_of(item).map(|it| &it.id) == Some(&iteration.id))
        .count();
    match (iteration.start_date, iteration.end_date()) {
        (Some(start), Some(end)) => t!(
            "github_projects.board.iteration_bar",
            iteration = iteration.title,
            dates = t!(
                "github_projects.board.date_range",
                start = day_month(start),
                end = day_month(end)
            ),
            items = items_text(count)
        ),
        _ => t!(
            "github_projects.board.footer",
            project = iteration.title,
            items = items_text(count)
        ),
    }
}

impl Render for ProjectBoard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let availability = self.state.read(cx).availability(cx);
        let content: AnyElement = match availability {
            Availability::Ready => match self.state.read(cx).project().cloned() {
                Some(project) => self.render_ready(&project, cx),
                None => div().into_any_element(),
            },
            Availability::NotConnected => EmptyState::new(
                IconName::Disconnected,
                t!("github_projects.empty.connect_title"),
            )
            .description(t!("github_projects.empty.connect_detail"))
            .into_any_element(),
            Availability::NoProjects => EmptyState::new(
                IconName::Blocks,
                t!("github_projects.empty.no_projects_title"),
            )
            .into_any_element(),
            Availability::MissingScope => EmptyState::new(
                IconName::Lock,
                t!("github_projects.empty.missing_scope_title"),
            )
            .description(t!("github_projects.empty.missing_scope_detail"))
            .into_any_element(),
            Availability::Failed(error) => {
                EmptyState::new(IconName::Warning, t!("github_projects.empty.failed_title"))
                    .tone(EmptyStateTone::Error)
                    .detail(error)
                    .into_any_element()
            }
            Availability::Loading => {
                EmptyState::new(IconName::LoadCircle, t!("github_projects.empty.loading"))
                    .spinning(true)
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("ProjectBoard")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_item))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

impl Focusable for ProjectBoard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ProjectBoard {}

impl WorkspaceItem for ProjectBoard {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.state
            .read(cx)
            .project()
            .map(|project| SharedString::from(project.summary.title.clone()))
            .unwrap_or_else(|| "GitHub Projects".into())
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
