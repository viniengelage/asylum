//! Profiles: separate installations (settings, data and credentials) that run side by side,
//! one process each. This is the registry of known profiles and the status bar control that
//! shows the active one and switches to, or creates, another.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use fs::{Fs, RemoveOptions};
use gpui::{
    Anchor, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global, Hsla,
    Task, WeakEntity, Window, prelude::*, rgb,
};
use language_model::LanguageModelRegistry;
use project::context_server_store::ContextServerStatus;
use project::project_settings::{ContextServerSettings, ProjectSettings};
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};
use ui::{
    ButtonLike, Divider, IconButton, IconName, IconSize, Indicator, Label, LabelSize, PopoverMenu,
    PopoverMenuHandle, Switch, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use util::paths::{PathMatcher, PathStyle, PathWithPosition};
use workspace::{
    HideStatusItem, ItemHandle, ModalView, StatusItemView, Workspace,
    notifications::NotifyTaskExt as _,
};

use crate::zed::mac_only_instance;

const DEFAULT_PROFILE_NAME: &str = "Padrão";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileColor {
    Amber,
    Teal,
    Purple,
    Blue,
    Rose,
}

impl ProfileColor {
    const ALL: [Self; 5] = [
        Self::Amber,
        Self::Teal,
        Self::Purple,
        Self::Blue,
        Self::Rose,
    ];

    fn hsla(self) -> Hsla {
        match self {
            Self::Amber => rgb(0xE8B368).into(),
            Self::Teal => rgb(0x5EEAD4).into(),
            Self::Purple => rgb(0xC084FC).into(),
            Self::Blue => rgb(0x60A5FA).into(),
            Self::Rose => rgb(0xFB7185).into(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Amber => "Âmbar",
            Self::Teal => "Verde-água",
            Self::Purple => "Roxo",
            Self::Blue => "Azul",
            Self::Rose => "Rosa",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppProfile {
    pub id: String,
    pub name: String,
    pub color: ProfileColor,
    /// Folders that always open in this profile, with everything inside them. An entry may
    /// start with `~` and may be a glob, such as `~/Projects/trix/**`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
}

impl AppProfile {
    fn initial(&self) -> SharedString {
        self.name
            .chars()
            .next()
            .map(|initial| initial.to_uppercase().collect::<String>())
            .unwrap_or_default()
            .into()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    profiles: Vec<AppProfile>,
}

// Every profile reads the same registry, so it sits next to their data directories rather
// than inside any one of them.
fn registry_path() -> PathBuf {
    paths::profiles_dir().join("profiles.json")
}

fn default_profile() -> AppProfile {
    AppProfile {
        id: paths::DEFAULT_PROFILE_ID.to_string(),
        name: DEFAULT_PROFILE_NAME.to_string(),
        color: ProfileColor::Amber,
        folders: Vec::new(),
    }
}

/// Loads the registry, which always lists the default profile first even before anything
/// was saved.
fn load_profiles() -> Result<Vec<AppProfile>> {
    let mut profiles = match std::fs::read(registry_path()) {
        Ok(json) => {
            serde_json::from_slice::<Registry>(&json)
                .context("failed to parse profiles.json")?
                .profiles
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).context("failed to read profiles.json"),
    };
    profiles.retain(|profile| paths::is_valid_profile_id(&profile.id));
    let default_position = profiles
        .iter()
        .position(|profile| profile.id == paths::DEFAULT_PROFILE_ID);
    let default = match default_position {
        Some(position) => profiles.remove(position),
        None => default_profile(),
    };
    profiles.insert(0, default);
    Ok(profiles)
}

fn save_profiles(profiles: &[AppProfile]) -> Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create the profiles directory")?;
    }
    let json = serde_json::to_vec_pretty(&Registry {
        profiles: profiles.to_vec(),
    })?;
    std::fs::write(&path, json).context("failed to write profiles.json")
}

/// Turns a profile name into an id that is unique among `taken`, dropping accents so that
/// "Pessoal" and "Família" become `pessoal` and `familia`.
fn profile_id_for_name(name: &str, taken: &[AppProfile]) -> String {
    let mut base = String::new();
    for character in name.trim().to_lowercase().chars() {
        let folded = match character {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            character if character.is_ascii_alphanumeric() => character,
            _ => '-',
        };
        if folded == '-' && (base.is_empty() || base.ends_with('-')) {
            continue;
        }
        base.push(folded);
    }
    let mut base = base.trim_end_matches('-').to_string();
    base.truncate(48);
    if base.is_empty() || base == paths::DEFAULT_PROFILE_ID {
        base = "perfil".to_string();
    }

    let is_taken = |id: &str| taken.iter().any(|profile| profile.id == id);
    if !is_taken(&base) {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|id| !is_taken(id))
        .unwrap_or(base)
}

fn expand_home(entry: &str) -> PathBuf {
    match entry.strip_prefix("~") {
        Some(rest) => paths::home_dir().join(rest.trim_start_matches('/')),
        None => PathBuf::from(entry),
    }
}

/// How specifically `entry` claims `path`: the length of the entry when it does, so that
/// `~/Projects/trix/app` wins over `~/Projects/trix` for a file inside the app.
fn folder_match_len(entry: &str, path: &Path) -> Option<usize> {
    let expanded = expand_home(entry.trim());
    let is_glob = entry.contains(['*', '?', '[', '{']);
    let matches = if is_glob {
        let Some(glob) = expanded.to_str() else {
            return None;
        };
        PathMatcher::new([glob], PathStyle::local())
            .map(|matcher| matcher.is_match_std_path(path))
            .unwrap_or_else(|error| {
                log::warn!("Ignoring the invalid profile folder {entry:?}: {error}");
                false
            })
    } else {
        path.starts_with(&expanded)
    };
    matches.then_some(entry.len())
}

/// The profile whose folders claim `path` most specifically.
fn owner_of<'a>(path: &Path, profiles: &'a [AppProfile]) -> Option<&'a AppProfile> {
    profiles
        .iter()
        .filter_map(|profile| {
            let longest = profile
                .folders
                .iter()
                .filter_map(|entry| folder_match_len(entry, path))
                .max()?;
            Some((longest, profile))
        })
        .max_by_key(|(longest, _)| *longest)
        .map(|(_, profile)| profile)
}

/// The profile other than this one that every one of `paths` belongs to. Paths opened
/// from outside the app (the `zed` CLI, Finder) reach whichever profile macOS picks, and
/// this is how they find their way to the right one.
pub fn foreign_owner(paths: &[String]) -> Option<AppProfile> {
    if paths.is_empty() {
        return None;
    }
    let profiles = load_profiles().log_err()?;
    let mut owner: Option<&AppProfile> = None;
    for path in paths {
        let parsed = PathWithPosition::parse_str(path);
        let candidate = owner_of(&parsed.path, &profiles)?;
        match owner {
            Some(owner) if owner.id != candidate.id => return None,
            _ => owner = Some(candidate),
        }
    }
    owner
        .filter(|owner| owner.id != active_profile_id())
        .cloned()
}

gpui::actions!(
    profiles,
    [
        /// Opens the list of profiles to rename, recolor, tie folders to or delete them.
        ManageProfiles
    ]
);

fn open_manager(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = ProfileStore::global(cx);
    let weak_workspace = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        ManageProfilesModal::new(store, weak_workspace, window, cx)
    });
}

/// Registers the profile actions and lets File > Open offer to open a folder in the profile
/// it belongs to.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ManageProfiles, window, cx| {
            open_manager(workspace, window, cx)
        });
    })
    .detach();

    cx.set_global(workspace::ForeignPathsHandler(Arc::new(|paths, _cx| {
        let paths: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let owner = foreign_owner(&paths)?;
        Some(workspace::ForeignPaths {
            owner_name: owner.name.into(),
            open_in_owner: Box::new(move |cx| forward_paths(owner.id, paths, cx)),
        })
    })));
}

