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

/// Two or more choices side by side; the chosen one is filled and outlined in the accent, as in
/// the boards.
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
        .border_1()
        .border_color(gpui::transparent_black())
        .when(selected, |this| {
            this.bg(colors.element_selected)
                .border_color(colors.border_focused)
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
        .rounded_md()
        .bg(if enabled { fill } else { fill.opacity(0.4) })
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

/// The surface of the boards' modals: 14 px corners, the strong border and a deep shadow. The
/// caller sets the width.
pub fn modal_surface(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .rounded(px(14.))
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.elevated_surface_background)
        .shadow_lg()
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
        .border_color(colors.border)
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
