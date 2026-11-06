use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, AppContext as _, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::model_benchmarks::{BenchmarkRow, ModelBenchmarksSource};
use crate::{
    AgentTool, AutomaticSubagentContext, AvailableAgents, ThreadEnvironment, ToolCallEventStream,
    ToolInput,
};

/// List the agents and models available for use with the `create_thread` and
/// `spawn_agent` tools.
///
/// The result may include external agents. The `spawn_agent` tool creates a
/// native Zed subagent, so its `model` must be an exact `models[].id` from the
/// agent entry where `is_native` is `true`.
///
/// Call this before either tool if you need to pick a specific agent or a
/// non-default model (for example, to use a cheaper model for bulk work). If
/// you're happy with the user's current defaults, you don't need to call this.
///
/// When the user turned on automatic subagent selection, the result also has
/// `automatic_selection`: the user's benchmark rows for each native model and the
/// rules to pick the model and effort of every new subagent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ListAgentsAndModelsToolInput {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ListAgentsAndModelsToolOutput {
    Success {
        #[serde(flatten)]
        agents: AvailableAgents,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        automatic_selection: Option<AutomaticSelectionInfo>,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomaticSelectionInfo {
    pub picks_model: bool,
    pub picks_effort: bool,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rules: String,
    pub benchmarks_file: String,
    pub models: Vec<ModelBenchmarkRows>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_without_rows: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelBenchmarkRows {
    pub id: String,
    pub rows: Vec<BenchmarkRow>,
}

pub(crate) fn automatic_selection_info(
    context: &AutomaticSubagentContext,
    native_model_ids: &[String],
) -> AutomaticSelectionInfo {
    let benchmarks = &context.benchmarks.benchmarks;
    let mut models = Vec::new();
    let mut models_without_rows = Vec::new();
    for model_id in native_model_ids {
        let rows: Vec<BenchmarkRow> = benchmarks.rows_for_model(model_id).cloned().collect();
        if rows.is_empty() {
            models_without_rows.push(model_id.clone());
        } else {
            models.push(ModelBenchmarkRows {
                id: model_id.clone(),
                rows,
            });
        }
    }

    let picks = match (context.selection.model, context.selection.effort) {
        (true, true) => "the model and the effort",
        (true, false) => "the model",
        _ => "the effort",
    };
    let mut instructions = format!(
        "The user turned on automatic subagent selection. For every new spawn_agent call, pick {picks} \
         from the rows in `models`: the cheapest row that can finish that subtask without rework, \
         following `rules` and each row's `good_for` and `avoid_for`."
    );
    if context.selection.model {
        instructions.push_str(" Pass the model's exact `id` in `model`.");
    }
    if context.selection.effort {
        instructions.push_str(" Pass the row's `effort` in `effort`.");
    }
    instructions.push_str(
        " Always pass `model_reason`: a few words in the user's language on why this row fits the \
         subtask (for example, the kind of work and what it doesn't need). The user sees it on the \
         subagent's card.",
    );
    if !models_without_rows.is_empty() {
        instructions.push_str(" Models in `models_without_rows` can't be picked.");
    }

    let benchmarks_file = match &context.benchmarks.source {
        ModelBenchmarksSource::Default => "the app's default benchmarks".to_string(),
        ModelBenchmarksSource::User(path) | ModelBenchmarksSource::Project(path) => {
            path.display().to_string()
        }
    };

    AutomaticSelectionInfo {
        picks_model: context.selection.model,
        picks_effort: context.selection.effort,
        instructions,
        rules: benchmarks.rules.clone(),
        benchmarks_file,
        models,
        models_without_rows,
    }
}

pub(crate) fn native_model_ids(agents: &AvailableAgents) -> Vec<String> {
    agents
        .agents
        .iter()
        .filter(|agent| agent.is_native)
        .flat_map(|agent| agent.models.iter().map(|model| model.id.clone()))
        .collect()
}

impl From<ListAgentsAndModelsToolOutput> for LanguageModelToolResultContent {
    fn from(output: ListAgentsAndModelsToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize list_agents_and_models output: {e}"))
            .into()
    }
}

pub struct ListAgentsAndModelsTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl ListAgentsAndModelsTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for ListAgentsAndModelsTool {
    type Input = ListAgentsAndModelsToolInput;
    type Output = ListAgentsAndModelsToolOutput;

    const NAME: &'static str = "list_agents_and_models";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List agents and models".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let agents = match self.environment.list_available_agents(cx) {
            Ok(agents) => agents,
            Err(error) => {
                return Task::ready(Err(ListAgentsAndModelsToolOutput::Error {
                    error: error.to_string(),
                }));
            }
        };
        let automatic = self.environment.automatic_subagent_selection(cx);
        cx.background_spawn(async move {
            let automatic_selection = automatic
                .await
                .map(|context| automatic_selection_info(&context, &native_model_ids(&agents)));
            Ok(ListAgentsAndModelsToolOutput::Success {
                agents,
                automatic_selection,
            })
        })
    }
}
