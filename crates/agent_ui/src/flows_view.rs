//! The esteira tab: one flow, its steps with the state of the current run, and the run panel
//! where the gates, the criteria left to you and the steps out of retries wait for an answer.

mod runner;

use crate::{AgentPanel, ThreadId};
use agent::NativeAgentConnection;
use anyhow::{Context as _, Result};
use editor::Editor;
use flows::run::{CriterionOutcome, FlowRun, RunEvent, RunEventKind, RunState, StepState, Waiting};
use flows::{Flow, FlowSource, StepKind};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use project::Project;
use runner::RunningStep;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use task_agents::TaskAgent;
use ui::{Divider, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Workspace,
    dock::StatusBarButton,
    item::{Item, ItemEvent, TabContentParams},
};

/// Runs a flow (esteira) in its own tab, each agent step in a thread of the Agent panel. The
/// composer sends `/<esteira> <pedido>` here.
#[derive(Clone, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = esteiras, name = "Rodar")]
#[serde(deny_unknown_fields)]
pub struct RunFlow {
    /// The flow's id: its file name without `.flow.md`. Without one a picker lists the flows.
    #[serde(default)]
    pub flow: String,
    /// What to build. Without one the flow's own example input is used.
    #[serde(default)]
    pub input: String,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, action: &RunFlow, window, cx| {
            run_flow(workspace, action.clone(), window, cx);
        });
    })
    .detach();
}

/// The native agent loads flows and task agents from trusted folders only, so a cloned
/// repository can't run its commands just by being opened.
fn native_connection(
    workspace: &Workspace,
    cx: &mut App,
) -> Result<Task<Result<Rc<NativeAgentConnection>>>> {
    let panel = workspace
        .panel::<AgentPanel>(cx)
        .context("o painel de agentes não está aberto")?;
    panel.update(cx, |panel, cx| panel.ensure_native_agent_connection(cx));
    let connect = panel
        .read(cx)
        .connection_store()
        .read(cx)
        .entry(&crate::Agent::NativeAgent)
        .map(|entry| entry.read(cx).wait_for_connection())
        .context("abra um projeto para rodar esteiras")?;
    Ok(cx.spawn(async move |_cx| {
        let connected = connect
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        connected
            .connection
            .downcast::<NativeAgentConnection>()
            .context("o Zed Agent não está disponível")
    }))
}

