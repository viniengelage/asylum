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
use i18n::t;
use language_model::LanguageModelRegistry;
use project::context_server_store::ContextServerStatus;
use project::project_settings::{ContextServerSettings, ProjectSettings};
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsFile, SettingsStore};
use ui::{
    ButtonLike, Divider, IconButton, IconName, IconSize, Indicator, Label, LabelSize, PopoverMenu,
    PopoverMenuHandle, Switch, TintColor, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use util::paths::{PathMatcher, PathStyle, PathWithPosition};
use workspace::{
    HideStatusItem, ItemHandle, ModalView, StatusItemView, Workspace,
    notifications::NotifyTaskExt as _,
};

use crate::zed::{mac_only_instance, profile_launchers};

/// The name the default profile is saved with until it is renamed. It shows translated, but
/// the registry keeps this value so that it doesn't depend on the language.
const DEFAULT_PROFILE_NAME: &str = "Padrão";
/// The color of the icon in the app's own bundle.
const BUNDLE_ICON_COLOR: Option<ProfileColor> = Some(ProfileColor::Purple);

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

    /// The app icon of profiles in this color, written into their launchers.
    fn launcher_icon(self) -> &'static [u8] {
        match self {
            Self::Amber => include_bytes!("../../resources/profile-icons/amber.icns"),
            Self::Teal => include_bytes!("../../resources/profile-icons/teal.icns"),
            Self::Purple => include_bytes!("../../resources/profile-icons/purple.icns"),
            Self::Blue => include_bytes!("../../resources/profile-icons/blue.icns"),
            Self::Rose => include_bytes!("../../resources/profile-icons/rose.icns"),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Amber => "amber",
            Self::Teal => "teal",
            Self::Purple => "purple",
            Self::Blue => "blue",
            Self::Rose => "rose",
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Amber => t!("zed.profiles.color_amber"),
            Self::Teal => t!("zed.profiles.color_teal"),
            Self::Purple => t!("zed.profiles.color_purple"),
            Self::Blue => t!("zed.profiles.color_blue"),
            Self::Rose => t!("zed.profiles.color_rose"),
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
    /// The entries of `folders` that open by themselves when this profile starts, so the
    /// user lands in their project instead of an empty window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_on_launch: Vec<String>,
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
        // Matches the app's own icon, so the default profile doesn't recolor it.
        color: ProfileColor::Purple,
        folders: Vec::new(),
        open_on_launch: Vec::new(),
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
    let mut default = match default_position {
        Some(position) => profiles.remove(position),
        None => default_profile(),
    };
    if default.name == DEFAULT_PROFILE_NAME {
        default.name = t!("zed.profiles.default_name").to_string();
    }
    profiles.insert(0, default);
    Ok(profiles)
}

fn save_profiles(profiles: &[AppProfile]) -> Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create the profiles directory")?;
    }
    let default_name = t!("zed.profiles.default_name");
    let profiles = profiles
        .iter()
        .cloned()
        .map(|mut profile| {
            if profile.id == paths::DEFAULT_PROFILE_ID && profile.name == default_name.as_ref() {
                profile.name = DEFAULT_PROFILE_NAME.to_string();
            }
            profile
        })
        .collect();
    let json = serde_json::to_vec_pretty(&Registry { profiles })?;
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

fn is_glob(entry: &str) -> bool {
    entry.contains(['*', '?', '[', '{'])
}

