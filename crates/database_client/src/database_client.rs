//! A Postgres client for the dock: connect, browse the catalog and run SQL. The socket lives on
//! the shared tokio runtime of `reqwest_client`; views only await the results.

mod catalog;
mod connect_view;
mod connection;
mod discovery;
mod grid;
mod panel;
mod query_view;
mod session;
mod statements;
mod table_view;
mod tls;
mod write_guard;

pub use catalog::{ColumnInfo, Relation, RelationKind, list_columns, list_relations};
pub use connection::{Environment, SavedConnection};
pub use panel::DatabasePanel;
pub use session::{Column, ConnectTarget, QueryOutcome, ResultSet, ServerError, Session};
pub use tls::SslMode;

use gpui::{Subscription, WeakEntity, actions};
use std::any::TypeId;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::StatusBarButton};

actions!(
    database_client,
    [
        /// Opens the database panel, or hands focus back if it already has it.
        ToggleFocus,
        /// Opens the form for a new Postgres connection.
        NewConnection,
        /// Saves the connection in the focused form and connects to it.
        SaveConnection,
        /// Reloads the open table with the typed WHERE and ORDER BY.
        ApplyTableFilter,
        /// Runs the SQL statement under the cursor, or the selection.
        RunStatement,
        /// Runs every statement in the SQL file.
        RunScript,
        /// Shows the plan of the statement under the cursor (EXPLAIN ANALYZE for reads).
        ExplainStatement,
        /// Cancels the query running in the SQL tab, on the server.
        CancelQuery,
        /// Creates a SQL file in .asylum/db/queries and opens it against the dock's connection.
        NewQuery,
        /// Opens the SQL file in the active editor against the dock's connection.
        OpenInDatabase,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<DatabasePanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        // `panel` opens the dock, `connect` also opens the connection form,
        // `table:<schema>.<name>[:<tab>]` opens a table once the dock has connected and
        // `query:<file relative to the project>[:run]` opens (and runs) a SQL file.
        if let (Ok(step), Some(window)) = (std::env::var("DATABASE_CLIENT_DEBUG_OPEN"), window) {
            cx.spawn_in(window, async move |workspace, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let panel = workspace.update_in(cx, |workspace, window, cx| {
                    open(workspace, window, cx);
                    let panel = workspace.panel::<DatabasePanel>(cx);
                    if step == "connect"
                        && let Some(panel) = panel.clone()
                    {
                        connect_view::open_in(
                            workspace,
                            panel,
                            connect_view::Prefill::New,
                            window,
                            cx,
                        );
                    }
                    panel
                })?;
                if let (Some(target), Some(panel)) = (step.strip_prefix("query:"), panel.clone()) {
                    let (file, run) = match target.strip_suffix(":run") {
                        Some(file) => (file.to_owned(), true),
                        None => (target.to_owned(), false),
                    };
                    for _ in 0..40 {
                        if panel.read_with(cx, |panel, _| panel.session().is_some()) {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(250))
                            .await;
                    }
                    panel.update_in(cx, |panel, window, cx| {
                        if let Some(root) = panel.root().map(|root| root.join(&file)) {
                            panel.open_query(root, window, cx);
                        }
                    })?;
                    if run {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(2))
                            .await;
                        let views = workspace.update(cx, |workspace, cx| {
                            workspace
                                .items_of_type::<query_view::SqlQueryView>(cx)
                                .collect::<Vec<_>>()
                        })?;
                        for view in views {
                            view.update_in(cx, |view, window, cx| view.run_statement(window, cx))?;
                        }
                    }
                    return anyhow::Ok(());
                }
                let (Some(target), Some(panel)) = (step.strip_prefix("table:"), panel) else {
                    return anyhow::Ok(());
                };
                let mut parts = target.splitn(2, ':');
                let relation = parts.next().unwrap_or_default().to_owned();
                let tab = parts.next().map(str::to_owned);
                let (schema, name) = relation
                    .split_once('.')
                    .map(|(schema, name)| (schema.to_owned(), name.to_owned()))
                    .unwrap_or_else(|| ("public".to_owned(), relation.clone()));
                for _ in 0..40 {
                    if panel.read_with(cx, |panel, _| panel.session().is_some()) {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                }
                panel.update_in(cx, |panel, window, cx| {
                    panel.open_table(schema, name, RelationKind::Table, None, window, cx)
                })?;
                if let Some(tab) = tab {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;
                    workspace.update(cx, |workspace, cx| {
                        let views = workspace
                            .items_of_type::<table_view::TableView>(cx)
                            .collect::<Vec<_>>();
                        for view in views {
                            view.update(cx, |view, cx| view.select_tab_named(&tab, cx));
                        }
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewQuery, window, cx| {
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                panel.update(cx, |panel, cx| panel.new_query(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &OpenInDatabase, window, cx| {
            let path = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .and_then(|project_path| {
                    workspace.project().read(cx).absolute_path(&project_path, cx)
                });
            let Some(path) = path else {
                return;
            };
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                panel.update(cx, |panel, cx| panel.open_query(path, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                connect_view::open_in(workspace, panel, connect_view::Prefill::New, window, cx);
            }
        });
    })
    .detach();
}

/// Adds the panel the first time it is asked for, so projects without a database don't get a
/// tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<DatabasePanel>(cx).is_none() {
        let panel = cx.new(|cx| DatabasePanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<DatabasePanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<DatabasePanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
}

fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<DatabasePanel>())
    })
}

/// The database button in the status bar's toolkit group, lit while the panel is open.
pub struct DatabaseToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl DatabaseToolkitButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let dock_subscriptions = workspace
            .all_docks()
            .into_iter()
            .map(|dock| cx.observe(dock, |_, _, cx| cx.notify()))
            .collect();
        Self {
            workspace: workspace.weak_handle(),
            _dock_subscriptions: dock_subscriptions,
        }
    }
}

impl Render for DatabaseToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();

        StatusBarButton::new("toolkit-database", IconName::Database, is_open)
            .tab_index(0isize)
            .aria_label("Banco")
            .tooltip(|_window, cx| Tooltip::for_action("Banco", &ToggleFocus, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<DatabasePanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for DatabaseToolkitButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The panel has no status bar button of its own, so this is the only visible way in.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    async fn sleep(duration: Duration) {
        reqwest_client::runtime()
            .spawn(async move { tokio::time::sleep(duration).await })
            .await
            .ok();
    }

    fn server_error(error: &anyhow::Error) -> &ServerError {
        error
            .downcast_ref::<ServerError>()
            .unwrap_or_else(|| panic!("expected a server error, got {error:#}"))
    }

    /// Runs against a real server:
    /// `DATABASE_CLIENT_URL=postgres://postgres:postgres@localhost:5432/postgres?sslmode=require
    /// cargo test -p database_client -- --ignored --nocapture`.
    /// Creates and drops the schema `database_client_spike`.
    #[test]
    #[ignore]
    fn against_a_real_server() {
        let url = std::env::var("DATABASE_CLIENT_URL").expect("DATABASE_CLIENT_URL");
        let target = ConnectTarget::parse(&url).unwrap();
        futures::executor::block_on(async {
            let started = Instant::now();
            let session = Session::connect(&target).await.unwrap();
            let connected_in = started.elapsed();
            let encrypted = session
                .run("select ssl from pg_stat_ssl where pid = pg_backend_pid()", 1)
                .await
                .unwrap();
            let encrypted = encrypted.result_sets[0].rows[0][0].as_deref() == Some("t");
            eprintln!(
                "Postgres {} · sslmode={} · TLS {} · conectou em {connected_in:?}",
                session.server_version,
                target.ssl_mode.as_str(),
                if encrypted { "sim" } else { "não" },
            );
            match target.ssl_mode {
                SslMode::Disable => assert!(!encrypted),
                SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => assert!(encrypted),
                SslMode::Prefer => {}
            }

            session
                .run(
                    "drop schema if exists database_client_spike cascade;
                     create schema database_client_spike;
                     create table database_client_spike.users (
                         id bigint primary key, name text not null, profile jsonb, roles text[],
                         document varchar(14) unique
                     );
                     insert into database_client_spike.users values
                         (1, 'Ana', '{\"theme\": \"dark\"}', '{investor,beta}', '1'),
                         (2, 'Bruno', null, '{investor}', '2'),
                         (3, 'Camila', '{}', '{}', null);
                     create table database_client_spike.devices (
                         id bigint primary key,
                         user_id bigint not null references database_client_spike.users(id) on delete cascade
                     );
                     create index devices_user_idx on database_client_spike.devices (user_id);
                     analyze database_client_spike.users;",
                    1000,
                )
                .await
                .unwrap();

            let relations = list_relations(&session).await.unwrap();
            let users = relations
                .iter()
                .find(|relation| {
                    relation.schema == "database_client_spike" && relation.name == "users"
                })
                .expect("the spike table is listed");
            assert_eq!(users.kind, RelationKind::Table);
            assert_eq!(users.estimated_rows, Some(3));
            eprintln!("{} relações no catálogo", relations.len());

            let columns = list_columns(&session, "database_client_spike", "users")
                .await
                .unwrap();
            let summary = columns
                .iter()
                .map(|column| {
                    format!(
                        "{} {}{}{}{}",
                        column.name,
                        column.type_name,
                        if column.not_null { " not null" } else { "" },
                        if column.primary_key { " pk" } else { "" },
                        if column.unique { " unique" } else { "" },
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                summary,
                [
                    "id bigint not null pk",
                    "name text not null",
                    "profile jsonb",
                    "roles text[]",
                    "document character varying(14) unique",
                ]
            );

            let indexes = catalog::list_indexes(&session, "database_client_spike", "users")
                .await
                .unwrap();
            let names = indexes
                .iter()
                .map(|index| (index.name.as_str(), index.primary, index.constraint))
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [
                    ("users_pkey", true, true),
                    ("users_document_key", false, true)
                ]
            );
            let foreign_keys =
                catalog::list_foreign_keys(&session, "database_client_spike", "users")
                    .await
                    .unwrap();
            assert_eq!(foreign_keys.len(), 1);
            assert_eq!(
                foreign_keys[0].direction,
                catalog::ForeignKeyDirection::ReferencedBy
            );
            assert_eq!(foreign_keys[0].on_delete, "CASCADE");
            assert_eq!(
                foreign_keys[0].other_table,
                "database_client_spike.devices"
            );
            let device_columns = list_columns(&session, "database_client_spike", "devices")
                .await
                .unwrap();
            let device_indexes =
                catalog::list_indexes(&session, "database_client_spike", "devices")
                    .await
                    .unwrap();
            let constraints =
                catalog::list_constraints(&session, "database_client_spike", "devices")
                    .await
                    .unwrap();
            let ddl = catalog::table_ddl(
                "database_client_spike",
                "devices",
                &device_columns,
                &constraints,
                &device_indexes,
            );
            eprintln!("{ddl}");
            assert!(ddl.starts_with("create table \"database_client_spike\".\"devices\" ("));
            assert!(ddl.contains("ON DELETE CASCADE"));
            assert!(ddl.contains("CREATE INDEX devices_user_idx"));

            let read_only = session.connect_read_only().await.unwrap();
            let error = read_only
                .run("delete from database_client_spike.users", 10)
                .await
                .unwrap_err();
            assert_eq!(server_error(&error).code, "25006");

            let outcome = session
                .run("select * from database_client_spike.users order by id", 1000)
                .await
                .unwrap();
            let [users] = outcome.result_sets.as_slice() else {
                panic!("one result set");
            };
            let types = users
                .columns
                .iter()
                .map(|column| column.type_name.as_deref())
                .collect::<Vec<_>>();
            assert_eq!(
                types,
                [
                    Some("int8"),
                    Some("text"),
                    Some("jsonb"),
                    Some("_text"),
                    Some("varchar")
                ]
            );
            assert_eq!(users.rows[0][2].as_deref(), Some("{\"theme\": \"dark\"}"));
            assert_eq!(users.rows[0][3].as_deref(), Some("{investor,beta}"));
            assert_eq!(users.rows[1][2], None);
            assert_eq!(users.rows_affected, Some(3));

            let started = Instant::now();
            let outcome = session
                .run("select g, md5(g::text) from generate_series(1, 5000000) g", 1000)
                .await
                .unwrap();
            let limited_in = started.elapsed();
            assert_eq!(outcome.result_sets[0].rows.len(), 1000);
            assert!(outcome.result_sets[0].truncated);
            eprintln!("limite de 1000 linhas em 5 M: {limited_in:?}");
            // The cancel that stopped the big query must not hit the next one.
            let outcome = session.run("select 1", 10).await.unwrap();
            assert_eq!(outcome.result_sets[0].rows, [[Some("1".to_owned())]]);

            // Inside a transaction the same cancel aborts it, so the next statement fails until
            // a ROLLBACK. Phase 3 has to page with a cursor there instead.
            session.run("begin", 10).await.unwrap();
            session
                .run("select g from generate_series(1, 5000000) g", 1000)
                .await
                .unwrap();
            let error = session.run("select 1", 10).await.unwrap_err();
            assert_eq!(server_error(&error).code, "25P02");
            session.run("rollback", 10).await.unwrap();

            // Through a cursor the limit leaves the transaction usable.
            session.run("begin", 10).await.unwrap();
            let outcome = session
                .run_in_cursor("select g from generate_series(1, 5000000) g", 1000)
                .await
                .unwrap();
            assert_eq!(outcome.result_sets[0].rows.len(), 1000);
            assert!(outcome.result_sets[0].truncated);
            assert_eq!(outcome.result_sets[0].columns[0].type_name.as_deref(), Some("int4"));
            let outcome = session.run("select 1", 10).await.unwrap();
            assert_eq!(outcome.result_sets[0].rows, [[Some("1".to_owned())]]);
            session.run("rollback", 10).await.unwrap();

            let outcome = session
                .run("select 1 as a; select 'x' as b, 2 as c", 10)
                .await
                .unwrap();
            assert_eq!(outcome.result_sets.len(), 2);
            assert_eq!(outcome.result_sets[1].columns[0].type_name, None);

            let error = session
                .run("select nme from database_client_spike.users", 10)
                .await
                .unwrap_err();
            let error = server_error(&error);
            assert_eq!(error.code, "42703");
            assert_eq!(error.position, Some(8));
            eprintln!("{error} · hint: {:?}", error.hint);

            let outcome = session
                .run(
                    "update database_client_spike.users set name = 'Bruna' where id = 2",
                    10,
                )
                .await
                .unwrap();
            assert_eq!(outcome.result_sets[0].rows_affected, Some(1));

            let started = Instant::now();
            let (slow, cancelled) = futures::join!(
                session.run("select pg_sleep(30)", 10),
                async {
                    sleep(Duration::from_millis(300)).await;
                    session.cancel().await
                }
            );
            let cancelled_in = started.elapsed();
            cancelled.unwrap();
            assert_eq!(server_error(&slow.unwrap_err()).code, "57014");
            assert!(cancelled_in < Duration::from_secs(2), "{cancelled_in:?}");
            eprintln!("pg_sleep(30) cancelado em {cancelled_in:?}");

            session
                .run("drop schema database_client_spike cascade", 10)
                .await
                .unwrap();

            if target.ssl_mode == SslMode::Require {
                let strict = ConnectTarget {
                    ssl_mode: SslMode::VerifyFull,
                    ..target.clone()
                };
                let error = Session::connect(&strict).await.err().unwrap();
                eprintln!("verify-full contra certificado autoassinado: {error:#}");
            }
        });
    }
}
