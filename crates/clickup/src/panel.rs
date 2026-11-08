use crate::{
    Account, AssignedTasks, ClickUpStore, Connection, TOKEN_SETTINGS_URL, ToggleFocus, api,
};
use chrono::{DateTime, Datelike as _, Local, TimeZone as _};
use gpui::{
    Action as _, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Pixels, Rgba,
    Subscription, Task, px,
};
use gpui::{ClipboardItem, WeakEntity};
use i18n::t;
use project::Project;
use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash as _, Hasher as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::{
    CommonAnimationExt, ContextMenu, ContextMenuEntry, EmptyState, HeaderBar, KeyBinding,
    PopoverMenu, PopoverTriggerArea, RowStatus, StatusRow, StatusTone, Tooltip, capsule_button,
    card, error_callout, grouped_block, initial_tile, navigator_footer, prelude::*, primary_button,
    secondary_button, section_header, section_label, segment_option, segment_option_with_count,
    segmented_control, select_field, status_pill,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

/// Long enough that pasting a token checks it once instead of once per keystroke of typing.
const CHECK_DEBOUNCE: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskTab {
    Mine,
    Mentions,
    Done,
}

/// The tasks sharing a status name, however many lists that status spans.
struct StatusGroup<'a> {
    name: String,
    color: Option<Hsla>,
    tasks: Vec<&'a api::Task>,
}

/// What the panel knows about the token in the field, before it is saved.
enum TokenCheck {
    Idle,
    Checking,
    Valid(Account),
    /// The field shows why, through `InputField`'s own error text.
    Invalid,
}

