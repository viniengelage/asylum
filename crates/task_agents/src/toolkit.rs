use anyhow::Result;
use base64::Engine as _;
use gpui::{App, Entity, Global, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// Whether a tool only looks at things or changes them. Acting tools go through the tool
/// permission prompt; reading tools run without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccess {
    Read,
    Act,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolkitContent {
    Text(String),
    Image { base64_png: String },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolkitOutput {
    pub content: Vec<ToolkitContent>,
}

impl ToolkitOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolkitContent::Text(text.into())],
        }
    }

    pub fn push_text(&mut self, text: impl Into<String>) {
        self.content.push(ToolkitContent::Text(text.into()));
    }

    pub fn push_png(&mut self, png: &[u8]) {
        self.content.push(ToolkitContent::Image {
            base64_png: base64::engine::general_purpose::STANDARD.encode(png),
        });
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                ToolkitContent::Text(text) => Some(text.as_str()),
                ToolkitContent::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A screenshot scaled down so its longest side is at most `max_side`, with the factor that maps
/// its pixels back to the source. Tools that take coordinates use the same factor, so the model
/// can point at what it saw.
pub struct ScaledScreenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

pub const SCREENSHOT_MAX_SIDE: u32 = 1280;

pub fn screenshot_scale(width: u32, height: u32) -> f64 {
    let longest = width.max(height).max(1);
    if longest <= SCREENSHOT_MAX_SIDE {
        1.0
    } else {
        f64::from(SCREENSHOT_MAX_SIDE) / f64::from(longest)
    }
}

pub fn scale_screenshot(image: image::DynamicImage) -> Result<ScaledScreenshot> {
    let scale = screenshot_scale(image.width(), image.height());
    let image = if scale < 1.0 {
        let width = (f64::from(image.width()) * scale).round().max(1.0) as u32;
        let height = (f64::from(image.height()) * scale).round().max(1.0) as u32;
        image.resize_exact(width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(ScaledScreenshot {
        png,
        width: image.width(),
        height: image.height(),
        scale,
    })
}

pub struct ToolkitCall {
    pub project: Entity<Project>,
    pub input: serde_json::Value,
}

type RunTool = Arc<dyn Fn(ToolkitCall, &mut App) -> Task<Result<ToolkitOutput>> + Send + Sync>;

#[derive(Clone)]
pub struct ToolkitTool {
    pub name: SharedString,
    /// Short Portuguese label for the tool call header, e.g. "Tocou na tela".
    pub title: SharedString,
    pub description: SharedString,
    pub input_schema: serde_json::Value,
    pub access: ToolAccess,
    run: RunTool,
}

impl ToolkitTool {
    pub fn new<I>(
        name: impl Into<SharedString>,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        access: ToolAccess,
        run: impl Fn(Entity<Project>, I, &mut App) -> Task<Result<ToolkitOutput>> + Send + Sync + 'static,
    ) -> Self
    where
        I: JsonSchema + DeserializeOwned + 'static,
    {
        let input_schema = serde_json::to_value(schemars::schema_for!(I))
            .unwrap_or_else(|_| serde_json::json!({ "type": "object", "properties": {} }));
        Self {
            name: name.into(),
            title: title.into(),
            description: description.into(),
            input_schema,
            access,
            run: Arc::new(move |call, cx| match serde_json::from_value::<I>(call.input) {
                Ok(input) => run(call.project, input, cx),
                Err(error) => Task::ready(Err(anyhow::anyhow!("invalid input: {error}"))),
            }),
        }
    }

    pub fn run(&self, call: ToolkitCall, cx: &mut App) -> Task<Result<ToolkitOutput>> {
        (self.run)(call, cx)
    }

    /// The input values tool permission patterns are matched against: every string in the
    /// input's top level, in order.
    pub fn permission_inputs(input: &serde_json::Value) -> Vec<String> {
        match input {
            serde_json::Value::Object(map) => map
                .values()
                .filter_map(|value| match value {
                    serde_json::Value::String(text) => Some(text.clone()),
                    serde_json::Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

pub struct Toolkit {
    pub id: SharedString,
    pub name: SharedString,
    pub description: SharedString,
    /// An `IconName` in snake case, resolved by the UI.
    pub icon: SharedString,
    pub tools: Vec<ToolkitTool>,
}

#[derive(Default)]
pub struct ToolkitRegistry {
    toolkits: Vec<Arc<Toolkit>>,
}

impl Global for ToolkitRegistry {}

/// Makes a toolkit's tools available to agent threads. Registering the same id again replaces
/// the earlier toolkit.
pub fn register_toolkit(toolkit: Toolkit, cx: &mut App) {
    let registry = cx.default_global::<ToolkitRegistry>();
    registry.toolkits.retain(|existing| existing.id != toolkit.id);
    registry.toolkits.push(Arc::new(toolkit));
}

pub fn toolkits(cx: &App) -> Vec<Arc<Toolkit>> {
    cx.try_global::<ToolkitRegistry>()
        .map(|registry| registry.toolkits.clone())
        .unwrap_or_default()
}

pub fn toolkit_for_tool(tool_name: &str, cx: &App) -> Option<Arc<Toolkit>> {
    toolkits(cx)
        .into_iter()
        .find(|toolkit| toolkit.tools.iter().any(|tool| tool.name == tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_scale() {
        assert_eq!(screenshot_scale(800, 600), 1.0);
        let scale = screenshot_scale(1080, 2400);
        assert!((scale - 1280.0 / 2400.0).abs() < 1e-9);
    }

    #[test]
    fn test_scale_screenshot_keeps_aspect() {
        let image = image::DynamicImage::new_rgba8(1080, 2400);
        let scaled = scale_screenshot(image).expect("scaled");
        assert_eq!((scaled.width, scaled.height), (576, 1280));
        assert!(!scaled.png.is_empty());
    }

    #[test]
    fn test_permission_inputs() {
        let input = serde_json::json!({ "url": "http://localhost", "x": 10, "full": true });
        let mut inputs = ToolkitTool::permission_inputs(&input);
        inputs.sort();
        assert_eq!(inputs, vec!["10".to_string(), "http://localhost".to_string()]);
    }
}
