//! The building blocks of a dock's content: section headings, grouped rows, cards and the empty
//! state. Docks used to separate their parts with full-width rules; these separate them with
//! space and a faint fill instead, so a dock reads as one surface.

use gpui::{AnyElement, Div, FontWeight, px};
use smallvec::SmallVec;

use crate::prelude::*;

/// The small uppercase heading over a group of rows (SCHEMAS, COMMITS, ABERTOS).
pub fn section_label(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .size(LabelSize::XSmall)
        .weight(FontWeight::SEMIBOLD)
        .color(Color::Muted)
}

/// A [`section_label`] with something at its right edge, such as a count, a note or a button.
pub fn section_header(text: impl Into<SharedString>, end: Option<AnyElement>) -> Div {
    h_flex()
        .w_full()
        .min_h(px(20.))
        .gap_2()
        .justify_between()
        .child(section_label(text))
        .children(end)
}

/// The tinted block that holds a section's rows. The radius is the row radius plus the block's
/// padding, so the corners of a hovered row nest inside the block's.
pub fn grouped_block(cx: &App) -> Div {
    v_flex()
        .w_full()
        .p_1()
        .gap_px()
        .rounded(px(10.))
        .bg(cx.theme().colors().element_background)
}

/// A bordered card inside a dock or a settings page: a connection, a result, a summary.
pub fn card(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .rounded(Radius::Lg.rems())
        .border_1()
        .border_color(colors.border)
        .bg(colors.element_background)
}

/// What a dock or list shows when it has nothing yet: an accent icon tile, a title, a short
/// explanation and, optionally, the actions that get the user out of the empty state.
#[derive(IntoElement)]
pub struct EmptyState {
    icon: IconName,
    title: SharedString,
    description: Option<SharedString>,
    actions: SmallVec<[AnyElement; 2]>,
}

impl EmptyState {
    pub fn new(icon: IconName, title: impl Into<SharedString>) -> Self {
        Self {
            icon,
            title: title.into(),
            description: None,
            actions: SmallVec::new(),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Adds content under the description: buttons, or hints such as keyboard shortcuts.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        v_flex()
            .flex_1()
            .w_full()
            .items_center()
            .justify_center()
            .gap_3()
            .px_8()
            .py_6()
            .child(
                h_flex()
                    .size(px(44.))
                    .flex_none()
                    .justify_center()
                    .rounded(Radius::Lg.rems())
                    .bg(cx.theme().colors().element_selected)
                    .child(Icon::new(self.icon).color(Color::Accent)),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(Label::new(self.title).weight(FontWeight::BOLD))
                    .children(self.description.map(|description| {
                        div().text_center().child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })),
            )
            .when(!self.actions.is_empty(), |this| {
                this.child(
                    v_flex()
                        .items_center()
                        .gap_1p5()
                        .pt_1()
                        .children(self.actions),
                )
            })
    }
}
