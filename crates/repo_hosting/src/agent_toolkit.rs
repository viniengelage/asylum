//! The Repo toolkit for agents: the pull requests of the project's Bitbucket repository, their
//! diffs, and comments, with the account connected in the Repo dock.

use crate::api::{ChangedFile, CheckState, PullRequest, RemoteRepository};
use crate::panel::{Target, detect_target};
use crate::{Connection, RepoStore, bitbucket};
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
const MAX_DIFF_CHARS: usize = 80_000;

struct RepoContext {
    client: Arc<dyn HttpClient>,
    credentials: bitbucket::Credentials,
    repository: RemoteRepository,
    branch: Option<String>,
}

async fn repo_context(project: &Entity<Project>, cx: &mut AsyncApp) -> Result<RepoContext> {
    let store = cx.update(RepoStore::global);
    let started = Instant::now();
    loop {
        let loading = cx.update(|cx| {
            matches!(
                store.read(cx).connection(crate::api::Hosting::Bitbucket),
                Connection::Loading
            )
        });
        if !loading || started.elapsed() > CONNECTION_TIMEOUT {
            break;
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    cx.update(|cx| {
        let (target, branch) = detect_target(project, cx);
        let repository = match target {
            Target::Hosted { repository, .. } => repository,
            Target::NoRepository => anyhow::bail!("O projeto não tem repositório git."),
            Target::Unsupported { remote_url } => anyhow::bail!(
                "O remoto {} não é do Bitbucket; só o Bitbucket é suportado por enquanto.",
                remote_url.unwrap_or_else(|| "(nenhum)".into())
            ),
        };
        let store = store.read(cx);
        let credentials = store.bitbucket_credentials().context(
            "Nenhuma conta do Bitbucket conectada. Peça para conectar no dock Repo.",
        )?;
        Ok(RepoContext {
            client: store.http_client(),
            credentials,
            repository,
            branch,
        })
    })
}

async fn resolve_number(context: &RepoContext, number: Option<u64>) -> Result<u64> {
    if let Some(number) = number {
        return Ok(number);
    }
    let branch = context
        .branch
        .as_deref()
        .context("Sem branch ativo; diga o número do PR.")?;
    let pull_requests =
        bitbucket::list_open_pull_requests(&context.client, &context.credentials, &context.repository)
            .await?;
    pull_requests
        .iter()
        .find(|pull_request| pull_request.source_branch == branch)
        .map(|pull_request| pull_request.number)
        .with_context(|| format!("Nenhum PR aberto a partir de `{branch}`; diga o número do PR."))
}

fn pull_request_line(pull_request: &PullRequest, branch: Option<&str>) -> String {
    let mut line = format!(
        "#{} {} · {} → {} · {} · {} aprovação(ões), {} comentário(s)",
        pull_request.number,
        pull_request.title,
        pull_request.source_branch,
        pull_request.destination_branch,
        pull_request.author.short_name(),
        pull_request.approval_count(),
        pull_request.comment_count
    );
    if pull_request.draft {
        line.push_str(" · rascunho");
    }
    if Some(pull_request.source_branch.as_str()) == branch {
        line.push_str(" · PR do branch atual");
    }
    line
}

fn file_line(file: &ChangedFile) -> String {
    format!(
        "- {:?} {}{} (+{} −{})",
        file.change,
        file.old_path
            .as_ref()
            .map(|old| format!("{old} → "))
            .unwrap_or_default(),
        file.path,
        file.lines_added,
        file.lines_removed
    )
}

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct PullRequestInput {
    /// The pull request number. Defaults to the one opened from the current branch.
    #[serde(default)]
    number: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct CommentInput {
    /// The pull request number. Defaults to the one opened from the current branch.
    #[serde(default)]
    number: Option<u64>,
    /// Markdown text of the comment.
    text: String,
}

fn list(project: Entity<Project>, _input: NoInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let pull_requests = bitbucket::list_open_pull_requests(
            &context.client,
            &context.credentials,
            &context.repository,
        )
        .await?;
        let mut text = format!(
            "{} · {} PR(s) aberto(s)\n",
            context.repository.full_name(),
            pull_requests.len()
        );
        for pull_request in &pull_requests {
            text.push_str(&pull_request_line(pull_request, context.branch.as_deref()));
            text.push('\n');
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn detail(project: Entity<Project>, input: PullRequestInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number).await?;
        let detail = bitbucket::get_pull_request_detail(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
        )
        .await?;
        let pull_request = &detail.pull_request;
        let mut text = format!(
            "{}\n{}\n\n{}\n",
            pull_request_line(pull_request, context.branch.as_deref()),
            pull_request.url,
            if pull_request.description.trim().is_empty() {
                "(sem descrição)"
            } else {
                pull_request.description.trim()
            }
        );
        if !detail.checks.is_empty() {
            text.push_str("\nChecks:\n");
            for check in &detail.checks {
                let state = match check.state {
                    CheckState::Passed => "passou",
                    CheckState::Failed => "falhou",
                    CheckState::Running => "rodando",
                    CheckState::Stopped => "parado",
                };
                text.push_str(&format!("- {} · {state}\n", check.name));
            }
        }
        text.push_str(&format!("\nArquivos ({}):\n", detail.files.len()));
        for file in &detail.files {
            text.push_str(&file_line(file));
            text.push('\n');
        }
        text.push_str(&format!("\nCommits ({}):\n", detail.commits.len()));
        for commit in &detail.commits {
            text.push_str(&format!("- {} {}\n", commit.short_hash(), commit.summary));
        }
        if !detail.comments.is_empty() {
            text.push_str(&format!("\nComentários ({}):\n", detail.comments.len()));
            for comment in &detail.comments {
                let place = comment
                    .inline
                    .as_ref()
                    .map(|(path, line)| match line {
                        Some(line) => format!(" em {path}:{line}"),
                        None => format!(" em {path}"),
                    })
                    .unwrap_or_default();
                text.push_str(&format!(
                    "- {}{}{}: {}\n",
                    if comment.is_reply { "  ↳ " } else { "" },
                    comment.author.short_name(),
                    place,
                    comment.body.trim()
                ));
            }
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn diff(project: Entity<Project>, input: PullRequestInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number).await?;
        let diff = bitbucket::get_pull_request_diff(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
        )
        .await?;
        let text = if diff.chars().count() > MAX_DIFF_CHARS {
            format!(
                "Diff do PR #{number} (cortado em {MAX_DIFF_CHARS} caracteres; leia os arquivos para o resto):\n{}",
                diff.chars().take(MAX_DIFF_CHARS).collect::<String>()
            )
        } else {
            format!("Diff do PR #{number}:\n{diff}")
        };
        Ok(ToolkitOutput::text(text))
    })
}

fn comment(project: Entity<Project>, input: CommentInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        if input.text.trim().is_empty() {
            return Err(anyhow!("O comentário está vazio."));
        }
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number).await?;
        bitbucket::post_comment(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
            &input.text,
        )
        .await?;
        Ok(ToolkitOutput::text(format!("Comentário publicado no PR #{number}.")))
    })
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "repo".into(),
            name: "Repo".into(),
            description: "Pull requests do Bitbucket conectado no dock Repo".into(),
            icon: "pull_request".into(),
            tools: vec![
                ToolkitTool::new(
                    "repo_pull_requests",
                    "Listou os PRs",
                    "Lists the open pull requests of the project's Bitbucket repository, marking \
                     the one opened from the current branch.",
                    ToolAccess::Read,
                    list,
                ),
                ToolkitTool::new(
                    "repo_pull_request",
                    "Leu o PR",
                    "Reads a pull request: description, checks, changed files, commits and \
                     comments. Defaults to the pull request of the current branch.",
                    ToolAccess::Read,
                    detail,
                ),
                ToolkitTool::new(
                    "repo_pr_diff",
                    "Leu o diff do PR",
                    "Returns the unified diff of a pull request against its destination branch. \
                     Defaults to the pull request of the current branch.",
                    ToolAccess::Read,
                    diff,
                ),
                ToolkitTool::new(
                    "repo_pr_comment",
                    "Comentou no PR",
                    "Publishes a comment on a pull request. Everyone on the pull request sees it, \
                     so only post what the user asked for.",
                    ToolAccess::Act,
                    comment,
                ),
            ],
        },
        cx,
    );
}
