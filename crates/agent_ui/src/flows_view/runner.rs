//! Runs the steps of a [`FlowRun`]: each agent step is a thread of the Agent panel with the
//! step's task agent, each `run:` step and command criterion a shell command in the project.

use super::FlowView;
use crate::agent_panel::CreateThreadOptions;
use crate::{Agent, AgentPanel, AgentThreadSource, ThreadId};
use acp_thread::{AcpThread, AgentThreadEntry, ToolCallStatus};
use agent::{AgentTool as _, NativeAgentConnection, SubmitStepTool, SubmitStepToolInput};
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, anyhow};
use flows::run::{CriterionOutcome, CriterionResult, FlowRun, RunAction, Submission};
use flows::{StepKind, StepLimits, Verifier};
use futures::FutureExt as _;
use gpui::{AppContext as _, AsyncWindowContext, Entity, WeakEntity};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use task_agents::TaskAgent;
use util::ResultExt as _;
use workspace::Workspace;

const THREAD_READY_TIMEOUT: Duration = Duration::from_secs(30);
const THREAD_READY_POLL: Duration = Duration::from_millis(100);
/// How many lines of a command's output go into the evidence.
const EVIDENCE_LINES: usize = 12;
const LIMIT_POLL: Duration = Duration::from_secs(1);

pub(super) struct RunningStep {
    /// The threads of the step's agents while they work, so Parar can cancel them.
    pub threads: Vec<Entity<agent::Thread>>,
    pub _task: gpui::Task<()>,
}

/// What a step's thread ended with.
struct StepThreadOutcome {
    submission: Option<SubmitStepToolInput>,
    stop_reason: acp::StopReason,
}

impl FlowView {
    /// Starts whatever the run needs next. Called after every change to the run.
    pub(super) fn drive(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        self.persist_run(cx);
        super::notify_runs_changed(cx);
        if self.running.is_some() {
            return;
        }
        let Some(run) = &self.run else {
            return;
        };
        match run.next_action() {
            RunAction::StartAgent { step } => self.start_agent_step(step, window, cx),
            RunAction::RunCommand { step } => self.start_command_step(step, window, cx),
            RunAction::StartParallel { step } => self.start_parallel_step(step, window, cx),
            RunAction::Wait | RunAction::Done => {}
        }
        cx.notify();
    }

