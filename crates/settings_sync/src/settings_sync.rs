//! Signs in with GitHub and keeps the user's configuration in a private gist, so it follows
//! them across machines without a server of Asylum's own.

mod github;
mod local_files;
mod sign_in_modal;
mod sync_plan;
mod title_bar_button;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local};
use client::Client;
use credentials_provider::CredentialsProvider;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task, TaskExt as _,
    actions,
};
use http_client::HttpClient;
use i18n::t;
use util::ResultExt as _;
use workspace::Workspace;

pub use github::{DeviceCode, GitHubUser};
pub use sign_in_modal::SignInModal;
pub use title_bar_button::render_title_bar_button;

use crate::local_files::{MANIFEST_CONTENT, MANIFEST_NAME, SyncRecord, SyncScope};
use crate::sync_plan::{ConflictWinner, Hashes, SyncAction, content_hash, plan_sync};

actions!(
    settings_sync,
    [
        /// Signs in with GitHub to sync settings across machines.
        SignIn,
        /// Signs out of GitHub and stops syncing settings.
        SignOut,
        /// Syncs settings with GitHub now.
        SyncNow
    ]
);

const CREDENTIALS_URL: &str = "https://github.com/login/oauth/asylum-settings-sync";
const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const FIRST_SYNC_DELAY: Duration = Duration::from_secs(10);

pub enum State {
    Restoring,
    SignedOut {
        error: Option<SharedString>,
    },
    AwaitingAuthorization {
        code: DeviceCode,
    },
    SignedIn {
        user: GitHubUser,
        token: String,
        sync: SyncStatus,
    },
}

#[derive(Clone)]
pub enum SyncStatus {
    NeverSynced,
    Syncing,
    Synced {
        at: DateTime<Local>,
        report: SyncReport,
    },
    Failed(SharedString),
}

#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted: usize,
    /// Copies that lost a conflict and were saved in the backups folder.
    pub backups: usize,
}

pub struct SettingsSync {
    http: Arc<dyn HttpClient>,
    credentials: Arc<dyn CredentialsProvider>,
    state: State,
    sign_in_task: Option<Task<()>>,
    sync_task: Option<Task<()>>,
    _periodic_sync: Task<()>,
}

struct GlobalSettingsSync(Entity<SettingsSync>);

impl Global for GlobalSettingsSync {}

pub fn init(client: &Arc<Client>, cx: &mut App) {
    let http: Arc<dyn HttpClient> = client.http_client();
    let credentials = client.credentials_provider();
    let sync = cx.new(|cx| SettingsSync::new(http, credentials, cx));
    cx.set_global(GlobalSettingsSync(sync));

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &SignIn, window, cx| {
                let Some(sync) = SettingsSync::global(cx) else {
                    return;
                };
                sync.update(cx, |sync, cx| sync.sign_in(cx));
                workspace.toggle_modal(window, cx, |window, cx| {
                    SignInModal::new(sync.clone(), window, cx)
                });
            })
            .register_action(|_, _: &SignOut, _, cx| {
                if let Some(sync) = SettingsSync::global(cx) {
                    sync.update(cx, |sync, cx| sync.sign_out(cx));
                }
            })
            .register_action(|_, _: &SyncNow, _, cx| {
                if let Some(sync) = SettingsSync::global(cx) {
                    sync.update(cx, |sync, cx| sync.sync_now(cx));
                }
            });
    })
    .detach();
}

