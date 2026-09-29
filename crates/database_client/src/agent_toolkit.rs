//! The database agent tools: `database_query` runs one SELECT on a read-only connection opened
//! next to the one in the project's Banco panel, so the agent can look at data without being
//! able to change it; `database_write_query` writes SQL into a query tab the user mentioned.

use crate::{
    agent_schema::PanelRegistry,
    connection::SavedConnection,
    panel::{ActiveState, DatabasePanel},
    query_view::{self, AgentWriteMode},
    statements,
};
use anyhow::{Context as _, anyhow};
use gpui::{App, AppContext as _, Entity, Window};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{path::Path, sync::Arc};
use task_agents::{
    DatabaseColumn, DatabaseConnectionInfo, DatabaseQueryResult, DatabaseWriteResult,
    SqlEditorSource, SqlEditorTab, ToolAccess, Toolkit, ToolkitOutput, ToolkitTool,
};

const DEFAULT_ROW_LIMIT: usize = 100;
const MAX_ROW_LIMIT: usize = 1000;
const MAX_CELL_CHARS: usize = 200;
const MAX_OUTPUT_CHARS: usize = 40_000;
/// What the thread's card keeps: it shows a handful of rows, and "Copiar CSV" copies these.
const MAX_CARD_ROWS: usize = 200;
const MAX_CARD_CELL_CHARS: usize = 1000;

#[derive(Deserialize, JsonSchema)]
struct QueryInput {
    /// One read-only statement: SELECT, WITH … SELECT, VALUES or TABLE. Use `database_schema`
    /// first to get table and column names right.
    sql: String,
    /// Maximum rows to return. Defaults to 100, at most 1000.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteQueryInput {
    /// The SQL to put in the editor. Only SQL: explanations, if the user asked for them, go in
    /// `--` comments.
    sql: String,
    /// The editor's path, from the mentioned SQL editor. Defaults to the most recently opened one.
    #[serde(default)]
    path: Option<String>,
    /// `append` (default) adds a new statement at the end, `replace_statement` rewrites the one
    /// under the cursor, `replace_all` rewrites the file.
    #[serde(default)]
    mode: AgentWriteMode,
}

fn write_query(
    project: Entity<Project>,
    input: WriteQueryInput,
    cx: &mut App,
) -> gpui::Task<anyhow::Result<ToolkitOutput>> {
    gpui::Task::ready(write_query_now(&project, input, cx))
}

fn write_query_now(
    project: &Entity<Project>,
    input: WriteQueryInput,
    cx: &mut App,
) -> anyhow::Result<ToolkitOutput> {
    if input.sql.trim().is_empty() {
        return Err(anyhow!("`sql` está vazio."));
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
            .with_context(|| {
                format!("Nenhuma aba SQL aberta em {path}. Peça para abrir a query.")
            })?,
        None => views
            .first()
            .context("Nenhuma aba SQL aberta. Peça para abrir uma query no painel Banco.")?,
    }
    .clone();
    let window = view.read(cx).window();
    let line = cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.write_from_agent(&input.sql, input.mode, window, cx)
        })
    })?;
    let file_name = view
        .read(cx)
        .path()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let view = view.read(cx);
    Ok(ToolkitOutput::text(format!(
        "Query escrita em {file_name}, linha {line}. Encerre o turno sem escrever mais nada."
    ))
    .with_raw_output(DatabaseWriteResult {
        connection: connection_info(view.connection()),
        abs_path: view.path().to_path_buf(),
        line,
    }))
}

fn connection_info(connection: &SavedConnection) -> DatabaseConnectionInfo {
    DatabaseConnectionInfo {
        name: connection.name.clone(),
        database: connection.database.clone(),
        environment: connection.environment.label().to_owned(),
    }
}

