//! Turns cells edited in the grid into UPDATE statements. Each one finds its row by primary key
//! and also checks the old value of every changed column, so a row someone else changed since
//! the page loaded matches nothing and isn't overwritten.

use crate::catalog::{qualified_name, quote_ident, quote_literal};
use collections::BTreeMap;

/// Pending changes of one page: `(row, column)` in the grid → new value (`None` is NULL).
pub type PendingEdits = BTreeMap<(usize, usize), Option<String>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowUpdate {
    pub row: usize,
    pub sql: String,
}

fn literal(value: Option<&str>) -> String {
    match value {
        Some(value) => quote_literal(value),
        None => "null".to_owned(),
    }
}

/// One UPDATE per edited row, in row order. `primary_key` holds the grid indexes of the
/// primary-key columns; cells that ended up equal to what was loaded are left out.
pub fn row_updates(
    schema: &str,
    table: &str,
    column_names: &[String],
    primary_key: &[usize],
    rows: &[Vec<Option<String>>],
    edits: &PendingEdits,
) -> Result<Vec<RowUpdate>, String> {
    if primary_key.is_empty() {
        return Err("A tabela não tem chave primária: sem ela não dá para achar a linha.".into());
    }
    let mut by_row: BTreeMap<usize, Vec<(usize, Option<&str>)>> = BTreeMap::default();
    for ((row, column), value) in edits {
        let old = rows
            .get(*row)
            .and_then(|values| values.get(*column))
            .ok_or_else(|| format!("A célula ({row}, {column}) não existe mais nesta página."))?;
        if old.as_deref() != value.as_deref() {
            by_row
                .entry(*row)
                .or_default()
                .push((*column, value.as_deref()));
        }
    }
    let target = qualified_name(schema, table);
    by_row
        .into_iter()
        .map(|(row, changes)| {
            let values = &rows[row];
            let name = |column: usize| {
                column_names
                    .get(column)
                    .map(|name| quote_ident(name))
                    .ok_or_else(|| format!("Coluna {column} desconhecida."))
            };
            let assignments = changes
                .iter()
                .map(|(column, value)| Ok(format!("{} = {}", name(*column)?, literal(*value))))
                .collect::<Result<Vec<_>, String>>()?;
            let mut conditions = primary_key
                .iter()
                .map(|column| {
                    let value = values.get(*column).and_then(|value| value.as_deref());
                    Ok(format!("{} = {}", name(*column)?, literal(value)))
                })
                .collect::<Result<Vec<_>, String>>()?;
            // Compare as text: that is what the grid shows, and `json` has no `=` operator.
            for (column, _) in &changes {
                let old = values.get(*column).and_then(|value| value.as_deref());
                conditions.push(format!(
                    "{}::text is not distinct from {}",
                    name(*column)?,
                    literal(old)
                ));
            }
            Ok(RowUpdate {
                row,
                sql: format!(
                    "update {target}\n   set {}\n where {}",
                    assignments.join(",\n       "),
                    conditions.join("\n   and ")
                ),
            })
        })
        .collect()
}

/// The script "Ver SQL" shows: every update inside one transaction.
pub fn preview(updates: &[RowUpdate]) -> String {
    let mut script = String::from("begin;\n");
    for update in updates {
        script.push_str(&update.sql);
        script.push_str(";\n");
    }
    script.push_str("commit;\n");
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_guard_the_old_values() {
        let names = ["id", "email", "roles"].map(str::to_owned);
        let rows = vec![
            vec![
                Some("48209".to_owned()),
                Some("diego.martins@trix.com.br".to_owned()),
                Some("{investor}".to_owned()),
            ],
            vec![
                Some("48205".to_owned()),
                None,
                Some("{investor}".to_owned()),
            ],
        ];
        let mut edits = PendingEdits::default();
        edits.insert((0, 1), Some("diego@trix.com.br".to_owned()));
        edits.insert((1, 1), Some("o'brien@x.com".to_owned()));
        edits.insert((1, 2), None);
        // Typed back to what it was: nothing to write.
        edits.insert((0, 2), Some("{investor}".to_owned()));
        let updates = row_updates("public", "users", &names, &[0], &rows, &edits).unwrap();
        assert_eq!(
            updates,
            [
                RowUpdate {
                    row: 0,
                    sql: "update \"public\".\"users\"\n   set \"email\" = 'diego@trix.com.br'\n \
                          where \"id\" = '48209'\n   and \"email\"::text is not distinct from \
                          'diego.martins@trix.com.br'"
                        .into(),
                },
                RowUpdate {
                    row: 1,
                    sql:
                        "update \"public\".\"users\"\n   set \"email\" = 'o''brien@x.com',\n       \
                          \"roles\" = null\n where \"id\" = '48205'\n   and \"email\"::text is \
                          not distinct from null\n   and \"roles\"::text is not distinct from \
                          '{investor}'"
                            .into(),
                },
            ]
        );
        assert!(preview(&updates).starts_with("begin;\nupdate"));
        assert!(preview(&updates).ends_with(";\ncommit;\n"));
    }

    #[test]
    fn needs_a_primary_key() {
        let error = row_updates("public", "log", &[], &[], &[], &PendingEdits::default());
        assert!(error.is_err());
    }
}
