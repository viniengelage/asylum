use gpui::{App, SharedString, Window};

/// What a CloudWatch view hands to the agent's chat.
#[derive(Clone, Debug)]
pub(crate) enum AgentContext {
    LogGroup { name: String },
    Query { path: Option<std::path::PathBuf>, query: String, log_groups: Vec<String> },
    Event { log_group: Option<String>, message: String, fields: Vec<(String, String)> },
    TailWindow { log_groups: Vec<String>, lines: Vec<String> },
}

/// Mentions `context` in the agent panel's message editor, with `prompt` after it; `submit`
/// sends it right away.
pub(crate) fn send_to_agent(
    _context: AgentContext,
    _title: SharedString,
    _prompt: Option<String>,
    _submit: bool,
    _window: &mut Window,
    _cx: &mut App,
) {
}
