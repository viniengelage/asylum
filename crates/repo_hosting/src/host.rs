//! One way into whichever host the repository lives on, so the panel and the agent's tools
//! don't branch on it at every call.

use crate::{
    api::{
        Check, Commit, Hosting, Issue, MergeOutcome, MergeStrategy, NewPullRequest, Person,
        Pipeline, PullRequest, PullRequestDetail, PullRequestState, RemoteRepository,
    },
    bitbucket, github,
};
use anyhow::Result;
use http_client::HttpClient;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credentials {
    Bitbucket(bitbucket::Credentials),
    GitHub(github::Credentials),
}

impl Credentials {
    pub fn hosting(&self) -> Hosting {
        match self {
            Credentials::Bitbucket(_) => Hosting::Bitbucket,
            Credentials::GitHub(_) => Hosting::GitHub,
        }
    }
}

/// What the merge card offers for a destination branch.
pub struct MergeOptions {
    /// The branch's default first, when the host has one.
    pub strategies: Vec<MergeStrategy>,
    pub allows_auto_merge: bool,
}

#[derive(Clone)]
pub struct Client {
    pub http_client: Arc<dyn HttpClient>,
    pub credentials: Credentials,
}

impl Client {
    pub fn new(http_client: Arc<dyn HttpClient>, credentials: Credentials) -> Self {
        Self {
            http_client,
            credentials,
        }
    }

    pub fn hosting(&self) -> Hosting {
        self.credentials.hosting()
    }

    pub fn github_credentials(&self) -> Option<&github::Credentials> {
        match &self.credentials {
            Credentials::GitHub(credentials) => Some(credentials),
            Credentials::Bitbucket(_) => None,
        }
    }

