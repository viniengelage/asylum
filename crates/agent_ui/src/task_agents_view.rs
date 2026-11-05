//! The Agentes tab: the task agents of the project and of the user, each an `.md` file, with a
//! form for its command, profile, model, toolkits and per-tool permissions, and its instructions.

mod new_agent_modal;

use crate::{AgentInitialContent, AgentPanel, AgentThreadSource};
use agent::AgentTool as _;
use agent_client_protocol::schema::v2 as acp;
use agent_settings::AgentProfile;
use anyhow::Result;
use editor::Editor;
use fs::Fs;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels, Stateful,
    Subscription, Task, WeakEntity, Window, actions,
};
use i18n::t;
use language_model::LanguageModelRegistry;
use new_agent_modal::NewAgentModal;
use project::Project;
use settings::{Settings as _, SettingsStore};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use task_agents::{
    ALL_TOOLS_PERMISSION_KEY, AuthoringCatalog, PROJECT_AGENTS_DIR, PermissionMode, ProfileSummary,
    TaskAgent, TaskAgentSource, ToolAccess, ToolPermissionRules, Toolkit, ToolkitSummary,
    command_slug, load_task_agents, personal_agents_dir, toolkits,
};
use ui::{
    ContextMenu, DropdownMenu, KeyBinding, Tooltip, choice_chip, pill_button, prelude::*,
    primary_button, secondary_button, segment_option, segmented_control, text_field, toggle_switch,
};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, ToolkitEntry, Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

actions!(
    task_agents,
    [
        /// Opens the Agentes tab, where task agents are created and configured.
        OpenTaskAgents,
    ]
);

/// Starts a task agent in a new Agent thread, as typing `/command` there would. Bind it in the
/// keymap, e.g. `"alt-cmd-r": ["task_agents::RunTaskAgent", { "command": "revisar" }]`.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = task_agents)]
#[serde(deny_unknown_fields)]
pub struct RunTaskAgent {
    /// The agent's command, without the slash.
    pub command: String,
    /// What to ask it. Without one the composer waits for you to type the request.
    #[serde(default)]
    pub prompt: Option<String>,
}

pub fn run_task_agent(
    workspace: &mut Workspace,
    action: &RunTaskAgent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let command = command_slug(&action.command);
    let text = match &action.prompt {
        Some(prompt) => format!("/{command} {prompt}"),
        None => format!("/{command} "),
    };
    workspace.focus_panel::<AgentPanel>(window, cx);
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    panel.update(cx, |panel, cx| {
        panel.external_thread(
            Some(crate::Agent::NativeAgent),
            None,
            None,
            None,
            Some(AgentInitialContent::ContentBlock {
                blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                auto_submit: action.prompt.is_some(),
            }),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    });
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &OpenTaskAgents, window, cx| {
            open(workspace, window, cx);
        });
        workspace.register_action(run_task_agent);
        if let Some(window) = window {
            run_debug_hooks(workspace.project().clone(), window, cx);
        }
    })
    .detach();
}

/// `TASK_AGENTS_DEBUG_OPEN=1` opens the tab at startup, and `TASK_AGENTS_DEBUG_TOOL="<tool>
/// <json>"` runs one toolkit tool after `TASK_AGENTS_DEBUG_DELAY` seconds (20 by default),
/// logging its text and saving any image next to the log, so toolkits can be checked against
/// real devices without a model in the loop.
fn run_debug_hooks(project: Entity<Project>, window: &mut Window, cx: &mut Context<Workspace>) {
    if std::env::var("TASK_AGENTS_DEBUG_OPEN").is_ok() {
        cx.spawn_in(window, async move |workspace, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            workspace.update_in(cx, |workspace, window, cx| open(workspace, window, cx))
        })
        .detach_and_log_err(cx);
    }
    let Ok(spec) = std::env::var("TASK_AGENTS_DEBUG_TOOL") else {
        return;
    };
    let delay = std::env::var("TASK_AGENTS_DEBUG_DELAY")
        .ok()
        .and_then(|delay| delay.parse().ok())
        .unwrap_or(20);
    let gap = std::env::var("TASK_AGENTS_DEBUG_GAP")
        .ok()
        .and_then(|gap| gap.parse().ok())
        .unwrap_or(3);
    cx.spawn(async move |_, cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(delay))
            .await;
        for step in spec.split(" ;; ") {
            let (name, input) = step.split_once(' ').unwrap_or((step, "{}"));
            let input: serde_json::Value = serde_json::from_str(input)?;
            let tool = cx.update(|cx| {
                toolkits(cx)
                    .into_iter()
                    .flat_map(|toolkit| toolkit.tools.clone())
                    .find(|tool| tool.name == name)
            });
            let Some(tool) = tool else {
                anyhow::bail!("task agents debug: no toolkit tool named {name}");
            };
            let output = cx
                .update(|cx| {
                    tool.run(
                        task_agents::ToolkitCall {
                            project: project.clone(),
                            input,
                        },
                        cx,
                    )
                })
                .await;
            match output {
                Ok(output) => {
                    for (index, content) in output.content.iter().enumerate() {
                        match content {
                            task_agents::ToolkitContent::Text(text) => {
                                log::info!("task agents debug {name}: {text}")
                            }
                            task_agents::ToolkitContent::Image { base64_png } => {
                                use base64::Engine as _;
                                let path = std::env::temp_dir()
                                    .join(format!("task_agents_debug_{name}_{index}.png"));
                                let png =
                                    base64::engine::general_purpose::STANDARD.decode(base64_png)?;
                                std::fs::write(&path, png)?;
                                log::info!("task agents debug {name}: image at {}", path.display());
                            }
                        }
                    }
                }
                Err(error) => log::error!("task agents debug {name} failed: {error:#}"),
            }
            cx.background_executor()
                .timer(std::time::Duration::from_secs(gap))
                .await;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(existing) = workspace.item_of_type::<TaskAgentsView>(cx) {
        workspace.activate_item(&existing, true, true, window, cx);
        existing.update(cx, |view, cx| view.reload(None, cx));
        return;
    }
    let project = workspace.project().clone();
    let weak_workspace = workspace.weak_handle();
    let view = cx.new(|cx| TaskAgentsView::new(project, weak_workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

struct Template {
    name: &'static str,
    command: &'static str,
    description: &'static str,
    icon: IconName,
    profile: &'static str,
    toolkits: &'static [&'static str],
    permissions: &'static [(&'static str, PermissionMode)],
    instructions: &'static str,
}

const TEMPLATES: &[Template] = &[
    Template {
        name: "Revisor de PR",
        command: "revisar",
        description: "Revisa o PR do branch contra o destino e a tarefa do ClickUp",
        icon: IconName::PullRequest,
        profile: "ask",
        toolkits: &["repo", "clickup"],
        permissions: &[("repo_pr_comment", PermissionMode::Confirm)],
        instructions: "Você revisa o pull request do branch atual (ou o número que o usuário der).\n\n## Como\n1. Leia o PR com `repo_pull_request` e o diff com `repo_pr_diff`.\n2. Leia a tarefa ligada com `clickup_task` e confira se o PR entrega os critérios de aceite.\n3. Abra os arquivos que precisar para entender o contexto.\n\n## Priorize\n- bugs de lógica, regressões e casos de borda\n- segurança: segredos, tokens, dados sensíveis\n- o que a tarefa pede e o PR não entrega\n\n## Ignore\n- estilo que o lint já pega\n- arquivos gerados (lockfiles, builds)\n\n## Formato\nListe os achados por severidade (bloqueia, sugestão, nit), cada um com `arquivo:linha`, o problema e a correção. Só publique comentários no PR se o usuário pedir.",
    },
    Template {
        name: "Commit",
        command: "commit",
        description: "Agrupa as mudanças e escreve as mensagens no padrão do repo",
        icon: IconName::GitCommit,
        profile: "write",
        toolkits: &[],
        permissions: &[],
        instructions: "Você prepara commits das mudanças locais.\n\n1. Rode `git status` e `git diff` para ver o que mudou, e `git log -20 --oneline` para aprender o padrão das mensagens do repo.\n2. Agrupe as mudanças em commits coesos e proponha a lista (mensagem + arquivos) antes de commitar.\n3. Nunca inclua arquivos com segredos (.env, chaves) e nunca faça push sem o usuário pedir.\n4. Depois da confirmação, faça `git add` dos arquivos de cada grupo e `git commit -m`.",
    },
    Template {
        name: "Virar US",
        command: "us",
        description: "Transforma a conversa ou a seleção em história no ClickUp",
        icon: IconName::ListTodo,
        profile: "ask",
        toolkits: &["clickup"],
        permissions: &[("clickup_create_task", PermissionMode::Confirm)],
        instructions: "Você transforma o que foi discutido (ou o texto que o usuário der) em uma história de usuário.\n\n## Formato\n- Título curto, no infinitivo\n- \"Como <persona>, quero <ação>, para <benefício>\"\n- Critérios de aceite em lista, verificáveis\n- Notas técnicas quando houver\n\nMostre a história primeiro. Só crie no ClickUp (`clickup_create_task`) quando o usuário aprovar; use a lista da tarefa do branch se ele não disser outra.",
    },
    Template {
        name: "QA do app",
        command: "qa",
        description: "Percorre um fluxo no simulador e no emulador e confere API e banco",
        icon: IconName::Smartphone,
        profile: "ask",
        toolkits: &["devices", "browser", "api", "database"],
        permissions: &[
            ("devices", PermissionMode::Allow),
            ("browser", PermissionMode::Allow),
            ("api_send", PermissionMode::Confirm),
        ],
        instructions: "Você testa o app como uma pessoa usaria.\n\n1. Se nenhum device estiver aberto, veja as opções com `device_list` e abra o do teste com `device_open`, que já devolve a tela; senão tire `device_screenshot` para ver onde o app está.\n2. Percorra o fluxo pedido com `device_tap`, `device_type` e `device_swipe`, tirando uma captura depois de cada passo.\n3. Quando uma tela depender do backend, confira com `api_send` ou `database_query` se os dados batem.\n4. Leia `device_logs` quando algo der errado.\n\nNo fim, faça um relatório: os passos, o que funcionou, e cada bug com como reproduzir, o esperado e o que aconteceu.",
    },
    Template {
        name: "Escrever testes",
        command: "testes",
        description: "Escreve testes para o arquivo ou a função pedida",
        icon: IconName::CheckDouble,
        profile: "write",
        toolkits: &[],
        permissions: &[],
        instructions: "Você escreve testes automatizados.\n\n1. Descubra o framework de testes do projeto e o padrão dos testes existentes.\n2. Cubra o caminho feliz, os casos de borda e os erros do código pedido.\n3. Rode os testes e corrija até passarem, sem mudar o comportamento do código testado sem avisar.",
    },
];

