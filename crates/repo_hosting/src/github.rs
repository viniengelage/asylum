//! GitHub's REST API, authenticated with the token the `gh` CLI already holds or with a
//! fine-grained token pasted in the panel. Auto-merge only exists in the GraphQL API.

use crate::api::{
    ApiError, ChangedFile, Check, CheckState, Comment, Commit, FileChange, Hosting, Issue,
    MergeOutcome, MergeStrategy, NewPullRequest, Participant, ParticipantRole, Person, Pipeline,
    PipelineState, PullRequest, PullRequestDetail, PullRequestState, RemoteRepository,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashMap, sync::Arc};

const API_URL: &str = "https://api.github.com";
const HOST: &str = "O GitHub";

/// Where GitHub lets you create fine-grained tokens.
pub const TOKEN_SETTINGS_URL: &str = "https://github.com/settings/personal-access-tokens/new";

/// The fine-grained permissions the panel and the tasks dock use, with what each is for.
pub const TOKEN_PERMISSIONS: [(&str, &str, &str); 5] = [
    ("Contents", "leitura", "diff e commits"),
    ("Pull requests", "escrita", "criar, comentar, merge"),
    ("Issues", "escrita", "itens das tarefas"),
    ("Actions", "escrita", "checks e re-rodar"),
    ("Projects", "escrita", "status e campos"),
];

/// Enough for any team's open pull requests while keeping a runaway `next` chain bounded.
const MAX_PAGES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub token: String,
}

/// The repository an `origin` URL points at, when it is on github.com.
pub fn parse_remote(url: &str) -> Option<RemoteRepository> {
    use git::GitHostingProvider as _;
    let parsed = git_hosting_providers::Github::public_instance().parse_remote_url(url)?;
    Some(RemoteRepository {
        hosting: Hosting::GitHub,
        owner: parsed.owner.to_string(),
        name: parsed.repo.to_string(),
    })
}

