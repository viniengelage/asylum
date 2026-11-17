//! Running the project: the run picker and run zone in the status bar, the Executions dock
//! with what runs and what ran, and the sources the picker offers (tasks.json, package.json
//! scripts, eas.json profiles, debug scenarios).

mod launch;
mod panel;
mod picker;
mod sources;
mod store;

pub use panel::ExecutionsPanel;
pub use picker::RunPickerView;
pub use sources::{RunCandidate, RunKind, RunOrigin};
pub use store::{RunRecord, RunState, RunStore};

use std::time::Duration;

use gpui::{
    Action, App, AppContext as _, Context, Entity, Subscription, WeakEntity, Window, actions,
};
use project::debugger::session::ThreadStatus;
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{
    ButtonLike, PopoverMenu, PopoverMenuHandle, StatusTone, Tooltip, prelude::*, status_pill,
};
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Toast, Workspace, notifications::NotificationId,
};

actions!(
    runs,
    [
        /// Opens the Executions dock, or hands focus back if it already has it.
        ToggleFocus,
        /// Opens the run picker in the status bar.
        OpenPicker,
        /// Runs the active run (the last one picked).
        RunActive,
        /// Debugs the active run.
        DebugActive,
        /// Stops what is running: the debug session or the runs started from the picker.
        StopActive,
        /// Adds the selected run to the favorites, or takes it out.
        ToggleFavorite,
    ]
);

/// Runs one of the favorites, by its slot (1 to 9).
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = runs)]
#[serde(deny_unknown_fields)]
pub struct RunFavorite {
    pub slot: usize,
}

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<ExecutionsPanel>(cx);
    workspace::register_lazy_panel::<ExecutionsPanel>(
        |workspace, window, cx| {
            if workspace.panel::<ExecutionsPanel>(cx).is_some() {
                return;
            }
            add_panel(workspace, window, cx);
        },
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            if workspace.panel::<ExecutionsPanel>(cx).is_none() {
                add_panel(workspace, window, cx);
                workspace.focus_panel::<ExecutionsPanel>(window, cx);
                return;
            }
            workspace.toggle_panel_focus::<ExecutionsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RunActive, window, cx| {
            run_active(workspace, false, window, cx);
        });
        workspace.register_action(|workspace, _: &DebugActive, window, cx| {
            run_active(workspace, true, window, cx);
        });
        workspace.register_action(|workspace, _: &StopActive, window, cx| {
            stop_active(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &RunFavorite, window, cx| {
            let Some(store) = store_of(workspace, cx) else {
                return;
            };
            let key = store
                .read(cx)
                .favorites()
                .get(action.slot.saturating_sub(1))
                .cloned();
            match key {
                Some(key) => store.update(cx, |store, cx| store.run_key(key, false, window, cx)),
                None => show_toast(
                    workspace,
                    i18n::t!("runs.favorites.empty_slot", slot = action.slot).to_string(),
                    cx,
                ),
            }
        });
    })
    .detach();
}

fn store_of(workspace: &Workspace, cx: &mut App) -> Option<Entity<RunStore>> {
    let workspace = workspace.weak_handle().upgrade()?;
    Some(RunStore::for_workspace(&workspace, cx))
}

fn add_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(store) = store_of(workspace, cx) else {
        return;
    };
    let handle = workspace.weak_handle();
    let panel = cx.new(|cx| ExecutionsPanel::new(handle, store, cx));
    workspace.add_panel(panel, window, cx);
}

fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    struct RunsToast;
    workspace.show_toast(
        Toast::new(NotificationId::unique::<RunsToast>(), message).autohide(),
        cx,
    );
}

fn run_active(
    workspace: &mut Workspace,
    debug: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !debug
        && workspace
            .debugger_provider()
            .and_then(|provider| provider.active_thread_state(cx))
            == Some(ThreadStatus::Stopped)
    {
        launch::dispatch_by_name("debugger::Continue", window, cx);
        return;
    }
    let Some(store) = store_of(workspace, cx) else {
        return;
    };
    let active = store.read(cx).active_key().cloned();
    match active {
        Some(key) => store.update(cx, |store, cx| store.run_key(key, debug, window, cx)),
        None => window.dispatch_action(OpenPicker.boxed_clone(), cx),
    }
}