/// The agent a template starts, or a blank one. Where it is saved is decided on creation.
fn template_agent(template: Option<&Template>) -> TaskAgent {
    let (name, command) = match template {
        Some(template) => (template.name.to_string(), template.command.to_string()),
        None => (
            t!("agent_ui.task_agents.new_agent").to_string(),
            "novo-agente".to_string(),
        ),
    };
    TaskAgent {
        id: command.clone(),
        name,
        command,
        description: template
            .map(|template| template.description.to_string())
            .unwrap_or_default(),
        profile: template.map(|template| template.profile.to_string()),
        model: None,
        subagent_model: None,
        subagent_effort: None,
        toolkits: template
            .map(|template| template.toolkits.iter().map(|id| id.to_string()).collect())
            .unwrap_or_default(),
        tools: Default::default(),
        permissions: template
            .map(|template| {
                template
                    .permissions
                    .iter()
                    .map(|(name, mode)| {
                        (
                            name.to_string(),
                            ToolPermissionRules {
                                default: Some(*mode),
                                ..Default::default()
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        takes: Vec::new(),
        gives: Vec::new(),
        context_servers: Vec::new(),
        enabled: true,
        triggers: Default::default(),
        instructions: template
            .map(|template| template.instructions.to_string())
            .unwrap_or_else(|| t!("agent_ui.task_agents.default_instructions").to_string()),
        source: TaskAgentSource::Personal,
        file_path: PathBuf::new(),
    }
}

/// How many times the model may fix its own file before what it wrote is saved as is.
const MAX_AUTHORING_REPAIRS: usize = 2;

fn permission_label(mode: Option<PermissionMode>) -> SharedString {
    match mode {
        None => t!("agent_ui.task_agents.permission.default"),
        Some(PermissionMode::Allow) => t!("agent_ui.task_agents.permission.allow"),
        Some(PermissionMode::Confirm) => t!("agent_ui.task_agents.permission.confirm"),
        Some(PermissionMode::Deny) => t!("agent_ui.task_agents.permission.deny"),
    }
}

/// The Zed Agent's tools that ask before acting, with what they do.
fn builtin_permission_tools() -> [(&'static str, SharedString, SharedString); 10] {
    [
        (
            agent::TerminalTool::NAME,
            t!("agent_ui.task_agents.tool.terminal.label"),
            t!("agent_ui.task_agents.tool.terminal.title"),
        ),
        (
            agent::EditFileTool::NAME,
            t!("agent_ui.task_agents.tool.edit_file.label"),
            t!("agent_ui.task_agents.tool.edit_file.title"),
        ),
        (
            agent::WriteFileTool::NAME,
            t!("agent_ui.task_agents.tool.write_file.label"),
            t!("agent_ui.task_agents.tool.write_file.title"),
        ),
        (
            agent::DeletePathTool::NAME,
            t!("agent_ui.task_agents.tool.delete_path.label"),
            t!("agent_ui.task_agents.tool.delete_path.title"),
        ),
        (
            agent::MovePathTool::NAME,
            t!("agent_ui.task_agents.tool.move_path.label"),
            t!("agent_ui.task_agents.tool.move_path.title"),
        ),
        (
            agent::CopyPathTool::NAME,
            t!("agent_ui.task_agents.tool.copy_path.label"),
            t!("agent_ui.task_agents.tool.copy_path.title"),
        ),
        (
            agent::CreateDirectoryTool::NAME,
            t!("agent_ui.task_agents.tool.create_directory.label"),
            t!("agent_ui.task_agents.tool.create_directory.title"),
        ),
        (
            agent::FetchTool::NAME,
            t!("agent_ui.task_agents.tool.fetch.label"),
            t!("agent_ui.task_agents.tool.fetch.title"),
        ),
        (
            agent::WebSearchTool::NAME,
            t!("agent_ui.task_agents.tool.web_search.label"),
            t!("agent_ui.task_agents.tool.web_search.title"),
        ),
        (
            agent::SkillTool::NAME,
            t!("agent_ui.task_agents.tool.skill.label"),
            t!("agent_ui.task_agents.tool.skill.title"),
        ),
    ]
}

fn next_permission(mode: Option<PermissionMode>) -> Option<PermissionMode> {
    match mode {
        None => Some(PermissionMode::Allow),
        Some(PermissionMode::Allow) => Some(PermissionMode::Confirm),
        Some(PermissionMode::Confirm) => Some(PermissionMode::Deny),
        Some(PermissionMode::Deny) => None,
    }
}

struct Draft {
    agent: TaskAgent,
    /// The agent as saved, to tell whether the draft has changes.
    original: TaskAgent,
    name: Entity<Editor>,
    command: Entity<Editor>,
    description: Entity<Editor>,
    instructions: Entity<Editor>,
    /// The keystroke of the "atalho" trigger; empty means no shortcut.
    shortcut: Entity<Editor>,
}

pub struct TaskAgentsView {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    agents: Vec<TaskAgent>,
    load_errors: Vec<task_agents::TaskAgentLoadError>,
    selected: Option<usize>,
    /// Set when a reload finishes; applied on the next render, where a window is available to
    /// build the editors.
    pending_selection: Option<usize>,
    draft: Option<Draft>,
    search: Entity<Editor>,
    status: Option<SharedString>,
    segment: Segment,
    detail_tab: DetailTab,
    editing_name: bool,
    adding_io: Option<(IoList, Entity<Editor>)>,
    flows: Vec<flows::Flow>,
    selected_flow: Option<usize>,
    _load_task: Task<()>,
    _flows_load: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    Agents,
    Flows,
}

/// The tabs of an agent's page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    General,
    Tools,
    Instructions,
    Runs,
}

/// Opens the Agentes tab on its Esteiras segment.
pub fn open_flows(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    open(workspace, window, cx);
    if let Some(view) = workspace.item_of_type::<TaskAgentsView>(cx) {
        view.update(cx, |view, cx| {
            view.segment = Segment::Flows;
            cx.notify();
        });
    }
}

impl TaskAgentsView {
    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fs = project.read(cx).fs().clone();
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("agent_ui.task_agents.search_placeholder"), window, cx);
            editor
        });
        let mut this = Self {
            project,
            workspace,
            fs,
            focus_handle: cx.focus_handle(),
            agents: Vec::new(),
            load_errors: Vec::new(),
            selected: None,
            pending_selection: None,
            draft: None,
            search: search.clone(),
            status: None,
            segment: Segment::Agents,
            detail_tab: DetailTab::General,
            editing_name: false,
            adding_io: None,
            flows: Vec::new(),
            selected_flow: None,
            _load_task: Task::ready(()),
            _flows_load: Task::ready(()),
            _subscriptions: vec![
                cx.subscribe(
                    &LanguageModelRegistry::global(cx),
                    |_, _, _: &language_model::Event, cx| cx.notify(),
                ),
                cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
                cx.observe_global::<flows::FlowsRevision>(|this, cx| this.reload_flows(cx)),
                cx.subscribe(&search, |_, _, event: &editor::EditorEvent, cx| {
                    if matches!(event, editor::EditorEvent::BufferEdited) {
                        cx.notify();
                    }
                }),
            ],
        };
        this.reload(None, cx);
        this.reload_flows(cx);
        this
    }

    fn reload_flows(&mut self, cx: &mut Context<Self>) {
        let roots = self.worktree_roots(cx);
        let fs = self.fs.clone();
        self._flows_load = cx.spawn(async move |this, cx| {
            let (flows, errors) = flows::load_flows(fs, roots).await;
            for error in errors {
                log::warn!(
                    "failed to load flow {}: {}",
                    error.path.display(),
                    error.message
                );
            }
            this.update(cx, |this, cx| {
                this.flows = flows;
                if this
                    .selected_flow
                    .is_some_and(|index| index >= this.flows.len())
                {
                    this.selected_flow = None;
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_segment(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let segment = |id: &'static str,
                       label: SharedString,
                       value: Segment,
                       current: Segment,
                       cx: &mut Context<Self>| {
            segment_option(id, label, value == current, true, cx).on_click(cx.listener(
                move |this, _, _window, cx| {
                    this.segment = value;
                    cx.notify();
                },
            ))
        };
        div().px_2().pt_2().child(
            segmented_control(cx)
                .child(segment(
                    "segment-agents",
                    t!("agent_ui.task_agents.segment.agents"),
                    Segment::Agents,
                    self.segment,
                    cx,
                ))
                .child(segment(
                    "segment-flows",
                    t!("agent_ui.task_agents.segment.flows"),
                    Segment::Flows,
                    self.segment,
                    cx,
                )),
        )
    }

    fn render_flow_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let mut list = v_flex()
            .id("flows-list")
            .size_full()
            .gap_1()
            .p_2()
            .overflow_y_scroll();
        let sections: [(SharedString, fn(&flows::FlowSource) -> bool); 2] = [
            (t!("agent_ui.task_agents.section.project"), |source| {
                matches!(source, flows::FlowSource::Project { .. })
            }),
            (t!("agent_ui.task_agents.section.personal"), |source| {
                matches!(source, flows::FlowSource::Personal)
            }),
        ];
        for (title, belongs) in sections {
            let entries: Vec<(usize, &flows::Flow)> = self
                .flows
                .iter()
                .enumerate()
                .filter(|(_, flow)| belongs(&flow.source))
                .filter(|(_, flow)| {
                    query.is_empty()
                        || flow.name.to_lowercase().contains(&query)
                        || flow.id.contains(&query)
                })
                .collect();
            if entries.is_empty() {
                continue;
            }
            list = list.child(
                div().pt_2().px_1().child(
                    Label::new(title)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
            for (index, flow) in entries {
                let selected = self.selected_flow == Some(index);
                let steps = flow.steps.len();
                list = list.child(
                    h_flex()
                        .id(("flow", index))
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .child(Icon::new(IconName::ListTree).size(IconSize::Small).color(
                            if selected {
                                Color::Accent
                            } else {
                                Color::Muted
                            },
                        ))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(Label::new(flow.name.clone()).single_line().truncate())
                                .child(
                                    Label::new(format!(
                                        "/{} · {}",
                                        flow.id,
                                        if steps == 1 {
                                            t!("agent_ui.task_agents.step_count.one", count = steps)
                                        } else {
                                            t!(
                                                "agent_ui.task_agents.step_count.other",
                                                count = steps
                                            )
                                        }
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.selected_flow = Some(index);
                            cx.notify();
                        })),
                );
            }
        }
        if self.flows.is_empty() {
            list = list.child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .child(Label::new(t!("agent_ui.task_agents.flows.empty_title")))
                    .child(
                        Label::new(t!("agent_ui.task_agents.flows.empty_hint"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }
        list
    }

    fn render_flow_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(flow) = self.selected_flow.and_then(|index| self.flows.get(index)) else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::ListTree)
                        .size(IconSize::XLarge)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(t!("agent_ui.task_agents.flows.intro_title")).size(LabelSize::Large),
                )
                .child(Label::new(t!("agent_ui.task_agents.flows.intro_body")).color(Color::Muted))
                .into_any_element();
        };
        let path = flow.file_path.clone();
        let flow_id = flow.id.clone();
        let mut steps = v_flex().gap_1();
        for (index, step) in flow.steps.iter().enumerate() {
            let (icon, color) = match step.kind {
                flows::StepKind::Agent(_) => (IconName::Person, Color::Accent),
                flows::StepKind::Gate(_) => (IconName::UserCheck, Color::Warning),
                flows::StepKind::Run(_) => (IconName::Terminal, Color::Success),
                flows::StepKind::Branch(_) => (IconName::GitBranch, Color::Accent),
                flows::StepKind::Parallel(_) => (IconName::ListTree, Color::Accent),
                flows::StepKind::GitHub(_) => (IconName::Blocks, Color::Accent),
            };
            steps = steps.child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(format!("{}", index + 1))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(Icon::new(icon).size(IconSize::Small).color(color))
                    .child(Label::new(step.label(&self.agents)).size(LabelSize::Small)),
            );
        }
        v_flex()
            .id("flow-detail")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(Headline::new(flow.name.clone()).size(HeadlineSize::Small))
                            .child(
                                Label::new(format!(
                                    "/{} · {}",
                                    flow.id,
                                    crate::flows_view::flow_source_label(&flow.source)
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        secondary_button(
                            "open-flow",
                            None,
                            t!("agent_ui.task_agents.flows.edit"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                let path = path.clone();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        crate::flows_view::open_flow_path(
                                            workspace, path, window, cx,
                                        )
                                    })
                                    .log_err();
                            },
                        )),
                    )
                    .child(
                        primary_button(
                            "run-flow-from-list",
                            Some(IconName::PlayFilled),
                            t!("agent_ui.task_agents.flows.run"),
                            None,
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::flows_view::RunFlow {
                                    flow: flow_id.clone(),
                                    input: String::new(),
                                }),
                                cx,
                            )
                        }),
                    ),
            )
            .when(!flow.input.is_empty(), |this| {
                this.child(
                    Label::new(t!("agent_ui.task_agents.flows.example", input = flow.input))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(steps)
            .when(!flow.notes.is_empty(), |this| {
                this.child(Label::new(flow.notes.clone()).size(LabelSize::Small))
            })
            .into_any_element()
    }

    fn worktree_roots(&self, cx: &App) -> Vec<(Arc<str>, PathBuf)> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree.as_local().map(|local| {
                    (
                        Arc::<str>::from(worktree.root_name_str()),
                        local.abs_path().to_path_buf(),
                    )
                })
            })
            .collect()
    }

    /// Reads the agent files again, keeping `select` (a file path) selected when given.
    fn reload(&mut self, select: Option<PathBuf>, cx: &mut Context<Self>) {
        let roots = self.worktree_roots(cx);
        let fs = self.fs.clone();
        let keep = select.or_else(|| {
            self.selected
                .and_then(|index| self.agents.get(index))
                .map(|agent| agent.file_path.clone())
        });
        self._load_task = cx.spawn(async move |this, cx| {
            let (agents, errors) = load_task_agents(fs, roots).await;
            this.update(cx, |this, cx| {
                this.agents = agents;
                this.load_errors = errors;
                let index = keep
                    .and_then(|path| this.agents.iter().position(|agent| agent.file_path == path))
                    .or(if this.agents.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                this.selected = None;
                this.draft = None;
                if let Some(index) = index {
                    this.pending_selection = Some(index);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.get(index).cloned() else {
            return;
        };
        let text_editor =
            |text: &str, placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
                let text = text.to_string();
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(&placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                })
            };
        let name = text_editor(
            &agent.name,
            t!("agent_ui.task_agents.editor.name_placeholder"),
            window,
            cx,
        );
        let command = text_editor(
            &agent.command,
            t!("agent_ui.task_agents.editor.command_placeholder"),
            window,
            cx,
        );
        let description = text_editor(
            &agent.description,
            t!("agent_ui.task_agents.editor.description_placeholder"),
            window,
            cx,
        );
        let shortcut = text_editor(
            agent.triggers.shortcut.as_deref().unwrap_or_default(),
            t!("agent_ui.task_agents.call.shortcut_placeholder"),
            window,
            cx,
        );
        let instructions = {
            let text = agent.instructions.clone();
            cx.new(|cx| {
                let mut editor = Editor::multi_line(window, cx);
                editor.set_placeholder_text(
                    &t!("agent_ui.task_agents.editor.instructions_placeholder"),
                    window,
                    cx,
                );
                editor.set_text(text, window, cx);
                editor.set_soft_wrap();
                editor
            })
        };
        self.set_markdown_language(&instructions, cx);
        self.selected = Some(index);
        self.editing_name = false;
        self.adding_io = None;
        self.draft = Some(Draft {
            original: agent.clone(),
            agent,
            name,
            command,
            description,
            instructions,
            shortcut,
        });
        self.status = None;
        cx.notify();
    }

    fn set_markdown_language(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        let language = self
            .project
            .read(cx)
            .languages()
            .language_for_name("Markdown");
        cx.spawn(async move |_, cx| {
            let language = language.await?;
            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn collect_draft(&self, cx: &App) -> Option<TaskAgent> {
        let draft = self.draft.as_ref()?;
        let mut agent = draft.agent.clone();
        agent.name = draft.name.read(cx).text(cx).trim().to_string();
        agent.command = command_slug(&draft.command.read(cx).text(cx));
        agent.description = draft.description.read(cx).text(cx).trim().to_string();
        agent.instructions = draft.instructions.read(cx).text(cx);
        let shortcut = draft.shortcut.read(cx).text(cx).trim().to_string();
        agent.triggers.shortcut = (!shortcut.is_empty()).then_some(shortcut);
        Some(agent)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = self.collect_draft(cx) else {
            return;
        };
        if agent.name.is_empty() || agent.command.is_empty() {
            self.status = Some(t!("agent_ui.task_agents.status.name_and_command_required"));
            cx.notify();
            return;
        }
        if [&agent.model, &agent.subagent_model]
            .into_iter()
            .flatten()
            .any(|model| task_agents::ModelReference::parse(model).is_none())
        {
            self.status = Some(t!("agent_ui.task_agents.status.invalid_model"));
            cx.notify();
            return;
        }
        let markdown = match agent.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                self.status = Some(t!("agent_ui.task_agents.status.save_failed", error = error));
                cx.notify();
                return;
            }
        };
        let fs = self.fs.clone();
        let path = agent.file_path;
        cx.spawn(async move |this, cx| {
            let result: Result<()> = async {
                if let Some(parent) = path.parent() {
                    fs.create_dir(parent).await?;
                }
                fs.atomic_write(path.clone(), markdown).await
            }
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.status = Some(t!(
                            "agent_ui.task_agents.status.saved",
                            path = path.display()
                        ));
                        this.refresh_agent_commands(cx);
                        this.reload(Some(path), cx);
                    }
                    Err(error) => {
                        this.status =
                            Some(t!("agent_ui.task_agents.status.save_failed", error = error));
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// The composer's slash menu comes from the native agent, which rereads the files when its
    /// project context refreshes; personal agents live outside the project, so ask for it.
    fn refresh_agent_commands(&self, cx: &mut App) {
        task_agents::notify_task_agents_changed(cx);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| panel.refresh_skills(cx));
        }
    }

    fn new_agent_path(&self, command: &str, personal: bool, cx: &App) -> PathBuf {
        let directory = if personal {
            personal_agents_dir()
        } else {
            self.worktree_roots(cx)
                .first()
                .map(|(_, root)| root.join(PROJECT_AGENTS_DIR))
                .unwrap_or_else(personal_agents_dir)
        };
        let mut path = directory.join(format!("{command}.md"));
        let mut suffix = 2;
        while self.agents.iter().any(|agent| agent.file_path == path) {
            path = directory.join(format!("{command}-{suffix}.md"));
            suffix += 1;
        }
        path
    }

    /// Writes a new agent file for `agent`, in the project or with the user's agents, and
    /// selects it. With `run_after`, a thread with its /command opens once the file is written.
    fn create_agent(
        &mut self,
        mut agent: TaskAgent,
        personal: bool,
        generated: bool,
        run_after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.new_agent_path(&agent.command, personal, cx);
        agent.id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| agent.command.clone());
        agent.source = if path.starts_with(personal_agents_dir()) {
            TaskAgentSource::Personal
        } else {
            TaskAgentSource::Project {
                worktree_root_name: self
                    .worktree_roots(cx)
                    .first()
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| {
                        t!("agent_ui.task_agents.source.project").to_string().into()
                    }),
            }
        };
        agent.file_path = path.clone();
        let markdown = match agent.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                self.status = Some(t!(
                    "agent_ui.task_agents.status.create_failed",
                    error = error
                ));
                cx.notify();
                return;
            }
        };
        let fs = self.fs.clone();
        let command = agent.command;
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                if let Some(parent) = path.parent() {
                    fs.create_dir(parent).await?;
                }
                fs.atomic_write(path.clone(), markdown).await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                let created = result.is_ok();
                this.status = Some(match result {
                    Ok(()) if generated => {
                        t!(
                            "agent_ui.task_agents.status.created_with_ai",
                            path = path.display()
                        )
                    }
                    Ok(()) => t!("agent_ui.task_agents.status.created", path = path.display()),
                    Err(error) => t!("agent_ui.task_agents.status.create_failed", error = error),
                });
                this.refresh_agent_commands(cx);
                this.reload(Some(path), cx);
                if created && run_after {
                    this.run_command(command, window, cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn open_new_agent_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let view = cx.entity().downgrade();
        let catalog = Arc::new(self.authoring_catalog(cx));
        let imported = self
            .agents
            .iter()
            .filter(|agent| matches!(agent.source, TaskAgentSource::Imported { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let personal = self.worktree_roots(cx).is_empty();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                NewAgentModal::new(view, catalog, imported, personal, window, cx)
            });
        });
    }

    fn authoring_catalog(&self, cx: &App) -> AuthoringCatalog {
        let settings = agent_settings::AgentSettings::get_global(cx);
        let profiles = settings
            .profiles
            .iter()
            .map(|(id, profile)| ProfileSummary {
                id: id.0.to_string(),
                name: profile.name.to_string(),
                enabled_tools: profile
                    .tools
                    .iter()
                    .filter(|(_, enabled)| **enabled)
                    .map(|(name, _)| name.to_string())
                    .collect(),
            })
            .collect();
        let examples = ["revisar", "qa"]
            .iter()
            .filter_map(|command| {
                TEMPLATES
                    .iter()
                    .find(|template| template.command == *command)
            })
            .filter_map(|template| {
                task_agents::example_markdown(
                    template.name,
                    template.command,
                    template.description,
                    template.profile,
                    template.toolkits,
                    template.permissions,
                    template.instructions,
                )
            })
            .collect();
        AuthoringCatalog {
            profiles,
            toolkits: toolkits(cx)
                .iter()
                .map(|toolkit| ToolkitSummary::from(toolkit.as_ref()))
                .collect(),
            existing_commands: self
                .agents
                .iter()
                .map(|agent| agent.command.clone())
                .collect(),
            examples,
        }
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.draft.as_ref().map(|draft| draft.agent.clone()) else {
            return;
        };
        let answers = [
            gpui::PromptButton::new(t!("agent_ui.task_agents.delete.confirm")),
            gpui::PromptButton::new(t!("agent_ui.task_agents.delete.cancel")),
        ];
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &t!("agent_ui.task_agents.delete.title", name = agent.name),
            Some(&t!(
                "agent_ui.task_agents.delete.detail",
                path = agent.file_path.display()
            )),
            &answers,
            cx,
        );
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = fs
                .trash(&agent.file_path, fs::RemoveOptions::default())
                .await
                .map(|_| ());
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.status =
                            Some(t!("agent_ui.task_agents.status.deleted", name = agent.name))
                    }
                    Err(error) => {
                        this.status = Some(t!(
                            "agent_ui.task_agents.status.delete_failed",
                            error = error
                        ))
                    }
                }
                this.refresh_agent_commands(cx);
                this.reload(None, cx);
            })
            .log_err();
        })
        .detach();
    }

    fn run_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.draft.as_ref().map(|draft| draft.agent.command.clone()) else {
            return;
        };
        self.run_command(command, window, cx);
    }

    fn run_command(&self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                run_task_agent(
                    workspace,
                    &RunTaskAgent {
                        command,
                        prompt: None,
                    },
                    window,
                    cx,
                )
            });
        }
    }

    fn open_file(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .draft
            .as_ref()
            .map(|draft| draft.agent.file_path.clone())
        else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, workspace::OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            });
        }
    }

    fn copy_to_project(&mut self, cx: &mut Context<Self>) {
        let Some(mut agent) = self.collect_draft(cx) else {
            return;
        };
        let path = self.new_agent_path(&agent.command, false, cx);
        agent.file_path = path;
        agent.source = TaskAgentSource::Project {
            worktree_root_name: t!("agent_ui.task_agents.source.project").to_string().into(),
        };
        if let Some(draft) = self.draft.as_mut() {
            draft.agent = agent;
        }
        self.save(cx);
    }

    fn toggle_toolkit(&mut self, toolkit_id: &str, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let toolkits = &mut draft.agent.toolkits;
        if let Some(position) = toolkits.iter().position(|id| id == toolkit_id) {
            toolkits.remove(position);
        } else {
            toolkits.push(toolkit_id.to_string());
        }
        cx.notify();
    }

    fn cycle_permission(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let current = draft
            .agent
            .permissions
            .get(key)
            .and_then(|rules| rules.default);
        match next_permission(current) {
            Some(mode) => {
                draft
                    .agent
                    .permissions
                    .entry(key.to_string())
                    .or_default()
                    .default = Some(mode);
            }
            None => {
                if let Some(rules) = draft.agent.permissions.get_mut(key) {
                    rules.default = None;
                    if rules.allow.is_empty() && rules.confirm.is_empty() && rules.deny.is_empty() {
                        draft.agent.permissions.remove(key);
                    }
                }
            }
        }
        cx.notify();
    }

    fn set_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.as_mut() {
            draft.agent.model = model;
            cx.notify();
        }
    }

    fn set_subagent_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.as_mut() {
            draft.agent.subagent_model = model;
            cx.notify();
        }
    }

    fn set_subagent_effort(
        &mut self,
        effort: Option<settings::SubagentThinkingEffort>,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.draft.as_mut() {
            draft.agent.subagent_effort = effort;
            cx.notify();
        }
    }

    fn set_profile(&mut self, profile: Option<String>, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.as_mut() {
            draft.agent.profile = profile;
            cx.notify();
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sections: [(SharedString, Option<&str>, fn(&TaskAgentSource) -> bool); 3] = [
            (
                t!("agent_ui.task_agents.section.project"),
                Some(PROJECT_AGENTS_DIR),
                |source| matches!(source, TaskAgentSource::Project { .. }),
            ),
            (
                t!("agent_ui.task_agents.section.personal"),
                None,
                |source| matches!(source, TaskAgentSource::Personal),
            ),
            (
                t!("agent_ui.task_agents.section.imported"),
                Some(".claude/agents"),
                |source| matches!(source, TaskAgentSource::Imported { .. }),
            ),
        ];
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let matches = |agent: &TaskAgent| {
            query.is_empty()
                || agent.name.to_lowercase().contains(&query)
                || agent.command.to_lowercase().contains(&query)
                || agent.description.to_lowercase().contains(&query)
        };
        let dirty = self.is_dirty(cx);
        let mut list = v_flex()
            .id("task-agents-list")
            .size_full()
            .pb_2()
            .overflow_y_scroll();
        for (title, path, belongs) in sections {
            let entries = self
                .agents
                .iter()
                .enumerate()
                .filter(|(_, agent)| belongs(&agent.source) && matches(agent))
                .collect::<Vec<_>>();
            if entries.is_empty() {
                continue;
            }
            list = list.child(
                h_flex()
                    .px_3p5()
                    .pt_2p5()
                    .pb_1()
                    .gap_1p5()
                    .child(
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .when_some(path, |this, path| {
                        this.child(
                            Label::new(path)
                                .size(LabelSize::XSmall)
                                .color(Color::Disabled)
                                .buffer_font(cx),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Label::new(entries.len().to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
            for (index, agent) in entries {
                let selected = self.selected == Some(index);
                let badge = if selected && dirty {
                    Some(t!("agent_ui.task_agents.badge.unsaved"))
                } else if !agent.enabled {
                    Some(t!("agent_ui.task_agents.badge.disabled"))
                } else if !agent.source.is_editable() {
                    Some(t!("agent_ui.task_agents.badge.read_only"))
                } else {
                    None
                };
                list = list.child(
                    div().px_1p5().child(
                        h_flex()
                            .id(("task-agent", index))
                            .h(px(46.))
                            .px_2()
                            .gap_2p5()
                            .rounded_lg()
                            .border_1()
                            .cursor_pointer()
                            .map(|this| {
                                if selected {
                                    this.bg(cx.theme().colors().element_selected)
                                        .border_color(cx.theme().colors().border)
                                } else {
                                    this.border_color(gpui::transparent_black())
                                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                                }
                            })
                            .child(agent_tile(agent, px(28.), IconSize::Small, cx))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Label::new(agent.name.clone())
                                            .weight(if selected {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::MEDIUM
                                            })
                                            .color(if agent.enabled {
                                                Color::Default
                                            } else {
                                                Color::Muted
                                            })
                                            .single_line()
                                            .truncate(),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .min_w_0()
                                            .child(
                                                Label::new(format!("/{}", agent.command))
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Accent)
                                                    .buffer_font(cx),
                                            )
                                            .child(
                                                Label::new(format!(
                                                    "· {}",
                                                    self.agent_detail(agent, cx)
                                                ))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line()
                                                .truncate(),
                                            ),
                                    ),
                            )
                            .when_some(badge, |this, badge| this.child(list_badge(badge, cx)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(index, window, cx);
                            })),
                    ),
                );
            }
        }
        if self.agents.is_empty() {
            list = list.child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .child(Label::new(t!("agent_ui.task_agents.empty.title")))
                    .child(
                        Label::new(t!("agent_ui.task_agents.empty.hint"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }
        for error in &self.load_errors {
            list = list.child(
                div().px_3p5().pt_1().child(
                    Label::new(format!(
                        "{}: {}",
                        error
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        error.message
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Error),
                ),
            );
        }
        list
    }

    /// The second line of an agent in the list, after its command: the esteiras it is a step
    /// of, or else how much it may do.
    fn agent_detail(&self, agent: &TaskAgent, cx: &App) -> String {
        let flows = self.flows_using(&agent.command).len();
        if flows > 0 {
            return if flows == 1 {
                t!("agent_ui.task_agents.flow_count.one", count = flows).to_string()
            } else {
                t!("agent_ui.task_agents.flow_count.other", count = flows).to_string()
            };
        }
        match agent.profile.as_deref() {
            Some("ask") => t!("agent_ui.task_agents.detail.read_only").to_string(),
            Some("write") => t!("agent_ui.task_agents.detail.writes").to_string(),
            Some(id) => AgentProfile::available_profiles(cx)
                .iter()
                .find(|(profile_id, _)| profile_id.0.as_ref() == id)
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| id.to_string()),
            None => t!("agent_ui.task_agents.detail.thread_profile").to_string(),
        }
    }

    fn flows_using(&self, command: &str) -> Vec<usize> {
        self.flows
            .iter()
            .enumerate()
            .filter(|(_, flow)| {
                flow.steps.iter().any(|step| match &step.kind {
                    flows::StepKind::Agent(agent_step) => agent_step.agent == command,
                    flows::StepKind::Parallel(parallel) => {
                        parallel.agents.iter().any(|agent| agent == command)
                    }
                    _ => false,
                })
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match (self.draft.as_ref(), self.collect_draft(cx)) {
            (Some(draft), Some(current)) => current != draft.original,
            _ => false,
        }
    }

    fn render_card(title: impl Into<SharedString>, action: Option<AnyElement>, cx: &App) -> Div {
        v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(40.))
                    .flex_none()
                    .px_3p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(title.into())
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(action),
            )
    }
    fn render_profile_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = self
            .draft
            .as_ref()
            .and_then(|draft| draft.agent.profile.clone());
        let profiles = AgentProfile::available_profiles(cx);
        let label = current
            .as_ref()
            .map(|id| {
                profiles
                    .iter()
                    .find(|(profile_id, _)| profile_id.0.as_ref() == id.as_str())
                    .map(|(_, name)| name.to_string())
                    .unwrap_or_else(|| id.clone())
            })
            .unwrap_or_else(|| t!("agent_ui.task_agents.profile.current_thread").to_string());
        let view = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let keep_view = view.clone();
            menu = menu.entry(
                t!("agent_ui.task_agents.profile.current_thread"),
                None,
                move |_window, cx| {
                    keep_view
                        .update(cx, |view, cx| view.set_profile(None, cx))
                        .log_err();
                },
            );
            for (profile_id, name) in profiles.iter() {
                let view = view.clone();
                let id = profile_id.0.to_string();
                menu = menu.entry(name.clone(), None, move |_window, cx| {
                    let id = id.clone();
                    view.update(cx, |view, cx| view.set_profile(Some(id), cx))
                        .log_err();
                });
            }
            menu
        });
        selector("task-agent-profile", IconName::ToolHammer, label, menu)
    }

    fn render_model_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = self
            .draft
            .as_ref()
            .and_then(|draft| draft.agent.model.clone());
        let view = cx.entity().downgrade();
        model_dropdown(
            "task-agent-model",
            current,
            t!("agent_ui.task_agents.model.thread"),
            move |model, cx| {
                view.update(cx, |view, cx| view.set_model(model, cx))
                    .log_err();
            },
            window,
            cx,
        )
    }

    fn render_agent_subagent_model_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = self
            .draft
            .as_ref()
            .and_then(|draft| draft.agent.subagent_model.clone());
        let view = cx.entity().downgrade();
        model_dropdown(
            "task-agent-subagent-model",
            current,
            t!("agent_ui.task_agents.model.subagent_default"),
            move |model, cx| {
                view.update(cx, |view, cx| view.set_subagent_model(model, cx))
                    .log_err();
            },
            window,
            cx,
        )
    }

    fn render_subagent_effort_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use settings::SubagentThinkingEffort;

        let current = self
            .draft
            .as_ref()
            .and_then(|draft| draft.agent.subagent_effort);
        let label = |effort: Option<SubagentThinkingEffort>| -> SharedString {
            match effort {
                None | Some(SubagentThinkingEffort::Inherit) => {
                    t!("agent_ui.task_agents.effort.default")
                }
                Some(SubagentThinkingEffort::Low) => t!("agent_ui.task_agents.effort.low"),
                Some(SubagentThinkingEffort::Medium) => t!("agent_ui.task_agents.effort.medium"),
                Some(SubagentThinkingEffort::High) => t!("agent_ui.task_agents.effort.high"),
                Some(SubagentThinkingEffort::XHigh) => t!("agent_ui.task_agents.effort.xhigh"),
                Some(SubagentThinkingEffort::Max) => t!("agent_ui.task_agents.effort.max"),
            }
        };
        let view = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let options = std::iter::once(None).chain(
                [
                    SubagentThinkingEffort::Low,
                    SubagentThinkingEffort::Medium,
                    SubagentThinkingEffort::High,
                    SubagentThinkingEffort::XHigh,
                    SubagentThinkingEffort::Max,
                ]
                .into_iter()
                .map(Some),
            );
            for effort in options {
                let view = view.clone();
                menu = menu.toggleable_entry(
                    label(effort),
                    effort == current,
                    IconPosition::End,
                    None,
                    move |_window, cx| {
                        view.update(cx, |view, cx| view.set_subagent_effort(effort, cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        selector(
            "task-agent-subagent-effort",
            IconName::Sparkle,
            label(current),
            menu,
        )
    }

    fn render_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(draft) = self.draft.as_ref() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::UserGroup)
                        .size(IconSize::XLarge)
                        .color(Color::Muted),
                )
                .child(Label::new(t!("agent_ui.task_agents.intro.title")).size(LabelSize::Large))
                .child(Label::new(t!("agent_ui.task_agents.intro.body")).color(Color::Muted))
                .child(
                    primary_button(
                        "empty-new-agent",
                        Some(IconName::Plus),
                        t!("agent_ui.task_agents.new_agent"),
                        None,
                        cx,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_new_agent_modal(window, cx)),
                    ),
                )
                .into_any_element();
        };
        v_flex()
            .size_full()
            .px(px(22.))
            .py(px(18.))
            .gap_4()
            .child(self.render_detail_header(cx))
            .child(
                div()
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .hover(|this| this.border_color(cx.theme().colors().border))
                    .text_color(cx.theme().colors().text_muted)
                    .child(draft.description.clone()),
            )
            .when_some(self.status.clone(), |this, status| {
                this.child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(self.render_detail_tabs(cx))
            .child(match self.detail_tab {
                DetailTab::General => self.render_general_tab(window, cx).into_any_element(),
                DetailTab::Tools => self.render_tools_tab(cx).into_any_element(),
                DetailTab::Instructions => div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_instructions_card(cx))
                    .into_any_element(),
                DetailTab::Runs => self.render_runs_tab(cx).into_any_element(),
            })
            .into_any_element()
    }

    fn render_general_tab(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_h_0()
            .items_start()
            .gap_4()
            .child(
                v_flex()
                    .id("task-agent-settings")
                    .w(px(560.))
                    .flex_none()
                    .h_full()
                    .gap_4()
                    .overflow_y_scroll()
                    .child(self.render_call_card(window, cx))
                    .child(self.render_model_card(window, cx))
                    .child(self.render_io_card(cx))
                    .child(
                        Label::new(t!("agent_ui.task_agents.io.flows_hint"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_instructions_card(cx)),
            )
    }

    /// The agent's threads, newest first, by the command they were handed to.
    fn runs(&self, cx: &App) -> Vec<crate::thread_metadata_store::ThreadMetadata> {
        let Some(draft) = self.draft.as_ref() else {
            return Vec::new();
        };
        let Some(store) = crate::thread_metadata_store::ThreadMetadataStore::try_global(cx) else {
            return Vec::new();
        };
        let command = draft.original.command.as_str();
        let mut runs = store
            .read(cx)
            .entries()
            .filter(|thread| thread.task_agent.as_deref() == Some(command))
            .cloned()
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| std::cmp::Reverse(run.updated_at));
        runs
    }

    fn render_detail_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let runs = self.runs(cx).len();
        let tab =
            |id: &'static str, label: SharedString, value: DetailTab, cx: &mut Context<Self>| {
                choice_chip(id, None, label, self.detail_tab == value, true, cx).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.detail_tab = value;
                        cx.notify();
                    }),
                )
            };
        let legend = |label: SharedString, color: gpui::Hsla| {
            h_flex()
                .gap_1()
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        let (success, warning, error) = {
            let status = cx.theme().status();
            (status.success, status.warning, status.error)
        };
        h_flex()
            .w_full()
            .gap_1()
            .child(tab(
                "agent-tab-general",
                t!("agent_ui.task_agents.tab.general"),
                DetailTab::General,
                cx,
            ))
            .child(tab(
                "agent-tab-tools",
                t!("agent_ui.task_agents.tab.tools"),
                DetailTab::Tools,
                cx,
            ))
            .child(tab(
                "agent-tab-instructions",
                t!("agent_ui.task_agents.tab.instructions"),
                DetailTab::Instructions,
                cx,
            ))
            .child(
                tab(
                    "agent-tab-runs",
                    t!("agent_ui.task_agents.tab.runs"),
                    DetailTab::Runs,
                    cx,
                )
                .when(runs > 0, |this| {
                    this.child(list_badge(runs.to_string(), cx))
                }),
            )
            .child(div().flex_1())
            .when(self.detail_tab == DetailTab::Tools, |this| {
                let on = self
                    .draft
                    .as_ref()
                    .map_or(0, |draft| draft.agent.toolkits.len());
                this.gap_3()
                    .child(
                        Label::new(t!(
                            "agent_ui.task_agents.tools_tab.count",
                            on = on,
                            total = toolkits(cx).len()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(legend(t!("agent_ui.task_agents.permission.allow"), success))
                    .child(legend(
                        t!("agent_ui.task_agents.permission.confirm"),
                        warning,
                    ))
                    .child(legend(t!("agent_ui.task_agents.permission.deny"), error))
            })
    }

    /// A tool's chip, in the color of what the agent may do with it; a click cycles the mode.
    fn tool_permission_chip(
        &self,
        agent: &TaskAgent,
        registered: &[Arc<Toolkit>],
        name: String,
        label: SharedString,
        title: SharedString,
        access: Option<ToolAccess>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mode_of = |key: &str| agent.permissions.get(key).and_then(|rules| rules.default);
        let tool_mode = mode_of(&name);
        let toolkit_mode = access.and_then(|_| {
            registered
                .iter()
                .find(|toolkit| toolkit.tools.iter().any(|tool| tool.name.as_ref() == name))
                .and_then(|toolkit| mode_of(toolkit.id.as_ref()))
        });
        let effective = tool_mode
            .or(toolkit_mode)
            .or(mode_of(ALL_TOOLS_PERMISSION_KEY));
        let look = match (effective, access) {
            (Some(PermissionMode::Allow), _) => ChipLook::On,
            (Some(PermissionMode::Confirm), _) => ChipLook::Ask,
            (Some(PermissionMode::Deny), _) => ChipLook::Never,
            (None, Some(ToolAccess::Read)) => ChipLook::On,
            (None, Some(_)) => ChipLook::Ask,
            (None, None) => ChipLook::Off,
        };
        let mode_label = match (effective, access) {
            (None, Some(ToolAccess::Read)) => t!("agent_ui.task_agents.permission.default_allow"),
            (None, Some(_)) => t!("agent_ui.task_agents.permission.default_confirm"),
            (mode, _) => permission_label(mode),
        };
        let key = name.clone();
        chip(
            SharedString::from(format!("perm-{name}")),
            label,
            look,
            false,
            cx,
        )
        .tooltip(Tooltip::text(t!(
            "agent_ui.task_agents.permission.tooltip",
            title = title,
            mode = mode_label
        )))
        .when(editable, |this| {
            this.on_click(cx.listener(move |this, _, _window, cx| {
                this.cycle_permission(&key, cx);
            }))
        })
    }

    fn render_tools_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return div().id("task-agent-tools");
        };
        let agent = draft.agent.clone();
        let editable = agent.source.is_editable();
        let registered = toolkits(cx);
        let mut cards = Vec::new();
        for toolkit in registered.iter() {
            let on = agent.toolkits.iter().any(|id| id == toolkit.id.as_ref());
            let toolkit_id = toolkit.id.to_string();
            let mut tools = h_flex().flex_wrap().gap_1p5();
            for tool in &toolkit.tools {
                let chip = self.tool_permission_chip(
                    &agent,
                    &registered,
                    tool.name.to_string(),
                    tool.title.to_lowercase().into(),
                    tool.description.clone(),
                    Some(tool.access),
                    editable && on,
                    cx,
                );
                tools = tools.child(chip.when(!on, |this| this.opacity(0.5)));
            }
            cards.push(
                Self::render_toolkit_card(
                    SharedString::from(format!("toolkit-card-{toolkit_id}")),
                    Icon::from_path(format!("icons/{}.svg", toolkit.icon)),
                    toolkit.name.clone(),
                    toolkit.id.clone(),
                    on,
                    editable,
                    tools,
                    Some(toolkit.description.clone()),
                    cx,
                )
                .when(editable, |this| {
                    this.on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_toolkit(&toolkit_id, cx);
                    }))
                })
                .into_any_element(),
            );
        }

        let configured = self
            .project
            .read(cx)
            .context_server_store()
            .read(cx)
            .configured_server_ids()
            .into_iter()
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>();
        let mut servers = configured.clone();
        for server in &agent.context_servers {
            if !servers.contains(server) {
                servers.push(server.clone());
            }
        }
        let mut server_chips = h_flex().flex_wrap().gap_1p5();
        for server in servers {
            let on = agent.context_servers.contains(&server);
            let missing = !configured.contains(&server);
            let server_id = server.clone();
            server_chips = server_chips.child(
                chip(
                    SharedString::from(format!("tools-tab-mcp-{server}")),
                    server.into(),
                    if missing {
                        ChipLook::Never
                    } else if on {
                        ChipLook::On
                    } else {
                        ChipLook::Off
                    },
                    true,
                    cx,
                )
                .when(editable, |this| {
                    this.on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_context_server(&server_id, cx);
                    }))
                }),
            );
        }
        cards.push(
            Self::render_toolkit_card(
                "toolkit-card-mcps".into(),
                Icon::new(IconName::Blocks),
                t!("agent_ui.task_agents.tools_tab.mcps"),
                t!("agent_ui.task_agents.tools_tab.mcps_id"),
                !agent.context_servers.is_empty(),
                false,
                server_chips,
                Some(t!("agent_ui.task_agents.model_card.mcps_hint")),
                cx,
            )
            .into_any_element(),
        );

        let mut left = v_flex().flex_1().min_w_0().gap_3();
        let mut right = v_flex().flex_1().min_w_0().gap_3();
        for (index, card) in cards.into_iter().enumerate() {
            if index % 2 == 0 {
                left = left.child(card);
            } else {
                right = right.child(card);
            }
        }
        div()
            .id("task-agent-tools")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .child(h_flex().items_start().gap_3().child(left).child(right))
    }

    fn render_toolkit_card(
        id: SharedString,
        icon: Icon,
        name: SharedString,
        subtitle: SharedString,
        on: bool,
        clickable: bool,
        chips: Div,
        hint: Option<SharedString>,
        cx: &App,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        v_flex()
            .id(id.clone())
            .w_full()
            .p_3()
            .gap_2p5()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .gap_2p5()
                    .child(
                        h_flex()
                            .flex_none()
                            .size(px(28.))
                            .justify_center()
                            .rounded(px(7.))
                            .bg(colors.element_selected)
                            .child(icon.size(IconSize::Small).color(Color::Muted)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).weight(FontWeight::SEMIBOLD).truncate())
                            .child(
                                Label::new(subtitle)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    )
                    .child(toggle_switch(
                        SharedString::from(format!("{id}-switch")),
                        on,
                        clickable,
                        cx,
                    )),
            )
            .child(chips)
            .when_some(hint, |this, hint| {
                this.child(
                    Label::new(hint)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
    }

    fn render_runs_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let runs = self.runs(cx);
        let mut list = v_flex()
            .id("task-agent-runs")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll();
        if runs.is_empty() {
            return list.child(
                v_flex()
                    .py_8()
                    .items_center()
                    .gap_1()
                    .child(Label::new(t!("agent_ui.task_agents.runs.empty")).color(Color::Muted))
                    .child(
                        Label::new(t!("agent_ui.task_agents.runs.empty_hint"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }
        let count = runs.len();
        let mut card = Self::render_card(t!("agent_ui.task_agents.runs.title"), None, cx);
        for (index, run) in runs.into_iter().enumerate() {
            let when = run
                .updated_at
                .with_timezone(&chrono::Local)
                .format("%d/%m %H:%M")
                .to_string();
            card = card.child(
                h_flex()
                    .id(("task-agent-run", index))
                    .w_full()
                    .px_3p5()
                    .py_2p5()
                    .gap_2p5()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                    .when(index + 1 < count, |this| {
                        this.border_b_1().border_color(cx.theme().colors().border)
                    })
                    .child(
                        Icon::new(if run.archived {
                            IconName::Archive
                        } else {
                            IconName::Thread
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(run.display_title())
                            .single_line()
                            .truncate()
                            .flex_1(),
                    )
                    .child(
                        Label::new(when)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_run(&run, window, cx);
                    })),
            );
        }
        list = list.child(card);
        list
    }

    fn open_run(
        &self,
        run: &crate::thread_metadata_store::ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    crate::Agent::from(run.agent_id.clone()),
                    run.thread_id,
                    Some(run.folder_paths().clone()),
                    run.title.clone(),
                    true,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
        });
    }

    fn render_detail_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return h_flex();
        };
        let agent = &draft.agent;
        let editable = agent.source.is_editable();
        let source_badge = match &agent.source {
            TaskAgentSource::Project { .. } => t!("agent_ui.task_agents.source.project"),
            TaskAgentSource::Personal => t!("agent_ui.task_agents.source.personal"),
            TaskAgentSource::Imported { .. } => t!("agent_ui.task_agents.source.imported"),
        };
        let name = if self.editing_name {
            div()
                .w(px(280.))
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_focused)
                .bg(cx.theme().colors().editor_background)
                .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                    this.editing_name = false;
                    cx.notify();
                }))
                .child(draft.name.clone())
                .into_any_element()
        } else {
            h_flex()
                .gap_1()
                .child(
                    Label::new(draft.name.read(cx).text(cx))
                        .size(LabelSize::Large)
                        .weight(FontWeight::SEMIBOLD),
                )
                .when(editable, |this| {
                    this.child(
                        IconButton::new("rename-agent", IconName::Pencil)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(t!("agent_ui.task_agents.rename")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.editing_name = true;
                                if let Some(draft) = this.draft.as_ref() {
                                    draft.name.focus_handle(cx).focus(window, cx);
                                }
                                cx.notify();
                            })),
                    )
                })
                .into_any_element()
        };
        h_flex()
            .w_full()
            .gap_3()
            .child(agent_tile(agent, px(40.), IconSize::Medium, cx))
            .child(
                v_flex().min_w_0().gap_0p5().child(name).child(
                    Label::new(self.display_path(&agent.file_path, cx))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .truncate(),
                ),
            )
            .child(list_badge(source_badge, cx))
            .child(div().flex_1())
            .when(editable, |this| {
                this.child(
                    h_flex()
                        .id("task-agent-enabled-row")
                        .gap_2()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            if let Some(draft) = this.draft.as_mut() {
                                draft.agent.enabled = !draft.agent.enabled;
                                cx.notify();
                            }
                        }))
                        .child(toggle_switch("task-agent-enabled", agent.enabled, true, cx))
                        .child(
                            Label::new(t!("agent_ui.task_agents.enabled")).size(LabelSize::Small),
                        ),
                )
                .child(
                    IconButton::new("delete-agent", IconName::Trash)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(t!("agent_ui.task_agents.delete.tooltip")))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.delete_selected(window, cx)),
                        ),
                )
            })
            .child(
                secondary_button(
                    "run-agent",
                    Some((IconName::PlayOutlined, Color::Default)),
                    t!("agent_ui.task_agents.run"),
                    true,
                    cx,
                )
                .tooltip(Tooltip::text(t!("agent_ui.task_agents.run_tooltip")))
                .on_click(cx.listener(|this, _, window, cx| this.run_selected(window, cx))),
            )
            .map(|this| {
                if editable {
                    this.child(
                        primary_button(
                            "save-agent",
                            None,
                            t!("agent_ui.task_agents.save"),
                            Some(KeyBinding::for_action_in(
                                &workspace::Save { save_intent: None },
                                &self.focus_handle,
                                cx,
                            )),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.save(cx))),
                    )
                } else {
                    this.child(
                        primary_button(
                            "copy-agent",
                            None,
                            t!("agent_ui.task_agents.copy_to_project"),
                            None,
                            cx,
                        )
                        .tooltip(Tooltip::text(t!(
                            "agent_ui.task_agents.copy_to_project_tooltip"
                        )))
                        .on_click(cx.listener(|this, _, _window, cx| this.copy_to_project(cx))),
                    )
                }
            })
    }

    /// The file relative to its project, or under `~` for the user's own agents.
    fn display_path(&self, path: &std::path::Path, cx: &App) -> String {
        for (_, root) in self.worktree_roots(cx) {
            if let Ok(relative) = path.strip_prefix(&root) {
                return relative.display().to_string();
            }
        }
        match path.strip_prefix(util::paths::home_dir()) {
            Ok(relative) => format!("~/{}", relative.display()),
            Err(_) => path.display().to_string(),
        }
    }

    fn render_call_card(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return div();
        };
        let using = self.flows_using(&draft.agent.command);
        let flow_names = using
            .iter()
            .filter_map(|index| self.flows.get(*index))
            .map(|flow| flow.name.clone())
            .collect::<Vec<_>>();
        let flows_text = match flow_names.as_slice() {
            [] => t!("agent_ui.task_agents.call.flows_none"),
            [only] => t!("agent_ui.task_agents.call.flows_one", flow = only),
            [rest @ .., last] => t!(
                "agent_ui.task_agents.call.flows_many",
                flows = rest.join(", "),
                last = last
            ),
        };
        let first_flow = using.first().copied();
        let editable = draft.agent.source.is_editable();
        let triggers = draft.agent.triggers.clone();
        let trigger_switch = |id: &'static str,
                              on: bool,
                              toggle: fn(&mut task_agents::TaskAgentTriggers),
                              cx: &mut Context<Self>| {
            toggle_switch(id, on, editable, cx)
                .when(editable, |this| {
                    this.on_click(cx.listener(move |this, _, _window, cx| {
                        if let Some(draft) = this.draft.as_mut() {
                            toggle(&mut draft.agent.triggers);
                            cx.notify();
                        }
                    }))
                })
                .into_any_element()
        };
        let shortcut_focused = draft.shortcut.focus_handle(cx).is_focused(window);
        div().child(
            Self::render_card(t!("agent_ui.task_agents.call.title"), None, cx)
                .child(card_row(
                    Some(IconName::AtSign),
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(t!("agent_ui.task_agents.call.composer"))
                                .weight(FontWeight::MEDIUM),
                        )
                        .child(
                            h_flex()
                                .h(px(22.))
                                .px_1p5()
                                .gap_0p5()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .child(
                                    Label::new("/")
                                        .size(LabelSize::Small)
                                        .color(Color::Accent)
                                        .buffer_font(cx),
                                )
                                .child(div().w(px(120.)).child(draft.command.clone())),
                        )
                        .into_any_element(),
                    t!("agent_ui.task_agents.call.composer_hint"),
                    Some(trigger_switch(
                        "trigger-composer",
                        triggers.composer,
                        |triggers| triggers.composer = !triggers.composer,
                        cx,
                    )),
                    false,
                    cx,
                ))
                .child(card_row(
                    Some(IconName::PullRequest),
                    Label::new(t!("agent_ui.task_agents.call.repo_dock"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.call.repo_dock_hint"),
                    Some(trigger_switch(
                        "trigger-repo-dock",
                        triggers.repo_dock,
                        |triggers| triggers.repo_dock = !triggers.repo_dock,
                        cx,
                    )),
                    false,
                    cx,
                ))
                .child(card_row(
                    Some(IconName::Keyboard),
                    Label::new(t!("agent_ui.task_agents.call.shortcut"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.call.shortcut_hint"),
                    Some(
                        div()
                            .w(px(150.))
                            .child(
                                text_field(shortcut_focused, cx)
                                    .h(px(26.))
                                    .child(div().flex_1().child(draft.shortcut.clone())),
                            )
                            .into_any_element(),
                    ),
                    false,
                    cx,
                ))
                .child(card_row(
                    Some(IconName::ListTree),
                    Label::new(t!("agent_ui.task_agents.call.flows"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    flows_text,
                    first_flow.map(|index| {
                        Button::new("agent-flows", t!("agent_ui.task_agents.call.view"))
                            .style(ButtonStyle::Transparent)
                            .color(Color::Accent)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.segment = Segment::Flows;
                                this.selected_flow = Some(index);
                                cx.notify();
                            }))
                            .into_any_element()
                    }),
                    true,
                    cx,
                )),
        )
    }

    fn render_model_card(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return div();
        };
        let agent = draft.agent.clone();
        let editable = agent.source.is_editable();
        let registered = toolkits(cx);

        let mut toolkit_chips = h_flex().flex_wrap().gap_1p5();
        for toolkit in registered.iter() {
            let on = agent.toolkits.iter().any(|id| id == toolkit.id.as_ref());
            let toolkit_id = toolkit.id.to_string();
            toolkit_chips = toolkit_chips.child(
                chip(
                    SharedString::from(format!("toolkit-{toolkit_id}")),
                    toolkit.name.clone(),
                    if on { ChipLook::On } else { ChipLook::Off },
                    false,
                    cx,
                )
                .tooltip(Tooltip::text(toolkit.description.clone()))
                .when(editable, |this| {
                    this.on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_toolkit(&toolkit_id, cx);
                    }))
                }),
            );
        }

        let configured = self
            .project
            .read(cx)
            .context_server_store()
            .read(cx)
            .configured_server_ids()
            .into_iter()
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>();
        let mut servers = configured.clone();
        for server in &agent.context_servers {
            if !servers.contains(server) {
                servers.push(server.clone());
            }
        }
        let mut server_chips = h_flex().flex_wrap().gap_1p5();
        if servers.is_empty() {
            server_chips = server_chips.child(
                Label::new(t!("agent_ui.task_agents.model_card.no_mcp"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for server in servers {
            let on = agent.context_servers.contains(&server);
            let missing = !configured.contains(&server);
            let server_id = server.clone();
            server_chips = server_chips.child(
                chip(
                    SharedString::from(format!("mcp-{server}")),
                    server.into(),
                    if missing {
                        ChipLook::Never
                    } else if on {
                        ChipLook::On
                    } else {
                        ChipLook::Off
                    },
                    true,
                    cx,
                )
                .when(missing, |this| {
                    this.tooltip(Tooltip::text(t!(
                        "agent_ui.task_agents.model_card.mcp_missing"
                    )))
                })
                .when(editable, |this| {
                    this.on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_context_server(&server_id, cx);
                    }))
                }),
            );
        }

        let mode_of = |key: &str| agent.permissions.get(key).and_then(|rules| rules.default);
        let all_mode = mode_of(ALL_TOOLS_PERMISSION_KEY);
        let mut permission_chips = h_flex().flex_wrap().gap_1p5();
        let mut permission_tools: Vec<(String, SharedString, SharedString, Option<ToolAccess>)> =
            builtin_permission_tools()
                .into_iter()
                .map(|(name, label, title)| (name.to_string(), label, title, None))
                .collect();
        for toolkit in registered.iter() {
            if !agent.toolkits.iter().any(|id| id == toolkit.id.as_ref()) {
                continue;
            }
            for tool in &toolkit.tools {
                permission_tools.push((
                    tool.name.to_string(),
                    tool.title.to_lowercase().into(),
                    tool.description.clone(),
                    Some(tool.access),
                ));
            }
        }
        for (name, label, title, access) in permission_tools {
            permission_chips = permission_chips.child(self.tool_permission_chip(
                &agent,
                &registered,
                name,
                label,
                title,
                access,
                editable,
                cx,
            ));
        }

        let dropdown = |element: AnyElement| div().w(px(210.)).flex().justify_end().child(element);
        div().child(
            Self::render_card(t!("agent_ui.task_agents.model_card.title"), None, cx)
                .child(card_row(
                    None,
                    Label::new(t!("agent_ui.task_agents.model_card.model"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.model_card.model_hint"),
                    Some(
                        dropdown(self.render_model_dropdown(window, cx).into_any_element())
                            .into_any_element(),
                    ),
                    false,
                    cx,
                ))
                .child(card_row(
                    None,
                    Label::new(t!("agent_ui.task_agents.model_card.tools"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.model_card.tools_hint"),
                    Some(
                        dropdown(self.render_profile_dropdown(window, cx).into_any_element())
                            .into_any_element(),
                    ),
                    false,
                    cx,
                ))
                .child(card_row(
                    None,
                    Label::new(t!("agent_ui.task_agents.model_card.subagents"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.model_card.subagents_hint"),
                    Some(
                        dropdown(
                            self.render_agent_subagent_model_dropdown(window, cx)
                                .into_any_element(),
                        )
                        .into_any_element(),
                    ),
                    false,
                    cx,
                ))
                .child(card_row(
                    None,
                    Label::new(t!("agent_ui.task_agents.model_card.subagent_effort"))
                        .weight(FontWeight::MEDIUM)
                        .into_any_element(),
                    t!("agent_ui.task_agents.model_card.subagent_effort_hint"),
                    Some(
                        dropdown(
                            self.render_subagent_effort_dropdown(window, cx)
                                .into_any_element(),
                        )
                        .into_any_element(),
                    ),
                    false,
                    cx,
                ))
                .child(chip_row(
                    t!("agent_ui.task_agents.model_card.toolkits"),
                    t!("agent_ui.task_agents.model_card.toolkits_hint"),
                    None,
                    toolkit_chips,
                    false,
                    cx,
                ))
                .child(chip_row(
                    t!("agent_ui.task_agents.model_card.mcps"),
                    t!("agent_ui.task_agents.model_card.mcps_hint"),
                    None,
                    server_chips,
                    false,
                    cx,
                ))
                .child(chip_row(
                    t!("agent_ui.task_agents.model_card.permissions"),
                    t!("agent_ui.task_agents.model_card.permissions_hint"),
                    Some(
                        Button::new(
                            "builtin-perm-all",
                            t!(
                                "agent_ui.task_agents.model_card.permissions_all",
                                mode = permission_label(all_mode)
                            ),
                        )
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Subtle)
                        .disabled(!editable)
                        .tooltip(Tooltip::text(t!(
                            "agent_ui.task_agents.model_card.permissions_all_tooltip"
                        )))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.cycle_permission(ALL_TOOLS_PERMISSION_KEY, cx);
                        }))
                        .into_any_element(),
                    ),
                    permission_chips,
                    true,
                    cx,
                )),
        )
    }

    fn render_io_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return div();
        };
        let editable = draft.agent.source.is_editable();
        let row = |list: IoList, cx: &mut Context<Self>| {
            let (values, title, hint) = match list {
                IoList::Takes => (
                    &draft.agent.takes,
                    t!("agent_ui.task_agents.io.takes"),
                    t!("agent_ui.task_agents.io.takes_hint"),
                ),
                IoList::Gives => (
                    &draft.agent.gives,
                    t!("agent_ui.task_agents.io.gives"),
                    t!("agent_ui.task_agents.io.gives_hint"),
                ),
            };
            let mut chips = h_flex().flex_wrap().gap_1p5();
            for (index, value) in values.iter().enumerate() {
                chips = chips.child(
                    h_flex()
                        .h(px(24.))
                        .pl_2()
                        .pr_0p5()
                        .gap_0p5()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(value.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        )
                        .when(editable, |this| {
                            this.child(
                                IconButton::new(
                                    SharedString::from(format!("io-remove-{list:?}-{index}")),
                                    IconName::Close,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| {
                                        this.remove_io(list, index, cx);
                                    },
                                )),
                            )
                        }),
                );
            }
            match &self.adding_io {
                Some((adding, editor)) if *adding == list => {
                    chips = chips.child(
                        div()
                            .w(px(140.))
                            .h(px(24.))
                            .px_1p5()
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(cx.theme().colors().border_focused)
                            .bg(cx.theme().colors().editor_background)
                            .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                                this.commit_io(cx);
                            }))
                            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| {
                                this.adding_io = None;
                                cx.notify();
                            }))
                            .child(editor.clone()),
                    );
                }
                _ if editable => {
                    chips = chips.child(
                        pill_button(
                            SharedString::from(format!("io-add-{list:?}")),
                            Some(IconName::Plus),
                            t!("agent_ui.task_agents.io.add"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.start_adding_io(list, window, cx);
                            },
                        )),
                    );
                }
                _ => {}
            }
            (title, hint, chips)
        };
        let (takes_title, takes_hint, takes_chips) = row(IoList::Takes, cx);
        let (gives_title, gives_hint, gives_chips) = row(IoList::Gives, cx);
        div().child(
            Self::render_card(t!("agent_ui.task_agents.io.title"), None, cx)
                .child(chip_row(
                    takes_title,
                    takes_hint,
                    None,
                    takes_chips,
                    false,
                    cx,
                ))
                .child(chip_row(
                    gives_title,
                    gives_hint,
                    None,
                    gives_chips,
                    true,
                    cx,
                )),
        )
    }

    fn render_instructions_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return div();
        };
        let lines = draft
            .instructions
            .read(cx)
            .buffer()
            .read(cx)
            .read(cx)
            .max_point()
            .row
            + 1;
        div().size_full().child(
            Self::render_card(
                t!("agent_ui.task_agents.instructions.title"),
                Some(
                    Button::new(
                        "open-agent-file",
                        t!("agent_ui.task_agents.instructions.open_file"),
                    )
                    .style(ButtonStyle::Transparent)
                    .color(Color::Accent)
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(IconName::FileMarkdown)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.open_file(window, cx)))
                    .into_any_element(),
                ),
                cx,
            )
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .py_2()
                    .bg(cx.theme().colors().editor_background)
                    .child(draft.instructions.clone()),
            )
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_none()
                    .px_3p5()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("agent_ui.task_agents.instructions.footer"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(if lines == 1 {
                            t!(
                                "agent_ui.task_agents.instructions.line_count.one",
                                count = lines
                            )
                        } else {
                            t!(
                                "agent_ui.task_agents.instructions.line_count.other",
                                count = lines
                            )
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx),
                    ),
            ),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoList {
    Takes,
    Gives,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChipLook {
    On,
    Off,
    Ask,
    Never,
}

fn stable_hash(text: &str) -> usize {
    text.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    })
}

