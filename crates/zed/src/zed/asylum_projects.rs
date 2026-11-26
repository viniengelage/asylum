use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, BorrowAppContext as _, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Global, Hsla, PathPromptOptions, Subscription, Task,
    Window, actions, px,
};
use i18n::t;
use picker::{Picker, PickerDelegate};
use project::project_settings::ProjectSettings;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, SettingsStore};
use ui::{
    ContextMenu, EmptyState, KeyBinding, ListItem, ListItemSpacing, PopoverMenu,
    PopoverTriggerArea, checkbox_indicator, error_callout, grouped_block, info_note, modal_surface,
    option_card, prelude::*, primary_button, secondary_button, section_label,
    select_field_with_leading, text_field, toggle_switch,
};
use util::ResultExt as _;
use util::paths::PathExt as _;
use workspace::{ModalView, MultiWorkspace, OpenMode, Workspace};

actions!(
    projects,
    [
        /// Lists the Asylum projects to switch to one.
        OpenProject,
        /// Turns a folder into an Asylum project, with its own `.asylum/settings.json`.
        NewProject,
    ]
);

/// A folder the user works in, possibly holding several repositories, with its own settings in
/// `.asylum/`. The profile is who the user is; the project is where they are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsylumProject {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub profile_id: String,
    /// Seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ProjectsRegistry {
    projects: Vec<AsylumProject>,
    /// Skips "Escolher projeto" at launch and opens the most recent project.
    #[serde(default)]
    open_last_directly: bool,
}

fn load_registry() -> Result<ProjectsRegistry> {
    match std::fs::read(paths::asylum_projects_file()) {
        Ok(json) => serde_json::from_slice(&json).context("failed to parse projects.json"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ProjectsRegistry::default())
        }
        Err(error) => Err(error).context("failed to read projects.json"),
    }
}

fn save_registry(registry: &ProjectsRegistry) -> Result<()> {
    let path = paths::asylum_projects_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create the profiles directory")?;
    }
    let json = serde_json::to_vec_pretty(registry)?;
    std::fs::write(&path, json).context("failed to write projects.json")
}

pub fn load_projects() -> Result<Vec<AsylumProject>> {
    Ok(load_registry()?.projects)
}

pub fn save_projects(projects: &[AsylumProject]) -> Result<()> {
    let mut registry = load_registry()?;
    registry.projects = projects.to_vec();
    save_registry(&registry)
}

fn set_open_last_directly(open_last_directly: bool) -> Result<()> {
    let mut registry = load_registry()?;
    registry.open_last_directly = open_last_directly;
    save_registry(&registry)
}

/// The tools a folder shows it needs, which "Novo projeto" offers to turn on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DetectedTools {
    repo: bool,
    releases: bool,
    docker: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RepoFinding {
    /// 0 when the folder itself is the repository, otherwise how many child folders are.
    nested_count: usize,
    branch: Option<String>,
    origin: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ExpoFinding {
    files: Vec<&'static str>,
    bundle_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ComposeFinding {
    file_name: &'static str,
    services: Vec<String>,
}

/// What "Novo projeto" found in a folder, with the details its rows show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FolderFindings {
    repo: Option<RepoFinding>,
    expo: Option<ExpoFinding>,
    compose: Option<ComposeFinding>,
}

impl FolderFindings {
    fn tools(&self) -> DetectedTools {
        DetectedTools {
            repo: self.repo.is_some(),
            releases: self.expo.is_some(),
            docker: self.compose.is_some(),
        }
    }
}

/// The directory holding a repository's HEAD and config: `.git` itself, or the one a worktree's
/// `.git` file points to.
fn git_dir(repository_root: &Path) -> Option<PathBuf> {
    let dot_git = repository_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
    Some(if target.is_absolute() {
        target
    } else {
        repository_root.join(target)
    })
}

fn git_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// The `origin` URL without its scheme, user and `.git`, e.g. `bitbucket.org/sorriso/app`.
fn git_origin(git_dir: &Path) -> Option<String> {
    // A worktree keeps its config in the main repository's git dir.
    let config_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|common| git_dir.join(common.trim()))
        .unwrap_or_else(|| git_dir.to_path_buf());
    let config = std::fs::read_to_string(config_dir.join("config")).ok()?;
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin && let Some(url) = line.strip_prefix("url") {
            let url = url.trim_start().strip_prefix('=')?.trim();
            let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
            let without_user = without_scheme
                .split_once('@')
                .map_or(without_scheme, |(_, rest)| rest);
            let path = without_user.trim_end_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            return Some(path.replacen(':', "/", 1));
        }
    }
    None
}

fn find_repo(root: &Path) -> Option<RepoFinding> {
    if let Some(git_dir) = git_dir(root) {
        return Some(RepoFinding {
            nested_count: 0,
            branch: git_branch(&git_dir),
            origin: git_origin(&git_dir),
        });
    }
    let mut nested: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter_map(|child| git_dir(&child))
        .collect();
    nested.sort();
    let first = nested.first()?;
    Some(RepoFinding {
        nested_count: nested.len(),
        branch: git_branch(first),
        origin: git_origin(first),
    })
}

