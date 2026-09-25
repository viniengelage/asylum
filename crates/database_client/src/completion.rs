//! Autocomplete for the SQL editor without a language server: tables and columns from a
//! catalog snapshot, aliases from the statement under the cursor (`t.` → the columns of the
//! table aliased `t`), and SQL keywords.

use collections::HashMap;
use editor::{CompletionProvider, Editor};
use gpui::{Context, Entity, Task, Window};
use language::{CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};
use std::{cell::RefCell, rc::Rc};

const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "join",
    "left join",
    "inner join",
    "on",
    "group by",
    "order by",
    "having",
    "limit",
    "offset",
    "insert into",
    "values",
    "update",
    "set",
    "delete from",
    "returning",
    "with",
    "as",
    "and",
    "or",
    "not",
    "null",
    "is null",
    "is not null",
    "in",
    "exists",
    "between",
    "like",
    "ilike",
    "case",
    "when",
    "then",
    "else",
    "end",
    "distinct",
    "union all",
    "count(*)",
    "coalesce",
    "now()",
    "interval",
    "true",
    "false",
    "asc",
    "desc",
    "begin",
    "commit",
    "rollback",
    "explain analyze",
];

/// Words after which the next word can't be an alias.
const NOT_ALIASES: &[&str] = &[
    "where",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "on",
    "using",
    "group",
    "order",
    "limit",
    "offset",
    "having",
    "union",
    "set",
    "values",
    "returning",
    "natural",
    "lateral",
    "window",
    "for",
];

#[derive(Default)]
pub struct SchemaCache {
    /// `(schema, relation)` → columns with their types.
    columns: HashMap<(String, String), Vec<(String, String)>>,
    relations: Vec<(String, String)>,
}

impl SchemaCache {
    pub fn new(columns: Vec<(String, String, String, String)>) -> Self {
        let mut cache = Self::default();
        for (schema, relation, column, type_name) in columns {
            let key = (schema, relation);
            if !cache.columns.contains_key(&key) {
                cache.relations.push(key.clone());
            }
            cache
                .columns
                .entry(key)
                .or_default()
                .push((column, type_name));
        }
        cache
    }

    /// `users`, `public.users` or `"Users"` → the relation it names, preferring `public`.
    fn resolve(&self, name: &str) -> Option<&(String, String)> {
        let unquote = |part: &str| part.trim_matches('"').to_owned();
        match name.split_once('.') {
            Some((schema, relation)) => {
                let (schema, relation) = (unquote(schema), unquote(relation));
                self.relations.iter().find(|(s, r)| {
                    s.eq_ignore_ascii_case(&schema) && r.eq_ignore_ascii_case(&relation)
                })
            }
            None => {
                let relation = unquote(name);
                let mut matches = self
                    .relations
                    .iter()
                    .filter(|(_, r)| r.eq_ignore_ascii_case(&relation));
                let first = matches.next()?;
                Some(
                    std::iter::once(first)
                        .chain(matches)
                        .find(|(schema, _)| schema == "public")
                        .unwrap_or(first),
                )
            }
        }
    }
}

