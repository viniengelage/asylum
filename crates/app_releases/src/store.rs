//! The accounts every window shares, and the releases of each project: detection of the app,
//! refreshing the three services, correlation and polling while something moves.

use crate::{
    accounts::{
        self, AccountKind, AccountToSave, AccountsSource, AppLink, StoredAccounts,
        load_link_override, parse_app_json, scan_app_config,
    },
    app_store_connect as asc, eas, google_play, is_unauthorized,
    model::{self, AppStoreData, Notice, Platform, Release, Snapshot},
};
use anyhow::Result;
use chrono::{DateTime, Local};
use collections::HashMap;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, SharedString, Task,
    WeakEntity,
};
use http_client::HttpClient;
use i18n::t;
use project::Project;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::ResultExt as _;

/// Often enough to see a build finish or a review start without hammering Apple's rate limits.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// Opening the dock again within this window shows the cache instead of refreshing.
const STALE_AFTER: chrono::Duration = chrono::Duration::seconds(60);
const CONFIG_FILES: [&str; 3] = ["app.json", "app.config.ts", "app.config.js"];
const SKIPPED_FOLDERS: [&str; 6] = ["node_modules", "ios", "android", "dist", "build", "target"];

pub enum AccountsState {
    Loading,
    Loaded(StoredAccounts),
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CheckState {
    Checking,
    Passed(SharedString),
    Refused,
    Failed(SharedString),
}

pub struct AccountsChanged;

/// The profile's store accounts, shared by every window.
pub struct AccountsStore {
    http_client: Arc<dyn HttpClient>,
    source: Arc<dyn AccountsSource>,
    state: AccountsState,
    checks: HashMap<AccountKind, CheckState>,
    load_task: Option<Task<()>>,
    check_tasks: HashMap<AccountKind, Task<()>>,
}

struct GlobalAccountsStore(Entity<AccountsStore>);

impl Global for GlobalAccountsStore {}

impl EventEmitter<AccountsChanged> for AccountsStore {}

impl AccountsStore {
    /// Created on first use, so the keychain is only read by people who open Lançamentos.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalAccountsStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = Self {
                http_client: cx.http_client(),
                source: accounts::accounts_source(None, cx),
                state: AccountsState::Loading,
                checks: HashMap::default(),
                load_task: None,
                check_tasks: HashMap::default(),
            };
            store.load(cx);
            store
        });
        cx.set_global(GlobalAccountsStore(store.clone()));
        store
    }

    pub fn state(&self) -> &AccountsState {
        &self.state
    }

    pub fn accounts(&self) -> Option<&StoredAccounts> {
        match &self.state {
            AccountsState::Loaded(accounts) => Some(accounts),
            _ => None,
        }
    }

    pub fn check(&self, kind: AccountKind) -> Option<&CheckState> {
        self.checks.get(&kind)
    }

    pub fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http_client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = source.load(cx).await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(accounts) => AccountsState::Loaded(accounts),
                    Err(error) => {
                        log::error!("Lançamentos: falha ao ler as contas: {error:#}");
                        AccountsState::Failed(t!("app_releases.accounts.keychain_failed"))
                    }
                };
                cx.emit(AccountsChanged);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Tests the account with its service and saves it only if the service accepted it.
    pub fn test_and_save(&mut self, account: AccountToSave, cx: &mut Context<Self>) -> Task<()> {
        let kind = account.kind();
        self.checks.insert(kind, CheckState::Checking);
        cx.notify();
        let http_client = self.http_client.clone();
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let tested = test_account(&http_client, &account).await;
            let check = check_state(tested);
            let saved = if matches!(check, CheckState::Passed(_)) {
                match source.save(account, cx).await {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("Lançamentos: falha ao salvar a conta: {error:#}");
                        this.update(cx, |this, cx| {
                            this.checks.insert(
                                kind,
                                CheckState::Failed(t!("app_releases.accounts.keychain_failed")),
                            );
                            cx.notify();
                        })
                        .log_err();
                        return;
                    }
                }
            } else {
                false
            };
            this.update(cx, |this, cx| {
                this.checks.insert(kind, check);
                if saved {
                    this.load(cx);
                }
                cx.notify();
            })
            .log_err();
        })
    }

    /// The connection test for an account that is already saved.
    pub fn test_saved(&mut self, kind: AccountKind, cx: &mut Context<Self>) {
        let Some(accounts) = self.accounts() else {
            return;
        };
        let account = match kind {
            AccountKind::AppStore => accounts.app_store.clone().map(AccountToSave::AppStore),
            AccountKind::GooglePlay => accounts.google_play.clone().map(AccountToSave::GooglePlay),
            AccountKind::Expo => accounts.expo.clone().map(AccountToSave::Expo),
        };
        let Some(account) = account else {
            return;
        };
        self.checks.insert(kind, CheckState::Checking);
        cx.notify();
        let http_client = self.http_client.clone();
        let task = cx.spawn(async move |this, cx| {
            let check = check_state(test_account(&http_client, &account).await);
            this.update(cx, |this, cx| {
                this.checks.insert(kind, check);
                cx.notify();
            })
            .log_err();
        });
        self.check_tasks.insert(kind, task);
    }

    pub fn forget(&mut self, kind: AccountKind, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.checks.remove(&kind);
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            source.forget(kind, cx).await?;
            this.update(cx, |this, cx| this.load(cx))
        })
    }
}