/// The token `gh auth login` saved, when the GitHub CLI is installed and logged in.
pub async fn token_from_gh_cli() -> Result<String> {
    let output = util::command::new_command("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .await
        .context("o GitHub CLI (gh) não está instalado")?;
    anyhow::ensure!(
        output.status.success(),
        "o gh não está logado: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let token = String::from_utf8(output.stdout)?.trim().to_string();
    anyhow::ensure!(!token.is_empty(), "o gh não devolveu um token");
    Ok(token)
}

#[derive(Deserialize)]
struct User {
    login: String,
    #[serde(default)]
    name: Option<String>,
}

impl From<User> for Person {
    fn from(user: User) -> Self {
        Person {
            // Requests for review and assignees go by login, so it doubles as the id.
            id: user.login.clone(),
            display_name: user
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| user.login.clone()),
            nickname: Some(user.login),
        }
    }
}

/// The account behind the token and the OAuth scopes it carries. Fine-grained tokens don't
/// list theirs, so an empty list means "unknown", not "none".
pub async fn get_user(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
) -> Result<(Person, Vec<String>)> {
    let response = send_raw(
        client,
        credentials,
        http_client::Method::GET,
        &format!("{API_URL}/user"),
        None,
        "application/vnd.github+json",
    )
    .await?;
    let scopes = response
        .headers
        .get("x-oauth-scopes")
        .map(|scopes| {
            scopes
                .split(',')
                .map(|scope| scope.trim().to_string())
                .filter(|scope| !scope.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let user: User = response.parse()?;
    Ok((user.into(), scopes))
}

fn repository_path(repository: &RemoteRepository) -> String {
    format!(
        "{API_URL}/repos/{}/{}",
        urlencoding::encode(&repository.owner),
        urlencoding::encode(&repository.name)
    )
}

#[derive(Deserialize)]
struct RepositoryReference {
    full_name: String,
}

#[derive(Deserialize)]
struct Endpoint {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    #[serde(default)]
    repo: Option<RepositoryReference>,
}

#[derive(Deserialize)]
struct GitHubPullRequest {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    user: User,
    head: Endpoint,
    base: Endpoint,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    review_comments: u64,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    merged_at: Option<DateTime<Utc>>,
    html_url: String,
    #[serde(default)]
    requested_reviewers: Vec<User>,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    auto_merge: Option<serde_json::Value>,
    #[serde(default)]
    node_id: String,
}

impl GitHubPullRequest {
    fn into_pull_request(self, reviews: Vec<Review>) -> PullRequest {
        let mut participants: Vec<Participant> = self
            .requested_reviewers
            .into_iter()
            .map(|user| Participant {
                person: user.into(),
                role: ParticipantRole::Reviewer,
                approved: false,
                requested_changes: false,
                participated_at: None,
            })
            .collect();
        // Only someone's latest verdict counts; comments don't change it.
        let mut verdicts: HashMap<String, Review> = HashMap::new();
        for review in reviews {
            if matches!(
                review.state.as_str(),
                "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
            ) {
                verdicts.insert(review.user.login.clone(), review);
            }
        }
        for (_, review) in verdicts {
            let approved = review.state == "APPROVED";
            let requested_changes = review.state == "CHANGES_REQUESTED";
            let person: Person = review.user.into();
            if let Some(existing) = participants
                .iter_mut()
                .find(|participant| participant.person.id == person.id)
            {
                existing.approved = approved;
                existing.requested_changes = requested_changes;
                existing.participated_at = review.submitted_at;
            } else {
                // Someone who already reviewed leaves the requested list; they were still asked.
                participants.push(Participant {
                    person,
                    role: ParticipantRole::Reviewer,
                    approved,
                    requested_changes,
                    participated_at: review.submitted_at,
                });
            }
        }
        PullRequest {
            number: self.number,
            title: self.title,
            state: match (self.state.as_str(), self.merged_at) {
                (_, Some(_)) => PullRequestState::Merged,
                ("closed", None) => PullRequestState::Declined,
                _ => PullRequestState::Open,
            },
            draft: self.draft,
            author: self.user.into(),
            source_branch: self.head.name,
            source_repository: self.head.repo.map(|repository| repository.full_name),
            destination_branch: self.base.name,
            description: self.body.unwrap_or_default(),
            comment_count: self.comments + self.review_comments,
            open_task_count: 0,
            updated_at: self.updated_at,
            url: self.html_url,
            participants,
        }
    }
}

#[derive(Deserialize)]
struct Review {
    user: User,
    state: String,
    #[serde(default)]
    submitted_at: Option<DateTime<Utc>>,
}

/// The open pull requests, most recently updated first, with who was asked to review.
pub async fn list_open_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<PullRequest>> {
    let url = format!(
        "{}/pulls?state=open&sort=updated&direction=desc&per_page=100",
        repository_path(repository)
    );
    let pull_requests: Vec<GitHubPullRequest> = get_all_pages(client, credentials, url).await?;
    Ok(pull_requests
        .into_iter()
        .map(|pull_request| pull_request.into_pull_request(Vec::new()))
        .collect())
}

/// The most recently updated merged or closed pull requests. One page.
pub async fn list_closed_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    state: PullRequestState,
) -> Result<Vec<PullRequest>> {
    let url = format!(
        "{}/pulls?state=closed&sort=updated&direction=desc&per_page=50",
        repository_path(repository)
    );
    let pull_requests: Vec<GitHubPullRequest> = get(client, credentials, &url).await?;
    Ok(pull_requests
        .into_iter()
        .map(|pull_request| pull_request.into_pull_request(Vec::new()))
        .filter(|pull_request| pull_request.state == state)
        .collect())
}

/// The Actions runs, newest first.
pub async fn list_pipelines(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<Pipeline>> {
    #[derive(Deserialize)]
    struct Actor {
        login: String,
    }
    #[derive(Deserialize)]
    struct PullReference {
        number: u64,
    }
    #[derive(Deserialize)]
    struct Run {
        run_number: u64,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default)]
        head_branch: Option<String>,
        #[serde(default)]
        event: String,
        #[serde(default)]
        actor: Option<Actor>,
        #[serde(default)]
        head_sha: Option<String>,
        #[serde(default)]
        created_at: Option<DateTime<Utc>>,
        #[serde(default)]
        run_started_at: Option<DateTime<Utc>>,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
        html_url: String,
        #[serde(default)]
        pull_requests: Vec<PullReference>,
    }
    #[derive(Deserialize)]
    struct Runs {
        workflow_runs: Vec<Run>,
    }
    let url = format!("{}/actions/runs?per_page=30", repository_path(repository));
    let runs: Runs = get(client, credentials, &url).await?;
    Ok(runs
        .workflow_runs
        .into_iter()
        .map(|run| {
            let state = run_state(run.status.as_deref(), run.conclusion.as_deref());
            let finished = !matches!(state, PipelineState::Running | PipelineState::Pending);
            let duration_seconds = match (run.run_started_at, run.updated_at) {
                (Some(started), Some(updated)) if finished => {
                    u64::try_from((updated - started).num_seconds()).ok()
                }
                _ => None,
            };
            Pipeline {
                number: run.run_number,
                state,
                pull_request: run.pull_requests.first().map(|pull| pull.number),
                ref_name: run.head_branch,
                trigger: run.event,
                creator: run.actor.map(|actor| actor.login),
                commit: run.head_sha,
                created_at: run.created_at,
                duration_seconds,
                url: run.html_url,
            }
        })
        .collect())
}

fn run_state(status: Option<&str>, conclusion: Option<&str>) -> PipelineState {
    match (status, conclusion) {
        (Some("completed"), Some("success")) => PipelineState::Passed,
        (Some("completed"), Some("failure" | "timed_out" | "startup_failure")) => {
            PipelineState::Failed
        }
        (Some("completed"), _) => PipelineState::Stopped,
        (Some("in_progress"), _) => PipelineState::Running,
        (Some("waiting" | "action_required"), _) => PipelineState::Paused,
        _ => PipelineState::Pending,
    }
}

/// The open issues, without the pull requests GitHub lists alongside them. `None` when the
/// repository turned issues off.
pub async fn list_open_issues(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<Vec<Issue>>> {
    #[derive(Deserialize)]
    struct Repository {
        #[serde(default)]
        has_issues: bool,
    }
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }
    #[derive(Deserialize)]
    struct GitHubIssue {
        number: u64,
        title: String,
        state: String,
        #[serde(default)]
        labels: Vec<Label>,
        #[serde(default)]
        assignee: Option<User>,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
        html_url: String,
        #[serde(default)]
        pull_request: Option<serde_json::Value>,
    }
    let base = repository_path(repository);
    let details: Repository = get(client, credentials, &base).await?;
    if !details.has_issues {
        return Ok(None);
    }
    let issues: Vec<GitHubIssue> = get(
        client,
        credentials,
        &format!("{base}/issues?state=open&sort=updated&per_page=50"),
    )
    .await?;
    Ok(Some(
        issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .map(|issue| {
                let mut labels = issue.labels.into_iter().map(|label| label.name);
                Issue {
                    number: issue.number,
                    title: issue.title,
                    state: issue.state,
                    kind: labels.next().unwrap_or_default(),
                    priority: String::new(),
                    assignee: issue
                        .assignee
                        .map(|user| Person::from(user).short_name().to_string()),
                    updated_at: issue.updated_at,
                    url: issue.html_url,
                }
            })
            .collect(),
    ))
}

/// The pull request's diff as a unified patch.
pub async fn get_pull_request_diff(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<String> {
    let url = format!("{}/pulls/{number}", repository_path(repository));
    let response = send_raw(
        client,
        credentials,
        http_client::Method::GET,
        &url,
        None,
        "application/vnd.github.diff",
    )
    .await?;
    Ok(response.text)
}

/// The check runs and commit statuses on the pull request's head commit.
pub async fn get_checks(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<Vec<Check>> {
    let pull_request: GitHubPullRequest = get(
        client,
        credentials,
        &format!("{}/pulls/{number}", repository_path(repository)),
    )
    .await?;
    get_commit_checks(client, credentials, repository, &pull_request.head.sha).await
}

async fn get_commit_checks(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    sha: &str,
) -> Result<Vec<Check>> {
    #[derive(Deserialize)]
    struct App {
        #[serde(default)]
        slug: Option<String>,
    }
    #[derive(Deserialize)]
    struct CheckRun {
        id: u64,
        name: String,
        status: String,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default)]
        html_url: Option<String>,
        #[serde(default)]
        completed_at: Option<DateTime<Utc>>,
        #[serde(default)]
        started_at: Option<DateTime<Utc>>,
        #[serde(default)]
        app: Option<App>,
    }
    #[derive(Deserialize)]
    struct CheckRuns {
        check_runs: Vec<CheckRun>,
    }
    #[derive(Deserialize)]
    struct Status {
        context: String,
        state: String,
        #[serde(default)]
        target_url: Option<String>,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct CombinedStatus {
        #[serde(default)]
        statuses: Vec<Status>,
    }

    let base = repository_path(repository);
    let runs_url = format!("{base}/commits/{sha}/check-runs?per_page=100");
    let statuses_url = format!("{base}/commits/{sha}/status");
    let runs = get::<CheckRuns>(client, credentials, &runs_url);
    let statuses = get::<CombinedStatus>(client, credentials, &statuses_url);
    let (runs, statuses) = futures::join!(runs, statuses);

    let mut checks: Vec<Check> = runs?
        .check_runs
        .into_iter()
        .map(|run| {
            let is_actions = run
                .app
                .and_then(|app| app.slug)
                .is_some_and(|slug| slug == "github-actions");
            Check {
                state: match (run.status.as_str(), run.conclusion.as_deref()) {
                    ("completed", Some("success" | "neutral" | "skipped")) => CheckState::Passed,
                    ("completed", Some("failure" | "timed_out" | "action_required")) => {
                        CheckState::Failed
                    }
                    ("completed", _) => CheckState::Stopped,
                    _ => CheckState::Running,
                },
                name: run.name,
                url: run.html_url,
                updated_at: run.completed_at.or(run.started_at),
                rerun_job: is_actions.then_some(run.id),
            }
        })
        .collect();
    checks.extend(statuses?.statuses.into_iter().map(|status| Check {
        name: status.context,
        state: match status.state.as_str() {
            "success" => CheckState::Passed,
            "failure" | "error" => CheckState::Failed,
            _ => CheckState::Running,
        },
        url: status.target_url.filter(|url| !url.is_empty()),
        updated_at: status.updated_at,
        rerun_job: None,
    }));
    Ok(checks)
}

pub async fn get_pull_request_detail(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<PullRequestDetail> {
    #[derive(Deserialize)]
    struct File {
        filename: String,
        #[serde(default)]
        previous_filename: Option<String>,
        status: String,
        #[serde(default)]
        additions: u64,
        #[serde(default)]
        deletions: u64,
    }
    #[derive(Deserialize)]
    struct IssueComment {
        id: u64,
        user: User,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        created_at: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct ReviewComment {
        id: u64,
        user: User,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        created_at: Option<DateTime<Utc>>,
        path: String,
        #[serde(default)]
        line: Option<u64>,
        #[serde(default)]
        original_line: Option<u64>,
        #[serde(default)]
        in_reply_to_id: Option<u64>,
    }
    #[derive(Deserialize)]
    struct GitAuthor {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        date: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct GitCommit {
        #[serde(default)]
        message: String,
        #[serde(default)]
        author: Option<GitAuthor>,
    }
    #[derive(Deserialize)]
    struct GitHubCommit {
        sha: String,
        commit: GitCommit,
        #[serde(default)]
        author: Option<User>,
    }

    let base = format!("{}/pulls/{number}", repository_path(repository));
    let issue_base = format!("{}/issues/{number}", repository_path(repository));
    let pull_request = get::<GitHubPullRequest>(client, credentials, &base);
    let reviews =
        get_all_pages::<Review>(client, credentials, format!("{base}/reviews?per_page=100"));
    let files = get_all_pages::<File>(client, credentials, format!("{base}/files?per_page=100"));
    let issue_comments = get_all_pages::<IssueComment>(
        client,
        credentials,
        format!("{issue_base}/comments?per_page=100"),
    );
    let review_comments = get_all_pages::<ReviewComment>(
        client,
        credentials,
        format!("{base}/comments?per_page=100"),
    );
    let commits =
        get_all_pages::<GitHubCommit>(client, credentials, format!("{base}/commits?per_page=100"));
    let (pull_request, reviews, files, issue_comments, review_comments, commits) = futures::join!(
        pull_request,
        reviews,
        files,
        issue_comments,
        review_comments,
        commits
    );
    let pull_request = pull_request?;
    let checks = get_commit_checks(client, credentials, repository, &pull_request.head.sha).await?;

    let mut comments: Vec<Comment> = issue_comments?
        .into_iter()
        .map(|comment| Comment {
            id: comment.id,
            author: comment.user.into(),
            body: comment.body.unwrap_or_default(),
            created_at: comment.created_at,
            inline: None,
            is_reply: false,
        })
        .collect();
    comments.extend(review_comments?.into_iter().map(|comment| Comment {
        id: comment.id,
        author: comment.user.into(),
        body: comment.body.unwrap_or_default(),
        created_at: comment.created_at,
        inline: Some((comment.path, comment.line.or(comment.original_line))),
        is_reply: comment.in_reply_to_id.is_some(),
    }));
    comments.sort_by_key(|comment| comment.created_at);

    let has_conflicts = pull_request.mergeable == Some(false);
    let auto_merge = pull_request.auto_merge.is_some();
    let mut commits: Vec<Commit> = commits?
        .into_iter()
        .map(|commit| {
            let git_author = commit.commit.author;
            Commit {
                summary: commit
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                author: commit
                    .author
                    .map(|user| user.login)
                    .or_else(|| git_author.as_ref().and_then(|author| author.name.clone()))
                    .unwrap_or_default(),
                date: git_author.and_then(|author| author.date),
                hash: commit.sha,
            }
        })
        .collect();
    // GitHub lists them oldest first.
    commits.reverse();

    Ok(PullRequestDetail {
        pull_request: pull_request.into_pull_request(reviews?),
        checks,
        files: files?
            .into_iter()
            .map(|file| {
                let change = match file.status.as_str() {
                    "added" => FileChange::Added,
                    "removed" => FileChange::Removed,
                    "renamed" => FileChange::Renamed,
                    _ => FileChange::Modified,
                };
                ChangedFile {
                    path: file.filename,
                    old_path: file.previous_filename,
                    change,
                    lines_added: file.additions,
                    lines_removed: file.deletions,
                }
            })
            .collect(),
        comments,
        commits,
        has_conflicts,
        auto_merge,
    })
}

pub async fn post_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    text: &str,
) -> Result<()> {
    let url = format!("{}/issues/{number}/comments", repository_path(repository));
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &url,
        Some(serde_json::json!({ "body": text })),
    )
    .await?;
    Ok(())
}

/// A comment on a line of the pull request's new version of `path`.
pub async fn post_inline_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    path: &str,
    line: u64,
    text: &str,
) -> Result<()> {
    let base = format!("{}/pulls/{number}", repository_path(repository));
    let pull_request: GitHubPullRequest = get(client, credentials, &base).await?;
    let body = serde_json::json!({
        "body": text,
        "commit_id": pull_request.head.sha,
        "path": path,
        "line": line,
        "side": "RIGHT",
    });
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!("{base}/comments"),
        Some(body),
    )
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct RepositoryDetails {
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default = "default_true")]
    allow_merge_commit: bool,
    #[serde(default = "default_true")]
    allow_squash_merge: bool,
    #[serde(default = "default_true")]
    allow_rebase_merge: bool,
    #[serde(default)]
    allow_auto_merge: bool,
}

fn default_true() -> bool {
    true
}

pub async fn get_default_destination(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<String>> {
    let details: RepositoryDetails = get(client, credentials, &repository_path(repository)).await?;
    Ok(details.default_branch)
}

pub async fn list_branches(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Branch {
        name: String,
    }
    let url = format!("{}/branches?per_page=100", repository_path(repository));
    let branches: Vec<Branch> = get_all_pages(client, credentials, url).await?;
    Ok(branches.into_iter().map(|branch| branch.name).collect())
}

/// The commits on `source` that `destination` doesn't have, newest first.
pub async fn list_commits_between(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    source: &str,
    destination: &str,
) -> Result<Vec<Commit>> {
    #[derive(Deserialize)]
    struct GitAuthor {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        date: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct GitCommit {
        #[serde(default)]
        message: String,
        #[serde(default)]
        author: Option<GitAuthor>,
    }
    #[derive(Deserialize)]
    struct GitHubCommit {
        sha: String,
        commit: GitCommit,
        #[serde(default)]
        author: Option<User>,
    }
    #[derive(Deserialize)]
    struct Comparison {
        #[serde(default)]
        commits: Vec<GitHubCommit>,
    }
    let url = format!(
        "{}/compare/{}...{}?per_page=30",
        repository_path(repository),
        urlencoding::encode(destination),
        urlencoding::encode(source)
    );
    let comparison: Comparison = get(client, credentials, &url).await?;
    Ok(comparison
        .commits
        .into_iter()
        .rev()
        .map(|commit| {
            let git_author = commit.commit.author;
            Commit {
                summary: commit
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                author: commit
                    .author
                    .map(|user| user.login)
                    .or_else(|| git_author.as_ref().and_then(|author| author.name.clone()))
                    .unwrap_or_default(),
                date: git_author.and_then(|author| author.date),
                hash: commit.sha,
            }
        })
        .collect())
}

pub async fn create_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    new: NewPullRequest,
) -> Result<PullRequest> {
    let base = format!("{}/pulls", repository_path(repository));
    let created: GitHubPullRequest = send(
        client,
        credentials,
        http_client::Method::POST,
        &base,
        Some(serde_json::json!({
            "title": new.title,
            "body": new.description,
            "head": new.source_branch,
            "base": new.destination_branch,
            "draft": new.draft,
        })),
    )
    .await?;
    if !new.reviewer_ids.is_empty() {
        send::<serde_json::Value>(
            client,
            credentials,
            http_client::Method::POST,
            &format!("{base}/{}/requested_reviewers", created.number),
            Some(serde_json::json!({ "reviewers": new.reviewer_ids })),
        )
        .await
        .context("o PR foi criado, mas o GitHub recusou os revisores")?;
    }
    Ok(created.into_pull_request(Vec::new()))
}

/// The merge methods the repository allows, plus whether it lets pull requests auto-merge.
pub async fn get_merge_strategies(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<(Vec<MergeStrategy>, bool)> {
    let details: RepositoryDetails = get(client, credentials, &repository_path(repository)).await?;
    let mut strategies = Vec::new();
    if details.allow_merge_commit {
        strategies.push(MergeStrategy::MergeCommit);
    }
    if details.allow_squash_merge {
        strategies.push(MergeStrategy::Squash);
    }
    if details.allow_rebase_merge {
        strategies.push(MergeStrategy::RebaseMerge);
    }
    Ok((strategies, details.allow_auto_merge))
}

fn merge_method(strategy: MergeStrategy) -> &'static str {
    match strategy {
        MergeStrategy::Squash | MergeStrategy::SquashFastForward => "squash",
        MergeStrategy::RebaseMerge | MergeStrategy::RebaseFastForward => "rebase",
        MergeStrategy::MergeCommit | MergeStrategy::FastForward => "merge",
    }
}

pub async fn merge_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    strategy: MergeStrategy,
    message: &str,
    delete_source_branch: bool,
) -> Result<MergeOutcome> {
    let base = format!("{}/pulls/{number}", repository_path(repository));
    let (title, body) = message.split_once('\n').unwrap_or((message, ""));
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::PUT,
        &format!("{base}/merge"),
        Some(serde_json::json!({
            "merge_method": merge_method(strategy),
            "commit_title": title.trim(),
            "commit_message": body.trim(),
        })),
    )
    .await?;
    let merged: GitHubPullRequest = get(client, credentials, &base).await?;
    let same_repository = merged
        .head
        .repo
        .as_ref()
        .is_some_and(|head| head.full_name.eq_ignore_ascii_case(&repository.full_name()));
    if delete_source_branch && same_repository {
        let branch_ref = format!(
            "{}/git/refs/heads/{}",
            repository_path(repository),
            merged.head.name
        );
        send::<serde_json::Value>(
            client,
            credentials,
            http_client::Method::DELETE,
            &branch_ref,
            None,
        )
        .await
        .context("o merge entrou, mas o GitHub não apagou o branch")?;
    }
    Ok(MergeOutcome::Merged(merged.into_pull_request(Vec::new())))
}