/// Tables named after FROM or JOIN in the statement, with their aliases.
fn referenced_tables(statement: &str) -> Vec<(String, Option<String>)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in statement.chars() {
        if character.is_alphanumeric() || matches!(character, '_' | '.' | '"') {
            current.push(character);
        } else {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            if character == ',' || character == '(' || character == ')' {
                tokens.push(character.to_string());
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    let mut tables = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let keyword = tokens[index].to_ascii_lowercase();
        if keyword == "from" || keyword == "join" || keyword == "update" || keyword == "into" {
            let mut next = index + 1;
            while let Some(table) = tokens.get(next).filter(|token| *token != "(") {
                let mut alias = None;
                let mut after = next + 1;
                if tokens
                    .get(after)
                    .is_some_and(|token| token.eq_ignore_ascii_case("as"))
                {
                    after += 1;
                }
                if let Some(candidate) = tokens.get(after)
                    && candidate != ","
                    && candidate != "("
                    && candidate != ")"
                    && !NOT_ALIASES.contains(&candidate.to_ascii_lowercase().as_str())
                {
                    alias = Some(candidate.clone());
                    after += 1;
                }
                tables.push((table.clone(), alias));
                if tokens.get(after).is_some_and(|token| token == ",") && keyword == "from" {
                    next = after + 1;
                    continue;
                }
                break;
            }
        }
        index += 1;
    }
    tables
}

#[derive(Debug, PartialEq, Eq)]
struct Suggestion {
    text: String,
    label: String,
    detail: Option<String>,
}

/// What to offer for `prefix` (the word before the cursor, dots included) in `statement`.
fn suggestions(cache: &SchemaCache, statement: &str, prefix: &str) -> Vec<Suggestion> {
    let tables = referenced_tables(statement);
    if let Some((qualifier, _)) = prefix.rsplit_once('.') {
        let aliased = tables
            .iter()
            .find(|(_, alias)| {
                alias
                    .as_deref()
                    .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
            })
            .map(|(table, _)| table.as_str())
            .unwrap_or(qualifier);
        if let Some(key) = cache.resolve(aliased)
            && let Some(columns) = cache.columns.get(key)
        {
            return columns
                .iter()
                .map(|(column, type_name)| Suggestion {
                    text: column.clone(),
                    label: format!("{column}  {type_name}"),
                    detail: Some(format!("{}.{} · {type_name}", key.0, key.1)),
                })
                .collect();
        }
        // `public.` → the tables of that schema.
        return cache
            .relations
            .iter()
            .filter(|(schema, _)| schema.eq_ignore_ascii_case(qualifier))
            .map(|(_, relation)| Suggestion {
                text: relation.clone(),
                label: relation.clone(),
                detail: Some(format!("tabela em {qualifier}")),
            })
            .collect();
    }
    let mut result = Vec::new();
    for (table, _) in &tables {
        if let Some(key) = cache.resolve(table)
            && let Some(columns) = cache.columns.get(key)
        {
            result.extend(columns.iter().map(|(column, type_name)| Suggestion {
                text: column.clone(),
                label: format!("{column}  {type_name}"),
                detail: Some(format!("{}.{} · {type_name}", key.0, key.1)),
            }));
        }
    }
    result.extend(cache.relations.iter().map(|(schema, relation)| {
        let text = if schema == "public" {
            relation.clone()
        } else {
            format!("{schema}.{relation}")
        };
        Suggestion {
            label: text.clone(),
            text,
            detail: Some("tabela".to_owned()),
        }
    }));
    result.extend(KEYWORDS.iter().map(|keyword| Suggestion {
        text: (*keyword).to_owned(),
        label: (*keyword).to_owned(),
        detail: None,
    }));
    result
}

pub struct SqlCompletionProvider {
    pub cache: Rc<RefCell<SchemaCache>>,
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<language::Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let offset = buffer_position.to_offset(buffer);
        let text = buffer.text();
        let word_start = text[..offset]
            .char_indices()
            .rev()
            .take_while(|(_, character)| {
                character.is_alphanumeric() || matches!(character, '_' | '.' | '"')
            })
            .last()
            .map_or(offset, |(index, _)| index);
        let prefix = &text[word_start..offset];
        // Only the part after the last dot is replaced.
        let replace_start = prefix
            .rfind('.')
            .map_or(word_start, |dot| word_start + dot + 1);
        let statements = crate::statements::split(&text);
        let statement = crate::statements::statement_at(&statements, offset)
            .map(|statement| &text[statement.range.clone()])
            .unwrap_or_default();
        let suggestions = suggestions(&self.cache.borrow(), statement, prefix);
        let replace_range = buffer.anchor_before(replace_start)..buffer_position;
        let completions = suggestions
            .into_iter()
            .take(500)
            .map(|suggestion| Completion {
                replace_range: replace_range.clone(),
                label: CodeLabel::plain(suggestion.label, Some(&suggestion.text)),
                new_text: suggestion.text,
                documentation: suggestion
                    .detail
                    .map(|detail| CompletionDocumentation::SingleLine(detail.into())),
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<language::Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars()
            .last()
            .is_some_and(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> SchemaCache {
        let column = |schema: &str, relation: &str, name: &str, type_name: &str| {
            (
                schema.to_owned(),
                relation.to_owned(),
                name.to_owned(),
                type_name.to_owned(),
            )
        };
        SchemaCache::new(vec![
            column("public", "transactions", "id", "bigint"),
            column("public", "transactions", "status", "text"),
            column("public", "accounts", "id", "bigint"),
            column("public", "accounts", "user_id", "bigint"),
            column("audit", "log", "message", "text"),
        ])
    }

    fn texts(suggestions: Vec<Suggestion>) -> Vec<String> {
        suggestions
            .into_iter()
            .map(|suggestion| suggestion.text)
            .collect()
    }

    #[test]
    fn aliases_resolve_to_their_columns() {
        let statement = "select t.st from transactions t join accounts as a on a.id = t.id";
        assert_eq!(
            texts(suggestions(&cache(), statement, "t.st")),
            ["id", "status"]
        );
        assert_eq!(
            texts(suggestions(&cache(), statement, "a.")),
            ["id", "user_id"]
        );
        // A table name works as its own qualifier.
        assert_eq!(
            texts(suggestions(
                &cache(),
                "select accounts. from accounts",
                "accounts."
            )),
            ["id", "user_id"]
        );
        assert_eq!(
            texts(suggestions(&cache(), "select * from audit.", "audit.")),
            ["log"]
        );
    }

    #[test]
    fn plain_words_offer_columns_tables_and_keywords() {
        let offered = texts(suggestions(
            &cache(),
            "select s from transactions where ",
            "s",
        ));
        assert_eq!(&offered[..2], ["id", "status"]);
        assert!(offered.contains(&"accounts".to_owned()));
        assert!(offered.contains(&"audit.log".to_owned()));
        assert!(offered.contains(&"group by".to_owned()));
    }

    #[test]
    fn finds_tables_and_aliases() {
        assert_eq!(
            referenced_tables("select * from a x, public.b y join c on c.id = x.id where 1 = 1"),
            [
                ("a".to_owned(), Some("x".to_owned())),
                ("public.b".to_owned(), Some("y".to_owned())),
                ("c".to_owned(), None),
            ]
        );
        assert_eq!(
            referenced_tables("update users set name = 'x' where id = 1"),
            [("users".to_owned(), None)]
        );
    }
}