pub struct ClickUpPanel {
    focus_handle: FocusHandle,
    store: Entity<ClickUpStore>,
    token_input: Entity<InputField>,
    token_check: TokenCheck,
    check_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// Set from the account menu: shows the token field again while the current token keeps
    /// working until a new one replaces it.
    replacing_token: bool,
    filter_input: Entity<InputField>,
    selected_tab: TaskTab,
    selected_list: Option<String>,
    /// Status names whose group is folded, lowercased.
    collapsed_groups: HashSet<String>,
    /// The task shown in place of the list, when one was opened.
    open_task: Option<OpenTask>,
    /// Images attached to comments, keyed by URL, as downloaded to the local cache.
    comment_images: HashMap<String, CommentImage>,
    /// Pull requests found in each task's description and comments, kept after its detail
    /// closes so the focus card can show them.
    pull_requests: HashMap<String, Vec<String>>,
    /// A timer toggled from the list failed; with a task open, its own error shows it instead.
    timer_error: Option<SharedString>,
    /// Only for checking whether the ClickUp toolkit is on, which can't read the workspace while
    /// the dock asks from inside a workspace update.
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    _timer_tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// A task opened from the list. The row it came from shows right away while the rest loads.
struct OpenTask {
    summary: api::Task,
    detail: Option<api::TaskDetail>,
    statuses: Vec<api::TaskStatus>,
    comments: Vec<api::Comment>,
    error: Option<SharedString>,
    comment_input: Entity<InputField>,
    posting_comment: bool,
    description_expanded: bool,
    load_task: Option<Task<()>>,
}

enum CommentImage {
    Loading,
    Ready(Arc<Path>),
    Failed,
}

/// Longer descriptions are cut here until "Mostrar mais" is clicked.
const DESCRIPTION_PREVIEW_CHARS: usize = 280;

impl ClickUpPanel {
    pub(crate) fn new(
        store: Entity<ClickUpStore>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let token_input = cx.new(|cx| {
            InputField::new(window, cx, "pk_…")
                .start_icon(IconName::Lock)
                .masked(true)
        });

        let this = cx.weak_entity();
        let token_editor = token_input.read(cx).editor().clone();
        let edit_subscription = token_editor.subscribe(
            Box::new(move |event, window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    this.update(cx, |this, cx| this.token_edited(window, cx))
                        .log_err();
                }
            }),
            window,
            cx,
        );
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());

        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("clickup.tasks.filter_placeholder"))
                .start_icon(IconName::MagnifyingGlass)
        });
        let filter_editor = filter_input.read(cx).editor().clone();
        let this = cx.weak_entity();
        let filter_subscription = filter_editor.subscribe(
            Box::new(move |event, _window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    this.update(cx, |_, cx| cx.notify()).log_err();
                }
            }),
            window,
            cx,
        );

        Self {
            focus_handle: cx.focus_handle(),
            store,
            token_input,
            token_check: TokenCheck::Idle,
            check_task: None,
            save_task: None,
            replacing_token: false,
            filter_input,
            selected_tab: TaskTab::Mine,
            selected_list: None,
            collapsed_groups: HashSet::default(),
            open_task: None,
            comment_images: HashMap::default(),
            pull_requests: HashMap::default(),
            timer_error: None,
            project,
            workspace,
            // Keeps the running timer's clock moving; nothing redraws while none is running.
            _timer_tick: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let Ok(()) = this.update(cx, |this, cx| {
                        if this.store.read(cx).running_timer().is_some() {
                            cx.notify();
                        }
                    }) else {
                        return;
                    };
                }
            }),
            _subscriptions: vec![edit_subscription, store_subscription, filter_subscription],
        }
    }

    fn typed_token(&self, cx: &App) -> String {
        self.token_input.read(cx).text(cx).trim().to_string()
    }

    fn set_field_error(&self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.token_input
            .update(cx, |input, cx| input.set_error(error, cx));
    }

    fn token_edited(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.token_check = TokenCheck::Idle;
        self.check_task = None;
        self.set_field_error(None, cx);
        cx.notify();

        let token = self.typed_token(cx);
        // Anything that does not look like a personal token would only earn a 401; the hint
        // under the field already says what to paste.
        if !token.starts_with("pk_") {
            return;
        }

        self.check_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHECK_DEBOUNCE).await;
            let Some(validation) = this
                .update(cx, |this, cx| {
                    this.token_check = TokenCheck::Checking;
                    cx.notify();
                    this.store.read(cx).validate(token, cx)
                })
                .log_err()
            else {
                return;
            };
            let result = validation.await;
            this.update(cx, |this, cx| this.finish_check(result, cx))
                .log_err();
        }));
    }

    fn finish_check(&mut self, result: anyhow::Result<Account>, cx: &mut Context<Self>) {
        self.token_check = match result {
            Ok(account) => TokenCheck::Valid(account),
            Err(error) => {
                let message: SharedString = if crate::is_unauthorized(&error) {
                    t!("clickup.connect.token_rejected")
                } else {
                    t!("clickup.connect.unreachable", error = error)
                };
                self.set_field_error(Some(message), cx);
                TokenCheck::Invalid
            }
        };
        cx.notify();
    }

    fn connect(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.save_task.is_some() {
            return;
        }
        let token = self.typed_token(cx);
        if token.is_empty() {
            return;
        }

        let already_checked = match &self.token_check {
            TokenCheck::Valid(account) if *account.token == *token => Some(account.clone()),
            _ => None,
        };
        let validation = match already_checked {
            Some(account) => Task::ready(Ok(account)),
            None => {
                self.check_task = None;
                self.token_check = TokenCheck::Checking;
                cx.notify();
                self.store.read(cx).validate(token, cx)
            }
        };

        self.save_task = Some(cx.spawn(async move |this, cx| {
            let account = match validation.await {
                Ok(account) => account,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.save_task = None;
                        this.finish_check(Err(error), cx);
                    })
                    .log_err();
                    return;
                }
            };
            let saved = this
                .update(cx, |this, cx| {
                    this.store.update(cx, |store, cx| store.save(account, cx))
                })
                .log_err();
            let result = match saved {
                Some(saved) => saved.await,
                None => return,
            };
            this.update_in(cx, |this, window, cx| {
                this.save_task = None;
                match result {
                    Ok(()) => {
                        this.replacing_token = false;
                        this.token_check = TokenCheck::Idle;
                        this.token_input
                            .update(cx, |input, cx| input.clear(window, cx));
                    }
                    Err(error) => {
                        log::error!("ClickUp: falha ao salvar o token no keychain: {error:#}");
                        let message = t!("clickup.connect.keychain_save_failed");
                        this.set_field_error(Some(message), cx);
                        this.token_check = TokenCheck::Invalid;
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn start_replacing_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replacing_token = true;
        self.token_check = TokenCheck::Idle;
        self.set_field_error(None, cx);
        self.token_input
            .update(cx, |input, cx| input.clear(window, cx));
        window.focus(&self.token_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn cancel_replacing_token(&mut self, cx: &mut Context<Self>) {
        self.replacing_token = false;
        self.check_task = None;
        self.token_check = TokenCheck::Idle;
        cx.notify();
    }

    fn render_step(&self, number: usize, title: SharedString, cx: &App) -> impl IntoElement {
        h_flex()
            .gap(DynamicSpacing::Base10.px(cx))
            .child(
                h_flex()
                    .flex_none()
                    .size(DynamicSpacing::Base20.px(cx))
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().colors().ghost_element_hover)
                    .child(
                        Label::new(number.to_string())
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Accent),
                    ),
            )
            .child(Label::new(title).size(LabelSize::Small))
    }

    fn render_check_hint(&self, cx: &App) -> Option<AnyElement> {
        let (icon, text, color): (Option<IconName>, SharedString, Color) = match &self.token_check {
            TokenCheck::Idle => (None, t!("clickup.connect.token_hint"), Color::Muted),
            TokenCheck::Checking => (
                Some(IconName::LoadCircle),
                t!("clickup.connect.checking"),
                Color::Muted,
            ),
            TokenCheck::Valid(account) => (
                Some(IconName::Check),
                t!(
                    "clickup.connect.connected_as",
                    name = account.user.display_name(),
                    workspaces = workspace_count(account.workspaces.len())
                ),
                Color::Success,
            ),
            TokenCheck::Invalid => return None,
        };
        let is_checking = matches!(self.token_check, TokenCheck::Checking);

        Some(
            h_flex()
                .gap(DynamicSpacing::Base06.px(cx))
                .when_some(icon, |this, icon| {
                    this.child(spinning_if(
                        Icon::new(icon).size(IconSize::XSmall).color(color),
                        is_checking,
                    ))
                })
                .child(Label::new(text).size(LabelSize::XSmall).color(color))
                .into_any_element(),
        )
    }

    fn render_connect(&self, show_rejected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let is_busy = self.save_task.is_some() || matches!(self.token_check, TokenCheck::Checking);
        let can_connect = !is_busy && !self.token_input.read(cx).is_empty(cx);
        let token_focus = self.token_input.focus_handle(cx);

        let steps = card(cx)
            .gap(DynamicSpacing::Base12.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(1, t!("clickup.connect.step_generate"), cx))
                    .child(
                        h_flex().pl(DynamicSpacing::Base32.px(cx)).child(
                            secondary_button(
                                "clickup-open-token-settings",
                                Some((IconName::ArrowUpRight, Color::Muted)),
                                t!("clickup.connect.open_settings"),
                                true,
                                cx,
                            )
                            .on_click(|_, _, cx| cx.open_url(TOKEN_SETTINGS_URL)),
                        ),
                    ),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(2, t!("clickup.connect.step_paste"), cx))
                    .child(self.token_input.clone())
                    .children(self.render_check_hint(cx)),
            )
            .child(
                primary_button(
                    "clickup-connect",
                    None,
                    t!("clickup.connect.connect"),
                    Some(KeyBinding::for_action_in(&menu::Confirm, &token_focus, cx)),
                    cx,
                )
                .w_full()
                .justify_center()
                .when(!can_connect, |this| this.opacity(0.5).cursor_default())
                .when(can_connect, |this| {
                    this.on_click(
                        cx.listener(|this, _, window, cx| this.connect(&menu::Confirm, window, cx)),
                    )
                }),
            )
            .when(self.replacing_token, |this| {
                this.child(
                    h_flex().justify_center().child(
                        Button::new(
                            "clickup-cancel-replace",
                            t!("clickup.connect.keep_current_token"),
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_replacing_token(cx))),
                    ),
                )
            });

        let after_connecting = v_flex()
            .w_full()
            .gap(DynamicSpacing::Base08.px(cx))
            .child(section_label(t!("clickup.connect.after_title")))
            .child(
                grouped_block(cx).children(
                    [
                        (IconName::GitBranch, t!("clickup.connect.after_branch")),
                        (IconName::Clock, t!("clickup.connect.after_timer")),
                        (IconName::PullRequest, t!("clickup.connect.after_pr")),
                    ]
                    .into_iter()
                    .map(|(icon, text)| {
                        h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .px(DynamicSpacing::Base08.px(cx))
                            .py(DynamicSpacing::Base06.px(cx))
                            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                            .child(Label::new(text).size(LabelSize::Small))
                    }),
                ),
            );

        v_flex()
            .key_context("ClickUpConnect")
            .on_action(cx.listener(Self::connect))
            .size_full()
            .child(
                v_flex()
                    .id("clickup-connect-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .items_center()
                    .gap(DynamicSpacing::Base16.px(cx))
                    .px(DynamicSpacing::Base24.px(cx))
                    .py(DynamicSpacing::Base16.px(cx))
                    .when(show_rejected, |this| {
                        this.child(error_callout(
                            t!("clickup.connect.saved_token_rejected"),
                            t!("clickup.connect.saved_token_rejected_detail"),
                            cx,
                        ))
                    })
                    .child(div().flex_none().w_full().child(EmptyState::new(
                        IconName::ListTodo,
                        t!("clickup.connect.title"),
                    )))
                    .child(steps)
                    .child(after_connecting),
            )
            .child(self.render_privacy_footer(cx))
    }

    fn render_privacy_footer(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .items_start()
            .gap(DynamicSpacing::Base08.px(cx))
            .px(DynamicSpacing::Base16.px(cx))
            .py(DynamicSpacing::Base12.px(cx))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Icon::new(IconName::Lock)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(t!("clickup.connect.privacy"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn render_account_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let store = self.store.clone();
        PopoverMenu::new("clickup-account-menu")
            .trigger_with_tooltip(
                IconButton::new("clickup-account-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text(t!("clickup.account.tooltip")),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let store = store.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(
                        ContextMenuEntry::new(t!("clickup.account.replace_token"))
                            .icon(IconName::Pencil)
                            .icon_color(Color::Muted)
                            .handler({
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.start_replacing_token(window, cx)
                                        })
                                        .log_err();
                                }
                            }),
                    )
                    .item(
                        ContextMenuEntry::new(t!("clickup.account.manage_token"))
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .handler(|_, cx| cx.open_url(TOKEN_SETTINGS_URL)),
                    )
                    .separator()
                    .item(
                        ContextMenuEntry::new(t!("clickup.account.disconnect"))
                            .icon(IconName::Exit)
                            .icon_color(Color::Error)
                            .handler(move |_, cx| {
                                store
                                    .update(cx, |store, cx| store.disconnect(cx))
                                    .detach_and_log_err(cx);
                            }),
                    )
                }))
            })
    }

    fn render_workspace_picker(&self, account: &Account, cx: &Context<Self>) -> AnyElement {
        let selected = self.store.read(cx).selected_workspace().cloned();
        let name: SharedString = selected
            .as_ref()
            .map(|workspace| workspace.name.clone().into())
            .unwrap_or_else(|| t!("clickup.account.no_workspace"));
        let has_choices = account.workspaces.len() > 1;
        let trigger = h_flex()
            .id("clickup-workspace-picker")
            .min_w_0()
            .gap(DynamicSpacing::Base08.px(cx))
            .px(DynamicSpacing::Base06.px(cx))
            .py(DynamicSpacing::Base04.px(cx))
            .rounded(Radius::Md.rems())
            .child(initial_tile(&name, cx))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .child(
                        Label::new(t!("clickup.tasks.workspace_scope"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when(has_choices, |this| {
                this.hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
            });

        if !has_choices {
            return trigger.into_any_element();
        }

        let store = self.store.clone();
        let workspaces = account.workspaces.clone();
        let selected_id = selected.map(|workspace| workspace.id);
        PopoverMenu::new("clickup-workspace-menu")
            .trigger(PopoverTriggerArea::new(trigger))
            .menu(move |window, cx| {
                let store = store.clone();
                let workspaces = workspaces.clone();
                let selected_id = selected_id.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for workspace in workspaces {
                        let store = store.clone();
                        let is_selected = selected_id.as_deref() == Some(workspace.id.as_str());
                        menu = menu.toggleable_entry(
                            workspace.name.clone(),
                            is_selected,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                let workspace_id = workspace.id.clone();
                                store.update(cx, |store, cx| {
                                    store.select_workspace(workspace_id, cx)
                                });
                            },
                        );
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn render_context_bar(&self, account: &Account, cx: &Context<Self>) -> impl IntoElement {
        let is_syncing = self.store.read(cx).tasks().is_syncing;
        let store = self.store.clone();
        HeaderBar::new("clickup-header")
            .child(self.render_workspace_picker(account, cx))
            .end_child(
                IconButton::new("clickup-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(is_syncing)
                    .tooltip(Tooltip::text(t!("clickup.tasks.sync_now")))
                    .on_click(move |_, _, cx| store.update(cx, |store, cx| store.sync(cx))),
            )
            .end_child(self.render_account_menu(cx))
    }

    fn focus_task<'a>(&self, tasks: &'a AssignedTasks, cx: &App) -> Option<(&'a api::Task, bool)> {
        if let Some(task) = tasks
            .open
            .iter()
            .find(|task| self.branch_for_task(task, cx).is_some())
        {
            return Some((task, true));
        }
        let timed_id = &self.store.read(cx).running_timer()?.task.as_ref()?.id;
        tasks
            .open
            .iter()
            .find(|task| task.id == *timed_id)
            .map(|task| (task, false))
    }

    fn render_timer_readout(
        &self,
        id: &'static str,
        task_id: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let started_at = timer_started_at(self.store.read(cx), task_id);
        let now_millis = Local::now().timestamp_millis();
        let (clock, caption) = match started_at {
            Some(started_at) => {
                let started = Local
                    .timestamp_millis_opt(started_at)
                    .single()
                    .map(|started| {
                        t!("clickup.task.timer_since", time = started.format("%H:%M")).to_string()
                    })
                    .unwrap_or_default();
                (format_clock(now_millis - started_at), started)
            }
            None => (
                "00:00:00".to_string(),
                t!("clickup.task.timer_stopped").to_string(),
            ),
        };
        let is_running = started_at.is_some();
        let toggled_id = task_id.to_string();
        h_flex()
            .w_full()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base10.px(cx))
            .py(DynamicSpacing::Base08.px(cx))
            .rounded(Radius::Md.rems())
            .bg(cx.theme().colors().elevated_surface_background)
            .child(
                if is_running {
                    StatusTone::Error
                } else {
                    StatusTone::Neutral
                }
                .dot(cx),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(clock)
                            .size(LabelSize::Large)
                            .weight(FontWeight::BOLD)
                            .buffer_font(cx)
                            .color(if is_running {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(caption)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                secondary_button(
                    id,
                    Some(if is_running {
                        (IconName::Stop, Color::Error)
                    } else {
                        (IconName::PlayFilled, Color::Default)
                    }),
                    if is_running {
                        t!("clickup.task.timer_stop")
                    } else {
                        t!("clickup.task.timer_start")
                    },
                    true,
                    cx,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_timer(toggled_id.clone(), cx)),
                ),
            )
    }

    fn render_focus_card(
        &self,
        task: &api::Task,
        from_branch: bool,
        now: DateTime<Local>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = self
            .pull_requests
            .get(&task.id)
            .and_then(|links| links.first())
            .map(|link| describe_pull_request(link).0);
        let is_due_today = task
            .due_date
            .and_then(|due| due_label(due, now))
            .is_some_and(|(_, color)| color == Color::Warning);
        let status_task = task.clone();
        let comment_task = task.clone();
        let url = task.url.clone();

        card(cx)
            .border_color(cx.theme().colors().border_focused)
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(section_label(t!("clickup.focus.title")))
                    .when(from_branch, |this| {
                        this.child(
                            Label::new(t!("clickup.focus.from_branch"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(task.display_id().to_string())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(task.name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(DynamicSpacing::Base04.px(cx))
                    .child(status_pill(
                        capitalize(&task.status.status),
                        StatusTone::Info,
                        cx,
                    ))
                    .when_some(pull_request, |this, pull_request| {
                        this.child(status_pill(pull_request, StatusTone::Neutral, cx))
                    })
                    .when(is_due_today, |this| {
                        this.child(status_pill(
                            t!("clickup.focus.due_today"),
                            StatusTone::Warning,
                            cx,
                        ))
                    }),
            )
            .child(self.render_timer_readout("clickup-focus-timer", &task.id, cx))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        capsule_button(
                            "clickup-focus-status",
                            None,
                            t!("clickup.task.field_status"),
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_task(status_task.clone(), window, cx)
                            },
                        )),
                    )
                    .child(
                        capsule_button(
                            "clickup-focus-comment",
                            None,
                            t!("clickup.task.comment"),
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_task_to_comment(comment_task.clone(), window, cx)
                            },
                        )),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("clickup-focus-open", t!("clickup.focus.open"))
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .color(Color::Accent)
                            .end_icon(
                                Icon::new(IconName::ArrowUpRight)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
    }

    fn render_task_segment(
        &self,
        tab: TaskTab,
        tasks: &AssignedTasks,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.selected_tab == tab;
        let option = match tab {
            TaskTab::Mine => segment_option_with_count(
                "clickup-tab-mine",
                t!("clickup.tasks.tab_mine"),
                tasks.open.len().to_string(),
                selected,
                true,
                cx,
            ),
            // The ClickUp API has no query for mentions, so the segment stays off.
            TaskTab::Mentions => {
                return segment_option(
                    "clickup-tab-mentions",
                    t!("clickup.tasks.tab_mentions"),
                    selected,
                    false,
                    cx,
                )
                .into_any_element();
            }
            TaskTab::Done => segment_option(
                "clickup-tab-done",
                t!("clickup.tasks.tab_done"),
                selected,
                true,
                cx,
            ),
        };
        option
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_tab = tab;
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_list_select(&self, tasks: &AssignedTasks, cx: &Context<Self>) -> impl IntoElement {
        let mut lists: Vec<String> = tasks
            .open
            .iter()
            .chain(&tasks.closed)
            .map(|task| task.list.name.clone())
            .collect();
        lists.sort();
        lists.dedup();
        let label: SharedString = self
            .selected_list
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| t!("clickup.tasks.all_lists"));
        let selected = self.selected_list.clone();
        let panel = cx.weak_entity();
        div().flex_none().w(px(136.)).child(
            PopoverMenu::new("clickup-list-filter")
                .trigger(PopoverTriggerArea::new(select_field(
                    "clickup-list-filter-trigger",
                    label,
                    cx,
                )))
                .anchor(gpui::Anchor::TopRight)
                .menu(move |window, cx| {
                    let lists = lists.clone();
                    let selected = selected.clone();
                    let panel = panel.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for choice in std::iter::once(None).chain(lists.into_iter().map(Some)) {
                            let label: SharedString = choice
                                .clone()
                                .map(SharedString::from)
                                .unwrap_or_else(|| t!("clickup.tasks.all_lists"));
                            let is_selected = choice == selected;
                            let panel = panel.clone();
                            menu = menu.toggleable_entry(
                                label,
                                is_selected,
                                IconPosition::End,
                                None,
                                move |_, cx| {
                                    let choice = choice.clone();
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.selected_list = choice;
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
    }

    fn render_task_row(
        &self,
        task: &api::Task,
        focus_id: Option<&str>,
        now: DateTime<Local>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let opened = task.clone();
        let is_closed = task.status.is_closed();
        let end = match timer_started_at(self.store.read(cx), &task.id) {
            Some(started_at) => Some(
                h_flex()
                    .flex_none()
                    .gap(DynamicSpacing::Base04.px(cx))
                    .child(StatusTone::Error.dot(cx))
                    .child(
                        Label::new(format_row_clock(now.timestamp_millis() - started_at))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx),
                    )
                    .into_any_element(),
            ),
            None if !is_closed => {
                task.due_date
                    .and_then(|due| due_label(due, now))
                    .map(|(label, color)| {
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(color)
                            .into_any_element()
                    })
            }
            None => None,
        };

        StatusRow::new(
            SharedString::from(format!("clickup-task-{}", task.id)),
            if is_closed {
                RowStatus::Stopped
            } else {
                RowStatus::Running
            },
            task.name.clone(),
        )
        .height(px(42.))
        .detail(format!("{} · {}", task.display_id(), task.list.name))
        .selected(focus_id == Some(task.id.as_str()))
        .when_some(parse_color(&task.status.color), |row, color| {
            row.dot_color(color)
        })
        .when_some(end, |row, end| row.end_slot(end))
        .on_click(
            cx.listener(move |this, _, window, cx| this.open_task(opened.clone(), window, cx)),
        )
    }

    fn render_group_header(&self, group: &StatusGroup, cx: &Context<Self>) -> impl IntoElement {
        let key = group.name.to_lowercase();
        let is_collapsed = self.collapsed_groups.contains(&key);
        h_flex()
            .id(SharedString::from(format!("clickup-group-{key}")))
            .gap(DynamicSpacing::Base06.px(cx))
            .px(DynamicSpacing::Base06.px(cx))
            .py(DynamicSpacing::Base04.px(cx))
            .mt(DynamicSpacing::Base04.px(cx))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&key) {
                    this.collapsed_groups.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(status_dot(group.color, cx))
            .child(section_label(group.name.to_uppercase()))
            .child(
                Label::new(group.tasks.len().to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
    }

    fn render_task_list(
        &self,
        tasks: &AssignedTasks,
        focus_id: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let source: &[api::Task] = match self.selected_tab {
            TaskTab::Mine => &tasks.open,
            TaskTab::Mentions => &[],
            TaskTab::Done => &tasks.closed,
        };
        let visible: Vec<&api::Task> = source
            .iter()
            .filter(|task| task_matches(task, &filter))
            .filter(|task| {
                self.selected_list
                    .as_ref()
                    .is_none_or(|list| task.list.name == *list)
            })
            .collect();

        if visible.is_empty() {
            let is_loading = tasks.synced_at.is_none();
            let (icon, title) = if is_loading {
                (IconName::LoadCircle, t!("clickup.tasks.loading"))
            } else if !filter.is_empty() || self.selected_list.is_some() {
                (IconName::MagnifyingGlass, t!("clickup.tasks.empty_filter"))
            } else if self.selected_tab == TaskTab::Done {
                (IconName::Check, t!("clickup.tasks.empty_done"))
            } else {
                (IconName::Check, t!("clickup.tasks.empty_open"))
            };
            return EmptyState::new(icon, title)
                .spinning(is_loading)
                .into_any_element();
        }

        let now = Local::now();
        let mut list = v_flex()
            .id("clickup-task-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base08.px(cx))
            .pb(DynamicSpacing::Base08.px(cx));

        match self.selected_tab {
            TaskTab::Mine | TaskTab::Mentions => {
                for group in group_by_status(&visible) {
                    let is_collapsed = self.collapsed_groups.contains(&group.name.to_lowercase());
                    list = list.child(self.render_group_header(&group, cx));
                    if !is_collapsed {
                        for task in &group.tasks {
                            list = list.child(self.render_task_row(task, focus_id, now, cx));
                        }
                    }
                }
            }
            TaskTab::Done => {
                for task in visible {
                    list = list.child(self.render_task_row(task, focus_id, now, cx));
                }
            }
        }
        list.into_any_element()
    }

    /// The board opens on the list picked in the dock, when there is one.
    fn open_sprint_board(&self, window: &mut Window, cx: &mut Context<Self>) {
        let tasks = self.store.read(cx).tasks();
        let list_id = self.selected_list.as_ref().and_then(|list| {
            tasks
                .open
                .iter()
                .chain(&tasks.closed)
                .find(|task| task.list.name == *list)
                .map(|task| task.list.id.clone())
        });
        let workspace = self.workspace.clone();
        // Adding the tab can touch the dock that holds this panel, so it waits for the
        // panel's update to end.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    crate::sprint_board::open(workspace, list_id, window, cx)
                })
                .log_err();
        });
    }

    fn render_footer(&self, tasks: &AssignedTasks, cx: &Context<Self>) -> impl IntoElement {
        let board = capsule_button(
            "clickup-sprint-board",
            Some(IconName::Blocks),
            t!("clickup.tasks.sprint_board"),
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| this.open_sprint_board(window, cx)))
        .into_any_element();
        let tone = if tasks.error.is_some() {
            StatusTone::Warning
        } else {
            StatusTone::Success
        };
        navigator_footer(Some(tone), sync_status_label(tasks), Some(board), cx)
            .h(px(40.))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
    }

    fn render_connected(&self, account: &Account, cx: &Context<Self>) -> AnyElement {
        if let Some(open_task) = &self.open_task {
            return self.render_task_detail(open_task, cx).into_any_element();
        }
        let tasks = self.store.read(cx).tasks();
        let now = Local::now();
        let focus = self.focus_task(tasks, cx);
        let focus_id = focus.map(|(task, _)| task.id.as_str());

        v_flex()
            .size_full()
            .child(self.render_context_bar(account, cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .px(DynamicSpacing::Base08.px(cx))
                    .pt(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .when_some(focus, |this, (task, from_branch)| {
                        this.child(self.render_focus_card(task, from_branch, now, cx))
                    })
                    .child(segmented_control(cx).children([
                        self.render_task_segment(TaskTab::Mine, tasks, cx),
                        self.render_task_segment(TaskTab::Mentions, tasks, cx),
                        self.render_task_segment(TaskTab::Done, tasks, cx),
                    ]))
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(div().flex_1().min_w_0().child(self.filter_input.clone()))
                            .child(self.render_list_select(tasks, cx)),
                    ),
            )
            .when_some(tasks.error.clone(), |this, error| {
                this.child(render_error(t!("clickup.tasks.sync_failed"), error, cx))
            })
            .when_some(self.timer_error.clone(), |this, error| {
                this.child(render_error(t!("clickup.common.request_failed"), error, cx))
            })
            .child(self.render_task_list(tasks, focus_id, cx))
            .child(self.render_footer(tasks, cx))
            .into_any_element()
    }
}

impl ClickUpPanel {
    pub(crate) fn open_task_id(&self) -> Option<&str> {
        self.open_task
            .as_ref()
            .map(|open_task| open_task.summary.id.as_str())
    }

    pub(crate) fn open_task(
        &mut self,
        task: api::Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_input = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &t!("clickup.task.comment_placeholder", id = task.display_id()),
            )
        });
        self.open_task = Some(OpenTask {
            summary: task,
            detail: None,
            statuses: Vec::new(),
            comments: Vec::new(),
            error: None,
            comment_input,
            posting_comment: false,
            description_expanded: false,
            load_task: None,
        });
        self.reload_open_task(cx);
        cx.notify();
    }

    fn open_task_to_comment(
        &mut self,
        task: api::Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_task(task, window, cx);
        if let Some(open_task) = &self.open_task {
            window.focus(&open_task.comment_input.focus_handle(cx), cx);
        }
    }

    fn close_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_task = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn reload_open_task(&mut self, cx: &mut Context<Self>) {
        let Some(open_task) = &mut self.open_task else {
            return;
        };
        let Some((http_client, token, _)) = self.store.read(cx).request_context() else {
            return;
        };
        let task_id = open_task.summary.id.clone();
        let list_id = open_task.summary.list.id.clone();
        let load = cx.background_spawn(async move {
            let detail = api::get_task(&http_client, &token, &task_id);
            let statuses = api::get_list_statuses(&http_client, &token, &list_id);
            let comments = api::get_comments(&http_client, &token, &task_id);
            futures::join!(detail, statuses, comments)
        });
        open_task.load_task = Some(cx.spawn(async move |this, cx| {
            let (detail, statuses, comments) = load.await;
            this.update(cx, |this, cx| {
                let Some(open_task) = &mut this.open_task else {
                    return;
                };
                let mut errors = Vec::new();
                match detail {
                    Ok(detail) => {
                        open_task.summary = detail.task.clone();
                        open_task.detail = Some(detail);
                    }
                    Err(error) => errors.push(format!("{error:#}")),
                }
                match statuses {
                    Ok(statuses) => open_task.statuses = statuses,
                    Err(error) => log::warn!("ClickUp: falha ao ler os status da lista: {error:#}"),
                }
                match comments {
                    Ok(comments) => open_task.comments = comments,
                    Err(error) => errors.push(format!("{error:#}")),
                }
                open_task.error = (!errors.is_empty()).then(|| errors.join("\n").into());
                let links = open_task_pull_requests(open_task);
                let task_id = open_task.summary.id.clone();
                this.pull_requests.insert(task_id, links);
                this.fetch_comment_images(cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Downloads the open task's comment images into the local cache, where they show from and
    /// the image viewer opens them.
    fn fetch_comment_images(&mut self, cx: &mut Context<Self>) {
        let Some(open_task) = &self.open_task else {
            return;
        };
        let Some((http_client, token, _)) = self.store.read(cx).request_context() else {
            return;
        };
        let urls: Vec<(String, Option<String>)> = open_task
            .comments
            .iter()
            .flat_map(|comment| comment.attachments())
            .filter(|attachment| attachment.is_image())
            .map(|attachment| {
                let extension = attachment.extension().map(str::to_string);
                (attachment.url, extension)
            })
            .filter(|(url, _)| !self.comment_images.contains_key(url))
            .collect();
        for (url, extension) in urls {
            self.comment_images
                .insert(url.clone(), CommentImage::Loading);
            let path = comment_image_path(&url, extension.as_deref());
            let http_client = http_client.clone();
            let token = token.clone();
            let download = cx.background_spawn({
                let url = url.clone();
                async move {
                    if !path.exists() {
                        let bytes = api::download_attachment(&http_client, &token, &url).await?;
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&path, bytes)?;
                    }
                    anyhow::Ok(Arc::<Path>::from(path.as_path()))
                }
            });
            cx.spawn(async move |this, cx| {
                let result = download.await;
                this.update(cx, |this, cx| {
                    let state = match result {
                        Ok(path) => CommentImage::Ready(path),
                        Err(error) => {
                            log::warn!("ClickUp: falha ao baixar a imagem {url}: {error:#}");
                            CommentImage::Failed
                        }
                    };
                    this.comment_images.insert(url, state);
                    cx.notify();
                })
            })
            .detach_and_log_err(cx);
        }
    }

    fn open_comment_image(&mut self, path: Arc<Path>, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path.to_path_buf(), OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    /// An image shown in place, which opens in the image viewer when clicked, or a link to any
    /// other file.
    fn render_comment_attachment(
        &self,
        id: SharedString,
        attachment: &api::CommentAttachment,
        cx: &Context<Self>,
    ) -> AnyElement {
        let url = attachment.url.clone();
        let name = attachment.display_name().to_string();
        let image = attachment
            .is_image()
            .then(|| self.comment_images.get(&url))
            .flatten();
        match image {
            Some(CommentImage::Ready(path)) => {
                let opened = path.clone();
                div()
                    .id(id)
                    .mt(DynamicSpacing::Base04.px(cx))
                    .max_w_full()
                    .rounded_md()
                    .overflow_hidden()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .cursor_pointer()
                    .tooltip(Tooltip::text(t!(
                        "clickup.task.open_attachment",
                        name = name
                    )))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_comment_image(opened.clone(), window, cx)
                    }))
                    .child(
                        gpui::img(path.clone())
                            .max_w_full()
                            .max_h(px(240.))
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .into_any_element()
            }
            Some(CommentImage::Loading) => {
                Label::new(t!("clickup.task.loading_attachment", name = name))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .mt(DynamicSpacing::Base04.px(cx))
                    .into_any_element()
            }
            Some(CommentImage::Failed) | None => render_file_link(
                id,
                if attachment.is_image() {
                    t!("clickup.task.open_in_browser", name = name).to_string()
                } else {
                    name
                },
                url,
                cx,
            ),
        }
    }

    fn set_open_task_status(&mut self, status: String, cx: &mut Context<Self>) {
        let Some(open_task) = &mut self.open_task else {
            return;
        };
        let Some((http_client, token, _)) = self.store.read(cx).request_context() else {
            return;
        };
        let task_id = open_task.summary.id.clone();
        // Shown at once; the reload that follows brings ClickUp's own version back.
        if let Some(new_status) = open_task
            .statuses
            .iter()
            .find(|candidate| candidate.status == status)
        {
            open_task.summary.status = new_status.clone();
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    api::set_task_status(&http_client, &token, &task_id, &status).await
                })
                .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result
                    && let Some(open_task) = &mut this.open_task
                {
                    open_task.error = Some(t!(
                        "clickup.task.status_failed",
                        error = format!("{error:#}")
                    ));
                }
                this.reload_open_task(cx);
                this.store.update(cx, |store, cx| store.sync(cx));
            })
        })
        .detach_and_log_err(cx);
    }

    fn post_open_task_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open_task) = &mut self.open_task else {
            return;
        };
        let text = open_task.comment_input.read(cx).text(cx).trim().to_string();
        if text.is_empty() || open_task.posting_comment {
            return;
        }
        let Some((http_client, token, _)) = self.store.read(cx).request_context() else {
            return;
        };
        let task_id = open_task.summary.id.clone();
        open_task.posting_comment = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    api::post_comment(&http_client, &token, &task_id, &text).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(open_task) = &mut this.open_task else {
                    return;
                };
                open_task.posting_comment = false;
                match result {
                    Ok(()) => {
                        open_task
                            .comment_input
                            .update(cx, |input, cx| input.clear(window, cx));
                        this.reload_open_task(cx);
                    }
                    Err(error) => {
                        open_task.error = Some(t!(
                            "clickup.task.comment_failed",
                            error = format!("{error:#}")
                        ));
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_timer(&mut self, task_id: String, cx: &mut Context<Self>) {
        let is_running = timer_started_at(self.store.read(cx), &task_id).is_some();
        let task = self.store.update(cx, |store, cx| {
            if is_running {
                store.stop_timer(cx)
            } else {
                store.start_timer(task_id, cx)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let error = result
                    .err()
                    .map(|error| t!("clickup.task.timer_failed", error = format!("{error:#}")));
                this.timer_error = None;
                match &mut this.open_task {
                    Some(open_task) => {
                        if error.is_some() {
                            open_task.error = error;
                        }
                    }
                    None => this.timer_error = error,
                }
                this.reload_open_task(cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// The branch checked out in this window's project, when its name carries the task's ID.
    fn branch_for_task(&self, task: &api::Task, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let repository = workspace
            .read(cx)
            .project()
            .read(cx)
            .active_repository(cx)?;
        let branch = repository.read(cx).branch.as_ref()?.name().to_string();
        let lowercase = branch.to_lowercase();
        let ids = [task.id.to_lowercase(), task.display_id().to_lowercase()];
        ids.iter()
            .any(|id| !id.is_empty() && lowercase.contains(id.as_str()))
            .then_some(branch)
    }

    fn render_field(label: SharedString, value: AnyElement, cx: &App) -> impl IntoElement {
        h_flex()
            .min_h(DynamicSpacing::Base24.px(cx))
            .gap(DynamicSpacing::Base08.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base04.px(cx))
            .child(
                div()
                    .flex_none()
                    .w(px(96.))
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(div().flex_1().min_w_0().child(value))
    }

    fn render_status_picker(&self, open_task: &OpenTask, cx: &Context<Self>) -> AnyElement {
        let status = &open_task.summary.status;
        let tone = match status.kind.as_str() {
            "closed" | "done" => StatusTone::Success,
            "custom" => StatusTone::Info,
            _ => StatusTone::Neutral,
        };
        let has_choices = !open_task.statuses.is_empty();
        let trigger = h_flex()
            .id("clickup-task-status")
            .gap(DynamicSpacing::Base04.px(cx))
            .child(status_pill(capitalize(&status.status), tone, cx))
            .when(has_choices, |this| {
                this.child(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
            });
        if !has_choices {
            return trigger.into_any_element();
        }
        let statuses = open_task.statuses.clone();
        let current = status.status.clone();
        let panel = cx.weak_entity();
        PopoverMenu::new("clickup-task-status-menu")
            .trigger(PopoverTriggerArea::new(trigger))
            .menu(move |window, cx| {
                let statuses = statuses.clone();
                let current = current.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for status in statuses {
                        let name = status.status.clone();
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            capitalize(&status.status),
                            status.status == current,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.set_open_task_status(name.clone(), cx)
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn render_task_detail(&self, open_task: &OpenTask, cx: &Context<Self>) -> impl IntoElement {
        let task = &open_task.summary;
        let detail = open_task.detail.as_ref();
        let now = Local::now();
        let url = task.url.clone();
        let copied_id = task.display_id().to_string();
        let comment_focus = open_task.comment_input.focus_handle(cx);

        let breadcrumb = detail
            .and_then(|detail| detail.folder.as_ref())
            .filter(|folder| !folder.hidden)
            .map(|folder| format!("{}  ›  {}", folder.name, task.list.name))
            .unwrap_or_else(|| task.list.name.clone());

        let assignees = match detail {
            Some(detail) if !detail.task.assignees.is_empty() => h_flex()
                .gap(DynamicSpacing::Base04.px(cx))
                .children(
                    detail
                        .task
                        .assignees
                        .iter()
                        .take(3)
                        .map(|assignee| initial_tile(assignee.display_name(), cx)),
                )
                .child(
                    Label::new(
                        detail
                            .task
                            .assignees
                            .iter()
                            .map(|assignee| assignee.display_name())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .size(LabelSize::Small)
                    .truncate(),
                )
                .into_any_element(),
            Some(_) => Label::new(t!("clickup.task.nobody"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            None => Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let due = match task.due_date.and_then(|due| due_label(due, now)) {
            Some((label, color)) => Label::new(label)
                .size(LabelSize::Small)
                .color(color)
                .into_any_element(),
            None => Label::new(t!("clickup.task.no_due_date"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let priority = match priority_pill(task, cx) {
            Some(pill) => h_flex().child(pill).into_any_element(),
            None => Label::new(t!("clickup.task.no_priority"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let spent = detail
            .and_then(|detail| detail.time_spent)
            .unwrap_or_default();
        let estimate = detail.and_then(|detail| detail.time_estimate);
        let spent_label = match estimate {
            Some(estimate) => format!(
                "{} {}",
                format_duration(spent),
                t!(
                    "clickup.task.of_estimate",
                    estimate = format_duration(estimate)
                )
            ),
            None => format_duration(spent),
        };
        let time = v_flex()
            .gap(DynamicSpacing::Base04.px(cx))
            .child(self.render_timer_readout("clickup-task-timer", &task.id, cx))
            .child(
                Label::new(spent_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element();

        let description = detail
            .and_then(|detail| detail.text_content.clone())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let pull_requests = open_task_pull_requests(open_task);
        let branch = self.branch_for_task(task, cx);
        let development_count = pull_requests.len() + usize::from(branch.is_some());

        let mut content = v_flex()
            .id("clickup-task-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base12.px(cx))
            .pb(DynamicSpacing::Base12.px(cx))
            .gap(DynamicSpacing::Base04.px(cx))
            .child(
                Label::new(breadcrumb)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                Label::new(task.name.clone())
                    .size(LabelSize::Large)
                    .weight(FontWeight::BOLD),
            )
            .child(div().h(DynamicSpacing::Base08.px(cx)))
            .child(
                grouped_block(cx)
                    .child(Self::render_field(
                        t!("clickup.task.field_status"),
                        self.render_status_picker(open_task, cx),
                        cx,
                    ))
                    .child(Self::render_field(
                        t!("clickup.task.field_assignees"),
                        assignees,
                        cx,
                    ))
                    .child(Self::render_field(t!("clickup.task.field_due"), due, cx))
                    .child(Self::render_field(
                        t!("clickup.task.field_priority"),
                        priority,
                        cx,
                    ))
                    .child(Self::render_field(t!("clickup.task.field_time"), time, cx)),
            )
            .child(render_section(
                t!("clickup.task.section_development"),
                Some(development_count),
                cx,
            ));

        if development_count == 0 {
            content = content.child(
                Label::new(t!("clickup.task.no_development", id = task.display_id()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        if let Some(branch) = branch {
            content = content.child(render_link_card(
                "clickup-task-branch",
                IconName::GitBranch,
                branch,
                None,
                Some(status_pill(
                    t!("clickup.task.branch_current"),
                    StatusTone::Info,
                    cx,
                )),
                None,
                cx,
            ));
        }
        for (index, link) in pull_requests.into_iter().enumerate() {
            let (title, subtitle) = describe_pull_request(&link);
            content = content.child(render_link_card(
                SharedString::from(format!("clickup-task-pr-{index}")),
                IconName::PullRequest,
                title,
                Some(subtitle),
                None,
                Some(link),
                cx,
            ));
        }

        content = content.child(render_section(
            t!("clickup.task.section_description"),
            None,
            cx,
        ));
        content = match description {
            Some(description) => {
                let is_long = description.chars().count() > DESCRIPTION_PREVIEW_CHARS;
                let shown = if is_long && !open_task.description_expanded {
                    let preview: String = description
                        .chars()
                        .take(DESCRIPTION_PREVIEW_CHARS)
                        .collect();
                    format!("{}…", preview.trim_end())
                } else {
                    description
                };
                content
                    .child(Label::new(shown).size(LabelSize::Small))
                    .when(is_long, |this| {
                        let expanded = open_task.description_expanded;
                        this.child(
                            Button::new(
                                "clickup-task-description-toggle",
                                if expanded {
                                    t!("clickup.task.show_less")
                                } else {
                                    t!("clickup.task.show_more")
                                },
                            )
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .color(Color::Accent)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(open_task) = &mut this.open_task {
                                    open_task.description_expanded =
                                        !open_task.description_expanded;
                                    cx.notify();
                                }
                            })),
                        )
                    })
            }
            None => content.child(
                Label::new(if detail.is_some() {
                    t!("clickup.task.no_description")
                } else {
                    "…".into()
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        };

        content = content.child(render_section(
            t!("clickup.task.section_activity"),
            Some(open_task.comments.len()),
            cx,
        ));
        if open_task.comments.is_empty() {
            content = content.child(
                Label::new(if detail.is_some() {
                    t!("clickup.task.no_comments")
                } else {
                    "…".into()
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }
        for comment in &open_task.comments {
            let when = comment
                .date
                .map(|date| relative_time(date, now))
                .unwrap_or_default();
            content = content.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "clickup-comment-{}",
                        comment.id
                    )))
                    .items_start()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .py(DynamicSpacing::Base04.px(cx))
                    .child(initial_tile(comment.user.display_name(), cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap(DynamicSpacing::Base06.px(cx))
                                    .child(
                                        Label::new(comment.user.display_name().to_string())
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Label::new(when)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Label::new(comment.comment_text.trim().to_string())
                                    .size(LabelSize::Small),
                            )
                            .children(comment.attachments().into_iter().enumerate().map(
                                |(index, attachment)| {
                                    self.render_comment_attachment(
                                        SharedString::from(format!(
                                            "clickup-comment-{}-attachment-{index}",
                                            comment.id
                                        )),
                                        &attachment,
                                        cx,
                                    )
                                },
                            )),
                    ),
            );
        }

        let posting = open_task.posting_comment;
        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| this.close_task(window, cx)),
            )
            .size_full()
            .child(
                HeaderBar::new("clickup-task-header")
                    .start_child(
                        Button::new("clickup-task-back", t!("clickup.task.back"))
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_task(window, cx)),
                            ),
                    )
                    .end_child(
                        Label::new(task.display_id().to_string())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .end_child(
                        IconButton::new("clickup-task-copy-id", IconName::Copy)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(t!("clickup.task.copy_id")))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied_id.clone()))
                            }),
                    )
                    .end_child(
                        IconButton::new("clickup-task-open", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(t!("clickup.common.open_in_clickup")))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .when_some(open_task.error.clone(), |this, error| {
                this.child(render_error(t!("clickup.common.request_failed"), error, cx))
            })
            .child(content)
            .child(
                v_flex()
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.post_open_task_comment(window, cx)
                    }))
                    .gap(DynamicSpacing::Base06.px(cx))
                    .p(DynamicSpacing::Base08.px(cx))
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(open_task.comment_input.clone())
                    .child(
                        h_flex().justify_end().child(
                            primary_button(
                                "clickup-task-comment",
                                Some(IconName::Send),
                                t!("clickup.task.comment"),
                                Some(KeyBinding::for_action_in(
                                    &menu::Confirm,
                                    &comment_focus,
                                    cx,
                                )),
                                cx,
                            )
                            .when(posting, |this| this.opacity(0.5).cursor_default())
                            .when(!posting, |this| {
                                this.on_click(cx.listener(|this, _, window, cx| {
                                    this.post_open_task_comment(window, cx)
                                }))
                            }),
                        ),
                    ),
            )
    }
}

fn render_error(title: SharedString, message: SharedString, cx: &App) -> impl IntoElement {
    div()
        .px(DynamicSpacing::Base08.px(cx))
        .pb(DynamicSpacing::Base06.px(cx))
        .child(error_callout(title, message, cx))
}

fn render_section(title: SharedString, count: Option<usize>, cx: &App) -> impl IntoElement {
    div()
        .pt(DynamicSpacing::Base12.px(cx))
        .pb(DynamicSpacing::Base04.px(cx))
        .child(section_header(
            title,
            count.map(|count| {
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled)
                    .into_any_element()
            }),
        ))
}

fn render_link_card(
    id: impl Into<ElementId>,
    icon: IconName,
    title: String,
    subtitle: Option<String>,
    tag: Option<gpui::Div>,
    url: Option<String>,
    cx: &App,
) -> impl IntoElement {
    card(cx)
        .flex_row()
        .items_center()
        .gap(DynamicSpacing::Base08.px(cx))
        .p(DynamicSpacing::Base08.px(cx))
        .id(id)
        .when_some(url, |this, url| {
            this.cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(move |_, _, cx| cx.open_url(&url))
        })
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(Label::new(title).size(LabelSize::Small).truncate())
                .when_some(subtitle, |this, subtitle| {
                    this.child(
                        Label::new(subtitle)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    )
                }),
        )
        .children(tag)
}

fn render_file_link(id: SharedString, name: String, url: String, cx: &App) -> AnyElement {
    h_flex()
        .id(id)
        .mt(DynamicSpacing::Base04.px(cx))
        .gap(DynamicSpacing::Base06.px(cx))
        .px(DynamicSpacing::Base06.px(cx))
        .py(DynamicSpacing::Base04.px(cx))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(
            Icon::new(IconName::File)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(name).size(LabelSize::XSmall).truncate())
        .into_any_element()
}

/// Where a comment image is cached. Named after its URL, so reopening the task doesn't download
/// it again.
fn comment_image_path(url: &str, extension: Option<&str>) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    let extension = extension
        .filter(|extension| {
            extension
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or("png");
    paths::temp_dir()
        .join("clickup-attachments")
        .join(format!("{:016x}.{extension}", hasher.finish()))
}

/// When the running timer is on `task_id`, the moment it started.
pub(crate) fn timer_started_at(store: &ClickUpStore, task_id: &str) -> Option<i64> {
    let timer = store.running_timer()?;
    (timer.task.as_ref()?.id == task_id).then_some(timer.start?)
}

/// Links to GitHub and Bitbucket pull requests mentioned in `texts`, in order, without repeats.
fn pull_request_links(texts: &[&str]) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    for text in texts {
        for word in text
            .split(|character: char| character.is_whitespace() || "()<>[]\"'".contains(character))
        {
            let word = word.trim_end_matches(['.', ',', ';', ':', '!', '?']);
            let is_pull_request = word.starts_with("https://")
                && (word.contains("github.com/") && word.contains("/pull/")
                    || word.contains("bitbucket.org/") && word.contains("/pull-requests/"));
            if is_pull_request && !links.iter().any(|link| link == word) {
                links.push(word.to_string());
            }
        }
    }
    links
}

fn open_task_pull_requests(open_task: &OpenTask) -> Vec<String> {
    let mut texts: Vec<&str> = open_task
        .comments
        .iter()
        .map(|comment| comment.comment_text.as_str())
        .collect();
    if let Some(description) = open_task
        .detail
        .as_ref()
        .and_then(|detail| detail.text_content.as_deref())
    {
        texts.push(description);
    }
    pull_request_links(&texts)
}

/// A pull request link as `#number` and `owner/repository`.
fn describe_pull_request(link: &str) -> (String, String) {
    let path = link
        .split_once("://")
        .map_or(link, |(_, rest)| rest)
        .split_once('/')
        .map_or("", |(_, path)| path);
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [owner, repository, "pull" | "pull-requests", number, ..] => {
            (format!("PR #{number}"), format!("{owner}/{repository}"))
        }
        _ => (link.to_string(), String::new()),
    }
}

fn format_duration(millis: i64) -> String {
    let minutes = millis.max(0) / 60_000;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match (hours, minutes) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

fn format_clock(millis: i64) -> String {
    let seconds = millis.max(0) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

fn format_row_clock(millis: i64) -> String {
    let seconds = millis.max(0) / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

fn relative_time(millis: i64, now: DateTime<Local>) -> String {
    let Some(time) = Local.timestamp_millis_opt(millis).single() else {
        return String::new();
    };
    let clock = time.format("%H:%M");
    match (now.date_naive() - time.date_naive()).num_days() {
        0 => t!("clickup.date.today_at", time = clock).to_string(),
        1 => t!("clickup.date.yesterday_at", time = clock).to_string(),
        _ => format!("{}, {clock}", day_and_month(time.day(), time.month0())),
    }
}

fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

pub(crate) fn status_dot(color: Option<Hsla>, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(8.))
        .rounded_full()
        .bg(color.unwrap_or_else(|| cx.theme().colors().icon_muted))
}

/// ClickUp sends status colors as `#rrggbb`.
pub(crate) fn parse_color(color: &str) -> Option<Hsla> {
    Rgba::try_from(color).ok().map(Hsla::from)
}

pub(crate) fn priority_pill(task: &api::Task, cx: &App) -> Option<gpui::Div> {
    let (tone, label) = match task.priority.as_ref()?.priority.as_str() {
        "urgent" => (StatusTone::Warning, t!("clickup.task.priority_urgent")),
        "high" => (StatusTone::Warning, t!("clickup.task.priority_high")),
        "normal" => (StatusTone::Neutral, t!("clickup.task.priority_normal")),
        _ => (StatusTone::Neutral, t!("clickup.task.priority_low")),
    };
    Some(status_pill(label, tone, cx))
}

pub(crate) fn task_matches(task: &api::Task, filter: &str) -> bool {
    filter.is_empty()
        || task.name.to_lowercase().contains(filter)
        || task.display_id().to_lowercase().contains(filter)
}

/// In-progress statuses first, in workflow order, then the not-started ones: what you are
/// working on leads the list.
fn group_by_status<'a>(tasks: &[&'a api::Task]) -> Vec<StatusGroup<'a>> {
    let mut groups: Vec<(i64, i64, StatusGroup<'a>)> = Vec::new();
    for task in tasks {
        let key = task.status.status.to_lowercase();
        if let Some((_, order, group)) = groups
            .iter_mut()
            .find(|(_, _, group)| group.name.to_lowercase() == key)
        {
            *order = (*order).min(task.status.orderindex);
            group.tasks.push(task);
            continue;
        }
        let kind_rank = match task.status.kind.as_str() {
            "custom" => 0,
            "open" => 1,
            _ => 2,
        };
        groups.push((
            kind_rank,
            task.status.orderindex,
            StatusGroup {
                name: task.status.status.clone(),
                color: parse_color(&task.status.color),
                tasks: vec![task],
            },
        ));
    }
    groups.sort_by_key(|(kind_rank, order, _)| (*kind_rank, *order));
    groups.into_iter().map(|(_, _, group)| group).collect()
}

fn month_abbreviation(month0: u32) -> SharedString {
    match month0 {
        0 => t!("clickup.date.month_jan"),
        1 => t!("clickup.date.month_feb"),
        2 => t!("clickup.date.month_mar"),
        3 => t!("clickup.date.month_apr"),
        4 => t!("clickup.date.month_may"),
        5 => t!("clickup.date.month_jun"),
        6 => t!("clickup.date.month_jul"),
        7 => t!("clickup.date.month_aug"),
        8 => t!("clickup.date.month_sep"),
        9 => t!("clickup.date.month_oct"),
        10 => t!("clickup.date.month_nov"),
        _ => t!("clickup.date.month_dec"),
    }
}

pub(crate) fn day_and_month(day: u32, month0: u32) -> SharedString {
    t!(
        "clickup.date.day_month",
        day = day,
        month = month_abbreviation(month0)
    )
}

/// "Today", "Tomorrow", "Yesterday" or "Sep 26", colored by urgency.
fn due_label(due_millis: i64, now: DateTime<Local>) -> Option<(String, Color)> {
    let due = Local.timestamp_millis_opt(due_millis).single()?;
    let days = (due.date_naive() - now.date_naive()).num_days();
    let label = match days {
        0 => t!("clickup.date.today").to_string(),
        1 => t!("clickup.date.tomorrow").to_string(),
        -1 => t!("clickup.date.yesterday").to_string(),
        _ => {
            if due.year() == now.year() {
                day_and_month(due.day(), due.month0()).to_string()
            } else {
                t!(
                    "clickup.date.day_month_year",
                    day = due.day(),
                    month = month_abbreviation(due.month0()),
                    year = due.year()
                )
                .to_string()
            }
        }
    };
    let color = if days < 0 {
        Color::Error
    } else if days == 0 {
        Color::Warning
    } else {
        Color::Muted
    };
    Some((label, color))
}

pub(crate) fn sync_status_label(tasks: &AssignedTasks) -> String {
    match tasks.synced_at {
        None => t!("clickup.tasks.syncing").to_string(),
        Some(synced_at) => {
            let minutes = synced_at.elapsed().as_secs() / 60;
            if minutes == 0 {
                t!("clickup.tasks.synced_now").to_string()
            } else {
                t!("clickup.tasks.synced_ago", minutes = minutes).to_string()
            }
        }
    }
}

fn spinning_if(icon: Icon, spinning: bool) -> AnyElement {
    if spinning {
        icon.with_rotate_animation(2).into_any_element()
    } else {
        icon.into_any_element()
    }
}

fn workspace_count(count: usize) -> String {
    if count == 1 {
        t!("clickup.account.workspace_count.one").to_string()
    } else {
        t!("clickup.account.workspace_count.other", count = count).to_string()
    }
}

impl Render for ClickUpPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.store.read(cx).connection() {
            Connection::Loading => {
                EmptyState::new(IconName::LoadCircle, t!("clickup.connection.loading"))
                    .spinning(true)
                    .into_any_element()
            }
            Connection::Disconnected => self.render_connect(false, cx).into_any_element(),
            Connection::Rejected => self.render_connect(true, cx).into_any_element(),
            Connection::Connected(_) if self.replacing_token => {
                self.render_connect(false, cx).into_any_element()
            }
            Connection::Connected(account) => {
                let account = account.clone();
                self.render_connected(&account, cx).into_any_element()
            }
            Connection::Unreachable { message } => {
                let message = message.clone();
                let store = self.store.clone();
                EmptyState::new(IconName::Disconnected, t!("clickup.connection.unreachable"))
                    .description(message)
                    .action(
                        secondary_button(
                            "clickup-retry",
                            None,
                            t!("clickup.connection.retry"),
                            true,
                            cx,
                        )
                        .on_click(move |_, _, cx| store.update(cx, |store, cx| store.retry(cx))),
                    )
                    .action(
                        Button::new(
                            "clickup-replace-unreachable",
                            t!("clickup.account.replace_token"),
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_replacing_token(window, cx);
                            this.store
                                .update(cx, |store, cx| store.disconnect(cx))
                                .detach_and_log_err(cx);
                        })),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("ClickUpPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(content)
    }
}

impl Focusable for ClickUpPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ClickUpPanel {}

impl Panel for ClickUpPanel {
    fn persistent_name() -> &'static str {
        "ClickUpPanel"
    }

    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::CLICKUP, &self.project, cx)
    }

    fn panel_key() -> &'static str {
        "ClickUpPanel"
    }

    /// Until an account is connected, pasting the token is the only thing to do here.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.store.read(cx).connection() {
            Connection::Connected(_) if !self.replacing_token => self.focus_handle.clone(),
            Connection::Disconnected | Connection::Rejected | Connection::Connected(_) => {
                self.token_input.focus_handle(cx)
            }
            Connection::Loading | Connection::Unreachable { .. } => self.focus_handle.clone(),
        }
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
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("ClickUp")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

#[cfg(test)]
mod tests {
    use super::{
        capitalize, describe_pull_request, due_label, format_clock, format_duration,
        format_row_clock, group_by_status, pull_request_links, task_matches,
    };
    use crate::api;
    use chrono::{Local, TimeZone as _};
    use ui::Color;

    fn task(id: &str, name: &str, status: &str, kind: &str, order: i64) -> api::Task {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "custom_id": null,
            "name": name,
            "url": format!("https://app.clickup.com/t/{id}"),
            "status": {"status": status, "color": "#4194f6", "type": kind, "orderindex": order},
            "priority": null,
            "list": {"id": "1", "name": "Sprint"}
        }))
        .unwrap()
    }

    #[test]
    fn groups_in_progress_before_not_started() {
        let tasks = [
            task("1", "a", "to do", "open", 0),
            task("2", "b", "in review", "custom", 2),
            task("3", "c", "In Progress", "custom", 1),
            task("4", "d", "in progress", "custom", 1),
        ];
        let references: Vec<&api::Task> = tasks.iter().collect();
        let groups = group_by_status(&references);
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["In Progress", "in review", "to do"]);
        assert_eq!(
            groups[0].tasks.len(),
            2,
            "status names group case-insensitively"
        );
    }

    #[test]
    fn filters_by_title_or_id() {
        let task = task("86a1b2", "Login com biometria", "to do", "open", 0);
        assert!(task_matches(&task, ""));
        assert!(task_matches(&task, "biometria"));
        assert!(task_matches(&task, "86a1"));
        assert!(!task_matches(&task, "extrato"));
    }

    #[test]
    fn labels_due_dates_relative_to_today() {
        let now = Local.with_ymd_and_hms(2026, 9, 23, 10, 0, 0).unwrap();
        let at = |day: u32| {
            Local
                .with_ymd_and_hms(2026, 9, day, 18, 0, 0)
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(
            due_label(at(23), now),
            Some(("Today".into(), Color::Warning))
        );
        assert_eq!(
            due_label(at(24), now),
            Some(("Tomorrow".into(), Color::Muted))
        );
        assert_eq!(
            due_label(at(22), now),
            Some(("Yesterday".into(), Color::Error))
        );
        assert_eq!(
            due_label(at(29), now),
            Some(("Sep 29".into(), Color::Muted))
        );
    }

    #[test]
    fn finds_pull_request_links() {
        let links = pull_request_links(&[
            "PR: https://github.com/trix/app/pull/152.",
            "Revisar (https://bitbucket.org/trix-investimentos/app/pull-requests/157) e https://github.com/trix/app/pull/152",
            "https://github.com/trix/app/issues/3 não é PR",
        ]);
        assert_eq!(
            links,
            vec![
                "https://github.com/trix/app/pull/152".to_string(),
                "https://bitbucket.org/trix-investimentos/app/pull-requests/157".to_string(),
            ]
        );
        assert_eq!(
            describe_pull_request(&links[1]),
            ("PR #157".to_string(), "trix-investimentos/app".to_string())
        );
    }

    #[test]
    fn formats_durations_and_clocks() {
        assert_eq!(format_duration(10_560_000), "2h 56m");
        assert_eq!(format_duration(14_400_000), "4h");
        assert_eq!(format_duration(90_000), "1m");
        assert_eq!(format_clock(2_533_000), "00:42:13");
        assert_eq!(format_row_clock(2_533_000), "42:13");
        assert_eq!(format_row_clock(3_733_000), "1:02:13");
        assert_eq!(capitalize("em andamento"), "Em andamento");
    }
}
