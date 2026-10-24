//! The GitHub Projects toolkit for agents: the items of the project in the Tarefas dock, read,
//! moved, commented and started with the account of the Repo dock. It also feeds the agent's
//! `@` menu, so an item can be mentioned like a file.

use crate::{
    Availability, ProjectsStore, RepositoryProjects,
    api::{self, Item, ItemKind, NewValue, Project as GitHubProject},
    github_repository, repository_projects,
};
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AsyncApp, Entity, Task, WindowHandle};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use task_agents::{
    PROJECT_ITEM_TOOL, PROJECT_ITEMS_TOOL, ProjectItemEntry, ProjectItemRow, ProjectItemSource,
    ProjectItemsCard, ToolAccess, Toolkit, ToolkitOutput, ToolkitTool,
};
use workspace::{MultiWorkspace, Workspace};

/// How long a tool waits for the first sync of a project nobody opened yet.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct ProjectsContext {
    pub(crate) state: Entity<RepositoryProjects>,
    pub(crate) project: GitHubProject,
    pub(crate) branch: Option<String>,
    viewer_login: String,
}

pub(crate) async fn projects_context(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<ProjectsContext> {
    let (repository, branch) = cx
        .update(|cx| github_repository(project, cx))
        .map(|(repository, _, branch)| (repository, branch))
        .context(
            "O projeto não está num repositório do GitHub; o GitHub Projects não se aplica.",
        )?;
    let state = cx.update(|cx| repository_projects(&repository, cx));
    let started = Instant::now();
    loop {
        let (availability, loaded) = cx.update(|cx| {
            let state = state.read(cx);
            (state.availability(cx), state.project().cloned())
        });
        match availability {
            Availability::Ready => {
                if let Some(loaded) = loaded {
                    let viewer_login = cx.update(|cx| {
                        state
                            .read(cx)
                            .viewer_login()
                            .unwrap_or_default()
                            .to_string()
                    });
                    return Ok(ProjectsContext {
                        state,
                        project: loaded,
                        branch,
                        viewer_login,
                    });
                }
            }
            Availability::NotConnected => anyhow::bail!(
                "Nenhuma conta do GitHub conectada neste perfil. Peça para conectar no dock Repo."
            ),
            Availability::NoProjects => anyhow::bail!(
                "Nem {} nem a conta têm um GitHub Project aberto.",
                repository.full_name()
            ),
            Availability::MissingScope => anyhow::bail!(
                "O token não tem acesso ao GitHub Projects. Peça para rodar `gh auth refresh -s project` ou dar Projects ao token."
            ),
            Availability::Failed(error) => {
                anyhow::bail!("Não deu para carregar o projeto: {error}")
            }
            Availability::Loading => {}
        }
        if started.elapsed() > LOAD_TIMEOUT {
            anyhow::bail!("O projeto ainda está carregando; tente de novo em instantes.");
        }
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
}

/// An item by `#198`, `198`, its project item id, or a piece of its title.
fn resolve_item<'a>(project: &'a GitHubProject, reference: &str) -> Result<&'a Item> {
    let reference = reference.trim();
    let number = reference
        .rsplit('#')
        .next()
        .and_then(|number| number.parse::<u64>().ok());
    if let Some(number) = number
        && let Some(item) = project
            .items
            .iter()
            .find(|item| item.number == Some(number))
    {
        return Ok(item);
    }
    if let Some(item) = project.item(reference) {
        return Ok(item);
    }
    let lowered = reference.to_lowercase();
    let mut matches = project
        .items
        .iter()
        .filter(|item| item.title.to_lowercase().contains(&lowered));
    match (matches.next(), matches.next()) {
        (Some(item), None) => Ok(item),
        (Some(_), Some(_)) => Err(anyhow!(
            "Mais de um item tem `{reference}` no título; diga o número."
        )),
        _ => Err(anyhow!(
            "Nenhum item `{reference}` em {}.",
            project.summary.title
        )),
    }
}

