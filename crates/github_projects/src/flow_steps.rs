//! The GitHub steps of esteiras: starting the item, opening its pull request, waiting for the
//! checks and moving or commenting on the item. None of them calls a model.

use crate::{
    agent_toolkit::{projects_context, window_for},
    api::{self, Item},
    github_repository,
};
use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Entity, Task};
use i18n::t;
use project::Project;
use repo_hosting::{
    RepoStore,
    api::{CheckState, CheckSummary, Hosting, NewPullRequest, PullRequest},
    host,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use task_agents::{GitHubStepOutcome, GitHubStepRequest, GitHubStepRunner};

/// GitHub takes a moment to start the checks of a new push; with none after this, there are
/// none to wait for.
const CHECKS_APPEAR_TIMEOUT: Duration = Duration::from_secs(120);
const CHECKS_POLL_INTERVAL: Duration = Duration::from_secs(20);
const MAX_LOG_CHARS: usize = 6_000;

pub(crate) fn register(cx: &mut App) {
    task_agents::register_github_step_runner(
        GitHubStepRunner {
            repository: Arc::new(|project, cx| {
                github_repository(project, cx).map(|(repository, _, _)| repository.full_name())
            }),
            run: Arc::new(run),
        },
        cx,
    );
}

fn run(
    project: &Entity<Project>,
    request: GitHubStepRequest,
    cx: &mut App,
) -> Task<Result<GitHubStepOutcome>> {
    let project = project.clone();
    cx.spawn(async move |cx| match request.action.as_str() {
        "start_item" => start(&project, &request, cx).await,
        "open_pr" => open_pull_request(&project, &request, cx).await,
        "wait_checks" => wait_checks(&project, &request, cx).await,
        "move_item" => move_item(&project, &request, cx).await,
        "comment_item" => comment(&project, &request, cx).await,
        other => anyhow::bail!("{}", t!("github_projects.flow.unknown_step", step = other)),
    })
}

fn passed(evidence: String, outputs: BTreeMap<String, String>) -> GitHubStepOutcome {
    GitHubStepOutcome {
        passed: true,
        evidence,
        outputs,
    }
}

fn failed(evidence: String, outputs: BTreeMap<String, String>) -> GitHubStepOutcome {
    GitHubStepOutcome {
        passed: false,
        evidence,
        outputs,
    }
}

/// The item the run is about: `#204` in its input, else the item of the checked-out branch.
fn run_item(
    project: &api::Project,
    request: &GitHubStepRequest,
    branch: Option<&str>,
) -> Result<Item> {
    let mentioned = request.request.split_whitespace().find_map(|word| {
        word.split('#')
            .nth(1)?
            .trim_end_matches(|character: char| !character.is_ascii_digit())
            .parse::<u64>()
            .ok()
    });
    let item = match mentioned {
        Some(number) => project
            .items
            .iter()
            .find(|item| item.number == Some(number))
            .with_context(|| {
                t!(
                    "github_projects.flow.item_not_in_project",
                    number = number,
                    project = project.summary.title
                )
            })?,
        None => branch
            .and_then(|branch| api::item_for_branch(&project.items, branch))
            .with_context(|| t!("github_projects.flow.no_item_for_run"))?,
    };
    Ok(item.clone())
}

async fn start(
    project: &Entity<Project>,
    request: &GitHubStepRequest,
    cx: &mut AsyncApp,
) -> Result<GitHubStepOutcome> {
    let context = projects_context(project, cx).await?;
    let item = run_item(&context.project, request, context.branch.as_deref())?;
    let branch = item
        .linked_branches
        .first()
        .cloned()
        .unwrap_or_else(|| api::branch_name(&item));
    let (window, workspace) = cx
        .update(|cx| window_for(project, cx))
        .with_context(|| t!("github_projects.flow.no_window_for_checkout"))?;
    let workspace = workspace.downgrade();
    let state = context.state.clone();
    let item_id = item.id.clone();
    let started = window.update(cx, |_, window, cx| {
        crate::start_item(
            workspace,
            project.clone(),
            state,
            item_id,
            branch.clone(),
            window,
            cx,
        )
    })?;
    started.await?;
    Ok(passed(
        t!(
            "github_projects.flow.started",
            item = item.reference(),
            branch = branch
        )
        .to_string(),
        BTreeMap::from([("branch".to_string(), branch)]),
    ))
}

fn github_client(cx: &mut AsyncApp) -> Result<host::Client> {
    cx.update(|cx| RepoStore::global(cx).read(cx).client(Hosting::GitHub))
        .with_context(|| t!("github_projects.flow.no_github_account"))
}

/// The open pull request from the checked-out branch.
async fn branch_pull_request(
    project: &Entity<Project>,
    client: &host::Client,
    cx: &mut AsyncApp,
) -> Result<Option<PullRequest>> {
    let (repository, _, branch) = cx
        .update(|cx| github_repository(project, cx))
        .with_context(|| t!("github_projects.errors.not_github_repository"))?;
    let branch = branch.with_context(|| t!("github_projects.flow.no_branch_checked_out"))?;
    let pull_requests = client.list_open_pull_requests(&repository).await?;
    Ok(pull_requests
        .into_iter()
        .find(|pull_request| pull_request.source_branch == branch))
}

async fn open_pull_request(
    project: &Entity<Project>,
    request: &GitHubStepRequest,
    cx: &mut AsyncApp,
) -> Result<GitHubStepOutcome> {
    let context = projects_context(project, cx).await?;
    let item = run_item(&context.project, request, context.branch.as_deref()).ok();
    let client = github_client(cx)?;
    let (repository, remote_name, _) = cx
        .update(|cx| github_repository(project, cx))
        .with_context(|| t!("github_projects.errors.not_github_repository"))?;
    let (window, workspace) = cx
        .update(|cx| window_for(project, cx))
        .with_context(|| t!("github_projects.flow.no_window_for_push"))?;
    let workspace = workspace.downgrade();
    let push = window.update(cx, |_, window, cx| {
        repo_hosting::push_current_branch(workspace, project, remote_name, window, cx)
    })?;
    let source_branch = push.await?;

    if let Some(existing) = branch_pull_request(project, &client, cx).await? {
        return Ok(passed(
            t!(
                "github_projects.flow.pull_request_existed",
                number = existing.number,
                branch = source_branch
            )
            .to_string(),
            BTreeMap::from([("pr".to_string(), existing.url)]),
        ));
    }
    let destination = cx
        .update(|cx| {
            context
                .state
                .read(cx)
                .info()
                .and_then(|info| info.default_branch.clone())
        })
        .unwrap_or_else(|| "main".to_string());
    let title = item
        .as_ref()
        .map_or_else(|| source_branch.clone(), |item| item.title.clone());
    let mut description = String::new();
    if let Some(number) = item.as_ref().and_then(|item| item.number) {
        description.push_str(&format!("Closes #{number}\n\n"));
    }
    description.push_str(request.summary.trim());
    let created = client
        .create_pull_request(
            &repository,
            NewPullRequest {
                title,
                description,
                source_branch: source_branch.clone(),
                destination_branch: destination.clone(),
                reviewer_ids: Vec::new(),
                close_source_branch: false,
                draft: request.draft,
            },
        )
        .await?;
    Ok(passed(
        if request.draft {
            t!(
                "github_projects.flow.draft_pull_request_opened",
                number = created.number,
                source = source_branch,
                destination = destination
            )
        } else {
            t!(
                "github_projects.flow.pull_request_opened",
                number = created.number,
                source = source_branch,
                destination = destination
            )
        }
        .to_string(),
        BTreeMap::from([("pr".to_string(), created.url)]),
    ))
}

async fn wait_checks(
    project: &Entity<Project>,
    request: &GitHubStepRequest,
    cx: &mut AsyncApp,
) -> Result<GitHubStepOutcome> {
    let client = github_client(cx)?;
    let (repository, _, _) = cx
        .update(|cx| github_repository(project, cx))
        .with_context(|| t!("github_projects.errors.not_github_repository"))?;
    let pull_request = branch_pull_request(project, &client, cx)
        .await?
        .with_context(|| t!("github_projects.flow.no_open_pull_request"))?;
    let deadline = Instant::now() + Duration::from_secs(u64::from(request.timeout_minutes) * 60);
    let started = Instant::now();
    let mut reran = false;
    loop {
        let checks = client.get_checks(&repository, pull_request.number).await?;
        let summary = CheckSummary::of(&checks);
        let summary_text = t!(
            "github_projects.flow.checks_passed",
            passed = summary.passed,
            total = summary.total
        )
        .to_string();
        if summary.total == 0 {
            if started.elapsed() > CHECKS_APPEAR_TIMEOUT {
                return Ok(passed(
                    t!("github_projects.flow.no_checks").to_string(),
                    BTreeMap::from([(
                        "checks".to_string(),
                        t!("github_projects.flow.no_checks_value").to_string(),
                    )]),
                ));
            }
        } else if summary.running == 0 {
            let failed_checks: Vec<_> = checks
                .iter()
                .filter(|check| check.state == CheckState::Failed)
                .collect();
            if failed_checks.is_empty() {
                return Ok(passed(
                    summary_text.clone(),
                    BTreeMap::from([("checks".to_string(), summary_text)]),
                ));
            }
            let jobs: Vec<u64> = failed_checks
                .iter()
                .filter_map(|check| check.rerun_job)
                .collect();
            if !reran && !jobs.is_empty() {
                for job in &jobs {
                    client.rerun_job(&repository, *job).await?;
                }
                reran = true;
            } else {
                let names: Vec<&str> = failed_checks
                    .iter()
                    .map(|check| check.name.as_str())
                    .collect();
                let log = match jobs.first() {
                    Some(job) => {
                        let log = client
                            .get_job_log(&repository, *job)
                            .await
                            .unwrap_or_default();
                        let start = log
                            .char_indices()
                            .rev()
                            .nth(MAX_LOG_CHARS)
                            .map_or(0, |(index, _)| index);
                        log[start..].to_string()
                    }
                    None => String::new(),
                };
                let mut outputs = BTreeMap::from([("checks".to_string(), summary_text)]);
                if !log.is_empty() {
                    outputs.insert("log do que falhou".to_string(), log.clone());
                }
                let evidence = if log.is_empty() {
                    t!(
                        "github_projects.flow.checks_failed",
                        names = names.join(", ")
                    )
                    .to_string()
                } else {
                    t!(
                        "github_projects.flow.checks_failed_with_log",
                        names = names.join(", "),
                        log = log
                    )
                    .to_string()
                };
                return Ok(failed(evidence, outputs));
            }
        }
        if Instant::now() > deadline {
            return Ok(failed(
                t!(
                    "github_projects.flow.checks_timed_out",
                    minutes = request.timeout_minutes
                )
                .to_string(),
                BTreeMap::new(),
            ));
        }
        cx.background_executor().timer(CHECKS_POLL_INTERVAL).await;
    }
}

async fn move_item(
    project: &Entity<Project>,
    request: &GitHubStepRequest,
    cx: &mut AsyncApp,
) -> Result<GitHubStepOutcome> {
    let context = projects_context(project, cx).await?;
    let item = run_item(&context.project, request, context.branch.as_deref())?;
    let option = context
        .project
        .columns()
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case(request.status.trim()))
        .with_context(|| {
            t!(
                "github_projects.flow.no_such_column",
                column = request.status
            )
        })?
        .clone();
    let moved = context.state.update(cx, |state, cx| {
        state.move_item(item.id.clone(), option.id.clone(), cx)
    });
    moved.await?;
    Ok(passed(
        t!(
            "github_projects.flow.moved",
            item = item.reference(),
            column = option.name
        )
        .to_string(),
        BTreeMap::new(),
    ))
}

