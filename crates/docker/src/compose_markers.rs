//! The state of each service at the end of its line in docker-compose.yml: "● running · :3000 ·
//! 0.4%", or what "Subir" would do to it while nothing runs ("vai subir · build", "fora · profile
//! mail"). Clicking one focuses the container in the dock. Also the compose file model the dock's
//! "subir compose" card and the agent read.

use crate::{
    DockerPanel,
    client::{Container, ContainerState, health_label},
    format_percent,
    store::{ComposeSelection, DockerStore},
};
use anyhow::Context as _;
use collections::{HashMap, HashSet};
use editor::{
    Addon, Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock},
};
use gpui::{
    App, AppContext as _, Context, Entity, Hsla, MouseButton, Subscription, Task, WeakEntity, px,
};
use i18n::t;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::prelude::*;
use util::ResultExt as _;

/// Edits settle this long before the markers move, so typing doesn't reflow them each key.
const REFRESH_DELAY: Duration = Duration::from_millis(300);

/// In the order `docker compose` itself prefers them.
const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || !editor.mode().is_full() {
            return;
        }
        let editor_entity = cx.entity();
        let markers = cx.new(|cx| ComposeMarkers::new(editor_entity, cx));
        editor.register_addon(ComposeMarkersAddon { _markers: markers });
    })
    .detach();
}

pub(crate) fn is_compose_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    (name.starts_with("docker-compose") || name.starts_with("compose"))
        && (name.ends_with(".yml") || name.ends_with(".yaml"))
}

/// The services of a compose file and the row of each key.
pub(crate) fn service_rows(text: &str) -> Vec<(u32, String)> {
    let mut rows = Vec::new();
    let mut in_services = false;
    let mut service_indent = None;
    for (row, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_services = line.trim_end() == "services:";
            service_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        let indent_of_services = *service_indent.get_or_insert(indent);
        if indent != indent_of_services {
            continue;
        }
        let Some(key) = line.trim().strip_suffix(':') else {
            continue;
        };
        let key = key.trim_matches(['"', '\'']);
        if !key.is_empty() && !key.contains(' ') {
            rows.push((row as u32, key.to_string()));
        }
    }
    rows
}

/// The compose file `docker compose` would pick in `root`.
pub(crate) fn compose_file(root: &Path) -> Option<PathBuf> {
    COMPOSE_FILES
        .iter()
        .map(|file| root.join(file))
        .find(|path| path.is_file())
}

/// The `.env*` files next to the compose file, for "Arquivo de ambiente".
pub(crate) fn env_files(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| name.starts_with(".env"))
        .collect();
    files.sort();
    files
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeService {
    pub name: String,
    pub profiles: Vec<String>,
    pub builds: bool,
    pub has_image: bool,
    /// A volume with no name or host path, which `down` leaves behind detached: the next `up`
    /// starts empty.
    pub anonymous_volumes: bool,
}