fn field_line(project: &GitHubProject, item: &Item) -> String {
    let mut parts = Vec::new();
    if let Some(status) = project.status_of(item) {
        parts.push(format!("Status: {}", status.name));
    }
    if let Some(priority) = project.priority_of(item) {
        parts.push(format!("Prioridade: {}", priority.name));
    }
    if let Some(iteration) = project.iteration_of(item) {
        parts.push(format!("Sprint: {}", iteration.title));
    }
    if let Some(estimate) = project.estimate_of(item) {
        parts.push(format!(
            "Estimativa: {}",
            crate::panel::format_number(estimate)
        ));
    }
    parts.join(" · ")
}

/// What the agent reads about an item, for mentions and `project_item`.
pub(crate) fn describe_item(project: &GitHubProject, item: &Item, branch: Option<&str>) -> String {
    let kind = match item.kind {
        ItemKind::Issue => "issue",
        ItemKind::PullRequest => "pull request",
        ItemKind::Draft => "rascunho",
    };
    let state = if item.closed { "fechada" } else { "aberta" };
    let mut text = format!(
        "{} {} ({kind}, {state}) · projeto {} · id {}\n",
        item.reference(),
        item.title,
        project.summary.title,
        item.id
    );
    let fields = field_line(project, item);
    if !fields.is_empty() {
        text.push_str(&fields);
        text.push('\n');
    }
    if !item.assignees.is_empty() {
        text.push_str(&format!("Responsáveis: {}\n", item.assignees.join(", ")));
    }
    if !item.labels.is_empty() {
        let labels: Vec<&str> = item
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .collect();
        text.push_str(&format!("Labels: {}\n", labels.join(", ")));
    }
    if !item.linked_branches.is_empty() {
        text.push_str(&format!("Branches: {}", item.linked_branches.join(", ")));
        if branch.is_some_and(|branch| item.linked_branches.iter().any(|linked| linked == branch)) {
            text.push_str(" · é o branch atual");
        }
        text.push('\n');
    }
    for pull_request in &item.pull_requests {
        text.push_str(&format!(
            "PR #{} ({}) {}\n",
            pull_request.number,
            pull_request.state.to_lowercase(),
            pull_request.url
        ));
    }
    let subtasks = item.subtasks();
    if !subtasks.is_empty() {
        let done = subtasks.iter().filter(|subtask| subtask.done).count();
        text.push_str(&format!("Subtarefas ({done}/{}):\n", subtasks.len()));
        for subtask in subtasks {
            text.push_str(&format!(
                "- [{}] {}\n",
                if subtask.done { "x" } else { " " },
                subtask.text
            ));
        }
    }
    if !item.body.trim().is_empty() {
        text.push_str(&format!("\nDescrição:\n{}\n", item.body.trim()));
    }
    if let Some(url) = &item.url {
        text.push_str(&format!("\n{url}\n"));
    }
    text
}

fn entries(project: &Entity<Project>, cx: &App) -> Vec<ProjectItemEntry> {
    let Some((repository, _, branch)) = github_repository(project, cx) else {
        return Vec::new();
    };
    let Some(state) = cx
        .try_global::<ProjectsStore>()
        .and_then(|store| store.repositories.get(&repository))
    else {
        return Vec::new();
    };
    let state = state.read(cx);
    let Some(github_project) = state.project() else {
        return Vec::new();
    };
    let login = state.viewer_login().unwrap_or_default();
    let current = branch
        .as_deref()
        .and_then(|branch| api::item_for_branch(&github_project.items, branch))
        .map(|item| item.id.clone());
    let mut entries: Vec<ProjectItemEntry> = github_project
        .items
        .iter()
        .filter(|item| !item.closed)
        .map(|item| ProjectItemEntry {
            project_id: github_project.summary.id.clone(),
            item_id: item.id.clone(),
            reference: item.reference(),
            title: item.title.clone(),
            status: github_project
                .status_of(item)
                .map(|status| status.name.clone()),
            is_current_branch: current.as_deref() == Some(item.id.as_str()),
            is_assigned_to_you: item.is_assigned_to(login),
        })
        .collect();
    entries.sort_by_key(|entry| (!entry.is_current_branch, !entry.is_assigned_to_you));
    entries
}

