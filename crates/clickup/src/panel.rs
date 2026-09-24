use crate::{
    Account, AssignedTasks, ClickUpStore, Connection, TOKEN_SETTINGS_URL, ToggleFocus, api,
};
use chrono::{DateTime, Datelike as _, Local, TimeZone as _};
use gpui::{
    Action as _, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Pixels, Rgba,
    Subscription, Task, px,
};
use gpui::{ClipboardItem, WeakEntity};
use std::{collections::HashSet, time::Duration};
use ui::{
    ButtonLike, CommonAnimationExt, ContextMenu, ContextMenuEntry, PopoverMenu, Tab, TabBar,
    TabPosition, TabStyle, Tooltip, prelude::*,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// Long enough that pasting a token checks it once instead of once per keystroke of typing.
const CHECK_DEBOUNCE: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskTab {
    Open,
    Closed,
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
    /// Status names whose group is folded, lowercased.
    collapsed_groups: HashSet<String>,
    /// The task shown in place of the list, when one was opened.
    open_task: Option<OpenTask>,
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

/// Longer descriptions are cut here until "Mostrar mais" is clicked.
const DESCRIPTION_PREVIEW_CHARS: usize = 280;

impl ClickUpPanel {
    pub(crate) fn new(
        store: Entity<ClickUpStore>,
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
            InputField::new(window, cx, "Filtrar por título ou ID")
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
            selected_tab: TaskTab::Open,
            collapsed_groups: HashSet::default(),
            open_task: None,
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
                    "O ClickUp recusou este token. Confira se copiou inteiro ou gere outro.".into()
                } else {
                    format!("Não foi possível falar com o ClickUp: {error}").into()
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
                        let message: SharedString =
                            "Não foi possível salvar o token no keychain.".into();
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

    fn render_badge(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .size(DynamicSpacing::Base48.px(cx))
            .justify_center()
            .rounded_lg()
            .bg(cx.theme().colors().ghost_element_hover)
            .border_1()
            .border_color(cx.theme().colors().border_focused.opacity(0.35))
            .child(
                Icon::new(IconName::ListTodo)
                    .size(IconSize::Medium)
                    .color(Color::Accent),
            )
    }

    fn render_step(&self, number: usize, title: &'static str, cx: &App) -> impl IntoElement {
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
            TokenCheck::Idle => (
                None,
                "Começa com pk_. Fica só no Keychain do macOS.".into(),
                Color::Muted,
            ),
            TokenCheck::Checking => (
                Some(IconName::LoadCircle),
                "Verificando com o ClickUp…".into(),
                Color::Muted,
            ),
            TokenCheck::Valid(account) => (
                Some(IconName::Check),
                format!(
                    "Conectado como {} · {}",
                    account.user.display_name(),
                    workspace_count(account.workspaces.len())
                )
                .into(),
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

    fn render_rejected_banner(&self, cx: &App) -> impl IntoElement {
        let warning = cx.theme().status().warning;
        h_flex()
            .w_full()
            .gap(DynamicSpacing::Base10.px(cx))
            .p(DynamicSpacing::Base10.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(warning.opacity(0.35))
            .bg(warning.opacity(0.08))
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        Label::new("O ClickUp recusou o token salvo")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new("Foi regenerado? Cole o token novo abaixo.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_connect(&self, show_rejected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let is_busy = self.save_task.is_some() || matches!(self.token_check, TokenCheck::Checking);
        let can_connect = !is_busy && !self.token_input.read(cx).is_empty(cx);

        let card = v_flex()
            .w_full()
            .gap(DynamicSpacing::Base12.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(1, "Gere um token em Settings → Apps → API Token", cx))
                    .child(
                        h_flex().pl(DynamicSpacing::Base32.px(cx)).child(
                            Button::new("clickup-open-token-settings", "Abrir no ClickUp")
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Medium)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::ArrowUpRight)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_click(|_, _, cx| cx.open_url(TOKEN_SETTINGS_URL)),
                        ),
                    ),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(2, "Cole o token aqui", cx))
                    .child(self.token_input.clone())
                    .children(self.render_check_hint(cx)),
            )
            .child(
                Button::new("clickup-connect", "Conectar")
                    .full_width()
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Large)
                    .label_size(LabelSize::Small)
                    .disabled(!can_connect)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.connect(&menu::Confirm, window, cx)),
                    ),
            )
            .when(self.replacing_token, |this| {
                this.child(
                    h_flex().justify_center().child(
                        Button::new("clickup-cancel-replace", "Manter o token atual")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.cancel_replacing_token(cx)),
                            ),
                    ),
                )
            });

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
                    .justify_center()
                    .items_center()
                    .gap(DynamicSpacing::Base20.px(cx))
                    .px(DynamicSpacing::Base24.px(cx))
                    .py(DynamicSpacing::Base24.px(cx))
                    .when(show_rejected, |this| this.child(self.render_rejected_banner(cx)))
                    .child(self.render_badge(cx))
                    .child(
                        v_flex()
                            .items_center()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Label::new("Conecte seu ClickUp")
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(
                                    "Use um token pessoal da sua conta. Sem apps para criar e sem expiração.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(card),
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
                Label::new(
                    "O token acessa tudo que sua conta vê no ClickUp. Fica só no Keychain; para revogar, regenere o token no ClickUp.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
    }

    fn render_centered_message(
        &self,
        icon: IconName,
        title: SharedString,
        detail: Option<SharedString>,
        actions: Option<AnyElement>,
        cx: &App,
    ) -> impl IntoElement {
        let is_loading = icon == IconName::LoadCircle;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base24.px(cx))
            .child(spinning_if(
                Icon::new(icon).size(IconSize::Medium).color(Color::Muted),
                is_loading,
            ))
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(actions)
    }

    fn render_account_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let store = self.store.clone();
        PopoverMenu::new("clickup-account-menu")
            .trigger_with_tooltip(
                IconButton::new("clickup-account-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text("Conta do ClickUp"),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let store = store.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(
                        ContextMenuEntry::new("Trocar token")
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
                        ContextMenuEntry::new("Gerenciar token no ClickUp")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .handler(|_, cx| cx.open_url(TOKEN_SETTINGS_URL)),
                    )
                    .separator()
                    .item(
                        ContextMenuEntry::new("Desconectar")
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
            .unwrap_or_else(|| "Nenhum workspace".into());
        let trigger = Button::new("clickup-workspace-picker", name)
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Medium)
            .label_size(LabelSize::Small)
            .start_icon(
                Icon::new(IconName::ListTodo)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            );

        if account.workspaces.len() < 2 {
            return trigger.into_any_element();
        }

        let store = self.store.clone();
        let workspaces = account.workspaces.clone();
        let selected_id = selected.map(|workspace| workspace.id);
        PopoverMenu::new("clickup-workspace-menu")
            .trigger(
                trigger.end_icon(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
            )
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
        h_flex()
            .gap(DynamicSpacing::Base06.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base08.px(cx))
            .child(self.render_workspace_picker(account, cx))
            .child(div().flex_1())
            .child(
                IconButton::new("clickup-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(is_syncing)
                    .tooltip(Tooltip::text("Sincronizar agora"))
                    .on_click(move |_, _, cx| store.update(cx, |store, cx| store.sync(cx))),
            )
    }

    fn render_task_tabs(&self, tasks: &AssignedTasks, cx: &Context<Self>) -> impl IntoElement {
        let tabs = [
            (TaskTab::Open, "Minhas", Some(tasks.open.len())),
            (TaskTab::Closed, "Concluídas", None),
        ];
        TabBar::new("clickup-task-tabs")
            .style(TabStyle::Pill)
            .children(
                tabs.into_iter()
                    .enumerate()
                    .map(|(index, (tab, label, count))| {
                        let selected = self.selected_tab == tab;
                        let position = if index == 0 {
                            TabPosition::First
                        } else {
                            TabPosition::Last
                        };
                        Tab::new(label)
                            .style(TabStyle::Pill)
                            .position(position)
                            .toggle_state(selected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_tab = tab;
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .gap(DynamicSpacing::Base06.px(cx))
                                    .child(Label::new(label).size(LabelSize::Small).color(
                                        if selected {
                                            Color::Default
                                        } else {
                                            Color::Muted
                                        },
                                    ))
                                    .when_some(count, |this, count| {
                                        this.child(
                                            Label::new(count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(if selected {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                }),
                                        )
                                    }),
                            )
                    }),
            )
    }

    fn render_task_row(
        &self,
        task: &api::Task,
        now: DateTime<Local>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let opened = task.clone();
        let is_timed = self
            .store
            .read(cx)
            .running_timer()
            .and_then(|timer| timer.task.as_ref())
            .is_some_and(|timed| timed.id == task.id);
        let due = task.due_date.and_then(|due| due_label(due, now));
        let priority_icon =
            task.priority
                .as_ref()
                .and_then(|priority| match priority.priority.as_str() {
                    "urgent" => Some((IconName::SignalHigh, Color::Error)),
                    "high" => Some((IconName::SignalMedium, Color::Warning)),
                    _ => None,
                });
        let is_closed = task.status.is_closed();

        h_flex()
            .id(SharedString::from(format!("clickup-task-{}", task.id)))
            .items_start()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base06.px(cx))
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_task(opened.clone(), window, cx)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .h(DynamicSpacing::Base20.px(cx))
                    .child(status_dot(parse_color(&task.status.color), cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(task.name.clone()).size(LabelSize::Small).color(
                        if is_closed {
                            Color::Muted
                        } else {
                            Color::Default
                        },
                    ))
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .min_w_0()
                            .child(
                                Label::new(task.display_id().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx),
                            )
                            .child(
                                Label::new("·")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(task.list.name.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .h(DynamicSpacing::Base20.px(cx))
                    .gap(DynamicSpacing::Base06.px(cx))
                    .when(is_timed, |this| {
                        this.child(
                            Icon::new(IconName::Record)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                    })
                    .when_some(priority_icon, |this, (icon, color)| {
                        this.child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    })
                    .when(!is_closed, |this| {
                        this.when_some(due, |this, (label, color)| {
                            this.child(Label::new(label).size(LabelSize::XSmall).color(color))
                        })
                    }),
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
            .child(
                Label::new(group.name.to_uppercase())
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                Label::new(group.tasks.len().to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
    }

    fn render_task_list(&self, tasks: &AssignedTasks, cx: &Context<Self>) -> AnyElement {
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let source = match self.selected_tab {
            TaskTab::Open => &tasks.open,
            TaskTab::Closed => &tasks.closed,
        };
        let visible: Vec<&api::Task> = source
            .iter()
            .filter(|task| task_matches(task, &filter))
            .collect();

        if visible.is_empty() {
            let (icon, title) = if tasks.synced_at.is_none() {
                (IconName::LoadCircle, "Buscando suas tarefas…")
            } else if !filter.is_empty() {
                (IconName::MagnifyingGlass, "Nenhuma tarefa com esse filtro")
            } else if self.selected_tab == TaskTab::Open {
                (IconName::Check, "Nenhuma tarefa aberta atribuída a você")
            } else {
                (IconName::Check, "Nada concluído nos últimos 30 dias")
            };
            return self
                .render_centered_message(icon, title.into(), None, None, cx)
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
            TaskTab::Open => {
                for group in group_by_status(&visible) {
                    let is_collapsed = self.collapsed_groups.contains(&group.name.to_lowercase());
                    list = list.child(self.render_group_header(&group, cx));
                    if !is_collapsed {
                        for task in &group.tasks {
                            list = list.child(self.render_task_row(task, now, cx));
                        }
                    }
                }
            }
            TaskTab::Closed => {
                for task in visible {
                    list = list.child(self.render_task_row(task, now, cx));
                }
            }
        }
        list.into_any_element()
    }

    fn render_sync_error(&self, error: SharedString, cx: &App) -> impl IntoElement {
        let warning = cx.theme().status().warning;
        h_flex()
            .mx(DynamicSpacing::Base08.px(cx))
            .mb(DynamicSpacing::Base06.px(cx))
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base08.px(cx))
            .rounded_md()
            .border_1()
            .border_color(warning.opacity(0.35))
            .bg(warning.opacity(0.08))
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(format!("Falha ao sincronizar: {error}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
    }

    fn render_connected(&self, account: &Account, cx: &Context<Self>) -> AnyElement {
        if let Some(open_task) = &self.open_task {
            return self.render_task_detail(open_task, cx).into_any_element();
        }
        let tasks = self.store.read(cx).tasks();
        let sync_error = tasks.error.clone();
        let sync_label = sync_status_label(tasks);
        let task_tabs = self.render_task_tabs(tasks, cx);
        let task_list = self.render_task_list(tasks, cx);

        v_flex()
            .size_full()
            .child(self.render_context_bar(account, cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .child(task_tabs)
                    .child(self.filter_input.clone()),
            )
            .when_some(sync_error, |this, error| {
                this.child(self.render_sync_error(error, cx))
            })
            .child(task_list)
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .px(DynamicSpacing::Base12.px(cx))
                    .py(DynamicSpacing::Base08.px(cx))
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .flex_none()
                            .size(DynamicSpacing::Base20.px(cx))
                            .justify_center()
                            .rounded_full()
                            .bg(cx.theme().colors().ghost_element_hover)
                            .child(
                                Label::new(initials(account.user.display_name()))
                                    .size(LabelSize::XSmall)
                                    .weight(FontWeight::SEMIBOLD)
                                    .color(Color::Accent),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(account.user.display_name().to_string())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .child(
                                Label::new(sync_label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(self.render_account_menu(cx)),
            )
            .into_any_element()
    }
}

impl ClickUpPanel {
    fn open_task(&mut self, task: api::Task, window: &mut Window, cx: &mut Context<Self>) {
        let comment_input = cx
            .new(|cx| InputField::new(window, cx, &format!("Comentar em {}…", task.display_id())));
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
                cx.notify();
            })
            .log_err();
        }));
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
                    open_task.error =
                        Some(format!("Não deu para trocar o status: {error:#}").into());
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
                        open_task.error = Some(format!("Não deu para comentar: {error:#}").into());
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_open_task_timer(&mut self, cx: &mut Context<Self>) {
        let Some(open_task) = &self.open_task else {
            return;
        };
        let task_id = open_task.summary.id.clone();
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
                if let Err(error) = result
                    && let Some(open_task) = &mut this.open_task
                {
                    open_task.error =
                        Some(format!("Não deu para mexer no timer: {error:#}").into());
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

    fn render_section_header(
        title: &'static str,
        count: Option<usize>,
        trailing: Option<AnyElement>,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .pt(DynamicSpacing::Base12.px(cx))
            .pb(DynamicSpacing::Base04.px(cx))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .when_some(count, |this, count| {
                        this.child(
                            Label::new(count.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Disabled),
                        )
                    }),
            )
            .children(trailing)
    }

    fn render_field(label: &'static str, value: AnyElement, cx: &App) -> impl IntoElement {
        h_flex()
            .min_h(DynamicSpacing::Base24.px(cx))
            .gap(DynamicSpacing::Base08.px(cx))
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
        let color = parse_color(&status.color);
        let trigger = ButtonLike::new("clickup-task-status")
            .style(ButtonStyle::Outlined)
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .px(DynamicSpacing::Base04.px(cx))
                    .child(status_dot(color, cx))
                    .child(Label::new(capitalize(&status.status)).size(LabelSize::Small))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        if open_task.statuses.is_empty() {
            return trigger.disabled(true).into_any_element();
        }
        let statuses = open_task.statuses.clone();
        let current = status.status.clone();
        let panel = cx.weak_entity();
        PopoverMenu::new("clickup-task-status-menu")
            .trigger(trigger)
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

    fn render_timer_card(&self, open_task: &OpenTask, cx: &Context<Self>) -> impl IntoElement {
        let started_at = timer_started_at(self.store.read(cx), &open_task.summary.id);
        let now_millis = Local::now().timestamp_millis();
        let (clock, caption) = match started_at {
            Some(started_at) => {
                let started = Local
                    .timestamp_millis_opt(started_at)
                    .single()
                    .map(|started| format!("desde {}", started.format("%H:%M")))
                    .unwrap_or_default();
                (format_clock(now_millis - started_at), started)
            }
            None => ("00:00:00".to_string(), "Timer parado".to_string()),
        };
        let is_running = started_at.is_some();
        h_flex()
            .gap(DynamicSpacing::Base10.px(cx))
            .p(DynamicSpacing::Base10.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(if is_running {
                cx.theme().colors().border_focused
            } else {
                cx.theme().colors().border_variant
            })
            .bg(cx.theme().colors().element_background)
            .child(
                div()
                    .flex_none()
                    .size(px(8.))
                    .rounded_full()
                    .bg(if is_running {
                        Color::Error.color(cx)
                    } else {
                        Color::Disabled.color(cx)
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .child(
                        Label::new(clock)
                            .size(LabelSize::Large)
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
                Button::new(
                    "clickup-task-timer",
                    if is_running { "Parar" } else { "Iniciar" },
                )
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(if is_running {
                        IconName::Stop
                    } else {
                        IconName::PlayFilled
                    })
                    .size(IconSize::XSmall)
                    .color(if is_running {
                        Color::Error
                    } else {
                        Color::Default
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_open_task_timer(cx))),
            )
    }

    fn render_task_detail(&self, open_task: &OpenTask, cx: &Context<Self>) -> impl IntoElement {
        let task = &open_task.summary;
        let detail = open_task.detail.as_ref();
        let now = Local::now();
        let url = task.url.clone();
        let copied_id = task.display_id().to_string();

        let breadcrumb = detail
            .and_then(|detail| detail.folder.as_ref())
            .filter(|folder| !folder.hidden)
            .map(|folder| format!("{}  ›  {}", folder.name, task.list.name))
            .unwrap_or_else(|| task.list.name.clone());

        let assignees = match detail {
            Some(detail) if !detail.task.assignees.is_empty() => h_flex()
                .gap(DynamicSpacing::Base04.px(cx))
                .children(detail.task.assignees.iter().take(3).map(|assignee| {
                    h_flex()
                        .flex_none()
                        .size(DynamicSpacing::Base20.px(cx))
                        .justify_center()
                        .rounded_full()
                        .bg(cx.theme().colors().ghost_element_hover)
                        .child(
                            Label::new(initials(assignee.display_name()))
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Accent),
                        )
                }))
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
            Some(_) => Label::new("Ninguém")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            None => Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let due = match task.due_date.and_then(|due| due_label(due, now)) {
            Some((label, color)) => h_flex()
                .gap(DynamicSpacing::Base04.px(cx))
                .child(
                    Icon::new(IconName::Clock)
                        .size(IconSize::XSmall)
                        .color(color),
                )
                .child(Label::new(label).size(LabelSize::Small).color(color))
                .into_any_element(),
            None => Label::new("Sem prazo")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let priority = match task
            .priority
            .as_ref()
            .map(|priority| priority.priority.as_str())
        {
            Some(priority) => {
                let (icon, color, label) = match priority {
                    "urgent" => (IconName::SignalHigh, Color::Error, "Urgente"),
                    "high" => (IconName::SignalMedium, Color::Warning, "Alta"),
                    "normal" => (IconName::SignalLow, Color::Muted, "Normal"),
                    _ => (IconName::SignalLow, Color::Disabled, "Baixa"),
                };
                h_flex()
                    .gap(DynamicSpacing::Base04.px(cx))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(Label::new(label).size(LabelSize::Small))
                    .into_any_element()
            }
            None => Label::new("Sem prioridade")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let spent = detail
            .and_then(|detail| detail.time_spent)
            .unwrap_or_default();
        let estimate = detail.and_then(|detail| detail.time_estimate);
        let time = v_flex()
            .gap(DynamicSpacing::Base04.px(cx))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(Label::new(format_duration(spent)).size(LabelSize::Small))
                    .when_some(estimate, |this, estimate| {
                        this.child(
                            Label::new(format!("de {} estimadas", format_duration(estimate)))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(
                estimate.filter(|estimate| *estimate > 0),
                |this, estimate| {
                    let ratio = (spent as f32 / estimate as f32).clamp(0., 1.);
                    this.child(
                        div()
                            .h(px(4.))
                            .w_full()
                            .rounded_full()
                            .bg(cx.theme().colors().element_background)
                            .child(div().h_full().w(relative(ratio)).rounded_full().bg(
                                if spent > estimate {
                                    Color::Error.color(cx)
                                } else {
                                    Color::Accent.color(cx)
                                },
                            )),
                    )
                },
            )
            .into_any_element();

        let description = detail
            .and_then(|detail| detail.text_content.clone())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let mut texts: Vec<&str> = open_task
            .comments
            .iter()
            .map(|comment| comment.comment_text.as_str())
            .collect();
        if let Some(description) = &description {
            texts.push(description);
        }
        let pull_requests = pull_request_links(&texts);
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
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(div().h(DynamicSpacing::Base08.px(cx)))
            .child(Self::render_field(
                "Status",
                self.render_status_picker(open_task, cx),
                cx,
            ))
            .child(Self::render_field("Responsáveis", assignees, cx))
            .child(Self::render_field("Prazo", due, cx))
            .child(Self::render_field("Prioridade", priority, cx))
            .child(Self::render_field("Tempo", time, cx))
            .child(div().h(DynamicSpacing::Base08.px(cx)))
            .child(self.render_timer_card(open_task, cx))
            .child(Self::render_section_header(
                "DESENVOLVIMENTO",
                Some(development_count),
                None,
                cx,
            ));

        if development_count == 0 {
            content = content.child(
                Label::new(format!(
                    "Nenhum PR citado. Um branch com {} no nome aparece aqui.",
                    task.display_id()
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }
        if let Some(branch) = branch {
            content = content.child(render_link_row(
                "clickup-task-branch",
                IconName::GitBranch,
                branch,
                "branch atual".to_string(),
                None,
                cx,
            ));
        }
        for (index, link) in pull_requests.into_iter().enumerate() {
            let (title, subtitle) = describe_pull_request(&link);
            content = content.child(render_link_row(
                SharedString::from(format!("clickup-task-pr-{index}")),
                IconName::PullRequest,
                title,
                subtitle,
                Some(link),
                cx,
            ));
        }

        content = content.child(Self::render_section_header("DESCRIÇÃO", None, None, cx));
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
                                    "Mostrar menos"
                                } else {
                                    "Mostrar mais"
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
                    "Sem descrição"
                } else {
                    "…"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        };

        content = content.child(Self::render_section_header(
            "COMENTÁRIOS",
            Some(open_task.comments.len()),
            None,
            cx,
        ));
        if open_task.comments.is_empty() {
            content = content.child(
                Label::new(if detail.is_some() {
                    "Nenhum comentário ainda"
                } else {
                    "…"
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
                    .child(
                        h_flex()
                            .flex_none()
                            .size(DynamicSpacing::Base20.px(cx))
                            .justify_center()
                            .rounded_full()
                            .bg(cx.theme().colors().ghost_element_hover)
                            .child(
                                Label::new(initials(comment.user.display_name()))
                                    .size(LabelSize::XSmall)
                                    .weight(FontWeight::SEMIBOLD)
                                    .color(Color::Accent),
                            ),
                    )
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
                            .children(comment.attachments().enumerate().map(
                                |(index, attachment)| {
                                    render_comment_attachment(
                                        SharedString::from(format!(
                                            "clickup-comment-{}-attachment-{index}",
                                            comment.id
                                        )),
                                        attachment,
                                        cx,
                                    )
                                },
                            )),
                    ),
            );
        }

        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| this.close_task(window, cx)),
            )
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px(DynamicSpacing::Base08.px(cx))
                    .py(DynamicSpacing::Base06.px(cx))
                    .child(
                        Button::new("clickup-task-back", "Tarefas")
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_task(window, cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base02.px(cx))
                            .child(
                                Label::new(task.display_id().to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                                    .mr(DynamicSpacing::Base04.px(cx)),
                            )
                            .child(
                                IconButton::new("clickup-task-copy-id", IconName::Copy)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Copiar ID"))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copied_id.clone(),
                                        ))
                                    }),
                            )
                            .child(
                                IconButton::new("clickup-task-open", IconName::ArrowUpRight)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Abrir no ClickUp"))
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            ),
                    ),
            )
            .when_some(open_task.error.clone(), |this, error| {
                this.child(self.render_sync_error(error, cx))
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
                        h_flex()
                            .justify_between()
                            .child(
                                Label::new("Enter envia")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new("clickup-task-comment", "Comentar")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
                                    .disabled(open_task.posting_comment)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.post_open_task_comment(window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

fn render_link_row(
    id: impl Into<ElementId>,
    icon: IconName,
    title: String,
    subtitle: String,
    url: Option<String>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .gap(DynamicSpacing::Base08.px(cx))
        .p(DynamicSpacing::Base08.px(cx))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
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
                .child(
                    Label::new(subtitle)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
}

/// An image shown in place, which opens the original when clicked, or a link to any other file.
fn render_comment_attachment(
    id: SharedString,
    attachment: &api::CommentAttachment,
    cx: &App,
) -> AnyElement {
    let url = attachment.url.clone().unwrap_or_default();
    if attachment.is_image()
        && let Some(preview) = attachment.preview_url()
    {
        let name = attachment.display_name().to_string();
        return div()
            .id(id)
            .mt(DynamicSpacing::Base04.px(cx))
            .max_w_full()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .cursor_pointer()
            .tooltip(Tooltip::text(format!("Abrir {name}")))
            .on_click(move |_, _, cx| cx.open_url(&url))
            .child(
                gpui::img(preview.to_string())
                    .max_w_full()
                    .max_h(px(240.))
                    .object_fit(gpui::ObjectFit::Contain)
                    .with_loading(|| {
                        Label::new("Carregando imagem…")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element()
                    })
                    .with_fallback(move || {
                        Label::new(format!("Não deu para carregar {name}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element()
                    }),
            )
            .into_any_element();
    }
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
        .child(
            Label::new(attachment.display_name().to_string())
                .size(LabelSize::XSmall)
                .truncate(),
        )
        .into_any_element()
}

/// When the running timer is on `task_id`, the moment it started.
fn timer_started_at(store: &ClickUpStore, task_id: &str) -> Option<i64> {
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

fn relative_time(millis: i64, now: DateTime<Local>) -> String {
    let Some(time) = Local.timestamp_millis_opt(millis).single() else {
        return String::new();
    };
    let clock = time.format("%H:%M");
    match (now.date_naive() - time.date_naive()).num_days() {
        0 => format!("hoje, {clock}"),
        1 => format!("ontem, {clock}"),
        _ => {
            let month = MONTHS
                .get(time.month0() as usize)
                .copied()
                .unwrap_or_default();
            format!("{} {month}, {clock}", time.day())
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

fn status_dot(color: Option<Hsla>, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(8.))
        .rounded_full()
        .bg(color.unwrap_or_else(|| cx.theme().colors().icon_muted))
}

/// ClickUp sends status colors as `#rrggbb`.
fn parse_color(color: &str) -> Option<Hsla> {
    Rgba::try_from(color).ok().map(Hsla::from)
}

fn task_matches(task: &api::Task, filter: &str) -> bool {
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

const MONTHS: [&str; 12] = [
    "jan", "fev", "mar", "abr", "mai", "jun", "jul", "ago", "set", "out", "nov", "dez",
];

/// "Hoje", "Amanhã", "Ontem" or "26 set", colored by urgency.
fn due_label(due_millis: i64, now: DateTime<Local>) -> Option<(String, Color)> {
    let due = Local.timestamp_millis_opt(due_millis).single()?;
    let days = (due.date_naive() - now.date_naive()).num_days();
    let label = match days {
        0 => "Hoje".to_string(),
        1 => "Amanhã".to_string(),
        -1 => "Ontem".to_string(),
        _ => {
            let month = MONTHS
                .get(due.month0() as usize)
                .copied()
                .unwrap_or_default();
            if due.year() == now.year() {
                format!("{} {month}", due.day())
            } else {
                format!("{} {month} {}", due.day(), due.year())
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

fn sync_status_label(tasks: &AssignedTasks) -> String {
    let open = match tasks.open.len() {
        1 => "1 aberta".to_string(),
        count => format!("{count} abertas"),
    };
    match tasks.synced_at {
        None => "Sincronizando…".to_string(),
        Some(synced_at) => {
            let minutes = synced_at.elapsed().as_secs() / 60;
            if minutes == 0 {
                format!("Sincronizado agora · {open}")
            } else {
                format!("Sincronizado há {minutes} min · {open}")
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
        "1 workspace".to_string()
    } else {
        format!("{count} workspaces")
    }
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

impl Render for ClickUpPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.store.read(cx).connection() {
            Connection::Loading => self
                .render_centered_message(
                    IconName::LoadCircle,
                    "Carregando o ClickUp…".into(),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
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
                let actions = h_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(
                        Button::new("clickup-retry", "Tentar de novo")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                store.update(cx, |store, cx| store.retry(cx))
                            }),
                    )
                    .child(
                        Button::new("clickup-replace-unreachable", "Trocar token")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_replacing_token(window, cx);
                                this.store
                                    .update(cx, |store, cx| store.disconnect(cx))
                                    .detach_and_log_err(cx);
                            })),
                    )
                    .into_any_element();
                self.render_centered_message(
                    IconName::Disconnected,
                    "Sem conexão com o ClickUp".into(),
                    Some(message),
                    Some(actions),
                    cx,
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
        group_by_status, initials, pull_request_links, task_matches,
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
            Some(("Hoje".into(), Color::Warning))
        );
        assert_eq!(
            due_label(at(24), now),
            Some(("Amanhã".into(), Color::Muted))
        );
        assert_eq!(due_label(at(22), now), Some(("Ontem".into(), Color::Error)));
        assert_eq!(
            due_label(at(29), now),
            Some(("29 set".into(), Color::Muted))
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
        assert_eq!(capitalize("em andamento"), "Em andamento");
    }

    #[test]
    fn initials_take_the_first_two_words() {
        assert_eq!(initials("Trix Investimentos"), "TI");
        assert_eq!(initials("vinicios"), "V");
        assert_eq!(initials("A B C"), "AB");
        assert_eq!(initials(""), "");
        assert_eq!(initials("Trix | Trx"), "TT");
    }
}
