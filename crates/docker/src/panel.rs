use crate::{
    ComposeDown, ComposeUp, FocusFilter, Inspect, NewContainer, OpenLogs, OpenTerminal, Prune,
    Refresh, Remove, Restart, ToggleFocus, TogglePause, ToggleStop,
    client::{COMPOSE_SERVICE_LABEL, Container, ContainerState, health_label},
    compose_markers::{self, ComposeProject},
    discovery, format_bytes, format_percent, logs_view, new_container,
    store::{
        ComposeSelection, Connection, ContainerAction, DockerStore, Group, GroupKind, StoreEvent,
    },
};
use collections::{HashMap, HashSet};
use editor::{Editor, EditorElement, EditorEvent, EditorStyle};
use gpui::{
    Action as _, AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable,
    FontStyle, FontWeight, PromptLevel, ScrollHandle, Subscription, TextStyle, WeakEntity, px,
    relative,
};
use i18n::t;
use project::Project;
use settings::Settings as _;
use std::path::{Path, PathBuf};
use theme_settings::ThemeSettings;
use ui::{
    CommonAnimationExt as _, ContextMenu, ContextMenuEntry, Divider, EmptyState, KeyBinding,
    PopoverMenu, PopoverMenuHandle, PopoverTriggerArea, RowStatus, StatusRow, StatusTone, Tooltip,
    button_group, capsule_button, card, checkbox_indicator, choice_chip, error_callout,
    group_icon_button, grouped_block, navigator_footer, prelude::*, primary_button,
    secondary_button, section_header, section_label, segment_option_with_count, segmented_control,
    select_field, status_pill, text_field, toggle_switch,
};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    All,
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    Images,
    Volumes,
    Networks,
}