/// Opens `paths` in the running instance of `profile_id`, starting it with them when it
/// isn't running.
pub fn forward_paths(profile_id: String, paths: Vec<String>, cx: &App) -> Task<Result<()>> {
    let app_path = cx.app_path().ok();
    cx.background_spawn(async move {
        let request = mac_only_instance::InstanceRequest::Open {
            paths: paths.clone(),
        };
        if mac_only_instance::send_to_profile(&profile_id, &request) {
            return Ok(());
        }
        launch_profile(&profile_id, paths, app_path).await
    })
}

fn active_profile_id() -> &'static str {
    paths::active_profile_id().unwrap_or(paths::DEFAULT_PROFILE_ID)
}

fn profile_arguments(profile_id: &str) -> Vec<OsString> {
    vec![OsString::from("--profile"), OsString::from(profile_id)]
}

/// Brings `profile_id` to the front, starting it when it isn't running.
fn open_profile(profile_id: String, cx: &App) -> Task<Result<()>> {
    let app_path = cx.app_path().ok();
    cx.background_spawn(async move {
        if mac_only_instance::activate_profile(&profile_id) {
            return Ok(());
        }
        launch_profile(&profile_id, Vec::new(), app_path).await
    })
}

async fn launch_profile(
    profile_id: &str,
    paths: Vec<String>,
    app_path: Option<PathBuf>,
) -> Result<()> {
    let bundle =
        app_path.filter(|path| path.extension().is_some_and(|extension| extension == "app"));
    match bundle {
        Some(bundle) => {
            // `-n` starts another instance of the bundle instead of activating the running one.
            let status = util::command::new_command("open")
                .arg("-n")
                .arg(&bundle)
                .arg("--args")
                .args(profile_arguments(profile_id))
                .args(&paths)
                .status()
                .await
                .context("failed to run open")?;
            anyhow::ensure!(status.success(), "open exited with {status}");
        }
        None => {
            let executable = std::env::current_exe().context("failed to locate the executable")?;
            util::command::new_command(executable)
                .args(profile_arguments(profile_id))
                .args(&paths)
                .spawn()
                .context("failed to start the profile")?;
        }
    }
    Ok(())
}

/// Runs `action` in the process of `profile_id`, starting it when it isn't running. A profile
/// only edits its own settings and keychain entries, so another profile's manager asks it to
/// open them instead.
fn run_in_profile(
    profile_id: String,
    action: &'static str,
    data: Option<serde_json::Value>,
    cx: &App,
) -> Task<Result<()>> {
    let app_path = cx.app_path().ok();
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        let request = mac_only_instance::InstanceRequest::Dispatch {
            action: action.to_string(),
            data,
        };
        if mac_only_instance::send_to_profile(&profile_id, &request) {
            return Ok(());
        }
        launch_profile(&profile_id, Vec::new(), app_path).await?;
        // A profile that was just started takes a moment to claim its socket.
        for _ in 0..80 {
            executor.timer(Duration::from_millis(250)).await;
            if mac_only_instance::send_to_profile(&profile_id, &request) {
                return Ok(());
            }
        }
        anyhow::bail!("o perfil abriu, mas não respondeu a tempo")
    })
}

/// Replaces this process with `profile_id`: its windows close and the profile opens in
/// their place, or comes to the front when it is already running.
fn switch_in_place(profile_id: String, window: &mut Window, cx: &mut App) {
    let running = cx.background_spawn({
        let profile_id = profile_id.clone();
        async move { mac_only_instance::activate_profile(&profile_id) }
    });
    window
        .spawn(cx, async move |cx| {
            let activated = running.await;
            cx.update(|_, cx| {
                if activated {
                    cx.quit();
                } else {
                    cx.set_restart_arguments(profile_arguments(&profile_id));
                    cx.restart();
                }
            })
        })
        .detach_and_log_err(cx);
}

/// The known profiles and which of them are running, shared by every window's indicator.
/// What a profile is set up with, as its own process last saw it. A profile can't read the
/// others' settings or keychain entries, so each one publishes this for the manager to show.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct ProfileSummary {
    #[serde(default)]
    ai_providers: Vec<String>,
    default_model: Option<String>,
    agent_profile: Option<String>,
    #[serde(default)]
    mcp_servers: Vec<McpServerSummary>,
    /// `None` when the ClickUp integration isn't part of this build.
    clickup_connected: Option<bool>,
    git_committer: Option<String>,
    #[serde(default)]
    git_name: Option<String>,
    #[serde(default)]
    git_email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct McpServerSummary {
    id: String,
    detail: String,
    enabled: bool,
}

const CLICKUP_CREDENTIALS_URL: &str = "https://app.clickup.com/asylum";
const CLICKUP_OPEN_ACTION: &str = "clickup::ToggleFocus";
const CLICKUP_DISCONNECT_ACTION: &str = "clickup::Disconnect";

fn summaries_dir() -> PathBuf {
    paths::profiles_dir().join("summaries")
}

fn load_summaries(profiles: &[AppProfile]) -> HashMap<String, ProfileSummary> {
    profiles
        .iter()
        .filter_map(|profile| {
            let json = std::fs::read(summaries_dir().join(format!("{}.json", profile.id))).ok()?;
            let summary = serde_json::from_slice(&json).log_err()?;
            Some((profile.id.clone(), summary))
        })
        .collect()
}

