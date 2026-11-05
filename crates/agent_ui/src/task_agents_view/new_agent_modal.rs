//! The "Novo agente" modal: start from a template, an imported `.claude/agents` file or a free
//! description that the default model turns into an agent, review the suggestion and create it.

use super::{
    MAX_AUTHORING_REPAIRS, TEMPLATES, TaskAgentsView, model_menu, permission_label, template_agent,
};
use agent_settings::AgentProfile;
use anyhow::Result;
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity, Window, actions,
};
use i18n::t;
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
use ui::{
    ContextMenu, PopoverMenu, PopoverTriggerArea, Tooltip, choice_chip, modal_footer,
    modal_surface, prelude::*, primary_button, secondary_button, segment_option, segmented_control,
    select_field, text_field, toggle_switch,
};
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
    name: Entity<Editor>,
    command: Entity<Editor>,
    catalog: Arc<AuthoringCatalog>,
    imported: Vec<TaskAgent>,
    start: Start,
    suggestion: TaskAgent,
    personal: bool,
    generation: Generation,
    _subscriptions: Vec<Subscription>,
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
                &t!("agent_ui.new_agent.description_placeholder"),
                window,
                cx,
            );
            editor.set_soft_wrap();
            editor
        });
        window.focus(&description.focus_handle(cx), cx);
        let suggestion = template_agent(None);
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(suggestion.name.clone(), window, cx);
            editor
        });
        let command = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(suggestion.command.clone(), window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&name, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.suggestion.name = editor.read(cx).text(cx).trim().to_string();
                    cx.notify();
                }
            }),
            cx.subscribe(&command, |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.suggestion.command = normalize_command(&editor.read(cx).text(cx));
                    cx.notify();
                }
            }),
        ];
        Self {
            view,
            focus_handle: cx.focus_handle(),
            description,
            name,
            command,
            catalog,
            imported,
            start: Start::Blank,
            suggestion,
            personal,
            generation: Generation::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Replaces the suggestion and shows its name and command in the editable fields.
    fn set_suggestion(&mut self, agent: TaskAgent, window: &mut Window, cx: &mut Context<Self>) {
        let name = agent.name.clone();
        let command = agent.command.clone();
        self.suggestion = agent;
        self.name
            .update(cx, |editor, cx| editor.set_text(name, window, cx));
        self.command
            .update(cx, |editor, cx| editor.set_text(command, window, cx));
    }

    fn start_from_template(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let template = index.and_then(|index| TEMPLATES.get(index));
        self.cancel_generation();
        self.set_suggestion(template_agent(template), window, cx);
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
        self.set_suggestion(agent, window, cx);
        self.start = Start::Imported(path);
        self.description.update(cx, |editor, cx| {
            editor.set_text(description, window, cx);
        });
        cx.notify();
    }

    fn cancel_generation(&mut self) {
        self.generation = Generation::default();
    }

    fn suggest(&mut self, _: &SuggestAgent, window: &mut Window, cx: &mut Context<Self>) {
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
            Some(t!("agent_ui.new_agent.error.no_description"))
        } else if model.is_none() {
            Some(t!("agent_ui.new_agent.error.no_model"))
        } else {
            None
        };
        if let Some(error) = error {
            self.generation.error = Some(error);
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
        let task = cx.spawn_in(window, async move |this, cx| {
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
                        this.generation.progress = Some(t!(
                            "agent_ui.new_agent.progress.fixing",
                            problems = problems.join("; ")
                        ));
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
            this.update_in(cx, |this, window, cx| {
                this.generation.running = false;
                this.generation.progress = None;
                match result {
                    Ok(agent) => {
                        this.set_suggestion(agent, window, cx);
                        this.start = Start::Generated;
                        this.generation.output.clear();
                    }
                    Err(error) => {
                        this.generation.error = Some(t!(
                            "agent_ui.new_agent.error.suggestion_failed",
                            error = format!("{error:#}")
                        ));
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        self.generation = Generation {
            running: true,
            output: String::new(),
            progress: Some(t!("agent_ui.new_agent.progress.writing")),
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
        if self.suggestion.name.is_empty() || self.suggestion.command.is_empty() {
            self.generation.error = Some(t!("agent_ui.new_agent.error.missing_identity"));
            cx.notify();
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

    fn render_section_label(label: SharedString) -> Label {
        Label::new(label)
            .size(LabelSize::XSmall)
            .weight(gpui::FontWeight::MEDIUM)
            .color(Color::Muted)
    }

    fn render_starts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().flex_wrap().gap_2().child(
            choice_chip(
                "new-agent-blank",
                Some(IconName::Plus),
                t!("agent_ui.new_agent.start.blank"),
                self.start == Start::Blank,
                true,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_from_template(None, window, cx);
            })),
        );
        for (index, template) in TEMPLATES.iter().enumerate() {
            row = row.child(
                choice_chip(
                    ("new-agent-template", index),
                    Some(template.icon),
                    template.name,
                    self.start == Start::Template(index),
                    true,
                    cx,
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
        let enabled = !self.imported.is_empty();
        let chip = choice_chip(
            "new-agent-import",
            Some(IconName::Download),
            t!("agent_ui.new_agent.start.import"),
            selected,
            enabled,
            cx,
        );
        if !enabled {
            return chip
                .tooltip(Tooltip::text(t!("agent_ui.new_agent.start.import_empty")))
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
            .trigger(PopoverTriggerArea::new(chip))
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
            .rounded(px(10.))
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
                        .tooltip(Tooltip::text(t!("agent_ui.new_agent.suggest_tooltip")))
                        .child(ui::KeyBinding::for_action_in(
                            &SuggestAgent,
                            &description_focus,
                            cx,
                        ))
                        .child(
                            Label::new(if running {
                                t!("agent_ui.new_agent.suggesting")
                            } else {
                                t!("agent_ui.new_agent.suggest")
                            })
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
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().surface_background)
    }

    fn render_column_header(title: SharedString, cx: &App) -> Div {
        h_flex()
            .justify_between()
            .px_3()
            .py_2p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Self::render_section_label(title))
    }

    fn render_value(label: SharedString, value: impl IntoElement) -> impl IntoElement {
        v_flex()
            .gap_1()
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
        tooltip: SharedString,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .gap_2()
            .tooltip(Tooltip::text(tooltip))
            .child(toggle_switch(id, true, true, cx))
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate(),
            )
    }

    fn render_identity(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent = &self.suggestion;
        let profiles = AgentProfile::available_profiles(cx);
        let profile = match agent.profile.as_deref() {
            None => t!("agent_ui.task_agents.profile.current_thread").to_string(),
            Some(id) => profiles
                .iter()
                .find(|(profile_id, _)| profile_id.0.as_ref() == id)
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| id.to_string()),
        };
        let modal = cx.entity().downgrade();
        let profile_menu = ContextMenu::build(window, cx, {
            let modal = modal.clone();
            let current = agent.profile.clone();
            move |mut menu, _window, _cx| {
                let keep = modal.clone();
                menu = menu.toggleable_entry(
                    t!("agent_ui.task_agents.profile.current_thread"),
                    current.is_none(),
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        keep.update(cx, |modal, cx| {
                            modal.suggestion.profile = None;
                            cx.notify();
                        })
                        .log_err();
                    },
                );
                for (profile_id, name) in profiles.iter() {
                    let modal = modal.clone();
                    let id = profile_id.0.to_string();
                    let toggled = current.as_deref() == Some(id.as_str());
                    menu = menu.toggleable_entry(
                        name.clone(),
                        toggled,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            let id = id.clone();
                            modal
                                .update(cx, |modal, cx| {
                                    modal.suggestion.profile = Some(id);
                                    cx.notify();
                                })
                                .log_err();
                        },
                    );
                }
                menu
            }
        });
        let (model, model_menu) = model_menu(
            agent.model.clone(),
            t!("agent_ui.task_agents.model.thread"),
            move |model, cx| {
                modal
                    .update(cx, |modal, cx| {
                        modal.suggestion.model = model;
                        cx.notify();
                    })
                    .log_err();
            },
            window,
            cx,
        );
        let name_focused = self.name.focus_handle(cx).is_focused(window);
        let command_focused = self.command.focus_handle(cx).is_focused(window);
        Self::render_column(cx).child(
            v_flex()
                .p_3()
                .gap_3()
                .child(Self::render_value(
                    t!("agent_ui.new_agent.field.name"),
                    text_field(name_focused, cx).child(div().flex_1().child(self.name.clone())),
                ))
                .child(Self::render_value(
                    t!("agent_ui.new_agent.field.command"),
                    text_field(command_focused, cx)
                        .child(
                            Label::new("/")
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .child(div().flex_1().child(self.command.clone())),
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div().flex_1().min_w_0().child(Self::render_value(
                                t!("agent_ui.new_agent.field.base_profile"),
                                PopoverMenu::new("new-agent-profile-menu")
                                    .full_width(true)
                                    .trigger(PopoverTriggerArea::new(select_field(
                                        "new-agent-profile",
                                        profile,
                                        cx,
                                    )))
                                    .menu(move |_window, _cx| Some(profile_menu.clone())),
                            )),
                        )
                        .child(
                            div().flex_1().min_w_0().child(Self::render_value(
                                t!("agent_ui.new_agent.field.model"),
                                PopoverMenu::new("new-agent-model-menu")
                                    .full_width(true)
                                    .trigger(PopoverTriggerArea::new(select_field(
                                        "new-agent-model",
                                        model,
                                        cx,
                                    )))
                                    .menu(move |_window, _cx| Some(model_menu.clone())),
                            )),
                        ),
                )
                .child(Self::render_value(
                    t!("agent_ui.new_agent.field.triggers"),
                    v_flex()
                        .pt_1()
                        .gap_2()
                        .child(Self::render_trigger(
                            "new-agent-trigger-composer",
                            t!(
                                "agent_ui.new_agent.trigger.composer",
                                command = agent.command
                            ),
                            t!("agent_ui.new_agent.trigger.composer_tooltip"),
                            cx,
                        ))
                        .child(Self::render_trigger(
                            "new-agent-trigger-sidebar",
                            t!("agent_ui.new_agent.trigger.sidebar"),
                            t!("agent_ui.new_agent.trigger.sidebar_tooltip"),
                            cx,
                        )),
                )),
        )
    }

    fn render_tool_row(
        icon: Icon,
        name: impl Into<SharedString>,
        description: Option<SharedString>,
        permission: (SharedString, Color),
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
                    .size(px(26.))
                    .justify_center()
                    .rounded(px(7.))
                    .bg(colors.element_selected)
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
                    (t!("agent_ui.task_agents.permission.allow"), Color::Success)
                }
                None => (
                    t!("agent_ui.task_agents.permission.confirm"),
                    Color::Warning,
                ),
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
                    Label::new(t!("agent_ui.new_agent.tools.base_only"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }
        Self::render_column(cx)
            .child(
                Self::render_column_header(t!("agent_ui.new_agent.tools.title"), cx).child(
                    div()
                        .id("new-agent-permissions-help")
                        .tooltip(Tooltip::text(t!(
                            "agent_ui.new_agent.tools.permissions_help"
                        )))
                        .child(
                            Label::new(t!("agent_ui.new_agent.tools.why"))
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
        Self::render_column(cx)
            .child(Self::render_column_header(
                t!("agent_ui.new_agent.output.title"),
                cx,
            ))
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
                        t!("agent_ui.new_agent.output.destination"),
                        h_flex().pt_0p5().child(
                            segmented_control(cx)
                                .w(px(200.))
                                .child(
                                    segment_option(
                                        "new-agent-project",
                                        t!("agent_ui.new_agent.output.project"),
                                        !personal,
                                        true,
                                        cx,
                                    )
                                    .tooltip(Tooltip::text(t!(
                                        "agent_ui.new_agent.output.project_tooltip"
                                    )))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.personal = false;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    segment_option(
                                        "new-agent-personal",
                                        t!("agent_ui.new_agent.output.personal"),
                                        personal,
                                        true,
                                        cx,
                                    )
                                    .tooltip(Tooltip::text(t!(
                                        "agent_ui.new_agent.output.personal_tooltip"
                                    )))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.personal = true;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                    ))
                    .child(Self::render_value(
                        t!("agent_ui.new_agent.output.save_to"),
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
            Start::Blank => t!("agent_ui.new_agent.badge.blank"),
            Start::Template(_) => t!("agent_ui.new_agent.badge.template"),
            Start::Imported(_) => t!("agent_ui.new_agent.badge.imported"),
            Start::Generated => t!("agent_ui.new_agent.badge.generated"),
        };
        let accent = Color::Accent.color(cx);
        h_flex()
            .gap_3()
            .child(Self::render_section_label(t!(
                "agent_ui.new_agent.suggestion"
            )))
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

/// What the agent is called with after `/`: no slash and no spaces, which the composer can't take.
fn normalize_command(text: &str) -> String {
    text.trim()
        .trim_start_matches('/')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

fn permission_color(mode: PermissionMode) -> Color {
    match mode {
        PermissionMode::Allow => Color::Success,
        PermissionMode::Confirm => Color::Warning,
        PermissionMode::Deny => Color::Error,
    }
}

fn permission_chip(label: SharedString, color: Color, cx: &App) -> impl IntoElement {
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
        modal_surface(cx)
            .key_context("NewTaskAgent")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::suggest))
            .on_action(cx.listener(Self::create))
            .on_action(cx.listener(Self::cancel))
            .w(px(1000.).min(viewport.width - px(64.)))
            .max_h(viewport.height - px(140.))
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
                            .child(
                                Headline::new(t!("agent_ui.task_agents.new_agent"))
                                    .size(HeadlineSize::Small),
                            )
                            .child(
                                Label::new(t!("agent_ui.new_agent.subtitle"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new("new-agent-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(t!("agent_ui.new_agent.close")))
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
                            .child(Self::render_section_label(t!(
                                "agent_ui.new_agent.start.title"
                            )))
                            .child(self.render_starts(cx)),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Self::render_section_label(t!(
                                "agent_ui.new_agent.description_title"
                            )))
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
                                        .child(self.render_identity(window, cx))
                                        .child(self.render_tools(cx))
                                        .child(self.render_output(cx)),
                                )
                            }),
                    ),
            )
            .child(
                modal_footer(cx)
                    .child(
                        Label::new(t!("agent_ui.new_agent.footer"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .flex_1(),
                    )
                    .child(
                        secondary_button(
                            "new-agent-create-and-run",
                            Some((IconName::PlayOutlined, Color::Default)),
                            t!("agent_ui.new_agent.create_and_run"),
                            !running,
                            cx,
                        )
                        .h(px(34.))
                        .rounded(px(8.))
                        .tooltip(Tooltip::text(t!(
                            "agent_ui.new_agent.create_and_run_tooltip"
                        )))
                        .when(!running, |this| {
                            this.on_click(cx.listener(|this, _, window, cx| {
                                this.create_suggested(true, window, cx)
                            }))
                        }),
                    )
                    .child(
                        primary_button(
                            "new-agent-create",
                            None,
                            t!("agent_ui.new_agent.create"),
                            Some(ui::KeyBinding::for_action_in(
                                &CreateSuggestedAgent,
                                &create_binding_focus,
                                cx,
                            )),
                            cx,
                        )
                        .h(px(34.))
                        .rounded(px(8.))
                        .when(running, |this| this.opacity(0.5).cursor_default())
                        .when(!running, |this| {
                            this.on_click(cx.listener(|this, _, window, cx| {
                                this.create(&CreateSuggestedAgent, window, cx)
                            }))
                        }),
                    ),
            )
    }
}
