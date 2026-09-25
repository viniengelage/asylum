use crate::{AgentToolOutput, AnyAgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, anyhow};
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Entity, SharedString, Task};
use language_model::{LanguageModelImage, LanguageModelImageExt as _, LanguageModelToolResultContent};
use project::Project;
use std::sync::Arc;
use task_agents::{ToolAccess, ToolkitCall, ToolkitContent, ToolkitTool};

/// Exposes a tool registered by another crate (devices, browser, API client…) to the model.
pub struct ToolkitToolAdapter {
    project: Entity<Project>,
    toolkit_id: SharedString,
    tool: ToolkitTool,
}

impl ToolkitToolAdapter {
    pub fn new(project: Entity<Project>, toolkit_id: SharedString, tool: ToolkitTool) -> Self {
        Self {
            project,
            toolkit_id,
            tool,
        }
    }

    pub fn toolkit_id(&self) -> &SharedString {
        &self.toolkit_id
    }
}

fn summarize_input(input: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(map) = input else {
        return None;
    };
    let parts = map
        .iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(text) if !text.is_empty() => {
                let text: String = text.chars().take(60).collect();
                Some(format!("{key}: {text}"))
            }
            serde_json::Value::Number(number) => Some(format!("{key}: {number}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl AnyAgentTool for ToolkitToolAdapter {
    fn name(&self) -> SharedString {
        self.tool.name.clone()
    }

    fn description(&self) -> SharedString {
        self.tool.description.clone()
    }

    fn kind(&self) -> acp::ToolKind {
        match self.tool.access {
            ToolAccess::Read => acp::ToolKind::Read,
            ToolAccess::Act => acp::ToolKind::Execute,
        }
    }

    fn initial_title(&self, input: serde_json::Value, _cx: &mut App) -> SharedString {
        match summarize_input(&input) {
            Some(summary) => format!("{} ({summary})", self.tool.title).into(),
            None => self.tool.title.clone(),
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = self.tool.input_schema.clone();
        language_model::tool_schema::normalize_tool_schema(&mut schema);
        schema
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<serde_json::Value>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<AgentToolOutput, AgentToolOutput>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| anyhow!("{error}"))?;
            let inputs = ToolkitTool::permission_inputs(&input);
            let title = cx.update(|cx| self.initial_title(input.clone(), cx));

            let authorization = cx.update(|cx| {
                let context = ToolPermissionContext::new(self.tool.name.to_string(), inputs);
                event_stream.authorize_toolkit_tool(
                    title.to_string(),
                    context,
                    self.tool.access == ToolAccess::Act,
                    cx,
                )
            });
            futures::select! {
                result = authorization.fuse() => result?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(anyhow!("Cancelado pelo usuário").into());
                }
            }

            let run = cx.update(|cx| {
                self.tool.run(
                    ToolkitCall {
                        project: self.project.clone(),
                        input,
                    },
                    cx,
                )
            });
            let output = futures::select! {
                output = run.fuse() => output?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(anyhow!("Cancelado pelo usuário").into());
                }
            };

            let mut llm_output = Vec::new();
            let mut tool_call_content = Vec::new();
            for content in &output.content {
                match content {
                    ToolkitContent::Text(text) => {
                        tool_call_content.push(acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
                        )));
                        llm_output.push(LanguageModelToolResultContent::Text(text.as_str().into()));
                    }
                    ToolkitContent::Image { base64_png } => {
                        tool_call_content.push(acp::ToolCallContent::Content(acp::Content::new(
                            acp::ContentBlock::Image(acp::ImageContent::new(
                                base64_png.clone(),
                                "image/png",
                            )),
                        )));
                        let image = cx
                            .background_spawn({
                                let base64_png = base64_png.clone();
                                async move {
                                    LanguageModelImage::from_base64_image(&base64_png, "image/png")
                                }
                            })
                            .await;
                        match image {
                            Ok(Some(image)) => {
                                llm_output.push(LanguageModelToolResultContent::Image(image))
                            }
                            Ok(None) => log::warn!("toolkit screenshot could not be converted"),
                            Err(error) => {
                                log::warn!("toolkit screenshot could not be converted: {error:#}")
                            }
                        }
                    }
                }
            }
            if llm_output.is_empty() {
                llm_output.push(LanguageModelToolResultContent::Text("OK".into()));
            }
            if !tool_call_content.is_empty() {
                event_stream
                    .update_fields(acp::ToolCallUpdateFields::new().content(tool_call_content));
            }
            Ok(AgentToolOutput {
                raw_output: serde_json::Value::String(output.text_content()),
                llm_output,
            })
        })
    }

    fn replay(
        &self,
        _input: serde_json::Value,
        _output: serde_json::Value,
        _event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        Ok(())
    }
}
