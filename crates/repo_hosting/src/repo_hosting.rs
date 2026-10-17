//! The Repo panel: the pull requests of the repository the project's `origin` points at, with
//! the one for the checked-out branch on top. Bitbucket Cloud authenticates with an Atlassian
//! API token and GitHub with the `gh` CLI's token or a fine-grained one, both kept in the
//! keychain, which is already scoped to the active profile.

mod agent_toolkit;
pub mod api;
mod bitbucket;
pub mod github;
pub mod host;
mod panel;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
use panel::PullRequestFollowup;
pub use panel::RepoPanel;

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use gpui::{Entity, Global, Subscription, Task, WeakEntity, actions};
use http_client::HttpClient;
use std::{any::TypeId, collections::HashMap, sync::Arc};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::StatusBarButton};

actions!(
    repo_hosting,
    [
        /// Opens the Repo panel, or hands focus back if it already has it.
        ToggleFocus,
        /// Forgets the Bitbucket token saved in the keychain.
        DisconnectBitbucket,
        /// Forgets the GitHub token saved in the keychain.
        DisconnectGitHub,
    ]
);

/// Ours alone, so it never collides with what git credential helpers keep for bitbucket.org.
const BITBUCKET_CREDENTIALS_URL: &str = "https://bitbucket.org/asylum";
/// Ours alone, so it never collides with what git credential helpers keep for github.com.
const GITHUB_CREDENTIALS_URL: &str = "https://github.com/asylum";

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<RepoPanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        agent_toolkit::prefetch_pull_requests(workspace.project().clone(), cx);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(
            |workspace, action: &zed_actions::repo_hosting::OpenPullRequest, window, cx| {
                show_pull_request(
                    workspace,
                    action.number,
                    PullRequestFollowup::Open,
                    window,
                    cx,
                );
            },
        );
        workspace.register_action(
            |workspace, action: &zed_actions::repo_hosting::CheckoutPullRequest, window, cx| {
                show_pull_request(
                    workspace,
                    action.number,
                    PullRequestFollowup::Checkout,
                    window,
                    cx,
                );
            },
        );
        workspace.register_action(
            |workspace, action: &zed_actions::repo_hosting::DiffPullRequest, window, cx| {
                show_pull_request(
                    workspace,
                    action.number,
                    PullRequestFollowup::Diff,
                    window,
                    cx,
                );
            },
        );
        workspace.register_action(|_workspace, _: &DisconnectBitbucket, _window, cx| {
            RepoStore::global(cx)
                .update(cx, |store, cx| {
                    store.disconnect(api::Hosting::Bitbucket, cx)
                })
                .detach_and_log_err(cx);
        });
        workspace.register_action(|_workspace, _: &DisconnectGitHub, _window, cx| {
            RepoStore::global(cx)
                .update(cx, |store, cx| store.disconnect(api::Hosting::GitHub, cx))
                .detach_and_log_err(cx);
        });
    })
    .detach();
}

/// Adds the panel the first time it is asked for, so projects that never look at their pull
/// requests don't get a tab for them.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<RepoPanel>(cx).is_none() {
        let store = RepoStore::global(cx);
        let project = workspace.project().clone();
        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| RepoPanel::new(store, project, weak_workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<RepoPanel>(window, cx);
}

fn show_pull_request(
    workspace: &mut Workspace,
    number: u64,
    followup: PullRequestFollowup,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open(workspace, window, cx);
    if let Some(panel) = workspace.panel::<RepoPanel>(cx) {
        panel.update(cx, |panel, cx| {
            panel.show_pull_request(number, followup, window, cx)
        });
    }
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<RepoPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<RepoPanel>(window, cx);
}

/// Credentials the host accepted, with who they belong to.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: api::Person,
    pub(crate) credentials: host::Credentials,
    /// The OAuth scopes the token carries. Empty for fine-grained tokens, which don't say.
    pub scopes: Vec<String>,
}

impl Account {
    /// Whether the token visibly lacks `scope`. Fine-grained tokens list nothing, so they are
    /// given the benefit of the doubt and the request itself tells.
    pub fn lacks_scope(&self, scope: &str) -> bool {
        !self.scopes.is_empty() && !self.scopes.iter().any(|granted| granted == scope)
    }
}

pub enum Connection {
    /// Reading the keychain and checking the saved token.
    Loading,
    Disconnected,
    Connected(Account),
    /// The saved token stopped working: revoked, expired, or its e-mail changed.
    Rejected,
    /// The saved token could not be checked; it is kept for the next attempt.
    Unreachable {
        message: SharedString,
    },
}

