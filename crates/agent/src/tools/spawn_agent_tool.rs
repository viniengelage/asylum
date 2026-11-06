use acp_thread::{SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo};
use agent_client_protocol::schema::v2 as acp_v2;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use acp_thread::AgentModelId;

use crate::tools::list_agents_and_models_tool::{automatic_selection_info, native_model_ids};
use crate::{
    AgentTool, AutomaticSubagentContext, ThreadEnvironment, ToolCallEventStream, ToolInput,
};

/// Spawn a sub-agent for a well-scoped task.
///
/// ### Designing delegated subtasks
/// - An agent does not see your conversation history. Include all relevant context (file paths, requirements, constraints) in the message.
/// - Subtasks must be concrete, well-defined, and self-contained.
/// - Delegated subtasks must materially advance the main task.
/// - Do not duplicate work between your work and delegated subtasks.
/// - Do not use this tool for tasks you could accomplish directly with one or two tool calls. For example, don't ask the agent to read a single file and return the contents, you can do this yourself.
/// - When you delegate work, focus on coordinating and synthesizing results instead of duplicating the same work yourself.
/// - Avoid issuing multiple delegate calls for the same unresolved subproblem unless the new delegated task is genuinely different and necessary.
/// - Narrow the delegated ask to the concrete output you need next.
/// - For code-edit subtasks, decompose work so each delegated task has a disjoint write set.
/// - When sending a follow-up using an existing agent session_id, the agent already has the context from the previous turn. Send only a short, direct message. Do NOT repeat the original task or context.
///
/// ### Parallel delegation patterns
/// - Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.
/// - Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.
/// - When a plan has multiple independent steps, prefer delegating those steps in parallel rather than serializing them unnecessarily.
/// - Reuse the returned session_id when you want to follow up on the same delegated subproblem instead of creating a duplicate session.
///
/// ### Model selection
/// - When the user requests a particular model or asks you to choose based on cost or capability, call `list_agents_and_models` first, then pass the exact `models[].id` from the native Zed agent entry (`is_native: true`) in `model`.
/// - Omit `model` to use the user's configured subagent model, or the parent model when no subagent model is configured.
/// - Do not silently choose a different model when an explicit model is unavailable unless the user allowed fallback.
/// - A resumed session keeps its existing model, so `model` cannot be combined with `session_id`.
///
/// ### Automatic selection
/// - When the user turned on automatic subagent selection, call `list_agents_and_models` before the first spawn: its `automatic_selection` has the user's benchmark rows and rules.
/// - Pick the row as it says, and pass `model`, `effort` and `model_reason` on every new session.
///
/// ### Output
/// - You will receive only the agent's final message as output.
/// - Successful calls return a session_id that you can use for follow-up messages.
/// - Error results may also include a session_id if a session was already created.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SpawnAgentToolInput {
    /// Short label displayed in the UI while the agent runs (e.g., "Researching alternatives")
    pub label: String,
    /// The prompt for the agent. For new sessions, include full context needed for the task. For follow-ups (with session_id), you can rely on the agent already having the previous message.
    pub message: String,
    /// Session ID of an existing agent session to continue instead of creating a new one. Omit to create a new agent.
    #[serde(default, deserialize_with = "deserialize_session_id")]
    pub session_id: Option<acp_v2::SessionId>,
    /// Optional model override. Pass the exact `models[].id` returned for the
    /// native Zed agent (`is_native: true`) by `list_agents_and_models`.
    /// Omit to preserve default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Thinking effort of a new subagent, from the benchmark row picked under
    /// automatic selection (`low`, `medium`, `high`, `xhigh` or `max`). Ignored
    /// unless automatic selection picks the effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Under automatic selection, a few words in the user's language on why the
    /// picked model and effort fit this subtask. Shown on the subagent's card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reason: Option<String>,
}