/// The template's icon for the agents made from one; any other agent gets one from its id, so
/// it keeps the same tile across sessions.
fn agent_icon(agent: &TaskAgent) -> IconName {
    if let Some(template) = TEMPLATES
        .iter()
        .find(|template| template.command == agent.command)
    {
        return template.icon;
    }
    const ICONS: &[IconName] = &[
        IconName::UserGroup,
        IconName::Sparkle,
        IconName::Book,
        IconName::Code,
        IconName::Terminal,
        IconName::ListTodo,
        IconName::FileTextOutlined,
        IconName::History,
    ];
    ICONS
        .get(stable_hash(&agent.id) % ICONS.len())
        .copied()
        .unwrap_or(IconName::UserGroup)
}

fn agent_tile(agent: &TaskAgent, size: Pixels, icon_size: IconSize, cx: &App) -> impl IntoElement {
    let color = cx
        .theme()
        .players()
        .color_for_participant(stable_hash(&agent.id) as u32)
        .cursor;
    div()
        .size(size)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(size / 5.6)
        .bg(color.opacity(0.16))
        .child(
            Icon::new(agent_icon(agent))
                .size(icon_size)
                .color(Color::Custom(color)),
        )
}

fn list_badge(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px_2()
        .py_px()
        .rounded_full()
        .bg(cx.theme().colors().element_hover)
        .child(
            Label::new(text.into())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

fn card_row(
    icon: Option<IconName>,
    title: AnyElement,
    hint: impl Into<SharedString>,
    trailing: Option<AnyElement>,
    last: bool,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .px_3p5()
        .py_2p5()
        .gap_2p5()
        .when(!last, |this| {
            this.border_b_1().border_color(cx.theme().colors().border)
        })
        .when_some(icon, |this, icon| {
            this.child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        })
        .child(
            v_flex().flex_1().min_w_0().gap_0p5().child(title).child(
                Label::new(hint.into())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
        .children(trailing)
}

fn chip_row(
    title: SharedString,
    hint: SharedString,
    action: Option<AnyElement>,
    chips: Div,
    last: bool,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .w_full()
        .px_3p5()
        .py_2p5()
        .gap_2()
        .when(!last, |this| {
            this.border_b_1().border_color(cx.theme().colors().border)
        })
        .child(
            h_flex()
                .gap_2()
                .child(Label::new(title).weight(FontWeight::MEDIUM))
                .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
                .child(div().flex_1())
                .children(action),
        )
        .child(chips)
}

fn chip(
    id: SharedString,
    label: SharedString,
    look: ChipLook,
    monospace: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    let (background, border, label_color) = match look {
        ChipLook::On => (colors.element_hover, colors.border, Color::Default),
        ChipLook::Off => (
            colors.editor_background,
            colors.border_variant,
            Color::Muted,
        ),
        ChipLook::Ask => (
            status.warning.opacity(0.12),
            status.warning.opacity(0.4),
            Color::Warning,
        ),
        ChipLook::Never => (
            colors.editor_background,
            colors.border_variant,
            Color::Muted,
        ),
    };
    h_flex()
        .id(id)
        .h(px(24.))
        .px_2()
        .gap_1p5()
        .rounded(px(5.))
        .border_1()
        .border_color(border)
        .bg(background)
        .cursor_pointer()
        .hover(|this| this.border_color(colors.border_focused))
        .map(|this| match look {
            ChipLook::Ask => this.child(
                Icon::new(IconName::Lock)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            ),
            _ => this.child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(match look {
                        ChipLook::On => status.success,
                        ChipLook::Never => status.error,
                        _ => colors.text_disabled,
                    }),
            ),
        })
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(label_color)
                .when(look == ChipLook::Never, |this| this.strikethrough())
                .when(monospace, |this| this.buffer_font(cx)),
        )
}

impl TaskAgentsView {
    fn toggle_context_server(&mut self, server: &str, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let servers = &mut draft.agent.context_servers;
        if let Some(position) = servers.iter().position(|id| id == server) {
            servers.remove(position);
        } else {
            servers.push(server.to_string());
        }
        cx.notify();
    }

    fn start_adding_io(&mut self, list: IoList, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("agent_ui.task_agents.io.placeholder"), window, cx);
            editor
        });
        editor.focus_handle(cx).focus(window, cx);
        self.adding_io = Some((list, editor));
        cx.notify();
    }

    fn commit_io(&mut self, cx: &mut Context<Self>) {
        let Some((list, editor)) = self.adding_io.take() else {
            return;
        };
        let value = editor.read(cx).text(cx).trim().to_string();
        if let Some(draft) = self.draft.as_mut()
            && !value.is_empty()
        {
            let values = match list {
                IoList::Takes => &mut draft.agent.takes,
                IoList::Gives => &mut draft.agent.gives,
            };
            if !values.contains(&value) {
                values.push(value);
            }
        }
        cx.notify();
    }

    fn remove_io(&mut self, list: IoList, index: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let values = match list {
            IoList::Takes => &mut draft.agent.takes,
            IoList::Gives => &mut draft.agent.gives,
        };
        if index < values.len() {
            values.remove(index);
        }
        cx.notify();
    }
}

