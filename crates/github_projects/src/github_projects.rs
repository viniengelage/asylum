//! GitHub Projects in the editor: the Tarefas dock, with the item of the checked-out branch on
//! top, and the project's board as a tab. Uses the GitHub account of the Repo dock.

mod agent_toolkit;
pub mod api;
mod board;
mod flow_steps;
mod panel;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use board::ProjectBoard;
pub use panel::TasksPanel;

use anyhow::{Context as _, Result};
use db::kvp::KeyValueStore;
use gpui::{Entity, Global, Subscription, Task, WeakEntity, actions};
use http_client::HttpClient;
use project::Project;
use repo_hosting::{
    Connection, RepoStore,
    api::{Hosting, RemoteRepository},
    github::Credentials,
};
use settings::SettingsStore;
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    AppState, HideStatusItem, ItemHandle, StatusItemView, Toast, Workspace, dock::StatusBarButton,
    notifications::NotificationId,
};

actions!(
    github_projects,
    [
        /// Opens the Tarefas dock with the GitHub project of the repository.
        ToggleFocus,
        /// Opens the project's board in a tab.
        OpenBoard,
    ]
);

/// A GraphQL read of a whole project costs a few points of the 5,000 an hour; once a minute
/// keeps several windows far below it.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<TasksPanel>(cx);
    flow_steps::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        agent_toolkit::prefetch(workspace.project().clone(), cx);
        workspace.register_action(
            |workspace, action: &zed_actions::github_projects::OpenProjectItem, window, cx| {
                if !toolkit_allowed(workspace, cx) {
                    return;
                }
                board::open(workspace, Some(action.item_id.clone()), window, cx);
            },
        );
        workspace.register_action(
            |workspace, action: &zed_actions::github_projects::StartProjectItem, window, cx| {
                start_from_workspace(workspace, action.item_id.clone(), window, cx);
            },
        );
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            if !toolkit_allowed(workspace, cx) {
                return;
            }
            if workspace.panel::<TasksPanel>(cx).is_none() {
                open(workspace, window, cx);
            } else {
                workspace.toggle_panel_focus::<TasksPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &OpenBoard, window, cx| {
            if !toolkit_allowed(workspace, cx) {
                return;
            }
            board::open(workspace, None, window, cx);
        });
    })
    .detach();
}

/// Whether the GitHub Projects toolkit is on for this workspace's project; says so when it isn't.
fn toolkit_allowed(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    if task_agents::toolkit_enabled(
        task_agents::toolkit_ids::GITHUB_PROJECTS,
        workspace.project(),
        cx,
    ) {
        return true;
    }
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<TasksPanel>(),
            task_agents::toolkit_disabled_message("GitHub Projects"),
        )
        .autohide(),
        cx,
    );
    false
}

/// Adds the dock the first time it is asked for.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<TasksPanel>(cx).is_none() {
        let project = workspace.project().clone();
        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| TasksPanel::new(project, weak_workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<TasksPanel>(window, cx);
}

/// One [`RepositoryProjects`] per GitHub repository, shared by every window that has it open.
#[derive(Default)]
struct ProjectsStore {
    repositories: HashMap<RemoteRepository, Entity<RepositoryProjects>>,
}

impl Global for ProjectsStore {}

