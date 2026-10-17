//! The esteira tab: one flow, its steps with the state of the current run, and the run panel
//! where the gates, the criteria left to you and the steps out of retries wait for an answer.

mod flow_editor;
pub mod history;
mod new_flow_modal;
mod runner;
mod suggest;

use crate::{AgentPanel, ThreadId};
use agent::NativeAgentConnection;
use anyhow::{Context as _, Result};
use editor::Editor;
use flow_editor::{BoardView, PanelTab, StepInspector};
use flows::run::{CriterionOutcome, FlowRun, RunEvent, RunEventKind, RunState, StepState, Waiting};
use flows::{Flow, FlowSource, StepKind};
use fs::Fs;
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

gpui::actions!(
    esteiras,
    [
        /// Creates an esteira: blank, from a template or with steps suggested for a request.
        NovaEsteira,
        /// Opens the tab of the run in progress, or the one waiting for you.
        AbrirExecucaoAtiva,
        /// Lists the esteiras in the Agentes tab.
        MostrarEsteiras,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, action: &RunFlow, window, cx| {
            run_flow(workspace, action.clone(), window, cx);
        });
        workspace.register_action(|workspace, _: &NovaEsteira, window, cx| {
            new_flow(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &AbrirExecucaoAtiva, window, cx| {
            let Some(view) = active_run_view(workspace, cx) else {
                show_error(workspace, anyhow::anyhow!("nenhuma esteira rodando"), cx);
                return;
            };
            workspace.activate_item(&view, true, true, window, cx);
        });
        workspace.register_action(|workspace, _: &MostrarEsteiras, window, cx| {
            crate::task_agents_view::open_flows(workspace, window, cx);
        });
    })
    .detach();
}

/// Opens a run from the history: its tab when it is still open, else a new tab with the run as
/// it was; with `resume` an interrupted run continues.
pub fn open_run(
    workspace: &mut Workspace,
    id: String,
    resume: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<FlowView>(cx)
        .find(|view| view.read(cx).run_id.as_deref() == Some(id.as_str()));
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let Some(record) = history::records(cx)
        .into_iter()
        .map(|(record, _)| record)
        .find(|record| record.id == id)
    else {
        return;
    };
    let Some((mut flow, run)) = record.restore() else {
        show_error(
            workspace,
            anyhow::anyhow!("a execução de “{}” não abre mais", record.flow_name),
            cx,
        );
        return;
    };
    flow.source = flow_source_for_path(workspace, &flow.file_path, cx);
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
        let agents = cx.update(|_, cx| connection.task_agents(&project, cx).to_vec())?;
        workspace.update_in(cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            let view = cx.new(|cx| {
                FlowView::new(
                    flow,
                    agents,
                    project.clone(),
                    weak_workspace,
                    true,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view.update(cx, |view, cx| {
                view.restore_run(&record, run, resume, window, cx)
            });
        })
    })
    .detach_and_log_err(cx);
}

/// Shows a step's thread in the Agent panel, loading it when it isn't open anymore.
pub fn open_step_thread(
    workspace: &mut Workspace,
    thread_id: ThreadId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.focus_panel::<AgentPanel>(window, cx);
    if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
        panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                crate::Agent::NativeAgent,
                thread_id,
                None,
                None,
                true,
                crate::AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });
    }
}

pub(crate) fn flow_source_label(source: &FlowSource) -> String {
    match source {
        FlowSource::Personal => "só para mim".to_string(),
        FlowSource::Project { worktree_root_name } => format!("projeto · {worktree_root_name}"),
    }
}

/// The run that waits for the user first, else any run in progress.
fn active_run_view(workspace: &Workspace, cx: &App) -> Option<Entity<FlowView>> {
    let views: Vec<Entity<FlowView>> = workspace.items_of_type::<FlowView>(cx).collect();
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

pub(crate) fn new_flow(
    workspace: &mut Workspace,
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
        let agents = cx.update(|_, cx| connection.task_agents(&project, cx).to_vec())?;
        workspace.update_in(cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            workspace.toggle_modal(window, cx, |window, cx| {
                new_flow_modal::NewFlowModal::new(
                    weak_workspace,
                    project.clone(),
                    agents,
                    window,
                    cx,
                )
            });
        })
    })
    .detach_and_log_err(cx);
}

