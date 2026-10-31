//! "Novo container": runs an image with ports, env, a volume and a network, showing the
//! equivalent `docker run`, or adds the same as a service to the project's docker-compose.yml
//! and brings only that service up.

use crate::{
    CreateContainer as CreateAction,
    client::{CreateContainer, CreateHostConfig, DockerError, PortBinding, RestartPolicy},
    format_bytes,
    panel::{caption, mono, mono_input, text},
    store::DockerStore,
};
use collections::HashMap;
use editor::{Editor, EditorEvent};
use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Subscription, Task, WeakEntity, px,
};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use ui::{
    CommonAnimationExt as _, ContextMenu, KeyBinding, PopoverMenu, PopoverTriggerArea, Tooltip,
    modal_surface, prelude::*, toggle_switch,
};
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];
const ENV_FILES: [&str; 4] = [".env.test", ".env.local", ".env.development", ".env"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Image,
    ComposeService,
}

struct EnvRow {
    key: Entity<Editor>,
    value: Entity<Editor>,
    revealed: bool,
}

enum Progress {
    Idle,
    Working(SharedString),
    Failed(SharedString),
}

pub struct NewContainerModal {
    workspace: WeakEntity<Workspace>,
    store: Entity<DockerStore>,
    project_root: Option<PathBuf>,
    focus_handle: FocusHandle,
    mode: Mode,
    image: Entity<Editor>,
    name: Entity<Editor>,
    host_port: Entity<Editor>,
    container_port: Entity<Editor>,
    env: Vec<EnvRow>,
    env_file: Option<PathBuf>,
    volume: Entity<Editor>,
    network: Option<String>,
    start_now: bool,
    remove_on_stop: bool,
    restart_always: bool,
    progress: Progress,
    create_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for NewContainerModal {}

impl ModalView for NewContainerModal {}

impl Focusable for NewContainerModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.image.focus_handle(cx)
    }
}

fn single_line(placeholder: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor
    })
}

fn is_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    ["PASSWORD", "SECRET", "TOKEN", "KEY", "PASS"]
        .iter()
        .any(|word| key.contains(word))
}