/// The GitHub repository `project` is on, with its remote's name and the checked-out branch.
pub fn github_repository(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(RemoteRepository, String, Option<String>)> {
    repo_hosting::hosted_repository(project, cx)
        .filter(|(repository, _, _)| repository.hosting == Hosting::GitHub)
}

pub fn repository_projects(
    repository: &RemoteRepository,
    cx: &mut App,
) -> Entity<RepositoryProjects> {
    if let Some(state) = cx
        .try_global::<ProjectsStore>()
        .and_then(|store| store.repositories.get(repository))
    {
        return state.clone();
    }
    let repo_store = RepoStore::global(cx);
    let state = cx.new(|cx| RepositoryProjects::new(repository.clone(), repo_store, cx));
    cx.default_global::<ProjectsStore>()
        .repositories
        .insert(repository.clone(), state.clone());
    state
}

/// What the dock and the board show before there is a project to show.
#[derive(Clone, PartialEq)]
pub enum Availability {
    /// The Repo dock has no GitHub account in this profile.
    NotConnected,
    Loading,
    /// None of the projects linked to the repository, its owner or the account is open.
    NoProjects,
    /// The token can't read projects: a classic token without `read:project`, or a
    /// fine-grained one without Projects.
    MissingScope,
    Failed(SharedString),
    Ready,
}

/// The projects of one GitHub repository and the selected one's items, kept in sync.
pub struct RepositoryProjects {
    repository: RemoteRepository,
    repo_store: Entity<RepoStore>,
    info: Option<api::Repository>,
    selected_project_id: Option<String>,
    project: Option<api::Project>,
    is_syncing: bool,
    synced_at: Option<Instant>,
    error: Option<SharedString>,
    missing_scope: bool,
    /// Items whose change was sent and not confirmed by a sync yet.
    pending_items: HashSet<String>,
    sync_task: Option<Task<()>>,
    /// Set when syncing stopped because no window of this repository has GitHub Projects on,
    /// so turning it back on resumes it.
    sync_paused: bool,
    _repo_store_subscription: Subscription,
    _settings_subscription: Subscription,
}

impl RepositoryProjects {
    fn new(
        repository: RemoteRepository,
        repo_store: Entity<RepoStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&repo_store, |this, _, cx| {
            if this.client(cx).is_some() {
                if this.sync_task.is_none() {
                    this.sync(cx);
                }
            } else {
                this.sync_task = None;
            }
            cx.notify();
        });
        let selected_project_id = KeyValueStore::global(cx)
            .read_kvp(&selection_key(&repository))
            .log_err()
            .flatten();
        let mut this = Self {
            repository,
            repo_store,
            info: None,
            selected_project_id,
            project: None,
            is_syncing: false,
            synced_at: None,
            error: None,
            missing_scope: false,
            pending_items: HashSet::default(),
            sync_task: None,
            sync_paused: false,
            _repo_store_subscription: subscription,
            _settings_subscription: cx.observe_global::<SettingsStore>(|this: &mut Self, cx| {
                if this.sync_paused && this.wanted_by_any_window(cx) {
                    this.sync(cx);
                }
            }),
        };
        this.sync(cx);
        this
    }

    pub fn repository(&self) -> &RemoteRepository {
        &self.repository
    }

    /// Shared by every window on the repository, so it keeps syncing while any of them has
    /// GitHub Projects on.
    fn wanted_by_any_window(&self, cx: &App) -> bool {
        AppState::any_workspace(cx, |workspace, cx| {
            let project = workspace.project();
            task_agents::toolkit_enabled(task_agents::toolkit_ids::GITHUB_PROJECTS, project, cx)
                && github_repository(project, cx)
                    .is_some_and(|(repository, _, _)| repository == self.repository)
        })
    }

    pub fn info(&self) -> Option<&api::Repository> {
        self.info.as_ref()
    }

    pub fn project(&self) -> Option<&api::Project> {
        self.project.as_ref()
    }

    pub fn projects(&self) -> &[api::ProjectSummary] {
        self.info
            .as_ref()
            .map_or(&[], |info| info.projects.as_slice())
    }

    pub fn viewer_login(&self) -> Option<&str> {
        self.info.as_ref().map(|info| info.viewer_login.as_str())
    }

    pub fn is_syncing(&self) -> bool {
        self.is_syncing
    }

    pub fn synced_at(&self) -> Option<Instant> {
        self.synced_at
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn is_pending(&self, item_id: &str) -> bool {
        self.pending_items.contains(item_id)
    }

    pub fn availability(&self, cx: &App) -> Availability {
        if !matches!(
            self.repo_store.read(cx).connection(Hosting::GitHub),
            Connection::Connected(_) | Connection::Loading
        ) {
            return Availability::NotConnected;
        }
        if self.missing_scope {
            return Availability::MissingScope;
        }
        if self.project.is_some() {
            return Availability::Ready;
        }
        if let Some(error) = &self.error {
            return Availability::Failed(error.clone());
        }
        if self
            .info
            .as_ref()
            .is_some_and(|info| info.projects.is_empty())
        {
            return Availability::NoProjects;
        }
        Availability::Loading
    }

    fn client(&self, cx: &App) -> Option<(Arc<dyn HttpClient>, Credentials)> {
        self.repo_store.read(cx).github_client()
    }

    /// Syncs again after the token was refused for Projects, first picking up the token
    /// `gh auth refresh -s project` just issued.
    pub fn retry_with_fresh_token(&mut self, cx: &mut Context<Self>) {
        let adopt = self
            .repo_store
            .update(cx, |store, cx| store.adopt_github_cli_token(cx));
        self.is_syncing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Err(error) = adopt.await {
                log::info!("Tarefas: token do gh não aproveitado: {error:#}");
            }
            this.update(cx, |this, cx| this.sync(cx))
        })
        .detach_and_log_err(cx);
    }

    /// Syncs now and then every [`SYNC_INTERVAL`].
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        let Some((http_client, credentials)) = self.client(cx) else {
            self.sync_task = None;
            return;
        };
        let repository = self.repository.clone();
        self.sync_paused = false;
        self.is_syncing = true;
        cx.notify();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok((info, selected)) = this.read_with(cx, |this, _| {
                    (this.info.clone(), this.selected_project_id.clone())
                }) else {
                    return;
                };
                let result = cx
                    .background_spawn({
                        let http_client = http_client.clone();
                        let credentials = credentials.clone();
                        let repository = repository.clone();
                        async move {
                            let info = match info {
                                Some(info) => info,
                                None => {
                                    api::get_repository(
                                        &http_client,
                                        &credentials,
                                        &repository.owner,
                                        &repository.name,
                                    )
                                    .await?
                                }
                            };
                            let summary = selected
                                .as_deref()
                                .and_then(|id| {
                                    info.projects.iter().find(|project| project.id == id)
                                })
                                .or_else(|| info.projects.first())
                                .cloned();
                            let project = match &summary {
                                Some(summary) => Some(
                                    api::get_project(&http_client, &credentials, summary).await?,
                                ),
                                None => None,
                            };
                            anyhow::Ok((info, project))
                        }
                    })
                    .await;
                let keep_syncing = this
                    .update(cx, |this, cx| {
                        let keep_syncing = this.finish_sync(result, cx);
                        if keep_syncing && !this.wanted_by_any_window(cx) {
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
                    this.is_syncing = true;
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    fn finish_sync(
        &mut self,
        result: Result<(api::Repository, Option<api::Project>)>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.is_syncing = false;
        let keep_syncing = match result {
            Ok((info, project)) => {
                self.info = Some(info);
                self.project = project;
                self.pending_items.clear();
                self.synced_at = Some(Instant::now());
                self.error = None;
                self.missing_scope = false;
                true
            }
            Err(error) if repo_hosting::is_unauthorized(&error) => {
                self.repo_store
                    .update(cx, |store, cx| store.mark_rejected(Hosting::GitHub, cx));
                false
            }
            Err(error) if repo_hosting::is_forbidden(&error) => {
                self.missing_scope = true;
                false
            }
            Err(error) => {
                log::warn!("Tarefas: falha ao sincronizar o GitHub Projects: {error:#}");
                self.error = Some(format!("{error:#}").into());
                true
            }
        };
        cx.notify();
        keep_syncing
    }

    pub fn select_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        if self.selected_project_id.as_deref() == Some(project_id.as_str())
            && self.project.is_some()
        {
            return;
        }
        let key = selection_key(&self.repository);
        let kvp = KeyValueStore::global(cx);
        let value = project_id.clone();
        cx.background_spawn(async move { kvp.write_kvp(key, value).await })
            .detach_and_log_err(cx);
        self.selected_project_id = Some(project_id);
        self.project = None;
        self.sync(cx);
    }

    /// Sends a field change, showing it right away and undoing it if GitHub refuses.
    pub fn set_field(
        &mut self,
        item_id: String,
        field_id: String,
        value: api::NewValue,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(project) = &mut self.project else {
            return Task::ready(Err(anyhow::anyhow!("nenhum projeto carregado")));
        };
        let project_id = project.summary.id.clone();
        let Some(item) = project.items.iter_mut().find(|item| item.id == item_id) else {
            return Task::ready(Err(anyhow::anyhow!("o item não está mais no projeto")));
        };
        let previous = item.values.get(&field_id).cloned();
        match &value {
            api::NewValue::Option(option) => {
                item.values
                    .insert(field_id.clone(), api::FieldValue::Option(option.clone()));
            }
            api::NewValue::Iteration(iteration) => {
                item.values.insert(
                    field_id.clone(),
                    api::FieldValue::Iteration(iteration.clone()),
                );
            }
            api::NewValue::Number(number) => {
                item.values
                    .insert(field_id.clone(), api::FieldValue::Number(*number));
            }
            api::NewValue::Clear => {
                item.values.remove(&field_id);
            }
        }
        self.pending_items.insert(item_id.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let item_id = item_id.clone();
                    let field_id = field_id.clone();
                    async move {
                        api::set_field(
                            &http_client,
                            &credentials,
                            &project_id,
                            &item_id,
                            &field_id,
                            value,
                        )
                        .await
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.pending_items.remove(&item_id);
                if result.is_err()
                    && let Some(item) = this.project.as_mut().and_then(|project| {
                        project.items.iter_mut().find(|item| item.id == item_id)
                    })
                {
                    match previous {
                        Some(previous) => {
                            item.values.insert(field_id, previous);
                        }
                        None => {
                            item.values.remove(&field_id);
                        }
                    }
                }
                cx.notify();
            })?;
            result
        })
    }

    /// Moves an item to a column of the board.
    pub fn move_item(
        &mut self,
        item_id: String,
        option_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(field_id) = self
            .project
            .as_ref()
            .and_then(|project| project.status.as_ref())
            .map(|field| field.id.clone())
        else {
            return Task::ready(Err(anyhow::anyhow!("o projeto não tem campo Status")));
        };
        self.set_field(item_id, field_id, api::NewValue::Option(option_id), cx)
    }

    pub fn assign_me(&mut self, item_id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(info) = &self.info else {
            return Task::ready(Err(anyhow::anyhow!("o repositório ainda não carregou")));
        };
        let viewer_id = info.viewer_id.clone();
        let viewer_login = info.viewer_login.clone();
        let Some(item) = self
            .project
            .as_mut()
            .and_then(|project| project.items.iter_mut().find(|item| item.id == item_id))
        else {
            return Task::ready(Err(anyhow::anyhow!("o item não está mais no projeto")));
        };
        if item.is_assigned_to(&viewer_login) {
            return Task::ready(Ok(()));
        }
        item.assignees.push(viewer_login);
        let content_id = item.content_id.clone();
        cx.notify();
        cx.background_spawn(async move {
            api::add_assignee(&http_client, &credentials, &content_id, &viewer_id).await
        })
    }

    pub fn comment(
        &mut self,
        item_id: &str,
        body: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(item) = self
            .project
            .as_ref()
            .and_then(|project| project.item(item_id))
        else {
            return Task::ready(Err(anyhow::anyhow!("o item não está mais no projeto")));
        };
        if item.kind == api::ItemKind::Draft {
            return Task::ready(Err(anyhow::anyhow!("rascunhos não têm comentários")));
        }
        let content_id = item.content_id.clone();
        cx.background_spawn(async move {
            api::add_comment(&http_client, &credentials, &content_id, &body).await
        })
    }

    pub fn toggle_subtask(
        &mut self,
        item_id: &str,
        line: usize,
        done: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(item) = self
            .project
            .as_mut()
            .and_then(|project| project.items.iter_mut().find(|item| item.id == item_id))
        else {
            return Task::ready(Err(anyhow::anyhow!("o item não está mais no projeto")));
        };
        item.body = api::toggle_subtask(&item.body, line, done);
        let item = item.clone();
        cx.notify();
        cx.background_spawn(async move {
            api::update_body(&http_client, &credentials, &item, &item.body).await
        })
    }

    pub fn add_draft(&mut self, title: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(project_id) = self
            .project
            .as_ref()
            .map(|project| project.summary.id.clone())
        else {
            return Task::ready(Err(anyhow::anyhow!("nenhum projeto carregado")));
        };
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                api::add_draft(&http_client, &credentials, &project_id, &title).await
            })
            .await?;
            this.update(cx, |this, cx| this.sync(cx))
        })
    }

    /// "Começar": creates the item's branch on GitHub linked to it, moves it to the column for
    /// work in progress and assigns it to you. Checking the branch out is up to the caller,
    /// which has a window for git's credential prompts.
    pub fn start(
        &mut self,
        item_id: String,
        branch: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((http_client, credentials)) = self.client(cx) else {
            return Task::ready(Err(anyhow::anyhow!("o GitHub não está conectado")));
        };
        let Some(info) = self.info.clone() else {
            return Task::ready(Err(anyhow::anyhow!("o repositório ainda não carregou")));
        };
        let Some(project) = &self.project else {
            return Task::ready(Err(anyhow::anyhow!("nenhum projeto carregado")));
        };
        let Some(item) = project.item(&item_id).cloned() else {
            return Task::ready(Err(anyhow::anyhow!("o item não está mais no projeto")));
        };
        let in_progress = project.in_progress_option().map(|option| option.id.clone());
        cx.spawn(async move |this, cx| {
            if item.kind == api::ItemKind::Issue && !item.linked_branches.contains(&branch) {
                let oid = info
                    .default_branch_oid
                    .clone()
                    .context("o repositório não tem branch padrão")?;
                cx.background_spawn({
                    let http_client = http_client.clone();
                    let credentials = credentials.clone();
                    let content_id = item.content_id.clone();
                    let repository_id = info.id.clone();
                    let branch = branch.clone();
                    async move {
                        api::create_linked_branch(
                            &http_client,
                            &credentials,
                            &content_id,
                            &repository_id,
                            &oid,
                            &branch,
                        )
                        .await
                    }
                })
                .await?;
            }
            let moved = match in_progress {
                Some(option_id) => Some(this.update(cx, |this, cx| {
                    this.move_item(item.id.clone(), option_id, cx)
                })?),
                None => None,
            };
            let assigned = this.update(cx, |this, cx| this.assign_me(&item.id, cx))?;
            if let Some(moved) = moved {
                moved.await?;
            }
            assigned.await?;
            // The branch is known before the next sync brings it.
            this.update(cx, |this, cx| {
                if let Some(item) = this
                    .project
                    .as_mut()
                    .and_then(|project| project.items.iter_mut().find(|item| item.id == item_id))
                    && !item.linked_branches.contains(&branch)
                {
                    item.linked_branches.push(branch);
                }
                cx.notify();
            })?;
            Ok(())
        })
    }
}

