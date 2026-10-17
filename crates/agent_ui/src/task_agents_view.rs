//! The Agentes tab: the task agents of the project and of the user, each an `.md` file, with a
//! form for its command, profile, model, toolkits and per-tool permissions, and its instructions.

mod new_agent_modal;

use crate::{AgentInitialContent, AgentPanel, AgentThreadSource};
use agent::AgentTool as _;
use agent_client_protocol::schema::v2 as acp;
use agent_settings::{AgentProfile, AgentSettings};
use anyhow::Result;
use editor::Editor;
use fs::Fs;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    Window, actions,
};
use language_model::LanguageModelRegistry;
use new_agent_modal::NewAgentModal;
use project::Project;
use settings::{
    LanguageModelProviderSetting, LanguageModelSelection, Settings as _, SettingsStore,
    update_settings_file,
};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use task_agents::{
    ALL_TOOLS_PERMISSION_KEY, AuthoringCatalog, PROJECT_AGENTS_DIR, PermissionMode, ProfileSummary,
    TaskAgent, TaskAgentSource, ToolAccess, ToolPermissionRules, ToolkitSummary, command_slug,
    load_task_agents, personal_agents_dir, toolkits,
};
use ui::{Chip, ContextMenu, Divider, DropdownMenu, Switch, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Workspace,
    dock::StatusBarButton,
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
        None => ("Novo agente".to_string(), "novo-agente".to_string()),
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
        instructions: template
            .map(|template| template.instructions.to_string())
            .unwrap_or_else(|| "Descreva aqui o que o agente faz, passo a passo.".to_string()),
        source: TaskAgentSource::Personal,
        file_path: PathBuf::new(),
    }
}

/// How many times the model may fix its own file before what it wrote is saved as is.
const MAX_AUTHORING_REPAIRS: usize = 2;

fn source_label(source: &TaskAgentSource) -> SharedString {
    match source {
        TaskAgentSource::Project { worktree_root_name } => {
            format!("{worktree_root_name}/{PROJECT_AGENTS_DIR}").into()
        }
        TaskAgentSource::Personal => "só para mim".into(),
        TaskAgentSource::Imported { worktree_root_name } => {
            format!("{worktree_root_name}/.claude/agents").into()
        }
    }
}

fn permission_label(mode: Option<PermissionMode>) -> &'static str {
    match mode {
        None => "padrão",
        Some(PermissionMode::Allow) => "livre",
        Some(PermissionMode::Confirm) => "pede",
        Some(PermissionMode::Deny) => "nunca",
    }
}

