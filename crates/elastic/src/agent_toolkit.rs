//! The agent's Elastic tools. They read through the connection open in the project's Elastic
//! dock and never write to the cluster; what they return has personal data masked first.
//! `elastic_esql` reads at most 24 h at a time without asking, `elastic_wide_esql` goes further
//! and asks every time.

use crate::{
    client::TimeRange,
    esql,
    mask::mask,
    panel::{Session, panel_for},
    query_view,
    results::{Rows, display},
    trace_view,
};
use anyhow::{Context as _, anyhow};
use chrono::Utc;
use collections::HashMap;
use gpui::{App, AppContext as _, Entity, Global, Task, Window};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{path::Path, sync::Arc};
use task_agents::{
    ElasticColumn, ElasticConnectionInfo, ElasticQueryResult, ElasticTraceResult, ElasticTraceSpan,
    LogMention, LogSource, ToolAccess, Toolkit, ToolkitOutput, ToolkitTool,
};

const DEFAULT_WINDOW_MINUTES: u32 = 60;
const MAX_WINDOW_MINUTES: u32 = 24 * 60;
const MAX_WIDE_WINDOW_MINUTES: u32 = 30 * 24 * 60;
const DEFAULT_ROW_LIMIT: usize = 100;
const MAX_ROW_LIMIT: usize = 500;
const MAX_CELL_CHARS: usize = 200;
/// What the thread's card keeps: it shows a handful of rows, and "Copiar CSV" copies these.
const MAX_CARD_ROWS: usize = 200;
const MAX_CARD_CELL_CHARS: usize = 1000;
const MAX_CARD_SPANS: usize = 40;
const MAX_OUTPUT_CHARS: usize = 40_000;

