//! Nova esteira: how it starts (blank, from a template, or with the steps suggested for a
//! request), the request, its name and where it is saved.

use super::FlowView;
use crate::task_agents_view::{primary_button, secondary_button, segment_option};
use editor::Editor;
use flows::{
    AgentStep, Criterion, Flow, FlowSource, FlowStep, GateStep, OnFail, RunStep, StepKind, Verifier,
};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, Window};
use project::Project;
use std::path::PathBuf;
use std::sync::Arc;
use task_agents::TaskAgent;
use ui::{KeyBinding, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    Blank,
    Template(usize),
    Suggest,
}

struct Template {
    name: &'static str,
    description: &'static str,
    build: fn() -> Vec<FlowStep>,
}

fn agent_step(command: &str, criteria: &[(&str, Verifier)], on_fail: OnFail) -> FlowStep {
    let mut step = AgentStep::new(command);
    step.criteria = criteria
        .iter()
        .map(|(text, verify)| Criterion {
            text: text.to_string(),
            required: true,
            verify: verify.clone(),
        })
        .collect();
    step.on_fail = on_fail;
    FlowStep {
        id: command.to_string(),
        kind: StepKind::Agent(step),
    }
}

fn gate(id: &str, note: &str) -> FlowStep {
    FlowStep {
        id: id.to_string(),
        kind: StepKind::Gate(GateStep {
            note: note.to_string(),
            adjust_to: None,
        }),
    }
}

const TEMPLATES: &[Template] = &[
    Template {
        name: "Feature do zero",
        description: "produto → design → você aprova → dev → testes → QA → docs → você aprova",
        build: || {
            vec![
                agent_step(
                    "produto",
                    &[("Critérios de aceite verificáveis", Verifier::SelfCheck)],
                    OnFail::default_for_agent(),
                ),
                agent_step(
                    "ux",
                    &[(
                        "Cobre os critérios da spec com uma tela ou estado",
                        Verifier::SelfCheck,
                    )],
                    OnFail::default_for_agent(),
                ),
                gate("aprovacao", "as telas do Designer UX"),
                agent_step(
                    "dev",
                    &[("Testes passam", Verifier::Command("npm test".to_string()))],
                    OnFail::default_for_agent(),
                ),
                agent_step(
                    "qa",
                    &[("Os critérios da spec passam no app", Verifier::SelfCheck)],
                    OnFail::BackTo {
                        step: "dev".to_string(),
                        max: OnFail::DEFAULT_MAX,
                    },
                ),
                agent_step("docs", &[], OnFail::default_for_agent()),
                gate("aprovacao-2", "o PR antes de marcar a tarefa como pronta"),
            ]
        },
    },
    Template {
        name: "Correção de bug",
        description: "dev reproduz e corrige → comando de testes → revisão → você aprova",
        build: || {
            vec![
                agent_step(
                    "dev",
                    &[(
                        "Há um teste que falhava antes da correção",
                        Verifier::SelfCheck,
                    )],
                    OnFail::default_for_agent(),
                ),
                FlowStep {
                    id: "comando".to_string(),
                    kind: StepKind::Run(RunStep {
                        command: "npm test".to_string(),
                        on_fail: OnFail::BackTo {
                            step: "dev".to_string(),
                            max: OnFail::DEFAULT_MAX,
                        },
                    }),
                },
                agent_step("revisar", &[], OnFail::AskYou),
                gate("aprovacao", "o PR da correção"),
            ]
        },
    },
    Template {
        name: "Revisão de PR",
        description: "revisor lê o PR → você aprova o que vai como comentário",
        build: || {
            vec![
                agent_step("revisar", &[], OnFail::AskYou),
                gate("aprovacao", "os achados antes de comentar no PR"),
            ]
        },
    },
];

