use crate::{
    NewConnection, NewQuery, ToggleFocus,
    client::{ClusterInfo, CurrentUser, DataStream, Elastic, ElasticError, MissingPrivileges},
    connect_view::{self, Prefill},
    connection::{self, AuthKind, Environment, QUERIES_DIR, STREAMS_FILE, SavedConnection, Scope},
    follow_view, query_view,
    results::format_count,
};
use anyhow::Context as _;
use collections::{HashMap, HashSet};
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    Action as _, AnyElement, AsyncApp, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, Pixels, Subscription, Task, WeakEntity, px, uniform_list,
};
use http_client::HttpClient;
use project::Project;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{ButtonLike, Checkbox, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

/// The index patterns whose privileges the dock checks, as the APM and log integrations name
/// their data streams.
const CHECKED_PATTERNS: [&str; 3] = ["logs-*", "traces-*", "metrics-*"];

/// What an open connection knows about the cluster. Views keep an `Arc` of it, so reconnecting
/// the dock doesn't pull it from under a running query.
pub struct Session {
    pub connection: SavedConnection,
    pub elastic: Elastic,
    pub info: ClusterInfo,
    pub kibana_version: Option<String>,
    pub user: Option<CurrentUser>,
    pub missing: MissingPrivileges,
    pub streams: Vec<DataStream>,
    pub sizes: HashMap<String, u64>,
    pub latency: Duration,
}

impl Session {
    /// `Elasticsearch 9.2.4 · via Kibana`.
    pub fn summary(&self) -> String {
        format!(
            "Elasticsearch {} · {}",
            self.info.version.number,
            match self.connection.via {
                connection::Via::Kibana => "via Kibana",
                connection::Via::Direct => "direto",
            }
        )
    }

    pub fn describe(&self) -> String {
        format!(
            "\"{}\" ({}, {}, {})",
            self.connection.name,
            self.connection.environment.label(),
            self.connection.host(),
            self.summary()
        )
    }
}

#[derive(Clone)]
pub struct Stored {
    pub connection: SavedConnection,
    pub scope: Scope,
}

pub enum ActiveState {
    Connecting,
    Connected(Arc<Session>),
    Failed {
        message: SharedString,
        unauthorized: bool,
    },
}

pub struct Active {
    pub connection: SavedConnection,
    pub state: ActiveState,
}

/// What this connection did today, for the card at the bottom of the dock.
#[derive(Default)]
pub struct Usage {
    pub queries: u64,
    pub documents: u64,
    pub following: Duration,
}

/// An app writing to `logs-*`, as its `service.name` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogApp {
    pub name: String,
    pub documents: u64,
    pub errors: Option<u64>,
}

pub enum AppsState {
    Loading,
    Loaded(Vec<LogApp>),
    Unavailable(SharedString),
}

#[derive(Clone)]
enum Row {
    ProjectHeader(usize),
    AppsHeader(usize),
    App(LogApp),
    AppsNote(SharedString),
    Stream {
        name: String,
        starred: bool,
        size: Option<u64>,
        indent: bool,
    },
    ProjectEmpty,
    AllHeader(usize),
    Group {
        key: String,
        label: String,
        count: usize,
        expanded: bool,
    },
    QueriesHeader(usize),
    Query {
        path: PathBuf,
        name: String,
    },
}

/// Panels by project, so a thread (which only knows its project) finds the right connection.
#[derive(Default)]
pub struct PanelRegistry(HashMap<EntityId, WeakEntity<ElasticPanel>>);

impl Global for PanelRegistry {}

pub fn panel_for(project: &Entity<Project>, cx: &App) -> Option<Entity<ElasticPanel>> {
    cx.try_global::<PanelRegistry>()?
        .0
        .get(&project.entity_id())
        .and_then(|panel| panel.upgrade())
}

