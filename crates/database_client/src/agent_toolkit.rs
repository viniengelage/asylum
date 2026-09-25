//! The `database_query` agent tool: one SELECT on a read-only connection opened next to the one
//! in the project's Banco panel, so the agent can look at data without being able to change it.

use crate::{
    agent_schema::PanelRegistry,
    panel::{ActiveState, DatabasePanel},
    statements,
};
use anyhow::{Context as _, anyhow};
use gpui::{App, AppContext as _, Entity};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};

const DEFAULT_ROW_LIMIT: usize = 100;
const MAX_ROW_LIMIT: usize = 1000;
const MAX_CELL_CHARS: usize = 200;
const MAX_OUTPUT_CHARS: usize = 40_000;

#[derive(Deserialize, JsonSchema)]
struct QueryInput {
    /// One read-only statement: SELECT, WITH … SELECT, VALUES or TABLE. Use `database_schema`
    /// first to get table and column names right.
    sql: String,
    /// Maximum rows to return. Defaults to 100, at most 1000.
    #[serde(default)]
    limit: Option<usize>,
}

fn panel_for(project: &Entity<Project>, cx: &App) -> anyhow::Result<Entity<DatabasePanel>> {
    cx.try_global::<PanelRegistry>()
        .and_then(|registry| registry.panel(project))
        .context("O painel Banco não está aberto neste projeto. Peça para abrir e conectar um banco.")
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

fn run_query(project: Entity<Project>, input: QueryInput, cx: &mut App) -> gpui::Task<anyhow::Result<ToolkitOutput>> {
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
    let limit = input.limit.unwrap_or(DEFAULT_ROW_LIMIT).clamp(1, MAX_ROW_LIMIT);
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
    cx.background_spawn(async move {
        let read_only = session.connect_read_only().await?;
        let outcome = read_only.run_in_cursor(&sql, limit).await?;
        let result = outcome
            .result_sets
            .into_iter()
            .next()
            .context("a consulta não devolveu resultado")?;
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
            let line = format!("| {} |\n", row.iter().map(cell).collect::<Vec<_>>().join(" | "));
            if text.len() + line.len() > MAX_OUTPUT_CHARS {
                text.push_str("… (saída cortada)\n");
                break;
            }
            text.push_str(&line);
        }
        Ok(ToolkitOutput::text(text))
    })
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "database".into(),
            name: "Banco".into(),
            description: "Consultas de leitura na conexão do painel Banco".into(),
            icon: "database".into(),
            tools: vec![ToolkitTool::new(
                "database_query",
                "Consultou o banco",
                "Runs one read-only SQL statement (SELECT, WITH, VALUES, TABLE) against the \
                 PostgreSQL database connected in the Banco panel, on a separate read-only \
                 connection, and returns the rows as a table.",
                ToolAccess::Read,
                run_query,
            )],
        },
        cx,
    );
}
