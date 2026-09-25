use crate::{
    NewConnection, ToggleFocus,
    catalog::{self, ColumnInfo, Relation, RelationKind},
    connect_view::{self, Prefill},
    connection::{self, Environment, SavedConnection, Scope},
    discovery::{self, Source, Suggestion},
    session::Session,
};
use collections::{HashMap, HashSet};
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    Action as _, AnyElement, AsyncApp, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Pixels, Subscription, Task, WeakEntity, px, uniform_list,
};
use project::Project;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{ButtonLike, ContextMenu, Indicator, PopoverMenu, Tooltip, prelude::*};
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
        latency: Duration,
    },
    Failed(SharedString),
}

pub struct Active {
    pub connection: SavedConnection,
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
        label: &'static str,
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
    },
    Column(ColumnInfo),
    ColumnsLoading,
    ColumnsFailed(SharedString),
    QueriesHeader(usize),
    Query {
        path: PathBuf,
        name: String,
    },
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
            InputField::new(window, cx, "Filtrar tabelas…").start_icon(IconName::MagnifyingGlass)
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

    /// The session of the active connection, for the views that browse it.
    pub fn session(&self) -> Option<(SavedConnection, Arc<Session>)> {
        let active = self.active.as_ref()?;
        match &active.state {
            ActiveState::Connected { session, .. } => {
                Some((active.connection.clone(), session.clone()))
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

    pub fn connect(&mut self, connection: SavedConnection, cx: &mut Context<Self>) {
        self.columns.clear();
        self.column_tasks.clear();
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
                    Ok((session, relations, latency)) => {
                        if this.expanded.is_empty() {
                            this.expanded.insert(schema_key("public"));
                            this.expanded.insert(group_key("public", "tables"));
                        }
                        ActiveState::Connected {
                            session: Arc::new(session),
                            relations,
                            latency,
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
            if !password.is_empty() {
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

    fn open_query(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
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

    fn tree_rows(&self, cx: &App) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        let Some(Active {
            state: ActiveState::Connected { relations, .. },
            ..
        }) = &self.active
        else {
            return rows;
        };
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let filtering = !filter.is_empty();
        let mut schemas: Vec<(&str, Vec<&Relation>)> = Vec::new();
        for relation in relations {
            if filtering && !relation.name.to_lowercase().contains(&filter) {
                continue;
            }
            match schemas.last_mut() {
                Some((schema, list)) if *schema == relation.schema => list.push(relation),
                _ => schemas.push((&relation.schema, vec![relation])),
            }
        }
        for (schema, list) in schemas {
            let expanded = filtering || self.expanded.contains(&schema_key(schema));
            rows.push(TreeRow::Schema {
                name: schema.to_owned(),
                count: list.len(),
                expanded,
            });
            if !expanded {
                continue;
            }
            let groups: [(&str, &'static str, IconName, fn(RelationKind) -> bool); 2] = [
                ("tables", "Tabelas", IconName::Table, |kind| {
                    matches!(
                        kind,
                        RelationKind::Table
                            | RelationKind::PartitionedTable
                            | RelationKind::ForeignTable
                    )
                }),
                ("views", "Views", IconName::Eye, |kind| {
                    matches!(kind, RelationKind::View | RelationKind::MaterializedView)
                }),
            ];
            for (group, label, icon, belongs) in groups {
                let members: Vec<&&Relation> =
                    list.iter().filter(|relation| belongs(relation.kind)).collect();
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
                if !expanded {
                    continue;
                }
                for relation in members {
                    let key = relation_key(schema, &relation.name);
                    let expanded = self.expanded.contains(&key);
                    rows.push(TreeRow::Relation {
                        schema: schema.to_owned(),
                        name: relation.name.clone(),
                        kind: relation.kind,
                        estimated_rows: relation.estimated_rows,
                        expanded,
                    });
                    if !expanded {
                        continue;
                    }
                    match self.columns.get(&key) {
                        Some(Columns::Loaded(columns)) => {
                            rows.extend(columns.iter().cloned().map(TreeRow::Column));
                        }
                        Some(Columns::Failed(error)) => {
                            rows.push(TreeRow::ColumnsFailed(error.clone()))
                        }
                        Some(Columns::Loading) | None => rows.push(TreeRow::ColumnsLoading),
                    }
                }
            }
        }
        if !self.saved_queries.is_empty() && !filtering {
            rows.push(TreeRow::QueriesHeader(self.saved_queries.len()));
            rows.extend(self.saved_queries.iter().map(|path| TreeRow::Query {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: path.clone(),
            }));
        }
        rows
    }

    fn render_row(&self, index: usize, row: TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let base = h_flex()
            .id(("db-row", index))
            .w_full()
            .h(px(26.))
            .px_2()
            .gap_1p5()
            .rounded_sm();
        let clickable = |base: gpui::Stateful<gpui::Div>| {
            base.cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
        };
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
        match row {
            TreeRow::Schema {
                name,
                count: total,
                expanded,
            } => clickable(base)
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
                .pl(px(24.))
                .child(chevron(expanded))
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    div()
                        .flex_1()
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
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
            } => clickable(base)
                .pl(px(40.))
                .child(chevron(expanded))
                .child(
                    Icon::new(match kind {
                        RelationKind::View | RelationKind::MaterializedView => IconName::Eye,
                        _ => IconName::Table,
                    })
                    .size(IconSize::Small)
                    .color(if expanded { Color::Accent } else { Color::Muted }),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .when(expanded, |label| label.weight(FontWeight::SEMIBOLD))
                            .truncate(),
                    ),
                )
                .when_some(estimated_rows, |this, rows| {
                    this.child(count(format_count(rows)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_relation(schema.clone(), name.clone(), cx)
                }))
                .into_any_element(),
            TreeRow::Column(column) => base
                .pl(px(74.))
                .child(
                    div()
                        .w(px(16.))
                        .when(column.primary_key, |this| {
                            this.child(
                                Label::new("PK")
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Warning),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .child(
                            Label::new(column.name)
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        )
                        .child(
                            Label::new(column.type_name)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(column.unique, |this| {
                    this.child(
                        Label::new("único")
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    )
                })
                .when(!column.not_null && !column.primary_key, |this| {
                    this.child(
                        Label::new("null")
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Disabled),
                    )
                })
                .into_any_element(),
            TreeRow::ColumnsLoading => base
                .pl(px(90.))
                .child(
                    Label::new("Lendo colunas…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            TreeRow::ColumnsFailed(error) => base
                .pl(px(90.))
                .child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error)
                        .truncate(),
                )
                .into_any_element(),
            TreeRow::QueriesHeader(total) => h_flex()
                .h(px(34.))
                .px_2()
                .pt_2()
                .gap_1p5()
                .child(
                    Label::new("QUERIES SALVAS")
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .child(count(total.to_string()))
                .child(div().flex_1())
                .child(
                    Label::new(QUERIES_DIR)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Disabled),
                )
                .into_any_element(),
            TreeRow::Query { path, name } => clickable(base)
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
                .into_any_element(),
        }
    }

    fn render_connection_card(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let connection = &active.connection;
        let version = match &active.state {
            ActiveState::Connected { session, .. } => short_version(&session.server_version),
            _ => None,
        };
        let subtitle = match version {
            Some(version) => format!("{} · PostgreSQL {version}", connection.address()),
            None => connection.address(),
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
        PopoverMenu::new("db-connection-menu")
            .full_width(true)
            .trigger(
                ButtonLike::new("db-connection-card")
                    .full_width()
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Large)
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
                                    .bg(Color::Accent.color(cx).opacity(0.12))
                                    .child(
                                        Icon::new(IconName::Database)
                                            .size(IconSize::Small)
                                            .color(Color::Accent),
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
                                            .child(environment_chip(connection.environment, cx)),
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
            ActiveState::Connected { latency, .. } => (
                Color::Success,
                format!("conectado · {} ms", latency.as_millis()),
            ),
            ActiveState::Failed(_) => (Color::Error, "falhou".to_string()),
        };
        let chip = |label: String, color: Color| {
            h_flex()
                .h(px(22.))
                .px_2()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .child(Indicator::dot().color(color))
                .child(Label::new(label).size(LabelSize::XSmall).color(color))
        };
        let read_only = active.connection.read_only;
        h_flex()
            .px_3()
            .pt_2()
            .gap_1p5()
            .flex_wrap()
            .child(chip(status, dot))
            .child(
                h_flex()
                    .h(px(22.))
                    .px_2()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(if read_only {
                            IconName::Lock
                        } else {
                            IconName::LockOff
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(if read_only {
                            "somente leitura"
                        } else {
                            "leitura e escrita"
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("db-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Recarregar o catálogo"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .into_any_element()
    }

    fn render_connected(&self, active: &Active, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree_rows(cx);
        let row_count = rows.len();
        let summary = match &active.state {
            ActiveState::Connected { relations, .. } => {
                let schemas = relations
                    .iter()
                    .map(|relation| relation.schema.as_str())
                    .collect::<HashSet<_>>()
                    .len();
                let tables = relations
                    .iter()
                    .filter(|relation| {
                        !matches!(
                            relation.kind,
                            RelationKind::View | RelationKind::MaterializedView
                        )
                    })
                    .count();
                Some(format!(
                    "{tables} {} · {schemas} {}",
                    if tables == 1 { "tabela" } else { "tabelas" },
                    if schemas == 1 { "schema" } else { "schemas" }
                ))
            }
            _ => None,
        };
        v_flex()
            .size_full()
            .child(div().px_3().pt_2p5().child(self.render_connection_card(active, cx)))
            .child(self.render_status_row(active, cx))
            .map(|this| match &active.state {
                ActiveState::Connecting => this.child(
                    div().flex_1().flex().items_center().justify_center().child(
                        Label::new("Conectando…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
                ActiveState::Failed(error) => {
                    let retry = active.connection.clone();
                    let edit = active.connection.clone();
                    let scope = self.stored(&edit.id).map(|stored| stored.scope);
                    this.child(
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
                                        Icon::new(IconName::XCircle)
                                            .size(IconSize::Small)
                                            .color(Color::Error),
                                    )
                                    .child(
                                        Label::new("Não deu para conectar")
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD),
                                    ),
                            )
                            .child(
                                Label::new(error.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("db-retry", "Tentar de novo")
                                            .style(ButtonStyle::Filled)
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.connect(retry.clone(), cx)
                                            })),
                                    )
                                    .when_some(scope, |this, scope| {
                                        this.child(
                                            Button::new("db-edit", "Editar conexão")
                                                .style(ButtonStyle::Subtle)
                                                .label_size(LabelSize::Small)
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
                            ),
                    )
                }
                ActiveState::Connected { .. } => this
                    .child(div().px_3().pt_2().child(self.filter_input.clone()))
                    .child(
                        h_flex().px_3().pt_3().pb_1().child(
                            Label::new("SCHEMAS")
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        div().flex_1().min_h_0().px_1().child(
                            uniform_list(
                                "db-tree",
                                row_count,
                                cx.processor(move |this, range: Range<usize>, _window, cx| {
                                    let rows = this.tree_rows(cx);
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
                    ),
            })
            .when(!matches!(active.state, ActiveState::Connected { .. }), |this| {
                this.child(div().flex_1())
            })
            .child(self.render_footer(summary, cx))
            .into_any_element()
    }

    fn render_footer(&self, summary: Option<String>, cx: &mut Context<Self>) -> AnyElement {
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
            .child(
                Button::new("db-new-connection-footer", "Nova conexão")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .color(Color::Accent)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_connect_view(Prefill::New, window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_suggestion(
        &self,
        index: usize,
        suggestion: &Suggestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, file, what) = match &suggestion.source {
            Source::Env { file, variable } => (IconName::FileLock, file.clone(), variable.clone()),
            Source::Compose {
                file,
                service,
                image,
            } => (IconName::Box, file.clone(), format!("service {service} · {image}")),
        };
        let password = match (&suggestion.source, &suggestion.password) {
            (_, None) => "sem senha no arquivo",
            (Source::Env { .. }, Some(_)) => "senha: do .env",
            (Source::Compose { .. }, Some(_)) => "senha: POSTGRES_PASSWORD",
        };
        let chosen = suggestion.clone();
        v_flex()
            .id(("db-suggestion", index))
            .p_2p5()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .cursor_pointer()
            .hover(|style| style.border_color(Color::Accent.color(cx)))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .child(
                        Label::new(file)
                            .size(LabelSize::Small)
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
                    )
                    .child(Label::new("Usar").size(LabelSize::XSmall).color(Color::Accent)),
            )
            .child(
                Label::new(suggestion.address())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                Label::new(password)
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_connect_view(Prefill::Suggestion(chosen.clone()), window, cx)
            }))
            .into_any_element()
    }

    fn render_idle(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_connections = !self.connections.is_empty();
        let section = |title: &'static str, count: usize| {
            h_flex()
                .pt_3()
                .pb_1p5()
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
        };
        v_flex()
            .id("db-idle")
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
                                Icon::new(IconName::Database)
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
                                "Conecte um Postgres para listar schemas, abrir tabelas e rodar \
                                 queries sem sair do editor.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                    )
                    .child(
                        Button::new("db-new-connection", "Nova conexão")
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
                this.child(section("CONEXÕES SALVAS", self.connections.len()))
                    .children(self.connections.iter().enumerate().map(|(index, stored)| {
                        let connection = stored.connection.clone();
                        h_flex()
                            .id(("db-saved", index))
                            .h(px(30.))
                            .px_2()
                            .gap_1p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                            .child(
                                Icon::new(IconName::Database)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(connection.name.clone()).size(LabelSize::Small))
                            .child(environment_chip(connection.environment, cx))
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
                                Label::new("Conectar")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connect(connection.clone(), cx)
                            }))
                    }))
            })
            .when(!self.suggestions.is_empty(), |this| {
                this.child(section("ENCONTRADAS NO PROJETO", self.suggestions.len()))
                    .child(
                        v_flex().gap_2().children(
                            self.suggestions
                                .iter()
                                .enumerate()
                                .map(|(index, suggestion)| {
                                    self.render_suggestion(index, suggestion, cx)
                                }),
                        ),
                    )
                    .child(
                        div().pt_2().child(
                            Label::new(
                                "Sugestões nunca conectam sozinhas. Nada vai para o repositório \
                                 sem você escolher \"Projeto\".",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Disabled),
                        ),
                    )
            })
            .into_any_element()
    }
}

async fn open_session(
    connection: &SavedConnection,
    credentials_provider: Arc<dyn CredentialsProvider>,
    fs: Arc<dyn Fs>,
    cx: &AsyncApp,
) -> anyhow::Result<(Session, Vec<Relation>, Duration)> {
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
    let session = Session::connect(&connection.target(password.as_deref())).await?;
    let relations = catalog::list_relations(&session).await?;
    let started = Instant::now();
    session.run("select 1", 1).await?;
    Ok((session, relations, started.elapsed()))
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
fn format_count(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1} M", count as f64 / 1_000_000.0).replace('.', ",")
    } else {
        let digits = count.to_string();
        let mut result = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                result.push('.');
            }
            result.push(digit);
        }
        result
    }
}

pub fn environment_color(environment: Environment) -> Color {
    match environment {
        Environment::Local => Color::Muted,
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
        .child(Label::new(environment.label()).size(LabelSize::XSmall).color(color))
}

impl Render for DatabasePanel {
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
                    Label::new("Abra uma pasta de projeto para conectar a um banco.")
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
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.open_connect_view(Prefill::New, window, cx)
            }))
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
        assert_eq!(format_count(48213), "48.213");
        assert_eq!(format_count(380_000), "380.000");
        assert_eq!(format_count(1_234_567), "1,2 M");
        assert_eq!(
            short_version("16.15 (Debian 16.15-1.pgdg13+2)").as_deref(),
            Some("16.15")
        );
    }
}
