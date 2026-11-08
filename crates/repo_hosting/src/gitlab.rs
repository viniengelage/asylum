//! GitLab's REST API v4, on gitlab.com or a self-hosted instance, authenticated with a personal
//! access token pasted in the panel or with the token the `glab` CLI holds, read from `glab` on
//! every request. Merge requests are shown as pull requests, numbered by their `iid`.

use crate::api::{
    self, ApiError, ChangedFile, Check, CheckState, Comment, Commit, FileChange, Hosting, Issue,
    MergeOutcome, MergeStrategy, NewPullRequest, Participant, ParticipantRole, Person, Pipeline,
    PipelineState, PullRequest, PullRequestDetail, PullRequestState, RemoteRepository,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request};
use i18n::t;
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashMap, sync::Arc};

const HOST: &str = "GitLab";
pub const PUBLIC_INSTANCE: &str = "gitlab.com";

/// The scopes the panel needs from a personal access token.
pub const TOKEN_SCOPES: [&str; 2] = ["api", "read_user"];

/// Enough for any team's open merge requests while keeping a runaway `next` chain bounded.
const MAX_PAGES: usize = 5;

/// Where `instance` lets you create personal access tokens, with the scopes already checked.
pub fn token_settings_url(instance: &str) -> String {
    format!(
        "https://{instance}/-/user_settings/personal_access_tokens?scopes={}",
        TOKEN_SCOPES.join(",")
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenSource {
    Token(String),
    /// `glab` refreshes its OAuth token by itself, so a copy would stop working.
    GlabCli,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// The host name of the GitLab instance the token belongs to.
    pub instance: String,
    pub source: TokenSource,
}

impl Credentials {
    pub fn from_token(instance: String, token: String) -> Self {
        Self {
            instance,
            source: TokenSource::Token(token),
        }
    }

    pub fn glab_cli(instance: String) -> Self {
        Self {
            instance,
            source: TokenSource::GlabCli,
        }
    }

    pub fn is_glab_cli(&self) -> bool {
        self.source == TokenSource::GlabCli
    }

    pub async fn token(&self) -> Result<String> {
        match &self.source {
            TokenSource::Token(token) => Ok(token.clone()),
            TokenSource::GlabCli => token_from_glab_cli(&self.instance).await,
        }
    }

    fn api_url(&self) -> String {
        format!("https://{}/api/v4", self.instance)
    }
}

/// The repository an `origin` URL points at, on gitlab.com, on one of the GitLab instances in
/// the `git_hosting_providers` setting, or on a host whose name says it is a GitLab.
pub fn parse_remote(url: &str, configured_base_urls: &[String]) -> Option<RemoteRepository> {
    use git::GitHostingProvider as _;
    let public = git_hosting_providers::Gitlab::public_instance();
    if let Some(parsed) = public.parse_remote_url(url) {
        return Some(RemoteRepository {
            hosting: Hosting::GitLab,
            owner: parsed.owner.to_string(),
            name: parsed.repo.to_string(),
            instance: None,
        });
    }
    let configured = configured_base_urls.iter().filter_map(|base_url| {
        let base_url = url::Url::parse(base_url).ok()?;
        Some(git_hosting_providers::Gitlab::new(HOST, base_url))
    });
    configured
        .chain(git_hosting_providers::Gitlab::from_remote_url(url).ok())
        .find_map(|provider| {
            let parsed = provider.parse_remote_url(url)?;
            let base_url = provider.base_url();
            let host = base_url.host_str()?;
            let instance = match base_url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            };
            Some(RemoteRepository {
                hosting: Hosting::GitLab,
                owner: parsed.owner.to_string(),
                name: parsed.repo.to_string(),
                instance: Some(instance),
            })
        })
}

/// The token `glab auth login` saved for `instance`, when the GitLab CLI is installed and
/// logged in there.
pub async fn token_from_glab_cli(instance: &str) -> Result<String> {
    let output = util::command::new_command("glab")
        .args(["auth", "token", "--hostname", instance])
        .output()
        .await
        .context(t!("repo_hosting.error.glab_not_installed"))?;
    let output = if output.status.success() {
        output
    } else {
        // Older glab releases have no `auth token`; their config holds the token instead.
        let fallback = util::command::new_command("glab")
            .args(["config", "get", "token", "--host", instance])
            .output()
            .await
            .context(t!("repo_hosting.error.glab_not_installed"))?;
        anyhow::ensure!(
            fallback.status.success() && !fallback.stdout.trim_ascii().is_empty(),
            "{}",
            t!(
                "repo_hosting.error.glab_not_logged_in",
                details = String::from_utf8_lossy(&output.stderr).trim()
            )
        );
        fallback
    };
    let token = String::from_utf8(output.stdout)?.trim().to_string();
    anyhow::ensure!(
        !token.is_empty(),
        "{}",
        t!("repo_hosting.error.glab_no_token")
    );
    Ok(token)
}

#[derive(Clone, Deserialize)]
struct User {
    id: u64,
    username: String,
    #[serde(default)]
    name: Option<String>,
}

impl From<User> for Person {
    fn from(user: User) -> Self {
        Person {
            // Reviewers and approvers are set by numeric id, so that is the id.
            id: user.id.to_string(),
            display_name: user
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| user.username.clone()),
            nickname: Some(user.username),
        }
    }
}

