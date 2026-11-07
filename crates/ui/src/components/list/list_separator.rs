use crate::prelude::*;

#[derive(IntoElement)]
pub struct ListSeparator;

impl RenderOnce for ListSeparator {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .px(DynamicSpacing::Base08.rems(cx))
            .my(DynamicSpacing::Base04.rems(cx))
            .child(div().h_px().w_full().bg(cx.theme().colors().border))
    }
}
