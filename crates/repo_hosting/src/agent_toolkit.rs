//! The Repo toolkit for agents: the pull requests of the project's Bitbucket repository, their
//! diffs, comments and merges, with the account connected in the Repo dock. It also feeds the
//! agent's `@` menu, so a pull request can be mentioned like a file.

use crate::api::{
    ChangedFile, CheckState, FileChange, PullRequest, PullRequestDetail, PullRequestState,
    RemoteRepository,
};
use crate::bitbucket::{MergeOutcome, MergeStrategy};
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
use task_agents::{
    PullRequestCard, PullRequestCheckCard, PullRequestCheckState, PullRequestEntry,
    PullRequestMergeCard, PullRequestReference, PullRequestReviewerCard, PullRequestSource,
    REPO_MERGE_TOOL, REPO_PULL_REQUEST_TOOL, ToolAccess, Toolkit, ToolkitOutput, ToolkitTool,
};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DIFF_CHARS: usize = 80_000;
const MAX_FILE_DIFF_CHARS: usize = 20_000;
const MERGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MERGE_POLL_ATTEMPTS: usize = 90;
/// How long a new window waits for git to find the repository before giving up on listing its
/// pull requests for the `@` menu.
const PREFETCH_ATTEMPTS: usize = 30;

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
        let credentials = store
            .bitbucket_credentials()
            .context("Nenhuma conta do Bitbucket conectada. Peça para conectar no dock Repo.")?;
        Ok(RepoContext {
            client: store.http_client(),
            credentials,
            repository,
            branch,
        })
    })
}

async fn resolve_number(
    context: &RepoContext,
    number: Option<u64>,
    cx: &mut AsyncApp,
) -> Result<u64> {
    if let Some(number) = number {
        return Ok(number);
    }
    let branch = context
        .branch
        .as_deref()
        .context("Sem branch ativo; diga o número do PR.")?;
    let pull_requests = bitbucket::list_open_pull_requests(
        &context.client,
        &context.credentials,
        &context.repository,
    )
    .await?;
    remember_pull_requests(context, &pull_requests, cx);
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

#[derive(Deserialize, JsonSchema)]
struct LineCommentInput {
    /// The pull request number. Defaults to the one opened from the current branch.
    #[serde(default)]
    number: Option<u64>,
    /// The file's path in the repository, as the diff names it.
    path: String,
    /// The line in the pull request's new version of the file.
    line: u64,
    /// Markdown text of the comment.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct CreateInput {
    title: String,
    /// Markdown description.
    #[serde(default)]
    description: Option<String>,
    /// The branch to open it from, as the remote names it. Defaults to the current branch,
    /// which must already be pushed.
    #[serde(default)]
    source_branch: Option<String>,
    /// Defaults to the repository's development branch, or its main branch.
    #[serde(default)]
    destination_branch: Option<String>,
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize, JsonSchema)]
struct MergeInput {
    /// The pull request number. Defaults to the one opened from the current branch.
    #[serde(default)]
    number: Option<u64>,
    /// `merge_commit`, `squash` or `fast_forward`. Defaults to the destination branch's own
    /// default.
    #[serde(default)]
    strategy: Option<String>,
    /// Whether to delete the source branch on the remote after merging.
    #[serde(default)]
    close_source_branch: bool,
}

fn remember_pull_requests(context: &RepoContext, pull_requests: &[PullRequest], cx: &mut AsyncApp) {
    let repository = context.repository.clone();
    let pull_requests = pull_requests.to_vec();
    cx.update(|cx| {
        RepoStore::global(cx).update(cx, |store, _| {
            store.set_pull_requests(repository, pull_requests)
        })
    });
}

fn check_state_label(state: CheckState) -> &'static str {
    match state {
        CheckState::Passed => "passou",
        CheckState::Failed => "falhou",
        CheckState::Running => "rodando",
        CheckState::Stopped => "parado",
    }
}

fn describe_detail(detail: &PullRequestDetail, branch: Option<&str>) -> String {
    let pull_request = &detail.pull_request;
    let mut text = format!(
        "{}\n{}\n\n{}\n",
        pull_request_line(pull_request, branch),
        pull_request.url,
        if pull_request.description.trim().is_empty() {
            "(sem descrição)"
        } else {
            pull_request.description.trim()
        }
    );
    let reviewers = pull_request
        .participants
        .iter()
        .filter(|participant| participant.approved || participant.requested_changes)
        .map(|participant| {
            let verdict = if participant.requested_changes {
                "pediu mudanças"
            } else {
                "aprovou"
            };
            format!("- {} {verdict}\n", participant.person.short_name())
        })
        .collect::<String>();
    if !reviewers.is_empty() {
        text.push_str("\nRevisões:\n");
        text.push_str(&reviewers);
    }
    if !detail.checks.is_empty() {
        text.push_str("\nChecks:\n");
        for check in &detail.checks {
            text.push_str(&format!(
                "- {} · {}\n",
                check.name,
                check_state_label(check.state)
            ));
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
                "- [{}] {}{}{}: {}\n",
                comment.id,
                if comment.is_reply { "↳ " } else { "" },
                comment.author.short_name(),
                place,
                comment.body.trim()
            ));
        }
    }
    text
}

