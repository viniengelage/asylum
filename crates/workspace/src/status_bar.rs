use crate::{ItemHandle, MultiWorkspace, Pane, dock::PanelButtons};
use gpui::{
    AnyElement, AnyView, App, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Role, SharedString, Styled, Subscription, WeakEntity, Window,
};
use settings::{SettingsContent, update_settings_file};
use std::{any::TypeId, sync::Arc};
use ui::{ContextMenu, IconPosition, prelude::*, right_click_menu};

/// Describes how a status-bar item can be hidden by the user.
///
/// Every [`StatusItemView`] must either provide this (so that the user gets a
/// "Hide Button" entry in the right-click menu) or explicitly return `None`
/// to opt out. Returning `None` should be reserved for items that are
/// already conditional on some other setting exposed elsewhere (e.g., the
/// activity indicator, which disappears on its own once there's no work to
/// display).
#[derive(Clone)]
pub struct HideStatusItem {
    hide: Arc<dyn Fn(&mut SettingsContent) + Send + Sync>,
}

impl HideStatusItem {
    pub fn new(hide: impl Fn(&mut SettingsContent) + Send + Sync + 'static) -> Self {
        Self {
            hide: Arc::new(hide),
        }
    }

    /// Persists the hide by updating the user settings file.
    pub fn apply(&self, cx: &App) {
        let hide = self.hide.clone();
        let fs = <dyn fs::Fs>::global(cx);
        update_settings_file(fs, cx, move |settings, _cx| (hide)(settings));
    }
}

pub trait StatusItemView: Render {
    /// Event callback that is triggered when the active pane item changes.
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn crate::ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    );

    /// Returns metadata describing how this item can be hidden from the
    /// status bar by writing to the user settings file.
    ///
    /// Implementors that return `None` must be inherently conditional on
    /// another user-exposed setting; otherwise, they should return `Some` so
    /// that the status bar can show a "Hide Button" entry in its
    /// right-click menu.
    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem>;
}

trait StatusItemViewHandle: Send {
    fn to_any(&self) -> AnyView;
    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    );
    fn item_type(&self) -> TypeId;
    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem>;
}

/// The groups the status bar is divided into, in the order they appear from left to right.
/// Dividers between them are what let the user tell a dock toggle from a tool from a fact
/// about the editor without reading every icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusItemZone {
    /// Buttons that show and hide whole docks.
    Docks,
    /// Project state: diagnostics, git and language servers. Where left items go.
    Status,
    /// Facts about the active editor, such as the cursor position and language. Where right
    /// items go.
    EditorInfo,
    /// One button per developer tool, under a `TOOLKIT` label.
    Toolkit,
    /// Buttons for the panels beside the editor, such as Device, Agent and Git.
    Panels,
}

impl StatusItemZone {
    fn id(self) -> &'static str {
        match self {
            Self::Docks => "status-bar-docks",
            Self::Status => "status-bar-status",
            Self::EditorInfo => "status-bar-editor-info",
            Self::Toolkit => "status-bar-toolkit",
            Self::Panels => "status-bar-panels",
        }
    }
}

pub struct StatusBar {
    left_items: Vec<Box<dyn StatusItemViewHandle>>,
    right_items: Vec<Box<dyn StatusItemViewHandle>>,
    /// Items registered straight into a zone. They are not addressable by position, since
    /// positions index the left and right items.
    zoned_items: Vec<(StatusItemZone, Box<dyn StatusItemViewHandle>)>,
    active_pane: Entity<Pane>,
    focus_handle: FocusHandle,
    _observe_active_pane: Subscription,
}

impl Focusable for StatusBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("status-bar")
            .track_focus(&self.focus_handle)
            .key_context("StatusBar")
            // Expose the status bar as an ARIA toolbar so assistive technology
            // announces it as a toolbar and region navigation can reach its
            // controls. The controls inside form a tab group: region navigation
            // lands on the first control (per the ARIA toolbar pattern), Tab
            // steps through them, and arrow keys move between them once focus is
            // inside.
            .role(Role::Toolbar)
            .aria_label("Status bar")
            .tab_group()
            .on_key_down(
                cx.listener(|status_bar, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "right" => {
                            status_bar.move_item_focus(true, window, cx);
                            cx.stop_propagation();
                        }
                        "left" => {
                            status_bar.move_item_focus(false, window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }),
            )
            .justify_between()
            .gap(DynamicSpacing::Base08.rems(cx))
            .px(DynamicSpacing::Base08.rems(cx))
            .py(DynamicSpacing::Base04.rems(cx))
            .workspace_card(cx)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_left_tools(cx))
            .child(self.render_right_tools(cx))
    }
}