fn describe_reference(
    project: &Entity<Project>,
    project_id: String,
    item_id: String,
    cx: &mut App,
) -> Task<Result<String>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let context = projects_context(&project, cx).await?;
        anyhow::ensure!(
            context.project.summary.id == project_id,
            "O item é de outro projeto; selecione-o no dock Tarefas."
        );
        let item = context
            .project
            .item(&item_id)
            .context("O item não está mais no projeto.")?;
        Ok(describe_item(
            &context.project,
            item,
            context.branch.as_deref(),
        ))
    })
}

#[derive(Deserialize, JsonSchema)]
struct ItemsInput {
    /// Only items assigned to the user. Defaults to true.
    #[serde(default)]
    assigned_to_me: Option<bool>,
    /// Only items in the current sprint (iteration). Defaults to true when the project has one.
    #[serde(default)]
    current_iteration: Option<bool>,
    /// Only items with this Status, by name (e.g. "Em revisão").
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ItemInput {
    /// The item: `#198`, `198`, or a piece of its title.
    item: String,
}

#[derive(Deserialize, JsonSchema)]
struct StartInput {
    /// The item: `#198`, `198`, or a piece of its title.
    item: String,
    /// The branch to create. Defaults to `feat/<number>-<title>` (`fix/` for bugs).
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateInput {
    /// The item: `#198`, `198`, or a piece of its title.
    item: String,
    /// The new Status, by name.
    #[serde(default)]
    status: Option<String>,
    /// The new priority, by name.
    #[serde(default)]
    priority: Option<String>,
    /// The sprint, by title, or `current` for the one today is in.
    #[serde(default)]
    iteration: Option<String>,
    #[serde(default)]
    estimate: Option<f64>,
    /// Assigns the item to the user.
    #[serde(default)]
    assign_me: bool,
    /// Moves to the done column even with a pull request still open. Only when the user said so
    /// after being told.
    #[serde(default)]
    even_with_open_pull_request: bool,
}

#[derive(Deserialize, JsonSchema)]
struct CommentInput {
    /// The item: `#198`, `198`, or a piece of its title.
    item: String,
    /// Markdown text of the comment.
    text: String,
}

fn list_items(
    project: Entity<Project>,
    input: ItemsInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = projects_context(&project, cx).await?;
        let github_project = &context.project;
        let today = crate::panel::today();
        let iteration = github_project.current_iteration(today);
        let mine = input.assigned_to_me.unwrap_or(true);
        let in_iteration =
            input.current_iteration.unwrap_or(iteration.is_some()) && iteration.is_some();
        let done_id = github_project.done_option().map(|option| option.id.clone());
        let wanted_status = input.status.as_deref().map(str::to_lowercase);
        let items: Vec<&Item> = github_project
            .items
            .iter()
            .filter(|item| !item.closed)
            .filter(|item| !mine || item.is_assigned_to(&context.viewer_login))
            .filter(|item| {
                !in_iteration
                    || github_project.iteration_of(item).map(|it| &it.id)
                        == iteration.map(|it| &it.id)
            })
            .filter(|item| {
                let status = github_project.status_of(item);
                match &wanted_status {
                    Some(wanted) => {
                        status.is_some_and(|status| status.name.to_lowercase() == *wanted)
                    }
                    None => status.map(|status| &status.id) != done_id.as_ref(),
                }
            })
            .collect();

        let title = match (in_iteration.then_some(iteration).flatten(), mine) {
            (Some(iteration), true) => format!("{} · seus itens", iteration.title),
            (Some(iteration), false) => format!("{} · todos os itens", iteration.title),
            (None, true) => "Seus itens".to_string(),
            (None, false) => "Itens abertos".to_string(),
        };
        let mut text = format!(
            "{title} em {} ({}):\n",
            github_project.summary.title,
            items.len()
        );
        for item in &items {
            text.push_str(&format!("- {} {}", item.reference(), item.title));
            let fields = field_line(github_project, item);
            if !fields.is_empty() {
                text.push_str(&format!(" · {fields}"));
            }
            if !item.assignees.is_empty() {
                text.push_str(&format!(" · {}", item.assignees.join(", ")));
            }
            text.push('\n');
        }
        let card = ProjectItemsCard {
            project_id: github_project.summary.id.clone(),
            project_title: github_project.summary.title.clone(),
            project_url: github_project.summary.url.clone(),
            title,
            items: items
                .iter()
                .map(|item| {
                    let status = github_project.status_of(item);
                    let in_progress = github_project
                        .in_progress_option()
                        .is_some_and(|option| Some(&option.id) == status.map(|status| &status.id));
                    let review = github_project
                        .review_option()
                        .is_some_and(|option| Some(&option.id) == status.map(|status| &status.id));
                    ProjectItemRow {
                        item_id: item.id.clone(),
                        reference: item.reference(),
                        title: item.title.clone(),
                        status: status.map(|status| status.name.clone()),
                        status_color: status.map(|status| status.color.clone()),
                        detail: if in_progress || review {
                            status.map(|status| status.name.to_lowercase())
                        } else {
                            github_project
                                .priority_of(item)
                                .map(|priority| priority.name.clone())
                        },
                        can_start: item.kind == ItemKind::Issue && item.linked_branches.is_empty(),
                        url: item.url.clone(),
                    }
                })
                .collect(),
        };
        Ok(ToolkitOutput::text(text).with_raw_output(card))
    })
}

