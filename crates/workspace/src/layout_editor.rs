use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Styled, WeakEntity,
    Window, actions, div, px,
};
use ui::{h_flex, prelude::*, v_flex, Color, IconButton, IconName, IconSize, Label, Tooltip};

use crate::{
    PaneGroup, StatusItemView, Workspace,
    item::ItemHandle,
    pane_group::{Member, PaneAxis},
};

actions!(
    layout_editor,
    [
        ToggleLayoutEditor,
        ApplyLayout,
    ]
);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ZoneKind {
    Editor,
    ProjectPanel,
    Terminal,
    Agent,
    Devices,
    WebPreview,
    StatusBar,
    Custom(String),
}

impl ZoneKind {
    pub fn display_name(&self) -> &str {
        match self {
            ZoneKind::Editor => "Editor",
            ZoneKind::ProjectPanel => "Files",
            ZoneKind::Terminal => "Terminal",
            ZoneKind::Agent => "Agent",
            ZoneKind::Devices => "Devices",
            ZoneKind::WebPreview => "Browser",
            ZoneKind::StatusBar => "Status Bar",
            ZoneKind::Custom(name) => name.as_str(),
        }
    }

    pub fn icon(&self) -> IconName {
        match self {
            ZoneKind::Editor => IconName::Code,
            ZoneKind::ProjectPanel => IconName::FileTree,
            ZoneKind::Terminal => IconName::Terminal,
            ZoneKind::Agent => IconName::ZedAgent,
            ZoneKind::Devices => IconName::Screen,
            ZoneKind::WebPreview => IconName::ToolWeb,
            ZoneKind::StatusBar => IconName::Menu,
            ZoneKind::Custom(_) => IconName::SquareDot,
        }
    }

    pub fn all_available() -> Vec<ZoneKind> {
        vec![
            ZoneKind::Editor,
            ZoneKind::ProjectPanel,
            ZoneKind::Terminal,
            ZoneKind::Agent,
            ZoneKind::Devices,
            ZoneKind::WebPreview,
            ZoneKind::StatusBar,
        ]
    }

    pub fn is_required(&self) -> bool {
        matches!(
            self,
            ZoneKind::Editor | ZoneKind::ProjectPanel | ZoneKind::Terminal | ZoneKind::StatusBar
        )
    }

    pub fn has_fixed_height(&self) -> bool {
        matches!(self, ZoneKind::StatusBar)
    }
}

#[derive(Clone)]
pub struct GridCell {
    pub zone: Option<ZoneKind>,
}

impl Default for GridCell {
    fn default() -> Self {
        Self { zone: None }
    }
}

pub struct LayoutEditorState {
    pub active: bool,
    pub columns: Vec<f32>,
    pub row_splits: Vec<Vec<f32>>,
    pub cells: Vec<Vec<GridCell>>,
    pub picking_zone_for: Option<(usize, usize)>,
}

impl Default for LayoutEditorState {
    fn default() -> Self {
        let columns = vec![1.0, 2.0, 1.0];
        let row_splits = vec![
            vec![1.0, 0.0],          // col 0: Files + StatusBar (fixed height)
            vec![2.0, 1.0],          // col 1: Editor + Terminal
            vec![1.0],               // col 2: Agent
        ];
        let cells = vec![
            vec![
                GridCell { zone: Some(ZoneKind::ProjectPanel) },
                GridCell { zone: Some(ZoneKind::StatusBar) },
            ],
            vec![
                GridCell { zone: Some(ZoneKind::Editor) },
                GridCell { zone: Some(ZoneKind::Terminal) },
            ],
            vec![
                GridCell { zone: Some(ZoneKind::Agent) },
            ],
        ];
        Self {
            active: false,
            columns,
            row_splits,
            cells,
            picking_zone_for: None,
        }
    }
}

impl LayoutEditorState {
    pub fn toggle(&mut self) {
        self.active = !self.active;
        self.picking_zone_for = None;
    }

    pub fn add_column(&mut self) {
        if self.columns.len() < 6 {
            self.columns.push(1.0);
            self.row_splits.push(vec![1.0]);
            self.cells.push(vec![GridCell::default()]);
        }
    }

    pub fn remove_column(&mut self) {
        if self.columns.len() > 1 {
            self.columns.pop();
            self.row_splits.pop();
            self.cells.pop();
        }
    }

    pub fn add_row(&mut self, col: usize) {
        if let Some(rows) = self.row_splits.get_mut(col) {
            if rows.len() < 4 {
                rows.push(1.0);
                if let Some(col_cells) = self.cells.get_mut(col) {
                    col_cells.push(GridCell::default());
                }
            }
        }
    }

