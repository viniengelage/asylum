//! The form controls of the Asylum boards. `ButtonStyle::Filled` is a neutral gray in the
//! Polar theme and a toggled `Button` only tints its label, so screens that follow the boards
//! use these instead.

use crate::{CommonAnimationExt, Divider, DividerColor, KeyBinding, prelude::*};
use gpui::{BoxShadow, Div, FontWeight, Stateful, hsla, px};

pub fn focus_ring(cx: &App) -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(
            px(0.),
            px(0.),
            cx.theme().colors().border_focused.opacity(0.25),
        )
        .spread_radius(px(3.)),
    ]
}

fn raised_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.5)).blur_radius(px(2.)),
        BoxShadow::new(px(0.), px(1.), hsla(0., 0., 1., 0.12)).inset(),
    ]
}

/// A form's main action, filled with the theme's strong accent as in the boards. The `Filled`
/// button style is a neutral gray in the Polar theme, so it can't say "this is the next step".
pub fn primary_button(
    id: impl Into<gpui::ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    key_binding: Option<KeyBinding>,
    cx: &App,
) -> Stateful<Div> {
    let fill = cx.theme().colors().border_focused;
    h_flex()
        .id(id)
        .h(px(30.))
        .px_3()
        .gap_1p5()
        .flex_none()
        .rounded(Radius::Sm.rems())
        .cursor_pointer()
        .bg(fill)
        .shadow(raised_shadow())
        .hover(move |this| this.bg(fill.opacity(0.85)))
        .children(icon.map(|icon| {
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(Color::Custom(gpui::white()))
        }))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Custom(gpui::white())),
        )
        .children(key_binding.map(|binding| binding.size(rems_from_px(12_f32))))
}

/// The bordered companion of [`primary_button`], e.g. Cancelar.
pub fn secondary_button(
    id: impl Into<gpui::ElementId>,
    icon: Option<(IconName, Color)>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .h(px(30.))
        .px_3()
        .gap_1p5()
        .flex_none()
        .rounded(Radius::Sm.rems())
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.element_background)
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
        })
        .children(icon.map(|(icon, color)| Icon::new(icon).size(IconSize::XSmall).color(color)))
        .child(Label::new(label).size(LabelSize::Small).color(if enabled {
            Color::Default
        } else {
            Color::Muted
        }))
}

/// The boards' bordered choice in a row of starting points (a template, "Em branco"); the chosen
/// one is filled.
pub fn choice_chip(
    id: impl Into<gpui::ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    selected: bool,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .h(px(32.))
        .pl(px(10.))
        .pr_3()
        .gap_1p5()
        .flex_none()
        .rounded(px(8.))
        .border_1()
        .map(|this| {
            if selected {
                this.bg(colors.element_selected)
                    .border_color(colors.border_selected)
            } else {
                this.border_color(colors.border)
            }
        })
        .when(enabled && !selected, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
        })
        .when(!enabled, |this| this.opacity(0.5))
        .children(icon.map(|icon| {
            Icon::new(icon).size(IconSize::Small).color(if selected {
                Color::Default
            } else {
                Color::Muted
            })
        }))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(FontWeight::MEDIUM)
                .color(if selected {
                    Color::Default
                } else {
                    Color::Muted
                }),
        )
}

/// The boards' single-line input box; the caller puts the editor (and any prefix) inside.
pub fn text_field(focused: bool, cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .min_w_0()
        .h(px(30.))
        .px(px(10.))
        .gap(px(1.))
        .rounded(Radius::Md.rems())
        .border_1()
        .border_color(if focused {
            colors.border_focused.opacity(0.7)
        } else {
            colors.border_selected
        })
        .bg(colors.element_background)
        .when(focused, |this| this.shadow(focus_ring(cx)))
}

/// The boards' select box: the chosen value and a chevron, in the [`text_field`] frame. Wrap it
/// in [`PopoverTriggerArea`] to open the choices.
pub fn select_field(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .w_full()
        .min_w_0()
        .h(px(30.))
        .px(px(10.))
        .gap_1()
        .justify_between()
        .rounded(Radius::Md.rems())
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.element_background)
        .cursor_pointer()
        .hover(|this| this.bg(colors.element_hover))
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(FontWeight::MEDIUM)
                .single_line()
                .truncate(),
        )
        .child(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
}