fn pull_request_card(detail: &PullRequestDetail, branch: Option<&str>) -> PullRequestCard {
    let pull_request = &detail.pull_request;
    PullRequestCard {
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.clone(),
        state: match pull_request.state {
            PullRequestState::Open => "open",
            PullRequestState::Merged => "merged",
            PullRequestState::Declined => "declined",
            PullRequestState::Superseded => "superseded",
        }
        .to_string(),
        draft: pull_request.draft,
        author: pull_request.author.short_name().to_string(),
        source_branch: pull_request.source_branch.clone(),
        destination_branch: pull_request.destination_branch.clone(),
        is_current_branch: Some(pull_request.source_branch.as_str()) == branch,
        checks: detail
            .checks
            .iter()
            .map(|check| PullRequestCheckCard {
                name: check.name.clone(),
                state: match check.state {
                    CheckState::Passed => PullRequestCheckState::Passed,
                    CheckState::Failed => PullRequestCheckState::Failed,
                    CheckState::Running => PullRequestCheckState::Running,
                    CheckState::Stopped => PullRequestCheckState::Stopped,
                },
                url: check.url.clone(),
            })
            .collect(),
        reviewers: pull_request
            .participants
            .iter()
            .filter(|participant| {
                participant.role == crate::api::ParticipantRole::Reviewer
                    || participant.approved
                    || participant.requested_changes
            })
            .map(|participant| PullRequestReviewerCard {
                name: participant.person.short_name().to_string(),
                approved: participant.approved,
                requested_changes: participant.requested_changes,
            })
            .collect(),
        file_count: detail.files.len(),
        lines_added: detail.files.iter().map(|file| file.lines_added).sum(),
        lines_removed: detail.files.iter().map(|file| file.lines_removed).sum(),
        conflict_count: detail
            .files
            .iter()
            .filter(|file| file.change == FileChange::Conflict)
            .count(),
        comment_count: pull_request.comment_count,
        open_task_count: pull_request.open_task_count,
    }
}

/// The part of a unified diff that changes `path`.
fn file_section<'a>(diff: &'a str, path: &str) -> Option<&'a str> {
    let header_suffix = format!(" b/{path}");
    let mut start = None;
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(start) = start {
                return diff.get(start..offset);
            }
            if line.trim_end().ends_with(&header_suffix) {
                start = Some(offset);
            }
        }
        offset += line.len();
    }
    start.and_then(|start| diff.get(start..))
}

fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() > limit {
        (text.chars().take(limit).collect(), true)
    } else {
        (text.to_string(), false)
    }
}

fn describe_reference(
    project: &Entity<Project>,
    reference: PullRequestReference,
    cx: &mut App,
) -> Task<Result<String>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let detail = bitbucket::get_pull_request_detail(
            &context.client,
            &context.credentials,
            &context.repository,
            reference.number,
        )
        .await?;
        let mut text = describe_detail(&detail, context.branch.as_deref());

        if let Some(comment_id) = reference.comment_id {
            let comment = detail
                .comments
                .iter()
                .find(|comment| comment.id == comment_id)
                .with_context(|| {
                    format!(
                        "O comentário {comment_id} não está mais no PR #{}.",
                        reference.number
                    )
                })?;
            let place = comment
                .inline
                .as_ref()
                .map(|(path, line)| match line {
                    Some(line) => format!(" em {path}:{line}"),
                    None => format!(" em {path}"),
                })
                .unwrap_or_default();
            text.push_str(&format!(
                "\nO usuário apontou o comentário [{}] de {}{}:\n{}\n",
                comment.id,
                comment.author.short_name(),
                place,
                comment.body.trim()
            ));
        }

        if let Some(path) = &reference.file_path {
            let diff = bitbucket::get_pull_request_diff(
                &context.client,
                &context.credentials,
                &context.repository,
                reference.number,
            )
            .await?;
            match file_section(&diff, path) {
                Some(section) => {
                    let (section, truncated) = truncate_chars(section, MAX_FILE_DIFF_CHARS);
                    text.push_str(&format!(
                        "\nO usuário apontou o arquivo {path}. O diff dele no PR{}:\n{section}\n",
                        if truncated { " (cortado)" } else { "" }
                    ));
                }
                None => text.push_str(&format!(
                    "\nO usuário apontou o arquivo {path}, que não aparece no diff do PR.\n"
                )),
            }
        }
        Ok(text)
    })
}