fn check_state(result: Result<SharedString>) -> CheckState {
    match result {
        Ok(detail) => CheckState::Passed(detail),
        Err(error) if is_unauthorized(&error) => CheckState::Refused,
        Err(error) => CheckState::Failed(format!("{error:#}").into()),
    }
}

async fn test_account(
    http_client: &Arc<dyn HttpClient>,
    account: &AccountToSave,
) -> Result<SharedString> {
    match account {
        AccountToSave::Expo(account) => {
            let viewer = eas::fetch_viewer(http_client, &account.token).await?;
            Ok(t!("app_releases.accounts.connected_as", name = viewer))
        }
        AccountToSave::AppStore(account) => {
            let count = asc::check(http_client, &account.credentials()).await?;
            Ok(t!("app_releases.accounts.apps_visible", count = count))
        }
        AccountToSave::GooglePlay(account) => {
            google_play::access_token(http_client, &account.service_account).await?;
            Ok(t!(
                "app_releases.accounts.connected_as",
                name = account.service_account.client_email
            ))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceStatus {
    /// The profile has no account for this service.
    NoAccount,
    /// The app config doesn't name the app for this service.
    NotLinked,
    Loading,
    Ready,
    /// The service refused the credential; only this service stops.
    Refused,
    /// The store has no app with the id the project names.
    AppNotFound,
    Failed(SharedString),
}

impl SourceStatus {
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Refused | Self::AppNotFound | Self::Failed(_))
    }
}

#[derive(Clone, Debug)]
pub struct Sources {
    pub eas: SourceStatus,
    pub app_store: SourceStatus,
    pub google_play: SourceStatus,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            eas: SourceStatus::Loading,
            app_store: SourceStatus::Loading,
            google_play: SourceStatus::Loading,
        }
    }
}

impl Sources {
    pub fn get(&self, kind: AccountKind) -> &SourceStatus {
        match kind {
            AccountKind::Expo => &self.eas,
            AccountKind::AppStore => &self.app_store,
            AccountKind::GooglePlay => &self.google_play,
        }
    }
}

/// One app of the project and what the services say about it.
pub struct AppReleases {
    pub folder: Arc<Path>,
    pub detected: AppLink,
    pub link: AppLink,
    pub releases: Vec<Release>,
    pub sources: Sources,
    pub refreshed_at: Option<DateTime<Local>>,
    pub app_store_app_id: Option<String>,
    pub beta_groups: Vec<asc::BetaGroup>,
    pub google_play_tracks: Vec<google_play::Track>,
}