fn run_flow(
    workspace: &mut Workspace,
    action: RunFlow,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let connect = match native_connection(workspace, cx) {
        Ok(connect) => connect,
        Err(error) => {
            show_error(workspace, error, cx);
            return;
        }
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let connection = connect.await?;
        let (flows, agents) = cx.update(|_, cx| {
            (
                connection.flows(&project, cx).to_vec(),
                connection.task_agents(&project, cx).to_vec(),
            )
        })?;
        workspace.update_in(cx, |workspace, window, cx| {
            if action.flow.is_empty() {
                if flows.is_empty() {
                    show_error(
                        workspace,
                        anyhow::anyhow!(
                            "nenhuma esteira em {} nem nas pessoais",
                            flows::PROJECT_FLOWS_DIR
                        ),
                        cx,
                    );
                    return;
                }
                let delegate = FlowPickerDelegate::new(
                    flows,
                    agents,
                    project.clone(),
                    workspace.weak_handle(),
                );
                workspace.toggle_modal(window, cx, |window, cx| {
                    Picker::uniform_list(delegate, window, cx)
                });
                return;
            }
            let Some(flow) = flows.into_iter().find(|flow| flow.id == action.flow) else {
                show_error(
                    workspace,
                    anyhow::anyhow!("a esteira “{}” não existe", action.flow),
                    cx,
                );
                return;
            };
            let input = if action.input.trim().is_empty() {
                flow.input.clone()
            } else {
                action.input.clone()
            };
            open_flow(
                workspace,
                flow,
                agents,
                project.clone(),
                Some(input),
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

/// Opens the flow's tab. With an input the run starts at once; without one the tab waits with
/// the flow's example in "O que construir".
fn open_flow(
    workspace: &mut Workspace,
    flow: Flow,
    agents: Vec<TaskAgent>,
    project: Entity<Project>,
    input: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let example = flow.input.clone();
    let weak_workspace = workspace.weak_handle();
    let view = cx.new(|cx| FlowView::new(flow, agents, project, weak_workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
    view.update(cx, |view, cx| match input {
        Some(input) => view.start(input, window, cx),
        None => view
            .input_editor
            .update(cx, |editor, cx| editor.set_text(example, window, cx)),
    });
}

struct FlowPickerDelegate {
    flows: Vec<Flow>,
    agents: Vec<TaskAgent>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl FlowPickerDelegate {
    fn new(
        flows: Vec<Flow>,
        agents: Vec<TaskAgent>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let matches = (0..flows.len()).collect();
        Self {
            flows,
            agents,
            project,
            workspace,
            matches,
            selected_index: 0,
        }
    }
}

impl PickerDelegate for FlowPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "flow picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Rodar esteira…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .flows
            .iter()
            .enumerate()
            .filter(|(_, flow)| {
                query.is_empty()
                    || flow.name.to_lowercase().contains(&query)
                    || flow.id.contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(flow) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.flows.get(*index))
            .cloned()
        else {
            return;
        };
        let agents = self.agents.clone();
        let project = self.project.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                open_flow(workspace, flow, agents, project, None, window, cx)
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let flow = self.flows.get(*self.matches.get(index)?)?;
        let steps = flow.steps.len();
        let source = match &flow.source {
            FlowSource::Personal => "pessoal".to_string(),
            FlowSource::Project { worktree_root_name } => worktree_root_name.to_string(),
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::ListTree).color(Color::Accent))
                .child(
                    h_flex().gap_2().child(Label::new(flow.name.clone())).child(
                        Label::new(format!("/{}", flow.id))
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    ),
                )
                .end_slot(
                    Label::new(format!(
                        "{steps} passo{} · {source}",
                        if steps == 1 { "" } else { "s" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
    }
}

fn show_error(workspace: &mut Workspace, error: anyhow::Error, cx: &mut Context<Workspace>) {
    use workspace::notifications::NotificationId;
    struct FlowError;
    workspace.show_toast(
        workspace::Toast::new(
            NotificationId::unique::<FlowError>(),
            format!("Esteira: {error:#}"),
        ),
        cx,
    );
}

pub struct FlowView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    flow: Flow,
    agents: Vec<TaskAgent>,
    run: Option<FlowRun>,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
    /// Every thread opened for each step in this run, retries and verifiers included.
    step_threads: Vec<Vec<ThreadId>>,
    running: Option<RunningStep>,
    input_editor: Entity<Editor>,
    adjust_editor: Entity<Editor>,
}

impl FlowView {
    fn new(
        flow: Flow,
        agents: Vec<TaskAgent>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("O que construir", window, cx);
            editor
        });
        let adjust_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("O que ajustar? (opcional)", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            step_threads: vec![Vec::new(); flow.steps.len()],
            flow,
            agents,
            run: None,
            started_at: None,
            finished_at: None,
            running: None,
            input_editor,
            adjust_editor,
        }
    }

    fn start(&mut self, input: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input_editor
            .update(cx, |editor, cx| editor.set_text(input.clone(), window, cx));
        self.stop(None, cx);
        self.step_threads = vec![Vec::new(); self.flow.steps.len()];
        self.run = Some(FlowRun::new(self.flow.clone(), input));
        self.started_at = Some(Instant::now());
        self.finished_at = None;
        self.drive(window, cx);
    }

    fn rerun(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input_editor.read(cx).text(cx);
        self.start(input, window, cx);
    }

    /// Cancels the step that is running and, with a reason, the run itself.
    fn stop(&mut self, reason: Option<&str>, cx: &mut Context<Self>) {
        if let Some(running) = self.running.take()
            && let Some(thread) = running.thread
        {
            thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
        }
        if let (Some(reason), Some(run)) = (reason, &mut self.run) {
            run.cancel(reason);
        }
        notify_runs_changed(cx);
        cx.notify();
    }

    fn after_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drive(window, cx);
    }

    fn open_thread(&self, thread_id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.focus_panel::<AgentPanel>(window, cx);
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.activate_retained_thread(thread_id, true, window, cx)
                    });
                }
            })
            .log_err();
    }

    fn step_label(&self, step: usize) -> String {
        self.flow
            .steps
            .get(step)
            .map(|flow_step| flow_step.label(&self.agents))
            .unwrap_or_default()
    }

    fn is_running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| !run.is_finished())
    }

    fn elapsed_minutes(&self) -> u64 {
        let Some(started_at) = self.started_at else {
            return 0;
        };
        let end = self.finished_at.unwrap_or_else(Instant::now);
        end.duration_since(started_at).as_secs().div_ceil(60)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        h_flex()
            .h(px(50.))
            .flex_none()
            .px_3p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(self.flow.name.clone()).weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(match &self.flow.source {
                    FlowSource::Personal => "pessoal".to_string(),
                    FlowSource::Project { worktree_root_name } => worktree_root_name.to_string(),
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(if running {
                Button::new("stop-flow", "Parar")
                    .start_icon(
                        Icon::new(IconName::Stop)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.stop(Some("você parou a esteira."), cx)
                    }))
                    .into_any_element()
            } else {
                Button::new("run-flow", "Rodar")
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| this.rerun(window, cx)))
                    .into_any_element()
            })
    }

    fn render_board(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let ask = v_flex()
            .gap_1p5()
            .px_3p5()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.panel_background)
            .child(
                Label::new("O QUE CONSTRUIR")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(self.input_editor.clone())
            .child(
                Label::new(
                    "Vira a entrada do 1º passo. Muda a cada execução sem mexer nos passos.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        let mut board = v_flex().gap_2().child(ask);
        for (index, flow_step) in self.flow.steps.iter().enumerate() {
            board = board.child(self.render_step(index, flow_step, cx));
        }
        div()
            .id("flow-board")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .bg(colors.editor_background)
            .px_8()
            .pt_5()
            .pb_10()
            .child(board)
    }

    fn render_step(
        &self,
        index: usize,
        flow_step: &flows::FlowStep,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let state = self
            .run
            .as_ref()
            .and_then(|run| run.steps.get(index))
            .cloned();
        let (icon, icon_color, title, subtitle) = match &flow_step.kind {
            StepKind::Agent(agent_step) => {
                let agent = self
                    .agents
                    .iter()
                    .find(|agent| agent.command == agent_step.agent);
                let model = agent_step
                    .model
                    .clone()
                    .or_else(|| agent.and_then(|agent| agent.model.clone()))
                    .unwrap_or_else(|| "modelo padrão".to_string());
                let mut subtitle = String::new();
                if let Some(target) = &agent_step.where_ {
                    subtitle.push_str(&format!("Onde: {}", target.tool.label()));
                    if let Some(file) = &target.file {
                        subtitle.push_str(&format!(" · {file}"));
                    }
                    subtitle.push_str(" · ");
                }
                subtitle.push_str(&model);
                if let Some(profile) = agent.and_then(|agent| agent.profile.clone()) {
                    subtitle.push_str(&format!(" · {profile}"));
                }
                if !agent_step.instructions.is_empty() {
                    subtitle.push_str(" · + instruções");
                }
                (
                    IconName::Person,
                    Color::Accent,
                    format!("{}  /{}", flow_step.label(&self.agents), agent_step.agent),
                    subtitle,
                )
            }
            StepKind::Gate(gate) => (
                IconName::UserCheck,
                Color::Warning,
                "Você aprova".to_string(),
                if gate.note.is_empty() {
                    "o que o passo anterior entregou".to_string()
                } else {
                    gate.note.clone()
                },
            ),
            StepKind::Run(run_step) => (
                IconName::Terminal,
                Color::Success,
                run_step.command.clone(),
                "comando no projeto · passa se sair 0".to_string(),
            ),
        };
        let is_gate = matches!(flow_step.kind, StepKind::Gate(_));
        let dimmed = state
            .as_ref()
            .is_some_and(|state| state.state == StepState::Queued);
        let threads = self.step_threads.get(index).cloned().unwrap_or_default();
        h_flex()
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(if is_gate {
                cx.theme().status().warning_border
            } else {
                colors.border
            })
            .bg(colors.panel_background)
            .when(dimmed, |this| this.opacity(0.5))
            .child(
                Label::new(format!("{}", index + 1))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .child(
                div()
                    .size(px(30.))
                    .flex_none()
                    .rounded_md()
                    .bg(colors.element_hover)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size(IconSize::Small).color(icon_color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(title)
                            .weight(FontWeight::SEMIBOLD)
                            .when(is_gate, |this| this.color(Color::Warning))
                            .truncate(),
                    )
                    .child(
                        Label::new(subtitle)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when_some(threads.last().copied(), |this, thread_id| {
                this.child(
                    IconButton::new(("open-step-thread", index), IconName::Thread)
                        .icon_size(IconSize::Small)
                        .tooltip(ui::Tooltip::text("Abrir a thread do passo"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_thread(thread_id, window, cx)
                        })),
                )
            })
            .when_some(state, |this, state| {
                this.child(state_chip(state.state, state.note, cx))
            })
    }

    fn render_run_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let panel = v_flex()
            .w(px(380.))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background);
        let Some(run) = &self.run else {
            return panel.child(
                v_flex()
                    .p_8()
                    .gap_4()
                    .items_center()
                    .child(Label::new("Nenhuma execução ainda.").color(Color::Muted))
                    .child(
                        Button::new("run-from-panel", "Rodar esteira")
                            .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.rerun(window, cx))),
                    ),
            );
        };
        let total = run.steps.len();
        let done = run.passed_steps();
        let progress = if total == 0 {
            0.
        } else {
            done as f32 / total as f32
        };
        let header = h_flex()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(Label::new("Execução").weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(format!(
                    "{done} de {total} · {} min",
                    self.elapsed_minutes()
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(if run.is_finished() {
                Button::new("rerun-flow", "Rodar de novo")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, window, cx| this.rerun(window, cx)))
                    .into_any_element()
            } else {
                Button::new("stop-run", "Parar")
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Stop).size(IconSize::XSmall))
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.stop(Some("você parou a esteira."), cx)
                    }))
                    .into_any_element()
            });
        let bar = div().px_4().pt_3().child(
            div()
                .h(px(4.))
                .w_full()
                .rounded_full()
                .bg(colors.element_hover)
                .child(
                    div()
                        .h_full()
                        .w(relative(progress))
                        .rounded_full()
                        .bg(cx.theme().status().success),
                ),
        );
        let mut feed = v_flex().p_4().gap_3p5();
        let last = run.events.len().saturating_sub(1);
        for (index, event) in run.events.iter().enumerate() {
            feed = feed.child(self.render_event(run, index, index == last, event, cx));
        }
        if let RunState::WaitingForYou(Waiting::Criteria { step, criteria }) = &run.state {
            feed = feed.child(self.render_criteria_box(*step, criteria, cx));
        }
        if run.state == RunState::Succeeded {
            feed = feed.child(self.render_done_box(run, cx));
        }
        panel.child(header).child(bar).child(
            div()
                .id("flow-run-feed")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(feed),
        )
    }

    fn render_event(
        &self,
        run: &FlowRun,
        index: usize,
        is_last: bool,
        event: &RunEvent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (icon, color) = match event.kind {
            RunEventKind::You | RunEventKind::Gate => (IconName::UserCheck, Color::Warning),
            RunEventKind::Delivered | RunEventKind::Verified | RunEventKind::Finished => {
                (IconName::Check, Color::Success)
            }
            RunEventKind::Failed => (IconName::Close, Color::Error),
            RunEventKind::Back => (IconName::Undo, Color::Error),
            RunEventKind::Stopped => (IconName::Stop, Color::Error),
        };
        let mut content = v_flex()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child(
                h_flex()
                    .min_h(px(20.))
                    .gap_2()
                    .child(
                        Label::new(event.title.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(clock(event.at))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    ),
            )
            .when(!event.message.is_empty(), |this| {
                this.child(
                    Label::new(event.message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            });

        if let (Some(step), RunEventKind::Verified | RunEventKind::Failed) =
            (event.step, event.kind)
            && event.title.starts_with("Critérios")
        {
            content = content.child(self.render_criteria_results(run, step, event.kind, cx));
        }
        if let (Some(step), RunEventKind::Delivered) = (event.step, event.kind)
            && let Some(thread_id) = self
                .step_threads
                .get(step)
                .and_then(|threads| threads.last().copied())
        {
            content = content.child(
                Button::new(("open-event-thread", index), "Abrir thread")
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_thread(thread_id, window, cx)
                    })),
            );
        }
        if is_last {
            match &run.state {
                RunState::WaitingForYou(Waiting::Gate { .. }) => {
                    content = content.child(self.render_gate_box(cx));
                }
                RunState::WaitingForYou(Waiting::Exhausted { .. }) => {
                    content = content.child(
                        answer_box(BoxTone::Fail, cx).child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("exhausted-stop", "Encerrar")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Some(run) = &mut this.run {
                                                run.answer_exhausted(false);
                                            }
                                            this.after_answer(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("exhausted-retry", "Refazer mais uma vez")
                                        .style(ButtonStyle::Filled)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Some(run) = &mut this.run {
                                                run.answer_exhausted(true);
                                            }
                                            this.after_answer(window, cx);
                                        })),
                                ),
                        ),
                    );
                }
                _ => {}
            }
        }

        h_flex()
            .items_start()
            .gap_2p5()
            .child(
                div()
                    .size(px(20.))
                    .flex_none()
                    .rounded_full()
                    .bg(cx.theme().colors().element_hover)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color)),
            )
            .child(content)
    }

    fn render_criteria_results(
        &self,
        run: &FlowRun,
        step: usize,
        kind: RunEventKind,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let criteria = match &run.flow.steps[step].kind {
            StepKind::Agent(agent_step) => agent_step.criteria.clone(),
            _ => Vec::new(),
        };
        let tone = if kind == RunEventKind::Failed {
            BoxTone::Fail
        } else {
            BoxTone::Plain
        };
        let mut results = answer_box(tone, cx);
        for result in &run.steps[step].criteria {
            let Some(criterion) = criteria.get(result.index) else {
                continue;
            };
            let (icon, color) = match result.outcome {
                CriterionOutcome::Passed => (IconName::Check, Color::Success),
                CriterionOutcome::Failed => (IconName::Close, Color::Error),
                CriterionOutcome::WithYou => (IconName::UserCheck, Color::Warning),
            };
            results = results.child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(if criterion.required {
                                    criterion.text.clone()
                                } else {
                                    format!("{} · opcional", criterion.text)
                                })
                                .size(LabelSize::Small),
                            )
                            .child(
                                Label::new(result.evidence.clone())
                                    .size(LabelSize::XSmall)
                                    .color(if result.outcome == CriterionOutcome::WithYou {
                                        Color::Warning
                                    } else {
                                        Color::Muted
                                    }),
                            ),
                    ),
            );
        }
        results
    }

    fn render_gate_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        answer_box(BoxTone::You, cx)
            .child(
                Label::new("A esteira está parada até você responder.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_2p5()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.adjust_editor.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("gate-adjust", "Pedir ajuste")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let note = this.adjust_editor.read(cx).text(cx);
                                this.adjust_editor
                                    .update(cx, |editor, cx| editor.clear(window, cx));
                                if let Some(run) = &mut this.run {
                                    run.answer_gate(false, &note);
                                }
                                this.after_answer(window, cx);
                            })),
                    )
                    .child(
                        Button::new("gate-approve", "Aprovar e seguir")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(run) = &mut this.run {
                                    run.answer_gate(true, "");
                                }
                                this.after_answer(window, cx);
                            })),
                    ),
            )
    }

    fn render_criteria_box(
        &self,
        step: usize,
        criteria: &[usize],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let texts = match &self.flow.steps[step].kind {
            StepKind::Agent(agent_step) => criteria
                .iter()
                .filter_map(|index| agent_step.criteria.get(*index))
                .map(|criterion| criterion.text.clone())
                .collect::<Vec<_>>()
                .join("; "),
            _ => String::new(),
        };
        let label = self.step_label(step);
        answer_box(BoxTone::You, cx)
            .child(
                Label::new("Critério com você")
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Warning),
            )
            .child(Label::new(texts).size(LabelSize::Small).color(Color::Muted))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("criteria-accept", "Aceitar sem")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener({
                                let label = label.clone();
                                move |this, _, window, cx| {
                                    if let Some(run) = &mut this.run {
                                        run.answer_criteria(step, &label, true);
                                    }
                                    this.after_answer(window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("criteria-redo", "Pedir para refazer")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(run) = &mut this.run {
                                    run.answer_criteria(step, &label, false);
                                }
                                this.after_answer(window, cx);
                            })),
                    ),
            )
    }

    fn render_done_box(&self, run: &FlowRun, cx: &mut Context<Self>) -> impl IntoElement {
        let answers = run
            .events
            .iter()
            .skip(1)
            .filter(|event| event.kind == RunEventKind::You)
            .count();
        let total = |value: String, caption: &'static str| {
            v_flex()
                .flex_1()
                .child(Label::new(value).weight(FontWeight::SEMIBOLD))
                .child(
                    Label::new(caption)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        answer_box(BoxTone::Done, cx)
            .child(
                Label::new("Pronto para revisão")
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Success),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(total(
                        format!("{} min", self.elapsed_minutes()),
                        "tempo total",
                    ))
                    .child(total(
                        format!(
                            "{}",
                            run.steps
                                .iter()
                                .map(|step| step.attempts as usize)
                                .sum::<usize>()
                        ),
                        "rodadas de passos",
                    ))
                    .child(total(format!("{answers}"), "respostas suas")),
            )
            .child(
                Label::new(
                    "As entregas ficam nas threads de cada passo. Nada é mergeado pela esteira.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gates = self
            .flow
            .steps
            .iter()
            .filter(|step| matches!(step.kind, StepKind::Gate(_)))
            .count();
        let total = self.flow.steps.len();
        let mut status = h_flex()
            .h(px(22.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(self.flow.file_path.display().to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                Label::new(format!("· {total} passos · {gates} portões"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        if let Some(run) = &self.run {
            let (text, color) = match &run.state {
                RunState::Running => (
                    format!(
                        "· Rodando · passo {} de {total} · {}",
                        run.cursor + 1,
                        self.step_label(run.cursor)
                    ),
                    Color::Warning,
                ),
                RunState::WaitingForYou(_) => (
                    format!(
                        "· Rodando · passo {} de {total} · esperando você",
                        run.cursor + 1
                    ),
                    Color::Warning,
                ),
                RunState::Succeeded => ("· Concluída".to_string(), Color::Success),
                RunState::Cancelled => ("· Parada".to_string(), Color::Error),
            };
            status = status.child(Label::new(text).size(LabelSize::XSmall).color(color));
        }
        status
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BoxTone {
    Plain,
    You,
    Fail,
    Done,
}

fn answer_box(tone: BoxTone, cx: &App) -> Div {
    let status = cx.theme().status();
    let colors = cx.theme().colors();
    let (border, background) = match tone {
        BoxTone::Plain => (colors.border, colors.editor_background),
        BoxTone::You => (status.warning_border, status.warning_background),
        BoxTone::Fail => (status.error_border, status.error_background),
        BoxTone::Done => (status.success_border, status.success_background),
    };
    v_flex()
        .gap_2()
        .px_3()
        .py_2p5()
        .rounded_lg()
        .border_1()
        .border_color(border)
        .bg(background)
}

fn state_chip(state: StepState, note: Option<String>, cx: &App) -> impl IntoElement {
    let (color, text) = match state {
        StepState::Queued => (Color::Muted, "na fila"),
        StepState::Running => (Color::Accent, "rodando"),
        StepState::Verifying => (Color::Accent, "verificando"),
        StepState::WaitingForYou => (Color::Warning, "esperando você"),
        StepState::Passed => (Color::Success, "ok"),
        StepState::Failed => (Color::Error, "refazendo"),
    };
    let text = match note {
        Some(note) => format!("{text} · {note}"),
        None => text.to_string(),
    };
    let hsla = color.color(cx);
    h_flex()
        .h(px(22.))
        .flex_none()
        .px_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(hsla)
        .bg(cx.theme().colors().editor_background)
        .child(div().size(px(6.)).rounded_full().bg(hsla))
        .child(Label::new(text).size(LabelSize::XSmall).color(color))
}

fn clock(at: SystemTime) -> String {
    let at: chrono::DateTime<chrono::Local> = at.into();
    at.format("%H:%M").to_string()
}

impl Render for FlowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.finished_at.is_none() && self.run.as_ref().is_some_and(FlowRun::is_finished) {
            self.finished_at = Some(Instant::now());
        }
        v_flex()
            .key_context("FlowView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_board(cx))
                    .child(self.render_run_panel(cx)),
            )
            .child(Divider::horizontal())
            .child(self.render_status(cx))
    }
}

impl Focusable for FlowView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for FlowView {}

impl Item for FlowView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.flow.name.clone().into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Bumped whenever a run changes, so the status bar chip re-renders.
#[derive(Default)]
struct FlowRunsRevision(usize);

impl gpui::Global for FlowRunsRevision {}

fn notify_runs_changed(cx: &mut App) {
    cx.default_global::<FlowRunsRevision>().0 += 1;
}

/// The status bar chip of the run in progress: what it waits on, and a click to its tab.
pub struct FlowRunChip {
    workspace: WeakEntity<Workspace>,
    _observe_runs: gpui::Subscription,
}

impl FlowRunChip {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            _observe_runs: cx.observe_global::<FlowRunsRevision>(|_, cx| cx.notify()),
        }
    }

    fn active_view(&self, cx: &App) -> Option<Entity<FlowView>> {
        let workspace = self.workspace.upgrade()?;
        let views: Vec<Entity<FlowView>> =
            workspace.read(cx).items_of_type::<FlowView>(cx).collect();
        views
            .iter()
            .find(|view| {
                matches!(
                    view.read(cx).run.as_ref().map(|run| &run.state),
                    Some(RunState::WaitingForYou(_))
                )
            })
            .or_else(|| views.iter().find(|view| view.read(cx).is_running()))
            .cloned()
    }
}

impl Render for FlowRunChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.active_view(cx) else {
            return div().into_any_element();
        };
        let flow_view = view.read(cx);
        let Some(run) = &flow_view.run else {
            return div().into_any_element();
        };
        let total = run.steps.len();
        let waiting = matches!(run.state, RunState::WaitingForYou(_));
        let text = if waiting {
            format!(
                "{} · passo {} de {total} · esperando você",
                flow_view.flow.name,
                run.cursor + 1
            )
        } else {
            format!(
                "{} · passo {} de {total} · {}",
                flow_view.flow.name,
                run.cursor + 1,
                flow_view.step_label(run.cursor)
            )
        };
        let color = if waiting {
            Color::Warning
        } else {
            Color::Accent
        };
        let workspace = self.workspace.clone();
        h_flex()
            .id("flow-run-chip")
            .h(px(20.))
            .px_1p5()
            .gap_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .child(div().size(px(6.)).rounded_full().bg(color.color(cx)))
            .child(Label::new(text).size(LabelSize::Small).color(color))
            .tooltip(ui::Tooltip::text("Abrir a execução"))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.activate_item(&view, true, true, window, cx);
                    })
                    .log_err();
            })
            .into_any_element()
    }
}

impl StatusItemView for FlowRunChip {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

/// The status bar's toolkit button for esteiras: lists them to run one.
pub struct FlowsToolkitButton {
    workspace: WeakEntity<Workspace>,
}

impl FlowsToolkitButton {
    pub fn new(workspace: &Workspace, _cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
        }
    }
}

impl Render for FlowsToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .is_some_and(|item| item.downcast::<FlowView>().is_some())
        });
        let workspace = self.workspace.clone();
        StatusBarButton::new("toolkit-flows", IconName::ListTree, is_open)
            .tab_index(0isize)
            .aria_label("Esteiras")
            .tooltip(|_window, cx| ui::Tooltip::for_action("Esteiras", &RunFlow::default(), cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        run_flow(workspace, RunFlow::default(), window, cx)
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for FlowsToolkitButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
