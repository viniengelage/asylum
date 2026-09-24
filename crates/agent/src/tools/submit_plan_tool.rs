use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Present the implementation plan to the user for review.
///
/// Only call this in plan mode, once you have explored enough of the project to
/// propose concrete changes. The plan is rendered as a card in the conversation
/// with a button that lets the user approve and execute it. Do not change any
/// files before the user approves.
///
/// After calling this tool, end your turn. Do not repeat the plan in prose: the
/// user already sees it. If the user replies with feedback instead of approving,
/// revise the plan and call this tool again with the complete updated plan.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SubmitPlanToolInput {
    /// A short title for the plan, e.g. "Add biometric login to the PIN screen".
    #[serde(default)]
    pub title: String,
    /// One or two sentences describing the approach and the reasoning behind it.
    #[serde(default)]
    pub summary: String,
    /// The ordered steps needed to carry out the plan.
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// Decisions or unknowns the user should weigh in on before executing.
    /// Leave empty when there are none.
    #[serde(default)]
    pub open_questions: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlanStep {
    /// What this step does, written as an imperative sentence.
    #[serde(default)]
    pub title: String,
    /// Project-relative paths this step creates or changes.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubmitPlanToolOutput {
    Submitted { steps: usize },
    Error { error: String },
}

impl From<SubmitPlanToolOutput> for LanguageModelToolResultContent {
    fn from(value: SubmitPlanToolOutput) -> Self {
        match value {
            SubmitPlanToolOutput::Submitted { steps } => format!(
                "The plan ({steps} steps) is now shown to the user, who will either approve it \
                 or reply with changes. End your turn now without restating the plan."
            )
            .into(),
            SubmitPlanToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct SubmitPlanTool;

impl AgentTool for SubmitPlanTool {
    type Input = SubmitPlanToolInput;
    type Output = SubmitPlanToolOutput;

    const NAME: &'static str = "submit_plan";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) if !input.title.is_empty() => SharedString::from(input.title),
            _ => "Writing plan".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |_cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| SubmitPlanToolOutput::Error {
                    error: error.to_string(),
                })?;

            if input.steps.is_empty() {
                return Err(SubmitPlanToolOutput::Error {
                    error: "The plan needs at least one step.".to_string(),
                });
            }

            Ok(SubmitPlanToolOutput::Submitted {
                steps: input.steps.len(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_input_deserializes_with_defaults() {
        // The plan card renders from the raw input while it is still streaming,
        // so a plan with missing fields must still deserialize.
        let input: SubmitPlanToolInput = serde_json::from_value(serde_json::json!({
            "title": "Add biometric login",
            "steps": [{ "title": "Install the dependency" }],
        }))
        .expect("partial plan should deserialize");

        assert_eq!(input.title, "Add biometric login");
        assert!(input.summary.is_empty());
        assert_eq!(input.steps.len(), 1);
        assert!(input.steps[0].files.is_empty());
        assert!(input.open_questions.is_empty());
    }
}