async fn comment(
    project: &Entity<Project>,
    request: &GitHubStepRequest,
    cx: &mut AsyncApp,
) -> Result<GitHubStepOutcome> {
    let context = projects_context(project, cx).await?;
    let item = run_item(&context.project, request, context.branch.as_deref())?;
    let text = if request.comment.trim().is_empty() {
        request.summary.trim().to_string()
    } else {
        request.comment.trim().to_string()
    };
    anyhow::ensure!(
        !text.is_empty(),
        "{}",
        t!("github_projects.flow.nothing_to_comment")
    );
    let commented = context
        .state
        .update(cx, |state, cx| state.comment(&item.id, text, cx));
    commented.await?;
    Ok(passed(
        t!("github_projects.flow.commented", item = item.reference()).to_string(),
        BTreeMap::new(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn item(number: u64, branches: &[&str]) -> Item {
        Item {
            id: format!("PVTI_{number}"),
            kind: api::ItemKind::Issue,
            content_id: format!("I_{number}"),
            number: Some(number),
            title: format!("Item {number}"),
            url: None,
            closed: false,
            body: String::new(),
            repository: Some("vini/asylum".to_string()),
            author: None,
            assignees: Vec::new(),
            labels: Vec::new(),
            linked_branches: branches.iter().map(|branch| branch.to_string()).collect(),
            pull_requests: Vec::new(),
            values: HashMap::default(),
        }
    }

    #[test]
    fn finds_the_item_of_the_run() {
        let project = api::Project {
            summary: api::ProjectSummary {
                id: "PVT_1".to_string(),
                title: "Asylum roadmap".to_string(),
                number: 3,
                url: String::new(),
            },
            status: None,
            priority: None,
            iteration: None,
            estimate: None,
            items: vec![item(204, &[]), item(198, &["projects-sync"])],
            rate_limit: api::RateLimit::default(),
        };
        let request = |text: &str| GitHubStepRequest {
            request: text.to_string(),
            ..GitHubStepRequest::default()
        };
        assert_eq!(
            run_item(&project, &request("Fazer o #204, com testes"), None)
                .map(|item| item.number)
                .ok(),
            Some(Some(204))
        );
        assert_eq!(
            run_item(&project, &request("continuar"), Some("projects-sync"))
                .map(|item| item.number)
                .ok(),
            Some(Some(198))
        );
        assert!(run_item(&project, &request("continuar"), Some("main")).is_err());
    }
}