/// The boards' round, bordered chip action, e.g. "+ Adicionar" next to a row of people.
pub fn pill_button(
    id: impl Into<gpui::ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .h(px(26.))
        .pl_2()
        .pr(px(10.))
        .gap(px(5.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(colors.border_selected)
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
        })
        .when(!enabled, |this| this.opacity(0.5))
        .children(icon.map(|icon| Icon::new(icon).size(IconSize::XSmall).color(Color::Muted)))
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

/// The boards' accent picker (a branch, a target): tinted in the accent with a chevron, in the
/// buffer font. Wrap it in [`PopoverTriggerArea`] to open the choices.
pub fn picker_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let accent = colors.border_focused;
    h_flex()
        .id(id)
        .min_w_0()
        .h(px(26.))
        .pl_2()
        .pr(px(6.))
        .gap(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(accent)
        .bg(accent.opacity(0.2))
        .hover(move |this| this.bg(accent.opacity(0.3)))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .truncate(),
        )
        .child(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
}

/// Where a setting's value comes from, for [`scope_chip`].
#[derive(Clone, Debug, PartialEq)]
pub enum ScopeChipKind {
    /// The file every profile shares.
    AllProfiles,
    /// A profile's own file, in the profile's color.
    Profile(gpui::Hsla),
    Project,
    /// No file sets it.
    Default,
}

/// The boards' Scope Chip: says where a setting's value comes from. The caller makes it open the
/// scope menu.
pub fn scope_chip(
    id: impl Into<gpui::ElementId>,
    kind: ScopeChipKind,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let accent = colors.text_accent;
    let base = h_flex()
        .id(id)
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .gap_1()
        .rounded(px(4.))
        .cursor_pointer();
    let label = Label::new(label)
        .size(LabelSize::XSmall)
        .weight(FontWeight::MEDIUM);
    match kind {
        ScopeChipKind::AllProfiles => base
            .bg(colors.element_hover)
            .hover(|this| this.bg(colors.element_selected))
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(label.color(Color::Muted)),
        ScopeChipKind::Profile(color) => base
            .bg(color.opacity(0.14))
            .hover(move |this| this.bg(color.opacity(0.24)))
            .child(div().size(px(6.)).rounded_full().bg(color))
            .child(label.color(Color::Custom(color))),
        ScopeChipKind::Project => base
            .bg(accent.opacity(0.14))
            .hover(move |this| this.bg(accent.opacity(0.24)))
            .child(
                Icon::new(IconName::Folder)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(accent)),
            )
            .child(label.color(Color::Custom(accent))),
        ScopeChipKind::Default => base
            .border_1()
            .border_color(colors.border)
            .hover(|this| this.bg(colors.element_hover))
            .child(label.color(Color::Placeholder)),
    }
}

/// The boards' Status Chip, e.g. "Sandbox" or "exit 1".
pub fn status_chip(
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    color: Color,
    cx: &App,
) -> Div {
    status_chip_shell(
        icon.map(|icon| {
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(color)
                .into_any_element()
        }),
        label.into(),
        color,
        cx,
    )
}

/// A [`status_chip`] whose icon spins, for work in progress such as "Atualizando…".
pub fn status_chip_spinning(
    icon: IconName,
    label: impl Into<SharedString>,
    color: Color,
    cx: &App,
) -> Div {
    status_chip_shell(
        Some(
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(color)
                .with_rotate_animation(2)
                .into_any_element(),
        ),
        label.into(),
        color,
        cx,
    )
}

fn status_chip_shell(icon: Option<AnyElement>, label: SharedString, color: Color, cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .gap_1()
        .rounded_full()
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.elevated_surface_background)
        .children(icon)
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .weight(FontWeight::MEDIUM)
                .color(color)
                .single_line(),
        )
}