impl ComposeService {
    /// The profile that keeps the service out of the selected profiles, if it is out.
    pub(crate) fn excluding_profile(&self, selection: &ComposeSelection) -> Option<String> {
        if self.profiles.is_empty() {
            return (!selection.default_profile)
                .then(|| t!("docker.compose_up.default_profile").to_string());
        }
        let included = self
            .profiles
            .iter()
            .any(|profile| selection.profiles.contains(profile));
        (!included)
            .then(|| self.profiles.first().cloned())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComposeProject {
    pub root: PathBuf,
    pub file: PathBuf,
    pub name: String,
    pub services: Vec<ComposeService>,
    pub profiles: Vec<String>,
}

impl ComposeProject {
    /// `None` when `root` has no compose file.
    pub(crate) fn load(root: &Path) -> Option<anyhow::Result<Self>> {
        let file = compose_file(root)?;
        Some(
            std::fs::read_to_string(&file)
                .map_err(anyhow::Error::from)
                .and_then(|text| Self::parse(root, &file, &text)),
        )
    }

    pub(crate) fn parse(root: &Path, file: &Path, text: &str) -> anyhow::Result<Self> {
        let document: serde_yaml::Value = serde_yaml::from_str(text)?;
        let name = document
            .get("name")
            .and_then(|name| name.as_str())
            .map(str::to_string)
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            })
            .unwrap_or_default();
        let services: Vec<ComposeService> = document
            .get("services")
            .and_then(|services| services.as_mapping())
            .context("no services")?
            .iter()
            .filter_map(|(name, spec)| {
                Some(ComposeService {
                    name: name.as_str()?.to_string(),
                    profiles: spec
                        .get("profiles")
                        .and_then(|profiles| profiles.as_sequence())
                        .map(|profiles| {
                            profiles
                                .iter()
                                .filter_map(|profile| profile.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    builds: spec.get("build").is_some(),
                    has_image: spec.get("image").is_some(),
                    anonymous_volumes: spec
                        .get("volumes")
                        .and_then(|volumes| volumes.as_sequence())
                        .is_some_and(|volumes| volumes.iter().any(is_anonymous_volume)),
                })
            })
            .collect();
        let mut profiles: Vec<String> = services
            .iter()
            .flat_map(|service| service.profiles.iter().cloned())
            .collect();
        profiles.sort();
        profiles.dedup();
        Ok(Self {
            root: root.to_path_buf(),
            file: file.to_path_buf(),
            name,
            services,
            profiles,
        })
    }

    pub(crate) fn service(&self, name: &str) -> Option<&ComposeService> {
        self.services.iter().find(|service| service.name == name)
    }

    /// The services without a profile, under "padrão".
    pub(crate) fn default_selection(&self) -> ComposeSelection {
        ComposeSelection {
            default_profile: true,
            services: self
                .services
                .iter()
                .filter(|service| service.profiles.is_empty())
                .map(|service| service.name.clone())
                .collect(),
            ..ComposeSelection::default()
        }
    }

    /// The checked services in the file's order, leaving out ones the file no longer has.
    pub(crate) fn selected_services<'a>(&'a self, selection: &ComposeSelection) -> Vec<&'a str> {
        self.services
            .iter()
            .filter(|service| selection.services.contains(&service.name))
            .map(|service| service.name.as_str())
            .collect()
    }

    /// Turns a profile (`None` for "padrão") on or off, checking or unchecking its services.
    pub(crate) fn toggle_profile(&self, selection: &mut ComposeSelection, profile: Option<&str>) {
        let on = match profile {
            None => {
                selection.default_profile = !selection.default_profile;
                selection.default_profile
            }
            Some(profile) => {
                if selection.profiles.remove(profile) {
                    false
                } else {
                    selection.profiles.insert(profile.to_string());
                    true
                }
            }
        };
        for service in &self.services {
            let member = match profile {
                None => service.profiles.is_empty(),
                Some(profile) => service.profiles.iter().any(|item| item == profile),
            };
            if !member {
                continue;
            }
            if on {
                selection.services.insert(service.name.clone());
            } else {
                selection.services.remove(&service.name);
            }
        }
    }

    fn options(&self, selection: &ComposeSelection) -> Vec<String> {
        let mut options = Vec::new();
        if let Some(env_file) = &selection.env_file {
            options.extend(["--env-file".to_string(), env_file.clone()]);
        }
        for profile in &selection.profiles {
            if self.profiles.contains(profile) {
                options.extend(["--profile".to_string(), profile.clone()]);
            }
        }
        options
    }

    fn location(&self) -> Vec<String> {
        vec![
            "compose".into(),
            "--project-directory".into(),
            self.root.to_string_lossy().into_owned(),
            "-f".into(),
            self.file.to_string_lossy().into_owned(),
        ]
    }

    /// The arguments after `docker`, and the shorter command the card and the terminal show.
    pub(crate) fn up_command(&self, selection: &ComposeSelection) -> (Vec<String>, String) {
        let mut tail: Vec<String> = self.options(selection);
        tail.extend(["up".to_string(), "-d".to_string()]);
        if selection.rebuild {
            tail.push("--build".into());
        }
        tail.extend(
            self.selected_services(selection)
                .into_iter()
                .map(str::to_string),
        );
        let label = format!("docker compose {}", tail.join(" "));
        let mut args = self.location();
        args.extend(tail);
        (args, label)
    }

    /// `down` without `-v`: named volumes always stay.
    pub(crate) fn down_command(&self) -> (Vec<String>, String) {
        let mut args = self.location();
        args.push("down".into());
        (args, "docker compose down".into())
    }
}

fn is_anonymous_volume(volume: &serde_yaml::Value) -> bool {
    match volume {
        serde_yaml::Value::String(volume) => !volume.contains(':'),
        serde_yaml::Value::Mapping(_) => {
            volume
                .get("type")
                .and_then(|kind| kind.as_str())
                .is_none_or(|kind| kind == "volume")
                && volume.get("source").is_none()
        }
        _ => false,
    }
}

/// A `docker compose` run in a terminal tab, against the daemon the dock talks to.
pub(crate) fn compose_terminal_task(
    project: &ComposeProject,
    (args, command_label): (Vec<String>, String),
    socket: Option<&Path>,
) -> task::SpawnInTerminal {
    let mut env = HashMap::default();
    if let Some(socket) = socket {
        env.insert(
            "DOCKER_HOST".to_string(),
            format!("unix://{}", socket.display()),
        );
    }
    task::SpawnInTerminal {
        id: task::TaskId(format!("docker-compose-{}", project.root.display())),
        full_label: format!("compose · {}", project.name),
        label: format!("compose · {}", project.name),
        command: Some("docker".into()),
        args,
        command_label,
        cwd: Some(project.root.clone()),
        env,
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: task::RevealStrategy::Always,
        reveal_target: task::RevealTarget::Dock,
        hide: task::HideStrategy::Never,
        shell: task::Shell::System,
        show_summary: true,
        show_command: true,
        show_rerun: true,
        save: task::SaveStrategy::None,
    }
}

/// Ties the markers to the editor's lifetime.
struct ComposeMarkersAddon {
    _markers: Entity<ComposeMarkers>,
}

impl Addon for ComposeMarkersAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct ComposeMarkers {
    editor: WeakEntity<Editor>,
    blocks: HashSet<CustomBlockId>,
    rows: Vec<(u32, String)>,
    project: Option<ComposeProject>,
    store_subscription: Option<Subscription>,
    refresh_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ComposeMarkers {
    fn new(editor: Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if matches!(
                event,
                EditorEvent::BufferEdited | EditorEvent::Saved | EditorEvent::TitleChanged
            ) {
                this.schedule_refresh(cx);
            }
        })];
        let mut this = Self {
            editor: editor.downgrade(),
            blocks: HashSet::default(),
            rows: Vec::new(),
            project: None,
            store_subscription: None,
            refresh_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.schedule_refresh(cx);
        this
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DELAY).await;
            this.update(cx, |this, cx| this.refresh(cx)).log_err();
        });
    }

    fn compose_path(&self, cx: &App) -> Option<PathBuf> {
        let editor = self.editor.upgrade()?;
        let editor = editor.read(cx);
        let project = editor.project()?;
        if !task_agents::toolkit_enabled(task_agents::toolkit_ids::DOCKER, project, cx) {
            return None;
        }
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let file = buffer.read(cx).file()?;
        let path = file.as_local()?.abs_path(cx);
        is_compose_file(&path).then_some(path)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let path = self.compose_path(cx);
        let text = path.as_ref().map(|_| editor.read(cx).text(cx));
        let rows = text.as_deref().map(service_rows).unwrap_or_default();
        // Only the file `docker compose` picks in its directory is the one "Subir" runs.
        let project = path
            .as_deref()
            .zip(text.as_deref())
            .and_then(|(path, text)| {
                let root = path.parent()?;
                if compose_file(root).as_deref() != Some(path) {
                    return None;
                }
                ComposeProject::parse(root, path, text).ok()
            });
        if let Some(project) = &project {
            DockerStore::global(cx)
                .update(cx, |store, cx| store.load_compose_selection(project, cx));
        }
        if path.is_some() && self.store_subscription.is_none() {
            let store = DockerStore::global(cx);
            let editor = self.editor.clone();
            // Stats change every couple of seconds; the markers read them while drawing.
            self.store_subscription = Some(cx.observe(&store, move |_, _, cx| {
                editor.update(cx, |_, cx| cx.notify()).ok();
            }));
        }
        if rows == self.rows && project == self.project && !self.blocks.is_empty() {
            return;
        }
        self.rows = rows.clone();
        self.project = project.clone();

        let old_blocks = std::mem::take(&mut self.blocks);
        let editor_handle = self.editor.clone();
        let new_blocks = editor.update(cx, |editor, cx| {
            if !old_blocks.is_empty() {
                editor.remove_blocks(old_blocks, None, cx);
            }
            let Some(path) = path else {
                return Vec::new();
            };
            let directory: Arc<Path> = path.parent().unwrap_or(Path::new("/")).into();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let blocks = rows.into_iter().map(|(row, service)| {
                let planned = project
                    .as_ref()
                    .and_then(|project| project.service(&service))
                    .cloned();
                let line_end = snapshot.line_len(multi_buffer::MultiBufferRow(row));
                BlockProperties {
                    placement: BlockPlacement::Near(
                        snapshot.anchor_after(language::Point::new(row, line_end)),
                    ),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: render_marker(
                        service,
                        planned,
                        directory.clone(),
                        editor_handle.clone(),
                    ),
                    priority: 0,
                }
            });
            editor.insert_blocks(blocks, None, cx)
        });
        self.blocks = new_blocks.into_iter().collect();
    }
}

