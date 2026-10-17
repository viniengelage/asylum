//! Building the esteira: the library of agents, the steps as an ordered list (or a canvas) and
//! the inspector of the selected step. Every change goes straight into the flow; Salvar writes
//! the `.flow.md` that the parser reads back.

use super::FlowView;
use crate::task_agents_view::{OpenTaskAgents, model_dropdown, selector};
use editor::{Editor, EditorEvent};
use flows::{
    AgentStep, BranchStep, Criterion, DesignTarget, DesignTool, FlowStep, GateStep, GitHubAction,
    GitHubStep, OnFail, ParallelStep, RunStep, StepKind, Verifier, unique_step_id, validate_flow,
};
use gpui::{CursorStyle, Entity, FontWeight, Hsla, Subscription, Window};
use task_agents::{TaskAgent, TaskAgentSource};
use ui::{
    Checkbox, ContextMenu, PopoverMenu, Tooltip, prelude::*, primary_button, secondary_button,
    segment_option,
};
use util::ResultExt as _;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelTab {
    Step,
    Run,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardView {
    List,
    Canvas,
}

#[derive(Clone)]
pub(super) enum NewStep {
    Agent(String),
    Gate,
    Command,
    Branch,
    Parallel,
    GitHub(GitHubAction),
}

/// A step dragged by its handle to another place in the list.
#[derive(Clone)]
pub(super) struct DraggedStep {
    index: usize,
    label: SharedString,
    icon: IconName,
}

/// An agent or a kind of step dragged from the library into the list.
#[derive(Clone)]
pub(super) struct DraggedNewStep {
    new_step: NewStep,
    label: SharedString,
    icon: IconName,
}

struct DragPreview {
    icon: IconName,
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(30.))
            .px_2p5()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_focused)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(
                Icon::new(self.icon)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

fn github_icon(action: &GitHubAction) -> IconName {
    match action {
        GitHubAction::StartItem => IconName::GitBranchPlus,
        GitHubAction::OpenPullRequest { .. } => IconName::PullRequest,
        GitHubAction::WaitChecks { .. } => IconName::CheckDouble,
        GitHubAction::MoveItem { .. } => IconName::Blocks,
        GitHubAction::CommentItem { .. } => IconName::Chat,
    }
}

/// The line under a GitHub step's name, as board I writes it.
fn github_summary(action: &GitHubAction) -> String {
    match action {
        GitHubAction::StartItem => {
            "cria o branch do item · move para em andamento · atribui você".to_string()
        }
        GitHubAction::OpenPullRequest { draft } => format!(
            "{}fecha o item · revisores do CODEOWNERS",
            if *draft { "rascunho · " } else { "" }
        ),
        GitHubAction::WaitChecks { timeout_minutes } => {
            format!("checks do PR · até {timeout_minutes} min")
        }
        GitHubAction::MoveItem { status } => format!("Status → {status}"),
        GitHubAction::CommentItem { text } if text.trim().is_empty() => {
            "comenta o que a esteira entregou".to_string()
        }
        GitHubAction::CommentItem { text } => format!("comenta “{}”", text.trim()),
    }
}

/// A chip's fill and text: tinted by what it says, as in the step component of the boards.
fn chip_colors(color: Color, cx: &App) -> (Hsla, Color) {
    match color {
        Color::Muted => (cx.theme().colors().element_hover, Color::Muted),
        color => (color.color(cx).opacity(0.14), color),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Override {
    Model,
    Instructions,
}

/// The text fields of the selected step. Rebuilt when the selection or the number of criteria
/// changes; every edit is written back into the flow.
pub(super) struct StepInspector {
    pub step: usize,
    note: Entity<Editor>,
    command: Entity<Editor>,
    instructions: Entity<Editor>,
    file: Entity<Editor>,
    criteria: Vec<CriterionEditors>,
    turns: Entity<Editor>,
    minutes: Entity<Editor>,
    usd: Entity<Editor>,
    pub open_override: Option<Override>,
    pub limits_open: bool,
    _subscriptions: Vec<Subscription>,
}

struct CriterionEditors {
    text: Entity<Editor>,
    command: Entity<Editor>,
}

const VERIFIERS: [(&str, VerifierKind); 4] = [
    ("você", VerifierKind::You),
    ("o próprio agente", VerifierKind::SelfCheck),
    ("outro agente", VerifierKind::Agent),
    ("um comando", VerifierKind::Command),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifierKind {
    You,
    SelfCheck,
    Agent,
    Command,
}

fn verifier_kind(verifier: &Verifier) -> VerifierKind {
    match verifier {
        Verifier::You => VerifierKind::You,
        Verifier::SelfCheck => VerifierKind::SelfCheck,
        Verifier::Agent(_) => VerifierKind::Agent,
        Verifier::Command(_) => VerifierKind::Command,
    }
}

fn section_label(text: &'static str) -> Label {
    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
}

fn description(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
}

fn field(editor: &Entity<Editor>, cx: &App) -> Div {
    div()
        .w_full()
        .px_2p5()
        .py_1p5()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().editor_background)
        .child(editor.clone())
}

impl FlowView {
    pub(super) fn validate(&mut self) {
        self.problems = validate_flow(&self.flow, &self.agents);
    }

    /// Applies a change to the steps. Not while a run uses them.
    pub(super) fn edit_flow(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut flows::Flow),
    ) -> bool {
        if self.is_running() {
            self.show_toast("Pare a execução antes de mudar os passos.", cx);
            return false;
        }
        change(&mut self.flow);
        self.saved = false;
        self.step_threads.resize(self.flow.steps.len(), Vec::new());
        self.validate();
        cx.notify();
        true
    }

    pub(super) fn show_toast(&self, message: &str, cx: &mut Context<Self>) {
        let message = message.to_string();
        self.workspace
            .update(cx, |workspace, cx| {
                use workspace::notifications::NotificationId;
                struct FlowEditorToast;
                workspace.show_toast(
                    workspace::Toast::new(NotificationId::unique::<FlowEditorToast>(), message),
                    cx,
                );
            })
            .log_err();
    }

    pub(super) fn insert_step(
        &mut self,
        at: usize,
        new_step: NewStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = match new_step {
            NewStep::Agent(command) => FlowStep {
                id: unique_step_id(&self.flow.steps, &command),
                kind: StepKind::Agent(AgentStep::new(command)),
            },
            NewStep::Gate => FlowStep {
                id: unique_step_id(&self.flow.steps, "aprovacao"),
                kind: StepKind::Gate(GateStep {
                    note: String::new(),
                    adjust_to: None,
                }),
            },
            NewStep::Command => FlowStep {
                id: unique_step_id(&self.flow.steps, "comando"),
                kind: StepKind::Run(RunStep {
                    command: "npm test".to_string(),
                    on_fail: OnFail::AskYou,
                }),
            },
            NewStep::Branch => {
                let output = self
                    .flow
                    .available_inputs(at, &self.agents)
                    .into_iter()
                    .rev()
                    .find(|input| input.from_step.is_some())
                    .map(|input| input.name)
                    .unwrap_or_default();
                FlowStep {
                    id: unique_step_id(&self.flow.steps, "se"),
                    kind: StepKind::Branch(BranchStep {
                        output,
                        contains: String::new(),
                        then_to: None,
                        else_to: None,
                    }),
                }
            }
            NewStep::GitHub(action) => {
                // Waiting for checks goes back to the last agent before it with the failed
                // check's log, as the boards draw it.
                let on_fail = match action {
                    GitHubAction::WaitChecks { .. } => self.flow.steps
                        [..at.min(self.flow.steps.len())]
                        .iter()
                        .rev()
                        .find(|step| matches!(step.kind, StepKind::Agent(_)))
                        .map_or(OnFail::AskYou, |step| OnFail::BackTo {
                            step: step.id.clone(),
                            max: 1,
                        }),
                    _ => OnFail::AskYou,
                };
                let action = match action {
                    GitHubAction::MoveItem { status } if status.is_empty() => {
                        GitHubAction::MoveItem {
                            status: "Em revisão".to_string(),
                        }
                    }
                    action => action,
                };
                let mut step = FlowStep {
                    id: String::new(),
                    kind: StepKind::GitHub(GitHubStep { action, on_fail }),
                };
                let base = match &step.kind {
                    StepKind::GitHub(github) => match github.action {
                        GitHubAction::StartItem => "comecar",
                        GitHubAction::OpenPullRequest { .. } => "pr",
                        GitHubAction::WaitChecks { .. } => "checks",
                        GitHubAction::MoveItem { .. } => "mover",
                        GitHubAction::CommentItem { .. } => "comentar",
                    },
                    _ => "github",
                };
                step.id = unique_step_id(&self.flow.steps, base);
                step
            }
            NewStep::Parallel => FlowStep {
                id: unique_step_id(&self.flow.steps, "paralelo"),
                kind: StepKind::Parallel(ParallelStep {
                    agents: Vec::new(),
                    read_only: true,
                    on_fail: OnFail::AskYou,
                }),
            },
        };
        let at = at.min(self.flow.steps.len());
        if self.edit_flow(cx, |flow| flow.steps.insert(at, step)) {
            self.insert_at = None;
            self.select_step(Some(at), window, cx);
        }
    }

    fn move_step(&mut self, index: usize, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|target| *target < self.flow.steps.len())
        };
        let Some(target) = target else {
            return;
        };
        if self.edit_flow(cx, |flow| flow.steps.swap(index, target)) {
            self.select_step(Some(target), window, cx);
        }
    }

    /// Moves the step at `from` to the insert point `at`, the gap before the step now at `at`.
    pub(super) fn move_step_to(
        &mut self,
        from: usize,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from >= self.flow.steps.len() || at == from || at == from + 1 {
            return;
        }
        let target = if at > from { at - 1 } else { at };
        if self.edit_flow(cx, |flow| {
            let step = flow.steps.remove(from);
            flow.steps.insert(target, step);
        }) {
            self.select_step(Some(target), window, cx);
        }
    }

    pub(super) fn move_selected(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.move_step(index, up, window, cx);
        }
    }

    pub(super) fn remove_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.remove_step(index, window, cx);
        }
    }

    fn remove_step(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.flow.steps.len() {
            return;
        }
        if self.edit_flow(cx, |flow| {
            flow.steps.remove(index);
        }) {
            let next = (!self.flow.steps.is_empty()).then(|| index.min(self.flow.steps.len() - 1));
            self.select_step(next, window, cx);
        }
    }

    fn duplicate_step(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.flow.steps.get(index).cloned() else {
            return;
        };
        if self.edit_flow(cx, |flow| {
            let id = unique_step_id(&flow.steps, &step.id);
            flow.steps.insert(index + 1, FlowStep { id, ..step });
        }) {
            self.select_step(Some(index + 1), window, cx);
        }
    }

    pub(super) fn select_step(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = index;
        self.panel_tab = PanelTab::Step;
        self.inspector = index.map(|index| self.build_inspector(index, None, false, window, cx));
        cx.notify();
    }

    fn rebuild_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let (step, open_override, limits_open) = (
            inspector.step,
            inspector.open_override,
            inspector.limits_open,
        );
        self.inspector = Some(self.build_inspector(step, open_override, limits_open, window, cx));
        cx.notify();
    }

    fn build_inspector(
        &self,
        step: usize,
        open_override: Option<Override>,
        limits_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> StepInspector {
        let mut subscriptions = Vec::new();
        let mut text_editor =
            |text: &str, placeholder: &str, multi_line: bool, cx: &mut Context<Self>| {
                let editor = cx.new(|cx| {
                    let mut editor = if multi_line {
                        Editor::auto_height(2, 12, window, cx)
                    } else {
                        Editor::single_line(window, cx)
                    };
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(text.to_string(), window, cx);
                    editor
                });
                subscriptions.push(cx.subscribe(
                    &editor,
                    |this: &mut Self, _, event: &EditorEvent, cx| {
                        if matches!(event, EditorEvent::BufferEdited) {
                            this.sync_inspector_text(cx);
                        }
                    },
                ));
                editor
            };
        let flow_step = self.flow.steps.get(step);
        let (note, command, instructions, file, criteria, limits) =
            match flow_step.map(|step| &step.kind) {
                Some(StepKind::Gate(gate)) => (
                    gate.note.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                    Vec::new(),
                    Default::default(),
                ),
                Some(StepKind::Run(run)) => (
                    String::new(),
                    run.command.clone(),
                    String::new(),
                    String::new(),
                    Vec::new(),
                    Default::default(),
                ),
                Some(StepKind::Branch(branch)) => (
                    branch.contains.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                    Vec::new(),
                    Default::default(),
                ),
                Some(StepKind::Parallel(_)) => Default::default(),
                Some(StepKind::GitHub(github)) => (
                    match &github.action {
                        GitHubAction::MoveItem { status } => status.clone(),
                        GitHubAction::CommentItem { text } => text.clone(),
                        _ => String::new(),
                    },
                    match &github.action {
                        GitHubAction::WaitChecks { timeout_minutes } => timeout_minutes.to_string(),
                        _ => String::new(),
                    },
                    String::new(),
                    String::new(),
                    Vec::new(),
                    Default::default(),
                ),
                Some(StepKind::Agent(agent_step)) => (
                    String::new(),
                    String::new(),
                    agent_step.instructions.clone(),
                    agent_step
                        .where_
                        .as_ref()
                        .and_then(|target| target.file.clone())
                        .unwrap_or_default(),
                    agent_step.criteria.clone(),
                    agent_step.limits,
                ),
                None => Default::default(),
            };
        let limits: flows::StepLimits = limits;
        let note = text_editor(&note, "Ex.: as telas do passo 2", false, cx);
        let command = text_editor(&command, "npm test", false, cx);
        let instructions = text_editor(
            &instructions,
            "Somadas às instruções do agente só nesta esteira",
            true,
            cx,
        );
        let file = text_editor(&file, "Arquivo ou link · página", false, cx);
        let criteria = criteria
            .iter()
            .map(|criterion| CriterionEditors {
                text: text_editor(&criterion.text, "Descreva o critério", false, cx),
                command: text_editor(
                    match &criterion.verify {
                        Verifier::Command(command) => command,
                        _ => "",
                    },
                    "npm test",
                    false,
                    cx,
                ),
            })
            .collect();
        let turns = text_editor(&limits.turns.to_string(), "30", false, cx);
        let minutes = text_editor(&limits.minutes.to_string(), "20", false, cx);
        let usd = text_editor(&limits.usd.to_string(), "1", false, cx);
        StepInspector {
            step,
            note,
            command,
            instructions,
            file,
            criteria,
            turns,
            minutes,
            usd,
            open_override,
            limits_open,
            _subscriptions: subscriptions,
        }
    }

    /// Writes the inspector's text fields into the selected step.
    fn sync_inspector_text(&mut self, cx: &mut Context<Self>) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let step = inspector.step;
        let note = inspector.note.read(cx).text(cx);
        let command = inspector.command.read(cx).text(cx);
        let instructions = inspector.instructions.read(cx).text(cx);
        let file = inspector.file.read(cx).text(cx);
        let criteria: Vec<(String, String)> = inspector
            .criteria
            .iter()
            .map(|editors| {
                (
                    editors.text.read(cx).text(cx),
                    editors.command.read(cx).text(cx),
                )
            })
            .collect();
        let turns = inspector.turns.read(cx).text(cx);
        let minutes = inspector.minutes.read(cx).text(cx);
        let usd = inspector.usd.read(cx).text(cx);
        self.edit_flow(cx, |flow| {
            let Some(flow_step) = flow.steps.get_mut(step) else {
                return;
            };
            match &mut flow_step.kind {
                StepKind::Gate(gate) => gate.note = note,
                StepKind::Run(run) => run.command = command,
                StepKind::Branch(branch) => branch.contains = note,
                StepKind::Parallel(_) => {}
                StepKind::GitHub(github) => match &mut github.action {
                    GitHubAction::MoveItem { status } => *status = note.trim().to_string(),
                    GitHubAction::CommentItem { text } => *text = note,
                    GitHubAction::WaitChecks { timeout_minutes } => {
                        if let Ok(minutes) = command.trim().trim_end_matches('m').parse() {
                            *timeout_minutes = minutes;
                        }
                    }
                    GitHubAction::StartItem | GitHubAction::OpenPullRequest { .. } => {}
                },
                StepKind::Agent(agent_step) => {
                    agent_step.instructions = instructions;
                    if let Some(target) = &mut agent_step.where_ {
                        target.file = (!file.trim().is_empty()).then(|| file.trim().to_string());
                    }
                    for (criterion, (text, command)) in agent_step.criteria.iter_mut().zip(criteria)
                    {
                        criterion.text = text;
                        if let Verifier::Command(existing) = &mut criterion.verify {
                            *existing = command;
                        }
                    }
                    if let Ok(turns) = turns.trim().parse() {
                        agent_step.limits.turns = turns;
                    }
                    if let Ok(minutes) = minutes.trim().parse() {
                        agent_step.limits.minutes = minutes;
                    }
                    if let Ok(usd) = usd.trim().replace(',', ".").parse() {
                        agent_step.limits.usd = usd;
                    }
                }
            }
        });
    }

    fn edit_agent_step(
        &mut self,
        step: usize,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut AgentStep),
    ) -> bool {
        self.edit_flow(cx, |flow| {
            if let Some(FlowStep {
                kind: StepKind::Agent(agent_step),
                ..
            }) = flow.steps.get_mut(step)
            {
                change(agent_step);
            }
        })
    }

    fn agent(&self, command: &str) -> Option<&TaskAgent> {
        self.agents.iter().find(|agent| agent.command == command)
    }

    /// Opens the `.flow.md` as text. Not through the project's opener, which would bring the
    /// file back to this editor.
    pub(super) fn open_flow_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.flow.file_path.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let save = if self.saved {
            gpui::Task::ready(true)
        } else {
            self.save(cx)
        };
        cx.spawn_in(window, async move |_this, cx| {
            if !save.await {
                return anyhow::Ok(());
            }
            let buffer = project
                .update(cx, |project, cx| project.open_local_buffer(&path, cx))
                .await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let editor =
                    cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Writes the flow to its file. The task resolves to whether it was written.
    pub(super) fn save(&mut self, cx: &mut Context<Self>) -> gpui::Task<bool> {
        if self.load_error.is_some() {
            return gpui::Task::ready(true);
        }
        if self.flow.steps.is_empty() {
            self.show_toast("Adicione ao menos um passo antes de salvar.", cx);
            return gpui::Task::ready(false);
        }
        let markdown = match self.flow.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                self.show_toast(&format!("Não foi possível salvar: {error:#}"), cx);
                return gpui::Task::ready(false);
            }
        };
        let fs = self.fs.clone();
        let path = self.flow.file_path.clone();
        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<()> = async {
                if let Some(parent) = path.parent() {
                    fs.create_dir(parent).await?;
                }
                fs.atomic_write(path.clone(), markdown).await
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.saved = true;
                    this.ever_saved = true;
                    flows::notify_flows_changed(cx);
                    this.refresh_flow_commands(cx);
                    cx.notify();
                    true
                }
                Err(error) => {
                    this.show_toast(&format!("Não foi possível salvar: {error:#}"), cx);
                    false
                }
            })
            .unwrap_or(false)
        })
    }

    /// The composer's slash menu comes from the native agent, which rereads the flows when its
    /// project context refreshes.
    fn refresh_flow_commands(&self, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if let Some(panel) = workspace.read(cx).panel::<crate::AgentPanel>(cx) {
            panel.update(cx, |panel, cx| panel.refresh_skills(cx));
        }
    }

    /// A new flow follows its name until it is saved for the first time; after that the file
    /// (and the `/command`) stays put.
    fn sync_name(&mut self, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx);
        let name = name.trim().to_string();
        if name.is_empty() || name == self.flow.name {
            return;
        }
        let ever_saved = self.ever_saved;
        self.edit_flow(cx, |flow| {
            flow.name = name.clone();
            if !ever_saved {
                let id = task_agents::command_slug(&name);
                if let Some(parent) = flow.file_path.parent() {
                    flow.file_path = parent.join(format!("{id}{}", flows::FLOW_FILE_SUFFIX));
                }
                flow.id = id;
            }
        });
    }

    pub(super) fn subscribe_name_editor(&mut self, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(
            &self.name_editor,
            |this: &mut Self, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.sync_name(cx);
                }
            },
        );
        self._subscriptions.push(subscription);
        let subscription = cx.subscribe(
            &self.library_search,
            |_: &mut Self, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            },
        );
        self._subscriptions.push(subscription);
    }

    // ---------- library ----------

    pub(super) fn render_library(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let query = self.library_search.read(cx).text(cx).trim().to_lowercase();
        let matches = |agent: &TaskAgent| {
            query.is_empty()
                || agent.name.to_lowercase().contains(&query)
                || agent.command.contains(&query)
        };
        let personal: Vec<&TaskAgent> = self
            .agents
            .iter()
            .filter(|agent| matches!(agent.source, TaskAgentSource::Personal) && matches(agent))
            .collect();
        let project: Vec<&TaskAgent> = self
            .agents
            .iter()
            .filter(|agent| !matches!(agent.source, TaskAgentSource::Personal) && matches(agent))
            .collect();
        let at = self.insert_at.unwrap_or(self.flow.steps.len());
        let item = |id: SharedString,
                    icon: IconName,
                    icon_color: Color,
                    command: Option<String>,
                    name: String,
                    new_step: NewStep,
                    cx: &mut Context<Self>| {
            h_flex()
                .id(id)
                .h(px(30.))
                .px_2()
                .gap_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().colors().element_hover))
                .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
                .when_some(command, |this, command| {
                    this.child(
                        Label::new(format!("/{command}"))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Accent),
                    )
                })
                .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
                .child(div().flex_1())
                .child(Label::new("+").color(Color::Muted))
                .tooltip(Tooltip::text("Clique ou arraste para a esteira"))
                .on_drag(
                    DraggedNewStep {
                        new_step: new_step.clone(),
                        label: name.into(),
                        icon,
                    },
                    |dragged, _offset, _window, cx| {
                        cx.new(|_| DragPreview {
                            icon: dragged.icon,
                            label: dragged.label.clone(),
                        })
                    },
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.insert_step(at, new_step.clone(), window, cx)
                }))
        };
        let section = |title: &'static str, count: Option<usize>| {
            h_flex()
                .px_3()
                .pt_3()
                .pb_1()
                .child(section_label(title))
                .child(div().flex_1())
                .when_some(count, |this, count| {
                    this.child(description(count.to_string()))
                })
        };
        let mut list = v_flex()
            .child(section("SÓ PARA MIM", Some(personal.len())))
            .child(
                v_flex()
                    .px_1()
                    .gap_px()
                    .children(personal.iter().map(|agent| {
                        item(
                            format!("library-{}", agent.command).into(),
                            IconName::Person,
                            Color::Muted,
                            Some(agent.command.clone()),
                            agent.name.clone(),
                            NewStep::Agent(agent.command.clone()),
                            cx,
                        )
                    })),
            )
            .child(section("DO PROJETO", Some(project.len())))
            .child(
                v_flex()
                    .px_1()
                    .gap_px()
                    .children(project.iter().map(|agent| {
                        item(
                            format!("library-project-{}", agent.command).into(),
                            IconName::Person,
                            Color::Muted,
                            Some(agent.command.clone()),
                            agent.name.clone(),
                            NewStep::Agent(agent.command.clone()),
                            cx,
                        )
                    })),
            )
            .child(section("PASSOS", None));
        list = list.child(
            v_flex()
                .px_1()
                .gap_px()
                .child(item(
                    "library-gate".into(),
                    IconName::UserCheck,
                    Color::Warning,
                    None,
                    "Você aprova".to_string(),
                    NewStep::Gate,
                    cx,
                ))
                .child(item(
                    "library-command".into(),
                    IconName::Terminal,
                    Color::Success,
                    None,
                    "Rodar comando".to_string(),
                    NewStep::Command,
                    cx,
                ))
                .child(item(
                    "library-branch".into(),
                    IconName::GitBranch,
                    Color::Accent,
                    None,
                    "Se / senão".to_string(),
                    NewStep::Branch,
                    cx,
                ))
                .child(item(
                    "library-parallel".into(),
                    IconName::ListTree,
                    Color::Accent,
                    None,
                    "Em paralelo".to_string(),
                    NewStep::Parallel,
                    cx,
                )),
        );
        if let Some(repository) = task_agents::github_steps_repository(&self.project, cx) {
            list =
                list.child(
                    h_flex()
                        .px_3()
                        .pt_3()
                        .pb_1()
                        .child(section_label("GITHUB"))
                        .child(div().flex_1())
                        .child(description(repository)),
                )
                .child(v_flex().px_1().gap_px().children(
                    GitHubAction::ALL.into_iter().map(|action| {
                        item(
                            format!("library-github-{}", action.key()).into(),
                            github_icon(&action),
                            Color::Accent,
                            None,
                            action.label().to_string(),
                            NewStep::GitHub(action),
                            cx,
                        )
                    }),
                ));
        }
        v_flex()
            .w(px(252.))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new("Meus agentes").size(LabelSize::Small).weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("new-task-agent-from-flow", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Novo agente"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenTaskAgents), cx)
                            }),
                    ),
            )
            .child(
                div().p_2().child(
                    h_flex()
                        .gap_1p5()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.editor_background)
                        .child(Icon::new(IconName::MagnifyingGlass).size(IconSize::Small).color(Color::Muted))
                        .child(div().flex_1().min_w_0().child(self.library_search.clone())),
                ),
            )
            .child(
                div()
                    .id("flow-library")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                div()
                    .p_3()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(description(if self.insert_at.is_some() {
                        "Clique para inserir no ponto marcado na esteira."
                    } else {
                        "Clique ou arraste para a esteira. Cada agente traz a sua config; o passo pode sobrescrever só nesta esteira."
                    })),
            )
    }

    // ---------- toolbar ----------

    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        let colors = cx.theme().colors();
        let can_suggest = !running && self.suggesting.is_none();
        h_flex()
            .h(px(50.))
            .flex_none()
            .px_3p5()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .w(px(260.))
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .hover(|this| this.border_color(colors.border))
                    .child(self.name_editor.clone()),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .w(px(150.))
                    .p_0p5()
                    .gap_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .child(
                        segment_option(
                            "board-list",
                            "Lista",
                            self.board_view == BoardView::List,
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.board_view = BoardView::List;
                            cx.notify();
                        })),
                    )
                    .child(
                        segment_option(
                            "board-canvas",
                            "Canvas",
                            self.board_view == BoardView::Canvas,
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.board_view = BoardView::Canvas;
                            cx.notify();
                        })),
                    ),
            )
            .child(
                secondary_button("suggest-steps", None, "Sugerir passos", can_suggest, cx).when(
                    can_suggest,
                    |this| {
                        this.on_click(
                            cx.listener(|this, _, window, cx| this.suggest_steps(window, cx)),
                        )
                    },
                ),
            )
            .child(
                secondary_button("show-flow-file", None, ".flow.md", true, cx)
                    .tooltip(Tooltip::text("Editar como texto"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_flow_file(window, cx))),
            )
            .child(
                secondary_button(
                    "save-flow",
                    None,
                    if self.saved { "Salva" } else { "Salvar" },
                    !self.saved,
                    cx,
                )
                .when(!self.saved, |this| {
                    this.on_click(cx.listener(|this, _, _window, cx| this.save(cx).detach()))
                }),
            )
            .child(if running {
                secondary_button(
                    "stop-flow",
                    Some((IconName::Stop, Color::Error)),
                    "Parar",
                    true,
                    cx,
                )
                .on_click(
                    cx.listener(|this, _, _window, cx| {
                        this.stop(Some("você parou a esteira."), cx)
                    }),
                )
                .into_any_element()
            } else {
                primary_button("run-flow", Some(IconName::PlayFilled), "Rodar", None, cx)
                    .on_click(cx.listener(|this, _, window, cx| this.rerun(window, cx)))
                    .into_any_element()
            })
    }

    // ---------- board ----------

    pub(super) fn render_board(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let request = self.input_editor.read(cx).text(cx);
        let cited = super::cited_project_item(&self.project, &request, cx);
        let ask = v_flex()
            .gap_1p5()
            .px_3p5()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.panel_background)
            .child(section_label(if cited.is_some() {
                "VEM DO GITHUB PROJECTS"
            } else {
                "O QUE CONSTRUIR"
            }))
            .when_some(cited.clone(), |this, item| {
                let item_id = item.item_id.clone();
                this.child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            h_flex()
                                .id("flow-cited-item")
                                .h(px(24.))
                                .px_2()
                                .gap_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border_focused)
                                .bg(colors.border_focused.opacity(0.15))
                                .cursor_pointer()
                                .tooltip(Tooltip::text("Abrir no quadro"))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(zed_actions::github_projects::OpenProjectItem {
                                            item_id: item_id.clone(),
                                        }),
                                        cx,
                                    )
                                })
                                .child(
                                    Icon::new(IconName::ListTodo)
                                        .size(IconSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(format!(
                                        "{} {}",
                                        item.reference
                                            .find('#')
                                            .map_or(item.reference.as_str(), |at| &item.reference[at..]),
                                        item.title
                                    ))
                                    .size(LabelSize::Small),
                                ),
                        )
                        .children(item.status.map(|status| {
                            let (fill, color) = chip_colors(Color::Muted, cx);
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(fill)
                                .child(Label::new(status).size(LabelSize::XSmall).color(color))
                        })),
                )
            })
            .child(self.input_editor.clone())
            .child(description(if cited.is_some() {
                "Título, corpo, subtarefas e campos do item viram a entrada do 1º passo. Rodar de um item: /esteira #204 no chat."
            } else {
                "Vira a entrada do 1º passo. Muda a cada execução sem mexer nos passos."
            }));
        let mut board = v_flex().child(ask);
        if let Some(error) = &self.load_error {
            board = board.child(div().h_4()).child(
                v_flex()
                    .gap_2()
                    .px_3()
                    .py_2p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().status().error_border)
                    .bg(cx.theme().status().error_background)
                    .child(
                        Label::new("O arquivo não abre como esteira.")
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("edit-broken-flow", "Editar como texto")
                            .style(ButtonStyle::Outlined)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_flow_file(window, cx)),
                            ),
                    ),
            );
        } else if self.board_view == BoardView::Canvas {
            board = board.child(div().h_4()).child(self.render_canvas(cx));
        } else {
            board = board.child(self.render_insert_point(0, cx));
            for (index, flow_step) in self.flow.steps.iter().enumerate() {
                board = board.child(self.render_step(index, flow_step, cx));
                for problem in self
                    .problems
                    .iter()
                    .filter(|problem| problem.step() == Some(index))
                {
                    board = board.child(
                        h_flex()
                            .gap_1p5()
                            .pl(px(54.))
                            .pt_1p5()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(problem.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                            ),
                    );
                }
                board = board.child(self.render_insert_point(index + 1, cx));
            }
            if !self.flow.steps.is_empty() {
                let binding = |action: &dyn gpui::Action, cx: &App| {
                    ui::KeyBinding::for_action_in(action, &self.focus_handle, cx)
                        .size(rems_from_px(11_f32))
                };
                let hint = |text: &'static str| {
                    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
                };
                board = board.child(
                    h_flex()
                        .pt_2()
                        .gap_1()
                        .child(binding(&super::SubirPasso, cx))
                        .child(binding(&super::DescerPasso, cx))
                        .child(hint("move o passo selecionado ·"))
                        .child(binding(&super::RemoverPasso, cx))
                        .child(hint("remove · arraste pela alça ou da coluna da esquerda")),
                );
            }
            if self.flow.steps.is_empty() {
                board = board.child(
                    div().py_8().flex().justify_center().child(
                        Label::new("Esteira vazia. Clique ou arraste um agente da esquerda, use + ou peça “Sugerir passos”.")
                            .color(Color::Muted),
                    ),
                );
            }
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

    fn render_insert_point(&self, at: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let agents = self.agents.clone();
        let has_github = task_agents::github_steps_repository(&self.project, cx).is_some();
        let marked = self.insert_at == Some(at);
        let dragging = cx.has_active_drag();
        let line = cx.theme().colors().border;
        let accent = cx.theme().colors().text_accent;
        h_flex()
            .id(("insert-point", at))
            .h(px(if dragging { 34. } else { 26. }))
            .pl(px(32.))
            .relative()
            .rounded_md()
            .drag_over::<DraggedStep>(move |style, _, _, _| style.bg(accent.opacity(0.12)))
            .drag_over::<DraggedNewStep>(move |style, _, _, _| style.bg(accent.opacity(0.12)))
            .on_drop(cx.listener(move |this, dragged: &DraggedStep, window, cx| {
                this.move_step_to(dragged.index, at, window, cx)
            }))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedNewStep, window, cx| {
                    this.insert_step(at, dragged.new_step.clone(), window, cx)
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(42.))
                    .top_0()
                    .bottom_0()
                    .w_px()
                    .bg(line),
            )
            .when(marked, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(if dragging { 16. } else { 12. }))
                        .h(px(2.))
                        .rounded_full()
                        .bg(cx.theme().colors().text_accent),
                )
            })
            .child(
                PopoverMenu::new(("insert-step", at))
                    .trigger(
                        IconButton::new(("insert-step-button", at), IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .shape(ui::IconButtonShape::Square)
                            .style(ButtonStyle::Outlined)
                            .tooltip(Tooltip::text("Adicionar passo aqui")),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        let agents = agents.clone();
                        Some(ContextMenu::build(
                            window,
                            cx,
                            move |mut menu, _window, _cx| {
                                menu = menu.header("Agentes");
                                for agent in &agents {
                                    let view = view.clone();
                                    let command = agent.command.clone();
                                    menu = menu.entry(
                                        format!("{} · /{}", agent.name, agent.command),
                                        None,
                                        move |window, cx| {
                                            view.update(cx, |view, cx| {
                                                view.insert_step(
                                                    at,
                                                    NewStep::Agent(command.clone()),
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .log_err();
                                        },
                                    );
                                }
                                menu = menu.separator().header("Passos");
                                let mut kinds = vec![
                                    ("Você aprova", NewStep::Gate),
                                    ("Rodar comando", NewStep::Command),
                                    ("Se / senão", NewStep::Branch),
                                    ("Em paralelo", NewStep::Parallel),
                                ];
                                if has_github {
                                    kinds.extend(
                                        GitHubAction::ALL.into_iter().map(|action| {
                                            (action.label(), NewStep::GitHub(action))
                                        }),
                                    );
                                }
                                for (label, new_step) in kinds {
                                    let view = view.clone();
                                    menu = menu.entry(label, None, move |window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.insert_step(at, new_step.clone(), window, cx)
                                        })
                                        .log_err();
                                    });
                                }
                                let view = view.clone();
                                menu.separator().entry(
                                    "Marcar este ponto para a biblioteca",
                                    None,
                                    move |_window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.insert_at = Some(at);
                                            cx.notify();
                                        })
                                        .log_err();
                                    },
                                )
                            },
                        ))
                    }),
            )
    }

    fn step_chips(&self, flow_step: &FlowStep) -> Vec<(String, Color)> {
        let mut chips = Vec::new();
        match &flow_step.kind {
            StepKind::Agent(agent_step) => {
                let count = agent_step.criteria.len();
                chips.push((
                    format!("{count} critério{}", if count == 1 { "" } else { "s" }),
                    Color::Muted,
                ));
                if let Some(agent) = self.agent(&agent_step.agent)
                    && !agent.gives.is_empty()
                {
                    chips.push((
                        format!("entrega {}", agent.gives.join(" + ")),
                        Color::Accent,
                    ));
                }
                if let OnFail::BackTo { step, .. } = &agent_step.on_fail
                    && let Some(target) = self.flow.step_index(step)
                {
                    chips.push((format!("volta → {}", target + 1), Color::Error));
                }
            }
            StepKind::Gate(_) => chips.push(("portão".to_string(), Color::Warning)),
            StepKind::Run(run) => {
                if let OnFail::BackTo { step, .. } = &run.on_fail
                    && let Some(target) = self.flow.step_index(step)
                {
                    chips.push((format!("volta → {}", target + 1), Color::Error));
                }
            }
            StepKind::Branch(branch) => {
                let target = |id: &Option<String>| {
                    id.as_deref()
                        .and_then(|id| self.flow.step_index(id))
                        .map(|target| format!("{}", target + 1))
                        .unwrap_or_else(|| "próximo".to_string())
                };
                chips.push((format!("sim → {}", target(&branch.then_to)), Color::Success));
                chips.push((format!("não → {}", target(&branch.else_to)), Color::Muted));
            }
            StepKind::Parallel(parallel) => {
                chips.push((format!("{} agentes", parallel.agents.len()), Color::Muted));
                if parallel.read_only {
                    chips.push(("só leitura".to_string(), Color::Muted));
                }
            }
            StepKind::GitHub(github) => {
                if matches!(github.action, GitHubAction::WaitChecks { .. }) {
                    chips.push(("1 critério".to_string(), Color::Muted));
                }
                let outputs = github.action.outputs();
                if let Some(output) = outputs.first() {
                    chips.push((format!("entrega {output}"), Color::Accent));
                }
                if let OnFail::BackTo { step, .. } = &github.on_fail
                    && let Some(target) = self.flow.step_index(step)
                {
                    chips.push((format!("volta → {}", target + 1), Color::Error));
                }
            }
        }
        chips
    }

    fn render_step(
        &self,
        index: usize,
        flow_step: &FlowStep,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let state = self
            .run
            .as_ref()
            .and_then(|run| run.steps.get(index))
            .cloned();
        let selected = self.selected == Some(index);
        let (icon, icon_color, title, command, subtitle) = match &flow_step.kind {
            StepKind::Agent(agent_step) => {
                let agent = self.agent(&agent_step.agent);
                let model = agent_step
                    .model
                    .clone()
                    .or_else(|| agent.and_then(|agent| agent.model.clone()))
                    .unwrap_or_else(|| "modelo da thread".to_string());
                let mut subtitle = String::new();
                if let Some(target) = agent_step
                    .where_
                    .as_ref()
                    .filter(|target| target.tool != DesignTool::Code)
                {
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
                    flow_step.label(&self.agents),
                    Some(format!("/{}", agent_step.agent)),
                    subtitle,
                )
            }
            StepKind::Gate(gate) => (
                IconName::UserCheck,
                Color::Warning,
                "Você aprova".to_string(),
                None,
                if gate.note.is_empty() {
                    "o que o passo anterior entregou".to_string()
                } else {
                    gate.note.clone()
                },
            ),
            StepKind::Run(run_step) => (
                IconName::Terminal,
                Color::Success,
                "Rodar comando".to_string(),
                None,
                format!("{} · passa se sair 0", run_step.command),
            ),
            StepKind::GitHub(github) => (
                github_icon(&github.action),
                Color::Accent,
                github.action.label().to_string(),
                Some("GitHub".to_string()),
                github_summary(&github.action),
            ),
            StepKind::Branch(branch) => (
                IconName::GitBranch,
                Color::Accent,
                "Se / senão".to_string(),
                None,
                if branch.output.is_empty() {
                    "escolha a saída que a condição olha".to_string()
                } else {
                    format!("se {} contém “{}”", branch.output, branch.contains)
                },
            ),
            StepKind::Parallel(parallel) => (
                IconName::ListTree,
                Color::Accent,
                flow_step.label(&self.agents),
                None,
                if parallel.agents.is_empty() {
                    "adicione os agentes que rodam juntos".to_string()
                } else {
                    format!(
                        "{} ao mesmo tempo{}",
                        parallel
                            .agents
                            .iter()
                            .map(|command| format!("/{command}"))
                            .collect::<Vec<_>>()
                            .join(" + "),
                        if parallel.read_only {
                            " · só leitura"
                        } else {
                            ""
                        }
                    )
                },
            ),
        };
        let is_gate = matches!(flow_step.kind, StepKind::Gate(_));
        let dimmed = state
            .as_ref()
            .is_some_and(|state| state.state == flows::run::StepState::Queued);
        let threads = self.step_threads.get(index).cloned().unwrap_or_default();
        let last = self.flow.steps.len().saturating_sub(1);
        let view = cx.entity().downgrade();
        let accent = colors.text_accent;
        let is_command = matches!(flow_step.kind, StepKind::Run(_));
        let tile = if is_gate {
            cx.theme().status().warning_background
        } else if is_command {
            cx.theme().status().success.opacity(0.14)
        } else {
            accent.opacity(0.14)
        };
        let dragged = DraggedStep {
            index,
            label: title.clone().into(),
            icon,
        };
        h_flex()
            .id(("flow-step", index))
            .gap_3()
            .p_3()
            .rounded_lg()
            .border_1()
            .cursor_pointer()
            .border_color(if selected {
                accent
            } else if is_gate {
                cx.theme().status().warning_border
            } else {
                colors.border
            })
            .bg(if selected {
                accent.opacity(0.10)
            } else {
                colors.panel_background
            })
            .when(dimmed, |this| this.opacity(0.5))
            .drag_over::<DraggedStep>(move |style, _, _, _| style.border_color(accent))
            .drag_over::<DraggedNewStep>(move |style, _, _, _| style.border_color(accent))
            .on_drop(cx.listener(move |this, dragged: &DraggedStep, window, cx| {
                this.move_step_to(dragged.index, index, window, cx)
            }))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedNewStep, window, cx| {
                    this.insert_step(index, dragged.new_step.clone(), window, cx)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.select_step(Some(index), window, cx)
            }))
            .child(
                div()
                    .id(("step-handle", index))
                    .flex_none()
                    .cursor(CursorStyle::OpenHand)
                    .tooltip(Tooltip::text("Arraste para mudar a ordem"))
                    .child(
                        Icon::new(IconName::Menu)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .on_drag(dragged, |dragged, _offset, _window, cx| {
                        cx.new(|_| DragPreview {
                            icon: dragged.icon,
                            label: dragged.label.clone(),
                        })
                    }),
            )
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
                    .bg(tile)
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
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(title)
                                    .weight(FontWeight::SEMIBOLD)
                                    .when(is_gate, |this| this.color(Color::Warning))
                                    .truncate(),
                            )
                            .when_some(command, |this, command| {
                                this.child(
                                    Label::new(command)
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Accent),
                                )
                            }),
                    )
                    .child(
                        Label::new(subtitle)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .children(self.step_chips(flow_step).into_iter().map(|(text, color)| {
                        let (fill, text_color) = chip_colors(color, cx);
                        div()
                            .h(px(22.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .bg(fill)
                            .child(Label::new(text).size(LabelSize::XSmall).color(text_color))
                    })),
            )
            .when_some(state, |this, state| {
                this.child(super::state_chip(state.state, state.note, cx))
            })
            .when_some(threads.last().copied(), |this, thread_id| {
                this.child(
                    IconButton::new(("open-step-thread", index), IconName::Thread)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Abrir a thread do passo"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_thread(thread_id, window, cx)
                        })),
                )
            })
            .child(
                v_flex()
                    .child(
                        IconButton::new(("step-up", index), IconName::ChevronUp)
                            .icon_size(IconSize::XSmall)
                            .disabled(index == 0)
                            .tooltip(Tooltip::text("Mover para cima"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.move_step(index, true, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new(("step-down", index), IconName::ChevronDown)
                            .icon_size(IconSize::XSmall)
                            .disabled(index == last)
                            .tooltip(Tooltip::text("Mover para baixo"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.move_step(index, false, window, cx)
                            })),
                    ),
            )
            .child(
                PopoverMenu::new(("step-menu", index))
                    .trigger(
                        IconButton::new(("step-menu-button", index), IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Mais ações")),
                    )
                    .menu(move |window, cx| {
                        let view = view.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            let duplicate = view.clone();
                            let remove = view.clone();
                            menu.entry("Duplicar", None, move |window, cx| {
                                duplicate
                                    .update(cx, |view, cx| view.duplicate_step(index, window, cx))
                                    .log_err();
                            })
                            .entry(
                                "Remover",
                                None,
                                move |window, cx| {
                                    remove
                                        .update(cx, |view, cx| view.remove_step(index, window, cx))
                                        .log_err();
                                },
                            )
                        }))
                    }),
            )
    }

    fn render_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut nodes = h_flex().gap_0().items_center().flex_wrap();
        for (index, flow_step) in self.flow.steps.iter().enumerate() {
            if index > 0 {
                nodes = nodes.child(div().w(px(28.)).h_px().bg(colors.border_variant));
            }
            let is_gate = matches!(flow_step.kind, StepKind::Gate(_));
            let subtitle = match &flow_step.kind {
                StepKind::Agent(agent_step) => self
                    .agent(&agent_step.agent)
                    .map(|agent| format!("entrega {}", agent.gives.join(" + ")))
                    .unwrap_or_default(),
                StepKind::Gate(_) => "pausa a esteira".to_string(),
                StepKind::Run(run) => run.command.clone(),
                StepKind::Branch(branch) => {
                    format!("se {} contém “{}”", branch.output, branch.contains)
                }
                StepKind::Parallel(parallel) => format!("{} juntos", parallel.agents.len()),
                StepKind::GitHub(github) => github_summary(&github.action),
            };
            let state = self
                .run
                .as_ref()
                .and_then(|run| run.steps.get(index))
                .cloned();
            nodes = nodes.child(
                v_flex()
                    .id(("canvas-node", index))
                    .w(px(150.))
                    .my_2()
                    .p_2p5()
                    .gap_1()
                    .rounded_lg()
                    .border_1()
                    .cursor_pointer()
                    .border_color(if self.selected == Some(index) {
                        colors.text_accent
                    } else if is_gate {
                        cx.theme().status().warning_border
                    } else {
                        colors.border
                    })
                    .bg(colors.panel_background)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_step(Some(index), window, cx)
                    }))
                    .child(description(format!("{}", index + 1)))
                    .child(
                        Label::new(flow_step.label(&self.agents))
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .when(is_gate, |this| this.color(Color::Warning))
                            .truncate(),
                    )
                    .child(description(subtitle))
                    .when_some(state, |this, state| {
                        this.child(super::state_chip(state.state, state.note, cx))
                    }),
            );
        }
        let mut loops = v_flex().gap_1().pt_3();
        for (index, flow_step) in self.flow.steps.iter().enumerate() {
            let back = match &flow_step.kind {
                StepKind::Agent(agent_step) => match &agent_step.on_fail {
                    OnFail::BackTo { step, max } => {
                        self.flow.step_index(step).map(|target| (target, *max))
                    }
                    _ => None,
                },
                StepKind::Run(run) => match &run.on_fail {
                    OnFail::BackTo { step, max } => {
                        self.flow.step_index(step).map(|target| (target, *max))
                    }
                    _ => None,
                },
                StepKind::Parallel(parallel) => match &parallel.on_fail {
                    OnFail::BackTo { step, max } => {
                        self.flow.step_index(step).map(|target| (target, *max))
                    }
                    _ => None,
                },
                StepKind::GitHub(github) => match &github.on_fail {
                    OnFail::BackTo { step, max } => {
                        self.flow.step_index(step).map(|target| (target, *max))
                    }
                    _ => None,
                },
                StepKind::Gate(_) | StepKind::Branch(_) => None,
            };
            if let Some((target, max)) = back {
                loops = loops.child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Undo)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .child(description(format!(
                            "{} {} → {} {} se reprovar (máx. {max})",
                            index + 1,
                            flow_step.label(&self.agents),
                            target + 1,
                            self.step_label(target)
                        ))),
                );
            }
            if let StepKind::Gate(gate) = &flow_step.kind {
                let target = gate
                    .adjust_to
                    .as_deref()
                    .and_then(|id| self.flow.step_index(id))
                    .or_else(|| index.checked_sub(1));
                if let Some(target) = target {
                    loops = loops.child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Undo)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(format!(
                                    "{} ajuste → {} {}",
                                    index + 1,
                                    target + 1,
                                    self.step_label(target)
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                            ),
                    );
                }
            }
        }
        v_flex()
            .child(nodes)
            .child(loops)
            .child(div().pt_3().child(description(
                "A visão canvas desenha a mesma lista; Se / senão e Em paralelo ganham ramos aqui.",
            )))
    }

    // ---------- inspector ----------

    pub(super) fn render_inspector(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(inspector) = &self.inspector else {
            return div()
                .p_8()
                .flex()
                .justify_center()
                .child(Label::new("Selecione um passo para configurar.").color(Color::Muted))
                .into_any_element();
        };
        let index = inspector.step;
        let Some(flow_step) = self.flow.steps.get(index) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let header = h_flex()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .child(Label::new(format!("Passo {}", index + 1)).weight(FontWeight::SEMIBOLD))
            .child(description(format!("de {}", self.flow.steps.len())))
            .child(div().flex_1())
            .child(
                IconButton::new("remove-selected-step", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Remover passo"))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.remove_step(index, window, cx)),
                    ),
            );
        let body = match &flow_step.kind {
            StepKind::Gate(gate) => self
                .render_gate_inspector(index, gate, inspector, window, cx)
                .into_any_element(),
            StepKind::Run(run) => self
                .render_command_inspector(index, run, inspector, window, cx)
                .into_any_element(),
            StepKind::Agent(agent_step) => self
                .render_agent_inspector(index, agent_step, inspector, window, cx)
                .into_any_element(),
            StepKind::Branch(branch) => {
                self.render_branch_inspector(index, branch, inspector, window, cx)
            }
            StepKind::Parallel(parallel) => {
                self.render_parallel_inspector(index, parallel, window, cx)
            }
            StepKind::GitHub(github) => self
                .render_github_inspector(index, github, inspector, window, cx)
                .into_any_element(),
        };
        v_flex()
            .child(header)
            .child(v_flex().p_4().gap_4().child(body))
            .into_any_element()
    }

    fn io_rows(
        &self,
        rows: Vec<(String, String, bool, &'static str)>,
        cx: &App,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .children(rows.into_iter().map(|(name, origin, missing, arrow)| {
                h_flex()
                    .h(px(30.))
                    .px_2p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .when(!arrow.is_empty(), |this| {
                        this.child(Label::new(arrow).size(LabelSize::XSmall).color(
                            if arrow == "→" {
                                Color::Accent
                            } else {
                                Color::Muted
                            },
                        ))
                    })
                    .child(
                        Label::new(name)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Success),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(origin)
                            .size(LabelSize::XSmall)
                            .color(if missing {
                                Color::Warning
                            } else {
                                Color::Muted
                            }),
                    )
            }))
    }

    fn available_rows(&self, index: usize) -> Vec<(String, String)> {
        self.flow
            .available_inputs(index, &self.agents)
            .into_iter()
            .map(|input| {
                let origin = match input.from_step {
                    None => "da esteira".to_string(),
                    Some(step) => format!("do passo {} · {}", step + 1, self.step_label(step)),
                };
                (input.name, origin)
            })
            .collect()
    }

    fn step_dropdown(
        &self,
        id: &'static str,
        current: Option<usize>,
        candidates: Vec<usize>,
        prefix: &'static str,
        empty: Option<&'static str>,
        on_select: impl Fn(&mut Self, Option<usize>, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = match current {
            Some(step) => format!("{prefix}{} · {}", step + 1, self.step_label(step)),
            None => empty.unwrap_or("").to_string(),
        };
        let view = cx.entity().downgrade();
        let labels: Vec<(usize, String)> = candidates
            .into_iter()
            .map(|step| {
                (
                    step,
                    format!("{prefix}{} · {}", step + 1, self.step_label(step)),
                )
            })
            .collect();
        let on_select = std::rc::Rc::new(on_select);
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            if let Some(empty) = empty {
                let view = view.clone();
                let on_select = on_select.clone();
                menu = menu.toggleable_entry(
                    empty,
                    current.is_none(),
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        view.update(cx, |view, cx| on_select(view, None, cx))
                            .log_err();
                    },
                );
            }
            for (step, label) in &labels {
                let view = view.clone();
                let on_select = on_select.clone();
                let step = *step;
                menu = menu.toggleable_entry(
                    label.clone(),
                    current == Some(step),
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        view.update(cx, |view, cx| on_select(view, Some(step), cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        selector(id, IconName::Undo, label, menu)
    }

    fn render_branch_inspector(
        &self,
        index: usize,
        branch: &BranchStep,
        inspector: &StepInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let outputs: Vec<(String, String)> =
            self.available_rows(index).into_iter().skip(1).collect();
        let view = cx.entity().downgrade();
        let current_output = branch.output.clone();
        let output_menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            if outputs.is_empty() {
                menu = menu.header("Nenhum passo antes entrega algo");
            }
            for (name, origin) in &outputs {
                let view = view.clone();
                let name = name.clone();
                menu = menu.toggleable_entry(
                    format!("{name} · {origin}"),
                    name == current_output,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let name = name.clone();
                        view.update(cx, |view, cx| {
                            view.edit_flow(cx, |flow| {
                                if let Some(FlowStep {
                                    kind: StepKind::Branch(branch),
                                    ..
                                }) = flow.steps.get_mut(index)
                                {
                                    branch.output = name;
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        let later: Vec<usize> = (index + 1..self.flow.steps.len()).collect();
        let current = |id: &Option<String>| id.as_deref().and_then(|id| self.flow.step_index(id));
        let set_target = move |then: bool| {
            move |this: &mut Self, step: Option<usize>, cx: &mut Context<Self>| {
                let id = step
                    .and_then(|step| this.flow.steps.get(step))
                    .map(|step| step.id.clone());
                this.edit_flow(cx, |flow| {
                    if let Some(FlowStep {
                        kind: StepKind::Branch(branch),
                        ..
                    }) = flow.steps.get_mut(index)
                    {
                        if then {
                            branch.then_to = id;
                        } else {
                            branch.else_to = id;
                        }
                    }
                });
            }
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("SE A SAÍDA"))
                    .child(selector(
                        "branch-output",
                        IconName::FileDoc,
                        if branch.output.is_empty() {
                            "escolha a saída".to_string()
                        } else {
                            branch.output.clone()
                        },
                        output_menu,
                    ))
                    .child(section_label("CONTÉM"))
                    .child(field(&inspector.note, cx))
                    .child(description(
                        "Compara sem diferenciar maiúsculas. Olha a entrega mais recente da saída.",
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("ENTÃO VAI PARA"))
                    .child(self.step_dropdown(
                        "branch-then",
                        current(&branch.then_to),
                        later.clone(),
                        "",
                        Some("Próximo passo"),
                        set_target(true),
                        window,
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("SENÃO VAI PARA"))
                    .child(self.step_dropdown(
                        "branch-else",
                        current(&branch.else_to),
                        later,
                        "",
                        Some("Próximo passo"),
                        set_target(false),
                        window,
                        cx,
                    ))
                    .child(description(
                        "Os passos pulados ficam como pulados nesta execução. Para repetir passos, use “Se não passar”.",
                    )),
            )
            .into_any_element()
    }

    fn render_parallel_inspector(
        &self,
        index: usize,
        parallel: &ParallelStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let edit_parallel =
            move |this: &mut Self, cx: &mut Context<Self>, change: &dyn Fn(&mut ParallelStep)| {
                this.edit_flow(cx, |flow| {
                    if let Some(FlowStep {
                        kind: StepKind::Parallel(parallel),
                        ..
                    }) = flow.steps.get_mut(index)
                    {
                        change(parallel);
                    }
                });
            };
        let candidates: Vec<(String, String)> = self
            .agents
            .iter()
            .filter(|agent| !parallel.agents.contains(&agent.command))
            .map(|agent| (agent.command.clone(), agent.name.clone()))
            .collect();
        let add_menu = ContextMenu::build(window, cx, {
            let view = view.clone();
            move |mut menu, _window, _cx| {
                if candidates.is_empty() {
                    menu = menu.header("Todos os agentes já estão no passo");
                }
                for (command, name) in &candidates {
                    let view = view.clone();
                    let command = command.clone();
                    menu = menu.entry(format!("{name} · /{command}"), None, move |_window, cx| {
                        let command = command.clone();
                        view.update(cx, |view, cx| {
                            edit_parallel(view, cx, &|parallel| {
                                parallel.agents.push(command.clone())
                            })
                        })
                        .log_err();
                    });
                }
                menu
            }
        });
        let mut agents = v_flex().gap_1();
        for (position, command) in parallel.agents.iter().enumerate() {
            let name = self
                .agent(command)
                .map(|agent| agent.name.clone())
                .unwrap_or_else(|| format!("/{command} (não existe)"));
            agents = agents.child(
                h_flex()
                    .gap_2()
                    .px_2p5()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Icon::new(IconName::Person)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(Label::new(name).size(LabelSize::Small))
                    .child(
                        Label::new(format!("/{command}"))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Accent),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new(("remove-parallel-agent", position), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Tirar do passo"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                edit_parallel(this, cx, &|parallel| {
                                    if position < parallel.agents.len() {
                                        parallel.agents.remove(position);
                                    }
                                })
                            })),
                    ),
            );
        }
        let on_fail_label = match &parallel.on_fail {
            OnFail::Retry { .. } => "Refaz o passo com o motivo",
            OnFail::BackTo { .. } => "Volta para um passo",
            OnFail::AskYou => "Chama você",
            OnFail::Continue => "Segue mesmo assim",
        };
        let on_fail_menu = ContextMenu::build(window, cx, {
            move |mut menu, _window, _cx| {
                for (label, on_fail) in [
                    (
                        "Refaz o passo com o motivo",
                        OnFail::Retry {
                            max: OnFail::DEFAULT_MAX,
                        },
                    ),
                    ("Chama você", OnFail::AskYou),
                    ("Segue mesmo assim", OnFail::Continue),
                ] {
                    let view = view.clone();
                    menu = menu.entry(label, None, move |_window, cx| {
                        let on_fail = on_fail.clone();
                        view.update(cx, |view, cx| {
                            edit_parallel(view, cx, &|parallel| parallel.on_fail = on_fail.clone())
                        })
                        .log_err();
                    });
                }
                menu
            }
        });
        let read_only = parallel.read_only;
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .child(section_label("AGENTES AO MESMO TEMPO"))
                            .child(div().flex_1())
                            .child(selector("parallel-add-agent", IconName::Plus, "agente", add_menu)),
                    )
                    .child(agents)
                    .child(description(
                        "Cada um roda na sua thread. O passo passa quando todos entregam; as entregas seguem juntas.",
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new(
                            "parallel-read-only",
                            if read_only {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label("Só leitura")
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            edit_parallel(this, cx, &|parallel| parallel.read_only = !read_only)
                        })),
                    )
                    .child(description(
                        "Desmarcado, os agentes editam o mesmo projeto e podem disputar arquivos.",
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("SE NÃO PASSAR"))
                    .child(selector("parallel-on-fail", IconName::Undo, on_fail_label, on_fail_menu)),
            )
            .into_any_element()
    }

    fn render_gate_inspector(
        &self,
        index: usize,
        gate: &GateStep,
        inspector: &StepInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shows: Vec<_> = self
            .available_rows(index)
            .into_iter()
            .skip(1)
            .map(|(name, origin)| (name, origin, false, ""))
            .collect();
        let shows = if shows.is_empty() {
            vec![(
                String::new(),
                "nada antes deste passo".to_string(),
                false,
                "",
            )]
        } else {
            shows
        };
        let current = gate
            .adjust_to
            .as_deref()
            .and_then(|id| self.flow.step_index(id))
            .or_else(|| {
                (0..index)
                    .rev()
                    .find(|step| matches!(self.flow.steps[*step].kind, StepKind::Agent(_)))
            });
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("O QUE VOCÊ APROVA"))
                    .child(field(&inspector.note, cx)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("MOSTRA PARA VOCÊ"))
                    .child(self.io_rows(shows, cx)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("PEDIR AJUSTE VOLTA PARA"))
                    .child(self.step_dropdown(
                        "gate-adjust-to",
                        current,
                        (0..index).collect(),
                        "",
                        None,
                        move |this, step, cx| {
                            let id = step
                                .and_then(|step| this.flow.steps.get(step))
                                .map(|step| step.id.clone());
                            this.edit_flow(cx, |flow| {
                                if let Some(FlowStep {
                                    kind: StepKind::Gate(gate),
                                    ..
                                }) = flow.steps.get_mut(index)
                                {
                                    gate.adjust_to = id;
                                }
                            });
                        },
                        window,
                        cx,
                    ))
                    .child(description(
                        "Seu texto vai junto como motivo. A esteira pausa até você responder.",
                    )),
            )
    }

    fn render_command_inspector(
        &self,
        index: usize,
        run: &RunStep,
        inspector: &StepInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = match &run.on_fail {
            OnFail::BackTo { step, .. } => self.flow.step_index(step),
            _ => None,
        };
        let candidates = (0..index)
            .filter(|step| matches!(self.flow.steps[*step].kind, StepKind::Agent(_)))
            .collect();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("COMANDO"))
                    .child(field(&inspector.command, cx))
                    .child(description(
                        "Roda no terminal do projeto. Passa se sair 0; a saída fica como prova.",
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("SE NÃO PASSAR"))
                    .child(self.step_dropdown(
                        "command-on-fail",
                        current,
                        candidates,
                        "Voltar para ",
                        Some("Chamar você"),
                        move |this, step, cx| {
                            let target = step
                                .and_then(|step| this.flow.steps.get(step))
                                .map(|step| step.id.clone());
                            this.edit_flow(cx, |flow| {
                                if let Some(FlowStep {
                                    kind: StepKind::Run(run),
                                    ..
                                }) = flow.steps.get_mut(index)
                                {
                                    run.on_fail = match target {
                                        Some(step) => OnFail::BackTo {
                                            step,
                                            max: OnFail::DEFAULT_MAX,
                                        },
                                        None => OnFail::AskYou,
                                    };
                                }
                            });
                        },
                        window,
                        cx,
                    )),
            )
    }

    /// Board I: what a GitHub step does, its own setting, what it delivers and where a
    /// failure goes.
    fn render_github_inspector(
        &self,
        index: usize,
        github: &GitHubStep,
        inspector: &StepInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let current_key = github.action.key();
        let action_menu = ContextMenu::build(window, cx, {
            move |mut menu, _window, _cx| {
                for action in GitHubAction::ALL {
                    let view = view.clone();
                    let selected = action.key() == current_key;
                    menu = menu.toggleable_entry(
                        action.label(),
                        selected,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            let action = match action.clone() {
                                GitHubAction::MoveItem { status } if status.is_empty() => {
                                    GitHubAction::MoveItem {
                                        status: "Em revisão".to_string(),
                                    }
                                }
                                action => action,
                            };
                            view.update(cx, |view, cx| {
                                view.edit_flow(cx, |flow| {
                                    if let Some(FlowStep {
                                        kind: StepKind::GitHub(github),
                                        ..
                                    }) = flow.steps.get_mut(index)
                                    {
                                        github.action = action;
                                    }
                                });
                                view.rebuild_inspector(window, cx);
                            })
                            .log_err();
                        },
                    );
                }
                menu
            }
        });
        let setting: Option<AnyElement> = match &github.action {
            GitHubAction::WaitChecks { .. } => Some(
                v_flex()
                    .gap_2()
                    .child(section_label("TEMPO MÁXIMO"))
                    .child(field(&inspector.command, cx))
                    .child(description(
                        "Minutos. Os checks que falharem rodam mais uma vez antes de o passo reprovar.",
                    ))
                    .into_any_element(),
            ),
            GitHubAction::MoveItem { .. } => Some(
                v_flex()
                    .gap_2()
                    .child(section_label("COLUNA"))
                    .child(field(&inspector.note, cx))
                    .child(description("O nome da coluna no projeto, como Em revisão."))
                    .into_any_element(),
            ),
            GitHubAction::CommentItem { .. } => Some(
                v_flex()
                    .gap_2()
                    .child(section_label("COMENTÁRIO"))
                    .child(field(&inspector.note, cx))
                    .child(description("Vazio comenta o que a esteira entregou até aqui."))
                    .into_any_element(),
            ),
            GitHubAction::OpenPullRequest { draft } => Some(
                Checkbox::new("github-step-draft", (*draft).into())
                    .label("Abrir como rascunho")
                    .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
                        let draft = state.selected();
                        this.edit_flow(cx, |flow| {
                            if let Some(FlowStep {
                                kind: StepKind::GitHub(github),
                                ..
                            }) = flow.steps.get_mut(index)
                            {
                                github.action = GitHubAction::OpenPullRequest { draft };
                            }
                        });
                    }))
                    .into_any_element(),
            ),
            GitHubAction::StartItem => None,
        };
        let mut io: Vec<(String, String, bool, &'static str)> = Vec::new();
        if matches!(github.action, GitHubAction::WaitChecks { .. }) {
            let source = self.flow.steps[..index]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, step)| {
                    matches!(
                        &step.kind,
                        StepKind::GitHub(GitHubStep {
                            action: GitHubAction::OpenPullRequest { .. },
                            ..
                        })
                    )
                });
            io.push(match source {
                Some((source, _)) => (
                    "PR".to_string(),
                    format!("do passo {} · Abrir PR", source + 1),
                    false,
                    "←",
                ),
                None => (
                    "PR".to_string(),
                    "o do branch atual".to_string(),
                    false,
                    "←",
                ),
            });
        }
        for output in github.action.outputs() {
            io.push((
                output.to_string(),
                if *output == "log do que falhou" {
                    "volta junto para quem corrige".to_string()
                } else {
                    "para os próximos passos".to_string()
                },
                false,
                "→",
            ));
        }
        let current = match &github.on_fail {
            OnFail::BackTo { step, .. } => self.flow.step_index(step),
            _ => None,
        };
        let candidates = (0..index)
            .filter(|step| matches!(self.flow.steps[*step].kind, StepKind::Agent(_)))
            .collect();
        let colors = cx.theme().colors();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("PASSO DO GITHUB"))
                    .child(selector(
                        "github-step-action",
                        github_icon(&github.action),
                        github.action.label(),
                        action_menu,
                    ))
                    .child(description(
                        "Não chama modelo: usa a conta do GitHub conectada no dock Repo.",
                    )),
            )
            .children(setting)
            .when(!io.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_2()
                        .child(section_label("RECEBE E ENTREGA"))
                        .child(self.io_rows(io, cx)),
                )
            })
            .when(
                matches!(github.action, GitHubAction::WaitChecks { .. }),
                |this| {
                    this.child(
                        v_flex()
                            .gap_2()
                            .child(section_label("CRITÉRIOS DE ACEITE"))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .p_2p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .child(
                                        Icon::new(IconName::Check)
                                            .size(IconSize::XSmall)
                                            .color(Color::Success),
                                    )
                                    .child(
                                        Label::new("Todos os checks obrigatórios passam")
                                            .size(LabelSize::Small),
                                    )
                                    .child(div().flex_1())
                                    .child(description("verificado pelo GitHub")),
                            ),
                    )
                },
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label("SE NÃO PASSAR"))
                    .child(self.step_dropdown(
                        "github-on-fail",
                        current,
                        candidates,
                        "Voltar para ",
                        Some("Chamar você"),
                        move |this, step, cx| {
                            let target = step
                                .and_then(|step| this.flow.steps.get(step))
                                .map(|step| step.id.clone());
                            this.edit_flow(cx, |flow| {
                                if let Some(FlowStep {
                                    kind: StepKind::GitHub(github),
                                    ..
                                }) = flow.steps.get_mut(index)
                                {
                                    github.on_fail = match target {
                                        Some(step) => OnFail::BackTo { step, max: 1 },
                                        None => OnFail::AskYou,
                                    };
                                }
                            });
                        },
                        window,
                        cx,
                    )),
            )
            .when(github.action.writes(), |this| {
                let status = cx.theme().status();
                this.child(
                    v_flex()
                        .gap_1p5()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(status.warning_border)
                        .bg(status.warning_background)
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Icon::new(IconName::Lock)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                )
                                .child(
                                    Label::new("ESCRITAS NO GITHUB")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                ),
                        )
                        .child(
                            Label::new(
                                "Rodar mostra a lista do que a esteira escreve (branch, PR, mover, comentar) e você aprova de uma vez. Merge nunca é um passo: fica com você.",
                            )
                            .size(LabelSize::Small),
                        ),
                )
            })
    }

    fn render_agent_inspector(
        &self,
        index: usize,
        agent_step: &AgentStep,
        inspector: &StepInspector,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent = self.agent(&agent_step.agent);
        let view = cx.entity().downgrade();

        let agent_label = agent
            .map(|agent| {
                format!(
                    "{} · /{} · {}",
                    agent.name,
                    agent.command,
                    if matches!(agent.source, TaskAgentSource::Personal) {
                        "só para mim"
                    } else {
                        "projeto"
                    }
                )
            })
            .unwrap_or_else(|| format!("/{} (não existe)", agent_step.agent));
        let agents = self.agents.clone();
        let current_agent = agent_step.agent.clone();
        let agent_menu = ContextMenu::build(window, cx, {
            let view = view.clone();
            move |mut menu, _window, _cx| {
                for agent in &agents {
                    let view = view.clone();
                    let command = agent.command.clone();
                    menu = menu.toggleable_entry(
                        format!("{} · /{}", agent.name, agent.command),
                        agent.command == current_agent,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            let command = command.clone();
                            view.update(cx, |view, cx| {
                                view.edit_agent_step(index, cx, |agent_step| {
                                    agent_step.agent = command
                                });
                                view.rebuild_inspector(window, cx);
                            })
                            .log_err();
                        },
                    );
                }
                menu
            }
        });
        let agent_summary = agent
            .map(|agent| {
                let toolkits = if agent.toolkits.is_empty() {
                    "—".to_string()
                } else {
                    agent.toolkits.join(", ")
                };
                format!(
                    "{} · {} · {toolkits}. Vem do agente; mudar aqui vale só nesta esteira.",
                    agent
                        .model
                        .clone()
                        .unwrap_or_else(|| "modelo da thread".to_string()),
                    agent
                        .profile
                        .clone()
                        .unwrap_or_else(|| "perfil da thread".to_string()),
                )
            })
            .unwrap_or_else(|| {
                "Esse agente não existe mais. Escolha outro ou crie-o na aba Agentes.".to_string()
            });

        let override_chip = |id: &'static str,
                             label: &'static str,
                             value: String,
                             kind: Override,
                             cx: &mut Context<Self>| {
            let open = inspector.open_override == Some(kind);
            h_flex()
                .id(id)
                .h(px(24.))
                .px_2()
                .gap_1()
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .border_color(if open {
                    cx.theme().colors().text_accent
                } else {
                    gpui::transparent_black()
                })
                .bg(cx.theme().colors().element_hover)
                .child(description(label))
                .child(Label::new(value).size(LabelSize::XSmall).color(if open {
                    Color::Accent
                } else {
                    Color::Default
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if let Some(inspector) = &mut this.inspector {
                        inspector.open_override = if inspector.open_override == Some(kind) {
                            None
                        } else {
                            Some(kind)
                        };
                    }
                    cx.notify();
                }))
        };
        let instruction_lines = agent_step
            .instructions
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        let mut agent_group = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .child(section_label("AGENTE"))
                    .child(div().flex_1())
                    .child(
                        Button::new("edit-task-agent", "editar agente")
                            .label_size(LabelSize::XSmall)
                            .style(ButtonStyle::Transparent)
                            .color(Color::Accent)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenTaskAgents), cx)
                            }),
                    ),
            )
            .child(selector(
                "step-agent",
                IconName::Person,
                agent_label,
                agent_menu,
            ))
            .child(description(agent_summary))
            .child(
                h_flex()
                    .gap_1p5()
                    .flex_wrap()
                    .child(override_chip(
                        "override-model",
                        "modelo",
                        agent_step
                            .model
                            .clone()
                            .unwrap_or_else(|| "do agente".to_string()),
                        Override::Model,
                        cx,
                    ))
                    .child(override_chip(
                        "override-instructions",
                        "instruções",
                        if instruction_lines == 0 {
                            "do agente".to_string()
                        } else {
                            format!("+ {instruction_lines} linha(s)")
                        },
                        Override::Instructions,
                        cx,
                    )),
            );
        match inspector.open_override {
            Some(Override::Model) => {
                agent_group = agent_group.child(model_dropdown(
                    "step-model",
                    agent_step.model.clone(),
                    "Do agente",
                    move |model, cx| {
                        view.update(cx, |view, cx| {
                            view.edit_agent_step(index, cx, |agent_step| agent_step.model = model);
                        })
                        .log_err();
                    },
                    window,
                    cx,
                ));
            }
            Some(Override::Instructions) => {
                agent_group = agent_group.child(field(&inspector.instructions, cx));
            }
            None => {}
        }

        let current_tool = agent_step
            .where_
            .as_ref()
            .map(|target| target.tool)
            .unwrap_or(DesignTool::Code);
        let where_group = v_flex()
            .gap_2()
            .child(section_label("ONDE ACONTECE"))
            .child(
                h_flex()
                    .p_0p5()
                    .gap_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .children(DesignTool::ALL.into_iter().map(|tool| {
                        segment_option(
                            SharedString::from(format!("where-{}", tool.label())),
                            tool.label(),
                            tool == current_tool,
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.edit_agent_step(index, cx, |agent_step| {
                                    agent_step.where_ =
                                        (tool != DesignTool::Code).then(|| DesignTarget {
                                            tool,
                                            file: agent_step
                                                .where_
                                                .as_ref()
                                                .and_then(|target| target.file.clone()),
                                        });
                                });
                                this.rebuild_inspector(window, cx);
                            },
                        ))
                    })),
            )
            .child(if current_tool == DesignTool::Code {
                description("Trabalha direto no repositório.").into_any_element()
            } else {
                field(&inspector.file, cx).into_any_element()
            });

        let available = self.available_rows(index);
        let mut io = Vec::new();
        if let Some(agent) = agent {
            for takes in &agent.takes {
                match available.iter().find(|(name, _)| name == takes) {
                    Some((_, origin)) => io.push((takes.clone(), origin.clone(), false, "←")),
                    None => io.push((
                        takes.clone(),
                        "ninguém entrega antes".to_string(),
                        true,
                        "←",
                    )),
                }
            }
            for gives in &agent.gives {
                io.push((
                    gives.clone(),
                    "para os próximos passos".to_string(),
                    false,
                    "→",
                ));
            }
        }
        let io_group = v_flex()
            .gap_2()
            .child(section_label("RECEBE E ENTREGA"))
            .child(if io.is_empty() {
                description(
                    "O agente não declara o que recebe nem o que entrega (takes/gives no .md).",
                )
                .into_any_element()
            } else {
                self.io_rows(io, cx).into_any_element()
            });

        let mut criteria_group = v_flex().gap_2().child(
            h_flex()
                .child(section_label("CRITÉRIOS DE ACEITE"))
                .child(div().flex_1())
                .child(
                    Button::new("add-criterion", "+ critério")
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Transparent)
                        .color(Color::Accent)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.edit_agent_step(index, cx, |agent_step| {
                                agent_step.criteria.push(Criterion {
                                    text: String::new(),
                                    required: true,
                                    verify: Verifier::SelfCheck,
                                })
                            });
                            this.rebuild_inspector(window, cx);
                        })),
                ),
        );
        for (criterion_index, criterion) in agent_step.criteria.iter().enumerate() {
            let Some(editors) = inspector.criteria.get(criterion_index) else {
                continue;
            };
            criteria_group = criteria_group.child(self.render_criterion(
                index,
                criterion_index,
                criterion,
                editors,
                &agent_step.agent,
                window,
                cx,
            ));
        }
        if agent_step.criteria.is_empty() {
            criteria_group = criteria_group.child(description(
                "Sem critérios: o passo avança quando o agente entregar.",
            ));
        }
        criteria_group = criteria_group.child(description(
            "Marcado = obrigatório: se falhar, vale o “se não passar”. Opcional vira pergunta para você.",
        ));

        let on_fail_group = self.render_on_fail(index, agent_step, window, cx);

        let limits = agent_step.limits;
        let limits_open = inspector.limits_open;
        let limit_field = |caption: &'static str, editor: &Entity<Editor>, cx: &App| {
            v_flex()
                .flex_1()
                .gap_1()
                .child(description(caption))
                .child(field(editor, cx))
        };
        let limits_group = v_flex()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .id("limits-toggle")
                    .px_2p5()
                    .py_2()
                    .gap_2()
                    .cursor_pointer()
                    .child(
                        Label::new("Limites")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!(
                            "{} turnos · {} min · US$ {}",
                            limits.turns, limits.minutes, limits.usd
                        ))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(inspector) = &mut this.inspector {
                            inspector.limits_open = !inspector.limits_open;
                        }
                        cx.notify();
                    })),
            )
            .when(limits_open, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .px_2p5()
                        .pb_2p5()
                        .child(limit_field("turnos", &inspector.turns, cx))
                        .child(limit_field("minutos", &inspector.minutes, cx))
                        .child(limit_field("US$", &inspector.usd, cx)),
                )
            });

        v_flex()
            .gap_4()
            .child(agent_group)
            .child(where_group)
            .child(io_group)
            .child(criteria_group)
            .child(on_fail_group)
            .child(limits_group)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_criterion(
        &self,
        step: usize,
        criterion_index: usize,
        criterion: &Criterion,
        editors: &CriterionEditors,
        own_agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let kind = verifier_kind(&criterion.verify);
        let kind_label = VERIFIERS
            .iter()
            .find(|(_, candidate)| *candidate == kind)
            .map(|(label, _)| *label)
            .unwrap_or("você");
        let verifier_menu = ContextMenu::build(window, cx, {
            let view = view.clone();
            let first_other = self
                .agents
                .iter()
                .find(|agent| agent.command != own_agent)
                .map(|agent| agent.command.clone())
                .unwrap_or_default();
            move |mut menu, _window, _cx| {
                for (label, candidate) in VERIFIERS {
                    let view = view.clone();
                    let first_other = first_other.clone();
                    menu = menu.toggleable_entry(
                        label,
                        candidate == kind,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            let first_other = first_other.clone();
                            view.update(cx, |view, cx| {
                                view.edit_agent_step(step, cx, |agent_step| {
                                    if let Some(criterion) =
                                        agent_step.criteria.get_mut(criterion_index)
                                    {
                                        criterion.verify = match candidate {
                                            VerifierKind::You => Verifier::You,
                                            VerifierKind::SelfCheck => Verifier::SelfCheck,
                                            VerifierKind::Agent => Verifier::Agent(first_other),
                                            VerifierKind::Command => {
                                                Verifier::Command("npm test".to_string())
                                            }
                                        };
                                    }
                                });
                                view.rebuild_inspector(window, cx);
                            })
                            .log_err();
                        },
                    );
                }
                menu
            }
        });
        let verifier_detail = match &criterion.verify {
            Verifier::Agent(command) => {
                let agents: Vec<String> = self
                    .agents
                    .iter()
                    .filter(|agent| agent.command != own_agent)
                    .map(|agent| agent.command.clone())
                    .collect();
                let current = command.clone();
                let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for command in &agents {
                        let view = view.clone();
                        let command = command.clone();
                        menu = menu.toggleable_entry(
                            format!("/{command}"),
                            command == current,
                            IconPosition::Start,
                            None,
                            move |_window, cx| {
                                let command = command.clone();
                                view.update(cx, |view, cx| {
                                    view.edit_agent_step(step, cx, |agent_step| {
                                        if let Some(criterion) =
                                            agent_step.criteria.get_mut(criterion_index)
                                        {
                                            criterion.verify = Verifier::Agent(command);
                                        }
                                    });
                                })
                                .log_err();
                            },
                        );
                    }
                    menu
                });
                Some(
                    selector(
                        "criterion-agent",
                        IconName::Person,
                        format!("/{command}"),
                        menu,
                    )
                    .into_any_element(),
                )
            }
            Verifier::Command(_) => Some(
                div()
                    .w(px(140.))
                    .child(field(&editors.command, cx))
                    .into_any_element(),
            ),
            _ => None,
        };
        let required = criterion.required;
        h_flex()
            .items_start()
            .gap_2()
            .px_2p5()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(
                Checkbox::new(
                    SharedString::from(format!("criterion-required-{criterion_index}")),
                    if required {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .tooltip(Tooltip::text("Obrigatório"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.edit_agent_step(step, cx, |agent_step| {
                        if let Some(criterion) = agent_step.criteria.get_mut(criterion_index) {
                            criterion.required = !criterion.required;
                        }
                    });
                })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(editors.text.clone())
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .child(description("verificado por"))
                            .child(selector(
                                "criterion-verifier",
                                IconName::Check,
                                kind_label,
                                verifier_menu,
                            ))
                            .children(verifier_detail)
                            .child(div().flex_1())
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "remove-criterion-{criterion_index}"
                                    )),
                                    IconName::Close,
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Remover critério"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.edit_agent_step(step, cx, |agent_step| {
                                            if criterion_index < agent_step.criteria.len() {
                                                agent_step.criteria.remove(criterion_index);
                                            }
                                        });
                                        this.rebuild_inspector(window, cx);
                                    },
                                )),
                            ),
                    ),
            )
    }

    fn render_on_fail(
        &self,
        index: usize,
        agent_step: &AgentStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let label = match &agent_step.on_fail {
            OnFail::Retry { .. } => "Refaz este passo com o motivo".to_string(),
            OnFail::BackTo { step, .. } => format!(
                "Volta para {}",
                self.flow
                    .step_index(step)
                    .map(|step| self.step_label(step))
                    .unwrap_or_else(|| step.clone())
            ),
            OnFail::AskYou => "Chama você".to_string(),
            OnFail::Continue => "Segue mesmo assim".to_string(),
        };
        let max = match &agent_step.on_fail {
            OnFail::Retry { max } | OnFail::BackTo { max, .. } => Some(*max),
            _ => None,
        };
        let back_options: Vec<(String, String)> = (0..index)
            .filter(|step| matches!(self.flow.steps[*step].kind, StepKind::Agent(_)))
            .map(|step| {
                (
                    self.flow.steps[step].id.clone(),
                    format!("Volta para {}", self.step_label(step)),
                )
            })
            .collect();
        let current = agent_step.on_fail.clone();
        let menu = ContextMenu::build(window, cx, {
            let view = view.clone();
            move |mut menu, _window, _cx| {
                let mut options: Vec<(String, OnFail)> = vec![(
                    "Refaz este passo com o motivo".to_string(),
                    OnFail::Retry {
                        max: max.unwrap_or(OnFail::DEFAULT_MAX),
                    },
                )];
                for (id, label) in &back_options {
                    options.push((
                        label.clone(),
                        OnFail::BackTo {
                            step: id.clone(),
                            max: max.unwrap_or(OnFail::DEFAULT_MAX),
                        },
                    ));
                }
                options.push(("Chama você".to_string(), OnFail::AskYou));
                options.push(("Segue mesmo assim".to_string(), OnFail::Continue));
                for (label, on_fail) in options {
                    let toggled = std::mem::discriminant(&on_fail)
                        == std::mem::discriminant(&current)
                        && match (&on_fail, &current) {
                            (OnFail::BackTo { step: a, .. }, OnFail::BackTo { step: b, .. }) => {
                                a == b
                            }
                            _ => true,
                        };
                    let view = view.clone();
                    menu = menu.toggleable_entry(
                        label,
                        toggled,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            let on_fail = on_fail.clone();
                            view.update(cx, |view, cx| {
                                view.edit_agent_step(index, cx, |agent_step| {
                                    agent_step.on_fail = on_fail
                                });
                            })
                            .log_err();
                        },
                    );
                }
                menu
            }
        });
        let max_menu = max.map(|max| {
            let view = view.clone();
            let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for count in 1..=5u8 {
                    let view = view.clone();
                    menu = menu.toggleable_entry(
                        count.to_string(),
                        count == max,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            view.update(cx, |view, cx| {
                                view.edit_agent_step(index, cx, |agent_step| match &mut agent_step
                                    .on_fail
                                {
                                    OnFail::Retry { max } | OnFail::BackTo { max, .. } => {
                                        *max = count
                                    }
                                    _ => {}
                                });
                            })
                            .log_err();
                        },
                    );
                }
                menu
            });
            h_flex()
                .gap_2()
                .child(description("até"))
                .child(selector(
                    "on-fail-max",
                    IconName::Rerun,
                    max.to_string(),
                    menu,
                ))
                .child(description("vezes, depois chama você"))
        });
        v_flex()
            .gap_2()
            .child(section_label("SE NÃO PASSAR"))
            .child(selector("on-fail", IconName::Undo, label, menu))
            .children(max_menu)
            .into_any_element()
    }
}