    pub fn remove_row(&mut self, col: usize) {
        if let Some(rows) = self.row_splits.get_mut(col) {
            if rows.len() > 1 {
                rows.pop();
                if let Some(col_cells) = self.cells.get_mut(col) {
                    col_cells.pop();
                }
            }
        }
    }

    pub fn set_zone(&mut self, col: usize, row: usize, zone: ZoneKind) {
        // Remove this zone from any other cell
        for col_cells in &mut self.cells {
            for cell in col_cells {
                if cell.zone.as_ref() == Some(&zone) {
                    cell.zone = None;
                }
            }
        }
        if let Some(col_cells) = self.cells.get_mut(col) {
            if let Some(cell) = col_cells.get_mut(row) {
                cell.zone = Some(zone);
            }
        }
        self.picking_zone_for = None;
    }

    pub fn clear_zone(&mut self, col: usize, row: usize) {
        if let Some(col_cells) = self.cells.get_mut(col) {
            if let Some(cell) = col_cells.get_mut(row) {
                cell.zone = None;
            }
        }
    }

    pub fn can_apply(&self) -> bool {
        ZoneKind::all_available()
            .iter()
            .filter(|z| z.is_required())
            .all(|z| self.zone_is_placed(z))
    }

    pub fn zone_is_placed(&self, zone: &ZoneKind) -> bool {
        self.cells.iter().flatten().any(|c| c.zone.as_ref() == Some(zone))
    }
}

pub fn apply_layout(
    columns: &[f32],
    row_splits: &[Vec<f32>],
    cells: &[Vec<GridCell>],
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let workspace_width = workspace.bounds.size.width;
    let total_flex: f32 = columns.iter().sum();
    let width_f32: f32 = workspace_width.into();
    if total_flex <= 0.0 || width_f32 <= 0.0 {
        return;
    }

    // 1. Close all docks first
    workspace.left_dock().update(cx, |dock, cx| { dock.set_open(false, window, cx); });
    workspace.right_dock().update(cx, |dock, cx| { dock.set_open(false, window, cx); });
    workspace.bottom_dock().update(cx, |dock, cx| { dock.set_open(false, window, cx); });
    workspace.devices_dock().update(cx, |dock, cx| { dock.set_open(false, window, cx); });

    // 2. Walk the grid — open docks for dock-zones, skip center-only zones
    for (col, col_flex) in columns.iter().enumerate() {
        let col_cells = cells.get(col).cloned().unwrap_or_default();

        for cell in &col_cells {
            let dock_size = gpui::px((*col_flex / total_flex) * width_f32);

            match &cell.zone {
                Some(ZoneKind::ProjectPanel) => {
                    workspace.left_dock().update(cx, |dock, cx| { dock.set_open(true, window, cx); });
                    workspace.resize_dock(crate::DockPosition::Left, dock_size, window, cx);
                }
                Some(ZoneKind::Agent) => {
                    workspace.right_dock().update(cx, |dock, cx| { dock.set_open(true, window, cx); });
                    workspace.resize_dock(crate::DockPosition::Right, dock_size, window, cx);
                }
                Some(ZoneKind::Devices) => {
                    workspace.devices_dock().update(cx, |dock, cx| { dock.set_open(true, window, cx); });
                    workspace.resize_dock(crate::DockPosition::Devices, dock_size, window, cx);
                }
                Some(ZoneKind::Terminal) => {
                    workspace.bottom_dock().update(cx, |dock, cx| { dock.set_open(true, window, cx); });
                }
                _ => {}
            }
        }
    }

    // 3. The center pane (editor) just stays as-is — it's the existing active pane.
    //    No need to create new panes or rebuild the center tree.
    //    The docks open/close + resize is what controls the layout.

    cx.notify();
}

// Status bar button
pub struct LayoutEditorButton {
    editor_state: Entity<LayoutEditorState>,
}

impl LayoutEditorButton {
    pub fn new(editor_state: Entity<LayoutEditorState>) -> Self {
        Self { editor_state }
    }
}

impl Render for LayoutEditorButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_active = self.editor_state.read(cx).active;
        IconButton::new("layout-editor-button", IconName::Split)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Layout Editor"))
            .toggle_state(is_active)
            .on_click({
                let state = self.editor_state.clone();
                move |_, _, cx| {
                    state.update(cx, |s, cx| { s.toggle(); cx.notify(); });
                }
            })
    }
}

impl StatusItemView for LayoutEditorButton {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}
    fn hide_setting(&self, _: &App) -> Option<crate::HideStatusItem> { None }
}

// Overlay
pub struct LayoutEditorOverlay {
    state: Entity<LayoutEditorState>,
    workspace: Option<WeakEntity<Workspace>>,
    focus_handle: FocusHandle,
}