/// Asks GitHub to merge once the required checks and reviews pass.
pub async fn enable_auto_merge(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    strategy: MergeStrategy,
) -> Result<()> {
    let pull_request: GitHubPullRequest = get(
        client,
        credentials,
        &format!("{}/pulls/{number}", repository_path(repository)),
    )
    .await?;
    let method = merge_method(strategy).to_uppercase();
    graphql::<serde_json::Value>(
        client,
        credentials,
        "mutation($id: ID!, $method: PullRequestMergeMethod!) { enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method}) { clientMutationId } }",
        serde_json::json!({ "id": pull_request.node_id, "method": method }),
    )
    .await?;
    Ok(())
}

pub async fn decline_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<()> {
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::PATCH,
        &format!("{}/pulls/{number}", repository_path(repository)),
        Some(serde_json::json!({ "state": "closed" })),
    )
    .await?;
    Ok(())
}

pub async fn set_approval(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    approve: bool,
) -> Result<()> {
    anyhow::ensure!(
        approve,
        "o GitHub não deixa retirar uma aprovação; peça mudanças numa nova revisão"
    );
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!("{}/pulls/{number}/reviews", repository_path(repository)),
        Some(serde_json::json!({ "event": "APPROVE" })),
    )
    .await?;
    Ok(())
}

