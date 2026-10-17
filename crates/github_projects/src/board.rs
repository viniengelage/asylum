//! Board D: the project as a board in a tab. Cards are dragged between the Status columns, and
//! the inspector edits the selected item's fields and starts work on it.

use crate::{
    Availability, RepositoryProjects,
    api::{self, FieldOption, Item, ItemKind, NewValue, Project},
    github_repository,
    panel::{ago, avatar, chip, format_number, item_matches, option_color, today},
    repository_projects, start_item,
};
use gpui::{
    Action as _, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Subscription, WeakEntity, actions, px,
};
use project::Project as WorkspaceProject;
use repo_hosting::api::RemoteRepository;
use ui::{
    Checkbox, ContextMenu, KeyBinding, PopoverMenu, PopoverTriggerArea, Tooltip, prelude::*,
    primary_button, secondary_button, segment_option, segmented_control,
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
        /// Starts the selected item: creates its branch, checks it out and moves it.
        StartItem,
    ]
);

/// Done items are history; the board shows the most recent few.
const DONE_ITEMS_SHOWN: usize = 4;
const ESTIMATES: [f64; 6] = [1.0, 2.0, 3.0, 5.0, 8.0, 13.0];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Board,
    Table,
}

#[derive(Clone)]
struct DraggedItem {
    id: String,
    title: SharedString,
}

struct DragPreview {
    title: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_focused)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

/// Opens the board of the project's GitHub repository, or brings the open one forward, with
/// `item_id` selected.
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
    let existing = workspace
        .items_of_type::<ProjectBoard>(cx)
        .find(|board| board.read(cx).repository == repository);
    let board = match existing {
        Some(board) => {
            workspace.activate_item(&board, true, true, window, cx);
            board
        }
        None => {
            let weak_workspace = workspace.weak_handle();
            let board =
                cx.new(|cx| ProjectBoard::new(repository, project, weak_workspace, window, cx));
            workspace.add_item_to_active_pane(Box::new(board.clone()), None, true, window, cx);
            board
        }
    };
    if item_id.is_some() {
        board.update(cx, |board, cx| {
            board.selected = item_id;
            cx.notify();
        });
    }
}