impl LayoutEditorOverlay {
    pub fn new(state: Entity<LayoutEditorState>, cx: &mut Context<Self>) -> Self {
        Self { state, workspace: None, focus_handle: cx.focus_handle() }
    }

    pub fn set_workspace(&mut self, workspace: WeakEntity<Workspace>) {
        self.workspace = Some(workspace);
    }
}

impl Focusable for LayoutEditorOverlay {
    fn focus_handle(&self, _: &App) -> FocusHandle { self.focus_handle.clone() }
}

impl Render for LayoutEditorOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        if !state.active {
            return div().into_any_element();
        }

        let num_cols = state.columns.len();
        let columns = state.columns.clone();
        let row_splits = state.row_splits.clone();
        let cells = state.cells.clone();
        let can_apply = state.can_apply();
        let picking_for = state.picking_zone_for;

        let border_color = cx.theme().colors().border;
        let drop_bg = cx.theme().colors().drop_target_background;
        let drop_border = cx.theme().colors().drop_target_border;

        // Toolbar
        let toolbar = h_flex()
            .w_full()
            .h(px(44.0))
            .px_4()
            .gap_3()
            .items_center()
            .justify_between()
            .bg(cx.theme().colors().title_bar_background)
            .border_b_1()
            .border_color(border_color)
            .child(
                h_flex().gap_2().items_center()
                    .child(Label::new("Layout Editor").size(ui::LabelSize::Default))
            )
            .child(
                h_flex().gap_1().items_center()
                    .child(IconButton::new("rm-col", IconName::Dash).icon_size(IconSize::Small)
                        .disabled(num_cols <= 1)
                        .on_click({ let s = self.state.clone(); move |_,_,cx| { s.update(cx, |s,cx| { s.remove_column(); cx.notify(); }); }}))
                    .child(Label::new(format!("{num_cols} cols")).size(ui::LabelSize::XSmall).color(Color::Muted))
                    .child(IconButton::new("add-col", IconName::Plus).icon_size(IconSize::Small)
                        .disabled(num_cols >= 6)
                        .on_click({ let s = self.state.clone(); move |_,_,cx| { s.update(cx, |s,cx| { s.add_column(); cx.notify(); }); }}))
                    .child(div().w(px(16.0)))
                    .child(
                        ui::Button::new("apply", "Apply Layout")
                            .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                            .disabled(!can_apply)
                            .on_click({
                                let state = self.state.clone();
                                let ws = self.workspace.clone();
                                move |_, window, cx| {
                                    if let Some(ws) = ws.as_ref() {
                                        let s = state.read(cx);
                                        let cols = s.columns.clone();
                                        let rows = s.row_splits.clone();
                                        let cells_clone = s.cells.clone();
                                        ws.update(cx, |workspace, cx| {
                                            apply_layout(&cols, &rows, &cells_clone, workspace, window, cx);
                                        }).ok();
                                    }
                                    state.update(cx, |s, cx| { s.active = false; cx.notify(); });
                                }
                            }),
                    )
                    .child(div().w(px(8.0)))
                    .child(IconButton::new("close", IconName::Close).icon_size(IconSize::Small)
                        .on_click({ let s = self.state.clone(); move |_,_,cx| { s.update(cx, |s,cx| { s.toggle(); cx.notify(); }); }}))
            );

        // Grid
        let mut column_elements: Vec<gpui::AnyElement> = Vec::new();
        for col in 0..num_cols {
            let rows = row_splits.get(col).cloned().unwrap_or_else(|| vec![1.0]);
            let num_rows = rows.len();
            let col_cells = cells.get(col).cloned().unwrap_or_default();

            let col_header = h_flex()
                .w_full().h(px(28.0)).px_1().gap_1().items_center().justify_center()
                .child(IconButton::new(SharedString::from(format!("rmr-{col}")), IconName::Dash)
                    .icon_size(IconSize::XSmall).disabled(num_rows <= 1)
                    .on_click({ let s = self.state.clone(); move |_,_,cx| { s.update(cx, |s,cx| { s.remove_row(col); cx.notify(); }); }}))
                .child(Label::new(format!("{num_rows}R")).size(ui::LabelSize::XSmall).color(Color::Muted))
                .child(IconButton::new(SharedString::from(format!("addr-{col}")), IconName::Plus)
                    .icon_size(IconSize::XSmall).disabled(num_rows >= 4)
                    .on_click({ let s = self.state.clone(); move |_,_,cx| { s.update(cx, |s,cx| { s.add_row(col); cx.notify(); }); }}));

            let mut row_elements: Vec<gpui::AnyElement> = Vec::new();
            for row in 0..num_rows {
                let cell = col_cells.get(row).cloned().unwrap_or_default();
                let has_zone = cell.zone.is_some();
                let is_picking = picking_for == Some((col, row));

                let cell_content = if is_picking {
                    // Show zone picker
                    let mut picker_items: Vec<gpui::AnyElement> = Vec::new();
                    for zone in ZoneKind::all_available() {
                        let already_placed = state.zone_is_placed(&zone);
                        let zone_clone = zone.clone();
                        picker_items.push(
                            div()
                                .id(SharedString::from(format!("pick-{col}-{row}-{}", zone.display_name())))
                                .px_2().py_1().rounded_md().cursor_pointer()
                                .hover(|s| s.bg(drop_bg))
                                .when(already_placed, |this| this.opacity(0.6))
                                .child(
                                    h_flex().gap_2().items_center()
                                        .child(ui::Icon::new(zone.icon()).size(IconSize::Small).color(
                                            if zone.is_required() && !already_placed { Color::Warning } else { Color::Muted }
                                        ))
                                        .child(Label::new(zone.display_name()).size(ui::LabelSize::Small))
                                        .when(zone.is_required() && !already_placed, |this| {
                                            this.child(Label::new("required").size(ui::LabelSize::XSmall).color(Color::Warning))
                                        })
                                        .when(already_placed, |this| {
                                            this.child(Label::new("(move)").size(ui::LabelSize::XSmall).color(Color::Muted))
                                        })
                                )
                                .on_mouse_down(MouseButton::Left, {
                                    let s = self.state.clone();
                                    move |_: &MouseDownEvent, _, cx| {
                                        s.update(cx, |s, cx| { s.set_zone(col, row, zone_clone.clone()); cx.notify(); });
                                    }
                                })
                                .into_any_element()
                        );
                    }
                    v_flex().gap_1().p_2().children(picker_items).into_any_element()
                } else if let Some(zone) = &cell.zone {
                    // Show assigned zone
                    let is_fixed = zone.has_fixed_height();
                    v_flex().gap_0p5().items_center().justify_center()
                        .child(ui::Icon::new(zone.icon()).size(if is_fixed { IconSize::Small } else { IconSize::Medium }).color(Color::Accent))
                        .child(Label::new(zone.display_name()).size(ui::LabelSize::Small).color(Color::Default))
                        .when(is_fixed, |this| {
                            this.child(Label::new("fixed height").size(ui::LabelSize::XSmall).color(Color::Disabled))
                        })
                        .child(
                            h_flex().gap_1().mt_1()
                                .child(IconButton::new(
                                    SharedString::from(format!("clear-{col}-{row}")),
                                    IconName::Close,
                                ).icon_size(IconSize::XSmall).on_click({
                                    let s = self.state.clone();
                                    move |_,_,cx| { s.update(cx, |s,cx| { s.clear_zone(col, row); cx.notify(); }); }
                                }))
                        )
                        .into_any_element()
                } else {
                    // Empty — show + button
                    div().items_center().justify_center()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("add-zone-{col}-{row}")),
                                IconName::Plus,
                            )
                            .icon_size(IconSize::Medium)
                            .tooltip(Tooltip::text("Assign a pane"))
                            .on_click({
                                let s = self.state.clone();
                                move |_,_,cx| {
                                    s.update(cx, |s, cx| {
                                        s.picking_zone_for = Some((col, row));
                                        cx.notify();
                                    });
                                }
                            })
                        )
                        .into_any_element()
                };

                let cell_el = div()
                    .id(SharedString::from(format!("cell-{col}-{row}")))
                    .flex_1()
                    .w_full()
                    .m(px(3.0))
                    .rounded_lg()
                    .border_2()
                    .border_color(if has_zone || is_picking { drop_border } else { border_color })
                    .bg(if has_zone {
                        drop_bg
                    } else if is_picking {
                        gpui::hsla(0.58, 0.8, 0.3, 0.3)
                    } else {
                        gpui::hsla(0.0, 0.0, 1.0, 0.03)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(cell_content);

                row_elements.push(cell_el.into_any_element());
            }

            column_elements.push(
                v_flex().flex_1().h_full()
                    .child(col_header)
                    .children(row_elements)
                    .into_any_element()
            );
        }

        // Bottom
        let bottom = h_flex()
            .w_full().h(px(32.0)).px_4().items_center()
            .bg(cx.theme().colors().title_bar_background)
            .border_t_1().border_color(border_color)
            .child(Label::new("Assign panes to grid cells, then click Apply Layout")
                .size(ui::LabelSize::XSmall).color(Color::Muted));

        div()
            .absolute().top_0().left_0().size_full()
            .bg(gpui::hsla(0.6, 0.6, 0.12, 0.95))
            .child(v_flex().size_full()
                .child(toolbar)
                .child(h_flex().flex_1().w_full().p_4().gap_2().children(column_elements))
                .child(bottom))
            .into_any_element()
    }
}