/// The account behind the token.
pub async fn get_user(client: &Arc<dyn HttpClient>, credentials: &Credentials) -> Result<Person> {
    let user: User = get(
        client,
        credentials,
        &format!("{}/user", credentials.api_url()),
    )
    .await?;
    Ok(user.into())
}

fn project_path(credentials: &Credentials, repository: &RemoteRepository) -> String {
    format!(
        "{}/projects/{}",
        credentials.api_url(),
        urlencoding::encode(&repository.full_name())
    )
}

fn merge_request_path(
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> String {
    format!(
        "{}/merge_requests/{number}",
        project_path(credentials, repository)
    )
}

#[derive(Deserialize)]
struct PipelineReference {
    id: u64,
}

#[derive(Deserialize)]
struct DiffRefs {
    base_sha: String,
    head_sha: String,
    start_sha: String,
}

#[derive(Deserialize)]
struct MergeRequest {
    iid: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    work_in_progress: bool,
    author: User,
    source_branch: String,
    target_branch: String,
    #[serde(default)]
    source_project_id: Option<u64>,
    #[serde(default)]
    target_project_id: Option<u64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    user_notes_count: u64,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    web_url: String,
    #[serde(default)]
    reviewers: Vec<User>,
    #[serde(default)]
    has_conflicts: bool,
    #[serde(default)]
    merge_when_pipeline_succeeds: bool,
    #[serde(default)]
    head_pipeline: Option<PipelineReference>,
    #[serde(default)]
    diff_refs: Option<DiffRefs>,
}

impl MergeRequest {
    fn into_pull_request(self, approvers: Vec<User>) -> PullRequest {
        let mut participants: Vec<Participant> = self
            .reviewers
            .into_iter()
            .map(|user| Participant {
                person: user.into(),
                role: ParticipantRole::Reviewer,
                approved: false,
                requested_changes: false,
                participated_at: None,
            })
            .collect();
        for approver in approvers {
            let person: Person = approver.into();
            if let Some(existing) = participants
                .iter_mut()
                .find(|participant| participant.person.id == person.id)
            {
                existing.approved = true;
            } else {
                participants.push(Participant {
                    person,
                    role: ParticipantRole::Participant,
                    approved: true,
                    requested_changes: false,
                    participated_at: None,
                });
            }
        }
        // The API doesn't name the source project, only its id; any value different from this
        // repository's name is enough to tell it's a fork.
        let source_repository = match (self.source_project_id, self.target_project_id) {
            (Some(source), Some(target)) if source != target => Some(source.to_string()),
            _ => None,
        };
        PullRequest {
            number: self.iid,
            title: self.title,
            state: match self.state.as_str() {
                "merged" => PullRequestState::Merged,
                "closed" | "locked" => PullRequestState::Declined,
                _ => PullRequestState::Open,
            },
            draft: self.draft || self.work_in_progress,
            author: self.author.into(),
            source_branch: self.source_branch,
            source_repository,
            destination_branch: self.target_branch,
            description: self.description.unwrap_or_default(),
            comment_count: self.user_notes_count,
            open_task_count: 0,
            updated_at: self.updated_at,
            url: self.web_url,
            participants,
        }
    }
}

/// The open merge requests, most recently updated first, with who was asked to review.
pub async fn list_open_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<PullRequest>> {
    let url = format!(
        "{}/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=100",
        project_path(credentials, repository)
    );
    let merge_requests: Vec<MergeRequest> = get_all_pages(client, credentials, url).await?;
    Ok(merge_requests
        .into_iter()
        .map(|merge_request| merge_request.into_pull_request(Vec::new()))
        .collect())
}

/// The most recently updated merged or closed merge requests. One page.
pub async fn list_closed_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    state: PullRequestState,
) -> Result<Vec<PullRequest>> {
    let state_name = match state {
        PullRequestState::Merged => "merged",
        _ => "closed",
    };
    let url = format!(
        "{}/merge_requests?state={state_name}&order_by=updated_at&sort=desc&per_page=50",
        project_path(credentials, repository)
    );
    let merge_requests: Vec<MergeRequest> = get(client, credentials, &url).await?;
    Ok(merge_requests
        .into_iter()
        .map(|merge_request| merge_request.into_pull_request(Vec::new()))
        .filter(|pull_request| pull_request.state == state)
        .collect())
}

