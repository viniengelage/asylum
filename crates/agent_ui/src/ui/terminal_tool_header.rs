use std::time::Duration;

use gpui::{AnyElement, ClickEvent, FontWeight, Window};
use i18n::t;
use ui::{CommonAnimationExt, Disclosure, Tooltip, prelude::*, status_chip};
use util::time::duration_alt_display;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub struct TerminalSandboxWarning {
    pub title: SharedString,
    pub detail: SharedString,
    pub docs_url: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCardState {
    AwaitingApproval,
    Running,
    Stopped,
    Failed,
    Succeeded,
}

#[derive(IntoElement, RegisterComponent)]
pub struct TerminalToolHeader {
    id: SharedString,
    hover_group: SharedString,
    working_dir: SharedString,
    command_text: Option<SharedString>,
    is_expanded: bool,
    elapsed: Option<Duration>,
    running: bool,
    awaiting_approval: bool,
    canceled: bool,
    failed: bool,
    exit_code: Option<i64>,
    sandbox_warning: Option<TerminalSandboxWarning>,
    on_toggle_expand: Option<ClickHandler>,
    on_stop: Option<ClickHandler>,
    command_slot: Option<AnyElement>,
}

impl TerminalToolHeader {
    pub fn new(
        id: impl Into<SharedString>,
        hover_group: impl Into<SharedString>,
        working_dir: impl Into<SharedString>,
        is_expanded: bool,
    ) -> Self {
        Self {
            id: id.into(),
            hover_group: hover_group.into(),
            working_dir: working_dir.into(),
            command_text: None,
            is_expanded,
            elapsed: None,
            running: false,
            awaiting_approval: false,
            canceled: false,
            failed: false,
            exit_code: None,
            sandbox_warning: None,
            on_toggle_expand: None,
            on_stop: None,
            command_slot: None,
        }
    }

    pub fn command_text(mut self, command_text: impl Into<SharedString>) -> Self {
        self.command_text = Some(command_text.into());
        self
    }

    pub fn elapsed(mut self, elapsed: Duration) -> Self {
        self.elapsed = Some(elapsed);
        self
    }

    pub fn running(mut self, running: bool) -> Self {
        self.running = running;
        self
    }

    pub fn awaiting_approval(mut self, awaiting_approval: bool) -> Self {
        self.awaiting_approval = awaiting_approval;
        self
    }

    pub fn canceled(mut self, canceled: bool) -> Self {
        self.canceled = canceled;
        self
    }

    pub fn failed(mut self, exit_code: Option<i64>) -> Self {
        self.failed = true;
        self.exit_code = exit_code;
        self
    }

    pub fn exit_code(mut self, exit_code: Option<i64>) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub fn sandbox_warning(mut self, warning: TerminalSandboxWarning) -> Self {
        self.sandbox_warning = Some(warning);
        self
    }

    pub fn on_toggle_expand(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle_expand = Some(Box::new(handler));
        self
    }

    pub fn on_stop(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_stop = Some(Box::new(handler));
        self
    }

    pub fn command_slot(mut self, element: impl IntoElement) -> Self {
        self.command_slot = Some(element.into_any_element());
        self
    }

    fn state(&self) -> TerminalCardState {
        if self.awaiting_approval {
            TerminalCardState::AwaitingApproval
        } else if self.running {
            TerminalCardState::Running
        } else if self.canceled {
            TerminalCardState::Stopped
        } else if self.failed {
            TerminalCardState::Failed
        } else {
            TerminalCardState::Succeeded
        }
    }
}

impl RenderOnce for TerminalToolHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.state();
        let Self {
            id,
            hover_group,
            working_dir,
            command_text,
            is_expanded,
            elapsed,
            running,
            exit_code,
            sandbox_warning,
            on_toggle_expand,
            on_stop,
            command_slot,
            ..
        } = self;

        let child_id = |name: &str| format!("terminal-tool-{name}-{id}");
        let colors = cx.theme().colors();

        let (icon_color, state_label) = match state {
            TerminalCardState::AwaitingApproval => (
                Color::Warning,
                t!("agent_ui.terminal_card.awaiting_approval"),
            ),
            TerminalCardState::Running => (Color::Accent, t!("agent_ui.terminal_card.running")),
            TerminalCardState::Stopped => (Color::Muted, t!("agent_ui.terminal_card.stopped")),
            TerminalCardState::Failed => (Color::Error, t!("agent_ui.terminal_card.failed")),
            TerminalCardState::Succeeded => (Color::Muted, t!("agent_ui.terminal_card.succeeded")),
        };
        let finished = matches!(
            state,
            TerminalCardState::Failed | TerminalCardState::Succeeded | TerminalCardState::Stopped
        );

        let header_row = h_flex()
            .id(child_id("header"))
            .h(px(36.))
            .px_3()
            .gap_2()
            .flex_none()
            .when(state == TerminalCardState::Failed, |this| {
                this.debug_selector(|| format!("terminal-tool-failed-{exit_code:?}"))
            })
            .child(
                Icon::new(IconName::ToolTerminal)
                    .size(IconSize::Small)
                    .color(icon_color),
            )
            .map(|header| match command_text.clone().filter(|_| finished) {
                Some(command_text) => header.child(
                    div().flex_1().min_w_0().overflow_hidden().child(
                        Label::new(command_text)
                            .buffer_font(cx)
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    ),
                ),
                None => header.child(
                    Label::new(state_label.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .single_line(),
                ),
            })
            .when_some(elapsed, |header, elapsed| {
                header.child(
                    Label::new(duration_alt_display(elapsed))
                        .buffer_font(cx)
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                )
            })
            .when(!finished || command_text.is_none(), |header| {
                header.child(div().flex_1())
            })
            .when_some(sandbox_warning, |header, warning| {
                let TerminalSandboxWarning {
                    title,
                    detail,
                    docs_url,
                } = warning;
                header.child(
                    status_chip(
                        Some(IconName::LockOff),
                        t!("agent_ui.sandbox.chip_off"),
                        Color::Warning,
                        cx,
                    )
                    .id(child_id("sandbox-not-applied"))
                    .cursor_pointer()
                    .tooltip(move |_window, cx| {
                        Tooltip::with_meta(
                            title.clone(),
                            None,
                            format!("{detail} {}", t!("agent_ui.terminal_card.sandbox_docs")),
                            cx,
                        )
                    })
                    .on_click(move |_, _, cx| cx.open_url(&docs_url)),
                )
            })
            .when(state == TerminalCardState::Stopped, |header| {
                header.child(status_chip(None, state_label.clone(), Color::Muted, cx))
            })
            .when(finished && state != TerminalCardState::Stopped, |header| {
                header.when_some(exit_code, |header, code| {
                    header.child(status_chip(
                        None,
                        format!("exit {code}"),
                        if code == 0 {
                            Color::Success
                        } else {
                            Color::Error
                        },
                        cx,
                    ))
                })
            })
            .when(running, |header| {
                header
                    .child(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::Small)
                            .color(Color::Accent)
                            .with_rotate_animation(2),
                    )
                    .when_some(on_stop, |header, handler| {
                        header.child(
                            h_flex()
                                .id(child_id("stop"))
                                .debug_selector(|| "ICON-Stop".into())
                                .h(px(24.))
                                .px_2()
                                .gap_1p5()
                                .rounded_sm()
                                .border_1()
                                .border_color(colors.border_selected)
                                .cursor_pointer()
                                .hover(|this| this.bg(colors.element_hover))
                                .child(
                                    Icon::new(IconName::Stop)
                                        .size(IconSize::XSmall)
                                        .color(Color::Error),
                                )
                                .child(
                                    Label::new(t!("agent_ui.terminal_card.stop"))
                                        .size(LabelSize::Small)
                                        .weight(FontWeight::MEDIUM),
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::with_meta(
                                        t!("agent_ui.terminal_card.stop_tooltip"),
                                        None,
                                        t!("agent_ui.terminal_card.stop_tooltip_meta"),
                                        cx,
                                    )
                                })
                                .on_click(handler),
                        )
                    })
            })
            .child(
                Disclosure::new(child_id("disclosure"), is_expanded)
                    .opened_icon(IconName::ChevronUp)
                    .closed_icon(IconName::ChevronDown)
                    .visible_on_hover(&hover_group)
                    .when_some(on_toggle_expand, |this, handler| this.on_click(handler)),
            );

        let command_block = v_flex()
            .py_2()
            .px_3()
            .gap_1p5()
            .bg(colors.editor_background)
            .border_t_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        Label::new("$")
                            .buffer_font(cx)
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    )
                    .child(div().flex_1().min_w_0().children(command_slot)),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Label::new(working_dir)
                                .buffer_font(cx)
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder)
                                .truncate_start(),
                        ),
                    ),
            );

        v_flex()
            .group(hover_group)
            .text_xs()
            .bg(colors.elevated_surface_background)
            .child(header_row)
            .when(!finished || is_expanded || command_text.is_none(), |this| {
                this.child(command_block)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn running_header_only_offers_stop_when_control_is_available(cx: &mut gpui::TestAppContext) {
        struct Header {
            stoppable: bool,
            stops: usize,
        }
        impl Render for Header {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().w(px(400.)).child(
                    TerminalToolHeader::new("test", "test", "/project", true)
                        .running(true)
                        .when(self.stoppable, |header| {
                            header.on_stop(cx.listener(|this, _, _, _| this.stops += 1))
                        }),
                )
            }
        }

        crate::test_support::init_test(cx);
        let (header, cx) = cx.add_window_view(|_, _| Header {
            stoppable: false,
            stops: 0,
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("ICON-Stop").is_none());

        header.update(cx, |header, cx| {
            header.stoppable = true;
            cx.notify();
        });
        let stop = cx
            .debug_bounds("ICON-Stop")
            .expect("owned process Stop control");
        cx.simulate_click(stop.center(), gpui::Modifiers::default());
        assert_eq!(header.read_with(cx, |header, _| header.stops), 1);
    }
}

impl Component for TerminalToolHeader {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn name() -> &'static str {
        "Terminal Tool Header"
    }

    fn description() -> &'static str {
        "The top of a terminal tool call card in the agent panel."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        let working_dir = "/Users/you/projects/zed";

        let card = |_id: &'static str, header: TerminalToolHeader| {
            v_flex()
                .w_full()
                .border_1()
                .border_color(cx.theme().colors().border.opacity(0.6))
                .rounded_md()
                .overflow_hidden()
                .child(
                    header.command_slot(
                        div().px_1p5().pb_1().child(
                            Label::new("cargo build --release")
                                .buffer_font(cx)
                                .size(LabelSize::XSmall),
                        ),
                    ),
                )
                .into_any_element()
        };

        let sandbox_warning = || TerminalSandboxWarning {
            title: "Ran without sandbox".into(),
            detail: "Unsandboxed execution is allowed for the rest of this thread.".into(),
            docs_url: "https://github.com/viniengelage/asylum/blob/main/docs/src/ai/sandboxing.md"
                .into(),
        };

        v_flex()
            .gap_4()
            .child(example_group(vec![
                single_example(
                    "Running",
                    card(
                        "running",
                        TerminalToolHeader::new(
                            "running",
                            "preview-terminal-header-group-running",
                            working_dir,
                            false,
                        )
                        .running(true),
                    ),
                ),
                single_example(
                    "Finished (long-running)",
                    card(
                        "elapsed",
                        TerminalToolHeader::new(
                            "elapsed",
                            "preview-terminal-header-group-elapsed",
                            working_dir,
                            false,
                        )
                        .elapsed(Duration::from_secs(83)),
                    ),
                ),
                single_example(
                    "Finished with exit 0",
                    card(
                        "truncated",
                        TerminalToolHeader::new(
                            "truncated",
                            "preview-terminal-header-group-truncated",
                            working_dir,
                            true,
                        )
                        .exit_code(Some(0)),
                    ),
                ),
                single_example(
                    "Failed with exit code",
                    card(
                        "failed",
                        TerminalToolHeader::new(
                            "failed",
                            "preview-terminal-header-group-failed",
                            working_dir,
                            false,
                        )
                        .failed(Some(101)),
                    ),
                ),
                single_example(
                    "Ran without sandbox",
                    card(
                        "sandbox",
                        TerminalToolHeader::new(
                            "sandbox",
                            "preview-terminal-header-group-sandbox",
                            working_dir,
                            false,
                        )
                        .sandbox_warning(sandbox_warning()),
                    ),
                ),
                single_example(
                    "Long path (truncated from the start)",
                    div()
                        .w_80()
                        .child(card(
                            "long-path",
                            TerminalToolHeader::new(
                                "long-path",
                                "preview-terminal-header-group-long-path",
                                "/Users/you/Documents/GitHub/worktrees/some-monorepo/working-tree-three/packages/deeply/nested/service/backend/src",
                                false,
                            ),
                        ))
                        .into_any_element(),
                ),
                single_example(
                    "Everything at once",
                    card(
                        "kitchen-sink",
                        TerminalToolHeader::new(
                            "kitchen-sink",
                            "preview-terminal-header-group-kitchen-sink",
                            working_dir,
                            true,
                        )
                        .elapsed(Duration::from_secs(3671))
                        .failed(Some(1))
                        .sandbox_warning(sandbox_warning()),
                    ),
                ),
            ]))
            .into_any_element()
    }
}