impl StatusBar {
    fn render_left_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The left side reads as one strip, so its divider takes the zones' own gap.
        h_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .min_w_0()
            .overflow_x_hidden()
            .children(self.render_zones(&[StatusItemZone::Docks, StatusItemZone::Status], cx))
    }

    fn render_right_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_shrink_0()
            .gap(DynamicSpacing::Base08.rems(cx))
            .overflow_x_hidden()
            .children(self.render_zones(
                &[
                    StatusItemZone::EditorInfo,
                    StatusItemZone::Toolkit,
                    StatusItemZone::Panels,
                ],
                cx,
            ))
    }

    /// Renders each zone that has items, with a divider between neighbours.
    fn render_zones(&self, zones: &[StatusItemZone], cx: &App) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for &zone in zones {
            let items = self.items_in_zone(zone, cx);
            if items.is_empty() {
                continue;
            }
            if !elements.is_empty() {
                elements.push(render_zone_divider(cx));
            }
            elements.push(render_zone(zone, &items, cx));
        }
        elements
    }

    /// The items shown in `zone`, in the order they are rendered.
    fn items_in_zone(&self, zone: StatusItemZone, cx: &App) -> Vec<&dyn StatusItemViewHandle> {
        let mut items: Vec<&dyn StatusItemViewHandle> = self
            .zoned_items
            .iter()
            .filter(|(item_zone, _)| *item_zone == zone)
            .map(|(_, item)| item.as_ref())
            .collect();
        items.extend(
            self.left_items
                .iter()
                .map(|item| item.as_ref())
                .filter(|item| zone_of(*item, StatusItemZone::Status, cx) == zone),
        );
        let right_items = self
            .right_items
            .iter()
            .map(|item| item.as_ref())
            .filter(|item| zone_of(*item, StatusItemZone::EditorInfo, cx) == zone);
        if zone == StatusItemZone::EditorInfo {
            // Editor info keeps the reversed order right items have always had, so the
            // first one added sits at its right end.
            items.extend(right_items.rev());
        } else {
            items.extend(right_items);
        }
        items
    }
}

/// The zone an item added to the left or right lands in. Dock panel buttons are registered
/// as plain left and right items but belong with the zone that matches their dock.
fn zone_of(item: &dyn StatusItemViewHandle, side_zone: StatusItemZone, cx: &App) -> StatusItemZone {
    item.to_any()
        .downcast::<PanelButtons>()
        .map(|panel_buttons| panel_buttons.read(cx).status_bar_zone(cx))
        .unwrap_or(side_zone)
}

fn render_zone(zone: StatusItemZone, items: &[&dyn StatusItemViewHandle], cx: &App) -> AnyElement {
    h_flex()
        .id(zone.id())
        // Only the status zone gives way when the bar is too narrow; the rest are buttons
        // that would be clipped mid-icon.
        .map(|this| {
            if zone == StatusItemZone::Status {
                this.min_w_0().overflow_x_hidden()
            } else {
                this.flex_none()
            }
        })
        .h(DynamicSpacing::Base24.rems(cx))
        .gap(DynamicSpacing::Base04.rems(cx))
        .when(zone == StatusItemZone::Toolkit, |this| {
            this.child(
                Label::new("TOOLKIT")
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Placeholder)
                    .mr(DynamicSpacing::Base04.rems(cx)),
            )
        })
        .children(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| render_hideable_item(zone.id(), index, *item, cx)),
        )
        .into_any_element()
}

fn render_zone_divider(cx: &App) -> AnyElement {
    div()
        .flex_none()
        .w_px()
        .h(DynamicSpacing::Base16.rems(cx))
        .bg(cx.theme().colors().border_selected)
        .into_any_element()
}

fn render_hideable_item(
    group_id: &'static str,
    index: usize,
    item: &dyn StatusItemViewHandle,
    cx: &App,
) -> impl IntoElement {
    let view = item.to_any();
    let Some(hide) = item.hide_setting(cx) else {
        return view.into_any_element();
    };

    let menu_id: SharedString = format!("{group_id}-item-menu-{index}").into();
    right_click_menu(menu_id)
        .trigger(move |_is_active, _window, _cx| view)
        .menu(move |window, cx| {
            let hide = hide.clone();
            ContextMenu::build(window, cx, move |menu, _window, _cx| {
                add_hide_button_entry(menu, hide)
            })
        })
        .into_any_element()
}

/// Appends a "Hide Button" entry aligned with surrounding toggleable entries.
pub fn add_hide_button_entry(menu: ContextMenu, hide: HideStatusItem) -> ContextMenu {
    menu.toggleable_entry(
        "Hide Button",
        false,
        IconPosition::Start,
        None,
        move |_window, cx| hide.apply(cx),
    )
}