impl AppReleases {
    pub fn name(&self) -> String {
        self.link
            .name
            .clone()
            .or_else(|| {
                self.folder
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_default()
    }

    pub fn platforms(&self) -> Vec<Platform> {
        let mut platforms = Vec::new();
        if self.link.bundle_id.is_some() {
            platforms.push(Platform::Ios);
        }
        if self.link.package.is_some() {
            platforms.push(Platform::Android);
        }
        // Linked only through EAS: its builds say which platforms the app ships.
        if platforms.is_empty() {
            platforms = vec![Platform::Ios, Platform::Android];
        }
        platforms
    }
}

pub enum Detection {
    Detecting,
    Done,
}

pub enum ReleasesEvent {
    Notice(Notice),
    TokenExpiring { days: i64 },
}

/// The releases of one project. Shared by the dock, the release tabs and the status chip.
pub struct ReleasesStore {
    project: WeakEntity<Project>,
    fs: Arc<dyn Fs>,
    roots: Vec<Arc<Path>>,
    accounts: Entity<AccountsStore>,
    detection: Detection,
    apps: Vec<AppReleases>,
    selected_app: usize,
    refreshing: bool,
    warned_token_expiry: bool,
    refresh_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

#[derive(Default)]
pub struct ReleasesStores {
    by_project: HashMap<EntityId, Entity<ReleasesStore>>,
}

impl Global for ReleasesStores {}

impl EventEmitter<ReleasesEvent> for ReleasesStore {}

impl ReleasesStore {
    pub fn for_project(project: &Entity<Project>, cx: &mut App) -> Entity<Self> {
        let project_id = project.entity_id();
        if let Some(store) = cx
            .try_global::<ReleasesStores>()
            .and_then(|stores| stores.by_project.get(&project_id))
        {
            return store.clone();
        }
        let accounts = AccountsStore::global(cx);
        let store = cx.new(|cx| {
            let subscriptions = vec![
                cx.subscribe(&accounts, |this: &mut Self, _, _: &AccountsChanged, cx| {
                    this.refresh(cx)
                }),
            ];
            let mut store = Self {
                project: project.downgrade(),
                fs: project.read(cx).fs().clone(),
                roots: Vec::new(),
                accounts,
                detection: Detection::Detecting,
                apps: Vec::new(),
                selected_app: 0,
                refreshing: false,
                warned_token_expiry: false,
                refresh_task: None,
                poll_task: None,
                _subscriptions: subscriptions,
            };
            store.detect(cx);
            store
        });
        cx.default_global::<ReleasesStores>()
            .by_project
            .insert(project_id, store.clone());
        cx.observe_release(project, move |_, cx| {
            if cx.has_global::<ReleasesStores>() {
                cx.global_mut::<ReleasesStores>()
                    .by_project
                    .remove(&project_id);
            }
        })
        .detach();
        store
    }

    /// The store of a project if Lançamentos was ever opened for it, without creating one.
    pub fn existing(project: &Entity<Project>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<ReleasesStores>()?
            .by_project
            .get(&project.entity_id())
            .cloned()
    }

    pub fn accounts(&self) -> &Entity<AccountsStore> {
        &self.accounts
    }

    pub fn detection(&self) -> &Detection {
        &self.detection
    }

    pub fn apps(&self) -> &[AppReleases] {
        &self.apps
    }

    pub fn selected_app_index(&self) -> usize {
        self.selected_app
    }

    pub fn selected_app(&self) -> Option<&AppReleases> {
        self.apps.get(self.selected_app)
    }

    pub fn select_app(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.apps.len() && index != self.selected_app {
            self.selected_app = index;
            cx.notify();
        }
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    pub fn is_polling(&self) -> bool {
        self.poll_task.is_some()
    }

    pub fn first_root(&self) -> Option<Arc<Path>> {
        self.roots.first().cloned()
    }

    pub fn find_release(&self, folder: &Path, version: &str) -> Option<&Release> {
        self.apps
            .iter()
            .find(|app| app.folder.as_ref() == folder)?
            .releases
            .iter()
            .find(|release| release.version == version)
    }

    pub fn app_for_folder(&self, folder: &Path) -> Option<&AppReleases> {
        self.apps.iter().find(|app| app.folder.as_ref() == folder)
    }

    /// Looks for Expo apps again, e.g. after the link was edited in "Lojas e EAS".
    pub fn detect(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        self.roots = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path())
            .collect();
        let roots = self.roots.clone();
        let fs = self.fs.clone();
        let key_value_store = KeyValueStore::global(cx);
        self.detection = Detection::Detecting;
        cx.notify();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let mut found = Vec::new();
            for root in &roots {
                found.extend(detect_apps(fs.as_ref(), root).await);
            }
            if found.is_empty()
                && let Some(root) = roots.first()
            {
                found.push((root.clone(), AppLink::default()));
            }
            let apps = found
                .into_iter()
                .map(|(folder, detected)| {
                    let overrides = load_link_override(&key_value_store, &folder.to_string_lossy());
                    AppReleases {
                        link: detected.merged_with(&overrides),
                        detected,
                        folder,
                        releases: Vec::new(),
                        sources: Sources::default(),
                        refreshed_at: None,
                        app_store_app_id: None,
                        beta_groups: Vec::new(),
                        google_play_tracks: Vec::new(),
                    }
                })
                .collect::<Vec<_>>();
            this.update(cx, |this, cx| {
                let previous_selection = this
                    .apps
                    .get(this.selected_app)
                    .map(|app| app.folder.clone());
                let mut apps = apps;
                for app in &mut apps {
                    if let Some(previous) =
                        this.apps.iter_mut().find(|old| old.folder == app.folder)
                    {
                        app.releases = std::mem::take(&mut previous.releases);
                        app.app_store_app_id = previous.app_store_app_id.take();
                    }
                }
                this.apps = apps;
                this.selected_app = previous_selection
                    .and_then(|folder| this.apps.iter().position(|app| app.folder == folder))
                    .unwrap_or(0);
                this.detection = Detection::Done;
                this.refresh_task = None;
                this.refresh(cx);
            })
            .log_err();
        }));
    }

