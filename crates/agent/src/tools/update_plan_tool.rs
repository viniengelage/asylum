use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Report progress while executing a plan the user approved.
///
/// Use this only after the user executed a plan you proposed with
/// `submit_plan`. Send every step of that plan, in the same order, with its
/// current status. Mark a step `in_progress` right before you start working
/// on it and `completed` as soon as it is done, so the user can follow along.
/// Keep exactly one step `in_progress` while you work.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpdatePlanToolInput {
    /// Every step of the approved plan, in order.
    #[serde(default)]
    pub steps: Vec<PlanStepProgress>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlanStepProgress {
    /// The step's title, as it appears in the plan.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: PlanStepStatus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UpdatePlanToolOutput {
    Updated { completed: usize, total: usize },
    Error { error: String },
}

impl From<UpdatePlanToolOutput> for LanguageModelToolResultContent {
    fn from(value: UpdatePlanToolOutput) -> Self {
        match value {
            UpdatePlanToolOutput::Updated { completed, total } => {
                format!("Progress updated: {completed} of {total} steps completed.").into()
            }
            UpdatePlanToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct UpdatePlanTool;

impl AgentTool for UpdatePlanTool {
    type Input = UpdatePlanToolInput;
    type Output = UpdatePlanToolOutput;

    const NAME: &'static str = "update_plan";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        let Ok(input) = input else {
            return "Updating plan".into();
        };
        let completed = input
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count();
        format!("Plan progress: {completed} of {}", input.steps.len()).into()
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
                .map_err(|error| UpdatePlanToolOutput::Error {
                    error: error.to_string(),
                })?;

            if input.steps.is_empty() {
                return Err(UpdatePlanToolOutput::Error {
                    error: "Send every step of the approved plan with its status.".to_string(),
                });
            }

            let completed = input
                .steps
                .iter()
                .filter(|step| step.status == PlanStepStatus::Completed)
                .count();
            Ok(UpdatePlanToolOutput::Updated {
                completed,
                total: input.steps.len(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_use_snake_case() {
        let input: UpdatePlanToolInput = serde_json::from_value(serde_json::json!({
            "steps": [
                { "title": "Install", "status": "completed" },
                { "title": "Hook", "status": "in_progress" },
                { "title": "Tests" },
            ],
        }))
        .expect("progress should deserialize");

        let statuses = input
            .steps
            .iter()
            .map(|step| step.status)
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            [
                PlanStepStatus::Completed,
                PlanStepStatus::InProgress,
                PlanStepStatus::Pending
            ]
        );
    }
}
