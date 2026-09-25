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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// As `format_type` prints it: `varchar(14)`, `timestamp with time zone`, `text[]`.
    pub type_name: String,
    pub not_null: bool,
    pub primary_key: bool,
    /// Unique on its own, not as part of a wider index.
    pub unique: bool,
    /// The default expression, as `pg_get_expr` prints it.
    pub default: Option<String>,
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Relies on `standard_conforming_strings`, on by default since Postgres 9.1.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn qualified_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

pub async fn list_columns(
    session: &Session,
    schema: &str,
    relation: &str,
) -> anyhow::Result<Vec<ColumnInfo>> {
    let sql = format!(
        "select a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull,
                coalesce((select bool_or(i.indisprimary) from pg_catalog.pg_index i
                           where i.indrelid = a.attrelid and a.attnum = any(i.indkey)), false),
                coalesce((select bool_or(i.indisunique and not i.indisprimary) from pg_catalog.pg_index i
                           where i.indrelid = a.attrelid and i.indnatts = 1 and i.indkey[0] = a.attnum), false),
                pg_catalog.pg_get_expr(d.adbin, d.adrelid)
           from pg_catalog.pg_attribute a
           left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
          where a.attrelid = {}::regclass and a.attnum > 0 and not a.attisdropped
          order by a.attnum",
        quote_literal(&qualified_name(schema, relation))
    );
    let rows = session.query_text(&sql).await?;
    Ok(rows.into_iter().filter_map(parse_column).collect())
}

fn parse_column(row: Vec<Option<String>>) -> Option<ColumnInfo> {
    let mut values = row.into_iter();
    let name = values.next().flatten()?;
    let type_name = values.next().flatten()?;
    let mut flag = || values.next().flatten().as_deref() == Some("t");
    let (not_null, primary_key, unique) = (flag(), flag(), flag());
    Some(ColumnInfo {
        name,
        type_name,
        not_null,
        primary_key,
        unique,
        default: values.next().flatten(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    /// `CREATE [UNIQUE] INDEX … USING btree (col)`, as `pg_get_indexdef` prints it.
    pub definition: String,
    pub primary: bool,
    pub unique: bool,
    pub size: String,
    /// Index scans since the statistics were last reset; `None` without statistics.
    pub scans: Option<i64>,
    /// Backs a PRIMARY KEY or UNIQUE constraint, so the DDL lists it with the table.
    pub constraint: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKeyDirection {
    /// This table points at another one.
    References,
    /// Another table points at this one.
    ReferencedBy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub direction: ForeignKeyDirection,
    /// The table on the other end, schema-qualified when it isn't on the search path.
    pub other_table: String,
    /// `FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE`.
    pub definition: String,
    pub on_delete: String,
}

pub async fn list_indexes(
    session: &Session,
    schema: &str,
    relation: &str,
) -> anyhow::Result<Vec<IndexInfo>> {
    let sql = format!(
        "select i.relname, pg_catalog.pg_get_indexdef(i.oid), x.indisprimary, x.indisunique,
                pg_catalog.pg_size_pretty(pg_catalog.pg_relation_size(i.oid)), s.idx_scan,
                exists (select 1 from pg_catalog.pg_constraint c where c.conindid = i.oid
                         and c.conrelid = x.indrelid and c.contype in ('p', 'u'))
           from pg_catalog.pg_index x
           join pg_catalog.pg_class i on i.oid = x.indexrelid
           left join pg_catalog.pg_stat_all_indexes s on s.indexrelid = x.indexrelid
          where x.indrelid = {}::regclass
          order by x.indisprimary desc, i.relname",
        quote_literal(&qualified_name(schema, relation))
    );
    let rows = session.query_text(&sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let definition = values.next().flatten()?;
            let primary = values.next().flatten().as_deref() == Some("t");
            let unique = values.next().flatten().as_deref() == Some("t");
            let size = values.next().flatten().unwrap_or_default();
            let scans = values.next().flatten().and_then(|scans| scans.parse().ok());
            let constraint = values.next().flatten().as_deref() == Some("t");
            Some(IndexInfo {
                name,
                definition,
                primary,
                unique,
                size,
                scans,
                constraint,
            })
        })
        .collect())
}

pub async fn list_foreign_keys(
    session: &Session,
    schema: &str,
    relation: &str,
) -> anyhow::Result<Vec<ForeignKey>> {
    let table = quote_literal(&qualified_name(schema, relation));
    let sql = format!(
        "select c.conname, c.conrelid = {table}::regclass,
                case when c.conrelid = {table}::regclass then c.confrelid::regclass::text
                     else c.conrelid::regclass::text end,
                pg_catalog.pg_get_constraintdef(c.oid), c.confdeltype::text
           from pg_catalog.pg_constraint c
          where c.contype = 'f' and (c.conrelid = {table}::regclass or c.confrelid = {table}::regclass)
          order by 2 desc, 3, 1"
    );
    let rows = session.query_text(&sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let outgoing = values.next().flatten().as_deref() == Some("t");
            let other_table = values.next().flatten()?;
            let definition = values.next().flatten()?;
            let on_delete = match values.next().flatten().as_deref() {
                Some("c") => "CASCADE",
                Some("n") => "SET NULL",
                Some("d") => "SET DEFAULT",
                Some("r") => "RESTRICT",
                _ => "NO ACTION",
            }
            .to_owned();
            Some(ForeignKey {
                name,
                direction: if outgoing {
                    ForeignKeyDirection::References
                } else {
                    ForeignKeyDirection::ReferencedBy
                },
                other_table,
                definition,
                on_delete,
            })
        })
        .collect())
}