    fn start_agent_step(
        &mut self,
        step: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(run) = &mut self.run else {
            return;
        };
        let label = run.flow.steps[step].label(&self.agents);
        let servers: Vec<String> = self
            .project
            .read(cx)
            .context_server_store()
            .read(cx)
            .configured_server_ids()
            .into_iter()
            .map(|id| id.0.to_string())
            .collect();
        let agent = match step_agent(run, step, &self.agents, &servers) {
            Ok(agent) => agent,
            Err(error) => {
                run.step_started(step);
                run.step_errored(step, &label, error.to_string());
                self.drive(window, cx);
                return;
            }
        };
        run.step_started(step);
        let prompt = step_prompt(run, step, &self.agents, None);
        let limits = match &run.flow.steps[step].kind {
            StepKind::Agent(agent_step) => Some(agent_step.limits),
            _ => None,
        };
        let title = format!("{} · {label}", run.flow.name);
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome =
                run_step_thread(&this, &workspace, step, title, agent, prompt, limits, cx).await;
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                if let Some(run) = &mut this.run {
                    if run.is_finished() {
                        return;
                    }
                    match outcome {
                        Ok(StepThreadOutcome {
                            submission: Some(submission),
                            ..
                        }) => {
                            let external =
                                run.agent_submitted(step, &label, submission_from_tool(submission));
                            if !external.is_empty() {
                                this.start_verification(step, label, external, window, cx);
                                return;
                            }
                        }
                        Ok(StepThreadOutcome {
                            submission: None,
                            stop_reason,
                        }) => {
                            let reason = match stop_reason {
                                acp::StopReason::Cancelled => "a thread do passo foi cancelada",
                                acp::StopReason::MaxTokens => {
                                    "o modelo chegou ao limite de tokens antes de entregar"
                                }
                                acp::StopReason::Refusal => "o modelo recusou o pedido",
                                _ => "o agente terminou sem chamar submit_step",
                            };
                            run.step_errored(step, &label, reason.to_string());
                        }
                        Err(error) => run.step_errored(step, &label, format!("{error:#}")),
                    }
                }
                this.drive(window, cx);
            })
            .ok();
        });
        self.running = Some(RunningStep {
            threads: Vec::new(),
            _task: task,
        });
    }

    fn start_parallel_step(
        &mut self,
        step: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(run) = &mut self.run else {
            return;
        };
        let StepKind::Parallel(parallel) = run.flow.steps[step].kind.clone() else {
            return;
        };
        let label = run.flow.steps[step].label(&self.agents);
        run.step_started(step);
        let mut branches = Vec::new();
        for command in &parallel.agents {
            let Some(agent) = self.agents.iter().find(|agent| &agent.command == command) else {
                branches.push((format!("/{command}"), None));
                continue;
            };
            let mut agent = with_submit_step(agent.clone());
            if parallel.read_only {
                agent.profile = Some(agent_settings::builtin_profiles::ASK.to_string());
            }
            let prompt = step_prompt(run, step, &self.agents, Some(command));
            let title = format!("{} · {}", run.flow.name, agent.name);
            branches.push((agent.name.clone(), Some((agent, prompt, title))));
        }
        let workspace = self.workspace.clone();
        let mut tasks = Vec::new();
        for (name, branch) in branches {
            let Some((agent, prompt, title)) = branch else {
                tasks.push((name, None));
                continue;
            };
            let workspace = workspace.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                run_step_thread(
                    &this,
                    &workspace,
                    step,
                    title,
                    agent,
                    prompt,
                    Some(StepLimits::default()),
                    cx,
                )
                .await
            });
            tasks.push((name, Some(task)));
        }
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut results = Vec::new();
            for (name, task) in tasks {
                let result = match task {
                    None => Err("o agente não existe mais; edite a esteira".to_string()),
                    Some(task) => match task.await {
                        Ok(StepThreadOutcome {
                            submission: Some(submission),
                            ..
                        }) => Ok(submission_from_tool(submission)),
                        Ok(StepThreadOutcome { .. }) => {
                            Err("terminou sem chamar submit_step".to_string())
                        }
                        Err(error) => Err(format!("{error:#}")),
                    },
                };
                results.push((name, result));
            }
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                if let Some(run) = &mut this.run
                    && !run.is_finished()
                {
                    run.parallel_finished(step, &label, results);
                }
                this.drive(window, cx);
            })
            .ok();
        });
        self.running = Some(RunningStep {
            threads: Vec::new(),
            _task: task,
        });
    }

    fn start_command_step(
        &mut self,
        step: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(run) = &mut self.run else {
            return;
        };
        let StepKind::Run(run_step) = &run.flow.steps[step].kind else {
            return;
        };
        let command = run_step.command.clone();
        run.step_started(step);
        let directory = self.working_directory(cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(run_command(command, directory)).await;
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                if let Some(run) = &mut this.run
                    && !run.is_finished()
                {
                    match result {
                        Ok((passed, evidence)) => run.command_finished(step, passed, evidence),
                        Err(error) => run.command_finished(step, false, format!("{error:#}")),
                    }
                }
                this.drive(window, cx);
            })
            .ok();
        });
        self.running = Some(RunningStep {
            threads: Vec::new(),
            _task: task,
        });
    }

    /// Checks the criteria verified by a command or by another agent, one after the other.
    fn start_verification(
        &mut self,
        step: usize,
        label: String,
        criteria: Vec<(usize, Verifier)>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(run) = &self.run else {
            return;
        };
        let directory = self.working_directory(cx);
        let workspace = self.workspace.clone();
        let agents = self.agents.clone();
        let flow_name = run.flow.name.clone();
        let delivery = delivery_summary(run, step);
        let criterion_texts: Vec<String> = match &run.flow.steps[step].kind {
            StepKind::Agent(agent_step) => agent_step
                .criteria
                .iter()
                .map(|criterion| criterion.text.clone())
                .collect(),
            _ => Vec::new(),
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut results = Vec::new();
            for (index, verifier) in criteria {
                let text = criterion_texts.get(index).cloned().unwrap_or_default();
                let result = match verifier {
                    Verifier::Command(command) => {
                        match cx
                            .background_spawn(run_command(command.clone(), directory.clone()))
                            .await
                        {
                            Ok((passed, evidence)) => CriterionResult {
                                index,
                                outcome: outcome(passed),
                                evidence: format!("{command} · {evidence}"),
                            },
                            Err(error) => CriterionResult {
                                index,
                                outcome: CriterionOutcome::Failed,
                                evidence: format!("{command} · {error:#}"),
                            },
                        }
                    }
                    Verifier::Agent(command) => {
                        let verdict = async {
                            let agent = agents
                                .iter()
                                .find(|agent| agent.command == command)
                                .with_context(|| format!("o agente /{command} não existe"))?;
                            let agent = with_submit_step(agent.clone());
                            let prompt = verifier_prompt(&flow_name, &label, &text, &delivery);
                            let title = format!("{flow_name} · /{command} confere {label}");
                            let outcome = run_step_thread(
                                &this, &workspace, step, title, agent, prompt, None, cx,
                            )
                            .await?;
                            let submission = outcome
                                .submission
                                .context("o verificador terminou sem chamar submit_step")?;
                            let check = submission.criteria.first().cloned().unwrap_or_default();
                            let evidence = if check.evidence.is_empty() {
                                submission.summary
                            } else {
                                check.evidence
                            };
                            anyhow::Ok((check.passed, format!("/{command} · {evidence}")))
                        }
                        .await;
                        match verdict {
                            Ok((passed, evidence)) => CriterionResult {
                                index,
                                outcome: outcome(passed),
                                evidence,
                            },
                            Err(error) => CriterionResult {
                                index,
                                outcome: CriterionOutcome::Failed,
                                evidence: format!("/{command} · {error:#}"),
                            },
                        }
                    }
                    Verifier::You | Verifier::SelfCheck => continue,
                };
                results.push(result);
            }
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                if let Some(run) = &mut this.run
                    && !run.is_finished()
                {
                    run.criteria_verified(step, &label, results);
                }
                this.drive(window, cx);
            })
            .ok();
        });
        self.running = Some(RunningStep {
            threads: Vec::new(),
            _task: task,
        });
    }

    fn working_directory(&self, cx: &gpui::App) -> Option<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .find_map(|worktree| {
                worktree
                    .read(cx)
                    .as_local()
                    .map(|local| local.abs_path().to_path_buf())
            })
    }
}