/// The accounts shared by every window of this profile: one keychain entry per host.
pub struct RepoStore {
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    bitbucket: Connection,
    github: Connection,
    load_tasks: HashMap<api::Hosting, Task<()>>,
    /// The open pull requests each repository had when last listed, which the agent's `@` menu
    /// offers without asking the host again.
    pull_requests: HashMap<api::RemoteRepository, Vec<api::PullRequest>>,
}

pub(crate) struct GlobalRepoStore(pub(crate) Entity<RepoStore>);

impl Global for GlobalRepoStore {}

fn credentials_url(hosting: api::Hosting) -> &'static str {
    match hosting {
        api::Hosting::Bitbucket => BITBUCKET_CREDENTIALS_URL,
        api::Hosting::GitHub => GITHUB_CREDENTIALS_URL,
    }
}

impl RepoStore {
    /// Created on first use, so the keychain is only read by people who open the panel.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalRepoStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = Self {
                http_client: cx.http_client(),
                credentials_provider: zed_credentials_provider::global(cx),
                bitbucket: Connection::Loading,
                github: Connection::Loading,
                load_tasks: HashMap::default(),
                pull_requests: HashMap::default(),
            };
            store.load(api::Hosting::Bitbucket, cx);
            store.load(api::Hosting::GitHub, cx);
            store
        });
        cx.set_global(GlobalRepoStore(store.clone()));
        store
    }

    pub fn connection(&self, hosting: api::Hosting) -> &Connection {
        match hosting {
            api::Hosting::Bitbucket => &self.bitbucket,
            api::Hosting::GitHub => &self.github,
        }
    }

    fn connection_mut(&mut self, hosting: api::Hosting) -> &mut Connection {
        match hosting {
            api::Hosting::Bitbucket => &mut self.bitbucket,
            api::Hosting::GitHub => &mut self.github,
        }
    }

    /// A client for `hosting` when its account is connected.
    pub fn client(&self, hosting: api::Hosting) -> Option<host::Client> {
        match self.connection(hosting) {
            Connection::Connected(account) => Some(host::Client::new(
                self.http_client.clone(),
                account.credentials.clone(),
            )),
            _ => None,
        }
    }

    /// The GitHub token, for what the Repo panel doesn't cover (GitHub Projects).
    pub fn github_client(&self) -> Option<(Arc<dyn HttpClient>, github::Credentials)> {
        let client = self.client(api::Hosting::GitHub)?;
        let credentials = client.github_credentials()?.clone();
        Some((client.http_client, credentials))
    }

    pub(crate) fn cached_pull_requests(
        &self,
        repository: &api::RemoteRepository,
    ) -> Option<&[api::PullRequest]> {
        self.pull_requests.get(repository).map(Vec::as_slice)
    }

    pub(crate) fn set_pull_requests(
        &mut self,
        repository: api::RemoteRepository,
        pull_requests: Vec<api::PullRequest>,
    ) {
        self.pull_requests.insert(repository, pull_requests);
    }

    /// Called when a request made with the saved token came back unauthorized.
    pub(crate) fn mark_rejected(&mut self, hosting: api::Hosting, cx: &mut Context<Self>) {
        let connection = self.connection_mut(hosting);
        if matches!(connection, Connection::Connected(_)) {
            *connection = Connection::Rejected;
            cx.notify();
        }
    }

    /// Checks credentials with Bitbucket without saving them.
    pub fn validate_bitbucket(
        &self,
        email: String,
        token: String,
        cx: &App,
    ) -> Task<Result<Account>> {
        let http_client = self.http_client.clone();
        let credentials = host::Credentials::Bitbucket(bitbucket::Credentials {
            email: email.trim().to_string(),
            token: token.trim().to_string(),
        });
        cx.background_spawn(async move { validate(&http_client, credentials).await })
    }

    /// Checks a GitHub token without saving it.
    pub fn validate_github(&self, token: String, cx: &App) -> Task<Result<Account>> {
        let http_client = self.http_client.clone();
        let credentials = host::Credentials::GitHub(github::Credentials {
            token: token.trim().to_string(),
        });
        cx.background_spawn(async move { validate(&http_client, credentials).await })
    }

    /// The account the `gh` CLI is logged into, checked but not saved.
    pub fn validate_github_cli(&self, cx: &App) -> Task<Result<Account>> {
        let http_client = self.http_client.clone();
        cx.background_spawn(async move {
            let token = github::token_from_gh_cli().await?;
            validate(
                &http_client,
                host::Credentials::GitHub(github::Credentials { token }),
            )
            .await
        })
    }

    pub fn save(&mut self, account: Account, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let hosting = account.credentials.hosting();
        let (username, token) = match &account.credentials {
            host::Credentials::Bitbucket(credentials) => {
                (credentials.email.clone(), credentials.token.clone())
            }
            host::Credentials::GitHub(credentials) => (
                account
                    .user
                    .nickname
                    .clone()
                    .unwrap_or_else(|| account.user.id.clone()),
                credentials.token.clone(),
            ),
        };
        cx.spawn(async move |this, cx| {
            credentials_provider
                .write_credentials(credentials_url(hosting), &username, token.as_bytes(), cx)
                .await?;
            this.update(cx, |this, cx| {
                *this.connection_mut(hosting) = Connection::Connected(account);
                cx.notify();
            })
        })
    }

    pub fn disconnect(
        &mut self,
        hosting: api::Hosting,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.load_tasks.remove(&hosting);
        *self.connection_mut(hosting) = Connection::Disconnected;
        cx.notify();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            credentials_provider
                .delete_credentials(credentials_url(hosting), cx)
                .await
        })
    }

    /// Checks the saved token again, after the host was unreachable.
    pub fn retry(&mut self, hosting: api::Hosting, cx: &mut Context<Self>) {
        *self.connection_mut(hosting) = Connection::Loading;
        cx.notify();
        self.load(hosting, cx);
    }

    fn load(&mut self, hosting: api::Hosting, cx: &mut Context<Self>) {
        let credentials_provider = self.credentials_provider.clone();
        let http_client = self.http_client.clone();
        let name = hosting.name();
        let task = cx.spawn(async move |this, cx| {
            let saved = credentials_provider
                .read_credentials(credentials_url(hosting), cx)
                .await;
            let connection = match saved {
                Ok(None) => Connection::Disconnected,
                Ok(Some((username, token))) => match String::from_utf8(token) {
                    Ok(token) => {
                        let credentials = match hosting {
                            api::Hosting::Bitbucket => {
                                host::Credentials::Bitbucket(bitbucket::Credentials {
                                    email: username,
                                    token,
                                })
                            }
                            api::Hosting::GitHub => {
                                host::Credentials::GitHub(github::Credentials { token })
                            }
                        };
                        match validate(&http_client, credentials).await {
                            Ok(account) => Connection::Connected(account),
                            Err(error) if is_unauthorized(&error) => Connection::Rejected,
                            Err(error) => {
                                log::warn!(
                                    "Repo: não foi possível checar o token do {name}: {error:#}"
                                );
                                Connection::Unreachable {
                                    message: error.to_string().into(),
                                }
                            }
                        }
                    }
                    Err(error) => {
                        log::error!("Repo: token do {name} ilegível no keychain: {error}");
                        Connection::Rejected
                    }
                },
                Err(error) => {
                    log::error!("Repo: falha ao ler o token do {name} do keychain: {error:#}");
                    Connection::Unreachable {
                        message: "Não foi possível ler o token do keychain".into(),
                    }
                }
            };
            this.update(cx, |this, cx| {
                *this.connection_mut(hosting) = connection;
                cx.notify();
            })
            .log_err();
        });
        self.load_tasks.insert(hosting, task);
    }
}

