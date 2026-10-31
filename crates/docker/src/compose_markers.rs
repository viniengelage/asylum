//! The state of each service at the end of its line in docker-compose.yml: "● rodando · :3000 ·
//! 0,4%". Clicking one focuses the container in the dock.

use crate::{
    DockerPanel,
    client::{Container, ContainerState},
    format_percent,
    store::DockerStore,
};
use collections::HashSet;
use editor::{
    Addon, Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock},
};
use gpui::{
    App, AppContext as _, Context, Entity, MouseButton, Subscription, Task, WeakEntity, px,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::prelude::*;
use util::ResultExt as _;

/// Edits settle this long before the markers move, so typing doesn't reflow them each key.
const REFRESH_DELAY: Duration = Duration::from_millis(300);

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
        let rows = match &path {
            Some(_) => service_rows(&editor.read(cx).text(cx)),
            None => Vec::new(),
        };
        if path.is_some() && self.store_subscription.is_none() {
            let store = DockerStore::global(cx);
            let editor = self.editor.clone();
            // Stats change every couple of seconds; the markers read them while drawing.
            self.store_subscription = Some(cx.observe(&store, move |_, _, cx| {
                editor.update(cx, |_, cx| cx.notify()).ok();
            }));
        }
        if rows == self.rows && !self.blocks.is_empty() {
            return;
        }
        self.rows = rows.clone();

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
                let line_end = snapshot.line_len(multi_buffer::MultiBufferRow(row));
                BlockProperties {
                    placement: BlockPlacement::Near(
                        snapshot.anchor_after(language::Point::new(row, line_end)),
                    ),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: render_marker(service, directory.clone(), editor_handle.clone()),
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
            && (container
                .compose_working_dir()
                .is_some_and(|dir| dir == directory)
                || container
                    .compose_config_files()
                    .iter()
                    .any(|file| file.parent() == Some(directory)))
    })
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
            parts.push(container.health().unwrap_or("rodando").to_string());
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
            parts.push("reiniciando".into());
            if let Some(count) = store
                .restart_count(&container.id)
                .filter(|count| *count > 0)
            {
                parts.push(format!("{count}×"));
            }
            Color::Error
        }
        ContainerState::Paused => {
            parts.push("pausado".into());
            Color::Warning
        }
        ContainerState::Exited | ContainerState::Dead => {
            parts.push("parado".into());
            if let Some(code) = container.exit_code() {
                parts.push(format!("saiu ({code})"));
            }
            Color::Placeholder
        }
        ContainerState::Created | ContainerState::Removing => {
            parts.push(container.state.label().into());
            Color::Placeholder
        }
    };
    (color, parts.join(" · "))
}

fn render_marker(service: String, directory: Arc<Path>, editor: WeakEntity<Editor>) -> RenderBlock {
    Arc::new(move |cx| {
        let Some(store) = DockerStore::try_global(cx.app) else {
            return div().into_any_element();
        };
        let store = store.read(cx.app);
        let Some(container) = container_for(store, &directory, &service) else {
            return div().into_any_element();
        };
        let (color, label) = marker_text(container, store);
        let container_id = container.id.clone();
        let colors = cx.app.theme().colors();
        let font = cx.editor_style.text.font();
        let font_size = cx
            .editor_style
            .text
            .font_size
            .to_pixels(cx.window.rem_size())
            * 0.78;
        let dot_color = match color {
            Color::Placeholder => colors.border_selected,
            color => color.color(cx.app),
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
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .cursor_pointer()
                    .hover(|this| this.bg(colors.ghost_element_hover))
                    .font(font)
                    .text_size(font_size)
                    .text_color(colors.text_placeholder)
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
    container_id: String,
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
        if let Some(panel) = workspace.panel::<DockerPanel>(cx) {
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
}
