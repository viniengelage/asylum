//! The ClickUp toolkit for agents: read the task behind the current branch, comment, move it and
//! create new tasks (a user story written from a thread), with the account of the ClickUp dock.

use crate::{ClickUpStore, Connection, api};
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AsyncApp, Entity, Task};
use http_client::HttpClient;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COMMENTS: usize = 30;

struct ClickUpContext {
    client: Arc<dyn HttpClient>,
    token: Arc<str>,
    workspace_id: String,
    open_tasks: Vec<api::Task>,
    branch: Option<String>,
}

async fn clickup_context(project: &Entity<Project>, cx: &mut AsyncApp) -> Result<ClickUpContext> {
    let store = cx.update(ClickUpStore::global);
    let started = Instant::now();
    loop {
        let loading = cx.update(|cx| matches!(store.read(cx).connection(), Connection::Loading));
        if !loading || started.elapsed() > CONNECTION_TIMEOUT {
            break;
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    cx.update(|cx| {
        let store = store.read(cx);
        let (client, token, workspace_id) = store.request_context().context(
            "O ClickUp não está conectado. Peça para conectar no dock do ClickUp.",
        )?;
        let branch = project
            .read(cx)
            .active_repository(cx)
            .and_then(|repository| {
                repository
                    .read(cx)
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string())
            });
        Ok(ClickUpContext {
            client,
            token,
            workspace_id,
            open_tasks: store.tasks().open.clone(),
            branch,
        })
    })
}

/// A custom ID such as `CU-86a1b2` inside a branch name like `feat/CU-86a1b2-biometria`.
fn custom_id_in_branch(branch: &str) -> Option<String> {
    let lower = branch.to_lowercase();
    let start = lower.find("cu-")?;
    let rest = &branch[start + 3..];
    let length = rest
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .count();
    (length > 0).then(|| format!("CU-{}", &rest[..length]))
}

fn resolve_task_id(context: &ClickUpContext, task_id: Option<String>) -> Result<String> {
    if let Some(task_id) = task_id.filter(|task_id| !task_id.trim().is_empty()) {
        return Ok(task_id.trim().to_string());
    }
    let branch = context
        .branch
        .as_deref()
        .context("Sem branch ativo; diga o ID da tarefa.")?;
    let lower = branch.to_lowercase();
    if let Some(task) = context.open_tasks.iter().find(|task| {
        lower.contains(&task.id.to_lowercase()) || lower.contains(&task.display_id().to_lowercase())
    }) {
        return Ok(task.id.clone());
    }
    custom_id_in_branch(branch)
        .with_context(|| format!("Nenhuma tarefa ligada ao branch `{branch}`; diga o ID da tarefa."))
}

fn format_duration(milliseconds: Option<i64>) -> Option<String> {
    let minutes = milliseconds? / 60_000;
    Some(format!("{}h{:02}", minutes / 60, minutes % 60))
}

#[derive(Deserialize, JsonSchema)]
struct TaskInput {
    /// The task ID or custom ID (e.g. `CU-86a1b2`). Defaults to the task in the current branch name.
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct CommentInput {
    #[serde(default)]
    task_id: Option<String>,
    /// Plain text of the comment.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct StatusInput {
    #[serde(default)]
    task_id: Option<String>,
    /// The new status, as the list names it (e.g. "em revisão").
    status: String,
}

#[derive(Deserialize, JsonSchema)]
struct CreateInput {
    /// The list to create the task in. Defaults to the list of the task in the current branch.
    #[serde(default)]
    list_id: Option<String>,
    /// Task title.
    name: String,
    /// Description in Markdown: the story, acceptance criteria, notes.
    description: String,
}

fn get_task(project: Entity<Project>, input: TaskInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = clickup_context(&project, cx).await?;
        let task_id = resolve_task_id(&context, input.task_id)?;
        let detail =
            api::get_task_by_any_id(&context.client, &context.token, &context.workspace_id, &task_id)
                .await?;
        let comments = api::get_comments(&context.client, &context.token, &detail.task.id)
            .await
            .unwrap_or_default();
        let task = &detail.task;
        let mut text = format!(
            "{} · {}\nStatus: {} · Lista: {} ({})\n{}\n",
            task.display_id(),
            task.name,
            task.status.status,
            task.list.name,
            task.list.id,
            task.url
        );
        if !task.assignees.is_empty() {
            text.push_str(&format!(
                "Responsáveis: {}\n",
                task.assignees
                    .iter()
                    .map(|assignee| assignee.display_name().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(estimate) = format_duration(detail.time_estimate) {
            text.push_str(&format!("Estimativa: {estimate}\n"));
        }
        if let Some(spent) = format_duration(detail.time_spent) {
            text.push_str(&format!("Tempo gasto: {spent}\n"));
        }
        text.push_str(&format!(
            "\n{}\n",
            detail
                .text_content
                .as_deref()
                .filter(|description| !description.trim().is_empty())
                .unwrap_or("(sem descrição)")
        ));
        if !comments.is_empty() {
            text.push_str(&format!("\nComentários ({}):\n", comments.len()));
            for comment in comments.iter().rev().take(MAX_COMMENTS).rev() {
                text.push_str(&format!(
                    "- {}: {}\n",
                    comment.user.display_name(),
                    comment.comment_text.trim()
                ));
            }
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn my_tasks(project: Entity<Project>, _input: NoInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = clickup_context(&project, cx).await?;
        let mut text = format!("{} tarefa(s) aberta(s) atribuídas a você:\n", context.open_tasks.len());
        for task in &context.open_tasks {
            text.push_str(&format!(
                "- {} · {} · {} · lista {}\n",
                task.display_id(),
                task.name,
                task.status.status,
                task.list.name
            ));
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn comment(project: Entity<Project>, input: CommentInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        if input.text.trim().is_empty() {
            return Err(anyhow!("O comentário está vazio."));
        }
        let context = clickup_context(&project, cx).await?;
        let task_id = resolve_task_id(&context, input.task_id)?;
        let detail =
            api::get_task_by_any_id(&context.client, &context.token, &context.workspace_id, &task_id)
                .await?;
        api::post_comment(&context.client, &context.token, &detail.task.id, &input.text).await?;
        Ok(ToolkitOutput::text(format!(
            "Comentário publicado em {}.",
            detail.task.display_id()
        )))
    })
}

fn set_status(project: Entity<Project>, input: StatusInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = clickup_context(&project, cx).await?;
        let task_id = resolve_task_id(&context, input.task_id)?;
        let detail =
            api::get_task_by_any_id(&context.client, &context.token, &context.workspace_id, &task_id)
                .await?;
        let statuses =
            api::get_list_statuses(&context.client, &context.token, &detail.task.list.id).await?;
        let wanted = input.status.trim().to_lowercase();
        let status = statuses
            .iter()
            .find(|status| status.status.to_lowercase() == wanted)
            .with_context(|| {
                format!(
                    "A lista não tem o status `{}`. Os disponíveis: {}",
                    input.status,
                    statuses
                        .iter()
                        .map(|status| status.status.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        api::set_task_status(&context.client, &context.token, &detail.task.id, &status.status)
            .await?;
        Ok(ToolkitOutput::text(format!(
            "{} agora está em `{}`.",
            detail.task.display_id(),
            status.status
        )))
    })
}

fn create(project: Entity<Project>, input: CreateInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = clickup_context(&project, cx).await?;
        let list_id = match input.list_id.filter(|list_id| !list_id.trim().is_empty()) {
            Some(list_id) => list_id,
            None => {
                let task_id = resolve_task_id(&context, None).context(
                    "Diga em qual lista criar (list_id) — não há tarefa no branch para usar a mesma lista.",
                )?;
                api::get_task_by_any_id(&context.client, &context.token, &context.workspace_id, &task_id)
                    .await?
                    .task
                    .list
                    .id
            }
        };
        let task = api::create_task(
            &context.client,
            &context.token,
            &list_id,
            &input.name,
            &input.description,
        )
        .await?;
        Ok(ToolkitOutput::text(format!(
            "Tarefa criada: {} · {}\n{}",
            task.display_id(),
            task.name,
            task.url
        )))
    })
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "clickup".into(),
            name: "ClickUp".into(),
            description: "Tarefas do ClickUp conectado no dock".into(),
            icon: "list_todo".into(),
            tools: vec![
                ToolkitTool::new(
                    "clickup_task",
                    "Leu a tarefa do ClickUp",
                    "Reads a ClickUp task with its description and comments. Defaults to the task \
                     whose ID is in the current branch name.",
                    ToolAccess::Read,
                    get_task,
                ),
                ToolkitTool::new(
                    "clickup_my_tasks",
                    "Listou suas tarefas",
                    "Lists the open ClickUp tasks assigned to the user.",
                    ToolAccess::Read,
                    my_tasks,
                ),
                ToolkitTool::new(
                    "clickup_comment",
                    "Comentou na tarefa",
                    "Posts a comment on a ClickUp task.",
                    ToolAccess::Act,
                    comment,
                ),
                ToolkitTool::new(
                    "clickup_set_status",
                    "Mudou o status da tarefa",
                    "Moves a ClickUp task to another status of its list.",
                    ToolAccess::Act,
                    set_status,
                ),
                ToolkitTool::new(
                    "clickup_create_task",
                    "Criou uma tarefa no ClickUp",
                    "Creates a ClickUp task (for example a user story) with a Markdown description.",
                    ToolAccess::Act,
                    create,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_id_in_branch() {
        assert_eq!(
            custom_id_in_branch("feat/CU-86a1b2-biometria"),
            Some("CU-86a1b2".to_string())
        );
        assert_eq!(custom_id_in_branch("main"), None);
    }
}
