//! Elasticsearch in the dock: ES|QL tabs, following data streams, APM traces and the agent's
//! read-only tools, over a connection that goes through Kibana's console proxy or straight to
//! the cluster.

mod agent_toolkit;
mod client;
mod completion;
mod connect_view;
mod connection;
mod esql;
mod follow_view;
mod mask;
mod panel;
mod query_view;
mod results;
mod trace_view;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use client::{
    Auth, ClusterInfo, CurrentUser, DataStream, Elastic, ElasticError, Endpoint, EsqlColumn,
    EsqlResult, Field, MissingPrivileges, TimeRange,
};
pub use connection::SavedConnection;
pub use panel::ElasticPanel;

use gpui::{Subscription, WeakEntity, actions};
use std::any::TypeId;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::StatusBarButton};

actions!(
    elastic,
    [
        /// Opens the Elastic dock, or hands focus back if it already has it.
        ToggleFocus,
        /// Opens the form for a new Elasticsearch connection.
        NewConnection,
        /// Saves the connection in the focused form and connects to it.
        SaveConnection,
        /// Creates an .esql file in .asylum/elastic/queries and opens it against the dock's connection.
        NewQuery,
        /// Opens the .esql file in the active editor against the dock's connection.
        OpenInElastic,
        /// Runs the ES|QL query under the cursor, or the selection.
        RunQuery,
        /// Stops waiting for the running query.
        CancelQuery,
        /// Pauses or resumes following a data stream.
        TogglePause,
        /// Applies the filter typed above the followed lines.
        ApplyFollowFilter,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<ElasticPanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewQuery, window, cx| {
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<ElasticPanel>(cx) {
                panel.update(cx, |panel, cx| panel.new_query(None, false, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &OpenInElastic, window, cx| {
            let path = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .and_then(|project_path| {
                    workspace
                        .project()
                        .read(cx)
                        .absolute_path(&project_path, cx)
                });
            let Some(path) = path else {
                return;
            };
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<ElasticPanel>(cx) {
                panel.update(cx, |panel, cx| panel.open_query(path, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            open(workspace, window, cx);
            if let Some(panel) = workspace.panel::<ElasticPanel>(cx) {
                connect_view::open_in(workspace, panel, connect_view::Prefill::New, window, cx);
            }
        });
    })
    .detach();
}

/// Adds the panel the first time it is asked for, so projects without Elasticsearch don't get a
/// tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ElasticPanel>(cx).is_none() {
        let panel = cx.new(|cx| ElasticPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<ElasticPanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ElasticPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<ElasticPanel>(window, cx);
}

fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<ElasticPanel>())
    })
}

/// Kibana's Discover with the query in ES|QL mode, for "Abrir no Kibana". Only for connections
/// that go through Kibana, since a direct one doesn't know Kibana's address.
pub(crate) fn kibana_discover_url(
    connection: &SavedConnection,
    query: &str,
    from: &str,
) -> Option<String> {
    if connection.via != connection::Via::Kibana {
        return None;
    }
    let base = connection.url.trim_end_matches('/');
    let state = format!(
        "(query:(esql:'{}'))",
        rison_escape(&esql::commands(query).join(" | "))
    );
    let time = format!("(time:(from:{from},to:now))");
    Some(format!(
        "{base}/app/discover#/?_a={}&_g={}",
        urlencoding::encode(&state),
        urlencoding::encode(&time)
    ))
}

pub(crate) fn kibana_trace_url(connection: &SavedConnection, trace_id: &str) -> Option<String> {
    if connection.via != connection::Via::Kibana {
        return None;
    }
    Some(format!(
        "{}/app/apm/link-to/trace/{}",
        connection.url.trim_end_matches('/'),
        urlencoding::encode(trace_id)
    ))
}

/// Rison quotes strings with `'` and escapes it and `!` with `!`.
fn rison_escape(text: &str) -> String {
    text.replace('!', "!!").replace('\'', "!'")
}

/// The Elastic button in the status bar's toolkit group, lit while the dock is open.
pub struct ElasticToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ElasticToolkitButton {
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

impl Render for ElasticToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();

        StatusBarButton::new("toolkit-elastic", IconName::CloudPulse, is_open)
            .tab_index(0isize)
            .aria_label("Elastic")
            .tooltip(|_window, cx| Tooltip::for_action("Elastic", &ToggleFocus, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<ElasticPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for ElasticToolkitButton {
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

    #[test]
    fn discover_link_carries_the_query() {
        let connection = SavedConnection {
            id: "a".into(),
            name: "trix-logs".into(),
            environment: connection::Environment::Prod,
            via: connection::Via::Kibana,
            url: "https://kibana.example.com/".into(),
            auth: connection::AuthKind::Password,
            username: "vinicios".into(),
        };
        let url = kibana_discover_url(&connection, "FROM logs-*\n| WHERE a == 'x'", "now-30m")
            .expect("url");
        assert!(url.starts_with("https://kibana.example.com/app/discover#/?_a="));
        assert!(
            url.contains(
                &urlencoding::encode("esql:'FROM logs-* | WHERE a == !'x!''").into_owned()
            )
        );
        assert_eq!(
            kibana_trace_url(&connection, "abc").as_deref(),
            Some("https://kibana.example.com/app/apm/link-to/trace/abc")
        );
    }
}

#[cfg(test)]
mod live_tests {
    //! Runs against a real cluster:
    //!
    //! ```sh
    //! export ELASTIC_KIBANA_URL=https://kibana.example.com   # or ELASTIC_URL=http://host:9200
    //! export ELASTIC_USERNAME=vinicios
    //! read -s ELASTIC_PASSWORD && export ELASTIC_PASSWORD      # or ELASTIC_API_KEY
    //! cargo test -p elastic -- --ignored --nocapture
    //! ```
    //!
    //! `ELASTIC_INDEX` picks the pattern to query (default `logs-*`).

    use std::{sync::Arc, time::Instant};

    use http_client::{HttpClient, Url};
    use reqwest_client::ReqwestClient;

    use super::*;

    #[test]
    #[ignore = "needs a cluster and credentials"]
    fn talks_to_the_cluster() -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let http_client: Arc<dyn HttpClient> = {
            let _guard = runtime.enter();
            Arc::new(ReqwestClient::proxy_and_user_agent(
                None,
                "Asylum/elastic-live-test",
            )?)
        };
        runtime.block_on(run(http_client))
    }

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }

    async fn run(http_client: Arc<dyn HttpClient>) -> anyhow::Result<()> {
        let endpoint = match (env("ELASTIC_KIBANA_URL"), env("ELASTIC_URL")) {
            (Some(url), _) => Endpoint::Kibana(Url::parse(&url)?),
            (None, Some(url)) => Endpoint::Direct(Url::parse(&url)?),
            (None, None) => anyhow::bail!("defina ELASTIC_KIBANA_URL ou ELASTIC_URL"),
        };
        let auth = match (env("ELASTIC_API_KEY"), env("ELASTIC_USERNAME")) {
            (Some(key), _) => Auth::ApiKey(key),
            (None, Some(username)) => Auth::Basic {
                username,
                password: env("ELASTIC_PASSWORD").unwrap_or_default(),
            },
            (None, None) => Auth::None,
        };
        let index = env("ELASTIC_INDEX").unwrap_or_else(|| "logs-*".to_string());
        let elastic = Elastic::new(http_client, endpoint, auth);

        match elastic.kibana_version().await {
            Ok(Some(version)) => println!("Kibana {version}"),
            Ok(None) => {}
            Err(error) => println!("api/status do Kibana falhou: {error}"),
        }

        let info = elastic.info().await?;
        println!("cluster {} · {}", info.cluster_name, info.version.number);

        match elastic.current_user().await {
            Ok(user) => println!(
                "usuário {} · realm {} ({}) · roles {}",
                user.username,
                user.authentication_realm.name,
                user.authentication_realm.kind,
                user.roles.join(", ")
            ),
            Err(error) => println!("_authenticate falhou: {error}"),
        }

        let patterns = [index.as_str(), "traces-apm*", "logs-apm.error-*"];
        match elastic
            .missing_privileges(&["monitor"], &patterns, &["read", "view_index_metadata"])
            .await
        {
            Ok(missing) if missing.is_empty() => println!("privilégios: tudo concedido"),
            Ok(missing) => println!("privilégios faltando: {missing:?}"),
            Err(error) => println!("_has_privileges falhou: {error}"),
        }

        match elastic.data_streams().await {
            Ok(streams) => {
                println!("{} data streams", streams.len());
                for stream in streams.iter().take(15) {
                    println!(
                        "  {} · {} índices · {}",
                        stream.name, stream.backing_indices, stream.status
                    );
                }
            }
            Err(error) => println!("_data_stream falhou: {error}"),
        }

        let fields = elastic.fields(&index).await?;
        println!(
            "{} campos em {index}; ECS comuns presentes: {:?}",
            fields.len(),
            [
                "@timestamp",
                "message",
                "log.level",
                "service.name",
                "trace.id"
            ]
            .iter()
            .filter(|name| fields.iter().any(|field| field.name == **name))
            .collect::<Vec<_>>()
        );

        let range = TimeRange {
            from: "now-30m".into(),
            to: "now".into(),
        };
        let started = Instant::now();
        let latest = elastic
            .esql(
                &format!("FROM {index} | SORT @timestamp DESC | LIMIT 5"),
                Some(&range),
            )
            .await?;
        println!(
            "ES|QL: {} colunas, {} linhas em {:?} (took {:?} ms)",
            latest.columns.len(),
            latest.values.len(),
            started.elapsed(),
            latest.took
        );

        let per_level = elastic
            .esql(
                &format!(
                    "FROM {index} | STATS docs = COUNT(*) BY log.level | SORT docs DESC | LIMIT 10"
                ),
                Some(&range),
            )
            .await;
        match per_level {
            Ok(result) => println!("últimos 30 min por log.level: {:?}", result.values),
            Err(error) => println!("STATS por log.level falhou: {error}"),
        }

        match elastic
            .esql("FROM logs-* | WHERE level == \"error\"", None)
            .await
        {
            Err(ElasticError::BadRequest { kind, message }) => {
                println!("erro de consulta chega como {kind}: {message}")
            }
            other => println!("a consulta com campo errado não falhou como esperado: {other:?}"),
        }
        Ok(())
    }
}
