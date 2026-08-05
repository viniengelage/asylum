use gpui::{AnyElement, Hsla};
use smallvec::SmallVec;

use crate::Tab;
use crate::prelude::*;

/// Where a header bar sits in the surface hierarchy, which determines its fill and divider color.
///
/// The themes deliberately paint `tab_active_background` and `toolbar_background` with the same
/// value as the content background, so a [`HeaderBarLevel::Pane`] strip recedes behind the content
/// while a [`HeaderBarLevel::Content`] strip reads as part of it. Picking the wrong level is what
/// makes two adjacent bars look like they belong to different applications.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HeaderBarLevel {
    /// The topmost strip of a pane or panel.
    Pane,
    /// A secondary strip *inside* a pane's content, such as breadcrumbs or a search row.
    Content,
}

impl HeaderBarLevel {
    pub fn background(&self, cx: &App) -> Hsla {
        match self {
            HeaderBarLevel::Pane => cx.theme().colors().tab_bar_background,
            HeaderBarLevel::Content => cx.theme().colors().toolbar_background,
        }
    }

    pub fn border(&self, cx: &App) -> Hsla {
        match self {
            HeaderBarLevel::Pane => cx.theme().colors().border,
            HeaderBarLevel::Content => cx.theme().colors().border_variant,
        }
    }
}

/// The shared shell for the strip at the top of a pane or panel.
///
/// Use this instead of hand-rolling an `h_flex` with a literal height: every header in the app is
/// expected to be exactly [`HeaderBar::height`] so that the strips of neighbouring panes line up
/// horizontally at any UI density.
///
/// For a strip made of tabs use [`crate::TabBar`], which shares these metrics.
#[derive(IntoElement, RegisterComponent)]
pub struct HeaderBar {
    id: ElementId,
    level: HeaderBarLevel,
    start_children: SmallVec<[AnyElement; 2]>,
    children: SmallVec<[AnyElement; 2]>,
    end_children: SmallVec<[AnyElement; 2]>,
}

impl HeaderBar {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            level: HeaderBarLevel::Pane,
            start_children: SmallVec::new(),
            children: SmallVec::new(),
            end_children: SmallVec::new(),
        }
    }

    pub fn level(mut self, level: HeaderBarLevel) -> Self {
        self.level = level;
        self
    }

    /// The height every header strip shares, matching a tab so tab bars and plain headers align.
    pub fn height(cx: &App) -> Pixels {
        Tab::container_height(cx)
    }

    /// Gap between adjacent items within one slot of a header.
    pub fn slot_gap(cx: &App) -> Pixels {
        DynamicSpacing::Base04.px(cx)
    }

    /// Horizontal padding of a header slot.
    pub fn slot_padding(cx: &App) -> Pixels {
        DynamicSpacing::Base06.px(cx)
    }

    pub fn start_children_mut(&mut self) -> &mut SmallVec<[AnyElement; 2]> {
        &mut self.start_children
    }

    pub fn start_child(mut self, start_child: impl IntoElement) -> Self {
        self.start_children_mut()
            .push(start_child.into_element().into_any());
        self
    }

    pub fn start_children(
        mut self,
        start_children: impl IntoIterator<Item = impl IntoElement>,
    ) -> Self {
        self.start_children_mut().extend(
            start_children
                .into_iter()
                .map(|child| child.into_any_element()),
        );
        self
    }

    pub fn end_children_mut(&mut self) -> &mut SmallVec<[AnyElement; 2]> {
        &mut self.end_children
    }

    pub fn end_child(mut self, end_child: impl IntoElement) -> Self {
        self.end_children_mut()
            .push(end_child.into_element().into_any());
        self
    }

    pub fn end_children(mut self, end_children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.end_children_mut().extend(
            end_children
                .into_iter()
                .map(|child| child.into_any_element()),
        );
        self
    }
}