fn read_item(
    project: Entity<Project>,
    input: ItemInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = projects_context(&project, cx).await?;
        let item = resolve_item(&context.project, &input.item)?;
        Ok(ToolkitOutput::text(describe_item(
            &context.project,
            item,
            context.branch.as_deref(),
        )))
    })
}

/// A window showing `project`, for the credential prompts a checkout may need.
pub(crate) fn window_for(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .find_map(|window| {
            let workspace = window.read(cx).ok().and_then(|multi_workspace| {
                multi_workspace
                    .workspaces()
                    .find(|workspace| workspace.read(cx).project() == project)
                    .cloned()
            })?;
            Some((window, workspace))
        })
}

fn start(project: Entity<Project>, input: StartInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = projects_context(&project, cx).await?;
        let item = resolve_item(&context.project, &input.item)?.clone();
        anyhow::ensure!(
            item.kind == ItemKind::Issue,
            "Só issues têm branch vinculado; {} é {}.",
            item.reference(),
            if item.kind == ItemKind::Draft { "um rascunho" } else { "um pull request" }
        );
        let branch = input
            .branch
            .filter(|branch| !branch.trim().is_empty())
            .unwrap_or_else(|| api::branch_name(&item));
        let (window, workspace) = cx
            .update(|cx| window_for(&project, cx))
            .context("Nenhuma janela mostra este projeto para fazer o checkout.")?;
        let workspace = workspace.downgrade();
        let state = context.state.clone();
        let item_id = item.id.clone();
        let started = window.update(cx, |_, window, cx| {
            crate::start_item(workspace, project.clone(), state, item_id, branch.clone(), window, cx)
        })?;
        started.await?;
        let moved_to = context
            .project
            .in_progress_option()
            .map(|option| option.name.clone())
            .unwrap_or_else(|| "em andamento".to_string());
        Ok(ToolkitOutput::text(format!(
            "{} começado: branch `{branch}` criado e vinculado, checkout feito, movido para {moved_to} e atribuído a você.",
            item.reference()
        )))
    })
}