fn find_expo(root: &Path) -> Option<ExpoFinding> {
    let files: Vec<&'static str> = ["app.json", "eas.json"]
        .into_iter()
        .filter(|name| root.join(name).is_file())
        .collect();
    if files.is_empty() {
        return None;
    }
    let bundle_id = std::fs::read_to_string(root.join("app.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|app| {
            let expo = app.get("expo").unwrap_or(&app);
            expo.pointer("/ios/bundleIdentifier")
                .or_else(|| expo.pointer("/android/package"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        });
    Some(ExpoFinding { files, bundle_id })
}

/// The top-level keys under `services:`. Line-based, since only the names are shown and the
/// crate has no YAML parser.
fn compose_services(text: &str) -> Vec<String> {
    let mut services = Vec::new();
    let mut indent: Option<usize> = None;
    let mut in_services = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        let depth = trimmed.len() - trimmed.trim_start().len();
        if depth == 0 {
            in_services = trimmed == "services:";
            indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        let service_indent = *indent.get_or_insert(depth);
        if depth == service_indent
            && let Some(name) = trimmed.trim_start().strip_suffix(':')
        {
            services.push(name.trim_matches(['"', '\'']).to_string());
        }
    }
    services
}

fn find_compose(root: &Path) -> Option<ComposeFinding> {
    let file_name = [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .into_iter()
    .find(|name| root.join(name).is_file())?;
    let services = std::fs::read_to_string(root.join(file_name))
        .map(|text| compose_services(&text))
        .unwrap_or_default();
    Some(ComposeFinding {
        file_name,
        services,
    })
}

fn find_in_folder(root: &Path) -> FolderFindings {
    FolderFindings {
        repo: find_repo(root),
        expo: find_expo(root),
        compose: find_compose(root),
    }
}

fn relative_age(seconds_ago: i64) -> SharedString {
    let minutes = seconds_ago.max(0) / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if hours < 1 {
        t!("zed.projects.age_minutes", count = minutes.max(1))
    } else if days < 1 {
        t!("zed.projects.age_hours", count = hours)
    } else if days < 2 {
        t!("zed.projects.age_yesterday")
    } else if days < 14 {
        t!("zed.projects.age_days", count = days)
    } else if days < 60 {
        t!("zed.projects.age_weeks", count = days / 7)
    } else {
        t!("zed.projects.age_months", count = days / 30)
    }
}

/// The line on a chooser row: repositories and branch (or the Expo app), then when it was last
/// opened, as in "2 repositórios · main · há 2 h".
fn project_summary(project: &AsylumProject, now: i64) -> SharedString {
    let findings = find_in_folder(&project.root);
    let mut parts: Vec<SharedString> = Vec::new();
    if let Some(repo) = &findings.repo {
        if repo.nested_count == 1 {
            parts.push(t!("zed.projects.summary_repository"));
        } else if repo.nested_count > 1 {
            parts.push(t!(
                "zed.projects.summary_repositories",
                count = repo.nested_count
            ));
        }
        parts.extend(repo.branch.clone().map(SharedString::from));
    } else if findings.expo.is_some() {
        parts.push(t!("zed.projects.summary_expo"));
    }
    parts.extend(project.last_opened.map(|opened| relative_age(now - opened)));
    parts.join(" · ").into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectStart {
    InheritProfile,
    Empty,
}

struct NewProjectRequest {
    root: PathBuf,
    name: String,
    profile_id: String,
    tools: DetectedTools,
    start: ProjectStart,
    profile_servers: Vec<String>,
    profile_toolkits: Vec<String>,
}

fn project_settings(request: &NewProjectRequest) -> serde_json::Value {
    use serde_json::{Map, Value, json};
    let mut toolkits = Map::new();
    if request.start == ProjectStart::Empty {
        for toolkit in &request.profile_toolkits {
            toolkits.insert(toolkit.clone(), Value::Bool(false));
        }
    }
    for (toolkit, on) in [
        (task_agents::toolkit_ids::REPO, request.tools.repo),
        (task_agents::toolkit_ids::RELEASES, request.tools.releases),
        (task_agents::toolkit_ids::DOCKER, request.tools.docker),
    ] {
        if on {
            toolkits.insert(toolkit.to_string(), Value::Bool(true));
        }
    }
    let mut settings = Map::new();
    if !toolkits.is_empty() {
        settings.insert("agent_toolkits".to_string(), Value::Object(toolkits));
    }
    if request.start == ProjectStart::Empty && !request.profile_servers.is_empty() {
        // Only `{"enabled": false}`: the server stays defined by the profile and is just off
        // here, so turning it back on doesn't need its command again.
        let servers: Map<String, Value> = request
            .profile_servers
            .iter()
            .map(|server| (server.clone(), json!({ "enabled": false })))
            .collect();
        settings.insert("context_servers".to_string(), Value::Object(servers));
    }
    Value::Object(settings)
}

/// The project whose root holds `path`, the deepest one when projects are nested.
pub fn project_for_path<'a>(
    projects: &'a [AsylumProject],
    path: &Path,
) -> Option<&'a AsylumProject> {
    projects
        .iter()
        .filter(|project| path.starts_with(&project.root))
        .max_by_key(|project| project.root.components().count())
}

/// A folder with `.asylum/settings.json` is a project even before it is registered, so a
/// project cloned from a repository works right away.
fn unregistered_project(root: &Path) -> Option<AsylumProject> {
    if !paths::asylum_project_settings_file(root).is_file() {
        return None;
    }
    let name = root.file_name()?.to_string_lossy().into_owned();
    Some(AsylumProject {
        id: name.clone(),
        name,
        root: root.to_path_buf(),
        profile_id: paths::current_profile_id().to_string(),
        last_opened: None,
    })
}

/// The projects the profiles' folders become the first time, when there is no registry yet.
/// Nothing is written inside the folders.
fn projects_from_profile_folders(
    profile_folders: Vec<(String, Vec<PathBuf>)>,
) -> Vec<AsylumProject> {
    let mut projects: Vec<AsylumProject> = Vec::new();
    for (profile_id, folders) in profile_folders {
        for root in folders {
            if projects.iter().any(|project| project.root == root) {
                continue;
            }
            let name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            projects.push(AsylumProject {
                id: project_id_for_name(&name, &projects),
                name,
                root,
                profile_id: profile_id.clone(),
                last_opened: None,
            });
        }
    }
    projects
}

fn migrate_profile_folders() -> Result<()> {
    if paths::asylum_projects_file().exists() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let profile_folders = crate::zed::app_profiles::profile_folders();
    #[cfg(not(target_os = "macos"))]
    let profile_folders = Vec::new();
    save_projects(&projects_from_profile_folders(profile_folders))
}

pub fn mark_opened(project_id: &str) -> Result<()> {
    let mut projects = load_projects()?;
    let Some(project) = projects.iter_mut().find(|project| project.id == project_id) else {
        return Ok(());
    };
    project.last_opened = Some(chrono::Utc::now().timestamp());
    save_projects(&projects)
}

#[derive(Default)]
struct ActiveAsylumProject {
    project: Option<AsylumProject>,
    _watch: Option<Task<()>>,
}

impl Global for ActiveAsylumProject {}

/// The project of the focused window, whose `.asylum/settings.json` is the settings layer above
/// the profile.
pub fn active_project(cx: &App) -> Option<AsylumProject> {
    cx.try_global::<ActiveAsylumProject>()
        .and_then(|active| active.project.clone())
}

fn project_for_workspace(workspace: &Workspace, cx: &App) -> Option<AsylumProject> {
    let roots: Vec<PathBuf> = workspace
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    let projects = load_projects().log_err().unwrap_or_default();
    roots
        .iter()
        .find_map(|root| project_for_path(&projects, root).cloned())
        .or_else(|| roots.iter().find_map(|root| unregistered_project(root)))
}

fn activate(project: Option<AsylumProject>, cx: &mut App) {
    let current = active_project(cx);
    if current.as_ref().map(|project| &project.root)
        == project.as_ref().map(|project| &project.root)
    {
        return;
    }
    let Some(project) = project else {
        cx.set_global(ActiveAsylumProject::default());
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.set_asylum_project_settings(None, cx).log_err();
        });
        return;
    };
    let path = paths::asylum_project_settings_file(&project.root);
    cx.update_global(|store: &mut SettingsStore, _| {
        store.set_asylum_project_settings_path(Some(path.clone()));
    });
    let fs = <dyn Fs>::global(cx);
    let (mut contents, watcher) =
        settings::watch_config_file(cx.background_executor(), fs, path.clone());
    let watch = cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(content) = contents.next().await {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.set_asylum_project_settings_path(Some(path.clone()));
                if let Err(error) = store.set_asylum_project_settings(Some(&content), cx) {
                    log::error!("Failed to read {}: {error:#}", path.display());
                }
                cx.refresh_windows();
            });
        }
    });
    mark_opened(&project.id).log_err();
    cx.set_global(ActiveAsylumProject {
        project: Some(project),
        _watch: Some(watch),
    });
}

