//! Lançamentos: one timeline per version of a mobile app, joining EAS builds and submissions,
//! App Store Connect and Google Play. The dock lists the versions, a version opens as a tab.

mod accounts;
mod accounts_view;
mod agent_toolkit;
mod app_store_connect;
mod clean_build;
mod dock;
mod eas;
mod google_play;
mod insights;
mod model;
mod release_item;
mod store;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use dock::ReleasesPanel;
pub use store::{AccountsStore, ReleasesStore};

use gpui::{Entity, Subscription, WeakEntity, actions};
use i18n::t;
use model::{NoticeKind, PlatformTimeline, StepKind, StepState};
use std::{any::TypeId, rc::Rc};
use store::ReleasesEvent;
use ui::{StatusTone, prelude::*, status_pill};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Toast, ToolkitEntry, Workspace,
    notifications::NotificationId,
};

actions!(
    app_releases,
    [
        /// Opens the Lançamentos dock, or hands focus back if it already has it.
        ToggleFocus,
        /// Opens the store and EAS accounts used to follow releases.
        OpenAccounts,
        /// Reads EAS, App Store Connect and Google Play again.
        Refresh,
        /// Builds on EAS what a branch has on origin, from a throwaway worktree.
        NewCleanBuild,
        /// Adds the ready-made "Lançar versão" agent, which takes a version from a branch to the
        /// stores with the Lançamentos tools.
        CreateReleaseAgent,
    ]
);

/// The failures the UI reacts to differently. Anything else surfaces as a plain error.
#[derive(Debug)]
pub enum ApiError {
    /// The credential was revoked, mistyped or lacks access to this app.
    Unauthorized,
    RateLimited,
    Status {
        status: u16,
        body: String,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(formatter, "{}", t!("app_releases.api.unauthorized")),
            Self::RateLimited => write!(formatter, "{}", t!("app_releases.api.rate_limited")),
            Self::Status { status, body } => write!(
                formatter,
                "{}",
                t!("app_releases.api.status", status = status, body = body)
            ),
        }
    }
}

impl std::error::Error for ApiError {}

pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ApiError>(),
        Some(ApiError::Unauthorized)
    )
}