/// Opens a flow by its file, for the Agentes tab and the project panel.
pub(crate) fn open_flow_path(
    workspace: &mut Workspace,
    path: std::path::PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<FlowView>(cx)
        .find(|view| view.read(cx).flow.file_path == path);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let connect = match native_connection(workspace, cx) {
        Ok(connect) => connect,
        Err(error) => {
            show_error(workspace, error, cx);
            return;
        }
    };
    let fs = project.read(cx).fs().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let connection = connect.await?;
        let agents = cx.update(|_, cx| connection.task_agents(&project, cx).to_vec())?;
        let content = fs.load(&path).await?;
        let source = workspace.update(cx, |workspace, cx| {
            flow_source_for_path(workspace, &path, cx)
        })?;
        let flow = flows::parse_flow(&path, &content, source);
        workspace.update_in(cx, |workspace, window, cx| match flow {
            Ok(flow) => open_flow(workspace, flow, agents, project.clone(), None, window, cx),
            Err(error) => show_error(
                workspace,
                error.context(format!("{} não abre como esteira", path.display())),
                cx,
            ),
        })
    })
    .detach_and_log_err(cx);
}

fn flow_source_for_path(workspace: &Workspace, path: &std::path::Path, cx: &App) -> FlowSource {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .find_map(|worktree| {
            let worktree = worktree.read(cx);
            let local = worktree.as_local()?;
            path.starts_with(local.abs_path().join(flows::PROJECT_FLOWS_DIR))
                .then(|| FlowSource::Project {
                    worktree_root_name: Arc::<str>::from(worktree.root_name_str()),
                })
        })
        .unwrap_or(FlowSource::Personal)
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
                    let weak_workspace = workspace.weak_handle();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        new_flow_modal::NewFlowModal::new(
                            weak_workspace,
                            project.clone(),
                            agents,
                            window,
                            cx,
                        )
                    });
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
    let view = cx.new(|cx| FlowView::new(flow, agents, project, weak_workspace, true, window, cx));
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

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let workspace = self.workspace.clone();
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("flow-picker-new", "Nova esteira")
                        .full_width()
                        .style(ButtonStyle::Outlined)
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            cx.emit(DismissEvent);
                            workspace
                                .update(cx, |workspace, cx| new_flow(workspace, window, cx))
                                .log_err();
                        })),
                )
                .into_any_element(),
        )
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
    fs: Arc<dyn Fs>,
    flow: Flow,
    agents: Vec<TaskAgent>,
    /// The file on disk matches the flow as it is now.
    saved: bool,
    /// The file exists; until then the file name follows the flow's name.
    ever_saved: bool,
    problems: Vec<flows::FlowProblem>,
    selected: Option<usize>,
    /// Where the library inserts, when the user marked a point in the list.
    insert_at: Option<usize>,
    panel_tab: PanelTab,
    board_view: BoardView,
    inspector: Option<StepInspector>,
    name_editor: Entity<Editor>,
    library_search: Entity<Editor>,
    suggesting: Option<Task<()>>,
    /// Why the file didn't parse; the tab then only offers to edit it as text.
    load_error: Option<String>,
    /// The run's id in the history.
    run_id: Option<String>,
    run_started_at: Option<SystemTime>,
    _subscriptions: Vec<gpui::Subscription>,
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
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Nome da esteira", window, cx);
            editor.set_text(flow.name.clone(), window, cx);
            editor
        });
        let library_search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Buscar agentes…", window, cx);
            editor
        });
        let fs = project.read(cx).fs().clone();
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
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            fs,
            step_threads: vec![Vec::new(); flow.steps.len()],
            flow,
            agents,
            saved,
            ever_saved: saved,
            problems: Vec::new(),
            selected: None,
            insert_at: None,
            panel_tab: PanelTab::Step,
            board_view: BoardView::List,
            inspector: None,
            name_editor,
            library_search,
            suggesting: None,
            load_error: None,
            run_id: None,
            run_started_at: None,
            _subscriptions: Vec::new(),
            run: None,
            started_at: None,
            finished_at: None,
            running: None,
            input_editor,
            adjust_editor,
        };
        this.validate();
        this.subscribe_name_editor(cx);
        this._subscriptions.push(cx.on_release(|this, cx| {
            if let Some(id) = &this.run_id {
                history::release(id, cx);
            }
        }));
        if !this.flow.steps.is_empty() {
            this.select_step(Some(0), window, cx);
        }
        this
    }

    fn start(&mut self, input: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(problem) = self.problems.first() {
            let message = match problem.step() {
                Some(step) => format!("Passo {}: {problem}", step + 1),
                None => problem.to_string(),
            };
            self.show_toast(&format!("Corrija antes de rodar. {message}"), cx);
            return;
        }
        self.input_editor
            .update(cx, |editor, cx| editor.set_text(input.clone(), window, cx));
        self.panel_tab = PanelTab::Run;
        self.stop(None, cx);
        self.step_threads = vec![Vec::new(); self.flow.steps.len()];
        self.run = Some(FlowRun::new(self.flow.clone(), input));
        if let Some(id) = &self.run_id {
            history::release(id, cx);
        }
        self.run_id = Some(uuid::Uuid::new_v4().to_string());
        self.run_started_at = Some(SystemTime::now());
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
        if let Some(running) = self.running.take() {
            for thread in running.threads {
                thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
            }
        }
        if let (Some(reason), Some(run)) = (reason, &mut self.run) {
            run.cancel(reason);
        }
        self.persist_run(cx);
        notify_runs_changed(cx);
        cx.notify();
    }

    pub(super) fn persist_run(&self, cx: &mut App) {
        let (Some(run), Some(id)) = (&self.run, &self.run_id) else {
            return;
        };
        let flow_markdown = match run.flow.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                log::error!("failed to keep the run of {}: {error:#}", run.flow.name);
                return;
            }
        };
        history::record(
            history::FlowRunRecord {
                id: id.clone(),
                flow_id: run.flow.id.clone(),
                flow_name: run.flow.name.clone(),
                flow_path: run.flow.file_path.clone(),
                flow_markdown,
                started_at: self.run_started_at.unwrap_or_else(SystemTime::now),
                updated_at: SystemTime::now(),
                snapshot: run.snapshot(),
                step_labels: run
                    .flow
                    .steps
                    .iter()
                    .map(|step| step.label(&self.agents))
                    .collect(),
                step_threads: self.step_threads.clone(),
            },
            cx,
        );
    }

    /// Shows a run from the history; with `resume`, it continues from the step it stopped on.
    fn restore_run(
        &mut self,
        record: &history::FlowRunRecord,
        run: FlowRun,
        resume: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_editor.update(cx, |editor, cx| {
            editor.set_text(run.input.clone(), window, cx)
        });
        self.run = Some(run);
        self.run_id = Some(record.id.clone());
        self.run_started_at = Some(record.started_at);
        self.step_threads = record.step_threads.clone();
        self.step_threads.resize(self.flow.steps.len(), Vec::new());
        self.panel_tab = PanelTab::Run;
        if resume {
            self.started_at = Some(Instant::now());
            if let Some(run) = &mut self.run {
                run.resume();
            }
            self.drive(window, cx);
        } else {
            self.finished_at = self.started_at;
            cx.notify();
        }
    }

    fn after_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drive(window, cx);
    }

    fn open_thread(&self, thread_id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                open_step_thread(workspace, thread_id, window, cx)
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

    fn render_run_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let panel = v_flex();
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
        panel.child(header).child(bar).child(feed)
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
                Label::new(if self.ever_saved {
                    self.flow.file_path.display().to_string()
                } else {
                    "Não salva".to_string()
                })
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
        StepState::Skipped => (Color::Muted, "pulado"),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.finished_at.is_none() && self.run.as_ref().is_some_and(FlowRun::is_finished) {
            self.finished_at = Some(Instant::now());
        }
        let colors = cx.theme().colors().clone();
        let tab = |id: &'static str,
                   label: &'static str,
                   tab: PanelTab,
                   current: PanelTab,
                   cx: &mut Context<Self>| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .full_width()
                .toggle_state(tab == current)
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.panel_tab = tab;
                    cx.notify();
                }))
        };
        let current_tab = self.panel_tab;
        let right = v_flex()
            .w(px(380.))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                div().p_2().child(
                    h_flex()
                        .p_0p5()
                        .gap_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .child(tab("panel-step", "Passo", PanelTab::Step, current_tab, cx))
                        .child(tab("panel-run", "Execução", PanelTab::Run, current_tab, cx)),
                ),
            )
            .child(
                div()
                    .id("flow-right-panel")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(match current_tab {
                        PanelTab::Step => self.render_inspector(window, cx),
                        PanelTab::Run => self.render_run_panel(cx).into_any_element(),
                    }),
            );
        v_flex()
            .key_context("FlowView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_library(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_toolbar(cx))
                            .child(div().flex_1().min_h_0().child(self.render_board(cx))),
                    )
                    .child(right),
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

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = workspace.weak_handle();
    }
}

