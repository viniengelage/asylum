//! Board E: the Tarefas dock. The item of the checked-out branch sits on top, then the items
//! grouped by the project's Status field.

use crate::{
    Availability, RepositoryProjects, ToggleFocus,
    api::{self, FieldOption, Item, Project},
    board, github_repository, repository_projects, start_item,
};
use chrono::{Local, NaiveDate};
use gpui::{
    Action as _, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Pixels,
    Subscription, WeakEntity, px,
};
use project::{Project as WorkspaceProject, git_store::GitStoreEvent};
use repo_hosting::api::RemoteRepository;
use std::{collections::HashSet, time::Instant};
use ui::{
    ContextMenu, PopoverMenu, PopoverTriggerArea, Tooltip, prelude::*, secondary_button,
    segment_option, segmented_control,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mine,
    Iteration,
    Mentions,
}

pub struct TasksPanel {
    focus_handle: FocusHandle,
    project: Entity<WorkspaceProject>,
    workspace: WeakEntity<Workspace>,
    repository: Option<RemoteRepository>,
    branch: Option<String>,
    state: Option<Entity<RepositoryProjects>>,
    tab: Tab,
    filter_input: Entity<InputField>,
    comment_input: Entity<InputField>,
    commenting: bool,
    collapsed: HashSet<String>,
    error: Option<SharedString>,
    _state_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl TasksPanel {
    pub fn new(
        project: Entity<WorkspaceProject>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "Filtrar por título, # ou label")
                .start_icon(IconName::MagnifyingGlass)
        });
        let comment_input = cx.new(|cx| InputField::new(window, cx, "Comentar no item…"));
        let mut subscriptions = Vec::new();
        let editor = filter_input.read(cx).editor().clone();
        let this = cx.weak_entity();
        subscriptions.push(editor.subscribe(
            Box::new(move |_event, _window, cx| {
                this.update(cx, |_, cx| cx.notify()).log_err();
            }),
            window,
            cx,
        ));
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryUpdated(_, _, true)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_) => this.refresh_repository(cx),
            _ => {}
        }));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            project,
            workspace,
            repository: None,
            branch: None,
            state: None,
            tab: Tab::Mine,
            filter_input,
            comment_input,
            commenting: false,
            collapsed: HashSet::default(),
            error: None,
            _state_subscription: None,
            _subscriptions: subscriptions,
        };
        this.refresh_repository(cx);
        this
    }

    fn refresh_repository(&mut self, cx: &mut Context<Self>) {
        let found = github_repository(&self.project, cx);
        self.branch = found.as_ref().and_then(|(_, _, branch)| branch.clone());
        let repository = found.map(|(repository, _, _)| repository);
        if repository != self.repository {
            self.repository = repository.clone();
            self.state = repository.map(|repository| repository_projects(&repository, cx));
            self._state_subscription = self
                .state
                .as_ref()
                .map(|state| cx.observe(state, |_, _, cx| cx.notify()));
        }
        cx.notify();
    }

    fn report(&mut self, task: gpui::Task<anyhow::Result<()>>, cx: &mut Context<Self>) {
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

    fn start(&mut self, item: &Item, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };
        self.error = None;
        let task = start_item(
            self.workspace.clone(),
            self.project.clone(),
            state,
            item.id.clone(),
            api::branch_name(item),
            window,
            cx,
        );
        self.report(task, cx);
    }

    fn open_board(&mut self, item_id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                board::open(workspace, item_id, window, cx)
            })
            .log_err();
    }

    fn post_comment(&mut self, item_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };
        let body = self.comment_input.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        self.comment_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.commenting = false;
        let task = state.update(cx, |state, cx| state.comment(&item_id, body, cx));
        self.report(task, cx);
    }

    fn visible_items<'a>(&self, project: &'a Project, login: &str, cx: &App) -> Vec<&'a Item> {
        let today = Local::now().date_naive();
        let current = project
            .current_iteration(today)
            .map(|iteration| iteration.id.clone());
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        project
            .items
            .iter()
            .filter(|item| match self.tab {
                Tab::Mine => item.is_assigned_to(login),
                Tab::Iteration => {
                    current.is_some()
                        && project.iteration_of(item).map(|iteration| &iteration.id)
                            == current.as_ref()
                }
                Tab::Mentions => item.body.contains(&format!("@{login}")),
            })
            .filter(|item| filter.is_empty() || item_matches(item, &filter))
            .collect()
    }

    fn render_source(&self, state: &RepositoryProjects, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let title = state
            .project()
            .map(|project| project.summary.title.clone())
            .unwrap_or_else(|| "GitHub Projects".to_string());
        let login = state.viewer_login().unwrap_or_default().to_string();
        let projects = state.projects().to_vec();
        let weak_state = self.state.as_ref().map(|state| state.downgrade());
        let project_url = state.project().map(|project| project.summary.url.clone());
        let panel = cx.weak_entity();
        h_flex()
            .h(px(40.))
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .size(px(20.))
                    .justify_center()
                    .rounded(px(5.))
                    .bg(colors.border_focused)
                    .child(
                        Label::new("GH")
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::BOLD)
                            .color(Color::Custom(gpui::white())),
                    ),
            )
            .child(
                PopoverMenu::new("tasks-project-picker")
                    .trigger(PopoverTriggerArea::new(
                        h_flex()
                            .id("tasks-project-trigger")
                            .gap_2()
                            .min_w_0()
                            .child(
                                Label::new(title)
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .child(
                                Label::new(format!("GitHub Projects · @{login}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ))
                    .menu(move |window, cx| {
                        let weak_state = weak_state.clone();
                        let projects = projects.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for summary in projects {
                                let weak_state = weak_state.clone();
                                let id = summary.id.clone();
                                menu = menu.entry(summary.title.clone(), None, move |_, cx| {
                                    if let Some(state) = weak_state.as_ref() {
                                        state
                                            .update(cx, |state, cx| {
                                                state.select_project(id.clone(), cx)
                                            })
                                            .log_err();
                                    }
                                });
                            }
                            menu
                        }))
                    }),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("tasks-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(state.is_syncing())
                    .tooltip(Tooltip::text("Sincronizar agora"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(state) = &this.state {
                            state.update(cx, |state, cx| state.sync(cx));
                        }
                    })),
            )
            .child(
                PopoverMenu::new("tasks-options")
                    .trigger_with_tooltip(
                        IconButton::new("tasks-options-trigger", IconName::Sliders)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted),
                        Tooltip::text("Opções"),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let panel = panel.clone();
                        let project_url = project_url.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.entry("Abrir quadro", None, {
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| panel.open_board(None, window, cx))
                                        .log_err();
                                }
                            })
                            .when_some(project_url, |menu, url| {
                                menu.entry("Abrir no GitHub", None, move |_, cx| cx.open_url(&url))
                            })
                        }))
                    }),
            )
    }

    fn render_focus_card(
        &self,
        project: &Project,
        item: &Item,
        state: &RepositoryProjects,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let subtasks = item.subtasks();
        let done = subtasks.iter().filter(|subtask| subtask.done).count();
        let url = item.url.clone();
        let item_id = item.id.clone();
        let status_menu = status_menu(
            "tasks-focus-status",
            project,
            item,
            self.state.clone(),
            PopoverTriggerArea::new(secondary_button(
                "tasks-focus-move",
                Some((IconName::ArrowRight, Color::Muted)),
                "Mover",
                true,
                cx,
            )),
        );
        div().p_2().child(
            v_flex()
                .gap_2p5()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(colors.border_focused)
                .bg(colors.element_background)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("EM FOCO")
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::BOLD)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new("branch atual")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(item.reference())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        )
                        .when_some(url, |this, url| {
                            this.child(
                                IconButton::new("tasks-focus-open", IconName::ArrowUpRight)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Abrir no GitHub"))
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                )
                .child(
                    Label::new(item.title.clone())
                        .size(LabelSize::Default)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(item_chips(project, item, true, cx))
                .child(
                    h_flex()
                        .h(px(40.))
                        .px_2p5()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .child(
                            Icon::new(IconName::ListTodo)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("{done}/{}", subtasks.len()))
                                .size(LabelSize::Default)
                                .buffer_font(cx)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(if subtasks.is_empty() {
                                "sem subtarefas".to_string()
                            } else {
                                "subtarefas".to_string()
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .when(state.is_pending(&item.id), |this| {
                            this.child(
                                Label::new("salvando…")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .child(div().flex_1())
                        .child(status_menu),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("tasks-focus-comment", "Comentar")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.commenting = !this.commenting;
                                    if this.commenting {
                                        window.focus(&this.comment_input.focus_handle(cx), cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("tasks-focus-board", "Abrir quadro")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Blocks)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_board(Some(item_id.clone()), window, cx)
                                })),
                        ),
                )
                .when(self.commenting, |this| {
                    let item_id = item.id.clone();
                    this.child(
                        div()
                            .key_context("TasksComment")
                            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                                this.post_comment(item_id.clone(), window, cx)
                            }))
                            .child(self.comment_input.clone()),
                    )
                }),
        )
    }

    fn render_row(&self, project: &Project, item: &Item, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status_color = project
            .status_of(item)
            .map_or(colors.text_muted, |option| option_color(option, cx));
        let started = !item.linked_branches.is_empty();
        let item_for_start = item.clone();
        let item_id = item.id.clone();
        let right: Option<(String, Color)> = item
            .pull_requests
            .first()
            .map(|pull_request| (format!("PR #{}", pull_request.number), Color::Accent))
            .or_else(|| {
                project
                    .priority_of(item)
                    .filter(|priority| priority.name.contains('0') || priority.name.contains('1'))
                    .map(|_| ("!".to_string(), Color::Error))
            });
        div().px_1p5().child(
            h_flex()
                .id(SharedString::from(format!("tasks-row-{}", item.id)))
                .group("tasks-row")
                .h(px(46.))
                .px_2p5()
                .gap_2p5()
                .rounded_lg()
                .cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_board(Some(item_id.clone()), window, cx)
                }))
                .child(div().size(px(7.)).rounded_full().bg(status_color))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            Label::new(item.title.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .child(
                            Label::new(item_meta(project, item))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(!started && item.kind == api::ItemKind::Issue, |this| {
                    this.child(
                        div()
                            .invisible()
                            .group_hover("tasks-row", |this| this.visible())
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("tasks-start-{}", item.id)),
                                    IconName::PlayOutlined,
                                )
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(
                                    "Começar: cria o branch e move para em andamento",
                                ))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.start(&item_for_start, window, cx)
                                    },
                                )),
                            ),
                    )
                })
                .when_some(right, |this, (text, color)| {
                    this.child(
                        Label::new(text)
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::MEDIUM)
                            .color(color),
                    )
                }),
        )
    }

    fn render_list(&self, project: &Project, login: &str, cx: &Context<Self>) -> impl IntoElement {
        let items = self.visible_items(project, login, cx);
        let mut order: Vec<&FieldOption> = Vec::new();
        for option in [project.in_progress_option(), project.review_option()]
            .into_iter()
            .flatten()
        {
            order.push(option);
        }
        for option in project.columns() {
            if !order.iter().any(|existing| existing.id == option.id) {
                order.push(option);
            }
        }
        let done_id = project.done_option().map(|option| option.id.clone());
        let mut groups: Vec<(Option<&FieldOption>, Vec<&Item>)> = order
            .into_iter()
            .map(|option| {
                let group: Vec<&Item> = items
                    .iter()
                    .copied()
                    .filter(|item| {
                        project.status_of(item).map(|status| &status.id) == Some(&option.id)
                    })
                    .collect();
                (Some(option), group)
            })
            .collect();
        let without_status: Vec<&Item> = items
            .iter()
            .copied()
            .filter(|item| project.status_of(item).is_none())
            .collect();
        groups.push((None, without_status));

        v_flex()
            .id("tasks-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_2()
            .children(
                groups
                    .into_iter()
                    .filter(|(_, items)| !items.is_empty())
                    .map(|(option, items)| {
                        let key = option
                            .map_or_else(|| "sem-status".to_string(), |option| option.id.clone());
                        let collapsed = self.collapsed.contains(&key)
                            || (option.map(|option| &option.id) == done_id.as_ref()
                                && !self.collapsed.contains(&format!("{key}-open")));
                        let title = option.map_or_else(
                            || "SEM STATUS".to_string(),
                            |option| option.name.to_uppercase(),
                        );
                        let color = option.map_or(cx.theme().colors().text_muted, |option| {
                            option_color(option, cx)
                        });
                        let count = items.len();
                        let toggle_key = key.clone();
                        let is_done = option.map(|option| &option.id) == done_id.as_ref();
                        v_flex()
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!("tasks-group-{key}")))
                                    .gap_1p5()
                                    .px_3p5()
                                    .pt_3()
                                    .pb_1()
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if is_done {
                                            let open_key = format!("{toggle_key}-open");
                                            if !this.collapsed.remove(&open_key) {
                                                this.collapsed.insert(open_key);
                                            }
                                        } else if !this.collapsed.remove(&toggle_key) {
                                            this.collapsed.insert(toggle_key.clone());
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        Icon::new(if collapsed {
                                            IconName::ChevronRight
                                        } else {
                                            IconName::ChevronDown
                                        })
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                    .child(div().size(px(6.)).rounded_full().bg(color))
                                    .child(
                                        Label::new(title)
                                            .size(LabelSize::XSmall)
                                            .weight(FontWeight::SEMIBOLD)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(count.to_string())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .when(!collapsed, |this| {
                                this.children(
                                    items
                                        .into_iter()
                                        .map(|item| self.render_row(project, item, cx)),
                                )
                            })
                    }),
            )
    }

    fn render_footer(
        &self,
        state: &RepositoryProjects,
        project: &Project,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let login = state.viewer_login().unwrap_or_default().to_string();
        let today = Local::now().date_naive();
        let synced = state
            .synced_at()
            .map(|at| format!("Sincronizado {}", ago(at)))
            .unwrap_or_else(|| "Sincronizando…".to_string());
        let mine = project
            .items
            .iter()
            .filter(|item| item.is_assigned_to(&login))
            .count();
        let detail = match project.current_iteration(today) {
            Some(iteration) => format!("{synced} · {mine} itens seus no {}", iteration.title),
            None => format!("{synced} · {mine} itens seus"),
        };
        let url = project.summary.url.clone();
        h_flex()
            .h(px(48.))
            .px_3()
            .gap_2p5()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(avatar(&login, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(login).size(LabelSize::Small))
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                IconButton::new("tasks-open-project", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Abrir o projeto no GitHub"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
    }

    fn render_message(
        &self,
        icon: IconName,
        title: impl Into<SharedString>,
        detail: Option<SharedString>,
        action: Option<AnyElement>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2p5()
            .px_6()
            .child(Icon::new(icon).size(IconSize::Medium).color(Color::Muted))
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    div().text_center().child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .children(action)
            .into_any_element()
    }
}

/// The colors GitHub lets a project's options have, in the theme's terms.
pub(crate) fn option_color(option: &FieldOption, cx: &App) -> Hsla {
    match option.color.as_str() {
        "BLUE" => Color::Info.color(cx),
        "GREEN" => Color::Success.color(cx),
        "YELLOW" | "ORANGE" => Color::Warning.color(cx),
        "RED" => Color::Error.color(cx),
        "PURPLE" | "PINK" => Color::Accent.color(cx),
        _ => cx.theme().colors().text_muted,
    }
}

/// A tinted chip, as the boards draw labels, fields and links.
pub(crate) fn chip(
    label: impl Into<SharedString>,
    color: Hsla,
    icon: Option<IconName>,
    mono: bool,
    cx: &App,
) -> impl IntoElement {
    let neutral = color == cx.theme().colors().text;
    h_flex()
        .h(px(20.))
        .px(px(7.))
        .gap_1()
        .rounded(px(5.))
        .bg(if neutral {
            cx.theme().colors().element_hover
        } else {
            color.opacity(0.14)
        })
        .children(icon.map(|icon| {
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(Color::Custom(color))
        }))
        .child({
            let label = Label::new(label)
                .size(LabelSize::XSmall)
                .weight(FontWeight::MEDIUM)
                .color(Color::Custom(color));
            if mono { label.buffer_font(cx) } else { label }
        })
}

/// Status, linked pull request, iteration and priority, as the cards show them.
pub(crate) fn item_chips(
    project: &Project,
    item: &Item,
    with_status: bool,
    cx: &App,
) -> impl IntoElement {
    let text = cx.theme().colors().text;
    h_flex()
        .flex_wrap()
        .gap_1p5()
        .when_some(
            project.status_of(item).filter(|_| with_status),
            |this, status| {
                this.child(chip(
                    status.name.clone(),
                    option_color(status, cx),
                    None,
                    false,
                    cx,
                ))
            },
        )
        .children(item.pull_requests.first().map(|pull_request| {
            let (color, suffix) = match pull_request.state.as_str() {
                "MERGED" => (Color::Accent.color(cx), " · merge"),
                "CLOSED" => (cx.theme().colors().text_muted, " · fechado"),
                _ => (Color::Accent.color(cx), ""),
            };
            chip(
                format!("PR #{}{suffix}", pull_request.number),
                color,
                Some(IconName::PullRequest),
                false,
                cx,
            )
        }))
        .children(project.iteration_of(item).map(|iteration| {
            chip(
                iteration.title.clone(),
                text,
                Some(IconName::History),
                false,
                cx,
            )
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
        .children(item.labels.iter().take(2).map(|label| {
            let color = if label.name.eq_ignore_ascii_case("bug") {
                Color::Error.color(cx)
            } else {
                Color::Info.color(cx)
            };
            chip(label.name.clone(), color, None, false, cx)
        }))
}

/// `#201 · feature · P2 · 3 pts`.
pub(crate) fn item_meta(project: &Project, item: &Item) -> String {
    let mut parts = vec![match item.number {
        Some(number) => format!("#{number}"),
        None => "rascunho".to_string(),
    }];
    parts.extend(item.labels.first().map(|label| label.name.clone()));
    parts.extend(
        project
            .priority_of(item)
            .map(|priority| priority.name.clone()),
    );
    if let Some(estimate) = project.estimate_of(item) {
        parts.push(format!("{} pts", format_number(estimate)));
    }
    if let Some(pull_request) = item.pull_requests.first() {
        parts.push(format!("PR #{}", pull_request.number));
    }
    parts.join(" · ")
}

pub(crate) fn format_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        format!("{number}")
    }
}

pub(crate) fn avatar(login: &str, cx: &App) -> impl IntoElement {
    let initials: String = login
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .take(2)
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase();
    let initials = if initials.chars().count() < 2 {
        login.chars().take(2).collect::<String>().to_uppercase()
    } else {
        initials
    };
    h_flex()
        .flex_none()
        .size(px(22.))
        .justify_center()
        .rounded_full()
        .bg(cx.theme().colors().border_focused.opacity(0.25))
        .child(
            Label::new(initials)
                .size(LabelSize::XSmall)
                .weight(FontWeight::BOLD)
                .color(Color::Accent),
        )
}

pub(crate) fn ago(at: Instant) -> String {
    let seconds = at.elapsed().as_secs();
    match seconds {
        0..=59 => format!("há {seconds} s"),
        60..=3599 => format!("há {} min", seconds / 60),
        _ => format!("há {} h", seconds / 3600),
    }
}

pub(crate) fn item_matches(item: &Item, filter: &str) -> bool {
    let filter = filter.trim_start_matches('#');
    item.title.to_lowercase().contains(filter)
        || item
            .number
            .is_some_and(|number| number.to_string() == filter)
        || item
            .labels
            .iter()
            .any(|label| label.name.to_lowercase().contains(filter))
}

/// A popover listing the project's columns that moves `item` to the one picked.
pub(crate) fn status_menu(
    id: &'static str,
    project: &Project,
    item: &Item,
    state: Option<Entity<RepositoryProjects>>,
    trigger: impl ui::PopoverTrigger,
) -> impl IntoElement {
    let options = project.columns().to_vec();
    let current = project.status_of(item).map(|option| option.id.clone());
    let item_id = item.id.clone();
    PopoverMenu::new(SharedString::from(format!("{id}-{}", item.id)))
        .trigger(trigger)
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            let options = options.clone();
            let current = current.clone();
            let item_id = item_id.clone();
            let state = state.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for option in options {
                    let state = state.clone();
                    let item_id = item_id.clone();
                    let option_id = option.id.clone();
                    menu = menu.toggleable_entry(
                        option.name.clone(),
                        current.as_deref() == Some(option.id.as_str()),
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| {
                            if let Some(state) = &state {
                                state
                                    .update(cx, |state, cx| {
                                        state.move_item(item_id.clone(), option_id.clone(), cx)
                                    })
                                    .detach_and_log_err(cx);
                            }
                        },
                    );
                }
                menu
            }))
        })
}

