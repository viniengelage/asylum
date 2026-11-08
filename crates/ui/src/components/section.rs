use gpui::{AnyElement, Div, FontWeight, px};
use smallvec::SmallVec;

use crate::{CommonAnimationExt, StatusTone, prelude::*};

pub fn section_label(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .size(LabelSize::XSmall)
        .weight(FontWeight::SEMIBOLD)
        .color(Color::Muted)
}

pub fn section_header(text: impl Into<SharedString>, end: Option<AnyElement>) -> Div {
    h_flex()
        .w_full()
        .min_h(px(20.))
        .gap_2()
        .justify_between()
        .child(section_label(text))
        .children(end)
}

/// The boards' navigator footer: a status dot and a subtle line on the left, an action on the
/// right.
pub fn navigator_footer(
    status: Option<StatusTone>,
    text: impl Into<SharedString>,
    action: Option<AnyElement>,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .flex_none()
        .px_3()
        .gap_2()
        .when_some(status, |this, status| this.child(status.dot(cx)))
        .child(
            div().flex_1().min_w_0().child(
                Label::new(text)
                    .size(LabelSize::Custom(rems_from_px(11_f32)))
                    .color(Color::Placeholder)
                    .truncate(),
            ),
        )
        .children(action)
}

pub fn grouped_block(cx: &App) -> Div {
    v_flex()
        .w_full()
        .p_1()
        .gap_px()
        .rounded(px(10.))
        .bg(cx.theme().colors().element_background)
}

pub fn card(cx: &App) -> Div {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .rounded(Radius::Lg.rems())
        .border_1()
        .border_color(colors.border)
        .bg(colors.element_background)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmptyStateTone {
    #[default]
    Default,
    Error,
}

#[derive(IntoElement)]
pub struct EmptyState {
    icon: IconName,
    title: SharedString,
    description: Option<SharedString>,
    detail: Option<SharedString>,
    tone: EmptyStateTone,
    spinning: bool,
    actions: SmallVec<[AnyElement; 2]>,
}

impl EmptyState {
    pub fn new(icon: IconName, title: impl Into<SharedString>) -> Self {
        Self {
            icon,
            title: title.into(),
            description: None,
            detail: None,
            tone: EmptyStateTone::Default,
            spinning: false,
            actions: SmallVec::new(),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn tone(mut self, tone: EmptyStateTone) -> Self {
        self.tone = tone;
        self
    }

    pub fn spinning(mut self, spinning: bool) -> Self {
        self.spinning = spinning;
        self
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_error = self.tone == EmptyStateTone::Error;
        let error = Color::Error.color(cx);
        let icon = Icon::new(self.icon).color(if is_error {
            Color::Error
        } else {
            Color::Accent
        });
        let icon = if self.spinning {
            icon.with_rotate_animation(2).into_any_element()
        } else {
            icon.into_any_element()
        };
        v_flex()
            .flex_1()
            .w_full()
            .items_center()
            .justify_center()
            .gap_3()
            .map(|this| if is_error { this.px_8() } else { this.px_10() })
            .py_6()
            .child(
                h_flex()
                    .size(px(44.))
                    .flex_none()
                    .justify_center()
                    .rounded(Radius::Lg.rems())
                    .map(|this| {
                        if is_error {
                            this.bg(error.opacity(0.1))
                        } else {
                            this.bg(cx.theme().colors().element_selected)
                        }
                    })
                    .child(icon),
            )
            .child(
                v_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_center()
                            .child(Label::new(self.title).weight(FontWeight::BOLD)),
                    )
                    .children(self.description.map(|description| {
                        div().text_center().child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })),
            )
            .children(self.detail.map(|detail| {
                div()
                    .w_full()
                    .p_2()
                    .rounded(Radius::Md.rems())
                    .border_1()
                    .border_color(error.opacity(0.2))
                    .bg(error.opacity(0.06))
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
            }))
            .when(!self.actions.is_empty(), |this| {
                this.child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .pt_1()
                        .children(self.actions),
                )
            })
    }
}