    /// Refreshes unless the last refresh is recent, for when the dock opens.
    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let stale = self.apps.iter().any(|app| {
            app.refreshed_at
                .is_none_or(|refreshed_at| Local::now() - refreshed_at > STALE_AFTER)
        });
        if stale && !self.refreshing {
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if matches!(self.detection, Detection::Detecting) {
            return;
        }
        let Some(accounts) = self.accounts.read(cx).accounts().cloned() else {
            return;
        };
        if !self.warned_token_expiry
            && let Some(days) = accounts
                .expo
                .as_ref()
                .and_then(|expo| expo.expires_soon(Local::now().date_naive()))
        {
            self.warned_token_expiry = true;
            cx.emit(ReleasesEvent::TokenExpiring { days });
        }
        let http_client = self.accounts.read(cx).http_client();
        let requests: Vec<(Arc<Path>, AppLink, Option<String>)> = self
            .apps
            .iter()
            .map(|app| {
                (
                    app.folder.clone(),
                    app.link.clone(),
                    app.app_store_app_id.clone(),
                )
            })
            .collect();
        self.refreshing = true;
        cx.notify();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let mut results = Vec::new();
            for (folder, link, app_store_app_id) in requests {
                let result = cx
                        .background_spawn({
                            let http_client = http_client.clone();
                            let accounts = accounts.clone();
                            async move {
                                fetch_app(&http_client, &accounts, &link, app_store_app_id).await
                            }
                        })
                        .await;
                results.push((folder, result));
            }
            this.update(cx, |this, cx| this.finish_refresh(results, cx))
                .log_err();
        }));
    }

    fn finish_refresh(&mut self, results: Vec<(Arc<Path>, Fetched)>, cx: &mut Context<Self>) {
        self.refreshing = false;
        let mut notices = Vec::new();
        for (folder, fetched) in results {
            let Some(app) = self.apps.iter_mut().find(|app| app.folder == folder) else {
                continue;
            };
            let mut snapshot = fetched.snapshot;
            snapshot.platforms = app.platforms();
            let releases = model::build_releases(&snapshot);
            if app.refreshed_at.is_some() {
                notices.extend(model::notable_changes(&app.releases, &releases));
            }
            app.releases = releases;
            app.sources = fetched.sources;
            app.refreshed_at = Some(Local::now());
            if fetched.app_store_app_id.is_some() {
                app.app_store_app_id = fetched.app_store_app_id;
            }
            if !fetched.beta_groups.is_empty() {
                app.beta_groups = fetched.beta_groups;
            }
            app.google_play_tracks = snapshot.google_play.unwrap_or_default();
        }
        for notice in notices {
            cx.emit(ReleasesEvent::Notice(notice));
        }
        let moving = self
            .apps
            .iter()
            .any(|app| model::anything_moving(&app.releases));
        self.poll_task = moving.then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(POLL_INTERVAL).await;
                this.update(cx, |this, cx| this.refresh(cx)).log_err();
            })
        });
        cx.notify();
    }

    /// Releases of every app of the project, for the status bar chip.
    pub fn all_releases(&self) -> impl Iterator<Item = (&AppReleases, &[Release])> {
        self.apps.iter().map(|app| (app, app.releases.as_slice()))
    }

    /// Sends a write the person already confirmed, then reads the stores again.
    pub fn run_write(
        &mut self,
        folder: &Path,
        write: StoreWrite,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(accounts) = self.accounts.read(cx).accounts().cloned() else {
            return Task::ready(Err(anyhow::anyhow!("{}", t!("app_releases.dock.loading"))));
        };
        let package = self
            .app_for_folder(folder)
            .and_then(|app| app.link.package.clone());
        let app_store_app_id = self
            .app_for_folder(folder)
            .and_then(|app| app.app_store_app_id.clone());
        let http_client = self.accounts.read(cx).http_client();
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                send_write(&http_client, &accounts, package, app_store_app_id, write).await
            })
            .await?;
            this.update(cx, |this, cx| this.refresh(cx))
        })
    }
}

