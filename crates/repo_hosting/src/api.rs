//! What the Repo panel shows, independent of the host it came from. Each host's module turns
//! its own API responses into these types.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use i18n::t;
use serde::de::DeserializeOwned;
use std::collections::HashMap;

/// The failures the UI reacts to differently. Anything else surfaces as a plain error.
#[derive(Debug)]
pub enum ApiError {
    /// The token was revoked, expired or mistyped, or the e-mail does not match it.
    Unauthorized {
        host: &'static str,
    },
    /// The token works but lacks a scope this request needs.
    Forbidden {
        host: &'static str,
        body: String,
    },
    RateLimited {
        host: &'static str,
    },
    Status {
        host: &'static str,
        status: u16,
        body: String,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            ApiError::Unauthorized { host } => t!("repo_hosting.error.unauthorized", host = host),
            ApiError::Forbidden { host, body } => {
                t!("repo_hosting.error.forbidden", host = host, body = body)
            }
            ApiError::RateLimited { host } => t!("repo_hosting.error.rate_limited", host = host),
            ApiError::Status { host, status, body } => t!(
                "repo_hosting.error.status",
                host = host,
                status = status,
                body = body
            ),
        };
        formatter.write_str(&message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hosting {
    Bitbucket,
    GitHub,
    GitLab,
}

impl Hosting {
    pub fn name(self) -> &'static str {
        match self {
            Hosting::Bitbucket => "Bitbucket",
            Hosting::GitHub => "GitHub",
            Hosting::GitLab => "GitLab",
        }
    }
}

/// The repository on the host that the local checkout's remote points at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteRepository {
    pub hosting: Hosting,
    /// The Bitbucket workspace, the GitHub owner or the GitLab namespace (subgroups included,
    /// as in `group/subgroup`).
    pub owner: String,
    pub name: String,
    /// The self-hosted GitLab's host name; `None` on the public instance of each host.
    pub instance: Option<String>,
}