pub(crate) fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl Render for TasksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match self.state.clone() {
            None => self.render_message(
                IconName::Blocks,
                "Projeto fora do GitHub",
                Some("O Tarefas do GitHub Projects aparece quando o origin é um repositório do github.com.".into()),
                None,
            ),
            Some(state_entity) => {
                let state = state_entity.read(cx);
                match state.availability(cx) {
                    Availability::NotConnected => self.render_message(
                        IconName::Disconnected,
                        "Conecte o GitHub no dock Repo",
                        Some("Repo e Tarefas usam a mesma conta do GitHub.".into()),
                        Some(
                            secondary_button("tasks-open-repo", Some((IconName::PullRequest, Color::Muted)), "Abrir o Repo", true, cx)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(repo_hosting::ToggleFocus.boxed_clone(), cx)
                                })
                                .into_any_element(),
                        ),
                    ),
                    Availability::Loading => self.render_message(IconName::LoadCircle, "Carregando o projeto…", None, None),
                    Availability::NoProjects => self.render_message(
                        IconName::Blocks,
                        "Nenhum projeto aberto",
                        Some(format!("Nem {} nem a sua conta têm um GitHub Project aberto.", state.repository().full_name()).into()),
                        None,
                    ),
                    Availability::MissingScope => self.render_message(
                        IconName::Lock,
                        "Falta acesso ao GitHub Projects",
                        Some("Rode gh auth refresh -s project, ou dê Projects ao token fine-grained, e tente de novo.".into()),
                        Some(
                            secondary_button("tasks-retry", Some((IconName::RotateCw, Color::Muted)), "Tentar de novo", true, cx)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(state) = &this.state {
                                        state.update(cx, |state, cx| state.sync(cx));
                                    }
                                }))
                                .into_any_element(),
                        ),
                    ),
                    Availability::Failed(error) => self.render_message(
                        IconName::Warning,
                        "Não deu para carregar o projeto",
                        Some(error),
                        Some(
                            secondary_button("tasks-retry", Some((IconName::RotateCw, Color::Muted)), "Tentar de novo", true, cx)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(state) = &this.state {
                                        state.update(cx, |state, cx| state.sync(cx));
                                    }
                                }))
                                .into_any_element(),
                        ),
                    ),
                    Availability::Ready => {
                        let Some(project) = state.project() else {
                            return div();
                        };
                        let login = state.viewer_login().unwrap_or_default().to_string();
                        let focus = self
                            .branch
                            .as_deref()
                            .and_then(|branch| api::item_for_branch(&project.items, branch));
                        let today = today();
                        let iteration = project.current_iteration(today);
                        let mine = project.items.iter().filter(|item| item.is_assigned_to(&login)).count();
                        let in_iteration = iteration.map(|iteration| {
                            project
                                .items
                                .iter()
                                .filter(|item| project.iteration_of(item).map(|it| &it.id) == Some(&iteration.id))
                                .count()
                        });
                        let mentions = project
                            .items
                            .iter()
                            .filter(|item| item.body.contains(&format!("@{login}")))
                            .count();
                        let tabs: Vec<(Tab, String, Option<usize>)> = [
                            Some((Tab::Mine, "Minhas".to_string(), Some(mine))),
                            iteration.map(|iteration| (Tab::Iteration, iteration.title.clone(), in_iteration)),
                            Some((Tab::Mentions, "Menções".to_string(), Some(mentions))),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        v_flex()
                            .size_full()
                            .child(self.render_source(state, cx))
                            .when_some(focus, |this, item| this.child(self.render_focus_card(project, item, state, cx)))
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    div().px_3().py_1().child(Label::new(error).size(LabelSize::XSmall).color(Color::Error)),
                                )
                            })
                            .child(
                                div().px_2().py_1().child(
                                    segmented_control(cx).children(tabs.into_iter().map(|(tab, label, count)| {
                                        let label = match count {
                                            Some(count) => format!("{label} {count}"),
                                            None => label,
                                        };
                                        segment_option(
                                            SharedString::from(format!("tasks-tab-{label}")),
                                            label,
                                            self.tab == tab,
                                            true,
                                            cx,
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.tab = tab;
                                            cx.notify();
                                        }))
                                    })),
                                ),
                            )
                            .child(
                                h_flex()
                                    .px_2()
                                    .py_1p5()
                                    .gap_1p5()
                                    .child(div().flex_1().child(self.filter_input.clone()))
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .px_1p5()
                                            .child(Icon::new(IconName::ListTree).size(IconSize::XSmall).color(Color::Muted))
                                            .child(Label::new(project.status.as_ref().map_or("Status".to_string(), |field| field.name.clone())).size(LabelSize::Small)),
                                    ),
                            )
                            .child(self.render_list(project, &login, cx))
                            .child(self.render_footer(state, project, cx))
                            .into_any_element()
                    }
                }
            }
        };
        v_flex()
            .key_context("TasksPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(content)
    }
}

impl Focusable for TasksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TasksPanel {}

impl Panel for TasksPanel {
    fn persistent_name() -> &'static str {
        "GitHubTasksPanel"
    }

    fn panel_key() -> &'static str {
        "GitHubTasksPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(400.)
    }

    /// No icon: the toolkit button is the way in, as with the Repo and ClickUp docks.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Tarefas")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}