impl ReleasesStore {
    /// Ratings, tester feedback and vitals of an app, read when the release tab asks.
    pub fn fetch_after_launch(
        &self,
        folder: &Path,
        cx: &mut Context<Self>,
    ) -> Task<crate::insights::AfterLaunch> {
        let accounts_store = self.accounts.read(cx);
        let accounts = accounts_store.accounts().cloned().unwrap_or_default();
        let http_client = accounts_store.http_client();
        let app = self.app_for_folder(folder);
        let app_store_app_id = app.and_then(|app| app.app_store_app_id.clone());
        let package = app.and_then(|app| app.link.package.clone());
        cx.background_spawn(async move {
            crate::insights::fetch_after_launch(&http_client, &accounts, app_store_app_id, package)
                .await
        })
    }

    /// What's New of every language of an App Store version, for editing.
    pub fn fetch_version_notes(
        &self,
        version_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<asc::Localization>>> {
        let accounts = self.accounts.read(cx);
        let Some(credentials) = accounts
            .accounts()
            .and_then(|accounts| accounts.app_store.as_ref())
            .map(|account| account.credentials())
        else {
            return Task::ready(Err(missing_account(AccountKind::AppStore)));
        };
        let http_client = accounts.http_client();
        cx.background_spawn(async move {
            asc::fetch_version_notes(&http_client, &credentials, &version_id).await
        })
    }
}

/// A change to a store. Every variant is only ever sent after an explicit confirmation.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreWrite {
    AddBuildToGroup {
        build_id: String,
        group_id: String,
    },
    WhatToTest {
        build_id: String,
        locale: String,
        text: String,
    },
    SubmitForBetaReview {
        build_id: String,
    },
    /// What's New per language: `(localization id, text)`.
    VersionNotes {
        notes: Vec<(String, String)>,
    },
    SubmitForReview {
        version_id: String,
    },
    StartPhasedRelease {
        version_id: String,
    },
    SetPhasedRelease {
        phased_release_id: String,
        state: asc::PhasedReleaseState,
    },
    ReleaseVersion {
        version_id: String,
    },
    Play(google_play::TrackChange),
}

impl StoreWrite {
    pub fn service(&self) -> AccountKind {
        match self {
            Self::Play(_) => AccountKind::GooglePlay,
            _ => AccountKind::AppStore,
        }
    }
}

fn missing_account(kind: AccountKind) -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        t!("app_releases.write.no_account", service = kind.label())
    )
}

async fn send_write(
    http_client: &Arc<dyn HttpClient>,
    accounts: &StoredAccounts,
    package: Option<String>,
    app_store_app_id: Option<String>,
    write: StoreWrite,
) -> Result<()> {
    let app_store = || {
        accounts
            .app_store
            .as_ref()
            .map(|account| account.credentials())
            .ok_or_else(|| missing_account(AccountKind::AppStore))
    };
    match write {
        StoreWrite::AddBuildToGroup { build_id, group_id } => {
            asc::add_build_to_group(http_client, &app_store()?, &group_id, &build_id).await
        }
        StoreWrite::WhatToTest {
            build_id,
            locale,
            text,
        } => asc::set_what_to_test(http_client, &app_store()?, &build_id, &locale, &text).await,
        StoreWrite::SubmitForBetaReview { build_id } => {
            asc::submit_for_beta_review(http_client, &app_store()?, &build_id).await
        }
        StoreWrite::VersionNotes { notes } => {
            let credentials = app_store()?;
            for (localization_id, text) in notes {
                asc::set_whats_new(http_client, &credentials, &localization_id, &text).await?;
            }
            Ok(())
        }
        StoreWrite::SubmitForReview { version_id } => {
            let app_id = app_store_app_id.ok_or_else(|| missing_account(AccountKind::AppStore))?;
            asc::submit_for_review(http_client, &app_store()?, &app_id, &version_id).await
        }
        StoreWrite::StartPhasedRelease { version_id } => {
            asc::create_phased_release(http_client, &app_store()?, &version_id).await
        }
        StoreWrite::SetPhasedRelease {
            phased_release_id,
            state,
        } => {
            asc::set_phased_release_state(http_client, &app_store()?, &phased_release_id, state)
                .await
        }
        StoreWrite::ReleaseVersion { version_id } => {
            asc::release_version(http_client, &app_store()?, &version_id).await
        }
        StoreWrite::Play(change) => {
            let account = accounts
                .google_play
                .as_ref()
                .ok_or_else(|| missing_account(AccountKind::GooglePlay))?;
            let package = package.ok_or_else(|| missing_account(AccountKind::GooglePlay))?;
            google_play::update_track(http_client, &account.service_account, &package, &change)
                .await
        }
    }
}

