use super::{ThreadView, ToolCallLayout};
use acp_thread::{ToolCall, ToolCallStatus};
use gpui::{AnyElement, FontWeight};
use task_agents::{ContainerAction, DOCKER_INSPECT_TOOL, DOCKER_LOGS_TOOL, DockerContainerResult};
use ui::prelude::*;

pub(super) fn docker_card(tool_call: &ToolCall) -> Option<DockerContainerResult> {
    if !matches!(tool_call.status(), ToolCallStatus::Completed) {
        return None;
    }
    match tool_call.tool_name.as_deref()? {
        DOCKER_LOGS_TOOL | DOCKER_INSPECT_TOOL => {
            serde_json::from_value(tool_call.raw_output.clone()?).ok()
        }
        _ => None,
    }
}

fn state_color(state: &str) -> Color {
    match state {
        "running" => Color::Success,
        "paused" => Color::Warning,
        "restarting" | "dead" => Color::Error,
        _ => Color::Placeholder,
    }
}

impl ThreadView {
    /// The container as the dock shows its row, the last log lines and the ways into the dock.
    pub(super) fn render_docker_card(
        &self,
        entry_ix: usize,
        card: DockerContainerResult,
        layout: ToolCallLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let color = state_color(&card.state);
        let dot_color = match color {
            Color::Placeholder => colors.border_selected,
            color => color.color(cx),
        };
        let label_color = match color {
            Color::Success => Color::Muted,
            color => color,
        };
        let action = |id: &'static str,
                      icon: IconName,
                      label: &'static str,
                      kind: ContainerAction| {
            let container_id = card.id.clone();
            h_flex()
                .id((id, entry_ix))
                .gap_1p5()
                .cursor_pointer()
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(project) = this.project.upgrade() {
                        task_agents::act_on_container(&project, &container_id, kind, window, cx);
                    }
                }))
        };
        v_flex()
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.ml_5().mr_5()
            })
            .mt_1()
            .mb_2()
            .rounded(px(10.))
            .border_1()
            .border_color(self.tool_card_border_color(cx))
            .bg(colors.elevated_surface_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .w_full()
                    .h(px(36.))
                    .px_3()
                    .gap_2()
                    .child(div().flex_none().size(px(8.)).rounded_full().bg(dot_color))
                    .child(
                        Label::new(card.name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::BOLD),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(card.image.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Placeholder)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(card.state_label.clone())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(label_color),
                    ),
            )
            .when(!card.log_lines.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .px_3()
                        .py_2()
                        .gap_0p5()
                        .border_t_1()
                        .border_b_1()
                        .border_color(self.tool_card_border_color(cx))
                        .bg(colors.editor_background)
                        .children(card.log_lines.iter().map(|line| {
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    Label::new(line.time.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Placeholder),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(line.text.clone())
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(if line.error {
                                                Color::Error
                                            } else {
                                                Color::Default
                                            })
                                            .single_line()
                                            .truncate(),
                                    ),
                                )
                        })),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .h(px(34.))
                    .px_3()
                    .justify_between()
                    .child(action(
                        "docker-card-logs",
                        IconName::Reader,
                        "Abrir logs",
                        ContainerAction::Logs,
                    ))
                    .when(card.state == "running", |this| {
                        this.child(action(
                            "docker-card-terminal",
                            IconName::Terminal,
                            "Abrir terminal",
                            ContainerAction::Terminal,
                        ))
                    })
                    .child(action(
                        "docker-card-inspect",
                        IconName::Eye,
                        "Inspecionar",
                        ContainerAction::Inspect,
                    )),
            )
            .into_any_element()
    }
}