pub struct ElasticPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    http: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    focus_handle: FocusHandle,
    root: Option<PathBuf>,
    connections: Vec<Stored>,
    project_streams: Vec<String>,
    saved_queries: Vec<PathBuf>,
    loaded: bool,
    load_error: Option<SharedString>,
    active: Option<Active>,
    filter_input: Entity<InputField>,
    expanded: HashSet<String>,
    pub(crate) usage: Usage,
    apps: AppsState,
    load_task: Task<()>,
    connect_task: Task<()>,
    apps_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ElasticPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "Filtrar data streams…")
                .start_icon(IconName::MagnifyingGlass)
        });
        let mut subscriptions = Vec::new();
        {
            let editor = filter_input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |_, cx| cx.notify()).log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        subscriptions.push(cx.subscribe(&project, |this, _, event, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
            ) {
                this.load(cx);
            }
        }));
        let this = cx.weak_entity();
        cx.default_global::<PanelRegistry>()
            .0
            .insert(project.entity_id(), this);
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            fs: workspace.app_state().fs.clone(),
            http: cx.http_client(),
            credentials_provider: zed_credentials_provider::global(cx),
            focus_handle: cx.focus_handle(),
            root: None,
            connections: Vec::new(),
            project_streams: Vec::new(),
            saved_queries: Vec::new(),
            loaded: false,
            load_error: None,
            active: None,
            filter_input,
            expanded: HashSet::default(),
            usage: Usage::default(),
            apps: AppsState::Loading,
            load_task: Task::ready(()),
            connect_task: Task::ready(()),
            apps_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load(cx);
        this
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.project
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

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn workspace(&self) -> WeakEntity<Workspace> {
        self.workspace.clone()
    }

    pub fn active(&self) -> Option<&Active> {
        self.active.as_ref()
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        match &self.active.as_ref()?.state {
            ActiveState::Connected(session) => Some(session.clone()),
            _ => None,
        }
    }

    pub fn project_streams(&self) -> &[String] {
        &self.project_streams
    }

    pub(crate) fn record_query(&mut self, documents: u64, cx: &mut Context<Self>) {
        self.usage.queries += 1;
        self.usage.documents += documents;
        cx.notify();
    }

    pub(crate) fn record_following(&mut self, elapsed: Duration, cx: &mut Context<Self>) {
        self.usage.following += elapsed;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project_root(cx) else {
            self.root = None;
            self.loaded = true;
            cx.notify();
            return;
        };
        self.root = Some(root.clone());
        let profile_connections = connection::load_profile(&KeyValueStore::global(cx), &root);
        let last_used = KeyValueStore::global(cx)
            .read_kvp(&last_used_key(&root))
            .log_err()
            .flatten();
        let fs = self.fs.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            let project_connections = connection::load_project(fs.as_ref(), &root).await;
            let project_streams = connection::load_project_streams(fs.as_ref(), &root).await;
            let saved_queries = list_queries(fs.as_ref(), &root).await;
            this.update(cx, |this, cx| {
                let mut connections = Vec::new();
                let mut errors = Vec::new();
                match project_connections {
                    Ok(found) => connections.extend(found.into_iter().map(|connection| Stored {
                        connection,
                        scope: Scope::Project,
                    })),
                    Err(error) => errors.push(format!("{error:#}")),
                }
                match profile_connections {
                    Ok(found) => connections.extend(found.into_iter().map(|connection| Stored {
                        connection,
                        scope: Scope::Profile,
                    })),
                    Err(error) => errors.push(format!("{error:#}")),
                }
                match project_streams {
                    Ok(streams) => this.project_streams = streams,
                    Err(error) => errors.push(format!("{error:#}")),
                }
                this.load_error = (!errors.is_empty()).then(|| errors.join("\n").into());
                this.connections = connections;
                this.saved_queries = saved_queries;
                this.loaded = true;
                if this.active.is_none() {
                    let preferred = this
                        .connections
                        .iter()
                        .find(|stored| Some(&stored.connection.id) == last_used.as_ref())
                        .or_else(|| this.connections.first())
                        .map(|stored| stored.connection.clone());
                    // Logs are read-only here, so production connects on its own too.
                    if let Some(connection) = preferred {
                        this.connect(connection, cx);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn connect(&mut self, connection: SavedConnection, cx: &mut Context<Self>) {
        self.active = Some(Active {
            connection: connection.clone(),
            state: ActiveState::Connecting,
        });
        cx.notify();
        if let Some(root) = self.root.clone() {
            let store = KeyValueStore::global(cx);
            let id = connection.id.clone();
            cx.background_spawn(async move { store.write_kvp(last_used_key(&root), id).await })
                .detach_and_log_err(cx);
        }
        let credentials_provider = self.credentials_provider.clone();
        let http = self.http.clone();
        self.connect_task = cx.spawn(async move |this, cx| {
            let result = open_session(&connection, credentials_provider, http, cx).await;
            this.update(cx, |this, cx| {
                let still_active = this
                    .active
                    .as_ref()
                    .is_some_and(|active| active.connection.id == connection.id);
                if !still_active {
                    return;
                }
                let state = match result {
                    Ok(session) => {
                        if this.expanded.is_empty() {
                            this.expanded.insert(group_key("logs"));
                        }
                        let session = Arc::new(session);
                        this.load_apps(session.clone(), cx);
                        ActiveState::Connected(session)
                    }
                    Err(error) => {
                        let unauthorized = matches!(
                            error.downcast_ref::<ElasticError>(),
                            Some(ElasticError::Unauthorized { .. })
                        );
                        ActiveState::Failed {
                            message: format!("{error:#}").into(),
                            unauthorized,
                        }
                    }
                };
                if let Some(active) = this.active.as_mut() {
                    active.state = state;
                }
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn apps(&self) -> Option<&[LogApp]> {
        match &self.apps {
            AppsState::Loaded(apps) => Some(apps),
            _ => None,
        }
    }

    /// Groups `logs-*` of the last 24 h by `service.name`, so the dock can list logs by app.
    fn load_apps(&mut self, session: Arc<Session>, cx: &mut Context<Self>) {
        self.apps = AppsState::Loading;
        self.apps_task = cx.spawn(async move |this, cx| {
            let state = load_apps(&session).await;
            this.update(cx, |this, cx| {
                this.apps = state;
                cx.notify();
            })
            .log_err();
        });
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.active = None;
        self.connect_task = Task::ready(());
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            let connection = active.connection.clone();
            self.connect(connection, cx);
        }
        self.load(cx);
    }

    /// Writes the connection (replacing `previous` when editing), stores the secret and
    /// connects to it.
    pub fn save_connection(
        &mut self,
        connection: SavedConnection,
        scope: Scope,
        secret: String,
        previous: Option<(String, Scope)>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(root) = self.root.clone() else {
            return Task::ready(Err(anyhow::anyhow!("abra uma pasta de projeto primeiro")));
        };
        let mut project = self.connections_in(Scope::Project);
        let mut profile = self.connections_in(Scope::Profile);
        if let Some((previous_id, previous_scope)) = &previous {
            let list = match previous_scope {
                Scope::Project => &mut project,
                Scope::Profile => &mut profile,
            };
            list.retain(|existing| existing.id != *previous_id);
        }
        match scope {
            Scope::Project => project.push(connection.clone()),
            Scope::Profile => profile.push(connection.clone()),
        }
        let project_changed =
            scope == Scope::Project || previous.as_ref().is_some_and(|(_, s)| *s == Scope::Project);
        let fs = self.fs.clone();
        let store = KeyValueStore::global(cx);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            if project_changed {
                connection::save_project(fs.as_ref(), &root, project).await?;
            }
            connection::save_profile(store, &root, profile).await?;
            if !secret.is_empty() && connection.auth != AuthKind::None {
                credentials_provider
                    .write_credentials(
                        &connection.keychain_url(),
                        &connection.username,
                        secret.as_bytes(),
                        cx,
                    )
                    .await?;
            }
            this.update(cx, |this, cx| {
                this.connections.retain(|stored| {
                    Some(&stored.connection.id) != previous.as_ref().map(|(id, _)| id)
                        && stored.connection.id != connection.id
                });
                this.connections.push(Stored {
                    connection: connection.clone(),
                    scope,
                });
                this.connect(connection, cx);
            })
        })
    }

    fn connections_in(&self, scope: Scope) -> Vec<SavedConnection> {
        self.connections
            .iter()
            .filter(|stored| stored.scope == scope)
            .map(|stored| stored.connection.clone())
            .collect()
    }

    fn stored(&self, id: &str) -> Option<&Stored> {
        self.connections
            .iter()
            .find(|stored| stored.connection.id == id)
    }

    fn open_connect_view(&mut self, prefill: Prefill, window: &mut Window, cx: &mut Context<Self>) {
        connect_view::open(self.workspace.clone(), cx.entity(), prefill, window, cx);
    }

    fn toggle_project_stream(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if let Some(index) = self
            .project_streams
            .iter()
            .position(|stream| *stream == name)
        {
            self.project_streams.remove(index);
        } else {
            self.project_streams.push(name);
            self.project_streams.sort();
        }
        let streams = self.project_streams.clone();
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            connection::save_project_streams(fs.as_ref(), &root, streams).await
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    /// Opens the `.esql` file against the active connection; without one it opens as text.
    pub(crate) fn open_query(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.session() {
            Some(session) => query_view::open(
                self.workspace.clone(),
                self.project.clone(),
                cx.weak_entity(),
                session,
                path,
                window,
                cx,
            )
            .detach_and_log_err(cx),
            None => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_abs_path(path, OpenOptions::default(), window, cx)
                            .detach_and_log_err(cx);
                    })
                    .log_err();
            }
        }
    }

    /// A new query file, starting from `body` (or a query on the first project stream), open and
    /// optionally run.
    pub(crate) fn new_query(
        &mut self,
        body: Option<String>,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_query(body, run, None, window, cx);
    }

    /// The agent's query in a new tab, read over the same window the agent used.
    pub(crate) fn open_agent_query(
        &mut self,
        query: String,
        window_minutes: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let time_window = query_view::TimeWindow::covering(window_minutes);
        self.create_query(Some(query), true, Some(time_window), window, cx);
    }

    fn create_query(
        &mut self,
        body: Option<String>,
        run: bool,
        time_window: Option<query_view::TimeWindow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let source = self
            .project_streams
            .first()
            .cloned()
            .unwrap_or_else(|| "logs-*".to_string());
        let body =
            body.unwrap_or_else(|| format!("FROM {source}\n| SORT @timestamp DESC\n| LIMIT 200"));
        let header = match &self.active {
            Some(active) => format!(
                "// {} · {}",
                active.connection.name,
                active.connection.environment.label()
            ),
            None => "// nova consulta".to_string(),
        };
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        let session = self.session();
        cx.spawn_in(window, async move |this, cx| {
            let path =
                query_view::create_query_file(fs.as_ref(), &root, &format!("{header}\n{body}\n"))
                    .await?;
            this.update(cx, |this, cx| {
                this.saved_queries.push(path.clone());
                this.saved_queries.sort();
                cx.notify();
            })?;
            let Some(session) = session else {
                return this.update_in(cx, |this, window, cx| this.open_query(path, window, cx));
            };
            let panel = this.clone();
            let view = cx
                .update(|window, cx| {
                    query_view::open(workspace, project, panel, session, path, window, cx)
                })?
                .await?;
            view.update_in(cx, |view, window, cx| {
                if let Some(time_window) = time_window {
                    view.set_time_window(time_window, cx);
                }
                if run {
                    view.run(window, cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn follow(
        &mut self,
        source: String,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        follow_view::open(
            self.workspace.clone(),
            self.project.clone(),
            cx.weak_entity(),
            session,
            source,
            filter,
            window,
            cx,
        );
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(session) = self.session() else {
            return rows;
        };
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let filtering = !filter.is_empty();
        let matches = |name: &str| !filtering || name.to_lowercase().contains(&filter);
        let starred: HashSet<&str> = self.project_streams.iter().map(String::as_str).collect();

        let project: Vec<&String> = self
            .project_streams
            .iter()
            .filter(|name| matches(name))
            .collect();
        rows.push(Row::ProjectHeader(project.len()));
        if project.is_empty() && !filtering {
            rows.push(Row::ProjectEmpty);
        }
        rows.extend(project.into_iter().map(|name| Row::Stream {
            name: name.clone(),
            starred: true,
            size: session.sizes.get(name).copied(),
            indent: false,
        }));

        match &self.apps {
            AppsState::Loading => {
                rows.push(Row::AppsHeader(0));
                rows.push(Row::AppsNote("Agrupando logs-* por service.name…".into()));
            }
            AppsState::Unavailable(reason) => {
                rows.push(Row::AppsHeader(0));
                rows.push(Row::AppsNote(reason.clone()));
            }
            AppsState::Loaded(apps) => {
                let shown: Vec<&LogApp> = apps.iter().filter(|app| matches(&app.name)).collect();
                rows.push(Row::AppsHeader(shown.len()));
                rows.extend(shown.into_iter().cloned().map(Row::App));
            }
        }

        let streams: Vec<&DataStream> = session
            .streams
            .iter()
            .filter(|stream| matches(&stream.name))
            .collect();
        rows.push(Row::AllHeader(session.streams.len()));
        let mut groups: Vec<(String, Vec<&DataStream>)> = Vec::new();
        for stream in streams {
            let kind = stream_kind(&stream.name);
            match groups.iter_mut().find(|(existing, _)| *existing == kind) {
                Some((_, members)) => members.push(stream),
                None => groups.push((kind, vec![stream])),
            }
        }
        groups.sort_by_key(|(kind, _)| group_order(kind));
        for (kind, members) in groups {
            let key = group_key(&kind);
            let expanded = filtering || self.expanded.contains(&key);
            rows.push(Row::Group {
                label: format!("{kind}-*"),
                key,
                count: members.len(),
                expanded,
            });
            if expanded {
                rows.extend(members.into_iter().map(|stream| Row::Stream {
                    name: stream.name.clone(),
                    starred: starred.contains(stream.name.as_str()),
                    size: session.sizes.get(&stream.name).copied(),
                    indent: true,
                }));
            }
        }

        if !self.saved_queries.is_empty() && !filtering {
            rows.push(Row::QueriesHeader(self.saved_queries.len()));
            rows.extend(self.saved_queries.iter().map(|path| {
                Row::Query {
                    name: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path: path.clone(),
                }
            }));
        }
        rows
    }

    fn render_row(&self, index: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let base = h_flex()
            .id(("elastic-row", index))
            .group("elastic-row")
            .w_full()
            .h(px(26.))
            .px_2()
            .gap_1p5()
            .rounded_sm();
        let clickable = |base: gpui::Stateful<gpui::Div>| {
            base.cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
        };
        let section = |title: &'static str, count: usize, right: Option<&'static str>| {
            h_flex()
                .h(px(34.))
                .px_2()
                .pt_2()
                .gap_1p5()
                .child(
                    Label::new(title)
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .when_some(right, |this, right| {
                    this.child(
                        Label::new(right)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Disabled),
                    )
                })
                .into_any_element()
        };
        match row {
            Row::ProjectHeader(count) => section("DO PROJETO", count, Some(".asylum/elastic")),
            Row::ProjectEmpty => div()
                .px_2()
                .pb_1()
                .child(
                    Label::new("Marque os data streams do projeto na lista abaixo.")
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled),
                )
                .into_any_element(),
            Row::AllHeader(count) => section("TODOS", count, None),
            Row::AppsHeader(count) => section("APPS", count, Some("service.name · 24 h")),
            Row::AppsNote(note) => div()
                .px_2()
                .pb_1()
                .child(
                    Label::new(note)
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled),
                )
                .into_any_element(),
            Row::App(app) => {
                let followed = app.name.clone();
                let queried = app.name.clone();
                let errors = app.errors.filter(|errors| *errors > 0);
                clickable(base)
                    .child(Icon::new(IconName::Server).size(IconSize::Small).color(
                        if errors.is_some() {
                            Color::Error
                        } else {
                            Color::Muted
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(app.name).size(LabelSize::Small).truncate()),
                    )
                    .child(
                        div().visible_on_hover("elastic-row").child(
                            IconButton::new(("elastic-query-app", index), IconName::FileCode)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Nova consulta com os logs deste app"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.new_query(Some(app_query(&queried)), true, window, cx)
                                })),
                        ),
                    )
                    .when_some(errors, |this, errors| {
                        this.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(Color::Error.color(cx).opacity(0.12))
                                .child(
                                    Label::new(format!("{} erros", format_count(errors)))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error),
                                ),
                        )
                    })
                    .child(
                        Label::new(format_count(app.documents))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Seguir os logs deste app"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.follow(
                            "logs-*".to_string(),
                            Some(app_filter(&followed)),
                            window,
                            cx,
                        )
                    }))
                    .into_any_element()
            }
            Row::QueriesHeader(count) => section("QUERIES SALVAS", count, Some(QUERIES_DIR)),
            Row::Group {
                key,
                label,
                count,
                expanded,
            } => clickable(base)
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    Icon::new(if expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Label::new(label).size(LabelSize::Small)),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx)))
                .into_any_element(),
            Row::Stream {
                name,
                starred,
                size,
                indent,
            } => {
                let toggled = name.clone();
                let queried = name.clone();
                let followed = name.clone();
                clickable(base)
                    .when(indent, |this| this.pl(px(28.)))
                    .child(
                        Checkbox::new(("elastic-star", index), starred.into())
                            .tooltip(Tooltip::text(if starred {
                                format!("Tirar do projeto ({STREAMS_FILE})")
                            } else {
                                format!("Marcar como do projeto ({STREAMS_FILE})")
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_project_stream(toggled.clone(), cx)
                            })),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(name)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().visible_on_hover("elastic-row").child(
                            IconButton::new(("elastic-query-stream", index), IconName::FileCode)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Nova consulta neste data stream"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.new_query(
                                        Some(format!(
                                            "FROM {queried}\n| SORT @timestamp DESC\n| LIMIT 200"
                                        )),
                                        true,
                                        window,
                                        cx,
                                    )
                                })),
                        ),
                    )
                    .when_some(size, |this, size| {
                        this.child(
                            Label::new(format_bytes(size))
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        )
                    })
                    .tooltip(Tooltip::text("Seguir os logs deste data stream"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.follow(followed.clone(), None, window, cx)
                    }))
                    .into_any_element()
            }
            Row::Query { path, name } => {
                clickable(base)
                    .child(
                        Icon::new(IconName::FileCode)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).truncate()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_query(path.clone(), window, cx)
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_connection_card(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let connection = &active.connection;
        let subtitle = match &active.state {
            ActiveState::Connected(session) => session.summary(),
            _ => connection.host(),
        };
        let this = cx.weak_entity();
        let others: Vec<SavedConnection> = self
            .connections
            .iter()
            .map(|stored| stored.connection.clone())
            .filter(|other| other.id != connection.id)
            .collect();
        let current = connection.clone();
        let scope = self.stored(&connection.id).map(|stored| stored.scope);
        let failed = matches!(active.state, ActiveState::Failed { .. });
        PopoverMenu::new("elastic-connection-menu")
            .full_width(true)
            .trigger(
                ButtonLike::new("elastic-connection-card")
                    .full_width()
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Large)
                    // The two-line content is taller than any `ButtonSize`, whose heights are fixed.
                    .height(rems_from_px(52_f32).into())
                    .child(
                        h_flex()
                            .w_full()
                            .py_1p5()
                            .gap_2p5()
                            .child(
                                div()
                                    .size_8()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .bg(if failed {
                                        Color::Error.color(cx).opacity(0.12)
                                    } else {
                                        Color::Accent.color(cx).opacity(0.12)
                                    })
                                    .child(
                                        Icon::new(IconName::CloudPulse)
                                            .size(IconSize::Small)
                                            .color(if failed {
                                                Color::Error
                                            } else {
                                                Color::Accent
                                            }),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(
                                                Label::new(connection.name.clone())
                                                    .weight(FontWeight::SEMIBOLD)
                                                    .truncate(),
                                            )
                                            .child(via_chip(connection, cx)),
                                    )
                                    .child(
                                        Label::new(subtitle)
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let this = this.clone();
                let others = others.clone();
                let current = current.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if !others.is_empty() {
                        menu = menu.header("Trocar de conexão");
                        for other in others.iter().cloned() {
                            let this = this.clone();
                            let label = format!("{} · {}", other.name, other.environment.label());
                            menu = menu.entry(label, None, move |_, cx| {
                                this.update(cx, |this, cx| this.connect(other.clone(), cx))
                                    .log_err();
                            });
                        }
                        menu = menu.separator();
                    }
                    let edit = current.clone();
                    let reconnect = current.clone();
                    menu.entry("Reconectar", None, {
                        let this = this.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| this.connect(reconnect.clone(), cx))
                                .log_err();
                        }
                    })
                    .when_some(scope, |menu, scope| {
                        let this = this.clone();
                        menu.entry("Editar conexão…", None, move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.open_connect_view(
                                    Prefill::Edit(edit.clone(), scope),
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                        })
                    })
                    .entry("Nova conexão…", None, {
                        let this = this.clone();
                        move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.open_connect_view(Prefill::New, window, cx)
                            })
                            .log_err();
                        }
                    })
                    .separator()
                    .entry("Desconectar", None, move |_, cx| {
                        this.update(cx, |this, cx| this.disconnect(cx)).log_err();
                    })
                }))
            })
            .into_any_element()
    }

    fn render_status_row(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let (dot, status) = match &active.state {
            ActiveState::Connecting => (Color::Muted, "conectando…".to_string()),
            ActiveState::Connected(session) => (
                Color::Success,
                match &session.user {
                    Some(user) => format!("sessão ok · {}", user.username),
                    None => format!("conectado · {} ms", session.latency.as_millis()),
                },
            ),
            ActiveState::Failed { unauthorized, .. } => (
                Color::Error,
                if *unauthorized {
                    "senha recusada".to_string()
                } else {
                    "falhou".to_string()
                },
            ),
        };
        let chip = |content: AnyElement| {
            h_flex()
                .h(px(22.))
                .px_2()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .child(content)
        };
        let missing = match &active.state {
            ActiveState::Connected(session) if !session.missing.is_empty() => {
                Some(describe_missing(&session.missing))
            }
            _ => None,
        };
        h_flex()
            .px_3()
            .pt_2()
            .gap_1p5()
            .flex_wrap()
            .child(chip(
                h_flex()
                    .gap_1p5()
                    .child(Indicator::dot().color(dot))
                    .child(Label::new(status).size(LabelSize::XSmall).color(dot))
                    .into_any_element(),
            ))
            .child(environment_chip(active.connection.environment, cx))
            .child(
                chip(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new("só leitura")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                )
                .id("elastic-read-only")
                .tooltip(Tooltip::text(
                    "O Asylum só lê: ES|QL, campos e data streams. Nada de index, delete ou manage.",
                )),
            )
            .when_some(missing, |this, missing| {
                this.child(
                    chip(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new("privilégios faltando")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .into_any_element(),
                    )
                    .id("elastic-missing-privileges")
                    .tooltip(Tooltip::text(missing)),
                )
            })
            .child(div().flex_1())
            .child(
                IconButton::new("elastic-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Recarregar os data streams"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .into_any_element()
    }

    fn render_failure(
        &self,
        active: &Active,
        message: SharedString,
        unauthorized: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let retry = active.connection.clone();
        let edit = active.connection.clone();
        let scope = self.stored(&edit.id).map(|stored| stored.scope);
        let title = if unauthorized {
            "O Elasticsearch recusou a senha salva"
        } else {
            "Não deu para conectar"
        };
        v_flex()
            .m_3()
            .p_3()
            .gap_2()
            .rounded_md()
            .bg(Color::Error.color(cx).opacity(0.08))
            .border_1()
            .border_color(Color::Error.color(cx).opacity(0.3))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(if unauthorized {
                            IconName::Lock
                        } else {
                            IconName::XCircle
                        })
                        .size(IconSize::Small)
                        .color(Color::Error),
                    )
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .when(unauthorized, |this| {
                this.child(
                    Label::new(format!(
                        "A conexão {} entra {} com {}. A senha mudou ou o usuário foi bloqueado; \
                         nada foi apagado.",
                        active.connection.name,
                        match active.connection.via {
                            connection::Via::Kibana => "pela URL do Kibana",
                            connection::Via::Direct => "direto no Elasticsearch",
                        },
                        match active.connection.auth {
                            AuthKind::Password => "usuário e senha",
                            AuthKind::ApiKey => "API key",
                            AuthKind::None => "acesso aberto",
                        }
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .child(
                Label::new(message)
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when_some(scope, |this, scope| {
                        this.child(
                            Button::new("elastic-edit", "Entrar de novo")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_connect_view(
                                        Prefill::Edit(edit.clone(), scope),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .child(
                        Button::new("elastic-retry", "Tentar de novo")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.connect(retry.clone(), cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let minutes = self.usage.following.as_secs() / 60;
        v_flex()
            .mx_3()
            .mb_2()
            .p_2p5()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().element_background)
            .child(
                Label::new("Hoje nesta conexão")
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!(
                    "{} {} · {} docs lidos · {minutes} min seguindo",
                    self.usage.queries,
                    if self.usage.queries == 1 {
                        "consulta"
                    } else {
                        "consultas"
                    },
                    format_count(self.usage.documents),
                ))
                .size(LabelSize::Small),
            )
            .child(
                Label::new("O Agent lê até 24 h por vez; janelas maiores pedem confirmação.")
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
            .into_any_element()
    }

    fn render_connected(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows(cx);
        let row_count = rows.len();
        let summary = self.session().map(|session| {
            format!(
                "{} data streams · {} no projeto",
                session.streams.len(),
                self.project_streams.len()
            )
        });
        v_flex()
            .size_full()
            .child(
                div()
                    .px_3()
                    .pt_2p5()
                    .child(self.render_connection_card(active, cx)),
            )
            .child(self.render_status_row(active, cx))
            .map(|this| match &active.state {
                ActiveState::Connecting => this.child(
                    div().flex_1().flex().items_center().justify_center().child(
                        Label::new("Conectando…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
                ActiveState::Failed {
                    message,
                    unauthorized,
                } => this
                    .child(self.render_failure(active, message.clone(), *unauthorized, cx))
                    .child(div().flex_1()),
                ActiveState::Connected(_) => this
                    .child(div().px_3().pt_2().child(self.filter_input.clone()))
                    .child(
                        div().flex_1().min_h_0().px_1().pt_1().child(
                            uniform_list(
                                "elastic-tree",
                                row_count,
                                cx.processor(move |this, range: Range<usize>, _window, cx| {
                                    let rows = this.rows(cx);
                                    range
                                        .filter_map(|index| {
                                            rows.get(index)
                                                .cloned()
                                                .map(|row| this.render_row(index, row, cx))
                                        })
                                        .collect()
                                }),
                            )
                            .size_full(),
                        ),
                    )
                    .child(self.render_usage(cx)),
            })
            .child(self.render_footer(summary, cx))
            .into_any_element()
    }

    fn render_footer(&self, summary: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let follow_target = self.project_streams.first().cloned();
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .when_some(summary, |this, summary| {
                this.child(
                    Label::new(summary)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .when(self.session().is_some(), |this| {
                this.child(
                    Button::new("elastic-new-query", "Nova consulta")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .color(Color::Accent)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_query(None, false, window, cx)
                        })),
                )
                .when_some(follow_target, |this, target| {
                    this.child(
                        Button::new("elastic-follow", "Seguir")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Accent)
                            .tooltip(Tooltip::text(format!("Seguir {target}")))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.follow(target.clone(), None, window, cx)
                            })),
                    )
                })
            })
            .into_any_element()
    }

    fn render_idle(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_connections = !self.connections.is_empty();
        v_flex()
            .id("elastic-idle")
            .size_full()
            .overflow_y_scroll()
            .px_3()
            .pt_6()
            .pb_3()
            .gap_1()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .pb_3()
                    .child(
                        div()
                            .size_12()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_lg()
                            .bg(Color::Accent.color(cx).opacity(0.12))
                            .child(
                                Icon::new(IconName::CloudPulse)
                                    .size(IconSize::Medium)
                                    .color(Color::Accent),
                            ),
                    )
                    .child(
                        Label::new(if has_connections {
                            "Nenhuma conexão aberta"
                        } else {
                            "Nenhuma conexão neste projeto"
                        })
                        .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div().max_w(px(300.)).text_center().child(
                            Label::new(
                                "Conecte o Elasticsearch pela URL do Kibana para consultar logs \
                                 com ES|QL, seguir data streams e abrir traces do APM.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                    )
                    .child(
                        Button::new("elastic-new-connection", "Nova conexão")
                            .style(ButtonStyle::Filled)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_connect_view(Prefill::New, window, cx)
                            })),
                    ),
            )
            .when_some(self.load_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .when(has_connections, |this| {
                this.child(
                    h_flex().pt_3().pb_1p5().child(
                        Label::new("CONEXÕES")
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    ),
                )
                .children(self.connections.iter().enumerate().map(|(index, stored)| {
                    let connection = stored.connection.clone();
                    h_flex()
                        .id(("elastic-saved", index))
                        .h(px(30.))
                        .px_2()
                        .gap_1p5()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .child(
                            Icon::new(IconName::CloudPulse)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(connection.name.clone()).size(LabelSize::Small))
                        .child(environment_chip(connection.environment, cx))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(connection.host())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .child(
                            Label::new("Conectar")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.connect(connection.clone(), cx)),
                        )
                }))
            })
            .into_any_element()
    }
}

async fn open_session(
    connection: &SavedConnection,
    credentials_provider: Arc<dyn CredentialsProvider>,
    http: Arc<dyn HttpClient>,
    cx: &AsyncApp,
) -> anyhow::Result<Session> {
    let secret = if connection.auth == AuthKind::None {
        None
    } else {
        match credentials_provider
            .read_credentials(&connection.keychain_url(), cx)
            .await
        {
            Ok(Some((_, bytes))) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Ok(None) => None,
            Err(error) => {
                log::error!("Elastic: falha ao ler o keychain: {error:#}");
                None
            }
        }
    };
    let elastic = Elastic::new(http, connection.endpoint()?, connection.auth_with(secret));
    let started = Instant::now();
    let info = elastic.info_or_fallback().await?;
    let latency = started.elapsed();
    let kibana_version = elastic.kibana_version().await.log_err().flatten();
    let user = elastic.current_user().await.log_err();
    // Only index privileges hide anything from the dock; without `monitor` it just loses the
    // exact cluster version and the data stream sizes.
    let missing = elastic
        .missing_privileges(&[], &CHECKED_PATTERNS, &["read", "view_index_metadata"])
        .await
        .log_err()
        .unwrap_or_default();
    let streams = match elastic.data_streams().await {
        Ok(streams) => streams,
        Err(error) => {
            log::warn!("Elastic: _data_stream falhou ({error}); tentando _resolve/index");
            elastic
                .resolved_data_streams()
                .await
                .context("não deu para listar os data streams")?
        }
    };
    let mut streams: Vec<DataStream> = streams
        .into_iter()
        .filter(|stream| !stream.name.starts_with('.'))
        .collect();
    streams.sort_by(|a, b| a.name.cmp(&b.name));
    let sizes = elastic
        .data_stream_sizes()
        .await
        .log_err()
        .unwrap_or_default()
        .into_iter()
        .collect();
    Ok(Session {
        connection: connection.clone(),
        elastic,
        info,
        kibana_version,
        user,
        missing,
        streams,
        sizes,
        latency,
    })
}

async fn list_queries(fs: &dyn Fs, root: &Path) -> Vec<PathBuf> {
    let Ok(mut entries) = fs.read_dir(&root.join(QUERIES_DIR)).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        if let Ok(path) = entry
            && path
                .extension()
                .is_some_and(|extension| extension == "esql")
        {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

pub(crate) fn app_filter(app: &str) -> String {
    format!("service.name == {}", crate::esql::string_literal(app))
}

fn app_query(app: &str) -> String {
    format!(
        "FROM logs-*\n| WHERE {}\n| SORT @timestamp DESC\n| LIMIT 200",
        app_filter(app)
    )
}

async fn load_apps(session: &Session) -> AppsState {
    // The error count needs log.level; without it the list still comes, with volume only.
    let queries = [
        "FROM logs-* | WHERE @timestamp >= NOW() - 24 hours AND service.name IS NOT NULL \
         | STATS docs = COUNT(*), erros = COUNT(*) WHERE log.level IN (\"error\", \"ERROR\", \"fatal\", \"FATAL\") BY service.name \
         | SORT docs DESC | LIMIT 100",
        "FROM logs-* | WHERE @timestamp >= NOW() - 24 hours AND service.name IS NOT NULL \
         | STATS docs = COUNT(*) BY service.name | SORT docs DESC | LIMIT 100",
    ];
    let mut last_error = None;
    for query in queries {
        match session.elastic.esql(query, None).await {
            Ok(result) => {
                let rows = crate::results::Rows::from(result);
                let number =
                    |row: usize, name: &str| rows.value(row, name).and_then(|value| value.as_u64());
                let apps: Vec<LogApp> = (0..rows.values.len())
                    .filter_map(|row| {
                        Some(LogApp {
                            name: rows.text(row, "service.name")?,
                            documents: number(row, "docs").unwrap_or(0),
                            errors: number(row, "erros"),
                        })
                    })
                    .collect();
                return if apps.is_empty() {
                    AppsState::Unavailable("Nenhum log com service.name nas últimas 24 h.".into())
                } else {
                    AppsState::Loaded(apps)
                };
            }
            Err(error) => last_error = Some(error),
        }
    }
    AppsState::Unavailable(
        match last_error {
            Some(ElasticError::BadRequest { .. }) => {
                "Os logs não têm service.name, então não dá para agrupar por app.".to_string()
            }
            Some(error) => format!("Não deu para agrupar por app: {error}"),
            None => String::new(),
        }
        .into(),
    )
}

fn last_used_key(root: &Path) -> String {
    format!("elastic-last-used-{}", root.display())
}

fn group_key(kind: &str) -> String {
    format!("group:{kind}")
}

/// `logs-trix-api-prod` → `logs`: the data stream naming scheme puts the type first.
pub(crate) fn stream_kind(name: &str) -> String {
    name.split_once('-')
        .map(|(kind, _)| kind.to_string())
        .unwrap_or_else(|| "outros".to_string())
}

fn group_order(kind: &str) -> (usize, String) {
    let rank = match kind {
        "logs" => 0,
        "traces" => 1,
        "metrics" => 2,
        _ => 3,
    };
    (rank, kind.to_string())
}

fn describe_missing(missing: &MissingPrivileges) -> String {
    let mut parts = Vec::new();
    if !missing.cluster.is_empty() {
        parts.push(format!("cluster: {}", missing.cluster.join(", ")));
    }
    for (pattern, privileges) in &missing.index {
        parts.push(format!("{pattern}: {}", privileges.join(", ")));
    }
    format!(
        "Sem {}. O que depende disso fica escondido; peça esses privilégios a quem administra o cluster.",
        parts.join("; ")
    )
}

/// `18234567890` → `18,2 GB`.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
    }
}

pub fn environment_color(environment: Environment) -> Color {
    match environment {
        Environment::Dev => Color::Success,
        Environment::Staging => Color::Warning,
        Environment::Prod => Color::Error,
    }
}

pub fn environment_chip(environment: Environment, cx: &App) -> impl IntoElement {
    let color = environment_color(environment);
    h_flex()
        .flex_none()
        .h(px(18.))
        .px_1p5()
        .gap_1()
        .rounded_full()
        .bg(color.color(cx).opacity(0.12))
        .child(Indicator::dot().color(color))
        .child(
            Label::new(environment.label())
                .size(LabelSize::XSmall)
                .color(color),
        )
}

fn via_chip(connection: &SavedConnection, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_none()
        .h(px(18.))
        .px_1p5()
        .rounded_md()
        .bg(Color::Accent.color(cx).opacity(0.12))
        .child(
            Label::new(connection.via.label())
                .size(LabelSize::XSmall)
                .color(Color::Accent),
        )
}

impl Render for ElasticPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if !self.loaded {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Procurando conexões no projeto…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else if self.root.is_none() {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Abra uma pasta de projeto para conectar ao Elasticsearch.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            match &self.active {
                Some(active) => self.render_connected(active, cx),
                None => self.render_idle(cx),
            }
        };
        v_flex()
            .key_context("ElasticPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.open_connect_view(Prefill::New, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NewQuery, window, cx| {
                    this.new_query(None, false, window, cx)
                }),
            )
            .child(content)
    }
}

impl Focusable for ElasticPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ElasticPanel {}

impl Panel for ElasticPanel {
    fn persistent_name() -> &'static str {
        "ElasticPanel"
    }

    fn panel_key() -> &'static str {
        "ElasticPanel"
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
        Some("Elastic")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        13
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_group_by_type() {
        assert_eq!(stream_kind("logs-trix-api-prod"), "logs");
        assert_eq!(stream_kind("traces-apm-default"), "traces");
        assert_eq!(stream_kind("audit"), "outros");
        assert_eq!(format_bytes(18_234_567_890), "17,0 GB");
        assert_eq!(format_bytes(512), "512 B");
    }
}