pub struct Fetched {
    pub snapshot: Snapshot,
    pub sources: Sources,
    pub app_store_app_id: Option<String>,
    pub beta_groups: Vec<asc::BetaGroup>,
}

fn source_failure(service: &str, error: anyhow::Error) -> SourceStatus {
    if is_unauthorized(&error) {
        SourceStatus::Refused
    } else {
        log::warn!("Lançamentos: {service} falhou: {error:#}");
        SourceStatus::Failed(format!("{error:#}").into())
    }
}

/// Reads the three services for one app. Each one fails on its own, so a refused credential
/// only blanks its own steps.
pub async fn fetch_app(
    http_client: &Arc<dyn HttpClient>,
    accounts: &StoredAccounts,
    link: &AppLink,
    known_app_store_app_id: Option<String>,
) -> Fetched {
    let eas = async {
        let Some(expo) = &accounts.expo else {
            return (None, SourceStatus::NoAccount);
        };
        let Some(project_id) = &link.eas_project_id else {
            return (None, SourceStatus::NotLinked);
        };
        match eas::fetch_activity(http_client, &expo.token, project_id).await {
            Ok(activity) => (Some(activity), SourceStatus::Ready),
            Err(error) => (None, source_failure("EAS", error)),
        }
    };
    let app_store = async {
        let Some(account) = &accounts.app_store else {
            return (None, SourceStatus::NoAccount, Vec::new());
        };
        let Some(bundle_id) = &link.bundle_id else {
            return (None, SourceStatus::NotLinked, Vec::new());
        };
        let credentials = account.credentials();
        let app_id = match known_app_store_app_id {
            Some(app_id) => app_id,
            None => match asc::find_app(http_client, &credentials, bundle_id).await {
                Ok(Some(app)) => app.id,
                Ok(None) => return (None, SourceStatus::AppNotFound, Vec::new()),
                Err(error) => {
                    return (None, source_failure("App Store Connect", error), Vec::new());
                }
            },
        };
        let (builds, versions, groups) = futures::join!(
            asc::fetch_builds(http_client, &credentials, &app_id),
            asc::fetch_versions(http_client, &credentials, &app_id),
            asc::fetch_beta_groups(http_client, &credentials, &app_id),
        );
        let groups = groups
            .map_err(|error| log::warn!("Lançamentos: grupos do TestFlight: {error:#}"))
            .unwrap_or_default();
        match (builds, versions) {
            (Ok(builds), Ok(versions)) => (
                Some(AppStoreData {
                    app_id,
                    builds,
                    versions,
                }),
                SourceStatus::Ready,
                groups,
            ),
            (Err(error), _) | (_, Err(error)) => {
                (None, source_failure("App Store Connect", error), Vec::new())
            }
        }
    };
    let google_play = async {
        let Some(account) = &accounts.google_play else {
            return (None, SourceStatus::NoAccount);
        };
        let Some(package) = &link.package else {
            return (None, SourceStatus::NotLinked);
        };
        match google_play::fetch_tracks(http_client, &account.service_account, package).await {
            Ok(tracks) => (Some(tracks), SourceStatus::Ready),
            Err(error) => {
                let not_found = matches!(
                    error.downcast_ref::<crate::ApiError>(),
                    Some(crate::ApiError::Status { status: 404, .. })
                );
                if not_found {
                    (None, SourceStatus::AppNotFound)
                } else {
                    (None, source_failure("Google Play", error))
                }
            }
        }
    };
    let ((eas, eas_status), (app_store, app_store_status, beta_groups), (google_play, play_status)) =
        futures::join!(eas, app_store, google_play);
    let app_store_app_id = app_store
        .as_ref()
        .map(|data: &AppStoreData| data.app_id.clone());
    Fetched {
        snapshot: Snapshot {
            eas,
            app_store,
            google_play,
            platforms: Vec::new(),
        },
        sources: Sources {
            eas: eas_status,
            app_store: app_store_status,
            google_play: play_status,
        },
        app_store_app_id,
        beta_groups,
    }
}