// The error carries the user's table, so the agent can fix the call in one go.
pub(crate) fn check_automatic_choice(
    context: &AutomaticSubagentContext,
    available_model_ids: &[String],
    model: Option<&str>,
    effort: Option<&str>,
    model_reason: Option<&str>,
) -> Result<(), String> {
    let benchmarks = &context.benchmarks.benchmarks;
    let covered: Vec<&String> = available_model_ids
        .iter()
        .filter(|model_id| benchmarks.has_model(model_id))
        .collect();
    // Nothing in the file fits the models at hand, so the subagent takes the usual defaults.
    if covered.is_empty() {
        return Ok(());
    }

    let reject = |problem: String| {
        let info = automatic_selection_info(context, available_model_ids);
        let info = serde_json::to_string(&info).unwrap_or_default();
        Err(format!(
            "{problem} Automatic subagent selection is on; fix the call using this: {info}"
        ))
    };

    if context.selection.model {
        let Some(model) = model else {
            return reject("`model` is missing.".to_string());
        };
        if !benchmarks.has_model(model) {
            return reject(format!(
                "{model} isn't in the user's benchmarks file, so it can't be picked."
            ));
        }
    }
    if context.selection.effort {
        let Some(effort) = effort else {
            return reject("`effort` is missing.".to_string());
        };
        if let Some(model) = model
            && benchmarks.has_model(model)
            && benchmarks.row_for(model, Some(effort)).is_none()
        {
            return reject(format!(
                "{model} has no row for effort {effort} in the user's benchmarks file."
            ));
        }
    }
    if model_reason.is_none_or(|reason| reason.trim().is_empty()) {
        return reject("`model_reason` is missing.".to_string());
    }
    Ok(())
}

fn deserialize_session_id<'de, D>(deserializer: D) -> Result<Option<acp_v2::SessionId>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };

    if value
        .as_str()
        .is_some_and(|session_id| session_id.trim().is_empty())
    {
        return Ok(None);
    }

    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum SpawnAgentToolOutput {
    Success {
        session_id: acp_v2::SessionId,
        output: String,
        session_info: SubagentSessionInfo,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        session_id: Option<acp_v2::SessionId>,
        error: String,
        session_info: Option<SubagentSessionInfo>,
    },
}