/// A project id from its name that no other project has, without accents or spaces.
fn project_id_for_name(name: &str, projects: &[AsylumProject]) -> String {
    let base: String = name
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| match character {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            character if character.is_ascii_alphanumeric() => character,
            _ => '-',
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() {
        "projeto".to_string()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut index = 2;
    while projects.iter().any(|project| project.id == candidate) {
        candidate = format!("{base}-{index}");
        index += 1;
    }
    candidate
}

/// The context servers profile `profile_id` defines. Read from its files because the settings
/// this process holds are those of the profile it runs as.
fn profile_context_servers(profile_id: &str) -> Result<Vec<String>> {
    let files = paths::shared_settings_file()
        .cloned()
        .into_iter()
        .chain([paths::profile_settings_file_for(profile_id)]);
    let mut servers = Vec::new();
    for file in files {
        let content = match std::fs::read_to_string(&file) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", file.display()));
            }
        };
        if content.trim().is_empty() {
            continue;
        }
        let settings: serde_json::Value = settings::parse_json_with_comments(&content)
            .with_context(|| format!("failed to parse {}", file.display()))?;
        if let Some(defined) = settings
            .get("context_servers")
            .and_then(serde_json::Value::as_object)
        {
            for server in defined.keys() {
                if !servers.contains(server) {
                    servers.push(server.clone());
                }
            }
        }
    }
    Ok(servers)
}

/// Registers the folder as a project and gives it a `.asylum/settings.json` when it has none.
fn create_project(request: NewProjectRequest) -> Result<AsylumProject> {
    let mut projects = load_projects()?;
    if let Some(existing) = projects.iter().find(|project| project.root == request.root) {
        anyhow::bail!(
            "{} is already the project {}",
            request.root.display(),
            existing.name
        );
    }
    let settings_file = paths::asylum_project_settings_file(&request.root);
    // A folder cloned with its `.asylum/settings.json` already says what it needs; the choices
    // made in the modal must not replace that.
    if !settings_file.exists() {
        if let Some(parent) = settings_file.parent() {
            std::fs::create_dir_all(parent).context("failed to create .asylum")?;
        }
        let mut json = serde_json::to_string_pretty(&project_settings(&request))?;
        json.push('\n');
        std::fs::write(&settings_file, json).context("failed to create the project settings")?;
    }
    let project = AsylumProject {
        id: project_id_for_name(&request.name, &projects),
        name: request.name,
        root: request.root,
        profile_id: request.profile_id,
        last_opened: Some(chrono::Utc::now().timestamp()),
    };
    projects.push(project.clone());
    save_projects(&projects)?;
    Ok(project)
}

fn open_project(project: &AsylumProject, window: &mut Window, cx: &mut App) {
    #[cfg(target_os = "macos")]
    if project.profile_id != paths::current_profile_id() {
        crate::zed::app_profiles::forward_paths(
            project.profile_id.clone(),
            vec![project.root.to_string_lossy().into_owned()],
            cx,
        )
        .detach_and_log_err(cx);
        return;
    }
    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    let paths = vec![project.root.clone()];
    cx.defer(move |cx| {
        if let Some(task) = handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
            })
            .log_err()
        {
            task.detach_and_log_err(cx);
        }
    });
}