fn container_for<'a>(
    store: &'a DockerStore,
    directory: &Path,
    service: &str,
) -> Option<&'a Container> {
    store.containers().iter().find(|container| {
        container
            .labels
            .get(crate::client::COMPOSE_SERVICE_LABEL)
            .map(String::as_str)
            == Some(service)
            && belongs_to_compose(container, directory)
    })
}

fn belongs_to_compose(container: &Container, directory: &Path) -> bool {
    container
        .compose_working_dir()
        .is_some_and(|dir| dir == directory)
        || container
            .compose_config_files()
            .iter()
            .any(|file| file.parent() == Some(directory))
}

/// The marker's dot and text, from what the store knows now.
pub(crate) fn marker_text(container: &Container, store: &DockerStore) -> (Color, String) {
    let port = container
        .published_ports()
        .first()
        .map(|(public, _)| format!(":{public}"));
    let mut parts: Vec<String> = Vec::new();
    let color = match container.state {
        ContainerState::Running => {
            parts.push(
                container
                    .health()
                    .map(health_label)
                    .unwrap_or_else(|| t!("docker.state.running"))
                    .to_string(),
            );
            parts.extend(port);
            if container.health().is_none()
                && let Some(stats) = store.stats(&container.id)
            {
                parts.push(format_percent(stats.cpu_percent));
            }
            if container.health() == Some("unhealthy") {
                Color::Error
            } else {
                Color::Success
            }
        }
        ContainerState::Restarting => {
            parts.push(t!("docker.state.restarting").to_string());
            if let Some(count) = store
                .restart_count(&container.id)
                .filter(|count| *count > 0)
            {
                parts.push(format!("{count}×"));
            }
            Color::Error
        }
        ContainerState::Paused => {
            parts.push(t!("docker.state.paused").to_string());
            Color::Warning
        }
        ContainerState::Exited | ContainerState::Dead => {
            parts.push(t!("docker.state.exited").to_string());
            if let Some(code) = container.exit_code() {
                parts.push(t!("docker.state.exited_with_code", code = code).to_string());
            }
            Color::Placeholder
        }
        ContainerState::Created | ContainerState::Removing => {
            parts.push(container.state.label().to_string());
            Color::Placeholder
        }
    };
    (color, parts.join(" · "))
}