fn mcp_server_detail(settings: &ContextServerSettings) -> String {
    match settings {
        ContextServerSettings::Stdio { command, .. } => {
            std::iter::once(command.path.display().to_string())
                .chain(command.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ")
        }
        ContextServerSettings::Http { url, .. } => url.clone(),
        ContextServerSettings::Extension { .. } => "extensão".to_string(),
    }
}

fn mcp_server_summaries(cx: &App) -> Vec<McpServerSummary> {
    let mut servers: Vec<McpServerSummary> = ProjectSettings::get_global(cx)
        .context_servers
        .iter()
        .map(|(id, settings)| McpServerSummary {
            id: id.to_string(),
            detail: mcp_server_detail(settings),
            enabled: settings.enabled(),
        })
        .collect();
    servers.sort_by(|a, b| a.id.cmp(&b.id));
    servers
}

pub struct ProfileStore {
    profiles: Vec<AppProfile>,
    running: HashSet<String>,
    summaries: HashMap<String, ProfileSummary>,
    clickup_connected: Option<bool>,
    git_committer: Option<String>,
    git_identity: git::repository::GitCommitter,
    load_error: Option<SharedString>,
    refresh_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

struct GlobalProfileStore(Entity<ProfileStore>);

impl Global for GlobalProfileStore {}

impl ProfileStore {
    fn global(cx: &mut App) -> Entity<ProfileStore> {
        if let Some(store) = cx.try_global::<GlobalProfileStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let subscriptions = vec![
                cx.observe_global::<SettingsStore>(|store: &mut ProfileStore, cx| {
                    store.publish_summary(cx)
                }),
                cx.subscribe(
                    &LanguageModelRegistry::global(cx),
                    |store, _, _: &language_model::Event, cx| store.publish_summary(cx),
                ),
            ];
            let mut store = ProfileStore {
                profiles: vec![default_profile()],
                running: HashSet::default(),
                summaries: HashMap::default(),
                clickup_connected: None,
                git_committer: None,
                git_identity: git::repository::GitCommitter {
                    name: None,
                    email: None,
                },
                load_error: None,
                refresh_task: None,
                publish_task: None,
                _subscriptions: subscriptions,
            };
            store.refresh(cx);
            store.refresh_accounts(cx);
            store
        });
        cx.set_global(GlobalProfileStore(store.clone()));
        store
    }

    fn active_profile(&self) -> AppProfile {
        self.profiles
            .iter()
            .find(|profile| profile.id == active_profile_id())
            .cloned()
            .unwrap_or_else(|| AppProfile {
                id: active_profile_id().to_string(),
                name: active_profile_id().to_string(),
                color: ProfileColor::Teal,
                folders: Vec::new(),
            })
    }

    /// The summary of this process's profile, read from its live state.
    fn current_summary(&self, cx: &App) -> ProfileSummary {
        let registry = LanguageModelRegistry::read_global(cx);
        let ai_providers = registry
            .providers()
            .iter()
            .filter(|provider| provider.is_authenticated(cx))
            .map(|provider| provider.name().0.to_string())
            .collect();
        let default_model = registry.default_model().map(|configured| {
            format!(
                "{} · {}",
                configured.provider.name().0,
                configured.model.name().0
            )
        });
        let agent_settings = AgentSettings::get_global(cx);
        let agent_profile = agent_settings
            .profiles
            .get(&agent_settings.default_profile)
            .map(|profile| profile.name.to_string());
        ProfileSummary {
            ai_providers,
            default_model,
            agent_profile,
            mcp_servers: mcp_server_summaries(cx),
            clickup_connected: cx
                .build_action(CLICKUP_OPEN_ACTION, None)
                .is_ok()
                .then_some(self.clickup_connected.unwrap_or(false)),
            git_committer: self.git_committer.clone(),
            git_name: self.git_identity.name.clone(),
            git_email: self.git_identity.email.clone(),
        }
    }

    /// Writes this profile's summary for the other profiles' managers, once changes settle.
    fn publish_summary(&mut self, cx: &mut Context<Self>) {
        self.publish_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let Ok(summary) = this.read_with(cx, |this, cx| this.current_summary(cx)) else {
                return;
            };
            let path = summaries_dir().join(format!("{}.json", active_profile_id()));
            cx.background_spawn(async move {
                std::fs::create_dir_all(summaries_dir())?;
                std::fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
                anyhow::Ok(())
            })
            .await
            .context("failed to publish the profile summary")
            .log_err();
        }));
    }

    /// Checks the accounts this process can see only asynchronously: whether ClickUp has a
    /// token and who git commits as.
    fn refresh_accounts(&mut self, cx: &mut Context<Self>) {
        let credentials = zed_credentials_provider::global(cx);
        cx.spawn(async move |this, cx| {
            let clickup_connected = credentials
                .read_credentials(CLICKUP_CREDENTIALS_URL, cx)
                .await
                .log_err()
                .map(|credentials| credentials.is_some());
            let committer = git::repository::get_git_committer(cx).await;
            let git_committer = match (&committer.name, &committer.email) {
                (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
                (Some(name), None) => Some(name.clone()),
                (None, Some(email)) => Some(email.clone()),
                (None, None) => None,
            };
            this.update(cx, |this, cx| {
                this.clickup_connected = clickup_connected;
                this.git_committer = git_committer;
                this.git_identity = committer;
                this.publish_summary(cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Sets who `profile_id` commits as. A profile other than the default one has a global git
    /// config of its own, so this leaves the other profiles untouched. That file holds no
    /// secret, so any profile's manager may edit it.
    fn set_git_identity(
        &mut self,
        profile_id: String,
        name: String,
        email: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (name, email) = (name.trim().to_string(), email.trim().to_string());
        cx.spawn(async move |this, cx| {
            let config_file = if profile_id == active_profile_id() {
                None
            } else if profile_id == paths::DEFAULT_PROFILE_ID {
                // This process may have pointed `--global` at its own profile's file.
                Some(paths::home_dir().join(".gitconfig"))
            } else {
                let file = paths::profiles_dir().join(&profile_id).join("gitconfig");
                if !file.exists() {
                    std::fs::write(&file, crate::profile_git_config_template(&profile_id))
                        .with_context(|| format!("failed to create {}", file.display()))?;
                }
                Some(file)
            };
            for (key, value) in [("user.name", &name), ("user.email", &email)] {
                let mut command = util::command::new_command("git");
                command.arg("config");
                match &config_file {
                    Some(file) => command.arg("--file").arg(file),
                    None => command.arg("--global"),
                };
                if value.is_empty() {
                    command.args(["--unset", key]);
                } else {
                    command.args([key, value.as_str()]);
                }
                let output = command.output().await.context("failed to run git")?;
                // `--unset` of a key that isn't set exits with 5, which is what we wanted anyway.
                let unset_missing = value.is_empty() && output.status.code() == Some(5);
                anyhow::ensure!(
                    output.status.success() || unset_missing,
                    "git config {key} falhou: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            this.update(cx, |this, cx| {
                if profile_id == active_profile_id() {
                    this.refresh_accounts(cx);
                    return;
                }
                // The other profile republishes its summary only when something changes there,
                // so the new identity is written into it here.
                let summary = this.summaries.entry(profile_id.clone()).or_default();
                summary.git_name = (!name.is_empty()).then(|| name.clone());
                summary.git_email = (!email.is_empty()).then(|| email.clone());
                summary.git_committer = match (&summary.git_name, &summary.git_email) {
                    (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
                    (Some(name), None) => Some(name.clone()),
                    (None, Some(email)) => Some(email.clone()),
                    (None, None) => None,
                };
                let summary = summary.clone();
                let path = summaries_dir().join(format!("{profile_id}.json"));
                cx.background_spawn(async move {
                    std::fs::create_dir_all(summaries_dir())?;
                    std::fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
                cx.notify();
            })
        })
    }

    /// Rereads the registry, which other profiles may have changed, and checks which
    /// profiles are running.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async move {
            let profiles = load_profiles();
            let running = profiles
                .as_ref()
                .map(|profiles| {
                    profiles
                        .iter()
                        .filter(|profile| profile.id != active_profile_id())
                        .filter(|profile| mac_only_instance::is_profile_running(&profile.id))
                        .map(|profile| profile.id.clone())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            let summaries = profiles
                .as_ref()
                .map(|profiles| load_summaries(profiles))
                .unwrap_or_default();
            (profiles, running, summaries)
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (profiles, running, summaries) = task.await;
            this.update(cx, |this, cx| {
                this.summaries = summaries;
                match profiles {
                    Ok(profiles) => {
                        this.profiles = profiles;
                        this.load_error = None;
                    }
                    Err(error) => {
                        log::error!("failed to load profiles: {error:#}");
                        this.load_error = Some(format!("{error:#}").into());
                    }
                }
                this.running = running;
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Makes `folders` open in `owner` from now on, taking them from any profile that had
    /// them, or releases them from every profile when `owner` is `None`.
    fn set_folders_owner(
        &mut self,
        folders: Vec<String>,
        owner: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                let mut profiles = load_profiles()?;
                for profile in &mut profiles {
                    profile.folders.retain(|folder| !folders.contains(folder));
                    if owner.as_deref() == Some(profile.id.as_str()) {
                        profile.folders.extend(folders.iter().cloned());
                    }
                }
                save_profiles(&profiles)
            })
            .await?;
            this.update(cx, |this, cx| this.refresh(cx))
        })
    }

    /// Changes the registry entry of `profile_id`.
    fn update_profile(
        &mut self,
        profile_id: String,
        change: impl FnOnce(&mut AppProfile) + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                let mut profiles = load_profiles()?;
                let profile = profiles
                    .iter_mut()
                    .find(|profile| profile.id == profile_id)
                    .context("o perfil não existe mais")?;
                change(profile);
                save_profiles(&profiles)
            })
            .await?;
            this.update(cx, |this, cx| this.refresh(cx))
        })
    }

    /// Deletes `profile_id`: its keychain entries, and its data and browser directories,
    /// which go to the Trash so a mistake can still be undone.
    fn delete(
        &mut self,
        profile_id: String,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |this, cx| {
            anyhow::ensure!(
                profile_id != paths::DEFAULT_PROFILE_ID,
                "O perfil padrão não pode ser excluído"
            );
            anyhow::ensure!(
                profile_id != active_profile_id(),
                "Não dá para excluir o perfil desta janela"
            );
            let data_dir = paths::profiles_dir().join(&profile_id);
            let credential_urls = cx
                .background_spawn({
                    let profile_id = profile_id.clone();
                    let data_dir = data_dir.clone();
                    async move {
                        anyhow::ensure!(
                            !mac_only_instance::is_profile_running(&profile_id),
                            "Feche o perfil antes de excluí-lo"
                        );
                        match std::fs::read(data_dir.join("credential_urls.json")) {
                            Ok(json) => serde_json::from_slice::<Vec<String>>(&json)
                                .context("failed to parse credential_urls.json"),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                Ok(Vec::new())
                            }
                            Err(error) => Err(error).context("failed to read credential_urls.json"),
                        }
                    }
                })
                .await?;

            // These URLs already carry the deleted profile's namespace, so they go straight to
            // the keychain rather than through this profile's credentials provider.
            for url in credential_urls {
                let deletion = cx.update(|cx| cx.delete_credentials(&url));
                if let Err(error) = deletion.await {
                    log::warn!("failed to delete the keychain entry {url}: {error:#}");
                }
            }

            let remove_options = RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            };
            fs.trash(&data_dir, remove_options)
                .await
                .with_context(|| format!("failed to move {} to the Trash", data_dir.display()))?;
            let browser_dir = web_preview::browser_cache_dir_for_profile(&profile_id);
            fs.trash(&browser_dir, remove_options).await.log_err();

            cx.background_spawn(async move {
                let mut profiles = load_profiles()?;
                profiles.retain(|profile| profile.id != profile_id);
                save_profiles(&profiles)
            })
            .await?;
            this.update(cx, |this, cx| this.refresh(cx))
        })
    }

    /// Registers a new profile, returning it once saved.
    fn create(
        &mut self,
        name: &str,
        color: ProfileColor,
        cx: &mut Context<Self>,
    ) -> Task<Result<AppProfile>> {
        let name = name.trim().to_string();
        cx.spawn(async move |this, cx| {
            anyhow::ensure!(!name.is_empty(), "Dê um nome ao perfil");
            let profile = cx
                .background_spawn(async move {
                    let mut profiles = load_profiles()?;
                    let profile = AppProfile {
                        id: profile_id_for_name(&name, &profiles),
                        name,
                        color,
                        folders: Vec::new(),
                    };
                    profiles.push(profile.clone());
                    save_profiles(&profiles)?;
                    anyhow::Ok(profile)
                })
                .await?;
            this.update(cx, |this, cx| this.refresh(cx))?;
            Ok(profile)
        })
    }
}

/// The local root folders of `workspace`, written with `~` for the home directory as the
/// registry stores them.
fn workspace_folders(workspace: &Workspace, cx: &App) -> Vec<String> {
    let home = paths::home_dir();
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| worktree.read(cx).is_local())
        .map(|worktree| {
            let path = worktree.read(cx).abs_path();
            match path.strip_prefix(home) {
                Ok(relative) => format!("~/{}", relative.display()),
                Err(_) => path.display().to_string(),
            }
        })
        .collect()
}

/// The chip at the start of the status bar naming the active profile.
pub struct ProfileIndicator {
    store: Entity<ProfileStore>,
    workspace: WeakEntity<Workspace>,
    menu_handle: PopoverMenuHandle<ProfilePicker>,
    _store_subscription: gpui::Subscription,
}

impl ProfileIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let store = ProfileStore::global(cx);
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            workspace: workspace.weak_handle(),
            menu_handle: PopoverMenuHandle::default(),
            _store_subscription: store_subscription,
        }
    }
}

