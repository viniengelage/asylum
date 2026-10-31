use crate::{
    FocusFilter, Inspect, NewContainer, OpenLogs, OpenTerminal, Refresh, Remove, Restart,
    ToggleFocus, TogglePause, ToggleStop,
    client::{Container, ContainerState},
    discovery, format_bytes, format_percent, logs_view, new_container,
    store::{Connection, ContainerAction, DockerStore, Group, GroupKind, StoreEvent},
};
use collections::{HashMap, HashSet};
use editor::{Editor, EditorElement, EditorEvent, EditorStyle};
use gpui::{
    Action as _, AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable,
    FontStyle, FontWeight, PromptLevel, ScrollHandle, Subscription, TextStyle, WeakEntity, px,
    relative,
};
use project::Project;
use settings::Settings as _;
use std::path::{Path, PathBuf};
use theme_settings::ThemeSettings;
use ui::{
    CommonAnimationExt as _, ContextMenu, ContextMenuEntry, KeyBinding, PopoverMenu,
    PopoverMenuHandle, PopoverTriggerArea, RowStatus, StatusRow, Tooltip, prelude::*,
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
            editor.set_placeholder_text("Filtrar containers, imagens, portas…", window, cx);
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
            cx.subscribe(&project, |this, _, event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
                ) {
                    this.project_root = project_root(&this.project, cx);
                    this.regroup(cx);
                }
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
            menu_handle: PopoverMenuHandle::default(),
            scroll_handle: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.regroup(cx);
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
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remover o container {name}?"),
            Some("O container é apagado, mesmo rodando. A imagem e os volumes nomeados continuam."),
            &["Remover", "Cancelar"],
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

    fn run_on_group(&mut self, group: &Group, action: ContainerAction, cx: &mut Context<Self>) {
        let containers: Vec<Container> = group
            .containers
            .iter()
            .filter(|container| match action {
                ContainerAction::Stop => container.state.is_up(),
                _ => true,
            })
            .cloned()
            .collect();
        for container in &containers {
            self.run(container, action, cx);
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
        let (runtime, chip): (SharedString, AnyElement) = match store.connection() {
            Connection::Ready {
                runtime,
                engine_version,
                ..
            } => (
                runtime.clone(),
                chip(cx)
                    .child(mono(
                        format!("engine {engine_version}"),
                        10.5,
                        Color::Placeholder,
                        cx,
                    ))
                    .into_any_element(),
            ),
            Connection::Connecting => (
                "Docker".into(),
                chip(cx)
                    .child(mono("conectando…", 10.5, Color::Placeholder, cx))
                    .into_any_element(),
            ),
            Connection::Unavailable { socket, .. } => (
                discovery::runtime_for(socket, false).into(),
                chip(cx)
                    .gap(px(6.))
                    .child(dot(6., Color::Error.color(cx)))
                    .child(mono("sem resposta", 10.5, Color::Error, cx))
                    .into_any_element(),
            ),
        };
        h_flex()
            .w_full()
            .h(px(44.))
            .flex_none()
            .px(px(12.))
            .gap(px(8.))
            .child(Icon::new(IconName::Server).size(IconSize::Medium))
            .child(text(runtime, 12.5).weight(FontWeight::MEDIUM))
            .child(chip)
            .child(div().flex_1())
            .child(
                icon_button(
                    "docker-refresh",
                    IconName::RotateCw,
                    28.,
                    IconSize::Medium,
                    false,
                    cx,
                )
                .tooltip(Tooltip::text("Recarregar"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, cx| store.reconnect(cx));
                })),
            )
    }

    fn render_filters(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let store = self.store.read(cx);
        let total = store.containers().len();
        let running = store
            .containers()
            .iter()
            .filter(|container| container.state.is_running())
            .count();
        let segment = self.segment;
        let focus_handle = self.focus_handle.clone();
        v_flex()
            .w_full()
            .flex_none()
            .gap(px(8.))
            .pt(px(10.))
            .px(px(12.))
            .pb(px(8.))
            .child(
                h_flex()
                    .w_full()
                    .h(px(28.))
                    .px(px(10.))
                    .gap(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(
                        if self
                            .filter_editor
                            .focus_handle(cx)
                            .contains_focused(window, cx)
                        {
                            colors.border_focused
                        } else {
                            colors.border
                        },
                    )
                    .bg(colors.editor_background)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(text_input(&self.filter_editor, 12., cx)),
                    )
                    .child(
                        KeyBinding::for_action_in(&FocusFilter, &focus_handle, cx)
                            .size(rems_from_px(10.5_f32))
                            .color(Color::Custom(colors.border_selected)),
                    ),
            )
            .child(
                h_flex()
                    .h(px(26.))
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .child(self.render_segment(Segment::All, "Todos", total, segment, cx))
                    .child(self.render_segment(Segment::Running, "Rodando", running, segment, cx))
                    .child(self.render_segment(
                        Segment::Stopped,
                        "Parados",
                        total - running,
                        segment,
                        cx,
                    )),
            )
    }

    fn render_segment(
        &self,
        value: Segment,
        label: &'static str,
        count: usize,
        current: Segment,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active = value == current;
        h_flex()
            .id(label)
            .h(px(20.))
            .px(px(8.))
            .gap(px(6.))
            .rounded(px(5.))
            .cursor_pointer()
            .when(active, |this| this.bg(colors.element_selected))
            .when(!active, |this| {
                this.hover(|this| this.bg(colors.ghost_element_hover))
            })
            .child(
                text(label, 11.5)
                    .weight(if active {
                        FontWeight::BOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .color(if active { Color::Default } else { Color::Muted }),
            )
            .child(mono(count.to_string(), 10.5, Color::Placeholder, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.segment = value;
                cx.notify();
            }))
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
            format!("compose · {visible}")
        } else {
            visible.to_string()
        };
        h_flex()
            .id(SharedString::from(format!("docker-group-{key}")))
            .w_full()
            .h(px(30.))
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
            .child(text(group.title().to_string(), 12.).weight(FontWeight::BOLD))
            .child(mono(count, 10.5, Color::Placeholder, cx))
            .child(div().flex_1())
            .when(is_compose, |this| {
                let restart_group = group.clone();
                let stop_group = group.clone();
                this.child(
                    icon_button(
                        SharedString::from(format!("docker-group-restart-{key}")),
                        IconName::RotateCw,
                        24.,
                        IconSize::Small,
                        false,
                        cx,
                    )
                    .tooltip(Tooltip::text("Reiniciar o compose"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.run_on_group(&restart_group, ContainerAction::Restart, cx);
                    })),
                )
                .child(
                    icon_button(
                        SharedString::from(format!("docker-group-stop-{key}")),
                        IconName::Stop,
                        24.,
                        IconSize::Small,
                        false,
                        cx,
                    )
                    .tooltip(Tooltip::text("Parar o compose"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.run_on_group(&stop_group, ContainerAction::Stop, cx);
                    })),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&key) {
                    this.collapsed_groups.insert(key.clone());
                }
                cx.notify();
            }))
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
        let row = StatusRow::new(
            SharedString::from(format!("docker-container-{}", container.id)),
            status,
            IconName::Box,
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
        .on_click(cx.listener(move |this, _, window, cx| {
            if this.selected.as_deref() == Some(id.as_str()) {
                this.selected = None;
            } else {
                this.selected = Some(id.clone());
            }
            this.focus_handle.focus(window, cx);
            cx.notify();
        }));
        v_flex()
            .w_full()
            .child(row)
            .when(selected, |this| {
                this.child(self.render_quick_actions(container, cx))
            })
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
        h_flex()
            .w_full()
            .gap(px(4.))
            .pt(px(2.))
            .pr(px(10.))
            .pb(px(8.))
            .pl(px(36.))
            .child(
                quick_chip(
                    "docker-quick-terminal",
                    IconName::Terminal,
                    "Terminal",
                    running,
                    cx,
                )
                .tooltip(Tooltip::text(if running {
                    "Abrir um shell dentro do container"
                } else {
                    "O container precisa estar rodando"
                }))
                .when(running, |this| {
                    this.on_click(cx.listener(move |this, _, window, cx| {
                        this.open_terminal(&terminal_container, window, cx)
                    }))
                }),
            )
            .child(
                quick_chip("docker-quick-logs", IconName::Reader, "Logs", true, cx).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.open_logs(&logs_container, window, cx)
                    }),
                ),
            )
            .child(div().flex_1())
            .when(up, |this| {
                this.child(
                    icon_button(
                        "docker-quick-pause",
                        if paused {
                            IconName::PlayFilled
                        } else {
                            IconName::DebugPause
                        },
                        24.,
                        IconSize::XSmall,
                        true,
                        cx,
                    )
                    .tooltip(Tooltip::text(if paused { "Retomar" } else { "Pausar" }))
                    .when(!busy, |this| {
                        this.on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_pause(&pause_container, cx)
                        }))
                    }),
                )
            })
            .child(
                icon_button(
                    "docker-quick-restart",
                    IconName::RotateCw,
                    24.,
                    IconSize::XSmall,
                    true,
                    cx,
                )
                .tooltip(Tooltip::text("Reiniciar"))
                .when(!busy, |this| {
                    this.on_click(cx.listener(move |this, _, _, cx| {
                        this.run(&restart_container, ContainerAction::Restart, cx)
                    }))
                }),
            )
            .child(
                icon_button(
                    "docker-quick-stop",
                    if up {
                        IconName::Stop
                    } else {
                        IconName::PlayFilled
                    },
                    24.,
                    IconSize::XSmall,
                    true,
                    cx,
                )
                .tooltip(Tooltip::text(if up { "Parar" } else { "Iniciar" }))
                .when(!busy, |this| {
                    this.on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_stop(&stop_container, cx)),
                    )
                }),
            )
            .child(self.render_more_menu(container, cx))
            .into_any_element()
    }

    fn render_more_menu(&self, container: &Container, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let container = container.clone();
        let focus_handle = self.focus_handle.clone();
        PopoverMenu::new("docker-container-menu")
            .with_handle(self.menu_handle.clone())
            .anchor(gpui::Anchor::TopRight)
            .trigger(PopoverTriggerArea::new(icon_button(
                "docker-quick-more",
                IconName::Ellipsis,
                24.,
                IconSize::XSmall,
                true,
                cx,
            )))
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
        let (title, summary, rows): (&str, String, Vec<(String, String)>) = match section {
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
                    "IMAGENS",
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
                ("VOLUMES", summary, rows)
            }
            Section::Networks => {
                let rows = store
                    .networks()
                    .iter()
                    .map(|network| (network.name.clone(), network.driver.clone()))
                    .collect();
                ("REDES", store.networks().len().to_string(), rows)
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
                    .id(title)
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
                    .child(caption(title))
                    .child(div().flex_1())
                    .child(mono(summary, 10.5, Color::Placeholder, cx))
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
        if children.is_empty() {
            children.push(
                h_flex()
                    .w_full()
                    .px(px(6.))
                    .py_2()
                    .child(
                        text(
                            if !loaded {
                                "Lendo os containers…"
                            } else if filter.is_empty() && self.segment == Segment::All {
                                "Nenhum container nesta máquina."
                            } else {
                                "Nenhum container com esse filtro."
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
            .child(
                div()
                    .w_full()
                    .h(px(1.))
                    .flex_none()
                    .my(px(4.))
                    .bg(cx.theme().colors().border),
            )
            .child(self.render_section(Section::Images, cx))
            .child(self.render_section(Section::Volumes, cx))
            .child(self.render_section(Section::Networks, cx))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let total = store.containers().len();
        let running: Vec<&Container> = store
            .containers()
            .iter()
            .filter(|container| container.state.is_running())
            .collect();
        let memory: u64 = running
            .iter()
            .filter_map(|container| store.stats(&container.id))
            .map(|stats| stats.memory_bytes)
            .sum();
        let mut summary = format!(
            "{total} {} · {} rodando",
            if total == 1 {
                "container"
            } else {
                "containers"
            },
            running.len()
        );
        if memory > 0 {
            summary.push_str(&format!(" · {}", format_bytes(memory)));
        }
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .w_full()
            .h(px(48.))
            .flex_none()
            .px(px(12.))
            .gap(px(8.))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .flex_1()
                    .child(text(summary, 11.5).color(Color::Placeholder)),
            )
            .child(
                h_flex()
                    .id("docker-new-container")
                    .h(px(28.))
                    .px(px(10.))
                    .gap(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .bg(cx.theme().colors().border_focused)
                    .hover(|this| this.opacity(0.9))
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Custom(cx.theme().colors().text)),
                    )
                    .child(text("Novo container", 12.5).weight(FontWeight::BOLD))
                    .child(
                        KeyBinding::for_action_in(&NewContainer, &focus_handle, cx)
                            .size(rems_from_px(11_f32))
                            .color(Color::Custom(cx.theme().colors().text.opacity(0.6))),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.workspace
                            .update(cx, |workspace, cx| {
                                new_container::open(workspace, window, cx)
                            })
                            .log_err();
                    })),
            )
    }

    fn render_unavailable(&self, socket: &Path, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
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
        v_flex()
            .flex_1()
            .w_full()
            .px(px(32.))
            .gap(px(14.))
            .items_center()
            .justify_center()
            .child(
                h_flex()
                    .size(px(56.))
                    .justify_center()
                    .rounded(px(14.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .child(Icon::new(IconName::Server).size(IconSize::Custom(rems_from_px(24_f32)))),
            )
            .child(text("O Docker não está rodando", 14.).weight(FontWeight::BOLD))
            .child(
                div().w(px(300.)).child(
                    text(
                        format!(
                            "Nenhum daemon respondeu em {}. Abra o Docker Desktop ou aponte para o socket do Colima ou do OrbStack.",
                            discovery::home_relative(socket)
                        ),
                        12.5,
                    )
                    .color(Color::Muted)
                    .text_center(),
                ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .pt(px(4.))
                    .child(
                        h_flex()
                            .id("docker-retry")
                            .h(px(28.))
                            .px(px(10.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.border_selected)
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.ghost_element_hover))
                            .child(
                                text("Tentar de novo", 12.5)
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Custom(colors.editor_foreground)),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.reconnect(cx));
                            })),
                    )
                    .when(desktop, |this| {
                        this.child(
                            h_flex()
                                .id("docker-open-desktop")
                                .h(px(28.))
                                .px(px(10.))
                                .rounded(px(6.))
                                .bg(colors.border_focused)
                                .cursor_pointer()
                                .hover(|this| this.opacity(0.9))
                                .child(text("Abrir Docker Desktop", 12.5).weight(FontWeight::BOLD))
                                .on_click(cx.listener(|this, _, _, cx| this.open_docker_desktop(cx))),
                        )
                    }),
            )
            .when(!others.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w(px(300.))
                        .gap(px(6.))
                        .p(px(12.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .child(caption("OUTRO RUNTIME"))
                        .children(others.into_iter().enumerate().map(|(index, candidate)| {
                            let path = candidate.path.clone();
                            h_flex()
                                .w_full()
                                .h(px(26.))
                                .gap(px(8.))
                                .child(dot(
                                    6.,
                                    if candidate.exists {
                                        Color::Success.color(cx)
                                    } else {
                                        colors.border_selected
                                    },
                                ))
                                .child(text(candidate.runtime.clone(), 12.).weight(FontWeight::MEDIUM))
                                .child(
                                    div().flex_1().min_w_0().child(
                                        mono(candidate.short_path(), 10.5, Color::Placeholder, cx)
                                            .truncate(),
                                    ),
                                )
                                .child(if candidate.exists {
                                    div()
                                        .id(("docker-use-socket", index))
                                        .cursor_pointer()
                                        .child(text("Usar", 11.5).color(Color::Accent))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let path = path.clone();
                                            this.store
                                                .update(cx, |store, cx| store.use_socket(path, cx));
                                        }))
                                        .into_any_element()
                                } else {
                                    text("não achado", 11.5)
                                        .color(Color::Placeholder)
                                        .into_any_element()
                                })
                        })),
                )
            })
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