fn outcome(passed: bool) -> CriterionOutcome {
    if passed {
        CriterionOutcome::Passed
    } else {
        CriterionOutcome::Failed
    }
}

/// The MCP server of a design tool, among the configured ones, by its name.
fn design_server(tool: flows::DesignTool, servers: &[String]) -> Option<&String> {
    let keyword = match tool {
        flows::DesignTool::Pencil => "pencil",
        flows::DesignTool::Penpot => "penpot",
        flows::DesignTool::Figma => "figma",
        flows::DesignTool::Code => return None,
    };
    servers
        .iter()
        .find(|server| server.to_lowercase().contains(keyword))
}

/// The step's task agent as the flow configures it: its model and instructions for this flow,
/// the MCP server of where it happens, and `submit_step` turned on so it can hand off.
fn step_agent(
    run: &FlowRun,
    step: usize,
    agents: &[TaskAgent],
    servers: &[String],
) -> Result<TaskAgent> {
    let StepKind::Agent(agent_step) = &run.flow.steps[step].kind else {
        return Err(anyhow!("o passo não é de agente"));
    };
    let mut agent = agents
        .iter()
        .find(|agent| agent.command == agent_step.agent)
        .cloned()
        .with_context(|| {
            format!(
                "o agente /{} não existe mais; edite a esteira",
                agent_step.agent
            )
        })?;
    if let Some(model) = &agent_step.model {
        agent.model = Some(model.clone());
    }
    if let Some(target) = &agent_step.where_
        && target.tool != flows::DesignTool::Code
    {
        let server = design_server(target.tool, servers).with_context(|| {
            format!(
                "o passo acontece no {}, mas nenhum MCP do {} está configurado (Configurações → MCP)",
                target.tool.label(),
                target.tool.label()
            )
        })?;
        if !agent.context_servers.contains(server) {
            agent.context_servers.push(server.clone());
        }
    }
    if !agent_step.instructions.trim().is_empty() {
        agent.instructions = format!(
            "{}\n\n## Nesta esteira\n{}",
            agent.instructions.trim_end(),
            agent_step.instructions.trim()
        );
    }
    Ok(with_submit_step(agent))
}

fn with_submit_step(mut agent: TaskAgent) -> TaskAgent {
    agent.tools.insert(SubmitStepTool::NAME.to_string(), true);
    agent
}

