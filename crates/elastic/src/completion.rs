//! Autocomplete for `.esql` without a language server: data streams after `FROM`, the fields of
//! the query's sources (read once from `_field_caps`), and ES|QL commands and functions.

use crate::{client::Elastic, esql};
use collections::{HashMap, HashSet};
use editor::{CompletionProvider, Editor};
use gpui::{Context, Entity, Task, Window};
use language::{CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};
use std::{cell::RefCell, rc::Rc};
use util::ResultExt as _;

const COMMANDS: &[&str] = &[
    "FROM",
    "WHERE",
    "EVAL",
    "STATS",
    "BY",
    "SORT",
    "LIMIT",
    "KEEP",
    "DROP",
    "RENAME",
    "DISSECT",
    "GROK",
    "MV_EXPAND",
    "ENRICH",
    "LOOKUP JOIN",
    "METADATA",
    "ASC",
    "DESC",
    "NULLS FIRST",
    "NULLS LAST",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS NULL",
    "IS NOT NULL",
    "LIKE",
    "RLIKE",
    "true",
    "false",
];

const FUNCTIONS: &[&str] = &[
    "COUNT(*)",
    "COUNT_DISTINCT()",
    "AVG()",
    "SUM()",
    "MIN()",
    "MAX()",
    "MEDIAN()",
    "PERCENTILE(, 95)",
    "VALUES()",
    "TOP(, 10, \"desc\")",
    "BUCKET(@timestamp, 1 minute)",
    "DATE_TRUNC(1 hour, @timestamp)",
    "DATE_FORMAT(\"HH:mm:ss\", @timestamp)",
    "NOW()",
    "TO_STRING()",
    "TO_LONG()",
    "LENGTH()",
    "SUBSTRING(, 0, 100)",
    "CONCAT()",
    "COALESCE()",
    "CASE()",
    "MV_COUNT()",
    "STARTS_WITH()",
    "ENDS_WITH()",
];

#[derive(Default)]
pub struct FieldCache {
    streams: Vec<String>,
    fields: HashMap<String, Vec<(String, String)>>,
    loading: HashSet<String>,
}

impl FieldCache {
    pub fn new(streams: Vec<String>) -> Self {
        Self {
            streams,
            ..Self::default()
        }
    }
}

struct Suggestion {
    text: String,
    label: String,
    detail: Option<String>,
}

pub struct EsqlCompletionProvider {
    pub cache: Rc<RefCell<FieldCache>>,
    pub elastic: Elastic,
}

impl EsqlCompletionProvider {
    /// Starts reading the fields of `sources` that aren't cached yet; the next completion has them.
    fn load_fields(&self, sources: &[String], cx: &mut Context<Editor>) {
        let pattern = sources.join(",");
        {
            let mut cache = self.cache.borrow_mut();
            if pattern.is_empty()
                || cache.fields.contains_key(&pattern)
                || !cache.loading.insert(pattern.clone())
            {
                return;
            }
        }
        let cache = self.cache.clone();
        let elastic = self.elastic.clone();
        cx.spawn(async move |_, _| {
            let fields = elastic.fields(&pattern).await.log_err().unwrap_or_default();
            let mut cache = cache.borrow_mut();
            cache.loading.remove(&pattern);
            cache.fields.insert(
                pattern,
                fields
                    .into_iter()
                    .map(|field| (field.name, field.kind))
                    .collect(),
            );
        })
        .detach();
    }
}

fn suggestions(cache: &FieldCache, query: &str, before_cursor: &str) -> Vec<Suggestion> {
    let previous_word = before_cursor
        .trim_end_matches(|character: char| !character.is_whitespace() && character != ',')
        .trim_end_matches([' ', ','])
        .rsplit(|character: char| character.is_whitespace() || character == '|')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let in_from = previous_word == "FROM"
        || before_cursor.rsplit('|').next().is_some_and(|command| {
            let command = command.trim_start().to_ascii_uppercase();
            command.starts_with("FROM ") && !command.contains(" METADATA")
        });
    if in_from {
        return cache
            .streams
            .iter()
            .map(|stream| Suggestion {
                text: stream.clone(),
                label: stream.clone(),
                detail: Some("data stream".to_string()),
            })
            .collect();
    }
    let mut result = Vec::new();
    let pattern = esql::sources(query).join(",");
    if let Some(fields) = cache.fields.get(&pattern) {
        result.extend(fields.iter().map(|(name, kind)| Suggestion {
            text: name.clone(),
            label: format!("{name}  {kind}"),
            detail: Some(kind.clone()),
        }));
    }
    result.extend(COMMANDS.iter().map(|keyword| Suggestion {
        text: (*keyword).to_string(),
        label: (*keyword).to_string(),
        detail: None,
    }));
    result.extend(FUNCTIONS.iter().map(|function| Suggestion {
        text: (*function).to_string(),
        label: (*function).to_string(),
        detail: Some("função".to_string()),
    }));
    result
}

impl CompletionProvider for EsqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<language::Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let (offset, text) = {
            let buffer = buffer.read(cx);
            (buffer_position.to_offset(buffer), buffer.text())
        };
        let word_start = text[..offset]
            .char_indices()
            .rev()
            .take_while(|(_, character)| {
                character.is_alphanumeric() || matches!(character, '_' | '.' | '@' | '-' | '*')
            })
            .last()
            .map_or(offset, |(index, _)| index);
        let blocks = esql::blocks(&text);
        let (query, block_start) = esql::block_at(&blocks, offset)
            .filter(|block| block.range.start <= offset)
            .map(|block| (&text[block.range.clone()], block.range.start))
            .unwrap_or(("", offset));
        self.load_fields(&esql::sources(query), cx);
        let before_cursor = &text[block_start.min(word_start)..word_start];
        let suggestions = suggestions(&self.cache.borrow(), query, before_cursor);
        let replace_range = buffer.read(cx).anchor_before(word_start)..buffer_position;
        let completions = suggestions
            .into_iter()
            .take(800)
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
        text.chars().last().is_some_and(|character| {
            character.is_alphanumeric() || matches!(character, '_' | '.' | '@')
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> FieldCache {
        let mut cache = FieldCache {
            streams: vec!["logs-trix-api-prod".into(), "traces-apm-default".into()],
            ..FieldCache::default()
        };
        cache.fields.insert(
            "logs-trix-api-prod".into(),
            vec![
                ("@timestamp".into(), "date".into()),
                ("log.level".into(), "keyword".into()),
            ],
        );
        cache
    }

    fn texts(suggestions: Vec<Suggestion>) -> Vec<String> {
        suggestions
            .into_iter()
            .map(|suggestion| suggestion.text)
            .collect()
    }

    #[test]
    fn from_offers_data_streams() {
        assert_eq!(
            texts(suggestions(&cache(), "FROM ", "FROM ")),
            ["logs-trix-api-prod", "traces-apm-default"]
        );
    }

    #[test]
    fn later_commands_offer_fields_first() {
        let query = "FROM logs-trix-api-prod\n| WHERE ";
        let offered = texts(suggestions(&cache(), query, query));
        assert_eq!(&offered[..2], ["@timestamp", "log.level"]);
        assert!(offered.contains(&"STATS".to_string()));
    }
}
