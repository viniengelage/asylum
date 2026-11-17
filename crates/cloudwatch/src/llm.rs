use anyhow::Result;
use gpui::{App, Task};

/// One request to the user's default model, outside any thread, returning the whole answer.
pub(crate) fn complete(_system: String, _prompt: String, _cx: &mut App) -> Task<Result<String>> {
    Task::ready(Err(anyhow::anyhow!("no model configured")))
}