/// The folders the running profile opens when the app starts, resolved to directories that
/// still exist. A folder that was moved or deleted is skipped rather than failing the launch.
pub fn launch_folders() -> Vec<PathBuf> {
    let Some(profiles) = load_profiles().log_err() else {
        return Vec::new();
    };
    profiles
        .into_iter()
        .find(|profile| profile.id == active_profile_id())
        .map(|profile| {
            profile
                .open_on_launch
                .iter()
                .filter(|entry| !is_glob(entry))
                .map(|entry| expand_home(entry.trim()))
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// How specifically `entry` claims `path`: the length of the entry when it does, so that
/// `~/Projects/trix/app` wins over `~/Projects/trix` for a file inside the app.
fn folder_match_len(entry: &str, path: &Path) -> Option<usize> {
    let expanded = expand_home(entry.trim());
    let matches = if is_glob(entry) {
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

/// Profiles are managed on the Profiles page of the settings window.
fn open_manager(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    window.dispatch_action(
        Box::new(zed_actions::OpenSettingsPage {
            page: "Profiles".to_string(),
            target: None,
        }),
        cx,
    );
}

/// Registers the profile actions and lets File > Open offer to open a folder in the profile
/// it belongs to.
pub fn init(cx: &mut App) {
    settings_ui::register_profiles_page(
        std::rc::Rc::new(|original_window, window, cx| {
            let store = ProfileStore::global(cx);
            let workspace = original_window
                .and_then(|handle| handle.read(cx).ok())
                .map(|multi_workspace| multi_workspace.workspace().downgrade())
                .unwrap_or_else(WeakEntity::new_invalid);
            cx.new(|cx| ManageProfilesModal::new_embedded(store, workspace, window, cx))
                .into()
        }),
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ManageProfiles, window, cx| {
            open_manager(workspace, window, cx)
        });
        if let Some(window) = window
            && let Some(moved) = SettingsScopesNotice::pending()
        {
            cx.defer_in(window, move |workspace, window, cx| {
                workspace.toggle_modal(window, cx, move |_, cx| {
                    SettingsScopesNotice::new(moved, cx)
                });
            });
        }
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

/// Says once, in each profile, that the settings now have a scope, and which ones moved out of
/// the shared file when they were split.
struct SettingsScopesNotice {
    moved: Vec<String>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for SettingsScopesNotice {}
impl ModalView for SettingsScopesNotice {}

impl Focusable for SettingsScopesNotice {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Deserialize)]
struct SettingsScopesRecord {
    #[serde(default)]
    moved: Vec<String>,
}

impl SettingsScopesNotice {
    fn shown_marker() -> PathBuf {
        paths::data_dir().join("settings-scopes-notice-shown")
    }

    /// The settings the split moved, when this profile hasn't said so yet. Claims the notice, so
    /// a second window doesn't show it again.
    fn pending() -> Option<Vec<String>> {
        static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if CLAIMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        if Self::shown_marker().exists() {
            return None;
        }
        let record = std::fs::read_to_string(settings::migration_record_file()).ok()?;
        let record: SettingsScopesRecord = serde_json::from_str(&record).log_err()?;
        if record.moved.is_empty() {
            return None;
        }
        std::fs::write(Self::shown_marker(), b"").log_err();
        Some(record.moved)
    }

    fn new(moved: Vec<String>, cx: &mut Context<Self>) -> Self {
        Self {
            moved,
            focus_handle: cx.focus_handle(),
        }
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_scope(
        marker: AnyElement,
        title: SharedString,
        detail: SharedString,
        path: SharedString,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_start()
            .gap_3()
            .p_3()
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .child(
                div()
                    .w(px(18.))
                    .pt_0p5()
                    .flex()
                    .justify_center()
                    .child(marker),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(title).weight(gpui::FontWeight::MEDIUM))
                    .child(
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(path)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Placeholder),
                    ),
            )
    }
}

impl Render for SettingsScopesNotice {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let default_name = load_profiles()
            .ok()
            .and_then(|profiles| {
                profiles
                    .into_iter()
                    .find(|profile| profile.id == paths::DEFAULT_PROFILE_ID)
            })
            .map(|profile| profile.name)
            .unwrap_or_else(|| default_profile().name);
        let profile_color = ProfileStore::global(cx)
            .read(cx)
            .active_profile()
            .color
            .hsla();
        let count = self.moved.len();
        let moved = self.moved.join(", ");
        ui::modal_surface(cx)
            .key_context("SettingsScopesNotice")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .w(px(580.))
            .child(
                v_flex()
                    .gap_1p5()
                    .px_5()
                    .pt_5()
                    .pb_3p5()
                    .child(
                        Label::new(t!("zed.scopes.title"))
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::BOLD),
                    )
                    .child(Label::new(t!("zed.scopes.detail")).color(Color::Muted)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .px_5()
                    .pb_3p5()
                    .child(Self::render_scope(
                        Icon::new(IconName::UserGroup)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .into_any_element(),
                        t!("zed.scopes.all_title"),
                        t!("zed.scopes.all_detail"),
                        "~/.config/asylum/settings.json".into(),
                        cx,
                    ))
                    .child(Self::render_scope(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(profile_color)
                            .into_any_element(),
                        t!("zed.scopes.profile_title"),
                        t!("zed.scopes.profile_detail"),
                        "profiles/<perfil>/config/settings.json".into(),
                        cx,
                    ))
                    .child(Self::render_scope(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .into_any_element(),
                        t!("zed.scopes.project_title"),
                        t!("zed.scopes.project_detail"),
                        ".zed/settings.json".into(),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .id("scopes-moved")
                    .px_5()
                    .py_2p5()
                    .gap_2p5()
                    .bg(Color::Warning.color(cx).opacity(0.06))
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("zed.scopes.moved", count = count, name = default_name))
                            .size(LabelSize::Small),
                    )
                    .tooltip(Tooltip::text(moved)),
            )
            .child(
                ui::modal_footer(cx)
                    .child(
                        Label::new(t!("zed.scopes.backup"))
                            .size(LabelSize::Small)
                            .color(Color::Placeholder),
                    )
                    .child(div().flex_1())
                    .child(
                        ui::secondary_button(
                            "scopes-see-keys",
                            None,
                            t!("zed.scopes.see_keys", count = count),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::OpenSettingsPage {
                                    page: "AI".to_string(),
                                    target: None,
                                }),
                                cx,
                            );
                            cx.emit(DismissEvent);
                        })),
                    )
                    .child(
                        ui::primary_button(
                            "scopes-got-it",
                            None,
                            t!("zed.scopes.got_it"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}

/// Opens `paths` in the running instance of `profile_id`, starting it with them when it
/// isn't running.
pub fn forward_paths(profile_id: String, paths: Vec<String>, cx: &App) -> Task<Result<()>> {
    cx.background_spawn(async move {
        let request = mac_only_instance::InstanceRequest::Open {
            paths: paths.clone(),
        };
        if mac_only_instance::send_to_profile(&profile_id, &request) {
            return Ok(());
        }
        launch_profile(&profile_id, paths).await
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
    cx.background_spawn(async move {
        if mac_only_instance::activate_profile(&profile_id) {
            return Ok(());
        }
        launch_profile(&profile_id, Vec::new()).await
    })
}

async fn launch_profile(profile_id: &str, paths: Vec<String>) -> Result<()> {
    match profile_launchers::app_for_profile(profile_id) {
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

/// Replaces the icon of this process in the Dock and the app switcher until it quits.
fn set_dock_icon(icns: &'static [u8]) {
    use objc2::{AllocAnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(main_thread) = MainThreadMarker::new() else {
        log::error!("the Dock icon can only be set from the main thread");
        return;
    };
    let data = NSData::with_bytes(icns);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        log::error!("failed to decode the profile's Dock icon");
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);
    // SAFETY: called on the main thread with a valid image.
    unsafe { application.setApplicationIconImage(Some(&image)) };
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
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        let request = mac_only_instance::InstanceRequest::Dispatch {
            action: action.to_string(),
            data,
        };
        if mac_only_instance::send_to_profile(&profile_id, &request) {
            return Ok(());
        }
        launch_profile(&profile_id, Vec::new()).await?;
        // A profile that was just started takes a moment to claim its socket.
        for _ in 0..80 {
            executor.timer(Duration::from_millis(250)).await;
            if mac_only_instance::send_to_profile(&profile_id, &request) {
                return Ok(());
            }
        }
        anyhow::bail!("{}", t!("zed.profiles.error_no_response"))
    })
}

/// Replaces this process with `profile_id`: its windows close and the profile opens in
/// their place, or comes to the front when it is already running.
fn switch_in_place(profile_id: String, window: &mut Window, cx: &mut App) {
    let running = cx.background_spawn({
        let profile_id = profile_id.clone();
        async move {
            let activated = mac_only_instance::activate_profile(&profile_id);
            (activated, profile_launchers::app_for_profile(&profile_id))
        }
    });
    window
        .spawn(cx, async move |cx| {
            let (activated, app) = running.await;
            cx.update(|_, cx| {
                if activated {
                    cx.quit();
                } else {
                    // Restarting reopens this process's own bundle, which from a launcher is
                    // the launcher of this profile rather than of the one being switched to.
                    if let Some(app) = app {
                        cx.set_restart_path(app);
                    }
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
    /// `None` when the Repo panel isn't part of this build.
    #[serde(default)]
    bitbucket_connected: Option<bool>,
    git_committer: Option<String>,
    #[serde(default)]
    git_name: Option<String>,
    #[serde(default)]
    git_email: Option<String>,
    #[serde(default)]
    agent_toolkits: Vec<ToolkitSummary>,
}

/// A toolkit of the agent and whether the profile has it on, before any project weighs in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ToolkitSummary {
    id: String,
    name: String,
    /// An `IconName` in snake case.
    icon: String,
    #[serde(default)]
    tool_count: usize,
    #[serde(default)]
    mention: Option<String>,
    enabled: bool,
}

impl ToolkitSummary {
    fn detail(&self) -> SharedString {
        let tools = if self.tool_count == 1 {
            t!("zed.profiles.toolkit_tools.one")
        } else {
            t!("zed.profiles.toolkit_tools.other", count = self.tool_count)
        };
        match &self.mention {
            Some(mention) => t!(
                "zed.profiles.toolkit_mention",
                tools = tools,
                mention = mention
            ),
            None => tools,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct McpServerSummary {
    id: String,
    /// The command or URL of the server, empty for one an extension provides.
    detail: String,
    enabled: bool,
    /// Listed in `agent.on_demand_context_servers`: the agent asks before loading its tools.
    #[serde(default)]
    on_demand: bool,
}

const CLICKUP_CREDENTIALS_URL: &str = "https://app.clickup.com/asylum";
const CLICKUP_OPEN_ACTION: &str = "clickup::ToggleFocus";
const CLICKUP_DISCONNECT_ACTION: &str = "clickup::Disconnect";
const BITBUCKET_CREDENTIALS_URL: &str = "https://bitbucket.org/asylum";
const REPO_OPEN_ACTION: &str = "repo_hosting::ToggleFocus";
const BITBUCKET_DISCONNECT_ACTION: &str = "repo_hosting::DisconnectBitbucket";

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
        ContextServerSettings::Extension { .. } => String::new(),
    }
}

fn toolkit_summaries(cx: &App) -> Vec<ToolkitSummary> {
    task_agents::toolkits(cx)
        .iter()
        .map(|toolkit| ToolkitSummary {
            id: toolkit.id.to_string(),
            name: toolkit.name.to_string(),
            icon: toolkit.icon.to_string(),
            tool_count: toolkit.tools.len(),
            mention: toolkit.mention.as_ref().map(|mention| mention.to_string()),
            enabled: task_agents::toolkit_enabled_in_profile(&toolkit.id, cx),
        })
        .collect()
}

fn mcp_server_summaries(cx: &App) -> Vec<McpServerSummary> {
    let on_demand_servers = &AgentSettings::get_global(cx).on_demand_context_servers;
    let mut servers: Vec<McpServerSummary> = ProjectSettings::get_global(cx)
        .context_servers
        .iter()
        .map(|(id, settings)| McpServerSummary {
            id: id.to_string(),
            detail: mcp_server_detail(settings),
            enabled: settings.enabled(),
            on_demand: on_demand_servers.iter().any(|server| server == id.as_ref()),
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
    bitbucket_connected: Option<bool>,
    git_committer: Option<String>,
    git_identity: git::repository::GitCommitter,
    load_error: Option<SharedString>,
    refresh_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    /// The profiles, as (id, name, color), that the launchers were last written for.
    synced_launchers: Option<Vec<(String, String, ProfileColor)>>,
    launchers_task: Option<Task<()>>,
    /// Where each profile's launcher is, once they were synced.
    launchers: HashMap<String, PathBuf>,
    /// The color the Dock icon shows, once it was first worked out: `None` inside is the
    /// app's own icon in a profile whose color isn't the app's.
    dock_icon: Option<Option<ProfileColor>>,
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
                bitbucket_connected: None,
                git_committer: None,
                git_identity: git::repository::GitCommitter {
                    name: None,
                    email: None,
                },
                load_error: None,
                refresh_task: None,
                publish_task: None,
                synced_launchers: None,
                launchers_task: None,
                launchers: HashMap::default(),
                dock_icon: None,
                _subscriptions: subscriptions,
            };
            store.refresh(cx);
            store.refresh_accounts(cx);
            store
        });
        cx.set_global(GlobalProfileStore(store.clone()));
        let profiles = store.read(cx).settings_profiles();
        settings_ui::set_settings_profiles(profiles, cx);
        store
    }

    /// Lets the settings window name the profile's scope and list the profiles.
    fn publish_to_settings_window(&self, cx: &mut App) {
        settings_ui::set_settings_profiles(self.settings_profiles(), cx);
    }

    fn settings_profiles(&self) -> settings_ui::SettingsProfiles {
        let to_settings_profile = |profile: &AppProfile| settings_ui::SettingsProfile {
            id: profile.id.clone().into(),
            name: profile.name.clone().into(),
            color: profile.color.hsla(),
            running: profile.id == active_profile_id() || self.running.contains(&profile.id),
        };
        settings_ui::SettingsProfiles {
            current: Some(to_settings_profile(&self.active_profile())),
            all: self.profiles.iter().map(to_settings_profile).collect(),
        }
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
                open_on_launch: Vec::new(),
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
        let default_model = registry
            .default_model()
            .map(|model| format!("{} · {}", model.provider_name.0, model.name.0));
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
            bitbucket_connected: cx
                .build_action(REPO_OPEN_ACTION, None)
                .is_ok()
                .then_some(self.bitbucket_connected.unwrap_or(false)),
            git_committer: self.git_committer.clone(),
            git_name: self.git_identity.name.clone(),
            git_email: self.git_identity.email.clone(),
            agent_toolkits: toolkit_summaries(cx),
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

    /// Checks the accounts this process can see only asynchronously: whether ClickUp and
    /// Bitbucket have a token and who git commits as.
    fn refresh_accounts(&mut self, cx: &mut Context<Self>) {
        let credentials = zed_credentials_provider::global(cx);
        cx.spawn(async move |this, cx| {
            let clickup_connected = credentials
                .read_credentials(CLICKUP_CREDENTIALS_URL, cx)
                .await
                .log_err()
                .map(|credentials| credentials.is_some());
            let bitbucket_connected = credentials
                .read_credentials(BITBUCKET_CREDENTIALS_URL, cx)
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
                this.bitbucket_connected = bitbucket_connected;
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
                    "{}",
                    t!(
                        "zed.profiles.error_git_config",
                        key = key,
                        message = String::from_utf8_lossy(&output.stderr).trim()
                    )
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
                        this.sync_launchers(cx);
                        this.update_dock_icon();
                    }
                    Err(error) => {
                        log::error!("failed to load profiles: {error:#}");
                        this.load_error = Some(format!("{error:#}").into());
                    }
                }
                this.running = running;
                this.publish_to_settings_window(cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Brings the launchers in `~/Applications` in line with the registry: one per profile
    /// other than the default, which is the app itself. Every running profile does this when
    /// it sees the registry change, so a profile renamed or recolored in one process updates
    /// its launcher even when that process doesn't run from a bundle.
    fn sync_launchers(&mut self, cx: &mut Context<Self>) {
        let launchers: Vec<(String, String, ProfileColor)> = self
            .profiles
            .iter()
            .filter(|profile| profile.id != paths::DEFAULT_PROFILE_ID)
            .map(|profile| (profile.id.clone(), profile.name.clone(), profile.color))
            .collect();
        if self.synced_launchers.as_ref() == Some(&launchers) {
            return;
        }
        let profiles: Vec<profile_launchers::LauncherProfile> = launchers
            .iter()
            .map(|(id, name, color)| profile_launchers::LauncherProfile {
                id: id.clone(),
                name: name.clone(),
                icon: color.launcher_icon(),
            })
            .collect();
        self.synced_launchers = Some(launchers);
        let sync = cx.background_spawn(async move {
            profile_launchers::sync_launchers(&profiles)
                .await
                .context("failed to update the profile launchers")
        });
        self.launchers_task = Some(cx.spawn(async move |this, cx| {
            let Some(launchers) = sync.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.launchers = launchers;
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Keeps the Dock icon in the profile's color, also after the color changes while the
    /// profile runs: the icon the process started with is never reloaded from its bundle.
    fn update_dock_icon(&mut self) {
        let color = self.active_profile().color;
        let shown = *self.dock_icon.get_or_insert_with(|| {
            if profile_launchers::runs_from_launcher() {
                // The launcher was written with the profile's color.
                Some(color)
            } else if active_profile_id() == paths::DEFAULT_PROFILE_ID {
                BUNDLE_ICON_COLOR
            } else {
                // Shows the app's icon until it gets the profile's color below.
                None
            }
        });
        if shown == Some(color) {
            return;
        }
        self.dock_icon = Some(Some(color));
        set_dock_icon(color.launcher_icon());
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
                    .with_context(|| t!("zed.profiles.error_profile_gone"))?;
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
                "{}",
                t!("zed.profiles.error_delete_default")
            );
            anyhow::ensure!(
                profile_id != active_profile_id(),
                "{}",
                t!("zed.profiles.error_delete_active")
            );
            let data_dir = paths::profiles_dir().join(&profile_id);
            let credential_urls = cx
                .background_spawn({
                    let profile_id = profile_id.clone();
                    let data_dir = data_dir.clone();
                    async move {
                        anyhow::ensure!(
                            !mac_only_instance::is_profile_running(&profile_id),
                            "{}",
                            t!("zed.profiles.error_close_before_delete")
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
            anyhow::ensure!(
                !name.is_empty(),
                "{}",
                t!("zed.profiles.error_name_required")
            );
            let profile = cx
                .background_spawn(async move {
                    let mut profiles = load_profiles()?;
                    let profile = AppProfile {
                        id: profile_id_for_name(&name, &profiles),
                        name,
                        color,
                        folders: Vec::new(),
                        open_on_launch: Vec::new(),
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

/// The segment at the start of the status bar naming the active profile.
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.store.read(cx).active_profile();
        let color = profile.color.hsla();
        let is_open = self.menu_handle.is_deployed();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let indicator = cx.weak_entity();
        // The segment is the status bar's left edge, so its corners follow the bar's, inside
        // the bar's 1px border.
        let corner_radius = pane_corner_radius_px(window) - px(1.);

        h_flex()
            .id("profile-segment")
            .h_full()
            .rounded_tl(corner_radius)
            .rounded_bl(corner_radius)
            .bg(color.opacity(if is_open { 0.22 } else { 0.14 }))
            .hover(|style| style.bg(color.opacity(0.22)))
            .child(
                PopoverMenu::new("profile-picker")
                    .with_handle(self.menu_handle.clone())
                    .anchor(Anchor::BottomLeft)
                    .offset(gpui::Point {
                        x: px(0.),
                        y: -px(6.),
                    })
                    .menu(move |window, cx| {
                        let store = store.clone();
                        let workspace = workspace.clone();
                        let picker = cx.new(|cx| ProfilePicker::new(store, workspace, window, cx));
                        // The segment stays lit while its menu is open, so it redraws when the
                        // menu opens and again when it is dismissed.
                        indicator.update(cx, |_, cx| cx.notify()).log_err();
                        let indicator = indicator.clone();
                        cx.subscribe(&picker, move |_, _: &DismissEvent, cx| {
                            indicator.update(cx, |_, cx| cx.notify()).log_err();
                        })
                        .detach();
                        Some(picker)
                    })
                    .trigger_with_tooltip(
                        // Transparent, so the hover is the segment's own fill rather than a
                        // smaller rectangle drawn inside it.
                        ButtonLike::new("profile-chip")
                            .style(ButtonStyle::Transparent)
                            .size(ButtonSize::None)
                            .height(relative(1.))
                            .tab_index(0isize)
                            .aria_label(t!("zed.indicator.aria_label", name = profile.name))
                            .child(
                                h_flex()
                                    .h_full()
                                    .px_3()
                                    .gap_1p5()
                                    .child(div().size_1p5().rounded_full().bg(color))
                                    .child(
                                        Label::new(profile.name)
                                            .size(LabelSize::Small)
                                            .weight(if is_open {
                                                gpui::FontWeight::BOLD
                                            } else {
                                                gpui::FontWeight::MEDIUM
                                            })
                                            .color(Color::Custom(color)),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronUpDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Custom(color.opacity(if is_open {
                                                1.
                                            } else {
                                                0.7
                                            }))),
                                    ),
                            ),
                        Tooltip::text(t!("zed.indicator.tooltip")),
                    ),
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
            editor.set_placeholder_text(&t!("zed.profiles.name_placeholder"), window, cx);
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
                t!("zed.profiles.status_this_window")
            } else if store.running.contains(&profile.id) {
                t!("zed.profiles.status_open")
            } else {
                t!("zed.profiles.status_closed")
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
                                .tooltip(Tooltip::text(t!("zed.picker.open_in_place")))
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
                Label::new(t!("zed.profiles.title"))
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
                    .child(Label::new(t!("zed.picker.new_profile"))),
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
                    .child(Label::new(t!("zed.picker.manage"))),
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
                t!("zed.picker.bound", project = project_name),
                t!("zed.picker.unbind_hint"),
            )
        } else {
            (
                IconName::Folder,
                t!("zed.picker.bind", project = project_name),
                match other_owner {
                    Some(owner) => t!("zed.picker.owned_by", owner = owner),
                    None => t!("zed.picker.opens_here"),
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
                .id(color.id())
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
                Label::new(t!("zed.picker.create_title"))
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
                Label::new(t!("zed.picker.create_description"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(error, |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("cancel-profile", t!("zed.profiles.cancel"))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            })),
                    )
                    .child(
                        Button::new("create-profile", t!("zed.picker.create_confirm"))
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
    /// Shown as the Profiles page of the settings window instead of a modal.
    embedded: bool,
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
            editor.set_placeholder_text(&t!("zed.profiles.name_placeholder"), window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            // The toolkit switches write the settings; the card reads them back live.
            cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
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
            embedded: false,
            _subscriptions: subscriptions,
        };
        this.select(active_profile_id().to_string(), window, cx);
        this
    }

    fn new_embedded(
        store: Entity<ProfileStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(store, workspace, window, cx);
        this.embedded = true;
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
                move |profile| {
                    profile.folders.retain(|entry| *entry != folder);
                    profile.open_on_launch.retain(|entry| *entry != folder);
                },
                cx,
            )
        });
        self.report(task, cx);
    }

    fn toggle_open_on_launch(&mut self, folder: String, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |store, cx| {
            store.update_profile(
                self.selected_id.clone(),
                move |profile| {
                    if profile.open_on_launch.contains(&folder) {
                        profile.open_on_launch.retain(|entry| *entry != folder);
                    } else {
                        profile.open_on_launch.push(folder);
                    }
                },
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
            prompt: Some(t!("zed.manager.add_folder_prompt")),
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
        let task = self.store.update(cx, |store, cx| {
            store.create(&t!("zed.manager.new_profile"), color, cx)
        });
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
            &t!("zed.manager.delete_title", name = profile.name),
            Some(t!("zed.manager.delete_detail").as_ref()),
            &[
                t!("zed.manager.delete").as_ref(),
                t!("zed.profiles.cancel").as_ref(),
            ],
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

    fn render_section_title(title: SharedString) -> impl IntoElement {
        Label::new(title)
            .size(LabelSize::XSmall)
            .weight(gpui::FontWeight::BOLD)
            .color(Color::Placeholder)
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let rows = store.profiles.iter().enumerate().map(|(index, profile)| {
            let is_selected = profile.id == self.selected_id;
            let status = if profile.id == active_profile_id() {
                t!("zed.profiles.status_this_window")
            } else if store.running.contains(&profile.id) {
                t!("zed.profiles.status_open")
            } else {
                t!("zed.profiles.status_closed")
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
                    .child(Self::render_section_title(t!("zed.profiles.title"))),
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
                    .child(Label::new(t!("zed.manager.new_profile")).color(Color::Muted)),
            )
            .child(div().flex_1())
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().element_background)
                    .child(Self::render_section_title(t!("zed.manager.shared_title")))
                    .child(
                        Label::new(t!("zed.manager.shared_detail"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_card(
        title: SharedString,
        action: Option<AnyElement>,
        rows: Vec<AnyElement>,
        empty: Option<SharedString>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .w_full()
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .child(
                h_flex()
                    .justify_between()
                    .px(px(14.))
                    .py_2p5()
                    .child(Self::render_section_title(title.to_uppercase().into()))
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
                    .border_color(cx.theme().colors().border)
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
            .px(px(14.))
            .py_2()
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

    fn render_status(label: SharedString, ok: bool) -> AnyElement {
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
        let name_editor = new_editor(name, &t!("zed.git.name_placeholder"), window, cx);
        let email_editor = new_editor(email, &t!("zed.git.email_placeholder"), window, cx);
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
        settings::update_settings_file(fs, cx, move |settings, cx| {
            // A server from a project or an extension isn't in this profile's file yet, and
            // switching it has to be remembered here all the same.
            if !settings
                .project
                .context_servers
                .contains_key(server.as_str())
                && let Some(merged) = cx
                    .global::<SettingsStore>()
                    .merged_settings()
                    .project
                    .context_servers
                    .get(server.as_str())
            {
                settings
                    .project
                    .context_servers
                    .insert(server.as_str().into(), merged.clone());
            }
            if let Some(server) = settings.project.context_servers.get_mut(server.as_str()) {
                server.set_enabled(enabled);
            }
        });
    }

    fn toggle_mcp_on_demand(&self, server: String, on_demand: bool, cx: &mut Context<Self>) {
        let fs = <dyn Fs>::global(cx);
        settings::update_settings_file(fs, cx, move |settings, _| {
            let servers = settings
                .agent
                .get_or_insert_default()
                .on_demand_context_servers
                .get_or_insert_default();
            servers.retain(|id| id != &server);
            if on_demand {
                servers.push(server);
                servers.sort();
            }
        });
    }

    /// Whether the MCP server `id` of this window's project is running, as a short label.
    fn mcp_status(&self, id: &str, cx: &App) -> Option<(SharedString, bool)> {
        let workspace = self.workspace.upgrade()?;
        let store = workspace.read(cx).project().read(cx).context_server_store();
        let store = store.read(cx);
        let server_id = store
            .server_ids()
            .iter()
            .find(|server| server.0.as_ref() == id)?;
        Some(match store.status_for_server(server_id)? {
            ContextServerStatus::Running => (t!("zed.mcp.status_running"), true),
            ContextServerStatus::Starting | ContextServerStatus::Authenticating => {
                (t!("zed.mcp.status_starting"), false)
            }
            ContextServerStatus::Stopped => (t!("zed.mcp.status_stopped"), false),
            ContextServerStatus::AuthRequired
            | ContextServerStatus::ClientSecretRequired { .. } => {
                (t!("zed.mcp.status_needs_login"), false)
            }
            ContextServerStatus::Error(_) => (t!("zed.mcp.status_error"), false),
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
            t!("zed.git.source_default")
        } else {
            t!("zed.git.source_profile")
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
                                Label::new(t!("zed.git.writes_to", source = source))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new(
                                            "cancel-git-identity",
                                            t!("zed.profiles.cancel"),
                                        )
                                        .label_size(LabelSize::Small)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.cancel_git_identity(cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        Button::new("save-git-identity", t!("zed.git.save"))
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
                    Some(committer) => t!("zed.git.commits_as", committer = committer),
                    None => t!("zed.git.no_name"),
                },
                Some(source),
                Some(
                    Button::new("edit-git-identity", t!("zed.git.edit"))
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
            t!("zed.git.providers"),
            Some(t!("zed.git.providers_detail")),
            Some(
                Label::new(t!("zed.git.coming_soon"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            false,
            cx,
        );
        Self::render_card(
            t!("zed.git.title"),
            None,
            vec![committer, providers],
            None,
            cx,
        )
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
                    Some(Self::render_status(t!("zed.profiles.connected"), true)),
                    false,
                    cx,
                )
            })
            .collect();
        if rows.is_empty() {
            rows.push(Self::render_row(
                IconName::AiZed,
                Color::Muted,
                t!("zed.ai.no_provider"),
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
            t!("zed.ai.default_model"),
            None,
            Some(text(&summary.default_model)),
            false,
            cx,
        ));
        rows.push(Self::render_row(
            IconName::Sliders,
            Color::Default,
            t!("zed.ai.agent_profile"),
            None,
            Some(text(&summary.agent_profile)),
            false,
            cx,
        ));
        let action = Some(
            Button::new(
                "configure-ai",
                if is_active {
                    t!("zed.ai.configure")
                } else {
                    t!("zed.ai.configure_there")
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
        Self::render_card(t!("zed.ai.title"), action, rows, None, cx)
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
                    let on_demand_server = server.id.clone();
                    let on_demand = server.on_demand;
                    h_flex()
                        .gap_2()
                        .children(status)
                        .when(server.enabled, |this| {
                            this.child(
                                Button::new(("mcp-on-demand", index), t!("zed.mcp.on_demand"))
                                    .label_size(LabelSize::XSmall)
                                    .toggle_state(on_demand)
                                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                    .tooltip(Tooltip::text(t!("zed.mcp.on_demand_tooltip")))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_mcp_on_demand(
                                            on_demand_server.clone(),
                                            !on_demand,
                                            cx,
                                        )
                                    })),
                            )
                        })
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
                        if !server.enabled {
                            t!("zed.profiles.off")
                        } else if server.on_demand {
                            t!("zed.mcp.on_demand")
                        } else {
                            t!("zed.profiles.on")
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
                    Some(if server.detail.is_empty() {
                        t!("zed.mcp.extension")
                    } else {
                        server.detail.clone().into()
                    }),
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
                    t!("zed.mcp.add")
                } else {
                    t!("zed.mcp.add_there")
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
            t!("zed.mcp.title"),
            action,
            rows,
            Some(t!("zed.mcp.empty")),
            cx,
        )
    }

    fn render_integrations_card(
        &self,
        summary: &ProfileSummary,
        is_active: bool,
        cx: &App,
    ) -> Option<impl IntoElement> {
        let integrations = [
            (
                summary.clickup_connected,
                "clickup-connection",
                IconName::ListTodo,
                "ClickUp",
                t!("zed.integrations.clickup_detail"),
                CLICKUP_OPEN_ACTION,
                CLICKUP_DISCONNECT_ACTION,
            ),
            (
                summary.bitbucket_connected,
                "bitbucket-connection",
                IconName::PullRequest,
                "Bitbucket",
                t!("zed.integrations.bitbucket_detail"),
                REPO_OPEN_ACTION,
                BITBUCKET_DISCONNECT_ACTION,
            ),
        ];
        let rows: Vec<AnyElement> = integrations
            .into_iter()
            .filter_map(
                |(connected, id, icon, name, detail, open_action, disconnect_action)| {
                    let connected = connected?;
                    let (label, action) = if connected {
                        (t!("zed.integrations.disconnect"), disconnect_action)
                    } else {
                        (t!("zed.integrations.connect"), open_action)
                    };
                    let trailing = h_flex()
                        .gap_2()
                        .when(!is_active, |this| {
                            this.child(Self::render_status(
                                if connected {
                                    t!("zed.profiles.connected")
                                } else {
                                    t!("zed.profiles.disconnected")
                                },
                                connected,
                            ))
                        })
                        .child(
                            Button::new(id, label)
                                .label_size(LabelSize::Small)
                                .on_click(self.profile_action(&self.selected_id, action, None)),
                        )
                        .into_any_element();
                    Some(Self::render_row(
                        icon,
                        Color::Default,
                        name,
                        Some(detail),
                        Some(trailing),
                        false,
                        cx,
                    ))
                },
            )
            .collect();
        (!rows.is_empty())
            .then(|| Self::render_card(t!("zed.integrations.title"), None, rows, None, cx))
    }

    fn render_toolkits_card(
        &self,
        summary: &ProfileSummary,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if summary.agent_toolkits.is_empty() {
            return None;
        }
        let project = is_active
            .then(|| self.workspace.upgrade())
            .flatten()
            .map(|workspace| workspace.read(cx).project().clone());
        let mut enabled_count = 0;
        let mut rows: Vec<AnyElement> = summary
            .agent_toolkits
            .iter()
            .enumerate()
            .map(|(index, toolkit)| {
                // In this window, a project's `.zed/settings.json` can disagree with the profile.
                let project_override = project.as_ref().and_then(|project| {
                    task_agents::toolkit_project_override(&toolkit.id, project, cx)
                });
                let enabled = project_override.unwrap_or(toolkit.enabled);
                if enabled {
                    enabled_count += 1;
                }
                let icon = toolkit.icon.parse::<IconName>().unwrap_or(IconName::Wrench);
                let on_off = |enabled: bool| {
                    if enabled {
                        t!("zed.profiles.on")
                    } else {
                        t!("zed.profiles.off")
                    }
                };
                let detail: SharedString = match project_override {
                    Some(enabled) => t!(
                        "zed.toolkits.project_override",
                        state = on_off(enabled),
                        profile_state = on_off(toolkit.enabled),
                    ),
                    None if enabled => toolkit.detail(),
                    None => t!("zed.toolkits.disabled_detail"),
                };
                let trailing = if is_active {
                    let switch = Switch::new(
                        ("toolkit-toggle", index),
                        if enabled {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    );
                    let switch = if project_override.is_some() {
                        // Switching the profile here would be undone by the project the moment
                        // it is saved, so the switch takes the user to the file that decides.
                        switch.on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(zed_actions::OpenProjectSettings), cx)
                        })
                    } else {
                        let toolkit_id = toolkit.id.clone();
                        switch.on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                            this.set_toolkit_enabled(
                                toolkit_id.clone(),
                                *state == ToggleState::Selected,
                                cx,
                            )
                        }))
                    };
                    h_flex()
                        .gap_2()
                        .when(project_override.is_some(), |this| {
                            this.child(
                                Label::new(t!("zed.toolkits.project"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                        })
                        .child(switch)
                        .into_any_element()
                } else {
                    Self::render_status(on_off(enabled), enabled)
                };
                Self::render_row(
                    icon,
                    if enabled {
                        Color::Default
                    } else {
                        Color::Muted
                    },
                    toolkit.name.clone(),
                    Some(detail),
                    Some(trailing),
                    false,
                    cx,
                )
            })
            .collect();
        rows.push(Self::render_row(
            IconName::Info,
            Color::Muted,
            t!("zed.toolkits.scope"),
            Some(t!("zed.toolkits.scope_detail")),
            None,
            false,
            cx,
        ));
        let total = summary.agent_toolkits.len();
        let count = Label::new(t!(
            "zed.toolkits.count",
            enabled = enabled_count,
            total = total
        ))
        .size(LabelSize::XSmall)
        .color(Color::Muted)
        .into_any_element();
        Some(Self::render_card(
            t!("zed.toolkits.title"),
            Some(count),
            rows,
            None,
            cx,
        ))
    }

    fn set_toolkit_enabled(&self, toolkit_id: String, enabled: bool, cx: &mut Context<Self>) {
        let fs = <dyn Fs>::global(cx);
        settings::update_settings_file(fs, cx, move |settings, cx| {
            let toolkits = &mut settings.project.agent_toolkits;
            // Leaving the key out means on, so the file only names what was turned off, unless
            // the file every profile shares turns it off.
            if enabled {
                toolkits.remove(toolkit_id.as_str());
                let off_below = [SettingsFile::Shared, SettingsFile::Global]
                    .into_iter()
                    .filter_map(|file| cx.global::<SettingsStore>().get_content_for_file(file))
                    .any(|content| {
                        content.project.agent_toolkits.get(toolkit_id.as_str()) == Some(&false)
                    });
                if off_below {
                    toolkits.insert(toolkit_id.as_str().into(), true);
                }
            } else {
                toolkits.insert(toolkit_id.as_str().into(), false);
            }
        });
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
                let toggled = folder.clone();
                let opens_on_launch = profile.open_on_launch.contains(folder);
                // A glob names many folders, so there's no single one to open.
                let launch_toggle = (!is_glob(folder)).then(|| {
                    IconButton::new(("open-on-launch", index), IconName::PlayOutlined)
                        .icon_size(IconSize::XSmall)
                        .icon_color(if opens_on_launch {
                            Color::Accent
                        } else {
                            Color::Muted
                        })
                        .toggle_state(opens_on_launch)
                        .tooltip(Tooltip::text(if opens_on_launch {
                            t!("zed.folders.open_on_launch_off")
                        } else {
                            t!("zed.folders.open_on_launch_on")
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_open_on_launch(toggled.clone(), cx)
                        }))
                });
                Self::render_row(
                    IconName::Folder,
                    Color::Custom(profile.color.hsla()),
                    folder.clone(),
                    opens_on_launch.then(|| t!("zed.folders.opens_on_launch")),
                    Some(
                        h_flex()
                            .gap_0p5()
                            .children(launch_toggle)
                            .child(
                                IconButton::new(("remove-folder", index), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text(t!("zed.folders.unlink")))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_folder(removed.clone(), cx)
                                    })),
                            )
                            .into_any_element(),
                    ),
                    true,
                    cx,
                )
            })
            .collect();
        let action = Button::new("add-folder", t!("zed.folders.add"))
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
            .on_click(cx.listener(|this, _, _, cx| this.add_folder(cx)))
            .into_any_element();
        Self::render_card(
            t!("zed.folders.title"),
            Some(action),
            rows,
            Some(t!("zed.folders.empty")),
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
            t!("zed.data.settings"),
            Some(t!("zed.data.settings_detail")),
            Some(
                Button::new(
                    "open-settings",
                    if is_active {
                        t!("zed.data.open")
                    } else {
                        t!("zed.data.open_there")
                    },
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
            t!("zed.data.history"),
            Some(t!("zed.data.history_detail")),
            Some(
                Label::new(t!("zed.data.isolated"))
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
                t!("zed.data.edit_elsewhere"),
                Some(t!("zed.data.edit_elsewhere_detail")),
                Some(
                    Button::new("open-profile", t!("zed.data.open_profile"))
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
        Self::render_card(t!("zed.data.title"), None, rows, None, cx)
    }

    fn render_details(&self, profile: &AppProfile, cx: &mut Context<Self>) -> impl IntoElement {
        let is_default = profile.id == paths::DEFAULT_PROFILE_ID;
        let is_active = profile.id == active_profile_id();
        let (is_running, summary, launcher) = {
            let store = self.store.read(cx);
            let summary = if is_active {
                Some(store.current_summary(cx))
            } else {
                store.summaries.get(&profile.id).cloned()
            };
            (
                store.running.contains(&profile.id),
                summary,
                store.launchers.get(&profile.id).cloned(),
            )
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
                    .id(color.id())
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
            Some(t!("zed.details.hint_default"))
        } else if is_active {
            Some(t!("zed.details.hint_active"))
        } else if is_running {
            Some(t!("zed.details.hint_running"))
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
                    .children(self.render_toolkits_card(summary, is_active, cx))
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
                        t!("zed.details.unknown_title"),
                        None,
                        Vec::new(),
                        Some(t!("zed.details.unknown_empty")),
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
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(data_dir)
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .when_some(launcher, |this, launcher| {
                                        this.child(
                                            Button::new(
                                                "reveal-launcher",
                                                t!("zed.details.reveal_launcher"),
                                            )
                                            .label_size(LabelSize::XSmall)
                                            .tooltip(Tooltip::text(t!(
                                                "zed.details.reveal_launcher_tooltip"
                                            )))
                                            .on_click(move |_, _, cx| cx.reveal_path(&launcher)),
                                        )
                                    }),
                            ),
                    )
                    .child(h_flex().gap_1p5().children(swatches)),
            )
            .child(h_flex().items_start().gap_3().child(left).child(right))
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(match (&self.error, &delete_hint) {
                        (Some(error), _) => Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                        (None, Some(hint)) => Label::new(hint.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        (None, None) => Label::new("").size(LabelSize::Small),
                    })
                    .child(
                        Button::new("delete-profile", t!("zed.details.delete"))
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

impl ManageProfilesModal {
    /// The Profiles page of the settings window: a header with the profiles, then the selected
    /// one's cards.
    fn render_embedded(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let store = self.store.read(cx);
        let segments = store
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let selected = profile.id == self.selected_id;
                let status = if profile.id == active_profile_id() {
                    t!("zed.profiles.status_this_window")
                } else if store.running.contains(&profile.id) {
                    t!("zed.profiles.status_open")
                } else {
                    t!("zed.profiles.status_closed")
                };
                let profile_id = profile.id.clone();
                h_flex()
                    .id(("profile-segment", index))
                    .h(px(26.))
                    .px_2p5()
                    .gap_1p5()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(colors.element_selected)
                            .border_color(colors.border_focused)
                    })
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(colors.element_hover))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(profile_id.clone(), window, cx)
                    }))
                    .child(div().size(px(7.)).rounded_full().bg(profile.color.hsla()))
                    .child(
                        Label::new(profile.name.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::MEDIUM)
                            .color(if selected {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(status)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            })
            .collect::<Vec<_>>();

        let selected = self.selected(cx);
        let closed_banner = selected.as_ref().and_then(|profile| {
            let store = self.store.read(cx);
            let closed = profile.id != active_profile_id() && !store.running.contains(&profile.id);
            closed.then(|| {
                let open_id = profile.id.clone();
                let color = profile.color.hsla();
                v_flex()
                    .w_full()
                    .child(
                        h_flex()
                            .w_full()
                            .px_7()
                            .py_2p5()
                            .gap_2p5()
                            .bg(color.opacity(0.06))
                            .child(div().size(px(7.)).rounded_full().bg(color))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        Label::new(t!(
                                            "zed.profiles.closed_banner",
                                            name = profile.name
                                        ))
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new(t!(
                                            "zed.profiles.closed_banner_detail",
                                            path = format!(
                                                "profiles/{}/config/settings.json",
                                                profile.id
                                            )
                                        ))
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(
                                ui::secondary_button(
                                    "open-closed-profile",
                                    None,
                                    t!("zed.profiles.open_named", name = profile.name),
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        open_profile(open_id.clone(), cx).detach_and_notify_err(
                                            this.workspace.clone(),
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            ),
                    )
                    .child(Divider::horizontal())
            })
        });

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .h(px(56.))
                    .flex_none()
                    .px_7()
                    .gap_3()
                    .bg(colors.panel_background)
                    .child(
                        Label::new(t!("zed.profiles.page_title"))
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::BOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .h(px(32.))
                            .p_0p5()
                            .gap_0p5()
                            .rounded_lg()
                            .border_1()
                            .border_color(colors.border_selected)
                            .bg(colors.editor_background)
                            .children(segments),
                    )
                    .child(
                        ui::secondary_button(
                            "new-profile",
                            None,
                            t!("zed.manager.new_profile"),
                            true,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.create_profile(window, cx)),
                        ),
                    ),
            )
            .child(Divider::horizontal())
            .children(closed_banner)
            .children(selected.map(|profile| self.render_embedded_details(&profile, cx)))
            .into_any_element()
    }

    fn render_identity_card(&self, profile: &AppProfile, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let swatches = ProfileColor::ALL
            .into_iter()
            .map(|color| {
                let is_selected = color == profile.color;
                div()
                    .id(color.id())
                    .size(px(18.))
                    .rounded_full()
                    .cursor_pointer()
                    .border_2()
                    .border_color(if is_selected {
                        colors.text
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(color.hsla())
                    .tooltip(Tooltip::text(color.label()))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_color(color, cx)))
            })
            .collect::<Vec<_>>();
        let launcher = self.store.read(cx).launchers.get(&profile.id).cloned();
        let label_cell = |label: SharedString| {
            div()
                .w(px(110.))
                .flex_none()
                .child(Label::new(label).color(Color::Muted))
        };
        let mut rows = vec![
            h_flex()
                .px(px(14.))
                .py_2()
                .gap_3()
                .child(label_cell(t!("zed.identity.name")))
                .child(
                    div()
                        .flex_1()
                        .h(px(28.))
                        .px_2p5()
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(colors.border_selected)
                        .bg(colors.editor_background)
                        .child(self.name_editor.clone()),
                )
                .into_any_element(),
            h_flex()
                .px(px(14.))
                .py_2()
                .gap_3()
                .child(label_cell(t!("zed.identity.color")))
                .child(h_flex().gap_2().children(swatches))
                .into_any_element(),
        ];
        if let Some(launcher) = launcher {
            rows.push(Self::render_row(
                IconName::ThisWindow,
                Color::Muted,
                t!("zed.identity.launcher"),
                Some(t!("zed.identity.launcher_detail")),
                Some(
                    ui::secondary_button(
                        "reveal-launcher",
                        None,
                        t!("zed.details.reveal_launcher"),
                        true,
                        cx,
                    )
                    .on_click(move |_, _, cx| cx.reveal_path(&launcher))
                    .into_any_element(),
                ),
                false,
                cx,
            ));
        }
        let folders = self.render_folders_card(profile, cx).into_any_element();
        v_flex()
            .w_full()
            .gap_4()
            .child(Self::render_card(
                t!("zed.identity.title"),
                None,
                rows,
                None,
                cx,
            ))
            .child(folders)
            .into_any_element()
    }

    fn render_embedded_details(&self, profile: &AppProfile, cx: &mut Context<Self>) -> AnyElement {
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
        let delete_hint = if is_default {
            Some(t!("zed.details.hint_default"))
        } else if is_active {
            Some(t!("zed.details.hint_active"))
        } else if is_running {
            Some(t!("zed.details.hint_running"))
        } else {
            None
        };
        let danger = h_flex()
            .w_full()
            .px(px(14.))
            .py_3()
            .gap_3()
            .rounded(px(10.))
            .border_1()
            .border_color(Color::Error.color(cx).opacity(0.25))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(t!("zed.details.delete_named", name = profile.name))
                            .weight(gpui::FontWeight::MEDIUM),
                    )
                    .child(
                        Label::new(match (&self.error, &delete_hint) {
                            (Some(error), _) => error.clone(),
                            (None, Some(hint)) => hint.clone(),
                            (None, None) => t!("zed.details.delete_detail"),
                        })
                        .size(LabelSize::Small)
                        .color(if self.error.is_some() {
                            Color::Error
                        } else {
                            Color::Muted
                        }),
                    ),
            )
            .child(
                ui::destructive_button(
                    "delete-profile",
                    t!("zed.details.delete"),
                    delete_hint.is_none(),
                    cx,
                )
                .when(delete_hint.is_none(), |this| {
                    this.on_click(
                        cx.listener(|this, _, window, cx| this.delete_selected(window, cx)),
                    )
                }),
            );

        let (left, right) = match &summary {
            Some(summary) => (
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_4()
                    .child(self.render_identity_card(profile, cx))
                    .child(self.render_git_card(profile, summary, cx))
                    .child(self.render_data_card(profile, is_active, cx))
                    .child(danger)
                    .into_any_element(),
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_4()
                    .child(self.render_ai_card(summary, is_active, cx))
                    .child(self.render_mcp_card(summary, is_active, cx))
                    .children(self.render_toolkits_card(summary, is_active, cx))
                    .children(self.render_integrations_card(summary, is_active, cx))
                    .into_any_element(),
            ),
            None => (
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_4()
                    .child(self.render_identity_card(profile, cx))
                    .child(self.render_data_card(profile, is_active, cx))
                    .child(danger)
                    .into_any_element(),
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_4()
                    .child(Self::render_card(
                        t!("zed.details.unknown_title"),
                        None,
                        Vec::new(),
                        Some(t!("zed.details.unknown_empty")),
                        cx,
                    ))
                    .into_any_element(),
            ),
        };

        div()
            .id("profile-page-details")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                h_flex()
                    .items_start()
                    .px_7()
                    .pt_5()
                    .pb_5()
                    .gap_4()
                    .child(left)
                    .child(right),
            )
            .into_any_element()
    }
}

impl Render for ManageProfilesModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.embedded {
            return self.render_embedded(cx);
        }
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
            .into_any_element()
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
            open_on_launch: Vec::new(),
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