impl StatusBar {
    pub fn new(
        active_pane: &Entity<Pane>,
        _multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            left_items: Default::default(),
            right_items: Default::default(),
            zoned_items: Default::default(),
            active_pane: active_pane.clone(),
            focus_handle: cx.focus_handle(),
            _observe_active_pane: cx.observe_in(active_pane, window, |this, _, window, cx| {
                this.update_active_pane_item(window, cx)
            }),
        };
        this.update_active_pane_item(window, cx);
        this
    }

    pub fn set_multi_workspace(
        &mut self,
        _multi_workspace: WeakEntity<MultiWorkspace>,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    pub fn add_left_item<T>(&mut self, item: Entity<T>, window: &mut Window, cx: &mut Context<Self>)
    where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        self.left_items.push(Box::new(item));
        cx.notify();
    }

    /// Adds an item to the left edge, with the buttons that show and hide whole docks.
    pub fn add_dock_item<T>(&mut self, item: Entity<T>, window: &mut Window, cx: &mut Context<Self>)
    where
        T: 'static + StatusItemView,
    {
        self.add_zoned_item(StatusItemZone::Docks, item, window, cx);
    }

    /// Adds a developer tool's button to the toolkit group on the right.
    pub fn add_toolkit_item<T>(
        &mut self,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        self.add_zoned_item(StatusItemZone::Toolkit, item, window, cx);
    }

    fn add_zoned_item<T>(
        &mut self,
        zone: StatusItemZone,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        self.zoned_items.push((zone, Box::new(item)));
        cx.notify();
    }

    pub fn item_of_type<T: StatusItemView>(&self) -> Option<Entity<T>> {
        self.left_items
            .iter()
            .chain(self.right_items.iter())
            .chain(self.zoned_items.iter().map(|(_, item)| item))
            .find_map(|item| item.to_any().downcast().ok())
    }

    pub fn position_of_item<T>(&self) -> Option<usize>
    where
        T: StatusItemView,
    {
        for (index, item) in self.left_items.iter().enumerate() {
            if item.item_type() == TypeId::of::<T>() {
                return Some(index);
            }
        }
        for (index, item) in self.right_items.iter().enumerate() {
            if item.item_type() == TypeId::of::<T>() {
                return Some(index + self.left_items.len());
            }
        }
        None
    }

    pub fn insert_item_after<T>(
        &mut self,
        position: usize,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        if position < self.left_items.len() {
            self.left_items.insert(position + 1, Box::new(item))
        } else {
            self.right_items
                .insert(position + 1 - self.left_items.len(), Box::new(item))
        }
        cx.notify()
    }

    pub fn remove_item_at(&mut self, position: usize, cx: &mut Context<Self>) {
        if position < self.left_items.len() {
            self.left_items.remove(position);
        } else {
            self.right_items.remove(position - self.left_items.len());
        }
        cx.notify();
    }

    pub fn add_right_item<T>(
        &mut self,
        item: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: 'static + StatusItemView,
    {
        let active_pane_item = self.active_pane.read(cx).active_item();
        item.set_active_pane_item(active_pane_item.as_deref(), window, cx);

        self.right_items.push(Box::new(item));
        cx.notify();
    }

    pub fn set_active_pane(
        &mut self,
        active_pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane = active_pane.clone();
        self._observe_active_pane = cx.observe_in(active_pane, window, |this, _, window, cx| {
            this.update_active_pane_item(window, cx)
        });
        self.update_active_pane_item(window, cx);
    }

    fn update_active_pane_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active_pane_item = self.active_pane.read(cx).active_item();
        for item in self
            .left_items
            .iter()
            .chain(&self.right_items)
            .chain(self.zoned_items.iter().map(|(_, item)| item))
        {
            item.set_active_pane_item(active_pane_item.as_deref(), window, cx);
        }
    }

    /// Moves focus between the interactive controls within the status bar in
    /// response to arrow keys. Navigation is clamped to the status bar so
    /// arrows move between items and stop at the ends (ARIA toolbar semantics);
    /// Tab is still used to leave the toolbar.
    fn move_item_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let previous = window.focused(cx);
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        let landed_in_status_bar = window
            .focused(cx)
            .is_some_and(|handle| self.focus_handle.contains(&handle, window));
        if !landed_in_status_bar && let Some(previous) = previous {
            window.focus(&previous, cx);
        }
        cx.notify();
    }
}

impl<T: StatusItemView> StatusItemViewHandle for Entity<T> {
    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.update(cx, |this, cx| {
            this.set_active_pane_item(active_pane_item, window, cx)
        });
    }

    fn item_type(&self) -> TypeId {
        TypeId::of::<T>()
    }

    fn hide_setting(&self, cx: &App) -> Option<HideStatusItem> {
        self.read(cx).hide_setting(cx)
    }
}

impl From<&dyn StatusItemViewHandle> for AnyView {
    fn from(val: &dyn StatusItemViewHandle) -> Self {
        val.to_any()
    }
}