pub struct ProjectBoard {
    focus_handle: FocusHandle,
    repository: RemoteRepository,
    project: Entity<WorkspaceProject>,
    workspace: WeakEntity<Workspace>,
    state: Entity<RepositoryProjects>,
    view: View,
    selected: Option<String>,
    filter_input: Entity<InputField>,
    branch_input: Entity<InputField>,
    comment_input: Entity<InputField>,
    new_item_input: Entity<InputField>,
    adding_item: bool,
    commenting: bool,
    show_all_done: bool,
    /// The item the branch field was filled for, so typing in it isn't overwritten.
    branch_filled_for: Option<String>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectBoard {
    fn new(
        repository: RemoteRepository,
        project: Entity<WorkspaceProject>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = repository_projects(&repository, cx);
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "is:open sprint:@current").start_icon(IconName::Filter)
        });
        let branch_input = cx.new(|cx| InputField::new(window, cx, "feat/…"));
        let comment_input = cx.new(|cx| InputField::new(window, cx, "Comentar no item…"));
        let new_item_input = cx.new(|cx| InputField::new(window, cx, "Título do novo item"));
        let mut subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
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
            project,
            workspace,
            state,
            view: View::Board,
            selected: None,
            filter_input,
            branch_input,
            comment_input,
            new_item_input,
            adding_item: false,
            commenting: false,
            show_all_done: false,
            branch_filled_for: None,
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

    fn select(&mut self, item_id: Option<String>, cx: &mut Context<Self>) {
        self.selected = item_id;
        self.commenting = false;
        cx.notify();
    }

    fn start_selected(&mut self, _: &StartItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item_id) = self.selected.clone() else {
            return;
        };
        let branch = self.branch_input.read(cx).text(cx).trim().to_string();
        if branch.is_empty() {
            return;
        }
        let task = start_item(
            self.workspace.clone(),
            self.project.clone(),
            self.state.clone(),
            item_id,
            branch,
            window,
            cx,
        );
        self.report(task, cx);
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

    fn post_comment(&mut self, item_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.comment_input.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        self.comment_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.commenting = false;
        let task = self
            .state
            .update(cx, |state, cx| state.comment(&item_id, body, cx));
        self.report(task, cx);
    }

    fn set_field(
        &mut self,
        item_id: String,
        field_id: String,
        value: NewValue,
        cx: &mut Context<Self>,
    ) {
        let task = self.state.update(cx, |state, cx| {
            state.set_field(item_id, field_id, value, cx)
        });
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
        let colors = cx.theme().colors();
        let state = self.state.read(cx);
        let projects = state.projects().to_vec();
        let weak_state = self.state.downgrade();
        let url = project.summary.url.clone();
        let roadmap_url = format!("{url}/views/1?layout=roadmap");
        let display_url = url.trim_start_matches("https://").to_string();
        h_flex()
            .h(px(56.))
            .px_4()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .size(px(28.))
                    .justify_center()
                    .rounded(px(7.))
                    .bg(colors.border_focused.opacity(0.2))
                    .child(
                        Icon::new(IconName::Blocks)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    ),
            )
            .child(
                v_flex()
                    .child(
                        PopoverMenu::new("board-project-picker")
                            .trigger(PopoverTriggerArea::new(
                                h_flex()
                                    .id("board-project-trigger")
                                    .gap_1p5()
                                    .child(
                                        Label::new(project.summary.title.clone())
                                            .size(LabelSize::Large)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ))
                            .menu(move |window, cx| {
                                let projects = projects.clone();
                                let weak_state = weak_state.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for summary in projects {
                                        let weak_state = weak_state.clone();
                                        let id = summary.id.clone();
                                        menu = menu.entry(
                                            summary.title.clone(),
                                            None,
                                            move |_, cx| {
                                                weak_state
                                                    .update(cx, |state, cx| {
                                                        state.select_project(id.clone(), cx)
                                                    })
                                                    .log_err();
                                            },
                                        );
                                    }
                                    menu
                                }))
                            }),
                    )
                    .child(
                        Label::new(display_url)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div().w(px(270.)).child(
                    segmented_control(cx)
                        .child(
                            segment_option(
                                "board-view-board",
                                "Quadro",
                                self.view == View::Board,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Board;
                                cx.notify();
                            })),
                        )
                        .child(
                            segment_option(
                                "board-view-table",
                                "Tabela",
                                self.view == View::Table,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Table;
                                cx.notify();
                            })),
                        )
                        .child(
                            segment_option("board-view-roadmap", "Roadmap", false, true, cx)
                                .tooltip(Tooltip::text("O roadmap abre no GitHub"))
                                .on_click(move |_, _, cx| cx.open_url(&roadmap_url)),
                        ),
                ),
            )
            .child(div().w(px(380.)).child(self.filter_input.clone()))
            .child(
                h_flex()
                    .gap_1p5()
                    .px_2()
                    .child(
                        Icon::new(IconName::ListTree)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "Colunas: {}",
                            project
                                .status
                                .as_ref()
                                .map_or("Status", |field| field.name.as_str())
                        ))
                        .size(LabelSize::Small),
                    ),
            )
            .child(div().flex_1())
            .child(
                secondary_button(
                    "board-open-github",
                    Some((IconName::ArrowUpRight, Color::Muted)),
                    "Abrir no GitHub",
                    true,
                    cx,
                )
                .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .child(
                primary_button(
                    "board-new-item",
                    Some(IconName::Plus),
                    "Novo item",
                    Some(KeyBinding::for_action_in(&NewItem, &self.focus_handle, cx)),
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.new_item(&NewItem, window, cx))),
            )
    }

    fn render_card(&self, project: &Project, item: &Item, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let selected = self.selected.as_deref() == Some(item.id.as_str());
        let accent = colors.border_focused;
        let item_id = item.id.clone();
        let dragged = DraggedItem {
            id: item.id.clone(),
            title: item.title.clone().into(),
        };
        let pending = self.state.read(cx).is_pending(&item.id);
        v_flex()
            .id(SharedString::from(format!("board-card-{}", item.id)))
            .w_full()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .cursor_pointer()
            .when(selected, |this| {
                this.border_color(accent).bg(accent.opacity(0.1))
            })
            .when(!selected, |this| {
                this.border_color(colors.border)
                    .bg(colors.element_background)
                    .hover(|this| this.border_color(colors.border_focused.opacity(0.5)))
            })
            .when(pending, |this| this.opacity(0.6))
            .on_click(cx.listener(move |this, _, _, cx| this.select(Some(item_id.clone()), cx)))
            .on_drag(dragged, |dragged, _offset, _window, cx| {
                cx.new(|_| DragPreview {
                    title: dragged.title.clone(),
                })
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(match item.kind {
                            ItemKind::PullRequest => IconName::PullRequest,
                            ItemKind::Draft => IconName::FileTextOutlined,
                            ItemKind::Issue if item.closed => IconName::Check,
                            ItemKind::Issue => IconName::Circle,
                        })
                        .size(IconSize::XSmall)
                        .color(if item.closed {
                            Color::Accent
                        } else {
                            Color::Success
                        }),
                    )
                    .child(
                        Label::new(item.reference())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(item.assignees.first().map(|login| avatar(login, cx))),
            )
            .child(Label::new(item.title.clone()).size(LabelSize::Small))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .children(item.labels.iter().take(2).map(|label| {
                        let color = if label.name.eq_ignore_ascii_case("bug") {
                            Color::Error.color(cx)
                        } else {
                            Color::Info.color(cx)
                        };
                        chip(label.name.clone(), color, None, false, cx)
                    }))
                    .children(project.priority_of(item).map(|priority| {
                        chip(
                            priority.name.clone(),
                            option_color(priority, cx),
                            None,
                            false,
                            cx,
                        )
                    }))
                    .children(project.estimate_of(item).map(|estimate| {
                        chip(
                            format!("{} pts", format_number(estimate)),
                            colors.text,
                            None,
                            false,
                            cx,
                        )
                    }))
                    .children(item.pull_requests.first().map(|pull_request| {
                        let (text, color) = match pull_request.state.as_str() {
                            "MERGED" => (
                                format!("PR #{} · merge", pull_request.number),
                                Color::Accent.color(cx),
                            ),
                            "CLOSED" => (
                                format!("PR #{} · fechado", pull_request.number),
                                colors.text_muted,
                            ),
                            _ => (
                                format!("PR #{}", pull_request.number),
                                Color::Accent.color(cx),
                            ),
                        };
                        chip(text, color, Some(IconName::PullRequest), false, cx)
                    }))
                    .children(
                        item.linked_branches
                            .first()
                            .filter(|_| item.pull_requests.is_empty())
                            .map(|branch| {
                                chip(
                                    branch.clone(),
                                    colors.text_muted,
                                    Some(IconName::GitBranch),
                                    true,
                                    cx,
                                )
                            }),
                    ),
            )
    }

    fn render_column(
        &self,
        project: &Project,
        option: Option<&FieldOption>,
        items: Vec<&Item>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let option_id = option.map(|option| option.id.clone());
        let title = option.map_or_else(|| "Sem status".to_string(), |option| option.name.clone());
        let color = option.map_or(colors.text_muted, |option| option_color(option, cx));
        let is_done = option.is_some()
            && option.map(|option| &option.id) == project.done_option().map(|option| &option.id);
        let iteration = is_done
            .then(|| {
                project
                    .current_iteration(today())
                    .map(|iteration| iteration.title.clone())
            })
            .flatten();
        let total = items.len();
        let shown = if is_done && !self.show_all_done {
            DONE_ITEMS_SHOWN.min(total)
        } else {
            total
        };
        let dragging = cx.has_active_drag();
        let drop_option = option_id.clone();
        let zone_option = option_id.clone();
        let column_name = title.clone();
        v_flex()
            .id(SharedString::from(format!(
                "board-column-{}",
                option_id.as_deref().unwrap_or("none")
            )))
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_2()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .overflow_y_scroll()
            .drag_over::<DraggedItem>(move |style, _, _, _| style.border_color(accent))
            .when_some(drop_option, |this, option_id| {
                this.on_drop(cx.listener(move |this, dragged: &DraggedItem, _, cx| {
                    this.move_item(dragged.id.clone(), option_id.clone(), cx)
                }))
            })
            .child(
                h_flex()
                    .h(px(30.))
                    .px_1()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(color))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(total.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(iteration.map(|title| {
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    })),
            )
            .children(
                items
                    .iter()
                    .take(shown)
                    .map(|item| self.render_card(project, item, cx)),
            )
            .when(dragging, |this| {
                this.when_some(zone_option, |this, _| {
                    this.child(
                        h_flex()
                            .id(SharedString::from(format!("board-drop-{column_name}")))
                            .h(px(64.))
                            .flex_none()
                            .justify_center()
                            .gap_1p5()
                            .rounded_lg()
                            .border_1()
                            .border_dashed()
                            .border_color(colors.border)
                            .drag_over::<DraggedItem>(move |style, _, _, _| {
                                style.border_color(accent).bg(accent.opacity(0.1))
                            })
                            .child(
                                Icon::new(IconName::ArrowDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(format!("Solte aqui · vira {column_name}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            ),
                    )
                })
            })
            .when(shown < total, |this| {
                this.child(
                    h_flex()
                        .id("board-show-done")
                        .h(px(28.))
                        .flex_none()
                        .justify_center()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_all_done = true;
                            cx.notify();
                        }))
                        .child(
                            Label::new(format!("+ {} itens", total - shown))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
    }

    fn render_board(
        &self,
        project: &Project,
        items: &[&Item],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let without_status: Vec<&Item> = items
            .iter()
            .copied()
            .filter(|item| project.status_of(item).is_none())
            .collect();
        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_start()
            .gap_3()
            .p_4()
            .when(!without_status.is_empty(), |this| {
                this.child(self.render_column(project, None, without_status, cx))
            })
            .children(project.columns().iter().map(|option| {
                let column: Vec<&Item> = items
                    .iter()
                    .copied()
                    .filter(|item| {
                        project.status_of(item).map(|status| &status.id) == Some(&option.id)
                    })
                    .collect();
                self.render_column(project, Some(option), column, cx)
            }))
    }

    fn render_table(
        &self,
        project: &Project,
        items: &[&Item],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let header = |label: &'static str, width: Option<f32>| {
            let cell = div().px_2().child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            );
            match width {
                Some(width) => cell.w(px(width)).flex_none(),
                None => cell.flex_1().min_w_0(),
            }
        };
        v_flex()
            .id("board-table")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .child(
                h_flex()
                    .h(px(32.))
                    .border_b_1()
                    .border_color(colors.border)
                    .child(header("ITEM", None))
                    .child(header("STATUS", Some(150.)))
                    .child(header("PRIORIDADE", Some(110.)))
                    .child(header("SPRINT", Some(140.)))
                    .child(header("ESTIMATIVA", Some(100.)))
                    .child(header("RESPONSÁVEIS", Some(160.))),
            )
            .children(items.iter().map(|item| {
                let item_id = item.id.clone();
                let selected = self.selected.as_deref() == Some(item.id.as_str());
                let cell = |content: AnyElement, width: f32| {
                    div().w(px(width)).flex_none().px_2().child(content)
                };
                let muted = |text: String| {
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element()
                };
                h_flex()
                    .id(SharedString::from(format!("board-row-{}", item.id)))
                    .h(px(36.))
                    .border_b_1()
                    .border_color(colors.border)
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.border_focused.opacity(0.1)))
                    .hover(|this| this.bg(colors.element_hover))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select(Some(item_id.clone()), cx)),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .gap_2()
                            .child(
                                Label::new(item.reference())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(item.title.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            ),
                    )
                    .child(cell(
                        project.status_of(item).map_or_else(
                            || muted("—".to_string()),
                            |status| {
                                chip(
                                    status.name.clone(),
                                    option_color(status, cx),
                                    None,
                                    false,
                                    cx,
                                )
                                .into_any_element()
                            },
                        ),
                        150.,
                    ))
                    .child(cell(
                        project.priority_of(item).map_or_else(
                            || muted("—".to_string()),
                            |priority| {
                                chip(
                                    priority.name.clone(),
                                    option_color(priority, cx),
                                    None,
                                    false,
                                    cx,
                                )
                                .into_any_element()
                            },
                        ),
                        110.,
                    ))
                    .child(cell(
                        muted(
                            project
                                .iteration_of(item)
                                .map_or("—".to_string(), |iteration| iteration.title.clone()),
                        ),
                        140.,
                    ))
                    .child(cell(
                        muted(
                            project
                                .estimate_of(item)
                                .map_or("—".to_string(), format_number),
                        ),
                        100.,
                    ))
                    .child(cell(
                        muted(if item.assignees.is_empty() {
                            "—".to_string()
                        } else {
                            item.assignees.join(", ")
                        }),
                        160.,
                    ))
            }))
    }

    fn field_row(
        &self,
        label: &'static str,
        value: AnyElement,
        menu: Option<Entity<ContextMenu>>,
        id: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let box_ = h_flex()
            .id(id)
            .w_full()
            .h(px(28.))
            .px_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .hover(|this| this.bg(colors.element_hover))
            .child(div().flex_1().min_w_0().child(value))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            );
        h_flex()
            .h(px(34.))
            .px_4()
            .gap_2p5()
            .child(
                div()
                    .w(px(96.))
                    .flex_none()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div().flex_1().min_w_0().child(match menu {
                    Some(menu) => PopoverMenu::new(SharedString::from(format!("{id}-menu")))
                        .trigger(PopoverTriggerArea::new(box_))
                        .menu(move |_, _| Some(menu.clone()))
                        .into_any_element(),
                    None => box_.into_any_element(),
                }),
            )
    }

    fn section_label(title: &'static str, trailing: Option<String>) -> impl IntoElement {
        h_flex()
            .px_4()
            .pt_3p5()
            .pb_1p5()
            .child(
                Label::new(title)
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .children(
                trailing.map(|text| Label::new(text).size(LabelSize::XSmall).color(Color::Muted)),
            )
    }

    fn option_menu(
        &self,
        item: &Item,
        field: &api::SingleSelectField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let board = cx.weak_entity();
        let item_id = item.id.clone();
        let field_id = field.id.clone();
        let current = item.option_id(&field.id).map(str::to_string);
        let options = field.options.clone();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            for option in options {
                let board = board.clone();
                let item_id = item_id.clone();
                let field_id = field_id.clone();
                let option_id = option.id.clone();
                menu = menu.toggleable_entry(
                    option.name.clone(),
                    current.as_deref() == Some(option.id.as_str()),
                    ui::IconPosition::Start,
                    None,
                    move |_, cx| {
                        board
                            .update(cx, |board, cx| {
                                board.set_field(
                                    item_id.clone(),
                                    field_id.clone(),
                                    NewValue::Option(option_id.clone()),
                                    cx,
                                )
                            })
                            .log_err();
                    },
                );
            }
            let board = board.clone();
            menu.separator().entry("Limpar", None, move |_, cx| {
                board
                    .update(cx, |board, cx| {
                        board.set_field(item_id.clone(), field_id.clone(), NewValue::Clear, cx)
                    })
                    .log_err();
            })
        })
    }

    fn render_inspector(
        &mut self,
        project: &Project,
        item: &Item,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.branch_filled_for.as_deref() != Some(item.id.as_str()) {
            let name = item
                .linked_branches
                .first()
                .cloned()
                .unwrap_or_else(|| api::branch_name(item));
            self.branch_input
                .update(cx, |input, cx| input.set_text(&name, window, cx));
            self.branch_filled_for = Some(item.id.clone());
        }
        let colors = cx.theme().colors().clone();
        let state = self.state.read(cx);
        let login = state.viewer_login().unwrap_or_default().to_string();
        let default_branch = state
            .info()
            .and_then(|info| info.default_branch.clone())
            .unwrap_or_else(|| "main".to_string());
        let current_branch = github_repository(&self.project, cx).and_then(|(_, _, branch)| branch);
        let started = !item.linked_branches.is_empty();
        let on_branch = current_branch
            .as_ref()
            .is_some_and(|branch| item.linked_branches.contains(branch));

        let status_value = match project.status_of(item) {
            Some(status) => h_flex()
                .gap_1p5()
                .child(
                    div()
                        .size(px(7.))
                        .rounded_full()
                        .bg(option_color(status, cx)),
                )
                .child(Label::new(status.name.clone()).size(LabelSize::Small))
                .into_any_element(),
            None => Label::new("—")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let status_menu = project
            .status
            .clone()
            .map(|field| self.option_menu(item, &field, window, cx));
        let priority = project.priority.clone();
        let priority_value = match project.priority_of(item) {
            Some(priority) => h_flex()
                .gap_1p5()
                .child(
                    div()
                        .size(px(7.))
                        .rounded_full()
                        .bg(option_color(priority, cx)),
                )
                .child(Label::new(priority.name.clone()).size(LabelSize::Small))
                .into_any_element(),
            None => Label::new("—")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let priority_menu = priority
            .as_ref()
            .map(|field| self.option_menu(item, field, window, cx));

        let iteration_value = match project.iteration_of(item) {
            Some(iteration) => {
                let dates = match (iteration.start_date, iteration.end_date()) {
                    (Some(start), Some(end)) => {
                        format!(" · {}–{}", start.format("%d"), end.format("%d %b"))
                    }
                    _ => String::new(),
                };
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::History)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{}{dates}", iteration.title))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .into_any_element()
            }
            None => Label::new("—")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        let iteration_menu = project.iteration.clone().map(|field| {
            let board = cx.weak_entity();
            let item_id = item.id.clone();
            let current = item.iteration_id(&field.id).map(str::to_string);
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for iteration in &field.iterations {
                    let board = board.clone();
                    let item_id = item_id.clone();
                    let field_id = field.id.clone();
                    let iteration_id = iteration.id.clone();
                    menu = menu.toggleable_entry(
                        iteration.title.clone(),
                        current.as_deref() == Some(iteration.id.as_str()),
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| {
                            board
                                .update(cx, |board, cx| {
                                    board.set_field(
                                        item_id.clone(),
                                        field_id.clone(),
                                        NewValue::Iteration(iteration_id.clone()),
                                        cx,
                                    )
                                })
                                .log_err();
                        },
                    );
                }
                menu
            })
        });
        let estimate_value = Label::new(
            project
                .estimate_of(item)
                .map_or("—".to_string(), format_number),
        )
        .size(LabelSize::Small)
        .buffer_font(cx)
        .into_any_element();
        let estimate_menu = project.estimate.clone().map(|field| {
            let board = cx.weak_entity();
            let item_id = item.id.clone();
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for estimate in ESTIMATES {
                    let board = board.clone();
                    let item_id = item_id.clone();
                    let field_id = field.id.clone();
                    menu = menu.entry(format_number(estimate), None, move |_, cx| {
                        board
                            .update(cx, |board, cx| {
                                board.set_field(
                                    item_id.clone(),
                                    field_id.clone(),
                                    NewValue::Number(estimate),
                                    cx,
                                )
                            })
                            .log_err();
                    });
                }
                menu
            })
        });
        let assignees_value = if item.assignees.is_empty() {
            h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Person)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("ninguém")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            Label::new(item.assignees.join(", "))
                .size(LabelSize::Small)
                .truncate()
                .into_any_element()
        };
        let assignees_menu =
            (item.kind != ItemKind::Draft && !item.is_assigned_to(&login)).then(|| {
                let state = self.state.downgrade();
                let item_id = item.id.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Atribuir a mim", None, move |_, cx| {
                        state
                            .update(cx, |state, cx| {
                                state.assign_me(&item_id, cx).detach_and_log_err(cx)
                            })
                            .log_err();
                    })
                })
            });

        let subtasks = item.subtasks();
        let done = subtasks.iter().filter(|subtask| subtask.done).count();
        let description: String = item
            .body
            .lines()
            .filter(|line| !api::subtasks(line).iter().any(|_| true))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .chars()
            .take(600)
            .collect();
        let item_url = item.url.clone();
        let kind = match item.kind {
            ItemKind::Issue => "Issue",
            ItemKind::PullRequest => "Pull request",
            ItemKind::Draft => "Rascunho",
        };
        let reference = match (&item.repository, item.number) {
            (Some(repository), Some(number)) => format!("{kind} · {repository}#{number}"),
            _ => kind.to_string(),
        };
        let item_id = item.id.clone();
        let comment_item = item.id.clone();
        let mention = zed_actions::agent::MentionProjectItem {
            project_id: project.summary.id.clone(),
            item_id: item.id.clone(),
            title: item.title.clone(),
            reference: item.reference(),
            prompt: None,
            submit: false,
        };

        v_flex()
            .w(px(400.))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                v_flex()
                    .gap_2()
                    .px_4()
                    .py_3p5()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(if item.closed { IconName::Check } else { IconName::Circle })
                                    .size(IconSize::XSmall)
                                    .color(if item.closed { Color::Accent } else { Color::Success }),
                            )
                            .child(Label::new(reference).size(LabelSize::XSmall).buffer_font(cx).color(Color::Muted))
                            .child(div().flex_1())
                            .when_some(item_url, |this, url| {
                                this.child(
                                    IconButton::new("board-item-open", IconName::ArrowUpRight)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text("Abrir no GitHub"))
                                        .on_click(move |_, _, cx| cx.open_url(&url)),
                                )
                            })
                            .child(
                                IconButton::new("board-item-close", IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .on_click(cx.listener(|this, _, _, cx| this.select(None, cx))),
                            ),
                    )
                    .child(Label::new(item.title.clone()).size(LabelSize::Large).weight(FontWeight::SEMIBOLD))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(if item.closed {
                                chip("Fechada", Color::Accent.color(cx), None, false, cx).into_any_element()
                            } else {
                                chip("Aberta", Color::Success.color(cx), None, false, cx).into_any_element()
                            })
                            .children(item.labels.first().map(|label| chip(label.name.clone(), Color::Info.color(cx), None, false, cx)))
                            .children(item.author.as_ref().map(|author| {
                                Label::new(format!("aberta por {author}")).size(LabelSize::XSmall).color(Color::Muted)
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("board-inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(Self::section_label("CAMPOS DO PROJETO", Some("sincronizam com o GitHub".to_string())))
                    .child(self.field_row("Status", status_value, status_menu, "board-field-status", cx))
                    .when(priority.is_some(), |this| {
                        this.child(self.field_row("Prioridade", priority_value, priority_menu, "board-field-priority", cx))
                    })
                    .when(project.iteration.is_some(), |this| {
                        this.child(self.field_row(
                            project.iteration.as_ref().map_or("Sprint", |_| "Sprint"),
                            iteration_value,
                            iteration_menu,
                            "board-field-iteration",
                            cx,
                        ))
                    })
                    .when(project.estimate.is_some(), |this| {
                        this.child(self.field_row("Estimativa", estimate_value, estimate_menu, "board-field-estimate", cx))
                    })
                    .child(self.field_row("Responsáveis", assignees_value, assignees_menu, "board-field-assignees", cx))
                    .child(Self::section_label("DESENVOLVIMENTO", None))
                    .child(
                        div().px_4().child(
                            v_flex()
                                .gap_1p5()
                                .p_3()
                                .rounded_lg()
                                .border_1()
                                .border_color(colors.border)
                                .bg(colors.editor_background)
                                .when(!started && item.pull_requests.is_empty(), |this| {
                                    this.child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(Icon::new(IconName::GitBranchPlus).size(IconSize::XSmall).color(Color::Muted))
                                            .child(Label::new("Nenhum branch ou PR ainda").size(LabelSize::Small)),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "Começar cria o branch abaixo a partir de {default_branch} e vincula ao item."
                                        ))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                    .child(self.branch_input.clone())
                                })
                                .children(item.linked_branches.iter().map(|branch| {
                                    h_flex()
                                        .gap_1p5()
                                        .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                                        .child(Label::new(branch.clone()).size(LabelSize::Small).buffer_font(cx))
                                        .when(current_branch.as_deref() == Some(branch.as_str()), |this| {
                                            this.child(chip("branch atual", Color::Accent.color(cx), None, false, cx))
                                        })
                                }))
                                .children(item.pull_requests.iter().map(|pull_request| {
                                    let url = pull_request.url.clone();
                                    h_flex()
                                        .id(SharedString::from(format!("board-pr-{}", pull_request.number)))
                                        .gap_1p5()
                                        .cursor_pointer()
                                        .on_click(move |_, _, cx| cx.open_url(&url))
                                        .child(Icon::new(IconName::PullRequest).size(IconSize::XSmall).color(Color::Accent))
                                        .child(Label::new(format!("PR #{}", pull_request.number)).size(LabelSize::Small))
                                        .child(Label::new(pull_request.state.to_lowercase()).size(LabelSize::XSmall).color(Color::Muted))
                                })),
                        ),
                    )
                    .when(!subtasks.is_empty(), |this| {
                        this.child(Self::section_label("SUBTAREFAS", Some(format!("{done}/{}", subtasks.len()))))
                            .children(subtasks.into_iter().map(|subtask| {
                                let state = self.state.downgrade();
                                let item_id = item.id.clone();
                                let line = subtask.line;
                                h_flex().h(px(28.)).px_4().child(
                                    Checkbox::new(SharedString::from(format!("board-subtask-{line}")), subtask.done.into())
                                        .label(subtask.text)
                                        .on_click(move |toggle, _, cx| {
                                            let done = toggle.selected();
                                            state
                                                .update(cx, |state, cx| {
                                                    state.toggle_subtask(&item_id, line, done, cx).detach_and_log_err(cx)
                                                })
                                                .log_err();
                                        }),
                                )
                            }))
                    })
                    .when(!description.is_empty(), |this| {
                        this.child(Self::section_label("DESCRIÇÃO", None)).child(
                            div().px_4().pb_3().child(Label::new(description).size(LabelSize::Small).color(Color::Muted)),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_2p5()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(colors.border)
                    .when_some(self.error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::XSmall).color(Color::Error))
                    })
                    .when(self.commenting, |this| {
                        this.child(
                            div()
                                .key_context("BoardComment")
                                .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                    this.post_comment(comment_item.clone(), window, cx)
                                }))
                                .child(self.comment_input.clone()),
                        )
                    })
                    .when(!started && item.kind == ItemKind::Issue, |this| {
                        this.child(
                            Label::new("Começar: cria o branch, faz checkout, move para Em andamento e atribui a você.")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .when(item.kind != ItemKind::Draft, |this| {
                                this.child(
                                    secondary_button("board-comment", Some((IconName::Chat, Color::Muted)), "Comentar", true, cx)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.commenting = !this.commenting;
                                            if this.commenting {
                                                window.focus(&this.comment_input.focus_handle(cx), cx);
                                            }
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                secondary_button("board-mention", Some((IconName::AtSign, Color::Muted)), "Agent", true, cx)
                                    .tooltip(Tooltip::text("Mencionar no Agent"))
                                    .on_click(move |_, window, cx| window.dispatch_action(mention.boxed_clone(), cx)),
                            )
                            .child(div().flex_1())
                            .when(item.kind == ItemKind::Issue && !on_branch, |this| {
                                this.child(
                                    primary_button(
                                        "board-start",
                                        Some(IconName::PlayFilled),
                                        if started { "Ir para o branch" } else { "Começar" },
                                        Some(KeyBinding::for_action_in(&StartItem, &self.focus_handle, cx)),
                                        cx,
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected = Some(item_id.clone());
                                        this.start_selected(&StartItem, window, cx)
                                    })),
                                )
                            }),
                    ),
            )
    }
}

