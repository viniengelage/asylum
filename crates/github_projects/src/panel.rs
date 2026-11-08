//! Board E: the Tarefas dock. The item of the checked-out branch sits on top, then the items
//! grouped by the project's Status field. The item selected here or on the board replaces the
//! list with its inspector.

use crate::{
    Availability, RepositoryProjects, StartItem, ToggleFocus,
    api::{self, FieldOption, Item, ItemKind, NewValue, Project},
    board, github_repository, repository_projects, start_item,
};
use chrono::{Local, NaiveDate};
use gpui::{
    Action as _, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    Subscription, WeakEntity, px,
};
use i18n::t;
use project::{Project as WorkspaceProject, git_store::GitStoreEvent};
use repo_hosting::api::RemoteRepository;
use std::{collections::HashSet, rc::Rc, time::Instant};
use ui::{
    ContextMenu, EmptyState, EmptyStateTone, HeaderBar, KeyBinding, PopoverMenu,
    PopoverTriggerArea, RowStatus, StatusRow, StatusTone, Tooltip, capsule_button, card,
    error_callout, grouped_block, navigator_footer, prelude::*, primary_button, secondary_button,
    section_header, section_label, segment_option, segmented_control, status_chip, status_pill,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const ESTIMATES: [f64; 6] = [1.0, 2.0, 3.0, 5.0, 8.0, 13.0];

type MenuBuilder = Rc<dyn Fn(&mut Window, &mut App) -> Entity<ContextMenu>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mine,
    Iteration,
    All,
}