fn update(
    project: Entity<Project>,
    input: UpdateInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = projects_context(&project, cx).await?;
        let github_project = &context.project;
        let item = resolve_item(github_project, &input.item)?.clone();
        let mut changes: Vec<(String, NewValue, String)> = Vec::new();

        if let Some(name) = &input.status {
            let field = github_project
                .status
                .as_ref()
                .context("O projeto não tem campo Status.")?;
            let option = field
                .options
                .iter()
                .find(|option| option.name.eq_ignore_ascii_case(name.trim()))
                .with_context(|| {
                    let names: Vec<&str> = field.options.iter().map(|option| option.name.as_str()).collect();
                    format!("Status `{name}` não existe. Há: {}.", names.join(", "))
                })?;
            let is_done = github_project.done_option().map(|done| &done.id) == Some(&option.id);
            if is_done
                && !input.even_with_open_pull_request
                && let Some(pull_request) = item
                    .pull_requests
                    .iter()
                    .find(|pull_request| pull_request.state == "OPEN")
            {
                anyhow::bail!(
                    "{} ainda tem o PR #{} aberto. Mover para {} agora tira o item do fluxo do merge. Pergunte ao usuário se quer esperar o merge ou mover mesmo assim.",
                    item.reference(),
                    pull_request.number,
                    option.name
                );
            }
            changes.push((field.id.clone(), NewValue::Option(option.id.clone()), format!("Status → {}", option.name)));
        }
        if let Some(name) = &input.priority {
            let field = github_project
                .priority
                .as_ref()
                .context("O projeto não tem campo de prioridade.")?;
            let option = field
                .options
                .iter()
                .find(|option| option.name.eq_ignore_ascii_case(name.trim()))
                .with_context(|| format!("Prioridade `{name}` não existe."))?;
            changes.push((field.id.clone(), NewValue::Option(option.id.clone()), format!("Prioridade → {}", option.name)));
        }
        if let Some(title) = &input.iteration {
            let field = github_project
                .iteration
                .as_ref()
                .context("O projeto não tem sprints.")?;
            let iteration = if title.trim().eq_ignore_ascii_case("current") {
                github_project.current_iteration(crate::panel::today())
            } else {
                field
                    .iterations
                    .iter()
                    .find(|iteration| iteration.title.eq_ignore_ascii_case(title.trim()))
            }
            .with_context(|| format!("Sprint `{title}` não existe."))?;
            changes.push((field.id.clone(), NewValue::Iteration(iteration.id.clone()), format!("Sprint → {}", iteration.title)));
        }
        if let Some(estimate) = input.estimate {
            let field = github_project
                .estimate
                .as_ref()
                .context("O projeto não tem campo de estimativa.")?;
            changes.push((field.id.clone(), NewValue::Number(estimate), format!("Estimativa → {}", crate::panel::format_number(estimate))));
        }
        anyhow::ensure!(
            !changes.is_empty() || input.assign_me,
            "Diga o que mudar: status, priority, iteration, estimate ou assign_me."
        );

        let mut done = Vec::new();
        for (field_id, value, description) in changes {
            let task = context.state.update(cx, |state, cx| {
                state.set_field(item.id.clone(), field_id, value, cx)
            });
            task.await?;
            done.push(description);
        }
        if input.assign_me {
            let task = context.state.update(cx, |state, cx| state.assign_me(&item.id, cx));
            task.await?;
            done.push("atribuído a você".to_string());
        }
        Ok(ToolkitOutput::text(format!(
            "{}: {}.",
            item.reference(),
            done.join(", ")
        )))
    })
}

fn comment(
    project: Entity<Project>,
    input: CommentInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        anyhow::ensure!(!input.text.trim().is_empty(), "O comentário está vazio.");
        let context = projects_context(&project, cx).await?;
        let item = resolve_item(&context.project, &input.item)?.clone();
        let task = context
            .state
            .update(cx, |state, cx| state.comment(&item.id, input.text, cx));
        task.await?;
        Ok(ToolkitOutput::text(format!(
            "Comentário publicado em {}.",
            item.reference()
        )))
    })
}