/// The Zed Agent's tools that ask before acting, with what they do.
const BUILTIN_PERMISSION_TOOLS: &[(&str, &str)] = &[
    (agent::TerminalTool::NAME, "Roda comandos no terminal"),
    (agent::EditFileTool::NAME, "Edita arquivos"),
    (agent::WriteFileTool::NAME, "Cria e sobrescreve arquivos"),
    (agent::DeletePathTool::NAME, "Apaga arquivos e pastas"),
    (agent::MovePathTool::NAME, "Move e renomeia arquivos"),
    (agent::CopyPathTool::NAME, "Copia arquivos"),
    (agent::CreateDirectoryTool::NAME, "Cria pastas"),
    (agent::FetchTool::NAME, "Busca uma URL"),
    (agent::WebSearchTool::NAME, "Pesquisa na web"),
    (agent::SkillTool::NAME, "Roda uma skill"),
];

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
    name: Entity<Editor>,
    command: Entity<Editor>,
    description: Entity<Editor>,
    instructions: Entity<Editor>,
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
            editor.set_placeholder_text("Buscar agentes…", window, cx);
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
                       label: &'static str,
                       value: Segment,
                       current: Segment,
                       cx: &mut Context<Self>| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .full_width()
                .toggle_state(value == current)
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.segment = value;
                    cx.notify();
                }))
        };
        div().px_2().pt_2().child(
            h_flex()
                .p_0p5()
                .gap_0p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
                .child(segment(
                    "segment-agents",
                    "Agentes",
                    Segment::Agents,
                    self.segment,
                    cx,
                ))
                .child(segment(
                    "segment-flows",
                    "Esteiras",
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
        let sections: [(&str, fn(&flows::FlowSource) -> bool); 2] = [
            ("DO PROJETO", |source| {
                matches!(source, flows::FlowSource::Project { .. })
            }),
            ("SÓ PARA MIM", |source| {
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
                                        "/{} · {steps} passo{}",
                                        flow.id,
                                        if steps == 1 { "" } else { "s" }
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
                    .child(Label::new("Nenhuma esteira ainda."))
                    .child(
                        Label::new("Monte uma com os seus agentes, de um modelo ou pedindo à IA.")
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
                .child(Icon::new(IconName::ListTree).size(IconSize::XLarge).color(Color::Muted))
                .child(Label::new("Esteiras de agentes").size(LabelSize::Large))
                .child(
                    Label::new("Cada esteira é um .flow.md com os passos: seus agentes, portões e comandos. Rode com /esteira no composer do Agent.")
                        .color(Color::Muted),
                )
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
                        Button::new("open-flow", "Editar")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let path = path.clone();
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        crate::flows_view::open_flow_path(
                                            workspace, path, window, cx,
                                        )
                                    })
                                    .log_err();
                            })),
                    )
                    .child(
                        Button::new("run-flow-from-list", "Rodar")
                            .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                            .style(ButtonStyle::Filled)
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
                    Label::new(format!("Exemplo: {}", flow.input))
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
            |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                let text = text.to_string();
                let placeholder = placeholder.to_string();
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(&placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                })
            };
        let name = text_editor(&agent.name, "Nome do agente", window, cx);
        let command = text_editor(&agent.command, "comando", window, cx);
        let description = text_editor(
            &agent.description,
            "O que ele faz, em uma frase",
            window,
            cx,
        );
        let instructions = {
            let text = agent.instructions.clone();
            cx.new(|cx| {
                let mut editor = Editor::auto_height(12, 40, window, cx);
                editor.set_placeholder_text("Instruções em Markdown", window, cx);
                editor.set_text(text, window, cx);
                editor.set_soft_wrap();
                editor
            })
        };
        self.selected = Some(index);
        self.draft = Some(Draft {
            agent,
            name,
            command,
            description,
            instructions,
        });
        self.status = None;
        cx.notify();
    }

    fn collect_draft(&self, cx: &App) -> Option<TaskAgent> {
        let draft = self.draft.as_ref()?;
        let mut agent = draft.agent.clone();
        agent.name = draft.name.read(cx).text(cx).trim().to_string();
        agent.command = command_slug(&draft.command.read(cx).text(cx));
        agent.description = draft.description.read(cx).text(cx).trim().to_string();
        agent.instructions = draft.instructions.read(cx).text(cx);
        Some(agent)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = self.collect_draft(cx) else {
            return;
        };
        if agent.name.is_empty() || agent.command.is_empty() {
            self.status = Some("Dê um nome e um comando ao agente.".into());
            cx.notify();
            return;
        }
        if [&agent.model, &agent.subagent_model]
            .into_iter()
            .flatten()
            .any(|model| task_agents::ModelReference::parse(model).is_none())
        {
            self.status = Some(
                "O modelo precisa ser provedor/modelo, ex.: anthropic/claude-opus-5-5.".into(),
            );
            cx.notify();
            return;
        }
        let markdown = match agent.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                self.status = Some(format!("Não foi possível salvar: {error}").into());
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
                        this.status = Some(format!("Salvo em {}", path.display()).into());
                        this.refresh_agent_commands(cx);
                        this.reload(Some(path), cx);
                    }
                    Err(error) => {
                        this.status = Some(format!("Não foi possível salvar: {error}").into());
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
                    .unwrap_or_else(|| "projeto".into()),
            }
        };
        agent.file_path = path.clone();
        let markdown = match agent.to_markdown() {
            Ok(markdown) => markdown,
            Err(error) => {
                self.status = Some(format!("Não foi possível criar: {error}").into());
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
                        format!("Criado com IA em {}. Revise antes de usar.", path.display()).into()
                    }
                    Ok(()) => format!("Criado em {}", path.display()).into(),
                    Err(error) => format!("Não foi possível criar: {error}").into(),
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
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Apagar o agente {}?", agent.name),
            Some(&format!(
                "{} vai para a Lixeira.",
                agent.file_path.display()
            )),
            &["Apagar", "Cancelar"],
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
                    Ok(()) => this.status = Some(format!("{} apagado.", agent.name).into()),
                    Err(error) => {
                        this.status = Some(format!("Não foi possível apagar: {error}").into())
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
            worktree_root_name: "projeto".into(),
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

    fn set_profile(&mut self, profile: Option<String>, cx: &mut Context<Self>) {
        if let Some(draft) = self.draft.as_mut() {
            draft.agent.profile = profile;
            cx.notify();
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sections: [(&str, fn(&TaskAgentSource) -> bool); 3] = [
            ("DO PROJETO", |source| {
                matches!(source, TaskAgentSource::Project { .. })
            }),
            ("SÓ PARA MIM", |source| {
                matches!(source, TaskAgentSource::Personal)
            }),
            ("IMPORTADOS", |source| {
                matches!(source, TaskAgentSource::Imported { .. })
            }),
        ];
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let matches = |agent: &TaskAgent| {
            query.is_empty()
                || agent.name.to_lowercase().contains(&query)
                || agent.command.to_lowercase().contains(&query)
                || agent.description.to_lowercase().contains(&query)
        };
        let mut list = v_flex()
            .id("task-agents-list")
            .size_full()
            .gap_1()
            .p_2()
            .overflow_y_scroll();
        for (title, belongs) in sections {
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
                div().pt_2().px_1().child(
                    Label::new(title)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
            for (index, agent) in entries {
                let selected = self.selected == Some(index);
                list = list.child(
                    h_flex()
                        .id(("task-agent", index))
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .child(Icon::new(IconName::UserGroup).size(IconSize::Small).color(
                            if selected {
                                Color::Accent
                            } else {
                                Color::Muted
                            },
                        ))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(Label::new(agent.name.clone()).single_line().truncate())
                                .child(
                                    Label::new(format!("/{}", agent.command))
                                        .size(LabelSize::Small)
                                        .color(Color::Accent)
                                        .buffer_font(cx),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(index, window, cx);
                        })),
                );
            }
        }
        if self.agents.is_empty() {
            list = list.child(
                v_flex().p_2().gap_1().child(Label::new("Nenhum agente ainda.")).child(
                    Label::new(
                        "Crie um a partir de um modelo, ou peça ao agente para criar um para você.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            );
        }
        for error in &self.load_errors {
            list = list.child(
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
            );
        }
        list
    }

    fn render_field(
        label: &'static str,
        hint: Option<&'static str>,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(editor.clone()),
            )
            .when_some(hint, |this, hint| {
                this.child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
            })
    }

    fn render_card(title: &'static str, cx: &App) -> Div {
        v_flex()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(title)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
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
            .unwrap_or_else(|| "Perfil atual da thread".to_string());
        let view = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let keep_view = view.clone();
            menu = menu.entry("Perfil atual da thread", None, move |_window, cx| {
                keep_view
                    .update(cx, |view, cx| view.set_profile(None, cx))
                    .log_err();
            });
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
            "Modelo da thread",
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
            "Modelo padrão dos subagents",
            move |model, cx| {
                view.update(cx, |view, cx| view.set_subagent_model(model, cx))
                    .log_err();
            },
            window,
            cx,
        )
    }

    fn render_subagent_model_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = AgentSettings::get_global(cx)
            .subagent_model
            .as_ref()
            .map(|selection| format!("{}/{}", selection.provider.0, selection.model));
        let fs = self.fs.clone();
        model_dropdown(
            "subagent-default-model",
            current,
            "Mesmo modelo da thread",
            move |model, cx| {
                let selection = model
                    .as_deref()
                    .and_then(task_agents::ModelReference::parse)
                    .map(|reference| LanguageModelSelection {
                        provider: LanguageModelProviderSetting(reference.provider),
                        model: reference.model,
                        enable_thinking: false,
                        effort: None,
                        speed: None,
                    });
                update_settings_file(fs.clone(), cx, move |settings, _cx| {
                    settings.agent.get_or_insert_default().subagent_model = selection;
                });
            },
            window,
            cx,
        )
    }

    fn render_builtin_permissions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return v_flex();
        };
        let agent = &draft.agent;
        let editable = agent.source.is_editable();
        let mode_of = |key: &str| agent.permissions.get(key).and_then(|rules| rules.default);
        let all_mode = mode_of(ALL_TOOLS_PERMISSION_KEY);
        let mut tools_row = h_flex().flex_wrap().gap_1();
        for (name, title) in BUILTIN_PERMISSION_TOOLS {
            let tool_mode = mode_of(name);
            let effective = tool_mode.or(all_mode);
            let color = match effective {
                Some(PermissionMode::Allow) => Color::Success,
                Some(PermissionMode::Confirm) => Color::Warning,
                Some(PermissionMode::Deny) => Color::Error,
                None => Color::Muted,
            };
            let patterns = agent
                .permissions
                .get(*name)
                .map(|rules| rules.allow.len() + rules.confirm.len() + rules.deny.len())
                .unwrap_or(0);
            let tool_name = name.to_string();
            tools_row = tools_row.child(
                Button::new(SharedString::from(format!("builtin-perm-{name}")), *name)
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Outlined)
                    .start_icon(
                        Icon::new(IconName::Circle)
                            .size(IconSize::XSmall)
                            .color(color),
                    )
                    .disabled(!editable)
                    .tooltip(Tooltip::text(format!(
                        "{title} · {}{}{}. Clique para trocar (padrão → livre → pede → nunca).",
                        permission_label(effective),
                        if tool_mode.is_none() && all_mode.is_some() {
                            " (de todas as ferramentas)"
                        } else {
                            ""
                        },
                        if patterns > 0 {
                            format!(" · {patterns} regra(s) por regex no arquivo")
                        } else {
                            String::new()
                        },
                    )))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.cycle_permission(&tool_name, cx);
                    })),
            );
        }
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new("Todas as ferramentas")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new(
                            "builtin-perm-all",
                            format!("tudo: {}", permission_label(all_mode)),
                        )
                        .label_size(LabelSize::XSmall)
                        .style(ButtonStyle::Subtle)
                        .disabled(!editable)
                        .tooltip(Tooltip::text(
                            "Vale para toda ferramenta sem permissão própria, toolkits e MCP inclusive",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.cycle_permission(ALL_TOOLS_PERMISSION_KEY, cx);
                        })),
                    ),
            )
            .child(tools_row)
    }

    fn render_toolkits(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(draft) = self.draft.as_ref() else {
            return v_flex();
        };
        let agent = &draft.agent;
        let editable = agent.source.is_editable();
        let mut column = v_flex().gap_2();
        let all_mode = agent
            .permissions
            .get(ALL_TOOLS_PERMISSION_KEY)
            .and_then(|rules| rules.default);
        let registered = toolkits(cx);
        if registered.is_empty() {
            column = column.child(
                Label::new("Nenhum toolkit registrado nesta build.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for toolkit in registered {
            let on = agent.toolkits.iter().any(|id| id == toolkit.id.as_ref());
            let toolkit_id = toolkit.id.to_string();
            let toolkit_mode = agent
                .permissions
                .get(toolkit.id.as_ref())
                .and_then(|rules| rules.default);
            let mut tools_row = h_flex().flex_wrap().gap_1();
            for tool in &toolkit.tools {
                let tool_mode = agent
                    .permissions
                    .get(tool.name.as_ref())
                    .and_then(|rules| rules.default);
                let effective = tool_mode.or(toolkit_mode).or(all_mode);
                let color = match effective {
                    Some(PermissionMode::Allow) => Color::Success,
                    Some(PermissionMode::Confirm) => Color::Warning,
                    Some(PermissionMode::Deny) => Color::Error,
                    None if tool.access == ToolAccess::Read => Color::Success,
                    None => Color::Warning,
                };
                let default_label = if tool.access == ToolAccess::Read {
                    "livre"
                } else {
                    "pede"
                };
                let mode_label = match effective {
                    None => default_label,
                    mode => permission_label(mode),
                };
                let tool_name = tool.name.to_string();
                tools_row = tools_row.child(
                    Button::new(
                        SharedString::from(format!("perm-{}", tool.name)),
                        tool.name.clone(),
                    )
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Outlined)
                    .color(if on { Color::Default } else { Color::Muted })
                    .start_icon(
                        Icon::new(IconName::Circle)
                            .size(IconSize::XSmall)
                            .color(color),
                    )
                    .disabled(!editable)
                    .tooltip(Tooltip::text(format!(
                        "{} · {}{}. Clique para trocar (padrão → livre → pede → nunca).",
                        tool.title,
                        mode_label,
                        if tool_mode.is_none() {
                            " (padrão)"
                        } else {
                            ""
                        }
                    )))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.cycle_permission(&tool_name, cx);
                    })),
                );
            }
            let toggle_id = toolkit_id.clone();
            let toolkit_key = toolkit_id.clone();
            column = column.child(
                v_flex()
                    .gap_1p5()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::from_path(format!("icons/{}.svg", toolkit.icon))
                                    .size(IconSize::Small)
                                    .color(if on { Color::Accent } else { Color::Muted }),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Label::new(toolkit.name.clone()))
                                    .child(
                                        Label::new(toolkit.description.clone())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("toolkit-mode-{toolkit_id}")),
                                    format!("tudo: {}", permission_label(toolkit_mode)),
                                )
                                .label_size(LabelSize::XSmall)
                                .style(ButtonStyle::Subtle)
                                .disabled(!editable)
                                .tooltip(Tooltip::text(
                                    "Permissão de todas as ferramentas do toolkit, salvo as que têm a sua",
                                ))
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.cycle_permission(&toolkit_key, cx);
                                })),
                            )
                            .child(
                                Switch::new(
                                    SharedString::from(format!("toolkit-{toggle_id}")),
                                    if on { ToggleState::Selected } else { ToggleState::Unselected },
                                )
                                .disabled(!editable)
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.toggle_toolkit(&toggle_id, cx);
                                })),
                            ),
                    )
                    .child(tools_row),
            );
        }
        column
    }

    fn render_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(draft) = self.draft.as_ref() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(IconName::UserGroup).size(IconSize::XLarge).color(Color::Muted))
                .child(Label::new("Agentes para tarefas").size(LabelSize::Large))
                .child(
                    Label::new(
                        "Cada agente é um .md com instruções, ferramentas e permissões. Chame com /comando no composer do Agent.",
                    )
                    .color(Color::Muted),
                )
                .child(
                    Button::new("empty-new-agent", "Novo agente")
                        .style(ButtonStyle::Filled)
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_new_agent_modal(window, cx)
                        })),
                )
                .into_any_element();
        };
        let agent = &draft.agent;
        let editable = agent.source.is_editable();
        v_flex()
            .id("task-agent-detail")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(
                h_flex()
                    .gap_3()
                    .child(Icon::new(IconName::UserGroup).size(IconSize::Medium).color(Color::Accent))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(agent.name.clone()).size(LabelSize::Large))
                            .child(
                                Label::new(agent.file_path.display().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    )
                    .child(
                        Chip::new(source_label(&agent.source)).label_color(Color::Muted),
                    )
                    .child(
                        IconButton::new("open-agent-file", IconName::FileMarkdown)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Abrir o .md"))
                            .on_click(cx.listener(|this, _, window, cx| this.open_file(window, cx))),
                    )
                    .when(editable, |this| {
                        this.child(
                            IconButton::new("delete-agent", IconName::Trash)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Apagar o agente"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.delete_selected(window, cx)
                                })),
                        )
                        .child(
                            Button::new("run-agent", "Chamar")
                                .style(ButtonStyle::Outlined)
                                .start_icon(Icon::new(IconName::PlayOutlined).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("Abre uma thread nova com /comando"))
                                .on_click(cx.listener(|this, _, window, cx| this.run_selected(window, cx))),
                        )
                        .child(
                            Button::new("save-agent", "Salvar")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(|this, _, _window, cx| this.save(cx))),
                        )
                    })
                    .when(!editable, |this| {
                        this.child(
                            Button::new("copy-agent", "Copiar para o projeto")
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text(
                                    "Agentes do .claude/agents são só leitura; a cópia vira um agente do projeto",
                                ))
                                .on_click(cx.listener(|this, _, _window, cx| this.copy_to_project(cx))),
                        )
                    }),
            )
            .when_some(self.status.clone(), |this, status| {
                this.child(Label::new(status).size(LabelSize::Small).color(Color::Muted))
            })
            .child(
                Self::render_card("IDENTIDADE", cx)
                    .child(Self::render_field("Nome", None, &draft.name, cx))
                    .child(Self::render_field(
                        "Comando",
                        Some("Digite /comando no composer do Agent para chamar este agente."),
                        &draft.command,
                        cx,
                    ))
                    .child(Self::render_field("Descrição", None, &draft.description, cx)),
            )
            .child(
                Self::render_card("MODELO E PERFIL", cx)
                    .child(setting_row(
                        "Ferramentas",
                        "perfil base; os toolkits abaixo somam a ele",
                        self.render_profile_dropdown(window, cx),
                    ))
                    .child(setting_row(
                        "Modelo",
                        "sobrepõe o modelo da thread",
                        self.render_model_dropdown(window, cx),
                    ))
                    .child(setting_row(
                        "Subagents",
                        "modelo dos subagents que ele abre",
                        self.render_agent_subagent_model_dropdown(window, cx),
                    )),
            )
            .child(
                Self::render_card("PERMISSÕES DO ZED AGENT", cx)
                    .child(
                        Label::new(
                            "Decide o que o agente e os subagents dele rodam sem perguntar. Padrão segue as suas configurações; o que você nega nelas continua negado.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(self.render_builtin_permissions(cx)),
            )
            .child(
                Self::render_card("FERRAMENTAS DO ASYLUM", cx)
                    .child(
                        Label::new(
                            "Ligue os toolkits que o agente usa. Cada ferramenta é livre, pede confirmação ou nunca roda; o que você nega nas configurações continua negado.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(self.render_toolkits(cx)),
            )
            .child(
                Self::render_card("INSTRUÇÕES", cx).child(
                    div()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(draft.instructions.clone()),
                ),
            )
            .into_any_element()
    }
}

/// A dropdown of every model of the connected providers, grouped by provider. `current` and
/// the value given to `on_select` are `provider/model`; `None` is the `empty_label` entry.
pub(crate) fn model_dropdown(
    id: &'static str,
    current: Option<String>,
    empty_label: &'static str,
    on_select: impl Fn(Option<String>, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> DropdownMenu {
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
        None => empty_label.into(),
        Some(reference) => groups
            .iter()
            .find_map(|(_, models)| {
                models
                    .iter()
                    .find(|(id, _)| id == reference)
                    .map(|(_, name)| name.clone())
            })
            // A model whose provider is signed out still shows, so it is not lost silently.
            .unwrap_or_else(|| format!("{reference} (indisponível)").into()),
    };
    let on_select = Rc::new(on_select);
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        let keep = on_select.clone();
        menu = menu.toggleable_entry(
            empty_label,
            current.is_none(),
            IconPosition::Start,
            None,
            move |_window, cx| keep(None, cx),
        );
        if groups.is_empty() {
            menu = menu.separator().header("Nenhum provedor conectado");
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
    selector(id, IconName::Sparkle, label, menu)
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

/// A labeled setting with its control on the right, as in the agent's cards.
fn setting_row(
    title: &'static str,
    hint: &'static str,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .gap_3()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .child(Label::new(title))
                .child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted)),
        )
        .child(div().flex_none().max_w(px(280.)).child(control))
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
        h_flex()
            .key_context("TaskAgents")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w(px(280.))
                    .h_full()
                    .flex_none()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        div().p_2().child(
                            h_flex()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border_variant)
                                .bg(cx.theme().colors().editor_background)
                                .child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1().min_w_0().child(self.search.clone())),
                        ),
                    )
                    .child(self.render_segment(cx))
                    .child(div().flex_1().min_h_0().child(if self.segment == Segment::Flows {
                        self.render_flow_list(cx).into_any_element()
                    } else {
                        self.render_list(cx).into_any_element()
                    }))
                    .child(
                        div().p_2().child(
                            h_flex()
                                .id("new-task-agent")
                                .gap_2()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border_variant)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().colors().element_hover))
                                .child(
                                    Icon::new(IconName::Plus)
                                        .size(IconSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(if self.segment == Segment::Flows { "Nova esteira" } else { "Novo agente" })
                                        .color(Color::Accent),
                                )
                                .child(div().flex_1())
                                .child(
                                    Label::new("de um modelo ou com IA")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.segment == Segment::Flows {
                                        this.workspace
                                            .update(cx, |workspace, cx| crate::flows_view::new_flow(workspace, window, cx))
                                            .log_err();
                                    } else {
                                        this.open_new_agent_modal(window, cx)
                                    }
                                })),
                        ),
                    )
                    .child(Divider::horizontal())
                    .child(
                        v_flex()
                            .p_2()
                            .gap_2()
                            .child(setting_row(
                                "Subagents",
                                "padrão do Zed Agent",
                                self.render_subagent_model_dropdown(window, cx),
                            ))
                            .child(
                                v_flex()
                                    .p_2()
                                    .gap_0p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().editor_background)
                                    .child(
                                        Label::new("COMO FICA NO REPO")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(
                                            "Cada agente é um .md com frontmatter em .asylum/agents; vai no git e o time inteiro usa.",
                                        )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(if self.segment == Segment::Flows {
                        self.render_flow_detail(cx)
                    } else {
                        self.render_detail(window, cx)
                    }),
            )
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
        "Agentes".into()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.workspace.upgrade().is_some_and(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .is_some_and(|item| item.downcast::<TaskAgentsView>().is_some())
        });
        let workspace = self.workspace.clone();
        StatusBarButton::new("toolkit-task-agents", IconName::UserGroup, is_open)
            .tab_index(0isize)
            .aria_label("Agentes")
            .tooltip(|_window, cx| Tooltip::for_action("Agentes", &OpenTaskAgents, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| open(workspace, window, cx))
                    .log_err();
            })
    }
}

impl StatusItemView for TaskAgentsToolkitButton {
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
