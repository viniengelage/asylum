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
            this.child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(colors.text_accent),
            )
        })
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
