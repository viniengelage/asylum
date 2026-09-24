//! ClickUp in the editor: the tasks assigned to you, in a dock panel. Authentication is a
//! personal API token kept in the system keychain, so there is no OAuth app to register.

mod api;
mod panel;

pub use panel::ClickUpPanel;

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use gpui::{Entity, Global, Subscription, Task, WeakEntity, actions};
use http_client::HttpClient;
use std::{
    any::TypeId,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::StatusBarButton};

actions!(
    clickup,
    [
        /// Opens the ClickUp panel, or hands focus back if it already has it.
        ToggleFocus,
        /// Forgets the ClickUp token saved in the keychain.
        Disconnect,
    ]
);

/// The keychain entry is keyed by a URL; this one is ours alone, so it never collides with
/// credentials ClickUp's own apps might store.
const CREDENTIALS_URL: &str = "https://app.clickup.com/asylum";
const CREDENTIALS_USERNAME: &str = "clickup-token";

/// Often enough that a status changed on the web shows up while you are still looking, rare
/// enough to stay far below ClickUp's 100 requests per minute.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
/// The closed tab is for "what did I just finish", not an archive.
const CLOSED_TASKS_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Where ClickUp lets you generate or regenerate the personal token.
pub const TOKEN_SETTINGS_URL: &str = "https://app.clickup.com/settings/apps";

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<ClickUpPanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|_workspace, _: &Disconnect, _window, cx| {
            ClickUpStore::global(cx)
                .update(cx, |store, cx| store.disconnect(cx))
                .detach_and_log_err(cx);
        });
    })
    .detach();
}

/// Adds the panel the first time it is asked for: someone who never uses ClickUp should not
/// get a tab for it.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ClickUpPanel>(cx).is_none() {
        let store = ClickUpStore::global(cx);
        let weak_workspace = workspace.weak_handle();
        let panel = cx.new(|cx| ClickUpPanel::new(store, weak_workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<ClickUpPanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ClickUpPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<ClickUpPanel>(window, cx);
}

/// A token that ClickUp accepted, with what it said about its owner.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: api::User,
    pub workspaces: Vec<api::Workspace>,
    token: Arc<str>,
}

impl Account {
    /// The only part of the token the UI ever shows, so it can be told apart from a newer one.
    pub fn token_suffix(&self) -> String {
        let characters: Vec<char> = self.token.chars().collect();
        let start = characters.len().saturating_sub(4);
        characters[start..].iter().collect()
    }
}

pub enum Connection {
    /// Reading the keychain and checking the saved token.
    Loading,
    Disconnected,
    Connected(Account),
    /// The saved token stopped working, usually because it was regenerated on ClickUp.
    Rejected,
    /// The saved token could not be checked; it is kept for the next attempt.
    Unreachable {
        message: SharedString,
    },
}

/// The assigned tasks of the selected workspace, as of the last sync.
#[derive(Default)]
pub struct AssignedTasks {
    pub open: Vec<api::Task>,
    pub closed: Vec<api::Task>,
    pub synced_at: Option<Instant>,
    pub is_syncing: bool,
    /// Why the last sync failed; the tasks from the sync before it stay visible.
    pub error: Option<SharedString>,
}

/// The account state shared by every window: there is one keychain entry, so there is one
/// connection.
pub struct ClickUpStore {
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    connection: Connection,
    selected_workspace_id: Option<String>,
    tasks: AssignedTasks,
    /// The timer running for the user in the selected workspace, as of the last sync.
    running_timer: Option<api::RunningTimer>,
    load_task: Option<Task<()>>,
    sync_task: Option<Task<()>>,
}

struct GlobalClickUpStore(Entity<ClickUpStore>);

impl Global for GlobalClickUpStore {}

