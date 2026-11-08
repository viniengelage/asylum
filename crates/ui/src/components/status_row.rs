use crate::prelude::*;
use gpui::{AnyElement, ClickEvent, FontWeight, Hsla, Pixels, px};

/// The state a [`StatusRow`] shows with its dot and trailing text, as the boards' Container Row
/// variants draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Running,
    Paused,
    Stopped,
    Failing,
}

impl RowStatus {
    fn dot(self, cx: &App) -> gpui::Hsla {
        match self {
            Self::Running => Color::Success.color(cx),
            Self::Paused => Color::Warning.color(cx),
            Self::Failing => Color::Error.color(cx),
            Self::Stopped => cx.theme().colors().border_selected,
        }
    }

    fn detail_color(self, cx: &App) -> Color {
        match self {
            Self::Running => Color::Muted,
            Self::Paused => Color::Warning,
            Self::Failing => Color::Error,
            Self::Stopped => Color::Custom(Color::Muted.color(cx).opacity(0.7)),
        }
    }
}

/// A 44 px list row with a status dot, a name over a monospaced detail, and a
/// monospaced state at the end: the boards' Container Row. When selected, its expansion is drawn
/// below the line on the same selected background.
#[derive(IntoElement)]
pub struct StatusRow {
    id: ElementId,
    status: RowStatus,
    name: SharedString,
    detail: SharedString,
    state: SharedString,
    selected: bool,
    dot_color: Option<Hsla>,
    group: Option<SharedString>,
    height: Pixels,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    end_slot: Option<AnyElement>,
    expansion: Option<AnyElement>,
}

impl StatusRow {
    pub fn new(id: impl Into<ElementId>, status: RowStatus, name: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            status,
            name: name.into(),
            detail: SharedString::default(),
            state: SharedString::default(),
            selected: false,
            dot_color: None,
            group: None,
            height: px(44.),
            on_click: None,
            end_slot: None,
            expansion: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = detail.into();
        self
    }

    /// The text at the end; it takes the status color, except while running.
    pub fn state(mut self, state: impl Into<SharedString>) -> Self {
        self.state = state.into();
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Overrides the status color of the dot, for lists whose dot means something else.
    pub fn dot_color(mut self, color: Hsla) -> Self {
        self.dot_color = Some(color);
        self
    }

    /// Names the row's hover group, so end slot controls can use `visible_on_hover`.
    pub fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }

    /// Replaces the state text, e.g. with a spinner while an action runs.
    pub fn end_slot(mut self, end_slot: impl IntoElement) -> Self {
        self.end_slot = Some(end_slot.into_any_element());
        self
    }

    /// Shown under the line while the row is selected, e.g. the selected container's actions.
    pub fn expansion(mut self, expansion: impl IntoElement) -> Self {
        self.expansion = Some(expansion.into_any_element());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for StatusRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let stopped = self.status == RowStatus::Stopped;
        let line = h_flex()
            .id(self.id)
            .when_some(self.group, |this, group| this.group(group))
            .w_full()
            .h(self.height)
            .flex_none()
            .px(px(10.))
            .gap(px(10.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(!self.selected, |this| {
                this.hover(|this| this.bg(colors.ghost_element_hover))
            })
            .child(
                div()
                    .size(px(8.))
                    .flex_none()
                    .rounded_full()
                    .bg(self.dot_color.unwrap_or_else(|| self.status.dot(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        Label::new(self.name)
                            .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                            .weight(FontWeight::MEDIUM)
                            .color(if stopped && !self.selected {
                                Color::Muted
                            } else {
                                Color::Default
                            })
                            .truncate(),
                    )
                    .when(!self.detail.is_empty(), |this| {
                        this.child(
                            Label::new(self.detail)
                                .size(LabelSize::Custom(rems_from_px(11_f32)))
                                .buffer_font(cx)
                                .color(Color::Placeholder)
                                .truncate(),
                        )
                    }),
            )
            .map(|this| match self.end_slot {
                Some(end_slot) => this.child(end_slot),
                None => this.when(!self.state.is_empty(), |this| {
                    this.child(
                        div().flex_none().child(
                            Label::new(self.state)
                                .size(LabelSize::Custom(rems_from_px(11_f32)))
                                .buffer_font(cx)
                                .color(self.status.detail_color(cx)),
                        ),
                    )
                }),
            })
            .when_some(self.on_click, |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            });
        v_flex()
            .w_full()
            .flex_none()
            .rounded(px(8.))
            .when(self.selected, |this| this.bg(colors.element_selected))
            .child(line)
            .when(self.selected, |this| this.children(self.expansion))
    }
}