impl ParentElement for HeaderBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for HeaderBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let slot_gap = HeaderBar::slot_gap(cx);
        let slot_padding = HeaderBar::slot_padding(cx);

        h_flex()
            .id(self.id)
            .group("header_bar")
            .flex_none()
            .w_full()
            .h(HeaderBar::height(cx))
            .px(slot_padding)
            .gap(slot_gap)
            .bg(self.level.background(cx))
            .border_b_1()
            .border_color(self.level.border(cx))
            .when(!self.start_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .h_full()
                        .gap(slot_gap)
                        .children(self.start_children),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap(slot_gap)
                    .children(self.children),
            )
            .when(!self.end_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .h_full()
                        .gap(slot_gap)
                        .children(self.end_children),
                )
            })
    }
}

impl Component for HeaderBar {
    fn scope() -> ComponentScope {
        ComponentScope::Layout
    }

    fn name() -> &'static str {
        "HeaderBar"
    }

    fn description() -> &'static str {
        "The shared shell for the strip at the top of a pane or panel, sized so that headers of \
        neighbouring panes align at any UI density."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        use crate::{Tab, TabBar, TabPosition};

        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "Levels",
                    vec![
                        single_example(
                            "Pane (topmost strip)",
                            HeaderBar::new("header_pane")
                                .child(Label::new("Workspaces"))
                                .end_child(
                                    IconButton::new("add", IconName::Plus)
                                        .icon_size(IconSize::Small),
                                )
                                .into_any_element(),
                        ),
                        single_example(
                            "Content (secondary strip)",
                            HeaderBar::new("header_content")
                                .level(HeaderBarLevel::Content)
                                .child(Label::new("src/main.rs").color(Color::Muted))
                                .into_any_element(),
                        ),
                    ],
                ),
                // Every variant of a pane's top strip, stacked so that any drift in height or
                // fill between them is visible at a glance.
                example_group_with_title(
                    "All header variants share one height",
                    vec![
                        single_example(
                            "Document tabs",
                            TabBar::new("preview_document_tabs")
                                .child(
                                    Tab::new("preview_tab_active")
                                        .position(TabPosition::First)
                                        .toggle_state(true)
                                        .start_slot(
                                            Icon::new(IconName::FileCode).size(IconSize::Small),
                                        )
                                        .child(Label::new("main.rs")),
                                )
                                .child(
                                    Tab::new("preview_tab_inactive")
                                        .position(TabPosition::Last)
                                        .start_slot(
                                            Icon::new(IconName::FileCode)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new("lib.rs").color(Color::Muted)),
                                )
                                .into_any_element(),
                        ),
                        single_example(
                            "Segmented tabs",
                            TabBar::segmented("preview_segmented")
                                .child(
                                    Tab::new("preview_segmented_a")
                                        .position(TabPosition::First)
                                        .full_width(true)
                                        .toggle_state(true)
                                        .child(Label::new("Changes")),
                                )
                                .child(
                                    Tab::new("preview_segmented_b")
                                        .position(TabPosition::Last)
                                        .full_width(true)
                                        .child(Label::new("History").color(Color::Muted)),
                                )
                                .into_any_element(),
                        ),
                        single_example(
                            "Section header",
                            HeaderBar::new("preview_section")
                                .start_child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new("Chats"))
                                .end_child(
                                    IconButton::new("preview_filter", IconName::Filter)
                                        .icon_size(IconSize::Small),
                                )
                                .into_any_element(),
                        ),
                        single_example(
                            "Icon-only tabs",
                            TabBar::segmented("preview_icon_tabs")
                                .child(
                                    Tab::new("preview_icon_a")
                                        .position(TabPosition::First)
                                        .full_width(true)
                                        .toggle_state(true)
                                        .child(Icon::new(IconName::FileTree).size(IconSize::Small)),
                                )
                                .child(
                                    Tab::new("preview_icon_b")
                                        .position(TabPosition::Last)
                                        .full_width(true)
                                        .child(
                                            Icon::new(IconName::GitBranch)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .into_any_element(),
                        ),
                    ],
                ),
            ])
            .into_any_element()
    }
}
