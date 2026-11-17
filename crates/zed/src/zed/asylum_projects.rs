use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, BorrowAppContext as _, Context, DismissEvent, Entity, Global, PathPromptOptions, Task,
    Window, actions,
};
use i18n::t;
use picker::{Picker, PickerDelegate};
use serde::{Deserialize, Serialize};
use settings::SettingsStore;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
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
}

pub fn load_projects() -> Result<Vec<AsylumProject>> {
    match std::fs::read(paths::asylum_projects_file()) {
        Ok(json) => Ok(serde_json::from_slice::<ProjectsRegistry>(&json)
            .context("failed to parse projects.json")?
            .projects),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("failed to read projects.json"),
    }
}

pub fn save_projects(projects: &[AsylumProject]) -> Result<()> {
    let path = paths::asylum_projects_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("failed to create the profiles directory")?;
    }
    let json = serde_json::to_vec_pretty(&ProjectsRegistry {
        projects: projects.to_vec(),
    })?;
    std::fs::write(&path, json).context("failed to write projects.json")
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

/// Registers `root` as a project of the current profile and gives it a `.asylum/settings.json`
/// when it has none.
pub fn create_project(root: PathBuf) -> Result<AsylumProject> {
    let mut projects = load_projects()?;
    if let Some(existing) = projects.iter().find(|project| project.root == root) {
        return Ok(existing.clone());
    }
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let settings_file = paths::asylum_project_settings_file(&root);
    if !settings_file.exists() {
        if let Some(parent) = settings_file.parent() {
            std::fs::create_dir_all(parent).context("failed to create .asylum")?;
        }
        std::fs::write(&settings_file, "{\n}\n")
            .context("failed to create the project settings")?;
    }
    let project = AsylumProject {
        id: project_id_for_name(&name, &projects),
        name,
        root,
        profile_id: paths::current_profile_id().to_string(),
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

fn new_project(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let chosen = workspace.prompt_for_open_path(
        PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("zed.projects.choose_folder")),
        },
        project::DirectoryLister::Local(
            workspace.project().clone(),
            workspace.app_state().fs.clone(),
        ),
        window,
        cx,
    );
    cx.spawn_in(window, async move |_, cx| {
        let Some(root) = chosen.await?.and_then(|paths| paths.into_iter().next()) else {
            return anyhow::Ok(());
        };
        let project = cx
            .background_spawn(async move { create_project(root) })
            .await?;
        cx.update(|window, cx| open_project(&project, window, cx))?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
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

/// The first empty window of a launch goes to the profile's project when there is one, or offers
/// the list when there are several. Windows restored with folders are left alone.
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
    let projects: Vec<AsylumProject> = load_projects()
        .log_err()
        .unwrap_or_default()
        .into_iter()
        .filter(|project| {
            project.profile_id == paths::current_profile_id() && project.root.is_dir()
        })
        .collect();
    match projects.as_slice() {
        [] => {}
        [project] => {
            let project = project.clone();
            cx.defer_in(window, move |_, window, cx| {
                open_project(&project, window, cx)
            });
        }
        _ => cx.defer_in(window, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, ProjectPicker::new);
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
            new_project(workspace, window, cx);
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