fn pull_request_entries(project: &Entity<Project>, cx: &App) -> Vec<PullRequestEntry> {
    let Some(store) = cx.try_global::<crate::GlobalRepoStore>() else {
        return Vec::new();
    };
    let (Target::Hosted { repository, .. }, branch) = detect_target(project, cx) else {
        return Vec::new();
    };
    let Some(pull_requests) = store.0.read(cx).cached_pull_requests(&repository) else {
        return Vec::new();
    };
    let mut entries = pull_requests
        .iter()
        .map(|pull_request| PullRequestEntry {
            number: pull_request.number,
            title: pull_request.title.clone(),
            author: pull_request.author.short_name().to_string(),
            source_branch: pull_request.source_branch.clone(),
            destination_branch: pull_request.destination_branch.clone(),
            is_current_branch: Some(pull_request.source_branch.as_str()) == branch.as_deref(),
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| !entry.is_current_branch);
    entries
}

/// Lists the project's pull requests once its repository shows up, so the agent's `@` menu has
/// them before anyone opens the Repo dock.
pub(crate) fn prefetch_pull_requests(project: Entity<Project>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut hosted = false;
        for _ in 0..PREFETCH_ATTEMPTS {
            hosted = cx.update(|cx| matches!(detect_target(&project, cx).0, Target::Hosted { .. }));
            if hosted {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(1)).await;
        }
        // Only Bitbucket projects read the keychain, as when the dock is opened.
        if !hosted {
            return;
        }
        let context = match repo_context(&project, cx).await {
            Ok(context) => context,
            // Not every project is on Bitbucket, and not every profile has an account.
            Err(_) => return,
        };
        match bitbucket::list_open_pull_requests(
            &context.client,
            &context.credentials,
            &context.repository,
        )
        .await
        {
            Ok(pull_requests) => remember_pull_requests(&context, &pull_requests, cx),
            Err(error) => log::warn!("Repo: falha ao listar os PRs para o Agent: {error:#}"),
        }
    })
    .detach();
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
        remember_pull_requests(&context, &pull_requests, cx);
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

fn detail(
    project: Entity<Project>,
    input: PullRequestInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number, cx).await?;
        let detail = bitbucket::get_pull_request_detail(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
        )
        .await?;
        let branch = context.branch.as_deref();
        Ok(ToolkitOutput::text(describe_detail(&detail, branch))
            .with_raw_output(pull_request_card(&detail, branch)))
    })
}

fn diff(
    project: Entity<Project>,
    input: PullRequestInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number, cx).await?;
        let diff = bitbucket::get_pull_request_diff(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
        )
        .await?;
        let (shown, truncated) = truncate_chars(&diff, MAX_DIFF_CHARS);
        let text = if truncated {
            let total_files = diff.matches("\ndiff --git ").count() + usize::from(diff.starts_with("diff --git "));
            let shown_files = shown.matches("\ndiff --git ").count() + usize::from(shown.starts_with("diff --git "));
            format!(
                "Diff do PR #{number}, cortado em {MAX_DIFF_CHARS} caracteres: mostra {shown_files} de {total_files} arquivos. Diga ao usuário quanto leu e leia o resto pelos arquivos:\n{shown}"
            )
        } else {
            format!("Diff do PR #{number}:\n{diff}")
        };
        Ok(ToolkitOutput::text(text))
    })
}

fn comment(
    project: Entity<Project>,
    input: CommentInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        if input.text.trim().is_empty() {
            return Err(anyhow!("O comentário está vazio."));
        }
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number, cx).await?;
        bitbucket::post_comment(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
            &input.text,
        )
        .await?;
        Ok(ToolkitOutput::text(format!(
            "Comentário publicado no PR #{number}."
        )))
    })
}

fn line_comment(
    project: Entity<Project>,
    input: LineCommentInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        if input.text.trim().is_empty() {
            return Err(anyhow!("O comentário está vazio."));
        }
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number, cx).await?;
        bitbucket::post_inline_comment(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
            &input.path,
            input.line,
            &input.text,
        )
        .await?;
        Ok(ToolkitOutput::text(format!(
            "Comentário publicado em {}:{} no PR #{number}.",
            input.path, input.line
        )))
    })
}

