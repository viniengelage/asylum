//! The "Novo agente" modal: start from a template, an imported `.claude/agents` file or a free
//! description that the default model turns into an agent, review the suggestion and create it.

use super::{MAX_AUTHORING_REPAIRS, TEMPLATES, TaskAgentsView, permission_label, template_agent};
use agent_settings::AgentProfile;
use anyhow::Result;
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
    Window, actions,
};
use language_model::{
    CompletionIntent, LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use task_agents::{
    ALL_TOOLS_PERMISSION_KEY, AuthoringCatalog, PermissionMode, TaskAgent, TaskAgentSource,
    ToolAccess, toolkits,
};
use ui::{ContextMenu, PopoverMenu, Switch, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

actions!(
    task_agents,
    [
        /// Asks the default model for an agent from the description typed in "Novo agente".
        SuggestAgent,
        /// Creates the agent suggested in "Novo agente".
        CreateSuggestedAgent,
    ]
);

#[derive(Clone, PartialEq)]
enum Start {
    Blank,
    Template(usize),
    Imported(PathBuf),
    Generated,
}

#[derive(Default)]
struct Generation {
    running: bool,
    /// The model's answer, streamed as it arrives.
    output: String,
    progress: Option<SharedString>,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

pub(super) struct NewAgentModal {
    view: WeakEntity<TaskAgentsView>,
    focus_handle: FocusHandle,
    description: Entity<Editor>,
    catalog: Arc<AuthoringCatalog>,
    imported: Vec<TaskAgent>,
    start: Start,
    suggestion: TaskAgent,
    personal: bool,
    generation: Generation,
}

impl NewAgentModal {
    pub(super) fn new(
        view: WeakEntity<TaskAgentsView>,
        catalog: Arc<AuthoringCatalog>,
        imported: Vec<TaskAgent>,
        personal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let description = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 10, window, cx);
            editor.set_placeholder_text(
                "Ex.: quando um PR mexer em telas do app, abrir o preview no simulador, tirar print das telas alteradas e comentar no PR as diferenças para o design.",
                window,
                cx,
            );
            editor.set_soft_wrap();
            editor
        });
        window.focus(&description.focus_handle(cx), cx);
        Self {
            view,
            focus_handle: cx.focus_handle(),
            description,
            catalog,
            imported,
            start: Start::Blank,
            suggestion: template_agent(None),
            personal,
            generation: Generation::default(),
        }
    }

    fn start_from_template(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let template = index.and_then(|index| TEMPLATES.get(index));
        self.cancel_generation();
        self.suggestion = template_agent(template);
        self.start = index.map_or(Start::Blank, Start::Template);
        let text = template.map_or("", |template| template.description);
        self.description.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
        cx.notify();
    }

    fn start_from_imported(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self
            .imported
            .iter()
            .find(|agent| agent.file_path == path)
            .cloned()
        else {
            return;
        };
        self.cancel_generation();
        let description = agent.description.clone();
        self.suggestion = agent;
        self.start = Start::Imported(path);
        self.description.update(cx, |editor, cx| {
            editor.set_text(description, window, cx);
        });
        cx.notify();
    }

    fn cancel_generation(&mut self) {
        self.generation = Generation::default();
    }

    fn suggest(&mut self, _: &SuggestAgent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.generation.running {
            return;
        }
        let description = self.description.read(cx).text(cx);
        let registry = LanguageModelRegistry::read_global(cx);
        let model = registry.default_model().and_then(|model| {
            registry
                .provider_for_model(&model)
                .log_err()
                .map(|provider| (model, provider))
        });
        let error = if description.trim().is_empty() {
            Some("Diga o que o agente deve fazer.")
        } else if model.is_none() {
            Some("Escolha um modelo no painel do Agent para gerar a sugestão.")
        } else {
            None
        };
        if let Some(error) = error {
            self.generation.error = Some(error.into());
            cx.notify();
            return;
        }
        let Some((model, provider)) = model else {
            return;
        };

        let catalog = self.catalog.clone();
        let mut messages = vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![task_agents::authoring_system_prompt(&catalog).into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![task_agents::authoring_user_prompt(&description, "").into()],
                cache: false,
                reasoning_details: None,
            },
        ];
        let task = cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            let result: Result<TaskAgent> = async {
                loop {
                    let request = LanguageModelRequest {
                        intent: Some(CompletionIntent::CreateFile),
                        messages: messages.clone(),
                        ..Default::default()
                    };
                    let mut response = provider.stream_completion_text(&model, request, cx).await?;
                    let mut text = String::new();
                    while let Some(chunk) = response.stream.next().await {
                        text.push_str(&chunk?);
                        this.update(cx, |this, cx| {
                            this.generation.output = text.clone();
                            cx.notify();
                        })?;
                    }

                    let markdown = task_agents::extract_agent_markdown(&text);
                    let problems = match task_agents::parse_task_agent(
                        Path::new("gerado.md"),
                        &markdown,
                        TaskAgentSource::Personal,
                    ) {
                        Ok(mut agent) => {
                            let problems = task_agents::review_generated_agent(&agent, &catalog);
                            if problems.is_empty() || attempt >= MAX_AUTHORING_REPAIRS {
                                task_agents::drop_unknown_references(&mut agent, &catalog);
                                return Ok(agent);
                            }
                            problems
                        }
                        Err(error) if attempt < MAX_AUTHORING_REPAIRS => {
                            vec![format!("o arquivo não abre: {error:#}")]
                        }
                        Err(error) => return Err(error),
                    };
                    attempt += 1;
                    this.update(cx, |this, cx| {
                        this.generation.progress =
                            Some(format!("Corrigindo: {}", problems.join("; ")).into());
                        cx.notify();
                    })?;
                    messages.push(LanguageModelRequestMessage {
                        role: Role::Assistant,
                        content: vec![text.into()],
                        cache: false,
                        reasoning_details: None,
                    });
                    messages.push(LanguageModelRequestMessage {
                        role: Role::User,
                        content: vec![task_agents::authoring_repair_prompt(&problems).into()],
                        cache: false,
                        reasoning_details: None,
                    });
                }
            }
            .await;
            this.update(cx, |this, cx| {
                this.generation.running = false;
                this.generation.progress = None;
                match result {
                    Ok(agent) => {
                        this.suggestion = agent;
                        this.start = Start::Generated;
                        this.generation.output.clear();
                    }
                    Err(error) => {
                        this.generation.error =
                            Some(format!("Não foi possível gerar a sugestão: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        self.generation = Generation {
            running: true,
            output: String::new(),
            progress: Some("Escrevendo a sugestão…".into()),
            error: None,
            _task: Some(task),
        };
        cx.notify();
    }

    fn create(&mut self, _: &CreateSuggestedAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.create_suggested(false, window, cx);
    }

    fn create_suggested(&mut self, run_after: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.generation.running {
            return;
        }
        let agent = self.suggestion.clone();
        let personal = self.personal;
        let generated = self.start == Start::Generated;
        self.view
            .update(cx, |view, cx| {
                view.create_agent(agent, personal, generated, run_after, window, cx)
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_section_label(label: &'static str) -> Label {
        Label::new(label)
            .size(LabelSize::XSmall)
            .weight(gpui::FontWeight::MEDIUM)
            .color(Color::Muted)
    }

    fn render_start_chip(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        icon: IconName,
        selected: bool,
    ) -> Button {
        Button::new(id, label)
            .style(ButtonStyle::Outlined)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(icon).size(IconSize::Small).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .toggle_state(selected)
    }

    fn render_starts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().flex_wrap().gap_1p5().child(
            self.render_start_chip(
                "new-agent-blank",
                "Em branco",
                IconName::Plus,
                self.start == Start::Blank,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_from_template(None, window, cx);
            })),
        );
        for (index, template) in TEMPLATES.iter().enumerate() {
            row = row.child(
                self.render_start_chip(
                    ("new-agent-template", index),
                    template.name,
                    template.icon,
                    self.start == Start::Template(index),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_from_template(Some(index), window, cx);
                })),
            );
        }
        row.child(self.render_import_chip(cx))
    }

    fn render_import_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = matches!(self.start, Start::Imported(_));
        let chip = self.render_start_chip(
            "new-agent-import",
            "Importar .claude/agents",
            IconName::Download,
            selected,
        );
        if self.imported.is_empty() {
            return chip
                .disabled(true)
                .tooltip(Tooltip::text(
                    "Nenhum agente em .claude/agents nos projetos abertos",
                ))
                .into_any_element();
        }
        let modal = cx.entity().downgrade();
        let imported = self
            .imported
            .iter()
            .map(|agent| {
                (
                    agent.name.clone(),
                    agent.command.clone(),
                    agent.file_path.clone(),
                )
            })
            .collect::<Vec<_>>();
        PopoverMenu::new("new-agent-import-menu")
            .trigger(chip)
            .menu(move |window, cx| {
                let modal = modal.clone();
                let imported = imported.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        for (name, command, path) in imported {
                            let modal = modal.clone();
                            menu = menu.custom_entry(
                                move |_window, _cx| {
                                    h_flex()
                                        .gap_2()
                                        .child(Label::new(name.clone()))
                                        .child(
                                            Label::new(format!("/{command}"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .into_any_element()
                                },
                                move |window, cx| {
                                    modal
                                        .update(cx, |modal, cx| {
                                            modal.start_from_imported(path.clone(), window, cx)
                                        })
                                        .log_err();
                                },
                            );
                        }
                        menu
                    },
                ))
            })
            .into_any_element()
    }

    fn render_description(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let running = self.generation.running;
        let description_focus = self.description.focus_handle(cx);
        v_flex()
            .gap_3()
            .px_4()
            .pt_3()
            .pb_2()
            .rounded_md()
            .border_1()
            .border_color(if description_focus.is_focused(window) {
                colors.border_focused
            } else {
                colors.border
            })
            .bg(colors.editor_background)
            .child(self.description.clone())
            .child(
                h_flex().child(
                    h_flex()
                        .id("new-agent-suggest")
                        .gap_1()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .when(!running, |this| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(colors.ghost_element_hover))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.suggest(&SuggestAgent, window, cx)
                                }))
                        })
                        .tooltip(Tooltip::text(
                            "A IA escolhe perfil, ferramentas e permissões e escreve as instruções",
                        ))
                        .child(ui::KeyBinding::for_action_in(
                            &SuggestAgent,
                            &description_focus,
                            cx,
                        ))
                        .child(
                            Label::new(if running { "sugerindo…" } else { "sugerir" })
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                ),
            )
    }

    fn render_column(cx: &App) -> Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().surface_background)
    }

    fn render_column_header(title: &'static str, cx: &App) -> Div {
        h_flex()
            .justify_between()
            .px_3()
            .py_2p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Self::render_section_label(title))
    }

    fn render_value(label: &'static str, value: impl IntoElement) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(value)
    }

    fn render_trigger(
        id: &'static str,
        label: impl Into<SharedString>,
        tooltip: &'static str,
    ) -> impl IntoElement {
        div().id(id).tooltip(Tooltip::text(tooltip)).child(
            Switch::new(id, ToggleState::Selected)
                .label(label)
                .label_size(LabelSize::Small),
        )
    }

    fn render_identity(&self, cx: &App) -> impl IntoElement {
        let agent = &self.suggestion;
        let profiles = AgentProfile::available_profiles(cx);
        let profile = match agent.profile.as_deref() {
            None => "Perfil atual da thread".to_string(),
            Some(id) => profiles
                .iter()
                .find(|(profile_id, _)| profile_id.0.as_ref() == id)
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| id.to_string()),
        };
        let model = agent
            .model
            .clone()
            .unwrap_or_else(|| "Modelo da thread".to_string());
        Self::render_column(cx).child(
            v_flex()
                .p_3()
                .gap_3()
                .child(Self::render_value(
                    "Nome",
                    Label::new(agent.name.clone()).single_line().truncate(),
                ))
                .child(Self::render_value(
                    "Comando",
                    Label::new(format!("/{}", agent.command))
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .buffer_font(cx),
                ))
                .child(Self::render_value(
                    "Perfil base",
                    Label::new(profile).single_line().truncate(),
                ))
                .child(Self::render_value(
                    "Modelo",
                    Label::new(model).single_line().truncate(),
                ))
                .child(Self::render_value(
                    "Gatilhos",
                    v_flex()
                        .pt_1()
                        .gap_1p5()
                        .child(Self::render_trigger(
                            "new-agent-trigger-composer",
                            format!("/{} no composer", agent.command),
                            "Sempre ligado: digite o comando no composer do Agent",
                        ))
                        .child(Self::render_trigger(
                            "new-agent-trigger-sidebar",
                            "na seção Agentes da sidebar",
                            "Sempre ligado: o agente aparece na seção Agentes da sidebar",
                        )),
                )),
        )
    }

    fn render_tool_row(
        icon: Icon,
        name: impl Into<SharedString>,
        description: Option<SharedString>,
        permission: (&'static str, Color),
        cx: &App,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .gap_2p5()
            .px_3()
            .py_2()
            .child(
                h_flex()
                    .flex_none()
                    .size_7()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.element_background)
                    .child(icon.size(IconSize::Small).color(Color::Muted)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    )
                    .when_some(description, |this, description| {
                        this.child(
                            Label::new(description)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                    }),
            )
            .child(permission_chip(permission.0, permission.1, cx))
    }

    fn render_tools(&self, cx: &App) -> impl IntoElement {
        let agent = &self.suggestion;
        let all_mode = agent
            .permissions
            .get(ALL_TOOLS_PERMISSION_KEY)
            .and_then(|rules| rules.default);
        let mut rows = v_flex().py_1();
        let registered = toolkits(cx);
        for toolkit_id in &agent.toolkits {
            let Some(toolkit) = registered
                .iter()
                .find(|toolkit| toolkit.id.as_ref() == toolkit_id.as_str())
            else {
                continue;
            };
            let toolkit_mode = agent
                .permissions
                .get(toolkit_id.as_str())
                .and_then(|rules| rules.default)
                .or(all_mode);
            let permission = match toolkit_mode {
                Some(mode) => (permission_label(Some(mode)), permission_color(mode)),
                None if toolkit
                    .tools
                    .iter()
                    .all(|tool| tool.access == ToolAccess::Read) =>
                {
                    ("livre", Color::Success)
                }
                None => ("pede", Color::Warning),
            };
            rows = rows.child(Self::render_tool_row(
                Icon::from_path(format!("icons/{}.svg", toolkit.icon)),
                toolkit.name.clone(),
                Some(toolkit.description.clone()),
                permission,
                cx,
            ));
        }
        let builtin_overrides = agent
            .permissions
            .iter()
            .filter(|(key, _)| {
                key.as_str() != ALL_TOOLS_PERMISSION_KEY
                    && !registered.iter().any(|toolkit| {
                        toolkit.id.as_ref() == key.as_str()
                            || toolkit
                                .tools
                                .iter()
                                .any(|tool| tool.name.as_ref() == key.as_str())
                    })
            })
            .filter_map(|(key, rules)| rules.default.map(|mode| (key.clone(), mode)))
            .collect::<Vec<_>>();
        for (tool, mode) in builtin_overrides {
            rows = rows.child(Self::render_tool_row(
                Icon::new(IconName::ToolHammer),
                tool,
                None,
                (permission_label(Some(mode)), permission_color(mode)),
                cx,
            ));
        }
        if agent.toolkits.is_empty() {
            rows = rows.child(
                div().px_3().py_2().child(
                    Label::new(
                        "Só as ferramentas do perfil base. Ligue toolkits depois, no formulário.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            );
        }
        Self::render_column(cx)
            .child(
                Self::render_column_header("FERRAMENTAS", cx).child(
                    div()
                        .id("new-agent-permissions-help")
                        .tooltip(Tooltip::text(
                            "livre: roda sem perguntar. pede: confirma com você antes de agir. nunca: a ferramenta fica bloqueada.",
                        ))
                        .child(
                            Label::new("por que cada")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        ),
                ),
            )
            .child(rows)
    }

    fn render_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self
            .view
            .upgrade()
            .map(|view| {
                view.read(cx)
                    .new_agent_path(&self.suggestion.command, self.personal, cx)
            })
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let (title, preview) = instructions_summary(&self.suggestion.instructions);
        let personal = self.personal;
        let colors = cx.theme().colors();
        Self::render_column(cx)
            .child(Self::render_column_header("SAÍDA", cx))
            .child(
                v_flex()
                    .flex_1()
                    .p_3()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new(title).line_clamp(2))
                            .when(!preview.is_empty(), |this| {
                                this.child(
                                    Label::new(preview)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .line_clamp(3),
                                )
                            }),
                    )
                    .child(Self::render_value(
                        "Destino",
                        h_flex().pt_0p5().child(
                            h_flex()
                                .gap_0p5()
                                .p_0p5()
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border_variant)
                                .bg(colors.editor_background)
                                .child(
                                    Button::new("new-agent-project", "Projeto")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .toggle_state(!personal)
                                        .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                        .tooltip(Tooltip::text(
                                            "Vai para .asylum/agents e entra no git",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.personal = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("new-agent-personal", "Só para mim")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .toggle_state(personal)
                                        .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                        .tooltip(Tooltip::text(
                                            "Fica nos seus dados, fora do repositório",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.personal = true;
                                            cx.notify();
                                        })),
                                ),
                        ),
                    ))
                    .child(Self::render_value(
                        "Salvar em",
                        Label::new(path)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate_start(),
                    )),
            )
    }

    fn render_suggestion_header(&self, cx: &App) -> impl IntoElement {
        let badge = match &self.start {
            Start::Blank => "em branco",
            Start::Template(_) => "do modelo",
            Start::Imported(_) => "importada",
            Start::Generated => "gerada agora",
        };
        let accent = Color::Accent.color(cx);
        h_flex()
            .gap_3()
            .child(Self::render_section_label("SUGESTÃO"))
            .child(
                ui::Chip::new(badge)
                    .icon(if self.start == Start::Generated {
                        IconName::Sparkle
                    } else {
                        IconName::FileMarkdown
                    })
                    .icon_color(Color::Accent)
                    .label_color(Color::Accent)
                    .label_size(LabelSize::XSmall)
                    .bg_color(accent.opacity(0.14))
                    .border_color(accent.opacity(0.3)),
            )
    }
}