impl From<SpawnAgentToolOutput> for LanguageModelToolResultContent {
    fn from(output: SpawnAgentToolOutput) -> Self {
        match output {
            SpawnAgentToolOutput::Success {
                session_id,
                output,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(
                &serde_json::json!({ "session_id": session_id, "output": output }),
            )
            .unwrap_or_else(|e| format!("Failed to serialize spawn_agent output: {e}"))
            .into(),
            SpawnAgentToolOutput::Error {
                session_id,
                error,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(
                &serde_json::json!({ "session_id": session_id, "error": error }),
            )
            .unwrap_or_else(|e| format!("Failed to serialize spawn_agent output: {e}"))
            .into(),
        }
    }
}

/// Tool that spawns an agent thread to work on a task.
pub struct SpawnAgentTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl SpawnAgentTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for SpawnAgentTool {
    type Input = SpawnAgentToolInput;
    type Output = SpawnAgentToolOutput;

    const NAME: &'static str = "spawn_agent";

    fn kind() -> acp_v2::ToolKind {
        acp_v2::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => i.label.into(),
            Err(value) => value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| SharedString::from(s.to_owned()))
                .unwrap_or_else(|| "Spawning agent".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| SpawnAgentToolOutput::Error {
                    session_id: None,
                    error: e.to_string(),
                    session_info: None,
                })?;

            let SpawnAgentToolInput {
                label,
                message,
                session_id,
                model,
                effort,
                model_reason,
            } = input;

            let automatic = if session_id.is_none() {
                cx.update(|cx| self.environment.automatic_subagent_selection(cx))
                    .await
            } else {
                None
            };
            let picks_effort = automatic
                .as_ref()
                .is_some_and(|context| context.selection.effort);
            if let Some(context) = automatic.as_ref() {
                let available_model_ids = cx
                    .update(|cx| self.environment.list_available_agents(cx))
                    .map(|agents| native_model_ids(&agents))
                    .unwrap_or_default();
                check_automatic_choice(
                    context,
                    &available_model_ids,
                    model.as_deref(),
                    effort.as_deref(),
                    model_reason.as_deref(),
                )
                .map_err(|error| SpawnAgentToolOutput::Error {
                    session_id: None,
                    error,
                    session_info: None,
                })?;
            }
            let effort = effort.filter(|_| picks_effort);

            let (subagent, mut session_info) = cx.update(|cx| {
                let subagent = match (session_id, model) {
                    (Some(_), Some(_)) => Err(anyhow::anyhow!(
                        "model cannot be changed when resuming a subagent session"
                    )),
                    (Some(session_id), None) => self.environment.resume_subagent(session_id, cx),
                    (None, model) => self.environment.create_subagent(
                        label,
                        model.map(AgentModelId::from),
                        effort,
                        cx,
                    ),
                };
                let subagent = subagent.map_err(|err| SpawnAgentToolOutput::Error {
                    session_id: None,
                    error: err.to_string(),
                    session_info: None,
                })?;
                let session_info = SubagentSessionInfo {
                    session_id: subagent.id(),
                    message_start_index: subagent.num_entries(cx),
                    message_end_index: None,
                };

                event_stream.update_fields(|update| {
                    update.meta(acp_v2::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        serde_json::json!(&session_info),
                    )]))
                });

                Ok((subagent, session_info))
            })?;

            let send_result = subagent.send(message, cx).await;

            let status = if send_result.is_ok() {
                "completed"
            } else {
                "error"
            };
            telemetry::event!(
                "Subagent Completed",
                subagent_session = session_info.session_id.to_string(),
                status,
            );

            session_info.message_end_index =
                cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

            let meta = Some(acp_v2::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )]));

            let (output, result) = match send_result {
                Ok(output) => (
                    output.clone(),
                    Ok(SpawnAgentToolOutput::Success {
                        session_id: session_info.session_id.clone(),
                        session_info,
                        output,
                    }),
                ),
                Err(e) => {
                    let error = e.to_string();
                    (
                        error.clone(),
                        Err(SpawnAgentToolOutput::Error {
                            session_id: Some(session_info.session_id.clone()),
                            error,
                            session_info: Some(session_info),
                        }),
                    )
                }
            };
            event_stream.update_fields(|update| {
                let update = update.content(vec![output.into()]);
                match meta {
                    Some(meta) => update.meta(meta),
                    None => update,
                }
            });
            result
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        let (content, session_info) = match output {
            SpawnAgentToolOutput::Success {
                output,
                session_info,
                ..
            } => (output.into(), Some(session_info)),
            SpawnAgentToolOutput::Error {
                error,
                session_info,
                ..
            } => (error.into(), session_info),
        };

        let meta = session_info.map(|session_info| {
            acp_v2::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )])
        });
        event_stream.update_fields(|update| {
            let update = update.content(vec![content]);
            match meta {
                Some(meta) => update.meta(meta),
                None => update,
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_ids_preserve_legacy_tool_json() -> Result<(), serde_json::Error> {
        let session_id = "  session/雪:\"quoted\"\\opaque  ";
        let input_json = json!({
            "label": "label",
            "message": "message",
            "session_id": session_id,
        });
        let input: SpawnAgentToolInput = serde_json::from_value(input_json.clone())?;
        assert_eq!(
            input.session_id.as_ref().map(|id| id.0.as_ref()),
            Some(session_id)
        );
        assert_eq!(serde_json::to_value(input)?, input_json);

        let output_json = json!({
            "session_id": session_id,
            "error": "failed after session creation",
            "session_info": null,
        });
        let output: SpawnAgentToolOutput = serde_json::from_value(output_json.clone())?;
        assert_eq!(serde_json::to_value(output)?, output_json);
        Ok(())
    }

    fn automatic_context(model: bool, effort: bool) -> AutomaticSubagentContext {
        let benchmarks = crate::model_benchmarks::ModelBenchmarks::parse(
            "## Modelos\n| id | effort | tier |\n|---|---|---|\n| anthropic/claude-haiku-5-5 | medium | baixo |\n| anthropic/claude-sonnet-5-5 | high | médio |\n\n## Regras\n- Seja barato.\n",
        );
        AutomaticSubagentContext {
            selection: crate::AutomaticSubagentSelection { model, effort },
            benchmarks: crate::model_benchmarks::LoadedModelBenchmarks {
                source: crate::model_benchmarks::ModelBenchmarksSource::Default,
                warnings: Vec::new(),
                benchmarks,
                fallback: None,
            },
        }
    }

    #[test]
    fn automatic_choice_must_come_from_the_benchmarks() {
        let available = vec![
            "claude-subscription/claude-haiku-5-5".to_string(),
            "claude-subscription/claude-sonnet-5-5".to_string(),
            "openai/gpt-6-astra".to_string(),
        ];
        let context = automatic_context(true, true);
        let check = |model, effort, reason| {
            check_automatic_choice(&context, &available, model, effort, reason)
        };

        assert_eq!(
            check(
                Some("claude-subscription/claude-haiku-5-5"),
                Some("medium"),
                Some("busca em arquivos")
            ),
            Ok(())
        );

        let missing_model = check(None, Some("medium"), Some("busca")).unwrap_err();
        assert!(missing_model.starts_with("`model` is missing."));
        assert!(missing_model.contains("claude-haiku-5-5"));
        assert!(missing_model.contains("Seja barato"));

        let not_in_file = check(Some("openai/gpt-6-astra"), Some("high"), Some("x")).unwrap_err();
        assert!(not_in_file.starts_with("openai/gpt-6-astra isn't in the user's benchmarks file"));

        let wrong_effort = check(
            Some("claude-subscription/claude-sonnet-5-5"),
            Some("max"),
            Some("x"),
        )
        .unwrap_err();
        assert!(wrong_effort.contains("has no row for effort max"));

        let no_reason = check(
            Some("claude-subscription/claude-sonnet-5-5"),
            Some("high"),
            Some("  "),
        )
        .unwrap_err();
        assert!(no_reason.starts_with("`model_reason` is missing."));
    }

    #[test]
    fn automatic_effort_alone_keeps_the_configured_model() {
        let available = vec!["anthropic/claude-sonnet-5-5".to_string()];
        let context = automatic_context(false, true);
        assert_eq!(
            check_automatic_choice(&context, &available, None, Some("high"), Some("testes")),
            Ok(())
        );
        assert!(
            check_automatic_choice(&context, &available, None, None, Some("testes"))
                .unwrap_err()
                .starts_with("`effort` is missing.")
        );
    }

    #[test]
    fn automatic_choice_is_skipped_when_no_model_has_rows() {
        let available = vec!["openai/gpt-6-astra".to_string()];
        let context = automatic_context(true, true);
        assert_eq!(
            check_automatic_choice(&context, &available, None, None, None),
            Ok(())
        );
    }

    #[test]
    fn deserializes_blank_session_id_as_absent() {
        for session_id in [json!(null), json!(""), json!("   ")] {
            let input: SpawnAgentToolInput = serde_json::from_value(json!({
                "label": "label",
                "message": "message",
                "session_id": session_id,
            }))
            .unwrap();

            assert!(input.session_id.is_none());
        }

        let input: SpawnAgentToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
        }))
        .unwrap();
        assert!(input.session_id.is_none());

        let input: SpawnAgentToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
            "session_id": "existing-session",
        }))
        .unwrap();
        assert_eq!(input.session_id.unwrap().to_string(), "existing-session");
    }
}
