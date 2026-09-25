//! What the Repo panel shows, independent of the host it came from. Each host's module turns
//! its own API responses into these types.

use chrono::{DateTime, Utc};

/// The failures the UI reacts to differently. Anything else surfaces as a plain error.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The token was revoked, expired or mistyped, or the e-mail does not match it.
    #[error("{host} recusou as credenciais")]
    Unauthorized { host: &'static str },
    /// The token works but lacks a scope this request needs.
    #[error("{host} negou acesso: falta um escopo no token ({body})")]
    Forbidden { host: &'static str, body: String },
    #[error("Limite de requisições de {host} atingido; tente de novo em instantes")]
    RateLimited { host: &'static str },
    #[error("{host} respondeu {status}: {body}")]
    Status {
        host: &'static str,
        status: u16,
        body: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hosting {
    Bitbucket,
}

impl Hosting {
    pub fn name(self) -> &'static str {
        match self {
            Hosting::Bitbucket => "Bitbucket",
        }
    }
}

/// The repository on the host that the local checkout's remote points at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteRepository {
    pub hosting: Hosting,
    /// The Bitbucket workspace or the GitHub owner.
    pub owner: String,
    pub name: String,
}

impl RemoteRepository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn web_url(&self) -> String {
        match self.hosting {
            Hosting::Bitbucket => format!("https://bitbucket.org/{}/{}", self.owner, self.name),
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
