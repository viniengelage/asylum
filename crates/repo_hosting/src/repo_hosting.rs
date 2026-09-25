//! The Repo panel: the pull requests of the repository the project's `origin` points at, with
//! the one for the checked-out branch on top. Bitbucket Cloud authenticates with an Atlassian
//! API token kept in the keychain, which is already scoped to the active profile.

mod api;
mod bitbucket;
mod panel;

pub use panel::RepoPanel;

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use gpui::{Entity, Global, Subscription, Task, WeakEntity, actions};
use http_client::HttpClient;
use std::{any::TypeId, sync::Arc};
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
    ]
);

/// Ours alone, so it never collides with what git credential helpers keep for bitbucket.org.
const BITBUCKET_CREDENTIALS_URL: &str = "https://bitbucket.org/asylum";

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<RepoPanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|_workspace, _: &DisconnectBitbucket, _window, cx| {
            RepoStore::global(cx)
                .update(cx, |store, cx| store.disconnect(cx))
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
    credentials: bitbucket::Credentials,
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
    load_task: Option<Task<()>>,
}

struct GlobalRepoStore(Entity<RepoStore>);

impl Global for GlobalRepoStore {}

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
                load_task: None,
            };
            store.load(cx);
            store
        });
        cx.set_global(GlobalRepoStore(store.clone()));
        store
    }

    pub fn connection(&self, hosting: api::Hosting) -> &Connection {
        match hosting {
            api::Hosting::Bitbucket => &self.bitbucket,
        }
    }

    pub(crate) fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http_client.clone()
    }

    pub(crate) fn bitbucket_credentials(&self) -> Option<bitbucket::Credentials> {
        match &self.bitbucket {
            Connection::Connected(account) => Some(account.credentials.clone()),
            _ => None,
        }
    }

    /// Called when a request made with the saved token came back unauthorized.
    pub(crate) fn mark_rejected(&mut self, cx: &mut Context<Self>) {
        if matches!(self.bitbucket, Connection::Connected(_)) {
            self.bitbucket = Connection::Rejected;
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
        let credentials = bitbucket::Credentials {
            email: email.trim().to_string(),
            token: token.trim().to_string(),
        };
        cx.background_spawn(async move { validate(&http_client, credentials).await })
    }

    pub fn save(&mut self, account: Account, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let credentials = account.credentials.clone();
        cx.spawn(async move |this, cx| {
            credentials_provider
                .write_credentials(
                    BITBUCKET_CREDENTIALS_URL,
                    &credentials.email,
                    credentials.token.as_bytes(),
                    cx,
                )
                .await?;
            this.update(cx, |this, cx| {
                this.bitbucket = Connection::Connected(account);
                cx.notify();
            })
        })
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.load_task = None;
        self.bitbucket = Connection::Disconnected;
        cx.notify();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            credentials_provider
                .delete_credentials(BITBUCKET_CREDENTIALS_URL, cx)
                .await
        })
    }

    /// Checks the saved token again, after Bitbucket was unreachable.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.bitbucket = Connection::Loading;
        cx.notify();
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let credentials_provider = self.credentials_provider.clone();
        let http_client = self.http_client.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let saved = credentials_provider
                .read_credentials(BITBUCKET_CREDENTIALS_URL, cx)
                .await;
            let connection = match saved {
                Ok(None) => Connection::Disconnected,
                Ok(Some((email, token))) => match String::from_utf8(token) {
                    Ok(token) => {
                        let credentials = bitbucket::Credentials { email, token };
                        match validate(&http_client, credentials).await {
                            Ok(account) => Connection::Connected(account),
                            Err(error) if is_unauthorized(&error) => Connection::Rejected,
                            Err(error) => {
                                log::warn!(
                                    "Repo: não foi possível checar o token do Bitbucket: {error:#}"
                                );
                                Connection::Unreachable {
                                    message: error.to_string().into(),
                                }
                            }
                        }
                    }
                    Err(error) => {
                        log::error!("Repo: token do Bitbucket ilegível no keychain: {error}");
                        Connection::Rejected
                    }
                },
                Err(error) => {
                    log::error!("Repo: falha ao ler o token do Bitbucket do keychain: {error:#}");
                    Connection::Unreachable {
                        message: "Não foi possível ler o token do keychain".into(),
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.bitbucket = connection;
                cx.notify();
            })
            .log_err();
        }));
    }
}

async fn validate(
    http_client: &Arc<dyn HttpClient>,
    credentials: bitbucket::Credentials,
) -> Result<Account> {
    let user = bitbucket::get_user(http_client, &credentials).await?;
    Ok(Account { user, credentials })
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
