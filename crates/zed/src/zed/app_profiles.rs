//! Profiles: separate installations (settings, data and credentials) that run side by side,
//! one process each. This is the registry of known profiles and the status bar control that
//! shows the active one and switches to, or creates, another.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{
    Anchor, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global, Hsla,
    Task, WeakEntity, Window, prelude::*, rgb,
};
use serde::{Deserialize, Serialize};
use ui::{
    ButtonLike, Divider, IconButton, IconName, IconSize, Label, LabelSize, PopoverMenu,
    PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;
use util::paths::{PathMatcher, PathStyle, PathWithPosition};
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Workspace, notifications::NotifyTaskExt as _,
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

/// Lets File > Open offer to open a folder in the profile it belongs to.
pub fn init(cx: &mut App) {
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
pub struct ProfileStore {
    profiles: Vec<AppProfile>,
    running: HashSet<String>,
    load_error: Option<SharedString>,
    refresh_task: Option<Task<()>>,
}

struct GlobalProfileStore(Entity<ProfileStore>);

impl Global for GlobalProfileStore {}

impl ProfileStore {
    fn global(cx: &mut App) -> Entity<ProfileStore> {
        if let Some(store) = cx.try_global::<GlobalProfileStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = ProfileStore {
                profiles: vec![default_profile()],
                running: HashSet::default(),
                load_error: None,
                refresh_task: None,
            };
            store.refresh(cx);
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
            (profiles, running)
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (profiles, running) = task.await;
            this.update(cx, |this, cx| {
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