pub fn init(cx: &mut App) {
    clean_build::remove_stale_worktrees(cx);
    workspace::register_panel_item::<ReleasesPanel>(cx);
    workspace::register_lazy_panel::<ReleasesPanel>(
        |workspace, window, cx| {
            if workspace.panel::<ReleasesPanel>(cx).is_some() || !toolkit_enabled(workspace, cx) {
                return;
            }
            add_panel(workspace, window, cx);
        },
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenAccounts, window, cx| {
            if toolkit_allowed(workspace, cx) {
                accounts_view::open(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &NewCleanBuild, window, cx| {
            if toolkit_allowed(workspace, cx) {
                clean_build::open(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &CreateReleaseAgent, window, cx| {
            if toolkit_allowed(workspace, cx) {
                agent_toolkit::create_release_agent(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &Refresh, _window, cx| {
            let store = ReleasesStore::for_project(workspace.project(), cx);
            store.update(cx, |store, cx| store.refresh(cx));
        });
    })
    .detach();
}

fn add_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = ReleasesStore::for_project(workspace.project(), cx);
    let project = workspace.project().clone();
    let weak_workspace = workspace.weak_handle();
    let panel = cx.new(|cx| ReleasesPanel::new(store, project, weak_workspace, window, cx));
    workspace.add_panel(panel, window, cx);
}

/// Adds the panel the first time it is asked for: someone who doesn't ship apps should not get
/// a tab for it.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<ReleasesPanel>(cx).is_none() {
        add_panel(workspace, window, cx);
    }
    workspace.focus_panel::<ReleasesPanel>(window, cx);
}

fn toolkit_enabled(workspace: &Workspace, cx: &App) -> bool {
    task_agents::toolkit_enabled(task_agents::toolkit_ids::RELEASES, workspace.project(), cx)
}

fn toolkit_allowed(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    if toolkit_enabled(workspace, cx) {
        return true;
    }
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<ReleasesPanel>(),
            task_agents::toolkit_disabled_message(&t!("app_releases.dock.title")),
        )
        .autohide(),
        cx,
    );
    false
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<ReleasesPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<ReleasesPanel>(window, cx);
}

pub(crate) fn step_title(kind: StepKind) -> SharedString {
    match kind {
        StepKind::Build => t!("app_releases.step.build"),
        StepKind::Submit => t!("app_releases.step.submit"),
        StepKind::Processing => t!("app_releases.step.processing"),
        StepKind::TestFlight => "TestFlight".into(),
        StepKind::Review => t!("app_releases.step.review"),
        StepKind::Live => t!("app_releases.step.live"),
        StepKind::Testing => t!("app_releases.step.testing"),
        StepKind::Production => t!("app_releases.step.production"),
    }
}

pub(crate) fn state_tone(state: StepState) -> StatusTone {
    match state {
        StepState::Done => StatusTone::Success,
        StepState::Running => StatusTone::Info,
        StepState::Attention => StatusTone::Warning,
        StepState::Failed => StatusTone::Error,
        StepState::Pending | StepState::Unknown => StatusTone::Neutral,
    }
}

/// The Status Pill text for a platform line: where the version is right now.
pub(crate) fn status_label(timeline: &PlatformTimeline) -> (SharedString, StatusTone) {
    let Some(step) = timeline.current_step() else {
        return (t!("app_releases.status.unknown"), StatusTone::Neutral);
    };
    if timeline.is_live() {
        return (t!("app_releases.status.live"), StatusTone::Success);
    }
    let label = match (step.kind, step.state) {
        (StepKind::Build, StepState::Running) => t!("app_releases.status.building"),
        (StepKind::Build, StepState::Failed) => t!("app_releases.status.build_failed"),
        (StepKind::Submit, StepState::Running) => t!("app_releases.status.submitting"),
        (StepKind::Submit, StepState::Failed) => t!("app_releases.status.submit_failed"),
        (StepKind::Submit, StepState::Done) => t!("app_releases.status.sent"),
        (StepKind::Processing, StepState::Running) => t!("app_releases.status.processing"),
        (StepKind::Processing, StepState::Failed) => t!("app_releases.status.invalid_build"),
        (StepKind::TestFlight, StepState::Running) => t!("app_releases.status.beta_review"),
        (StepKind::TestFlight, StepState::Attention) => t!("app_releases.status.beta_rejected"),
        (StepKind::TestFlight, StepState::Pending) => t!("app_releases.status.processed"),
        (StepKind::Review, StepState::Running) => t!("app_releases.status.in_review"),
        (StepKind::Review, StepState::Attention) => t!("app_releases.status.rejected"),
        (StepKind::Review, StepState::Pending) => t!("app_releases.status.in_testflight"),
        (StepKind::Live, StepState::Pending) => t!("app_releases.status.approved"),
        (StepKind::Live, StepState::Running) => t!("app_releases.status.releasing"),
        (StepKind::Live, StepState::Attention) => t!("app_releases.status.phased_paused"),
        (StepKind::Testing, StepState::Pending) => t!("app_releases.status.sent"),
        (StepKind::Testing, _) => t!("app_releases.status.testing"),
        (StepKind::Production, StepState::Pending) => t!("app_releases.status.testing"),
        (StepKind::Production, StepState::Running) => t!("app_releases.status.rolling_out"),
        (StepKind::Production, StepState::Attention) => t!("app_releases.status.rollout_halted"),
        (_, StepState::Pending) => t!("app_releases.status.waiting"),
        (_, StepState::Failed) => t!("app_releases.status.failed"),
        _ => step_title(step.kind),
    };
    (label, state_tone(step.state))
}

/// What a person can do next, under the pill.
pub(crate) fn next_action(timeline: &PlatformTimeline) -> Option<SharedString> {
    let step = timeline.current_step()?;
    if timeline.is_live() {
        return None;
    }
    Some(match (step.kind, step.state) {
        (StepKind::Build, StepState::Running) => t!("app_releases.next.wait_build"),
        (StepKind::Build | StepKind::Submit, StepState::Failed) => t!("app_releases.next.see_logs"),
        (StepKind::Submit, StepState::Pending) => t!("app_releases.next.submit"),
        (StepKind::Processing, _) => t!("app_releases.next.wait_apple"),
        (StepKind::TestFlight, StepState::Pending) => t!("app_releases.next.add_group"),
        (StepKind::TestFlight, StepState::Attention) => t!("app_releases.next.check_testflight"),
        (StepKind::Review, StepState::Pending) => t!("app_releases.next.submit_review"),
        (StepKind::Review, StepState::Running) => t!("app_releases.next.wait_review"),
        (StepKind::Review, StepState::Attention) => t!("app_releases.next.resolution_center"),
        (StepKind::Live, StepState::Pending) => t!("app_releases.next.release"),
        (StepKind::Live, _) => t!("app_releases.next.follow_phased"),
        (StepKind::Testing, StepState::Pending) => t!("app_releases.next.wait_track"),
        (StepKind::Testing, _) | (StepKind::Production, StepState::Pending) => {
            t!("app_releases.next.promote")
        }
        (StepKind::Production, StepState::Running) => t!("app_releases.next.raise_rollout"),
        (StepKind::Production, StepState::Attention) => t!("app_releases.next.resume_rollout"),
        _ => return None,
    })
}

fn notice_text(kind: NoticeKind, version: &str, platform: &str) -> SharedString {
    match kind {
        NoticeKind::Rejected => t!(
            "app_releases.notice.rejected",
            version = version,
            platform = platform
        ),
        NoticeKind::Approved => t!(
            "app_releases.notice.approved",
            version = version,
            platform = platform
        ),
        NoticeKind::BuildFailed => t!(
            "app_releases.notice.build_failed",
            version = version,
            platform = platform
        ),
        NoticeKind::RolloutHalted => t!(
            "app_releases.notice.rollout_halted",
            version = version,
            platform = platform
        ),
    }
}

fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<ReleasesPanel>())
    })
}

