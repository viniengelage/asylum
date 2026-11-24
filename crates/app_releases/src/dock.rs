use crate::{
    OpenAccounts, ToggleFocus,
    accounts::AccountKind,
    model::Release,
    next_action, release_item, status_label,
    store::{AccountsState, AppReleases, Detection, ReleasesStore, SourceStatus},
};
use gpui::{
    Action as _, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels, Subscription,
    WeakEntity, px,
};
use i18n::t;
use project::Project;
use ui::{
    ContextMenu, EmptyState, EmptyStateTone, HeaderBar, PopoverMenu, StatusTone, Tooltip, card,
    error_callout, navigator_footer, prelude::*, primary_button, secondary_button, segment_option,
    segment_option_with_count, segmented_control, select_field, status_pill,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

/// How many versions "Recentes" shows; older ones are under "Histórico".
const RECENT_RELEASES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    Recent,
    Moving,
    History,
}

pub struct ReleasesPanel {
    focus_handle: FocusHandle,
    store: Entity<ReleasesStore>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    filter: Filter,
    _subscriptions: Vec<Subscription>,
}

impl ReleasesPanel {
    pub(crate) fn new(
        store: Entity<ReleasesStore>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let accounts = store.read(cx).accounts().clone();
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&accounts, |_, _, cx| cx.notify()),
        ];
        Self {
            focus_handle: cx.focus_handle(),
            store,
            project,
            workspace,
            filter: Filter::Recent,
            _subscriptions: subscriptions,
        }
    }

    fn open_release(
        &self,
        folder: std::sync::Arc<std::path::Path>,
        version: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                release_item::open(workspace, store, folder, version, window, cx)
            })
            .log_err();
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let refreshing = store.is_refreshing();
        let app_picker = (store.apps().len() > 1).then(|| {
            let selected = store
                .selected_app()
                .map(AppReleases::name)
                .unwrap_or_default();
            let names: Vec<String> = store.apps().iter().map(AppReleases::name).collect();
            let store_handle = self.store.clone();
            PopoverMenu::new("releases-app-picker")
                .trigger(ui::PopoverTriggerArea::new(select_field(
                    "releases-app-picker-trigger",
                    selected,
                    cx,
                )))
                .menu(move |window, cx| {
                    let names = names.clone();
                    let store = store_handle.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            for (index, name) in names.iter().enumerate() {
                                let store = store.clone();
                                menu = menu.entry(name.clone(), None, move |_window, cx| {
                                    store.update(cx, |store, cx| store.select_app(index, cx));
                                });
                            }
                            menu
                        },
                    ))
                })
                .into_any_element()
        });
        HeaderBar::new("releases-header")
            .start_child(
                Label::new(t!("app_releases.dock.title"))
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .start_children(app_picker)
            .end_child(
                IconButton::new("releases-clean-build", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        t!("app_releases.clean_build.title"),
                        &crate::NewCleanBuild,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::NewCleanBuild), cx)
                    }),
            )
            .end_child(
                IconButton::new("releases-release-agent", IconName::Sparkle)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        t!("app_releases.agent.create"),
                        &crate::CreateReleaseAgent,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::CreateReleaseAgent), cx)
                    }),
            )
            .end_child(
                IconButton::new("releases-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(refreshing)
                    .tooltip(Tooltip::text(t!("app_releases.dock.refresh")))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.store.update(cx, |store, cx| store.refresh(cx));
                    })),
            )
            .end_child(
                IconButton::new("releases-accounts", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        t!("app_releases.accounts.title"),
                        &OpenAccounts,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenAccounts.boxed_clone(), cx)
                    }),
            )
    }

    fn render_source_problems(&self, app: &AppReleases, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for kind in AccountKind::ALL {
            let status = app.sources.get(kind);
            let (title, message) = match status {
                SourceStatus::Refused => (
                    t!("app_releases.problem.refused", service = kind.label()),
                    match kind {
                        AccountKind::AppStore => t!("app_releases.problem.refused_ios"),
                        AccountKind::GooglePlay => t!("app_releases.problem.refused_android"),
                        AccountKind::Expo => t!("app_releases.problem.refused_eas"),
                    },
                ),
                SourceStatus::AppNotFound => (
                    t!("app_releases.problem.app_not_found", service = kind.label()),
                    match kind {
                        AccountKind::AppStore => {
                            app.link.bundle_id.clone().unwrap_or_default().into()
                        }
                        AccountKind::GooglePlay => {
                            app.link.package.clone().unwrap_or_default().into()
                        }
                        AccountKind::Expo => {
                            app.link.eas_project_id.clone().unwrap_or_default().into()
                        }
                    },
                ),
                SourceStatus::Failed(message) => (
                    t!("app_releases.problem.failed", service = kind.label()),
                    message.clone(),
                ),
                _ => continue,
            };
            elements.push(
                v_flex()
                    .gap_1()
                    .child(error_callout(title, message, cx))
                    .when(matches!(status, SourceStatus::Refused), |this| {
                        this.child(
                            h_flex().justify_end().child(
                                secondary_button(
                                    ("releases-review-account", kind as usize),
                                    None,
                                    t!("app_releases.problem.review_account"),
                                    true,
                                    cx,
                                )
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(OpenAccounts.boxed_clone(), cx)
                                }),
                            ),
                        )
                    })
                    .into_any_element(),
            );
        }
        let missing: Vec<&str> = AccountKind::ALL
            .into_iter()
            .filter(|kind| *app.sources.get(*kind) == SourceStatus::NoAccount)
            .map(AccountKind::label)
            .collect();
        if !missing.is_empty() {
            elements.push(
                Label::new(t!(
                    "app_releases.problem.missing_accounts",
                    services = missing.join(", ")
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
            );
        }
        elements
    }

    fn render_release(
        &self,
        index: usize,
        app: &AppReleases,
        release: &Release,
        cx: &Context<Self>,
    ) -> AnyElement {
        let folder = app.folder.clone();
        let version = release.version.clone();
        let commit = release
            .platforms
            .iter()
            .find_map(|timeline| timeline.commit.clone())
            .map(|commit| commit.chars().take(7).collect::<String>());
        card(cx)
            .id(("release-card", index))
            .p_3()
            .gap_2()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(release.title())
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .children(commit.map(|commit| {
                        Label::new(commit)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                    })),
            )
            .children(release.platforms.iter().map(|timeline| {
                let (label, tone) = status_label(timeline);
                h_flex()
                    .gap_2()
                    .child(
                        div().w(px(56.)).flex_none().child(
                            Label::new(timeline.platform.label())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(status_pill(label, tone, cx))
                    .children(next_action(timeline).map(|action| {
                        div().flex_1().min_w_0().child(
                            Label::new(action)
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder)
                                .truncate(),
                        )
                    }))
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_release(folder.clone(), version.clone(), window, cx);
            }))
            .into_any_element()
    }

    fn render_filter(&self, releases: &[Release], cx: &Context<Self>) -> impl IntoElement {
        let moving = releases
            .iter()
            .filter(|release| release.is_moving() || release.needs_attention())
            .count();
        let option = |id: &'static str, label: SharedString, filter: Filter, cx: &Context<Self>| {
            segment_option(id, label, self.filter == filter, true, cx).on_click(cx.listener(
                move |this, _, _window, cx| {
                    this.filter = filter;
                    cx.notify();
                },
            ))
        };
        segmented_control(cx)
            .w_full()
            .child(option(
                "releases-filter-recent",
                t!("app_releases.filter.recent"),
                Filter::Recent,
                cx,
            ))
            .child(
                segment_option_with_count(
                    "releases-filter-moving",
                    t!("app_releases.filter.moving"),
                    moving.to_string(),
                    self.filter == Filter::Moving,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.filter = Filter::Moving;
                    cx.notify();
                })),
            )
            .child(option(
                "releases-filter-history",
                t!("app_releases.filter.history"),
                Filter::History,
                cx,
            ))
    }

    fn filtered<'a>(&self, releases: &'a [Release]) -> Vec<(usize, &'a Release)> {
        releases
            .iter()
            .enumerate()
            .filter(|(index, release)| match self.filter {
                Filter::Recent => *index < RECENT_RELEASES,
                Filter::Moving => release.is_moving() || release.needs_attention(),
                Filter::History => *index >= RECENT_RELEASES || release.is_live_everywhere(),
            })
            .collect()
    }

    fn render_footer(&self, app: &AppReleases, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let (tone, text) = if store.is_refreshing() {
            (StatusTone::Info, t!("app_releases.dock.refreshing"))
        } else if let Some(refreshed_at) = app.refreshed_at {
            let time = refreshed_at.format("%H:%M").to_string();
            if store.is_polling() {
                (
                    StatusTone::Info,
                    t!("app_releases.dock.updated_polling", time = time),
                )
            } else {
                (
                    StatusTone::Success,
                    t!("app_releases.dock.updated", time = time),
                )
            }
        } else {
            (StatusTone::Neutral, t!("app_releases.dock.not_loaded"))
        };
        navigator_footer(Some(tone), text, None, cx).py_2()
    }

    fn render_no_accounts(&self, cx: &Context<Self>) -> AnyElement {
        EmptyState::new(IconName::Lock, t!("app_releases.empty.no_accounts_title"))
            .description(t!("app_releases.empty.no_accounts_description"))
            .action(
                primary_button(
                    "releases-setup-accounts",
                    None,
                    t!("app_releases.accounts.setup"),
                    None,
                    cx,
                )
                .on_click(|_, window, cx| window.dispatch_action(OpenAccounts.boxed_clone(), cx)),
            )
            .into_any_element()
    }

    fn render_not_linked(&self, cx: &Context<Self>) -> AnyElement {
        EmptyState::new(IconName::Link, t!("app_releases.empty.not_linked_title"))
            .description(t!("app_releases.empty.not_linked_description"))
            .action(
                primary_button(
                    "releases-link-app",
                    None,
                    t!("app_releases.empty.link_app"),
                    None,
                    cx,
                )
                .on_click(|_, window, cx| window.dispatch_action(OpenAccounts.boxed_clone(), cx)),
            )
            .action(
                secondary_button(
                    "releases-detect-again",
                    None,
                    t!("app_releases.empty.detect_again"),
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.store.update(cx, |store, cx| store.detect(cx));
                })),
            )
            .into_any_element()
    }

    fn render_content(&self, cx: &Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let accounts = store.accounts().read(cx);
        match accounts.state() {
            AccountsState::Loading => {
                return EmptyState::new(IconName::LoadCircle, t!("app_releases.dock.loading"))
                    .spinning(true)
                    .into_any_element();
            }
            AccountsState::Failed(message) => {
                return EmptyState::new(IconName::Warning, t!("app_releases.dock.accounts_failed"))
                    .description(message.clone())
                    .tone(EmptyStateTone::Error)
                    .into_any_element();
            }
            AccountsState::Loaded(stored) if stored.is_empty() => {
                return self.render_no_accounts(cx);
            }
            AccountsState::Loaded(_) => {}
        }
        if matches!(store.detection(), Detection::Detecting) {
            return EmptyState::new(IconName::LoadCircle, t!("app_releases.dock.detecting"))
                .spinning(true)
                .into_any_element();
        }
        let Some(app) = store.selected_app() else {
            return self.render_not_linked(cx);
        };
        if !app.link.is_linked() {
            return self.render_not_linked(cx);
        }

        let releases = &app.releases;
        let list: Vec<AnyElement> = self
            .filtered(releases)
            .into_iter()
            .map(|(index, release)| self.render_release(index, app, release, cx))
            .collect();
        let empty_list = list.is_empty();
        let loading_first_time = app.refreshed_at.is_none() && store.is_refreshing();

        v_flex()
            .size_full()
            .child(
                v_flex()
                    .id("releases-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .gap_3()
                    .children(self.render_source_problems(app, cx))
                    .child(self.render_filter(releases, cx))
                    .when(loading_first_time, |this| {
                        this.child(
                            EmptyState::new(
                                IconName::LoadCircle,
                                t!("app_releases.dock.refreshing"),
                            )
                            .spinning(true),
                        )
                    })
                    .when(empty_list && !loading_first_time, |this| {
                        this.child(
                            Label::new(match self.filter {
                                Filter::Moving => t!("app_releases.dock.nothing_moving"),
                                _ => t!("app_releases.dock.no_releases"),
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .children(list),
            )
            .child(self.render_footer(app, cx))
            .into_any_element()
    }
}

impl Render for ReleasesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ReleasesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(self.render_content(cx)))
    }
}

impl Focusable for ReleasesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ReleasesPanel {}

impl Panel for ReleasesPanel {
    fn persistent_name() -> &'static str {
        "ReleasesPanel"
    }

    fn panel_key() -> &'static str {
        "ReleasesPanel"
    }

    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::RELEASES, &self.project, cx)
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

    /// Opening the dock is what refreshes when nothing is moving.
    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.store
                .update(cx, |store, cx| store.refresh_if_stale(cx));
        }
    }

    /// No icon: the toolkit menu is the way in, like the other tool docks.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Smartphone)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Lançamentos")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        15
    }
}