#[derive(Deserialize, JsonSchema)]
struct EsqlInput {
    /// One ES|QL query starting with FROM (e.g. `FROM logs-* | WHERE log.level == "error" |
    /// STATS c = COUNT(*) BY message | SORT c DESC | LIMIT 20`). Don't filter on @timestamp for
    /// the window: `window_minutes` does it.
    query: String,
    /// How far back from now to read, in minutes. Defaults to 60.
    #[serde(default)]
    window_minutes: Option<u32>,
    /// Maximum rows to return. Defaults to 100, at most 500.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct FieldsInput {
    /// Index pattern or data stream, e.g. `logs-*` or `traces-apm*`.
    index: String,
    /// Only fields whose name contains this text.
    #[serde(default)]
    contains: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct TraceInput {
    /// The `trace.id` of a log line or APM document.
    trace_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct RecentErrorsInput {
    /// Only this `service.name`.
    #[serde(default)]
    service: Option<String>,
    /// How far back from now, in minutes. Defaults to 60, at most 1440.
    #[serde(default)]
    window_minutes: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteQueryInput {
    /// The ES|QL query to put in the editor. Explanations, if the user asked for them, go in
    /// `//` comments.
    query: String,
    /// The editor's path, from the mentioned ES|QL editor. Defaults to the most recent one.
    #[serde(default)]
    path: Option<String>,
    /// Replace the query under the cursor instead of adding a new one after the last.
    #[serde(default)]
    replace: bool,
}

fn session_for(project: &Entity<Project>, cx: &App) -> anyhow::Result<Arc<Session>> {
    let panel = panel_for(project, cx).context(
        "O dock Elastic não está aberto neste projeto. Peça para abrir o Elastic na barra de \
         status e conectar.",
    )?;
    panel.read(cx).session().context(
        "O dock Elastic ainda está conectando, ou a conexão falhou. Peça para conferir o dock.",
    )
}

fn cell(value: &serde_json::Value) -> String {
    let text = mask(&display(value))
        .replace(['\n', '\r'], " ")
        .replace('|', "\\|");
    if text.chars().count() > MAX_CELL_CHARS {
        format!("{}…", text.chars().take(MAX_CELL_CHARS).collect::<String>())
    } else {
        text
    }
}

/// Rows as a markdown table, masked, for the model.
pub(crate) fn rows_as_text(rows: &Rows, max_rows: usize) -> String {
    let names = rows
        .columns
        .iter()
        .map(|column| column.name.replace('|', "\\|"))
        .collect::<Vec<_>>();
    let mut text = format!(
        "| {} |\n|{}\n",
        names.join(" | "),
        " --- |".repeat(names.len().max(1))
    );
    for row in rows.values.iter().take(max_rows) {
        let line = format!(
            "| {} |\n",
            row.iter().map(cell).collect::<Vec<_>>().join(" | ")
        );
        if text.len() + line.len() > MAX_OUTPUT_CHARS {
            text.push_str("… (saída cortada)\n");
            break;
        }
        text.push_str(&line);
    }
    text
}

fn with_limit(query: &str, limit: usize) -> String {
    let has_limit = esql::commands(query)
        .iter()
        .any(|command| command.to_ascii_uppercase().starts_with("LIMIT "));
    if has_limit {
        query.trim().to_string()
    } else {
        format!("{}\n| LIMIT {limit}", query.trim())
    }
}

fn run_esql(
    project: Entity<Project>,
    input: EsqlInput,
    max_window: u32,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let window = input
        .window_minutes
        .unwrap_or(DEFAULT_WINDOW_MINUTES)
        .max(1);
    if window > max_window {
        return Task::ready(Err(anyhow!(
            "Janela de {window} min passa do limite de {max_window} min desta tool.{}",
            if max_window == MAX_WINDOW_MINUTES {
                " Para janelas maiores use elastic_wide_esql, que pede confirmação ao usuário."
            } else {
                ""
            }
        )));
    }
    let query = input.query.trim().to_string();
    if !esql::commands(&query)
        .first()
        .is_some_and(|command| command.to_ascii_uppercase().starts_with("FROM "))
    {
        return Task::ready(Err(anyhow!("A consulta precisa começar com FROM.")));
    }
    let limit = input
        .limit
        .unwrap_or(DEFAULT_ROW_LIMIT)
        .clamp(1, MAX_ROW_LIMIT);
    let session = match session_for(&project, cx) {
        Ok(session) => session,
        Err(error) => return Task::ready(Err(error)),
    };
    let query = with_limit(&query, limit);
    cx.background_spawn(async move { esql_output(&session, &[query], window, limit).await })
}

/// Runs the first of `queries` that Elasticsearch accepts. Later ones are simpler fallbacks
/// for clusters whose mappings lack fields the first one names.
async fn esql_output(
    session: &Session,
    queries: &[String],
    window: u32,
    limit: usize,
) -> anyhow::Result<ToolkitOutput> {
    let to = Utc::now();
    let from = to - chrono::Duration::minutes(i64::from(window));
    let range = TimeRange {
        from: esql::iso(from),
        to: esql::iso(to),
    };
    let mut last_error = None;
    for query in queries {
        match session.elastic.esql(query, Some(&range)).await {
            Ok(result) => {
                let took = result.took;
                let rows = Rows::from(result);
                let mut text = format!(
                    "Elastic {} · últimos {window} min · {} linha(s){}\n\n",
                    session.describe(),
                    rows.values.len(),
                    took.map(|took| format!(" · took {took} ms"))
                        .unwrap_or_default()
                );
                text.push_str(&rows_as_text(&rows, limit));
                text.push_str(
                    "\nDados pessoais foram mascarados («email», «cpf»…). O usuário vê estas \
                     linhas num card logo abaixo desta chamada: responda o que foi perguntado \
                     com o fato (quantos, qual, desde quando), sem repetir a tabela.",
                );
                let card = query_card(session, query, window, limit, took, &rows);
                return Ok(ToolkitOutput::text(text).with_raw_output(card));
            }
            Err(error @ crate::client::ElasticError::BadRequest { .. }) => {
                last_error = Some(error);
            }
            Err(error) => return Err(anyhow!("{error}")),
        }
    }
    Err(match last_error {
        Some(error) => anyhow!("{error}"),
        None => anyhow!("nenhuma consulta para rodar"),
    })
}

fn list_data_streams(
    project: Entity<Project>,
    _input: NoInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let session = match session_for(&project, cx) {
        Ok(session) => session,
        Err(error) => return Task::ready(Err(error)),
    };
    let project_streams = panel_for(&project, cx)
        .map(|panel| panel.read(cx).project_streams().to_vec())
        .unwrap_or_default();
    let apps = panel_for(&project, cx)
        .and_then(|panel| panel.read(cx).apps().map(<[_]>::to_vec))
        .unwrap_or_default();
    let mut text = format!("Data streams em {}:\n", session.describe());
    if !apps.is_empty() {
        text.push_str(
            "Apps (service.name em logs-*, últimas 24 h; filtre com WHERE service.name == \"…\"):\n",
        );
        for app in &apps {
            text.push_str(&format!(
                "- {}: {} logs{}\n",
                app.name,
                app.documents,
                app.errors
                    .map(|errors| format!(", {errors} erros"))
                    .unwrap_or_default()
            ));
        }
        text.push('\n');
    }
    if !project_streams.is_empty() {
        text.push_str(&format!("Do projeto: {}\n", project_streams.join(", ")));
    }
    for stream in &session.streams {
        text.push_str(&format!(
            "- {}{}\n",
            stream.name,
            session
                .sizes
                .get(&stream.name)
                .map(|size| format!(" ({} MB)", size / 1_048_576))
                .unwrap_or_default()
        ));
    }
    if !session.missing.is_empty() {
        text.push_str(&format!(
            "\nPrivilégios que faltam: {:?}. O que depende deles não aparece.\n",
            session.missing
        ));
    }
    Task::ready(Ok(ToolkitOutput::text(text)))
}

fn list_fields(
    project: Entity<Project>,
    input: FieldsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let session = match session_for(&project, cx) {
        Ok(session) => session,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.background_spawn(async move {
        let fields = session
            .elastic
            .fields(&input.index)
            .await
            .map_err(|error| anyhow!("{error}"))?;
        let contains = input.contains.map(|text| text.to_lowercase());
        let fields: Vec<_> = fields
            .into_iter()
            .filter(|field| {
                contains
                    .as_ref()
                    .is_none_or(|contains| field.name.to_lowercase().contains(contains))
            })
            .collect();
        let mut text = format!("{} campos em {}:\n", fields.len(), input.index);
        for field in fields.iter().take(600) {
            text.push_str(&format!("{} {}\n", field.name, field.kind));
        }
        if fields.len() > 600 {
            text.push_str("… (use `contains` para filtrar)\n");
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn get_trace(
    project: Entity<Project>,
    input: TraceInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let session = match session_for(&project, cx) {
        Ok(session) => session,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.background_spawn(async move {
        let trace = trace_view::load_trace(&session, input.trace_id.trim()).await?;
        if trace.spans.is_empty() {
            return Err(anyhow!(
                "Nenhum span com trace.id {} em traces-apm*.",
                input.trace_id
            ));
        }
        let trace_id = input.trace_id.trim();
        let start = trace.start_us();
        let card = ElasticTraceResult {
            connection: connection_info(&session),
            trace_id: trace_id.to_string(),
            duration_us: trace.duration_us(),
            spans: trace
                .spans
                .iter()
                .take(MAX_CARD_SPANS)
                .map(|span| ElasticTraceSpan {
                    name: span.name.clone(),
                    service: span.service.clone(),
                    event: span.event.clone(),
                    kind: span.kind.clone(),
                    depth: span.depth,
                    offset_us: span.start_us.saturating_sub(start),
                    duration_us: span.duration_us,
                    failed: span.failed,
                    status: span.status,
                })
                .collect(),
            total_spans: trace.spans.len(),
            errors: trace
                .errors
                .iter()
                .take(5)
                .map(|error| error.message.clone())
                .collect(),
            log_count: trace.logs.len(),
        };
        Ok(ToolkitOutput::text(format!(
            "{}\nO usuário vê a cascata num card logo abaixo desta chamada; explique o que \
             importa sem repetir a lista de spans.",
            trace.describe(trace_id)
        ))
        .with_raw_output(card))
    })
}

fn connection_info(session: &Session) -> ElasticConnectionInfo {
    ElasticConnectionInfo {
        name: session.connection.name.clone(),
        environment: session.connection.environment.label().to_string(),
    }
}

fn query_card(
    session: &Session,
    query: &str,
    window: u32,
    limit: usize,
    took: Option<u64>,
    rows: &Rows,
) -> ElasticQueryResult {
    ElasticQueryResult {
        connection: connection_info(session),
        query: query.to_string(),
        window_minutes: window,
        source: esql::sources(query).into_iter().next(),
        columns: rows
            .columns
            .iter()
            .map(|column| ElasticColumn {
                name: column.name.clone(),
                kind: column.kind.clone(),
            })
            .collect(),
        rows: rows
            .values
            .iter()
            .take(MAX_CARD_ROWS)
            .map(|row| {
                row.iter()
                    .map(|value| {
                        (!value.is_null())
                            .then(|| display(value).chars().take(MAX_CARD_CELL_CHARS).collect())
                    })
                    .collect()
            })
            .collect(),
        total_rows: rows.values.len(),
        limit,
        took_ms: took,
    }
}

fn recent_errors(
    project: Entity<Project>,
    input: RecentErrorsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let session = match session_for(&project, cx) {
        Ok(session) => session,
        Err(error) => return Task::ready(Err(error)),
    };
    let service = input
        .service
        .as_deref()
        .filter(|service| !service.is_empty())
        .map(|service| {
            format!(
                "\n| WHERE service.name == {}",
                esql::string_literal(service)
            )
        })
        .unwrap_or_default();
    let levels = "log.level IN (\"error\", \"ERROR\", \"fatal\", \"FATAL\")";
    // With APM error documents in logs-*, their message lives in other fields; without them,
    // naming those fields is an error, so the plain log query follows.
    let queries = [
        format!(
            "FROM logs-*\n| WHERE {levels} OR processor.event == \"error\"{service}\
             \n| EVAL text = COALESCE(message, error.exception.message, error.log.message)\
             \n| STATS vezes = COUNT(*), ultima = MAX(@timestamp) BY text, service.name\
             \n| SORT vezes DESC\n| LIMIT 25"
        ),
        format!(
            "FROM logs-*\n| WHERE {levels}{service}\
             \n| STATS vezes = COUNT(*), ultima = MAX(@timestamp) BY message\
             \n| SORT vezes DESC\n| LIMIT 25"
        ),
    ];
    let window = input
        .window_minutes
        .unwrap_or(DEFAULT_WINDOW_MINUTES)
        .clamp(1, MAX_WINDOW_MINUTES);
    cx.background_spawn(async move { esql_output(&session, &queries, window, 25).await })
}

fn write_query(
    project: Entity<Project>,
    input: WriteQueryInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    Task::ready(write_query_now(&project, input, cx))
}

fn write_query_now(
    project: &Entity<Project>,
    input: WriteQueryInput,
    cx: &mut App,
) -> anyhow::Result<ToolkitOutput> {
    if input.query.trim().is_empty() {
        return Err(anyhow!("`query` está vazia."));
    }
    let views = query_view::query_views(project, cx);
    let path = input
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let view = match path {
        Some(path) => views
            .iter()
            .find(|view| view.read(cx).path() == Path::new(path))
            .with_context(|| format!("Nenhuma aba ES|QL aberta em {path}."))?,
        None => views
            .first()
            .context("Nenhuma aba ES|QL aberta. Peça para abrir uma consulta no dock Elastic.")?,
    }
    .clone();
    let window = view.read(cx).window();
    let line = cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.write_from_agent(&input.query, input.replace, window, cx)
        })
    })?;
    let file_name = view
        .read(cx)
        .path()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok(ToolkitOutput::text(format!(
        "Consulta escrita em {file_name}, linha {line}. Encerre o turno sem escrever mais nada."
    )))
}

/// Documents and text the dock handed to the agent, kept until the app quits so a mention can
/// be read again when the thread is sent.
#[derive(Default)]
struct Remembered {
    next: usize,
    entries: HashMap<String, String>,
}

impl Global for Remembered {}

fn remember(prefix: &str, text: String, cx: &mut App) -> String {
    let remembered = cx.default_global::<Remembered>();
    remembered.next += 1;
    let id = format!("{prefix}:{}", remembered.next);
    remembered.entries.insert(id.clone(), text);
    id
}

pub(crate) fn remember_document(document: serde_json::Value, cx: &mut App) -> String {
    let text = serde_json::to_string_pretty(&document).unwrap_or_default();
    remember(
        "doc",
        format!("Documento de log:\n```json\n{text}\n```"),
        cx,
    )
}

pub(crate) fn remember_text(text: String, cx: &mut App) -> String {
    remember("text", text, cx)
}

pub(crate) fn query_mention_id(path: &Path) -> String {
    format!("esql:{}", path.display())
}

fn list_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    query_view::query_views(project, cx)
        .into_iter()
        .map(|view| {
            let view = view.read(cx);
            LogMention {
                id: query_mention_id(view.path()).into(),
                title: view
                    .path()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
                detail: view.session().connection.name.clone().into(),
            }
        })
        .collect()
}

fn describe_mention(
    project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    if let Some(path) = id.strip_prefix("esql:") {
        let view = query_view::query_views(project, cx)
            .into_iter()
            .find(|view| view.read(cx).path() == Path::new(path));
        return Task::ready(
            view.map(|view| view.read(cx).describe_for_agent(cx))
                .with_context(|| format!("A aba ES|QL {path} foi fechada.")),
        );
    }
    if let Some(trace_id) = id.strip_prefix("trace:") {
        let session = match session_for(project, cx) {
            Ok(session) => session,
            Err(error) => return Task::ready(Err(error)),
        };
        let trace_id = trace_id.to_string();
        return cx.background_spawn(async move {
            let trace = trace_view::load_trace(&session, &trace_id).await?;
            Ok(format!(
                "Trace do APM em {}.\n{}\nUse elastic_esql para ler mais logs deste trace \
                 (WHERE trace.id == \"{trace_id}\").",
                session.describe(),
                trace.describe(&trace_id)
            ))
        });
    }
    let text = cx
        .try_global::<Remembered>()
        .and_then(|remembered| remembered.entries.get(id).cloned());
    Task::ready(
        text.map(|text| {
            format!(
                "{}\n\nDados pessoais mascarados. Para ler mais, use elastic_esql.",
                mask(&text)
            )
        })
        .context("Esse log não está mais na memória do dock Elastic."),
    )
}

fn open_query(
    project: &Entity<Project>,
    query: String,
    window_minutes: u32,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let panel = panel_for(project, cx).context("O dock Elastic não está aberto.")?;
    panel.update(cx, |panel, cx| {
        panel.open_agent_query(query, window_minutes, window, cx)
    });
    Ok(())
}

fn follow(
    project: &Entity<Project>,
    source: String,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let panel = panel_for(project, cx).context("O dock Elastic não está aberto.")?;
    anyhow::ensure!(
        panel.read(cx).session().is_some(),
        "O dock Elastic não está conectado."
    );
    panel.update(cx, |panel, cx| panel.follow(source, None, window, cx));
    Ok(())
}

fn open_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    if let Some(path) = id.strip_prefix("esql:") {
        let view = query_view::query_views(project, cx)
            .into_iter()
            .find(|view| view.read(cx).path() == Path::new(path))
            .with_context(|| format!("A aba ES|QL {path} foi fechada."))?;
        return query_view::activate(&view, window, cx);
    }
    if let Some(trace_id) = id.strip_prefix("trace:") {
        let panel = panel_for(project, cx).context("O dock Elastic não está aberto.")?;
        let (workspace, session) = {
            let panel = panel.read(cx);
            (
                panel.workspace(),
                panel
                    .session()
                    .context("O dock Elastic não está conectado.")?,
            )
        };
        trace_view::open(
            workspace,
            project.clone(),
            panel.downgrade(),
            session,
            trace_id.to_string(),
            window,
            cx,
        );
    }
    Ok(())
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "elastic".into(),
            name: "Elastic".into(),
            description: "Logs, traces e erros do Elasticsearch, só leitura".into(),
            icon: "cloud_pulse".into(),
            tools: vec![
                ToolkitTool::new(
                    "elastic_esql",
                    "Consultou o Elastic",
                    "Runs one ES|QL query (starting with FROM) against the Elasticsearch \
                     connected in the Elastic dock and returns the rows as a table, with \
                     personal data masked. The time window is `window_minutes` back from now \
                     (default 60, at most 1440), applied to @timestamp for you. Use it to \
                     answer questions about logs and APM data yourself: aggregate with STATS \
                     when counting, keep few columns, and answer with the fact. Logs follow ECS: \
                     message, log.level, service.name, trace.id, error.stack_trace; one app's logs \
                     are `FROM logs-* | WHERE service.name == \"<app>\"`.",
                    ToolAccess::Read,
                    |project, input: EsqlInput, cx| {
                        run_esql(project, input, MAX_WINDOW_MINUTES, cx)
                    },
                ),
                ToolkitTool::new(
                    "elastic_wide_esql",
                    "Consultou o Elastic (janela grande)",
                    "Same as elastic_esql for windows longer than 24 h (up to 30 days). It asks \
                     the user first, since it reads much more of the cluster. Prefer \
                     elastic_esql and narrow the question when you can.",
                    ToolAccess::Act,
                    |project, input: EsqlInput, cx| {
                        run_esql(project, input, MAX_WIDE_WINDOW_MINUTES, cx)
                    },
                ),
                ToolkitTool::new(
                    "elastic_data_streams",
                    "Listou os data streams",
                    "Lists the apps writing logs (their service.name, with volume and errors in \
                     the last 24 h) and the data streams of the connected cluster (logs-*, \
                     traces-*, metrics-*), the ones marked as this project's first. Call it \
                     first when the user names an app.",
                    ToolAccess::Read,
                    list_data_streams,
                ),
                ToolkitTool::new(
                    "elastic_fields",
                    "Leu os campos",
                    "Lists the mapped fields of an index pattern with their types, to write \
                     ES|QL with the right names.",
                    ToolAccess::Read,
                    list_fields,
                ),
                ToolkitTool::new(
                    "elastic_trace",
                    "Leu o trace",
                    "Reads one APM trace by trace.id: its transactions and spans with timings, \
                     outcomes and HTTP status, the APM errors and the log lines of that trace.",
                    ToolAccess::Read,
                    get_trace,
                ),
                ToolkitTool::new(
                    "elastic_recent_errors",
                    "Leu os erros recentes",
                    "The most frequent error messages of the last `window_minutes` (default \
                     60), grouped by message and service, with count and last time seen.",
                    ToolAccess::Read,
                    recent_errors,
                ),
                // Read access, so it doesn't ask: it only edits the open editor (one undo brings
                // it back), and nothing runs until the user does.
                ToolkitTool::new(
                    "elastic_write_query",
                    "Escreveu a consulta",
                    "Writes an ES|QL query into an .esql editor tab of the Elastic dock, which \
                     the user mentioned with @. Nothing runs: the user reviews and runs it. \
                     After writing, end the turn without any text.",
                    ToolAccess::Read,
                    write_query,
                ),
            ],
        },
        cx,
    );
    task_agents::register_log_source(
        LogSource {
            list: Arc::new(list_mentions),
            describe: Arc::new(describe_mention),
            open: Arc::new(open_mention),
            open_query: Arc::new(open_query),
            follow: Arc::new(follow),
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_get_a_limit_when_they_have_none() {
        assert_eq!(with_limit("FROM logs-*", 100), "FROM logs-*\n| LIMIT 100");
        assert_eq!(
            with_limit("FROM logs-* | LIMIT 5", 100),
            "FROM logs-* | LIMIT 5"
        );
    }
}