fn project_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
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
            usage.unwrap_or_else(|| container.health().unwrap_or_default().to_string()),
        ),
        ContainerState::Restarting => (
            RowStatus::Failing,
            match restart_count {
                Some(count) if count > 0 => format!("reiniciando · {count}×"),
                _ => "reiniciando".to_string(),
            },
        ),
        ContainerState::Paused => (RowStatus::Paused, "pausado".to_string()),
        ContainerState::Exited | ContainerState::Dead => {
            let mut state = match container.exit_code() {
                Some(code) => format!("saiu ({code})"),
                None => container.state.label().to_string(),
            };
            if let Some(age) = container.status_age().and_then(pt_age) {
                state.push_str(&format!(" · há {age}"));
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
pub(crate) fn pt_age(age: &str) -> Option<String> {
    let age = age.trim().to_lowercase();
    if age.starts_with("less than") {
        return Some("instantes".into());
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
    Some(match unit {
        "second" => format!("{count} s"),
        "minute" => format!("{count} min"),
        "hour" => format!("{count} h"),
        "day" => format!("{count} d"),
        "week" => format!("{count} sem"),
        "month" => format!("{count} {}", if count == 1 { "mês" } else { "meses" }),
        "year" => format!("{count} {}", if count == 1 { "ano" } else { "anos" }),
        _ => return None,
    })
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

/// The boards' section titles: 10.5 bold caps, spaced.
pub(crate) fn caption(label: impl Into<SharedString>) -> Label {
    text(label, 10.5)
        .weight(FontWeight::BOLD)
        .color(Color::Placeholder)
}

pub(crate) fn dot(size: f32, color: gpui::Hsla) -> Div {
    div().size(px(size)).flex_none().rounded_full().bg(color)
}

fn chip(cx: &App) -> Div {
    h_flex()
        .py(px(2.))
        .px(px(6.))
        .rounded(px(4.))
        .bg(cx.theme().colors().editor_background)
}

/// The boards' square icon buttons: 28 px in headers, 24 px in rows, bordered next to a row.
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    size: f32,
    icon_size: IconSize,
    bordered: bool,
    cx: &App,
) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .size(px(size))
        .flex_none()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when(bordered, |this| this.border_1().border_color(colors.border))
        .hover(|this| this.bg(colors.ghost_element_hover))
        .child(Icon::new(icon).size(icon_size))
}

fn quick_chip(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    enabled: bool,
    cx: &App,
) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .h(px(24.))
        .px(px(8.))
        .gap(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(colors.border)
        .bg(colors.elevated_surface_background)
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(colors.ghost_element_hover))
        })
        .when(!enabled, |this| this.opacity(0.5))
        .child(Icon::new(icon).size(IconSize::XSmall))
        .child(
            text(label, 11.5)
                .weight(FontWeight::MEDIUM)
                .color(Color::Custom(colors.editor_foreground)),
        )
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
        let status_color = match status {
            RowStatus::Running => Color::Success.color(cx),
            RowStatus::Paused => Color::Warning.color(cx),
            RowStatus::Failing => Color::Error.color(cx),
            RowStatus::Stopped => cx.theme().colors().border_selected,
        };
        let port = container
            .published_ports()
            .first()
            .map(|(public, _)| *public);

        let entry = |label: String,
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
                    .child(dot(6., status_color))
                    .child(text(name.clone(), 12.5).weight(FontWeight::BOLD))
                    .child(mono(short_id.clone(), 10.5, Color::Placeholder, cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("docker-copy-id", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Copiar o id"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(full_id.clone()))
                            }),
                    )
                    .into_any_element()
            })
            .separator()
            .header("Interagir");
        if running {
            menu = menu.item(entry(
                "Abrir terminal (sh)".into(),
                IconName::Terminal,
                Box::new(OpenTerminal),
                Box::new(|panel, container, window, cx| panel.open_terminal(container, window, cx)),
            ));
        }
        menu = menu
            .item(entry(
                "Ver logs".into(),
                IconName::Reader,
                Box::new(OpenLogs),
                Box::new(|panel, container, window, cx| panel.open_logs(container, window, cx)),
            ))
            .item(entry(
                "Inspecionar".into(),
                IconName::Eye,
                Box::new(Inspect),
                Box::new(|panel, container, window, cx| panel.inspect(container, window, cx)),
            ));
        if let Some(port) = port.filter(|_| running) {
            menu = menu.item(
                ContextMenuEntry::new(format!("Abrir localhost:{port}"))
                    .icon(IconName::ArrowUpRight)
                    .icon_color(Color::Muted)
                    .handler(move |_, cx| cx.open_url(&format!("http://localhost:{port}"))),
            );
        }
        menu = menu.separator().header("Ciclo de vida");
        if up {
            menu = menu.item(entry(
                if paused { "Retomar" } else { "Pausar" }.into(),
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
                "Reiniciar".into(),
                IconName::RotateCw,
                Box::new(Restart),
                Box::new(|panel, container, _, cx| {
                    panel.run(container, ContainerAction::Restart, cx)
                }),
            ))
            .item(entry(
                if up { "Parar" } else { "Iniciar" }.into(),
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
                ContextMenuEntry::new(format!(
                    "Perguntar ao Agent sobre {}",
                    container.display_name()
                ))
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
                            .child(text("Remover container…", 12.5).color(Color::Error)),
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
        "\\033[32m●\\033[0m \\033[1mDentro de {}\\033[0m  \\033[2m{} · usuário {} · {}  —  sai com exit ou ^D; o container continua rodando\\033[0m\\n\\n",
        shell_escape(&name),
        shell_escape(&container.image),
        shell_escape(&user),
        shell_escape(&working_dir),
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
        assert_eq!(pt_age("2 hours").as_deref(), Some("2 h"));
        assert_eq!(pt_age("About an hour").as_deref(), Some("1 h"));
        assert_eq!(pt_age("8 minutes").as_deref(), Some("8 min"));
        assert_eq!(pt_age("Less than a second").as_deref(), Some("instantes"));
        assert_eq!(pt_age("3 months").as_deref(), Some("3 meses"));
    }
}