pub struct DockerPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<DockerStore>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    segment: Segment,
    selected: Option<String>,
    collapsed_groups: HashSet<String>,
    open_sections: HashSet<Section>,
    project_root: Option<PathBuf>,
    groups: Vec<Group>,
    compose: Option<ComposeProject>,
    /// The compose file's name and why it didn't parse.
    compose_error: Option<(String, String)>,
    env_files: Vec<String>,
    compose_running: bool,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl DockerPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let store = DockerStore::global(cx);
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("docker.panel.filter_placeholder"), window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&store, |this, _, event, cx| match event {
                StoreEvent::ContainersChanged => this.regroup(cx),
            }),
            cx.subscribe(&filter_editor, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited { .. }) {
                    cx.notify();
                }
            }),
            cx.subscribe(&project, |this, _, event, cx| match event {
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                    this.project_root = project_root(&this.project, cx);
                    this.regroup(cx);
                    this.reload_compose(cx);
                }
                project::Event::WorktreeUpdatedEntries(_, changes) => {
                    let touches_compose = changes.iter().any(|(path, _, _)| {
                        path.components().count() == 1
                            && path.file_name().is_some_and(|name| {
                                name.starts_with(".env")
                                    || compose_markers::is_compose_file(Path::new(name))
                            })
                    });
                    if touches_compose {
                        this.reload_compose(cx);
                    }
                }
                _ => {}
            }),
        ];
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project_root: project_root(&project, cx),
            project,
            store,
            focus_handle: cx.focus_handle(),
            filter_editor,
            segment: Segment::All,
            selected: None,
            collapsed_groups: HashSet::default(),
            open_sections: HashSet::default(),
            groups: Vec::new(),
            compose: None,
            compose_error: None,
            env_files: Vec::new(),
            compose_running: false,
            menu_handle: PopoverMenuHandle::default(),
            scroll_handle: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.regroup(cx);
        this.reload_compose(cx);
        this
    }

    /// Selects the container and scrolls the list to it, e.g. from a compose marker.
    pub fn select(&mut self, container_id: String, cx: &mut Context<Self>) {
        let group_key = self
            .groups
            .iter()
            .find(|group| {
                group
                    .containers
                    .iter()
                    .any(|container| container.id == container_id)
            })
            .map(Group::key);
        if let Some(key) = group_key {
            self.collapsed_groups.remove(&key);
        }
        self.segment = Segment::All;
        self.selected = Some(container_id);
        cx.notify();
    }

    fn regroup(&mut self, cx: &mut Context<Self>) {
        self.groups = self.store.read(cx).groups(self.project_root.as_deref());
        if let Some(selected) = &self.selected
            && self.store.read(cx).container(selected).is_none()
        {
            self.selected = None;
        }
        cx.notify();
    }

    fn reload_compose(&mut self, cx: &mut Context<Self>) {
        let root = self.project_root.clone();
        self.env_files = root
            .as_deref()
            .map(compose_markers::env_files)
            .unwrap_or_default();
        match root.as_deref().and_then(ComposeProject::load) {
            None => {
                self.compose = None;
                self.compose_error = None;
            }
            Some(Ok(project)) => {
                self.store
                    .update(cx, |store, cx| store.load_compose_selection(&project, cx));
                self.compose = Some(project);
                self.compose_error = None;
            }
            Some(Err(error)) => {
                let file = root
                    .as_deref()
                    .and_then(compose_markers::compose_file)
                    .and_then(|file| {
                        file.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                self.compose = None;
                self.compose_error = Some((file, error.to_string()));
            }
        }
        cx.notify();
    }

    fn compose_selection(&self, project: &ComposeProject, cx: &App) -> ComposeSelection {
        self.store
            .read(cx)
            .compose_selection(&project.root)
            .cloned()
            .unwrap_or_else(|| project.default_selection())
    }

    fn update_compose_selection(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&ComposeProject, &mut ComposeSelection),
    ) {
        let Some(project) = self.compose.clone() else {
            return;
        };
        let mut selection = self.compose_selection(&project, cx);
        change(&project, &mut selection);
        self.store.update(cx, |store, cx| {
            store.set_compose_selection(project.root.clone(), selection, cx)
        });
    }

    fn project_compose_running(&self) -> bool {
        self.groups.iter().any(|group| {
            matches!(group.kind, GroupKind::Compose { .. })
                && group
                    .containers
                    .iter()
                    .any(|container| container.state.is_running())
        })
    }

    fn compose_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.compose.clone() else {
            return;
        };
        if self.compose_running {
            return;
        }
        let selection = self.compose_selection(&project, cx);
        if project.selected_services(&selection).is_empty() {
            self.toast("compose", t!("docker.compose_up.no_services"), cx);
            return;
        }
        self.run_compose(
            &project,
            project.up_command(&selection),
            |code| t!("docker.compose_up.failed", code = code).to_string(),
            window,
            cx,
        );
    }

    fn confirm_compose_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.compose.clone() else {
            return;
        };
        if self.compose_running {
            return;
        }
        let created: HashSet<&str> = self
            .groups
            .iter()
            .filter(|group| matches!(group.kind, GroupKind::Compose { .. }))
            .flat_map(|group| &group.containers)
            .filter_map(|container| container.labels.get(COMPOSE_SERVICE_LABEL))
            .map(String::as_str)
            .collect();
        let losing: Vec<&str> = project
            .services
            .iter()
            .filter(|service| service.anonymous_volumes && created.contains(service.name.as_str()))
            .map(|service| service.name.as_str())
            .collect();
        let failed = |code| t!("docker.compose_down.failed", code = code).to_string();
        if losing.is_empty() {
            self.run_compose(&project, project.down_command(), failed, window, cx);
            return;
        }
        let confirm = t!("docker.compose_down.confirm");
        let cancel = t!("docker.remove.cancel");
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("docker.compose_down.title", project = project.name),
            Some(&t!(
                "docker.compose_down.detail",
                services = losing.join(", ")
            )),
            &[confirm.as_ref(), cancel.as_ref()],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update_in(cx, |this, window, cx| {
                    this.run_compose(&project, project.down_command(), failed, window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Runs `docker compose` in a terminal tab; the store sees the result through `/events`, so
    /// only a failed exit needs telling.
    fn run_compose(
        &mut self,
        project: &ComposeProject,
        command: (Vec<String>, String),
        failed: fn(String) -> String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let socket = self
            .store
            .read(cx)
            .docker()
            .map(|docker| docker.socket().to_path_buf());
        let task = compose_markers::compose_terminal_task(project, command, socket.as_deref());
        let Some(status) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.spawn_in_terminal(task, window, cx)
            })
            .log_err()
        else {
            return;
        };
        self.compose_running = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let status = status.await;
            this.update(cx, |this, cx| {
                this.compose_running = false;
                cx.notify();
                match status {
                    Some(Ok(status)) if !status.success() => {
                        let code = status
                            .code()
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "?".into());
                        this.toast("compose", failed(code), cx);
                    }
                    Some(Err(error)) => this.toast("compose", error.to_string(), cx),
                    _ => {}
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn selected_container(&self, cx: &App) -> Option<Container> {
        let id = self.selected.as_ref()?;
        self.store.read(cx).container(id).cloned()
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter_editor.read(cx).text(cx).trim().to_lowercase()
    }

    fn matches(&self, container: &Container, filter: &str) -> bool {
        let segment_ok = match self.segment {
            Segment::All => true,
            Segment::Running => container.state.is_running(),
            Segment::Stopped => !container.state.is_running(),
        };
        if !segment_ok {
            return false;
        }
        if filter.is_empty() {
            return true;
        }
        let ports = container
            .published_ports()
            .iter()
            .map(|(public, private)| format!(":{public} :{private}"))
            .collect::<Vec<_>>()
            .join(" ");
        [
            container.display_name(),
            container.name(),
            &container.image,
            container.compose_project().unwrap_or_default(),
            &ports,
        ]
        .iter()
        .any(|text| text.to_lowercase().contains(filter))
    }

    fn toast(&self, id: &str, message: impl Into<String>, cx: &mut App) {
        let message = message.into();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::composite::<DockerPanel>(SharedString::from(
                            id.to_string(),
                        )),
                        message,
                    )
                    .autohide(),
                    cx,
                );
            })
            .log_err();
    }

    fn run(&mut self, container: &Container, action: ContainerAction, cx: &mut Context<Self>) {
        let name = container.display_name().to_string();
        let id = container.id.clone();
        let task = self
            .store
            .update(cx, |store, cx| store.run(id.clone(), action, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.toast(&id, format!("{name} {}", action.done()), cx),
                Err(error) => this.toast(&id, format!("{name}: {error}"), cx),
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_pause(&mut self, container: &Container, cx: &mut Context<Self>) {
        let action = if container.state == ContainerState::Paused {
            ContainerAction::Unpause
        } else {
            ContainerAction::Pause
        };
        self.run(container, action, cx);
    }

    fn toggle_stop(&mut self, container: &Container, cx: &mut Context<Self>) {
        let action = if container.state.is_up() {
            ContainerAction::Stop
        } else {
            ContainerAction::Start
        };
        self.run(container, action, cx);
    }

    fn confirm_remove(
        &mut self,
        container: &Container,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = container.display_name().to_string();
        let confirm = t!("docker.remove.confirm");
        let cancel = t!("docker.remove.cancel");
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("docker.remove.title", name = name),
            Some(&t!("docker.remove.detail")),
            &[confirm.as_ref(), cancel.as_ref()],
            cx,
        );
        let container = container.clone();
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| {
                    this.run(&container, ContainerAction::Remove, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn confirm_prune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).is_pruning() {
            return;
        }
        let confirm = t!("docker.prune.confirm");
        let cancel = t!("docker.remove.cancel");
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("docker.prune.title"),
            Some(&t!("docker.prune.detail")),
            &[confirm.as_ref(), cancel.as_ref()],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return anyhow::Ok(());
            }
            let task = this.update(cx, |this, cx| {
                this.store.update(cx, |store, cx| store.prune(cx))
            })?;
            let result = task.await;
            this.update(cx, |this, cx| {
                let message = match result {
                    Ok(report) => t!(
                        "docker.prune.done",
                        images = report.images,
                        networks = report.networks,
                        volumes = report.volumes,
                        space = format_bytes(report.space_reclaimed)
                    )
                    .to_string(),
                    Err(error) => t!("docker.prune.failed", error = error).to_string(),
                };
                this.toast("prune", message, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn restart_group(&mut self, group: &Group, cx: &mut Context<Self>) {
        for container in &group.containers {
            self.run(container, ContainerAction::Restart, cx);
        }
    }

    fn open_terminal(&self, container: &Container, window: &mut Window, cx: &mut Context<Self>) {
        let Some(docker) = self.store.read(cx).docker().cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        let container = container.clone();
        window
            .spawn(cx, async move |cx| {
                let details = docker.inspect(&container.id).await.ok();
                workspace.update_in(cx, |workspace, window, cx| {
                    open_terminal(workspace, &container, details.as_ref(), &docker, window, cx);
                })
            })
            .detach_and_log_err(cx);
    }

    fn open_logs(&self, container: &Container, window: &mut Window, cx: &mut Context<Self>) {
        let id = container.id.clone();
        let name = SharedString::from(container.display_name().to_string());
        self.workspace
            .update(cx, |workspace, cx| {
                logs_view::open(workspace, id, name, window, cx)
            })
            .log_err();
    }

    fn inspect(&self, container: &Container, window: &mut Window, cx: &mut Context<Self>) {
        let Some(docker) = self.store.read(cx).docker().cloned() else {
            return;
        };
        inspect(
            self.workspace.clone(),
            docker,
            container.clone(),
            window,
            cx,
        );
    }

    fn ask_agent(&self, container: &Container, window: &mut Window, cx: &mut App) {
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id: format!("container:{}", container.id),
                title: container.display_name().to_string(),
                prompt: None,
                submit: false,
            }),
            cx,
        );
    }

    fn with_selected(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        run: impl FnOnce(&mut Self, &Container, &mut Window, &mut Context<Self>),
    ) {
        if let Some(container) = self.selected_container(cx) {
            run(self, &container, window, cx);
        }
    }

    fn render_engine(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let is_ready = matches!(store.connection(), Connection::Ready { .. });
        let is_pruning = store.is_pruning();
        let (runtime, detail, pill): (SharedString, Option<String>, AnyElement) =
            match store.connection() {
                Connection::Ready {
                    docker,
                    runtime,
                    engine_version,
                } => (
                    runtime.clone(),
                    Some(
                        t!(
                            "docker.engine.detail",
                            version = engine_version,
                            socket = socket_file_name(docker.socket())
                        )
                        .to_string(),
                    ),
                    status_pill(t!("docker.engine.connected"), StatusTone::Success, cx)
                        .into_any_element(),
                ),
                Connection::Connecting => (
                    "Docker".into(),
                    None,
                    status_pill(t!("docker.engine.connecting"), StatusTone::Neutral, cx)
                        .into_any_element(),
                ),
                Connection::Unavailable { socket, .. } => (
                    discovery::runtime_for(socket, false).into(),
                    Some(discovery::home_relative(socket)),
                    status_pill(t!("docker.engine.no_response"), StatusTone::Error, cx)
                        .into_any_element(),
                ),
            };
        h_flex()
            .w_full()
            .h(px(52.))
            .flex_none()
            .px(px(12.))
            .gap(px(10.))
            .child(
                h_flex()
                    .size(px(28.))
                    .flex_none()
                    .justify_center()
                    .rounded(Radius::Sm.rems())
                    .bg(Color::Info.color(cx).opacity(0.12))
                    .child(
                        Icon::new(IconName::Server)
                            .size(IconSize::Small)
                            .color(Color::Info),
                    ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(1.))
                    .child(text(runtime, 12.5).weight(FontWeight::BOLD).truncate())
                    .when_some(detail, |this, detail| {
                        this.child(mono(detail, 10.5, Color::Placeholder, cx).truncate())
                    }),
            )
            .child(pill)
            .child(div().flex_1())
            .when(is_ready, |this| {
                this.child(if is_pruning {
                    h_flex()
                        .id("docker-prune")
                        .size(px(24.))
                        .flex_none()
                        .justify_center()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                        .tooltip(Tooltip::text(t!("docker.prune.in_progress")))
                        .into_any_element()
                } else {
                    IconButton::new("docker-prune", IconName::Eraser)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(t!("docker.prune.tooltip")))
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_prune(window, cx)))
                        .into_any_element()
                })
            })
            .child(
                IconButton::new("docker-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(t!("docker.engine.reload")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |store, cx| store.reconnect(cx));
                    })),
            )
            .child(self.render_runtime_switch())
    }

    fn render_runtime_switch(&self) -> impl IntoElement {
        let store = self.store.clone();
        PopoverMenu::new("docker-runtime-switch")
            .anchor(gpui::Anchor::TopRight)
            .trigger_with_tooltip(
                IconButton::new("docker-runtime-switch-trigger", IconName::ChevronUpDown)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text(t!("docker.engine.switch_runtime")),
            )
            .menu(move |window, cx| {
                let store = store.clone();
                let state = store.read(cx);
                let active = match state.connection() {
                    Connection::Ready { docker, .. } => Some(docker.socket().to_path_buf()),
                    Connection::Unavailable { socket, .. } => Some(socket.clone()),
                    Connection::Connecting => None,
                };
                let candidates = state.candidates().to_vec();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(t!("docker.engine.runtimes_header"));
                    for candidate in candidates {
                        let store = store.clone();
                        let path = candidate.path.clone();
                        let toggled = active.as_deref() == Some(candidate.path.as_path());
                        menu = menu.item(
                            ContextMenuEntry::new(format!(
                                "{} · {}",
                                candidate.runtime,
                                candidate.short_path()
                            ))
                            .toggleable(IconPosition::Start, toggled)
                            .disabled(!candidate.exists)
                            .handler(move |_, cx| {
                                let path = path.clone();
                                store.update(cx, |store, cx| store.use_socket(path, cx));
                            }),
                        );
                    }
                    menu
                }))
            })
    }

    fn render_filters(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let total = store.containers().len();
        let running = store
            .containers()
            .iter()
            .filter(|container| container.state.is_running())
            .count();
        let focused = self
            .filter_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .w_full()
            .flex_none()
            .gap(px(8.))
            .pt(px(10.))
            .px(px(12.))
            .pb(px(8.))
            .child(
                text_field(focused, cx)
                    .flex_1()
                    .gap(px(8.))
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(text_input(&self.filter_editor, 12., cx)),
                    )
                    .child(
                        KeyBinding::for_action_in(&FocusFilter, &focus_handle, cx)
                            .size(rems_from_px(10.5_f32)),
                    ),
            )
            .child(
                segmented_control(cx)
                    .flex_none()
                    .child(self.render_segment(Segment::All, total, cx))
                    .child(self.render_segment(Segment::Running, running, cx))
                    .child(self.render_segment(Segment::Stopped, total - running, cx)),
            )
    }

    fn render_segment(&self, value: Segment, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let (id, label) = match value {
            Segment::All => ("docker-segment-all", t!("docker.segment.all")),
            Segment::Running => ("docker-segment-running", t!("docker.segment.running")),
            Segment::Stopped => ("docker-segment-stopped", t!("docker.segment.stopped")),
        };
        segment_option_with_count(
            id,
            label,
            count.to_string(),
            value == self.segment,
            true,
            cx,
        )
        .flex_none()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.segment = value;
            cx.notify();
        }))
        .into_any_element()
    }

    fn render_group_header(
        &self,
        group: &Group,
        visible: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = group.key();
        let collapsed = self.collapsed_groups.contains(&key);
        let is_compose = matches!(group.kind, GroupKind::Compose { .. });
        let count = if is_compose {
            let running = group
                .containers
                .iter()
                .filter(|container| container.state.is_running())
                .count();
            t!(
                "docker.group.compose_count",
                running = running,
                total = group.containers.len()
            )
            .to_string()
        } else {
            visible.to_string()
        };
        h_flex()
            .id(SharedString::from(format!("docker-group-{key}")))
            .w_full()
            .h(px(32.))
            .flex_none()
            .px(px(6.))
            .gap(px(8.))
            .rounded(px(6.))
            .cursor_pointer()
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Icon::new(if is_compose {
                    IconName::Blocks
                } else {
                    IconName::Box
                })
                .size(IconSize::Small),
            )
            .child(text(group.title(), 12.).weight(FontWeight::BOLD))
            .child(mono(count, 10.5, Color::Placeholder, cx))
            .child(div().flex_1())
            .when(is_compose, |this| {
                let restart_group = group.clone();
                let mut buttons = vec![
                    group_icon_button(
                        SharedString::from(format!("docker-group-restart-{key}")),
                        IconName::RotateCw,
                        22.,
                    )
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(t!("docker.group.restart_compose")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.restart_group(&restart_group, cx);
                    }))
                    .into_any_element(),
                ];
                if self.compose.is_some() {
                    buttons.push(
                        group_icon_button(
                            SharedString::from(format!("docker-group-down-{key}")),
                            IconName::Power,
                            22.,
                        )
                        .icon_color(Color::Muted)
                        .disabled(self.compose_running)
                        .tooltip(Tooltip::text(t!("docker.group.down_compose")))
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.confirm_compose_down(window, cx);
                        }))
                        .into_any_element(),
                    );
                }
                this.child(button_group(buttons, cx))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&key) {
                    this.collapsed_groups.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// "Subir compose": shown while none of the project's compose services runs.
    fn render_compose_card(&self, project: &ComposeProject, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let hover = colors.ghost_element_hover;
        let command_background = colors.editor_background;
        let selection = self.compose_selection(project, cx);
        let selected = project.selected_services(&selection).len();
        let (_, command) = project.up_command(&selection);
        let can_start = selected > 0 && !self.compose_running;
        let focus_handle = self.focus_handle.clone();

        let profile_chip = |index: usize, profile: Option<String>, cx: &mut Context<Self>| {
            let on = match &profile {
                None => selection.default_profile,
                Some(profile) => selection.profiles.contains(profile),
            };
            let label: SharedString = match &profile {
                None => t!("docker.compose_up.default_profile"),
                Some(profile) => profile.clone().into(),
            };
            choice_chip(("docker-compose-profile", index), None, label, on, true, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let profile = profile.clone();
                    this.update_compose_selection(cx, |project, selection| {
                        project.toggle_profile(selection, profile.as_deref())
                    });
                }))
                .into_any_element()
        };
        let mut chips = vec![profile_chip(0, None, cx)];
        for (index, profile) in project.profiles.iter().enumerate() {
            chips.push(profile_chip(index + 1, Some(profile.clone()), cx));
        }

        let services: Vec<AnyElement> = project
            .services
            .iter()
            .enumerate()
            .map(|(index, service)| {
                let checked = selection.services.contains(&service.name);
                let outside = (!checked)
                    .then(|| service.excluding_profile(&selection))
                    .flatten();
                let name = service.name.clone();
                h_flex()
                    .id(("docker-compose-service", index))
                    .w_full()
                    .h(px(26.))
                    .px(px(6.))
                    .gap(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(move |this| this.bg(hover))
                    .when(outside.is_some(), |this| this.opacity(0.55))
                    .child(checkbox_indicator(checked, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(mono(service.name.clone(), 11.5, Color::Default, cx).truncate()),
                    )
                    .when_some(outside, |this, profile| {
                        this.child(mono(
                            t!("docker.compose_up.service_profile", profile = profile),
                            10.5,
                            Color::Placeholder,
                            cx,
                        ))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let name = name.clone();
                        this.update_compose_selection(cx, |_, selection| {
                            if !selection.services.remove(&name) {
                                selection.services.insert(name);
                            }
                        });
                    }))
                    .into_any_element()
            })
            .collect();

        let this = cx.weak_entity();
        let env_files = self.env_files.clone();
        let env_label: SharedString = selection
            .env_file
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| t!("docker.compose_up.no_env_file"));
        let env_select = PopoverMenu::new("docker-compose-env")
            .trigger(PopoverTriggerArea::new(select_field(
                "docker-compose-env-trigger",
                env_label,
                cx,
            )))
            .menu(move |window, cx| {
                let this = this.clone();
                let env_files = env_files.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let choices = std::iter::once(None).chain(env_files.into_iter().map(Some));
                    for env_file in choices {
                        let this = this.clone();
                        let label: SharedString = env_file
                            .clone()
                            .map(SharedString::from)
                            .unwrap_or_else(|| t!("docker.compose_up.no_env_file"));
                        menu = menu.entry(label, None, move |_, cx| {
                            let env_file = env_file.clone();
                            this.update(cx, |this, cx| {
                                this.update_compose_selection(cx, |_, selection| {
                                    selection.env_file = env_file
                                })
                            })
                            .log_err();
                        });
                    }
                    menu
                }))
            });

        let start_label = if selected == 1 {
            t!("docker.compose_up.start.one")
        } else {
            t!("docker.compose_up.start.other", count = selected)
        };
        let copy = command.clone();

        card(cx)
            .border_color(accent)
            .p(px(12.))
            .gap(px(12.))
            .child(
                h_flex()
                    .gap(px(10.))
                    .child(
                        h_flex()
                            .size(px(28.))
                            .flex_none()
                            .justify_center()
                            .rounded(Radius::Sm.rems())
                            .bg(Color::Accent.color(cx).opacity(0.12))
                            .child(
                                Icon::new(IconName::Blocks)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            ),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                text(project.name.clone(), 12.5)
                                    .weight(FontWeight::BOLD)
                                    .truncate(),
                            )
                            .child(
                                mono(
                                    if project.services.len() == 1 {
                                        t!("docker.compose_up.summary.one")
                                    } else {
                                        t!(
                                            "docker.compose_up.summary.other",
                                            count = project.services.len()
                                        )
                                    },
                                    10.5,
                                    Color::Placeholder,
                                    cx,
                                )
                                .truncate(),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section_header(t!("docker.compose_up.profiles"), None))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(chips)),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(section_header(
                        t!(
                            "docker.compose_up.services",
                            selected = selected,
                            total = project.services.len()
                        ),
                        None,
                    ))
                    .children(services),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section_label(t!("docker.compose_up.env_file")))
                    .child(env_select),
            )
            .child(
                h_flex()
                    .id("docker-compose-rebuild")
                    .gap(px(8.))
                    .cursor_pointer()
                    .child(toggle_switch(
                        "docker-compose-rebuild-switch",
                        selection.rebuild,
                        true,
                        cx,
                    ))
                    .child(text(t!("docker.compose_up.rebuild"), 12.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_compose_selection(cx, |_, selection| {
                            selection.rebuild = !selection.rebuild
                        });
                    })),
            )
            .child(
                card(cx)
                    .p(px(12.))
                    .gap(px(6.))
                    .bg(command_background)
                    .child(section_label(t!("docker.compose_up.command")))
                    .child(mono(command, 11.5, Color::Default, cx)),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        primary_button(
                            "docker-compose-up",
                            Some(IconName::PlayFilled),
                            start_label,
                            Some(KeyBinding::for_action_in(&ComposeUp, &focus_handle, cx)),
                            cx,
                        )
                        .when(!can_start, |this| this.opacity(0.5).cursor_default())
                        .when(selected == 0, |this| {
                            this.tooltip(Tooltip::text(t!("docker.compose_up.no_services")))
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.compose_up(window, cx))),
                    )
                    .child(
                        secondary_button(
                            "docker-compose-copy",
                            Some((IconName::Copy, Color::Muted)),
                            t!("docker.compose_up.copy_command"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            this.toast("compose-copy", t!("docker.compose_up.copied"), cx);
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_container(&self, container: &Container, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let selected = self.selected.as_deref() == Some(container.id.as_str());
        let transition = store.transition(&container.id);
        let (status, state) = row_state(
            container,
            store.stats(&container.id).map(|stats| {
                format!(
                    "{} · {}",
                    format_percent(stats.cpu_percent),
                    format_bytes(stats.memory_bytes)
                )
            }),
            store.restart_count(&container.id),
        );
        let id = container.id.clone();
        StatusRow::new(
            SharedString::from(format!("docker-container-{}", container.id)),
            status,
            container.display_name().to_string(),
        )
        .detail(detail_line(container))
        .selected(selected)
        .map(|row| match transition {
            Some(action) => row.end_slot(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                    .child(mono(action.in_progress(), 11., Color::Muted, cx)),
            ),
            None => row.state(state),
        })
        .when(selected, |row| {
            row.expansion(self.render_quick_actions(container, cx))
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            if this.selected.as_deref() == Some(id.as_str()) {
                this.selected = None;
            } else {
                this.selected = Some(id.clone());
            }
            this.focus_handle.focus(window, cx);
            cx.notify();
        }))
        .into_any_element()
    }

    fn render_quick_actions(&self, container: &Container, cx: &mut Context<Self>) -> AnyElement {
        let running = container.state.is_running();
        let paused = container.state == ContainerState::Paused;
        let up = container.state.is_up();
        let busy = self.store.read(cx).transition(&container.id).is_some();
        let terminal_container = container.clone();
        let logs_container = container.clone();
        let pause_container = container.clone();
        let restart_container = container.clone();
        let stop_container = container.clone();
        let mut lifecycle: Vec<AnyElement> = Vec::new();
        if up {
            lifecycle.push(
                group_icon_button(
                    "docker-quick-pause",
                    if paused {
                        IconName::PlayFilled
                    } else {
                        IconName::DebugPause
                    },
                    24.,
                )
                .icon_color(Color::Muted)
                .disabled(busy)
                .tooltip(Tooltip::text(if paused {
                    t!("docker.container.resume")
                } else {
                    t!("docker.container.pause")
                }))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_pause(&pause_container, cx)),
                )
                .into_any_element(),
            );
        }
        lifecycle.push(
            group_icon_button("docker-quick-restart", IconName::RotateCw, 24.)
                .icon_color(Color::Muted)
                .disabled(busy)
                .tooltip(Tooltip::text(t!("docker.container.restart")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run(&restart_container, ContainerAction::Restart, cx)
                }))
                .into_any_element(),
        );
        lifecycle.push(
            group_icon_button(
                "docker-quick-stop",
                if up {
                    IconName::Stop
                } else {
                    IconName::PlayFilled
                },
                24.,
            )
            .icon_color(Color::Muted)
            .disabled(busy)
            .tooltip(Tooltip::text(if up {
                t!("docker.container.stop")
            } else {
                t!("docker.container.start")
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_stop(&stop_container, cx)))
            .into_any_element(),
        );
        lifecycle.push(self.render_more_menu(container, cx).into_any_element());
        h_flex()
            .w_full()
            .gap(px(6.))
            .pt(px(2.))
            .pr(px(8.))
            .pb(px(8.))
            .pl(px(28.))
            .child(
                capsule_button(
                    "docker-quick-terminal",
                    Some(IconName::Terminal),
                    t!("docker.quick.terminal"),
                    cx,
                )
                .tooltip(Tooltip::text(if running {
                    t!("docker.quick.terminal_tooltip")
                } else {
                    t!("docker.quick.terminal_needs_running")
                }))
                .when(!running, |this| this.opacity(0.5).cursor_default())
                .when(running, |this| {
                    this.on_click(cx.listener(move |this, _, window, cx| {
                        this.open_terminal(&terminal_container, window, cx)
                    }))
                }),
            )
            .child(
                capsule_button(
                    "docker-quick-logs",
                    Some(IconName::Reader),
                    t!("docker.quick.logs"),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_logs(&logs_container, window, cx)
                })),
            )
            .child(div().flex_1())
            .child(button_group(lifecycle, cx))
            .into_any_element()
    }

    fn render_more_menu(&self, container: &Container, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let container = container.clone();
        let focus_handle = self.focus_handle.clone();
        PopoverMenu::new("docker-container-menu")
            .with_handle(self.menu_handle.clone())
            .anchor(gpui::Anchor::TopRight)
            .trigger(
                group_icon_button("docker-quick-more", IconName::Ellipsis, 24.)
                    .icon_color(Color::Muted),
            )
            .menu(move |window, cx| {
                Some(container_menu(
                    this.clone(),
                    container.clone(),
                    focus_handle.clone(),
                    window,
                    cx,
                ))
            })
    }

    fn render_section(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let filter = self.filter_text(cx);
        let open = self.open_sections.contains(&section);
        let (title, summary, rows): (SharedString, String, Vec<(String, String)>) = match section {
            Section::Images => {
                let total: i64 = store.images().iter().map(|image| image.size).sum();
                let rows = store
                    .images()
                    .iter()
                    .map(|image| {
                        let name = image
                            .tags()
                            .next()
                            .map(str::to_string)
                            .unwrap_or_else(|| short_hash(&image.id));
                        (name, format_bytes(image.size.max(0) as u64))
                    })
                    .collect();
                (
                    t!("docker.section.images"),
                    format!(
                        "{} · {}",
                        store.images().len(),
                        format_bytes(total.max(0) as u64)
                    ),
                    rows,
                )
            }
            Section::Volumes => {
                let sizes: Vec<i64> = store
                    .volumes()
                    .iter()
                    .filter_map(|volume| store.volume_size(&volume.name))
                    .filter(|size| *size >= 0)
                    .collect();
                let rows = store
                    .volumes()
                    .iter()
                    .map(|volume| {
                        (
                            volume.name.clone(),
                            store
                                .volume_size(&volume.name)
                                .filter(|size| *size >= 0)
                                .map(|size| format_bytes(size as u64))
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                let summary = if sizes.is_empty() {
                    store.volumes().len().to_string()
                } else {
                    format!(
                        "{} · {}",
                        store.volumes().len(),
                        format_bytes(sizes.iter().sum::<i64>() as u64)
                    )
                };
                (t!("docker.section.volumes"), summary, rows)
            }
            Section::Networks => {
                let rows = store
                    .networks()
                    .iter()
                    .map(|network| (network.name.clone(), network.driver.clone()))
                    .collect();
                (
                    t!("docker.section.networks"),
                    store.networks().len().to_string(),
                    rows,
                )
            }
        };
        let rows: Vec<(String, String)> = rows
            .into_iter()
            .filter(|(name, _)| filter.is_empty() || name.to_lowercase().contains(&filter))
            .collect();
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(match section {
                        Section::Images => "docker-section-images",
                        Section::Volumes => "docker-section-volumes",
                        Section::Networks => "docker-section-networks",
                    })
                    .w_full()
                    .h(px(30.))
                    .px(px(6.))
                    .gap(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(section_label(title))
                    .child(div().flex_1())
                    .child(
                        Label::new(summary)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Placeholder),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.open_sections.remove(&section) {
                            this.open_sections.insert(section);
                        }
                        cx.notify();
                    })),
            )
            .when(open, |this| {
                this.children(rows.into_iter().map(|(name, detail)| {
                    h_flex()
                        .w_full()
                        .h(px(26.))
                        .pl(px(26.))
                        .pr(px(6.))
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(mono(name, 11., Color::Muted, cx).truncate()),
                        )
                        .child(mono(detail, 10.5, Color::Placeholder, cx))
                }))
            })
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter_text(cx);
        let groups = self.groups.clone();
        let loaded = self.store.read(cx).is_loaded();
        let mut children: Vec<AnyElement> = Vec::new();
        if loaded && filter.is_empty() && self.segment == Segment::All {
            if let Some(project) = &self.compose
                && !self.project_compose_running()
            {
                children.push(self.render_compose_card(project, cx));
            } else if let Some((file, error)) = &self.compose_error {
                children.push(
                    error_callout(
                        t!("docker.compose_up.read_failed", file = file),
                        error.clone(),
                        cx,
                    )
                    .into_any_element(),
                );
            }
        }
        let has_card = !children.is_empty();
        for group in &groups {
            let visible: Vec<&Container> = group
                .containers
                .iter()
                .filter(|container| self.matches(container, &filter))
                .collect();
            if visible.is_empty() && (!filter.is_empty() || self.segment != Segment::All) {
                continue;
            }
            children.push(self.render_group_header(group, visible.len(), cx));
            if self.collapsed_groups.contains(&group.key()) {
                continue;
            }
            for container in visible {
                children.push(self.render_container(container, cx));
            }
        }
        if children.len() == usize::from(has_card) {
            children.push(
                h_flex()
                    .w_full()
                    .px(px(6.))
                    .py_2()
                    .child(
                        text(
                            if !loaded {
                                t!("docker.list.loading")
                            } else if filter.is_empty() && self.segment == Segment::All {
                                t!("docker.list.empty")
                            } else {
                                t!("docker.list.no_match")
                            },
                            12.,
                        )
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        v_flex()
            .id("docker-list")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .gap(px(2.))
            .pt(px(4.))
            .px(px(8.))
            .pb(px(8.))
            .children(children)
            .child(div().my(px(4.)).child(Divider::horizontal()))
            .child(self.render_section(Section::Images, cx))
            .child(self.render_section(Section::Volumes, cx))
            .child(self.render_section(Section::Networks, cx))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let total = store.containers().len();
        let memory: u64 = store
            .containers()
            .iter()
            .filter(|container| container.state.is_running())
            .filter_map(|container| store.stats(&container.id))
            .map(|stats| stats.memory_bytes)
            .sum();
        let mut summary = if total == 1 {
            t!("docker.footer.summary.one")
        } else {
            t!("docker.footer.summary.other", count = total)
        }
        .to_string();
        if memory > 0 {
            summary.push_str(&format!(" · {}", format_bytes(memory)));
        }
        let focus_handle = self.focus_handle.clone();
        let new_container_button = primary_button(
            "docker-new-container",
            Some(IconName::Plus),
            t!("docker.footer.new_container"),
            Some(KeyBinding::for_action_in(&NewContainer, &focus_handle, cx)),
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.workspace
                .update(cx, |workspace, cx| {
                    new_container::open(workspace, window, cx)
                })
                .log_err();
        }))
        .into_any_element();
        navigator_footer(None, summary, Some(new_container_button), cx)
            .h(px(48.))
            .border_t_1()
            .border_color(cx.theme().colors().border)
    }

    fn render_unavailable(&self, socket: &Path, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let others: Vec<discovery::SocketCandidate> = store
            .candidates()
            .iter()
            .filter(|candidate| candidate.path != socket && candidate.runtime != "Docker Desktop")
            .filter(|candidate| !candidate.path.ends_with("var/run/docker.sock"))
            .cloned()
            .collect();
        let desktop =
            cfg!(target_os = "macos") && discovery::runtime_for(socket, false) == "Docker Desktop";
        let runtimes = (!others.is_empty()).then(|| {
            v_flex()
                .w(px(300.))
                .gap(px(6.))
                .pt_2()
                .child(section_label(t!("docker.unavailable.other_runtime")))
                .child(
                    grouped_block(cx).children(others.into_iter().enumerate().map(
                        |(index, candidate)| {
                            let path = candidate.path.clone();
                            h_flex()
                                .w_full()
                                .h(px(32.))
                                .px_2()
                                .gap(px(8.))
                                .child(
                                    if candidate.exists {
                                        StatusTone::Success
                                    } else {
                                        StatusTone::Neutral
                                    }
                                    .dot(cx),
                                )
                                .child(
                                    text(candidate.runtime.clone(), 12.).weight(FontWeight::MEDIUM),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        mono(candidate.short_path(), 10.5, Color::Placeholder, cx)
                                            .truncate(),
                                    ),
                                )
                                .child(if candidate.exists {
                                    capsule_button(
                                        ("docker-use-socket", index),
                                        None,
                                        t!("docker.unavailable.use"),
                                        cx,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let path = path.clone();
                                        this.store
                                            .update(cx, |store, cx| store.use_socket(path, cx));
                                    }))
                                    .into_any_element()
                                } else {
                                    text(t!("docker.unavailable.not_found"), 11.5)
                                        .color(Color::Placeholder)
                                        .into_any_element()
                                })
                        },
                    )),
                )
        });
        EmptyState::new(IconName::Server, t!("docker.unavailable.title"))
            .description(t!(
                "docker.unavailable.detail",
                socket = discovery::home_relative(socket)
            ))
            .action(
                h_flex()
                    .gap(px(8.))
                    .child(
                        secondary_button(
                            "docker-retry",
                            None,
                            t!("docker.unavailable.retry"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |store, cx| store.reconnect(cx));
                        })),
                    )
                    .when(desktop, |this| {
                        this.child(
                            primary_button(
                                "docker-open-desktop",
                                None,
                                t!("docker.unavailable.open_docker_desktop"),
                                None,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.open_docker_desktop(cx))),
                        )
                    }),
            )
            .when_some(runtimes, |this, runtimes| this.action(runtimes))
            .action(
                div().w(px(300.)).text_center().child(
                    Label::new(t!("docker.unavailable.docker_host_hint"))
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                ),
            )
            .into_any_element()
    }

    fn open_docker_desktop(&self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn(async move |_, cx| {
            let status = util::command::new_command("open")
                .args(["-a", "Docker"])
                .status()
                .await;
            if let Err(error) = status {
                log::warn!("Docker Desktop não abriu: {error}");
            }
            // Docker Desktop takes a few seconds to bring the daemon up.
            cx.background_executor()
                .timer(std::time::Duration::from_secs(4))
                .await;
            store.update(cx, |store, cx| store.reconnect(cx));
        })
        .detach();
    }
}