/// The outputs every earlier step delivered in this run, by name, latest delivery first wins.
fn delivered_inputs(run: &FlowRun, step: usize) -> Vec<(String, String, usize)> {
    let mut inputs: Vec<(String, String, usize)> = Vec::new();
    for (index, step_run) in run.steps.iter().enumerate().take(step) {
        let Some(submission) = &step_run.submission else {
            continue;
        };
        for (name, value) in &submission.outputs {
            inputs.retain(|(existing, _, _)| existing != name);
            inputs.push((name.clone(), value.clone(), index));
        }
    }
    inputs
}

fn delivery_summary(run: &FlowRun, step: usize) -> String {
    let Some(submission) = run
        .steps
        .get(step)
        .and_then(|step| step.submission.as_ref())
    else {
        return String::new();
    };
    let mut text = submission.summary.clone();
    for (name, value) in &submission.outputs {
        text.push_str(&format!("\n- {name}: {value}"));
    }
    text
}

/// What the step's agent reads first. Never starts with `/`: a leading command would make the
/// native agent swap the task agent for the plain one, without this flow's overrides.
/// `branch` is the agent of an "Em paralelo" step that the prompt is for.
fn step_prompt(run: &FlowRun, step: usize, agents: &[TaskAgent], branch: Option<&str>) -> String {
    let flow = &run.flow;
    let total = flow.steps.len();
    let mut prompt = format!(
        "Você é o passo {} de {total} da esteira “{}”.\n\n## Pedido\n{}\n",
        step + 1,
        flow.name,
        run.input.trim()
    );

    let inputs = delivered_inputs(run, step);
    if !inputs.is_empty() {
        prompt.push_str("\n## O que os passos anteriores entregaram\n");
        for (name, value, from) in &inputs {
            let from_label = flow.steps[*from].label(agents);
            prompt.push_str(&format!("- `{name}` (de {from_label}): {value}\n"));
        }
    }

    if let (StepKind::Parallel(parallel), Some(branch)) = (&flow.steps[step].kind, branch) {
        let others: Vec<String> = parallel
            .agents
            .iter()
            .filter(|command| command.as_str() != branch)
            .map(|command| format!("/{command}"))
            .collect();
        prompt.push_str(&format!(
            "\n## Em paralelo\nVocê roda ao mesmo tempo que {}. {}\n",
            others.join(", "),
            if parallel.read_only {
                "Só leitura: não altere arquivos do projeto."
            } else {
                "Não edite os mesmos arquivos que os outros."
            }
        ));
        let gives = agents
            .iter()
            .find(|agent| agent.command == branch)
            .map(|agent| agent.gives.clone())
            .unwrap_or_default();
        if !gives.is_empty() {
            prompt.push_str("\n## Entregue\n");
            for name in &gives {
                prompt.push_str(&format!("- `{name}`\n"));
            }
        }
        if let Some(feedback) = &run.steps[step].feedback {
            prompt.push_str(&format!("\n## Refaça considerando\n{feedback}\n"));
        }
        prompt.push_str(
            "\n## Como entregar\nQuando terminar, chame `submit_step` uma única vez com `summary` e `outputs` \
             pelo nome exato acima. Depois encerre o turno.",
        );
        return prompt;
    }
    let StepKind::Agent(agent_step) = &flow.steps[step].kind else {
        return prompt;
    };
    let gives = agents
        .iter()
        .find(|agent| agent.command == agent_step.agent)
        .map(|agent| agent.gives.clone())
        .unwrap_or_default();
    if !gives.is_empty() {
        prompt.push_str("\n## Entregue\n");
        for name in &gives {
            prompt.push_str(&format!("- `{name}`\n"));
        }
    }
    if let Some(target) = &agent_step.where_ {
        prompt.push_str(&format!("\n## Onde\n{}", target.tool.label()));
        if let Some(file) = &target.file {
            prompt.push_str(&format!(" · {file}"));
        }
        prompt.push('\n');
    }
    if !agent_step.criteria.is_empty() {
        prompt.push_str("\n## Critérios de aceite\n");
        for (index, criterion) in agent_step.criteria.iter().enumerate() {
            let required = if criterion.required {
                "obrigatório"
            } else {
                "opcional"
            };
            let verify = match &criterion.verify {
                Verifier::SelfCheck => "você confere e informa no submit_step".to_string(),
                other => format!("confere: {}", other.label()),
            };
            prompt.push_str(&format!(
                "{}. {} ({required} · {verify})\n",
                index + 1,
                criterion.text
            ));
        }
    }
    if let Some(feedback) = &run.steps[step].feedback {
        prompt.push_str(&format!(
            "\n## Refaça considerando\n{feedback}\nAs entregas anteriores deste passo estão no projeto; corrija o que foi apontado.\n"
        ));
    }
    prompt.push_str(
        "\n## Como entregar\nQuando terminar, chame `submit_step` uma única vez: `summary` com o que você fez, \
         `outputs` com cada entrega pelo nome exato acima (caminho, link ou texto curto) e, em `criteria`, \
         o número, o resultado e a evidência de cada critério que você confere. Depois encerre o turno.",
    );
    prompt
}