fn selection_key(repository: &RemoteRepository) -> String {
    format!("github_projects.selected.{}", repository.full_name())
}

/// Starts an item from a view with a window: creates the branch, moves the item, then checks
/// the branch out.
pub fn start_item(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    state: Entity<RepositoryProjects>,
    item_id: String,
    branch: String,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let Some((_, remote_name, _)) = github_repository(&project, cx) else {
        return Task::ready(Err(anyhow::anyhow!(
            "o projeto não está num repositório do GitHub"
        )));
    };
    let started = state.update(cx, |state, cx| state.start(item_id, branch.clone(), cx));
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        started.await?;
        let checkout = window_handle.update(cx, |_, window, cx| {
            repo_hosting::checkout_remote_branch(
                workspace,
                &project,
                remote_name,
                branch,
                window,
                cx,
            )
        })?;
        checkout.await
    })
}

/// Starts an item from an action, such as the agent card's Começar, with the branch name the
/// item suggests.
fn start_from_workspace(
    workspace: &mut Workspace,
    item_id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some((repository, _, _)) = github_repository(&project, cx) else {
        return;
    };
    let state = repository_projects(&repository, cx);
    let Some(branch) = state
        .read(cx)
        .project()
        .and_then(|github_project| github_project.item(&item_id))
        .map(api::branch_name)
    else {
        return;
    };
    let task = start_item(
        workspace.weak_handle(),
        project,
        state,
        item_id,
        branch,
        window,
        cx,
    );
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = task.await {
            workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Whether a dock is showing the Tarefas panel.
fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<TasksPanel>())
    })
}

/// The GitHub Projects button in the status bar's toolkit group, lit while the dock is open.
pub struct ProjectsToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ProjectsToolkitButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let dock_subscriptions = workspace
            .all_docks()
            .into_iter()
            .map(|dock| cx.observe(dock, |_, _, cx| cx.notify()))
            .collect();
        Self {
            workspace: workspace.weak_handle(),
            _dock_subscriptions: dock_subscriptions,
        }
    }
}

impl Render for ProjectsToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();
        StatusBarButton::new("toolkit-github-projects", IconName::Blocks, is_open)
            .tab_index(0isize)
            .aria_label("GitHub Projects")
            .tooltip(|_window, cx| {
                Tooltip::for_action("Tarefas do GitHub Projects", &ToggleFocus, cx)
            })
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<TasksPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for ProjectsToolkitButton {
    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            !task_agents::toolkit_enabled(
                task_agents::toolkit_ids::GITHUB_PROJECTS,
                workspace.read(cx).project(),
                cx,
            )
        })
    }

    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
