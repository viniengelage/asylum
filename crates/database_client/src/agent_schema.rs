//! What the agent's `database_schema` tool reads: the catalog of the connection open in the
//! project's Banco panel, as text. It only queries the catalog, never the data.

use crate::{
    catalog::{self, ForeignKeyDirection, Relation, RelationKind},
    panel::{ActiveState, DatabasePanel},
};
use anyhow::{Context as _, anyhow};
use collections::HashMap;
use gpui::{App, AppContext as _, Entity, EntityId, Global, Task, WeakEntity};
use project::Project;

/// Panels by project, so a thread (which only knows its project) finds the right connection.
#[derive(Default)]
pub struct PanelRegistry(HashMap<EntityId, WeakEntity<DatabasePanel>>);

impl Global for PanelRegistry {}

pub fn register_panel(project: &Entity<Project>, panel: WeakEntity<DatabasePanel>, cx: &mut App) {
    cx.default_global::<PanelRegistry>()
        .0
        .insert(project.entity_id(), panel);
}

fn kind_label(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::PartitionedTable => "partitioned table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
    }
}

fn find<'a>(relations: &'a [Relation], name: &str) -> Option<&'a Relation> {
    let name = name.trim().trim_matches('"');
    match name.split_once('.') {
        Some((schema, relation)) => relations.iter().find(|candidate| {
            candidate
                .schema
                .eq_ignore_ascii_case(schema.trim_matches('"'))
                && candidate
                    .name
                    .eq_ignore_ascii_case(relation.trim_matches('"'))
        }),
        None => {
            let mut matches = relations
                .iter()
                .filter(|candidate| candidate.name.eq_ignore_ascii_case(name));
            let first = matches.next()?;
            Some(
                std::iter::once(first)
                    .chain(matches)
                    .find(|candidate| candidate.schema == "public")
                    .unwrap_or(first),
            )
        }
    }
}

pub fn describe_for_agent(
    project: Entity<Project>,
    table: Option<String>,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let panel = cx
        .try_global::<PanelRegistry>()
        .and_then(|registry| registry.0.get(&project.entity_id()).cloned())
        .and_then(|panel| panel.upgrade());
    let Some(panel) = panel else {
        return Task::ready(Err(anyhow!(
            "The Banco panel isn't open in this project. Ask the user to open it and connect to a \
             database."
        )));
    };
    let panel = panel.read(cx);
    let Some(active) = panel.active() else {
        return Task::ready(Err(anyhow!(
            "No database is connected in the Banco panel. Ask the user to connect one."
        )));
    };
    let ActiveState::Connected {
        session, relations, ..
    } = &active.state
    else {
        return Task::ready(Err(anyhow!(
            "The Banco panel is still connecting, or the connection failed."
        )));
    };
    let connection = active.connection.clone();
    let session = session.clone();
    let relations = relations.clone();
    let mut header = format!(
        "Database `{}` ({}, {}, PostgreSQL {})",
        connection.database,
        connection.environment.label(),
        connection.address(),
        session
            .server_version
            .split_whitespace()
            .next()
            .unwrap_or_default()
    );
    if connection.read_only {
        header.push_str(", read-only connection");
    }
    let Some(table) = table else {
        let mut text = format!("{header}\n");
        let mut current_schema = None;
        for relation in &relations {
            if current_schema != Some(relation.schema.as_str()) {
                current_schema = Some(relation.schema.as_str());
                text.push_str(&format!("\nSchema {}:\n", relation.schema));
            }
            text.push_str(&format!(
                "- {} ({}",
                relation.name,
                kind_label(relation.kind)
            ));
            if let Some(rows) = relation.estimated_rows {
                text.push_str(&format!(", ~{rows} rows"));
            }
            text.push_str(")\n");
        }
        text.push_str("\nCall again with `table` to see columns, keys and indexes.");
        return Task::ready(Ok(text));
    };
    let Some(relation) = find(&relations, &table).cloned() else {
        let names = relations
            .iter()
            .map(|relation| format!("{}.{}", relation.schema, relation.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Task::ready(Err(anyhow!(
            "No table or view named `{table}`. Known relations: {names}"
        )));
    };
    cx.background_spawn(async move {
        let schema = &relation.schema;
        let name = &relation.name;
        let columns = catalog::list_columns(&session, schema, name)
            .await
            .context("reading columns")?;
        let indexes = catalog::list_indexes(&session, schema, name).await?;
        let foreign_keys = catalog::list_foreign_keys(&session, schema, name).await?;
        let constraints = catalog::list_constraints(&session, schema, name).await?;
        let mut text = format!("{header}\n\n{schema}.{name} ({}", kind_label(relation.kind));
        if let Some(rows) = relation.estimated_rows {
            text.push_str(&format!(", ~{rows} rows"));
        }
        text.push_str(")\n\nColumns:\n");
        for column in &columns {
            text.push_str(&format!("- {} {}", column.name, column.type_name));
            if column.not_null {
                text.push_str(" not null");
            }
            if let Some(default) = &column.default {
                text.push_str(&format!(" default {default}"));
            }
            if column.primary_key {
                text.push_str(" (primary key)");
            } else if column.unique {
                text.push_str(" (unique)");
            }
            text.push('\n');
        }
        if !indexes.is_empty() {
            text.push_str("\nIndexes:\n");
            for index in &indexes {
                text.push_str(&format!("- {}\n", index.definition));
            }
        }
        if !foreign_keys.is_empty() {
            text.push_str("\nForeign keys:\n");
            for foreign_key in &foreign_keys {
                let direction = match foreign_key.direction {
                    ForeignKeyDirection::References => "references",
                    ForeignKeyDirection::ReferencedBy => "referenced by",
                };
                text.push_str(&format!(
                    "- {direction} {}: {}\n",
                    foreign_key.other_table, foreign_key.definition
                ));
            }
        }
        text.push_str("\nDDL:\n");
        text.push_str(&catalog::table_ddl(
            schema,
            name,
            &columns,
            &constraints,
            &indexes,
        ));
        Ok(text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation(schema: &str, name: &str) -> Relation {
        Relation {
            schema: schema.to_owned(),
            name: name.to_owned(),
            kind: RelationKind::Table,
            estimated_rows: None,
        }
    }

    #[test]
    fn finds_tables_preferring_public() {
        let relations = [
            relation("audit", "users"),
            relation("public", "users"),
            relation("audit", "log"),
        ];
        let found = |name| find(&relations, name).map(|r| format!("{}.{}", r.schema, r.name));
        assert_eq!(found("users").as_deref(), Some("public.users"));
        assert_eq!(found("audit.users").as_deref(), Some("audit.users"));
        assert_eq!(found("\"LOG\"").as_deref(), Some("audit.log"));
        assert_eq!(found("missing"), None);
    }
}