/// A dropdown of every model of the connected providers, grouped by provider. `current` and
/// the value given to `on_select` are `provider/model`; `None` is the `empty_label` entry.
pub(crate) fn model_dropdown(
    id: &'static str,
    current: Option<String>,
    empty_label: impl Into<SharedString>,
    on_select: impl Fn(Option<String>, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> DropdownMenu {
    let (label, menu) = model_menu(current, empty_label, on_select, window, cx);
    selector(id, IconName::Sparkle, label, menu)
}

/// The chosen model's name and the menu of the signed-in providers' models.
pub(crate) fn model_menu(
    current: Option<String>,
    empty_label: impl Into<SharedString>,
    on_select: impl Fn(Option<String>, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> (SharedString, Entity<ContextMenu>) {
    let empty_label: SharedString = empty_label.into();
    let registry = LanguageModelRegistry::read_global(cx);
    let mut groups: Vec<(SharedString, Vec<(String, SharedString)>)> = Vec::new();
    for provider in registry.visible_providers() {
        if !provider.is_authenticated(cx) {
            continue;
        }
        let models: Vec<_> = provider
            .provided_models(cx)
            .into_iter()
            .map(|model| {
                (
                    format!("{}/{}", model.provider_id().0, model.id().0),
                    model.name().0,
                )
            })
            .collect();
        if !models.is_empty() {
            groups.push((provider.name().0, models));
        }
    }
    let label: SharedString = match current.as_deref() {
        None => empty_label.clone(),
        Some(reference) => groups
            .iter()
            .find_map(|(_, models)| {
                models
                    .iter()
                    .find(|(id, _)| id == reference)
                    .map(|(_, name)| name.clone())
            })
            // A model whose provider is signed out still shows, so it is not lost silently.
            .unwrap_or_else(|| t!("agent_ui.task_agents.model.unavailable", model = reference)),
    };
    let on_select = Rc::new(on_select);
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        let keep = on_select.clone();
        menu = menu.toggleable_entry(
            empty_label.clone(),
            current.is_none(),
            IconPosition::Start,
            None,
            move |_window, cx| keep(None, cx),
        );
        if groups.is_empty() {
            menu = menu
                .separator()
                .header(t!("agent_ui.task_agents.model.no_provider"));
        }
        for (provider, models) in groups {
            menu = menu.separator().header(provider);
            for (reference, name) in models {
                let on_select = on_select.clone();
                let toggled = current.as_deref() == Some(reference.as_str());
                menu = menu.toggleable_entry(
                    name,
                    toggled,
                    IconPosition::Start,
                    None,
                    move |_window, cx| on_select(Some(reference.clone()), cx),
                );
            }
        }
        menu
    });
    (label, menu)
}

/// The trigger of the tab's choices: what is chosen, with an icon for what kind of choice it is,
/// sized to its content like the Agent panel's own profile and model selectors.
pub(crate) fn selector(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    menu: Entity<ContextMenu>,
) -> DropdownMenu {
    DropdownMenu::new_with_element(
        id,
        h_flex()
            .gap_1p5()
            .min_w_0()
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                Label::new(label.into())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate(),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element(),
        menu,
    )
    .no_chevron()
    .style(ui::DropdownStyle::Outlined)
    .trigger_size(ButtonSize::Compact)
    .attach(gpui::Anchor::BottomRight)
}

impl TaskAgentsView {
    fn apply_pending_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.pending_selection.take() {
            self.select(index, window, cx);
        }
    }
}

impl Render for TaskAgentsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending_selection(window, cx);
        let flows_segment = self.segment == Segment::Flows;
        h_flex()
            .key_context("TaskAgents")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &workspace::Save, _window, cx| {
                if this.segment == Segment::Agents {
                    this.save(cx);
                }
            }))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w(px(300.))
                    .h_full()
                    .flex_none()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(self.render_segment(cx))
                    .child(
                        div().p_2p5().child(
                            h_flex()
                                .h(px(30.))
                                .gap_1p5()
                                .px_2()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1().min_w_0().child(self.search.clone())),
                        ),
                    )
                    .child(div().flex_1().min_h_0().child(if flows_segment {
                        self.render_flow_list(cx).into_any_element()
                    } else {
                        self.render_list(cx).into_any_element()
                    }))
                    .child(
                        div().px_2p5().py_2().child(
                            h_flex()
                                .id("new-task-agent")
                                .h(px(34.))
                                .gap_2()
                                .px_2p5()
                                .rounded_lg()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().colors().element_hover))
                                .child(
                                    Icon::new(IconName::Plus)
                                        .size(IconSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(if flows_segment {
                                        t!("agent_ui.task_agents.new_flow")
                                    } else {
                                        t!("agent_ui.task_agents.new_agent")
                                    })
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Accent),
                                )
                                .child(div().flex_1())
                                .child(
                                    Label::new(t!("agent_ui.task_agents.new_hint"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.segment == Segment::Flows {
                                        this.workspace
                                            .update(cx, |workspace, cx| {
                                                crate::flows_view::new_flow(workspace, window, cx)
                                            })
                                            .log_err();
                                    } else {
                                        this.open_new_agent_modal(window, cx)
                                    }
                                })),
                        ),
                    )
                    .child(
                        div().p_2p5().child(
                            v_flex()
                                .p_2p5()
                                .gap_1()
                                .rounded_lg()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .child(
                                    Label::new(t!("agent_ui.task_agents.repo.title"))
                                        .size(LabelSize::XSmall)
                                        .weight(FontWeight::SEMIBOLD)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(if flows_segment {
                                        t!(
                                            "agent_ui.task_agents.repo.flows",
                                            directory = flows::PROJECT_FLOWS_DIR
                                        )
                                    } else {
                                        t!(
                                            "agent_ui.task_agents.repo.agents",
                                            directory = PROJECT_AGENTS_DIR
                                        )
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        ),
                    ),
            )
            .child(div().flex_1().min_w_0().h_full().child(if flows_segment {
                self.render_flow_detail(cx)
            } else {
                self.render_detail(window, cx)
            }))
    }
}

impl Focusable for TaskAgentsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TaskAgentsView {}

impl Item for TaskAgentsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        t!("agent_ui.task_agents.segment.agents")
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::UserGroup).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// The status bar's toolkit button for the Agentes tab.
pub struct TaskAgentsToolkitButton {
    workspace: WeakEntity<Workspace>,
}

impl TaskAgentsToolkitButton {
    pub fn new(workspace: &Workspace, _cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
        }
    }
}

impl Render for TaskAgentsToolkitButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The status bar lists it in the toolkit menu instead; see `toolkit_entry`.
        gpui::Empty
    }
}

impl StatusItemView for TaskAgentsToolkitButton {
    fn toolkit_entry(&self, cx: &App) -> Option<ToolkitEntry> {
        let is_open = self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .is_some_and(|item| item.downcast::<TaskAgentsView>().is_some())
        });
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            icon: IconName::UserGroup,
            label: t!("agent_ui.task_agents.segment.agents"),
            is_open,
            action: Box::new(OpenTaskAgents),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| open(workspace, window, cx))
                    .log_err();
            }),
        })
    }

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
