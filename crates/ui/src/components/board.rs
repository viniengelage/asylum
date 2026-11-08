use gpui::{AnyElement, ClickEvent, Div, FontWeight, Hsla, Stateful, px};
use smallvec::SmallVec;

use crate::{card, grouped_block, prelude::*};

pub fn board_row() -> Div {
    h_flex().flex_1().min_h_0().items_start().gap_3().p_4()
}

/// The chrome of a board card: highlighted while it is the one inspected, faded while a move
/// is waiting on the server.
pub fn board_card(
    id: impl Into<ElementId>,
    selected: bool,
    pending: bool,
    cx: &App,
) -> Stateful<Div> {
    let accent = cx.theme().colors().border_focused;
    card(cx)
        .id(id)
        .gap_2()
        .p_3()
        .rounded(Radius::Md.rems())
        .cursor_pointer()
        .when(selected, |this| {
            this.bg(accent.opacity(0.16)).border_color(accent)
        })
        .when(!selected, |this| {
            this.hover(|this| this.border_color(accent.opacity(0.5)))
        })
        .when(pending, |this| this.opacity(0.6))
}

pub struct BoardDragPreview {
    title: SharedString,
}

impl BoardDragPreview {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl Render for BoardDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        card(cx)
            .px_3()
            .py_2()
            .rounded(Radius::Md.rems())
            .border_color(cx.theme().colors().border_focused)
            .shadow_md()
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

type ApplyDrop = Box<dyn FnOnce(Stateful<Div>, Hsla) -> Stateful<Div>>;

/// The dragged payload's type is only known to the board, so the drop handling is kept as
/// closures that add it to the column and to the drop slot.
struct DropTarget {
    column: ApplyDrop,
    slot: ApplyDrop,
}

type ShowMore = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct BoardColumn {
    key: SharedString,
    leading: AnyElement,
    title: SharedString,
    count: usize,
    muted: bool,
    drop_target: Option<DropTarget>,
    children: SmallVec<[AnyElement; 4]>,
    show_more: Option<(usize, ShowMore)>,
}

impl BoardColumn {
    /// `count` is the column's full size, which can be more than the cards it shows.
    pub fn new(
        key: impl Into<SharedString>,
        leading: impl IntoElement,
        title: impl Into<SharedString>,
        count: usize,
    ) -> Self {
        Self {
            key: key.into(),
            leading: leading.into_any_element(),
            title: title.into(),
            count,
            muted: false,
            drop_target: None,
            children: SmallVec::new(),
            show_more: None,
        }
    }

    /// Fades the column, for the done and closed ones that are history.
    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// Makes the column take cards dragged as `T`; while one is dragged, a slot at the end says
    /// where it goes.
    pub fn on_drop<T: 'static>(
        mut self,
        handler: impl Fn(&T, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.drop_target = Some(DropTarget {
            column: Box::new(move |column, accent| {
                column
                    .drag_over::<T>(move |style, _, _, _| style.bg(accent.opacity(0.06)))
                    .on_drop(handler)
            }),
            slot: Box::new(|slot, accent| {
                slot.drag_over::<T>(move |style, _, _, _| {
                    style.border_color(accent).bg(accent.opacity(0.1))
                })
            }),
        });
        self
    }

    /// Adds a "+ N items" button under the cards when `hidden` of them are left out.
    pub fn show_more(
        mut self,
        hidden: usize,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.show_more = (hidden > 0).then(|| (hidden, Box::new(handler) as ShowMore));
        self
    }
}

impl ParentElement for BoardColumn {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for BoardColumn {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let accent = cx.theme().colors().border_focused;
        let border = cx.theme().colors().border;
        let dragging = self.drop_target.is_some() && cx.has_active_drag();
        let (on_column, on_slot) = match self.drop_target {
            Some(target) => (Some(target.column), Some(target.slot)),
            None => (None, None),
        };
        let column = grouped_block(cx)
            .id(SharedString::from(format!("board-column-{}", self.key)))
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_2()
            .rounded(Radius::Lg.rems())
            .overflow_y_scroll()
            .when(self.muted, |this| this.opacity(0.65));
        let column = match on_column {
            Some(apply) => apply(column, accent),
            None => column,
        };
        let drop_label = i18n::t!("ui.board.drop_here", column = self.title);
        column
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px_1()
                    .gap_2()
                    .child(self.leading)
                    .child(
                        Label::new(self.title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::BOLD)
                            .truncate(),
                    )
                    .child(
                        Label::new(self.count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .children(self.children)
            .when_some(on_slot.filter(|_| dragging), |this, apply| {
                let slot = h_flex()
                    .id(SharedString::from(format!("board-drop-{}", self.key)))
                    .h(px(64.))
                    .flex_none()
                    .justify_center()
                    .gap_1p5()
                    .rounded(Radius::Md.rems())
                    .border_1()
                    .border_dashed()
                    .border_color(border)
                    .child(
                        Icon::new(IconName::ArrowDown)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(drop_label)
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    );
                this.child(apply(slot, accent))
            })
            .when_some(self.show_more, |this, (hidden, handler)| {
                this.child(
                    h_flex().flex_none().justify_center().child(
                        Button::new(
                            "board-show-more",
                            i18n::t!("ui.board.more_items", count = hidden),
                        )
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .on_click(handler),
                    ),
                )
            })
    }
}
