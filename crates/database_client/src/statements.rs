//! Splits SQL into statements the way Postgres reads it (quotes, `E''` escapes, dollar quoting,
//! nested block comments) and says what each one does, so the editor can run the statement
//! under the cursor and ask before a risky write.

use crate::connection::{Environment, SavedConnection};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transaction {
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    /// SELECT, WITH … SELECT, VALUES, TABLE, SHOW, EXPLAIN without ANALYZE of a write.
    Read,
    /// INSERT, UPDATE, DELETE, MERGE, COPY, TRUNCATE, or a WITH that writes.
    Write {
        /// An UPDATE or DELETE without a WHERE, or a TRUNCATE: every row goes.
        every_row: bool,
    },
    /// CREATE, ALTER, DROP, GRANT, REVOKE, COMMENT, REINDEX, CLUSTER.
    Ddl,
    Transaction(Transaction),
    Other,
}

impl StatementKind {
    /// Whether the statement can run inside a cursor, which keeps a row limit from aborting an
    /// open transaction.
    pub fn is_cursor_safe(self, first_keyword: Option<&str>) -> bool {
        self == Self::Read && matches!(first_keyword, Some("select" | "with" | "values" | "table"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Risk {
    /// UPDATE or DELETE without WHERE, or TRUNCATE.
    EveryRow,
    /// Schema change.
    Ddl,
    /// Any write on a production connection.
    ProductionWrite,
}

/// Why this statement needs a confirmation on this connection, if it does.
pub fn risk(kind: StatementKind, connection: &SavedConnection) -> Option<Risk> {
    let production = connection.environment == Environment::Prod;
    let asks = connection.confirm_writes || production;
    match kind {
        StatementKind::Write { every_row: true } if asks => Some(Risk::EveryRow),
        StatementKind::Ddl if asks => Some(Risk::Ddl),
        StatementKind::Write { .. } if production => Some(Risk::ProductionWrite),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// Byte range in the source, without surrounding whitespace or the trailing `;`.
    pub range: Range<usize>,
    pub kind: StatementKind,
    pub first_keyword: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token {
    Word,
    Semicolon,
    OpenParen,
    CloseParen,
    Other,
}

/// Walks the source outside quotes and comments, calling `visit` with every word, parenthesis
/// and semicolon and its byte range.
fn scan(source: &str, mut visit: impl FnMut(Token, Range<usize>)) {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let mut depth = 1;
                index += 2;
                while index < bytes.len() && depth > 0 {
                    if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                        depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            }
            b'\'' => {
                let start = index;
                let escapes = index > 0
                    && matches!(bytes[index - 1], b'e' | b'E')
                    && (index < 2 || !is_word_byte(bytes[index - 2]));
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' if escapes => index += 2,
                        b'\'' if bytes.get(index + 1) == Some(&b'\'') => index += 2,
                        b'\'' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                visit(Token::Other, start..index.min(bytes.len()));
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'"' {
                        if bytes.get(index + 1) == Some(&b'"') {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                visit(Token::Word, start..index);
            }
            b'$' if index == 0 || !is_word_byte(bytes[index - 1]) => {
                // `$tag$ … $tag$`; `$1` is a parameter, not a quote.
                let tag_end = bytes[index + 1..]
                    .iter()
                    .position(|byte| !is_word_byte(*byte))
                    .map(|offset| index + 1 + offset);
                let is_quote = tag_end.is_some_and(|end| {
                    bytes[end] == b'$' && !bytes[index + 1..end].first().is_some_and(u8::is_ascii_digit)
                });
                match tag_end {
                    Some(end) if is_quote => {
                        let start = index;
                        let tag = &source[index..=end];
                        match source[end + 1..].find(tag) {
                            Some(offset) => index = end + 1 + offset + tag.len(),
                            None => index = bytes.len(),
                        }
                        visit(Token::Other, start..index);
                    }
                    _ => index += 1,
                }
            }
            b';' => {
                visit(Token::Semicolon, index..index + 1);
                index += 1;
            }
            b'(' => {
                visit(Token::OpenParen, index..index + 1);
                index += 1;
            }
            b')' => {
                visit(Token::CloseParen, index..index + 1);
                index += 1;
            }
            byte if is_word_byte(byte) => {
                let start = index;
                while index < bytes.len() && is_word_byte(bytes[index]) {
                    index += 1;
                }
                visit(Token::Word, start..index);
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                let start = index;
                index += source[index..].chars().next().map_or(1, char::len_utf8);
                visit(Token::Other, start..index);
            }
        }
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// Every statement in the source, skipping ones that are only whitespace or comments.
pub fn split(source: &str) -> Vec<Statement> {
    let mut statements = Vec::new();
    let mut current: Option<Range<usize>> = None;
    let finish = |range: Option<Range<usize>>, statements: &mut Vec<Statement>| {
        if let Some(range) = range {
            let text = &source[range.clone()];
            let (kind, first_keyword) = classify(text);
            statements.push(Statement {
                range,
                kind,
                first_keyword,
            });
        }
    };
    let mut tokens = Vec::new();
    scan(source, |token, range| tokens.push((token, range)));
    for (token, range) in tokens {
        if token == Token::Semicolon {
            finish(current.take(), &mut statements);
            continue;
        }
        current = Some(match current {
            Some(existing) => existing.start..range.end,
            None => range,
        });
    }
    finish(current.take(), &mut statements);
    statements
}

/// The statement the cursor is in, or the one right before it when the cursor sits in the
/// blank space after a statement.
pub fn statement_at(statements: &[Statement], offset: usize) -> Option<&Statement> {
    statements
        .iter()
        .find(|statement| statement.range.start <= offset && offset <= statement.range.end)
        .or_else(|| {
            statements
                .iter()
                .rev()
                .find(|statement| statement.range.end <= offset)
        })
        .or_else(|| statements.first())
}

/// What the statement does, from its top-level keywords.
pub fn classify(text: &str) -> (StatementKind, Option<String>) {
    let mut words = Vec::new();
    let mut depth = 0usize;
    scan(text, |token, range| match token {
        Token::OpenParen => depth += 1,
        Token::CloseParen => depth = depth.saturating_sub(1),
        Token::Word => words.push((depth, text[range].to_ascii_lowercase())),
        Token::Semicolon | Token::Other => {}
    });
    let first = words.first().map(|(_, word)| word.clone());
    let top_level = |word: &str| words.iter().any(|(level, w)| *level == 0 && w == word);
    let anywhere = |word: &str| words.iter().any(|(_, w)| w == word);
    let kind = match first.as_deref() {
        Some("select" | "values" | "table" | "show") => StatementKind::Read,
        Some("with") => {
            if ["insert", "update", "delete", "merge"]
                .iter()
                .any(|verb| anywhere(verb))
            {
                StatementKind::Write {
                    every_row: false,
                }
            } else {
                StatementKind::Read
            }
        }
        Some("explain") => {
            let analyze = words.iter().take(4).any(|(_, word)| word == "analyze");
            let writes = ["insert", "update", "delete", "merge"]
                .iter()
                .any(|verb| anywhere(verb));
            if analyze && writes {
                StatementKind::Write {
                    every_row: false,
                }
            } else {
                StatementKind::Read
            }
        }
        Some("update" | "delete") => StatementKind::Write {
            every_row: !top_level("where"),
        },
        Some("truncate") => StatementKind::Write { every_row: true },
        Some("insert" | "merge" | "copy") => StatementKind::Write { every_row: false },
        Some(
            "create" | "alter" | "drop" | "grant" | "revoke" | "comment" | "reindex" | "cluster"
            | "refresh",
        ) => StatementKind::Ddl,
        Some("begin" | "start") => StatementKind::Transaction(Transaction::Begin),
        Some("commit" | "end") => StatementKind::Transaction(Transaction::Commit),
        Some("rollback" | "abort") => StatementKind::Transaction(Transaction::Rollback),
        _ => StatementKind::Other,
    };
    (kind, first)
}

/// Converts the 1-based character position Postgres reports into a byte offset in `text`.
pub fn char_position_to_offset(text: &str, position: u32) -> usize {
    text.char_indices()
        .nth(position.saturating_sub(1) as usize)
        .map_or(text.len(), |(offset, _)| offset)
}

/// The identifier (with dots, as in `t.status`) that starts at `offset`.
pub fn identifier_at(text: &str, offset: usize) -> Range<usize> {
    let end = text[offset..]
        .char_indices()
        .find(|(_, character)| !(character.is_alphanumeric() || *character == '_' || *character == '.'))
        .map_or(text.len(), |(index, _)| offset + index);
    offset..end.max(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(source: &str) -> Vec<&str> {
        split(source)
            .into_iter()
            .map(|statement| &source[statement.range])
            .collect()
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_and_comments() {
        let source = "-- saldo; do dia\nselect ';' as a, \"we;ird\" from t;\n\n\
                      update t set a = 'it''s; fine' where id = 1 ;\n\
                      /* comment; /* nested; */ still */ select E'\\'; x';\n\
                      create function f() returns int as $body$ select 1; $body$ language sql;\n\
                      select $1::int;  -- trailing\n";
        assert_eq!(
            texts(source),
            [
                "select ';' as a, \"we;ird\" from t",
                "update t set a = 'it''s; fine' where id = 1",
                "select E'\\'; x'",
                "create function f() returns int as $body$ select 1; $body$ language sql",
                "select $1::int",
            ]
        );
    }

    #[test]
    fn comment_only_chunks_are_skipped() {
        assert!(split("-- nothing here\n/* nor here */ ;;").is_empty());
        assert_eq!(texts("select 1"), ["select 1"]);
    }

    #[test]
    fn statement_under_the_cursor() {
        let source = "select 1;\n\nselect 2;\n";
        let statements = split(source);
        let at = |offset| &source[statement_at(&statements, offset).unwrap().range.clone()];
        assert_eq!(at(0), "select 1");
        assert_eq!(at(8), "select 1");
        // Blank line after the first statement still runs it.
        assert_eq!(at(10), "select 1");
        assert_eq!(at(13), "select 2");
        assert_eq!(at(source.len()), "select 2");
    }

    #[test]
    fn classifies_what_statements_do() {
        let kind = |text: &str| classify(text).0;
        assert_eq!(kind("select * from users"), StatementKind::Read);
        assert_eq!(kind("(select 1) union (select 2)"), StatementKind::Read);
        assert_eq!(kind("with x as (select 1) select * from x"), StatementKind::Read);
        assert_eq!(
            kind("with gone as (delete from t returning *) select count(*) from gone"),
            StatementKind::Write { every_row: false }
        );
        assert_eq!(
            kind("delete from pix_keys"),
            StatementKind::Write { every_row: true }
        );
        assert_eq!(
            kind("delete from pix_keys where id in (select id from x where y)"),
            StatementKind::Write { every_row: false }
        );
        assert_eq!(
            kind("update users set name = (select 'x' where true)"),
            StatementKind::Write { every_row: true }
        );
        assert_eq!(kind("truncate audit.log"), StatementKind::Write { every_row: true });
        assert_eq!(kind("explain select 1"), StatementKind::Read);
        assert_eq!(
            kind("explain analyze delete from t"),
            StatementKind::Write { every_row: false }
        );
        assert_eq!(kind("drop table t"), StatementKind::Ddl);
        assert_eq!(
            kind("BEGIN"),
            StatementKind::Transaction(Transaction::Begin)
        );
        assert_eq!(
            kind("rollback"),
            StatementKind::Transaction(Transaction::Rollback)
        );
        assert_eq!(kind("vacuum analyze"), StatementKind::Other);
    }

    #[test]
    fn production_asks_before_any_write() {
        let mut connection = SavedConnection {
            id: "a".into(),
            name: "trix".into(),
            environment: Environment::Dev,
            host: "localhost".into(),
            port: 5432,
            database: "trix".into(),
            user: "app".into(),
            ssl_mode: crate::tls::SslMode::Prefer,
            read_only: false,
            confirm_writes: false,
        };
        let write = StatementKind::Write { every_row: false };
        let wipe = StatementKind::Write { every_row: true };
        assert_eq!(risk(wipe, &connection), None);
        connection.confirm_writes = true;
        assert_eq!(risk(wipe, &connection), Some(Risk::EveryRow));
        assert_eq!(risk(write, &connection), None);
        assert_eq!(risk(StatementKind::Ddl, &connection), Some(Risk::Ddl));
        connection.environment = Environment::Prod;
        connection.confirm_writes = false;
        assert_eq!(risk(write, &connection), Some(Risk::ProductionWrite));
        assert_eq!(risk(StatementKind::Read, &connection), None);
    }

    #[test]
    fn error_positions_map_to_bytes() {
        let text = "select ção, nme from t";
        let offset = char_position_to_offset(text, 13);
        assert_eq!(&text[identifier_at(text, offset)], "nme");
        let text = "select t.stauts from t";
        assert_eq!(&text[identifier_at(text, char_position_to_offset(text, 8))], "t.stauts");
    }
}
