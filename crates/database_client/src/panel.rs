use crate::{
    NewConnection, NewQuery, ToggleFocus,
    catalog::{self, ColumnInfo, ObjectKind, Relation, RelationKind, SchemaObject},
    connect_view::{self, Prefill},
    connection::{self, Environment, SavedConnection, Scope},
    discovery::{self, Source, Suggestion},
    import_view, query_view,
    session::Session,
    table_view,
};
use anyhow::Context as _;
use collections::{HashMap, HashSet};
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    Action as _, AnyElement, AsyncApp, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Pixels, Stateful, Subscription, Task, WeakEntity, px, uniform_list,
};
use i18n::t;
use project::Project;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use ui::{
    ContextMenu, EmptyState, EmptyStateTone, PopoverMenu, PopoverTriggerArea, StatusTone, Tooltip,
    capsule_button, choice_card, choice_card_with_body, navigator_footer, prelude::*,
    primary_button, secondary_button, section_header, segmented_control, select_field, status_pill,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const QUERIES_DIR: &str = ".asylum/db/queries";

#[derive(Clone)]
pub struct Stored {
    pub connection: SavedConnection,
    pub scope: Scope,
}

pub enum ActiveState {
    Connecting,
    Connected {
        session: Arc<Session>,
        relations: Vec<Relation>,
        /// Functions, sequences and enums, listed next to the tables.
        objects: Vec<SchemaObject>,
        /// Column names by `relation_key`, so the filter also finds a table by its columns.
        column_names: HashMap<String, Vec<String>>,
        database_size: Option<String>,
        /// Filled only when `relations` is empty.
        other_databases: Vec<String>,
        latency: Duration,
    },
    Failed(SharedString),
}

pub struct Active {
    pub connection: SavedConnection,
    /// The dock's Leitura/Escrita switch, for the tabs opened from now on.
    pub read_only: bool,
    pub state: ActiveState,
}

enum Columns {
    Loading,
    Loaded(Vec<ColumnInfo>),
    Failed(SharedString),
}

#[derive(Clone)]
enum TreeRow {
    Schema {
        name: String,
        count: usize,
        expanded: bool,
    },
    Group {
        key: String,
        label: SharedString,
        icon: IconName,
        count: usize,
        expanded: bool,
    },
    Relation {
        schema: String,
        name: String,
        kind: RelationKind,
        estimated_rows: Option<i64>,
        expanded: bool,
        indent: Pixels,
    },
    Column {
        column: ColumnInfo,
        indent: Pixels,
    },
    Object {
        kind: ObjectKind,
        name: String,
    },
    ColumnsLoading,
    ColumnsFailed(SharedString),
    QueriesHeader,
    Query {
        path: PathBuf,
        name: String,
    },
}

#[derive(Clone, PartialEq, Eq)]
enum ActiveItem {
    Table {
        connection_id: String,
        schema: String,
        name: String,
    },
    Query(PathBuf),
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    focus_handle: FocusHandle,
    root: Option<PathBuf>,
    connections: Vec<Stored>,
    suggestions: Vec<Suggestion>,
    saved_queries: Vec<PathBuf>,
    query_modified: HashMap<PathBuf, SystemTime>,
    /// What the active tab of the workspace shows, to mark it in the tree.
    active_item: Option<ActiveItem>,
    loaded: bool,
    load_error: Option<SharedString>,
    active: Option<Active>,
    filter_input: Entity<InputField>,
    expanded: HashSet<String>,
    columns: HashMap<String, Columns>,
    load_task: Task<()>,
    connect_task: Task<()>,
    column_tasks: HashMap<String, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("database_client.panel.filter_placeholder"))
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
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(
                cx.subscribe(&workspace_entity, |this, workspace, event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        let active_item = active_item_of(workspace.read(cx), cx);
                        if active_item != this.active_item {
                            this.active_item = active_item;
                            cx.notify();
                        }
                    }
                }),
            );
        }
        crate::agent_schema::register_panel(&project, cx.weak_entity(), cx);
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            fs: workspace.app_state().fs.clone(),
            credentials_provider: zed_credentials_provider::global(cx),
            focus_handle: cx.focus_handle(),
            root: None,
            connections: Vec::new(),
            suggestions: Vec::new(),
            saved_queries: Vec::new(),
            query_modified: HashMap::default(),
            active_item: None,
            loaded: false,
            load_error: None,
            active: None,
            filter_input,
            expanded: HashSet::default(),
            columns: HashMap::default(),
            load_task: Task::ready(()),
            connect_task: Task::ready(()),
            column_tasks: HashMap::default(),
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

    pub fn active(&self) -> Option<&Active> {
        self.active.as_ref()
    }

    /// The session of the active connection, for the views that browse it. The connection
    /// carries the dock's read mode, so the tabs opened from it follow the switch.
    pub fn session(&self) -> Option<(SavedConnection, Arc<Session>)> {
        let active = self.active.as_ref()?;
        match &active.state {
            ActiveState::Connected { session, .. } => {
                let mut connection = active.connection.clone();
                connection.read_only = active.read_only || active.connection.read_only;
                Some((connection, session.clone()))
            }
            _ => None,
        }
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
            let suggestions = discovery::discover(fs.as_ref(), &root).await;
            let saved_queries = list_queries(fs.as_ref(), &root).await;
            let mut query_modified = HashMap::default();
            for path in &saved_queries {
                if let Ok(Some(metadata)) = fs.metadata(path).await {
                    query_modified.insert(path.clone(), metadata.mtime.timestamp_for_user());
                }
            }
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
                this.load_error = (!errors.is_empty()).then(|| errors.join("\n").into());
                this.connections = connections;
                this.suggestions = suggestions;
                this.saved_queries = saved_queries;
                this.query_modified = query_modified;
                this.loaded = true;
                if this.active.is_none() {
                    let preferred = this
                        .connections
                        .iter()
                        .find(|stored| Some(&stored.connection.id) == last_used.as_ref())
                        .or_else(|| this.connections.first())
                        .map(|stored| stored.connection.clone());
                    // Production waits for an explicit click.
                    if let Some(connection) =
                        preferred.filter(|connection| connection.environment != Environment::Prod)
                    {
                        this.connect(connection, cx);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    pub(crate) fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// For the debug hook: connects to the saved connection with this id, if there is one.
    pub(crate) fn connect_saved(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(stored) = self.stored(id) {
            let connection = stored.connection.clone();
            self.connect(connection, cx);
        }
    }

    pub fn connect(&mut self, connection: SavedConnection, cx: &mut Context<Self>) {
        self.columns.clear();
        self.column_tasks.clear();
        let read_only = match &self.active {
            Some(active)
                if active.connection.id == connection.id
                    && active.connection.environment == connection.environment =>
            {
                active.read_only
            }
            _ => connection.read_only || connection.environment == Environment::Prod,
        };
        self.active = Some(Active {
            connection: connection.clone(),
            read_only,
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
        let fs = self.fs.clone();
        self.connect_task = cx.spawn(async move |this, cx| {
            let result = open_session(&connection, credentials_provider, fs, cx).await;
            this.update(cx, |this, cx| {
                let Some(active) = this.active.as_mut() else {
                    return;
                };
                if active.connection.id != connection.id {
                    return;
                }
                active.state = match result {
                    Ok(opened) => {
                        if this.expanded.is_empty() {
                            this.expanded.insert(schema_key("public"));
                        }
                        let mut column_names: HashMap<String, Vec<String>> = HashMap::default();
                        for (schema, relation, column, _) in opened.columns {
                            column_names
                                .entry(relation_key(&schema, &relation))
                                .or_default()
                                .push(column.to_lowercase());
                        }
                        ActiveState::Connected {
                            session: Arc::new(opened.session),
                            relations: opened.relations,
                            objects: opened.objects,
                            column_names,
                            database_size: opened.database_size,
                            other_databases: opened.other_databases,
                            latency: opened.latency,
                        }
                    }
                    Err(error) => ActiveState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.active = None;
        self.connect_task = Task::ready(());
        self.columns.clear();
        self.column_tasks.clear();
        cx.notify();
    }

    fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.read_only = read_only || active.connection.read_only;
            cx.notify();
        }
    }

    fn set_environment(&mut self, environment: Environment, cx: &mut Context<Self>) {
        let Some(active) = &self.active else {
            return;
        };
        if active.connection.environment == environment {
            return;
        }
        let Some(scope) = self
            .stored(&active.connection.id)
            .map(|stored| stored.scope)
        else {
            return;
        };
        let mut connection = active.connection.clone();
        connection.environment = environment;
        let previous = Some((connection.id.clone(), scope));
        self.save_connection(connection, scope, String::new(), previous, cx)
            .detach_and_log_err(cx);
    }

    fn tables_with_pending_edits(&self, cx: &App) -> HashSet<String> {
        let Some(workspace) = self.workspace.upgrade() else {
            return HashSet::default();
        };
        let Some(connection_id) = self.active.as_ref().map(|active| &active.connection.id) else {
            return HashSet::default();
        };
        workspace
            .read(cx)
            .items_of_type::<table_view::TableView>(cx)
            .filter_map(|view| {
                let view = view.read(cx);
                let (view_connection, schema, name) = view.relation();
                (view_connection == connection_id && view.has_pending_edits())
                    .then(|| relation_key(schema, name))
            })
            .collect()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            let connection = active.connection.clone();
            self.connect(connection, cx);
        }
        self.load(cx);
    }

    /// Writes the connection (replacing `previous` when editing), stores the password and
    /// connects to it.
    pub fn save_connection(
        &mut self,
        connection: SavedConnection,
        scope: Scope,
        password: String,
        previous: Option<(String, Scope)>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let password = (!password.is_empty()).then_some(password);
        let persist = self.persist(vec![(connection.clone(), password)], scope, previous, cx);
        cx.spawn(async move |this, cx| {
            persist.await?;
            this.update(cx, |this, cx| this.connect(connection, cx))
        })
    }

    pub(crate) fn import_connections(
        &mut self,
        connections: Vec<(SavedConnection, Option<String>)>,
        scope: Scope,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.persist(connections, scope, None, cx)
    }

    pub(crate) fn has_connection_like(
        &self,
        host: &str,
        port: u16,
        database: &str,
        user: &str,
    ) -> bool {
        self.connections
            .iter()
            .any(|stored| stored.connection.same_target(host, port, database, user))
    }

    /// Writes `additions` to `scope` (replacing `previous` when editing) in one save per list and
    /// stores the passwords that came with them.
    fn persist(
        &mut self,
        additions: Vec<(SavedConnection, Option<String>)>,
        scope: Scope,
        previous: Option<(String, Scope)>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(root) = self.root.clone() else {
            return Task::ready(Err(anyhow::anyhow!(
                "{}",
                t!("database_client.panel.error_no_project_folder")
            )));
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
        let added = additions
            .iter()
            .map(|(connection, _)| connection.clone())
            .collect::<Vec<_>>();
        match scope {
            Scope::Project => project.extend(added.iter().cloned()),
            Scope::Profile => profile.extend(added.iter().cloned()),
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
            for (connection, password) in &additions {
                let Some(password) = password.as_ref().filter(|password| !password.is_empty())
                else {
                    continue;
                };
                credentials_provider
                    .write_credentials(
                        &connection.keychain_url(),
                        &connection.user,
                        password.as_bytes(),
                        cx,
                    )
                    .await?;
            }
            this.update(cx, |this, cx| {
                this.connections.retain(|stored| {
                    Some(&stored.connection.id) != previous.as_ref().map(|(id, _)| id)
                        && !added
                            .iter()
                            .any(|connection| connection.id == stored.connection.id)
                });
                this.connections.extend(
                    added
                        .into_iter()
                        .map(|connection| Stored { connection, scope }),
                );
                cx.notify();
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

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.weak_entity();
        let fs = self.fs.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    import_view::ImportConnections::new(panel, fs, window, cx)
                })
            })
            .log_err();
    }

    /// Opens the file against the active connection; without one it opens as plain text.
    pub(crate) fn open_query(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.session() {
            Some((connection, session)) => query_view::open(
                self.workspace.clone(),
                self.project.clone(),
                connection,
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

    /// For the agent's "Abrir no Banco": a new query file with the agent's SQL, run right away,
    /// so the user sees every row in the grid.
    pub(crate) fn open_agent_query(
        &mut self,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let (connection, _) = self
            .session()
            .with_context(|| t!("database_client.panel.error_not_connected").to_string())?;
        let header = format!(
            "-- {} · {} · {}",
            connection.name,
            connection.environment.label(),
            t!("database_client.panel.agent_query_header")
        );
        self.open_query_with(header, sql, true, window, cx)
    }

    /// A new query file with `sql` under `header`, open against the active connection and run
    /// when `run` is set. Writes from the table tab (ALTER TABLE, the pending UPDATEs) come here
    /// so they go through the editor and its write confirmation.
    pub(crate) fn open_query_with(
        &mut self,
        header: String,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let root = self
            .root
            .clone()
            .with_context(|| t!("database_client.panel.error_no_query_folder").to_string())?;
        let (connection, session) = self
            .session()
            .with_context(|| t!("database_client.panel.error_not_connected").to_string())?;
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let path = query_view::create_query_file(fs.as_ref(), &root, &header).await?;
            this.update(cx, |this, cx| {
                this.saved_queries.push(path.clone());
                this.saved_queries.sort();
                this.query_modified.insert(path.clone(), SystemTime::now());
                cx.notify();
            })?;
            let view = cx
                .update(|window, cx| {
                    query_view::open(workspace, project, connection, session, path, window, cx)
                })?
                .await?;
            view.update_in(cx, |view, window, cx| {
                view.write_from_agent(&sql, query_view::AgentWriteMode::Append, window, cx);
                if run {
                    view.run_statement(window, cx);
                }
            })
        })
        .detach_and_log_err(cx);
        Ok(())
    }

    pub(crate) fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let header = match &self.active {
            Some(active) => format!(
                "-- {} · {}",
                active.connection.name,
                active.connection.environment.label()
            ),
            None => format!("-- {}", t!("database_client.panel.new_query_header")),
        };
        let fs = self.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let path = query_view::create_query_file(fs.as_ref(), &root, &header).await?;
            this.update_in(cx, |this, window, cx| {
                this.saved_queries.push(path.clone());
                this.saved_queries.sort();
                this.query_modified.insert(path.clone(), SystemTime::now());
                this.open_query(path, window, cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn open_table(
        &mut self,
        schema: String,
        name: String,
        kind: RelationKind,
        estimated_rows: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((connection, session)) = self.session() else {
            return;
        };
        table_view::open(
            self.workspace.clone(),
            connection,
            session,
            schema,
            name,
            kind,
            estimated_rows,
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

    fn toggle_relation(&mut self, schema: String, name: String, cx: &mut Context<Self>) {
        let key = relation_key(&schema, &name);
        let expanding = !self.expanded.contains(&key);
        self.toggle(key.clone(), cx);
        if !expanding || matches!(self.columns.get(&key), Some(Columns::Loaded(_))) {
            return;
        }
        let Some((_, session)) = self.session() else {
            return;
        };
        self.columns.insert(key.clone(), Columns::Loading);
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = catalog::list_columns(&session, &schema, &name).await;
                this.update(cx, |this, cx| {
                    this.columns.insert(
                        key.clone(),
                        match result {
                            Ok(columns) => Columns::Loaded(columns),
                            Err(error) => Columns::Failed(format!("{error:#}").into()),
                        },
                    );
                    this.column_tasks.remove(&key);
                    cx.notify();
                })
                .log_err();
            }
        });
        self.column_tasks.insert(key, task);
    }

    fn push_relations<'a>(
        &self,
        schema: &str,
        relations: impl Iterator<Item = &'a &'a Relation>,
        indent: Pixels,
        rows: &mut Vec<TreeRow>,
    ) {
        for relation in relations {
            let key = relation_key(schema, &relation.name);
            let expanded = self.expanded.contains(&key);
            rows.push(TreeRow::Relation {
                schema: schema.to_owned(),
                name: relation.name.clone(),
                kind: relation.kind,
                estimated_rows: relation.estimated_rows,
                expanded,
                indent,
            });
            if !expanded {
                continue;
            }
            match self.columns.get(&key) {
                Some(Columns::Loaded(columns)) => {
                    rows.extend(columns.iter().cloned().map(|column| TreeRow::Column {
                        column,
                        indent: indent + px(36.),
                    }));
                }
                Some(Columns::Failed(error)) => rows.push(TreeRow::ColumnsFailed(error.clone())),
                Some(Columns::Loading) | None => rows.push(TreeRow::ColumnsLoading),
            }
        }
    }

    fn tree_rows(&self, cx: &App) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        let Some(Active {
            state:
                ActiveState::Connected {
                    relations,
                    objects,
                    column_names,
                    ..
                },
            ..
        }) = &self.active
        else {
            return rows;
        };
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let filtering = !filter.is_empty();
        let mut schemas: Vec<(&str, Vec<&Relation>, Vec<&SchemaObject>)> = Vec::new();
        for relation in relations {
            if filtering
                && !relation.name.to_lowercase().contains(&filter)
                && !column_names
                    .get(&relation_key(&relation.schema, &relation.name))
                    .is_some_and(|columns| columns.iter().any(|column| column.contains(&filter)))
            {
                continue;
            }
            match schemas
                .iter_mut()
                .find(|(schema, _, _)| *schema == relation.schema)
            {
                Some((_, list, _)) => list.push(relation),
                None => schemas.push((&relation.schema, vec![relation], Vec::new())),
            }
        }
        for object in objects {
            if filtering && !object.name.to_lowercase().contains(&filter) {
                continue;
            }
            match schemas
                .iter_mut()
                .find(|(schema, _, _)| *schema == object.schema)
            {
                Some((_, _, list)) => list.push(object),
                None => schemas.push((&object.schema, Vec::new(), vec![object])),
            }
        }
        schemas.sort_by_key(|(schema, _, _)| (*schema != "public", *schema));
        for (schema, list, schema_objects) in schemas {
            let expanded = filtering || self.expanded.contains(&schema_key(schema));
            rows.push(TreeRow::Schema {
                name: schema.to_owned(),
                count: list.len() + schema_objects.len(),
                expanded,
            });
            if !expanded {
                continue;
            }
            let is_view = |kind: RelationKind| {
                matches!(kind, RelationKind::View | RelationKind::MaterializedView)
            };
            let tables = list.iter().filter(|relation| !is_view(relation.kind));
            self.push_relations(schema, tables, px(20.), &mut rows);
            let views: Vec<&&Relation> = list
                .iter()
                .filter(|relation| is_view(relation.kind))
                .collect();
            if !views.is_empty() {
                let key = group_key(schema, "views");
                let expanded = filtering || self.expanded.contains(&key);
                rows.push(TreeRow::Group {
                    key,
                    label: t!("database_client.panel.group_views"),
                    icon: IconName::Eye,
                    count: views.len(),
                    expanded,
                });
                if expanded {
                    self.push_relations(schema, views.into_iter(), px(36.), &mut rows);
                }
            }
            let object_groups = [
                (
                    ObjectKind::Function,
                    "functions",
                    t!("database_client.panel.group_functions"),
                    IconName::Code,
                ),
                (
                    ObjectKind::Sequence,
                    "sequences",
                    t!("database_client.panel.group_sequences"),
                    IconName::Hash,
                ),
                (
                    ObjectKind::Enum,
                    "enums",
                    t!("database_client.panel.group_enums"),
                    IconName::List,
                ),
            ];
            for (kind, group, label, icon) in object_groups {
                let members: Vec<&&SchemaObject> = schema_objects
                    .iter()
                    .filter(|object| object.kind == kind)
                    .collect();
                if members.is_empty() {
                    continue;
                }
                let key = group_key(schema, group);
                let expanded = filtering || self.expanded.contains(&key);
                rows.push(TreeRow::Group {
                    key,
                    label,
                    icon,
                    count: members.len(),
                    expanded,
                });
                if expanded {
                    rows.extend(members.into_iter().map(|object| TreeRow::Object {
                        kind,
                        name: object.name.clone(),
                    }));
                }
            }
        }
        if !self.saved_queries.is_empty() && !filtering {
            rows.push(TreeRow::QueriesHeader);
            rows.extend(self.saved_queries.iter().map(|path| {
                TreeRow::Query {
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

    fn render_row(
        &self,
        index: usize,
        row: TreeRow,
        pending_tables: &HashSet<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover = cx.theme().colors().element_hover;
        let accent = cx.theme().colors().border_focused;
        let base = h_flex()
            .id(("db-row", index))
            .w_full()
            .h(px(26.))
            .pr_2()
            .gap_1p5()
            .rounded(Radius::Sm.rems());
        let clickable = |base: Stateful<Div>| base.cursor_pointer().hover(|style| style.bg(hover));
        let selected = |base: Stateful<Div>| base.bg(accent.opacity(0.16));
        let chevron = |expanded: bool| {
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(IconSize::XSmall)
            .color(Color::Muted)
        };
        let count = |count: String| {
            Label::new(count)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Muted)
        };
        let active_connection = self
            .active
            .as_ref()
            .map(|active| active.connection.id.clone());
        match row {
            TreeRow::Schema {
                name,
                count: total,
                expanded,
            } => clickable(base)
                .pl_1()
                .child(chevron(expanded))
                .child(
                    Icon::new(IconName::Blocks)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    ),
                )
                .child(count(total.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(schema_key(&name), cx)))
                .into_any_element(),
            TreeRow::Group {
                key,
                label,
                icon,
                count: total,
                expanded,
            } => clickable(base)
                .pl(px(20.))
                .child(chevron(expanded))
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    div()
                        .flex_1()
                        .child(Label::new(label).size(LabelSize::Small)),
                )
                .child(count(total.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx)))
                .into_any_element(),
            TreeRow::Relation {
                schema,
                name,
                kind,
                estimated_rows,
                expanded,
                indent,
            } => {
                let is_selected = matches!(
                    &self.active_item,
                    Some(ActiveItem::Table { connection_id, schema: s, name: n })
                        if Some(connection_id) == active_connection.as_ref()
                            && *s == schema
                            && *n == name
                );
                let has_pending_edits = pending_tables.contains(&relation_key(&schema, &name));
                clickable(base)
                    .pl(indent)
                    .when(is_selected, selected)
                    .child(
                        div()
                            .id(("db-row-chevron", index))
                            .child(chevron(expanded))
                            .on_click({
                                let schema = schema.clone();
                                let name = name.clone();
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_relation(schema.clone(), name.clone(), cx)
                                })
                            }),
                    )
                    .child(
                        Icon::new(match kind {
                            RelationKind::View | RelationKind::MaterializedView => IconName::Eye,
                            _ => IconName::Table,
                        })
                        .size(IconSize::Small)
                        .color(if is_selected {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(name.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .when(is_selected, |label| label.weight(FontWeight::SEMIBOLD))
                                .truncate(),
                        ),
                    )
                    .when(has_pending_edits, |this| {
                        this.child(StatusTone::Warning.dot(cx))
                    })
                    .when_some(estimated_rows, |this, rows| {
                        this.child(count(format_count_compact(rows)))
                    })
                    .tooltip(Tooltip::text(t!("database_client.panel.open_table")))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_table(
                            schema.clone(),
                            name.clone(),
                            kind,
                            estimated_rows,
                            window,
                            cx,
                        )
                    }))
                    .into_any_element()
            }
            TreeRow::Column { column, indent } => base
                .pl(indent)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Label::new(column.name)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .when(column.primary_key, |label| label.color(Color::Warning)),
                        )
                        .child(
                            Label::new(column.type_name)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(column.primary_key, |this| {
                    this.child(
                        Label::new("PK")
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Warning),
                    )
                })
                .when(column.unique && !column.primary_key, |this| {
                    this.child(
                        Label::new(t!("database_client.panel.unique"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
            TreeRow::Object { kind, name } => base
                .pl(px(54.))
                .child(
                    Icon::new(match kind {
                        ObjectKind::Function => IconName::Code,
                        ObjectKind::Sequence => IconName::Hash,
                        ObjectKind::Enum => IconName::List,
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .into_any_element(),
            TreeRow::ColumnsLoading => base
                .pl(px(72.))
                .child(
                    Label::new(t!("database_client.panel.reading_columns"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            TreeRow::ColumnsFailed(error) => base
                .pl(px(72.))
                .child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error)
                        .truncate(),
                )
                .into_any_element(),
            TreeRow::QueriesHeader => div()
                .pt_3()
                .px_1()
                .child(section_header(
                    t!("database_client.panel.saved_queries"),
                    None,
                ))
                .into_any_element(),
            TreeRow::Query { path, name } => {
                let is_selected =
                    matches!(&self.active_item, Some(ActiveItem::Query(open)) if *open == path);
                let age = self
                    .query_modified
                    .get(&path)
                    .map(|modified| relative_age(*modified));
                clickable(base)
                    .pl_1()
                    .when(is_selected, selected)
                    .child(Icon::new(IconName::FileCode).size(IconSize::Small).color(
                        if is_selected {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    ))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(name)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .when(is_selected, |label| label.weight(FontWeight::SEMIBOLD))
                                .truncate(),
                        ),
                    )
                    .when_some(age, |this, age| this.child(count(age.to_string())))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_query(path.clone(), window, cx)
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_connection_selector(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let connection = &active.connection;
        let subtitle = match &active.state {
            ActiveState::Connected { session, .. } => {
                match short_version(&session.server_version) {
                    Some(version) => t!(
                        "database_client.panel.connection_subtitle",
                        version = version,
                        address = connection.address_with_tunnel()
                    )
                    .to_string(),
                    None => connection.address_with_tunnel(),
                }
            }
            _ => connection.address_with_tunnel(),
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
        let hover = cx.theme().colors().element_hover;
        let selector = h_flex()
            .id("db-connection-selector")
            .w_full()
            .p_1p5()
            .gap_2p5()
            .rounded(Radius::Md.rems())
            .hover(move |style| style.bg(hover))
            .child(
                div()
                    .size(px(32.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(Radius::Md.rems())
                    .bg(Color::Info.color(cx).opacity(0.16))
                    .child(
                        Icon::new(IconName::Database)
                            .size(IconSize::Small)
                            .color(Color::Info),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(connection.name.clone())
                            .weight(FontWeight::BOLD)
                            .truncate(),
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
                    .size(IconSize::Small)
                    .color(Color::Muted),
            );
        PopoverMenu::new("db-connection-menu")
            .full_width(true)
            .trigger(PopoverTriggerArea::new(selector))
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let this = this.clone();
                let others = others.clone();
                let current = current.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if !others.is_empty() {
                        menu = menu.header(t!("database_client.panel.switch_connection"));
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
                    let reconnect = current.clone();
                    let edit = current.clone();
                    menu.entry(t!("database_client.panel.reconnect"), None, {
                        let this = this.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| this.connect(reconnect.clone(), cx))
                                .log_err();
                        }
                    })
                    .when_some(scope, |menu, scope| {
                        let this = this.clone();
                        menu.entry(
                            t!("database_client.panel.edit_connection_ellipsis"),
                            None,
                            move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.open_connect_view(
                                        Prefill::Edit(edit.clone(), scope),
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                            },
                        )
                    })
                    .entry(t!("database_client.panel.new_connection_ellipsis"), None, {
                        let this = this.clone();
                        move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.open_connect_view(Prefill::New, window, cx)
                            })
                            .log_err();
                        }
                    })
                    .separator()
                    .entry(
                        t!("database_client.panel.disconnect"),
                        None,
                        move |_, cx| {
                            this.update(cx, |this, cx| this.disconnect(cx)).log_err();
                        },
                    )
                }))
            })
            .into_any_element()
    }

    fn render_mode_row(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let current = active.connection.environment;
        let can_change_environment = self.stored(&active.connection.id).is_some();
        let this = cx.weak_entity();
        let environment_select = environment_select(
            "db-environment",
            current,
            can_change_environment,
            move |environment, cx| {
                this.update(cx, |this, cx| this.set_environment(environment, cx))
                    .log_err();
            },
            cx,
        );
        let read_only = active.read_only;
        let locked_read_only = active.connection.read_only;
        h_flex()
            .w_full()
            .gap_2()
            .child(div().flex_1().min_w_0().child(environment_select))
            .child(
                segmented_control(cx)
                    .flex_none()
                    .child(
                        segment(
                            "db-mode-read",
                            t!("database_client.panel.mode_read"),
                            read_only,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.set_read_only(true, cx))),
                    )
                    .child(
                        ui::segment_option(
                            "db-mode-write",
                            t!("database_client.panel.mode_write"),
                            !read_only,
                            !locked_read_only,
                            cx,
                        )
                        .flex_initial()
                        .px_2p5()
                        .h(px(26.))
                        .when(locked_read_only, |this| {
                            this.tooltip(Tooltip::text(t!("database_client.panel.read_only")))
                        })
                        .when(!locked_read_only, |this| {
                            this.on_click(
                                cx.listener(|this, _, _, cx| this.set_read_only(false, cx)),
                            )
                        }),
                    ),
            )
            .into_any_element()
    }

    fn render_connected(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree_rows(cx);
        let row_count = rows.len();
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .px_3()
                    .pt_3()
                    .gap_2()
                    .child(self.render_connection_selector(active, cx))
                    .child(self.render_mode_row(active, cx)),
            )
            .map(|this| match &active.state {
                ActiveState::Connecting => this.child(
                    EmptyState::new(IconName::LoadCircle, t!("database_client.panel.connecting"))
                        .spinning(true),
                ),
                ActiveState::Failed(error) => {
                    let retry = active.connection.clone();
                    let edit = active.connection.clone();
                    let scope = self.stored(&edit.id).map(|stored| stored.scope);
                    this.child(
                        EmptyState::new(
                            IconName::XCircle,
                            t!("database_client.panel.connect_failed"),
                        )
                        .tone(EmptyStateTone::Error)
                        .detail(error.clone())
                        .action(
                            primary_button(
                                "db-retry",
                                Some(IconName::RotateCw),
                                t!("database_client.panel.retry"),
                                None,
                                cx,
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.connect(retry.clone(), cx)),
                            ),
                        )
                        .when_some(scope, |this, scope| {
                            this.action(
                                secondary_button(
                                    "db-edit",
                                    None,
                                    t!("database_client.connect.edit_title"),
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_connect_view(
                                            Prefill::Edit(edit.clone(), scope),
                                            window,
                                            cx,
                                        )
                                    },
                                )),
                            )
                        }),
                    )
                }
                ActiveState::Connected {
                    relations,
                    other_databases,
                    ..
                } if relations.is_empty() => this
                    .child(
                        EmptyState::new(
                            IconName::Database,
                            t!(
                                "database_client.panel.no_tables",
                                database = active.connection.database
                            ),
                        )
                        .description(if other_databases.is_empty() {
                            t!("database_client.panel.no_tables_description")
                        } else {
                            t!(
                                "database_client.panel.other_databases",
                                databases = other_databases.join(", ")
                            )
                        }),
                    )
                    .child(self.render_tree(row_count, cx)),
                ActiveState::Connected { relations, .. } => {
                    let schema_count = relations
                        .iter()
                        .map(|relation| relation.schema.as_str())
                        .collect::<HashSet<_>>()
                        .len();
                    this.child(div().px_3().pt_3().child(self.filter_input.clone()))
                        .child(div().px_4().pt_3().pb_1().child(section_header(
                            t!("database_client.panel.schemas_header", count = schema_count),
                            None,
                        )))
                        .child(self.render_tree(row_count, cx))
                }
            })
            .child(self.render_footer(active, cx))
            .into_any_element()
    }

    fn render_tree(&self, row_count: usize, cx: &mut Context<Self>) -> Div {
        div().flex_1().min_h_0().px_2().child(
            uniform_list(
                "db-tree",
                row_count,
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    let rows = this.tree_rows(cx);
                    let pending_tables = this.tables_with_pending_edits(cx);
                    range
                        .filter_map(|index| {
                            rows.get(index)
                                .cloned()
                                .map(|row| this.render_row(index, row, &pending_tables, cx))
                        })
                        .collect()
                }),
            )
            .size_full(),
        )
    }

    fn render_footer(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let (tone, text) = match &active.state {
            ActiveState::Connecting => (
                StatusTone::Neutral,
                t!("database_client.panel.status_connecting"),
            ),
            ActiveState::Connected { latency, .. } => (
                StatusTone::Success,
                t!(
                    "database_client.panel.status_connected",
                    milliseconds = latency.as_millis()
                ),
            ),
            ActiveState::Failed(_) => {
                (StatusTone::Error, t!("database_client.panel.status_failed"))
            }
        };
        let this = cx.weak_entity();
        let current = active.connection.clone();
        let scope = self.stored(&current.id).map(|stored| stored.scope);
        let more_menu = PopoverMenu::new("db-footer-menu")
            .trigger_with_tooltip(
                IconButton::new("db-footer-menu-button", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text(t!("database_client.panel.more_actions")),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let this = this.clone();
                let current = current.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let reconnect = current.clone();
                    let edit = current.clone();
                    menu.entry(t!("database_client.panel.reload_catalog"), None, {
                        let this = this.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| this.refresh(cx)).log_err();
                        }
                    })
                    .entry(t!("database_client.panel.reconnect"), None, {
                        let this = this.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| this.connect(reconnect.clone(), cx))
                                .log_err();
                        }
                    })
                    .when_some(scope, |menu, scope| {
                        let this = this.clone();
                        menu.entry(
                            t!("database_client.panel.edit_connection_ellipsis"),
                            None,
                            move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.open_connect_view(
                                        Prefill::Edit(edit.clone(), scope),
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                            },
                        )
                    })
                    .separator()
                    .entry(
                        t!("database_client.panel.disconnect"),
                        None,
                        move |_, cx| {
                            this.update(cx, |this, cx| this.disconnect(cx)).log_err();
                        },
                    )
                }))
            });
        navigator_footer(
            Some(tone),
            text,
            Some(
                h_flex()
                    .gap_1()
                    .when(self.session().is_some(), |this| {
                        this.child(
                            capsule_button(
                                "db-new-query",
                                Some(IconName::Plus),
                                t!("database_client.panel.new_query"),
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_query(window, cx)),
                            ),
                        )
                    })
                    .child(more_menu)
                    .into_any_element(),
            ),
            cx,
        )
        .h(px(44.))
        .into_any_element()
    }

    fn render_suggestion(
        &self,
        index: usize,
        suggestion: &Suggestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, file, what) = match &suggestion.source {
            Source::Env { file, variable } => (IconName::FileCode, file.clone(), variable.clone()),
            Source::Compose {
                file,
                service,
                image,
            } => (
                IconName::Box,
                file.clone(),
                format!("service {service} · {image}"),
            ),
        };
        let password = match (&suggestion.source, &suggestion.password) {
            (_, None) => t!("database_client.panel.suggestion_no_password"),
            (Source::Env { .. }, Some(_)) => t!("database_client.panel.suggestion_password_env"),
            (Source::Compose { .. }, Some(_)) => {
                t!("database_client.panel.suggestion_password_compose")
            }
        };
        let chosen = suggestion.clone();
        let hover = cx.theme().colors().element_hover;
        v_flex()
            .id(("db-suggestion", index))
            .px_1p5()
            .py_1()
            .gap_0p5()
            .rounded(Radius::Sm.rems())
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        Label::new(file)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(what)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    ),
            )
            .child(
                Label::new(format!("{} · {password}", suggestion.address()))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .truncate(),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_connect_view(Prefill::Suggestion(chosen.clone()), window, cx)
            }))
            .into_any_element()
    }

    fn render_idle(&self, cx: &mut Context<Self>) -> AnyElement {
        let first_suggestion = self.suggestions.first().cloned();
        v_flex()
            .id("db-idle")
            .size_full()
            .overflow_y_scroll()
            .px_3()
            .py_3()
            .gap_2()
            .when_some(self.load_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .when_some(first_suggestion, |this, first| {
                let body = v_flex()
                    .gap_0p5()
                    .children(
                        self.suggestions
                            .iter()
                            .enumerate()
                            .map(|(index, suggestion)| {
                                self.render_suggestion(index, suggestion, cx)
                            }),
                    )
                    .child(
                        div().pt_1p5().child(
                            primary_button(
                                "db-connect-found",
                                None,
                                t!(
                                    "database_client.panel.connect_named",
                                    name = first.database.clone()
                                ),
                                None,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.open_connect_view(
                                        Prefill::Suggestion(first.clone()),
                                        window,
                                        cx,
                                    )
                                },
                            )),
                        ),
                    );
                this.child(choice_card_with_body(
                    "db-found-card",
                    IconName::MagnifyingGlass,
                    t!("database_client.panel.found_card_title"),
                    t!("database_client.panel.found_card_description"),
                    Some(t!("database_client.panel.recommended")),
                    Some(body.into_any_element()),
                    true,
                    cx,
                ))
            })
            .child(
                choice_card(
                    "db-new-connection-card",
                    IconName::Plus,
                    t!("database_client.connect.new_title"),
                    t!("database_client.panel.new_connection_card_description"),
                    None,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_connect_view(Prefill::New, window, cx)
                })),
            )
            .child(
                choice_card(
                    "db-import-card",
                    IconName::Download,
                    t!("database_client.panel.import_card_title"),
                    t!("database_client.panel.import_card_description"),
                    None,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
            )
            .when(!self.connections.is_empty(), |this| {
                let hover = cx.theme().colors().element_hover;
                this.child(div().pt_3().px_1().child(section_header(
                    t!("database_client.panel.saved_connections"),
                    None,
                )))
                .children(self.connections.iter().enumerate().map(|(index, stored)| {
                    let connection = stored.connection.clone();
                    h_flex()
                        .id(("db-saved", index))
                        .h(px(30.))
                        .px_1p5()
                        .gap_1p5()
                        .rounded(Radius::Sm.rems())
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(
                            Icon::new(IconName::Database)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(connection.name.clone()).size(LabelSize::Small))
                        .child(status_pill(
                            connection.environment.label(),
                            environment_tone(connection.environment),
                            cx,
                        ))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(connection.address())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .child(
                            Label::new(t!("database_client.panel.connect"))
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

struct Opened {
    session: Session,
    relations: Vec<Relation>,
    objects: Vec<SchemaObject>,
    columns: Vec<(String, String, String, String)>,
    database_size: Option<String>,
    other_databases: Vec<String>,
    latency: Duration,
}

async fn open_session(
    connection: &SavedConnection,
    credentials_provider: Arc<dyn CredentialsProvider>,
    fs: Arc<dyn Fs>,
    cx: &AsyncApp,
) -> anyhow::Result<Opened> {
    let password = match credentials_provider
        .read_credentials(&connection.keychain_url(), cx)
        .await
    {
        Ok(Some((_, bytes))) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => None,
        Err(error) => {
            log::error!("Banco: falha ao ler o keychain: {error:#}");
            None
        }
    };
    let password = match password {
        Some(password) => Some(password),
        None => fs
            .load(&discovery::pgpass_path())
            .await
            .ok()
            .and_then(|text| {
                discovery::pgpass_password(
                    &text,
                    &connection.host,
                    connection.port,
                    &connection.database,
                    &connection.user,
                )
            }),
    };
    let target = connection.open_target(password).await?;
    let session = Session::connect(&target).await?;
    let relations = catalog::list_relations(&session).await?;
    let other_databases = if relations.is_empty() {
        catalog::list_databases(&session)
            .await
            .log_err()
            .unwrap_or_default()
            .into_iter()
            .filter(|database| *database != connection.database)
            .collect()
    } else {
        Vec::new()
    };
    // The dock still works without these; a failure only leaves their part of the tree empty.
    let objects = catalog::list_objects(&session)
        .await
        .log_err()
        .unwrap_or_default();
    let columns = catalog::list_all_columns(&session)
        .await
        .log_err()
        .unwrap_or_default();
    let database_size = catalog::database_size(&session).await.log_err().flatten();
    let started = Instant::now();
    session.run("select 1", 1).await?;
    Ok(Opened {
        session,
        relations,
        objects,
        columns,
        database_size,
        other_databases,
        latency: started.elapsed(),
    })
}

async fn list_queries(fs: &dyn Fs, root: &Path) -> Vec<PathBuf> {
    let Ok(mut entries) = fs.read_dir(&root.join(QUERIES_DIR)).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        if let Ok(path) = entry
            && path.extension().is_some_and(|extension| extension == "sql")
        {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn last_used_key(root: &Path) -> String {
    format!("database_client-last-used-{}", root.display())
}

fn schema_key(schema: &str) -> String {
    format!("schema:{schema}")
}

fn group_key(schema: &str, group: &str) -> String {
    format!("group:{schema}:{group}")
}

fn relation_key(schema: &str, name: &str) -> String {
    format!("relation:{schema}.{name}")
}

/// `16.4 (Debian 16.4-1.pgdg120+2)` → `16.4`.
fn short_version(version: &str) -> Option<String> {
    version
        .split_whitespace()
        .next()
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
}

/// `48213` → `48.213`, `1234567` → `1,2 M`, as the dock shows row estimates.
pub(crate) fn number_separators() -> (char, char) {
    match i18n::current_language() {
        i18n::Language::PortugueseBrazil => (',', '.'),
        i18n::Language::English => ('.', ','),
    }
}

fn format_count_compact(count: i64) -> String {
    match count {
        1_000_000.. => format_count(count),
        1_000.. => t!(
            "database_client.panel.rows_compact_thousands",
            count = count / 1_000
        )
        .to_string(),
        _ => count.to_string(),
    }
}

pub(crate) fn format_count(count: i64) -> String {
    let (decimal_separator, thousands_separator) = number_separators();
    if count >= 1_000_000 {
        format!("{:.1} M", count as f64 / 1_000_000.0).replace('.', &decimal_separator.to_string())
    } else {
        let digits = count.to_string();
        let mut result = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                result.push(thousands_separator);
            }
            result.push(digit);
        }
        result
    }
}

/// What the workspace's active tab shows, for the tree to mark it.
fn active_item_of(workspace: &Workspace, cx: &App) -> Option<ActiveItem> {
    let item = workspace.active_item(cx)?;
    if let Some(table) = item.downcast::<table_view::TableView>() {
        let (connection_id, schema, name) = table.read(cx).relation();
        return Some(ActiveItem::Table {
            connection_id: connection_id.to_owned(),
            schema: schema.to_owned(),
            name: name.to_owned(),
        });
    }
    item.downcast::<query_view::SqlQueryView>()
        .map(|query| ActiveItem::Query(query.read(cx).path().to_path_buf()))
}

/// `há 5 min`, `há 3 h`, `ontem`, `há 5 d`, for the saved queries.
fn relative_age(modified: SystemTime) -> SharedString {
    let seconds = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs();
    match seconds {
        0..60 => t!("database_client.panel.age_now"),
        60..3_600 => t!("database_client.panel.age_minutes", count = seconds / 60),
        3_600..86_400 => t!("database_client.panel.age_hours", count = seconds / 3_600),
        86_400..172_800 => t!("database_client.panel.age_yesterday"),
        _ => t!("database_client.panel.age_days", count = seconds / 86_400),
    }
}

const SQL_KEYWORDS: &[&str] = &[
    "add",
    "alter",
    "and",
    "as",
    "begin",
    "by",
    "case",
    "column",
    "commit",
    "concurrently",
    "create",
    "delete",
    "desc",
    "distinct",
    "else",
    "end",
    "explain",
    "false",
    "filter",
    "from",
    "group",
    "if",
    "index",
    "insert",
    "interval",
    "into",
    "is",
    "join",
    "left",
    "limit",
    "not",
    "null",
    "on",
    "or",
    "order",
    "rollback",
    "select",
    "set",
    "table",
    "then",
    "true",
    "update",
    "values",
    "when",
    "where",
];

/// One line of SQL colored like the editor, for the read-only SQL the boards show in cards and
/// popovers.
pub(crate) fn sql_line(line: &str, cx: &App) -> Div {
    let syntax = cx.theme().syntax();
    let color_of = |name: &str, fallback: Color| {
        syntax
            .style_for_name(name)
            .and_then(|style| style.color)
            .map(Color::Custom)
            .unwrap_or(fallback)
    };
    let keyword = color_of("keyword", Color::Accent);
    let string = color_of("string", Color::Success);
    let number = color_of("number", Color::Warning);
    let mut tokens: Vec<(String, Color)> = Vec::new();
    let mut push = |text: &str, color: Color| match tokens.last_mut() {
        Some((last, last_color)) if *last_color == color => last.push_str(text),
        _ => tokens.push((text.to_owned(), color)),
    };
    let mut rest = line;
    while !rest.is_empty() {
        if rest.starts_with("--") {
            push(rest, Color::Muted);
            break;
        }
        let first = rest.chars().next().unwrap_or(' ');
        let end = if first == '\'' {
            rest[1..].find('\'').map_or(rest.len(), |end| end + 2)
        } else if first.is_alphanumeric() || first == '_' {
            rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(rest.len())
        } else {
            first.len_utf8()
        };
        let token = &rest[..end];
        let color = if first == '\'' {
            string
        } else if token.chars().all(|c| c.is_ascii_digit() || c == '.') && first.is_ascii_digit() {
            number
        } else if SQL_KEYWORDS.contains(&token.to_lowercase().as_str()) {
            keyword
        } else if first.is_alphanumeric() || first == '_' {
            Color::Default
        } else {
            Color::Muted
        };
        push(token, color);
        rest = &rest[end..];
    }
    h_flex().children(tokens.into_iter().map(|(text, color)| {
        Label::new(text)
            .size(LabelSize::Small)
            .buffer_font(cx)
            .color(color)
            .single_line()
    }))
}

/// The SQL clause marker of the table filter (`WHERE`, `ORDER BY`).
pub(crate) fn sql_tag(text: &'static str, cx: &App) -> Div {
    h_flex()
        .flex_none()
        .h(px(20.))
        .px_1p5()
        .rounded_sm()
        .bg(cx.theme().colors().border_focused.opacity(0.22))
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Accent),
        )
}

/// A [`ui::segment_option`] as wide as its label, for segmented controls that sit in a toolbar.
pub(crate) fn segment(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<Div> {
    ui::segment_option(id, label, selected, true, cx)
        .flex_initial()
        .px_2p5()
        .h(px(26.))
}

pub(crate) fn environment_tone(environment: Environment) -> StatusTone {
    match environment {
        Environment::Local => StatusTone::Neutral,
        Environment::Dev => StatusTone::Success,
        Environment::Staging => StatusTone::Warning,
        Environment::Prod => StatusTone::Error,
    }
}

pub(crate) fn environment_select(
    id: impl Into<gpui::ElementId>,
    current: Environment,
    enabled: bool,
    on_select: impl Fn(Environment, &mut App) + Clone + 'static,
    cx: &App,
) -> PopoverMenu<ContextMenu> {
    let id = id.into();
    let trigger_id = gpui::ElementId::NamedChild(Arc::new(id.clone()), "trigger".into());
    PopoverMenu::new(id)
        .full_width(true)
        .trigger(PopoverTriggerArea::new(
            select_field(trigger_id, current.label(), cx).child(
                div()
                    .flex()
                    .flex_none()
                    .child(environment_tone(current).dot(cx)),
            ),
        ))
        .anchor(gpui::Anchor::TopLeft)
        .menu(move |window, cx| {
            if !enabled {
                return None;
            }
            let on_select = on_select.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for environment in Environment::ALL {
                    let on_select = on_select.clone();
                    menu = menu.toggleable_entry(
                        environment.label(),
                        environment == current,
                        IconPosition::End,
                        None,
                        move |_, cx| on_select(environment, cx),
                    );
                }
                menu
            }))
        })
}

pub(crate) fn scope_control(
    id_prefix: &'static str,
    current: Scope,
    on_select: impl Fn(&Scope, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let on_select = std::rc::Rc::new(on_select);
    let option = |scope: Scope, label: SharedString| {
        let on_select = on_select.clone();
        segment(
            gpui::ElementId::NamedChild(
                Arc::new(id_prefix.into()),
                match scope {
                    Scope::Profile => "profile".into(),
                    Scope::Project => "project".into(),
                },
            ),
            label,
            current == scope,
            cx,
        )
        .on_click(move |_, window, cx| on_select(&scope, window, cx))
    };
    segmented_control(cx)
        .child(option(
            Scope::Profile,
            t!("database_client.connect.scope_profile"),
        ))
        .child(option(
            Scope::Project,
            t!(
                "database_client.connect.scope_project",
                file = connection::PROJECT_FILE
            ),
        ))
}

pub(crate) fn connection_chip(
    id: impl Into<gpui::ElementId>,
    connection: &SavedConnection,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .flex_none()
        .h(px(30.))
        .px_2p5()
        .gap_2()
        .rounded(Radius::Sm.rems())
        .border_1()
        .border_color(colors.border_selected)
        .bg(colors.element_background)
        .child(
            Icon::new(IconName::Database)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(connection.name.clone())
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
        )
        .child(environment_tone(connection.environment).dot(cx))
        .child(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
}

impl Render for DatabasePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if !self.loaded {
            EmptyState::new(
                IconName::LoadCircle,
                t!("database_client.panel.searching_connections"),
            )
            .spinning(true)
            .into_any_element()
        } else if self.root.is_none() {
            EmptyState::new(
                IconName::Folder,
                t!("database_client.panel.open_project_folder"),
            )
            .into_any_element()
        } else {
            match &self.active {
                Some(active) => self.render_connected(active, cx),
                None => self.render_idle(cx),
            }
        };
        v_flex()
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.open_connect_view(Prefill::New, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewQuery, window, cx| this.new_query(window, cx)))
            .child(content)
    }
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Panel for DatabasePanel {
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::DATABASE, &self.project, cx)
    }

    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }

    fn panel_key() -> &'static str {
        "DatabasePanel"
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
        Some(IconName::Database)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Banco")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        12
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_like_the_dock() {
        assert_eq!(format_count(3), "3");
        assert_eq!(format_count(48213), "48,213");
        assert_eq!(format_count(380_000), "380,000");
        assert_eq!(format_count(1_234_567), "1.2 M");
        assert_eq!(
            short_version("16.15 (Debian 16.15-1.pgdg13+2)").as_deref(),
            Some("16.15")
        );
    }
}
