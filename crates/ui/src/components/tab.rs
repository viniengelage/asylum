use std::cmp::Ordering;

use gpui::{AnyElement, Hsla, IntoElement, Stateful};

use crate::group_divider;
use smallvec::SmallVec;

use crate::prelude::*;

/// Both of a tab's slots are the same square so the content between them stays optically
/// centred; they used to differ by 2px, which shifted every centred label off-centre.
const TAB_SLOT_SIZE: Pixels = px(14.);

/// A pill sits inside a strip rounded to [`TAB_STRIP_RADIUS`] with a 3px inset, so its own
/// radius has to be smaller or the two sets of corners collide.
const TAB_PILL_RADIUS: Pixels = px(7.);

/// Radius of the rounded strip that holds [`TabStyle::Pill`] tabs.
pub const TAB_STRIP_RADIUS: Pixels = px(10.);

/// A document tab's close button is a hit target, so it gets a larger square than the 14px
/// icon slot.
const DOCUMENT_TAB_CLOSE_SLOT_SIZE: Pixels = px(18.);
const DOCUMENT_TAB_MIN_WIDTH: Pixels = px(120.);
const DOCUMENT_TAB_MAX_WIDTH: Pixels = px(200.);

/// How a [`Tab`] marks itself as selected.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum TabStyle {
    /// The selected tab is painted with the surface of the content below it, so the two read
    /// as one plane. Needs [`Tab::surface`].
    #[default]
    Surface,
    /// The selected tab is a raised pill inside a rounded strip: elevated fill, light border
    /// and a short shadow, like the thumb of a segmented control. This is the style for every island's strip, where continuity with the
    /// content was too subtle to tell which tab was active.
    Pill,
    /// A [`TabStyle::Pill`] whose selected tab takes the accent tint of an active toggle
    /// instead of rising out of the strip. The terminal panel uses it so its tabs read as one
    /// glued capsule rather than as document tabs.
    Capsule,
}

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
    document: bool,
    style: TabStyle,
    surface: Option<Hsla>,
    start_slot: Option<AnyElement>,
    end_slot: Option<AnyElement>,
    end_slot_indicator: Option<AnyElement>,
    end_slot_always_visible: bool,
    hover_group: Option<SharedString>,
    leading_divider: Option<SharedString>,
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
            document: false,
            style: TabStyle::Surface,
            surface: None,
            start_slot: None,
            end_slot: None,
            end_slot_indicator: None,
            end_slot_always_visible: false,
            hover_group: None,
            leading_divider: None,
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

    /// Sizes a [`TabStyle::Pill`] tab as a pane's document tab: between 120 and 200px wide,
    /// with its label left-aligned and truncated, and an 18px close slot.
    ///
    /// The children must be able to shrink (`min_w_0`) for the label to truncate.
    pub fn document(mut self, document: bool) -> Self {
        self.document = document;
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

    pub fn style(mut self, style: TabStyle) -> Self {
        self.style = style;
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

    /// A status mark, like a modified file's dot, shown in the end slot of a
    /// [`TabStyle::Pill`] tab while the pointer is not over it. The end slot's button takes its
    /// place on hover.
    pub fn end_slot_indicator<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.end_slot_indicator = element.into().map(IntoElement::into_any_element);
        self
    }

    /// Keeps a [`TabStyle::Pill`] tab's end slot visible on unselected tabs too, for the
    /// `"always"` close button setting. An indicator still owns the slot until hover.
    pub fn end_slot_always_visible(mut self, visible: bool) -> Self {
        self.end_slot_always_visible = visible;
        self
    }

    /// Names this [`TabStyle::Capsule`] tab's hover group, so the dividers on either side of it
    /// can hide while the pointer is over it.
    pub fn hover_group(mut self, group: impl Into<SharedString>) -> Self {
        self.hover_group = Some(group.into());
        self
    }

    /// Draws a divider in the gap before this [`TabStyle::Capsule`] tab. It takes the previous
    /// tab's [`Tab::hover_group`], because hovering either neighbour hides it.
    pub fn leading_divider(mut self, previous_hover_group: Option<SharedString>) -> Self {
        self.leading_divider = previous_hover_group;
        self
    }

    pub fn content_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx) - px(1.)
    }

    pub fn container_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx)
    }

    /// Height of a [`TabStyle::Pill`] tab.
    pub fn pill_height(cx: &App) -> Pixels {
        DynamicSpacing::Base26.px(cx)
    }

    fn render_pill(self, cx: &App) -> Stateful<Div> {
        let colors = cx.theme().colors();
        let selected = self.selected;
        let full_width = self.full_width;
        let document = self.document;
        let close_side = self.close_side;
        let capsule = self.style == TabStyle::Capsule;
        let own_hover_group = self.hover_group.clone();
        let leading_divider = self.leading_divider.map(|previous_hover_group| {
            // GPUI keeps one group-hover style per element, so each neighbour's hover hides a
            // different element of the pair.
            let divider = div()
                .when_some(own_hover_group, |this, group| {
                    this.group_hover(group, |style| style.invisible())
                })
                .child(group_divider(px(14.)));
            h_flex()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-1.5))
                .group_hover(previous_hover_group, |style| style.invisible())
                .child(divider)
        });
        let text_color = if selected {
            colors.text
        } else {
            colors.text_muted
        };

        // The close button only shows where it is unambiguous what it closes: on the selected
        // tab, or on the tab under the pointer. An indicator owns the slot until the pointer
        // arrives, even on the selected tab, or a modified file would never show its dot.
        let has_end_slot = self.end_slot.is_some();
        let has_indicator = self.end_slot_indicator.is_some();
        let end_slot_on_hover_only = has_indicator || (!selected && !self.end_slot_always_visible);
        let action_slot = (has_end_slot || has_indicator).then(|| {
            h_flex()
                .relative()
                .size(if document {
                    DOCUMENT_TAB_CLOSE_SLOT_SIZE
                } else {
                    TAB_SLOT_SIZE
                })
                .flex_none()
                .justify_center()
                .when_some(self.end_slot_indicator, |this, indicator| {
                    this.child(
                        h_flex()
                            .absolute()
                            .inset_0()
                            .justify_center()
                            .when(has_end_slot, |this| {
                                this.group_hover("", |style| style.invisible())
                            })
                            .child(indicator),
                    )
                })
                .when_some(self.end_slot, |this, end_slot| {
                    this.child(
                        h_flex()
                            .size_full()
                            .justify_center()
                            .when(end_slot_on_hover_only, |this| this.visible_on_hover(""))
                            .child(end_slot),
                    )
                })
        });
        let icon_slot = self.start_slot.map(|start_slot| {
            h_flex()
                .size(TAB_SLOT_SIZE)
                .flex_none()
                .justify_center()
                .child(start_slot)
        });
        let (leading_action, trailing_action) = match close_side {
            TabCloseSide::Start => (action_slot, None),
            TabCloseSide::End => (None, action_slot),
        };

        self.div
            .when_some(self.hover_group, |this, group| this.group(group))
            .relative()
            .flex_none()
            .h(Tab::pill_height(cx))
            .when(full_width, |this| this.w_full())
            .when(document, |this| {
                this.min_w(DOCUMENT_TAB_MIN_WIDTH)
                    .max_w(DOCUMENT_TAB_MAX_WIDTH)
            })
            .rounded(TAB_PILL_RADIUS)
            .border_1()
            .map(|this| {
                if selected && capsule {
                    this.bg(colors.element_selected)
                        .border_color(gpui::transparent_black())
                } else if selected {
                    this.bg(colors.elevated_surface_background)
                        .border_color(colors.border_selected)
                        .shadow(vec![
                            gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.5))
                                .blur_radius(px(2.)),
                        ])
                } else {
                    this.border_color(gpui::transparent_black())
                        .hover(|style| style.bg(colors.ghost_element_hover))
                        .active(|style| style.bg(colors.ghost_element_active))
                }
            })
            .cursor_pointer()
            .children(leading_divider)
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h_full()
                    .px(DynamicSpacing::Base10.px(cx))
                    .gap(DynamicSpacing::Base06.px(cx))
                    .when(full_width, |this| this.w_full().justify_center())
                    .when(document, |this| {
                        // The close square already has air around its glyph, so the full
                        // inset on its side would push the glyph away from the edge.
                        let (start, end) = match close_side {
                            TabCloseSide::Start => (DynamicSpacing::Base04, DynamicSpacing::Base10),
                            TabCloseSide::End => (DynamicSpacing::Base10, DynamicSpacing::Base04),
                        };
                        this.w_full().pl(start.px(cx)).pr(end.px(cx))
                    })
                    .text_color(text_color)
                    .children(leading_action)
                    .children(icon_slot)
                    .map(|this| {
                        if document {
                            this.child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .children(self.children),
                            )
                        } else {
                            this.children(self.children)
                        }
                    })
                    .children(trailing_action),
            )
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
        if matches!(self.style, TabStyle::Pill | TabStyle::Capsule) {
            return self.render_pill(cx);
        }

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