pub(super) struct NewFlowModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    agents: Vec<TaskAgent>,
    /// The project's first local folder; without one the flow can only be personal.
    project_root: Option<(Arc<str>, PathBuf)>,
    personal: bool,
    start: Start,
    name: Entity<Editor>,
    request: Entity<Editor>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl NewFlowModal {
    pub(super) fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        agents: Vec<TaskAgent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project_root = project.read(cx).visible_worktrees(cx).find_map(|worktree| {
            let worktree = worktree.read(cx);
            worktree.as_local().map(|local| {
                (
                    Arc::<str>::from(worktree.root_name_str()),
                    local.abs_path().to_path_buf(),
                )
            })
        });
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Ex.: Feature do zero", window, cx);
            editor
        });
        let request = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text(
                "O que a esteira deve construir, ex.: Criar um todo list na área logada",
                window,
                cx,
            );
            editor
        });
        request.focus_handle(cx).focus(window, cx);
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            agents,
            personal: project_root.is_none(),
            project_root,
            start: Start::Blank,
            _subscriptions: vec![
                cx.subscribe(&name, |_, _, event: &editor::EditorEvent, cx| {
                    if matches!(event, editor::EditorEvent::BufferEdited) {
                        cx.notify();
                    }
                }),
            ],
            name,
            request,
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.name.read(cx).text(cx).trim().to_string();
        let name = if typed.is_empty() {
            match self.start {
                Start::Template(index) => TEMPLATES
                    .get(index)
                    .map(|template| template.name.to_string())
                    .unwrap_or_default(),
                _ => "Nova esteira".to_string(),
            }
        } else {
            typed
        };
        let id = task_agents::command_slug(&name);
        let (source, directory) = match (&self.project_root, self.personal) {
            (Some((root_name, root)), false) => (
                FlowSource::Project {
                    worktree_root_name: root_name.clone(),
                },
                root.join(flows::PROJECT_FLOWS_DIR),
            ),
            _ => (FlowSource::Personal, flows::personal_flows_dir()),
        };
        let request = self.request.read(cx).text(cx);
        let steps = match self.start {
            Start::Template(index) => TEMPLATES
                .get(index)
                .map(|template| (template.build)())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let flow = Flow {
            file_path: directory.join(format!("{id}{}", flows::FLOW_FILE_SUFFIX)),
            id,
            name,
            input: request.trim().to_string(),
            steps,
            notes: String::new(),
            source,
        };
        let suggest = self.start == Start::Suggest;
        let agents = self.agents.clone();
        let project = self.project.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let weak_workspace = workspace.weak_handle();
                let view = cx.new(|cx| {
                    FlowView::new(flow, agents, project, weak_workspace, false, window, cx)
                });
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view.update(cx, |view, cx| {
                    view.input_editor.update(cx, |editor, cx| {
                        editor.set_text(request.clone(), window, cx)
                    });
                    if suggest {
                        view.suggest_steps(window, cx);
                    }
                });
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn render_start(
        &self,
        id: &'static str,
        start: Start,
        icon: IconName,
        title: &'static str,
        detail: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match (self.start, start) {
            (Start::Template(_), Start::Template(_)) => true,
            (current, start) => current == start,
        };
        let colors = cx.theme().colors();
        v_flex()
            .id(id)
            .flex_1()
            .min_w_0()
            .p_3()
            .gap_1()
            .rounded_lg()
            .border_1()
            .cursor_pointer()
            .border_color(if selected {
                colors.text_accent
            } else {
                colors.border
            })
            .bg(if selected {
                colors.text_accent.opacity(0.12)
            } else {
                colors.editor_background
            })
            .hover(|this| this.border_color(colors.border_focused))
            .child(
                div()
                    .pb_2()
                    .child(Icon::new(icon).size(IconSize::Medium).color(if selected {
                        Color::Accent
                    } else {
                        Color::Muted
                    })),
            )
            .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
            .child(
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.start = start;
                cx.notify();
            }))
    }

    /// Where the file will be written, as the "Salvar em" hint shows it.
    fn file_label(&self, cx: &App) -> String {
        let typed = self.name.read(cx).text(cx).trim().to_string();
        let name = if typed.is_empty() {
            match self.start {
                Start::Template(index) => TEMPLATES
                    .get(index)
                    .map(|template| template.name.to_string())
                    .unwrap_or_default(),
                _ => "Nova esteira".to_string(),
            }
        } else {
            typed
        };
        let file = format!(
            "{}{}",
            task_agents::command_slug(&name),
            flows::FLOW_FILE_SUFFIX
        );
        if self.personal || self.project_root.is_none() {
            let directory = flows::personal_flows_dir();
            match directory.strip_prefix(util::paths::home_dir()) {
                Ok(relative) => format!("~/{}/{file}", relative.display()),
                Err(_) => directory.join(file).display().to_string(),
            }
        } else {
            format!("{}/{file}", flows::PROJECT_FLOWS_DIR)
        }
    }
}

impl EventEmitter<DismissEvent> for NewFlowModal {}