/// Runs an Actions job again, with the same commit and inputs.
pub async fn rerun_job(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    job_id: u64,
) -> Result<()> {
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!(
            "{}/actions/jobs/{job_id}/rerun",
            repository_path(repository)
        ),
        None,
    )
    .await?;
    Ok(())
}

/// The end of an Actions job's log, where the failure usually is.
pub async fn get_job_log(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    job_id: u64,
) -> Result<String> {
    let response = send_raw(
        client,
        credentials,
        http_client::Method::GET,
        &format!("{}/actions/jobs/{job_id}/logs", repository_path(repository)),
        None,
        "application/vnd.github+json",
    )
    .await?;
    Ok(response.text)
}

/// A GraphQL query or mutation, with GitHub's `errors` turned into a failure.
pub async fn graphql<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    query: &str,
    variables: serde_json::Value,
) -> Result<T> {
    #[derive(Deserialize)]
    struct GraphQlError {
        message: String,
        #[serde(default, rename = "type")]
        kind: Option<String>,
    }
    #[derive(Deserialize)]
    struct GraphQlResponse<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GraphQlError>,
    }
    let response: GraphQlResponse<T> = send(
        client,
        credentials,
        http_client::Method::POST,
        &format!("{API_URL}/graphql"),
        Some(serde_json::json!({ "query": query, "variables": variables })),
    )
    .await?;
    if let Some(error) = response.errors.first() {
        if matches!(
            error.kind.as_deref(),
            Some("FORBIDDEN" | "INSUFFICIENT_SCOPES")
        ) {
            return Err(ApiError::Forbidden {
                host: HOST,
                body: error.message.clone(),
            }
            .into());
        }
        anyhow::bail!("o GitHub recusou: {}", error.message);
    }
    response
        .data
        .context("o GitHub respondeu sem dados nem erros")
}

