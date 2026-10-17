//! The form controls of the Asylum boards. `ButtonStyle::Filled` is a neutral gray in the
//! Polar theme and a toggled `Button` only tints its label, so screens that follow the boards
//! use these instead.

use crate::{KeyBinding, prelude::*};
use gpui::{Div, FontWeight, Stateful, px};

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
        .rounded_md()
        .cursor_pointer()
        .bg(fill)
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
        .rounded_md()
        .border_1()
        .border_color(colors.border)
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

/// Two or more choices side by side; the chosen one is filled, as in the boards.
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
        .rounded(px(5.))
        .when(selected, |this| this.bg(colors.element_selected))
        .when(enabled && !selected, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.element_hover))
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .weight(if selected {
                    FontWeight::MEDIUM
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

/// The bordered track that holds a row of [`segment_option`]s.
pub fn segmented_control(cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .p_0p5()
        .gap_0p5()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
}
