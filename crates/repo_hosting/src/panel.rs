use crate::{
    Account, Connection, RepoStore, ToggleFocus,
    api::{
        self, CheckState, CheckSummary, FileChange, PullRequest, PullRequestDetail,
        PullRequestState, RemoteRepository,
    },
    api::{MergeOutcome, MergeStrategy, NewPullRequest},
    bitbucket, github, gitlab, host,
    pull_request_changes::open_pull_request_changes,
};
use anyhow::Context as _;
use askpass::AskPassDelegate;
use chrono::{DateTime, Utc};
use editor::Editor;
use git::repository::PushOptions;
use git::repository::{FetchOptions, Remote};
use git_hosting_providers::GitHostingProviderSettings;
use git_ui::{branch_diff::BranchDiff, commit_view::CommitView};
use git_ui_core::askpass_modal::AskPassModal;
use gpui::AsyncApp;
use gpui::{
    Action as _, ClipboardItem, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, Pixels, Stateful, Subscription, Task, WeakEntity, px,
};
use i18n::t;
use project::{
    Project,
    git_store::{GitStoreEvent, Repository},
};
use settings::{GitHostingProviderKind, Settings as _, SettingsStore};
use std::{collections::HashSet, future::Future, time::Duration, time::Instant};
use ui::{
    Checkbox, CommonAnimationExt, ContextMenu, ContextMenuEntry, EmptyState, HeaderBar,
    HeaderBarLevel, KeyBinding, PopoverMenu, PopoverTriggerArea, RowStatus, StatusRow, StatusTone,
    Tooltip, capsule_button, card, checkbox_indicator, grouped_block, initial_tile,
    navigator_footer, picker_button, pill_button, prelude::*, primary_button, right_click_menu,
    secondary_button, section_header, section_label, segment_option, segment_option_with_count,
    segmented_control, select_field, status_pill,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// Bitbucket allows 1,000 repository requests an hour; a list and one checks call every 90
/// seconds stays far below it with several windows open.
const SYNC_INTERVAL: Duration = Duration::from_secs(90);
/// Longer descriptions are cut here until "Mostrar mais" is clicked.
const DESCRIPTION_PREVIEW_CHARS: usize = 280;

/// What the connect screen found in a host's CLI.
enum CliAccount {
    NotChecked,
    Checking,
    Found(Account),
    Missing(SharedString),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cli {
    Gh,
    Glab,
}

impl Cli {
    fn hosting(self) -> api::Hosting {
        match self {
            Cli::Gh => api::Hosting::GitHub,
            Cli::Glab => api::Hosting::GitLab,
        }
    }
}

/// What the project's active repository is, as far as the panel is concerned.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Target {
    NoRepository,
    /// A remote the panel can't talk to yet, or none at all.
    Unsupported {
        remote_url: Option<String>,
    },
    Hosted {
        repository: RemoteRepository,
        remote_name: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    AwaitingYourReview,
    Yours,
    Others,
}

impl Group {
    fn title(self) -> SharedString {
        match self {
            Group::AwaitingYourReview => t!("repo_hosting.list.group_awaiting_review"),
            Group::Yours => t!("repo_hosting.list.group_yours"),
            Group::Others => t!("repo_hosting.list.group_others"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Group::AwaitingYourReview => "awaiting-your-review",
            Group::Yours => "yours",
            Group::Others => "others",
        }
    }

    fn color(self, cx: &App) -> Hsla {
        match self {
            Group::AwaitingYourReview => Color::Info.color(cx),
            Group::Yours => Color::Accent.color(cx),
            Group::Others => Color::Muted.color(cx),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListTab {
    PullRequests,
    Issues,
    Pipelines,
}

/// What the list's other tabs show, loaded when they are first opened.
struct Loadable<T> {
    value: Option<T>,
    loading: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Self {
            value: None,
            loading: false,
            error: None,
            task: None,
        }
    }
}

/// The open pull requests of the target repository, as of the last sync.
#[derive(Default)]
struct PullRequests {
    list: Vec<PullRequest>,
    /// The checks of the checked-out branch's pull request, by number.
    branch_checks: Option<(u64, Vec<api::Check>)>,
    synced_at: Option<Instant>,
    is_syncing: bool,
    /// Why the last sync failed; the pull requests from the sync before it stay visible.
    error: Option<SharedString>,
}

/// What to do once a pull request asked for by number is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestFollowup {
    Open,
    Checkout,
    Diff,
}

enum PendingAction {
    Checkout,
    Approval,
    /// Fetching a commit that isn't in the local repository yet, to open it.
    OpeningCommit,
    Declining,
}

/// The merge card that replaces the approvals card while a merge is being set up.
struct MergeForm {
    strategies: Vec<MergeStrategy>,
    strategy: Option<MergeStrategy>,
    message_editor: Entity<Editor>,
    close_source_branch: bool,
    /// Switch to the destination, pull it and delete the local branch once merged.
    clean_up_local: bool,
    /// GitHub only: the repository lets pull requests merge themselves once allowed.
    allows_auto_merge: bool,
    /// Ask GitHub to merge when the checks and reviews pass, instead of merging now.
    auto_merge: bool,
    submitting: bool,
    progress: Option<SharedString>,
    _load_task: Task<()>,
}

/// A pull request opened from the list. The row it came from shows right away while the rest
/// loads.
struct OpenPullRequest {
    summary: PullRequest,
    detail: Option<PullRequestDetail>,
    error: Option<SharedString>,
    comment_input: Entity<InputField>,
    posting_comment: bool,
    description_expanded: bool,
    merging: Option<MergeForm>,
    /// A success worth saying, such as a merge whose local cleanup also went through.
    notice: Option<SharedString>,
    pending_action: Option<PendingAction>,
    load_task: Option<Task<()>>,
    /// Actions jobs asked to run again, until the reload shows them running.
    rerunning_jobs: HashSet<u64>,
}

/// What the local checkout says about the branch a new pull request would come from.
struct LocalBranchStatus {
    local_name: String,
    /// No upstream on the target remote yet, or it was deleted there: pushing publishes it.
    needs_upstream: bool,
    ahead: u32,
    behind: u32,
    /// Tracking a branch on another remote, which is a fork's pull request.
    tracks_other_remote: bool,
    has_uncommitted_changes: bool,
    head_subject: Option<String>,
}

/// The "Novo pull request" form, shown in place of the list.
struct CreateForm {
    source_branch: String,
    destination: Option<String>,
    branches: Vec<String>,
    title_input: Entity<InputField>,
    description_editor: Entity<Editor>,
    reviewers: Vec<api::Person>,
    candidates: Vec<api::Person>,
    close_source_branch: bool,
    draft: bool,
    /// Already on the remote and not on the destination, newest first.
    commits: Vec<api::Commit>,
    commits_loaded: bool,
    /// In the checkout but not on the remote yet, newest first; pushed when the PR is created.
    local_commits: Vec<api::Commit>,
    /// What the PR would change against the destination, from the local checkout.
    diff_summary: Option<DiffSummary>,
    local_loaded: bool,
    /// Title and description are filled from the commits once; after that they are the user's.
    autofilled: bool,
    /// The autofilled title and where it came from, so the hint goes away once it is edited.
    autofilled_title: Option<(String, SharedString)>,
    has_default_reviewers: bool,
    error: Option<SharedString>,
    submitting: bool,
    _load_task: Task<()>,
    commits_task: Option<Task<()>>,
    local_task: Option<Task<()>>,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct DiffSummary {
    added: u64,
    removed: u64,
    files: usize,
}

pub struct RepoPanel {
    focus_handle: FocusHandle,
    store: Entity<RepoStore>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    email_input: Entity<InputField>,
    token_input: Entity<InputField>,
    github_token_input: Entity<InputField>,
    gitlab_token_input: Entity<InputField>,
    /// The account the `gh` CLI is logged into, looked up when a host needs connecting.
    gh_account: CliAccount,
    gh_task: Option<Task<()>>,
    /// The account the `glab` CLI is logged into on the target's GitLab instance.
    glab_account: CliAccount,
    glab_task: Option<Task<()>>,
    /// Whether the Atlassian CLI is installed, looked up when Bitbucket needs connecting: its
    /// login doesn't reach the Bitbucket API, which people tend to assume.
    acli_found: Option<bool>,
    acli_task: Option<Task<()>>,
    connect_error: Option<SharedString>,
    connect_task: Option<Task<()>>,
    /// Set from the account menu: shows the token fields again while the current token keeps
    /// working until a new one replaces it.
    replacing_token: bool,
    target: Target,
    /// The checked-out branch as the remote names it, which is what pull requests refer to.
    branch: Option<String>,
    pull_requests: PullRequests,
    sync_task: Option<Task<()>>,
    /// Set when syncing stopped because the Repo toolkit was turned off, so turning it back on
    /// resumes it.
    sync_paused: bool,
    filter_input: Entity<InputField>,
    collapsed_groups: HashSet<Group>,
    open_pull_request: Option<OpenPullRequest>,
    creating: Option<CreateForm>,
    list_tab: ListTab,
    /// Which pull requests the list shows; only the open ones are kept in sync.
    state_filter: PullRequestState,
    closed_pull_requests: Loadable<(PullRequestState, Vec<PullRequest>)>,
    pipelines: Loadable<Vec<api::Pipeline>>,
    /// `None` inside when the repository has no issue tracker.
    issues: Loadable<Option<Vec<api::Issue>>>,
    _subscriptions: Vec<Subscription>,
}

impl RepoPanel {
    pub(crate) fn new(
        store: Entity<RepoStore>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let email_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("repo_hosting.connect.email_placeholder"))
                .start_icon(IconName::AtSign)
        });
        let token_input = cx.new(|cx| {
            InputField::new(window, cx, "ATATT…")
                .start_icon(IconName::Lock)
                .masked(true)
        });
        let github_token_input = cx.new(|cx| {
            InputField::new(window, cx, "github_pat_…")
                .start_icon(IconName::Lock)
                .masked(true)
        });
        let gitlab_token_input = cx.new(|cx| {
            InputField::new(window, cx, "glpat-…")
                .start_icon(IconName::Lock)
                .masked(true)
        });
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("repo_hosting.list.filter_placeholder"))
                .start_icon(IconName::MagnifyingGlass)
        });

        let mut subscriptions = Vec::new();
        for input in [
            &email_input,
            &token_input,
            &github_token_input,
            &gitlab_token_input,
            &filter_input,
        ] {
            let editor = input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |this, cx| {
                            this.connect_error = None;
                            cx.notify();
                        })
                        .log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        subscriptions.push(cx.observe(&store, |this, _, cx| this.connection_changed(cx)));
        subscriptions.push(cx.observe_global::<task_agents::ActiveTaskAgents>(|_, cx| cx.notify()));
        subscriptions.push(cx.observe_global::<SettingsStore>(|this: &mut Self, cx| {
            if this.sync_paused
                && task_agents::toolkit_enabled(task_agents::toolkit_ids::REPO, &this.project, cx)
            {
                this.sync(cx);
            }
        }));
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryUpdated(_, _, true)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_) => this.refresh_target(cx),
            _ => {}
        }));

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            project,
            workspace,
            email_input,
            token_input,
            github_token_input,
            gitlab_token_input,
            gh_account: CliAccount::NotChecked,
            gh_task: None,
            glab_account: CliAccount::NotChecked,
            glab_task: None,
            acli_found: None,
            acli_task: None,
            connect_error: None,
            connect_task: None,
            replacing_token: false,
            target: Target::NoRepository,
            branch: None,
            pull_requests: PullRequests::default(),
            sync_task: None,
            sync_paused: false,
            filter_input,
            collapsed_groups: HashSet::default(),
            open_pull_request: None,
            creating: None,
            list_tab: ListTab::PullRequests,
            state_filter: PullRequestState::Open,
            closed_pull_requests: Loadable::default(),
            pipelines: Loadable::default(),
            issues: Loadable::default(),
            _subscriptions: subscriptions,
        };
        this.refresh_target(cx);
        this
    }

    /// A client for the target repository's host, when its account is connected.
    fn client(&self, cx: &App) -> Option<host::Client> {
        let Target::Hosted { repository, .. } = &self.target else {
            return None;
        };
        self.store.read(cx).client_for(repository)
    }

    /// The host of the target repository; Bitbucket until one is detected.
    fn hosting(&self) -> api::Hosting {
        match &self.target {
            Target::Hosted { repository, .. } => repository.hosting,
            _ => api::Hosting::Bitbucket,
        }
    }

    fn account(&self, cx: &App) -> Option<Account> {
        let Target::Hosted { repository, .. } = &self.target else {
            return None;
        };
        self.store.read(cx).account_for(repository).cloned()
    }

    /// The GitLab instance `glab` is asked about: the target's, or gitlab.com for other hosts.
    fn gitlab_instance(&self) -> String {
        match &self.target {
            Target::Hosted { repository, .. } if repository.hosting == api::Hosting::GitLab => {
                repository.instance().to_string()
            }
            _ => gitlab::PUBLIC_INSTANCE.to_string(),
        }
    }

    fn token_input(&self, hosting: api::Hosting) -> &Entity<InputField> {
        match hosting {
            api::Hosting::Bitbucket => &self.token_input,
            api::Hosting::GitHub => &self.github_token_input,
            api::Hosting::GitLab => &self.gitlab_token_input,
        }
    }

    fn refresh_target(&mut self, cx: &mut Context<Self>) {
        let (target, branch) = detect_target(&self.project, cx);
        let branch_changed = branch != self.branch;
        self.branch = branch;
        if target != self.target {
            self.target = target;
            // The instance glab was asked about may not be the new target's.
            self.glab_account = CliAccount::NotChecked;
            self.glab_task = None;
            self.check_local_credentials(cx);
            self.pull_requests = PullRequests::default();
            self.open_pull_request = None;
            self.creating = None;
            self.closed_pull_requests = Loadable::default();
            self.pipelines = Loadable::default();
            self.issues = Loadable::default();
            self.sync(cx);
            self.load_tab_data(cx);
        } else if branch_changed {
            // The branch's pull request may be a different one, with other checks.
            self.sync(cx);
        }
        cx.notify();
    }

    fn needs_connecting(&self, cx: &App) -> bool {
        let Target::Hosted { repository, .. } = &self.target else {
            return false;
        };
        self.replacing_token
            || match self.store.read(cx).connection(repository.hosting) {
                Connection::Disconnected | Connection::Rejected => true,
                Connection::Connected(account) => !account.credentials.serves(repository),
                Connection::Loading | Connection::Unreachable { .. } => false,
            }
    }

    /// Looks up the credentials other CLIs keep on this Mac once the connect screen is about to
    /// show.
    fn check_local_credentials(&mut self, cx: &mut Context<Self>) {
        if !self.needs_connecting(cx) {
            return;
        }
        self.check_cli_account(Cli::Gh, cx);
        self.check_cli_account(Cli::Glab, cx);
        self.check_acli(cx);
    }

    fn check_acli(&mut self, cx: &mut Context<Self>) {
        if self.hosting() != api::Hosting::Bitbucket
            || self.acli_found.is_some()
            || self.acli_task.is_some()
        {
            return;
        }
        let probe = cx.background_spawn(async move {
            util::command::new_command("acli")
                .arg("--version")
                .output()
                .await
                .is_ok_and(|output| output.status.success())
        });
        self.acli_task = Some(cx.spawn(async move |this, cx| {
            let found = probe.await;
            this.update(cx, |this, cx| {
                this.acli_found = Some(found);
                this.acli_task = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn cli_account(&self, cli: Cli) -> &CliAccount {
        match cli {
            Cli::Gh => &self.gh_account,
            Cli::Glab => &self.glab_account,
        }
    }

    fn set_cli_account(&mut self, cli: Cli, account: CliAccount, task: Option<Task<()>>) {
        match cli {
            Cli::Gh => {
                self.gh_account = account;
                self.gh_task = task;
            }
            Cli::Glab => {
                self.glab_account = account;
                self.glab_task = task;
            }
        }
    }

    fn check_cli_account(&mut self, cli: Cli, cx: &mut Context<Self>) {
        if !matches!(self.cli_account(cli), CliAccount::NotChecked) {
            return;
        }
        let validation = match cli {
            Cli::Gh => self.store.read(cx).validate_github_cli(cx),
            Cli::Glab => self
                .store
                .read(cx)
                .validate_gitlab_cli(self.gitlab_instance(), cx),
        };
        let task = cx.spawn(async move |this, cx| {
            let result = validation.await;
            this.update(cx, |this, cx| {
                let account = match result {
                    Ok(account) => CliAccount::Found(account),
                    Err(error) => CliAccount::Missing(format!("{error:#}").into()),
                };
                this.set_cli_account(cli, account, None);
                cx.notify();
            })
            .log_err();
        });
        self.set_cli_account(cli, CliAccount::Checking, Some(task));
        cx.notify();
    }

    fn retry_cli_account(&mut self, cli: Cli, cx: &mut Context<Self>) {
        self.set_cli_account(cli, CliAccount::NotChecked, None);
        self.check_local_credentials(cx);
    }

    fn use_cli_account(&mut self, cli: Cli, cx: &mut Context<Self>) {
        let CliAccount::Found(account) = self.cli_account(cli) else {
            return;
        };
        if self.connect_task.is_some() {
            return;
        }
        let account = account.clone();
        let save = self.store.update(cx, |store, cx| store.save(account, cx));
        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| {
                this.connect_task = None;
                match result {
                    Ok(()) => this.replacing_token = false,
                    Err(error) => {
                        log::error!("Repo: falha ao salvar o token no keychain: {error:#}");
                        this.connect_error = Some(t!("repo_hosting.connect.keychain_save_failed"));
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    /// Connects GitHub or GitLab, which only take a token.
    fn connect_with_token(
        &mut self,
        hosting: api::Hosting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connect_task.is_some() {
            return;
        }
        let token = self
            .token_input(hosting)
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if token.is_empty() {
            return;
        }
        let (validation, rejected) = match hosting {
            api::Hosting::GitLab => (
                self.store
                    .read(cx)
                    .validate_gitlab(self.gitlab_instance(), token, cx),
                t!("repo_hosting.gitlab_connect.token_rejected"),
            ),
            api::Hosting::GitHub | api::Hosting::Bitbucket => (
                self.store.read(cx).validate_github(token, cx),
                t!("repo_hosting.github_connect.token_rejected"),
            ),
        };
        self.connect_error = None;
        cx.notify();
        self.connect_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match validation.await {
                Ok(account) => {
                    let saved = this
                        .update(cx, |this, cx| {
                            this.store.update(cx, |store, cx| store.save(account, cx))
                        })
                        .log_err();
                    match saved {
                        Some(saved) => saved.await.map_err(|error| {
                            log::error!("Repo: falha ao salvar o token no keychain: {error:#}");
                            t!("repo_hosting.connect.keychain_save_failed")
                        }),
                        None => return,
                    }
                }
                Err(error) if crate::is_unauthorized(&error) => Err(rejected),
                Err(error) => Err(t!(
                    "repo_hosting.connect.unreachable",
                    host = hosting.name(),
                    error = error
                )),
            };
            this.update_in(cx, |this, window, cx| {
                this.connect_task = None;
                match result {
                    Ok(()) => {
                        this.replacing_token = false;
                        this.token_input(hosting)
                            .update(cx, |input, cx| input.clear(window, cx));
                    }
                    Err(message) => this.connect_error = Some(message),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn connection_changed(&mut self, cx: &mut Context<Self>) {
        self.check_local_credentials(cx);
        let is_connected = self.account(cx).is_some();
        if !is_connected {
            self.sync_task = None;
            self.pull_requests = PullRequests::default();
            self.open_pull_request = None;
            self.creating = None;
        } else if self.sync_task.is_none() {
            self.sync(cx);
        }
        cx.notify();
    }

    /// Syncs now and then every [`SYNC_INTERVAL`].
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            self.sync_task = None;
            return;
        };
        let Some(client) = self.client(cx) else {
            self.sync_task = None;
            return;
        };
        self.sync_paused = false;
        self.pull_requests.is_syncing = true;
        cx.notify();

        self.sync_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(branch) = this.read_with(cx, |this, _| this.branch.clone()) else {
                    return;
                };
                let result = cx
                    .background_spawn({
                        let client = client.clone();
                        let repository = repository.clone();
                        async move {
                            let list = client.list_open_pull_requests(&repository).await?;
                            let branch_number = branch.and_then(|branch| {
                                list.iter()
                                    .find(|pull_request| pull_request.source_branch == branch)
                                    .map(|pull_request| pull_request.number)
                            });
                            let branch_checks = match branch_number {
                                Some(number) => client
                                    .get_checks(&repository, number)
                                    .await
                                    .log_err()
                                    .map(|checks| (number, checks)),
                                None => None,
                            };
                            anyhow::Ok((list, branch_checks))
                        }
                    })
                    .await;

                let keep_syncing = this
                    .update(cx, |this, cx| {
                        let keep_syncing = this.finish_sync(result, cx);
                        if keep_syncing
                            && !task_agents::toolkit_enabled(
                                task_agents::toolkit_ids::REPO,
                                &this.project,
                                cx,
                            )
                        {
                            this.sync_paused = true;
                            return false;
                        }
                        keep_syncing
                    })
                    .unwrap_or(false);
                if !keep_syncing {
                    return;
                }
                cx.background_executor().timer(SYNC_INTERVAL).await;
                this.update(cx, |this, cx| {
                    this.pull_requests.is_syncing = true;
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    /// Returns whether syncing should go on.
    fn finish_sync(
        &mut self,
        result: anyhow::Result<(Vec<PullRequest>, Option<(u64, Vec<api::Check>)>)>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pull_requests.is_syncing = false;
        let keep_syncing = match result {
            Ok((list, branch_checks)) => {
                if matches!(self.list_tab, ListTab::Pipelines | ListTab::Issues) {
                    self.load_tab_data(cx);
                }
                if let Target::Hosted { repository, .. } = &self.target {
                    let repository = repository.clone();
                    let list = list.clone();
                    self.store
                        .update(cx, |store, _| store.set_pull_requests(repository, list));
                }
                self.pull_requests.list = list;
                self.pull_requests.branch_checks = branch_checks;
                self.pull_requests.synced_at = Some(Instant::now());
                self.pull_requests.error = None;
                true
            }
            Err(error) if crate::is_unauthorized(&error) => {
                {
                    let hosting = self.hosting();
                    self.store
                        .update(cx, |store, cx| store.mark_rejected(hosting, cx));
                }
                false
            }
            Err(error) => {
                log::warn!("Repo: falha ao sincronizar os pull requests: {error:#}");
                self.pull_requests.error = Some(format!("{error:#}").into());
                true
            }
        };
        cx.notify();
        keep_syncing
    }

    fn connect(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let hosting = self.hosting();
        if hosting != api::Hosting::Bitbucket {
            self.connect_with_token(hosting, window, cx);
            return;
        }
        if self.connect_task.is_some() {
            return;
        }
        let email = self.email_input.read(cx).text(cx).trim().to_string();
        let token = self.token_input.read(cx).text(cx).trim().to_string();
        if email.is_empty() || token.is_empty() {
            return;
        }
        if !email.contains('@') {
            self.connect_error = Some(t!("repo_hosting.connect.use_atlassian_email"));
            cx.notify();
            return;
        }
        let validation = self.store.read(cx).validate_bitbucket(email, token, cx);
        self.connect_error = None;
        cx.notify();

        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = match validation.await {
                Ok(account) => {
                    let saved = this
                        .update(cx, |this, cx| {
                            this.store.update(cx, |store, cx| store.save(account, cx))
                        })
                        .log_err();
                    match saved {
                        Some(saved) => saved.await.map_err(|error| {
                            log::error!("Repo: falha ao salvar o token no keychain: {error:#}");
                            t!("repo_hosting.connect.keychain_save_failed")
                        }),
                        None => return,
                    }
                }
                Err(error) if crate::is_unauthorized(&error) => {
                    Err(t!("repo_hosting.connect.bitbucket_rejected"))
                }
                // Checking the token only reads the account, so a missing scope here is that one.
                Err(error) if crate::is_forbidden(&error) => {
                    Err(t!("repo_hosting.connect.missing_user_scope"))
                }
                Err(error) => Err(t!(
                    "repo_hosting.connect.unreachable",
                    host = "Bitbucket",
                    error = error
                )),
            };
            this.update_in(cx, |this, window, cx| {
                this.connect_task = None;
                match result {
                    Ok(()) => {
                        this.replacing_token = false;
                        this.token_input
                            .update(cx, |input, cx| input.clear(window, cx));
                    }
                    Err(message) => this.connect_error = Some(message),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn start_replacing_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replacing_token = true;
        self.connect_error = None;
        if let Some(account) = self.account(cx)
            && let host::Credentials::Bitbucket(credentials) = account.credentials
        {
            self.email_input.update(cx, |input, cx| {
                input.set_text(&credentials.email, window, cx)
            });
        }
        self.token_input
            .update(cx, |input, cx| input.clear(window, cx));
        window.focus(&self.token_input(self.hosting()).focus_handle(cx), cx);
        self.check_local_credentials(cx);
        cx.notify();
    }

    fn render_connect(
        &self,
        repository: &RemoteRepository,
        show_rejected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let hosting = repository.hosting;
        let needs_email = hosting == api::Hosting::Bitbucket;
        let is_busy = self.connect_task.is_some();
        let token_input = self.token_input(hosting);
        let can_connect = !is_busy
            && !token_input.read(cx).is_empty(cx)
            && (!needs_email || !self.email_input.read(cx).is_empty(cx));
        let profile = paths::current_profile_id();
        let origin = repository.web_url();
        let origin = origin.trim_start_matches("https://");

        let scopes = match hosting {
            api::Hosting::GitHub => github::token_permissions()
                .into_iter()
                .map(|(permission, level)| format!("{permission} ({level})"))
                .collect::<Vec<_>>()
                .join(", "),
            api::Hosting::GitLab => gitlab::TOKEN_SCOPES.join(", "),
            api::Hosting::Bitbucket => bitbucket::TOKEN_SCOPES.join(", "),
        };
        let token_settings_url = match hosting {
            api::Hosting::GitHub => github::TOKEN_SETTINGS_URL.to_string(),
            api::Hosting::GitLab => gitlab::token_settings_url(repository.instance()),
            api::Hosting::Bitbucket => bitbucket::TOKEN_SETTINGS_URL.to_string(),
        };

        let hint: Option<AnyElement> = match &self.connect_error {
            Some(error) => Some(
                Label::new(error.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element(),
            ),
            None if is_busy => Some(
                h_flex()
                    .gap_1p5()
                    .child(spinning_if(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        true,
                    ))
                    .child(
                        Label::new(t!("repo_hosting.connect.verifying", host = hosting.name()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            None => None,
        };

        let provider_card = card(cx)
            .border_color(colors.border_focused)
            .p_3p5()
            .gap_2p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(hosting.name())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(status_pill(
                        t!("repo_hosting.connect.from_origin"),
                        StatusTone::Info,
                        cx,
                    )),
            )
            .when(needs_email, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(t!("repo_hosting.connect.email_label"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(self.email_input.clone()),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                Label::new(t!("repo_hosting.connect.token_label"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new(
                                    "repo-token-settings",
                                    t!("repo_hosting.connect.create_token"),
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .end_icon(
                                    Icon::new(IconName::ArrowUpRight)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(move |_, _, cx| cx.open_url(&token_settings_url)),
                            ),
                    )
                    .child(token_input.clone()),
            )
            .child(
                Label::new(t!("repo_hosting.connect.scopes_note", scopes = scopes))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .children(hint)
            .child(
                primary_button(
                    "repo-connect",
                    None,
                    t!("repo_hosting.connect.connect"),
                    Some(KeyBinding::for_action_in(
                        &menu::Confirm,
                        &self.focus_handle,
                        cx,
                    )),
                    cx,
                )
                .w_full()
                .justify_center()
                .when(!can_connect, |this| this.opacity(0.5))
                .when(can_connect, |this| {
                    this.on_click(
                        cx.listener(|this, _, window, cx| this.connect(&menu::Confirm, window, cx)),
                    )
                }),
            );

        let gh_row = self.render_cli_row(Cli::Gh, hosting, is_busy, cx);
        let gh_lacks_project_scope = matches!(
            &self.gh_account,
            CliAccount::Found(account) if account.lacks_scope("project")
        );
        let glab_row = self.render_cli_row(Cli::Glab, hosting, is_busy, cx);

        v_flex()
            .key_context("RepoConnect")
            .on_action(cx.listener(Self::connect))
            .size_full()
            .child(
                v_flex()
                    .id("repo-connect-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_4()
                    .px_5()
                    .pb_6()
                    .when(show_rejected, |this| {
                        let (title, detail) = match hosting {
                            api::Hosting::GitHub => (
                                t!("repo_hosting.github_connect.rejected_saved_title"),
                                t!("repo_hosting.github_connect.rejected_saved_detail"),
                            ),
                            api::Hosting::GitLab => (
                                t!("repo_hosting.gitlab_connect.rejected_saved_title"),
                                t!("repo_hosting.gitlab_connect.rejected_saved_detail"),
                            ),
                            api::Hosting::Bitbucket => (
                                t!("repo_hosting.connect.bitbucket_rejected_saved_title"),
                                t!("repo_hosting.connect.bitbucket_rejected_saved_detail"),
                            ),
                        };
                        this.child(div().pt_4().child(render_warning(title, detail, cx)))
                    })
                    .child(
                        div().flex_none().child(
                            EmptyState::new(
                                IconName::PullRequest,
                                t!("repo_hosting.connect.title_v2"),
                            )
                            .description(t!(
                                "repo_hosting.connect.origin_v2",
                                origin = origin,
                                profile = profile
                            )),
                        ),
                    )
                    .child(provider_card)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section_header(
                                t!("repo_hosting.connect.found_on_mac"),
                                None,
                            ))
                            .child(grouped_block(cx).child(gh_row).child(glab_row))
                            .when(gh_lacks_project_scope, |this| {
                                this.child(
                                    Label::new(t!(
                                        "repo_hosting.github_connect.missing_project_scope"
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                                )
                            })
                            .when(self.acli_found == Some(true), |this| {
                                this.child(render_warning(
                                    t!("repo_hosting.connect.acli_warning_title"),
                                    t!("repo_hosting.connect.acli_warning_detail"),
                                    cx,
                                ))
                            }),
                    )
                    .when(self.replacing_token, |this| {
                        this.child(
                            h_flex().justify_center().child(
                                secondary_button(
                                    "repo-cancel-replace",
                                    None,
                                    match hosting {
                                        api::Hosting::GitHub | api::Hosting::GitLab => {
                                            t!("repo_hosting.connect.keep_current_account")
                                        }
                                        api::Hosting::Bitbucket => {
                                            t!("repo_hosting.connect.keep_current_token")
                                        }
                                    },
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.replacing_token = false;
                                        this.connect_error = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                        )
                    }),
            )
            .child(
                HeaderBar::footer("repo-connect-footer")
                    .level(HeaderBarLevel::Content)
                    .wrapping(true)
                    .start_child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div().py_2().child(
                            Label::new(t!("repo_hosting.connect.footer_v2", profile = profile))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// A CLI's row in the "found on this Mac" list; its account can only be used for the
    /// CLI's own host.
    fn render_cli_row(
        &self,
        cli: Cli,
        hosting: api::Hosting,
        is_busy: bool,
        cx: &mut Context<Self>,
    ) -> StatusRow {
        let (row_id, use_id, retry_id, label) = match cli {
            Cli::Gh => (
                "repo-gh-row",
                "repo-use-gh",
                "repo-retry-gh",
                t!("repo_hosting.connect.gh_row"),
            ),
            Cli::Glab => (
                "repo-glab-row",
                "repo-use-glab",
                "repo-retry-glab",
                t!("repo_hosting.connect.glab_row"),
            ),
        };
        let can_use = cli.hosting() == hosting && !is_busy;
        match self.cli_account(cli) {
            CliAccount::NotChecked | CliAccount::Checking => {
                StatusRow::new(row_id, RowStatus::Paused, label)
                    .state(t!("repo_hosting.connect.looking"))
            }
            CliAccount::Found(account) => StatusRow::new(row_id, RowStatus::Running, label)
                .detail(account.user.short_name().to_string())
                .end_slot(
                    secondary_button(use_id, None, t!("repo_hosting.connect.use"), can_use, cx)
                        .when(can_use, |this| {
                            this.on_click(
                                cx.listener(move |this, _, _, cx| this.use_cli_account(cli, cx)),
                            )
                        }),
                ),
            CliAccount::Missing(reason) => StatusRow::new(row_id, RowStatus::Failing, label)
                .detail(match cli {
                    Cli::Gh => t!("repo_hosting.github_connect.gh_missing", reason = reason),
                    Cli::Glab => t!("repo_hosting.gitlab_connect.glab_missing", reason = reason),
                })
                .end_slot(
                    secondary_button(
                        retry_id,
                        Some((IconName::RotateCw, Color::Muted)),
                        t!("repo_hosting.github_connect.search_again"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.retry_cli_account(cli, cx))),
                ),
        }
    }

    fn render_centered_message(
        &self,
        icon: IconName,
        title: SharedString,
        detail: Option<SharedString>,
        actions: Option<AnyElement>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base24.px(cx))
            .child(Icon::new(icon).size(IconSize::Medium).color(Color::Muted))
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(actions)
    }

    fn render_loading(&self, title: SharedString, cx: &App) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(DynamicSpacing::Base10.px(cx))
            .child(spinning_if(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
                true,
            ))
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
            .into_any_element()
    }

    fn render_account_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let store = self.store.clone();
        let hosting = self.hosting();
        let manage_tokens_url = match hosting {
            api::Hosting::Bitbucket => bitbucket::TOKEN_SETTINGS_URL.to_string(),
            api::Hosting::GitHub => "https://github.com/settings/tokens".to_string(),
            api::Hosting::GitLab => format!(
                "https://{}/-/user_settings/personal_access_tokens",
                self.gitlab_instance()
            ),
        };
        let replace_label = if self
            .account(cx)
            .is_some_and(|account| account.credentials.is_cli())
        {
            t!("repo_hosting.account.use_token")
        } else {
            t!("repo_hosting.account.replace_token")
        };
        PopoverMenu::new("repo-account-menu")
            .trigger_with_tooltip(
                IconButton::new("repo-account-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text(t!("repo_hosting.account.tooltip", host = hosting.name())),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let store = store.clone();
                let replace_label = replace_label.clone();
                let manage_tokens_url = manage_tokens_url.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(
                        ContextMenuEntry::new(replace_label)
                            .icon(IconName::Pencil)
                            .icon_color(Color::Muted)
                            .handler({
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.start_replacing_token(window, cx)
                                        })
                                        .log_err();
                                }
                            }),
                    )
                    .item(
                        ContextMenuEntry::new(match hosting {
                            api::Hosting::Bitbucket => {
                                t!("repo_hosting.account.manage_tokens_atlassian")
                            }
                            api::Hosting::GitHub => t!("repo_hosting.account.manage_tokens_github"),
                            api::Hosting::GitLab => t!("repo_hosting.account.manage_tokens_gitlab"),
                        })
                        .icon(IconName::ArrowUpRight)
                        .icon_color(Color::Muted)
                        .handler(move |_, cx| cx.open_url(&manage_tokens_url)),
                    )
                    .separator()
                    .item(
                        ContextMenuEntry::new(t!("repo_hosting.account.disconnect"))
                            .icon(IconName::Exit)
                            .icon_color(Color::Error)
                            .handler(move |_, cx| {
                                store
                                    .update(cx, |store, cx| store.disconnect(hosting, cx))
                                    .detach_and_log_err(cx);
                            }),
                    )
                }))
            })
    }

    fn render_context_bar(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let web_url = repository.web_url();
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .child(initial_tile(repository.hosting.name(), cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(repository.full_name())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        Label::new(t!(
                            "repo_hosting.repo.subtitle",
                            host = repository.hosting.name(),
                            account = account.user.short_name()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                    ),
            )
            .child(
                IconButton::new("repo-open-web", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(t!(
                        "repo_hosting.common.open_in",
                        host = host_name(&web_url)
                    )))
                    .on_click(move |_, _, cx| cx.open_url(&web_url)),
            )
            .child(
                IconButton::new("repo-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(self.pull_requests.is_syncing)
                    .tooltip(Tooltip::text(t!("repo_hosting.list.sync_now")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sync(cx);
                        this.load_tab_data(cx);
                    })),
            )
            .child(self.render_account_menu(cx))
    }

    fn branch_pull_request(&self) -> Option<&PullRequest> {
        let branch = self.branch.as_deref()?;
        self.pull_requests
            .list
            .iter()
            .find(|pull_request| pull_request.source_branch == branch)
    }

    fn render_branch_card(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let branch = self.branch.clone()?;
        let card = card(cx)
            .border_color(cx.theme().colors().border_focused.opacity(0.6))
            .mx_2()
            .mb_2()
            .p_2p5()
            .gap_2();
        let header = |destination: Option<&str>, number: Option<u64>| {
            let route = match destination {
                Some(destination) => format!("{branch} → {destination}"),
                None => branch.clone(),
            };
            h_flex()
                .gap_1p5()
                .child(section_label(t!("repo_hosting.branch.header")))
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(route)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .when_some(number, |this, number| {
                    this.child(
                        Label::new(format!("#{number}"))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Placeholder),
                    )
                })
        };

        let Some(pull_request) = self.branch_pull_request() else {
            if self.pull_requests.synced_at.is_none() {
                return None;
            }
            return Some(
                card.child(header(None, None))
                    .child(
                        Label::new(t!("repo_hosting.branch.create_pull_request"))
                            .size(LabelSize::Custom(rems_from_px(13_f32)))
                            .weight(FontWeight::BOLD),
                    )
                    .child(
                        h_flex().child(
                            primary_button(
                                "repo-branch-create",
                                Some(IconName::Plus),
                                t!("repo_hosting.common.create_pull_request"),
                                None,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_creating(window, cx)),
                            ),
                        ),
                    )
                    .into_any_element(),
            );
        };

        let checks = self
            .pull_requests
            .branch_checks
            .as_ref()
            .filter(|(number, _)| *number == pull_request.number)
            .map(|(_, checks)| CheckSummary::of(checks))
            .filter(|summary| summary.total > 0);
        let reviewer_count = pull_request.reviewers().count();
        let approvals = pull_request.approval_count();
        let (state_label, state_tone) = state_tone(pull_request);
        let opened = pull_request.clone();
        let review = mention_action(pull_request, None, None, Some(REVIEW_PROMPT));

        Some(
            card.child(header(
                Some(&pull_request.destination_branch),
                Some(pull_request.number),
            ))
            .child(
                Label::new(pull_request.title.clone())
                    .size(LabelSize::Custom(rems_from_px(13_f32)))
                    .weight(FontWeight::BOLD),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .child(status_pill(state_label, state_tone, cx))
                    .when_some(checks, |this, summary| {
                        this.child(status_pill(
                            t!(
                                "repo_hosting.branch.checks_pill",
                                passed = summary.passed,
                                total = summary.total
                            ),
                            check_summary_tone(summary),
                            cx,
                        ))
                    })
                    .child(status_pill(
                        approval_label(approvals, reviewer_count),
                        approval_tone(approvals, reviewer_count),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        primary_button(
                            "repo-branch-open",
                            None,
                            t!("repo_hosting.branch.open_pull_request"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_pull_request(opened.clone(), false, window, cx)
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            "repo-branch-review",
                            t!("repo_hosting.common.review_with_agent"),
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ZedAgent).size(IconSize::XSmall))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(review.boxed_clone(), cx)
                        }),
                    )
                    .child(div().flex_1())
                    .when(pull_request.comment_count > 0, |this| {
                        this.child(comment_count_meta(pull_request.comment_count))
                    }),
            )
            .into_any_element(),
        )
    }

    fn render_row(
        &self,
        pull_request: &PullRequest,
        dot: Hsla,
        highlight_recent: bool,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let opened = pull_request.clone();
        let group = SharedString::from(format!("repo-pr-row-{}", pull_request.number));
        let mention = mention_action(pull_request, None, None, None);
        let is_branch = self.branch.as_deref() == Some(pull_request.source_branch.as_str());
        // Only the checked-out branch's pull request has its checks in the list data.
        let checks = self
            .pull_requests
            .branch_checks
            .as_ref()
            .filter(|(number, _)| is_branch && *number == pull_request.number)
            .map(|(_, checks)| CheckSummary::of(checks))
            .filter(|summary| summary.total > 0);
        let age = pull_request
            .updated_at
            .map(|updated_at| compact_age(updated_at, now));
        let is_recent = pull_request
            .updated_at
            .is_some_and(|updated_at| (now - updated_at).num_hours() < 24);
        let author = if self
            .account(cx)
            .is_some_and(|account| account.user.id == pull_request.author.id)
        {
            t!("repo_hosting.list.you").to_string()
        } else {
            pull_request.author.short_name().to_string()
        };
        let meta = h_flex()
            .flex_none()
            .gap_1p5()
            .when(pull_request.draft, |this| {
                this.child(
                    Label::new(t!("repo_hosting.common.draft"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when_some(checks, |this, summary| {
                this.child(check_summary_tone(summary).dot(cx))
                    .when(summary.running > 0, |this| {
                        this.child(spinning_if(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            true,
                        ))
                    })
            })
            .map(|this| {
                if pull_request.comment_count > 0 {
                    this.child(comment_count_meta(pull_request.comment_count))
                } else if let Some(age) = age {
                    this.child(Label::new(age).size(LabelSize::XSmall).color(
                        if is_recent && highlight_recent {
                            Color::Warning
                        } else {
                            Color::Muted
                        },
                    ))
                } else {
                    this
                }
            })
            .child(
                IconButton::new(
                    SharedString::from(format!("repo-pr-mention-{}", pull_request.number)),
                    IconName::AtSign,
                )
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .visible_on_hover(group.clone())
                .tooltip(Tooltip::text(t!("repo_hosting.common.mention_in_agent")))
                .on_click(move |_, window, cx| window.dispatch_action(mention.boxed_clone(), cx)),
            );

        let row = StatusRow::new(
            SharedString::from(format!("repo-pr-{}", pull_request.number)),
            RowStatus::Running,
            pull_request.title.clone(),
        )
        .dot_color(dot)
        .group(group)
        .height(px(42.))
        .detail(format!(
            "#{} {author} → {}",
            pull_request.number, pull_request.destination_branch
        ))
        .selected(is_branch)
        .end_slot(meta)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_pull_request(opened.clone(), false, window, cx)
        }));

        let menu_pull_request = pull_request.clone();
        let this = cx.weak_entity();
        right_click_menu(SharedString::from(format!(
            "repo-pr-menu-{}",
            pull_request.number
        )))
        .trigger(move |_, _, _| row)
        .menu(move |window, cx| {
            let pull_request = menu_pull_request.clone();
            let this = this.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                pull_request_menu(menu, &pull_request, this)
            })
        })
    }

    fn render_group_header(
        &self,
        group: Group,
        count: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_collapsed = self.collapsed_groups.contains(&group);
        h_flex()
            .id(SharedString::from(format!("repo-group-{}", group.id())))
            .gap_1p5()
            .px_1p5()
            .py_1()
            .mt_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&group) {
                    this.collapsed_groups.insert(group);
                }
                cx.notify();
            }))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(section_label(group.title()))
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Placeholder),
            )
    }

    fn render_list(&self, account: &Account, cx: &Context<Self>) -> AnyElement {
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let visible: Vec<&PullRequest> = self
            .pull_requests
            .list
            .iter()
            .filter(|pull_request| pull_request_matches(pull_request, &filter))
            .collect();

        if visible.is_empty() {
            if self.pull_requests.synced_at.is_none() {
                return self.render_loading(t!("repo_hosting.list.loading_pull_requests"), cx);
            }
            let (icon, title) = if filter.is_empty() {
                (IconName::Check, t!("repo_hosting.list.empty"))
            } else {
                (
                    IconName::MagnifyingGlass,
                    t!("repo_hosting.list.empty_filtered"),
                )
            };
            return self
                .render_centered_message(icon, title, None, None, cx)
                .into_any_element();
        }

        let now = Utc::now();
        let mut list = v_flex()
            .id("repo-pr-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base08.px(cx))
            .pb(DynamicSpacing::Base08.px(cx));
        for (group, pull_requests) in group_pull_requests(&visible, &account.user.id) {
            list = list.child(self.render_group_header(group, pull_requests.len(), cx));
            if !self.collapsed_groups.contains(&group) {
                for pull_request in pull_requests {
                    list = list.child(self.render_row(
                        pull_request,
                        group.color(cx),
                        group == Group::Yours,
                        now,
                        cx,
                    ));
                }
            }
        }
        list.into_any_element()
    }

    fn render_connected(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Some(form) = &self.creating {
            return self.render_create_form(repository, form, cx);
        }
        if let Some(open_pull_request) = &self.open_pull_request {
            return self
                .render_detail(repository, account, open_pull_request, cx)
                .into_any_element();
        }
        let sync_label = match self.pull_requests.synced_at {
            None => t!("repo_hosting.list.syncing"),
            Some(synced_at) => match synced_at.elapsed().as_secs() / 60 {
                0 => t!("repo_hosting.list.synced_now"),
                minutes => t!("repo_hosting.list.synced_minutes_ago", minutes = minutes),
            },
        };
        let sync_tone = if self.pull_requests.error.is_some() {
            StatusTone::Error
        } else if self.pull_requests.is_syncing || self.pull_requests.synced_at.is_none() {
            StatusTone::Warning
        } else {
            StatusTone::Success
        };

        v_flex()
            .size_full()
            .child(self.render_context_bar(repository, account, cx))
            .children(self.render_branch_card(cx))
            .child(div().px_2().pb_2().child(self.render_list_tabs(cx)))
            .when(self.list_tab == ListTab::PullRequests, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .px_2()
                        .pb_2()
                        .child(div().flex_1().min_w_0().child(self.filter_input.clone()))
                        .child(self.render_state_filter(cx)),
                )
                .when_some(self.pull_requests.error.clone(), |this, error| {
                    this.child(render_inline_error(
                        t!("repo_hosting.list.sync_failed", error = error).to_string(),
                        cx,
                    ))
                })
                .child(if self.state_filter == PullRequestState::Open {
                    self.render_list(account, cx)
                } else {
                    self.render_closed_list(cx)
                })
            })
            .when(self.list_tab == ListTab::Pipelines, |this| {
                this.child(self.render_pipelines(cx))
            })
            .when(self.list_tab == ListTab::Issues, |this| {
                this.child(self.render_issues(cx))
            })
            .child(
                navigator_footer(
                    Some(sync_tone),
                    sync_label,
                    Some(
                        capsule_button(
                            "repo-new-pr",
                            Some(IconName::Plus),
                            t!("repo_hosting.list.new_pull_request"),
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_creating(window, cx)),
                        )
                        .into_any_element(),
                    ),
                    cx,
                )
                .h(px(48.))
                .border_t_1()
                .border_color(cx.theme().colors().border),
            )
            .into_any_element()
    }
}

impl RepoPanel {
    fn open_pull_request(
        &mut self,
        pull_request: PullRequest,
        focus_composer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_input = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &t!(
                    "repo_hosting.detail.reply_placeholder",
                    number = pull_request.number
                ),
            )
        });
        if focus_composer {
            window.focus(&comment_input.focus_handle(cx), cx);
        }
        self.open_pull_request = Some(OpenPullRequest {
            summary: pull_request,
            detail: None,
            error: None,
            comment_input,
            posting_comment: false,
            description_expanded: false,
            merging: None,
            notice: None,
            pending_action: None,
            load_task: None,
            rerunning_jobs: HashSet::default(),
        });
        self.reload_open_pull_request(cx);
        cx.notify();
    }

    /// Opens a pull request by number, as the agent's cards and mentions ask for it. One the
    /// list doesn't have (merged, declined, or not synced yet) is fetched first.
    pub(crate) fn show_pull_request(
        &mut self,
        number: u64,
        followup: PullRequestFollowup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let known = self
            .open_pull_request
            .as_ref()
            .map(|open_pull_request| &open_pull_request.summary)
            .filter(|pull_request| pull_request.number == number)
            .or_else(|| {
                self.pull_requests
                    .list
                    .iter()
                    .find(|pull_request| pull_request.number == number)
            })
            .cloned();
        if let Some(pull_request) = known {
            if self
                .open_pull_request
                .as_ref()
                .is_none_or(|open_pull_request| open_pull_request.summary.number != number)
            {
                self.open_pull_request(pull_request, false, window, cx);
            }
            self.run_followup(followup, window, cx);
            return;
        }

        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let load = cx.background_spawn(async move {
            client.get_pull_request_detail(&repository, number).await
        });
        cx.spawn_in(window, async move |this, cx| {
            match load.await {
                Ok(detail) => this.update_in(cx, |this, window, cx| {
                    this.open_pull_request(detail.pull_request, false, window, cx);
                    this.run_followup(followup, window, cx);
                })?,
                Err(error) => workspace.update(cx, |workspace, cx| {
                    workspace.show_error(
                        error.context(t!("repo_hosting.detail.open_failed", number = number)),
                        cx,
                    )
                })?,
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn run_followup(
        &mut self,
        followup: PullRequestFollowup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match followup {
            PullRequestFollowup::Open => {}
            PullRequestFollowup::Checkout => self.checkout(window, cx),
            PullRequestFollowup::Diff => self.open_changes_tab(window, cx),
        }
    }

    fn mention_selected(
        &mut self,
        _: &zed_actions::repo_hosting::MentionSelectedPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pull_request = self
            .open_pull_request
            .as_ref()
            .map(|open_pull_request| &open_pull_request.summary)
            .or_else(|| self.branch_pull_request());
        if let Some(pull_request) = pull_request {
            window.dispatch_action(
                mention_action(pull_request, None, None, None).boxed_clone(),
                cx,
            );
        }
    }

    fn close_pull_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_pull_request = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Runs a failed GitHub Actions job again and reloads the checks once GitHub took it.
    fn rerun_check(&mut self, job_id: u64, name: String, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        if !open_pull_request.rerunning_jobs.insert(job_id) {
            return;
        }
        cx.notify();
        let rerun = cx.background_spawn(async move { client.rerun_job(&repository, job_id).await });
        cx.spawn(async move |this, cx| {
            let result = rerun.await;
            this.update(cx, |this, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                match result {
                    Ok(()) => {
                        open_pull_request.notice =
                            Some(t!("repo_hosting.checks.rerun_started", name = name));
                        this.reload_open_pull_request(cx);
                    }
                    Err(error) => {
                        open_pull_request.rerunning_jobs.remove(&job_id);
                        open_pull_request.error = Some(t!(
                            "repo_hosting.checks.rerun_failed",
                            name = name,
                            error = format!("{error:#}")
                        ));
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Board C: each check of the GitHub Actions run, with Re-rodar on the failed ones.
    fn render_checks_section(
        &self,
        open_pull_request: &OpenPullRequest,
        checks: &[api::Check],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let summary = CheckSummary::of(checks);
        let colors = cx.theme().colors();
        v_flex()
            .child(
                section_header(
                    t!("repo_hosting.checks.header"),
                    Some(
                        Label::new(format!("{}/{}", summary.passed, summary.total))
                            .size(LabelSize::XSmall)
                            .color(Color::Custom(check_summary_tone(summary).color(cx)))
                            .into_any_element(),
                    ),
                )
                .pt_3()
                .pb_1(),
            )
            .children(checks.iter().enumerate().map(|(index, check)| {
                let (icon, color) = match check.state {
                    CheckState::Passed => (IconName::Check, Color::Success),
                    CheckState::Failed => (IconName::XCircle, Color::Error),
                    CheckState::Running => (IconName::LoadCircle, Color::Warning),
                    CheckState::Stopped => (IconName::Dash, Color::Muted),
                };
                let url = check.url.clone();
                let rerunning = check
                    .rerun_job
                    .is_some_and(|job| open_pull_request.rerunning_jobs.contains(&job));
                let rerun = (check.state == CheckState::Failed)
                    .then_some(check.rerun_job)
                    .flatten();
                let name = check.name.clone();
                h_flex()
                    .id(("repo-check", index))
                    .h(px(32.))
                    .px_2p5()
                    .gap_2()
                    .rounded_md()
                    .when(check.state == CheckState::Failed, |this| {
                        this.bg(Color::Error.color(cx).opacity(0.1))
                    })
                    .when_some(url, |this, url| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(colors.element_hover))
                            .tooltip(Tooltip::text(t!("repo_hosting.checks.open_log")))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    })
                    .child(spinning_if(
                        Icon::new(icon).size(IconSize::XSmall).color(color),
                        check.state == CheckState::Running,
                    ))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(check.name.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .when_some(rerun, |this, job_id| {
                        this.child(
                            Button::new(
                                ("repo-check-rerun", index),
                                if rerunning {
                                    t!("repo_hosting.checks.rerunning")
                                } else {
                                    t!("repo_hosting.checks.rerun")
                                },
                            )
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Rerun).size(IconSize::XSmall))
                            .disabled(rerunning)
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.rerun_check(job_id, name.clone(), cx),
                            )),
                        )
                    })
                    .when_some(check.updated_at, |this, at| {
                        this.child(
                            Label::new(compact_age(at, Utc::now()))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
            }))
    }

    fn reload_open_pull_request(&mut self, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let number = open_pull_request.summary.number;
        let load = cx.background_spawn(async move {
            client.get_pull_request_detail(&repository, number).await
        });
        open_pull_request.load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                match result {
                    Ok(detail) => {
                        open_pull_request.summary = detail.pull_request.clone();
                        open_pull_request.detail = Some(detail);
                        open_pull_request.error = None;
                        open_pull_request.rerunning_jobs.clear();
                    }
                    Err(error) => {
                        if crate::is_unauthorized(&error) {
                            {
                                let hosting = this.hosting();
                                this.store
                                    .update(cx, |store, cx| store.mark_rejected(hosting, cx));
                            }
                            return;
                        }
                        open_pull_request.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn post_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let text = open_pull_request
            .comment_input
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if text.is_empty() || open_pull_request.posting_comment {
            return;
        }
        let number = open_pull_request.summary.number;
        open_pull_request.posting_comment = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { client.post_comment(&repository, number, &text).await },
                )
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                open_pull_request.posting_comment = false;
                match result {
                    Ok(()) => {
                        open_pull_request
                            .comment_input
                            .update(cx, |input, cx| input.clear(window, cx));
                        this.reload_open_pull_request(cx);
                    }
                    Err(error) => {
                        open_pull_request.error = Some(t!(
                            "repo_hosting.detail.comment_failed",
                            error = format!("{error:#}")
                        ));
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_approval(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        if open_pull_request.pending_action.is_some() {
            return;
        }
        let number = open_pull_request.summary.number;
        open_pull_request.pending_action = Some(PendingAction::Approval);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { client.set_approval(&repository, number, approve).await },
                )
                .await;
            this.update(cx, |this, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                open_pull_request.pending_action = None;
                if let Err(error) = result {
                    let error = format!("{error:#}");
                    open_pull_request.error = Some(if approve {
                        t!("repo_hosting.approval.approve_failed", error = error)
                    } else {
                        t!("repo_hosting.approval.unapprove_failed", error = error)
                    });
                }
                this.reload_open_pull_request(cx);
                this.sync(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    /// Asks for a password the same way the git panel does, in a modal over the workspace.
    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AskPassDelegate {
        askpass_delegate(
            self.workspace.clone(),
            operation,
            window,
            &mut cx.to_async(),
        )
    }

    fn set_pending_action(&mut self, action: Option<PendingAction>, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request {
            open_pull_request.pending_action = action;
            cx.notify();
        }
    }

    fn set_open_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request {
            open_pull_request.error = Some(error.into());
            cx.notify();
        }
    }

    fn is_busy(&self) -> bool {
        self.open_pull_request
            .as_ref()
            .is_some_and(|open_pull_request| open_pull_request.pending_action.is_some())
    }

    /// Fetches the pull request's branch from the remote and switches to it, tracking it.
    fn checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { remote_name, .. } = self.target.clone() else {
            return;
        };
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let branch = open_pull_request.summary.source_branch.clone();
        self.set_pending_action(Some(PendingAction::Checkout), cx);
        let askpass = self.askpass_delegate("git fetch", window, cx);
        cx.spawn(async move |this, cx| {
            let result = async {
                fetch_remote(&repository, &remote_name, askpass, cx).await?;
                let switch = repository.update(cx, |repository, _| {
                    repository.change_branch(format!("{remote_name}/{branch}"))
                });
                switch.await??;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.set_pending_action(None, cx);
                if let Err(error) = result {
                    this.set_open_error(
                        t!(
                            "repo_hosting.detail.checkout_failed",
                            error = format!("{error:#}")
                        )
                        .to_string(),
                        cx,
                    );
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Opens a commit in Zed's commit view. A commit this clone doesn't have yet is fetched
    /// first; one the remote doesn't have either (a fork's, or a force-pushed one) opens on the
    /// web instead.
    fn open_commit(&mut self, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let web_url = format!("{}/commits/{hash}", remote_repository.web_url());
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            cx.open_url(&web_url);
            return;
        };
        if self.is_busy() {
            return;
        }
        self.set_pending_action(Some(PendingAction::OpeningCommit), cx);
        let askpass = self.askpass_delegate("git fetch", window, cx);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut is_local = has_commit(&repository, &hash, cx).await;
            if !is_local {
                match fetch_remote(&repository, &remote_name, askpass, cx).await {
                    Ok(()) => is_local = has_commit(&repository, &hash, cx).await,
                    Err(error) => log::warn!("Repo: fetch para abrir {hash} falhou: {error:#}"),
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.set_pending_action(None, cx);
                if is_local {
                    CommitView::open(
                        hash,
                        repository.downgrade(),
                        workspace,
                        None,
                        None,
                        window,
                        cx,
                    );
                } else {
                    cx.open_url(&web_url);
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn open_changes_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        let pull_request = open_pull_request.summary.clone();
        let detail = open_pull_request.detail.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                open_pull_request_changes(
                    workspace,
                    pull_request,
                    detail,
                    client,
                    repository,
                    window,
                    cx,
                )
            })
            .log_err();
    }

    fn render_commit_row(
        &self,
        commit: &api::Commit,
        unpushed: bool,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let hash = commit.hash.clone();
        let mut meta = vec![commit.author.clone()];
        if let Some(date) = commit.date {
            meta.push(relative_age(date, now));
        }
        meta.retain(|part| !part.is_empty());
        h_flex()
            .id(SharedString::from(format!(
                "repo-pr-commit-{}",
                commit.hash
            )))
            .items_start()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base06.px(cx))
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .tooltip(Tooltip::text(t!("repo_hosting.detail.open_commit")))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_commit(hash.clone(), window, cx)),
            )
            .child(
                h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                    Icon::new(IconName::GitCommit)
                        .size(IconSize::Small)
                        .color(if unpushed {
                            Color::Warning
                        } else {
                            Color::Muted
                        }),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(commit.summary.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Label::new(commit.short_hash().to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new("·")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(meta.join(" · "))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .when(unpushed, |this| {
                                this.child(
                                    Label::new("·")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Disabled),
                                )
                                .child(
                                    Label::new(t!("repo_hosting.create.not_pushed"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            }),
                    ),
            )
    }

    fn render_field(label: SharedString, value: AnyElement) -> impl IntoElement {
        h_flex()
            .items_start()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .w(px(74.))
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(div().flex_1().min_w_0().child(value))
    }

    fn render_detail(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = &open_pull_request.summary;
        let now = Utc::now();
        let (state_label, state_tone) = state_tone(pull_request);
        let author = if pull_request.author.id == account.user.id {
            t!("repo_hosting.list.you").to_string()
        } else {
            pull_request.author.short_name().to_string()
        };
        let author_meta = match pull_request.updated_at {
            Some(updated_at) => t!(
                "repo_hosting.detail.author_age",
                author = author,
                age = relative_age(updated_at, now)
            )
            .to_string(),
            None => author,
        };

        let content = v_flex()
            .id("repo-pr-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pb_3()
            .gap_1()
            .child(
                Label::new(format!(
                    "{} → {}",
                    pull_request.source_branch, pull_request.destination_branch
                ))
                .size(LabelSize::Custom(rems_from_px(11_f32)))
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate(),
            )
            .child(
                Label::new(pull_request.title.clone())
                    .size(LabelSize::Custom(rems_from_px(15_f32)))
                    .weight(FontWeight::BOLD),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .pt_1()
                    .pb_2()
                    .child(status_pill(state_label, state_tone, cx))
                    .children(self.render_task_pill(repository, pull_request, cx))
                    .child(
                        Label::new(author_meta)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    ),
            );
        let content = self.render_overview(content, open_pull_request, now, cx);
        let is_open = pull_request.state == PullRequestState::Open;

        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.close_pull_request(window, cx)
                }),
            )
            .size_full()
            .child(self.render_detail_header(pull_request, cx))
            .when_some(open_pull_request.error.clone(), |this, error| {
                this.child(render_inline_error(error.to_string(), cx))
            })
            .child(content)
            .when_some(open_pull_request.notice.clone(), |this, notice| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .px_3()
                        .py_1p5()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .child(
                            Label::new(notice)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .map(|this| match &open_pull_request.merging {
                Some(form) => this.child(self.render_merge_actions(open_pull_request, form, cx)),
                None => this.child(
                    v_flex()
                        .gap_2()
                        .p_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .when(is_open, |this| {
                            this.child(self.render_merge_strip(account, open_pull_request, cx))
                        })
                        .child(self.render_composer(open_pull_request, cx)),
                ),
            })
    }

    /// The ClickUp task the branch names, or on GitHub the issue the description closes.
    fn render_task_pill(
        &self,
        repository: &RemoteRepository,
        pull_request: &PullRequest,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let (label, url, tooltip) = match clickup_task_id(&pull_request.source_branch) {
            Some(task_id) => {
                let url = format!(
                    "https://app.clickup.com/t/{}",
                    task_id.trim_start_matches("CU-")
                );
                (
                    SharedString::from(task_id),
                    url,
                    t!("repo_hosting.detail.open_task"),
                )
            }
            None => {
                let issue = (self.hosting() == api::Hosting::GitHub)
                    .then(|| closed_issue_number(&pull_request.description))
                    .flatten()?;
                (
                    t!("repo_hosting.detail.closes_issue", issue = issue),
                    format!("{}/issues/{issue}", repository.web_url()),
                    t!("repo_hosting.detail.open_issue"),
                )
            }
        };
        Some(
            status_pill(label, StatusTone::Neutral, cx)
                .id("repo-pr-task")
                .cursor_pointer()
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element(),
        )
    }

    fn render_overview(
        &self,
        content: Stateful<Div>,
        open_pull_request: &OpenPullRequest,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let pull_request = &open_pull_request.summary;
        let detail = open_pull_request.detail.as_ref();
        let loading = || {
            Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };

        let reviewers: Vec<&api::Participant> = pull_request.reviewers().collect();
        let reviewers = if reviewers.is_empty() {
            match detail {
                Some(_) => Label::new(t!("repo_hosting.detail.nobody"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                None => loading(),
            }
        } else {
            v_flex()
                .gap_1p5()
                .children(reviewers.iter().map(|reviewer| {
                    let (status, color) = if reviewer.approved {
                        (t!("repo_hosting.detail.reviewer_approved"), Color::Success)
                    } else if reviewer.requested_changes {
                        (
                            t!("repo_hosting.detail.reviewer_changes_requested"),
                            Color::Error,
                        )
                    } else {
                        (
                            t!("repo_hosting.detail.reviewer_pending"),
                            Color::Placeholder,
                        )
                    };
                    h_flex()
                        .gap_1p5()
                        .child(avatar(&reviewer.person.display_name, Color::Info, cx))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(reviewer.person.short_name().to_string())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            ),
                        )
                        .child(Label::new(status).size(LabelSize::XSmall).color(color))
                }))
                .into_any_element()
        };

        let checks = match detail {
            None => loading(),
            Some(detail) if detail.checks.is_empty() => {
                Label::new(t!("repo_hosting.detail.no_checks"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }
            Some(detail) => {
                let summary = CheckSummary::of(&detail.checks);
                let (icon, text) = if summary.failed > 0 {
                    (
                        IconName::XCircle,
                        t!(
                            "repo_hosting.detail.checks_failed",
                            passed = summary.passed,
                            total = summary.total,
                            failed = summary.failed
                        ),
                    )
                } else if summary.running > 0 {
                    (
                        IconName::LoadCircle,
                        t!(
                            "repo_hosting.detail.checks_running",
                            passed = summary.passed,
                            total = summary.total,
                            running = summary.running
                        ),
                    )
                } else {
                    (
                        IconName::Check,
                        t!(
                            "repo_hosting.detail.checks_passed",
                            passed = summary.passed,
                            total = summary.total
                        ),
                    )
                };
                let color = Color::Custom(check_summary_tone(summary).color(cx));
                h_flex()
                    .gap_1p5()
                    .child(spinning_if(
                        Icon::new(icon).size(IconSize::XSmall).color(color),
                        summary.running > 0,
                    ))
                    .child(Label::new(text).size(LabelSize::Small).color(color))
                    .into_any_element()
            }
        };

        let changes = match detail {
            None => loading(),
            Some(detail) => self.render_changes(detail, cx),
        };

        let job_checks = matches!(self.hosting(), api::Hosting::GitHub | api::Hosting::GitLab)
            && detail.is_some_and(|detail| !detail.checks.is_empty());

        let mut content = content
            .child(
                grouped_block(cx)
                    .child(Self::render_field(
                        t!("repo_hosting.detail.field_reviewers"),
                        reviewers,
                    ))
                    .when(!job_checks, |this| {
                        this.child(Self::render_field(
                            t!("repo_hosting.detail.field_checks"),
                            checks,
                        ))
                    })
                    .child(Self::render_field(
                        t!("repo_hosting.detail.field_changes"),
                        changes,
                    )),
            )
            .when_some(detail.filter(|_| job_checks), |this, detail| {
                this.child(self.render_checks_section(open_pull_request, &detail.checks, cx))
            })
            .when_some(open_pull_request.merging.as_ref(), |this, form| {
                this.child(div().h_2())
                    .child(self.render_merge_card(open_pull_request, form, cx))
            });

        let description = pull_request.description.trim().to_string();
        content = content.child(
            section_header(t!("repo_hosting.detail.description_header"), None)
                .pt_3()
                .pb_1(),
        );
        content = if description.is_empty() {
            content.child(
                Label::new(t!("repo_hosting.detail.no_description"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        } else {
            let is_long = description.chars().count() > DESCRIPTION_PREVIEW_CHARS;
            let shown = if is_long && !open_pull_request.description_expanded {
                let preview: String = description
                    .chars()
                    .take(DESCRIPTION_PREVIEW_CHARS)
                    .collect();
                format!("{}…", preview.trim_end())
            } else {
                description
            };
            content
                .child(Label::new(shown).size(LabelSize::Small).color(Color::Muted))
                .when(is_long, |this| {
                    let expanded = open_pull_request.description_expanded;
                    this.child(
                        h_flex().child(
                            Button::new(
                                "repo-pr-description-toggle",
                                if expanded {
                                    t!("repo_hosting.detail.show_less")
                                } else {
                                    t!("repo_hosting.detail.show_more")
                                },
                            )
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .color(Color::Accent)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(open_pull_request) = &mut this.open_pull_request {
                                    open_pull_request.description_expanded =
                                        !open_pull_request.description_expanded;
                                    cx.notify();
                                }
                            })),
                        ),
                    )
                })
        };

        let comments = detail
            .map(|detail| detail.comments.as_slice())
            .unwrap_or_default();
        let events = detail
            .map(|detail| activity_events(&detail.pull_request, &detail.checks, now))
            .unwrap_or_default();
        content = content.child(
            section_header(
                t!(
                    "repo_hosting.detail.activity_header",
                    count = comments.len() + events.len()
                ),
                None,
            )
            .pt_3()
            .pb_1(),
        );
        if comments.is_empty() && events.is_empty() {
            content = content.child(
                Label::new(if detail.is_some() {
                    t!("repo_hosting.detail.no_activity")
                } else {
                    SharedString::from("…")
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }
        for comment in comments {
            content = content.child(render_comment(comment, pull_request, now, cx));
        }
        for (index, event) in events.into_iter().enumerate() {
            content = content.child(
                h_flex()
                    .id(SharedString::from(format!("repo-pr-event-{index}")))
                    .gap_2()
                    .py_0p5()
                    .pl_1p5()
                    .when_some(event.url, |this, url| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                            .tooltip(Tooltip::text(t!("repo_hosting.common.open_in_browser")))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    })
                    .child(div().size(px(4.)).rounded_full().bg(event.color.color(cx)))
                    .child(
                        Label::new(event.text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            );
        }
        content
    }

    fn render_changes(&self, detail: &PullRequestDetail, cx: &Context<Self>) -> AnyElement {
        let added: u64 = detail.files.iter().map(|file| file.lines_added).sum();
        let removed: u64 = detail.files.iter().map(|file| file.lines_removed).sum();
        let text = if detail.files.len() == 1 {
            t!(
                "repo_hosting.detail.changes_files.one",
                added = added,
                removed = removed
            )
        } else {
            t!(
                "repo_hosting.detail.changes_files.other",
                count = detail.files.len(),
                added = added,
                removed = removed
            )
        };
        h_flex()
            .id("repo-pr-changes")
            .gap_1p5()
            .cursor_pointer()
            .tooltip(Tooltip::text(t!("repo_hosting.detail.diff_open")))
            .on_click(cx.listener(|this, _, window, cx| this.open_changes_tab(window, cx)))
            .child(Label::new(text).size(LabelSize::Small).color(Color::Accent))
            .child(
                Icon::new(IconName::DiffSplit)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_detail_header(
        &self,
        pull_request: &PullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let menu_pull_request = pull_request.clone();
        let panel = cx.weak_entity();
        h_flex()
            .gap_1()
            .px_2()
            .py_1p5()
            .child(
                div().flex_1().min_w_0().child(
                    Button::new("repo-pr-back", t!("repo_hosting.list.tab_pull_requests"))
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_pull_request(window, cx)),
                        ),
                ),
            )
            .child(
                Label::new(format!("#{}", pull_request.number))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Placeholder),
            )
            .children(self.render_task_agent_buttons(pull_request, &pull_request.url, cx))
            .child(
                PopoverMenu::new("repo-pr-more")
                    .trigger_with_tooltip(
                        IconButton::new("repo-pr-more-button", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted),
                        Tooltip::text(t!("repo_hosting.detail.more_actions")),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let pull_request = menu_pull_request.clone();
                        let panel = panel.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            pull_request_menu(menu, &pull_request, panel)
                        }))
                    }),
            )
    }

    /// The task agents that asked for a button here (their "botão no dock Repo" trigger); each
    /// starts the agent with this pull request as the request.
    fn render_task_agent_buttons(
        &self,
        pull_request: &PullRequest,
        url: &str,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(active) = cx.try_global::<task_agents::ActiveTaskAgents>() else {
            return Vec::new();
        };
        let prompt = format!(
            "PR #{} — {} ({url})",
            pull_request.number, pull_request.title
        );
        active
            .agents
            .iter()
            .filter(|agent| agent.enabled && agent.triggers.repo_dock)
            .enumerate()
            .map(|(index, agent)| {
                let command = agent.command.clone();
                let prompt = prompt.clone();
                Button::new(("repo-pr-task-agent", index), agent.name.clone())
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::XSmall)
                    .start_icon(Icon::new(IconName::PlayOutlined).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(t!(
                        "repo_hosting.common.run_task_agent",
                        command = command
                    )))
                    .on_click(move |_, window, cx| {
                        match cx.build_action(
                            "task_agents::RunTaskAgent",
                            Some(serde_json::json!({ "command": command, "prompt": prompt })),
                        ) {
                            Ok(action) => window.dispatch_action(action, cx),
                            Err(error) => log::error!("couldn't start the task agent: {error}"),
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_merge_strip(
        &self,
        account: &Account,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = &open_pull_request.summary;
        let gates = merge_gates(pull_request, open_pull_request.detail.as_ref());
        // `max_by_key` keeps the last of equals; reversed, that is the first gate listed.
        let worst = gates
            .iter()
            .rev()
            .filter(|gate| gate.level != GateLevel::Ok)
            .max_by_key(|gate| gate.level);
        let unapproved: Vec<&str> = pull_request
            .reviewers()
            .filter(|reviewer| !reviewer.approved)
            .map(|reviewer| reviewer.person.short_name())
            .collect();
        let message = worst.map(|gate| {
            if gate.kind == GateKind::Approvals && !unapproved.is_empty() {
                t!(
                    "repo_hosting.merge.missing_approval",
                    names = unapproved.join(", ")
                )
            } else {
                t!("repo_hosting.merge.blocked_by", reason = gate.text)
            }
        });
        let is_pending = open_pull_request.pending_action.is_some();
        let is_author = pull_request.author.id == account.user.id;
        let approved_by_me = pull_request.has_approval_from(&account.user.id);

        let approve = (!is_author).then(|| {
            secondary_button(
                "repo-pr-approve",
                Some((
                    IconName::Check,
                    if approved_by_me {
                        Color::Success
                    } else {
                        Color::Muted
                    },
                )),
                if approved_by_me {
                    t!("repo_hosting.approval.approved")
                } else {
                    t!("repo_hosting.approval.approve")
                },
                !is_pending,
                cx,
            )
            .when(approved_by_me, |this| {
                this.tooltip(Tooltip::text(t!("repo_hosting.approval.remove_tooltip")))
            })
            .when(!is_pending, |this| {
                this.on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_approval(!approved_by_me, cx)),
                )
            })
        });
        let merge_enabled = !is_pending && message.is_none();
        let merge = match &message {
            None => primary_button(
                "repo-pr-merge",
                Some(IconName::Check),
                t!("repo_hosting.merge.button"),
                None,
                cx,
            )
            .when(is_pending, |this| this.opacity(0.5).cursor_default()),
            Some(message) => secondary_button(
                "repo-pr-merge",
                None,
                t!("repo_hosting.merge.button"),
                merge_enabled,
                cx,
            )
            .tooltip(Tooltip::text(message.clone())),
        }
        .when(merge_enabled, |this| {
            this.on_click(cx.listener(|this, _, window, cx| this.start_merging(window, cx)))
        });

        match message {
            Some(message) => warning_box(cx)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(message).size(LabelSize::Small)),
                )
                .children(approve)
                .child(merge),
            None => h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1())
                .children(approve)
                .child(merge),
        }
    }

    fn render_composer(
        &self,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let posting = open_pull_request.posting_comment;
        let focus_handle = open_pull_request.comment_input.focus_handle(cx);
        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.post_comment(window, cx)),
            )
            .gap_1p5()
            .child(open_pull_request.comment_input.clone())
            .child(
                h_flex().justify_end().child(
                    primary_button(
                        "repo-pr-comment",
                        Some(IconName::Send),
                        t!("repo_hosting.common.comment"),
                        Some(KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)),
                        cx,
                    )
                    .when(posting, |this| this.opacity(0.5).cursor_default())
                    .when(!posting, |this| {
                        this.on_click(
                            cx.listener(|this, _, window, cx| this.post_comment(window, cx)),
                        )
                    }),
                ),
            )
    }
}

impl RepoPanel {
    fn local_branch_status(&self, cx: &App) -> Option<LocalBranchStatus> {
        let Target::Hosted { remote_name, .. } = &self.target else {
            return None;
        };
        let repository = self.project.read(cx).active_repository(cx)?;
        let repository = repository.read(cx);
        let branch = repository.branch.as_ref()?;
        let mut status = LocalBranchStatus {
            local_name: branch.name().to_string(),
            needs_upstream: true,
            ahead: 0,
            behind: 0,
            tracks_other_remote: false,
            has_uncommitted_changes: !repository.statuses_by_path.is_empty(),
            head_subject: repository
                .head_commit
                .as_ref()
                .and_then(|commit| commit.message.lines().next().map(str::to_string)),
        };
        if let Some(upstream) = &branch.upstream {
            if upstream.remote_name() != Some(remote_name.as_str()) {
                status.tracks_other_remote = true;
            } else if let Some(tracking) = upstream.tracking.status() {
                status.needs_upstream = false;
                status.ahead = tracking.ahead;
                status.behind = tracking.behind;
            }
        }
        Some(status)
    }

    fn start_creating(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(source_branch) = self.branch.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let my_id = self.account(cx).map(|account| account.user.id);

        let title_input =
            cx.new(|cx| InputField::new(window, cx, &t!("repo_hosting.create.title_placeholder")));
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 14, window, cx);
            editor.set_placeholder_text(
                &t!("repo_hosting.create.description_placeholder"),
                window,
                cx,
            );
            editor
        });
        window.focus(&title_input.focus_handle(cx), cx);

        // People already reviewing or authoring here are who you'd most likely ask; listing
        // the workspace's members needs a scope the token doesn't have.
        let mut candidates: Vec<api::Person> = Vec::new();
        for pull_request in &self.pull_requests.list {
            let people = std::iter::once(&pull_request.author).chain(
                pull_request
                    .participants
                    .iter()
                    .map(|participant| &participant.person),
            );
            for person in people {
                add_person(&mut candidates, person, my_id.as_deref());
            }
        }

        let load = cx.background_spawn(async move {
            let destination = client.get_default_destination(&repository);
            let branches = client.list_branches(&repository);
            let reviewers = client.get_default_reviewers(&repository);
            futures::join!(destination, branches, reviewers)
        });
        let load_task = cx.spawn_in(window, async move |this, cx| {
            let (destination, branches, reviewers) = load.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.creating else {
                    return;
                };
                let branches = branches
                    .context("Repo: falha ao listar os branches")
                    .log_err()
                    .unwrap_or_default();
                form.destination = destination
                    .context("Repo: falha ao achar o branch de destino padrão")
                    .log_err()
                    .flatten()
                    .or_else(|| {
                        ["develop", "main", "master"]
                            .into_iter()
                            .find(|name| branches.iter().any(|branch| branch == name))
                            .map(str::to_string)
                    });
                form.branches = branches;
                // Default reviewers need a repository admin to have set them; without them
                // (or without permission to read them) the form just starts empty.
                let reviewers = reviewers
                    .context("Repo: revisores padrão indisponíveis")
                    .log_err()
                    .unwrap_or_default();
                form.has_default_reviewers = !reviewers.is_empty();
                for reviewer in reviewers {
                    add_person(&mut form.candidates, &reviewer, my_id.as_deref());
                    if Some(reviewer.id.as_str()) != my_id.as_deref() {
                        add_person(&mut form.reviewers, &reviewer, None);
                    }
                }
                this.reload_create_commits(window, cx);
                cx.notify();
            })
            .log_err();
        });

        self.open_pull_request = None;
        self.creating = Some(CreateForm {
            source_branch,
            destination: None,
            branches: Vec::new(),
            title_input,
            description_editor,
            reviewers: Vec::new(),
            candidates,
            close_source_branch: true,
            draft: false,
            commits: Vec::new(),
            commits_loaded: false,
            local_commits: Vec::new(),
            diff_summary: None,
            local_loaded: false,
            autofilled: false,
            autofilled_title: None,
            has_default_reviewers: false,
            error: None,
            submitting: false,
            _load_task: load_task,
            commits_task: None,
            local_task: None,
        });
        cx.notify();
    }

    fn close_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn set_destination(
        &mut self,
        destination: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.creating else {
            return;
        };
        form.destination = Some(destination);
        form.error = None;
        self.reload_create_commits(window, cx);
        cx.notify();
    }

    /// The commits the remote already has, then the title and description made from them.
    fn reload_create_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let needs_upstream = self
            .local_branch_status(cx)
            .is_none_or(|status| status.needs_upstream);
        self.reload_local_commits(window, cx);
        let Some(form) = &mut self.creating else {
            return;
        };
        let Some(destination) = form.destination.clone() else {
            form.commits_loaded = true;
            self.autofill(window, cx);
            return;
        };
        if needs_upstream {
            form.commits.clear();
            form.commits_loaded = true;
            self.autofill(window, cx);
            return;
        }
        let source = form.source_branch.clone();
        form.commits_loaded = false;
        let load = cx.background_spawn(async move {
            client
                .list_commits_between(&repository, &source, &destination)
                .await
        });
        form.commits_task = Some(cx.spawn_in(window, async move |this, cx| {
            let commits = load.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.creating else {
                    return;
                };
                form.commits = commits
                    .context("Repo: falha ao listar os commits do branch")
                    .log_err()
                    .unwrap_or_default();
                form.commits_loaded = true;
                this.autofill(window, cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// The commits only the checkout has and the diff against the destination, read with the
    /// git CLI because the remote can't see either before the push.
    fn reload_local_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { remote_name, .. } = self.target.clone() else {
            return;
        };
        let status = self.local_branch_status(cx);
        let work_directory = self
            .project
            .read(cx)
            .active_repository(cx)
            .map(|repository| repository.read(cx).work_directory_abs_path.clone());
        let Some(form) = &mut self.creating else {
            return;
        };
        let destination = form.destination.clone();
        let (Some(work_directory), Some(destination), Some(status)) =
            (work_directory, destination, status)
        else {
            form.local_commits.clear();
            form.diff_summary = None;
            form.local_loaded = true;
            form.local_task = None;
            return;
        };
        let base_ref = format!("{remote_name}/{destination}");
        // Before the first push everything the destination lacks is local; after it, only
        // what is ahead of the upstream.
        let unpushed_range = if status.needs_upstream || status.tracks_other_remote {
            format!("{base_ref}..HEAD")
        } else {
            "@{upstream}..HEAD".to_string()
        };
        form.local_loaded = false;
        let load = cx.background_spawn(async move {
            let commits = git_output(
                &work_directory,
                &[
                    "log",
                    "--max-count=100",
                    "--format=%H%x1f%an%x1f%aI%x1f%s",
                    &unpushed_range,
                ],
            )
            .await
            .map(|output| parse_local_commits(&output));
            let diff = git_output(
                &work_directory,
                &["diff", "--numstat", &format!("{base_ref}...HEAD")],
            )
            .await
            .map(|output| parse_numstat(&output));
            (commits, diff)
        });
        form.local_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (commits, diff) = load.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.creating else {
                    return;
                };
                form.local_commits = commits
                    .context("Repo: falha ao listar os commits locais")
                    .log_err()
                    .unwrap_or_default();
                form.diff_summary = diff
                    .context("Repo: falha ao calcular o diff contra o destino")
                    .log_err();
                form.local_loaded = true;
                this.autofill(window, cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn autofill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let status = self.local_branch_status(cx);
        let Some(form) = &mut self.creating else {
            return;
        };
        if form.autofilled || !form.commits_loaded || !form.local_loaded {
            return;
        }
        form.autofilled = true;
        let head_subject = status
            .as_ref()
            .and_then(|status| status.head_subject.clone());

        let mut subjects: Vec<String> = form
            .commits
            .iter()
            .rev()
            .map(|commit| commit.summary.clone())
            .collect();
        for commit in form.local_commits.iter().rev() {
            if !subjects.contains(&commit.summary) {
                subjects.push(commit.summary.clone());
            }
        }
        if subjects.is_empty()
            && let Some(head_subject) = &head_subject
        {
            subjects.push(head_subject.clone());
        }

        if form.title_input.read(cx).is_empty(cx) {
            let (title, source) = match subjects.as_slice() {
                [only] => (only.clone(), t!("repo_hosting.create.title_hint_commit")),
                _ => match humanize_branch(&form.source_branch) {
                    Some(title) => (title, t!("repo_hosting.create.title_hint_branch")),
                    None => (
                        head_subject.unwrap_or_default(),
                        t!("repo_hosting.create.title_hint_commit"),
                    ),
                },
            };
            if !title.is_empty() {
                form.autofilled_title = Some((title.clone(), source));
            }
            form.title_input
                .update(cx, |input, cx| input.set_text(&title, window, cx));
        }
        if form.description_editor.read(cx).text(cx).trim().is_empty() {
            let description = pull_request_description(&subjects, &form.source_branch);
            form.description_editor
                .update(cx, |editor, cx| editor.set_text(description, window, cx));
        }
    }

    fn submit_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let status = self.local_branch_status(cx);
        let repository = self.project.read(cx).active_repository(cx);
        let is_via_collab = self.project.read(cx).is_via_collab();
        let host_name = remote_repository.hosting.name();
        let Some(form) = &mut self.creating else {
            return;
        };
        if form.submitting {
            return;
        }

        let title = form.title_input.read(cx).text(cx).trim().to_string();
        let description = form.description_editor.read(cx).text(cx).trim().to_string();
        let problem = match (&form.destination, &status) {
            _ if title.is_empty() => Some(t!("repo_hosting.create.error_no_title")),
            (None, _) => Some(t!("repo_hosting.create.error_no_destination")),
            (Some(destination), _) if *destination == form.source_branch => {
                Some(t!("repo_hosting.create.error_same_branch"))
            }
            (_, None) => Some(t!("repo_hosting.create.error_no_branch")),
            (_, Some(status)) if status.tracks_other_remote => Some(t!(
                "repo_hosting.create.error_other_remote",
                host = host_name
            )),
            (_, Some(status)) if status.behind > 0 => Some(if status.behind == 1 {
                t!("repo_hosting.create.error_behind.one")
            } else {
                t!(
                    "repo_hosting.create.error_behind.other",
                    count = status.behind
                )
            }),
            (_, Some(status)) if is_via_collab && (status.needs_upstream || status.ahead > 0) => {
                Some(t!("repo_hosting.create.error_collab"))
            }
            _ => None,
        };
        if let Some(problem) = problem {
            form.error = Some(problem);
            cx.notify();
            return;
        }
        let (Some(destination), Some(status), Some(repository)) =
            (form.destination.clone(), status, repository)
        else {
            return;
        };

        let new_pull_request = NewPullRequest {
            title,
            description,
            source_branch: form.source_branch.clone(),
            destination_branch: destination,
            reviewer_ids: form
                .reviewers
                .iter()
                .map(|person| person.id.clone())
                .collect(),
            close_source_branch: form.close_source_branch,
            draft: form.draft,
        };
        form.submitting = true;
        form.error = None;
        cx.notify();

        let needs_push = status.needs_upstream || status.ahead > 0;
        let askpass = self.askpass_delegate("git push", window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if needs_push {
                    let push = repository.update(cx, |repository, cx| {
                        repository.push(
                            status.local_name.clone().into(),
                            new_pull_request.source_branch.clone().into(),
                            remote_name.clone().into(),
                            status.needs_upstream.then_some(PushOptions::SetUpstream),
                            askpass,
                            cx,
                        )
                    });
                    push.await?.with_context(|| {
                        t!("repo_hosting.error.push_failed", remote = remote_name)
                    })?;
                }
                cx.background_spawn(async move {
                    client
                        .create_pull_request(&remote_repository, new_pull_request)
                        .await
                })
                .await
            }
            .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(created) => {
                    this.creating = None;
                    this.sync(cx);
                    this.open_pull_request(created, false, window, cx);
                }
                Err(error) => {
                    if let Some(form) = &mut this.creating {
                        form.submitting = false;
                        form.error = Some(t!(
                            "repo_hosting.create.error_failed",
                            error = format!("{error:#}")
                        ));
                        cx.notify();
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_create_form(
        &self,
        repository: &RemoteRepository,
        form: &CreateForm,
        cx: &Context<Self>,
    ) -> AnyElement {
        let status = self.local_branch_status(cx);
        let Target::Hosted { remote_name, .. } = &self.target else {
            return div().into_any_element();
        };
        let needs_push = status
            .as_ref()
            .is_some_and(|status| status.needs_upstream || status.ahead > 0);

        let (dot, sync_text, sync_note) = match &status {
            Some(status) if status.tracks_other_remote => (
                Color::Error,
                t!("repo_hosting.create.sync_other_remote"),
                SharedString::from("fork"),
            ),
            Some(status) if status.behind > 0 => (
                Color::Error,
                if status.behind == 1 {
                    t!("repo_hosting.create.sync_behind.one", remote = remote_name)
                } else {
                    t!(
                        "repo_hosting.create.sync_behind.other",
                        remote = remote_name,
                        count = status.behind
                    )
                },
                t!("repo_hosting.create.sync_behind_note"),
            ),
            Some(status) if status.needs_upstream => (
                Color::Warning,
                t!(
                    "repo_hosting.create.sync_not_published",
                    remote = remote_name
                ),
                t!("repo_hosting.create.sync_not_published_note"),
            ),
            Some(status) if status.ahead > 0 => (
                Color::Warning,
                if status.ahead == 1 {
                    t!("repo_hosting.create.sync_ahead.one", remote = remote_name)
                } else {
                    t!(
                        "repo_hosting.create.sync_ahead.other",
                        remote = remote_name,
                        count = status.ahead
                    )
                },
                t!("repo_hosting.create.sync_ahead_note"),
            ),
            Some(_) => (
                Color::Success,
                t!("repo_hosting.create.sync_up_to_date", remote = remote_name),
                SharedString::default(),
            ),
            None => (
                Color::Error,
                t!("repo_hosting.create.error_no_branch_label"),
                SharedString::default(),
            ),
        };

        let destination_label: SharedString = form
            .destination
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| t!("repo_hosting.create.destination_placeholder"));
        let panel = cx.weak_entity();
        let mut branches: Vec<String> = form
            .branches
            .iter()
            .filter(|branch| **branch != form.source_branch)
            .cloned()
            .collect();
        if let Some(destination) = &form.destination
            && let Some(index) = branches.iter().position(|branch| branch == destination)
        {
            let destination = branches.remove(index);
            branches.insert(0, destination);
        }
        let selected_destination = form.destination.clone();
        let destination_picker = PopoverMenu::new("repo-create-destination")
            .trigger(PopoverTriggerArea::new(picker_button(
                "repo-create-destination-button",
                destination_label,
                cx,
            )))
            .menu(move |window, cx| {
                let branches = branches.clone();
                let panel = panel.clone();
                let selected = selected_destination.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for branch in branches.iter().take(60) {
                        let panel = panel.clone();
                        let name = branch.clone();
                        menu = menu.toggleable_entry(
                            branch.clone(),
                            selected.as_deref() == Some(branch.as_str()),
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.set_destination(name.clone(), window, cx)
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            });

        let route = v_flex()
            .gap(px(10.))
            .p(px(10.))
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .min_w_0()
                            .h(px(26.))
                            .gap(px(6.))
                            .px_2()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(form.source_branch.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(destination_picker),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(7.)).rounded_full().bg(dot.color(cx)))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(sync_text)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(sync_note)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .when(
                status
                    .as_ref()
                    .is_some_and(|status| status.has_uncommitted_changes),
                |this| {
                    this.child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(t!("repo_hosting.create.uncommitted_warning"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                },
            );

        let field_label = |label: SharedString, hint: Option<SharedString>| {
            h_flex()
                .justify_between()
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .when_some(hint, |this, hint| {
                    this.child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
                })
        };

        let reviewers = self.render_reviewer_picker(form, cx);
        let commits = self.render_create_commits(form, status.as_ref(), cx);

        let content = v_flex()
            .id("repo-create-form")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pt_1()
            .pb_3()
            .gap(px(14.))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(t!(
                            "repo_hosting.create.in_repository",
                            repository = repository.full_name()
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("repo_hosting.create.title"))
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(route)
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label(
                        t!("repo_hosting.create.field_title"),
                        form.autofilled_title
                            .as_ref()
                            .filter(|(title, _)| form.title_input.read(cx).text(cx) == *title)
                            .map(|(_, source)| source.clone()),
                    ))
                    .child(form.title_input.clone()),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label(
                        t!("repo_hosting.create.field_description"),
                        Some(t!("repo_hosting.create.field_description_hint")),
                    ))
                    .child(
                        div()
                            .p(px(10.))
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().colors().border_selected)
                            .bg(cx.theme().colors().editor_background)
                            .child(form.description_editor.clone()),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label(
                        t!("repo_hosting.detail.field_reviewers"),
                        form.has_default_reviewers
                            .then(|| t!("repo_hosting.create.reviewers_hint")),
                    ))
                    .child(reviewers),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(Self::render_create_option(
                        "repo-create-close-source",
                        t!("repo_hosting.create.close_source_branch"),
                        form.close_source_branch,
                        |form| form.close_source_branch = !form.close_source_branch,
                        cx,
                    ))
                    .child(Self::render_create_option(
                        "repo-create-draft",
                        t!("repo_hosting.create.draft"),
                        form.draft,
                        |form| form.draft = !form.draft,
                        cx,
                    )),
            )
            .child(commits)
            .children(self.render_create_changes(form, cx));

        let submit_label = if form.submitting {
            if needs_push {
                t!("repo_hosting.create.pushing")
            } else {
                t!("repo_hosting.create.creating")
            }
        } else if needs_push {
            t!("repo_hosting.create.publish_and_create")
        } else {
            t!("repo_hosting.common.create_pull_request")
        };

        v_flex()
            .key_context("RepoCreatePullRequest")
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.close_create_form(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &menu::SecondaryConfirm, window, cx| {
                if this.creating.as_ref().is_some_and(|form| !form.submitting) {
                    this.submit_create_form(window, cx)
                }
            }))
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px(DynamicSpacing::Base08.px(cx))
                    .py(DynamicSpacing::Base06.px(cx))
                    .child(
                        Button::new(
                            "repo-create-back",
                            t!("repo_hosting.list.tab_pull_requests"),
                        )
                        .style(ButtonStyle::Transparent)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_create_form(window, cx)),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(t!("repo_hosting.create.new_badge"))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(self.render_create_menu(form, needs_push)),
                    ),
            )
            .child(content)
            .child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .when_some(form.error.clone(), |this, error| {
                        this.child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        )
                    })
                    .when(needs_push && status.is_some(), |this| {
                        let status = status.as_ref();
                        let local = status
                            .map(|status| status.local_name.clone())
                            .unwrap_or_default();
                        let flag = if status.is_some_and(|status| status.needs_upstream) {
                            "-u "
                        } else {
                            ""
                        };
                        this.child(
                            h_flex()
                                .px_1()
                                .gap(px(6.))
                                .child(
                                    Icon::new(IconName::ArrowUp)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(t!(
                                        "repo_hosting.create.push_hint",
                                        command = format!("git push {flag}{remote_name} {local}")
                                    ))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                                    .truncate(),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                secondary_button(
                                    "repo-create-cancel",
                                    None,
                                    t!("repo_hosting.common.cancel"),
                                    !form.submitting,
                                    cx,
                                )
                                .bg(cx.theme().colors().element_background)
                                .when(!form.submitting, |this| {
                                    this.on_click(cx.listener(|this, _, window, cx| {
                                        this.close_create_form(window, cx)
                                    }))
                                }),
                            )
                            .child(
                                primary_button(
                                    "repo-create-submit",
                                    Some(IconName::PullRequest),
                                    submit_label,
                                    (!form.submitting).then(|| {
                                        KeyBinding::for_action_in(
                                            &menu::SecondaryConfirm,
                                            &self.focus_handle,
                                            cx,
                                        )
                                    }),
                                    cx,
                                )
                                .flex_1()
                                .min_w_0()
                                .justify_center()
                                .when(form.submitting, |this| this.opacity(0.5).cursor_default())
                                .when(!form.submitting, |this| {
                                    this.on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_create_form(window, cx)
                                    }))
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_reviewer_picker(&self, form: &CreateForm, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let candidates = form.candidates.clone();
        let selected: Vec<String> = form
            .reviewers
            .iter()
            .map(|person| person.id.clone())
            .collect();
        let add = PopoverMenu::new("repo-create-reviewers")
            .trigger(PopoverTriggerArea::new(pill_button(
                "repo-create-add-reviewer",
                Some(IconName::Plus),
                t!("repo_hosting.create.add_reviewer"),
                !candidates.is_empty(),
                cx,
            )))
            .menu(move |window, cx| {
                let candidates = candidates.clone();
                let selected = selected.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for person in candidates.iter() {
                        let panel = panel.clone();
                        let toggled = person.clone();
                        menu = menu.toggleable_entry(
                            person.short_name().to_string(),
                            selected.contains(&person.id),
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        if let Some(form) = &mut panel.creating {
                                            toggle_person(&mut form.reviewers, &toggled);
                                            cx.notify();
                                        }
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            });

        h_flex()
            .flex_wrap()
            .gap(DynamicSpacing::Base06.px(cx))
            .children(form.reviewers.iter().map(|person| {
                let removed = person.clone();
                h_flex()
                    .id(SharedString::from(format!(
                        "repo-create-reviewer-{}",
                        person.id
                    )))
                    .h(px(26.))
                    .gap(px(6.))
                    .pl_1()
                    .pr(px(6.))
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().colors().border_selected)
                    .bg(cx.theme().colors().element_background)
                    .child(avatar(&person.display_name, Color::Info, cx))
                    .child(Label::new(person.short_name().to_string()).size(LabelSize::Small))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "repo-create-reviewer-remove-{}",
                                person.id
                            )))
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|this| this.bg(cx.theme().colors().element_hover))
                            .child(
                                Icon::new(IconName::Close)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = &mut this.creating {
                                    toggle_person(&mut form.reviewers, &removed);
                                    cx.notify();
                                }
                            })),
                    )
            }))
            .child(add)
    }

    fn render_create_option(
        id: &'static str,
        label: SharedString,
        checked: bool,
        toggle: fn(&mut CreateForm),
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .gap_2()
            .cursor_pointer()
            .child(checkbox_indicator(checked, cx))
            .child(Label::new(label).size(LabelSize::Small))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(form) = &mut this.creating {
                    toggle(form);
                    cx.notify();
                }
            }))
    }

    fn render_create_commits(
        &self,
        form: &CreateForm,
        status: Option<&LocalBranchStatus>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let now = Utc::now();
        let mut list = v_flex().gap_0p5().child(
            section_header(
                format!(
                    "{} · {}",
                    t!("repo_hosting.create.commits_header"),
                    form.commits.len() + form.local_commits.len()
                ),
                Some(
                    Label::new(t!("repo_hosting.create.commits_hint"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element(),
                ),
            )
            .pt_3()
            .pb_1(),
        );
        if !form.commits_loaded || !form.local_loaded {
            list = list.child(
                Label::new(t!("repo_hosting.detail.loading_commits"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        } else if form.local_commits.is_empty()
            && status.is_some_and(|status| status.needs_upstream)
        {
            list = list.child(
                Label::new(t!("repo_hosting.create.commits_not_published"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for commit in &form.local_commits {
            list = list.child(self.render_commit_row(commit, true, now, cx));
        }
        for commit in &form.commits {
            if form
                .local_commits
                .iter()
                .any(|local| local.hash == commit.hash)
            {
                continue;
            }
            list = list.child(self.render_commit_row(commit, false, now, cx));
        }
        list
    }

    /// The board's "+482 −97 em 12 arquivos · Diff contra develop" card.
    fn render_create_changes(&self, form: &CreateForm, cx: &Context<Self>) -> Option<AnyElement> {
        let summary = form.diff_summary?;
        let destination = form.destination.clone()?;
        if summary.files == 0 {
            return None;
        }
        Some(
            h_flex()
                .gap_2()
                .py_2()
                .pl(px(10.))
                .pr_2()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().element_background)
                .child(
                    Icon::new(IconName::FileDiff)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(format!("+{}", summary.added))
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Created),
                )
                .child(
                    Label::new(format!("−{}", summary.removed))
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Deleted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(if summary.files == 1 {
                            t!("repo_hosting.create.changed_files.one")
                        } else {
                            t!(
                                "repo_hosting.create.changed_files.other",
                                count = summary.files
                            )
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                    ),
                )
                .child(
                    h_flex()
                        .id("repo-create-diff")
                        .flex_none()
                        .h(px(26.))
                        .pl_2()
                        .pr(px(10.))
                        .gap(px(6.))
                        .rounded_lg()
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                        .child(
                            Icon::new(IconName::DiffSplit)
                                .size(IconSize::XSmall)
                                .color(Color::Default),
                        )
                        .child(
                            Label::new(t!(
                                "repo_hosting.create.diff_against",
                                branch = destination
                            ))
                            .size(LabelSize::Small),
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_create_diff(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn open_create_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { remote_name, .. } = &self.target else {
            return;
        };
        let Some(destination) = self
            .creating
            .as_ref()
            .and_then(|form| form.destination.clone())
        else {
            return;
        };
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            return;
        };
        let base_ref: SharedString = format!("{remote_name}/{destination}").into();
        let project = self.project.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                BranchDiff::deploy_branch_diff_with_base_ref(
                    workspace, project, repository, base_ref, None, window, cx,
                );
            })
            .log_err();
    }

    fn render_create_menu(&self, form: &CreateForm, needs_push: bool) -> impl IntoElement {
        let Target::Hosted { repository, .. } = &self.target else {
            return div().into_any_element();
        };
        let host = self.hosting().name();
        let web_url =
            repository.new_pull_request_url(&form.source_branch, form.destination.as_deref());
        let source_branch = form.source_branch.clone();
        PopoverMenu::new("repo-create-menu")
            .trigger(
                IconButton::new("repo-create-menu-button", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
            )
            .menu(move |window, cx| {
                let web_url = web_url.clone();
                let source_branch = source_branch.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(
                        ContextMenuEntry::new(t!("repo_hosting.common.open_in", host = host))
                            .icon(IconName::ArrowUpRight)
                            .disabled(needs_push)
                            .handler(move |_, cx| cx.open_url(&web_url)),
                    )
                    .item(
                        ContextMenuEntry::new(t!("repo_hosting.create.copy_branch"))
                            .icon(IconName::Copy)
                            .handler(move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    source_branch.clone(),
                                ))
                            }),
                    )
                }))
            })
            .into_any_element()
    }
}

/// Runs git in the checkout and returns what it printed.
async fn git_output(work_directory: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
    let output = util::command::new_command("git")
        .current_dir(work_directory)
        .args(args)
        .output()
        .await
        .context("git não encontrado")?;
    anyhow::ensure!(
        output.status.success(),
        "git {}: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git log --format=%H%x1f%an%x1f%aI%x1f%s` lines, newest first.
fn parse_local_commits(output: &str) -> Vec<api::Commit> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\u{1f}');
            let hash = fields.next()?.to_string();
            let author = fields.next()?.to_string();
            let date = DateTime::parse_from_rfc3339(fields.next()?)
                .ok()
                .map(|date| date.with_timezone(&Utc));
            let summary = fields.next()?.to_string();
            (!hash.is_empty()).then_some(api::Commit {
                hash,
                summary,
                author,
                date,
            })
        })
        .collect()
}

/// Sums `git diff --numstat`; binary files count as changed without lines.
fn parse_numstat(output: &str) -> DiffSummary {
    let mut summary = DiffSummary::default();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        summary.added += fields
            .next()
            .and_then(|added| added.parse::<u64>().ok())
            .unwrap_or(0);
        summary.removed += fields
            .next()
            .and_then(|removed| removed.parse::<u64>().ok())
            .unwrap_or(0);
        summary.files += 1;
    }
    summary
}

fn add_person(people: &mut Vec<api::Person>, person: &api::Person, except_id: Option<&str>) {
    if person.id.is_empty() || Some(person.id.as_str()) == except_id {
        return;
    }
    if !people.iter().any(|existing| existing.id == person.id) {
        people.push(person.clone());
    }
}

fn toggle_person(people: &mut Vec<api::Person>, person: &api::Person) {
    if let Some(index) = people.iter().position(|existing| existing.id == person.id) {
        people.remove(index);
    } else {
        people.push(person.clone());
    }
}

/// "feat/CU-86a1b2-login-com-biometria" → "Login com biometria".
fn humanize_branch(branch: &str) -> Option<String> {
    let last = branch.rsplit('/').next().unwrap_or(branch);
    let words: Vec<&str> = last
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .collect();
    let words: Vec<&str> = match words.as_slice() {
        [prefix, id, rest @ ..] if prefix.eq_ignore_ascii_case("cu") && !id.is_empty() => {
            rest.to_vec()
        }
        _ => words,
    };
    let sentence = words.join(" ");
    let mut characters = sentence.chars();
    let first = characters.next()?;
    Some(first.to_uppercase().chain(characters).collect())
}

/// The commit subjects, oldest first, as a list, and the ClickUp task the branch is for.
fn pull_request_description(subjects: &[String], branch: &str) -> String {
    let mut description = if subjects.len() > 1 {
        subjects
            .iter()
            .map(|subject| format!("- {subject}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    if let Some(task_id) = clickup_task_id(branch) {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(&format!("Closes {task_id}."));
    }
    description
}

impl RepoPanel {
    fn start_merging(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let is_checked_out = self
            .open_pull_request
            .as_ref()
            .is_some_and(|open_pull_request| {
                self.branch.as_deref() == Some(open_pull_request.summary.source_branch.as_str())
                    && !open_pull_request.summary.is_from_fork(&repository)
            });
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let pull_request = &open_pull_request.summary;
        let destination = pull_request.destination_branch.clone();
        let mut message = format!("{} (#{})", pull_request.title, pull_request.number);
        if let Some(task_id) = clickup_task_id(&pull_request.source_branch) {
            message.push_str(&format!("\n\nCloses {task_id}."));
        }
        let message_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_text(message, window, cx);
            editor
        });

        let load = cx.background_spawn(async move {
            client.get_merge_options(&repository, &destination).await
        });
        let load_task = cx.spawn(async move |this, cx| {
            let options = load.await;
            this.update(cx, |this, cx| {
                let Some(form) = this
                    .open_pull_request
                    .as_mut()
                    .and_then(|open_pull_request| open_pull_request.merging.as_mut())
                else {
                    return;
                };
                match options
                    .context("Repo: estratégias de merge indisponíveis")
                    .log_err()
                {
                    Some(options) => {
                        form.strategies = options.strategies;
                        form.allows_auto_merge = options.allows_auto_merge;
                    }
                    None => {
                        form.strategies = vec![MergeStrategy::MergeCommit, MergeStrategy::Squash];
                    }
                }
                form.strategy = form.strategies.first().copied();
                cx.notify();
            })
            .log_err();
        });

        open_pull_request.error = None;
        open_pull_request.notice = None;
        open_pull_request.merging = Some(MergeForm {
            strategies: Vec::new(),
            strategy: None,
            message_editor,
            close_source_branch: true,
            clean_up_local: is_checked_out,
            allows_auto_merge: false,
            auto_merge: false,
            submitting: false,
            progress: None,
            _load_task: load_task,
        });
        cx.notify();
    }

    fn merge_form_mut(&mut self) -> Option<&mut MergeForm> {
        self.open_pull_request
            .as_mut()
            .and_then(|open_pull_request| open_pull_request.merging.as_mut())
    }

    fn cancel_merging(&mut self, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request
            && !open_pull_request
                .merging
                .as_ref()
                .is_some_and(|form| form.submitting)
        {
            open_pull_request.merging = None;
            cx.notify();
        }
    }

    fn set_merge_progress(&mut self, progress: SharedString, cx: &mut Context<Self>) {
        if let Some(form) = self.merge_form_mut() {
            form.progress = Some(progress);
            cx.notify();
        }
    }

    fn merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let repository = self.project.read(cx).active_repository(cx);
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        // The checkbox was set up for the branch checked out back then; only the PR's own
        // branch may be deleted, so check it's still the one checked out.
        let local_status = (self.branch.as_deref()
            == Some(open_pull_request.summary.source_branch.as_str()))
        .then(|| self.local_branch_status(cx))
        .flatten();
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let number = open_pull_request.summary.number;
        let destination = open_pull_request.summary.destination_branch.clone();
        let Some(form) = &mut open_pull_request.merging else {
            return;
        };
        let Some(strategy) = form.strategy else {
            return;
        };
        if form.submitting {
            return;
        }
        let message = form.message_editor.read(cx).text(cx).trim().to_string();
        let close_source_branch = form.close_source_branch;
        if form.auto_merge {
            form.submitting = true;
            form.progress = Some(t!("repo_hosting.merge.enabling_auto_merge"));
            open_pull_request.error = None;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        client
                            .enable_auto_merge(&remote_repository, number, strategy)
                            .await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(open_pull_request) = &mut this.open_pull_request {
                        match result {
                            Ok(()) => {
                                open_pull_request.merging = None;
                                open_pull_request.notice = Some(t!(
                                    "repo_hosting.merge.auto_merge_enabled",
                                    destination = destination
                                ));
                            }
                            Err(error) => {
                                if let Some(form) = &mut open_pull_request.merging {
                                    form.submitting = false;
                                    form.progress = None;
                                }
                                open_pull_request.error = Some(t!(
                                    "repo_hosting.merge.auto_merge_rejected",
                                    error = format!("{error:#}")
                                ));
                            }
                        }
                    }
                    this.reload_open_pull_request(cx);
                    cx.notify();
                })
            })
            .detach_and_log_err(cx);
            return;
        }
        // Only a branch whose every commit is on the remote can be deleted safely: those
        // commits are in the pull request that just merged.
        let clean_up = form
            .clean_up_local
            .then(|| local_status.zip(repository))
            .flatten()
            .map(|(status, repository)| {
                (
                    status.ahead == 0 && !status.needs_upstream,
                    status.local_name,
                    repository,
                )
            });
        form.submitting = true;
        form.progress = Some(t!(
            "repo_hosting.merge.merging",
            host = remote_repository.hosting.name()
        ));
        open_pull_request.error = None;
        cx.notify();

        let askpass = (
            self.askpass_delegate("git fetch", window, cx),
            self.askpass_delegate("git pull", window, cx),
        );
        cx.spawn_in(window, async move |this, cx| {
            let merged = async {
                let outcome = cx
                    .background_spawn({
                        let client = client.clone();
                        let remote_repository = remote_repository.clone();
                        async move {
                            client
                                .merge_pull_request(
                                    &remote_repository,
                                    number,
                                    strategy,
                                    &message,
                                    close_source_branch,
                                )
                                .await
                        }
                    })
                    .await?;
                let task_url = match outcome {
                    MergeOutcome::Merged(pull_request) => return anyhow::Ok(pull_request),
                    MergeOutcome::Pending { task_url } => task_url,
                };
                this.update(cx, |this, cx| {
                    this.set_merge_progress(t!("repo_hosting.merge.bitbucket_still_merging"), cx)
                })?;
                for _ in 0..MERGE_POLL_ATTEMPTS {
                    cx.background_executor().timer(MERGE_POLL_INTERVAL).await;
                    let task = cx
                        .background_spawn({
                            let client = client.clone();
                            let remote_repository = remote_repository.clone();
                            let task_url = task_url.clone();
                            async move {
                                client.get_merge_task(&remote_repository,
                                    &task_url,
                                )
                                .await
                            }
                        })
                        .await?;
                    if let Some(pull_request) = task {
                        return Ok(pull_request);
                    }
                }
                anyhow::bail!("{}", t!("repo_hosting.merge.not_finished"))
            }
            .await;

            let merged = match merged {
                Ok(merged) => merged,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        if let Some(form) = this.merge_form_mut() {
                            form.submitting = false;
                            form.progress = None;
                        }
                        this.set_open_error(
                            t!("repo_hosting.merge.blocked", error = format!("{error:#}"))
                                .to_string(),
                            cx,
                        );
                    })?;
                    return anyhow::Ok(());
                }
            };

            let cleanup = match clean_up {
                Some((force_allowed, local_branch, repository)) => {
                    this.update(cx, |this, cx| {
                        this.set_merge_progress(
                            t!(
                                "repo_hosting.merge.switching_back",
                                destination = destination
                            ),
                            cx,
                        )
                    })?;
                    Some(
                        clean_up_after_merge(
                            &repository,
                            &remote_name,
                            &destination,
                            &local_branch,
                            force_allowed && strategy.rewrites_commits(),
                            askpass,
                            cx,
                        )
                        .await
                        .map(|()| local_branch),
                    )
                }
                None => None,
            };

            this.update(cx, |this, cx| {
                if let Some(open_pull_request) = &mut this.open_pull_request {
                    open_pull_request.summary = merged;
                    open_pull_request.merging = None;
                    open_pull_request.notice = Some(match &cleanup {
                        Some(Ok(local_branch)) => t!(
                            "repo_hosting.merge.merged_and_cleaned",
                            destination = destination,
                            branch = local_branch
                        ),
                        _ => t!("repo_hosting.merge.merged", destination = destination),
                    });
                    if let Some(Err(error)) = &cleanup {
                        open_pull_request.error = Some(t!(
                            "repo_hosting.merge.cleanup_failed",
                            error = format!("{error:#}")
                        ));
                    }
                }
                this.reload_open_pull_request(cx);
                this.sync(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn decline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let number = open_pull_request.summary.number;
        let decline_label = t!("repo_hosting.merge.decline");
        let cancel_label = t!("repo_hosting.common.cancel");
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &t!("repo_hosting.merge.decline_prompt", number = number),
            Some(&t!(
                "repo_hosting.merge.decline_prompt_detail",
                host = self.hosting().name()
            )),
            &[decline_label.as_ref(), cancel_label.as_ref()],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            this.update(cx, |this, cx| {
                this.set_pending_action(Some(PendingAction::Declining), cx)
            })?;
            let result = cx
                .background_spawn(
                    async move { client.decline_pull_request(&repository, number).await },
                )
                .await;
            this.update(cx, |this, cx| {
                this.set_pending_action(None, cx);
                match result {
                    Ok(()) => {
                        if let Some(open_pull_request) = &mut this.open_pull_request {
                            open_pull_request.merging = None;
                            open_pull_request.notice = Some(t!("repo_hosting.merge.declined"));
                        }
                        this.reload_open_pull_request(cx);
                        this.sync(cx);
                    }
                    Err(error) => this.set_open_error(
                        t!(
                            "repo_hosting.merge.decline_failed",
                            error = format!("{error:#}")
                        )
                        .to_string(),
                        cx,
                    ),
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_merge_card(
        &self,
        open_pull_request: &OpenPullRequest,
        form: &MergeForm,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let gates = merge_gates(
            &open_pull_request.summary,
            open_pull_request.detail.as_ref(),
        );
        let worst = gates
            .iter()
            .map(|gate| gate.level)
            .max()
            .unwrap_or(GateLevel::Ok);
        let destination = &open_pull_request.summary.destination_branch;
        let (title, dot) = match worst {
            GateLevel::Ok => (
                t!("repo_hosting.merge.title_ready", destination = destination),
                Color::Success,
            ),
            GateLevel::Warning => (
                t!(
                    "repo_hosting.merge.title_warnings",
                    destination = destination
                ),
                Color::Warning,
            ),
            GateLevel::Blocking => (
                t!(
                    "repo_hosting.merge.title_blocked",
                    destination = destination
                ),
                Color::Error,
            ),
        };
        let checked_out_source =
            self.branch.as_deref() == Some(open_pull_request.summary.source_branch.as_str());

        let hosting = self.hosting();
        let strategies = if form.strategies.is_empty() {
            Label::new(t!("repo_hosting.merge.loading_strategies"))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        } else {
            segmented_control(cx)
                .children(form.strategies.iter().map(|strategy| {
                    let strategy = *strategy;
                    let selected = form.strategy == Some(strategy);
                    segment_option(
                        SharedString::from(format!("repo-merge-{strategy:?}")),
                        strategy.label(hosting),
                        selected,
                        !form.submitting,
                        cx,
                    )
                    .when(!form.submitting, |this| {
                        this.on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(form) = this.merge_form_mut() {
                                form.strategy = Some(strategy);
                                cx.notify();
                            }
                        }))
                    })
                }))
                .into_any_element()
        };
        let strategy_hint = form.strategy.map(|strategy| match strategy {
            MergeStrategy::Squash | MergeStrategy::SquashFastForward => {
                let commits = open_pull_request
                    .detail
                    .as_ref()
                    .map_or(0, |detail| detail.commits.len());
                if commits == 1 {
                    t!(
                        "repo_hosting.merge.hint_squash.one",
                        destination = destination
                    )
                } else {
                    t!(
                        "repo_hosting.merge.hint_squash.other",
                        count = commits,
                        destination = destination
                    )
                }
            }
            MergeStrategy::MergeCommit => t!("repo_hosting.merge.hint_merge_commit"),
            MergeStrategy::FastForward => {
                t!(
                    "repo_hosting.merge.hint_fast_forward",
                    destination = destination
                )
            }
            MergeStrategy::RebaseFastForward | MergeStrategy::RebaseMerge => {
                t!("repo_hosting.merge.hint_rebase", destination = destination)
            }
        });

        v_flex()
            .gap(DynamicSpacing::Base10.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_focused)
            .bg(cx.theme().colors().element_background)
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(div().size(px(8.)).rounded_full().bg(dot.color(cx)))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base04.px(cx))
                    .children(gates.into_iter().map(|gate| {
                        let (icon, color) = match gate.level {
                            GateLevel::Ok => (IconName::Check, Color::Success),
                            GateLevel::Warning => (IconName::Warning, Color::Warning),
                            GateLevel::Blocking => (IconName::XCircle, Color::Error),
                        };
                        h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .child(div().flex_1().min_w_0().child(
                                Label::new(gate.text).size(LabelSize::Small).color(
                                    if gate.level == GateLevel::Ok {
                                        Color::Default
                                    } else {
                                        color
                                    },
                                ),
                            ))
                    })),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(t!("repo_hosting.merge.strategy"))
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(strategies)
                    .when_some(strategy_hint, |this, hint| {
                        this.child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
                    }),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(t!("repo_hosting.merge.commit_message"))
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .p(DynamicSpacing::Base08.px(cx))
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(form.message_editor.clone()),
                    ),
            )
            .when(form.allows_auto_merge, |this| {
                this.child(
                    Checkbox::new("repo-merge-auto", form.auto_merge.into())
                        .label(t!("repo_hosting.merge.auto_merge"))
                        .disabled(form.submitting)
                        .on_click({
                            let panel = cx.weak_entity();
                            move |state, _, cx| {
                                let checked = state.selected();
                                panel
                                    .update(cx, |panel, cx| {
                                        if let Some(form) = panel.merge_form_mut() {
                                            form.auto_merge = checked;
                                            cx.notify();
                                        }
                                    })
                                    .log_err();
                            }
                        }),
                )
            })
            .child(
                Checkbox::new("repo-merge-close-source", form.close_source_branch.into())
                    .label(match hosting {
                        api::Hosting::Bitbucket => t!("repo_hosting.merge.close_branch_bitbucket"),
                        api::Hosting::GitHub => t!("repo_hosting.merge.delete_branch_github"),
                        api::Hosting::GitLab => t!("repo_hosting.merge.delete_branch_gitlab"),
                    })
                    .disabled(form.submitting)
                    .on_click({
                        let panel = cx.weak_entity();
                        move |state, _, cx| {
                            let checked = state.selected();
                            panel
                                .update(cx, |panel, cx| {
                                    if let Some(form) = panel.merge_form_mut() {
                                        form.close_source_branch = checked;
                                        cx.notify();
                                    }
                                })
                                .log_err();
                        }
                    }),
            )
            .when(checked_out_source, |this| {
                this.child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            Checkbox::new("repo-merge-clean-up", form.clean_up_local.into())
                                .label(t!(
                                    "repo_hosting.merge.clean_up_local",
                                    destination = destination
                                ))
                                .disabled(form.submitting)
                                .on_click({
                                    let panel = cx.weak_entity();
                                    move |state, _, cx| {
                                        let checked = state.selected();
                                        panel
                                            .update(cx, |panel, cx| {
                                                if let Some(form) = panel.merge_form_mut() {
                                                    form.clean_up_local = checked;
                                                    cx.notify();
                                                }
                                            })
                                            .log_err();
                                    }
                                }),
                        )
                        .child(
                            Label::new(format!(
                                "git switch {destination} · git pull · git branch -d"
                            ))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .ml(DynamicSpacing::Base24.px(cx)),
                        ),
                )
            })
    }

    fn render_merge_actions(
        &self,
        open_pull_request: &OpenPullRequest,
        form: &MergeForm,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let gates = merge_gates(
            &open_pull_request.summary,
            open_pull_request.detail.as_ref(),
        );
        let blocked = gates.iter().any(|gate| gate.level == GateLevel::Blocking);
        let destination = &open_pull_request.summary.destination_branch;
        let label = match form.strategy {
            Some(MergeStrategy::Squash | MergeStrategy::SquashFastForward) => {
                t!(
                    "repo_hosting.merge.submit_squash",
                    destination = destination
                )
            }
            Some(MergeStrategy::FastForward) => {
                t!(
                    "repo_hosting.merge.submit_fast_forward",
                    destination = destination
                )
            }
            Some(MergeStrategy::RebaseFastForward | MergeStrategy::RebaseMerge) => {
                t!(
                    "repo_hosting.merge.submit_rebase",
                    destination = destination
                )
            }
            _ => t!("repo_hosting.merge.submit_merge", destination = destination),
        };
        let label = if form.auto_merge {
            t!("repo_hosting.merge.submit_auto_merge")
        } else {
            label
        };
        let waiting_on_checks = open_pull_request.detail.as_ref().is_some_and(|detail| {
            let summary = CheckSummary::of(&detail.checks);
            summary.failed > 0 || summary.running > 0
        });
        let can_submit = !form.submitting && form.strategy.is_some() && !blocked;
        let declining = matches!(
            open_pull_request.pending_action,
            Some(PendingAction::Declining)
        );
        v_flex()
            .key_context("RepoMerge")
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_merging(cx)))
            .gap(DynamicSpacing::Base06.px(cx))
            .p(DynamicSpacing::Base08.px(cx))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(form.progress.clone(), |this, progress| {
                this.child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(spinning_if(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            true,
                        ))
                        .child(
                            Label::new(progress)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(form.auto_merge && waiting_on_checks, |this| {
                this.child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(t!("repo_hosting.merge.waiting_on_checks"))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        secondary_button(
                            "repo-merge-cancel",
                            None,
                            t!("repo_hosting.common.cancel"),
                            !form.submitting,
                            cx,
                        )
                        .when(!form.submitting, |this| {
                            this.on_click(cx.listener(|this, _, _, cx| this.cancel_merging(cx)))
                        }),
                    )
                    .child(
                        secondary_button(
                            "repo-merge-decline",
                            Some((IconName::XCircle, Color::Error)),
                            if declining {
                                t!("repo_hosting.merge.declining")
                            } else {
                                t!("repo_hosting.merge.decline")
                            },
                            !(form.submitting || declining),
                            cx,
                        )
                        .when(!(form.submitting || declining), |this| {
                            this.on_click(
                                cx.listener(|this, _, window, cx| this.decline(window, cx)),
                            )
                        }),
                    )
                    .child(div().flex_1())
                    .child(
                        primary_button("repo-merge-submit", Some(IconName::Check), label, None, cx)
                            .when(!can_submit, |this| this.opacity(0.5).cursor_default())
                            .when(blocked, |this| {
                                this.tooltip(Tooltip::text(t!(
                                    "repo_hosting.merge.resolve_conflicts"
                                )))
                            })
                            .when(can_submit, |this| {
                                this.on_click(
                                    cx.listener(|this, _, window, cx| this.merge(window, cx)),
                                )
                            }),
                    ),
            )
    }
}

/// Bitbucket's own answer can take its whole request timeout; this covers the rest of a slow
/// merge before telling the user to look on the web.
const MERGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MERGE_POLL_ATTEMPTS: usize = 90;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum GateLevel {
    Ok,
    Warning,
    /// Only conflicts: Bitbucket's own rules decide the rest, and its refusal is shown as is.
    Blocking,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GateKind {
    Approvals,
    Other,
}

struct Gate {
    kind: GateKind,
    level: GateLevel,
    text: SharedString,
}

/// What the merge card lists before merging. Most are warnings: Bitbucket Cloud doesn't say
/// which merge checks the branch enforces, so it has the last word.
fn merge_gates(pull_request: &PullRequest, detail: Option<&PullRequestDetail>) -> Vec<Gate> {
    let mut gates = Vec::new();
    let mut gate = |kind, level, text: SharedString| gates.push(Gate { kind, level, text });

    if let Some(detail) = detail {
        let summary = CheckSummary::of(&detail.checks);
        if summary.total == 0 {
            gate(
                GateKind::Other,
                GateLevel::Ok,
                t!("repo_hosting.gate.no_checks"),
            );
        } else if summary.failed > 0 {
            gate(
                GateKind::Other,
                GateLevel::Warning,
                t!(
                    "repo_hosting.gate.checks_failed",
                    failed = summary.failed,
                    total = summary.total
                ),
            );
        } else if summary.running > 0 {
            gate(
                GateKind::Other,
                GateLevel::Warning,
                t!(
                    "repo_hosting.gate.checks_running",
                    running = summary.running,
                    total = summary.total
                ),
            );
        } else {
            gate(
                GateKind::Other,
                GateLevel::Ok,
                t!(
                    "repo_hosting.gate.checks_passed",
                    passed = summary.passed,
                    total = summary.total
                ),
            );
        }
    }

    let reviewers = pull_request.reviewers().count();
    let approvals = pull_request.approval_count();
    gate(
        GateKind::Approvals,
        if reviewers > 0 && approvals >= reviewers {
            GateLevel::Ok
        } else {
            GateLevel::Warning
        },
        approval_label(approvals, reviewers),
    );
    for participant in &pull_request.participants {
        if participant.requested_changes {
            gate(
                GateKind::Other,
                GateLevel::Warning,
                t!(
                    "repo_hosting.gate.changes_requested",
                    name = participant.person.short_name()
                ),
            );
        }
    }
    if pull_request.draft {
        gate(
            GateKind::Other,
            GateLevel::Warning,
            t!("repo_hosting.gate.draft"),
        );
    }
    if pull_request.open_task_count > 0 {
        gate(
            GateKind::Other,
            GateLevel::Warning,
            if pull_request.open_task_count == 1 {
                t!("repo_hosting.gate.open_tasks.one")
            } else {
                t!(
                    "repo_hosting.gate.open_tasks.other",
                    count = pull_request.open_task_count
                )
            },
        );
    }
    if let Some(detail) = detail {
        let conflicts = detail
            .files
            .iter()
            .filter(|file| file.change == FileChange::Conflict)
            .count();
        if conflicts > 0 {
            gate(
                GateKind::Other,
                GateLevel::Blocking,
                if conflicts == 1 {
                    t!("repo_hosting.gate.conflicts_in_files.one")
                } else {
                    t!(
                        "repo_hosting.gate.conflicts_in_files.other",
                        count = conflicts
                    )
                },
            );
        } else if detail.has_conflicts {
            gate(
                GateKind::Other,
                GateLevel::Blocking,
                t!(
                    "repo_hosting.gate.conflicts_with",
                    branch = pull_request.destination_branch
                ),
            );
        } else {
            gate(
                GateKind::Other,
                GateLevel::Ok,
                t!(
                    "repo_hosting.gate.no_conflicts_with",
                    branch = pull_request.destination_branch
                ),
            );
        }
    }
    gates
}

/// After the pull request of the checked-out branch merged: switch to the destination, bring it
/// up to date and delete the merged branch.
async fn clean_up_after_merge(
    repository: &Entity<Repository>,
    remote_name: &str,
    destination: &str,
    local_branch: &str,
    force_delete: bool,
    (fetch_askpass, pull_askpass): (AskPassDelegate, AskPassDelegate),
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    fetch_remote(repository, remote_name, fetch_askpass, cx).await?;
    let switch = repository.update(cx, |repository, _| {
        repository.change_branch(destination.to_string())
    });
    if switch.await?.is_err() {
        // No local copy of the destination yet: create one tracking the remote's.
        let switch = repository.update(cx, |repository, _| {
            repository.change_branch(format!("{remote_name}/{destination}"))
        });
        switch.await??;
    }
    let pull = repository.update(cx, |repository, cx| {
        repository.pull(
            Some(destination.to_string().into()),
            remote_name.to_string().into(),
            false,
            pull_askpass,
            cx,
        )
    });
    pull.await??;
    let delete = repository.update(cx, |repository, _| {
        repository.delete_branch(false, local_branch.to_string(), false)
    });
    if let Err(error) = delete.await? {
        // Squash and rebase merges leave commits git doesn't recognize as merged.
        if !force_delete {
            return Err(error.context(t!(
                "repo_hosting.merge.branch_not_deleted",
                branch = local_branch
            )));
        }
        let delete = repository.update(cx, |repository, _| {
            repository.delete_branch(false, local_branch.to_string(), true)
        });
        delete.await??;
    }
    Ok(())
}

impl RepoPanel {
    /// Loads a list tab's data in the background into the slot `slot` picks.
    fn load_into<T, F>(
        &mut self,
        slot: fn(&mut Self) -> &mut Loadable<T>,
        fetch: impl FnOnce(host::Client, RemoteRepository) -> F,
        cx: &mut Context<Self>,
    ) where
        T: 'static,
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send,
    {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let load = cx.background_spawn(fetch(client, repository));
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if let Err(error) = &result
                    && crate::is_unauthorized(error)
                {
                    {
                        let hosting = this.hosting();
                        this.store
                            .update(cx, |store, cx| store.mark_rejected(hosting, cx));
                    }
                    return;
                }
                let entry = slot(this);
                entry.loading = false;
                match result {
                    Ok(value) => {
                        entry.value = Some(value);
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(describe_tab_error(&error).into()),
                }
                cx.notify();
            })
            .log_err();
        });
        let entry = slot(self);
        entry.loading = true;
        entry.task = Some(task);
        cx.notify();
    }

    /// Loads whatever the selected list tab shows that isn't kept in sync.
    fn load_tab_data(&mut self, cx: &mut Context<Self>) {
        match self.list_tab {
            ListTab::PullRequests if self.state_filter != PullRequestState::Open => {
                let state = self.state_filter;
                self.load_into(
                    |this| &mut this.closed_pull_requests,
                    move |client, repository| async move {
                        let list = client.list_closed_pull_requests(&repository, state).await?;
                        Ok((state, list))
                    },
                    cx,
                );
            }
            ListTab::PullRequests => {}
            ListTab::Pipelines => self.load_into(
                |this| &mut this.pipelines,
                |client, repository| async move { client.list_pipelines(&repository).await },
                cx,
            ),
            ListTab::Issues => self.load_into(
                |this| &mut this.issues,
                |client, repository| async move { client.list_open_issues(&repository).await },
                cx,
            ),
        }
    }

    fn select_list_tab(&mut self, tab: ListTab, cx: &mut Context<Self>) {
        if self.list_tab == tab {
            return;
        }
        self.list_tab = tab;
        let already_loaded = match tab {
            ListTab::PullRequests => true,
            ListTab::Pipelines => self.pipelines.value.is_some(),
            ListTab::Issues => self.issues.value.is_some(),
        };
        if !already_loaded {
            self.load_tab_data(cx);
        }
        cx.notify();
    }

    fn set_state_filter(&mut self, state: PullRequestState, cx: &mut Context<Self>) {
        self.state_filter = state;
        self.load_tab_data(cx);
        cx.notify();
    }

    fn render_list_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let open_count = self.pull_requests.list.len();
        let issue_count = self
            .issues
            .value
            .as_ref()
            .and_then(|issues| issues.as_ref().map(Vec::len));
        let running = self.pipelines.value.as_ref().map(|pipelines| {
            pipelines
                .iter()
                .filter(|pipeline| {
                    matches!(
                        pipeline.state,
                        api::PipelineState::Running | api::PipelineState::Pending
                    )
                })
                .count()
        });
        let tabs = [
            (
                ListTab::PullRequests,
                "repo-list-tab-pull-requests",
                t!("repo_hosting.list.tab_pull_requests"),
                Some(open_count),
            ),
            (
                ListTab::Issues,
                "repo-list-tab-issues",
                t!("repo_hosting.list.tab_issues"),
                issue_count,
            ),
            (
                ListTab::Pipelines,
                "repo-list-tab-pipelines",
                match self.hosting() {
                    api::Hosting::Bitbucket | api::Hosting::GitLab => {
                        t!("repo_hosting.list.tab_pipelines")
                    }
                    api::Hosting::GitHub => t!("repo_hosting.list.tab_actions"),
                },
                running.filter(|count| *count > 0),
            ),
        ];
        segmented_control(cx).children(tabs.into_iter().map(|(tab, id, label, count)| {
            let selected = self.list_tab == tab;
            match count {
                Some(count) => {
                    segment_option_with_count(id, label, count.to_string(), selected, true, cx)
                }
                None => segment_option(id, label, selected, true, cx),
            }
            .on_click(cx.listener(move |this, _, _, cx| this.select_list_tab(tab, cx)))
        }))
    }

    fn render_state_filter(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let current = self.state_filter;
        PopoverMenu::new("repo-state-filter")
            .trigger(PopoverTriggerArea::new(
                select_field("repo-state-filter-trigger", state_filter_label(current), cx)
                    .w(px(104.))
                    .flex_none(),
            ))
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for state in [
                        PullRequestState::Open,
                        PullRequestState::Merged,
                        PullRequestState::Declined,
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            state_filter_label(state),
                            state == current,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_state_filter(state, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_closed_list(&self, cx: &Context<Self>) -> AnyElement {
        let loaded = self
            .closed_pull_requests
            .value
            .as_ref()
            .filter(|(state, _)| *state == self.state_filter)
            .map(|(_, list)| list);
        let Some(list) = loaded else {
            if let Some(error) = self.closed_pull_requests.error.clone() {
                return self
                    .render_centered_message(IconName::Warning, error, None, None, cx)
                    .into_any_element();
            }
            return self.render_loading(t!("repo_hosting.list.loading_pull_requests"), cx);
        };
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let visible: Vec<&PullRequest> = list
            .iter()
            .filter(|pull_request| pull_request_matches(pull_request, &filter))
            .collect();
        if visible.is_empty() {
            return self
                .render_centered_message(
                    IconName::MagnifyingGlass,
                    t!("repo_hosting.list.nothing_here"),
                    None,
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        let dot = match self.state_filter {
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Declined => Color::Error,
            _ => Color::Muted,
        }
        .color(cx);
        v_flex()
            .id("repo-closed-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base08.px(cx))
            .pb(DynamicSpacing::Base08.px(cx))
            .children(
                visible
                    .into_iter()
                    .map(|pull_request| self.render_row(pull_request, dot, false, now, cx)),
            )
            .into_any_element()
    }

    fn render_tab_status<T>(
        &self,
        loadable: &Loadable<T>,
        what: SharedString,
        cx: &App,
    ) -> Option<AnyElement> {
        if loadable.value.is_some() {
            return loadable.error.clone().map(|error| {
                render_inline_error(
                    t!("repo_hosting.list.refresh_failed", error = error).to_string(),
                    cx,
                )
                .into_any_element()
            });
        }
        Some(match &loadable.error {
            Some(error) => self
                .render_centered_message(IconName::Warning, error.clone(), None, None, cx)
                .into_any_element(),
            None => self.render_loading(what, cx),
        })
    }

    fn render_pipelines(&self, cx: &Context<Self>) -> AnyElement {
        let status =
            self.render_tab_status(&self.pipelines, t!("repo_hosting.pipelines.loading"), cx);
        let Some(pipelines) = self.pipelines.value.as_ref() else {
            return status.unwrap_or_else(|| div().into_any_element());
        };
        if pipelines.is_empty() {
            return self
                .render_centered_message(
                    IconName::PlayOutlined,
                    t!("repo_hosting.pipelines.empty"),
                    Some(match self.hosting() {
                        api::Hosting::Bitbucket => t!("repo_hosting.pipelines.empty_bitbucket"),
                        api::Hosting::GitHub => t!("repo_hosting.pipelines.empty_github"),
                        api::Hosting::GitLab => t!("repo_hosting.pipelines.empty_gitlab"),
                    }),
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        v_flex()
            .flex_1()
            .min_h_0()
            .children(status)
            .child(
                v_flex()
                    .id("repo-pipelines")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .children(
                        pipelines
                            .iter()
                            .map(|pipeline| render_pipeline_row(pipeline, now, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_issues(&self, cx: &Context<Self>) -> AnyElement {
        let status = self.render_tab_status(&self.issues, t!("repo_hosting.issues.loading"), cx);
        let Some(issues) = self.issues.value.as_ref() else {
            return status.unwrap_or_else(|| div().into_any_element());
        };
        let Some(issues) = issues else {
            return self
                .render_centered_message(
                    IconName::ListTodo,
                    t!("repo_hosting.issues.disabled", host = self.hosting().name()),
                    Some(t!("repo_hosting.issues.disabled_detail")),
                    None,
                    cx,
                )
                .into_any_element();
        };
        if issues.is_empty() {
            return self
                .render_centered_message(
                    IconName::Check,
                    t!("repo_hosting.issues.empty"),
                    None,
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        v_flex()
            .flex_1()
            .min_h_0()
            .children(status)
            .child(
                v_flex()
                    .id("repo-issues")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .children(issues.iter().map(|issue| render_issue_row(issue, now, cx))),
            )
            .into_any_element()
    }
}

fn state_filter_label(state: PullRequestState) -> SharedString {
    match state {
        PullRequestState::Open => t!("repo_hosting.state_filter.open"),
        PullRequestState::Merged => t!("repo_hosting.state_filter.merged"),
        PullRequestState::Declined => t!("repo_hosting.state_filter.declined"),
        PullRequestState::Superseded => t!("repo_hosting.state_filter.superseded"),
    }
}

/// Which host a web URL is on, for "Abrir no …" labels. Self-hosted GitLab instances can have
/// any host name, so whatever isn't github.com or bitbucket.org is taken for one.
fn host_name(url: &str) -> &'static str {
    if url.starts_with("https://github.com") {
        api::Hosting::GitHub.name()
    } else if url.starts_with("https://bitbucket.org") {
        api::Hosting::Bitbucket.name()
    } else {
        api::Hosting::GitLab.name()
    }
}

/// Errors of the on-demand tabs, with the missing scope spelled out when that's the cause.
fn describe_tab_error(error: &anyhow::Error) -> String {
    if crate::is_forbidden(error) {
        let text = format!("{error:#}");
        if text.contains("GitHub") {
            return t!("repo_hosting.error.github_tab_forbidden").to_string();
        }
        if text.contains("GitLab") {
            return t!(
                "repo_hosting.error.gitlab_tab_forbidden",
                scopes = gitlab::TOKEN_SCOPES.join(", ")
            )
            .to_string();
        }
        let scope = if text.contains("issue") {
            "read:issue:bitbucket"
        } else if text.contains("pipeline") {
            "read:pipeline:bitbucket"
        } else {
            return t!("repo_hosting.error.bitbucket_tab_forbidden_read").to_string();
        };
        return t!("repo_hosting.error.bitbucket_tab_forbidden", scope = scope).to_string();
    }
    format!("{error:#}")
}

fn format_duration(seconds: u64) -> String {
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, seconds) => format!("{minutes}m {seconds}s"),
    }
}

fn render_pipeline_row(pipeline: &api::Pipeline, now: DateTime<Utc>, cx: &App) -> impl IntoElement {
    let (icon, color, spinning) = match pipeline.state {
        api::PipelineState::Passed => (IconName::Check, Color::Success, false),
        api::PipelineState::Failed => (IconName::XCircle, Color::Error, false),
        api::PipelineState::Running => (IconName::LoadCircle, Color::Warning, true),
        api::PipelineState::Pending => (IconName::Clock, Color::Muted, false),
        api::PipelineState::Paused => (IconName::Stop, Color::Warning, false),
        api::PipelineState::Stopped => (IconName::Stop, Color::Muted, false),
    };
    let title = match (&pipeline.ref_name, pipeline.pull_request) {
        (Some(ref_name), _) => ref_name.clone(),
        (None, Some(number)) => format!("PR #{number}"),
        (None, None) => "Pipeline".to_string(),
    };
    let trigger = match pipeline.trigger.as_str() {
        "PUSH" => "push".to_string(),
        "MANUAL" => "manual".to_string(),
        "SCHEDULE" => t!("repo_hosting.pipelines.trigger_scheduled").to_string(),
        other => other.to_lowercase().replace('_', " "),
    };
    let mut meta = vec![format!("#{}", pipeline.number)];
    meta.extend((!trigger.is_empty()).then_some(trigger));
    meta.extend(pipeline.creator.clone());
    meta.extend(
        pipeline
            .created_at
            .map(|created_at| relative_age(created_at, now)),
    );
    let url = pipeline.url.clone();
    h_flex()
        .id(SharedString::from(format!(
            "repo-pipeline-{}",
            pipeline.number
        )))
        .items_start()
        .gap(DynamicSpacing::Base10.px(cx))
        .px(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base06.px(cx))
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .tooltip(Tooltip::text(t!(
            "repo_hosting.common.open_in",
            host = host_name(&url)
        )))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(h_flex().h(DynamicSpacing::Base20.px(cx)).child(spinning_if(
            Icon::new(icon).size(IconSize::Small).color(color),
            spinning,
        )))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(Label::new(title).size(LabelSize::Small).truncate())
                        .when_some(pipeline.commit.as_ref(), |this, commit| {
                            this.child(
                                Label::new(commit.get(..7).unwrap_or(commit).to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Accent),
                            )
                        }),
                )
                .child(
                    Label::new(meta.join(" · "))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .when_some(
            pipeline.duration_seconds.filter(|seconds| *seconds > 0),
            |this, seconds| {
                this.child(
                    h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                        Label::new(format_duration(seconds))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            },
        )
}

fn render_issue_row(issue: &api::Issue, now: DateTime<Utc>, cx: &App) -> impl IntoElement {
    let color = match issue.kind.as_str() {
        "bug" => Color::Error,
        "enhancement" => Color::Info,
        "proposal" => Color::Accent,
        _ => Color::Muted,
    };
    let kind = match issue.kind.as_str() {
        "bug" => t!("repo_hosting.issues.kind_bug"),
        "enhancement" => t!("repo_hosting.issues.kind_enhancement"),
        "proposal" => t!("repo_hosting.issues.kind_proposal"),
        "task" => t!("repo_hosting.issues.kind_task"),
        other => SharedString::from(other.to_string()),
    };
    let mut meta = vec![format!("#{}", issue.number), kind.to_string()];
    if matches!(issue.priority.as_str(), "critical" | "blocker" | "major") {
        meta.push(issue.priority.clone());
    }
    if issue.state == "on hold" {
        meta.push(t!("repo_hosting.issues.on_hold").to_string());
    }
    meta.push(
        issue
            .assignee
            .clone()
            .unwrap_or_else(|| t!("repo_hosting.issues.unassigned").to_string()),
    );
    let url = issue.url.clone();
    h_flex()
        .id(SharedString::from(format!("repo-issue-{}", issue.number)))
        .items_start()
        .gap(DynamicSpacing::Base10.px(cx))
        .px(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base06.px(cx))
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .tooltip(Tooltip::text(t!(
            "repo_hosting.common.open_in",
            host = host_name(&url)
        )))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(
            h_flex()
                .flex_none()
                .h(DynamicSpacing::Base20.px(cx))
                .child(div().size(px(8.)).rounded_full().bg(color.color(cx))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(issue.title.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(
                    Label::new(meta.join(" · "))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .when_some(issue.updated_at, |this, updated_at| {
            this.child(
                h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                    Label::new(compact_age(updated_at, now))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
}

/// Asks for git credentials in a modal of `workspace` when fetching or pushing needs them.
pub(crate) fn askpass_delegate(
    workspace: WeakEntity<Workspace>,
    operation: &'static str,
    window: &Window,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    let window = window.window_handle();
    AskPassDelegate::new_with_cancellation(cx, move |prompt, tx, cancellation, cx| {
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        AskPassModal::new(
                            operation.into(),
                            prompt.into(),
                            tx,
                            cancellation,
                            window,
                            cx,
                        )
                    });
                })
            })
            .log_err();
    })
}

pub(crate) async fn fetch_remote(
    repository: &Entity<Repository>,
    remote_name: &str,
    askpass: AskPassDelegate,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let fetch = repository.update(cx, |repository, cx| {
        repository.fetch(
            FetchOptions::Remote(Remote {
                name: remote_name.to_string().into(),
            }),
            askpass,
            cx,
        )
    });
    fetch.await??;
    Ok(())
}

async fn has_commit(repository: &Entity<Repository>, hash: &str, cx: &mut AsyncApp) -> bool {
    let show = repository.update(cx, |repository, _| repository.show(hash.to_string()));
    matches!(show.await, Ok(Ok(_)))
}

/// The active repository's hosted remote, preferring `origin`, and its checked-out branch as
/// that remote names it.
pub(crate) fn detect_target(project: &Entity<Project>, cx: &App) -> (Target, Option<String>) {
    let Some(repository) = project.read(cx).active_repository(cx) else {
        return (Target::NoRepository, None);
    };
    let repository = repository.read(cx);
    let gitlab_base_urls: Vec<String> = GitHostingProviderSettings::get_global(cx)
        .git_hosting_providers
        .iter()
        .filter(|provider| provider.provider == GitHostingProviderKind::Gitlab)
        .map(|provider| provider.base_url.clone())
        .collect();
    let remotes = [
        ("origin", repository.remote_origin_url.as_deref()),
        ("upstream", repository.remote_upstream_url.as_deref()),
    ];
    for (remote_name, url) in remotes {
        if let Some(hosted) = url.and_then(|url| host::parse_remote(url, &gitlab_base_urls)) {
            let branch = repository
                .branch
                .as_ref()
                .map(|branch| remote_branch_name(branch, remote_name));
            return (
                Target::Hosted {
                    repository: hosted,
                    remote_name: remote_name.to_string(),
                },
                branch,
            );
        }
    }
    let remote_url = remotes
        .into_iter()
        .find_map(|(_, url)| url.map(str::to_string));
    let branch = repository
        .branch
        .as_ref()
        .map(|branch| branch.name().to_string());
    (Target::Unsupported { remote_url }, branch)
}

/// A local branch can track a remote one with another name; pull requests use the remote's.
fn remote_branch_name(branch: &git::repository::Branch, remote_name: &str) -> String {
    branch
        .upstream
        .as_ref()
        .filter(|upstream| upstream.remote_name() == Some(remote_name))
        .and_then(|upstream| upstream.branch_name())
        .unwrap_or_else(|| branch.name())
        .to_string()
}

/// Awaiting your review first, then yours, then the rest, each most recently updated first.
fn group_pull_requests<'a>(
    pull_requests: &[&'a PullRequest],
    user_id: &str,
) -> Vec<(Group, Vec<&'a PullRequest>)> {
    let mut groups: Vec<(Group, Vec<&'a PullRequest>)> = vec![
        (Group::AwaitingYourReview, Vec::new()),
        (Group::Yours, Vec::new()),
        (Group::Others, Vec::new()),
    ];
    for pull_request in pull_requests {
        let group = if pull_request.author.id == user_id {
            1
        } else if pull_request.awaits_review_from(user_id) {
            0
        } else {
            2
        };
        if let Some((_, members)) = groups.get_mut(group) {
            members.push(pull_request);
        }
    }
    for (_, members) in &mut groups {
        members.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    }
    groups.retain(|(_, members)| !members.is_empty());
    groups
}

fn pull_request_matches(pull_request: &PullRequest, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let number = filter.trim_start_matches('#');
    pull_request.title.to_lowercase().contains(filter)
        || pull_request.number.to_string() == number
        || pull_request.source_branch.to_lowercase().contains(filter)
        || pull_request
            .author
            .display_name
            .to_lowercase()
            .contains(filter)
        || pull_request
            .author
            .short_name()
            .to_lowercase()
            .contains(filter)
}

/// The ClickUp task a branch is named after, as ClickUp's own "copy branch name" writes it:
/// `CU-86a1b2` anywhere in the name.
/// The issue a GitHub pull request closes, as "Closes #198" in its description says.
pub(crate) fn closed_issue_number(description: &str) -> Option<u64> {
    const KEYWORDS: [&str; 9] = [
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let words: Vec<&str> = description.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        let keyword = pair[0].trim_end_matches(':').to_lowercase();
        if !KEYWORDS.contains(&keyword.as_str()) {
            return None;
        }
        pair[1]
            .strip_prefix('#')?
            .trim_end_matches(|character: char| !character.is_ascii_digit())
            .parse()
            .ok()
    })
}

fn clickup_task_id(branch: &str) -> Option<String> {
    let lowercase = branch.to_lowercase();
    let start = lowercase.find("cu-")?;
    let id: String = branch[start + 3..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .collect();
    (!id.is_empty()).then(|| format!("CU-{id}"))
}

struct ActivityEvent {
    text: String,
    color: Color,
    url: Option<String>,
}

/// Approvals and check results, newest first, as one-line events under the comments.
fn activity_events(
    pull_request: &PullRequest,
    checks: &[api::Check],
    now: DateTime<Utc>,
) -> Vec<ActivityEvent> {
    let mut events: Vec<(DateTime<Utc>, String, Color, Option<String>)> = Vec::new();
    for participant in &pull_request.participants {
        let Some(at) = participant.participated_at else {
            continue;
        };
        let name = participant.person.short_name();
        if participant.approved {
            events.push((
                at,
                t!("repo_hosting.activity.approved", name = name).to_string(),
                Color::Success,
                None,
            ));
        } else if participant.requested_changes {
            events.push((
                at,
                t!("repo_hosting.gate.changes_requested", name = name).to_string(),
                Color::Error,
                None,
            ));
        }
    }
    for check in checks {
        let Some(at) = check.updated_at else {
            continue;
        };
        let name = &check.name;
        let (text, color) = match check.state {
            CheckState::Passed => (
                t!("repo_hosting.activity.check_passed", name = name),
                Color::Success,
            ),
            CheckState::Failed => (
                t!("repo_hosting.activity.check_failed", name = name),
                Color::Error,
            ),
            CheckState::Running => (
                t!("repo_hosting.activity.check_running", name = name),
                Color::Warning,
            ),
            CheckState::Stopped => (
                t!("repo_hosting.activity.check_stopped", name = name),
                Color::Muted,
            ),
        };
        events.push((at, text.to_string(), color, check.url.clone()));
    }
    events.sort_by_key(|(at, _, _, _)| std::cmp::Reverse(*at));
    events
        .into_iter()
        .map(|(at, text, color, url)| ActivityEvent {
            text: format!("{text} · {}", relative_age(at, now)),
            color,
            url,
        })
        .collect()
}

/// "12 min", "3 h", "5 d": the list's right column.
fn compact_age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - at).num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes} min")
    } else if minutes < 24 * 60 {
        format!("{} h", minutes / 60)
    } else {
        format!("{} d", minutes / (24 * 60))
    }
}

pub(crate) fn relative_age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if (now - at).num_minutes() < 1 {
        t!("repo_hosting.common.now").to_string()
    } else {
        t!("repo_hosting.common.ago", age = compact_age(at, now)).to_string()
    }
}

fn approval_label(approvals: usize, reviewers: usize) -> SharedString {
    match (approvals, reviewers) {
        (_, 0) => match approvals {
            0 => t!("repo_hosting.approval.no_reviewers"),
            1 => t!("repo_hosting.approval.count.one"),
            count => t!("repo_hosting.approval.count.other", count = count),
        },
        (approvals, 1) => t!(
            "repo_hosting.approval.of_reviewers.one",
            approvals = approvals
        ),
        (approvals, reviewers) => t!(
            "repo_hosting.approval.of_reviewers.other",
            approvals = approvals,
            reviewers = reviewers
        ),
    }
}

fn check_summary_tone(summary: CheckSummary) -> StatusTone {
    if summary.failed > 0 {
        StatusTone::Error
    } else if summary.running > 0 || summary.passed < summary.total {
        StatusTone::Warning
    } else {
        StatusTone::Success
    }
}

fn approval_tone(approvals: usize, reviewers: usize) -> StatusTone {
    if reviewers == 0 {
        StatusTone::Neutral
    } else if approvals >= reviewers {
        StatusTone::Success
    } else {
        StatusTone::Warning
    }
}

fn state_tone(pull_request: &PullRequest) -> (SharedString, StatusTone) {
    match pull_request.state {
        PullRequestState::Open if pull_request.draft => {
            (t!("repo_hosting.common.draft"), StatusTone::Neutral)
        }
        PullRequestState::Open => (t!("repo_hosting.state.open"), StatusTone::Info),
        PullRequestState::Merged => (t!("repo_hosting.state.merged"), StatusTone::Info),
        PullRequestState::Declined => (t!("repo_hosting.state.declined"), StatusTone::Error),
        PullRequestState::Superseded => (t!("repo_hosting.state.superseded"), StatusTone::Neutral),
    }
}

pub(crate) fn avatar(name: &str, color: Color, cx: &App) -> gpui::Div {
    h_flex()
        .flex_none()
        .size(px(18.))
        .justify_center()
        .rounded_full()
        .bg(color.color(cx).opacity(0.16))
        .child(
            Label::new(initials(name))
                .size(LabelSize::XSmall)
                .weight(FontWeight::SEMIBOLD)
                .color(color),
        )
}

const REVIEW_PROMPT: &str = "Revise este PR: leia o diff com repo_pr_diff e aponte bugs, riscos \
     e melhorias com arquivo:linha. Não publique nada no PR sem eu pedir.";
const FIX_CHECKS_PROMPT: &str = "Veja quais checks deste PR falharam e por quê, corrija no código e \
     rode os testes localmente. Não faça commit nem push sem eu confirmar.";
const RESOLVE_COMMENT_PROMPT: &str = "Resolva no código o que este comentário pede. Não responda no \
     PR sem eu pedir.";

/// Puts the pull request, or one of its comments or files, into the agent panel. With a prompt
/// the agent starts on it right away in a thread of its own.
pub(crate) fn mention_action(
    pull_request: &PullRequest,
    comment_id: Option<u64>,
    file_path: Option<String>,
    prompt: Option<&str>,
) -> zed_actions::agent::MentionPullRequest {
    zed_actions::agent::MentionPullRequest {
        number: pull_request.number,
        title: pull_request.title.clone(),
        comment_id,
        file_path,
        prompt: prompt.map(str::to_string),
        submit: prompt.is_some(),
    }
}

fn pull_request_menu(
    menu: ContextMenu,
    pull_request: &PullRequest,
    panel: WeakEntity<RepoPanel>,
) -> ContextMenu {
    let mention = mention_action(pull_request, None, None, None);
    let review = mention_action(pull_request, None, None, Some(REVIEW_PROMPT));
    let fix_checks = mention_action(pull_request, None, None, Some(FIX_CHECKS_PROMPT));
    let number = pull_request.number;
    let url = pull_request.url.clone();
    let copied_url = pull_request.url.clone();
    let checkout_panel = panel.clone();
    menu.entry(
        t!("repo_hosting.common.mention_in_agent"),
        None,
        move |window, cx| window.dispatch_action(mention.boxed_clone(), cx),
    )
    .entry(
        t!("repo_hosting.common.review_with_agent"),
        None,
        move |window, cx| window.dispatch_action(review.boxed_clone(), cx),
    )
    .entry(
        t!("repo_hosting.menu.fix_checks"),
        None,
        move |window, cx| window.dispatch_action(fix_checks.boxed_clone(), cx),
    )
    .separator()
    .entry(
        t!("repo_hosting.detail.checkout"),
        None,
        move |window, cx| {
            checkout_panel
                .update(cx, |panel, cx| {
                    panel.show_pull_request(number, PullRequestFollowup::Checkout, window, cx)
                })
                .log_err();
        },
    )
    .entry(
        t!("repo_hosting.menu.open_diff"),
        None,
        move |window, cx| {
            panel
                .update(cx, |panel, cx| {
                    panel.show_pull_request(number, PullRequestFollowup::Diff, window, cx)
                })
                .log_err();
        },
    )
    .entry(
        t!("repo_hosting.common.open_in", host = host_name(&url)),
        None,
        move |_, cx| cx.open_url(&url),
    )
    .entry(t!("repo_hosting.common.copy_link"), None, move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(copied_url.clone()))
    })
}

fn render_comment(
    comment: &api::Comment,
    pull_request: &PullRequest,
    now: DateTime<Utc>,
    cx: &App,
) -> impl IntoElement {
    let mention = mention_action(pull_request, Some(comment.id), None, None);
    let resolve = mention_action(
        pull_request,
        Some(comment.id),
        None,
        Some(RESOLVE_COMMENT_PROMPT),
    );
    let mut meta = Vec::new();
    if let Some((path, line)) = &comment.inline {
        let file = path.rsplit('/').next().unwrap_or(path);
        meta.push(match line {
            Some(line) => format!("{file}:{line}"),
            None => file.to_string(),
        });
    }
    if let Some(created_at) = comment.created_at {
        meta.push(relative_age(created_at, now));
    }
    h_flex()
        .id(SharedString::from(format!(
            "repo-pr-comment-{}",
            comment.id
        )))
        .items_start()
        .gap(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base04.px(cx))
        .when(comment.is_reply, |this| {
            this.pl(DynamicSpacing::Base24.px(cx))
        })
        .child(avatar(&comment.author.display_name, Color::Info, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(
                            Label::new(comment.author.short_name().to_string())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(meta.join(" · "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .child(Label::new(comment.body.trim().to_string()).size(LabelSize::Small))
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base02.px(cx))
                        .child(
                            Button::new(("repo-pr-comment-mention", comment.id), "@ Agent")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text(t!("repo_hosting.detail.mention_comment")))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(mention.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            Button::new(
                                ("repo-pr-comment-resolve", comment.id),
                                t!("repo_hosting.detail.ask_agent_to_resolve"),
                            )
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(resolve.boxed_clone(), cx)
                            }),
                        ),
                ),
        )
}

fn warning_box(cx: &App) -> Div {
    let warning = cx.theme().status().warning;
    h_flex()
        .w_full()
        .gap_2()
        .p(px(10.))
        .rounded(px(10.))
        .border_1()
        .border_color(warning.opacity(0.2))
        .bg(warning.opacity(0.08))
}

fn render_warning(title: SharedString, detail: SharedString, cx: &App) -> impl IntoElement {
    warning_box(cx)
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Warning),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

fn render_inline_error(message: String, cx: &App) -> impl IntoElement {
    div().px_2().pb_1p5().child(
        warning_box(cx)
            .p_2()
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(message)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            ),
    )
}

fn comment_count_meta(count: u64) -> impl IntoElement {
    h_flex()
        .gap_0p5()
        .child(
            Icon::new(IconName::Chat)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(count.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

fn spinning_if(icon: Icon, spinning: bool) -> AnyElement {
    if spinning {
        icon.with_rotate_animation(2).into_any_element()
    } else {
        icon.into_any_element()
    }
}

fn initials(name: &str) -> String {
    name.split(|character: char| character.is_whitespace() || character == '.')
        .filter_map(|word| word.chars().next())
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

impl Render for RepoPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.target.clone() {
            Target::NoRepository => self
                .render_centered_message(
                    IconName::GitBranch,
                    t!("repo_hosting.panel.no_repository"),
                    Some(t!("repo_hosting.panel.no_repository_detail")),
                    None,
                    cx,
                )
                .into_any_element(),
            Target::Unsupported { remote_url } => self
                .render_centered_message(
                    IconName::PullRequest,
                    t!("repo_hosting.panel.unsupported_remote"),
                    Some(match remote_url {
                        Some(url) => t!("repo_hosting.panel.unsupported_remote_detail", url = url),
                        None => t!("repo_hosting.panel.no_origin"),
                    }),
                    None,
                    cx,
                )
                .into_any_element(),
            Target::Hosted { repository, .. } => {
                match self.store.read(cx).connection(repository.hosting) {
                    Connection::Loading => {
                        self.render_loading(t!("repo_hosting.panel.loading_account"), cx)
                    }
                    Connection::Disconnected => self
                        .render_connect(&repository, false, cx)
                        .into_any_element(),
                    Connection::Rejected => self
                        .render_connect(&repository, true, cx)
                        .into_any_element(),
                    Connection::Connected(account)
                        if self.replacing_token || !account.credentials.serves(&repository) =>
                    {
                        self.render_connect(&repository, false, cx)
                            .into_any_element()
                    }
                    Connection::Connected(account) => {
                        let account = account.clone();
                        self.render_connected(&repository, &account, cx)
                    }
                    Connection::Unreachable { message } => {
                        let message = message.clone();
                        let store = self.store.clone();
                        let hosting = repository.hosting;
                        let actions = h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .child(
                                Button::new("repo-retry", t!("repo_hosting.panel.retry"))
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, _, cx| {
                                        store.update(cx, |store, cx| store.retry(hosting, cx))
                                    }),
                            )
                            .child(
                                Button::new(
                                    "repo-replace-unreachable",
                                    t!("repo_hosting.account.replace_token"),
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.start_replacing_token(window, cx);
                                        let hosting = this.hosting();
                                        this.store
                                            .update(cx, |store, cx| store.disconnect(hosting, cx))
                                            .detach_and_log_err(cx);
                                    },
                                )),
                            )
                            .into_any_element();
                        self.render_centered_message(
                            IconName::Disconnected,
                            t!(
                                "repo_hosting.panel.unreachable",
                                host = repository.hosting.name()
                            ),
                            Some(message),
                            Some(actions),
                            cx,
                        )
                        .into_any_element()
                    }
                }
            }
        };

        v_flex()
            .key_context("RepoPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::mention_selected))
            .size_full()
            .child(content)
    }
}

impl Focusable for RepoPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RepoPanel {}

impl Panel for RepoPanel {
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::REPO, &self.project, cx)
    }

    fn persistent_name() -> &'static str {
        "RepoPanel"
    }

    fn panel_key() -> &'static str {
        "RepoPanel"
    }

    /// Until an account is connected, filling in the token is the only thing to do here.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        let Target::Hosted { repository, .. } = &self.target else {
            return self.focus_handle.clone();
        };
        match self.store.read(cx).connection(repository.hosting) {
            Connection::Connected(account)
                if !self.replacing_token && account.credentials.serves(repository) =>
            {
                self.focus_handle.clone()
            }
            Connection::Disconnected | Connection::Rejected | Connection::Connected(_) => {
                if repository.hosting == api::Hosting::Bitbucket
                    && self.email_input.read(cx).is_empty(cx)
                {
                    self.email_input.focus_handle(cx)
                } else {
                    self.token_input(repository.hosting).focus_handle(cx)
                }
            }
            Connection::Loading | Connection::Unreachable { .. } => self.focus_handle.clone(),
        }
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Repo")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Participant, ParticipantRole, Person};

    fn person(id: &str) -> Person {
        Person {
            id: id.to_string(),
            display_name: id.to_string(),
            nickname: None,
        }
    }

    fn pull_request(number: u64, author: &str, reviewers: &[(&str, bool)]) -> PullRequest {
        PullRequest {
            number,
            title: format!("PR {number}"),
            state: PullRequestState::Open,
            draft: false,
            author: person(author),
            source_branch: format!("feat/{number}"),
            source_repository: None,
            destination_branch: "develop".to_string(),
            description: String::new(),
            comment_count: 0,
            open_task_count: 0,
            updated_at: DateTime::from_timestamp(1_790_000_000 + number as i64, 0),
            url: String::new(),
            participants: reviewers
                .iter()
                .map(|(id, approved)| Participant {
                    person: person(id),
                    role: ParticipantRole::Reviewer,
                    approved: *approved,
                    requested_changes: false,
                    participated_at: None,
                })
                .collect(),
        }
    }

    #[test]
    fn reads_local_commits_and_numstat() {
        let commits = parse_local_commits(
            "e41c9a2ff\u{1f}Ana Souza\u{1f}2026-11-03T10:00:00-03:00\u{1f}LoginScreen: Face ID\n\
             7b03d1fee\u{1f}Ana Souza\u{1f}not a date\u{1f}Guarda o token\n",
        );
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].summary, "LoginScreen: Face ID");
        assert_eq!(commits[0].date, DateTime::from_timestamp(1_793_710_800, 0));
        assert_eq!(commits[1].date, None);

        assert_eq!(
            parse_numstat("10\t2\tsrc/a.ts\n-\t-\tlogo.png\n0\t5\tsrc/b.ts\n"),
            DiffSummary {
                added: 10,
                removed: 7,
                files: 3,
            }
        );
    }

    #[test]
    fn groups_review_requests_before_your_own() {
        let pull_requests = [
            pull_request(1, "ana", &[("me", false)]),
            pull_request(2, "me", &[("ana", true)]),
            pull_request(3, "rafael", &[("me", true)]),
            pull_request(4, "joao", &[]),
            pull_request(5, "ana", &[("me", false)]),
        ];
        let references: Vec<&PullRequest> = pull_requests.iter().collect();
        let groups: Vec<(Group, Vec<u64>)> = group_pull_requests(&references, "me")
            .into_iter()
            .map(|(group, members)| {
                (
                    group,
                    members
                        .iter()
                        .map(|pull_request| pull_request.number)
                        .collect(),
                )
            })
            .collect();
        assert!(
            groups
                == vec![
                    (Group::AwaitingYourReview, vec![5, 1]),
                    (Group::Yours, vec![2]),
                    (Group::Others, vec![4, 3]),
                ]
        );
    }

    #[test]
    fn matches_pull_requests_by_the_upstream_branch_name() {
        let branch = |upstream: Option<&str>| git::repository::Branch {
            is_head: true,
            ref_name: "refs/heads/biometria".into(),
            upstream: upstream.map(|ref_name| git::repository::Upstream {
                ref_name: ref_name.to_string().into(),
                tracking: git::repository::UpstreamTracking::Gone,
            }),
            most_recent_commit: None,
        };
        assert_eq!(
            remote_branch_name(
                &branch(Some("refs/remotes/origin/feat/CU-86a1b2-biometria")),
                "origin"
            ),
            "feat/CU-86a1b2-biometria"
        );
        assert_eq!(
            remote_branch_name(&branch(Some("refs/remotes/fork/outro")), "origin"),
            "biometria"
        );
        assert_eq!(remote_branch_name(&branch(None), "origin"), "biometria");
    }

    #[test]
    fn fills_the_title_and_description_from_the_branch() {
        assert_eq!(
            humanize_branch("feat/CU-86a1b2-login-com-biometria").as_deref(),
            Some("Login com biometria")
        );
        assert_eq!(
            humanize_branch("fix_rota_inicial").as_deref(),
            Some("Fix rota inicial")
        );
        assert_eq!(humanize_branch("feat/"), None);

        let subjects = ["Instala expo".to_string(), "Adiciona hook".to_string()];
        assert_eq!(
            pull_request_description(&subjects, "feat/CU-86a1b2-biometria"),
            "- Instala expo\n- Adiciona hook\n\nCloses CU-86a1b2."
        );
        assert_eq!(pull_request_description(&subjects[..1], "fix/x"), "");
    }

    #[test]
    fn only_conflicts_block_the_merge() {
        let mut pull_request = pull_request(157, "me", &[("ana", true), ("rafael", false)]);
        pull_request.open_task_count = 1;
        let detail = |change| PullRequestDetail {
            pull_request: pull_request.clone(),
            checks: Vec::new(),
            files: vec![api::ChangedFile {
                path: "a.ts".to_string(),
                old_path: None,
                change,
                lines_added: 1,
                lines_removed: 0,
            }],
            comments: Vec::new(),
            commits: Vec::new(),
            has_conflicts: false,
            auto_merge: false,
        };
        let levels = |detail: &PullRequestDetail| {
            merge_gates(&pull_request, Some(detail))
                .into_iter()
                .map(|gate| gate.level)
                .max()
        };
        assert_eq!(
            levels(&detail(FileChange::Modified)),
            Some(GateLevel::Warning)
        );
        assert_eq!(
            levels(&detail(FileChange::Conflict)),
            Some(GateLevel::Blocking)
        );
    }

    #[test]
    fn finds_the_issue_a_pull_request_closes() {
        assert_eq!(
            closed_issue_number("Sincroniza.\n\nCloses #198."),
            Some(198)
        );
        assert_eq!(closed_issue_number("fixes: #7"), Some(7));
        assert_eq!(closed_issue_number("Relacionado a #12"), None);
    }

    #[test]
    fn finds_the_clickup_task_in_the_branch() {
        assert_eq!(
            clickup_task_id("feat/CU-86a1b2-biometria").as_deref(),
            Some("CU-86a1b2")
        );
        assert_eq!(clickup_task_id("cu-9x_fix").as_deref(), Some("CU-9x"));
        assert_eq!(clickup_task_id("feat/login"), None);
        assert_eq!(clickup_task_id("feat/cu-"), None);
    }

    #[test]
    fn filters_by_title_number_or_author() {
        let pull_request = pull_request(157, "ana.souza", &[]);
        assert!(pull_request_matches(&pull_request, "#157"));
        assert!(pull_request_matches(&pull_request, "157"));
        assert!(pull_request_matches(&pull_request, "ana"));
        assert!(pull_request_matches(&pull_request, "pr 1"));
        assert!(!pull_request_matches(&pull_request, "#15"));
    }

    #[test]
    fn labels_ages_and_approvals() {
        let now = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        assert_eq!(
            compact_age(now - chrono::Duration::minutes(12), now),
            "12 min"
        );
        assert_eq!(compact_age(now - chrono::Duration::hours(3), now), "3 h");
        assert_eq!(compact_age(now - chrono::Duration::days(5), now), "5 d");
        assert_eq!(relative_age(now, now), "now");
        assert_eq!(approval_label(1, 2), "1 of 2 approvals");
        assert_eq!(approval_label(0, 0), "no reviewers");
        assert_eq!(initials("ana.souza"), "AS");
        assert_eq!(initials("Rafael M"), "RM");
    }
}