async fn validate(
    http_client: &Arc<dyn HttpClient>,
    credentials: host::Credentials,
) -> Result<Account> {
    match &credentials {
        host::Credentials::Bitbucket(bitbucket_credentials) => {
            let user = bitbucket::get_user(http_client, bitbucket_credentials).await?;
            Ok(Account {
                user,
                credentials,
                scopes: Vec::new(),
            })
        }
        host::Credentials::GitHub(github_credentials) => {
            let (user, scopes) = github::get_user(http_client, github_credentials).await?;
            Ok(Account {
                user,
                credentials,
                scopes,
            })
        }
    }
}

pub fn is_forbidden(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<api::ApiError>(),
        Some(api::ApiError::Forbidden { .. })
    )
}

pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<api::ApiError>(),
        Some(api::ApiError::Unauthorized { .. })
    )
}

/// Whether a dock is currently showing the Repo panel.
fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<RepoPanel>())
    })
}

/// The Repo button in the status bar's toolkit group, lit while the panel is open.
pub struct RepoToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl RepoToolkitButton {
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

impl Render for RepoToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();

        StatusBarButton::new("toolkit-repo", IconName::PullRequest, is_open)
            .tab_index(0isize)
            .aria_label("Repo")
            .tooltip(|_window, cx| Tooltip::for_action("Pull requests", &ToggleFocus, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<RepoPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for RepoToolkitButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The panel has no status bar button of its own, so this is the only visible way in.
        None
    }
}