/// Bumped whenever a run changes, so the status bar chip and the sidebar re-render.
#[derive(Default)]
pub struct FlowRunsRevision(usize);

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

/// Folders whose agents and esteiras may run: the same trust rule as the native agent's.
fn trusted_roots(project: &Entity<Project>, cx: &mut App) -> Vec<(Arc<str>, std::path::PathBuf)> {
    let trusted_worktrees = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx);
    let worktree_store = project.read(cx).worktree_store();
    let worktrees = project.read(cx).visible_worktrees(cx).collect::<Vec<_>>();
    worktrees
        .into_iter()
        .filter_map(|worktree| {
            let worktree_id = worktree.read(cx).id();
            let is_trusted = trusted_worktrees.as_ref().is_none_or(|trusted_worktrees| {
                trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.can_trust(&worktree_store, worktree_id, cx)
                })
            });
            let worktree = worktree.read(cx);
            let local = worktree.as_local()?;
            is_trusted.then(|| {
                (
                    Arc::<str>::from(worktree.root_name_str()),
                    local.abs_path().to_path_buf(),
                )
            })
        })
        .collect()
}

pub fn register_project_item(cx: &mut App) {
    workspace::register_project_item::<FlowView>(cx);
}

/// A `.flow.md` opened from the project: the flow, or why it doesn't parse, and the agents the
/// editor offers.
pub struct FlowFile {
    flow: std::result::Result<Flow, String>,
    file_path: std::path::PathBuf,
    agents: Vec<TaskAgent>,
    project_path: project::ProjectPath,
    entry_id: Option<project::ProjectEntryId>,
}