impl ClickUpStore {
    /// Created on first use rather than at startup, so the keychain is only read by people
    /// who open ClickUp.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalClickUpStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = Self {
                http_client: cx.http_client(),
                credentials_provider: zed_credentials_provider::global(cx),
                connection: Connection::Loading,
                selected_workspace_id: None,
                tasks: AssignedTasks::default(),
                running_timer: None,
                load_task: None,
                sync_task: None,
            };
            store.load(cx);
            store
        });
        cx.set_global(GlobalClickUpStore(store.clone()));
        store
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn tasks(&self) -> &AssignedTasks {
        &self.tasks
    }

    pub fn running_timer(&self) -> Option<&api::RunningTimer> {
        self.running_timer.as_ref()
    }

    /// What a request needs: the HTTP client, the token and the selected workspace's id.
    pub(crate) fn request_context(&self) -> Option<(Arc<dyn HttpClient>, Arc<str>, String)> {
        let Connection::Connected(account) = &self.connection else {
            return None;
        };
        Some((
            self.http_client.clone(),
            account.token.clone(),
            self.selected_workspace_id.clone()?,
        ))
    }

    /// Starts the timer on `task_id`, which stops whichever one was running.
    pub fn start_timer(&mut self, task_id: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some((http_client, token, workspace_id)) = self.request_context() else {
            return Task::ready(Err(anyhow::anyhow!("ClickUp não está conectado")));
        };
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                api::start_timer(&http_client, &token, &workspace_id, &task_id).await
            })
            .await?;
            this.update(cx, |this, cx| this.sync(cx))
        })
    }

    pub fn stop_timer(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some((http_client, token, workspace_id)) = self.request_context() else {
            return Task::ready(Err(anyhow::anyhow!("ClickUp não está conectado")));
        };
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                api::stop_timer(&http_client, &token, &workspace_id).await
            })
            .await?;
            this.update(cx, |this, cx| this.sync(cx))
        })
    }

    pub fn selected_workspace(&self) -> Option<&api::Workspace> {
        let Connection::Connected(account) = &self.connection else {
            return None;
        };
        let selected_id = self.selected_workspace_id.as_deref()?;
        account
            .workspaces
            .iter()
            .find(|workspace| workspace.id == selected_id)
    }

    pub fn select_workspace(&mut self, workspace_id: String, cx: &mut Context<Self>) {
        if self.selected_workspace_id.as_deref() == Some(workspace_id.as_str()) {
            return;
        }
        self.selected_workspace_id = Some(workspace_id);
        self.tasks = AssignedTasks::default();
        self.sync(cx);
    }

    /// Syncs now instead of waiting for the next tick.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        let Connection::Connected(account) = &self.connection else {
            return;
        };
        let Some(workspace_id) = self.selected_workspace_id.clone() else {
            return;
        };
        let http_client = self.http_client.clone();
        let token = account.token.clone();
        let user_id = account.user.id;
        self.tasks.is_syncing = true;
        cx.notify();

        self.sync_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let closed_since = SystemTime::now()
                    .checked_sub(CLOSED_TASKS_WINDOW)
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64);
                let result = cx
                    .background_spawn({
                        let http_client = http_client.clone();
                        let token = token.clone();
                        let workspace_id = workspace_id.clone();
                        async move {
                            let open = api::get_assigned_tasks(
                                &http_client,
                                &token,
                                &workspace_id,
                                user_id,
                                None,
                            );
                            let closed = api::get_assigned_tasks(
                                &http_client,
                                &token,
                                &workspace_id,
                                user_id,
                                closed_since,
                            );
                            let timer = api::get_running_timer(&http_client, &token, &workspace_id);
                            let (open, closed, timer) = futures::join!(open, closed, timer);
                            // A timer that can't be read shouldn't hide the tasks.
                            let timer = timer
                                .context("ClickUp: falha ao ler o timer")
                                .log_err()
                                .flatten();
                            anyhow::Ok((open?, closed?, timer))
                        }
                    })
                    .await;

                let keep_syncing = this
                    .update(cx, |this, cx| this.finish_sync(result, cx))
                    .unwrap_or(false);
                if !keep_syncing {
                    return;
                }
                cx.background_executor().timer(SYNC_INTERVAL).await;
                this.update(cx, |this, cx| {
                    this.tasks.is_syncing = true;
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    /// Returns whether syncing should go on.
    fn finish_sync(
        &mut self,
        result: Result<(Vec<api::Task>, Vec<api::Task>, Option<api::RunningTimer>)>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.tasks.is_syncing = false;
        let keep_syncing = match result {
            Ok((open, closed, running_timer)) => {
                self.running_timer = running_timer;
                self.tasks.open = open
                    .into_iter()
                    .filter(|task| !task.status.is_closed())
                    .collect();
                self.tasks.closed = closed
                    .into_iter()
                    .filter(|task| task.status.is_closed())
                    .collect();
                self.tasks
                    .closed
                    .sort_by_key(|task| std::cmp::Reverse(task.date_closed.unwrap_or_default()));
                self.tasks.synced_at = Some(Instant::now());
                self.tasks.error = None;
                true
            }
            Err(error) if is_unauthorized(&error) => {
                self.connection = Connection::Rejected;
                self.tasks = AssignedTasks::default();
                false
            }
            Err(error) => {
                log::warn!("ClickUp: falha ao sincronizar as tarefas: {error:#}");
                self.tasks.error = Some(format!("{error:#}").into());
                true
            }
        };
        cx.notify();
        keep_syncing
    }

    fn set_connected(&mut self, account: Account, cx: &mut Context<Self>) {
        let selection_still_exists = self.selected_workspace_id.as_ref().is_some_and(|id| {
            account
                .workspaces
                .iter()
                .any(|workspace| &workspace.id == id)
        });
        if !selection_still_exists {
            self.selected_workspace_id = account
                .workspaces
                .first()
                .map(|workspace| workspace.id.clone());
            self.tasks = AssignedTasks::default();
        }
        self.connection = Connection::Connected(account);
        self.sync(cx);
    }

    /// Checks a token with ClickUp without saving it, so the panel can say whose account it
    /// is before anything is stored.
    pub fn validate(&self, token: String, cx: &App) -> Task<Result<Account>> {
        let http_client = self.http_client.clone();
        cx.background_spawn(async move { validate_token(&http_client, token).await })
    }

    pub fn save(&mut self, account: Account, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let token = account.token.clone();
        cx.spawn(async move |this, cx| {
            credentials_provider
                .write_credentials(CREDENTIALS_URL, CREDENTIALS_USERNAME, token.as_bytes(), cx)
                .await?;
            this.update(cx, |this, cx| this.set_connected(account, cx))
        })
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.load_task = None;
        self.sync_task = None;
        self.connection = Connection::Disconnected;
        self.tasks = AssignedTasks::default();
        cx.notify();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            credentials_provider
                .delete_credentials(CREDENTIALS_URL, cx)
                .await
        })
    }

    /// Checks the saved token again, after ClickUp was unreachable.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.connection = Connection::Loading;
        cx.notify();
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let credentials_provider = self.credentials_provider.clone();
        let http_client = self.http_client.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let connection = match read_saved_token(&credentials_provider, cx).await {
                Ok(None) => Connection::Disconnected,
                Ok(Some(token)) => match validate_token(&http_client, token).await {
                    Ok(account) => {
                        this.update(cx, |this, cx| this.set_connected(account, cx))
                            .log_err();
                        return;
                    }
                    Err(error) if is_unauthorized(&error) => Connection::Rejected,
                    Err(error) => {
                        log::warn!("ClickUp: não foi possível checar o token salvo: {error:#}");
                        Connection::Unreachable {
                            message: error.to_string().into(),
                        }
                    }
                },
                Err(error) => {
                    log::error!("ClickUp: falha ao ler o token do keychain: {error:#}");
                    Connection::Unreachable {
                        message: "Não foi possível ler o token do keychain".into(),
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.connection = connection;
                cx.notify();
            })
            .log_err();
        }));
    }
}

