//! Bitbucket Cloud's REST API 2.0, authenticated with an Atlassian API token and the e-mail of
//! its account (HTTP Basic). App passwords stopped working in 2026, and an API token needs no
//! OAuth consumer to be registered.

use crate::api::{
    ApiError, ChangedFile, Check, CheckState, Comment, Commit, FileChange, Hosting, Issue,
    Participant, ParticipantRole, Person, Pipeline, PipelineState, PullRequest, PullRequestDetail,
    PullRequestState, RemoteRepository,
};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, Request};
use serde::{Deserialize, de::DeserializeOwned};
use std::sync::Arc;
use util::ResultExt as _;

const API_URL: &str = "https://api.bitbucket.org/2.0";
const HOST: &str = "O Bitbucket";

/// Where Atlassian lets you create, list and revoke API tokens.
pub const TOKEN_SETTINGS_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";

/// The scopes the panel uses, as Atlassian names them when creating a scoped token. The user
/// scope is the one people miss: without it the token can't say whose it is.
pub const TOKEN_SCOPES: [&str; 5] = [
    "read:user:bitbucket",
    "read:repository:bitbucket",
    "read:pullrequest:bitbucket",
    "write:pullrequest:bitbucket",
    "read:pipeline:bitbucket",
];

/// Enough for any team's open pull requests while keeping a runaway `next` chain bounded.
const MAX_PAGES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// The Atlassian account's e-mail: API tokens authenticate with it, not the username.
    pub email: String,
    pub token: String,
}

impl Credentials {
    fn authorization(&self) -> String {
        let pair = format!("{}:{}", self.email, self.token);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }
}

/// The repository an `origin` URL points at, when it is on bitbucket.org. Bitbucket Server and
/// Data Center speak another API.
pub fn parse_remote(url: &str) -> Option<RemoteRepository> {
    use git::GitHostingProvider as _;
    let parsed = git_hosting_providers::Bitbucket::public_instance().parse_remote_url(url)?;
    Some(RemoteRepository {
        hosting: Hosting::Bitbucket,
        owner: parsed.owner.to_string(),
        name: parsed.repo.to_string(),
    })
}

#[derive(Deserialize)]
struct User {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
}

impl From<User> for Person {
    fn from(user: User) -> Self {
        let nickname = user.nickname.filter(|nickname| !nickname.is_empty());
        Person {
            id: user.uuid.or(user.account_id).unwrap_or_default(),
            display_name: user
                .display_name
                .or_else(|| nickname.clone())
                .unwrap_or_else(|| "Usuário removido".to_string()),
            nickname,
        }
    }
}