/// Lists the project's items for the `@` menu without waiting for someone to open the dock.
pub(crate) fn prefetch(project: Entity<Project>, cx: &mut App) {
    cx.spawn(async move |cx| {
        for _ in 0..30 {
            let enabled = cx.update(|cx| {
                task_agents::toolkit_enabled(
                    task_agents::toolkit_ids::GITHUB_PROJECTS,
                    &project,
                    cx,
                )
            });
            if !enabled {
                return;
            }
            let repository = cx.update(|cx| github_repository(&project, cx));
            if let Some((repository, _, _)) = repository {
                cx.update(|cx| repository_projects(&repository, cx));
                return;
            }
            cx.background_executor().timer(Duration::from_secs(1)).await;
        }
    })
    .detach();
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_project_item_source(
        ProjectItemSource {
            list: Arc::new(entries),
            describe: Arc::new(describe_reference),
        },
        cx,
    );
    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::GITHUB_PROJECTS.into(),
            name: "GitHub Projects".into(),
            description: "Itens do projeto do GitHub no dock Tarefas".into(),
            icon: "blocks".into(),
            mention: Some("@Item".into()),
            tools: vec![
                ToolkitTool::new(
                    PROJECT_ITEMS_TOOL,
                    "Leu o projeto",
                    "Lists the open items of the GitHub project in the Tarefas dock: by default \
                     the user's items in the current sprint, with status, priority and sprint. \
                     Use this instead of any GitHub MCP server tool.",
                    ToolAccess::Read,
                    list_items,
                ),
                ToolkitTool::new(
                    PROJECT_ITEM_TOOL,
                    "Leu o item",
                    "Reads one item of the GitHub project: fields, subtasks, linked branches and \
                     pull requests, and its description.",
                    ToolAccess::Read,
                    read_item,
                ),
                ToolkitTool::new(
                    "project_item_start",
                    "Começou o item",
                    "Starts an issue of the project: creates its branch on GitHub linked to it, \
                     checks it out, moves it to work in progress and assigns it to the user. Only \
                     when the user asked; they confirm it.",
                    ToolAccess::AlwaysConfirm,
                    start,
                ),
                ToolkitTool::new(
                    "project_item_update",
                    "Mudou o item",
                    "Changes an item's Status, priority, sprint or estimate, or assigns it to the \
                     user. Only what the user asked for; they confirm it. Moving to done with a \
                     pull request still open is refused unless the user said to after being told.",
                    ToolAccess::AlwaysConfirm,
                    update,
                ),
                ToolkitTool::new(
                    "project_item_comment",
                    "Comentou no item",
                    "Publishes a comment on an issue or pull request of the project. Everyone \
                     sees it, so only post what the user asked for.",
                    ToolAccess::Act,
                    comment,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn project(items: Vec<Item>) -> GitHubProject {
        GitHubProject {
            summary: api::ProjectSummary {
                id: "PVT_1".to_string(),
                title: "Asylum roadmap".to_string(),
                number: 3,
                url: "https://github.com/users/vini/projects/3".to_string(),
            },
            status: None,
            priority: None,
            iteration: None,
            estimate: None,
            items,
            rate_limit: api::RateLimit::default(),
        }
    }

    fn item(number: u64, title: &str) -> Item {
        Item {
            id: format!("PVTI_{number}"),
            kind: ItemKind::Issue,
            content_id: format!("I_{number}"),
            number: Some(number),
            title: title.to_string(),
            url: None,
            closed: false,
            body: String::new(),
            repository: Some("vini/asylum".to_string()),
            author: None,
            assignees: Vec::new(),
            labels: Vec::new(),
            linked_branches: Vec::new(),
            pull_requests: Vec::new(),
            values: HashMap::default(),
        }
    }

    #[test]
    fn resolves_items_by_number_id_or_title() {
        let project = project(vec![
            item(188, "Release do Windows sem web preview"),
            item(204, "Re-rodar job do Actions"),
            item(206, "Docs do Actions"),
        ]);
        assert_eq!(
            resolve_item(&project, "#188").map(|item| item.number).ok(),
            Some(Some(188))
        );
        assert_eq!(
            resolve_item(&project, "asylum#204")
                .map(|item| item.number)
                .ok(),
            Some(Some(204))
        );
        assert_eq!(
            resolve_item(&project, "PVTI_206")
                .map(|item| item.number)
                .ok(),
            Some(Some(206))
        );
        assert_eq!(
            resolve_item(&project, "windows")
                .map(|item| item.number)
                .ok(),
            Some(Some(188))
        );
        assert!(resolve_item(&project, "actions").is_err());
        assert!(resolve_item(&project, "#999").is_err());
    }
}