impl Render for ProfileIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.store.read(cx).active_profile();
        let color = profile.color.hsla();
        let store = self.store.clone();
        let workspace = self.workspace.clone();

        PopoverMenu::new("profile-picker")
            .with_handle(self.menu_handle.clone())
            .anchor(Anchor::BottomLeft)
            .menu(move |window, cx| {
                let store = store.clone();
                let workspace = workspace.clone();
                Some(cx.new(|cx| ProfilePicker::new(store, workspace, window, cx)))
            })
            .trigger_with_tooltip(
                ButtonLike::new("profile-chip")
                    .tab_index(0isize)
                    .aria_label(format!("Perfil {}", profile.name))
                    .child(
                        h_flex()
                            .gap_1()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(color.opacity(0.35))
                            .bg(color.opacity(0.14))
                            .child(div().size_1p5().rounded_full().bg(color))
                            .child(
                                Label::new(profile.name)
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD)
                                    .color(Color::Custom(color)),
                            )
                            .child(
                                Icon::new(IconName::ChevronUpDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Custom(color)),
                            ),
                    ),
                Tooltip::text("Trocar de perfil"),
            )
    }
}

impl StatusItemView for ProfileIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // Which profile a window belongs to is the one thing that must never be hidden.
        None
    }
}

enum PickerMode {
    List,
    Create {
        name_editor: Entity<Editor>,
        color: ProfileColor,
        error: Option<SharedString>,
        creating: bool,
    },
}

/// The popover listing the profiles, with the form that creates a new one.
pub struct ProfilePicker {
    store: Entity<ProfileStore>,
    workspace: WeakEntity<Workspace>,
    mode: PickerMode,
    focus_handle: FocusHandle,
    _store_subscription: gpui::Subscription,
}

impl EventEmitter<DismissEvent> for ProfilePicker {}

impl Focusable for ProfilePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            PickerMode::List => self.focus_handle.clone(),
            PickerMode::Create { name_editor, .. } => name_editor.focus_handle(cx),
        }
    }
}