#[derive(Clone)]
struct ProfileChoice {
    id: String,
    name: SharedString,
    color: Option<Hsla>,
}

/// The profiles a project can belong to, the current one always among them.
fn profile_choices() -> Vec<ProfileChoice> {
    #[cfg(target_os = "macos")]
    let mut choices: Vec<ProfileChoice> = crate::zed::app_profiles::load_profiles()
        .log_err()
        .unwrap_or_default()
        .into_iter()
        .map(|profile| ProfileChoice {
            id: profile.id,
            name: profile.name.into(),
            color: Some(profile.color.hsla()),
        })
        .collect();
    #[cfg(not(target_os = "macos"))]
    let mut choices: Vec<ProfileChoice> = Vec::new();
    let current = paths::current_profile_id();
    if !choices.iter().any(|choice| choice.id == current) {
        choices.insert(
            0,
            ProfileChoice {
                id: current.to_string(),
                name: current.to_string().into(),
                color: None,
            },
        );
    }
    choices
}

fn compact_path(path: &Path) -> SharedString {
    path.compact().to_string_lossy().into_owned().into()
}

fn render_labeled(label: SharedString, value: impl IntoElement) -> impl IntoElement {
    v_flex().gap_1p5().child(section_label(label)).child(value)
}

fn profile_dot(color: Hsla) -> impl IntoElement {
    div().flex_none().size(px(7.)).rounded_full().bg(color)
}

pub struct NewProjectModal {
    name: Entity<Editor>,
    root: Option<PathBuf>,
    /// The name filled in from the folder, replaced again when another folder is chosen unless
    /// the user typed their own.
    auto_name: Option<String>,
    profiles: Vec<ProfileChoice>,
    profile_id: String,
    findings: FolderFindings,
    enabled: DetectedTools,
    start: ProjectStart,
    error: Option<SharedString>,
    creating: bool,
    _create_task: Option<Task<()>>,
}