pub(crate) fn project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .find(|worktree| {
            worktree
                .read(cx)
                .root_entry()
                .is_some_and(|entry| entry.is_dir())
        })
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

/// `trix-api:dev · :3000→3000`.
fn detail_line(container: &Container) -> String {
    let ports = container.published_ports();
    let ports = ports
        .iter()
        .map(|(public, private)| {
            if public == private {
                format!(":{public}")
            } else {
                format!(":{public}→{private}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if ports.is_empty() {
        container.image.clone()
    } else {
        format!("{} · {ports}", container.image)
    }
}

/// The state as the row writes it, for the agent's card.
pub(crate) fn state_label(container: &Container, store: &DockerStore) -> String {
    let usage = store.stats(&container.id).map(|stats| {
        format!(
            "{} · {}",
            format_percent(stats.cpu_percent),
            format_bytes(stats.memory_bytes)
        )
    });
    row_state(container, usage, store.restart_count(&container.id)).1
}

/// The dot and the text at the end of a row, as the Container Row variants draw them.
fn row_state(
    container: &Container,
    usage: Option<String>,
    restart_count: Option<i64>,
) -> (RowStatus, String) {
    match container.state {
        ContainerState::Running => (
            if container.health() == Some("unhealthy") {
                RowStatus::Failing
            } else {
                RowStatus::Running
            },
            usage.unwrap_or_else(|| {
                container
                    .health()
                    .map(|health| health_label(health).to_string())
                    .unwrap_or_default()
            }),
        ),
        ContainerState::Restarting => (
            RowStatus::Failing,
            match restart_count {
                Some(count) if count > 0 => {
                    format!("{} · {count}×", t!("docker.state.restarting"))
                }
                _ => t!("docker.state.restarting").to_string(),
            },
        ),
        ContainerState::Paused => (RowStatus::Paused, t!("docker.state.paused").to_string()),
        ContainerState::Exited | ContainerState::Dead => {
            let mut state = match container.exit_code() {
                Some(code) => t!("docker.state.exited_with_code", code = code).to_string(),
                None => container.state.label().to_string(),
            };
            if let Some(age) = container.status_age().and_then(age_label) {
                state.push_str(&format!(" · {}", t!("docker.age.ago", age = age)));
            }
            (
                if container
                    .exit_code()
                    .is_some_and(|code| code != 0 && code != 143 && code != 137)
                {
                    RowStatus::Failing
                } else {
                    RowStatus::Stopped
                },
                state,
            )
        }
        ContainerState::Created | ContainerState::Removing => {
            (RowStatus::Stopped, container.state.label().to_string())
        }
    }
}

/// Docker's "2 hours", "About a minute", "Less than a second" as the boards write them.
pub(crate) fn age_label(age: &str) -> Option<String> {
    let age = age.trim().to_lowercase();
    if age.starts_with("less than") {
        return Some(t!("docker.age.moments").to_string());
    }
    let mut words = age.split_whitespace();
    let first = words.next()?;
    let (count, unit) = match first {
        "about" | "an" | "a" => {
            let next = words.next()?;
            let unit = if next == "an" || next == "a" {
                words.next()?
            } else {
                next
            };
            (1, unit.to_string())
        }
        number => (number.parse::<u64>().ok()?, words.next()?.to_string()),
    };
    let unit = unit.trim_end_matches('s');
    let label = match unit {
        "second" => t!("docker.age.seconds", count = count),
        "minute" => t!("docker.age.minutes", count = count),
        "hour" => t!("docker.age.hours", count = count),
        "day" => t!("docker.age.days", count = count),
        "week" => t!("docker.age.weeks", count = count),
        "month" if count == 1 => t!("docker.age.months.one", count = count),
        "month" => t!("docker.age.months.other", count = count),
        "year" if count == 1 => t!("docker.age.years.one", count = count),
        "year" => t!("docker.age.years.other", count = count),
        _ => return None,
    };
    Some(label.to_string())
}

fn short_hash(id: &str) -> String {
    let hash = id.trim_start_matches("sha256:");
    hash.get(..12).unwrap_or(hash).to_string()
}

pub(crate) fn text(label: impl Into<SharedString>, size: f32) -> Label {
    Label::new(label).size(LabelSize::Custom(rems_from_px(size)))
}

pub(crate) fn mono(label: impl Into<SharedString>, size: f32, color: Color, cx: &App) -> Label {
    text(label, size).buffer_font(cx).color(color)
}

fn socket_file_name(socket: &Path) -> String {
    socket
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| socket.display().to_string())
}

/// A single-line editor drawn in the interface font at `size`, for fields that follow the
/// boards instead of `InputField`'s fixed look.
pub(crate) fn text_input(editor: &Entity<Editor>, size: f32, cx: &App) -> AnyElement {
    let settings = ThemeSettings::get_global(cx);
    let colors = cx.theme().colors();
    let text_style = TextStyle {
        font_family: settings.ui_font.family.clone(),
        font_features: settings.ui_font.features.clone(),
        font_size: rems_from_px(size).into(),
        font_weight: settings.ui_font.weight,
        font_style: FontStyle::Normal,
        line_height: relative(1.2),
        color: colors.text,
        ..Default::default()
    };
    EditorElement::new(
        editor,
        EditorStyle {
            background: gpui::transparent_black(),
            local_player: cx.theme().players().local(),
            syntax: cx.theme().syntax().clone(),
            text: text_style,
            ..Default::default()
        },
    )
    .into_any_element()
}

/// The same field in the code font, for values like ports, paths and env.
pub(crate) fn mono_input(editor: &Entity<Editor>, size: f32, cx: &App) -> AnyElement {
    let settings = ThemeSettings::get_global(cx);
    let colors = cx.theme().colors();
    let text_style = TextStyle {
        font_family: settings.buffer_font.family.clone(),
        font_features: settings.buffer_font.features.clone(),
        font_size: rems_from_px(size).into(),
        font_weight: settings.buffer_font.weight,
        font_style: FontStyle::Normal,
        line_height: relative(1.2),
        color: colors.text,
        ..Default::default()
    };
    EditorElement::new(
        editor,
        EditorStyle {
            background: gpui::transparent_black(),
            local_player: cx.theme().players().local(),
            syntax: cx.theme().syntax().clone(),
            text: text_style,
            ..Default::default()
        },
    )
    .into_any_element()
}

/// The ⋯ menu of a container, in the boards' sections: Interagir, Ciclo de vida, Agent and
/// the destructive Remover at the end.
fn container_menu(
    panel: WeakEntity<DockerPanel>,
    container: Container,
    focus_handle: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _window, cx| {
        let running = container.state.is_running();
        let paused = container.state == ContainerState::Paused;
        let up = container.state.is_up();
        let name = container.display_name().to_string();
        let short_id = container.short_id().to_string();
        let full_id = container.id.clone();
        let (status, _) = row_state(&container, None, None);
        let status_tone = match status {
            RowStatus::Running => StatusTone::Success,
            RowStatus::Paused => StatusTone::Warning,
            RowStatus::Failing => StatusTone::Error,
            RowStatus::Stopped => StatusTone::Neutral,
        };
        let port = container
            .published_ports()
            .first()
            .map(|(public, _)| *public);

        let entry = |label: SharedString,
                     icon: IconName,
                     action: Box<dyn gpui::Action>,
                     run: Box<
            dyn Fn(&mut DockerPanel, &Container, &mut Window, &mut Context<DockerPanel>),
        >| {
            let panel = panel.clone();
            let container = container.clone();
            ContextMenuEntry::new(label)
                .icon(icon)
                .icon_color(Color::Muted)
                .action(action)
                .handler(move |window, cx| {
                    panel
                        .update(cx, |panel, cx| run(panel, &container, window, cx))
                        .log_err();
                })
        };

        let mut menu = menu
            .sectioned()
            .fixed_width(px(272.).into())
            .context(focus_handle)
            .custom_row(move |_window, cx| {
                let full_id = full_id.clone();
                h_flex()
                    .w_full()
                    .h(px(32.))
                    .px(px(8.))
                    .gap(px(8.))
                    .child(status_tone.dot(cx))
                    .child(text(name.clone(), 12.5).weight(FontWeight::BOLD))
                    .child(mono(short_id.clone(), 10.5, Color::Placeholder, cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("docker-copy-id", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(t!("docker.menu.copy_id")))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(full_id.clone()))
                            }),
                    )
                    .into_any_element()
            })
            .separator()
            .header(t!("docker.menu.interact"));
        if running {
            menu = menu.item(entry(
                t!("docker.menu.open_terminal"),
                IconName::Terminal,
                Box::new(OpenTerminal),
                Box::new(|panel, container, window, cx| panel.open_terminal(container, window, cx)),
            ));
        }
        menu = menu
            .item(entry(
                t!("docker.menu.view_logs"),
                IconName::Reader,
                Box::new(OpenLogs),
                Box::new(|panel, container, window, cx| panel.open_logs(container, window, cx)),
            ))
            .item(entry(
                t!("docker.menu.inspect"),
                IconName::Eye,
                Box::new(Inspect),
                Box::new(|panel, container, window, cx| panel.inspect(container, window, cx)),
            ));
        if let Some(port) = port.filter(|_| running) {
            menu = menu.item(
                ContextMenuEntry::new(t!("docker.menu.open_localhost", port = port))
                    .icon(IconName::ArrowUpRight)
                    .icon_color(Color::Muted)
                    .handler(move |_, cx| cx.open_url(&format!("http://localhost:{port}"))),
            );
        }
        menu = menu.separator().header(t!("docker.menu.lifecycle"));
        if up {
            menu = menu.item(entry(
                if paused {
                    t!("docker.container.resume")
                } else {
                    t!("docker.container.pause")
                },
                if paused {
                    IconName::PlayFilled
                } else {
                    IconName::DebugPause
                },
                Box::new(TogglePause),
                Box::new(|panel, container, _, cx| panel.toggle_pause(container, cx)),
            ));
        }
        menu = menu
            .item(entry(
                t!("docker.container.restart"),
                IconName::RotateCw,
                Box::new(Restart),
                Box::new(|panel, container, _, cx| {
                    panel.run(container, ContainerAction::Restart, cx)
                }),
            ))
            .item(entry(
                if up {
                    t!("docker.container.stop")
                } else {
                    t!("docker.container.start")
                },
                if up {
                    IconName::Stop
                } else {
                    IconName::PlayFilled
                },
                Box::new(ToggleStop),
                Box::new(|panel, container, _, cx| panel.toggle_stop(container, cx)),
            ))
            .separator()
            .header("Agent")
            .item({
                let panel = panel.clone();
                let container = container.clone();
                ContextMenuEntry::new(t!("docker.menu.ask_agent", name = container.display_name()))
                    .icon(IconName::Sparkle)
                    .icon_color(Color::Muted)
                    .handler(move |window, cx| {
                        panel
                            .update(cx, |panel, cx| panel.ask_agent(&container, window, cx))
                            .log_err();
                    })
            })
            .separator();
        let remove_panel = panel.clone();
        let remove_container = container.clone();
        let remove_focus = focus_handle_for_binding(&panel, cx);
        menu.custom_entry(
            move |_window, cx| {
                h_flex()
                    .w_full()
                    .gap(px(10.))
                    .child(
                        Icon::new(IconName::Trash)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(text(t!("docker.menu.remove"), 12.5).color(Color::Error)),
                    )
                    .when_some(remove_focus.clone(), |this, focus| {
                        this.child(
                            KeyBinding::for_action_in(&Remove, &focus, cx)
                                .size(rems_from_px(11_f32))
                                .color(Color::Placeholder),
                        )
                    })
                    .into_any_element()
            },
            move |window, cx| {
                remove_panel
                    .update(cx, |panel, cx| {
                        panel.confirm_remove(&remove_container, window, cx)
                    })
                    .log_err();
            },
        )
    })
}

