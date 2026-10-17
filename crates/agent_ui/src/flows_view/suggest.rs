//! "Sugerir passos": the default model writes the steps for "O que construir" with the user's
//! agents. What the parser or the validation rejects goes back to the model, a couple of times.

use super::FlowView;
use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use gpui::Window;
use language_model::{
    CompletionIntent, LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use ui::prelude::*;
use util::ResultExt as _;

const MAX_SUGGESTION_REPAIRS: usize = 2;

impl FlowView {
    pub(super) fn suggest_steps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            self.show_toast("Pare a execução antes de mudar os passos.", cx);
            return;
        }
        let request = self.input_editor.read(cx).text(cx);
        if request.trim().is_empty() {
            self.show_toast(
                "Escreva em “O que construir” o que a esteira deve entregar.",
                cx,
            );
            return;
        }
        let registry = LanguageModelRegistry::read_global(cx);
        let Some((model, provider)) = registry.default_model().and_then(|model| {
            registry
                .provider_for_model(&model)
                .log_err()
                .map(|provider| (model, provider))
        }) else {
            self.show_toast(
                "Escolha um modelo no painel do Agent para sugerir passos.",
                cx,
            );
            return;
        };
        let agents = self.agents.clone();
        let id = self.flow.id.clone();
        let source = self.flow.source.clone();
        let name = self.flow.name.clone();
        let mut messages = vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![flows::suggestion_system_prompt(&agents).into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![flows::suggestion_user_prompt(&name, &request).into()],
                cache: false,
                reasoning_details: None,
            },
        ];
        self.suggesting = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<flows::Flow> = async {
                let mut attempt = 0;
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
                    }
                    match flows::review_suggestion(&text, &id, source.clone(), &agents) {
                        Ok(flow) => return Ok(flow),
                        Err(problems) if attempt < MAX_SUGGESTION_REPAIRS => {
                            attempt += 1;
                            messages.push(LanguageModelRequestMessage {
                                role: Role::Assistant,
                                content: vec![text.into()],
                                cache: false,
                                reasoning_details: None,
                            });
                            messages.push(LanguageModelRequestMessage {
                                role: Role::User,
                                content: vec![flows::suggestion_repair_prompt(&problems).into()],
                                cache: false,
                                reasoning_details: None,
                            });
                        }
                        Err(problems) => return Err(anyhow!(problems.join("; "))),
                    }
                }
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.suggesting = None;
                match result {
                    Ok(suggestion) => this.apply_suggestion(suggestion, window, cx),
                    Err(error) => {
                        this.show_toast(&format!("Não foi possível sugerir: {error:#}"), cx);
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    /// Replaces the steps with the suggestion, with an undo in the toast.
    fn apply_suggestion(
        &mut self,
        suggestion: flows::Flow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.flow.steps.clone();
        let count = suggestion.steps.len();
        let changed = self.edit_flow(cx, |flow| {
            flow.steps = suggestion.steps;
            if flow.notes.trim().is_empty() {
                flow.notes = suggestion.notes;
            }
        });
        if !changed {
            return;
        }
        self.select_step(Some(0), window, cx);
        let request = self.input_editor.read(cx).text(cx);
        let short: String = request.chars().take(40).collect();
        let view = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                use workspace::notifications::NotificationId;
                struct SuggestedSteps;
                workspace.show_toast(
                    workspace::Toast::new(
                        NotificationId::unique::<SuggestedSteps>(),
                        format!("Sugestão para “{short}”: {count} passos. Edite à vontade."),
                    )
                    .on_click("Desfazer", move |window, cx| {
                        let previous = previous.clone();
                        view.update(cx, |view, cx| {
                            if view.edit_flow(cx, |flow| flow.steps = previous) {
                                view.select_step(Some(0), window, cx);
                            }
                        })
                        .log_err();
                    }),
                    cx,
                );
            })
            .log_err();
    }
}
