use std::cmp::Ordering;

use gpui::{AnyElement, Hsla, IntoElement, Stateful};
use smallvec::SmallVec;

use crate::prelude::*;

/// Both of a tab's slots are the same square so the content between them stays optically
/// centred; they used to differ by 2px, which shifted every centred label off-centre.
const TAB_SLOT_SIZE: Pixels = px(14.);

/// The position of a [`Tab`] within a list of tabs.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabPosition {
    /// The tab is first in the list.
    First,

    /// The tab is in the middle of the list (i.e., it is not the first or last tab).
    ///
    /// The [`Ordering`] is where this tab is positioned with respect to the selected tab.
    Middle(Ordering),

    /// The tab is last in the list.
    Last,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabCloseSide {
    Start,
    End,
}

#[derive(IntoElement, RegisterComponent)]
pub struct Tab {
    div: Stateful<Div>,
    selected: bool,
    position: TabPosition,
    close_side: TabCloseSide,
    full_width: bool,
    surface: Option<Hsla>,
    start_slot: Option<AnyElement>,
    end_slot: Option<AnyElement>,
    children: SmallVec<[AnyElement; 2]>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            div: div()
                .id(id.clone())
                .debug_selector(|| format!("TAB-{}", id)),
            selected: false,
            position: TabPosition::First,
            close_side: TabCloseSide::End,
            full_width: false,
            surface: None,
            start_slot: None,
            end_slot: None,
            children: SmallVec::new(),
        }
    }

    pub fn position(mut self, position: TabPosition) -> Self {
        self.position = position;
        self
    }

    /// Makes the tab fill its parent's width and center its content.
    ///
    /// Without this a tab shrinks to its content, so a caller that sizes the tab through a
    /// wrapper (`flex_1`, `w(relative(..))`) would paint the tab background over the label only
    /// and let the parent's background show through the rest of the slot.
    pub fn full_width(mut self, full_width: bool) -> Self {
        self.full_width = full_width;
        self
    }

    /// The background of the content this tab sits on top of.
    ///
    /// The selected tab is painted with it so the tab and the content read as one plane
    /// instead of a chip floating above a different surface. Every tab strip should pass the
    /// background of whatever it covers: a pane passes its item's background, a panel's own
    /// sub-tabs pass the panel background.
    ///
    /// Without it the selected tab falls back to `tab_active_background`, which is continuous
    /// with the content only in themes that deliberately set the two to the same value.
    pub fn surface(mut self, surface: Hsla) -> Self {
        self.surface = Some(surface);
        self
    }

    pub fn close_side(mut self, close_side: TabCloseSide) -> Self {
        self.close_side = close_side;
        self
    }

    pub fn start_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.start_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn end_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.end_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn content_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx) - px(1.)
    }

    pub fn container_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx)
    }
}