/// The relation a plain `SELECT … FROM x` reads, for the card's title. `None` for joins,
/// subqueries and lists of tables, where one name would be misleading.
fn single_source(sql: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut depth = 0usize;
    let mut quote = None;
    for character in sql.chars() {
        if let Some(open) = quote {
            if character == open {
                quote = None;
            }
            if open == '"' {
                word.push(character);
            }
            continue;
        }
        match character {
            '\'' => quote = Some('\''),
            '"' => {
                quote = Some('"');
                word.push(character);
            }
            '(' | ')' | ',' | ';' => {
                if depth == 0 && !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                match character {
                    '(' => {
                        depth += 1;
                        if depth == 1 {
                            words.push("(".to_owned());
                        }
                    }
                    ')' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => words.push(",".to_owned()),
                    _ => {}
                }
            }
            character if character.is_whitespace() => {
                if depth == 0 && !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
            }
            character => {
                if depth == 0 {
                    word.push(character);
                }
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if !words.first()?.eq_ignore_ascii_case("select") {
        return None;
    }
    let from = words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("from"))?;
    let source = words.get(from + 1)?;
    if source == "(" {
        return None;
    }
    let ends_the_from = [
        "where",
        "group",
        "order",
        "limit",
        "offset",
        "having",
        "window",
        "fetch",
        "for",
        "union",
        "except",
        "intersect",
    ];
    let rest = &words[from + 2..];
    let from_list = rest.iter().take_while(|word| {
        !ends_the_from
            .iter()
            .any(|end| word.eq_ignore_ascii_case(end))
    });
    for word in from_list {
        if word == "," || word.eq_ignore_ascii_case("join") {
            return None;
        }
    }
    Some(source.trim_matches('"').to_owned())
}

fn list_sql_editors(project: &Entity<Project>, cx: &App) -> Vec<SqlEditorTab> {
    query_view::query_views(project, cx)
        .into_iter()
        .map(|view| {
            let view = view.read(cx);
            SqlEditorTab {
                abs_path: view.path().to_path_buf(),
                connection: view.connection().name.clone().into(),
            }
        })
        .collect()
}

fn describe_sql_editor(
    project: &Entity<Project>,
    abs_path: &Path,
    cx: &App,
) -> anyhow::Result<String> {
    let view = query_view::query_views(project, cx)
        .into_iter()
        .find(|view| view.read(cx).path() == abs_path)
        .with_context(|| format!("A aba SQL {} foi fechada.", abs_path.display()))?;
    Ok(view.read(cx).describe_for_agent(cx))
}

fn panel_for(project: &Entity<Project>, cx: &App) -> anyhow::Result<Entity<DatabasePanel>> {
    cx.try_global::<PanelRegistry>()
        .and_then(|registry| registry.panel(project))
        .context(
            "O painel Banco não está aberto neste projeto. Peça para abrir e conectar um banco.",
        )
}

fn cell(value: &Option<String>) -> String {
    match value {
        None => "NULL".to_string(),
        Some(text) => {
            let text = text.replace(['\n', '\r'], " ").replace('|', "\\|");
            if text.chars().count() > MAX_CELL_CHARS {
                format!("{}…", text.chars().take(MAX_CELL_CHARS).collect::<String>())
            } else {
                text
            }
        }
    }
}

fn run_query(
    project: Entity<Project>,
    input: QueryInput,
    cx: &mut App,
) -> gpui::Task<anyhow::Result<ToolkitOutput>> {
    let statements = statements::split(&input.sql);
    let [statement] = statements.as_slice() else {
        return gpui::Task::ready(Err(anyhow!(
            "Mande exatamente um statement (recebi {}).",
            statements.len()
        )));
    };
    if !statement
        .kind
        .is_cursor_safe(statement.first_keyword.as_deref())
    {
        return gpui::Task::ready(Err(anyhow!(
            "database_query só roda leituras (SELECT, WITH, VALUES, TABLE). Escritas passam pelo editor SQL do Banco."
        )));
    }
    let sql = input.sql[statement.range.clone()].to_string();
    let limit = input
        .limit
        .unwrap_or(DEFAULT_ROW_LIMIT)
        .clamp(1, MAX_ROW_LIMIT);
    let panel = match panel_for(&project, cx) {
        Ok(panel) => panel,
        Err(error) => return gpui::Task::ready(Err(error)),
    };
    let panel = panel.read(cx);
    let Some(active) = panel.active() else {
        return gpui::Task::ready(Err(anyhow!(
            "Nenhum banco conectado no painel Banco. Peça para conectar um."
        )));
    };
    let ActiveState::Connected { session, .. } = &active.state else {
        return gpui::Task::ready(Err(anyhow!(
            "O painel Banco ainda está conectando, ou a conexão falhou."
        )));
    };
    let session = session.clone();
    let header = format!(
        "Banco `{}` ({}, {})",
        active.connection.database,
        active.connection.environment.label(),
        active.connection.address()
    );
    let connection = connection_info(&active.connection);
    cx.background_spawn(async move {
        let read_only = session.connect_read_only().await?;
        let outcome = read_only.run_in_cursor(&sql, limit).await?;
        let result = outcome
            .result_sets
            .into_iter()
            .next()
            .context("a consulta não devolveu resultado")?;
        let card = DatabaseQueryResult {
            connection,
            source: single_source(&sql),
            columns: result
                .columns
                .iter()
                .map(|column| DatabaseColumn {
                    name: column.name.clone(),
                    type_name: column.type_name.clone(),
                })
                .collect(),
            rows: result
                .rows
                .iter()
                .take(MAX_CARD_ROWS)
                .map(|row| {
                    row.iter()
                        .map(|value| {
                            value
                                .as_ref()
                                .map(|value| value.chars().take(MAX_CARD_CELL_CHARS).collect())
                        })
                        .collect()
                })
                .collect(),
            total_rows: result.rows.len(),
            truncated: result.truncated,
            limit,
            elapsed_ms: outcome.elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            sql,
        };
        let mut text = format!(
            "{header} · {} linha(s){} em {} ms\n\n",
            result.rows.len(),
            if result.truncated {
                format!(" (limite de {limit}; há mais)")
            } else {
                String::new()
            },
            outcome.elapsed.as_millis()
        );
        let names = result
            .columns
            .iter()
            .map(|column| column.name.replace('|', "\\|"))
            .collect::<Vec<_>>();
        text.push_str(&format!("| {} |\n", names.join(" | ")));
        text.push_str(&format!("|{}\n", " --- |".repeat(names.len().max(1))));
        for row in &result.rows {
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
        text.push_str(
            "\nO usuário vê estas linhas num card logo abaixo desta chamada. Responda o que foi \
             perguntado numa frase, com o fato (sim/não, o número, o que chama atenção), e não \
             repita as linhas em tabela.",
        );
        Ok(ToolkitOutput::text(text).with_raw_output(card))
    })
}

fn open_query(
    project: &Entity<Project>,
    sql: String,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    panel_for(project, cx)?.update(cx, |panel, cx| panel.open_agent_query(sql, window, cx))
}

fn focus_sql_editor(
    project: &Entity<Project>,
    abs_path: &Path,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let view = query_view::query_views(project, cx)
        .into_iter()
        .find(|view| view.read(cx).path() == abs_path)
        .with_context(|| format!("A aba SQL {} foi fechada.", abs_path.display()))?;
    query_view::activate(&view, window, cx)
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "database".into(),
            name: "Banco".into(),
            description: "Consultas de leitura na conexão do painel Banco".into(),
            icon: "database".into(),
            tools: vec![
                ToolkitTool::new(
                    "database_query",
                    "Consultou o banco",
                    "Runs one read-only SQL statement (SELECT, WITH, VALUES, TABLE) against the \
                     PostgreSQL database connected in the Banco panel, on a separate read-only \
                     connection, and returns the rows as a table. Use it to answer questions \
                     about the data yourself: run the query and answer from the rows instead \
                     of giving the user SQL to run. The user sees the rows in a card under the \
                     call, so select only the columns that answer the question and reply with \
                     the fact in a sentence, without repeating the rows.",
                    ToolAccess::Read,
                    run_query,
                ),
                // Read access, so it doesn't ask: it only edits the open editor (one undo
                // brings it back), and nothing reaches the database until the user runs it.
                ToolkitTool::new(
                    "database_write_query",
                    "Escreveu a query",
                    "Writes SQL into a SQL editor tab of the Banco panel, which the user \
                     mentioned with @. Nothing runs: the user reviews and runs it. After \
                     writing, end the turn without any text.",
                    ToolAccess::Read,
                    write_query,
                ),
            ],
        },
        cx,
    );
    task_agents::register_sql_editor_source(
        SqlEditorSource {
            list: Arc::new(list_sql_editors),
            describe: Arc::new(describe_sql_editor),
            open_query: Arc::new(open_query),
            focus: Arc::new(focus_sql_editor),
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::single_source;

    #[test]
    fn names_the_single_relation_a_select_reads() {
        assert_eq!(
            single_source("select id, email from users where email = 'a@b.c'").as_deref(),
            Some("users")
        );
        assert_eq!(
            single_source("SELECT count(*) FROM public.payments WHERE status = 'failed'")
                .as_deref(),
            Some("public.payments")
        );
        assert_eq!(
            single_source("select * from \"Orders\" limit 5").as_deref(),
            Some("Orders")
        );
        assert_eq!(
            single_source("select (select 1 from a), b from c").as_deref(),
            Some("c")
        );
    }

    #[test]
    fn leaves_joins_and_subqueries_unnamed() {
        assert_eq!(
            single_source("select * from users u join orders o on o.user_id = u.id"),
            None
        );
        assert_eq!(single_source("select * from users, orders"), None);
        assert_eq!(single_source("select * from (select 1) as t"), None);
        assert_eq!(single_source("with x as (select 1) select * from x"), None);
        assert_eq!(single_source("select 1"), None);
    }
}