#[derive(Deserialize)]
struct Page<T> {
    values: Vec<T>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct Branch {
    name: String,
}

#[derive(Deserialize)]
struct RepositoryReference {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct Endpoint {
    branch: Branch,
    #[serde(default)]
    repository: Option<RepositoryReference>,
}

#[derive(Deserialize)]
struct Link {
    href: String,
}

#[derive(Deserialize, Default)]
struct Links {
    html: Option<Link>,
}

#[derive(Deserialize)]
struct BitbucketParticipant {
    user: User,
    role: String,
    #[serde(default)]
    approved: bool,
    /// `approved`, `changes_requested` or null.
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    participated_on: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct BitbucketPullRequest {
    id: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    author: User,
    source: Endpoint,
    destination: Endpoint,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    comment_count: u64,
    #[serde(default)]
    task_count: u64,
    #[serde(default)]
    updated_on: Option<DateTime<Utc>>,
    #[serde(default)]
    links: Links,
    #[serde(default)]
    participants: Vec<BitbucketParticipant>,
}

impl BitbucketPullRequest {
    fn into_pull_request(self, repository: &RemoteRepository) -> PullRequest {
        let url = self
            .links
            .html
            .map(|link| link.href)
            .unwrap_or_else(|| format!("{}/pull-requests/{}", repository.web_url(), self.id));
        PullRequest {
            number: self.id,
            title: self.title,
            state: match self.state.as_str() {
                "MERGED" => PullRequestState::Merged,
                "DECLINED" => PullRequestState::Declined,
                "SUPERSEDED" => PullRequestState::Superseded,
                _ => PullRequestState::Open,
            },
            draft: self.draft,
            author: self.author.into(),
            source_branch: self.source.branch.name,
            source_repository: self
                .source
                .repository
                .and_then(|repository| repository.full_name),
            destination_branch: self.destination.branch.name,
            description: self.description.unwrap_or_default(),
            comment_count: self.comment_count,
            open_task_count: self.task_count,
            updated_at: self.updated_on,
            url,
            participants: self
                .participants
                .into_iter()
                .map(|participant| Participant {
                    person: participant.user.into(),
                    role: if participant.role == "REVIEWER" {
                        ParticipantRole::Reviewer
                    } else {
                        ParticipantRole::Participant
                    },
                    approved: participant.approved,
                    requested_changes: participant.state.as_deref() == Some("changes_requested"),
                    participated_at: participant.participated_on,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct Status {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    key: Option<String>,
    state: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    updated_on: Option<DateTime<Utc>>,
}

impl From<Status> for Check {
    fn from(status: Status) -> Self {
        Check {
            name: status
                .name
                .filter(|name| !name.trim().is_empty())
                .or(status.key)
                .unwrap_or_else(|| "Build".to_string()),
            state: match status.state.as_str() {
                "SUCCESSFUL" => CheckState::Passed,
                "FAILED" => CheckState::Failed,
                "INPROGRESS" => CheckState::Running,
                _ => CheckState::Stopped,
            },
            url: status.url.filter(|url| !url.is_empty()),
            updated_at: status.updated_on,
        }
    }
}

#[derive(Deserialize)]
struct DiffPath {
    path: String,
}

#[derive(Deserialize)]
struct DiffStat {
    status: String,
    #[serde(default)]
    lines_added: u64,
    #[serde(default)]
    lines_removed: u64,
    #[serde(default)]
    old: Option<DiffPath>,
    #[serde(default)]
    new: Option<DiffPath>,
}

impl DiffStat {
    fn into_changed_file(self) -> Option<ChangedFile> {
        let old_path = self.old.map(|old| old.path);
        let new_path = self.new.map(|new| new.path);
        let change = match self.status.as_str() {
            "added" => FileChange::Added,
            "removed" => FileChange::Removed,
            "renamed" => FileChange::Renamed,
            "merge conflict" | "local deleted" | "remote deleted" => FileChange::Conflict,
            _ => FileChange::Modified,
        };
        let path = new_path.or_else(|| old_path.clone())?;
        Some(ChangedFile {
            path,
            old_path: (change == FileChange::Renamed)
                .then_some(old_path)
                .flatten(),
            change,
            lines_added: self.lines_added,
            lines_removed: self.lines_removed,
        })
    }
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    raw: String,
}

#[derive(Deserialize)]
struct Inline {
    path: String,
    #[serde(default)]
    to: Option<u64>,
    #[serde(default)]
    from: Option<u64>,
}

#[derive(Deserialize)]
struct CommentParent {}

#[derive(Deserialize)]
struct BitbucketComment {
    id: u64,
    user: User,
    content: Content,
    #[serde(default)]
    created_on: Option<DateTime<Utc>>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    inline: Option<Inline>,
    #[serde(default)]
    parent: Option<CommentParent>,
}

#[derive(Deserialize)]
struct CommitAuthor {
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    user: Option<User>,
}

#[derive(Deserialize)]
struct BitbucketCommit {
    hash: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    date: Option<DateTime<Utc>>,
    #[serde(default)]
    author: Option<CommitAuthor>,
}

impl From<BitbucketCommit> for Commit {
    fn from(commit: BitbucketCommit) -> Self {
        let author = commit.author.and_then(|author| {
            let raw = author.raw;
            author
                .user
                .map(|user| Person::from(user).short_name().to_string())
                // "Name <email>" as git wrote it.
                .or_else(|| raw.map(|raw| raw.split('<').next().unwrap_or(&raw).trim().to_string()))
        });
        Commit {
            summary: commit
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string(),
            hash: commit.hash,
            author: author.unwrap_or_default(),
            date: commit.date,
        }
    }
}

pub async fn get_user(client: &Arc<dyn HttpClient>, credentials: &Credentials) -> Result<Person> {
    let user: User = get(client, credentials, &format!("{API_URL}/user")).await?;
    Ok(user.into())
}

fn repository_path(repository: &RemoteRepository) -> String {
    format!(
        "{API_URL}/repositories/{}/{}",
        urlencoding::encode(&repository.owner),
        urlencoding::encode(&repository.name)
    )
}

/// The open pull requests, most recently updated first, with their reviewers.
pub async fn list_open_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<PullRequest>> {
    // The list leaves participants out unless asked; without them "pedem sua revisão" can't be
    // told apart from the rest.
    let url = format!(
        "{}/pullrequests?state=OPEN&pagelen=50&sort=-updated_on&fields=%2Bvalues.participants",
        repository_path(repository)
    );
    let pull_requests: Vec<BitbucketPullRequest> = get_all_pages(client, credentials, url).await?;
    Ok(pull_requests
        .into_iter()
        .map(|pull_request| pull_request.into_pull_request(repository))
        .collect())
}

/// The most recently updated merged or declined pull requests. One page: this is for looking
/// something up, not for keeping in sync.
pub async fn list_closed_pull_requests(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    state: PullRequestState,
) -> Result<Vec<PullRequest>> {
    let state = match state {
        PullRequestState::Merged => "MERGED",
        PullRequestState::Declined => "DECLINED",
        PullRequestState::Superseded => "SUPERSEDED",
        PullRequestState::Open => "OPEN",
    };
    let url = format!(
        "{}/pullrequests?state={state}&pagelen=50&sort=-updated_on",
        repository_path(repository)
    );
    let page: Page<BitbucketPullRequest> = get(client, credentials, &url).await?;
    Ok(page
        .values
        .into_iter()
        .map(|pull_request| pull_request.into_pull_request(repository))
        .collect())
}

pub async fn list_pipelines(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<Pipeline>> {
    #[derive(Deserialize)]
    struct Named {
        name: String,
    }
    #[derive(Deserialize)]
    struct PipelineStatus {
        name: String,
        #[serde(default)]
        result: Option<Named>,
        #[serde(default)]
        stage: Option<Named>,
    }
    #[derive(Deserialize)]
    struct CommitReference {
        hash: String,
    }
    #[derive(Deserialize)]
    struct PullRequestReference {
        id: u64,
    }
    #[derive(Deserialize)]
    struct Target {
        #[serde(default)]
        ref_name: Option<String>,
        #[serde(default)]
        commit: Option<CommitReference>,
        #[serde(default)]
        pullrequest: Option<PullRequestReference>,
    }
    #[derive(Deserialize)]
    struct BitbucketPipeline {
        build_number: u64,
        state: PipelineStatus,
        #[serde(default)]
        target: Option<Target>,
        #[serde(default)]
        trigger: Option<Named>,
        #[serde(default)]
        creator: Option<User>,
        #[serde(default)]
        created_on: Option<DateTime<Utc>>,
        #[serde(default)]
        duration_in_seconds: Option<u64>,
    }

    let url = format!(
        "{}/pipelines/?sort=-created_on&pagelen=30",
        repository_path(repository)
    );
    let page: Page<BitbucketPipeline> = get(client, credentials, &url).await?;
    Ok(page
        .values
        .into_iter()
        .map(|pipeline| {
            let result = pipeline.state.result.map(|result| result.name);
            let stage = pipeline.state.stage.map(|stage| stage.name);
            let state = match (
                pipeline.state.name.as_str(),
                result.as_deref(),
                stage.as_deref(),
            ) {
                ("IN_PROGRESS", _, Some("PAUSED")) => PipelineState::Paused,
                ("IN_PROGRESS" | "RUNNING", _, _) => PipelineState::Running,
                ("PENDING", _, _) => PipelineState::Pending,
                (_, Some("SUCCESSFUL"), _) => PipelineState::Passed,
                (_, Some("FAILED" | "ERROR"), _) => PipelineState::Failed,
                _ => PipelineState::Stopped,
            };
            let target = pipeline.target;
            Pipeline {
                url: format!(
                    "{}/pipelines/results/{}",
                    repository.web_url(),
                    pipeline.build_number
                ),
                number: pipeline.build_number,
                state,
                ref_name: target.as_ref().and_then(|target| target.ref_name.clone()),
                pull_request: target
                    .as_ref()
                    .and_then(|target| target.pullrequest.as_ref())
                    .map(|pull_request| pull_request.id),
                commit: target
                    .and_then(|target| target.commit)
                    .map(|commit| commit.hash),
                trigger: pipeline
                    .trigger
                    .map(|trigger| trigger.name)
                    .unwrap_or_default(),
                creator: pipeline
                    .creator
                    .map(|creator| Person::from(creator).short_name().to_string()),
                created_at: pipeline.created_on,
                duration_seconds: pipeline.duration_in_seconds,
            }
        })
        .collect())
}

/// The open issues, or `None` when the repository has no issue tracker (teams on Jira).
pub async fn list_open_issues(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<Vec<Issue>>> {
    #[derive(Deserialize)]
    struct RepositoryFlags {
        #[serde(default)]
        has_issues: bool,
    }
    #[derive(Deserialize)]
    struct BitbucketIssue {
        id: u64,
        title: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        priority: String,
        #[serde(default)]
        assignee: Option<User>,
        #[serde(default)]
        updated_on: Option<DateTime<Utc>>,
        #[serde(default)]
        links: Links,
    }

    let base = repository_path(repository);
    let flags: RepositoryFlags = get(client, credentials, &base).await?;
    if !flags.has_issues {
        return Ok(None);
    }
    let query = urlencoding::encode(r#"state="new" OR state="open" OR state="on hold""#);
    let url = format!("{base}/issues?q={query}&sort=-updated_on&pagelen=50");
    let page: Page<BitbucketIssue> = get(client, credentials, &url).await?;
    Ok(Some(
        page.values
            .into_iter()
            .map(|issue| Issue {
                url: issue
                    .links
                    .html
                    .map(|link| link.href)
                    .unwrap_or_else(|| format!("{}/issues/{}", repository.web_url(), issue.id)),
                number: issue.id,
                title: issue.title,
                state: issue.state,
                kind: issue.kind,
                priority: issue.priority,
                assignee: issue
                    .assignee
                    .map(|assignee| Person::from(assignee).short_name().to_string()),
                updated_at: issue.updated_on,
            })
            .collect(),
    ))
}

/// The pull request's diff as a unified patch, the way Bitbucket shows it.
pub async fn get_pull_request_diff(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<String> {
    let url = format!("{}/pullrequests/{number}/diff", repository_path(repository));
    let response = send_raw(
        client,
        credentials,
        http_client::Method::GET,
        &url,
        None,
        "text/plain",
    )
    .await?;
    Ok(response.text)
}

pub async fn get_checks(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<Vec<Check>> {
    let url = format!(
        "{}/pullrequests/{number}/statuses?pagelen=100",
        repository_path(repository)
    );
    let statuses: Vec<Status> = get_all_pages(client, credentials, url).await?;
    Ok(statuses.into_iter().map(Check::from).collect())
}

pub async fn get_pull_request_detail(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<PullRequestDetail> {
    let base = format!("{}/pullrequests/{number}", repository_path(repository));
    let pull_request = get::<BitbucketPullRequest>(client, credentials, &base);
    let checks = get_checks(client, credentials, repository, number);
    let files =
        get_all_pages::<DiffStat>(client, credentials, format!("{base}/diffstat?pagelen=100"));
    let comments = get_all_pages::<BitbucketComment>(
        client,
        credentials,
        format!("{base}/comments?pagelen=100"),
    );
    let commits =
        get_all_pages::<BitbucketCommit>(client, credentials, format!("{base}/commits?pagelen=50"));
    let (pull_request, checks, files, comments, commits) =
        futures::join!(pull_request, checks, files, comments, commits);

    let comments = comments?
        .into_iter()
        .filter(|comment| !comment.deleted)
        .map(|comment| Comment {
            id: comment.id,
            author: comment.user.into(),
            body: comment.content.raw,
            created_at: comment.created_on,
            inline: comment
                .inline
                .map(|inline| (inline.path, inline.to.or(inline.from))),
            is_reply: comment.parent.is_some(),
        })
        .collect();
    Ok(PullRequestDetail {
        pull_request: pull_request?.into_pull_request(repository),
        checks: checks?,
        files: files?
            .into_iter()
            .filter_map(DiffStat::into_changed_file)
            .collect(),
        comments,
        commits: commits?.into_iter().map(Commit::from).collect(),
    })
}

pub async fn post_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    text: &str,
) -> Result<()> {
    let url = format!(
        "{}/pullrequests/{number}/comments",
        repository_path(repository)
    );
    let body = serde_json::json!({ "content": { "raw": text } });
    send::<serde_json::Value>(
        client,
        credentials,
        http_client::Method::POST,
        &url,
        Some(body),
    )
    .await?;
    Ok(())
}

/// Where new pull requests should go by default: the branching model's development branch
/// when the repository uses one (usually `develop`), else its main branch.
pub async fn get_default_destination(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct NamedBranch {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        branch: Option<Branch>,
    }
    #[derive(Deserialize)]
    struct BranchingModel {
        #[serde(default)]
        development: Option<NamedBranch>,
    }
    #[derive(Deserialize)]
    struct Repository {
        #[serde(default)]
        mainbranch: Option<Branch>,
    }

    let base = repository_path(repository);
    let model = get::<BranchingModel>(client, credentials, &format!("{base}/branching-model"))
        .await
        .context("Bitbucket: falha ao ler o branching model")
        .log_err();
    let development = model
        .and_then(|model| model.development)
        .and_then(|development| {
            development
                .branch
                .map(|branch| branch.name)
                .or(development.name)
        });
    if development.is_some() {
        return Ok(development);
    }
    let repository: Repository = get(client, credentials, &base).await?;
    Ok(repository.mainbranch.map(|branch| branch.name))
}

/// The remote's branches, most recently committed to first.
pub async fn list_branches(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<String>> {
    let url = format!(
        "{}/refs/branches?pagelen=100&sort=-target.date&fields=values.name%2Cnext",
        repository_path(repository)
    );
    let branches: Vec<Branch> = get_all_pages(client, credentials, url).await?;
    Ok(branches.into_iter().map(|branch| branch.name).collect())
}

/// The repository's default reviewers, including those inherited from the project.
pub async fn get_default_reviewers(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
) -> Result<Vec<Person>> {
    #[derive(Deserialize)]
    struct DefaultReviewer {
        user: User,
    }
    let url = format!(
        "{}/effective-default-reviewers?pagelen=100",
        repository_path(repository)
    );
    let reviewers: Vec<DefaultReviewer> = get_all_pages(client, credentials, url).await?;
    Ok(reviewers
        .into_iter()
        .map(|reviewer| reviewer.user.into())
        .collect())
}

/// The commits on `source` that `destination` doesn't have, newest first: what a pull request
/// between them would carry.
pub async fn list_commits_between(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    source: &str,
    destination: &str,
) -> Result<Vec<Commit>> {
    let url = format!(
        "{}/commits?include={}&exclude={}&pagelen=30",
        repository_path(repository),
        urlencoding::encode(source),
        urlencoding::encode(destination)
    );
    // One page is plenty to fill a description; the count past it doesn't matter here.
    let page: Page<BitbucketCommit> = get(client, credentials, &url).await?;
    Ok(page.values.into_iter().map(Commit::from).collect())
}

pub struct NewPullRequest {
    pub title: String,
    pub description: String,
    pub source_branch: String,
    pub destination_branch: String,
    pub reviewer_ids: Vec<String>,
    pub close_source_branch: bool,
    pub draft: bool,
}

pub async fn create_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    new: NewPullRequest,
) -> Result<PullRequest> {
    let body = serde_json::json!({
        "title": new.title,
        "description": new.description,
        "source": { "branch": { "name": new.source_branch } },
        "destination": { "branch": { "name": new.destination_branch } },
        "reviewers": new.reviewer_ids.iter().map(|id| serde_json::json!({ "uuid": id })).collect::<Vec<_>>(),
        "close_source_branch": new.close_source_branch,
        "draft": new.draft,
    });
    let url = format!("{}/pullrequests", repository_path(repository));
    let created: BitbucketPullRequest = send(
        client,
        credentials,
        http_client::Method::POST,
        &url,
        Some(body),
    )
    .await?;
    Ok(created.into_pull_request(repository))
}

/// The ways a pull request into `destination` may be merged, the branch's default first.
pub async fn get_merge_strategies(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    destination: &str,
) -> Result<Vec<MergeStrategy>> {
    #[derive(Deserialize)]
    struct BranchStrategies {
        #[serde(default)]
        merge_strategies: Vec<String>,
        #[serde(default)]
        default_merge_strategy: Option<String>,
    }
    let url = format!(
        "{}/refs/branches/{}",
        repository_path(repository),
        urlencoding::encode(destination)
    );
    let branch: BranchStrategies = get(client, credentials, &url).await?;
    let mut strategies: Vec<MergeStrategy> = branch
        .merge_strategies
        .iter()
        .filter_map(|name| MergeStrategy::from_api(name))
        .collect();
    if strategies.is_empty() {
        strategies = MergeStrategy::ALL.to_vec();
    }
    if let Some(default) = branch
        .default_merge_strategy
        .as_deref()
        .and_then(MergeStrategy::from_api)
        && let Some(index) = strategies.iter().position(|strategy| *strategy == default)
    {
        let default = strategies.remove(index);
        strategies.insert(0, default);
    }
    Ok(strategies)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStrategy {
    MergeCommit,
    Squash,
    FastForward,
    SquashFastForward,
    RebaseFastForward,
    RebaseMerge,
}

impl MergeStrategy {
    const ALL: [MergeStrategy; 3] = [
        MergeStrategy::MergeCommit,
        MergeStrategy::Squash,
        MergeStrategy::FastForward,
    ];

    fn from_api(name: &str) -> Option<Self> {
        Some(match name {
            "merge_commit" => MergeStrategy::MergeCommit,
            "squash" => MergeStrategy::Squash,
            "fast_forward" => MergeStrategy::FastForward,
            "squash_fast_forward" => MergeStrategy::SquashFastForward,
            "rebase_fast_forward" => MergeStrategy::RebaseFastForward,
            "rebase_merge" => MergeStrategy::RebaseMerge,
            _ => return None,
        })
    }

    fn api_name(self) -> &'static str {
        match self {
            MergeStrategy::MergeCommit => "merge_commit",
            MergeStrategy::Squash => "squash",
            MergeStrategy::FastForward => "fast_forward",
            MergeStrategy::SquashFastForward => "squash_fast_forward",
            MergeStrategy::RebaseFastForward => "rebase_fast_forward",
            MergeStrategy::RebaseMerge => "rebase_merge",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MergeStrategy::MergeCommit => "Merge commit",
            MergeStrategy::Squash => "Squash",
            MergeStrategy::FastForward => "Fast-forward",
            MergeStrategy::SquashFastForward => "Squash + FF",
            MergeStrategy::RebaseFastForward => "Rebase + FF",
            MergeStrategy::RebaseMerge => "Rebase + merge",
        }
    }

    /// Whether the commits end up rewritten, so git won't see the local branch as merged.
    pub fn rewrites_commits(self) -> bool {
        !matches!(
            self,
            MergeStrategy::MergeCommit | MergeStrategy::FastForward
        )
    }
}

pub enum MergeOutcome {
    Merged(PullRequest),
    /// Bitbucket took longer than its request timeout and handed back a task to poll.
    Pending {
        task_url: String,
    },
}

pub async fn merge_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    strategy: MergeStrategy,
    message: &str,
    close_source_branch: bool,
) -> Result<MergeOutcome> {
    let url = format!(
        "{}/pullrequests/{number}/merge",
        repository_path(repository)
    );
    let body = serde_json::json!({
        "type": "pullrequest_merge_parameters",
        "message": message,
        "close_source_branch": close_source_branch,
        "merge_strategy": strategy.api_name(),
    });
    let response = send_raw(
        client,
        credentials,
        http_client::Method::POST,
        &url,
        Some(body),
        "application/json",
    )
    .await?;
    if response.status == 202 {
        let task_url = response
            .location
            .filter(|location| location.starts_with(API_URL))
            .context("o Bitbucket aceitou o merge mas não disse onde acompanhar")?;
        return Ok(MergeOutcome::Pending { task_url });
    }
    let merged: BitbucketPullRequest = response.parse()?;
    Ok(MergeOutcome::Merged(merged.into_pull_request(repository)))
}

/// `None` while the merge is still running.
pub async fn get_merge_task(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    task_url: &str,
) -> Result<Option<PullRequest>> {
    #[derive(Deserialize)]
    struct MergeTask {
        task_status: String,
        #[serde(default)]
        merge_result: Option<BitbucketPullRequest>,
    }
    anyhow::ensure!(task_url.starts_with(API_URL), "task de merge fora da API");
    let task: MergeTask = get(client, credentials, task_url).await?;
    match task.task_status.as_str() {
        "PENDING" => Ok(None),
        "SUCCESS" => Ok(Some(
            task.merge_result
                .context("merge concluído sem o pull request na resposta")?
                .into_pull_request(repository),
        )),
        other => anyhow::bail!("o merge terminou como {other}"),
    }
}

pub async fn decline_pull_request(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
) -> Result<()> {
    let url = format!(
        "{}/pullrequests/{number}/decline",
        repository_path(repository)
    );
    send::<serde_json::Value>(client, credentials, http_client::Method::POST, &url, None).await?;
    Ok(())
}

pub async fn set_approval(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    repository: &RemoteRepository,
    number: u64,
    approve: bool,
) -> Result<()> {
    let url = format!(
        "{}/pullrequests/{number}/approve",
        repository_path(repository)
    );
    let method = if approve {
        http_client::Method::POST
    } else {
        http_client::Method::DELETE
    };
    send::<serde_json::Value>(client, credentials, method, &url, None).await?;
    Ok(())
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
        let page: Page<T> = get(client, credentials, &current).await?;
        values.extend(page.values);
        // `next` is a full URL; the credentials only ever go to the API host.
        url = page.next.filter(|next| next.starts_with(API_URL));
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
    let response = send_raw(client, credentials, method, url, body, "application/json").await?;
    response.parse()
}

struct RawResponse {
    status: u16,
    location: Option<String>,
    text: String,
    endpoint: String,
}

impl RawResponse {
    fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        // Approving and a few other writes answer 204 with no body.
        let text = if self.text.trim().is_empty() {
            "null"
        } else {
            &self.text
        };
        serde_json::from_str(text)
            .with_context(|| format!("resposta inesperada do Bitbucket em {}", self.endpoint))
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
        .header("Authorization", credentials.authorization())
        .header("Accept", accept)
        // The diffstat of a pull request answers with a redirect to the commit range's.
        .follow_redirects(RedirectPolicy::FollowAll);
    let body = match body {
        Some(body) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(serde_json::to_string(&body)?)
        }
        None => AsyncBody::default(),
    };
    let request = builder.body(body)?;

    // Errors name the endpoint; the query only makes them longer.
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
        .with_context(|| format!("falha ao chamar o Bitbucket em {endpoint}"))?;

    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;

    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("Location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    match status {
        401 => return Err(ApiError::Unauthorized { host: HOST }.into()),
        403 => {
            return Err(ApiError::Forbidden {
                host: HOST,
                body: error_message(&text),
            }
            .into());
        }
        429 => return Err(ApiError::RateLimited { host: HOST }.into()),
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
        status,
        location,
        text,
        endpoint,
    })
}

/// Bitbucket wraps errors as `{"error": {"message": ...}}`; anything else is shown as sent.
fn error_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ErrorDetail,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        message: String,
    }
    serde_json::from_str::<ErrorBody>(body)
        .map(|body| body.error.message)
        .unwrap_or_else(|_| body.trim().chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    fn repository() -> RemoteRepository {
        RemoteRepository {
            hosting: Hosting::Bitbucket,
            owner: "trix".to_string(),
            name: "trix_frontend_app".to_string(),
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            email: "voce@empresa.com".to_string(),
            token: "ATATT123".to_string(),
        }
    }

    #[test]
    fn parses_bitbucket_remotes_only() {
        for url in [
            "git@bitbucket.org:trix/trix_frontend_app.git",
            "https://vinicios@bitbucket.org/trix/trix_frontend_app.git",
            "ssh://git@bitbucket.org/trix/trix_frontend_app",
        ] {
            assert_eq!(parse_remote(url), Some(repository()), "{url}");
        }
        assert_eq!(parse_remote("git@github.com:zed-industries/zed.git"), None);
    }

    #[test]
    fn authenticates_with_the_email_and_token() {
        // base64("voce@empresa.com:ATATT123")
        assert_eq!(
            credentials().authorization(),
            "Basic dm9jZUBlbXByZXNhLmNvbTpBVEFUVDEyMw=="
        );
    }

    #[test]
    fn parses_a_pull_request_with_participants() {
        let pull_request: BitbucketPullRequest = serde_json::from_str(
            r#"{
                "id": 157, "title": "Login com biometria", "state": "OPEN", "draft": false,
                "author": {"display_name": "Vinicios", "uuid": "{me}", "nickname": "vinicios-trix"},
                "source": {"branch": {"name": "feat/CU-86a1b2-biometria"},
                           "repository": {"full_name": "trix/trix_frontend_app"}},
                "destination": {"branch": {"name": "develop"}},
                "description": "Closes CU-86a1b2.",
                "comment_count": 2,
                "created_on": "2026-09-20T12:00:00.123456+00:00",
                "updated_on": "2026-09-24T09:30:00+00:00",
                "links": {"html": {"href": "https://bitbucket.org/trix/trix_frontend_app/pull-requests/157"}},
                "participants": [
                    {"user": {"display_name": "Ana Souza", "uuid": "{ana}", "nickname": "ana.souza"},
                     "role": "REVIEWER", "approved": true, "state": "approved",
                     "participated_on": "2026-09-24T08:00:00+00:00"},
                    {"user": {"display_name": "Rafael M", "uuid": "{rafael}"},
                     "role": "REVIEWER", "approved": false, "state": null, "participated_on": null},
                    {"user": {"display_name": "João", "uuid": "{joao}"},
                     "role": "PARTICIPANT", "approved": false, "state": "changes_requested"}
                ]
            }"#,
        )
        .unwrap();
        let pull_request = pull_request.into_pull_request(&repository());
        assert_eq!(pull_request.number, 157);
        assert_eq!(pull_request.state, PullRequestState::Open);
        assert_eq!(pull_request.author.short_name(), "vinicios-trix");
        assert_eq!(pull_request.reviewers().count(), 2);
        assert_eq!(pull_request.approval_count(), 1);
        assert!(pull_request.awaits_review_from("{rafael}"));
        assert!(!pull_request.awaits_review_from("{ana}"));
        assert!(pull_request.participants[2].requested_changes);
        assert!(!pull_request.is_from_fork(&repository()));
        assert!(pull_request.updated_at.is_some());
    }

    #[test]
    fn tolerates_what_lists_leave_out() {
        let pull_request: BitbucketPullRequest = serde_json::from_str(
            r#"{"id": 3, "title": "x", "state": "MERGED", "author": {"type": "team"},
                "source": {"branch": {"name": "a"}}, "destination": {"branch": {"name": "main"}}}"#,
        )
        .unwrap();
        let pull_request = pull_request.into_pull_request(&repository());
        assert_eq!(pull_request.state, PullRequestState::Merged);
        assert_eq!(pull_request.author.display_name, "Usuário removido");
        assert_eq!(
            pull_request.url,
            "https://bitbucket.org/trix/trix_frontend_app/pull-requests/3"
        );
        assert!(pull_request.participants.is_empty());
    }

    #[test]
    fn parses_diffstat_statuses_and_comments() {
        let files: Page<DiffStat> = serde_json::from_str(
            r#"{"values": [
                {"status": "modified", "lines_added": 210, "lines_removed": 40,
                 "old": {"path": "src/screens/auth/LoginScreen.tsx"}, "new": {"path": "src/screens/auth/LoginScreen.tsx"}},
                {"status": "added", "lines_added": 96, "lines_removed": 0, "old": null, "new": {"path": "src/hooks/useBiometrics.ts"}},
                {"status": "renamed", "old": {"path": "a.ts"}, "new": {"path": "b.ts"}},
                {"status": "removed", "old": {"path": "old.ts"}, "new": null}
            ]}"#,
        )
        .unwrap();
        let files: Vec<ChangedFile> = files
            .values
            .into_iter()
            .filter_map(DiffStat::into_changed_file)
            .collect();
        assert_eq!(files[1].change, FileChange::Added);
        assert_eq!(files[2].old_path.as_deref(), Some("a.ts"));
        assert_eq!(files[3].path, "old.ts");
        assert_eq!(files[0].old_path, None);

        let statuses: Page<Status> = serde_json::from_str(
            r#"{"values": [
                {"key": "pipeline", "name": "Pipeline #1284", "state": "SUCCESSFUL", "url": "https://x"},
                {"key": "sonar", "name": "", "state": "INPROGRESS"}
            ]}"#,
        )
        .unwrap();
        let checks: Vec<Check> = statuses.values.into_iter().map(Check::from).collect();
        assert_eq!(checks[0].state, CheckState::Passed);
        assert_eq!(checks[1].name, "sonar");

        let comments: Page<BitbucketComment> = serde_json::from_str(
            r#"{"values": [
                {"id": 1, "user": {"display_name": "Ana"}, "content": {"raw": "Dá pra reaproveitar"},
                 "inline": {"path": "LoginScreen.tsx", "to": 88}, "created_on": "2026-09-24T08:00:00+00:00"},
                {"id": 2, "user": {"display_name": "Ana"}, "content": {"raw": ""}, "deleted": true,
                 "parent": {"id": 1}}
            ], "next": "https://api.bitbucket.org/2.0/x?page=2"}"#,
        )
        .unwrap();
        assert_eq!(comments.values[0].inline.as_ref().unwrap().to, Some(88));
        assert!(comments.values[1].deleted && comments.values[1].parent.is_some());
    }