fn focus_handle_for_binding(panel: &WeakEntity<DockerPanel>, cx: &App) -> Option<FocusHandle> {
    panel
        .upgrade()
        .map(|panel| panel.read(cx).focus_handle.clone())
}

/// A shell inside the container in a terminal tab, through the docker CLI pointed at the same
/// daemon the dock talks to. Leaving the shell leaves the container running.
pub(crate) fn open_terminal(
    workspace: &mut Workspace,
    container: &Container,
    details: Option<&crate::client::ContainerDetails>,
    docker: &crate::Docker,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let name = container.display_name().to_string();
    let user = details
        .map(|details| details.config.user.clone())
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "root".into());
    let working_dir = details
        .map(|details| details.config.working_dir.clone())
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| "/".into());
    let banner = format!(
        "\\033[32m●\\033[0m \\033[1m{}\\033[0m  \\033[2m{} · {} · {}  —  {}\\033[0m\\n\\n",
        shell_escape(&t!("docker.terminal.inside", name = name)),
        shell_escape(&container.image),
        shell_escape(&t!("docker.terminal.user", user = user)),
        shell_escape(&working_dir),
        shell_escape(&t!("docker.terminal.exit_hint")),
    );
    let script = format!(
        "printf '{banner}'; exec docker exec -it {} sh -c 'command -v bash >/dev/null && exec bash || exec sh'",
        container.id
    );
    let mut env = HashMap::default();
    env.insert(
        "DOCKER_HOST".to_string(),
        format!("unix://{}", docker.socket().display()),
    );
    let task = task::SpawnInTerminal {
        id: task::TaskId(format!("docker-exec-{}", container.id)),
        full_label: format!("{name} · sh"),
        label: format!("{name} · sh"),
        command: Some("sh".into()),
        args: vec!["-c".into(), script],
        command_label: format!("docker exec -it {name} sh"),
        cwd: None,
        env,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: task::RevealStrategy::Always,
        reveal_target: task::RevealTarget::Dock,
        hide: task::HideStrategy::Never,
        shell: task::Shell::System,
        show_summary: false,
        show_command: false,
        show_rerun: false,
        save: task::SaveStrategy::None,
    };
    workspace.spawn_in_terminal(task, window, cx).detach();
}