impl project::ProjectItem for FlowFile {
    fn try_open(
        project: &Entity<Project>,
        path: &project::ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !path.path.as_unix_str().ends_with(flows::FLOW_FILE_SUFFIX) {
            return None;
        }
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let fs = project.read(cx).fs().clone();
        let roots = trusted_roots(project, cx);
        let source = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
            .map(|worktree| FlowSource::Project {
                worktree_root_name: Arc::<str>::from(worktree.read(cx).root_name_str()),
            })
            .unwrap_or(FlowSource::Personal);
        let project_path = path.clone();
        Some(cx.spawn(async move |cx| {
            let content = fs.load(&abs_path).await?;
            let flow = flows::parse_flow(&abs_path, &content, source)
                .map_err(|error| format!("{error:#}"));
            let (agents, _) = task_agents::load_task_agents(fs, roots).await;
            Ok(cx.new(|_| FlowFile {
                flow,
                file_path: abs_path,
                agents,
                project_path,
                entry_id,
            }))
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<project::ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<project::ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

impl workspace::item::ProjectItem for FlowView {
    type Item = FlowFile;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<FlowFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let file = item.read(cx);
        let agents = file.agents.clone();
        let (flow, load_error) = match &file.flow {
            Ok(flow) => (flow.clone(), None),
            Err(error) => {
                let id = file
                    .file_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.trim_end_matches(flows::FLOW_FILE_SUFFIX).to_string())
                    .unwrap_or_default();
                (
                    Flow {
                        name: id.clone(),
                        id,
                        input: String::new(),
                        steps: Vec::new(),
                        notes: String::new(),
                        source: FlowSource::Personal,
                        file_path: file.file_path.clone(),
                    },
                    Some(error.clone()),
                )
            }
        };
        let mut view = FlowView::new(
            flow,
            agents,
            project,
            WeakEntity::new_invalid(),
            true,
            window,
            cx,
        );
        view.load_error = load_error;
        view
    }
}

#[cfg(test)]
mod tests {
    use super::flow_editor::NewStep;
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use settings::SettingsStore;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn init_test(cx: &mut TestAppContext) {
        let settings_store = cx.update(SettingsStore::test);
        cx.set_global(settings_store);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
    }

    fn agent(command: &str, takes: &[&str], gives: &[&str]) -> TaskAgent {
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
            takes: takes.iter().map(|name| name.to_string()).collect(),
            gives: gives.iter().map(|name| name.to_string()).collect(),
            context_servers: Vec::new(),
            instructions: String::new(),
            source: task_agents::TaskAgentSource::Personal,
            file_path: PathBuf::new(),
        }
    }

    #[gpui::test]
    async fn test_builds_and_saves_a_flow(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/app", serde_json::json!({ "src": {} }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/app")], cx).await;
        let flow = Flow {
            id: "nova".to_string(),
            name: "Nova".to_string(),
            input: String::new(),
            steps: Vec::new(),
            notes: String::new(),
            source: FlowSource::Personal,
            file_path: PathBuf::from("/app/.asylum/flows/nova.flow.md"),
        };
        let agents = vec![
            agent("produto", &["pedido"], &["spec.md"]),
            agent("dev", &["spec.md"], &["PR"]),
        ];
        let (view, cx) = cx.add_window_view(|window, cx| {
            FlowView::new(
                flow,
                agents,
                project.clone(),
                WeakEntity::new_invalid(),
                false,
                window,
                cx,
            )
        });

