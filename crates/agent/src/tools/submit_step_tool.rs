use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Hand your step of the flow ("esteira") to the next one.
///
/// Call this once, when the work of your step is done. Fill `outputs` with every output the step
/// asked for, by its exact name, and check each acceptance criterion assigned to you. Then end
/// your turn: the flow verifies the criteria and moves on, or sends the step back with the reason.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SubmitStepToolInput {
    /// What you did, in one or two sentences.
    #[serde(default)]
    pub summary: String,
    /// Each expected output by its exact name, with its content: a path, a link or a short text.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    /// Your own check of each acceptance criterion verified by you, by its number.
    #[serde(default)]
    pub criteria: Vec<StepSelfCheck>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepSelfCheck {
    /// The criterion's number, as listed in the step's instructions (1 is the first).
    #[serde(default)]
    pub number: usize,
    #[serde(default)]
    pub passed: bool,
    /// What shows it: a file, a command's result, a screen you checked.
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubmitStepToolOutput {
    Submitted { outputs: usize },
    Error { error: String },
}

impl From<SubmitStepToolOutput> for LanguageModelToolResultContent {
    fn from(value: SubmitStepToolOutput) -> Self {
        match value {
            SubmitStepToolOutput::Submitted { outputs } => format!(
                "Delivery recorded ({outputs} outputs). End your turn now; the flow verifies \
                 the criteria and continues."
            )
            .into(),
            SubmitStepToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct SubmitStepTool;

impl AgentTool for SubmitStepTool {
    type Input = SubmitStepToolInput;
    type Output = SubmitStepToolOutput;

    const NAME: &'static str = "submit_step";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Entrega do passo".into()
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
                .map_err(|error| SubmitStepToolOutput::Error {
                    error: error.to_string(),
                })?;
            if input.summary.trim().is_empty() && input.outputs.is_empty() {
                return Err(SubmitStepToolOutput::Error {
                    error: "Describe what you did in `summary` and fill `outputs`.".to_string(),
                });
            }
            Ok(SubmitStepToolOutput::Submitted {
                outputs: input.outputs.len(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_input_deserializes_with_defaults() {
        let input: SubmitStepToolInput = serde_json::from_value(serde_json::json!({
            "outputs": { "spec.md": "docs/specs/todo-list.md" },
            "criteria": [{ "number": 1, "passed": true }],
        }))
        .expect("partial submission should deserialize");
        assert!(input.summary.is_empty());
        assert_eq!(input.outputs.len(), 1);
        assert!(input.criteria[0].evidence.is_empty());
    }
}