impl ProfilePicker {
    fn new(
        store: Entity<ProfileStore>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        store.update(cx, |store, cx| store.refresh(cx));
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            workspace,
            mode: PickerMode::List,
            focus_handle: cx.focus_handle(),
            _store_subscription: store_subscription,
        }
    }

    fn open(&mut self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        open_profile(profile_id, cx).detach_and_notify_err(self.workspace.clone(), window, cx);
        cx.emit(DismissEvent);
    }

    fn open_in_place(&mut self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        switch_in_place(profile_id, window, cx);
    }

    fn start_creating(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Nome do perfil", window, cx);
            editor
        });
        name_editor.focus_handle(cx).focus(window, cx);
        let taken_colors: Vec<ProfileColor> = self
            .store
            .read(cx)
            .profiles
            .iter()
            .map(|profile| profile.color)
            .collect();
        let color = ProfileColor::ALL
            .into_iter()
            .find(|color| !taken_colors.contains(color))
            .unwrap_or(ProfileColor::Teal);
        self.mode = PickerMode::Create {
            name_editor,
            color,
            error: None,
            creating: false,
        };
        cx.notify();
    }

    fn set_color(&mut self, new_color: ProfileColor, cx: &mut Context<Self>) {
        if let PickerMode::Create { color, .. } = &mut self.mode {
            *color = new_color;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let PickerMode::Create {
            name_editor,
            color,
            error,
            creating,
        } = &mut self.mode
        else {
            return;
        };
        if *creating {
            return;
        }
        *creating = true;
        *error = None;
        let name = name_editor.read(cx).text(cx);
        let color = *color;
        let task = self
            .store
            .update(cx, |store, cx| store.create(&name, color, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(profile) => this.open(profile.id, window, cx),
                Err(creation_error) => {
                    if let PickerMode::Create {
                        error, creating, ..
                    } = &mut this.mode
                    {
                        *creating = false;
                        *error = Some(format!("{creation_error:#}").into());
                    }
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            PickerMode::List => cx.emit(DismissEvent),
            PickerMode::Create { .. } => {
                self.mode = PickerMode::List;
                self.focus_handle.focus(window, cx);
                cx.notify();
            }
        }
    }

    fn render_mark(profile: &AppProfile, cx: &App) -> impl IntoElement {
        div()
            .flex_none()
            .size_6()
            .rounded_md()
            .flex()
            .items_center()
            .justify_center()
            .bg(profile.color.hsla())
            .child(
                Label::new(profile.initial())
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::BOLD)
                    .color(Color::Custom(cx.theme().colors().editor_background)),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let active_id = active_profile_id();
        let rows = store.profiles.iter().enumerate().map(|(index, profile)| {
            let is_active = profile.id == active_id;
            let status = if is_active {
                "esta janela"
            } else if store.running.contains(&profile.id) {
                "aberto"
            } else {
                "fechado"
            };
            let open_id = profile.id.clone();
            let in_place_id = profile.id.clone();
            h_flex()
                .id(("profile", index))
                .gap_2()
                .px_1p5()
                .py_1()
                .rounded_md()
                .when(is_active, |this| {
                    this.bg(cx.theme().colors().element_selected)
                })
                .when(!is_active, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(open_id.clone(), window, cx)
                        }))
                })
                .child(Self::render_mark(profile, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(profile.name.clone()).truncate())
                        .child(
                            Label::new(status)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .map(|this| {
                    if is_active {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                    } else {
                        this.child(
                            IconButton::new(("open-in-place", index), IconName::ThisWindow)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Abrir nesta janela"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_in_place(in_place_id.clone(), window, cx)
                                })),
                        )
                    }
                })
        });

        v_flex()
            .gap_0p5()
            .child(
                Label::new("PERFIS")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted)
                    .mx_1p5()
                    .my_1(),
            )
            .children(rows)
            .when_some(store.load_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error)
                        .mx_1p5(),
                )
            })
            .child(Divider::horizontal().my_1())
            .children(self.render_folder_binding(cx))
            .child(
                h_flex()
                    .id("new-profile")
                    .gap_2()
                    .px_1p5()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.start_creating(window, cx)))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Novo perfil…")),
            )
            .child(
                h_flex()
                    .id("manage-profiles")
                    .gap_2()
                    .px_1p5()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_manager(window, cx)))
                    .child(
                        Icon::new(IconName::Settings)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Gerenciar perfis…")),
            )
    }

    fn open_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| open_manager(workspace, window, cx));
    }

    fn toggle_folder_binding(&mut self, bind: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let folders = workspace_folders(workspace.read(cx), cx);
        let owner = bind.then(|| active_profile_id().to_string());
        self.store
            .update(cx, |store, cx| store.set_folders_owner(folders, owner, cx))
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    /// The row that ties the open project to the active profile, so that opening it from
    /// the CLI or Finder always lands here.
    fn render_folder_binding(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let workspace = self.workspace.upgrade()?;
        let folders = workspace_folders(workspace.read(cx), cx);
        let project_name = folders
            .first()
            .and_then(|folder| Path::new(folder).file_name())
            .map(|name| name.to_string_lossy().into_owned())?;
        let store = self.store.read(cx);
        let active = store.active_profile();
        let is_bound = folders.iter().all(|folder| active.folders.contains(folder));
        let other_owner = store
            .profiles
            .iter()
            .find(|profile| {
                profile.id != active.id
                    && folders
                        .iter()
                        .any(|folder| profile.folders.contains(folder))
            })
            .map(|profile| profile.name.clone());

        let (icon, label, detail) = if is_bound {
            (
                IconName::Check,
                format!("{project_name} abre sempre aqui"),
                "Clique para desvincular".to_string(),
            )
        } else {
            (
                IconName::Folder,
                format!("Sempre abrir {project_name} neste perfil"),
                match other_owner {
                    Some(owner) => format!("Hoje pertence a {owner}"),
                    None => "Pelo terminal ou Finder, abre aqui".to_string(),
                },
            )
        };

        Some(
            h_flex()
                .id("bind-project")
                .gap_2()
                .px_1p5()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.toggle_folder_binding(!is_bound, window, cx)
                }))
                .child(Icon::new(icon).size(IconSize::Small).color(if is_bound {
                    Color::Accent
                } else {
                    Color::Muted
                }))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(Label::new(label).truncate())
                        .child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                ),
        )
    }

    fn render_create(
        &self,
        name_editor: &Entity<Editor>,
        selected_color: ProfileColor,
        error: Option<SharedString>,
        creating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let swatches = ProfileColor::ALL.into_iter().map(|color| {
            let is_selected = color == selected_color;
            div()
                .id(color.label())
                .size_5()
                .rounded_full()
                .cursor_pointer()
                .border_2()
                .border_color(if is_selected {
                    cx.theme().colors().text
                } else {
                    gpui::transparent_black()
                })
                .bg(color.hsla())
                .tooltip(Tooltip::text(color.label()))
                .on_click(cx.listener(move |this, _, _, cx| this.set_color(color, cx)))
        });

        v_flex()
            .gap_2()
            .p_1p5()
            .child(
                Label::new("NOVO PERFIL")
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_focused)
                    .bg(cx.theme().colors().editor_background)
                    .child(name_editor.clone()),
            )
            .child(h_flex().gap_1p5().children(swatches))
            .child(
                Label::new(
                    "Começa com settings, extensões e contas vazios. Keymap e temas são compartilhados.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when_some(error, |this, error| {
                this.child(Label::new(error).size(LabelSize::XSmall).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("cancel-profile", "Cancelar")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            })),
                    )
                    .child(
                        Button::new("create-profile", "Criar e abrir")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Filled)
                            .disabled(creating)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

impl Render for ProfilePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.mode {
            PickerMode::List => self.render_list(cx).into_any_element(),
            PickerMode::Create {
                name_editor,
                color,
                error,
                creating,
            } => self
                .render_create(&name_editor.clone(), *color, error.clone(), *creating, cx)
                .into_any_element(),
        };

        v_flex()
            .key_context("ProfilePicker")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(18.))
            .p_1()
            .elevation_2(cx)
            .child(content)
    }
}