async fn read_link(fs: &dyn Fs, folder: &Path) -> Option<AppLink> {
    for file in CONFIG_FILES {
        let path = folder.join(file);
        let Ok(text) = fs.load(&path).await else {
            continue;
        };
        let link = if file.ends_with(".json") {
            parse_app_json(&text)
        } else {
            scan_app_config(&text)
        };
        if link.is_some() {
            return link;
        }
    }
    None
}

async fn child_folders(fs: &dyn Fs, folder: &Path) -> Vec<PathBuf> {
    let Ok(mut entries) = fs.read_dir(folder).await else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else {
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.') || SKIPPED_FOLDERS.contains(&name.as_str()) {
            continue;
        }
        if fs.is_dir(&path).await {
            folders.push(path);
        }
    }
    folders.sort();
    folders
}

/// Expo apps at the root, one level down, or under `apps/` of a monorepo.
async fn detect_apps(fs: &dyn Fs, root: &Path) -> Vec<(Arc<Path>, AppLink)> {
    let mut found = Vec::new();
    if let Some(link) = read_link(fs, root).await {
        found.push((Arc::from(root), link));
        return found;
    }
    let mut candidates = child_folders(fs, root).await;
    candidates.extend(child_folders(fs, &root.join("apps")).await);
    for folder in candidates {
        if let Some(link) = read_link(fs, &folder).await {
            found.push((Arc::from(folder.as_path()), link));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::ExpoAccount, app_store_connect::tests::TEST_P8, eas::tests::ACTIVITY_FIXTURE,
    };
    use fs::FakeFs;
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response};
    use serde_json::json;

    #[gpui::test]
    async fn finds_expo_apps_in_a_monorepo(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                "apps": {
                    "mobile": {
                        "app.json": r#"{"expo":{"name":"Trix","ios":{"bundleIdentifier":"br.com.trix"}}}"#
                    },
                    "web": { "package.json": "{}" }
                },
                "node_modules": {
                    "lib": { "app.json": r#"{"expo":{"ios":{"bundleIdentifier":"x"}}}"# }
                }
            }),
        )
        .await;
        let apps = detect_apps(fs.as_ref(), Path::new("/repo")).await;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0.as_ref(), Path::new("/repo/apps/mobile"));
        assert_eq!(apps[0].1.bundle_id.as_deref(), Some("br.com.trix"));
    }

    #[gpui::test]
    async fn a_refused_store_does_not_stop_the_others(_cx: &mut TestAppContext) {
        let http_client = FakeHttpClient::create(|request| async move {
            let host = request.uri().host().unwrap_or_default().to_string();
            Ok(match host.as_str() {
                "api.expo.dev" => Response::builder()
                    .status(200)
                    .body(ACTIVITY_FIXTURE.into())?,
                "api.appstoreconnect.apple.com" => Response::builder()
                    .status(401)
                    .body(r#"{"errors":[{"status":"401","title":"NOT_AUTHORIZED"}]}"#.into())?,
                other => panic!("unexpected request to {other}"),
            })
        });
        let accounts = StoredAccounts {
            app_store: Some(accounts::AppStoreAccount {
                issuer_id: "issuer".into(),
                key_id: "KEY".into(),
                private_key: TEST_P8.into(),
            }),
            google_play: None,
            expo: Some(ExpoAccount {
                token: "expo".into(),
                expires_on: None,
            }),
        };
        let link = AppLink {
            name: Some("Trix".into()),
            bundle_id: Some("br.com.trix".into()),
            package: Some("br.com.trix".into()),
            eas_project_id: Some("f3c1".into()),
        };
        let fetched = fetch_app(&http_client, &accounts, &link, None).await;
        assert_eq!(fetched.sources.eas, SourceStatus::Ready);
        assert_eq!(fetched.sources.app_store, SourceStatus::Refused);
        assert_eq!(fetched.sources.google_play, SourceStatus::NoAccount);
        assert!(fetched.snapshot.eas.is_some());
        assert!(fetched.snapshot.app_store.is_none());
    }
}