impl NewContainerModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        project_root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = DockerStore::global(cx);
        let image = single_line("postgres:16-alpine", window, cx);
        let name = single_line("opcional", window, cx);
        let host_port = single_line("host", window, cx);
        let container_port = single_line("container", window, cx);
        let volume = single_line("volume:/caminho/no/container", window, cx);
        let mut subscriptions = Vec::new();
        for editor in [&image, &name, &host_port, &container_port, &volume] {
            subscriptions.push(cx.subscribe(editor, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited { .. }) {
                    cx.notify();
                }
            }));
        }
        let env_file = project_root.as_ref().and_then(|root| {
            ENV_FILES
                .iter()
                .map(|file| root.join(file))
                .find(|path| path.is_file())
        });
        let network = store.read(cx).networks().iter().find_map(|network| {
            let project = project_root
                .as_ref()?
                .file_name()?
                .to_string_lossy()
                .to_lowercase();
            (network.name == format!("{project}_default")).then(|| network.name.clone())
        });
        let mut this = Self {
            workspace,
            store,
            project_root,
            focus_handle: cx.focus_handle(),
            mode: Mode::Image,
            image,
            name,
            host_port,
            container_port,
            env: Vec::new(),
            env_file,
            volume,
            network,
            start_now: true,
            remove_on_stop: false,
            restart_always: false,
            progress: Progress::Idle,
            create_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.push_env_row(window, cx);
        this
    }

    fn push_env_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = single_line("CHAVE", window, cx);
        let value = single_line("valor", window, cx);
        self._subscriptions.push(cx.subscribe_in(
            &key,
            window,
            |this, editor, event: &EditorEvent, window, cx| {
                if !matches!(event, EditorEvent::BufferEdited { .. }) {
                    return;
                }
                let is_last = this
                    .env
                    .last()
                    .is_some_and(|row| row.key.entity_id() == editor.entity_id());
                let key_text = editor.read(cx).text(cx);
                if is_last && !key_text.trim().is_empty() {
                    this.push_env_row(window, cx);
                }
                if let Some(row) = this
                    .env
                    .iter_mut()
                    .find(|row| row.key.entity_id() == editor.entity_id())
                {
                    let masked = is_secret(&key_text) && !row.revealed;
                    row.value
                        .update(cx, |value, cx| value.set_masked(masked, cx));
                }
                cx.notify();
            },
        ));
        self._subscriptions
            .push(cx.subscribe(&value, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited { .. }) {
                    cx.notify();
                }
            }));
        self.env.push(EnvRow {
            key,
            value,
            revealed: false,
        });
    }

    fn load_env_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.env_file.clone() else {
            return;
        };
        let entries: Vec<(String, String)> = match dotenvy::from_path_iter(&path) {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(error) => {
                self.progress =
                    Progress::Failed(format!("{} não abriu: {error}", file_name(&path)).into());
                cx.notify();
                return;
            }
        };
        self.env
            .retain(|row| !row.key.read(cx).text(cx).trim().is_empty());
        for (key, value) in entries {
            if self
                .env
                .iter()
                .any(|row| row.key.read(cx).text(cx).trim() == key)
            {
                continue;
            }
            self.push_env_row(window, cx);
            if let Some(row) = self.env.last() {
                let masked = is_secret(&key);
                row.key
                    .update(cx, |editor, cx| editor.set_text(key, window, cx));
                row.value.update(cx, |editor, cx| {
                    editor.set_text(value, window, cx);
                    editor.set_masked(masked, cx);
                });
            }
        }
        self.push_env_row(window, cx);
        cx.notify();
    }

    fn env_values(&self, cx: &App) -> Vec<(String, String)> {
        self.env
            .iter()
            .filter_map(|row| {
                let key = row.key.read(cx).text(cx).trim().to_string();
                (!key.is_empty()).then(|| (key, row.value.read(cx).text(cx)))
            })
            .collect()
    }

    fn field(editor: &Entity<Editor>, cx: &App) -> String {
        editor.read(cx).text(cx).trim().to_string()
    }

    fn ports(&self, cx: &App) -> Option<(u16, u16)> {
        let container = Self::field(&self.container_port, cx).parse::<u16>().ok()?;
        let host = match Self::field(&self.host_port, cx) {
            host if host.is_empty() => container,
            host => host.parse::<u16>().ok()?,
        };
        Some((host, container))
    }

    /// Who already publishes the host port, or, when the host port was moved off the
    /// container's own, who has that one.
    fn port_note(&self, cx: &App) -> Option<(String, bool)> {
        let (host, container) = self.ports(cx)?;
        let store = self.store.read(cx);
        let owner = |port: u16| {
            store.containers().iter().find(|candidate| {
                candidate.state.is_up()
                    && candidate
                        .published_ports()
                        .iter()
                        .any(|(public, _)| *public == port)
            })
        };
        let describe = |container: &crate::Container| match container.compose_project() {
            Some(_) => format!("pelo {} do compose", container.display_name()),
            None => format!("pelo {}", container.display_name()),
        };
        if let Some(conflict) = owner(host) {
            return Some((
                format!("{host} já está em uso {}", describe(conflict)),
                true,
            ));
        }
        if host != container
            && let Some(owner) = owner(container)
        {
            return Some((
                format!("{container} já está em uso {}", describe(owner)),
                false,
            ));
        }
        None
    }

    fn local_image(&self, cx: &App) -> Option<i64> {
        let image = Self::field(&self.image, cx);
        if image.is_empty() {
            return None;
        }
        let wanted = if image.contains(':') {
            image
        } else {
            format!("{image}:latest")
        };
        self.store
            .read(cx)
            .images()
            .iter()
            .find(|candidate| candidate.tags().any(|tag| tag == wanted))
            .map(|image| image.size)
    }

    fn spec(&self, cx: &App) -> CreateContainer {
        let mut spec = CreateContainer {
            image: Self::field(&self.image, cx),
            env: self
                .env_values(cx)
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
            exposed_ports: HashMap::default(),
            host_config: CreateHostConfig {
                port_bindings: HashMap::default(),
                binds: Vec::new(),
                auto_remove: self.remove_on_stop,
                restart_policy: self.restart_always.then(|| RestartPolicy {
                    name: "always".into(),
                }),
                network_mode: self.network.clone(),
            },
        };
        if let Some((host, container)) = self.ports(cx) {
            let key = format!("{container}/tcp");
            spec.exposed_ports
                .insert(key.clone(), serde_json::json!({}));
            spec.host_config.port_bindings.insert(
                key,
                vec![PortBinding {
                    host_port: host.to_string(),
                }],
            );
        }
        let volume = Self::field(&self.volume, cx);
        if !volume.is_empty() {
            spec.host_config.binds.push(volume);
        }
        spec
    }

    /// What `docker run` would take, secrets as `***`.
    fn command(&self, cx: &App) -> String {
        let mut parts = vec!["docker run".to_string()];
        if self.start_now {
            parts.push("-d".into());
        }
        if self.remove_on_stop {
            parts.push("--rm".into());
        }
        if self.restart_always {
            parts.push("--restart always".into());
        }
        let name = Self::field(&self.name, cx);
        if !name.is_empty() {
            parts.push(format!("--name {name}"));
        }
        if let Some((host, container)) = self.ports(cx) {
            parts.push(format!("-p {host}:{container}"));
        }
        for (key, value) in self.env_values(cx) {
            let value = if is_secret(&key) {
                "***".to_string()
            } else {
                value
            };
            parts.push(format!("-e {key}={value}"));
        }
        let volume = Self::field(&self.volume, cx);
        if !volume.is_empty() {
            parts.push(format!("-v {volume}"));
        }
        if let Some(network) = &self.network {
            parts.push(format!("--network {network}"));
        }
        let image = Self::field(&self.image, cx);
        parts.push(if image.is_empty() {
            "<imagem>".into()
        } else {
            image
        });
        if !self.start_now {
            parts[0] = "docker create".into();
        }
        parts.join(" ")
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.progress, Progress::Working(_)) {
            return;
        }
        let image = Self::field(&self.image, cx);
        if image.is_empty() {
            self.progress = Progress::Failed("Escolha uma imagem.".into());
            cx.notify();
            return;
        }
        if !Self::field(&self.host_port, cx).is_empty()
            || !Self::field(&self.container_port, cx).is_empty()
        {
            if self.ports(cx).is_none() {
                self.progress =
                    Progress::Failed("As portas precisam ser números de 1 a 65535.".into());
                cx.notify();
                return;
            }
        }
        match self.mode {
            Mode::Image => self.create_from_image(image, cx),
            Mode::ComposeService => self.add_compose_service(image, window, cx),
        }
    }

    fn create_from_image(&mut self, image: String, cx: &mut Context<Self>) {
        let Some(docker) = self.store.read(cx).docker().cloned() else {
            self.progress = Progress::Failed("O Docker não está conectado.".into());
            cx.notify();
            return;
        };
        let spec = self.spec(cx);
        let name = Self::field(&self.name, cx);
        let start_now = self.start_now;
        self.progress = Progress::Working("Criando…".into());
        cx.notify();
        self.create_task = cx.spawn(async move |this, cx| {
            let name_arg = (!name.is_empty()).then_some(name.as_str());
            let created = match docker.create(name_arg, &spec).await {
                Err(DockerError::NotFound(_)) => {
                    let (sender, mut receiver) = mpsc::unbounded_channel();
                    let pull = docker.pull(&image, sender);
                    let report = async {
                        while let Some(event) = receiver.recv().await {
                            let detail = match event
                                .progress_detail
                                .as_ref()
                                .and_then(|detail| Some((detail.current?, detail.total?)))
                            {
                                Some((current, total)) if total > 0 => {
                                    format!("Baixando {image}… {}%", current * 100 / total)
                                }
                                _ => format!("Baixando {image}…"),
                            };
                            this.update(cx, |this, cx| {
                                this.progress = Progress::Working(detail.into());
                                cx.notify();
                            })
                            .ok();
                        }
                    };
                    let (pulled, ()) = futures::join!(pull, report);
                    match pulled {
                        Ok(()) => docker.create(name_arg, &spec).await,
                        Err(error) => Err(error),
                    }
                }
                other => other,
            };
            let result = match created {
                Ok(id) if start_now => docker.start(&id).await.map(|()| id),
                other => other,
            };
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    let label = if name.is_empty() {
                        image.clone()
                    } else {
                        name.clone()
                    };
                    this.finish(
                        format!(
                            "{label} {}",
                            if start_now {
                                "criado e iniciado"
                            } else {
                                "criado"
                            }
                        ),
                        cx,
                    );
                }
                Err(error) => {
                    this.progress = Progress::Failed(format!("{error}").into());
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn finish(&mut self, message: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.refresh(cx);
            store.refresh_resources(cx);
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<NewContainerModal>(), message).autohide(),
                    cx,
                );
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn compose_file(&self) -> Option<PathBuf> {
        let root = self.project_root.as_ref()?;
        COMPOSE_FILES
            .iter()
            .rev()
            .map(|file| root.join(file))
            .find(|path| path.is_file())
    }

    /// Writes the service into the compose file (creating `docker-compose.yml` when the project
    /// has none) and brings only that service up in a terminal.
    fn add_compose_service(&mut self, image: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.project_root.clone() else {
            self.progress =
                Progress::Failed("Abra uma pasta de projeto para usar o compose.".into());
            cx.notify();
            return;
        };
        let service = Self::field(&self.name, cx);
        if service.is_empty()
            || !service.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            self.progress =
                Progress::Failed("Dê um nome ao serviço (letras, números, - e _).".into());
            cx.notify();
            return;
        }
        let path = self
            .compose_file()
            .unwrap_or_else(|| root.join("docker-compose.yml"));
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let service_spec = ComposeService {
            name: service.clone(),
            image,
            ports: self.ports(cx),
            env: self.env_values(cx),
            volume: Some(Self::field(&self.volume, cx)).filter(|volume| !volume.is_empty()),
            restart_always: self.restart_always,
        };
        let updated = match add_service(&existing, &service_spec) {
            Ok(updated) => updated,
            Err(message) => {
                self.progress = Progress::Failed(message.into());
                cx.notify();
                return;
            }
        };
        if let Err(error) = std::fs::write(&path, updated) {
            self.progress = Progress::Failed(
                format!("{} não pôde ser salvo: {error}", file_name(&path)).into(),
            );
            cx.notify();
            return;
        }
        let start_now = self.start_now;
        let socket = self
            .store
            .read(cx)
            .docker()
            .map(|docker| docker.socket().to_path_buf());
        self.workspace
            .update(cx, |workspace, cx| {
                if start_now {
                    let mut env = HashMap::default();
                    if let Some(socket) = socket {
                        env.insert(
                            "DOCKER_HOST".to_string(),
                            format!("unix://{}", socket.display()),
                        );
                    }
                    let task = task::SpawnInTerminal {
                        id: task::TaskId(format!("docker-compose-up-{service}")),
                        full_label: format!("compose · {service}"),
                        label: format!("compose · {service}"),
                        command: Some("docker".into()),
                        args: vec![
                            "compose".into(),
                            "-f".into(),
                            path.to_string_lossy().into_owned(),
                            "up".into(),
                            "-d".into(),
                            service.clone(),
                        ],
                        command_label: format!("docker compose up -d {service}"),
                        cwd: Some(root),
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
                    };
                    workspace.spawn_in_terminal(task, window, cx).detach();
                }
            })
            .log_err();
        self.finish(
            format!("Serviço {service} escrito em {}", file_name(&path)),
            cx,
        );
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_field(
        &self,
        label: &str,
        editor: &Entity<Editor>,
        width: Option<f32>,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap(px(6.))
            .map(|this| match width {
                Some(width) => this.w(px(width)).flex_none(),
                None => this.flex_1().min_w_0(),
            })
            .child(field_label(label))
            .child(
                input_box(editor, 32., window, cx)
                    .child(div().flex_1().child(mono_input(editor, 12.5, cx))),
            )
    }

    fn render_mode(
        &self,
        value: Mode,
        icon: IconName,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let active = self.mode == value;
        h_flex()
            .id(label)
            .h(px(24.))
            .px(px(10.))
            .gap(px(6.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |this| this.bg(colors.element_selected))
            .when(!active, |this| {
                this.hover(|this| this.bg(colors.ghost_element_hover))
            })
            .child(Icon::new(icon).size(IconSize::XSmall).color(if active {
                Color::Default
            } else {
                Color::Muted
            }))
            .child(
                text(label, 12.)
                    .weight(if active {
                        FontWeight::BOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .color(if active { Color::Default } else { Color::Muted }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.mode = value;
                cx.notify();
            }))
    }

    fn render_option(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        toggle: fn(&mut Self),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .gap(px(8.))
            .cursor_pointer()
            .child(toggle_switch(
                SharedString::from(format!("{id}-switch")),
                on,
                true,
                cx,
            ))
            .child(text(label, 12.).color(Color::Custom(cx.theme().colors().editor_foreground)))
            .on_click(cx.listener(move |this, _, _, cx| {
                toggle(this);
                cx.notify();
            }))
    }

    fn render_env(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let env_file = self.env_file.clone();
        v_flex()
            .w_full()
            .gap(px(6.))
            .child(
                h_flex()
                    .w_full()
                    .child(div().flex_1().child(field_label("Variáveis de ambiente")))
                    .when_some(env_file, |this, path| {
                        this.child(
                            div()
                                .id("docker-load-env")
                                .cursor_pointer()
                                .child(
                                    text(format!("Carregar de {}", file_name(&path)), 11.5)
                                        .color(Color::Accent),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.load_env_file(window, cx)
                                })),
                        )
                    }),
            )
            .children(self.env.iter().enumerate().map(|(index, row)| {
                let secret = is_secret(&row.key.read(cx).text(cx));
                let revealed = row.revealed;
                h_flex()
                    .w_full()
                    .gap(px(8.))
                    .child(
                        input_box(&row.key, 30., window, cx)
                            .w(px(200.))
                            .flex_none()
                            .child(div().flex_1().child(env_key_input(&row.key, cx))),
                    )
                    .child(
                        input_box(&row.value, 30., window, cx)
                            .flex_1()
                            .gap(px(8.))
                            .child(div().flex_1().child(mono_input(&row.value, 12., cx)))
                            .when(secret, |this| {
                                this.child(
                                    IconButton::new(
                                        ("docker-env-reveal", index),
                                        if revealed {
                                            IconName::Eye
                                        } else {
                                            IconName::EyeOff
                                        },
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text(if revealed {
                                        "Esconder"
                                    } else {
                                        "Mostrar"
                                    }))
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if let Some(row) = this.env.get_mut(index) {
                                                row.revealed = !row.revealed;
                                                let masked = !row.revealed;
                                                row.value.update(cx, |value, cx| {
                                                    value.set_masked(masked, cx)
                                                });
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                            }),
                    )
            }))
    }

    fn render_network(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let networks: Vec<String> = self
            .store
            .read(cx)
            .networks()
            .iter()
            .map(|network| network.name.clone())
            .collect();
        let this = cx.weak_entity();
        let current = self.network.clone().unwrap_or_else(|| "bridge".into());
        v_flex()
            .w(px(200.))
            .flex_none()
            .gap(px(6.))
            .child(field_label("Rede"))
            .child(
                PopoverMenu::new("docker-network")
                    .trigger(PopoverTriggerArea::new(
                        select_box("docker-network-trigger", cx)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(mono(current, 12., Color::Default, cx).truncate()),
                            )
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ))
                    .menu(move |window, cx| {
                        let this = this.clone();
                        let networks = networks.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for network in networks {
                                let this = this.clone();
                                let label = network.clone();
                                menu = menu.entry(label, None, move |_, cx| {
                                    let network = network.clone();
                                    this.update(cx, |this, cx| {
                                        this.network = (network != "bridge").then_some(network);
                                        cx.notify();
                                    })
                                    .log_err();
                                });
                            }
                            menu
                        }))
                    }),
            )
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn field_label(label: &str) -> Label {
    text(label.to_string(), 11.5)
        .weight(FontWeight::MEDIUM)
        .color(Color::Muted)
}

fn input_box(editor: &Entity<Editor>, height: f32, window: &Window, cx: &App) -> Div {
    let colors = cx.theme().colors();
    let focused = editor.focus_handle(cx).contains_focused(window, cx);
    h_flex()
        .w_full()
        .h(px(height))
        .px(px(10.))
        .rounded(px(6.))
        .border_1()
        .border_color(if focused {
            colors.border_focused
        } else {
            colors.border
        })
        .bg(colors.editor_background)
}

fn select_box(id: &'static str, cx: &App) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .w_full()
        .h(px(32.))
        .px(px(10.))
        .gap(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .hover(|this| this.bg(colors.ghost_element_hover))
}

/// Env keys are drawn in the accent, as in the board.
fn env_key_input(editor: &Entity<Editor>, cx: &App) -> AnyElement {
    let accent = Color::Accent.color(cx);
    div()
        .text_color(accent)
        .child(mono_input(editor, 12., cx))
        .into_any_element()
}

struct ComposeService {
    name: String,
    image: String,
    ports: Option<(u16, u16)>,
    env: Vec<(String, String)>,
    volume: Option<String>,
    restart_always: bool,
}

/// Adds the service at the end of `services:` (and a named volume under the top-level
/// `volumes:`), keeping the file's own indentation. Refuses a name that is already a service.
fn add_service(existing: &str, service: &ComposeService) -> Result<String, String> {
    let lines: Vec<&str> = existing.lines().collect();
    let top_level = |line: &str| {
        !line.is_empty() && !line.starts_with([' ', '\t', '#']) && line.trim_end().ends_with(':')
    };
    let services_start = lines.iter().position(|line| line.trim_end() == "services:");
    let indent = services_start
        .and_then(|start| {
            lines[start + 1..]
                .iter()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line.len() - line.trim_start().len())
        })
        .filter(|indent| *indent > 0)
        .unwrap_or(2);
    let pad = " ".repeat(indent);
    if let Some(start) = services_start {
        let taken = lines[start + 1..]
            .iter()
            .take_while(|line| !top_level(line))
            .any(|line| line.trim_end() == format!("{pad}{}:", service.name));
        if taken {
            return Err(format!("Já existe um serviço {} no compose.", service.name));
        }
    }

    let mut block = vec![format!("{pad}{}:", service.name)];
    block.push(format!("{pad}{pad}image: {}", service.image));
    if let Some((host, container)) = service.ports {
        block.push(format!("{pad}{pad}ports: [\"{host}:{container}\"]"));
    }
    if !service.env.is_empty() {
        block.push(format!("{pad}{pad}environment:"));
        for (key, value) in &service.env {
            block.push(format!("{pad}{pad}{pad}{key}: {}", yaml_scalar(value)));
        }
    }
    if let Some(volume) = &service.volume {
        block.push(format!("{pad}{pad}volumes: [{volume}]"));
    }
    if service.restart_always {
        block.push(format!("{pad}{pad}restart: always"));
    }

    let mut output: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    match services_start {
        Some(start) => {
            let mut end = lines[start + 1..]
                .iter()
                .position(|line| top_level(line))
                .map_or(lines.len(), |offset| start + 1 + offset);
            while end > start + 1 && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            output.splice(end..end, block);
        }
        None => {
            if output.last().is_some_and(|line| !line.trim().is_empty()) {
                output.push(String::new());
            }
            output.push("services:".into());
            output.extend(block);
        }
    }

    let named_volume = service
        .volume
        .as_ref()
        .and_then(|volume| volume.split(':').next())
        .filter(|name| !name.is_empty() && !name.starts_with(['.', '/', '~']));
    if let Some(name) = named_volume {
        let volumes_start = output.iter().position(|line| line.trim_end() == "volumes:");
        match volumes_start {
            Some(start) => {
                let end = output[start + 1..]
                    .iter()
                    .position(|line| top_level(line))
                    .map_or(output.len(), |offset| start + 1 + offset);
                let declared = output[start + 1..end]
                    .iter()
                    .any(|line| line.trim_end().trim_end_matches(':').trim() == name);
                if !declared {
                    let mut insert_at = end;
                    while insert_at > start + 1 && output[insert_at - 1].trim().is_empty() {
                        insert_at -= 1;
                    }
                    output.insert(insert_at, format!("{pad}{name}:"));
                }
            }
            None => {
                output.push("volumes:".into());
                output.push(format!("{pad}{name}:"));
            }
        }
    }
    let mut text = output.join("\n");
    text.push('\n');
    Ok(text)
}

fn yaml_scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-./@".contains(character))
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null"
        )
        && value.parse::<f64>().is_err();
    if plain {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

impl Render for NewContainerModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let port_note = self.port_note(cx);
        let local_image = self.local_image(cx);
        let image_text = Self::field(&self.image, cx);
        let command = self.command(cx);
        let image_focused = self.image.focus_handle(cx).contains_focused(window, cx);
        let compose = self.mode == Mode::ComposeService;
        let primary_label = match (compose, self.start_now) {
            (true, true) => "Escrever e subir",
            (true, false) => "Escrever no compose",
            (false, true) => "Criar e iniciar",
            (false, false) => "Criar",
        };
        let focus_handle = self.focus_handle.clone();
        modal_surface(cx)
            .key_context("DockerNewContainer")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &CreateAction, window, cx| this.create(window, cx)))
            .w(px(580.))
            .bg(colors.elevated_surface_background)
            .child(
                h_flex()
                    .w_full()
                    .p(px(20.))
                    .gap(px(12.))
                    .items_start()
                    .child(
                        h_flex()
                            .size(px(36.))
                            .flex_none()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(Color::Accent.color(cx).opacity(0.16))
                            .child(Icon::new(IconName::Box).size(IconSize::Medium).color(Color::Accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(4.))
                            .child(text("Novo container", 14.).weight(FontWeight::BOLD))
                            .child(
                                text(
                                    "Roda uma imagem com as opções abaixo. Para virar serviço do projeto, use \"Serviço do compose\": o Asylum escreve no docker-compose.yml.",
                                    12.,
                                )
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        IconButton::new("docker-new-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(px(14.))
                    .px(px(20.))
                    .pb(px(18.))
                    .child(
                        h_flex()
                            .h(px(30.))
                            .p(px(2.))
                            .gap(px(2.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(self.render_mode(Mode::Image, IconName::Box, "Da imagem", cx))
                            .child(self.render_mode(
                                Mode::ComposeService,
                                IconName::Blocks,
                                "Serviço do compose",
                                cx,
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(12.))
                            .items_start()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(6.))
                                    .child(field_label("Imagem"))
                                    .child(
                                        input_box(&self.image, 32., window, cx)
                                            .gap(px(8.))
                                            .when(image_focused, |this| this.border_color(colors.border_focused))
                                            .child(
                                                Icon::new(IconName::CloudDownload)
                                                    .size(IconSize::Small)
                                                    .color(Color::Muted),
                                            )
                                            .child(div().flex_1().child(mono_input(&self.image, 12.5, cx)))
                                            .when(!image_text.is_empty(), |this| {
                                                this.child(
                                                    h_flex()
                                                        .py(px(2.))
                                                        .px(px(6.))
                                                        .rounded(px(4.))
                                                        .bg(colors.element_selected)
                                                        .child(match local_image {
                                                            Some(size) => mono(
                                                                format!("local · {}", format_bytes(size.max(0) as u64)),
                                                                10.5,
                                                                Color::Success,
                                                                cx,
                                                            ),
                                                            None => mono("baixa ao criar", 10.5, Color::Placeholder, cx),
                                                        }),
                                                )
                                            }),
                                    ),
                            )
                            .child(self.render_field(
                                if compose { "Serviço" } else { "Nome" },
                                &self.name,
                                Some(200.),
                                window,
                                cx,
                            )),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .gap(px(6.))
                            .child(field_label("Portas · host → container"))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap(px(8.))
                                    .child(
                                        input_box(&self.host_port, 32., window, cx)
                                            .w(px(96.))
                                            .flex_none()
                                            .child(div().flex_1().child(mono_input(&self.host_port, 12.5, cx))),
                                    )
                                    .child(Icon::new(IconName::ArrowRight).size(IconSize::Small))
                                    .child(
                                        input_box(&self.container_port, 32., window, cx)
                                            .w(px(96.))
                                            .flex_none()
                                            .child(div().flex_1().child(mono_input(&self.container_port, 12.5, cx))),
                                    )
                                    .when_some(port_note, |this, (note, conflict)| {
                                        this.child(
                                            div().flex_1().min_w_0().child(
                                                text(note, 11.5).color(if conflict {
                                                    Color::Warning
                                                } else {
                                                    Color::Placeholder
                                                }),
                                            ),
                                        )
                                    }),
                            ),
                    )
                    .child(self.render_env(window, cx))
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(12.))
                            .items_start()
                            .child(self.render_field("Volume", &self.volume, None, window, cx))
                            .when(!compose, |this| this.child(self.render_network(cx))),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(20.))
                            .pt(px(2.))
                            .child(self.render_option("docker-start-now", "Iniciar agora", self.start_now, |this| this.start_now = !this.start_now, cx))
                            .when(!compose, |this| {
                                this.child(self.render_option(
                                    "docker-remove-on-stop",
                                    "Remover ao parar (--rm)",
                                    self.remove_on_stop,
                                    |this| this.remove_on_stop = !this.remove_on_stop,
                                    cx,
                                ))
                            })
                            .child(self.render_option(
                                "docker-restart-always",
                                "Reiniciar sempre",
                                self.restart_always,
                                |this| this.restart_always = !this.restart_always,
                                cx,
                            )),
                    )
                    .when(!compose, |this| {
                        let copy = self.command_for_clipboard(cx);
                        this.child(
                            v_flex()
                                .w_full()
                                .gap(px(6.))
                                .p(px(12.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(colors.border)
                                .bg(colors.editor_background)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .child(div().flex_1().child(caption("COMANDO EQUIVALENTE")))
                                        .child(
                                            IconButton::new("docker-copy-command", IconName::Copy)
                                                .icon_size(IconSize::XSmall)
                                                .icon_color(Color::Muted)
                                                .tooltip(Tooltip::text("Copiar o comando"))
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                                }),
                                        ),
                                )
                                .child(mono(command, 11.5, Color::Custom(colors.editor_foreground), cx)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .h(px(56.))
                    .px(px(20.))
                    .gap(px(8.))
                    .bg(colors.element_background)
                    .child(div().flex_1().min_w_0().child(match &self.progress {
                        Progress::Idle => mono("esc cancela", 11.5, Color::Placeholder, cx).into_any_element(),
                        Progress::Working(message) => h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::LoadCircle)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted)
                                    .with_rotate_animation(2),
                            )
                            .child(text(message.clone(), 11.5).color(Color::Muted))
                            .into_any_element(),
                        Progress::Failed(message) => text(message.clone(), 11.5)
                            .color(Color::Error)
                            .into_any_element(),
                    }))
                    .child(
                        h_flex()
                            .id("docker-new-cancel")
                            .h(px(28.))
                            .px(px(10.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.border_selected)
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.ghost_element_hover))
                            .child(
                                text("Cancelar", 12.5)
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Custom(colors.editor_foreground)),
                            )
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        h_flex()
                            .id("docker-new-create")
                            .h(px(28.))
                            .px(px(10.))
                            .gap(px(6.))
                            .rounded(px(6.))
                            .bg(colors.border_focused)
                            .cursor_pointer()
                            .hover(|this| this.opacity(0.9))
                            .child(text(primary_label, 12.5).weight(FontWeight::BOLD))
                            .child(
                                KeyBinding::for_action_in(&CreateAction, &focus_handle, cx)
                                    .size(rems_from_px(11_f32))
                                    .color(Color::Custom(colors.text.opacity(0.6))),
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}

impl NewContainerModal {
    /// The command as it would really run, secrets included: it goes to the user's own
    /// clipboard.
    fn command_for_clipboard(&self, cx: &App) -> String {
        let masked = self.command(cx);
        self.env_values(cx)
            .into_iter()
            .filter(|(key, _)| is_secret(key))
            .fold(masked, |command, (key, value)| {
                command.replace(&format!("-e {key}=***"), &format!("-e {key}={value}"))
            })
    }
}

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    // The form reads the local images and networks.
    let store = DockerStore::global(cx);
    store.update(cx, |store, cx| store.refresh_resources(cx));
    let workspace_handle = workspace.weak_handle();
    let project_root = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
    workspace.toggle_modal(window, cx, |window, cx| {
        NewContainerModal::new(workspace_handle, project_root, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> ComposeService {
        ComposeService {
            name: "pg-testes".into(),
            image: "postgres:16-alpine".into(),
            ports: Some((5433, 5432)),
            env: vec![
                ("POSTGRES_DB".into(), "trix_test".into()),
                ("POSTGRES_PASSWORD".into(), "s3cr3t: yes".into()),
            ],
            volume: Some("pgtest:/var/lib/postgresql/data".into()),
            restart_always: false,
        }
    }

    #[test]
    fn appends_the_service_and_its_volume() {
        let existing = "services:\n  api:\n    build: ./api\n\nvolumes:\n  pgdata:\n";
        let updated = add_service(existing, &service()).expect("adds");
        assert_eq!(
            updated,
            "services:\n  api:\n    build: ./api\n  pg-testes:\n    image: postgres:16-alpine\n    ports: [\"5433:5432\"]\n    environment:\n      POSTGRES_DB: trix_test\n      POSTGRES_PASSWORD: \"s3cr3t: yes\"\n    volumes: [pgtest:/var/lib/postgresql/data]\n\nvolumes:\n  pgdata:\n  pgtest:\n"
        );
        assert!(add_service(&updated, &service()).is_err());
    }

    #[test]
    fn creates_the_file_layout_when_empty() {
        let updated = add_service("", &service()).expect("adds");
        assert!(updated.starts_with("services:\n  pg-testes:\n"));
        assert!(updated.ends_with("volumes:\n  pgtest:\n"));
    }
}