fn verifier_prompt(flow_name: &str, step_label: &str, criterion: &str, delivery: &str) -> String {
    format!(
        "Você confere um critério de aceite da esteira “{flow_name}”.\n\n## Critério\n{criterion}\n\n\
         ## O que {step_label} entregou\n{delivery}\n\n## Como responder\nVerifique no projeto, sem alterar \
         nada, e chame `submit_step` com `summary` curto e `criteria: [{{\"number\": 1, \"passed\": \
         true|false, \"evidence\": \"...\"}}]`. Depois encerre o turno."
    )
}

fn submission_from_tool(input: SubmitStepToolInput) -> Submission {
    Submission {
        summary: input.summary,
        outputs: input.outputs,
        self_checks: input
            .criteria
            .into_iter()
            .filter(|check| check.number > 0)
            .map(|check| (check.number - 1, (check.passed, check.evidence)))
            .collect(),
    }
}

fn completed_submission(thread: &AcpThread) -> Option<SubmitStepToolInput> {
    thread.entries().iter().rev().find_map(|entry| match entry {
        AgentThreadEntry::ToolCall(call)
            if call.tool_name.as_deref() == Some(SubmitStepTool::NAME)
                && matches!(call.status(), ToolCallStatus::Completed) =>
        {
            call.raw_input
                .clone()
                .and_then(|input| serde_json::from_value(input).ok())
        }
        _ => None,
    })
}

