use crate::session::Session;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    fn from_relkind(relkind: &str) -> Option<Self> {
        match relkind {
            "r" => Some(Self::Table),
            "p" => Some(Self::PartitionedTable),
            "v" => Some(Self::View),
            "m" => Some(Self::MaterializedView),
            "f" => Some(Self::ForeignTable),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// The planner's estimate from the last ANALYZE; `None` when the table was never analyzed.
    pub estimated_rows: Option<i64>,
}

const RELATIONS_SQL: &str = "\
select n.nspname, c.relname, c.relkind::text, c.reltuples::bigint
  from pg_catalog.pg_class c
  join pg_catalog.pg_namespace n on n.oid = c.relnamespace
 where c.relkind in ('r', 'p', 'v', 'm', 'f')
   and not c.relispartition
   and n.nspname <> 'information_schema'
   and n.nspname not like 'pg\\_%'
 order by n.nspname, c.relname";

/// Tables, views and friends in every user schema, in the order the dock lists them.
pub async fn list_relations(session: &Session) -> anyhow::Result<Vec<Relation>> {
    let rows = session.query_text(RELATIONS_SQL).await?;
    Ok(rows.into_iter().filter_map(parse_relation).collect())
}

fn parse_relation(row: Vec<Option<String>>) -> Option<Relation> {
    let mut values = row.into_iter();
    let schema = values.next().flatten()?;
    let name = values.next().flatten()?;
    let kind = RelationKind::from_relkind(&values.next().flatten()?)?;
    // Since Postgres 14 a table that was never analyzed reports -1.
    let estimated_rows = values
        .next()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|rows| *rows >= 0);
    Some(Relation {
        schema,
        name,
        kind,
        estimated_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[Option<&str>]) -> Vec<Option<String>> {
        values.iter().map(|value| value.map(str::to_owned)).collect()
    }

    #[test]
    fn never_analyzed_tables_have_no_estimate() {
        let relation = parse_relation(row(&[Some("public"), Some("users"), Some("r"), Some("-1")]));
        assert_eq!(
            relation,
            Some(Relation {
                schema: "public".into(),
                name: "users".into(),
                kind: RelationKind::Table,
                estimated_rows: None,
            })
        );

        let relation = parse_relation(row(&[Some("audit"), Some("log"), Some("p"), Some("1200")]));
        assert_eq!(relation.map(|relation| relation.estimated_rows), Some(Some(1200)));
    }

    #[test]
    fn unknown_kinds_are_skipped() {
        assert_eq!(
            parse_relation(row(&[Some("public"), Some("users_seq"), Some("S"), Some("1")])),
            None
        );
    }
}