async fn get_all_pages<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    first_url: String,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    let mut url = Some(first_url);
    for _ in 0..MAX_PAGES {
        let Some(current) = url.take() else {
            break;
        };
        let response = send_raw(
            client,
            credentials,
            http_client::Method::GET,
            &current,
            None,
            "application/vnd.github+json",
        )
        .await?;
        let page: Vec<T> = response.parse()?;
        values.extend(page);
        // The token only ever goes to the API host.
        url = response
            .headers
            .get("link")
            .and_then(|link| next_page(link))
            .filter(|next| next.starts_with(API_URL));
    }
    Ok(values)
}

/// The `rel="next"` URL of a `Link` header.
fn next_page(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, rest) = part.split_once(';')?;
        rest.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

async fn get<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    url: &str,
) -> Result<T> {
    send(client, credentials, http_client::Method::GET, url, None).await
}

async fn send<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    method: http_client::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let response = send_raw(
        client,
        credentials,
        method,
        url,
        body,
        "application/vnd.github+json",
    )
    .await?;
    response.parse()
}

struct RawResponse {
    text: String,
    endpoint: String,
    /// Lowercased names.
    headers: HashMap<String, String>,
}

impl RawResponse {
    fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        // Merges, reruns and deletes answer with no body.
        let text = if self.text.trim().is_empty() {
            "null"
        } else {
            &self.text
        };
        serde_json::from_str(text)
            .with_context(|| format!("resposta inesperada do GitHub em {}", self.endpoint))
    }
}