/// Opens a thread in the Agent panel for the step, sends the prompt with the step's task agent
/// and waits for the turn. A turn without `submit_step` gets one reminder.
#[allow(clippy::too_many_arguments)]
async fn run_step_thread(
    this: &WeakEntity<FlowView>,
    workspace: &WeakEntity<Workspace>,
    step: usize,
    title: String,
    agent: TaskAgent,
    prompt: String,
    limits: Option<StepLimits>,
    cx: &mut AsyncWindowContext,
) -> Result<StepThreadOutcome> {
    let started_at = Instant::now();
    let (panel, thread_id) = workspace.update_in(cx, |workspace, window, cx| {
        let panel = workspace
            .panel::<AgentPanel>(cx)
            .context("o painel de agentes não está aberto")?;
        let thread_id = panel.update(cx, |panel, cx| {
            panel.create_thread_with_options(
                CreateThreadOptions {
                    title: Some(title.into()),
                    agent: Some(Agent::NativeAgent),
                    ..Default::default()
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });
        anyhow::Ok((panel, thread_id))
    })??;
    this.update(cx, |this, cx| {
        if let Some(threads) = this.step_threads.get_mut(step) {
            threads.push(thread_id);
        }
        cx.notify();
    })?;

    let acp_thread = wait_for_root_thread(&panel, thread_id, cx).await?;
    let (connection, native_thread) = cx.update(|_, cx| {
        let connection = acp_thread
            .read(cx)
            .connection()
            .clone()
            .downcast::<NativeAgentConnection>()
            .context("a thread do passo não é do Zed Agent")?;
        let session_id = acp_thread.read(cx).session_id().clone();
        let native_thread = connection
            .thread(&session_id, cx)
            .context("a sessão do passo sumiu")?;
        anyhow::Ok((connection, native_thread))
    })??;
    cx.update(|_, cx| {
        native_thread.update(cx, |thread, cx| thread.set_task_agent(Some(agent), cx));
    })?;
    this.update(cx, |this, _| {
        if let Some(running) = &mut this.running {
            running.threads.push(native_thread.clone());
        }
    })?;

    let limits = limits.map(|limits| Limits {
        limits,
        started_at,
        native_thread: native_thread.clone(),
    });
    let stop_reason = send(&connection, &acp_thread, prompt, limits.as_ref(), cx).await?;
    let submission = cx.update(|_, cx| completed_submission(acp_thread.read(cx)))?;
    if submission.is_some() || stop_reason != acp::StopReason::EndTurn {
        return Ok(StepThreadOutcome {
            submission,
            stop_reason,
        });
    }
    let stop_reason = send(
        &connection,
        &acp_thread,
        "Você encerrou sem chamar `submit_step`. Se o trabalho está pronto, chame `submit_step` agora; \
         se falta algo, termine e então chame."
            .to_string(),
        limits.as_ref(),
        cx,
    )
    .await?;
    let submission = cx.update(|_, cx| completed_submission(acp_thread.read(cx)))?;
    Ok(StepThreadOutcome {
        submission,
        stop_reason,
    })
}

struct Limits {
    limits: StepLimits,
    started_at: Instant,
    native_thread: Entity<agent::Thread>,
}

fn assistant_turns(thread: &AcpThread) -> usize {
    thread
        .entries()
        .iter()
        .filter(|entry| matches!(entry, AgentThreadEntry::AssistantMessage(_)))
        .count()
}

/// Sends a message and waits for the turn. With limits, a turn past the step's minutes or
/// number of assistant turns is cancelled and the step fails with that reason.
async fn send(
    connection: &Rc<NativeAgentConnection>,
    acp_thread: &Entity<AcpThread>,
    text: String,
    limits: Option<&Limits>,
    cx: &mut AsyncWindowContext,
) -> Result<acp::StopReason> {
    let content = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
    let mut completion = cx
        .update(|_, cx| connection.send_and_wait_for_completion(acp_thread.clone(), content, cx))?
        .fuse();
    let Some(limits) = limits else {
        return completion.await;
    };
    loop {
        let mut tick = cx.background_executor().timer(LIMIT_POLL).fuse();
        futures::select_biased! {
            result = completion => return result,
            _ = tick => {
                let turns = cx.update(|_, cx| assistant_turns(acp_thread.read(cx)))?;
                let minutes = u64::from(limits.limits.minutes);
                let exceeded = if minutes > 0
                    && limits.started_at.elapsed() > Duration::from_secs(minutes * 60)
                {
                    Some(format!("passou do limite de {minutes} min"))
                } else if limits.limits.turns > 0 && turns > limits.limits.turns as usize {
                    Some(format!("passou do limite de {} turnos", limits.limits.turns))
                } else {
                    None
                };
                if let Some(reason) = exceeded {
                    let cancel = cx.update(|_, cx| {
                        limits.native_thread.update(cx, |thread, cx| thread.cancel(cx))
                    })?;
                    cancel.await;
                    completion.await.log_err();
                    return Err(anyhow!(reason));
                }
            }
        }
    }
}

/// The panel creates the thread's view right away, but its session only exists once the native
/// agent connects.
async fn wait_for_root_thread(
    panel: &Entity<AgentPanel>,
    thread_id: ThreadId,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<AcpThread>> {
    let attempts = THREAD_READY_TIMEOUT.as_millis() / THREAD_READY_POLL.as_millis();
    for _ in 0..attempts {
        let thread = cx.update(|_, cx| {
            panel
                .read(cx)
                .conversation_view_for_id(&thread_id, cx)
                .and_then(|view| view.read(cx).root_thread(cx))
        })?;
        if let Some(thread) = thread {
            return Ok(thread);
        }
        cx.background_executor().timer(THREAD_READY_POLL).await;
    }
    Err(anyhow!(
        "a thread do passo não conectou em {} s",
        THREAD_READY_TIMEOUT.as_secs()
    ))
}

/// Runs a command in the project's first local folder. Passes when it exits with 0; the
/// evidence is the exit and the last lines it printed.
async fn run_command(command: String, directory: Option<PathBuf>) -> Result<(bool, String)> {
    let mut process = util::command::new_command("/bin/sh");
    process.arg("-c").arg(&command);
    if let Some(directory) = &directory {
        process.current_dir(directory);
    }
    let output = process
        .output()
        .await
        .with_context(|| format!("não foi possível rodar `{command}`"))?;
    let code = output
        .status
        .code()
        .map(|code| format!("saiu {code}"))
        .unwrap_or_else(|| "interrompido".to_string());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = printed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(EVIDENCE_LINES)..].join("\n");
    let evidence = if tail.is_empty() {
        code
    } else {
        format!("{code}\n{tail}")
    };
    Ok((output.status.success(), evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flows::{AgentStep, Criterion, Flow, FlowSource, FlowStep, OnFail};
    use std::collections::BTreeMap;

    fn agent(command: &str, gives: &[&str]) -> TaskAgent {
        TaskAgent {
            id: command.to_string(),
            name: command.to_uppercase(),
            command: command.to_string(),
            description: String::new(),
            profile: None,
            model: None,
            subagent_model: None,
            toolkits: Vec::new(),
            tools: BTreeMap::new(),
            permissions: BTreeMap::new(),
            takes: Vec::new(),
            gives: gives.iter().map(|name| name.to_string()).collect(),
            context_servers: Vec::new(),
            enabled: true,
            instructions: "Instruções do agente.".to_string(),
            source: task_agents::TaskAgentSource::Personal,
            file_path: PathBuf::new(),
        }
    }

    fn run() -> FlowRun {
        let mut ux = AgentStep::new("ux");
        ux.instructions = "Use o design system.".to_string();
        ux.model = Some("anthropic/claude-opus-5-5".to_string());
        ux.where_ = Some(flows::DesignTarget {
            tool: flows::DesignTool::Pencil,
            file: Some("design/app.pen".to_string()),
        });
        ux.criteria = vec![Criterion {
            text: "Cobre a spec".to_string(),
            required: true,
            verify: Verifier::SelfCheck,
        }];
        ux.on_fail = OnFail::Retry { max: 1 };
        let flow = Flow {
            id: "feature".to_string(),
            name: "Feature do zero".to_string(),
            input: String::new(),
            steps: vec![
                FlowStep {
                    id: "produto".to_string(),
                    kind: StepKind::Agent(AgentStep::new("produto")),
                },
                FlowStep {
                    id: "ux".to_string(),
                    kind: StepKind::Agent(ux),
                },
            ],
            notes: String::new(),
            source: FlowSource::Personal,
            file_path: PathBuf::from("feature.flow.md"),
        };
        FlowRun::new(flow, "/criar um todo list".to_string())
    }

    #[test]
    fn test_step_prompt_carries_inputs_criteria_and_feedback() {
        let agents = vec![agent("produto", &["spec.md"]), agent("ux", &["telas"])];
        let mut run = run();
        run.step_started(0);
        run.agent_submitted(
            0,
            "PRODUTO",
            submission_from_tool(SubmitStepToolInput {
                summary: "Spec escrita".to_string(),
                outputs: BTreeMap::from([("spec.md".to_string(), "docs/todo.md".to_string())]),
                criteria: Vec::new(),
            }),
        );
        run.step_started(1);
        run.agent_submitted(
            1,
            "UX",
            submission_from_tool(SubmitStepToolInput {
                summary: "Telas".to_string(),
                outputs: BTreeMap::new(),
                criteria: vec![agent::StepSelfCheck {
                    number: 1,
                    passed: false,
                    evidence: "falta a tela vazia".to_string(),
                }],
            }),
        );

        let prompt = step_prompt(&run, 1, &agents, None);
        assert!(!prompt.trim_start().starts_with('/'));
        assert!(prompt.contains("passo 2 de 2"));
        assert!(prompt.contains("`spec.md` (de PRODUTO): docs/todo.md"));
        assert!(prompt.contains("- `telas`"));
        assert!(prompt.contains("1. Cobre a spec (obrigatório"));
        assert!(prompt.contains("## Refaça considerando\nCobre a spec"));

        let agent = step_agent(&run, 1, &agents, &["pencil-mcp".to_string()])
            .expect("the step's agent exists");
        assert_eq!(agent.model.as_deref(), Some("anthropic/claude-opus-5-5"));
        assert!(
            agent
                .instructions
                .ends_with("## Nesta esteira\nUse o design system.")
        );
        assert_eq!(agent.tools.get(SubmitStepTool::NAME), Some(&true));
        assert_eq!(agent.context_servers, ["pencil-mcp"]);
        let error =
            step_agent(&run, 1, &agents, &["penpot".to_string()]).expect_err("no Pencil server");
        assert!(error.to_string().contains("nenhum MCP do Pencil"));
    }

    #[test]
    fn test_unknown_agent_is_reported() {
        let error = step_agent(&run(), 0, &[], &[]).expect_err("no agents loaded");
        assert!(error.to_string().contains("/produto"));
    }
}