/// Lists every profile and edits the one selected: its name, color and folders, or deletes it.
pub struct ManageProfilesModal {
    store: Entity<ProfileStore>,
    workspace: WeakEntity<Workspace>,
    selected_id: String,
    name_editor: Entity<Editor>,
    /// The editors for who git commits as, while that identity is being edited.
    git_identity_editors: Option<(Entity<Editor>, Entity<Editor>)>,
    error: Option<SharedString>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<DismissEvent> for ManageProfilesModal {}
impl ModalView for ManageProfilesModal {}

impl Focusable for ManageProfilesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ManageProfilesModal {
    fn new(
        store: Entity<ProfileStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        store.update(cx, |store, cx| {
            store.refresh(cx);
            store.refresh_accounts(cx);
        });
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Nome do perfil", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&name_editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Blurred) {
                    this.save_name(cx);
                }
            }),
        ];
        let mut this = Self {
            store,
            workspace,
            selected_id: String::new(),
            name_editor,
            git_identity_editors: None,
            error: None,
            _subscriptions: subscriptions,
        };
        this.select(active_profile_id().to_string(), window, cx);
        this
    }

    fn selected(&self, cx: &App) -> Option<AppProfile> {
        self.store
            .read(cx)
            .profiles
            .iter()
            .find(|profile| profile.id == self.selected_id)
            .cloned()
    }

    fn select(&mut self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_id.is_empty() {
            self.save_name(cx);
        }
        let name = self
            .store
            .read(cx)
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        self.selected_id = profile_id;
        self.git_identity_editors = None;
        self.error = None;
        self.name_editor
            .update(cx, |editor, cx| editor.set_text(name, window, cx));
        cx.notify();
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.selected(cx) else {
            return;
        };
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() || name == profile.name {
            return;
        }
        let task = self.store.update(cx, |store, cx| {
            store.update_profile(profile.id, move |profile| profile.name = name, cx)
        });
        self.report(task, cx);
    }

    fn set_color(&mut self, color: ProfileColor, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |store, cx| {
            store.update_profile(
                self.selected_id.clone(),
                move |profile| profile.color = color,
                cx,
            )
        });
        self.report(task, cx);
    }

    fn remove_folder(&mut self, folder: String, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |store, cx| {
            store.update_profile(
                self.selected_id.clone(),
                move |profile| profile.folders.retain(|entry| *entry != folder),
                cx,
            )
        });
        self.report(task, cx);
    }

    fn add_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Vincular ao perfil".into()),
        });
        let profile_id = self.selected_id.clone();
        cx.spawn(async move |this, cx| {
            let Some(paths) = paths.await.ok().and_then(|paths| paths.log_err()).flatten() else {
                return anyhow::Ok(());
            };
            let home = paths::home_dir();
            let folders: Vec<String> = paths
                .iter()
                .map(|path| match path.strip_prefix(home) {
                    Ok(relative) => format!("~/{}", relative.display()),
                    Err(_) => path.display().to_string(),
                })
                .collect();
            this.update(cx, |this, cx| {
                let task = this.store.update(cx, |store, cx| {
                    store.set_folders_owner(folders, Some(profile_id), cx)
                });
                this.report(task, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn create_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let taken_colors: Vec<ProfileColor> = self
            .store
            .read(cx)
            .profiles
            .iter()
            .map(|profile| profile.color)
            .collect();
        let color = ProfileColor::ALL
            .into_iter()
            .find(|color| !taken_colors.contains(color))
            .unwrap_or(ProfileColor::Teal);
        let task = self
            .store
            .update(cx, |store, cx| store.create("Novo perfil", color, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(profile) => {
                    this.select(profile.id, window, cx);
                    this.name_editor.update(cx, |editor, cx| {
                        editor.select_all(&editor::actions::SelectAll, window, cx)
                    });
                    this.name_editor.focus_handle(cx).focus(window, cx);
                }
                Err(error) => {
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.selected(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).app_state().fs.clone();
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Excluir o perfil {}?", profile.name),
            Some(
                "As contas e tokens dele saem do Keychain, e a pasta de dados e o cache do navegador vão para a Lixeira.",
            ),
            &["Excluir", "Cancelar"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            let task = this.update(cx, |this, cx| {
                this.store
                    .update(cx, |store, cx| store.delete(profile.id, fs, cx))
            })?;
            let result = task.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => this.select(active_profile_id().to_string(), window, cx),
                Err(error) => {
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn report(&mut self, task: Task<Result<()>>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.error = result.err().map(|error| format!("{error:#}").into());
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.save_name(cx);
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.save_name(cx);
    }

    fn render_section_title(title: &'static str) -> impl IntoElement {
        Label::new(title)
            .size(LabelSize::XSmall)
            .weight(gpui::FontWeight::SEMIBOLD)
            .color(Color::Muted)
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let rows = store.profiles.iter().enumerate().map(|(index, profile)| {
            let is_selected = profile.id == self.selected_id;
            let status = if profile.id == active_profile_id() {
                "esta janela"
            } else if store.running.contains(&profile.id) {
                "aberto"
            } else {
                "fechado"
            };
            let profile_id = profile.id.clone();
            h_flex()
                .id(("manage-profile", index))
                .gap_2()
                .px_1p5()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(is_selected, |this| {
                    this.bg(cx.theme().colors().element_selected)
                })
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select(profile_id.clone(), window, cx)
                }))
                .child(ProfilePicker::render_mark(profile, cx))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(Label::new(profile.name.clone()).truncate())
                        .child(
                            Label::new(status)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
        });

        v_flex()
            .flex_none()
            .w(rems(13.))
            .h_full()
            .p_2()
            .gap_0p5()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(
                div()
                    .px_1p5()
                    .py_1()
                    .child(Self::render_section_title("PERFIS")),
            )
            .children(rows)
            .child(
                h_flex()
                    .id("manage-new-profile")
                    .gap_2()
                    .px_1p5()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.create_profile(window, cx)))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Novo perfil").color(Color::Muted)),
            )
            .child(div().flex_1())
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().element_background)
                    .child(Self::render_section_title("COMPARTILHADO"))
                    .child(
                        Label::new(
                            "Keymap, temas e global_settings.json valem para todos os perfis.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
    }

    fn render_card(
        title: &'static str,
        action: Option<AnyElement>,
        rows: Vec<AnyElement>,
        empty: Option<&'static str>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().element_background.opacity(0.4))
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_1p5()
                    .child(Self::render_section_title(title))
                    .children(action),
            )
            .when(rows.is_empty(), |this| {
                this.when_some(empty, |this, empty| {
                    this.child(
                        div()
                            .px_3()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(Label::new(empty).size(LabelSize::Small).color(Color::Muted)),
                    )
                })
            })
            .children(rows.into_iter().map(|row| {
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(row)
            }))
    }

    fn render_row(
        icon: IconName,
        icon_color: Color,
        title: impl Into<SharedString>,
        detail: Option<SharedString>,
        trailing: Option<AnyElement>,
        mono: bool,
        cx: &App,
    ) -> AnyElement {
        h_flex()
            .gap_2p5()
            .px_3()
            .py_1p5()
            .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .when(mono, |label| label.buffer_font(cx))
                            .truncate(),
                    )
                    .when_some(detail, |this, detail| {
                        this.child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .children(trailing)
            .into_any_element()
    }

    fn render_status(label: &'static str, ok: bool) -> AnyElement {
        let color = if ok { Color::Success } else { Color::Muted };
        h_flex()
            .gap_1()
            .child(Indicator::dot().color(color))
            .child(Label::new(label).size(LabelSize::XSmall).color(color))
            .into_any_element()
    }

    /// A click handler that runs `action` in `profile_id`: here when it is this window's
    /// profile, or in that profile's own process otherwise.
    fn profile_action(
        &self,
        profile_id: &str,
        action: &'static str,
        data: Option<serde_json::Value>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let profile_id = profile_id.to_string();
        let workspace = self.workspace.clone();
        move |_, window, cx| {
            if profile_id == active_profile_id() {
                match cx.build_action(action, data.clone()) {
                    Ok(action) => window.dispatch_action(action, cx),
                    Err(error) => log::error!("failed to build {action}: {error:?}"),
                }
            } else {
                run_in_profile(profile_id.clone(), action, data.clone(), cx).detach_and_notify_err(
                    workspace.clone(),
                    window,
                    cx,
                );
            }
        }
    }

    fn start_editing_git_identity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let (name, email) = if self.selected_id == active_profile_id() {
            (
                store.git_identity.name.clone().unwrap_or_default(),
                store.git_identity.email.clone().unwrap_or_default(),
            )
        } else {
            let summary = store.summaries.get(&self.selected_id);
            (
                summary
                    .and_then(|summary| summary.git_name.clone())
                    .unwrap_or_default(),
                summary
                    .and_then(|summary| summary.git_email.clone())
                    .unwrap_or_default(),
            )
        };
        let new_editor =
            |text: String, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                })
            };
        let name_editor = new_editor(name, "Nome", window, cx);
        let email_editor = new_editor(email, "E-mail", window, cx);
        name_editor.focus_handle(cx).focus(window, cx);
        self.git_identity_editors = Some((name_editor, email_editor));
        cx.notify();
    }

    fn save_git_identity(&mut self, cx: &mut Context<Self>) {
        let Some((name_editor, email_editor)) = self.git_identity_editors.take() else {
            return;
        };
        let name = name_editor.read(cx).text(cx);
        let email = email_editor.read(cx).text(cx);
        let profile_id = self.selected_id.clone();
        let task = self.store.update(cx, |store, cx| {
            store.set_git_identity(profile_id, name, email, cx)
        });
        self.report(task, cx);
        cx.notify();
    }

    fn cancel_git_identity(&mut self, cx: &mut Context<Self>) {
        self.git_identity_editors = None;
        cx.notify();
    }

    fn toggle_mcp_server(&self, server: String, enabled: bool, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            let store = workspace.read(cx).project().read(cx).context_server_store();
            store.update(cx, |store, cx| {
                let Some(id) = store
                    .server_ids()
                    .iter()
                    .find(|id| id.0.as_ref() == server.as_str())
                    .cloned()
                else {
                    return;
                };
                if enabled {
                    if let Some(server) = store.get_server(&id) {
                        store.start_server(server, cx);
                    }
                } else {
                    store.stop_server(&id, cx).log_err();
                }
            });
        }
        let fs = <dyn Fs>::global(cx);
        settings::update_settings_file(fs, cx, move |settings, _| {
            if let Some(server) = settings.project.context_servers.get_mut(server.as_str()) {
                server.set_enabled(enabled);
            }
        });
    }

    /// Whether the MCP server `id` of this window's project is running, as a short label.
    fn mcp_status(&self, id: &str, cx: &App) -> Option<(&'static str, bool)> {
        let workspace = self.workspace.upgrade()?;
        let store = workspace.read(cx).project().read(cx).context_server_store();
        let store = store.read(cx);
        let server_id = store
            .server_ids()
            .iter()
            .find(|server| server.0.as_ref() == id)?;
        Some(match store.status_for_server(server_id)? {
            ContextServerStatus::Running => ("rodando", true),
            ContextServerStatus::Starting | ContextServerStatus::Authenticating => {
                ("iniciando", false)
            }
            ContextServerStatus::Stopped => ("parado", false),
            ContextServerStatus::AuthRequired
            | ContextServerStatus::ClientSecretRequired { .. } => ("pede login", false),
            ContextServerStatus::Error(_) => ("erro", false),
        })
    }

    fn render_git_card(
        &self,
        profile: &AppProfile,
        summary: &ProfileSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_default = profile.id == paths::DEFAULT_PROFILE_ID;
        let source = if is_default {
            "~/.gitconfig, que os outros perfis herdam"
        } else {
            "gitconfig deste perfil, herda o ~/.gitconfig"
        };
        let committer = match &self.git_identity_editors {
            Some((name_editor, email_editor)) => {
                let field = |editor: &Entity<Editor>, cx: &App| {
                    div()
                        .flex_1()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(editor.clone())
                };
                v_flex()
                    .gap_1p5()
                    .px_3()
                    .py_2()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(field(name_editor, cx))
                            .child(field(email_editor, cx)),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                Label::new(format!("Grava em {source}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("cancel-git-identity", "Cancelar")
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.cancel_git_identity(cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("save-git-identity", "Salvar")
                                            .label_size(LabelSize::Small)
                                            .style(ButtonStyle::Filled)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_git_identity(cx)
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            _ => Self::render_row(
                IconName::Person,
                Color::Default,
                match &summary.git_committer {
                    Some(committer) => format!("Commits como {committer}"),
                    None => "Sem user.name no git".to_string(),
                },
                Some(source.into()),
                Some(
                    Button::new("edit-git-identity", "Editar")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_editing_git_identity(window, cx)
                        }))
                        .into_any_element(),
                ),
                false,
                cx,
            ),
        };
        let providers = Self::render_row(
            IconName::GitBranch,
            Color::Muted,
            "GitHub e Bitbucket",
            Some("Contas por perfil chegam com o dock Repo".into()),
            Some(
                Label::new("em breve")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            false,
            cx,
        );
        Self::render_card("CONTAS GIT", None, vec![committer, providers], None, cx)
    }

    fn render_ai_card(
        &self,
        summary: &ProfileSummary,
        is_active: bool,
        cx: &App,
    ) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = summary
            .ai_providers
            .iter()
            .map(|provider| {
                Self::render_row(
                    IconName::AiZed,
                    Color::Default,
                    provider.clone(),
                    None,
                    Some(Self::render_status("conectado", true)),
                    false,
                    cx,
                )
            })
            .collect();
        if rows.is_empty() {
            rows.push(Self::render_row(
                IconName::AiZed,
                Color::Muted,
                "Nenhum provedor conectado",
                None,
                None,
                false,
                cx,
            ));
        }
        let text = |value: &Option<String>| {
            Label::new(value.clone().unwrap_or_else(|| "—".to_string()))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        rows.push(Self::render_row(
            IconName::Sparkle,
            Color::Default,
            "Modelo padrão",
            None,
            Some(text(&summary.default_model)),
            false,
            cx,
        ));
        rows.push(Self::render_row(
            IconName::Sliders,
            Color::Default,
            "Perfil do agente",
            None,
            Some(text(&summary.agent_profile)),
            false,
            cx,
        ));
        let action = Some(
            Button::new(
                "configure-ai",
                if is_active {
                    "Configurar"
                } else {
                    "Configurar lá"
                },
            )
            .label_size(LabelSize::Small)
            .on_click(self.profile_action(
                &self.selected_id,
                "zed::OpenSettingsPage",
                Some(serde_json::json!({ "page": "AI" })),
            ))
            .into_any_element(),
        );
        Self::render_card("IA", action, rows, None, cx)
    }

    fn render_mcp_card(
        &self,
        summary: &ProfileSummary,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = summary
            .mcp_servers
            .iter()
            .enumerate()
            .map(|(index, server)| {
                let trailing = if is_active {
                    let status = self
                        .mcp_status(&server.id, cx)
                        .filter(|_| server.enabled)
                        .map(|(label, ok)| Self::render_status(label, ok));
                    let toggled_server = server.id.clone();
                    h_flex()
                        .gap_2()
                        .children(status)
                        .child(
                            Switch::new(
                                ("mcp-toggle", index),
                                if server.enabled {
                                    ToggleState::Selected
                                } else {
                                    ToggleState::Unselected
                                },
                            )
                            .on_click(cx.listener(
                                move |this, state: &ToggleState, _, cx| {
                                    this.toggle_mcp_server(
                                        toggled_server.clone(),
                                        *state == ToggleState::Selected,
                                        cx,
                                    )
                                },
                            )),
                        )
                        .into_any_element()
                } else {
                    Self::render_status(
                        if server.enabled {
                            "ligado"
                        } else {
                            "desligado"
                        },
                        server.enabled,
                    )
                };
                Self::render_row(
                    IconName::Server,
                    if server.enabled {
                        Color::Default
                    } else {
                        Color::Muted
                    },
                    server.id.clone(),
                    Some(server.detail.clone().into()),
                    Some(trailing),
                    true,
                    cx,
                )
            })
            .collect();
        let action = Some(
            Button::new(
                "add-mcp",
                if is_active {
                    "Adicionar"
                } else {
                    "Adicionar lá"
                },
            )
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
            .on_click(self.profile_action(
                &self.selected_id,
                "zed::OpenSettingsPage",
                Some(serde_json::json!({ "page": "MCP Servers" })),
            ))
            .into_any_element(),
        );
        Self::render_card(
            "SERVIDORES MCP",
            action,
            rows,
            Some("Nenhum servidor MCP configurado."),
            cx,
        )
    }

    fn render_integrations_card(
        &self,
        summary: &ProfileSummary,
        is_active: bool,
        cx: &App,
    ) -> Option<impl IntoElement> {
        let connected = summary.clickup_connected?;
        let (label, action) = if connected {
            ("Desconectar", CLICKUP_DISCONNECT_ACTION)
        } else {
            ("Conectar", CLICKUP_OPEN_ACTION)
        };
        let trailing = h_flex()
            .gap_2()
            .when(!is_active, |this| {
                this.child(Self::render_status(
                    if connected {
                        "conectado"
                    } else {
                        "desconectado"
                    },
                    connected,
                ))
            })
            .child(
                Button::new("clickup-connection", label)
                    .label_size(LabelSize::Small)
                    .on_click(self.profile_action(&self.selected_id, action, None)),
            )
            .into_any_element();
        let row = Self::render_row(
            IconName::ListTodo,
            Color::Default,
            "ClickUp",
            Some("token pessoal no Keychain deste perfil".into()),
            Some(trailing),
            false,
            cx,
        );
        Some(Self::render_card("INTEGRAÇÕES", None, vec![row], None, cx))
    }

    fn render_folders_card(
        &self,
        profile: &AppProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = profile
            .folders
            .iter()
            .enumerate()
            .map(|(index, folder)| {
                let removed = folder.clone();
                Self::render_row(
                    IconName::Folder,
                    Color::Custom(profile.color.hsla()),
                    folder.clone(),
                    None,
                    Some(
                        IconButton::new(("remove-folder", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Desvincular pasta"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_folder(removed.clone(), cx)
                            }))
                            .into_any_element(),
                    ),
                    true,
                    cx,
                )
            })
            .collect();
        let action = Button::new("add-folder", "Pasta")
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
            .on_click(cx.listener(|this, _, _, cx| this.add_folder(cx)))
            .into_any_element();
        Self::render_card(
            "PASTAS DO PERFIL",
            Some(action),
            rows,
            Some("Pastas vinculadas abrem sempre neste perfil."),
            cx,
        )
    }

    fn render_data_card(
        &self,
        profile: &AppProfile,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings_row = Self::render_row(
            IconName::FileCode,
            Color::Default,
            "settings.json do perfil",
            Some("sobrepõe o global_settings.json compartilhado".into()),
            Some(
                Button::new(
                    "open-settings",
                    if is_active { "Abrir" } else { "Abrir lá" },
                )
                .label_size(LabelSize::Small)
                .on_click(self.profile_action(&profile.id, "zed::OpenSettingsFile", None))
                .into_any_element(),
            ),
            false,
            cx,
        );
        let data_row = Self::render_row(
            IconName::Thread,
            Color::Default,
            "Threads, histórico e workspaces",
            Some("banco separado, nada vaza para os outros perfis".into()),
            Some(
                Label::new("isolado")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            false,
            cx,
        );
        let mut rows = vec![settings_row, data_row];
        if !is_active {
            let open_id = profile.id.clone();
            rows.push(Self::render_row(
                IconName::ThisWindow,
                Color::Default,
                "Editar contas, IA e MCPs",
                Some("Só o próprio perfil mexe no Keychain e nos settings dele".into()),
                Some(
                    Button::new("open-profile", "Abrir perfil")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            open_profile(open_id.clone(), cx).detach_and_notify_err(
                                this.workspace.clone(),
                                window,
                                cx,
                            );
                        }))
                        .into_any_element(),
                ),
                false,
                cx,
            ));
        }
        Self::render_card("DADOS DO PERFIL", None, rows, None, cx)
    }

    fn render_details(&self, profile: &AppProfile, cx: &mut Context<Self>) -> impl IntoElement {
        let is_default = profile.id == paths::DEFAULT_PROFILE_ID;
        let is_active = profile.id == active_profile_id();
        let (is_running, summary) = {
            let store = self.store.read(cx);
            let summary = if is_active {
                Some(store.current_summary(cx))
            } else {
                store.summaries.get(&profile.id).cloned()
            };
            (store.running.contains(&profile.id), summary)
        };
        let data_dir = if is_default {
            paths::data_dir().display().to_string()
        } else {
            paths::profiles_dir()
                .join(&profile.id)
                .display()
                .to_string()
        };

        let swatches = ProfileColor::ALL
            .into_iter()
            .map(|color| {
                let is_selected = color == profile.color;
                div()
                    .id(color.label())
                    .size_4()
                    .rounded_full()
                    .cursor_pointer()
                    .border_2()
                    .border_color(if is_selected {
                        cx.theme().colors().text
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(color.hsla())
                    .tooltip(Tooltip::text(color.label()))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_color(color, cx)))
            })
            .collect::<Vec<_>>();

        let delete_hint = if is_default {
            Some("O perfil padrão é a instalação original e não pode ser excluído.")
        } else if is_active {
            Some("Troque para outro perfil para excluir este.")
        } else if is_running {
            Some("Feche o perfil para excluí-lo.")
        } else {
            None
        };

        let (left, right) = match &summary {
            Some(summary) => (
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(self.render_git_card(profile, summary, cx))
                    .child(self.render_ai_card(summary, is_active, cx))
                    .child(self.render_data_card(profile, is_active, cx))
                    .into_any_element(),
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(self.render_mcp_card(summary, is_active, cx))
                    .children(self.render_integrations_card(summary, is_active, cx))
                    .child(self.render_folders_card(profile, cx))
                    .into_any_element(),
            ),
            None => (
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(Self::render_card(
                        "CONTAS, IA E MCP",
                        None,
                        Vec::new(),
                        Some("Abra este perfil uma vez para ver as contas, a IA e os MCPs dele aqui."),
                        cx,
                    ))
                    .child(self.render_data_card(profile, is_active, cx))
                    .into_any_element(),
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(self.render_folders_card(profile, cx))
                    .into_any_element(),
            ),
        };

        v_flex()
            .id("profile-details")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .size_9()
                            .rounded_lg()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(profile.color.hsla())
                            .child(
                                Label::new(profile.initial())
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(Color::Custom(cx.theme().colors().editor_background)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .bg(cx.theme().colors().editor_background)
                                    .child(self.name_editor.clone()),
                            )
                            .child(
                                Label::new(data_dir)
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(h_flex().gap_1p5().children(swatches)),
            )
            .child(h_flex().items_start().gap_3().child(left).child(right))
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(match (&self.error, delete_hint) {
                        (Some(error), _) => Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                        (None, Some(hint)) => {
                            Label::new(hint).size(LabelSize::Small).color(Color::Muted)
                        }
                        (None, None) => Label::new("").size(LabelSize::Small),
                    })
                    .child(
                        Button::new("delete-profile", "Excluir perfil")
                            .label_size(LabelSize::Small)
                            .color(Color::Error)
                            .disabled(delete_hint.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.delete_selected(window, cx)),
                            ),
                    ),
            )
    }
}