fn create(
    project: Entity<Project>,
    input: CreateInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        if input.title.trim().is_empty() {
            return Err(anyhow!("O PR precisa de um título."));
        }
        let context = repo_context(&project, cx).await?;
        let source_branch = input
            .source_branch
            .or_else(|| context.branch.clone())
            .context("Sem branch ativo; diga de qual branch abrir o PR.")?;
        let branches = bitbucket::list_branches(
            &context.client,
            &context.credentials,
            &context.repository,
        )
        .await?;
        if !branches.contains(&source_branch) {
            anyhow::bail!(
                "O branch `{source_branch}` não está no Bitbucket. Faça push dele antes de abrir o PR."
            );
        }
        let destination_branch = match input.destination_branch {
            Some(destination) => destination,
            None => bitbucket::get_default_destination(
                &context.client,
                &context.credentials,
                &context.repository,
            )
            .await?
            .context("O repositório não tem branch padrão; diga o destino do PR.")?,
        };
        let reviewer_ids = bitbucket::get_default_reviewers(
            &context.client,
            &context.credentials,
            &context.repository,
        )
        .await
        .map(|reviewers| reviewers.into_iter().map(|person| person.id).collect())
        .unwrap_or_else(|error| {
            log::warn!("Repo: revisores padrão indisponíveis: {error:#}");
            Vec::new()
        });
        let created = bitbucket::create_pull_request(
            &context.client,
            &context.credentials,
            &context.repository,
            bitbucket::NewPullRequest {
                title: input.title.trim().to_string(),
                description: input.description.unwrap_or_default(),
                source_branch: source_branch.clone(),
                destination_branch: destination_branch.clone(),
                reviewer_ids,
                close_source_branch: false,
                draft: input.draft,
            },
        )
        .await?;
        Ok(ToolkitOutput::text(format!(
            "PR #{} aberto: {} ({source_branch} → {destination_branch})\n{}",
            created.number, created.title, created.url
        )))
    })
}