/// The 20 px tile with a name's initial, tinted with an accent derived from the name.
pub fn initial_tile(name: &str, cx: &App) -> Div {
    let accents = cx.theme().accents();
    let accent = if accents.0.is_empty() {
        cx.theme().colors().text_accent
    } else {
        accents.color_for_index(stable_name_hash(name))
    };
    let initial: SharedString = name
        .chars()
        .next()
        .map(|letter| letter.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_string())
        .into();

    div()
        .flex_none()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(Radius::Sm.rems())
        .bg(accent.opacity(0.2))
        .child(
            Label::new(initial)
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Custom(accent)),
        )
}

// FNV-1a rather than `DefaultHasher`, whose output may change between Rust releases and
// would recolor every tile.
fn stable_name_hash(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// The rounded rail under the tab bar that holds a content toolbar (breadcrumbs, actions).
pub fn toolbar_rail(cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .min_h(px(30.))
        .items_center()
        .justify_between()
        .gap(DynamicSpacing::Base06.rems(cx))
        .pl(px(10.))
        .pr(px(3.))
        .rounded(crate::TAB_STRIP_RADIUS)
        .bg(colors.element_background.opacity(0.6))
        .border_1()
        .border_color(colors.border_variant)
}

/// `Callout` is a borderless full-width strip; the boards' inline error is a rounded, tinted box.
pub fn error_callout(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let error = Color::Error.color(cx);
    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .p_3()
        .rounded(Radius::Md.rems())
        .border_1()
        .border_color(error.opacity(0.2))
        .bg(error.opacity(0.06))
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .color(Color::Error),
                )
                .child(Label::new(message).size(LabelSize::Small)),
        )
}

pub fn segment_option(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .h(px(28.))
        .justify_center()
        .rounded(px(7.))
        .border_1()
        .border_color(gpui::transparent_black())
        .when(selected, |this| {
            this.bg(colors.elevated_surface_background)
                .border_color(colors.border_selected)
                .shadow(vec![
                    BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.5)).blur_radius(px(2.)),
                ])
        })
        .when(enabled && !selected, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .color(if !enabled {
                    Color::Disabled
                } else if selected {
                    Color::Default
                } else {
                    Color::Muted
                })
                .truncate(),
        )
}

pub fn segmented_control(cx: &App) -> Div {
    h_flex()
        .p(px(3.))
        .gap_0p5()
        .rounded(px(10.))
        .bg(cx.theme().colors().element_background)
}

pub fn button_group(children: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    segmented_control(cx)
        .flex_none()
        .items_center()
        .children(interleave_dividers(children, px(14.)))
}

/// For a row that already sits inside a rail, so it adds no capsule of its own.
pub fn button_row(children: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .gap(DynamicSpacing::Base06.px(cx))
        .children(interleave_dividers(children, px(12.)))
}

pub fn group_icon_button(id: impl Into<ElementId>, icon: IconName, side: f32) -> IconButton {
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .size(ButtonSize::None)
        .width(rems_from_px(side))
        .height(rems_from_px(side).into())
}

/// The divider between the buttons of a [`button_group`] or [`button_row`], also used between
/// the tabs of a [`crate::TabStyle::Capsule`] strip so both rails share one rule.
pub fn group_divider(height: Pixels) -> Divider {
    Divider::vertical()
        .color(DividerColor::BorderVariant)
        .h(height)
}

fn interleave_dividers(
    children: impl IntoIterator<Item = AnyElement>,
    divider_height: Pixels,
) -> Vec<AnyElement> {
    let mut interleaved = Vec::new();
    for child in children {
        if !interleaved.is_empty() {
            interleaved.push(group_divider(divider_height).into_any_element());
        }
        interleaved.push(child);
    }
    interleaved
}

/// The radio of the boards' choice cards: an accent ring with a dot when chosen, a plain ring
/// otherwise. The card around it owns the click.
pub fn radio_indicator(selected: bool, cx: &App) -> Div {
    let colors = cx.theme().colors();
    div()
        .flex()
        .flex_none()
        .size(px(16.))
        .items_center()
        .justify_center()
        .rounded_full()
        .border(px(1.5))
        .border_color(if selected {
            colors.border_focused
        } else {
            colors.border
        })
        .when(selected, |this| {
            this.child(div().size(px(8.)).rounded_full().bg(colors.text_accent))
        })
}

