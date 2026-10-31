//! One `DockerStore` per app: the daemon is the machine's, so every window shares its
//! connection, its container list (kept current by `/events`) and the stats it polls while some
//! window shows the dock.

use crate::{
    client::{Container, ContainerState, Docker, DockerError, Image, Network, Stats, Volume},
    discovery::{self, SocketCandidate},
};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, TaskExt as _,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use util::ResultExt as _;
use workspace::AppState;

const PREFERRED_SOCKET_KEY: &str = "docker-preferred-socket";
const STATS_INTERVAL: Duration = Duration::from_secs(2);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
/// Events come in bursts (a compose restart sends dozens); one refresh covers a burst.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub enum Connection {
    Connecting,
    Unavailable {
        socket: PathBuf,
        reason: SharedString,
    },
    Ready {
        docker: Docker,
        runtime: SharedString,
        engine_version: SharedString,
    },
}

/// What a lifecycle action is doing to a container until the daemon answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Remove,
}

impl ContainerAction {
    pub fn in_progress(self) -> &'static str {
        match self {
            Self::Start => "iniciando…",
            Self::Stop => "parando…",
            Self::Restart => "reiniciando…",
            Self::Pause => "pausando…",
            Self::Unpause => "retomando…",
            Self::Remove => "removendo…",
        }
    }

    pub fn done(self) -> &'static str {
        match self {
            Self::Start => "iniciado",
            Self::Stop => "parado",
            Self::Restart => "reiniciado",
            Self::Pause => "pausado",
            Self::Unpause => "retomado",
            Self::Remove => "removido",
        }
    }

    async fn run(self, docker: &Docker, id: &str) -> Result<(), DockerError> {
        match self {
            Self::Start => docker.start(id).await,
            Self::Stop => docker.stop(id).await,
            Self::Restart => docker.restart(id).await,
            Self::Pause => docker.pause(id).await,
            Self::Unpause => docker.unpause(id).await,
            Self::Remove => docker.remove(id).await,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupKind {
    Compose {
        project: String,
        working_dir: Option<PathBuf>,
    },
    Standalone,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub kind: GroupKind,
    pub containers: Vec<Container>,
}

impl Group {
    pub fn key(&self) -> String {
        match &self.kind {
            GroupKind::Compose { project, .. } => format!("compose:{project}"),
            GroupKind::Standalone => "avulsos".into(),
        }
    }

    pub fn title(&self) -> &str {
        match &self.kind {
            GroupKind::Compose { project, .. } => project,
            GroupKind::Standalone => "Avulsos",
        }
    }
}

pub enum StoreEvent {
    ContainersChanged,
}

pub struct DockerStore {
    connection: Connection,
    candidates: Vec<SocketCandidate>,
    preferred_socket: Option<PathBuf>,
    containers: Vec<Container>,
    loaded: bool,
    stats: HashMap<String, Stats>,
    restart_counts: HashMap<String, i64>,
    transitions: HashMap<String, ContainerAction>,
    images: Vec<Image>,
    volumes: Vec<Volume>,
    volume_sizes: HashMap<String, i64>,
    networks: Vec<Network>,
    connect_task: Task<()>,
    events_task: Task<()>,
    stats_task: Task<()>,
    refresh_task: Task<()>,
    resources_task: Task<()>,
}

struct GlobalDockerStore(Entity<DockerStore>);

impl Global for GlobalDockerStore {}

impl EventEmitter<StoreEvent> for DockerStore {}

impl DockerStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalDockerStore>() {
            return store.0.clone();
        }
        let store = cx.new(Self::new);
        cx.set_global(GlobalDockerStore(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDockerStore>()
            .map(|store| store.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let preferred_socket = KeyValueStore::global(cx)
            .read_kvp(PREFERRED_SOCKET_KEY)
            .log_err()
            .flatten()
            .map(PathBuf::from);
        let mut this = Self {
            connection: Connection::Connecting,
            candidates: Vec::new(),
            preferred_socket,
            containers: Vec::new(),
            loaded: false,
            stats: HashMap::default(),
            restart_counts: HashMap::default(),
            transitions: HashMap::default(),
            images: Vec::new(),
            volumes: Vec::new(),
            volume_sizes: HashMap::default(),
            networks: Vec::new(),
            connect_task: Task::ready(()),
            events_task: Task::ready(()),
            stats_task: Task::ready(()),
            refresh_task: Task::ready(()),
            resources_task: Task::ready(()),
        };
        this.connect(cx);
        this
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn docker(&self) -> Option<&Docker> {
        match &self.connection {
            Connection::Ready { docker, .. } => Some(docker),
            _ => None,
        }
    }

    pub fn candidates(&self) -> &[SocketCandidate] {
        &self.candidates
    }

    pub fn containers(&self) -> &[Container] {
        &self.containers
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn container(&self, id: &str) -> Option<&Container> {
        self.containers.iter().find(|container| container.id == id)
    }

    /// By id, id prefix, container name or compose service, as the agent and mentions name
    /// them.
    pub fn find(&self, name: &str) -> Option<&Container> {
        let name = name.trim().trim_start_matches(['/', '@']);
        self.containers
            .iter()
            .find(|container| container.id == name || container.name() == name)
            .or_else(|| {
                self.containers
                    .iter()
                    .find(|container| container.display_name() == name)
            })
            .or_else(|| {
                (name.len() >= 4)
                    .then(|| {
                        self.containers
                            .iter()
                            .find(|container| container.id.starts_with(name))
                    })
                    .flatten()
            })
    }

    pub fn stats(&self, id: &str) -> Option<Stats> {
        self.stats.get(id).copied()
    }

    pub fn restart_count(&self, id: &str) -> Option<i64> {
        self.restart_counts.get(id).copied()
    }

    pub fn transition(&self, id: &str) -> Option<ContainerAction> {
        self.transitions.get(id).copied()
    }

    pub fn images(&self) -> &[Image] {
        &self.images
    }

    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }

    pub fn volume_size(&self, name: &str) -> Option<i64> {
        self.volume_sizes.get(name).copied()
    }

    pub fn networks(&self) -> &[Network] {
        &self.networks
    }

    /// The compose project whose directory is `project_root` first, everything else under
    /// "Avulsos".
    pub fn groups(&self, project_root: Option<&Path>) -> Vec<Group> {
        let mut project_group: Option<Group> = None;
        let mut standalone = Vec::new();
        for container in &self.containers {
            let in_project = project_root.is_some_and(|root| {
                container
                    .compose_working_dir()
                    .is_some_and(|dir| same_dir(dir, root))
            });
            if in_project && let Some(project) = container.compose_project() {
                project_group
                    .get_or_insert_with(|| Group {
                        kind: GroupKind::Compose {
                            project: project.to_string(),
                            working_dir: container.compose_working_dir().map(Path::to_path_buf),
                        },
                        containers: Vec::new(),
                    })
                    .containers
                    .push(container.clone());
            } else {
                standalone.push(container.clone());
            }
        }
        let mut groups: Vec<Group> = project_group.into_iter().collect();
        if !standalone.is_empty() {
            groups.push(Group {
                kind: GroupKind::Standalone,
                containers: standalone,
            });
        }
        for group in &mut groups {
            group.containers.sort_by_key(|container| container.created);
        }
        groups
    }

    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection = Connection::Connecting;
        cx.notify();
        self.connect(cx);
    }

    /// "Usar" in the empty state: tries that socket now and remembers it for next time.
    pub fn use_socket(&mut self, socket: PathBuf, cx: &mut Context<Self>) {
        self.preferred_socket = Some(socket.clone());
        let store = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            store
                .write_kvp(
                    PREFERRED_SOCKET_KEY.to_string(),
                    socket.to_string_lossy().into_owned(),
                )
                .await
        })
        .detach_and_log_err(cx);
        self.reconnect(cx);
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let preferred_socket = self.preferred_socket.clone();
        self.connect_task = cx.spawn(async move |this, cx| {
            let candidates = cx.background_spawn(async { discovery::candidates() }).await;
            let mut order: Vec<PathBuf> = preferred_socket.into_iter().collect();
            order.extend(
                candidates
                    .iter()
                    .filter(|candidate| candidate.exists)
                    .map(|candidate| candidate.path.clone()),
            );
            let mut first_error = None;
            let mut ready = None;
            for socket in order {
                let docker = Docker::new(&socket);
                match docker.ping().await {
                    Ok(()) => {
                        ready = Some(docker);
                        break;
                    }
                    Err(error) => {
                        first_error.get_or_insert((socket, error));
                    }
                }
            }
            let connection = match ready {
                Some(docker) => {
                    let version = docker.version().await.ok();
                    let info = docker.info().await.ok();
                    let runtime = runtime_name(docker.socket(), info.as_ref());
                    Connection::Ready {
                        engine_version: version
                            .map(|version| short_version(&version.version))
                            .unwrap_or_default()
                            .into(),
                        runtime: runtime.into(),
                        docker,
                    }
                }
                None => {
                    let (socket, reason) = match first_error {
                        Some((socket, error)) => (socket, error.to_string()),
                        None => (
                            candidates
                                .iter()
                                .find(|candidate| candidate.path.ends_with("var/run/docker.sock"))
                                .or(candidates.first())
                                .map(|candidate| candidate.path.clone())
                                .unwrap_or_else(|| PathBuf::from("/var/run/docker.sock")),
                            "nenhum socket do Docker encontrado".to_string(),
                        ),
                    };
                    Connection::Unavailable {
                        socket,
                        reason: reason.into(),
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.candidates = candidates;
                let is_ready = matches!(connection, Connection::Ready { .. });
                this.connection = connection;
                cx.notify();
                if is_ready {
                    this.refresh(cx);
                    this.refresh_resources(cx);
                    this.watch_events(cx);
                    this.poll_stats(cx);
                } else {
                    this.containers.clear();
                    this.stats.clear();
                    this.loaded = false;
                    cx.emit(StoreEvent::ContainersChanged);
                    this.retry_later(cx);
                }
            })
            .log_err();
        });
    }

    /// Waits for the daemon to come back after it went away, quietly, while nobody pressed
    /// "Tentar de novo".
    fn retry_later(&mut self, cx: &mut Context<Self>) {
        self.events_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECONNECT_INTERVAL).await;
            this.update(cx, |this, cx| {
                if matches!(this.connection, Connection::Unavailable { .. }) {
                    this.connect(cx);
                }
            })
            .log_err();
        });
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(docker) = self.docker().cloned() else {
            return;
        };
        self.refresh_task = cx.spawn(async move |this, cx| {
            let containers = docker.containers().await;
            let containers = match containers {
                Ok(containers) => containers,
                Err(error) => {
                    log::warn!("Docker: lista de containers falhou: {error}");
                    if matches!(error, DockerError::DaemonUnavailable { .. }) {
                        this.update(cx, |this, cx| this.reconnect(cx)).log_err();
                    }
                    return;
                }
            };
            let mut restart_counts = HashMap::default();
            for container in containers
                .iter()
                .filter(|container| container.state == ContainerState::Restarting)
            {
                if let Ok(details) = docker.inspect(&container.id).await {
                    restart_counts.insert(container.id.clone(), details.restart_count);
                }
            }
            this.update(cx, |this, cx| {
                let ids: HashSet<&str> = containers
                    .iter()
                    .map(|container| container.id.as_str())
                    .collect();
                this.stats.retain(|id, _| ids.contains(id.as_str()));
                this.restart_counts = restart_counts;
                this.containers = containers;
                this.loaded = true;
                cx.emit(StoreEvent::ContainersChanged);
                cx.notify();
            })
            .log_err();
        });
    }

    /// Images, volumes and networks, for the collapsed sections. Volume sizes come after the
    /// rest, since the daemon walks every volume for them.
    pub fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        let Some(docker) = self.docker().cloned() else {
            return;
        };
        self.resources_task = cx.spawn(async move |this, cx| {
            let (images, volumes, networks) =
                futures::join!(docker.images(), docker.volumes(), docker.networks());
            this.update(cx, |this, cx| {
                this.images = images.log_err().unwrap_or_default();
                this.volumes = volumes.log_err().unwrap_or_default();
                this.networks = networks.log_err().unwrap_or_default();
                cx.notify();
            })
            .log_err();
            let sizes = docker.volume_sizes().await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                this.volume_sizes = sizes;
                cx.notify();
            })
            .log_err();
        });
    }

    fn watch_events(&mut self, cx: &mut Context<Self>) {
        let Some(docker) = self.docker().cloned() else {
            return;
        };
        self.events_task = cx.spawn(async move |this, cx| {
            let mut events = match docker.events().await {
                Ok(events) => events,
                Err(error) => {
                    log::warn!("Docker: eventos indisponíveis: {error}");
                    return;
                }
            };
            while let Some(event) = events.recv().await {
                let touches_resources = matches!(event.action.as_str(), "create" | "destroy");
                cx.background_executor().timer(EVENT_DEBOUNCE).await;
                while events.try_recv().is_ok() {}
                let updated = this.update(cx, |this, cx| {
                    this.refresh(cx);
                    if touches_resources {
                        this.refresh_resources(cx);
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
            // The stream only ends when the daemon goes away.
            this.update(cx, |this, cx| this.reconnect(cx)).log_err();
        });
    }

    fn poll_stats(&mut self, cx: &mut Context<Self>) {
        self.stats_task = cx.spawn(async move |this, cx| {
            loop {
                let request = this
                    .update(cx, |this, cx| {
                        if !stats_wanted(cx) {
                            return None;
                        }
                        let docker = this.docker()?.clone();
                        let ids: Vec<String> = this
                            .containers
                            .iter()
                            .filter(|container| container.state == ContainerState::Running)
                            .map(|container| container.id.clone())
                            .collect();
                        Some((docker, ids))
                    })
                    .ok();
                let Some(request) = request else {
                    return;
                };
                if let Some((docker, ids)) = request {
                    let samples = futures::future::join_all(ids.into_iter().map(|id| {
                        let docker = docker.clone();
                        async move {
                            let stats = docker.stats(&id).await;
                            (id, stats)
                        }
                    }))
                    .await;
                    let updated = this.update(cx, |this, cx| {
                        for (id, stats) in samples {
                            if let Ok(stats) = stats {
                                this.stats.insert(id, stats);
                            }
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                cx.background_executor().timer(STATS_INTERVAL).await;
            }
        });
    }

    /// Runs a lifecycle action, showing it on the row until the daemon answers. Errors come back
    /// in pt-BR for the caller's toast.
    pub fn run(
        &mut self,
        container_id: String,
        action: ContainerAction,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(docker) = self.docker().cloned() else {
            return Task::ready(Err(anyhow::anyhow!("O Docker não está conectado.")));
        };
        self.transitions.insert(container_id.clone(), action);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = action.run(&docker, &container_id).await;
            this.update(cx, |this, cx| {
                this.transitions.remove(&container_id);
                this.refresh(cx);
                cx.notify();
            })?;
            result.map_err(|error| anyhow::anyhow!("{error}"))
        })
    }
}

/// Stats cost the daemon a sample per container every interval, so they only run while some
/// window shows the dock.
fn stats_wanted(cx: &App) -> bool {
    AppState::any_workspace(cx, |workspace, cx| crate::panel_is_visible(workspace, cx))
}

fn same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn runtime_name(socket: &Path, info: Option<&crate::client::SystemInfo>) -> String {
    let by_path = discovery::runtime_for(socket, true);
    if by_path != "Docker Desktop" {
        return by_path;
    }
    match info {
        Some(info) if info.operating_system.contains("Docker Desktop") => "Docker Desktop".into(),
        Some(info) if info.operating_system.contains("OrbStack") => "OrbStack".into(),
        Some(_) if cfg!(target_os = "macos") => "Docker Desktop".into(),
        Some(_) => "Docker Engine".into(),
        None => by_path,
    }
}

/// `28.4.0` → `28.4`, as the header shows it.
fn short_version(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    match parts.as_slice() {
        [major, minor, ..] => format!("{major}.{minor}"),
        _ => version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, state: ContainerState, labels: &[(&str, &str)]) -> Container {
        Container {
            id: id.into(),
            names: vec![format!("/{id}")],
            image: "image".into(),
            command: String::new(),
            created: 0,
            state,
            status: String::new(),
            ports: Vec::new(),
            labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[gpui::test]
    fn groups_the_project_compose_first(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|_| DockerStore {
            connection: Connection::Connecting,
            candidates: Vec::new(),
            preferred_socket: None,
            containers: vec![
                container(
                    "api",
                    ContainerState::Running,
                    &[
                        ("com.docker.compose.project", "trix"),
                        ("com.docker.compose.service", "api"),
                        ("com.docker.compose.project.working_dir", "/work/trix"),
                    ],
                ),
                container(
                    "other",
                    ContainerState::Running,
                    &[
                        ("com.docker.compose.project", "other"),
                        ("com.docker.compose.project.working_dir", "/work/other"),
                    ],
                ),
                container("localstack", ContainerState::Exited, &[]),
            ],
            loaded: true,
            stats: HashMap::default(),
            restart_counts: HashMap::default(),
            transitions: HashMap::default(),
            images: Vec::new(),
            volumes: Vec::new(),
            volume_sizes: HashMap::default(),
            networks: Vec::new(),
            connect_task: Task::ready(()),
            events_task: Task::ready(()),
            stats_task: Task::ready(()),
            refresh_task: Task::ready(()),
            resources_task: Task::ready(()),
        });
        store.read_with(cx, |store, _| {
            let groups = store.groups(Some(Path::new("/work/trix")));
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0].title(), "trix");
            assert_eq!(groups[0].containers.len(), 1);
            assert_eq!(groups[1].title(), "Avulsos");
            assert_eq!(groups[1].containers.len(), 2);
            assert_eq!(
                store.find("api").map(|container| container.id.as_str()),
                Some("api")
            );
            assert_eq!(store.groups(None).len(), 1);
        });
    }

    #[test]
    fn shortens_engine_versions() {
        assert_eq!(short_version("28.4.0"), "28.4");
        assert_eq!(short_version("dev"), "dev");
    }
}