impl NewProjectModal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("zed.projects.name_placeholder"), window, cx);
            editor
        });
        cx.subscribe(&name, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        })
        .detach();
        Self {
            name,
            root: None,
            auto_name: None,
            profiles: profile_choices(),
            profile_id: paths::current_profile_id().to_string(),
            findings: FolderFindings::default(),
            enabled: DetectedTools::default(),
            start: ProjectStart::InheritProfile,
            error: None,
            creating: false,
            _create_task: None,
        }
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The system dialog rather than `Workspace::prompt_for_open_path`, whose in-app prompt
        // is a modal of its own and would close this one.
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("zed.projects.choose_folder")),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(root) = chosen.await??.and_then(|paths| paths.into_iter().next()) else {
                return anyhow::Ok(());
            };
            let findings = cx
                .background_spawn({
                    let root = root.clone();
                    async move { find_in_folder(&root) }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_root(root, findings, window, cx)
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn set_root(
        &mut self,
        root: PathBuf,
        findings: FolderFindings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let folder_name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let typed = self.name.read(cx).text(cx);
        if typed.trim().is_empty() || self.auto_name.as_deref() == Some(typed.as_str()) {
            self.name.update(cx, |editor, cx| {
                editor.set_text(folder_name.clone(), window, cx)
            });
            self.auto_name = Some(folder_name);
        }
        self.root = Some(root);
        self.enabled = findings.tools();
        self.findings = findings;
        self.error = None;
        cx.notify();
    }

    fn can_create(&self, cx: &App) -> bool {
        !self.creating && self.root.is_some() && !self.name.read(cx).text(cx).trim().is_empty()
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_create(cx) {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        if let Some(existing) = load_projects()
            .log_err()
            .unwrap_or_default()
            .into_iter()
            .find(|project| project.root == root)
        {
            self.error = Some(t!("zed.projects.already_project", name = existing.name));
            cx.notify();
            return;
        }
        let current_profile_servers = (self.profile_id == paths::current_profile_id()).then(|| {
            ProjectSettings::get_global(cx)
                .context_servers
                .keys()
                .map(|server| server.to_string())
                .collect::<Vec<_>>()
        });
        let mut request = NewProjectRequest {
            root,
            name: self.name.read(cx).text(cx).trim().to_string(),
            profile_id: self.profile_id.clone(),
            tools: self.enabled,
            start: self.start,
            profile_servers: Vec::new(),
            profile_toolkits: task_agents::toolkits(cx)
                .iter()
                .map(|toolkit| toolkit.id.to_string())
                .collect(),
        };
        self.creating = true;
        self.error = None;
        cx.notify();
        self._create_task = Some(cx.spawn_in(window, async move |this, cx| {
            let created = cx
                .background_spawn(async move {
                    request.profile_servers = match current_profile_servers {
                        Some(servers) => servers,
                        None if request.start == ProjectStart::Empty => {
                            profile_context_servers(&request.profile_id)?
                        }
                        None => Vec::new(),
                    };
                    create_project(request)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.creating = false;
                match created {
                    Ok(project) => {
                        open_project(&project, window, cx);
                        cx.emit(DismissEvent);
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_found_row(
        id: &'static str,
        icon: IconName,
        title: SharedString,
        detail: SharedString,
        turns_on: SharedString,
        on: bool,
        toggle: fn(&mut DetectedTools),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .w_full()
            .h(px(44.))
            .px(px(10.))
            .gap(px(10.))
            .rounded(Radius::Md.rems())
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                toggle(&mut this.enabled);
                cx.notify();
            }))
            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_px()
                    .child(Label::new(title).size(LabelSize::Small).truncate())
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                    ),
            )
            .child(
                Label::new(turns_on)
                    .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                    .color(Color::Muted),
            )
            .child(toggle_switch(
                SharedString::from(format!("{id}-switch")),
                on,
                true,
                cx,
            ))
    }

    fn render_found_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        if let Some(repo) = &self.findings.repo {
            let title = if repo.nested_count > 1 {
                t!("zed.projects.found_repositories", count = repo.nested_count)
            } else {
                t!("zed.projects.found_repository")
            };
            let detail = [
                repo.branch.clone(),
                repo.origin.clone().map(|origin| format!("origin {origin}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            rows.push(
                Self::render_found_row(
                    "new-project-found-repo",
                    IconName::GitBranch,
                    title,
                    detail.into(),
                    t!("zed.projects.found_repo_turns_on"),
                    self.enabled.repo,
                    |tools| tools.repo = !tools.repo,
                    cx,
                )
                .into_any_element(),
            );
        }
        if let Some(expo) = &self.findings.expo {
            let mut detail = expo.files.join(" + ");
            if let Some(bundle_id) = &expo.bundle_id {
                detail.push_str(" · ");
                detail.push_str(bundle_id);
            }
            rows.push(
                Self::render_found_row(
                    "new-project-found-releases",
                    IconName::Smartphone,
                    t!("zed.projects.found_expo"),
                    detail.into(),
                    t!("zed.projects.found_releases_turns_on"),
                    self.enabled.releases,
                    |tools| tools.releases = !tools.releases,
                    cx,
                )
                .into_any_element(),
            );
        }
        if let Some(compose) = &self.findings.compose {
            let detail = if compose.services.is_empty() {
                SharedString::default()
            } else {
                let names = compose.services.join(", ");
                match compose.services.len() {
                    1 => t!("zed.projects.found_service", names = names),
                    count => t!("zed.projects.found_services", count = count, names = names),
                }
            };
            rows.push(
                Self::render_found_row(
                    "new-project-found-docker",
                    IconName::Docker,
                    compose.file_name.into(),
                    detail,
                    t!("zed.projects.found_docker_turns_on"),
                    self.enabled.docker,
                    |tools| tools.docker = !tools.docker,
                    cx,
                )
                .into_any_element(),
            );
        }
        rows
    }

    fn render_start_card(
        &self,
        start: ProjectStart,
        profile_name: &SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (id, title, detail) = match start {
            ProjectStart::InheritProfile => (
                "new-project-inherit",
                t!("zed.projects.start_inherit"),
                t!("zed.projects.start_inherit_detail", profile = profile_name),
            ),
            ProjectStart::Empty => (
                "new-project-empty",
                t!("zed.projects.start_empty"),
                t!("zed.projects.start_empty_detail"),
            ),
        };
        option_card(id, title, detail, self.start == start, cx).on_click(cx.listener(
            move |this, _, _, cx| {
                this.start = start;
                cx.notify();
            },
        ))
    }
}

impl EventEmitter<DismissEvent> for NewProjectModal {}
impl ModalView for NewProjectModal {}

impl Focusable for NewProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for NewProjectModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name_focus = self.name.focus_handle(cx);
        let name_focused = name_focus.is_focused(window);
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == self.profile_id);
        let profile_name = profile
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| self.profile_id.clone().into());
        let profile_color = profile.and_then(|profile| profile.color);
        let modal = cx.entity().downgrade();
        let profile_menu = ContextMenu::build(window, cx, {
            let profiles = self.profiles.clone();
            let current = self.profile_id.clone();
            move |mut menu, _window, _cx| {
                for profile in profiles {
                    let modal = modal.clone();
                    let toggled = profile.id == current;
                    let id = profile.id;
                    menu = menu.toggleable_entry(
                        profile.name,
                        toggled,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            let id = id.clone();
                            modal
                                .update(cx, |modal, cx| {
                                    modal.profile_id = id;
                                    cx.notify();
                                })
                                .log_err();
                        },
                    );
                }
                menu
            }
        });
        let folder = match &self.root {
            Some(root) => Label::new(compact_path(root))
                .size(LabelSize::Small)
                .truncate(),
            None => Label::new(t!("zed.projects.no_folder"))
                .size(LabelSize::Small)
                .color(Color::Placeholder),
        };
        let found_rows = self.render_found_rows(cx);
        let can_create = self.can_create(cx);

        modal_surface(cx)
            .key_context("AsylumNewProject")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(px(600.))
            .p(px(22.))
            .gap(px(18.))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(t!("zed.projects.new")).weight(FontWeight::BOLD)),
                    )
                    .child(
                        IconButton::new("new-project-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(render_labeled(
                t!("zed.projects.field_folder"),
                h_flex()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(
                            text_field(false, cx)
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(folder),
                        ),
                    )
                    .child(
                        secondary_button(
                            "new-project-choose-folder",
                            None,
                            t!("zed.projects.choose"),
                            true,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.choose_folder(window, cx)),
                        ),
                    ),
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(render_labeled(
                        t!("zed.projects.field_name"),
                        text_field(name_focused, cx).child(div().flex_1().child(self.name.clone())),
                    )))
                    .child(
                        div().flex_1().min_w_0().child(render_labeled(
                            t!("zed.projects.field_profile"),
                            PopoverMenu::new("new-project-profile-menu")
                                .full_width(true)
                                .trigger(PopoverTriggerArea::new(select_field_with_leading(
                                    "new-project-profile",
                                    profile_color
                                        .map(|color| profile_dot(color).into_any_element()),
                                    profile_name.clone(),
                                    cx,
                                )))
                                .menu(move |_window, _cx| Some(profile_menu.clone())),
                        )),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label(t!("zed.projects.found_title")))
                    .child(grouped_block(cx).map(|block| {
                        if found_rows.is_empty() {
                            block.child(
                                div().px(px(10.)).py_2().child(
                                    Label::new(t!("zed.projects.found_none"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        } else {
                            block.children(found_rows)
                        }
                    })),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_label(t!("zed.projects.start_title")))
                    .child(
                        h_flex()
                            .items_stretch()
                            .gap_2()
                            .child(self.render_start_card(
                                ProjectStart::InheritProfile,
                                &profile_name,
                                cx,
                            ))
                            .child(self.render_start_card(ProjectStart::Empty, &profile_name, cx)),
                    ),
            )
            .child(info_note(t!("zed.projects.settings_note"), cx))
            .children(
                self.error
                    .clone()
                    .map(|error| error_callout(t!("zed.projects.error_title"), error, cx)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        secondary_button(
                            "new-project-cancel",
                            None,
                            t!("zed.projects.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        primary_button(
                            "new-project-create",
                            None,
                            t!("zed.projects.create"),
                            Some(KeyBinding::for_action_in(&menu::Confirm, &name_focus, cx)),
                            cx,
                        )
                        .when(!can_create, |button| button.opacity(0.5).cursor_default())
                        .when(can_create, |button| {
                            button.on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            }))
                        }),
                    ),
            )
    }
}

