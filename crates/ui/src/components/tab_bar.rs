use gpui::{AnyElement, ScrollHandle};
use smallvec::SmallVec;

use crate::prelude::*;
use crate::{HeaderBar, HeaderBarLevel, Tab, TabPosition};

/// How a [`TabBar`] distributes its tabs.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabBarLayout {
    /// Tabs take their content width, align to the start and scroll when they overflow.
    /// This is the layout for document tabs.
    Scrollable,
    /// Tabs split the available width evenly and center their content.
    ///
    /// Use this for a fixed set of mutually exclusive views — a panel's own sub-tabs, for
    /// instance. Tabs rendered this way must be built with [`Tab::full_width`] so their
    /// background covers the whole slot.
    Segmented,
}

#[derive(IntoElement, RegisterComponent)]
pub struct TabBar {
    id: ElementId,
    layout: TabBarLayout,
    start_children: SmallVec<[AnyElement; 2]>,
    children: SmallVec<[AnyElement; 2]>,
    end_children: SmallVec<[AnyElement; 2]>,
    scroll_handle: Option<ScrollHandle>,
}

impl TabBar {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            layout: TabBarLayout::Scrollable,
            start_children: SmallVec::new(),
            children: SmallVec::new(),
            end_children: SmallVec::new(),
            scroll_handle: None,
        }
    }

    /// Builds a tab bar whose tabs split the width evenly instead of scrolling.
    ///
    /// The tabs passed in still need [`Tab::full_width`] so they fill the slot they are given.
    pub fn segmented(id: impl Into<ElementId>) -> Self {
        Self {
            layout: TabBarLayout::Segmented,
            ..Self::new(id)
        }
    }

    pub fn track_scroll(mut self, scroll_handle: &ScrollHandle) -> Self {
        self.scroll_handle = Some(scroll_handle.clone());
        self
    }

    pub fn start_children_mut(&mut self) -> &mut SmallVec<[AnyElement; 2]> {
        &mut self.start_children
    }

    pub fn start_child(mut self, start_child: impl IntoElement) -> Self
    where
        Self: Sized,
    {
        self.start_children_mut()
            .push(start_child.into_element().into_any());
        self
    }

    pub fn start_children(
        mut self,
        start_children: impl IntoIterator<Item = impl IntoElement>,
    ) -> Self
    where
        Self: Sized,
    {
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

    pub fn end_child(mut self, end_child: impl IntoElement) -> Self
    where
        Self: Sized,
    {
        self.end_children_mut()
            .push(end_child.into_element().into_any());
        self
    }

    pub fn end_children(mut self, end_children: impl IntoIterator<Item = impl IntoElement>) -> Self
    where
        Self: Sized,
    {
        self.end_children_mut().extend(
            end_children
                .into_iter()
                .map(|child| child.into_any_element()),
        );
        self
    }
}

impl ParentElement for TabBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for TabBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let border_color = HeaderBarLevel::Pane.border(cx);
        let slot_gap = HeaderBar::slot_gap(cx);
        let slot_padding = HeaderBar::slot_padding(cx);
        let layout = self.layout;

        div()
            .id(self.id)
            .group("tab_bar")
            .flex()
            .flex_none()
            .w_full()
            .h(HeaderBar::height(cx))
            .rounded_t_lg()
            .bg(HeaderBarLevel::Pane.background(cx))
            .when(!self.start_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap(slot_gap)
                        .px(slot_padding)
                        .border_b_1()
                        .border_r_1()
                        .border_color(border_color)
                        .children(self.start_children),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_hidden()
                    // The tabs draw their own bottom border; this fills the rule in across
                    // whatever part of the strip they do not cover.
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .border_b_1()
                            .border_color(border_color),
                    )
                    .child(match layout {
                        TabBarLayout::Scrollable => h_flex()
                            .id("tabs")
                            .flex_grow_1()
                            .overflow_x_scroll()
                            .when_some(self.scroll_handle, |this, scroll_handle| {
                                this.track_scroll(&scroll_handle)
                            })
                            .children(self.children),
                        TabBarLayout::Segmented => h_flex()
                            .id("tabs")
                            .w_full()
                            .h_full()
                            .children(self.children.into_iter().map(|tab| {
                                div().flex_1().min_w_0().h_full().child(tab)
                            })),
                    }),
            )
            .when(!self.end_children.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap(slot_gap)
                        .px(slot_padding)
                        .border_color(border_color)
                        .border_b_1()
                        .border_l_1()
                        .children(self.end_children),
                )
            })
    }
}

impl Component for TabBar {
    fn scope() -> ComponentScope {
        ComponentScope::Navigation
    }

    fn name() -> &'static str {
        "TabBar"
    }

    fn description() -> &'static str {
        "A horizontal bar containing tabs for navigation between different views \
        or sections."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "Basic Usage",
                    vec![
                        single_example(
                            "Empty TabBar",
                            TabBar::new("empty_tab_bar").into_any_element(),
                        ),
                        single_example(
                            "With Tabs",
                            TabBar::new("tab_bar_with_tabs")
                                .child(Tab::new("tab1"))
                                .child(Tab::new("tab2"))
                                .child(Tab::new("tab3"))
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "With Start and End Children",
                    vec![single_example(
                        "Full TabBar",
                        TabBar::new("full_tab_bar")
                            .start_child(Button::new("start_button", "Start"))
                            .child(Tab::new("tab1"))
                            .child(Tab::new("tab2"))
                            .child(Tab::new("tab3"))
                            .end_child(Button::new("end_button", "End"))
                            .into_any_element(),
                    )],
                ),
                example_group_with_title(
                    "Segmented",
                    vec![single_example(
                        "Equal-width tabs",
                        TabBar::segmented("segmented_tab_bar")
                            .child(
                                Tab::new("segmented_android")
                                    .position(TabPosition::First)
                                    .full_width(true)
                                    .toggle_state(true)
                                    .child(Label::new("Android")),
                            )
                            .child(
                                Tab::new("segmented_ios")
                                    .position(TabPosition::Last)
                                    .full_width(true)
                                    .child(Label::new("iOS")),
                            )
                            .into_any_element(),
                    )],
                ),
            ])
            .into_any_element()
    }
}