fn stop_active(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let debugging = workspace
        .debugger_provider()
        .and_then(|provider| provider.active_thread_state(cx))
        .is_some_and(|status| status != ThreadStatus::Exited && status != ThreadStatus::Ended);
    if debugging {
        launch::dispatch_by_name("debugger::Stop", window, cx);
    }
    let Some(store) = store_of(workspace, cx) else {
        return;
    };
    let stopped = store.update(cx, |store, cx| store.stop_all(cx));
    if stopped == 0 && !debugging {
        show_toast(workspace, i18n::t!("runs.nothing_running").to_string(), cx);
    }
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// The boards' result pill for a run: ok, failed · exit N, stopped by you, reused.
pub fn record_pill(record: &RunRecord, cx: &App) -> Div {
    let (label, tone) = match record.state {
        RunState::Running => (i18n::t!("runs.state.running"), StatusTone::Info),
        RunState::Succeeded => (i18n::t!("runs.state.ok"), StatusTone::Success),
        RunState::Failed {
            exit_code: Some(code),
        } => (
            i18n::t!("runs.state.failed_exit", code = code),
            StatusTone::Error,
        ),
        RunState::Failed { exit_code: None } => (i18n::t!("runs.state.failed"), StatusTone::Error),
        RunState::StoppedByUser => (i18n::t!("runs.state.stopped"), StatusTone::Neutral),
        RunState::Reused => (i18n::t!("runs.state.reused"), StatusTone::Warning),
    };
    status_pill(label, tone, cx)
}

/// The run zone of the status bar: play, the active run and device (opening the picker),
/// debug, and the Executions dock.
pub struct RunStatusItem {
    workspace: WeakEntity<Workspace>,
    store: Entity<RunStore>,
    picker_handle: PopoverMenuHandle<RunPickerView>,
    _subscriptions: Vec<Subscription>,
}

impl RunStatusItem {
    fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<RunStore>,
        picker_handle: PopoverMenuHandle<RunPickerView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            workspace,
            store,
            picker_handle,
            _subscriptions: subscriptions,
        }
    }
}

/// Adds the run zone to the workspace's status bar and binds the picker action to it.
pub fn add_status_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = store_of(workspace, cx) else {
        return;
    };
    let picker_handle = PopoverMenuHandle::default();
    let handle = workspace.weak_handle();
    let item = cx.new({
        let picker_handle = picker_handle.clone();
        |cx| RunStatusItem::new(handle, store, picker_handle, cx)
    });
    workspace.register_action(move |_, _: &OpenPicker, window, cx| {
        picker_handle.toggle(window, cx);
    });
    workspace.status_bar().update(cx, |status_bar, cx| {
        status_bar.add_run_item(item, window, cx);
    });
}

impl Render for RunStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let running_count = store.running().count();
        let active_label = store
            .active()
            .map(|candidate| candidate.label.clone())
            .or_else(|| {
                store.active_key().and_then(|key| {
                    key.split_once(':')
                        .map(|(_, label)| label.to_string().into())
                })
            })
            .unwrap_or_else(|| i18n::t!("runs.status.pick"));
        let device = task_agents::active_device(cx).map(|device| device.name);
        let label = match device {
            Some(device) => format!("{active_label} · {device}"),
            None => active_label.to_string(),
        };
        let workspace = self.workspace.clone();
        h_flex()
            .gap_0p5()
            .child(
                IconButton::new("run-active", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Success)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(i18n::t!("runs.status.run"), &RunActive, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(RunActive.boxed_clone(), cx);
                    }),
            )
            .child(
                PopoverMenu::new("run-picker")
                    .anchor(gpui::Anchor::BottomLeft)
                    .with_handle(self.picker_handle.clone())
                    .menu(move |window, cx| {
                        let workspace = workspace.upgrade()?;
                        Some(cx.new(|cx| RunPickerView::new(&workspace, window, cx)))
                    })
                    .trigger_with_tooltip(
                        ButtonLike::new("run-picker-trigger").child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(label)
                                        .size(LabelSize::Small)
                                        .single_line()
                                        .truncate(),
                                )
                                .when(running_count > 0, |this| {
                                    this.child(status_pill(
                                        running_count.to_string(),
                                        StatusTone::Info,
                                        cx,
                                    ))
                                })
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                        |_window, cx| {
                            Tooltip::for_action(i18n::t!("runs.status.picker"), &OpenPicker, cx)
                        },
                    ),
            )
            .child(
                IconButton::new("run-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(i18n::t!("runs.status.debug"), &DebugActive, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(DebugActive.boxed_clone(), cx);
                    }),
            )
            .child(
                IconButton::new("run-executions", IconName::ListTree)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(i18n::t!("runs.status.executions"), &ToggleFocus, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(ToggleFocus.boxed_clone(), cx);
                    }),
            )
    }
}

impl StatusItemView for RunStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The run zone is the only visible way to the picker and the Executions dock.
        None
    }
}
