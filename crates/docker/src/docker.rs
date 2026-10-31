//! Docker in the dock: the project's containers grouped by compose project, their terminal and
//! logs in the center, a form for new containers, state markers in docker-compose.yml and the
//! agent's tools, all through the Engine API on the daemon's socket.

mod agent_toolkit;
mod client;
mod compose_markers;
mod discovery;
mod logs_view;
mod new_container;
mod panel;
mod store;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use client::{Container, ContainerState, Docker, DockerError};
pub use panel::DockerPanel;
pub use store::{ContainerAction, DockerStore};

use gpui::{Subscription, WeakEntity, actions};
use std::any::TypeId;
use std::rc::Rc;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Toast, ToolkitEntry, Workspace,
    notifications::NotificationId,
};

actions!(
    docker,
    [
        /// Opens the Docker dock, or hands focus back if it already has it.
        ToggleFocus,
        /// Opens the form for a new container.
        NewContainer,
        /// Creates and starts the container in the open form.
        CreateContainer,
        /// Reconnects to the daemon and reloads the containers.
        Refresh,
        /// Focuses the dock's filter.
        FocusFilter,
        /// Opens a shell inside the selected container.
        OpenTerminal,
        /// Follows the selected container's logs.
        OpenLogs,
        /// Shows the selected container's inspect output.
        Inspect,
        /// Pauses the selected container, or resumes it when paused.
        TogglePause,
        /// Restarts the selected container.
        Restart,
        /// Stops the selected container, or starts it when stopped.
        ToggleStop,
        /// Removes the selected container, after confirming.
        Remove,
        /// Pauses or resumes following the logs.
        ToggleFollow,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<DockerPanel>(cx);
    compose_markers::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewContainer, window, cx| {
            if !toolkit_allowed(workspace, cx) {
                return;
            }
            new_container::open(workspace, window, cx);
        });
    })
    .detach();
}

/// Whether the Docker toolkit is on for this workspace's project; says so when it isn't.
fn toolkit_allowed(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    if task_agents::toolkit_enabled(task_agents::toolkit_ids::DOCKER, workspace.project(), cx) {
        return true;
    }
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<DockerPanel>(),
            task_agents::toolkit_disabled_message("Docker"),
        )
        .autohide(),
        cx,
    );
    false
}

/// Adds the panel the first time it is asked for, so projects without Docker don't get a tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<DockerPanel>(cx).is_none() {
        let panel = cx.new(|cx| DockerPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<DockerPanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<DockerPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<DockerPanel>(window, cx);
}

pub(crate) fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<DockerPanel>())
    })
}

/// The Docker button in the status bar's toolkit group, lit while the dock is open.
pub struct DockerToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl DockerToolkitButton {
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

impl Render for DockerToolkitButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The status bar lists it in the toolkit menu instead; see `toolkit_entry`.
        gpui::Empty
    }
}

impl StatusItemView for DockerToolkitButton {
    fn toolkit_entry(&self, cx: &App) -> Option<ToolkitEntry> {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            icon: IconName::Docker,
            label: "Docker".into(),
            is_open,
            action: Box::new(ToggleFocus),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<DockerPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            }),
        })
    }

    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            !task_agents::toolkit_enabled(
                task_agents::toolkit_ids::DOCKER,
                workspace.read(cx).project(),
                cx,
            )
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
        // The panel has no status bar button of its own, so this is the only visible way in.
        None
    }
}

/// Human sizes as the boards write them: `182 MB`, `2,1 GB`.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit >= 3 {
        format!(
            "{} {}",
            format!("{value:.1}").replace('.', ","),
            UNITS[unit]
        )
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `0,4%`, `12%`.
pub(crate) fn format_percent(percent: f64) -> String {
    if percent < 10.0 {
        format!("{percent:.1}%").replace('.', ",")
    } else {
        format!("{percent:.0}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_boards() {
        assert_eq!(format_bytes(182_000_000), "182 MB");
        assert_eq!(format_bytes(2_100_000_000), "2,1 GB");
        assert_eq!(format_percent(0.42), "0,4%");
        assert_eq!(format_percent(23.4), "23%");
    }
}