        view.update_in(cx, |view, window, cx| {
            view.insert_step(0, NewStep::Agent("dev".to_string()), window, cx);
            assert_eq!(view.problems.len(), 1, "dev needs spec.md before it");
            view.insert_step(0, NewStep::Agent("produto".to_string()), window, cx);
            view.insert_step(1, NewStep::Gate, window, cx);
            view.insert_step(3, NewStep::Command, window, cx);
            assert!(view.problems.is_empty(), "{:?}", view.problems);
            assert_eq!(view.selected, Some(3));
            let ids: Vec<&str> = view
                .flow
                .steps
                .iter()
                .map(|step| step.id.as_str())
                .collect();
            assert_eq!(ids, ["produto", "aprovacao", "dev", "comando"]);
        });
        let save = view.update(cx, |view, cx| view.save(cx));
        assert!(save.await);
        let saved = fs
            .load(Path::new("/app/.asylum/flows/nova.flow.md"))
            .await
            .expect("the flow was written");
        let parsed = flows::parse_flow(
            Path::new("/app/.asylum/flows/nova.flow.md"),
            &saved,
            FlowSource::Personal,
        )
        .expect("the saved flow parses");
        view.update(cx, |view, _| {
            assert!(view.saved);
            assert_eq!(parsed.steps, view.flow.steps);
        });
    }
}