    pub async fn list_open_pull_requests(
        &self,
        repository: &RemoteRepository,
    ) -> Result<Vec<PullRequest>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_open_pull_requests(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(credentials) => {
                github::list_open_pull_requests(&self.http_client, credentials, repository).await
            }
        }
    }

    pub async fn list_closed_pull_requests(
        &self,
        repository: &RemoteRepository,
        state: PullRequestState,
    ) -> Result<Vec<PullRequest>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_closed_pull_requests(
                    &self.http_client,
                    credentials,
                    repository,
                    state,
                )
                .await
            }
            Credentials::GitHub(credentials) => {
                github::list_closed_pull_requests(&self.http_client, credentials, repository, state)
                    .await
            }
        }
    }

    pub async fn list_pipelines(&self, repository: &RemoteRepository) -> Result<Vec<Pipeline>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_pipelines(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(credentials) => {
                github::list_pipelines(&self.http_client, credentials, repository).await
            }
        }
    }

    pub async fn list_open_issues(
        &self,
        repository: &RemoteRepository,
    ) -> Result<Option<Vec<Issue>>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_open_issues(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(credentials) => {
                github::list_open_issues(&self.http_client, credentials, repository).await
            }
        }
    }

    pub async fn get_pull_request_diff(
        &self,
        repository: &RemoteRepository,
        number: u64,
    ) -> Result<String> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_pull_request_diff(&self.http_client, credentials, repository, number)
                    .await
            }
            Credentials::GitHub(credentials) => {
                github::get_pull_request_diff(&self.http_client, credentials, repository, number)
                    .await
            }
        }
    }

    pub async fn get_checks(
        &self,
        repository: &RemoteRepository,
        number: u64,
    ) -> Result<Vec<Check>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_checks(&self.http_client, credentials, repository, number).await
            }
            Credentials::GitHub(credentials) => {
                github::get_checks(&self.http_client, credentials, repository, number).await
            }
        }
    }

    pub async fn get_pull_request_detail(
        &self,
        repository: &RemoteRepository,
        number: u64,
    ) -> Result<PullRequestDetail> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_pull_request_detail(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                )
                .await
            }
            Credentials::GitHub(credentials) => {
                github::get_pull_request_detail(&self.http_client, credentials, repository, number)
                    .await
            }
        }
    }

    pub async fn post_comment(
        &self,
        repository: &RemoteRepository,
        number: u64,
        text: &str,
    ) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::post_comment(&self.http_client, credentials, repository, number, text)
                    .await
            }
            Credentials::GitHub(credentials) => {
                github::post_comment(&self.http_client, credentials, repository, number, text).await
            }
        }
    }

    pub async fn post_inline_comment(
        &self,
        repository: &RemoteRepository,
        number: u64,
        path: &str,
        line: u64,
        text: &str,
    ) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::post_inline_comment(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                    path,
                    line,
                    text,
                )
                .await
            }
            Credentials::GitHub(credentials) => {
                github::post_inline_comment(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                    path,
                    line,
                    text,
                )
                .await
            }
        }
    }

    pub async fn get_default_destination(
        &self,
        repository: &RemoteRepository,
    ) -> Result<Option<String>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_default_destination(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(credentials) => {
                github::get_default_destination(&self.http_client, credentials, repository).await
            }
        }
    }

    pub async fn list_branches(&self, repository: &RemoteRepository) -> Result<Vec<String>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_branches(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(credentials) => {
                github::list_branches(&self.http_client, credentials, repository).await
            }
        }
    }

    /// GitHub has no default reviewers outside CODEOWNERS, which it applies by itself.
    pub async fn get_default_reviewers(
        &self,
        repository: &RemoteRepository,
    ) -> Result<Vec<Person>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_default_reviewers(&self.http_client, credentials, repository).await
            }
            Credentials::GitHub(_) => Ok(Vec::new()),
        }
    }

    pub async fn list_commits_between(
        &self,
        repository: &RemoteRepository,
        source: &str,
        destination: &str,
    ) -> Result<Vec<Commit>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::list_commits_between(
                    &self.http_client,
                    credentials,
                    repository,
                    source,
                    destination,
                )
                .await
            }
            Credentials::GitHub(credentials) => {
                github::list_commits_between(
                    &self.http_client,
                    credentials,
                    repository,
                    source,
                    destination,
                )
                .await
            }
        }
    }

    pub async fn create_pull_request(
        &self,
        repository: &RemoteRepository,
        new: NewPullRequest,
    ) -> Result<PullRequest> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::create_pull_request(&self.http_client, credentials, repository, new)
                    .await
            }
            Credentials::GitHub(credentials) => {
                github::create_pull_request(&self.http_client, credentials, repository, new).await
            }
        }
    }

    pub async fn get_merge_options(
        &self,
        repository: &RemoteRepository,
        destination: &str,
    ) -> Result<MergeOptions> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => Ok(MergeOptions {
                strategies: bitbucket::get_merge_strategies(
                    &self.http_client,
                    credentials,
                    repository,
                    destination,
                )
                .await?,
                allows_auto_merge: false,
            }),
            Credentials::GitHub(credentials) => {
                let (strategies, allows_auto_merge) =
                    github::get_merge_strategies(&self.http_client, credentials, repository)
                        .await?;
                Ok(MergeOptions {
                    strategies,
                    allows_auto_merge,
                })
            }
        }
    }

    pub async fn merge_pull_request(
        &self,
        repository: &RemoteRepository,
        number: u64,
        strategy: MergeStrategy,
        message: &str,
        close_source_branch: bool,
    ) -> Result<MergeOutcome> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::merge_pull_request(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                    strategy,
                    message,
                    close_source_branch,
                )
                .await
            }
            Credentials::GitHub(credentials) => {
                github::merge_pull_request(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                    strategy,
                    message,
                    close_source_branch,
                )
                .await
            }
        }
    }

    /// `None` while the merge is still running. Only Bitbucket hands back tasks.
    pub async fn get_merge_task(
        &self,
        repository: &RemoteRepository,
        task_url: &str,
    ) -> Result<Option<PullRequest>> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::get_merge_task(&self.http_client, credentials, repository, task_url)
                    .await
            }
            Credentials::GitHub(_) => anyhow::bail!("o GitHub não acompanha merges por task"),
        }
    }

    pub async fn enable_auto_merge(
        &self,
        repository: &RemoteRepository,
        number: u64,
        strategy: MergeStrategy,
    ) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(_) => anyhow::bail!("o Bitbucket não tem auto-merge"),
            Credentials::GitHub(credentials) => {
                github::enable_auto_merge(
                    &self.http_client,
                    credentials,
                    repository,
                    number,
                    strategy,
                )
                .await
            }
        }
    }

    pub async fn decline_pull_request(
        &self,
        repository: &RemoteRepository,
        number: u64,
    ) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::decline_pull_request(&self.http_client, credentials, repository, number)
                    .await
            }
            Credentials::GitHub(credentials) => {
                github::decline_pull_request(&self.http_client, credentials, repository, number)
                    .await
            }
        }
    }

    pub async fn set_approval(
        &self,
        repository: &RemoteRepository,
        number: u64,
        approve: bool,
    ) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(credentials) => {
                bitbucket::set_approval(&self.http_client, credentials, repository, number, approve)
                    .await
            }
            Credentials::GitHub(credentials) => {
                github::set_approval(&self.http_client, credentials, repository, number, approve)
                    .await
            }
        }
    }

    pub async fn rerun_job(&self, repository: &RemoteRepository, job_id: u64) -> Result<()> {
        match &self.credentials {
            Credentials::Bitbucket(_) => anyhow::bail!("só jobs do GitHub Actions re-rodam daqui"),
            Credentials::GitHub(credentials) => {
                github::rerun_job(&self.http_client, credentials, repository, job_id).await
            }
        }
    }

    pub async fn get_job_log(&self, repository: &RemoteRepository, job_id: u64) -> Result<String> {
        match &self.credentials {
            Credentials::Bitbucket(_) => anyhow::bail!("só jobs do GitHub Actions têm log aqui"),
            Credentials::GitHub(credentials) => {
                github::get_job_log(&self.http_client, credentials, repository, job_id).await
            }
        }
    }
}

/// The repository an `origin` URL points at, on any host the panel speaks to.
pub fn parse_remote(url: &str) -> Option<RemoteRepository> {
    bitbucket::parse_remote(url).or_else(|| github::parse_remote(url))
}