/// The Lançamentos entry in the status bar's toolkit menu.
pub struct ReleasesToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ReleasesToolkitButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let dock_subscriptions = workspace
            .all_docks()
            .into_iter()
            .map(|dock| cx.observe(dock, |_, _, cx| cx.notify()))
            .collect();
        Self {
            workspace: workspace.weak_handle(),
            _dock_subscriptions: dock_subscriptions,
        }
    }
}

impl Render for ReleasesToolkitButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl StatusItemView for ReleasesToolkitButton {
    fn toolkit_entry(&self, cx: &App) -> Option<ToolkitEntry> {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            id: task_agents::toolkit_ids::RELEASES,
            is_open,
            action: Box::new(ToggleFocus),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<ReleasesPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            }),
        })
    }

    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }

    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| !toolkit_enabled(workspace.read(cx), cx))
    }
}

/// The most urgent release state of the project, in the status bar. Gone when everything is
/// live and nothing moves, and until Lançamentos is opened once for the project.
pub struct ReleasesStatusChip {
    workspace: WeakEntity<Workspace>,
    store: Option<Entity<ReleasesStore>>,
    _subscriptions: Vec<Subscription>,
}

impl ReleasesStatusChip {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let mut chip = Self {
            workspace: workspace.weak_handle(),
            store: None,
            _subscriptions: Vec::new(),
        };
        chip.attach(workspace.project().clone(), cx);
        if chip.store.is_none() {
            let project = workspace.project().clone();
            chip._subscriptions
                .push(
                    cx.observe_global::<store::ReleasesStores>(move |chip: &mut Self, cx| {
                        if chip.store.is_none() {
                            chip.attach(project.clone(), cx);
                        }
                    }),
                );
        }
        chip
    }

    fn attach(&mut self, project: Entity<project::Project>, cx: &mut Context<Self>) {
        let Some(store) = ReleasesStore::existing(&project, cx) else {
            return;
        };
        self._subscriptions
            .push(cx.observe(&store, |_, _, cx| cx.notify()));
        self._subscriptions
            .push(cx.subscribe(&store, |chip: &mut Self, store, event, cx| {
                chip.show_event(&store, event, cx);
            }));
        self.store = Some(store);
        cx.notify();
    }

    fn show_event(
        &mut self,
        store: &Entity<ReleasesStore>,
        event: &ReleasesEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let toast = match event {
            ReleasesEvent::Notice(notice) => {
                let text = notice_text(notice.kind, &notice.version, notice.platform.label());
                let toast = Toast::new(
                    NotificationId::unique::<ReleasesStatusChip>(),
                    text.to_string(),
                );
                let resolution_center_url = (notice.kind == NoticeKind::Rejected)
                    .then(|| {
                        store
                            .read(cx)
                            .apps()
                            .iter()
                            .find_map(|app| app.app_store_app_id.clone())
                    })
                    .flatten()
                    .map(|app_id| app_store_connect::resolution_center_url(&app_id));
                match resolution_center_url {
                    Some(url) => toast.on_click(
                        t!("app_releases.release.resolution_center").to_string(),
                        move |_window, cx| cx.open_url(&url),
                    ),
                    None => toast.autohide(),
                }
            }
            ReleasesEvent::TokenExpiring { days } => Toast::new(
                NotificationId::unique::<ReleasesStatusChip>(),
                t!("app_releases.notice.token_expiring", days = days).to_string(),
            )
            .on_click(
                t!("app_releases.accounts.open").to_string(),
                |window, cx| window.dispatch_action(Box::new(OpenAccounts), cx),
            ),
        };
        workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
    }
}

impl Render for ReleasesStatusChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(store) = self.store.as_ref() else {
            return div().into_any_element();
        };
        let store = store.read(cx);
        let urgent = store
            .all_releases()
            .filter_map(|(_, releases)| model::most_urgent(releases))
            .max_by_key(|(_, timeline)| timeline.urgency());
        let Some((release, timeline)) = urgent else {
            return div().into_any_element();
        };
        let (label, tone) = status_label(timeline);
        let text = format!(
            "{} · {} {}",
            release.version,
            timeline.platform.label(),
            label
        );
        let workspace = self.workspace.clone();
        h_flex()
            .id("releases-status-chip")
            .cursor_pointer()
            .child(status_pill(text, tone, cx))
            .tooltip(ui::Tooltip::text(t!("app_releases.chip.tooltip")))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| open(workspace, window, cx))
                    .log_err();
            })
            .into_any_element()
    }
}

impl StatusItemView for ReleasesStatusChip {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }

    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| !toolkit_enabled(workspace.read(cx), cx))
    }
}