impl SettingsSync {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSettingsSync>()
            .map(|global| global.0.clone())
    }

    fn new(
        http: Arc<dyn HttpClient>,
        credentials: Arc<dyn CredentialsProvider>,
        cx: &mut Context<Self>,
    ) -> Self {
        let restore = cx.spawn({
            let credentials = credentials.clone();
            async move |this, cx| {
                let saved = credentials.read_credentials(CREDENTIALS_URL, cx).await;
                this.update(cx, |this, cx| {
                    this.state = match saved {
                        Ok(Some((login, token))) => match String::from_utf8(token) {
                            Ok(token) => State::SignedIn {
                                user: user_from_login(login),
                                token,
                                sync: SyncStatus::NeverSynced,
                            },
                            Err(_) => State::SignedOut { error: None },
                        },
                        Ok(None) => State::SignedOut { error: None },
                        Err(error) => {
                            log::error!(
                                "settings sync: failed to read the saved sign-in: {error:#}"
                            );
                            State::SignedOut { error: None }
                        }
                    };
                    cx.notify();
                })
                .log_err();
            }
        });

        let periodic_sync = cx.spawn(async move |this, cx| {
            restore.await;
            cx.background_executor().timer(FIRST_SYNC_DELAY).await;
            loop {
                if this.update(cx, |this, cx| this.sync_now(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(SYNC_INTERVAL).await;
            }
        });

        Self {
            http,
            credentials,
            state: State::Restoring,
            sign_in_task: None,
            sync_task: None,
            _periodic_sync: periodic_sync,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_signing_in(&self) -> bool {
        self.sign_in_task.is_some()
    }

    pub fn sign_in(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            State::SignedIn { .. } | State::AwaitingAuthorization { .. }
        ) {
            return;
        }
        self.state = State::SignedOut { error: None };
        cx.notify();

        let http = self.http.clone();
        let credentials = self.credentials.clone();
        self.sign_in_task = Some(cx.spawn(async move |this, cx| {
            let result = authorize(&http, &credentials, &this, cx).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((user, token)) => {
                        this.state = State::SignedIn {
                            user,
                            token,
                            sync: SyncStatus::NeverSynced,
                        };
                        this.sync_now(cx);
                    }
                    Err(error) => {
                        log::error!("settings sync: sign-in failed: {error:#}");
                        this.state = State::SignedOut {
                            error: Some(format!("{error:#}").into()),
                        };
                    }
                }
                this.sign_in_task = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if self.sign_in_task.take().is_some() {
            self.state = State::SignedOut { error: None };
            cx.notify();
        }
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.sign_in_task = None;
        self.sync_task = None;
        self.state = State::SignedOut { error: None };
        cx.notify();

        let credentials = self.credentials.clone();
        cx.spawn(async move |_, cx| {
            credentials
                .delete_credentials(CREDENTIALS_URL, cx)
                .await
                .with_context(|| t!("settings_sync.error.delete_sign_in").to_string())?;
            // The next account to sign in may use a different gist, so this machine starts over
            // as if it had never synced. The shared record stays for the other profiles, and is
            // only reused by the account that wrote it.
            cx.background_spawn(async move { local_files::delete_profile_record() })
                .await
        })
        .detach_and_log_err(cx);
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let State::SignedIn { user, token, sync } = &mut self.state else {
            return;
        };
        if matches!(sync, SyncStatus::Syncing) {
            return;
        }
        *sync = SyncStatus::Syncing;
        cx.notify();

        let http = self.http.clone();
        let token = token.clone();
        let login = user.login.clone();
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let login = login.clone();
                    async move { run_all_syncs(&http, &token, &login).await }
                })
                .await;
            this.update(cx, |this, cx| {
                let State::SignedIn { sync, .. } = &mut this.state else {
                    return;
                };
                match result {
                    Ok(report) => {
                        if report.backups > 0 {
                            log::warn!(
                                "settings sync: {} file(s) changed on both sides; the other copy is in {}",
                                report.backups,
                                local_files::backups_dir().display()
                            );
                        }
                        *sync = SyncStatus::Synced {
                            at: Local::now(),
                            report,
                        };
                    }
                    Err(error) if error.is::<github::Unauthorized>() => {
                        log::warn!("settings sync: the GitHub sign-in of {login} is no longer valid");
                        this.sign_out(cx);
                        this.state = State::SignedOut {
                            error: Some(t!("settings_sync.error.sign_in_rejected")),
                        };
                    }
                    Err(error) => {
                        log::error!("settings sync failed: {error:#}");
                        *sync = SyncStatus::Failed(format!("{error:#}").into());
                    }
                }
                this.sync_task = None;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn set_awaiting_authorization(&mut self, code: DeviceCode, cx: &mut Context<Self>) {
        self.state = State::AwaitingAuthorization { code };
        cx.notify();
    }
}

/// GitHub serves every user's avatar at this address, so a restored sign-in can show it without
/// a request.
fn user_from_login(login: String) -> GitHubUser {
    GitHubUser {
        avatar_url: format!("https://github.com/{login}.png?size=64"),
        name: None,
        login,
    }
}

async fn authorize(
    http: &Arc<dyn HttpClient>,
    credentials: &Arc<dyn CredentialsProvider>,
    this: &gpui::WeakEntity<SettingsSync>,
    cx: &mut AsyncApp,
) -> Result<(GitHubUser, String)> {
    let code = github::request_device_code(http).await?;
    let mut interval = Duration::from_secs(code.interval.max(1));
    let expires_at = std::time::Instant::now() + Duration::from_secs(code.expires_in);
    this.update(cx, |this, cx| {
        this.set_awaiting_authorization(code.clone(), cx)
    })?;

    let token = loop {
        cx.background_executor().timer(interval).await;
        anyhow::ensure!(
            std::time::Instant::now() < expires_at,
            t!("settings_sync.error.code_expired").to_string()
        );
        match github::poll_for_token(http, &code.device_code).await? {
            github::TokenPoll::Pending => {}
            github::TokenPoll::SlowDown => interval += Duration::from_secs(5),
            github::TokenPoll::Authorized(token) => break token,
        }
    };

    let user = github::current_user(http, &token).await?;
    credentials
        .write_credentials(CREDENTIALS_URL, &user.login, token.as_bytes(), cx)
        .await
        .with_context(|| t!("settings_sync.error.save_sign_in").to_string())?;
    Ok((user, token))
}

/// Each app profile syncs its own files to its own gist, the same way each one keeps its own
/// settings.
fn profile_gist_description() -> String {
    match paths::active_profile_id() {
        Some(profile) => format!("Asylum settings sync ({profile})"),
        None => "Asylum settings sync".to_string(),
    }
}

/// Not "Asylum settings sync (shared)", since `shared` is a valid profile id.
const SHARED_GIST_DESCRIPTION: &str = "Asylum shared settings sync";

async fn run_all_syncs(http: &Arc<dyn HttpClient>, token: &str, login: &str) -> Result<SyncReport> {
    let mut report = run_sync(
        http,
        token,
        login,
        SyncScope::Profile,
        &profile_gist_description(),
    )
    .await?;
    let Some(_lock) = local_files::try_lock_shared_sync()? else {
        log::info!("settings sync: another profile is syncing the shared settings; skipping them");
        return Ok(report);
    };
    let shared_report = run_sync(
        http,
        token,
        login,
        SyncScope::Shared,
        SHARED_GIST_DESCRIPTION,
    )
    .await?;
    report.add(&shared_report);
    Ok(report)
}

impl SyncReport {
    fn add(&mut self, other: &SyncReport) {
        self.uploaded += other.uploaded;
        self.downloaded += other.downloaded;
        self.deleted += other.deleted;
        self.backups += other.backups;
    }
}

async fn run_sync(
    http: &Arc<dyn HttpClient>,
    token: &str,
    login: &str,
    scope: SyncScope,
    description: &str,
) -> Result<SyncReport> {
    let mut record = local_files::read_record(scope)?;
    if record.owner.as_deref().is_some_and(|owner| owner != login) {
        record = SyncRecord::default();
    }
    let local = local_files::read_local_files(scope)?;
    let owner = (scope == SyncScope::Shared).then(|| login.to_string());

    let mut remote = None;
    if let Some(gist_id) = record.gist_id.clone() {
        match github::gist_files(http, token, &gist_id).await {
            Ok(files) => remote = Some((gist_id, files)),
            // Deleted on GitHub: start over as a machine that never synced.
            Err(error) if error.is::<github::NotFound>() => record = SyncRecord::default(),
            Err(error) => return Err(error),
        }
    }
    if remote.is_none()
        && let Some(gist_id) = github::find_gist(http, token, description).await?
    {
        let files = github::gist_files(http, token, &gist_id).await?;
        remote = Some((gist_id, files));
    }
    let Some((gist_id, mut remote)) = remote else {
        let mut files = local.clone();
        files.insert(MANIFEST_NAME.to_string(), MANIFEST_CONTENT.to_string());
        let gist_id = github::create_gist(http, token, description, &files).await?;
        local_files::write_record(
            scope,
            &SyncRecord {
                gist_id: Some(gist_id),
                owner,
                base: hashes(&local),
            },
        )?;
        return Ok(SyncReport {
            uploaded: local.len(),
            ..SyncReport::default()
        });
    };

    // Files this scope doesn't sync are left alone in the gist, which is how a profile gist
    // from before the shared gist keeps its keymap and themes instead of losing them.
    remote.remove(MANIFEST_NAME);
    remote.retain(|name, content| {
        local_files::local_path(scope, name).is_some() && !content.trim().is_empty()
    });

    // A machine that has never synced takes what's in the gist, so signing in on a new
    // machine brings the user's settings over instead of overwriting them with defaults.
    let conflict_winner = if record.base.is_empty() {
        ConflictWinner::Remote
    } else {
        ConflictWinner::Local
    };
    let actions = plan_sync(
        &hashes(&local),
        &hashes(&remote),
        &record.base,
        conflict_winner,
    );

    let backup_prefix = match scope {
        SyncScope::Shared => "shared-",
        SyncScope::Profile => "",
    };
    let mut report = SyncReport::default();
    let mut remote_changes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (name, action) in &actions {
        let Some(path) = local_files::local_path(scope, name) else {
            continue;
        };
        let backup_name = format!("{backup_prefix}{name}");
        match action {
            SyncAction::Upload => {
                remote_changes.insert(name.clone(), local.get(name).cloned());
                report.uploaded += 1;
            }
            SyncAction::UploadKeepingRemoteBackup => {
                if let Some(content) = remote.get(name) {
                    local_files::write_backup(&backup_name, content, "github")?;
                    report.backups += 1;
                }
                remote_changes.insert(name.clone(), local.get(name).cloned());
                report.uploaded += 1;
            }
            SyncAction::Download => {
                if let Some(content) = remote.get(name) {
                    local_files::write_local_file(&path, content)?;
                    report.downloaded += 1;
                }
            }
            SyncAction::DownloadKeepingLocalBackup => {
                if let Some(content) = local.get(name) {
                    local_files::write_backup(&backup_name, content, "local")?;
                    report.backups += 1;
                }
                if let Some(content) = remote.get(name) {
                    local_files::write_local_file(&path, content)?;
                    report.downloaded += 1;
                }
            }
            SyncAction::DeleteRemote => {
                remote_changes.insert(name.clone(), None);
                report.deleted += 1;
            }
            SyncAction::DeleteLocal => {
                local_files::delete_local_file(&path)?;
                report.deleted += 1;
            }
        }
    }

    if !remote_changes.is_empty() {
        github::update_gist(http, token, &gist_id, &remote_changes).await?;
    }

    record.gist_id = Some(gist_id);
    record.owner = owner;
    record.base = hashes(&local_files::read_local_files(scope)?);
    local_files::write_record(scope, &record)?;
    Ok(report)
}

fn hashes(files: &BTreeMap<String, String>) -> Hashes {
    files
        .iter()
        .map(|(name, content)| (name.clone(), content_hash(content)))
        .collect()
}