/// The table's own constraints (primary key, unique, check, outgoing foreign keys) as
/// `pg_get_constraintdef` prints them, for the DDL.
pub async fn list_constraints(
    session: &Session,
    schema: &str,
    relation: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let sql = format!(
        "select conname, pg_catalog.pg_get_constraintdef(oid)
           from pg_catalog.pg_constraint
          where conrelid = {}::regclass and contype in ('p', 'u', 'c', 'f', 'x')
          order by case contype when 'p' then 0 when 'u' then 1 when 'c' then 2 else 3 end, conname",
        quote_literal(&qualified_name(schema, relation))
    );
    let rows = session.query_text(&sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            Some((values.next().flatten()?, values.next().flatten()?))
        })
        .collect())
}

/// A `CREATE TABLE` rebuilt from the catalog. Postgres has no function that prints one, so
/// this covers columns, defaults, constraints and indexes, not storage options or grants.
pub fn table_ddl(
    schema: &str,
    relation: &str,
    columns: &[ColumnInfo],
    constraints: &[(String, String)],
    indexes: &[IndexInfo],
) -> String {
    let mut lines = columns
        .iter()
        .map(|column| {
            let mut line = format!("    {} {}", quote_ident(&column.name), column.type_name);
            if let Some(default) = &column.default {
                line.push_str(&format!(" default {default}"));
            }
            if column.not_null {
                line.push_str(" not null");
            }
            line
        })
        .collect::<Vec<_>>();
    lines.extend(
        constraints
            .iter()
            .map(|(name, definition)| format!("    constraint {} {definition}", quote_ident(name))),
    );
    let mut ddl = format!(
        "create table {} (\n{}\n);\n",
        qualified_name(schema, relation),
        lines.join(",\n")
    );
    for index in indexes.iter().filter(|index| !index.constraint) {
        ddl.push_str(&format!("{};\n", index.definition));
    }
    ddl
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
    fn identifiers_and_literals_are_quoted() {
        assert_eq!(qualified_name("public", "my \"odd\" table"), "\"public\".\"my \"\"odd\"\" table\"");
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn unknown_kinds_are_skipped() {
        assert_eq!(
            parse_relation(row(&[Some("public"), Some("users_seq"), Some("S"), Some("1")])),
            None
        );
    }
}
