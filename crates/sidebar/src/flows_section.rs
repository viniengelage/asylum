//! The ESTEIRAS section: the recent runs, live while their tab is open, each grouping the threads
//! of its steps. A run the app closed in the middle of can resume.

use crate::Sidebar;
use agent_ui::ThreadId;
use agent_ui::flows_view::history::{FlowRunRecord, RecordStatus};
use gpui::{AnyElement, App, Context, Window};
use std::time::SystemTime;
use ui::{Tooltip, prelude::*};

/// The runs shown without "Ver todas".
const VISIBLE_RUNS: usize = 5;

fn status_text(status: RecordStatus) -> (&'static str, Color) {
    match status {
        RecordStatus::Running => ("rodando", Color::Accent),
        RecordStatus::WaitingForYou => ("esperando você", Color::Warning),
        RecordStatus::Succeeded => ("concluída", Color::Success),
        RecordStatus::Cancelled => ("parada", Color::Error),
        RecordStatus::Interrupted => ("interrompida", Color::Warning),
    }
}

fn elapsed_label(at: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(at)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    match seconds {
        0..60 => "agora".to_string(),
        60..3600 => format!("{} min", seconds / 60),
        3600..86400 => format!("{} h", seconds / 3600),
        _ => format!("{} d", seconds / 86400),
    }
}

impl Sidebar {
    fn open_flow_run(&mut self, id: String, resume: bool, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            agent_ui::flows_view::open_run(workspace, id, resume, window, cx)
        });
    }

    fn open_flow_step_thread(&mut self, thread_id: ThreadId, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            agent_ui::flows_view::open_step_thread(workspace, thread_id, window, cx)
        });
    }

    pub(crate) fn render_flows_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let records = agent_ui::flows_view::history::records(cx);
        if records.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let collapsed = self.flows_section_collapsed;
        let live = records
            .iter()
            .filter(|(record, is_live)| {
                matches!(
                    record.status(*is_live),
                    RecordStatus::Running | RecordStatus::WaitingForYou
                )
            })
            .count();
        let header = h_flex()
            .id("flows-section-header")
            .px_3()
            .pt_2()
            .pb_1()
            .gap_1()
            .cursor_pointer()
            .child(
                Label::new("ESTEIRAS")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(live > 0, |this| {
                this.child(
                    Label::new(format!("{live} ativa{}", if live == 1 { "" } else { "s" }))
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .on_click(cx.listener(|this, _, _window, cx| {
                this.flows_section_collapsed = !this.flows_section_collapsed;
                this.serialize(cx);
                cx.notify();
            }));
        let mut section = v_flex()
            .flex_none()
            .pb_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(header);
        if !collapsed {
            let shown = if self.flows_section_show_all {
                records.len()
            } else {
                VISIBLE_RUNS
            };
            for (record, is_live) in records.iter().take(shown) {
                section = section.child(self.render_flow_run(record, *is_live, cx));
            }
            if records.len() > VISIBLE_RUNS {
                section = section.child(
                    h_flex().px_3().pt_0p5().child(
                        Button::new(
                            "flows-show-all",
                            if self.flows_section_show_all {
                                "Ver menos".to_string()
                            } else {
                                format!("Ver todas ({})", records.len())
                            },
                        )
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Transparent)
                        .color(Color::Muted)
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.flows_section_show_all = !this.flows_section_show_all;
                            cx.notify();
                        })),
                    ),
                );
            }
        }
        Some(section.into_any_element())
    }

    fn render_flow_run(
        &self,
        record: &FlowRunRecord,
        is_live: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status = record.status(is_live);
        let (status_label, status_color) = status_text(status);
        let total = record.snapshot.steps.len();
        let expanded = self.expanded_flow_runs.contains(&record.id);
        let progress = match status {
            RecordStatus::Succeeded => format!("{total} de {total}"),
            _ => format!(
                "passo {} de {total}",
                (record.snapshot.cursor + 1).min(total)
            ),
        };
        let id = record.id.clone();
        let toggle_id = record.id.clone();
        let resume_id = record.id.clone();
        let forget_id = record.id.clone();
        let element_id = SharedString::from(format!("flow-run-{}", record.id));
        let row = h_flex()
            .id(element_id)
            .mx_1()
            .px_2()
            .py_1()
            .gap_1p5()
            .rounded_md()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .child(
                IconButton::new(
                    SharedString::from(format!("flow-run-expand-{}", record.id)),
                    if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text("Threads dos passos"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if !this.expanded_flow_runs.remove(&toggle_id) {
                        this.expanded_flow_runs.insert(toggle_id.clone());
                    }
                    cx.notify();
                })),
            )
            .child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(status_color.color(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(record.flow_name.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!("{progress} · {status_label}"))
                            .size(LabelSize::XSmall)
                            .color(status_color),
                    ),
            )
            .when(status == RecordStatus::Interrupted, |this| {
                this.child(
                    Button::new(
                        SharedString::from(format!("flow-run-resume-{}", record.id)),
                        "Retomar",
                    )
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_flow_run(resume_id.clone(), true, window, cx)
                    })),
                )
            })
            .when(
                !is_live && !matches!(status, RecordStatus::Interrupted),
                |this| {
                    this.child(
                        IconButton::new(
                            SharedString::from(format!("flow-run-forget-{}", record.id)),
                            IconName::Close,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Tirar do histórico"))
                        .on_click(move |_, _window, cx| {
                            agent_ui::flows_view::history::forget(&forget_id, cx)
                        }),
                    )
                },
            )
            .child(
                Label::new(elapsed_label(record.updated_at))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_flow_run(id.clone(), false, window, cx)
            }));
        let mut group = v_flex().child(row);
        if expanded {
            for (index, label) in record.step_labels.iter().enumerate() {
                let state = record.snapshot.steps.get(index).map(|step| step.state);
                let thread = record
                    .step_threads
                    .get(index)
                    .and_then(|threads| threads.last().copied());
                let state_color = match state {
                    Some(flows::run::StepState::Passed) => Color::Success,
                    Some(flows::run::StepState::Failed) => Color::Error,
                    Some(flows::run::StepState::WaitingForYou) => Color::Warning,
                    Some(flows::run::StepState::Running | flows::run::StepState::Verifying) => {
                        Color::Accent
                    }
                    _ => Color::Muted,
                };
                group = group.child(
                    h_flex()
                        .id(SharedString::from(format!(
                            "flow-run-step-{}-{index}",
                            record.id
                        )))
                        .ml(px(30.))
                        .mr_1()
                        .px_2()
                        .py_0p5()
                        .gap_1p5()
                        .rounded_md()
                        .when(thread.is_some(), |this| {
                            this.cursor_pointer()
                                .hover(|this| this.bg(cx.theme().colors().element_hover))
                        })
                        .child(
                            Label::new(format!("{}", index + 1))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .child(
                            Label::new(label.clone())
                                .size(LabelSize::XSmall)
                                .color(state_color)
                                .truncate(),
                        )
                        .when_some(thread, |this, thread_id| {
                            this.child(div().flex_1())
                                .child(
                                    Icon::new(IconName::Thread)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_flow_step_thread(thread_id, window, cx)
                                }))
                        }),
                );
            }
        }
        group
    }

    pub(crate) fn observe_flow_runs(cx: &mut Context<Self>) -> gpui::Subscription {
        cx.observe_global::<agent_ui::flows_view::FlowRunsRevision>(|_, cx| cx.notify())
    }
}