/// The first line of the instructions (without the Markdown heading marks) and a few of the
/// lines after it, standing in for the agent's output, which the file format doesn't type.
fn instructions_summary(instructions: &str) -> (String, String) {
    let mut lines = instructions
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let title = lines
        .next()
        .map(|line| line.trim_start_matches('#').trim().to_string())
        .unwrap_or_default();
    let preview = lines
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.trim_start_matches(['-', '*']).trim())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    (title, preview)
}

fn permission_color(mode: PermissionMode) -> Color {
    match mode {
        PermissionMode::Allow => Color::Success,
        PermissionMode::Confirm => Color::Warning,
        PermissionMode::Deny => Color::Error,
    }
}

fn permission_chip(label: &'static str, color: Color, cx: &App) -> impl IntoElement {
    let tint = color.color(cx);
    ui::Chip::new(label)
        .label_color(color)
        .label_size(LabelSize::XSmall)
        .bg_color(tint.opacity(0.12))
        .border_color(tint.opacity(0.4))
}

impl EventEmitter<DismissEvent> for NewAgentModal {}

impl Focusable for NewAgentModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for NewAgentModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for NewAgentModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.generation.running;
        let viewport = window.viewport_size();
        let description_focus = self.description.focus_handle(cx);
        // The create binding differs inside the description editor, where cmd-enter suggests.
        let create_binding_focus = if description_focus.is_focused(window) {
            description_focus
        } else {
            self.focus_handle.clone()
        };
        v_flex()
            .key_context("NewTaskAgent")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::suggest))
            .on_action(cx.listener(Self::create))
            .on_action(cx.listener(Self::cancel))
            .w(px(960.).min(viewport.width - px(64.)))
            .max_h(viewport.height - px(140.))
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .items_start()
                    .px_6()
                    .pt_5()
                    .pb_4()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(Headline::new("Novo agente").size(HeadlineSize::Small))
                            .child(
                                Label::new(
                                    "Descreva a tarefa. O Asylum sugere ferramentas, gatilhos e saída — tudo editável depois.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new("new-agent-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Fechar"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                v_flex()
                    .id("new-agent-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_5()
                    .gap_5()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Self::render_section_label("COMEÇAR DE"))
                            .child(self.render_starts(cx)),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Self::render_section_label("O QUE ELE FAZ"))
                            .child(self.render_description(window, cx))
                            .when_some(self.generation.error.clone(), |this, error| {
                                this.child(
                                    Label::new(error).size(LabelSize::Small).color(Color::Error),
                                )
                            })
                            .when_some(self.generation.progress.clone(), |this, progress| {
                                this.child(
                                    Label::new(progress)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .gap_2p5()
                            .child(self.render_suggestion_header(cx))
                            .when(running && !self.generation.output.is_empty(), |this| {
                                this.child(
                                    div()
                                        .max_h(px(260.))
                                        .overflow_hidden()
                                        .p_3()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .child(
                                            Label::new(self.generation.output.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .buffer_font(cx),
                                        ),
                                )
                            })
                            .when(!running || self.generation.output.is_empty(), |this| {
                                this.child(
                                    h_flex()
                                        .items_stretch()
                                        .gap_3()
                                        .child(self.render_identity(cx))
                                        .child(self.render_tools(cx))
                                        .child(self.render_output(cx)),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .px_6()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(
                            "Nada roda até você salvar e chamar. Permissões \"pede\" sempre confirmam.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .flex_1(),
                    )
                    .child(
                        Button::new("new-agent-create-and-run", "Criar e chamar")
                            .style(ButtonStyle::Outlined)
                            .disabled(running)
                            .start_icon(Icon::new(IconName::PlayOutlined).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Cria o agente e abre uma thread nova com /comando"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_suggested(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("new-agent-create", "Criar agente")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .disabled(running)
                            .key_binding(ui::KeyBinding::for_action_in(
                                &CreateSuggestedAgent,
                                &create_binding_focus,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create(&CreateSuggestedAgent, window, cx)
                            })),
                    ),
            )
    }
}