/// What "Subir" would do to a service that has no container yet, from the dock's selection.
fn planned_text(
    service: &ComposeService,
    store: &DockerStore,
    directory: &Path,
) -> Option<(Color, String)> {
    // The plan only applies while nothing of this compose runs, the same state that shows "Subir".
    if store.containers().iter().any(|container| {
        container.state.is_running() && belongs_to_compose(container, directory)
    }) {
        return None;
    }
    let selection = store.compose_selection(directory)?;
    if selection.services.contains(&service.name) {
        let mut label = t!("docker.marker.will_start").to_string();
        if service.builds && (selection.rebuild || !service.has_image) {
            label.push_str(&format!(" · {}", t!("docker.marker.build")));
        }
        return Some((Color::Accent, label));
    }
    let profile = service.excluding_profile(selection)?;
    Some((
        Color::Placeholder,
        t!("docker.marker.excluded", profile = profile).to_string(),
    ))
}

fn render_marker(
    service: String,
    planned: Option<ComposeService>,
    directory: Arc<Path>,
    editor: WeakEntity<Editor>,
) -> RenderBlock {
    Arc::new(move |cx| {
        let Some(store) = DockerStore::try_global(cx.app) else {
            return div().into_any_element();
        };
        let store = store.read(cx.app);
        let container = container_for(store, &directory, &service);
        let (color, label) = match container {
            Some(container) => marker_text(container, store),
            None => match planned
                .as_ref()
                .and_then(|planned| planned_text(planned, store, &directory))
            {
                Some(planned) => planned,
                None => return div().into_any_element(),
            },
        };
        let container_id = container.map(|container| container.id.clone());
        let colors = cx.app.theme().colors();
        let font = cx.editor_style.text.font();
        let font_size = cx
            .editor_style
            .text
            .font_size
            .to_pixels(cx.window.rem_size())
            * 0.78;
        let (dot_color, text_color, border_color): (Hsla, Hsla, Hsla) = match color {
            Color::Placeholder => (
                colors.border_selected,
                colors.text_placeholder,
                colors.border,
            ),
            Color::Accent => (
                colors.border_focused,
                colors.text_accent,
                colors.border_focused.opacity(0.5),
            ),
            color => (color.color(cx.app), colors.text_placeholder, colors.border),
        };
        let editor = editor.clone();
        h_flex()
            .id(cx.block_id)
            .h(cx.line_height)
            .child(
                h_flex()
                    .id("docker-compose-marker")
                    .gap(px(6.))
                    .px(px(7.))
                    .py(px(1.))
                    .rounded_full()
                    .border_1()
                    .border_color(border_color)
                    .bg(colors.elevated_surface_background)
                    .cursor_pointer()
                    .hover(|this| this.bg(colors.ghost_element_hover))
                    .font(font)
                    .text_size(font_size)
                    .text_color(text_color)
                    .child(div().size(px(6.)).rounded_full().bg(dot_color))
                    .child(label)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        focus_in_dock(&editor, container_id.clone(), window, cx);
                    }),
            )
            .into_any_element()
    })
}