/// "Escolher projeto" at launch, or the welcome when there is no project yet.
pub struct ProjectChooser {
    focus_handle: FocusHandle,
    search: Entity<Editor>,
    projects: Vec<AsylumProject>,
    profiles: Vec<ProfileChoice>,
    matches: Vec<usize>,
    selected: usize,
    open_last_directly: bool,
    /// Each row's "2 repositórios · main · há 2 h", by project id; read from disk after opening.
    summaries: std::collections::HashMap<String, SharedString>,
    _summaries_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectChooser {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let registry = load_registry().log_err().unwrap_or_default();
        let mut profiles = profile_choices();
        let current = paths::current_profile_id();
        if let Some(position) = profiles.iter().position(|profile| profile.id == current) {
            let profile = profiles.remove(position);
            profiles.insert(0, profile);
        }
        let mut projects: Vec<AsylumProject> = registry
            .projects
            .into_iter()
            .filter(|project| project.root.is_dir())
            .collect();
        for project in &projects {
            if !profiles
                .iter()
                .any(|profile| profile.id == project.profile_id)
            {
                profiles.push(ProfileChoice {
                    id: project.profile_id.clone(),
                    name: project.profile_id.clone().into(),
                    color: None,
                });
            }
        }
        let group_of = |project: &AsylumProject| {
            profiles
                .iter()
                .position(|profile| profile.id == project.profile_id)
                .unwrap_or(profiles.len())
        };
        projects.sort_by(|left, right| {
            group_of(left)
                .cmp(&group_of(right))
                .then(right.last_opened.cmp(&left.last_opened))
        });
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("zed.projects.search"), window, cx);
            editor
        });
        let subscriptions = vec![cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.update_matches(cx);
            }
        })];
        let matches = (0..projects.len()).collect();
        let summaries_task = cx.spawn({
            let projects = projects.clone();
            async move |this, cx| {
                let summaries = cx
                    .background_spawn(async move {
                        let now = chrono::Utc::now().timestamp();
                        projects
                            .iter()
                            .map(|project| (project.id.clone(), project_summary(project, now)))
                            .collect()
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.summaries = summaries;
                    cx.notify();
                })
                .log_err();
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            search,
            projects,
            profiles,
            matches,
            selected: 0,
            open_last_directly: registry.open_last_directly,
            summaries: Default::default(),
            _summaries_task: summaries_task,
            _subscriptions: subscriptions,
        }
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        self.matches = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                query.is_empty()
                    || project.name.to_lowercase().contains(&query)
                    || project
                        .root
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        cx.notify();
    }

    fn open_at(&mut self, position: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self
            .matches
            .get(position)
            .and_then(|index| self.projects.get(*index))
            .cloned()
        else {
            return;
        };
        open_project(&project, window, cx);
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_at(self.selected, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn toggle_open_last(&mut self, cx: &mut Context<Self>) {
        self.open_last_directly = !self.open_last_directly;
        let open_last_directly = self.open_last_directly;
        cx.background_spawn(async move { set_open_last_directly(open_last_directly) })
            .detach_and_log_err(cx);
        cx.notify();
    }

    fn new_project_button(cx: &mut Context<Self>) -> impl IntoElement {
        secondary_button(
            "project-chooser-new",
            None,
            t!("zed.projects.new_ellipsis"),
            true,
            cx,
        )
        .on_click(|_, window, cx| window.dispatch_action(Box::new(NewProject), cx))
    }

    fn open_loose_folder_button(cx: &mut Context<Self>) -> impl IntoElement {
        Button::new(
            "project-chooser-open-folder",
            t!("zed.projects.open_loose_folder"),
        )
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .on_click(cx.listener(|_, _, window, cx| {
            window.dispatch_action(Box::new(workspace::Open::default()), cx);
            cx.emit(DismissEvent);
        }))
    }

    fn render_row(
        &self,
        position: usize,
        project: &AsylumProject,
        other_profile: Option<&SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let summary = match other_profile {
            Some(profile) => Some(t!("zed.projects.other_profile", profile = profile)),
            None => self.summaries.get(&project.id).cloned(),
        };
        h_flex()
            .id(("project-chooser-row", position))
            .w_full()
            .h(px(48.))
            .px(px(10.))
            .gap(px(10.))
            .rounded(px(7.))
            .cursor_pointer()
            .map(|this| {
                if position == self.selected {
                    this.bg(accent.opacity(0.16))
                } else {
                    this.hover(|this| this.bg(colors.element_hover))
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_at(position, window, cx)))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .size(px(28.))
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .bg(colors.element_hover)
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_px()
                    .child(
                        Label::new(project.name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        Label::new(compact_path(&project.root))
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                            .truncate(),
                    ),
            )
            .children(
                summary
                    .filter(|summary| !summary.is_empty())
                    .map(|summary| {
                        Label::new(summary)
                            .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                            .color(Color::Muted)
                    }),
            )
            .when(position < 9, |this| {
                this.child(
                    KeyBinding::for_action_in(
                        &workspace::ActivatePane(position),
                        &self.search.focus_handle(cx),
                        cx,
                    )
                    .size(rems_from_px(11_f32)),
                )
            })
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        modal_surface(cx).w(px(480.)).child(
            EmptyState::new(IconName::Folder, t!("zed.projects.welcome_title"))
                .description(t!("zed.projects.welcome_description"))
                .action(
                    primary_button(
                        "project-chooser-welcome-new",
                        Some(IconName::Plus),
                        t!("zed.projects.new"),
                        None,
                        cx,
                    )
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewProject), cx)),
                )
                .action(
                    secondary_button(
                        "project-chooser-welcome-open-folder",
                        Some((IconName::FolderOpen, Color::Muted)),
                        t!("zed.projects.open_loose_folder"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(workspace::Open::default()), cx);
                        cx.emit(DismissEvent);
                    })),
                ),
        )
    }

    fn render_chooser(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_focused = self.search.focus_handle(cx).is_focused(window);
        let current = paths::current_profile_id();
        let mut groups: Vec<(String, Vec<(usize, &AsylumProject)>)> = Vec::new();
        for (position, index) in self.matches.iter().enumerate() {
            let Some(project) = self.projects.get(*index) else {
                continue;
            };
            match groups.last_mut() {
                Some((profile_id, rows)) if *profile_id == project.profile_id => {
                    rows.push((position, project))
                }
                _ => groups.push((project.profile_id.clone(), vec![(position, project)])),
            }
        }
        let mut sections: Vec<AnyElement> = Vec::new();
        for (profile_id, rows) in groups {
            let profile = self
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id);
            let profile_name: SharedString = profile
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| profile_id.clone().into());
            let profile_color = profile
                .and_then(|profile| profile.color)
                .unwrap_or(cx.theme().colors().text_muted);
            let other_profile = (profile_id != current).then(|| profile_name.clone());
            sections.push(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .px_1()
                            .gap_1p5()
                            .child(profile_dot(profile_color))
                            .child(section_label(profile_name.to_uppercase())),
                    )
                    .child(grouped_block(cx).children(rows.into_iter().map(
                        |(position, project)| {
                            self.render_row(position, project, other_profile.as_ref(), cx)
                                .into_any_element()
                        },
                    )))
                    .into_any_element(),
            );
        }
        let query = self.search.read(cx).text(cx);

        v_flex()
            .w(px(640.))
            .gap_5()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(t!("zed.projects.chooser_title"))
                            .size(LabelSize::Custom(rems_from_px(20_f32)))
                            .weight(FontWeight::BOLD),
                    )
                    .child(
                        Label::new(t!("zed.projects.chooser_description"))
                            .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                            .color(Color::Muted),
                    ),
            )
            .child(
                text_field(search_focused, cx)
                    .h(px(32.))
                    .gap_2()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().child(self.search.clone())),
            )
            .child(
                v_flex()
                    .id("project-chooser-list")
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .gap_5()
                    .map(|list| {
                        if sections.is_empty() {
                            list.child(
                                Label::new(t!("zed.projects.no_matches", query = query.trim()))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        } else {
                            list.children(sections)
                        }
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Self::new_project_button(cx))
                    .child(Self::open_loose_folder_button(cx))
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .id("project-chooser-open-last")
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_open_last(cx)))
                            .child(checkbox_indicator(self.open_last_directly, cx))
                            .child(
                                Label::new(t!("zed.projects.open_last"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
    }
}

impl EventEmitter<DismissEvent> for ProjectChooser {}
impl ModalView for ProjectChooser {}

impl Focusable for ProjectChooser {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The welcome has no search field, and focusing one that isn't drawn would count as
        // focus leaving the modal and close it.
        if self.projects.is_empty() {
            self.focus_handle.clone()
        } else {
            self.search.focus_handle(cx)
        }
    }
}

impl Render for ProjectChooser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.projects.is_empty() {
            self.render_welcome(cx).into_any_element()
        } else {
            self.render_chooser(window, cx).into_any_element()
        };
        div()
            .key_context("AsylumProjectChooser")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            // ⌘1–9 are bound to `ActivatePane` in the workspace; handling it here, below the
            // workspace, opens the nth project instead while the chooser is up.
            .on_action(
                cx.listener(|this, action: &workspace::ActivatePane, window, cx| {
                    this.open_at(action.0, window, cx)
                }),
            )
            .child(body)
    }
}