/// A form's destructive action (Excluir), filled in the boards' dark rose: the theme's error
/// color, darkened so white-ish text reads on it.
pub fn destructive_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let error = Color::Error.color(cx);
    let fill = gpui::Hsla {
        l: error.l * 0.5,
        ..error
    };
    let text = gpui::Hsla { l: 0.95, ..error };
    h_flex()
        .id(id)
        .h(px(30.))
        .px_3()
        .flex_none()
        .rounded(Radius::Sm.rems())
        .bg(if enabled { fill } else { fill.opacity(0.4) })
        .when(enabled, |this| this.shadow(raised_shadow()))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(move |this| this.bg(fill.opacity(0.85)))
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Custom(if enabled {
                    text
                } else {
                    text.opacity(0.6)
                })),
        )
}

/// The boards' on/off switch: a 30×18 track, accent when on, with a white knob.
pub fn toggle_switch(
    id: impl Into<gpui::ElementId>,
    on: bool,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .when(on, |this| this.justify_end())
        .bg(if on {
            colors.border_focused
        } else {
            colors.border_selected
        })
        .when(!enabled, |this| this.opacity(0.5))
        .when(enabled, |this| this.cursor_pointer())
        .child(div().size(px(14.)).rounded_full().bg(gpui::white()))
}

/// The boards' checkbox: filled with the accent and a white check when checked, a plain
/// square otherwise. The row around it owns the click.
pub fn checkbox_indicator(checked: bool, cx: &App) -> Div {
    let colors = cx.theme().colors();
    div()
        .flex()
        .flex_none()
        .size(px(16.))
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .map(|this| {
            if checked {
                this.bg(colors.border_focused).child(
                    Icon::new(IconName::Check)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(gpui::white())),
                )
            } else {
                this.border(px(1.5)).border_color(colors.border_selected)
            }
        })
}

/// The boards' 4 px progress line, filled with the accent up to `fraction`.
pub fn progress_line(fraction: f32, cx: &App) -> Div {
    let colors = cx.theme().colors();
    div()
        .w_full()
        .h(px(4.))
        .rounded_full()
        .bg(colors.element_selected)
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(colors.border_focused)
                .w(gpui::relative(fraction.clamp(0.0, 1.0))),
        )
}

pub fn modal_surface(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .rounded(Radius::Xl.rems())
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.elevated_surface_background)
        .shadow(crate::ElevationIndex::ModalSurface.shadow(cx))
        .overflow_hidden()
}

/// The bar along a modal's bottom edge: a hint on the left, the actions on the right.
pub fn modal_footer(cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .h(px(56.))
        .flex_none()
        .pl_6()
        .pr_4()
        .gap_2()
        .border_t_1()
        .border_color(colors.border_variant)
        .bg(colors.elevated_surface_background)
}

/// Lets any of the controls above open a [`crate::PopoverMenu`], which only takes clickable,
/// toggleable triggers.
#[derive(IntoElement)]
pub struct PopoverTriggerArea {
    base: Stateful<Div>,
    on_click: Option<Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>>,
    cursor_style: gpui::CursorStyle,
}

impl PopoverTriggerArea {
    pub fn new(base: Stateful<Div>) -> Self {
        Self {
            base,
            on_click: None,
            cursor_style: gpui::CursorStyle::PointingHand,
        }
    }
}

impl Clickable for PopoverTriggerArea {
    fn on_click(
        mut self,
        handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    fn cursor_style(mut self, cursor_style: gpui::CursorStyle) -> Self {
        self.cursor_style = cursor_style;
        self
    }
}

impl Styled for PopoverTriggerArea {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.base.style()
    }
}

impl Toggleable for PopoverTriggerArea {
    fn toggle_state(self, _selected: bool) -> Self {
        self
    }
}

impl RenderOnce for PopoverTriggerArea {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        self.base
            .cursor(self.cursor_style)
            .when_some(self.on_click, |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            })
    }
}