fn pipeline_state(status: &str) -> PipelineState {
    match status {
        "success" => PipelineState::Passed,
        "failed" => PipelineState::Failed,
        "running" => PipelineState::Running,
        "canceled" | "canceling" | "skipped" => PipelineState::Stopped,
        "manual" | "scheduled" => PipelineState::Paused,
        _ => PipelineState::Pending,
    }
}

/// The CI/CD pipelines, newest first.
pub async fn list_pipelines(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<Pipeline>> {
    #[derive(Deserialize)]
    struct GitLabPipeline {
        id: u64,
        #[serde(default)]
        iid: Option<u64>,
        status: String,
        #[serde(default, rename = "ref")]
        ref_name: Option<String>,
        #[serde(default)]
        sha: Option<String>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        created_at: Option<DateTime<Utc>>,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
        web_url: String,
    }
    let url = format!(
        "{}/pipelines?per_page=30",
        project_path(credentials, repository)
    );
    let pipelines: Vec<GitLabPipeline> = get(client, credentials, &url).await?;
    Ok(pipelines
        .into_iter()
        .map(|pipeline| {
            let state = pipeline_state(&pipeline.status);
            let finished = !matches!(state, PipelineState::Running | PipelineState::Pending);
            let duration_seconds = match (pipeline.created_at, pipeline.updated_at) {
                (Some(created), Some(updated)) if finished => {
                    u64::try_from((updated - created).num_seconds()).ok()
                }
                _ => None,
            };
            // Merge request pipelines run on `refs/merge-requests/<iid>/head`.
            let pull_request = pipeline.ref_name.as_deref().and_then(|ref_name| {
                ref_name
                    .strip_prefix("refs/merge-requests/")?
                    .split('/')
                    .next()?
                    .parse()
                    .ok()
            });
            Pipeline {
                number: pipeline.iid.unwrap_or(pipeline.id),
                state,
                ref_name: pipeline.ref_name.filter(|_| pull_request.is_none()),
                pull_request,
                trigger: pipeline.source.unwrap_or_default(),
                creator: None,
                commit: pipeline.sha,
                created_at: pipeline.created_at,
                duration_seconds,
                url: pipeline.web_url,
            }
        })
        .collect())
}

/// The open issues. `None` when the project turned issues off.
pub async fn list_open_issues(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<Vec<Issue>>> {
    #[derive(Deserialize)]
    struct GitLabIssue {
        iid: u64,
        title: String,
        state: String,
        #[serde(default)]
        issue_type: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        assignees: Vec<User>,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
        web_url: String,
    }
    let details = get_project(client, credentials, repository).await?;
    if !details.issues_enabled {
        return Ok(None);
    }
    let issues: Vec<GitLabIssue> = get(
        client,
        credentials,
        &format!(
            "{}/issues?state=opened&order_by=updated_at&sort=desc&per_page=50",
            project_path(credentials, repository)
        ),
    )
    .await?;
    Ok(Some(
        issues
            .into_iter()
            .map(|issue| {
                let priority = issue
                    .labels
                    .iter()
                    .find(|label| label.to_lowercase().starts_with("priority"))
                    .cloned()
                    .unwrap_or_default();
                Issue {
                    number: issue.iid,
                    title: issue.title,
                    state: issue.state,
                    kind: issue.issue_type.unwrap_or_default(),
                    priority,
                    assignee: issue
                        .assignees
                        .into_iter()
                        .next()
                        .map(|user| Person::from(user).short_name().to_string()),
                    updated_at: issue.updated_at,
                    url: issue.web_url,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct FileDiff {
    old_path: String,
    new_path: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    renamed_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    diff: String,
}

async fn get_diffs(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<Vec<FileDiff>> {
    get_all_pages(
        client,
        credentials,
        format!(
            "{}/diffs?per_page=100",
            merge_request_path(credentials, repository, number)
        ),
    )
    .await
}

/// The merge request's diff as a unified patch. GitLab hands back each file's hunks alone, so
/// the file headers are put back.
pub async fn get_pull_request_diff(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<String> {
    let diffs = get_diffs(client, credentials, repository, number).await?;
    let mut patch = String::new();
    for file in diffs {
        let old = if file.new_file {
            "/dev/null".to_string()
        } else {
            format!("a/{}", file.old_path)
        };
        let new = if file.deleted_file {
            "/dev/null".to_string()
        } else {
            format!("b/{}", file.new_path)
        };
        patch.push_str(&format!(
            "diff --git a/{} b/{}\n--- {old}\n+++ {new}\n",
            file.old_path, file.new_path
        ));
        patch.push_str(&file.diff);
        if !file.diff.ends_with('\n') {
            patch.push('\n');
        }
    }
    Ok(patch)
}

async fn get_merge_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<MergeRequest> {
    get(
        client,
        credentials,
        &merge_request_path(credentials, repository, number),
    )
    .await
}

/// The jobs of the merge request's head pipeline.
pub async fn get_checks(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<Vec<Check>> {
    let merge_request = get_merge_request(client, credentials, repository, number).await?;
    match merge_request.head_pipeline {
        Some(pipeline) => get_pipeline_jobs(client, credentials, repository, pipeline.id).await,
        None => Ok(Vec::new()),
    }
}

async fn get_pipeline_jobs(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    pipeline_id: u64,
) -> Result<Vec<Check>> {
    #[derive(Deserialize)]
    struct Job {
        id: u64,
        name: String,
        status: String,
        #[serde(default)]
        web_url: Option<String>,
        #[serde(default)]
        started_at: Option<DateTime<Utc>>,
        #[serde(default)]
        finished_at: Option<DateTime<Utc>>,
    }
    let jobs: Vec<Job> = get_all_pages(
        client,
        credentials,
        format!(
            "{}/pipelines/{pipeline_id}/jobs?per_page=100",
            project_path(credentials, repository)
        ),
    )
    .await?;
    Ok(jobs
        .into_iter()
        .map(|job| Check {
            state: match job.status.as_str() {
                "success" => CheckState::Passed,
                "failed" => CheckState::Failed,
                "canceled" | "canceling" | "skipped" | "manual" => CheckState::Stopped,
                _ => CheckState::Running,
            },
            name: job.name,
            url: job.web_url,
            updated_at: job.finished_at.or(job.started_at),
            rerun_job: Some(job.id),
        })
        .collect())
}

#[derive(Deserialize)]
struct Position {
    #[serde(default)]
    new_path: Option<String>,
    #[serde(default)]
    old_path: Option<String>,
    #[serde(default)]
    new_line: Option<u64>,
    #[serde(default)]
    old_line: Option<u64>,
}

#[derive(Deserialize)]
struct Note {
    id: u64,
    #[serde(default)]
    body: String,
    author: User,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    position: Option<Position>,
}

#[derive(Deserialize)]
struct Discussion {
    id: String,
    #[serde(default)]
    notes: Vec<Note>,
}

async fn get_discussions(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<Vec<Discussion>> {
    get_all_pages(
        client,
        credentials,
        format!(
            "{}/discussions?per_page=100",
            merge_request_path(credentials, repository, number)
        ),
    )
    .await
}

pub async fn get_pull_request_detail(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<PullRequestDetail> {
    #[derive(Deserialize)]
    struct Approver {
        user: User,
    }
    #[derive(Deserialize)]
    struct Approvals {
        #[serde(default)]
        approved_by: Vec<Approver>,
    }
    #[derive(Deserialize)]
    struct GitLabCommit {
        id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        author_name: String,
        #[serde(default)]
        authored_date: Option<DateTime<Utc>>,
    }

    let base = merge_request_path(credentials, repository, number);
    let merge_request = get_merge_request(client, credentials, repository, number);
    let approvals_url = format!("{base}/approvals");
    let approvals = get::<Approvals>(client, credentials, &approvals_url);
    let diffs = get_diffs(client, credentials, repository, number);
    let discussions = get_discussions(client, credentials, repository, number);
    let commits =
        get_all_pages::<GitLabCommit>(client, credentials, format!("{base}/commits?per_page=100"));
    let (merge_request, approvals, diffs, discussions, commits) =
        futures::join!(merge_request, approvals, diffs, discussions, commits);
    let merge_request = merge_request?;
    let checks = match &merge_request.head_pipeline {
        Some(pipeline) => get_pipeline_jobs(client, credentials, repository, pipeline.id).await?,
        None => Vec::new(),
    };

    let mut comments = Vec::new();
    for discussion in discussions? {
        let notes = discussion.notes.into_iter().filter(|note| !note.system);
        for (index, note) in notes.enumerate() {
            let inline = note.position.map(|position| {
                (
                    position.new_path.or(position.old_path).unwrap_or_default(),
                    position.new_line.or(position.old_line),
                )
            });
            comments.push(Comment {
                id: note.id,
                author: note.author.into(),
                body: note.body,
                created_at: note.created_at,
                inline,
                is_reply: index > 0,
            });
        }
    }
    comments.sort_by_key(|comment| comment.created_at);

    let has_conflicts = merge_request.has_conflicts;
    let auto_merge = merge_request.merge_when_pipeline_succeeds;
    let approvers = approvals?
        .approved_by
        .into_iter()
        .map(|approver| approver.user)
        .collect();

    Ok(PullRequestDetail {
        pull_request: merge_request.into_pull_request(approvers),
        checks,
        files: diffs?
            .into_iter()
            .map(|file| {
                let change = if file.new_file {
                    FileChange::Added
                } else if file.deleted_file {
                    FileChange::Removed
                } else if file.renamed_file {
                    FileChange::Renamed
                } else {
                    FileChange::Modified
                };
                let (lines_added, lines_removed) = count_diff_lines(&file.diff);
                ChangedFile {
                    old_path: file.renamed_file.then_some(file.old_path),
                    path: file.new_path,
                    change,
                    lines_added,
                    lines_removed,
                }
            })
            .collect(),
        comments,
        commits: commits?
            .into_iter()
            .map(|commit| Commit {
                hash: commit.id,
                summary: commit.title.trim().to_string(),
                author: commit.author_name,
                date: commit.authored_date,
            })
            .collect(),
        has_conflicts,
        auto_merge,
    })
}

fn count_diff_lines(diff: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

pub async fn post_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    text: &str,
) -> Result<()> {
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!(
            "{}/notes",
            merge_request_path(credentials, repository, number)
        ),
        Some(serde_json::json!({ "body": text })),
    )
    .await?;
    Ok(())
}

/// A comment on a line of the merge request's new version of `path`.
pub async fn post_inline_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    path: &str,
    line: u64,
    text: &str,
) -> Result<()> {
    let merge_request = get_merge_request(client, credentials, repository, number).await?;
    let diff_refs = merge_request.diff_refs.context(t!(
        "repo_hosting.error.unexpected_response",
        host = HOST,
        endpoint = "diff_refs"
    ))?;
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!(
            "{}/discussions",
            merge_request_path(credentials, repository, number)
        ),
        Some(serde_json::json!({
            "body": text,
            "position": {
                "position_type": "text",
                "base_sha": diff_refs.base_sha,
                "head_sha": diff_refs.head_sha,
                "start_sha": diff_refs.start_sha,
                "old_path": path,
                "new_path": path,
                "new_line": line,
            },
        })),
    )
    .await?;
    Ok(())
}

/// Comments are listed by note id while threads are resolved by discussion id, so the
/// discussion is found by its first note.
pub async fn resolve_discussion(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    root_comment_id: u64,
) -> Result<()> {
    let discussions = get_discussions(client, credentials, repository, number).await?;
    let discussion = discussions
        .into_iter()
        .find(|discussion| discussion.notes.first().map(|note| note.id) == Some(root_comment_id))
        .with_context(|| t!("repo_hosting.changes.thread_not_found"))?;
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::PUT,
        &format!(
            "{}/discussions/{}?resolved=true",
            merge_request_path(credentials, repository, number),
            urlencoding::encode(&discussion.id)
        ),
        None,
    )
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct Project {
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    issues_enabled: bool,
    #[serde(default)]
    merge_method: Option<String>,
    #[serde(default)]
    squash_option: Option<String>,
}

async fn get_project(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Project> {
    get(client, credentials, &project_path(credentials, repository)).await
}

pub async fn get_default_destination(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<String>> {
    Ok(get_project(client, credentials, repository)
        .await?
        .default_branch)
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
    let url = format!(
        "{}/repository/branches?per_page=100",
        project_path(credentials, repository)
    );
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
    struct GitLabCommit {
        id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        author_name: String,
        #[serde(default)]
        authored_date: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct Comparison {
        #[serde(default)]
        commits: Vec<GitLabCommit>,
    }
    let url = format!(
        "{}/repository/compare?from={}&to={}",
        project_path(credentials, repository),
        urlencoding::encode(destination),
        urlencoding::encode(source)
    );
    let comparison: Comparison = get(client, credentials, &url).await?;
    let mut commits: Vec<Commit> = comparison
        .commits
        .into_iter()
        .map(|commit| Commit {
            hash: commit.id,
            summary: commit.title.trim().to_string(),
            author: commit.author_name,
            date: commit.authored_date,
        })
        .collect();
    commits.sort_by_key(|commit| std::cmp::Reverse(commit.date));
    Ok(commits)
}

pub async fn create_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    new: NewPullRequest,
) -> Result<PullRequest> {
    let reviewer_ids: Vec<u64> = new
        .reviewer_ids
        .iter()
        .filter_map(|id| id.parse().ok())
        .collect();
    // GitLab marks a merge request as a draft by its title.
    let title = if new.draft {
        format!("Draft: {}", new.title)
    } else {
        new.title
    };
    let created: MergeRequest = send(
        client,
        credentials,
        http_client::Method::POST,
        &format!("{}/merge_requests", project_path(credentials, repository)),
        Some(serde_json::json!({
            "title": title,
            "description": new.description,
            "source_branch": new.source_branch,
            "target_branch": new.destination_branch,
            "reviewer_ids": reviewer_ids,
            "remove_source_branch": new.close_source_branch,
        })),
    )
    .await?;
    Ok(created.into_pull_request(Vec::new()))
}

/// The merge methods the project allows, by its merge method and squash setting.
pub async fn get_merge_strategies(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<MergeStrategy>> {
    let project = get_project(client, credentials, repository).await?;
    let (plain, squashed) = match project.merge_method.as_deref() {
        Some("ff") => (MergeStrategy::FastForward, MergeStrategy::SquashFastForward),
        _ => (MergeStrategy::MergeCommit, MergeStrategy::Squash),
    };
    Ok(match project.squash_option.as_deref() {
        Some("never") => vec![plain],
        Some("always") => vec![squashed],
        Some("default_on") => vec![squashed, plain],
        _ => vec![plain, squashed],
    })
}

fn merge_body(
    strategy: MergeStrategy,
    message: &str,
    delete_source_branch: bool,
) -> serde_json::Value {
    let squash = matches!(
        strategy,
        MergeStrategy::Squash | MergeStrategy::SquashFastForward
    );
    let mut body = serde_json::json!({
        "squash": squash,
        "should_remove_source_branch": delete_source_branch,
    });
    let message = message.trim();
    if !message.is_empty() {
        let key = if squash {
            "squash_commit_message"
        } else {
            "merge_commit_message"
        };
        body[key] = serde_json::Value::String(message.to_string());
    }
    body
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
    let merged: MergeRequest = send(
        client,
        credentials,
        http_client::Method::PUT,
        &format!(
            "{}/merge",
            merge_request_path(credentials, repository, number)
        ),
        Some(merge_body(strategy, message, delete_source_branch)),
    )
    .await?;
    Ok(MergeOutcome::Merged(merged.into_pull_request(Vec::new())))
}

/// Asks GitLab to merge once the pipeline passes.
pub async fn enable_auto_merge(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    strategy: MergeStrategy,
) -> Result<()> {
    let mut body = merge_body(strategy, "", false);
    // Older instances only know `merge_when_pipeline_succeeds`; newer ones renamed it.
    body["merge_when_pipeline_succeeds"] = serde_json::Value::Bool(true);
    body["auto_merge"] = serde_json::Value::Bool(true);
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::PUT,
        &format!(
            "{}/merge",
            merge_request_path(credentials, repository, number)
        ),
        Some(body),
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
        http_client::Method::PUT,
        &merge_request_path(credentials, repository, number),
        Some(serde_json::json!({ "state_event": "close" })),
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
    let action = if approve { "approve" } else { "unapprove" };
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &format!(
            "{}/{action}",
            merge_request_path(credentials, repository, number)
        ),
        None,
    )
    .await?;
    Ok(())
}

/// Runs a CI job again, with the same commit and variables.
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
            "{}/jobs/{job_id}/retry",
            project_path(credentials, repository)
        ),
        None,
    )
    .await?;
    Ok(())
}

/// A CI job's log.
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
        &format!(
            "{}/jobs/{job_id}/trace",
            project_path(credentials, repository)
        ),
        None,
    )
    .await?;
    Ok(response.text)
}

async fn get_all_pages<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    first_url: String,
) -> Result<Vec<T>> {
    let api_url = credentials.api_url();
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
        )
        .await?;
        let page: Vec<T> = response.parse()?;
        values.extend(page);
        // The token only ever goes to the instance's API.
        url = response
            .headers
            .get("link")
            .and_then(|link| api::next_page(link))
            .filter(|next| next.starts_with(&api_url));
    }
    Ok(values)
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
    send_raw(client, credentials, method, url, body)
        .await?
        .parse()
}

/// Sends a request and turns error statuses into [`ApiError`]s.
async fn send_raw(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    method: http_client::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<api::RawResponse> {
    let token = credentials.token().await?;
    // `Bearer` takes both personal access tokens and the OAuth tokens `glab` holds.
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "Asylum");
    let body = match body {
        Some(body) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(serde_json::to_string(&body)?)
        }
        None => AsyncBody::default(),
    };
    let request = builder.body(body)?;

    let api_url = credentials.api_url();
    let endpoint = url
        .strip_prefix(api_url.as_str())
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or(url)
        .to_string();
    let mut response = client.send(request).await.with_context(|| {
        t!(
            "repo_hosting.error.request_failed",
            host = HOST,
            endpoint = endpoint
        )
    })?;

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
        .get("ratelimit-remaining")
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
    Ok(api::RawResponse {
        text,
        endpoint,
        headers,
        host: HOST,
    })
}

/// GitLab answers errors as `{"message": ...}`, where the message can also be a list or a map
/// of field errors, or as OAuth's `{"error": ...}`.
fn error_message(body: &str) -> String {
    let fallback = || body.chars().take(200).collect();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return fallback();
    };
    match value.get("message").or_else(|| value.get("error")) {
        Some(serde_json::Value::String(message)) => message.clone(),
        Some(serde_json::Value::Null) | None => fallback(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gitlab_remotes() {
        assert_eq!(
            parse_remote("git@gitlab.com:trix/app.git", &[]),
            Some(RemoteRepository {
                hosting: Hosting::GitLab,
                owner: "trix".to_string(),
                name: "app".to_string(),
                instance: None,
            })
        );
        assert_eq!(
            parse_remote("https://gitlab.com/trix/mobile/app.git", &[]),
            Some(RemoteRepository {
                hosting: Hosting::GitLab,
                owner: "trix/mobile".to_string(),
                name: "app".to_string(),
                instance: None,
            })
        );
        assert_eq!(
            parse_remote(
                "git@code.empresa.com:time/app.git",
                &["https://code.empresa.com".to_string()]
            ),
            Some(RemoteRepository {
                hosting: Hosting::GitLab,
                owner: "time".to_string(),
                name: "app".to_string(),
                instance: Some("code.empresa.com".to_string()),
            })
        );
        assert_eq!(
            parse_remote("https://gitlab.empresa.com/time/app.git", &[])
                .and_then(|repository| repository.instance),
            Some("gitlab.empresa.com".to_string())
        );
        assert_eq!(
            parse_remote("git@github.com:viniengelage/asylum.git", &[]),
            None
        );
    }

    #[test]
    fn marks_approvers_on_the_merge_request() {
        let merge_request: MergeRequest = serde_json::from_value(serde_json::json!({
            "iid": 42,
            "title": "Sincronizar",
            "state": "opened",
            "author": { "id": 1, "username": "vini" },
            "source_branch": "feat/x",
            "target_branch": "main",
            "source_project_id": 7,
            "target_project_id": 7,
            "web_url": "https://gitlab.com/trix/app/-/merge_requests/42",
            "reviewers": [{ "id": 2, "username": "ana" }, { "id": 3, "username": "rafa" }],
        }))
        .unwrap();
        let pull_request = merge_request.into_pull_request(vec![
            User {
                id: 3,
                username: "rafa".to_string(),
                name: None,
            },
            User {
                id: 4,
                username: "leo".to_string(),
                name: None,
            },
        ]);
        assert_eq!(pull_request.number, 42);
        assert_eq!(pull_request.reviewers().count(), 2);
        assert_eq!(pull_request.approval_count(), 2);
        assert!(pull_request.awaits_review_from("2"));
        assert!(pull_request.has_approval_from("3"));
        assert!(pull_request.source_repository.is_none());
    }

    #[test]
    fn reads_nested_error_messages() {
        assert_eq!(
            error_message(r#"{"message":"404 Not found"}"#),
            "404 Not found"
        );
        assert_eq!(
            error_message(r#"{"message":{"title":["is too long"]}}"#),
            r#"{"title":["is too long"]}"#
        );
        assert_eq!(
            error_message(r#"{"error":"invalid_token"}"#),
            "invalid_token"
        );
    }
}
