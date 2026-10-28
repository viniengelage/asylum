use crate::learnings::{
    MAX_LEARNING_FILE_SIZE, PROJECT_LEARNINGS_DIR, learning_file_name, personal_learnings_dir,
    render_learning, save_learning_file,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result};
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Saves a learning: a short note that is shown to you at the start of every future thread, so later sessions don't have to rediscover it.
///
/// Save a learning only for something non-obvious that a future session would otherwise get wrong: a decision the user made, an approach that was tried and rejected, a correction the user gave you, or a trap in the codebase. Don't save what the code, the project rules, or the existing learnings already say, and don't save details that only matter to the current task.
///
/// Saving a learning with the same title as an existing one replaces it. The user is always asked to approve the learning before it is saved.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SaveLearningToolInput {
    /// A short title that names the learning, e.g. "Run the sidebar tests with DYLD_FRAMEWORK_PATH".
    pub title: String,
    /// The learning itself, in Markdown. Keep it to a few lines: state the fact, then why it matters and how to apply it.
    pub content: String,
    /// Where to save the learning. `project` learnings are saved in the project's `.zed/learnings/` folder and are shared with the team when committed. `personal` learnings are private to the user and apply to every project.
    pub scope: LearningScope,
    /// For `project` learnings in projects with several root directories: the name of the root directory the learning belongs to.
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LearningScope {
    Project,
    Personal,
}

pub struct SaveLearningTool {
    project: Entity<Project>,
}

impl SaveLearningTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for SaveLearningTool {
    type Input = SaveLearningToolInput;
    type Output = String;

    const NAME: &'static str = "save_learning";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Save learning {}", MarkdownInlineCode(&input.title)).into(),
            Err(_) => "Save learning".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;
            save_learning(project, input, &event_stream, cx)
                .await
                .map_err(|error| format!("{error:#}"))
        })
    }
}

async fn save_learning(
    project: Entity<Project>,
    input: SaveLearningToolInput,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<String> {
    let text = render_learning(&input.title, &input.content);
    anyhow::ensure!(
        text.len() <= MAX_LEARNING_FILE_SIZE,
        "The learning is too long ({} bytes, the limit is {MAX_LEARNING_FILE_SIZE}). Keep it to a few lines.",
        text.len()
    );
    let file_name = learning_file_name(&input.title)?;
    let (path, display_path) = match input.scope {
        LearningScope::Personal => {
            let path = personal_learnings_dir().join(&file_name);
            let display_path = path.display().to_string();
            (path, display_path)
        }
        LearningScope::Project => {
            project_learning_path(&project, input.root.as_deref(), &file_name, cx)?
        }
    };

    event_stream.update_fields(|update| update.content(vec![text.clone().into()]));
    // Learnings are added to every future system prompt, so the user always
    // approves them: otherwise instructions picked up from untrusted content
    // could persist across threads.
    let authorize = cx.update(|cx| {
        event_stream.authorize_always_prompt(
            format!("Save learning to {}", MarkdownInlineCode(&display_path)),
            ToolPermissionContext::new(SaveLearningTool::NAME, vec![display_path.clone()])
                .once_only(),
            cx,
        )
    });
    futures::select! {
        result = authorize.fuse() => result?,
        _ = event_stream.cancelled_by_user().fuse() => {
            anyhow::bail!("Saving the learning was cancelled by the user");
        }
    }

    let fs = project.read_with(cx, |project, _| project.fs().clone());
    save_learning_file(fs.as_ref(), &path, &text).await?;
    Ok(match input.scope {
        LearningScope::Project => format!(
            "Saved the learning to `{display_path}`. It is shown in every thread for this project, and shared with the team once committed."
        ),
        LearningScope::Personal => format!(
            "Saved the learning to `{display_path}`. It is shown in every thread, in every project."
        ),
    })
}

fn project_learning_path(
    project: &Entity<Project>,
    root: Option<&str>,
    file_name: &str,
    cx: &AsyncApp,
) -> Result<(PathBuf, String)> {
    project.read_with(cx, |project, cx| {
        anyhow::ensure!(
            project.is_local(),
            "Project learnings can only be saved in local projects. Save it as a `personal` learning instead."
        );
        let mut worktrees = project
            .visible_worktrees(cx)
            .filter(|worktree| root.is_none_or(|root| worktree.read(cx).root_name_str() == root));
        let worktree = worktrees.next().with_context(|| match root {
            Some(root) => format!("There is no project root directory named `{root}`"),
            None => "The project has no root directory".to_string(),
        })?;
        anyhow::ensure!(
            worktrees.next().is_none(),
            "The project has several root directories. Set `root` to the one the learning belongs to."
        );
        let worktree = worktree.read(cx);
        let relative_path = format!("{PROJECT_LEARNINGS_DIR}/{file_name}");
        Ok((
            worktree.abs_path().join(&relative_path),
            format!("{}/{relative_path}", worktree.root_name_str()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;

    #[gpui::test]
    async fn test_save_project_learning_after_approval(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "main.rs": "" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let tool = Arc::new(SaveLearningTool::new(project));
        let (event_stream, mut events) = ToolCallEventStream::test();

        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(SaveLearningToolInput {
                    title: "Sidebar tests need DYLD".to_string(),
                    content: "Set DYLD_FRAMEWORK_PATH before running them.".to_string(),
                    scope: LearningScope::Project,
                    root: None,
                }),
                event_stream,
                cx,
            )
        });

        events.expect_update_fields().await;
        let authorization = events.expect_authorization().await;
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let output = task.await.expect("saving should succeed");
        assert!(
            output.contains("root/.zed/learnings/sidebar-tests-need-dyld.md"),
            "{output}"
        );
        let saved = fs
            .load(Path::new(path!(
                "/root/.zed/learnings/sidebar-tests-need-dyld.md"
            )))
            .await
            .expect("learning file should exist");
        assert_eq!(
            saved,
            "# Sidebar tests need DYLD\n\nSet DYLD_FRAMEWORK_PATH before running them.\n"
        );
    }
}