impl Render for ManageProfilesModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = self
            .selected(cx)
            .map(|profile| self.render_details(&profile, cx).into_any_element());

        h_flex()
            .key_context("ManageProfilesModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(64.))
            .h(rems(42.))
            .overflow_hidden()
            .items_start()
            .child(self.render_nav(cx))
            .children(details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str) -> AppProfile {
        AppProfile {
            id: id.to_string(),
            name: id.to_string(),
            color: ProfileColor::Teal,
            folders: Vec::new(),
        }
    }

    #[test]
    fn test_owner_of() {
        let mut trabalho = profile("trabalho");
        trabalho.folders = vec!["/Users/me/Projects/trix".to_string()];
        let mut app = profile("app");
        app.folders = vec!["/Users/me/Projects/trix/app".to_string()];
        let mut pessoal = profile("pessoal");
        pessoal.folders = vec!["/Users/me/Personal/**".to_string()];
        let profiles = [trabalho, app, pessoal];

        let owner = |path: &str| owner_of(Path::new(path), &profiles).map(|p| p.id.as_str());
        assert_eq!(
            owner("/Users/me/Projects/trix/api/main.rs"),
            Some("trabalho")
        );
        assert_eq!(owner("/Users/me/Projects/trix/app/index.ts"), Some("app"));
        assert_eq!(owner("/Users/me/Projects/trixie"), None);
        assert_eq!(owner("/Users/me/Personal/blog/post.md"), Some("pessoal"));
        assert_eq!(owner("/tmp/scratch"), None);
    }

    #[test]
    fn test_profile_id_for_name() {
        assert_eq!(profile_id_for_name("Pessoal", &[]), "pessoal");
        assert_eq!(
            profile_id_for_name("  Família Silva ", &[]),
            "familia-silva"
        );
        assert_eq!(profile_id_for_name("Side / Projeto!", &[]), "side-projeto");
        assert_eq!(profile_id_for_name("!!!", &[]), "perfil");
        assert_eq!(profile_id_for_name("Default", &[]), "perfil");
        assert_eq!(
            profile_id_for_name("Pessoal", &[profile("pessoal"), profile("pessoal-2")]),
            "pessoal-3"
        );
        assert!(paths::is_valid_profile_id(&profile_id_for_name(
            &"Nome muito comprido ".repeat(10),
            &[]
        )));
    }
}
