use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Task, TaskExt as _, WeakEntity,
};
use gpui_tokio::Tokio;
use project::Project;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{AwsProfile, CloudWatch, CloudWatchError, LogGroup, Target, connect, load_profiles};

pub const CLOUDWATCH_DIR: &str = ".asylum/cloudwatch";
pub const GROUPS_FILE: &str = ".asylum/cloudwatch/groups.json";
pub const QUERIES_DIR: &str = ".asylum/cloudwatch/queries";
pub const QUERY_EXTENSION: &str = "insights";
/// Agent queries over a wider window ask for confirmation first.
pub const AGENT_FREE_WINDOW: Duration = Duration::from_secs(60 * 60);
const LOG_GROUP_LIMIT: usize = 2000;

#[derive(Clone, Debug)]
pub enum Connection {
    Idle,
    Connecting,
    Connected(CloudWatch),
    SessionExpired {
        profile: Option<String>,
        since: SystemTime,
        message: String,
    },
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// `YYYY-MM-DD` in UTC; counters reset when the day changes.
    pub day: String,
    pub bytes_scanned: u64,
    pub queries: u32,
    pub tail_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedQuery {
    pub path: PathBuf,
    pub name: String,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    /// A connection came up; views that stopped on an expired session run again.
    Connected,
    SessionExpired,
    TargetChanged,
}

#[derive(Default, Serialize, Deserialize)]
struct GroupsFile {
    groups: Vec<String>,
}

/// CloudWatch state shared by the dock, the query and live tail tabs and the agent's tools of one
/// workspace.
pub struct CloudWatchStore {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    target: Target,
    connection: Connection,
    account_id: Option<String>,
    profiles: Vec<AwsProfile>,
    log_groups: Vec<LogGroup>,
    log_groups_error: Option<String>,
    loading_log_groups: bool,
    project_groups: Vec<String>,
    saved_queries: Vec<SavedQuery>,
    usage: Usage,
    _connect: Task<()>,
    _load: Task<()>,
}

impl EventEmitter<StoreEvent> for CloudWatchStore {}

#[derive(Default)]
struct Stores(Vec<(WeakEntity<Workspace>, Entity<CloudWatchStore>)>);

impl Global for Stores {}

impl CloudWatchStore {
    /// Takes the handle and project instead of the workspace entity because callers run while the
    /// workspace is being updated.
    pub fn for_workspace(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Entity<Self> {
        let stores = cx.default_global::<Stores>();
        stores
            .0
            .retain(|(workspace, _)| workspace.upgrade().is_some());
        if let Some((_, store)) = stores.0.iter().find(|(existing, _)| *existing == workspace) {
            return store.clone();
        }
        let store = cx.new(|cx| Self::new(workspace.clone(), project, cx));
        cx.default_global::<Stores>()
            .0
            .push((workspace, store.clone()));
        store
    }

    pub fn try_for_workspace(workspace: &WeakEntity<Workspace>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Stores>()?
            .0
            .iter()
            .find(|(existing, _)| existing == workspace)
            .map(|(_, store)| store.clone())
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let project_key = project_key(&project, cx);
        let target = project_key
            .as_deref()
            .and_then(|key| read_json::<Target>(&target_key(key), cx))
            .unwrap_or_default();
        let usage = read_json::<Usage>(&usage_key(target.profile.as_deref()), cx)
            .filter(|usage| usage.day == today())
            .unwrap_or_else(|| Usage {
                day: today(),
                ..Usage::default()
            });
        let mut this = Self {
            workspace,
            project,
            target,
            connection: Connection::Idle,
            account_id: None,
            profiles: Vec::new(),
            log_groups: Vec::new(),
            log_groups_error: None,
            loading_log_groups: false,
            project_groups: Vec::new(),
            saved_queries: Vec::new(),
            usage,
            _connect: Task::ready(()),
            _load: Task::ready(()),
        };
        this.reload_project_files(cx);
        this.reload_profiles(cx);
        this.connect(cx);
        this
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn client(&self) -> Option<CloudWatch> {
        match &self.connection {
            Connection::Connected(client) => Some(client.clone()),
            _ => None,
        }
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn profiles(&self) -> &[AwsProfile] {
        &self.profiles
    }

    /// The profile in use, after the same default resolution the SDK applies.
    pub fn profile_name(&self) -> String {
        self.target
            .profile
            .clone()
            .or_else(|| std::env::var("AWS_PROFILE").ok())
            .unwrap_or_else(|| "default".to_string())
    }

    pub fn region(&self) -> Option<String> {
        if let Some(client) = self.client() {
            return Some(client.region().to_string());
        }
        self.target.region.clone().or_else(|| {
            let profile = self.profile_name();
            self.profiles
                .iter()
                .find(|candidate| candidate.name == profile)
                .and_then(|profile| profile.region.clone())
        })
    }

    pub fn log_groups(&self) -> &[LogGroup] {
        &self.log_groups
    }

    pub fn log_groups_error(&self) -> Option<&str> {
        self.log_groups_error.as_deref()
    }

    pub fn is_loading_log_groups(&self) -> bool {
        self.loading_log_groups
    }

    pub fn project_groups(&self) -> &[String] {
        &self.project_groups
    }

    pub fn saved_queries(&self) -> &[SavedQuery] {
        &self.saved_queries
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        if target == self.target {
            return;
        }
        self.target = target;
        if let Some(key) = project_key(&self.project, cx) {
            write_json(target_key(&key), &self.target, cx);
        }
        self.usage = read_json::<Usage>(&usage_key(self.target.profile.as_deref()), cx)
            .filter(|usage| usage.day == today())
            .unwrap_or_else(|| Usage {
                day: today(),
                ..Usage::default()
            });
        cx.emit(StoreEvent::TargetChanged);
        self.connect(cx);
    }

    /// Resolves credentials again, as after `aws login` refreshed the cache.
    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.connection = Connection::Connecting;
        cx.notify();
        let target = self.target.clone();
        let http_client = self.project.read(cx).client().http_client();
        let connect = Tokio::spawn(cx, async move {
            let client = connect(&target, http_client).await?;
            let account_id = client.account_id().await.log_err();
            let groups = client.list_log_groups(None, LOG_GROUP_LIMIT).await;
            Ok::<_, CloudWatchError>((client, account_id, groups))
        });
        self.loading_log_groups = true;
        self._connect = cx.spawn(async move |this, cx| {
            let result = match connect.await {
                Ok(result) => result,
                Err(error) => Err(CloudWatchError::Other(error.into())),
            };
            this.update(cx, |this, cx| {
                this.loading_log_groups = false;
                match result {
                    Ok((client, account_id, groups)) => {
                        this.connection = Connection::Connected(client);
                        this.account_id = account_id;
                        match groups {
                            Ok(groups) => {
                                this.log_groups = groups;
                                this.log_groups_error = None;
                            }
                            Err(error) => {
                                this.log_groups_error = Some(error.to_string());
                                this.report_error(&error, cx);
                            }
                        }
                        cx.emit(StoreEvent::Connected);
                    }
                    Err(error) => {
                        this.report_error(&error, cx);
                        if !matches!(this.connection, Connection::SessionExpired { .. }) {
                            this.connection = Connection::Failed(error.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    /// Every caller that gets an error back from AWS hands it here, so an expired session shows up
    /// in the dock and the other tabs no matter which one hit it first.
    pub fn report_error(&mut self, error: &CloudWatchError, cx: &mut Context<Self>) {
        if let CloudWatchError::SessionExpired { profile, message } = error {
            if !matches!(self.connection, Connection::SessionExpired { .. }) {
                self.connection = Connection::SessionExpired {
                    profile: profile.clone(),
                    since: SystemTime::now(),
                    message: message.clone(),
                };
                cx.emit(StoreEvent::SessionExpired);
                cx.notify();
            }
        }
    }

    pub fn record_query(&mut self, bytes_scanned: u64, cx: &mut Context<Self>) {
        self.roll_usage_day();
        self.usage.queries += 1;
        self.usage.bytes_scanned += bytes_scanned;
        self.save_usage(cx);
    }

    pub fn record_tail(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.roll_usage_day();
        self.usage.tail_seconds += seconds;
        self.save_usage(cx);
    }

    fn roll_usage_day(&mut self) {
        let day = today();
        if self.usage.day != day {
            self.usage = Usage {
                day,
                ..Usage::default()
            };
        }
    }

    fn save_usage(&mut self, cx: &mut Context<Self>) {
        write_json(usage_key(self.target.profile.as_deref()), &self.usage, cx);
        cx.notify();
    }

    pub fn reload_profiles(&mut self, cx: &mut Context<Self>) {
        let load = Tokio::spawn_result(cx, load_profiles());
        cx.spawn(async move |this, cx| {
            let profiles = load.await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.profiles = profiles;
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn reload_log_groups(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client() else {
            self.connect(cx);
            return;
        };
        self.loading_log_groups = true;
        cx.notify();
        let load = Tokio::spawn(cx, async move {
            client.list_log_groups(None, LOG_GROUP_LIMIT).await
        });
        self._load = cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(result) => result,
                Err(error) => Err(CloudWatchError::Other(error.into())),
            };
            this.update(cx, |this, cx| {
                this.loading_log_groups = false;
                match result {
                    Ok(groups) => {
                        this.log_groups = groups;
                        this.log_groups_error = None;
                    }
                    Err(error) => {
                        this.log_groups_error = Some(error.to_string());
                        this.report_error(&error, cx);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    pub fn queries_dir(&self, cx: &App) -> Option<PathBuf> {
        self.project_root(cx).map(|root| root.join(QUERIES_DIR))
    }

    /// Reads `groups.json` and lists the saved queries again.
    pub fn reload_project_files(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project_root(cx) else {
            return;
        };
        let fs = self.project.read(cx).fs().clone();
        cx.spawn(async move |this, cx| {
            let groups = read_groups(fs.as_ref(), &root)
                .await
                .log_err()
                .unwrap_or_default();
            let queries = list_queries(fs.as_ref(), &root.join(QUERIES_DIR)).await;
            this.update(cx, |this, cx| {
                this.project_groups = groups;
                this.saved_queries = queries;
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub fn set_project_group(&mut self, group: &str, included: bool, cx: &mut Context<Self>) {
        let already = self.project_groups.iter().any(|existing| existing == group);
        if included == already {
            return;
        }
        if included {
            self.project_groups.push(group.to_string());
        } else {
            self.project_groups.retain(|existing| existing != group);
        }
        cx.notify();
        let Some(root) = self.project_root(cx) else {
            return;
        };
        let fs = self.project.read(cx).fs().clone();
        let groups = self.project_groups.clone();
        cx.background_spawn(async move { write_groups(fs.as_ref(), &root, groups).await })
            .detach_and_log_err(cx);
    }
}

async fn read_groups(fs: &dyn Fs, root: &Path) -> anyhow::Result<Vec<String>> {
    let path = root.join(GROUPS_FILE);
    if !fs.is_file(&path).await {
        return Ok(Vec::new());
    }
    let text = fs.load(&path).await?;
    let file: GroupsFile =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.groups)
}

async fn write_groups(fs: &dyn Fs, root: &Path, groups: Vec<String>) -> anyhow::Result<()> {
    let dir = root.join(CLOUDWATCH_DIR);
    fs.create_dir(&dir).await?;
    let text = serde_json::to_string_pretty(&GroupsFile { groups })? + "\n";
    fs.atomic_write(root.join(GROUPS_FILE), text).await
}

async fn list_queries(fs: &dyn Fs, dir: &Path) -> Vec<SavedQuery> {
    use futures::StreamExt as _;

    let Ok(mut entries) = fs.read_dir(dir).await else {
        return Vec::new();
    };
    let mut queries = Vec::new();
    while let Some(entry) = entries.next().await {
        let Some(path) = entry.log_err() else {
            continue;
        };
        if path.extension().and_then(|extension| extension.to_str()) != Some(QUERY_EXTENSION) {
            continue;
        }
        let modified = fs
            .metadata(&path)
            .await
            .log_err()
            .flatten()
            .map(|metadata| metadata.mtime.timestamp_for_user());
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        queries.push(SavedQuery {
            path,
            name,
            modified,
        });
    }
    queries.sort_by(|a, b| b.modified.cmp(&a.modified));
    queries
}

fn project_key(project: &Entity<Project>, cx: &App) -> Option<String> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

fn target_key(project_key: &str) -> String {
    format!("cloudwatch.target.{project_key}")
}

fn usage_key(profile: Option<&str>) -> String {
    format!("cloudwatch.usage.{}", profile.unwrap_or("default"))
}

fn read_json<T: for<'de> Deserialize<'de>>(key: &str, cx: &App) -> Option<T> {
    KeyValueStore::global(cx)
        .read_kvp(key)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
}

fn write_json<T: Serialize>(key: String, value: &T, cx: &App) {
    let Some(json) = serde_json::to_string(value).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move { store.write_kvp(key, json).await })
        .detach_and_log_err(cx);
}

fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() / 86_400)
        .unwrap_or_default();
    civil_date(days as i64)
}

/// Days since 1970-01-01 to `YYYY-MM-DD` (Howard Hinnant's algorithm).
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_774), "2026-11-17");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }

    #[gpui::test]
    async fn groups_file_round_trip(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({})).await;
        let root = Path::new("/project");
        assert!(read_groups(fs.as_ref(), root).await.unwrap().is_empty());
        write_groups(fs.as_ref(), root, vec!["/ecs/trix-api-prod".into()])
            .await
            .unwrap();
        assert_eq!(
            read_groups(fs.as_ref(), root).await.unwrap(),
            vec!["/ecs/trix-api-prod".to_string()]
        );
    }
}
