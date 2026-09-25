use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, Entity, Global, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where the schema comes from: the app installs this with the database panel, so the agent
/// doesn't depend on the Postgres client. It receives the thread's project and the table asked
/// for, and describes the catalog of the connection open in that project's panel.
#[derive(Clone)]
pub struct DatabaseSchemaSource(
    pub Arc<dyn Fn(Entity<Project>, Option<String>, &mut App) -> Task<Result<String>>>,
);

impl Global for DatabaseSchemaSource {}

/// Read the schema of the Postgres database the user has connected in the Banco panel.
///
/// Without `table`, lists every schema with its tables and views and their estimated row
/// counts. With `table` (`users` or `public.users`), returns that table's columns with types,
/// nullability, defaults and keys, its indexes, the foreign keys in both directions, and a
/// CREATE TABLE statement rebuilt from the catalog.
///
/// This only reads the catalog; it never runs a query on the data. Use it before writing SQL
/// against the user's database so table and column names are right. If no database is
/// connected, tell the user to connect one in the Banco panel.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct DatabaseSchemaToolInput {
    /// A table or view to describe, optionally schema-qualified. Leave empty to list them all.
    #[serde(default)]
    pub table: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DatabaseSchemaToolOutput {
    Schema { schema: String },
    Error { error: String },
}

impl From<DatabaseSchemaToolOutput> for LanguageModelToolResultContent {
    fn from(value: DatabaseSchemaToolOutput) -> Self {
        match value {
            DatabaseSchemaToolOutput::Schema { schema } => schema.into(),
            DatabaseSchemaToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct DatabaseSchemaTool {
    project: Entity<Project>,
}

impl DatabaseSchemaTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for DatabaseSchemaTool {
    type Input = DatabaseSchemaToolInput;
    type Output = DatabaseSchemaToolOutput;

    const NAME: &'static str = "database_schema";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input.ok().and_then(|input| input.table) {
            Some(table) => format!("Reading the schema of {table}").into(),
            None => "Listing database tables".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let source = cx.try_global::<DatabaseSchemaSource>().cloned();
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| DatabaseSchemaToolOutput::Error {
                    error: error.to_string(),
                })?;
            let Some(source) = source else {
                return Err(DatabaseSchemaToolOutput::Error {
                    error: "This build has no database panel.".to_string(),
                });
            };
            let table = input.table.filter(|table| !table.trim().is_empty());
            let task = cx.update(|cx| (source.0)(project, table, cx));
            match task.await {
                Ok(schema) => Ok(DatabaseSchemaToolOutput::Schema { schema }),
                Err(error) => Err(DatabaseSchemaToolOutput::Error {
                    error: format!("{error:#}"),
                }),
            }
        })
    }
}