    #[test]
    fn parses_commits_with_or_without_an_account() {
        let commits: Page<BitbucketCommit> = serde_json::from_str(
            r#"{"values": [
                {"hash": "e41c9a2f00", "message": "LoginScreen oferece Face ID\n\nDetalhes", "date": "2026-09-24T09:00:00+00:00",
                 "author": {"raw": "Vinicios <v@x.com>", "user": {"display_name": "Vinicios", "nickname": "vinicios-trix"}}},
                {"hash": "90fe5b8", "message": "Instala expo", "author": {"raw": "Rafael M <r@x.com>"}}
            ]}"#,
        )
        .unwrap();
        let commits: Vec<Commit> = commits.values.into_iter().map(Commit::from).collect();
        assert_eq!(commits[0].summary, "LoginScreen oferece Face ID");
        assert_eq!(commits[0].author, "vinicios-trix");
        assert_eq!(commits[0].short_hash(), "e41c9a2");
        assert_eq!(commits[1].author, "Rafael M");
    }

    #[gpui::test]
    async fn picks_the_development_branch_then_the_main_one() {
        let client = |model: &'static str| -> Arc<dyn HttpClient> {
            FakeHttpClient::create(move |request| async move {
                let body = if request.uri().path().ends_with("/branching-model") {
                    model
                } else {
                    r#"{"mainbranch": {"name": "main"}}"#
                };
                Ok(Response::builder().status(200).body(body.into())?)
            })
        };
        let with_develop =
            client(r#"{"development": {"name": "develop", "branch": {"name": "develop"}}}"#);
        assert_eq!(
            get_default_destination(&with_develop, &credentials(), &repository())
                .await
                .unwrap()
                .as_deref(),
            Some("develop")
        );
        let without = client(r#"{"development": null}"#);
        assert_eq!(
            get_default_destination(&without, &credentials(), &repository())
                .await
                .unwrap()
                .as_deref(),
            Some("main")
        );
    }

    #[gpui::test]
    async fn creates_a_pull_request_with_reviewers() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|mut request| async move {
            assert_eq!(request.method(), http_client::Method::POST);
            let mut body = String::new();
            request.body_mut().read_to_string(&mut body).await?;
            let body: serde_json::Value = serde_json::from_str(&body)?;
            assert_eq!(body["source"]["branch"]["name"], "feat/x");
            assert_eq!(body["destination"]["branch"]["name"], "develop");
            assert_eq!(body["reviewers"][0]["uuid"], "{ana}");
            assert_eq!(body["close_source_branch"], true);
            Ok(Response::builder().status(201).body(
                r#"{"id": 158, "title": "X", "state": "OPEN", "author": {"uuid": "{me}"},
                    "source": {"branch": {"name": "feat/x"}}, "destination": {"branch": {"name": "develop"}}}"#
                    .into(),
            )?)
        });
        let created = create_pull_request(
            &client,
            &credentials(),
            &repository(),
            NewPullRequest {
                title: "X".to_string(),
                description: String::new(),
                source_branch: "feat/x".to_string(),
                destination_branch: "develop".to_string(),
                reviewer_ids: vec!["{ana}".to_string()],
                close_source_branch: true,
                draft: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(created.number, 158);
    }

    #[gpui::test]
    async fn merges_now_or_hands_back_a_task() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path().to_string();
            let response = if path.ends_with("/157/merge") {
                Response::builder().status(200).body(
                    r#"{"id": 157, "title": "X", "state": "MERGED", "author": {"uuid": "{me}"},
                        "source": {"branch": {"name": "a"}}, "destination": {"branch": {"name": "develop"}}}"#
                        .into(),
                )?
            } else if path.ends_with("/158/merge") {
                Response::builder()
                    .status(202)
                    .header(
                        "Location",
                        "https://api.bitbucket.org/2.0/repositories/trix/trix_frontend_app/pullrequests/158/merge/task-status/t1",
                    )
                    .body("".into())?
            } else if path.ends_with("/task-status/t1") {
                Response::builder()
                    .status(200)
                    .body(r#"{"task_status": "PENDING"}"#.into())?
            } else {
                Response::builder()
                    .status(200)
                    .body(r#"{"merge_strategies": ["merge_commit", "squash"], "default_merge_strategy": "squash"}"#.into())?
            };
            Ok(response)
        });
        let merge = |number| {
            let client = client.clone();
            async move {
                merge_pull_request(
                    &client,
                    &credentials(),
                    &repository(),
                    number,
                    MergeStrategy::Squash,
                    "msg",
                    true,
                )
                .await
                .unwrap()
            }
        };
        assert!(matches!(
            merge(157).await,
            MergeOutcome::Merged(pull_request) if pull_request.state == PullRequestState::Merged
        ));
        let MergeOutcome::Pending { task_url } = merge(158).await else {
            panic!("expected a pending merge");
        };
        assert!(
            get_merge_task(&client, &credentials(), &repository(), &task_url)
                .await
                .unwrap()
                .is_none()
        );
        let strategies = get_merge_strategies(&client, &credentials(), &repository(), "develop")
            .await
            .unwrap();
        assert_eq!(
            strategies,
            [MergeStrategy::Squash, MergeStrategy::MergeCommit]
        );
    }

    #[gpui::test]
    async fn lists_pipelines_and_tells_when_issues_are_off() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path().to_string();
            let body = if path.ends_with("/pipelines/") {
                r#"{"values": [
                    {"build_number": 1284, "state": {"name": "COMPLETED", "result": {"name": "SUCCESSFUL"}},
                     "target": {"ref_name": "develop", "commit": {"hash": "abc"}}, "trigger": {"name": "PUSH"},
                     "creator": {"display_name": "Ana", "nickname": "ana.souza"}, "duration_in_seconds": 372},
                    {"build_number": 1285, "state": {"name": "IN_PROGRESS", "stage": {"name": "RUNNING"}},
                     "target": {"pullrequest": {"id": 157}}},
                    {"build_number": 1286, "state": {"name": "COMPLETED", "result": {"name": "FAILED"}}}
                ]}"#
            } else {
                r#"{"has_issues": false}"#
            };
            Ok(Response::builder().status(200).body(body.into())?)
        });
        let pipelines = list_pipelines(&client, &credentials(), &repository())
            .await
            .unwrap();
        assert_eq!(pipelines[0].state, PipelineState::Passed);
        assert_eq!(pipelines[0].creator.as_deref(), Some("ana.souza"));
        assert_eq!(pipelines[1].state, PipelineState::Running);
        assert_eq!(pipelines[1].pull_request, Some(157));
        assert_eq!(pipelines[2].state, PipelineState::Failed);
        assert!(pipelines[0].url.ends_with("/pipelines/results/1284"));
        assert!(
            list_open_issues(&client, &credentials(), &repository())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[gpui::test]
    async fn follows_pages_only_on_the_api_host() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let body = match request.uri().query().unwrap_or_default() {
                query if query.contains("page=2") => {
                    r#"{"values": [{"state": "FAILED", "key": "b"}], "next": "https://evil.example/2.0/x"}"#
                }
                _ => {
                    r#"{"values": [{"state": "SUCCESSFUL", "key": "a"}], "next": "https://api.bitbucket.org/2.0/repositories/trix/trix_frontend_app/pullrequests/1/statuses?page=2"}"#
                }
            };
            Ok(Response::builder().status(200).body(body.into())?)
        });
        let checks = get_checks(&client, &credentials(), &repository(), 1)
            .await
            .unwrap();
        assert_eq!(checks.len(), 2);
    }

    #[gpui::test]
    async fn reports_rejected_and_underscoped_tokens() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let status = if request.uri().path() == "/2.0/user" {
                401
            } else {
                403
            };
            Ok(Response::builder().status(status).body(
                r#"{"type": "error", "error": {"message": "Your credentials lack one or more required privilege scopes."}}"#.into(),
            )?)
        });
        let error = get_user(&client, &credentials()).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ApiError>(),
            Some(ApiError::Unauthorized { .. })
        ));
        let error = list_open_pull_requests(&client, &credentials(), &repository())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("privilege scopes"));
    }
}