impl Render for ProjectBoard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let availability = self.state.read(cx).availability(cx);
        let content: AnyElement = match availability {
            Availability::Ready => {
                let state = self.state.read(cx);
                let project = state.project().cloned();
                match project {
                    Some(project) => {
                        let login = state.viewer_login().unwrap_or_default().to_string();
                        let synced = state.synced_at().map(ago);
                        let items = self.filtered(&project, &login, cx);
                        let selected = self
                            .selected
                            .as_deref()
                            .and_then(|id| project.item(id))
                            .cloned();
                        let footer = format!(
                            "{} · {} itens{}",
                            project.summary.title,
                            project.items.len(),
                            synced
                                .map(|ago| format!(" · sincronizado {ago}"))
                                .unwrap_or_default()
                        );
                        let rate = format!(
                            "GraphQL {} / {} pontos",
                            project.rate_limit.remaining, project.rate_limit.limit
                        );
                        v_flex()
                            .size_full()
                            .child(self.render_toolbar(&project, cx))
                            .when(self.adding_item, |this| {
                                this.child(
                                    h_flex()
                                        .px_4()
                                        .py_2()
                                        .gap_2()
                                        .border_b_1()
                                        .border_color(colors.border)
                                        .key_context("BoardNewItem")
                                        .on_action(cx.listener(
                                            |this, _: &menu::Confirm, window, cx| {
                                                this.submit_new_item(window, cx)
                                            },
                                        ))
                                        .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                                            this.adding_item = false;
                                            cx.notify();
                                        }))
                                        .child(div().w(px(420.)).child(self.new_item_input.clone()))
                                        .child(
                                            Label::new(
                                                "Enter cria um rascunho no projeto · Esc cancela",
                                            )
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        ),
                                )
                            })
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_h_0()
                                    .child(match self.view {
                                        View::Board => self
                                            .render_board(&project, &items, cx)
                                            .into_any_element(),
                                        View::Table => self
                                            .render_table(&project, &items, cx)
                                            .into_any_element(),
                                    })
                                    .when_some(selected, |this, item| {
                                        this.child(
                                            self.render_inspector(&project, &item, window, cx),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .h(px(24.))
                                    .px_3()
                                    .border_t_1()
                                    .border_color(colors.border)
                                    .child(
                                        Label::new(footer)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Label::new(rate)
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    ),
                            )
                            .into_any_element()
                    }
                    None => div().into_any_element(),
                }
            }
            other => {
                let (title, detail): (&str, Option<SharedString>) = match other {
                    Availability::NotConnected => (
                        "Conecte o GitHub no dock Repo",
                        Some("Repo e Tarefas usam a mesma conta do GitHub.".into()),
                    ),
                    Availability::NoProjects => ("Nenhum projeto aberto", None),
                    Availability::MissingScope => (
                        "Falta acesso ao GitHub Projects",
                        Some("Rode gh auth refresh -s project, ou dê Projects ao token fine-grained.".into()),
                    ),
                    Availability::Failed(error) => ("Não deu para carregar o projeto", Some(error)),
                    _ => ("Carregando o projeto…", None),
                };
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Blocks)
                            .size(IconSize::Medium)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .children(detail.map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("ProjectBoard")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_item))
            .on_action(cx.listener(Self::start_selected))
            .size_full()
            .bg(colors.editor_background)
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