impl InteractiveElement for Tab {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl StatefulInteractiveElement for Tab {}

impl Toggleable for Tab {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl ParentElement for Tab {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for Tab {
    #[allow(refining_impl_trait)]
    fn render(self, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        let (text_color, tab_bg) = match self.selected {
            false => (
                cx.theme().colors().text_muted,
                cx.theme().colors().tab_inactive_background,
            ),
            true => (
                cx.theme().colors().text,
                self.surface
                    .unwrap_or_else(|| cx.theme().colors().tab_active_background),
            ),
        };

        // Only unselected tabs get pointer feedback: the selected tab is painted with the
        // surface of the content below it, so the tab and that content read as one plane.
        // Lightening it on hover would break that continuity, and there is nothing to
        // activate on an active tab.
        let (tab_hover_bg, tab_pressed_bg) = (
            cx.theme().colors().ghost_element_hover,
            cx.theme().colors().ghost_element_active,
        );

        let has_action_slot = self.end_slot.is_some();
        let has_icon_slot = self.start_slot.is_some();
        let icon_slot = h_flex()
            .size(TAB_SLOT_SIZE)
            .justify_center()
            .children(self.start_slot);
        let action_slot = h_flex()
            .size(TAB_SLOT_SIZE)
            .justify_center()
            .children(self.end_slot);
        let close_side = self.close_side;
        let full_width = self.full_width;

        self.div
            .h(Tab::container_height(cx))
            .when(self.full_width, |this| this.w_full())
            .bg(tab_bg)
            .when(!self.selected, |this| {
                this.hover(|style| style.bg(tab_hover_bg))
                    .active(|style| style.bg(tab_pressed_bg))
            })
            .border_color(cx.theme().colors().border)
            .map(|this| match self.position {
                TabPosition::First => {
                    if self.selected {
                        this.pl_px()
                            .border_r_1()
                            .pb_px()
                            .rounded_tl(pane_corner_radius())
                    } else {
                        this.pl_px().pr_px().border_b_1()
                    }
                }
                TabPosition::Last => {
                    if self.selected {
                        this.border_l_1()
                            .border_r_1()
                            .pb_px()
                            .rounded_tr(pane_corner_radius())
                    } else {
                        this.pl_px().border_b_1().border_r_1()
                    }
                }
                TabPosition::Middle(Ordering::Equal) => this.border_l_1().border_r_1().pb_px(),
                TabPosition::Middle(Ordering::Less) => this.border_l_1().pr_px().border_b_1(),
                TabPosition::Middle(Ordering::Greater) => this.border_r_1().pl_px().border_b_1(),
            })
            .cursor_pointer()
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h(Tab::content_height(cx))
                    .gap(DynamicSpacing::Base04.px(cx))
                    .text_color(text_color)
                    .map(|this| {
                        if full_width {
                            // A tab that fills its slot has to keep the icon next to its own
                            // label: laying the three elements out in one row pins the icon to
                            // the far edge and leaves the action button hard against the
                            // divider it shares with the next tab. Instead the icon and label
                            // travel together in the middle, the action sits on its side, and
                            // an empty square of the same width balances the other side so the
                            // pair stays centred in the tab.
                            let balance = div().size(TAB_SLOT_SIZE).flex_none();
                            let (leading, trailing) = match (has_action_slot, close_side) {
                                (false, _) => (None, None),
                                (true, TabCloseSide::Start) => (Some(action_slot), Some(balance)),
                                (true, TabCloseSide::End) => (Some(balance), Some(action_slot)),
                            };
                            this.w_full()
                                // Wider than a document tab's `Base04`: this tab's edge is a
                                // divider shared with the next one, so the action button needs
                                // room on both sides of it.
                                .px(DynamicSpacing::Base06.px(cx))
                                .children(leading)
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .justify_center()
                                        .gap(DynamicSpacing::Base04.px(cx))
                                        // Unlike a document tab, which reserves the square so
                                        // that labels line up down a scrolling strip, a
                                        // centred tab with no icon must not reserve it: the
                                        // empty square would push the label off centre.
                                        .children(has_icon_slot.then_some(icon_slot))
                                        .children(self.children),
                                )
                                .children(trailing)
                        } else {
                            let (leading, trailing) = match close_side {
                                TabCloseSide::End => (icon_slot, action_slot),
                                TabCloseSide::Start => (action_slot, icon_slot),
                            };
                            this.px(DynamicSpacing::Base04.px(cx))
                                .child(leading)
                                .children(self.children)
                                .child(trailing)
                        }
                    }),
            )
    }
}

impl Component for Tab {
    fn scope() -> ComponentScope {
        ComponentScope::Navigation
    }

    fn description() -> &'static str {
        "A tab component that can be used in a tabbed interface, \
        supporting different positions and states."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![example_group_with_title(
                "Variations",
                vec![
                    single_example(
                        "Default",
                        Tab::new("default").child("Default Tab").into_any_element(),
                    ),
                    single_example(
                        "Selected",
                        Tab::new("selected")
                            .toggle_state(true)
                            .child("Selected Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "First",
                        Tab::new("first")
                            .position(TabPosition::First)
                            .child("First Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Middle",
                        Tab::new("middle")
                            .position(TabPosition::Middle(Ordering::Equal))
                            .child("Middle Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Last",
                        Tab::new("last")
                            .position(TabPosition::Last)
                            .child("Last Tab")
                            .into_any_element(),
                    ),
                ],
            )])
            .into_any_element()
    }
}