impl Focusable for NewFlowModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for NewFlowModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for NewFlowModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let colors = cx.theme().colors().clone();
        let suggest = self.start == Start::Suggest;
        let section = |text: &'static str| {
            Label::new(text)
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::SEMIBOLD)
                .color(Color::Muted)
        };
        let where_button = |id: &'static str,
                            label: &'static str,
                            personal: bool,
                            current: bool,
                            enabled: bool,
                            cx: &mut Context<Self>| {
            segment_option(id, label, personal == current, enabled, cx).when(enabled, |this| {
                this.on_click(cx.listener(move |this, _, _window, cx| {
                    this.personal = personal;
                    cx.notify();
                }))
            })
        };
        let create_binding = KeyBinding::for_action_in(&menu::Confirm, &self.focus_handle, cx);
        let personal_agents = self
            .agents
            .iter()
            .filter(|agent| matches!(agent.source, task_agents::TaskAgentSource::Personal))
            .count();
        let project_agents = self.agents.len() - personal_agents;
        let template_names = TEMPLATES
            .iter()
            .map(|template| template.name)
            .collect::<Vec<_>>()
            .join(", ");
        let mut templates = v_flex().gap_1();
        if let Start::Template(selected) = self.start {
            for (index, template) in TEMPLATES.iter().enumerate() {
                templates = templates.child(
                    h_flex()
                        .id(("flow-template", index))
                        .px_2p5()
                        .py_2()
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .border_1()
                        .border_color(if index == selected {
                            colors.text_accent
                        } else {
                            colors.border
                        })
                        .child(
                            Icon::new(IconName::ListTree)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(template.name).size(LabelSize::Small))
                                .child(
                                    Label::new(template.description)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.start = Start::Template(index);
                            cx.notify();
                        })),
                );
            }
        }
        let request_focused = self.request.focus_handle(cx).contains_focused(window, cx);
        v_flex()
            .key_context("NewFlow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.create(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(px(720.).min(viewport.width - px(64.)))
            .max_h(viewport.height - px(140.))
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .items_start()
                    .px_5()
                    .pt_5()
                    .pb_4()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(Headline::new("Nova esteira").size(HeadlineSize::Small))
                            .child(
                                Label::new(
                                    "Uma lista de passos com seus agentes. Tudo é editável depois, passo a passo.",
                                )
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new("new-flow-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Fechar"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                v_flex()
                    .id("new-flow-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .pb_5()
                    .gap_5()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("COMEÇAR DE"))
                            .child(
                                h_flex()
                                    .items_start()
                                    .gap_2()
                                    .child(self.render_start(
                                        "new-flow-blank",
                                        Start::Blank,
                                        IconName::Plus,
                                        "Em branco",
                                        "só o pedido e um +".to_string(),
                                        cx,
                                    ))
                                    .child(self.render_start(
                                        "new-flow-template",
                                        match self.start {
                                            Start::Template(index) => Start::Template(index),
                                            _ => Start::Template(0),
                                        },
                                        IconName::Blocks,
                                        "De um modelo",
                                        template_names,
                                        cx,
                                    ))
                                    .child(self.render_start(
                                        "new-flow-suggest",
                                        Start::Suggest,
                                        IconName::Sparkle,
                                        "Sugerir pelo pedido",
                                        "a IA propõe os passos com seus agentes".to_string(),
                                        cx,
                                    )),
                            )
                            .child(templates),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("O QUE CONSTRUIR"))
                            .child(
                                v_flex()
                                    .px_3()
                                    .pt_3()
                                    .pb_2()
                                    .gap_3()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(if request_focused || suggest {
                                        colors.text_accent
                                    } else {
                                        colors.border
                                    })
                                    .bg(colors.editor_background)
                                    .child(self.request.clone())
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Label::new(format!(
                                                    "Usa só agentes da sua lista: {personal_agents} pessoa{}, {project_agents} do projeto",
                                                    if personal_agents == 1 { "l" } else { "is" }
                                                ))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            )
                                            .child(div().flex_1())
                                            .when(suggest, |this| {
                                                this.child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            create_binding
                                                                .clone()
                                                                .size(rems_from_px(11_f32)),
                                                        )
                                                        .child(
                                                            Label::new("sugerir")
                                                                .size(LabelSize::XSmall)
                                                                .color(Color::Accent),
                                                        ),
                                                )
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_start()
                            .gap_4()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .child(section("NOME"))
                                    .child(
                                        div()
                                            .px_2p5()
                                            .py_1p5()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(colors.border)
                                            .bg(colors.editor_background)
                                            .child(self.name.clone()),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .child(section("SALVAR EM"))
                                    .child(
                                        h_flex()
                                            .p_0p5()
                                            .gap_0p5()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(colors.border)
                                            .bg(colors.editor_background)
                                            .child(where_button(
                                                "new-flow-project",
                                                "Projeto",
                                                false,
                                                self.personal,
                                                self.project_root.is_some(),
                                                cx,
                                            ))
                                            .child(where_button(
                                                "new-flow-personal",
                                                "Só para mim",
                                                true,
                                                self.personal,
                                                true,
                                                cx,
                                            )),
                                    )
                                    .child(
                                        Label::new(self.file_label(cx))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .buffer_font(cx)
                                            .truncate(),
                                    ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Label::new(if suggest {
                            "A sugestão abre no editor da esteira; nada roda até você clicar em Rodar."
                        } else {
                            "A esteira abre no editor; nada roda até você clicar em Rodar."
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        secondary_button("new-flow-cancel", None, "Cancelar", true, cx)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        primary_button(
                            "new-flow-create",
                            None,
                            if suggest { "Sugerir e abrir" } else { "Criar e abrir" },
                            Some(create_binding),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}
