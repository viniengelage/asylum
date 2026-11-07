use crate::prelude::*;

#[derive(IntoElement)]
pub struct ListSeparator;

impl RenderOnce for ListSeparator {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Inset from both edges so the separator reads as a pause between groups of rows
        // rather than a cut across the whole menu or panel.
        div()
            .w_full()
            .px(DynamicSpacing::Base08.rems(cx))
            .my(DynamicSpacing::Base04.rems(cx))
            .child(div().h_px().w_full().bg(cx.theme().colors().border))
    }
}