impl RemoteRepository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The host name the repository's web pages and API live on.
    pub fn instance(&self) -> &str {
        match (&self.instance, self.hosting) {
            (Some(instance), _) => instance,
            (None, Hosting::Bitbucket) => "bitbucket.org",
            (None, Hosting::GitHub) => "github.com",
            (None, Hosting::GitLab) => "gitlab.com",
        }
    }

    pub fn web_url(&self) -> String {
        format!("https://{}/{}/{}", self.instance(), self.owner, self.name)
    }

    /// The host's page for opening a pull request from `source`.
    pub fn new_pull_request_url(&self, source: &str, destination: Option<&str>) -> String {
        let source = urlencoding::encode(source);
        match (self.hosting, destination) {
            (Hosting::Bitbucket, Some(destination)) => format!(
                "{}/pull-requests/new?source={source}&dest={}",
                self.web_url(),
                urlencoding::encode(destination)
            ),
            (Hosting::Bitbucket, None) => {
                format!("{}/pull-requests/new?source={source}", self.web_url())
            }
            (Hosting::GitHub, Some(destination)) => format!(
                "{}/compare/{}...{source}?expand=1",
                self.web_url(),
                urlencoding::encode(destination)
            ),
            (Hosting::GitHub, None) => format!("{}/pull/new/{source}", self.web_url()),
            (Hosting::GitLab, Some(destination)) => format!(
                "{}/-/merge_requests/new?merge_request%5Bsource_branch%5D={source}&merge_request%5Btarget_branch%5D={}",
                self.web_url(),
                urlencoding::encode(destination)
            ),
            (Hosting::GitLab, None) => format!(
                "{}/-/merge_requests/new?merge_request%5Bsource_branch%5D={source}",
                self.web_url()
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// Stable across renames: what "is this me" is decided by.
    pub id: String,
    pub display_name: String,
    /// The handle people @mention, when the host has one.
    pub nickname: Option<String>,
}

impl Person {
    /// The short name the list shows, as in "ana.souza → develop".
    pub fn short_name(&self) -> &str {
        self.nickname
            .as_deref()
            .filter(|nickname| !nickname.trim().is_empty())
            .unwrap_or(&self.display_name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Merged,
    Declined,
    Superseded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticipantRole {
    Reviewer,
    Participant,
}

#[derive(Clone, Debug)]
pub struct Participant {
    pub person: Person,
    pub role: ParticipantRole,
    pub approved: bool,
    pub requested_changes: bool,
    pub participated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: PullRequestState,
    pub draft: bool,
    pub author: Person,
    pub source_branch: String,
    /// Set when the branch lives in a fork rather than in this repository.
    pub source_repository: Option<String>,
    pub destination_branch: String,
    pub description: String,
    pub comment_count: u64,
    /// Bitbucket's pull request tasks that are still unresolved.
    pub open_task_count: u64,
    pub updated_at: Option<DateTime<Utc>>,
    pub url: String,
    /// Reviewers and everyone who commented or approved. Lists only carry it when the host
    /// sends it there; the detail always does.
    pub participants: Vec<Participant>,
}

impl PullRequest {
    pub fn reviewers(&self) -> impl Iterator<Item = &Participant> {
        self.participants
            .iter()
            .filter(|participant| participant.role == ParticipantRole::Reviewer)
    }

    pub fn approval_count(&self) -> usize {
        self.participants
            .iter()
            .filter(|participant| participant.approved)
            .count()
    }

    /// Whether `user_id` was asked to review and has not approved yet.
    pub fn awaits_review_from(&self, user_id: &str) -> bool {
        self.reviewers()
            .any(|reviewer| reviewer.person.id == user_id && !reviewer.approved)
    }

    pub fn has_approval_from(&self, user_id: &str) -> bool {
        self.participants
            .iter()
            .any(|participant| participant.person.id == user_id && participant.approved)
    }

    /// Whether the branch can be checked out from this repository's own remote.
    pub fn is_from_fork(&self, repository: &RemoteRepository) -> bool {
        self.source_repository
            .as_deref()
            .is_some_and(|source| !source.eq_ignore_ascii_case(&repository.full_name()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Running,
    Stopped,
}

/// A build status or pipeline reported on the pull request's head commit.
#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub url: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    /// The GitHub Actions or GitLab CI job behind the check, which can be run again on its own.
    pub rerun_job: Option<u64>,
}

/// The checks as the design summarizes them: "4/4" when every check passed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckSummary {
    pub passed: usize,
    pub failed: usize,
    pub running: usize,
    pub total: usize,
}

impl CheckSummary {
    pub fn of(checks: &[Check]) -> Self {
        let mut summary = Self {
            total: checks.len(),
            ..Self::default()
        };
        for check in checks {
            match check.state {
                CheckState::Passed => summary.passed += 1,
                CheckState::Failed => summary.failed += 1,
                CheckState::Running => summary.running += 1,
                CheckState::Stopped => {}
            }
        }
        summary
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Removed,
    Modified,
    Renamed,
    /// The destination changed the same lines; the host can't merge until it's resolved.
    Conflict,
}

#[derive(Clone, Debug)]
pub struct ChangedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub change: FileChange,
    pub lines_added: u64,
    pub lines_removed: u64,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    pub author: Person,
    pub body: String,
    pub created_at: Option<DateTime<Utc>>,
    /// The file and line an inline comment is on.
    pub inline: Option<(String, Option<u64>)>,
    pub is_reply: bool,
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    /// The first line of the message.
    pub summary: String,
    /// The account when the host matched the commit's e-mail to one, else the name git recorded.
    pub author: String,
    pub date: Option<DateTime<Utc>>,
}

impl Commit {
    pub fn short_hash(&self) -> &str {
        self.hash.get(..7).unwrap_or(&self.hash)
    }
}

/// A pull request with what only its detail view needs.
#[derive(Clone, Debug)]
pub struct PullRequestDetail {
    pub pull_request: PullRequest,
    pub checks: Vec<Check>,
    pub files: Vec<ChangedFile>,
    pub comments: Vec<Comment>,
    /// Newest first, as the host lists them.
    pub commits: Vec<Commit>,
    /// GitHub says so for the whole pull request; Bitbucket marks the files instead.
    pub has_conflicts: bool,
    /// Whether GitHub will merge it by itself once the checks and reviews allow.
    pub auto_merge: bool,
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
    pub fn label(self, hosting: Hosting) -> &'static str {
        match (self, hosting) {
            (MergeStrategy::MergeCommit, Hosting::GitHub) => "Merge",
            (MergeStrategy::RebaseMerge, Hosting::GitHub) => "Rebase",
            (MergeStrategy::MergeCommit, _) => "Merge commit",
            (MergeStrategy::Squash, _) => "Squash",
            (MergeStrategy::FastForward, _) => "Fast-forward",
            (MergeStrategy::SquashFastForward, _) => "Squash + FF",
            (MergeStrategy::RebaseFastForward, _) => "Rebase + FF",
            (MergeStrategy::RebaseMerge, _) => "Rebase + merge",
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

pub struct NewPullRequest {
    pub title: String,
    pub description: String,
    pub source_branch: String,
    pub destination_branch: String,
    pub reviewer_ids: Vec<String>,
    pub close_source_branch: bool,
    pub draft: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineState {
    Pending,
    Running,
    Passed,
    Failed,
    Stopped,
    Paused,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub number: u64,
    pub state: PipelineState,
    /// The branch or tag it ran on; `None` for a pull request pipeline, which has `pull_request`.
    pub ref_name: Option<String>,
    pub pull_request: Option<u64>,
    /// How it started: a push, a pull request, by hand or on a schedule.
    pub trigger: String,
    pub creator: Option<String>,
    pub commit: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub duration_seconds: Option<u64>,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub kind: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub url: String,
}

/// The `rel="next"` URL of a `Link` header.
pub(crate) fn next_page(link: &str) -> Option<String> {
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

pub(crate) struct RawResponse {
    pub text: String,
    pub endpoint: String,
    /// Lowercased names.
    pub headers: HashMap<String, String>,
    pub host: &'static str,
}

impl RawResponse {
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        // Merges, approvals, reruns and deletes can answer with no body.
        let text = if self.text.trim().is_empty() {
            "null"
        } else {
            &self.text
        };
        serde_json::from_str(text).with_context(|| {
            t!(
                "repo_hosting.error.unexpected_response",
                host = self.host,
                endpoint = self.endpoint
            )
        })
    }
}