fn focus_in_dock(
    editor: &WeakEntity<Editor>,
    container_id: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor
        .upgrade()
        .and_then(|editor| editor.read(cx).workspace())
    else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        crate::open(workspace, window, cx);
        if let Some(container_id) = container_id
            && let Some(panel) = workspace.panel::<DockerPanel>(cx)
        {
            panel.update(cx, |panel, cx| panel.select(container_id, cx));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_service_keys() {
        let text = "services:\n  api:\n    build: ./api\n    ports: [\"3000:3000\"]\n  # comentário\n  worker:\n    command: pnpm worker\nvolumes:\n  pgdata:\n";
        assert_eq!(
            service_rows(text),
            vec![(1, "api".to_string()), (5, "worker".to_string())]
        );
        assert!(is_compose_file(Path::new("/p/docker-compose.yml")));
        assert!(is_compose_file(Path::new("/p/compose.override.yaml")));
        assert!(!is_compose_file(Path::new("/p/config.yml")));
    }

    #[test]
    fn reads_profiles_builds_and_anonymous_volumes() {
        let text = "services:\n  api:\n    build: ./api\n  postgres:\n    image: postgres:16\n    volumes:\n      - pgdata:/var/lib/postgresql/data\n  cache:\n    image: redis\n    volumes:\n      - /data\n  mail:\n    image: mailpit\n    profiles: [mail]\nvolumes:\n  pgdata:\n";
        let project =
            ComposeProject::parse(Path::new("/p/trix"), Path::new("/p/trix/compose.yml"), text)
                .expect("compose");
        assert_eq!(project.name, "trix");
        assert_eq!(project.profiles, vec!["mail".to_string()]);
        let api = project.service("api").expect("api");
        assert!(api.builds && !api.has_image);
        assert!(
            !project
                .service("postgres")
                .expect("postgres")
                .anonymous_volumes
        );
        assert!(project.service("cache").expect("cache").anonymous_volumes);

        let mut selection = project.default_selection();
        assert_eq!(
            project.selected_services(&selection),
            vec!["api", "postgres", "cache"]
        );
        let mail = project.service("mail").expect("mail");
        assert_eq!(mail.excluding_profile(&selection).as_deref(), Some("mail"));
        selection.rebuild = true;
        project.toggle_profile(&mut selection, Some("mail"));
        let (args, label) = project.up_command(&selection);
        assert_eq!(
            label,
            "docker compose --profile mail up -d --build api postgres cache mail"
        );
        assert_eq!(
            &args[..5],
            [
                "compose",
                "--project-directory",
                "/p/trix",
                "-f",
                "/p/trix/compose.yml"
            ]
        );
        project.toggle_profile(&mut selection, None);
        assert_eq!(project.selected_services(&selection), vec!["mail"]);
    }
}