pub struct TasksPanel {
    focus_handle: FocusHandle,
    project: Entity<WorkspaceProject>,
    workspace: WeakEntity<Workspace>,
    repository: Option<RemoteRepository>,
    branch: Option<String>,
    state: Option<Entity<RepositoryProjects>>,
    tab: Tab,
    inspected: Option<String>,
    filter_input: Entity<InputField>,
    branch_input: Entity<InputField>,
    /// The item the branch field was filled for, so typing in it isn't overwritten.
    branch_filled_for: Option<String>,
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
            InputField::new(window, cx, &t!("github_projects.panel.filter_placeholder"))
                .start_icon(IconName::MagnifyingGlass)
        });
        let branch_input = cx.new(|cx| InputField::new(window, cx, "feat/…"));
        let comment_input = cx
            .new(|cx| InputField::new(window, cx, &t!("github_projects.item.comment_placeholder")));
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
            inspected: None,
            filter_input,
            branch_input,
            branch_filled_for: None,
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

    pub fn inspect(&mut self, item_id: Option<String>, cx: &mut Context<Self>) {
        if self.inspected != item_id {
            self.commenting = false;
            self.error = None;
        }
        self.inspected = item_id;
        cx.notify();
    }

    pub fn inspected_item(&self) -> Option<&str> {
        self.inspected.as_deref()
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

    fn move_item(&mut self, item_id: String, option_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };
        self.error = None;
        let task = state.update(cx, |state, cx| state.move_item(item_id, option_id, cx));
        self.report(task, cx);
    }

    fn start(
        &mut self,
        item_id: String,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.state.clone() else {
            return;
        };
        self.error = None;
        let task = start_item(
            self.workspace.clone(),
            self.project.clone(),
            state,
            item_id,
            branch,
            window,
            cx,
        );
        self.report(task, cx);
    }

    fn start_inspected(&mut self, _: &StartItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item_id) = self.inspected.clone() else {
            return;
        };
        let branch = self.branch_input.read(cx).text(cx).trim().to_string();
        if branch.is_empty() {
            return;
        }
        self.start(item_id, branch, window, cx);
    }

    fn set_field(
        &mut self,
        item_id: String,
        field_id: String,
        value: NewValue,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.state.clone() else {
            return;
        };
        self.error = None;
        let task = state.update(cx, |state, cx| {
            state.set_field(item_id, field_id, value, cx)
        });
        self.report(task, cx);
    }

    fn assign_me(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };
        self.error = None;
        let task = state.update(cx, |state, cx| state.assign_me(&item_id, cx));
        self.report(task, cx);
    }

    fn open_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| board::open(workspace, None, window, cx))
            .log_err();
    }

    fn toggle_commenting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commenting = !self.commenting;
        if self.commenting {
            window.focus(&self.comment_input.focus_handle(cx), cx);
        }
        cx.notify();
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
        self.error = None;
        let task = state.update(cx, |state, cx| state.comment(&item_id, body, cx));
        self.report(task, cx);
    }

    fn fill_branch_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item_id) = self.inspected.clone() else {
            return;
        };
        if self.branch_filled_for.as_deref() == Some(item_id.as_str()) {
            return;
        }
        let Some(name) = self
            .state
            .as_ref()
            .and_then(|state| state.read(cx).project())
            .and_then(|project| project.item(&item_id))
            .map(|item| {
                item.linked_branches
                    .first()
                    .cloned()
                    .unwrap_or_else(|| api::branch_name(item))
            })
        else {
            return;
        };
        self.branch_input
            .update(cx, |input, cx| input.set_text(&name, window, cx));
        self.branch_filled_for = Some(item_id);
    }

    fn visible_items<'a>(&self, project: &'a Project, login: &str, cx: &App) -> Vec<&'a Item> {
        let current = project
            .current_iteration(today())
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
                Tab::All => true,
            })
            .filter(|item| filter.is_empty() || item_matches(item, &filter))
            .collect()
    }

    fn render_header(&self, state: &RepositoryProjects, cx: &Context<Self>) -> impl IntoElement {
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
        HeaderBar::new("tasks-header")
            .start_child(
                h_flex()
                    .size(px(20.))
                    .flex_none()
                    .justify_center()
                    .rounded(Radius::Sm.rems())
                    .bg(colors.border_focused)
                    .child(
                        Icon::new(IconName::Github)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(gpui::white())),
                    ),
            )
            .child(
                PopoverMenu::new("tasks-project-picker")
                    .trigger(PopoverTriggerArea::new(
                        h_flex()
                            .id("tasks-project-trigger")
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(
                                        Label::new(title)
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD)
                                            .truncate(),
                                    )
                                    .child(
                                        Label::new(format!("GitHub Projects · @{login}"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
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
            .end_child(
                IconButton::new("tasks-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(state.is_syncing())
                    .tooltip(Tooltip::text(t!("github_projects.panel.sync_now")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(state) = &this.state {
                            state.update(cx, |state, cx| state.sync(cx));
                        }
                    })),
            )
            .end_child(
                PopoverMenu::new("tasks-options")
                    .trigger_with_tooltip(
                        IconButton::new("tasks-options-trigger", IconName::Sliders)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted),
                        Tooltip::text(t!("github_projects.panel.options")),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let panel = panel.clone();
                        let project_url = project_url.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.entry(t!("github_projects.panel.open_board"), None, {
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| panel.open_board(window, cx))
                                        .log_err();
                                }
                            })
                            .when_some(project_url, |menu, url| {
                                menu.entry(
                                    t!("github_projects.item.open_on_github"),
                                    None,
                                    move |_, cx| cx.open_url(&url),
                                )
                            })
                        }))
                    }),
            )
    }

    fn render_comment_input(&self, item_id: String, cx: &Context<Self>) -> impl IntoElement {
        div()
            .key_context("TasksComment")
            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                this.post_comment(item_id.clone(), window, cx)
            }))
            .child(self.comment_input.clone())
    }

    fn render_focus_card(
        &self,
        project: &Project,
        item: &Item,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let url = item.url.clone();
        let move_menu = status_menu(
            "tasks-focus-status",
            project,
            item,
            {
                let this = cx.weak_entity();
                Rc::new(move |item_id, option_id, cx: &mut App| {
                    this.update(cx, |this, cx| this.move_item(item_id, option_id, cx))
                        .log_err();
                })
            },
            Button::new("tasks-focus-move", t!("github_projects.panel.move"))
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::ArrowRight)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
        );
        div().p_2().child(
            card(cx)
                .gap_2p5()
                .p_3()
                .border_color(colors.border_focused)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(t!("github_projects.panel.in_focus"))
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::BOLD)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(t!("github_projects.panel.in_focus_detail"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(item_number(item))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    Label::new(item.title.clone())
                        .size(LabelSize::Default)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(item_chips(project, item, cx))
                .child(
                    h_flex()
                        .gap_1()
                        .child(move_menu)
                        .when(item.kind != ItemKind::Draft, |this| {
                            this.child(
                                Button::new(
                                    "tasks-focus-comment",
                                    t!("github_projects.item.comment"),
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_commenting(window, cx),
                                )),
                            )
                        })
                        .when_some(url, |this, url| {
                            this.child(
                                Button::new(
                                    "tasks-focus-open",
                                    t!("github_projects.item.open_on_github"),
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::ArrowUpRight)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                )
                .when(self.commenting, |this| {
                    this.child(self.render_comment_input(item.id.clone(), cx))
                }),
        )
    }

    fn render_row(&self, project: &Project, item: &Item, cx: &Context<Self>) -> impl IntoElement {
        let dot = project
            .status_of(item)
            .map_or(StatusTone::Neutral, option_tone)
            .color(cx);
        let can_start = item.linked_branches.is_empty() && item.kind == ItemKind::Issue;
        let start_id = item.id.clone();
        let start_branch = api::branch_name(item);
        let item_id = item.id.clone();
        div().px_1p5().child(
            StatusRow::new(
                SharedString::from(format!("tasks-row-{}", item.id)),
                RowStatus::Running,
                item.title.clone(),
            )
            .dot_color(dot)
            .detail(item_meta(item))
            .group("tasks-row")
            .end_slot(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .when(can_start, |this| {
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
                                    .tooltip(Tooltip::text(t!(
                                        "github_projects.panel.start_tooltip"
                                    )))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.start(
                                                start_id.clone(),
                                                start_branch.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                ),
                        )
                    })
                    .children(priority_pill(project, item, cx)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.inspect(Some(item_id.clone()), cx))),
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
                        let is_done = option.map(|option| &option.id) == done_id.as_ref();
                        let collapsed = self.collapsed.contains(&key)
                            || (is_done && !self.collapsed.contains(&format!("{key}-open")));
                        let title = option.map_or_else(
                            || t!("github_projects.panel.no_status").to_string(),
                            |option| option.name.to_uppercase(),
                        );
                        let tone = option.map_or(StatusTone::Neutral, option_tone);
                        let count = items.len();
                        let toggle_key = key.clone();
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
                                    .child(tone.dot(cx))
                                    .child(section_label(title))
                                    .child(section_label(count.to_string())),
                            )
                            .when(!collapsed, |this| {
                                this.child(
                                    v_flex().when(is_done, |this| this.opacity(0.6)).children(
                                        items
                                            .into_iter()
                                            .map(|item| self.render_row(project, item, cx)),
                                    ),
                                )
                            })
                    }),
            )
    }

    fn render_footer(&self, state: &RepositoryProjects, cx: &Context<Self>) -> impl IntoElement {
        let (tone, text) = match state.synced_at() {
            Some(at) if !state.is_syncing() => (
                StatusTone::Success,
                t!("github_projects.panel.synced", ago = ago(at)),
            ),
            _ => (StatusTone::Neutral, t!("github_projects.panel.syncing")),
        };
        navigator_footer(
            Some(tone),
            text,
            Some(
                capsule_button(
                    "tasks-open-board",
                    Some(IconName::Blocks),
                    t!("github_projects.panel.board"),
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_board(window, cx)))
                .into_any_element(),
            ),
            cx,
        )
        .py_2()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
    }

    fn render_error(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        self.error.clone().map(|error| {
            div().px_2().py_1().child(error_callout(
                t!("github_projects.panel.action_failed"),
                error,
                cx,
            ))
        })
    }

    fn render_overview(
        &self,
        project: &Project,
        state: &RepositoryProjects,
        cx: &Context<Self>,
    ) -> AnyElement {
        let login = state.viewer_login().unwrap_or_default().to_string();
        let focus = self
            .branch
            .as_deref()
            .and_then(|branch| api::item_for_branch(&project.items, branch));
        let iteration = project.current_iteration(today());
        let tabs: Vec<(Tab, String)> = [
            Some((Tab::Mine, t!("github_projects.panel.tab_mine").to_string())),
            iteration.map(|iteration| (Tab::Iteration, iteration.title.clone())),
            Some((Tab::All, t!("github_projects.panel.tab_all").to_string())),
        ]
        .into_iter()
        .flatten()
        .collect();
        v_flex()
            .size_full()
            .child(self.render_header(state, cx))
            .when_some(focus, |this, item| {
                this.child(self.render_focus_card(project, item, cx))
            })
            .children(self.render_error(cx))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .child(
                        segmented_control(cx).children(tabs.into_iter().map(|(tab, label)| {
                            segment_option(
                                SharedString::from(format!("tasks-tab-{label}")),
                                label,
                                self.tab == tab,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.tab = tab;
                                    cx.notify();
                                },
                            ))
                        })),
                    ),
            )
            .child(div().px_2().py_1p5().child(self.filter_input.clone()))
            .child(self.render_list(project, &login, cx))
            .child(self.render_footer(state, cx))
            .into_any_element()
    }

    fn option_menu(
        &self,
        item: &Item,
        field: &api::SingleSelectField,
        cx: &Context<Self>,
    ) -> MenuBuilder {
        let panel = cx.weak_entity();
        let item_id = item.id.clone();
        let field_id = field.id.clone();
        let current = item.option_id(&field.id).map(str::to_string);
        let options = field.options.clone();
        Rc::new(move |window, cx| {
            let panel = panel.clone();
            let item_id = item_id.clone();
            let field_id = field_id.clone();
            let current = current.clone();
            let options = options.clone();
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for option in options {
                    let panel = panel.clone();
                    let item_id = item_id.clone();
                    let field_id = field_id.clone();
                    let option_id = option.id.clone();
                    menu = menu.toggleable_entry(
                        option.name.clone(),
                        current.as_deref() == Some(option.id.as_str()),
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.set_field(
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
                menu.separator()
                    .entry(t!("github_projects.board.clear"), None, move |_, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.set_field(
                                    item_id.clone(),
                                    field_id.clone(),
                                    NewValue::Clear,
                                    cx,
                                )
                            })
                            .log_err();
                    })
            })
        })
    }

    fn iteration_menu(
        &self,
        item: &Item,
        field: &api::IterationField,
        cx: &Context<Self>,
    ) -> MenuBuilder {
        let panel = cx.weak_entity();
        let item_id = item.id.clone();
        let current = item.iteration_id(&field.id).map(str::to_string);
        let field = field.clone();
        Rc::new(move |window, cx| {
            let panel = panel.clone();
            let item_id = item_id.clone();
            let current = current.clone();
            let field = field.clone();
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for iteration in &field.iterations {
                    let panel = panel.clone();
                    let item_id = item_id.clone();
                    let field_id = field.id.clone();
                    let iteration_id = iteration.id.clone();
                    menu = menu.toggleable_entry(
                        iteration.title.clone(),
                        current.as_deref() == Some(iteration.id.as_str()),
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.set_field(
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
        })
    }

    fn estimate_menu(&self, item: &Item, field_id: String, cx: &Context<Self>) -> MenuBuilder {
        let panel = cx.weak_entity();
        let item_id = item.id.clone();
        Rc::new(move |window, cx| {
            let panel = panel.clone();
            let item_id = item_id.clone();
            let field_id = field_id.clone();
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for estimate in ESTIMATES {
                    let panel = panel.clone();
                    let item_id = item_id.clone();
                    let field_id = field_id.clone();
                    menu = menu.entry(format_number(estimate), None, move |_, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.set_field(
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
        })
    }

    fn assignee_menu(&self, item: &Item, cx: &Context<Self>) -> MenuBuilder {
        let panel = cx.weak_entity();
        let item_id = item.id.clone();
        Rc::new(move |window, cx| {
            let panel = panel.clone();
            let item_id = item_id.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                menu.entry(t!("github_projects.board.assign_me"), None, move |_, cx| {
                    panel
                        .update(cx, |panel, cx| panel.assign_me(item_id.clone(), cx))
                        .log_err();
                })
            })
        })
    }

    fn render_inspector(
        &self,
        project: &Project,
        item: &Item,
        state: &RepositoryProjects,
        cx: &Context<Self>,
    ) -> AnyElement {
        let login = state.viewer_login().unwrap_or_default().to_string();
        let default_branch = state
            .info()
            .and_then(|info| info.default_branch.clone())
            .unwrap_or_else(|| "main".to_string());
        let started = !item.linked_branches.is_empty();
        let on_branch = self
            .branch
            .as_ref()
            .is_some_and(|branch| item.linked_branches.contains(branch));
        let reference = match (&item.repository, item.number) {
            (Some(repository), Some(number)) => format!("{repository}#{number}"),
            _ => item.reference(),
        };
        let kind = match item.kind {
            ItemKind::Issue => t!("github_projects.board.kind_issue"),
            ItemKind::PullRequest => t!("github_projects.board.kind_pull_request"),
            ItemKind::Draft => t!("github_projects.board.kind_draft"),
        };
        let byline = match (&item.author, item.kind) {
            (Some(author), ItemKind::Issue) => {
                t!("github_projects.inspector.issue_opened_by", author = author)
            }
            (Some(author), ItemKind::PullRequest) => {
                t!(
                    "github_projects.inspector.pull_request_opened_by",
                    author = author
                )
            }
            (Some(author), ItemKind::Draft) => {
                t!(
                    "github_projects.inspector.draft_created_by",
                    author = author
                )
            }
            (None, _) => kind,
        };
        let description: String = item.body.trim().chars().take(600).collect();
        let muted_dash = || {
            Label::new("—")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        let option_value = |option: Option<&FieldOption>| match option {
            Some(option) => h_flex()
                .gap_1p5()
                .child(option_tone(option).dot(cx))
                .child(Label::new(option.name.clone()).size(LabelSize::Small))
                .into_any_element(),
            None => muted_dash(),
        };

        let mut fields: Vec<AnyElement> = vec![field_row(
            "tasks-field-status",
            t!("github_projects.item.status_field"),
            option_value(project.status_of(item)),
            project
                .status
                .as_ref()
                .map(|field| self.option_menu(item, field, cx)),
            cx,
        )];
        if let Some(field) = &project.iteration {
            let value = match project.iteration_of(item) {
                Some(iteration) => h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::History)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(iteration.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .into_any_element(),
                None => muted_dash(),
            };
            fields.push(field_row(
                "tasks-field-iteration",
                t!("github_projects.board.field_sprint"),
                value,
                Some(self.iteration_menu(item, field, cx)),
                cx,
            ));
        }
        if let Some(field) = &project.priority {
            fields.push(field_row(
                "tasks-field-priority",
                t!("github_projects.board.field_priority"),
                option_value(project.priority_of(item)),
                Some(self.option_menu(item, field, cx)),
                cx,
            ));
        }
        if let Some(field) = &project.estimate {
            fields.push(field_row(
                "tasks-field-estimate",
                t!("github_projects.board.field_estimate"),
                Label::new(
                    project
                        .estimate_of(item)
                        .map_or("—".to_string(), format_number),
                )
                .size(LabelSize::Small)
                .buffer_font(cx)
                .into_any_element(),
                Some(self.estimate_menu(item, field.id.clone(), cx)),
                cx,
            ));
        }
        let assignee_value = if item.assignees.is_empty() {
            h_flex()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Person)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(t!("github_projects.board.nobody"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_1p5()
                .children(item.assignees.first().map(|login| avatar(login, cx)))
                .child(
                    Label::new(item.assignees.join(", "))
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .into_any_element()
        };
        fields.push(field_row(
            "tasks-field-assignee",
            t!("github_projects.board.field_assignees"),
            assignee_value,
            (item.kind != ItemKind::Draft && !item.is_assigned_to(&login))
                .then(|| self.assignee_menu(item, cx)),
            cx,
        ));

        let can_start = item.kind == ItemKind::Issue && !on_branch;
        let item_url = item.url.clone();
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.inspect(None, cx)))
            .child(
                HeaderBar::new("tasks-inspector-header")
                    .start_child(
                        Button::new("tasks-inspector-back", t!("github_projects.dock.title"))
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                            .on_click(cx.listener(|this, _, _, cx| this.inspect(None, cx))),
                    )
                    .end_child(
                        Label::new(reference)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .when_some(item_url, |this, url| {
                        this.end_child(
                            IconButton::new("tasks-inspector-open", IconName::ArrowUpRight)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(t!("github_projects.item.open_on_github")))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .child(
                v_flex()
                    .id("tasks-inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .py_3()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new(byline)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(item.title.clone())
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(grouped_block(cx).children(fields))
                    .when(!description.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1p5()
                                .child(section_header(
                                    t!("github_projects.board.section_description"),
                                    None,
                                ))
                                .child(
                                    Label::new(description)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(section_header(
                                t!("github_projects.board.section_development"),
                                None,
                            ))
                            .when(
                                !started
                                    && item.pull_requests.is_empty()
                                    && item.kind == ItemKind::Issue,
                                |this| {
                                    this.child(self.branch_input.clone()).child(
                                        Label::new(t!(
                                            "github_projects.board.start_creates_branch",
                                            branch = default_branch
                                        ))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                },
                            )
                            .children(item.linked_branches.iter().map(|branch| {
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        Icon::new(IconName::GitBranch)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(branch.clone())
                                            .size(LabelSize::Small)
                                            .buffer_font(cx)
                                            .truncate(),
                                    )
                                    .when(self.branch.as_deref() == Some(branch.as_str()), |this| {
                                        this.child(status_chip(
                                            None,
                                            t!("github_projects.item.current_branch"),
                                            Color::Accent,
                                            cx,
                                        ))
                                    })
                            }))
                            .children(item.pull_requests.iter().map(|pull_request| {
                                let url = pull_request.url.clone();
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "tasks-pr-{}",
                                        pull_request.number
                                    )))
                                    .gap_1p5()
                                    .cursor_pointer()
                                    .on_click(move |_, _, cx| cx.open_url(&url))
                                    .child(pull_request_chip(pull_request, cx))
                            })),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .children(self.error.clone().map(|error| {
                        error_callout(t!("github_projects.panel.action_failed"), error, cx)
                    }))
                    .when(self.commenting, |this| {
                        this.child(self.render_comment_input(item.id.clone(), cx))
                    })
                    .child(
                        card(cx)
                            .gap_2p5()
                            .p_3()
                            .when(can_start && !started, |this| {
                                this.child(
                                    Label::new(t!("github_projects.board.start_hint"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .child(
                                h_flex()
                                    .gap_1()
                                    .when(item.kind != ItemKind::Draft, |this| {
                                        this.child(
                                            Button::new(
                                                "tasks-inspector-comment",
                                                t!("github_projects.item.comment"),
                                            )
                                            .style(ButtonStyle::Subtle)
                                            .label_size(LabelSize::Small)
                                            .start_icon(
                                                Icon::new(IconName::Chat)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.toggle_commenting(window, cx)
                                                }),
                                            ),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .when(can_start, |this| {
                                        this.child(
                                            primary_button(
                                                "tasks-inspector-start",
                                                Some(IconName::PlayFilled),
                                                if started {
                                                    t!("github_projects.board.go_to_branch")
                                                } else {
                                                    t!("github_projects.board.start")
                                                },
                                                Some(KeyBinding::for_action_in(
                                                    &StartItem,
                                                    &self.focus_handle,
                                                    cx,
                                                )),
                                                cx,
                                            )
                                            .on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.start_inspected(&StartItem, window, cx)
                                                }),
                                            ),
                                        )
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_ready(&self, state: &RepositoryProjects, cx: &Context<Self>) -> AnyElement {
        let Some(project) = state.project() else {
            return div().into_any_element();
        };
        match self.inspected.as_deref().and_then(|id| project.item(id)) {
            Some(item) => self.render_inspector(project, item, state, cx),
            None => self.render_overview(project, state, cx),
        }
    }
}

fn retry_button(id: &'static str, cx: &App) -> gpui::Stateful<gpui::Div> {
    secondary_button(
        id,
        Some((IconName::RotateCw, Color::Muted)),
        t!("github_projects.empty.retry"),
        true,
        cx,
    )
}

/// A property of the inspected item: its name, its value and, when it can change, a menu.
fn field_row(
    id: &'static str,
    label: SharedString,
    value: AnyElement,
    menu: Option<MenuBuilder>,
    cx: &App,
) -> AnyElement {
    let hover = cx.theme().colors().ghost_element_hover;
    let has_menu = menu.is_some();
    let row = h_flex()
        .id(id)
        .w_full()
        .h(px(32.))
        .px_2()
        .gap_2()
        .rounded(Radius::Md.rems())
        .when(has_menu, |this| this.hover(move |this| this.bg(hover)))
        .child(
            div()
                .w(px(96.))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().flex_1().min_w_0().child(value))
        .when(has_menu, |this| {
            this.child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        });
    match menu {
        Some(menu) => PopoverMenu::new(SharedString::from(format!("{id}-menu")))
            .full_width(true)
            .trigger(PopoverTriggerArea::new(row))
            .menu(move |window, cx| Some(menu(window, cx)))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The colors GitHub lets a project's options have, as status tones.
pub(crate) fn option_tone(option: &FieldOption) -> StatusTone {
    match option.color.as_str() {
        "BLUE" => StatusTone::Info,
        "GREEN" => StatusTone::Success,
        "YELLOW" | "ORANGE" => StatusTone::Warning,
        "RED" => StatusTone::Error,
        _ => StatusTone::Neutral,
    }
}

/// P0 and P1 stand out; the rest stay neutral.
pub(crate) fn priority_pill(project: &Project, item: &Item, cx: &App) -> Option<gpui::Div> {
    project.priority_of(item).map(|priority| {
        let urgent = priority.name.contains('0') || priority.name.contains('1');
        status_pill(
            priority.name.clone(),
            if urgent {
                StatusTone::Error
            } else {
                StatusTone::Neutral
            },
            cx,
        )
    })
}

fn pull_request_chip(pull_request: &api::LinkedPullRequest, cx: &App) -> gpui::Div {
    let (text, color) = match pull_request.state.as_str() {
        "MERGED" => (
            t!(
                "github_projects.item.pull_request_merged",
                number = pull_request.number
            ),
            Color::Accent,
        ),
        "CLOSED" => (
            t!(
                "github_projects.item.pull_request_closed",
                number = pull_request.number
            ),
            Color::Muted,
        ),
        _ => (format!("PR #{}", pull_request.number).into(), Color::Accent),
    };
    status_chip(Some(IconName::PullRequest), text, color, cx)
}

/// Status, iteration and linked pull request, as the focus card shows them.
fn item_chips(project: &Project, item: &Item, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_wrap()
        .gap_1p5()
        .children(
            project
                .status_of(item)
                .map(|status| status_pill(status.name.clone(), option_tone(status), cx)),
        )
        .children(project.iteration_of(item).map(|iteration| {
            status_chip(
                Some(IconName::History),
                iteration.title.clone(),
                Color::Default,
                cx,
            )
        }))
        .children(
            item.pull_requests
                .first()
                .map(|pull_request| pull_request_chip(pull_request, cx)),
        )
}

/// `#212`, or "draft" for a draft.
pub(crate) fn item_number(item: &Item) -> String {
    match item.number {
        Some(number) => format!("#{number}"),
        None => t!("github_projects.item.draft").to_string(),
    }
}

/// `#212 · bug, ui`.
fn item_meta(item: &Item) -> String {
    let mut meta = item_number(item);
    if !item.labels.is_empty() {
        let labels: Vec<&str> = item
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .collect();
        meta.push_str(" · ");
        meta.push_str(&labels.join(", "));
    }
    meta
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

pub(crate) fn ago(at: Instant) -> SharedString {
    let seconds = at.elapsed().as_secs();
    match seconds {
        0..=59 => t!("github_projects.ago.seconds", count = seconds),
        60..=3599 => t!("github_projects.ago.minutes", count = seconds / 60),
        _ => t!("github_projects.ago.hours", count = seconds / 3600),
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
    on_pick: Rc<dyn Fn(String, String, &mut App)>,
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
            let on_pick = on_pick.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for option in options {
                    let on_pick = on_pick.clone();
                    let item_id = item_id.clone();
                    let option_id = option.id.clone();
                    menu = menu.toggleable_entry(
                        option.name.clone(),
                        current.as_deref() == Some(option.id.as_str()),
                        ui::IconPosition::Start,
                        None,
                        move |_, cx| on_pick(item_id.clone(), option_id.clone(), cx),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.fill_branch_input(window, cx);
        let content: AnyElement = match self.state.clone() {
            None => EmptyState::new(
                IconName::Blocks,
                t!("github_projects.empty.not_github_title"),
            )
            .description(t!("github_projects.empty.not_github_detail"))
            .into_any_element(),
            Some(state_entity) => {
                let state = state_entity.read(cx);
                match state.availability(cx) {
                    Availability::NotConnected => EmptyState::new(
                        IconName::Disconnected,
                        t!("github_projects.empty.connect_title"),
                    )
                    .description(t!("github_projects.empty.connect_detail"))
                    .action(
                        secondary_button(
                            "tasks-open-repo",
                            Some((IconName::PullRequest, Color::Muted)),
                            t!("github_projects.empty.open_repo"),
                            true,
                            cx,
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(repo_hosting::ToggleFocus.boxed_clone(), cx)
                        }),
                    )
                    .into_any_element(),
                    Availability::Loading => {
                        EmptyState::new(IconName::LoadCircle, t!("github_projects.empty.loading"))
                            .spinning(true)
                            .into_any_element()
                    }
                    Availability::NoProjects => EmptyState::new(
                        IconName::Blocks,
                        t!("github_projects.empty.no_projects_title"),
                    )
                    .description(t!(
                        "github_projects.empty.no_projects_detail",
                        repository = state.repository().full_name()
                    ))
                    .into_any_element(),
                    Availability::MissingScope => EmptyState::new(
                        IconName::Lock,
                        t!("github_projects.empty.missing_scope_title"),
                    )
                    .description(t!("github_projects.empty.missing_scope_detail_retry"))
                    .action(retry_button("tasks-retry", cx).on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(state) = &this.state {
                                state.update(cx, |state, cx| state.retry_with_fresh_token(cx));
                            }
                        },
                    )))
                    .into_any_element(),
                    Availability::Failed(error) => {
                        EmptyState::new(IconName::Warning, t!("github_projects.empty.failed_title"))
                            .tone(EmptyStateTone::Error)
                            .detail(error)
                            .action(retry_button("tasks-retry", cx).on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(state) = &this.state {
                                        state.update(cx, |state, cx| state.sync(cx));
                                    }
                                },
                            )))
                            .into_any_element()
                    }
                    Availability::Ready => self.render_ready(state, cx),
                }
            }
        };
        v_flex()
            .key_context("TasksPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::start_inspected))
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
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::GITHUB_PROJECTS, &self.project, cx)
    }

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

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some(match i18n::current_language() {
            i18n::Language::English => "Tasks",
            i18n::Language::PortugueseBrazil => "Tarefas",
        })
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}