async fn read_saved_token(
    credentials_provider: &Arc<dyn CredentialsProvider>,
    cx: &gpui::AsyncApp,
) -> Result<Option<String>> {
    let Some((username, bytes)) = credentials_provider
        .read_credentials(CREDENTIALS_URL, cx)
        .await?
    else {
        return Ok(None);
    };
    if username != CREDENTIALS_USERNAME {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(bytes)?))
}

async fn validate_token(http_client: &Arc<dyn HttpClient>, token: String) -> Result<Account> {
    let token = token.trim().to_string();
    let user = api::get_user(http_client, &token).await?;
    let workspaces = api::get_workspaces(http_client, &token).await?;
    Ok(Account {
        user,
        workspaces,
        token: token.into(),
    })
}

pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<api::ApiError>(),
        Some(api::ApiError::Unauthorized)
    )
}

/// Whether a dock is currently showing the ClickUp panel.
fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<ClickUpPanel>())
    })
}

/// The ClickUp button in the status bar's toolkit group, lit while the panel is open.
pub struct ClickUpToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ClickUpToolkitButton {
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

impl Render for ClickUpToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();

        StatusBarButton::new("toolkit-clickup", IconName::ListTodo, is_open)
            .tab_index(0isize)
            .aria_label("ClickUp")
            .tooltip(|_window, cx| Tooltip::for_action("ClickUp", &ToggleFocus, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<ClickUpPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for ClickUpToolkitButton {
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    fn fake_client(user_status: u16) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| async move {
            assert_eq!(
                request
                    .headers()
                    .get("Authorization")
                    .map(|value| value.as_bytes()),
                Some(b"pk_123_ABCD".as_slice()),
                "personal tokens go in the header without a Bearer prefix"
            );
            let body = match request.uri().path() {
                "/api/v2/user" if user_status == 200 => {
                    r#"{"user":{"id":7,"username":"Vinicios","email":"v@example.com"}}"#
                }
                "/api/v2/user" => r#"{"err":"Token invalid","ECODE":"OAUTH_025"}"#,
                "/api/v2/team" => {
                    r#"{"teams":[{"id":"1","name":"Trix"},{"id":"2","name":"Pessoal"}]}"#
                }
                path => panic!("unexpected request to {path}"),
            };
            let status = if request.uri().path() == "/api/v2/user" {
                user_status
            } else {
                200
            };
            Ok(Response::builder().status(status).body(body.into())?)
        })
    }

    #[gpui::test]
    async fn accepts_a_valid_token() {
        let account = validate_token(&fake_client(200), " pk_123_ABCD\n".into())
            .await
            .unwrap();
        assert_eq!(account.user.display_name(), "Vinicios");
        assert_eq!(account.workspaces.len(), 2);
        assert_eq!(account.token_suffix(), "ABCD");
    }

    #[gpui::test]
    async fn reports_a_rejected_token() {
        let error = validate_token(&fake_client(401), "pk_123_ABCD".into())
            .await
            .unwrap_err();
        assert!(is_unauthorized(&error));
    }
}