pub struct ProjectPickerDelegate {
    projects: Vec<AsylumProject>,
    matches: Vec<usize>,
    selected_index: usize,
    active_root: Option<PathBuf>,
}

impl ProjectPickerDelegate {
    fn new(cx: &App) -> Self {
        let mut projects = load_projects().log_err().unwrap_or_default();
        projects.sort_by(|left, right| {
            (left.profile_id != paths::current_profile_id())
                .cmp(&(right.profile_id != paths::current_profile_id()))
                .then(right.last_opened.cmp(&left.last_opened))
        });
        let matches = (0..projects.len()).collect();
        Self {
            projects,
            matches,
            selected_index: 0,
            active_root: active_project(cx).map(|project| project.root),
        }
    }
}

impl PickerDelegate for ProjectPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "asylum projects"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        t!("zed.projects.search").to_string().into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                query.is_empty()
                    || project.name.to_lowercase().contains(&query)
                    || project
                        .root
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(project) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.projects.get(*index))
            .cloned()
        else {
            return;
        };
        open_project(&project, window, cx);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(t!("zed.projects.none"))
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let project = self.projects.get(*self.matches.get(ix)?)?;
        let is_active = self.active_root.as_ref() == Some(&project.root);
        let other_profile = project.profile_id != paths::current_profile_id();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(if is_active {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(Label::new(project.name.clone()))
                                .when(is_active, |this| {
                                    this.child(
                                        Label::new(t!("zed.projects.active"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    )
                                })
                                .when(other_profile, |this| {
                                    this.child(
                                        Label::new(project.profile_id.clone())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .child(
                            Label::new(project.root.to_string_lossy().into_owned())
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder)
                                .truncate(),
                        ),
                ),
        )
    }
}

pub struct ProjectPicker {
    picker: Entity<Picker<ProjectPickerDelegate>>,
}

impl ProjectPicker {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = ProjectPickerDelegate::new(cx);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent))
            .detach();
        Self { picker }
    }
}

