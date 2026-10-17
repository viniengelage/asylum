use super::{ThreadView, ToolCallLayout};
use acp_thread::{ToolCall, ToolCallStatus};
use gpui::{AnyElement, FontWeight};
use task_agents::{PROJECT_ITEMS_TOOL, ProjectItemsCard};
use ui::prelude::*;

/// The card `project_items` leaves in the thread: the items, startable from there.
pub(super) fn project_card(tool_call: &ToolCall) -> Option<ProjectItemsCard> {
    if !matches!(tool_call.status(), ToolCallStatus::Completed)
        || tool_call.tool_name.as_deref()? != PROJECT_ITEMS_TOOL
    {
        return None;
    }
    serde_json::from_value(tool_call.raw_output.clone()?).ok()
}

/// GitHub's names for the colors a project's options can have, in the theme's terms.
fn status_color(color: Option<&str>) -> Color {
    match color {
        Some("BLUE") => Color::Info,
        Some("GREEN") => Color::Success,
        Some("YELLOW" | "ORANGE") => Color::Warning,
        Some("RED") => Color::Error,
        Some("PURPLE" | "PINK") => Color::Accent,
        _ => Color::Muted,
    }
}

impl ThreadView {
    pub(super) fn render_project_card(
        &self,
        entry_ix: usize,
        card: ProjectItemsCard,
        layout: ToolCallLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let count = card.items.len();
        v_flex()
            .when(layout == ToolCallLayout::Standalone, |this| this.ml_5().mr_5())
            .mt_1()
            .mb_2()
            .rounded_lg()
            .border_1()
            .border_color(self.tool_card_border_color(cx))
            .bg(colors.element_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .gap_2()
                    .px_3p5()
                    .pt_3()
                    .pb_2p5()
                    .child(Icon::new(IconName::Blocks).size(IconSize::Small).color(Color::Muted))
                    .child(
                        Label::new(card.title.clone())
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .bg(colors.element_hover)
                            .child(Label::new(count.to_string()).size(LabelSize::XSmall)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .px_3p5()
                    .py_2p5()
                    .border_t_1()
                    .border_color(colors.border)
                    .children(card.items.iter().enumerate().map(|(index, item)| {
                        let item_id = item.item_id.clone();
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(7.))
                                    .rounded_full()
                                    .bg(status_color(item.status_color.as_deref()).color(cx)),
                            )
                            .child(
                                div().min_w_0().child(
                                    Label::new(format!(
                                        "{} {}",
                                        item.reference
                                            .find('#')
                                            .map_or(item.reference.as_str(), |at| &item.reference[at..]),
                                        item.title
                                    ))
                                    .size(LabelSize::Small)
                                    .truncate(),
                                ),
                            )
                            .children(item.detail.clone().map(|detail| {
                                Label::new(detail)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                            }))
                            .child(div().flex_1())
                            .when(item.can_start, |this| {
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("project-card-start-{entry_ix}-{index}")),
                                        "Começar",
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::PlayOutlined).size(IconSize::XSmall).color(Color::Muted))
                                    .on_click(move |_, window, cx| {
                                        window.dispatch_action(
                                            Box::new(zed_actions::github_projects::StartProjectItem {
                                                item_id: item_id.clone(),
                                            }),
                                            cx,
                                        )
                                    }),
                                )
                            })
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Button::new(SharedString::from(format!("project-card-board-{entry_ix}")), "Abrir quadro")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Blocks).size(IconSize::XSmall).color(Color::Muted))
                            .on_click(|_, window, cx| {
                                if let Ok(action) = cx.build_action("github_projects::OpenBoard", None) {
                                    window.dispatch_action(action, cx);
                                }
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("project-card-dock-{entry_ix}")), "Ver no dock")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ListTodo).size(IconSize::XSmall).color(Color::Muted))
                            .on_click(|_, window, cx| {
                                if let Ok(action) = cx.build_action("github_projects::ToggleFocus", None) {
                                    window.dispatch_action(action, cx);
                                }
                            }),
                    ),
            )
            .into_any_element()
    }
}