fn merge(project: Entity<Project>, input: MergeInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let context = repo_context(&project, cx).await?;
        let number = resolve_number(&context, input.number, cx).await?;
        let detail = bitbucket::get_pull_request_detail(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
        )
        .await?;
        let pull_request = &detail.pull_request;
        if pull_request.state != PullRequestState::Open {
            anyhow::bail!("O PR #{number} não está aberto.");
        }
        let conflicts = detail
            .files
            .iter()
            .filter(|file| file.change == FileChange::Conflict)
            .count();
        if conflicts > 0 {
            anyhow::bail!(
                "O PR #{number} tem conflitos em {conflicts} arquivo(s) com {}; resolva antes do merge.",
                pull_request.destination_branch
            );
        }
        let strategies = bitbucket::get_merge_strategies(
            &context.client,
            &context.credentials,
            &context.repository,
            &pull_request.destination_branch,
        )
        .await?;
        let strategy = match input.strategy.as_deref() {
            Some(name) => strategies
                .iter()
                .copied()
                .find(|strategy| strategy_matches(*strategy, name))
                .with_context(|| {
                    format!(
                        "O destino não aceita a estratégia `{name}`. Aceita: {}.",
                        strategies
                            .iter()
                            .map(|strategy| strategy.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
            None => strategies
                .first()
                .copied()
                .context("O destino não aceita nenhuma estratégia de merge.")?,
        };
        let message = format!(
            "Merged in {} (pull request #{number})\n\n{}",
            pull_request.source_branch, pull_request.title
        );
        let outcome = bitbucket::merge_pull_request(
            &context.client,
            &context.credentials,
            &context.repository,
            number,
            strategy,
            &message,
            input.close_source_branch,
        )
        .await?;
        if let MergeOutcome::Pending { task_url } = outcome {
            let mut merged = false;
            for _ in 0..MERGE_POLL_ATTEMPTS {
                cx.background_executor().timer(MERGE_POLL_INTERVAL).await;
                if bitbucket::get_merge_task(
                    &context.client,
                    &context.credentials,
                    &context.repository,
                    &task_url,
                )
                .await?
                .is_some()
                {
                    merged = true;
                    break;
                }
            }
            if !merged {
                anyhow::bail!(
                    "O Bitbucket ainda está fazendo o merge do PR #{number}; confira no dock Repo."
                );
            }
        }
        let card = PullRequestMergeCard {
            number,
            title: pull_request.title.clone(),
            url: pull_request.url.clone(),
            destination_branch: pull_request.destination_branch.clone(),
            strategy: strategy.label().to_string(),
        };
        Ok(ToolkitOutput::text(format!(
            "PR #{number} mergeado em {} ({}).",
            card.destination_branch, card.strategy
        ))
        .with_raw_output(card))
    })
}

fn strategy_matches(strategy: MergeStrategy, name: &str) -> bool {
    let normalize = |text: &str| text.trim().to_lowercase().replace([' ', '-'], "_");
    let name = normalize(name);
    name == strategy.api_name() || name == normalize(strategy.label())
}

const PREFER_OVER_MCP: &str =
    "Use this instead of any Bitbucket MCP server tool: it uses the account of the Repo dock.";

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_pull_request_source(
        PullRequestSource {
            list: Arc::new(pull_request_entries),
            describe: Arc::new(describe_reference),
        },
        cx,
    );
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
                    format!(
                        "Lists the open pull requests of the project's Bitbucket repository, \
                         marking the one opened from the current branch. {PREFER_OVER_MCP}"
                    ),
                    ToolAccess::Read,
                    list,
                ),
                ToolkitTool::new(
                    REPO_PULL_REQUEST_TOOL,
                    "Leu o PR",
                    format!(
                        "Reads a pull request: description, reviews, checks, changed files, \
                         commits and comments (with their ids). Defaults to the pull request of \
                         the current branch. The user sees it as a card with the checks and \
                         reviews, so answer with what matters instead of repeating them. \
                         {PREFER_OVER_MCP}"
                    ),
                    ToolAccess::Read,
                    detail,
                ),
                ToolkitTool::new(
                    "repo_pr_diff",
                    "Leu o diff do PR",
                    format!(
                        "Returns the unified diff of a pull request against its destination \
                         branch. Defaults to the pull request of the current branch. A big diff \
                         is cut; say how much of it you read. {PREFER_OVER_MCP}"
                    ),
                    ToolAccess::Read,
                    diff,
                ),
                ToolkitTool::new(
                    "repo_pr_comment",
                    "Comentou no PR",
                    "Publishes a general comment on a pull request. Everyone on the pull request \
                     sees it, so only post what the user asked for.",
                    ToolAccess::Act,
                    comment,
                ),
                ToolkitTool::new(
                    "repo_pr_line_comment",
                    "Comentou na linha",
                    "Publishes a comment on a line of a file in a pull request, the way a \
                     reviewer does. Everyone on the pull request sees it, so only post what the \
                     user asked for.",
                    ToolAccess::Act,
                    line_comment,
                ),
                ToolkitTool::new(
                    "repo_create_pr",
                    "Abriu um PR",
                    "Opens a pull request from a branch that is already pushed, with the \
                     repository's default reviewers. Push the branch first when it isn't.",
                    ToolAccess::Act,
                    create,
                ),
                ToolkitTool::new(
                    REPO_MERGE_TOOL,
                    "Fez merge do PR",
                    "Merges a pull request on Bitbucket. Only when the user asked for it: read \
                     the pull request first so they see its checks and reviews, and the user \
                     confirms every merge.",
                    ToolAccess::AlwaysConfirm,
                    merge,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/src/checkout/usePix.ts b/src/checkout/usePix.ts\n--- a/src/checkout/usePix.ts\n+++ b/src/checkout/usePix.ts\n@@ -41 +41 @@\n-await sleep(1000)\n+await sleep(backoff(attempt))\ndiff --git a/src/c.ts b/src/c.ts\n+c\n";

    #[test]
    fn cuts_the_diff_of_one_file() {
        let section = file_section(DIFF, "src/checkout/usePix.ts").unwrap();
        assert!(section.starts_with("diff --git a/src/checkout/usePix.ts"));
        assert!(section.contains("backoff(attempt)"));
        assert!(!section.contains("src/c.ts"));
        assert!(file_section(DIFF, "src/c.ts").unwrap().ends_with("+c\n"));
        assert_eq!(file_section(DIFF, "usePix.ts"), None);
    }

    #[test]
    fn matches_strategies_by_api_name_or_label() {
        assert!(strategy_matches(MergeStrategy::Squash, "squash"));
        assert!(strategy_matches(MergeStrategy::MergeCommit, "merge_commit"));
        assert!(strategy_matches(MergeStrategy::MergeCommit, "Merge commit"));
        assert!(strategy_matches(MergeStrategy::FastForward, "fast-forward"));
        assert!(!strategy_matches(MergeStrategy::Squash, "fast_forward"));
    }
}