impl gpui::EventEmitter<DismissEvent> for ProjectPicker {}
impl ModalView for ProjectPicker {}

impl gpui::Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone()).child(
            h_flex()
                .p_1p5()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("projects-new", t!("zed.projects.new"))
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(NewProject), cx)),
                ),
        )
    }
}

/// The first empty window of a launch goes straight to the only project (or the last one, when
/// the user asked for that), and otherwise shows "Escolher projeto", or the welcome when there is
/// no project yet. Windows restored with folders are left alone.
fn offer_projects_on_launch(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    static OFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if workspace.visible_worktrees(cx).next().is_some()
        || OFFERED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let registry = load_registry().log_err().unwrap_or_default();
    let projects: Vec<AsylumProject> = registry
        .projects
        .into_iter()
        .filter(|project| project.root.is_dir())
        .collect();
    let direct = match projects.as_slice() {
        [project] => Some(project.clone()),
        _ if registry.open_last_directly => projects
            .iter()
            .filter(|project| project.last_opened.is_some())
            .max_by_key(|project| project.last_opened)
            .cloned(),
        _ => None,
    };
    match direct {
        Some(project) => cx.defer_in(window, move |_, window, cx| {
            open_project(&project, window, cx)
        }),
        None => cx.defer_in(window, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, ProjectChooser::new);
        }),
    }
}

pub fn init(cx: &mut App) {
    migrate_profile_folders().log_err();
    cx.set_global(ActiveAsylumProject::default());
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &OpenProject, window, cx| {
            workspace.toggle_modal(window, cx, ProjectPicker::new);
        });
        workspace.register_action(|workspace, _: &NewProject, window, cx| {
            workspace.hide_modal(window, cx);
            workspace.toggle_modal(window, cx, NewProjectModal::new);
        });
        let Some(window) = window else {
            return;
        };
        offer_projects_on_launch(workspace, window, cx);
        let project = project_for_workspace(workspace, cx);
        if window.is_window_active() || active_project(cx).is_none() {
            activate(project, cx);
        }
        cx.observe_window_activation(window, |workspace, window, cx| {
            if window.is_window_active() {
                let project = project_for_workspace(workspace, cx);
                activate(project, cx);
            }
        })
        .detach();
        cx.subscribe_in(
            &workspace.project().clone(),
            window,
            |workspace, _, event, window, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
                ) && window.is_window_active()
                {
                    let project = project_for_workspace(workspace, cx);
                    activate(project, cx);
                }
            },
        )
        .detach();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, root: &str) -> AsylumProject {
        AsylumProject {
            id: id.to_string(),
            name: id.to_string(),
            root: PathBuf::from(root),
            profile_id: "trabalho".to_string(),
            last_opened: None,
        }
    }

    #[test]
    fn profile_folders_become_projects_once_each() {
        let projects = projects_from_profile_folders(vec![
            (
                "trabalho".to_string(),
                vec![PathBuf::from("/p/Trx"), PathBuf::from("/p/api")],
            ),
            ("pessoal".to_string(), vec![PathBuf::from("/p/Trx")]),
        ]);
        let summary: Vec<(&str, &str)> = projects
            .iter()
            .map(|project| (project.id.as_str(), project.profile_id.as_str()))
            .collect();
        assert_eq!(summary, vec![("trx", "trabalho"), ("api", "trabalho")]);
    }

    #[test]
    fn project_ids_are_plain_and_unique() {
        let taken = vec![project("trix", "/a")];
        assert_eq!(project_id_for_name("Trix", &taken), "trix-2");
        assert_eq!(
            project_id_for_name("Organizadin Família", &[]),
            "organizadin-familia"
        );
        assert_eq!(project_id_for_name("***", &[]), "projeto");
    }

    #[test]
    fn deepest_project_claims_the_path() {
        let projects = vec![
            project("trx", "/Users/dev/Projects/Trx"),
            project("app", "/Users/dev/Projects/Trx/trix_frontend_app"),
        ];
        let found = project_for_path(
            &projects,
            Path::new("/Users/dev/Projects/Trx/trix_frontend_app/src"),
        );
        assert_eq!(found.map(|project| project.id.as_str()), Some("app"));
        let found = project_for_path(&projects, Path::new("/Users/dev/Projects/Trx/backend"));
        assert_eq!(found.map(|project| project.id.as_str()), Some("trx"));
        assert!(project_for_path(&projects, Path::new("/Users/dev/Other")).is_none());
    }
}
