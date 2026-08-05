//! One-off developer utilities, reached from a single status bar menu rather than each
//! needing its own button, action and keybinding.

mod json_prettier;

pub use json_prettier::JsonPrettierPanel;

use gpui::{Action as _, actions};
use ui::{ContextMenu, ContextMenuEntry, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

actions!(
    toolbox,
    [
        /// Opens the JSON Prettier tool.
        OpenJsonPrettier,
    ]
);

pub fn init(cx: &mut App) {
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

/// The status bar button that lists the tools.
pub struct ToolboxButton {
    menu_handle: PopoverMenuHandle<ContextMenu>,
}

impl ToolboxButton {
    pub fn new() -> Self {
        Self {
            menu_handle: PopoverMenuHandle::default(),
        }
    }
}

impl Default for ToolboxButton {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for ToolboxButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new("toolbox-menu")
            .menu(|window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _window, _cx| {
                    menu.item(
                        ContextMenuEntry::new("JSON Prettier")
                            .icon(IconName::Json)
                            // `action` only supplies the keybinding shown on the right; the
                            // click itself goes through `handler`.
                            .action(OpenJsonPrettier.boxed_clone())
                            .handler(|window, cx| {
                                window.dispatch_action(OpenJsonPrettier.boxed_clone(), cx);
                            }),
                    )
                }))
            })
            .trigger_with_tooltip(
                IconButton::new("toolbox", IconName::ToolHammer)
                    .icon_size(IconSize::Small)
                    .shape(ui::IconButtonShape::Square),
                Tooltip::text("Tools"),
            )
            .anchor(gpui::Anchor::BottomRight)
            .with_handle(self.menu_handle.clone())
    }
}

impl StatusItemView for ToolboxButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // The tools stand alone; none of them act on the active item.
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The menu is the only way in to the tools, so there is nothing to fall back on
        // if it were hidden.
        None
    }
}