/// Sends a request and turns error statuses into [`ApiError`]s.
async fn send_raw(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    method: http_client::Method,
    url: &str,
    body: Option<serde_json::Value>,
    accept: &str,
) -> Result<RawResponse> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("Authorization", format!("Bearer {}", credentials.token))
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "Asylum");
    let body = match body {
        Some(body) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(serde_json::to_string(&body)?)
        }
        None => AsyncBody::default(),
    };
    let request = builder.body(body)?;

    let endpoint = url
        .strip_prefix(API_URL)
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url)
        .to_string();
    let mut response = client
        .send(request)
        .await
        .with_context(|| format!("falha ao chamar o GitHub em {endpoint}"))?;

    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    let status = response.status().as_u16();
    let headers: HashMap<String, String> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((
                name.as_str().to_lowercase(),
                value.to_str().ok()?.to_string(),
            ))
        })
        .collect();
    let rate_limited = headers
        .get("x-ratelimit-remaining")
        .is_some_and(|remaining| remaining == "0");
    match status {
        401 => return Err(ApiError::Unauthorized { host: HOST }.into()),
        403 | 429 if rate_limited || status == 429 => {
            return Err(ApiError::RateLimited { host: HOST }.into());
        }
        403 => {
            return Err(ApiError::Forbidden {
                host: HOST,
                body: error_message(&text),
            }
            .into());
        }
        _ if !(200..300).contains(&status) => {
            return Err(ApiError::Status {
                host: HOST,
                status,
                body: error_message(&text),
            }
            .into());
        }
        _ => {}
    }
    Ok(RawResponse {
        text,
        endpoint,
        headers,
    })
}