/// Text that goes inside a single-quoted printf format.
fn shell_escape(text: &str) -> String {
    text.replace('\'', "")
        .replace('%', "%%")
        .replace('\\', "\\\\")
}

/// The container's `docker inspect` in a read-only JSON tab, with env values as they are: it is
/// the user's own machine.
pub(crate) fn inspect(
    workspace: WeakEntity<Workspace>,
    docker: crate::Docker,
    container: Container,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let details = docker
                .raw_inspect(&container.id)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let (project, language) = workspace.update(cx, |workspace, _| {
                (
                    workspace.project().clone(),
                    workspace.app_state().languages.language_for_name("JSON"),
                )
            })?;
            let language = language.await.log_err();
            let buffer = project
                .update(cx, |project, cx| project.create_buffer(language, true, cx))
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_text(details, cx));
            workspace.update_in(cx, |workspace, window, cx| {
                let title = format!("{} · inspect", container.display_name());
                let buffer = cx
                    .new(|cx| editor::MultiBuffer::singleton(buffer, cx).with_title(title.clone()));
                let editor = cx.new(|cx| {
                    let mut editor = Editor::for_multibuffer(buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor.set_breadcrumb_header(title);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
}

impl Render for DockerPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connection = self.store.read(cx).connection().clone();
        v_flex()
            .key_context("DockerPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &NewContainer, window, cx| {
                this.workspace
                    .update(cx, |workspace, cx| {
                        new_container::open(workspace, window, cx)
                    })
                    .log_err();
            }))
            .on_action(cx.listener(|this, _: &Prune, window, cx| this.confirm_prune(window, cx)))
            .on_action(cx.listener(|this, _: &ComposeUp, window, cx| this.compose_up(window, cx)))
            .on_action(cx.listener(|this, _: &ComposeDown, window, cx| {
                this.confirm_compose_down(window, cx)
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.store.update(cx, |store, cx| store.reconnect(cx));
            }))
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                this.filter_editor.focus_handle(cx).focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenTerminal, window, cx| {
                this.with_selected(window, cx, |this, container, window, cx| {
                    if container.state.is_running() {
                        this.open_terminal(container, window, cx)
                    }
                })
            }))
            .on_action(cx.listener(|this, _: &OpenLogs, window, cx| {
                this.with_selected(window, cx, |this, container, window, cx| {
                    this.open_logs(container, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Inspect, window, cx| {
                this.with_selected(window, cx, |this, container, window, cx| {
                    this.inspect(container, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &TogglePause, window, cx| {
                this.with_selected(window, cx, |this, container, _, cx| {
                    if container.state.is_up() {
                        this.toggle_pause(container, cx)
                    }
                })
            }))
            .on_action(cx.listener(|this, _: &Restart, window, cx| {
                this.with_selected(window, cx, |this, container, _, cx| {
                    this.run(container, ContainerAction::Restart, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &ToggleStop, window, cx| {
                this.with_selected(window, cx, |this, container, _, cx| {
                    this.toggle_stop(container, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Remove, window, cx| {
                this.with_selected(window, cx, |this, container, window, cx| {
                    this.confirm_remove(container, window, cx)
                })
            }))
            .child(self.render_engine(cx))
            .child(
                div()
                    .w_full()
                    .h(px(1.))
                    .flex_none()
                    .bg(cx.theme().colors().border),
            )
            .map(|this| match &connection {
                Connection::Unavailable { socket, .. } => {
                    this.child(self.render_unavailable(socket, cx))
                }
                _ => this
                    .child(self.render_filters(window, cx))
                    .child(self.render_list(cx))
                    .child(self.render_footer(cx)),
            })
    }
}

impl Focusable for DockerPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DockerPanel {}

impl Panel for DockerPanel {
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::DOCKER, &self.project, cx)
    }

    fn persistent_name() -> &'static str {
        "DockerPanel"
    }

    fn panel_key() -> &'static str {
        "DockerPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Docker)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Docker")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        14
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_docker_ages() {
        assert_eq!(age_label("2 hours").as_deref(), Some("2 h"));
        assert_eq!(age_label("About an hour").as_deref(), Some("1 h"));
        assert_eq!(age_label("8 minutes").as_deref(), Some("8 min"));
        assert_eq!(age_label("Less than a second").as_deref(), Some("moments"));
        assert_eq!(age_label("3 months").as_deref(), Some("3 months"));
    }
}
