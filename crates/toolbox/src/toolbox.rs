//! One-off developer utilities. Each tool has its own button in the status bar's toolkit
//! group, lit while the tool is open.

mod json_prettier;

pub use json_prettier::JsonPrettierPanel;

use gpui::{Subscription, WeakEntity, actions};
use std::any::TypeId;
use std::rc::Rc;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, ToolkitEntry, Workspace, dock::Panel};

actions!(
    toolbox,
    [
        /// Opens the JSON Prettier tool.
        OpenJsonPrettier,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_lazy_panel::<JsonPrettierPanel>(json_prettier::restore_panel, cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenJsonPrettier, window, cx| {
            json_prettier::open(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &json_prettier::ToggleFocus, window, cx| {
            json_prettier::toggle_focus(workspace, window, cx);
        });
    })
    .detach();
}

/// Whether a dock is currently showing the panel of type `T`.
fn panel_is_visible<T: Panel>(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<T>())
    })
}

/// The status bar buttons for the tools.
pub struct ToolboxButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ToolboxButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        // The tools open as dock panels, so a dock changing is what lights or dims a button.
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

impl Render for ToolboxButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The status bar lists it in the toolkit menu instead; see `toolkit_entry`.
        gpui::Empty
    }
}

impl StatusItemView for ToolboxButton {
    fn toolkit_entry(&self, cx: &App) -> Option<ToolkitEntry> {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible::<JsonPrettierPanel>(workspace.read(cx), cx));
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            icon: IconName::Json,
            label: "JSON Prettier".into(),
            is_open,
            action: Box::new(OpenJsonPrettier),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<JsonPrettierPanel>(window, cx);
                        } else {
                            json_prettier::open(workspace, window, cx);
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
        // The tools stand alone; none of them act on the active item.
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // These buttons are the only visible way in to the tools, so there is nothing to
        // fall back on if they were hidden.
        None
    }
}