/// GitHub wraps errors as `{"message": ...}`; anything else is shown as sent.
fn error_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: String,
    }
    serde_json::from_str::<ErrorBody>(body)
        .map(|error| error.message)
        .unwrap_or_else(|_| body.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remotes() {
        let repository = parse_remote("git@github.com:viniengelage/asylum.git");
        assert_eq!(
            repository,
            Some(RemoteRepository {
                hosting: Hosting::GitHub,
                owner: "viniengelage".to_string(),
                name: "asylum".to_string(),
            })
        );
        assert_eq!(parse_remote("git@bitbucket.org:trix/app.git"), None);
    }

    #[test]
    fn follows_the_next_link() {
        let link = "<https://api.github.com/repos/a/b/pulls?page=2>; rel=\"next\", <https://api.github.com/repos/a/b/pulls?page=5>; rel=\"last\"";
        assert_eq!(
            next_page(link).as_deref(),
            Some("https://api.github.com/repos/a/b/pulls?page=2")
        );
        assert_eq!(next_page("<https://x>; rel=\"prev\""), None);
    }

    #[test]
    fn keeps_each_reviewers_latest_verdict() {
        let pull_request: GitHubPullRequest = serde_json::from_value(serde_json::json!({
            "number": 212,
            "title": "Sincronizar",
            "state": "open",
            "user": { "login": "vini" },
            "head": { "ref": "feat/x", "sha": "abc", "repo": { "full_name": "vini/asylum" } },
            "base": { "ref": "main", "sha": "def" },
            "html_url": "https://github.com/vini/asylum/pull/212",
            "requested_reviewers": [{ "login": "ana" }],
        }))
        .unwrap();
        let review = |login: &str, state: &str| Review {
            user: User {
                login: login.to_string(),
                name: None,
            },
            state: state.to_string(),
            submitted_at: None,
        };
        let pull_request = pull_request.into_pull_request(vec![
            review("rafa", "CHANGES_REQUESTED"),
            review("rafa", "COMMENTED"),
            review("rafa", "APPROVED"),
        ]);
        assert_eq!(pull_request.reviewers().count(), 2);
        assert_eq!(pull_request.approval_count(), 1);
        assert!(pull_request.awaits_review_from("ana"));
        assert!(pull_request.has_approval_from("rafa"));
    }
}
