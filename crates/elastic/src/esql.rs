//! Reading ES|QL text without a parser: enough to find the query under the cursor, to derive the
//! histogram query from it and to point at the place an error names.

use std::ops::Range;

use chrono::{DateTime, Utc};

/// A query in an `.esql` file: consecutive non-blank lines, so one file can keep several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub range: Range<usize>,
    /// 0-based line of `range.start` in the file.
    pub first_line: usize,
}

pub fn blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut offset = 0;
    let mut current: Option<Block> = None;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        let content = line.trim_end_matches(['\n', '\r']);
        if content.trim().is_empty() {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
        } else {
            let end = offset + content.len();
            match &mut current {
                Some(block) => block.range.end = end,
                None => {
                    current = Some(Block {
                        range: offset..end,
                        first_line: line_index,
                    })
                }
            }
        }
        offset += line.len();
    }
    blocks.extend(current);
    // A block made only of comments is a note, not a query.
    blocks.retain(|block| !strip_comments(&text[block.range.clone()]).trim().is_empty());
    blocks
}

/// The block under `offset`, or the closest one above it when the cursor sits on a blank line.
pub fn block_at(blocks: &[Block], offset: usize) -> Option<&Block> {
    blocks
        .iter()
        .find(|block| block.range.start <= offset && offset <= block.range.end)
        .or_else(|| blocks.iter().rev().find(|block| block.range.end < offset))
        .or_else(|| blocks.first())
}

/// Removes `//` and `/* */` comments, keeping string literals and line breaks.
pub fn strip_comments(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut characters = query.chars().peekable();
    let mut in_string = false;
    while let Some(character) = characters.next() {
        if in_string {
            result.push(character);
            if character == '\\' {
                if let Some(next) = characters.next() {
                    result.push(next);
                }
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                result.push(character);
            }
            '/' if characters.peek() == Some(&'/') => {
                for next in characters.by_ref() {
                    if next == '\n' {
                        result.push('\n');
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                characters.next();
                let mut previous = ' ';
                for next in characters.by_ref() {
                    if next == '\n' {
                        result.push('\n');
                    }
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            _ => result.push(character),
        }
    }
    result
}

/// The commands of a query (`FROM x`, `WHERE …`), split on the pipes outside strings.
pub fn commands(query: &str) -> Vec<String> {
    let query = strip_comments(query);
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in query.chars() {
        if in_string {
            current.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                current.push(character);
            }
            '|' => commands.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    commands.push(current);
    commands
        .into_iter()
        .map(|command| command.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|command| !command.is_empty())
        .collect()
}

fn keyword(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// The index patterns after `FROM`, without `METADATA`.
pub fn sources(query: &str) -> Vec<String> {
    let commands = commands(query);
    let Some(from) = commands
        .first()
        .filter(|command| keyword(command) == "FROM")
    else {
        return Vec::new();
    };
    let rest = from[4..].trim();
    let rest = match rest.to_ascii_uppercase().find(" METADATA ") {
        Some(index) => &rest[..index],
        None => rest,
    };
    rest.split(',')
        .map(|source| source.trim().trim_matches('"').to_string())
        .filter(|source| !source.is_empty())
        .collect()
}

pub fn aggregates(query: &str) -> bool {
    commands(query)
        .iter()
        .any(|command| matches!(keyword(command).as_str(), "STATS" | "INLINESTATS"))
}

/// The count per time bucket of what the query filters, for the bars above the results. Only
/// for queries that read documents: once they aggregate there is no timeline to draw.
pub fn histogram_query(
    query: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    buckets: usize,
) -> Option<String> {
    let commands = commands(query);
    if keyword(commands.first()?) != "FROM" || aggregates(query) {
        return None;
    }
    // Ordering, limits and column choices don't change which documents match, and KEEP or DROP
    // could remove @timestamp.
    let kept: Vec<&String> = commands
        .iter()
        .filter(|command| {
            !matches!(
                keyword(command).as_str(),
                "SORT" | "LIMIT" | "KEEP" | "DROP" | "RENAME"
            )
        })
        .collect();
    let mut histogram = kept
        .iter()
        .map(|command| command.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    histogram.push_str(&format!(
        " | STATS docs = COUNT(*) BY bucket = BUCKET(@timestamp, {buckets}, \"{}\", \"{}\") | SORT bucket",
        iso(from),
        iso(to)
    ));
    Some(histogram)
}

pub fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// What an ES|QL error points at: `line 3:9: Unknown column [level], did you mean [log.level]?`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorLocation {
    /// 1-based, relative to the query sent.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    pub message: String,
    /// The unknown name and the one Elasticsearch suggests instead.
    pub replacement: Option<(String, String)>,
}

pub fn error_location(message: &str) -> Option<ErrorLocation> {
    let start = message.find("line ")?;
    let rest = &message[start + "line ".len()..];
    let colon = rest.find(':')?;
    let line: usize = rest[..colon].parse().ok()?;
    let rest = &rest[colon + 1..];
    let colon = rest.find(':')?;
    let column: usize = rest[..colon].parse().ok()?;
    let detail = rest[colon + 1..].trim();
    let detail = detail.lines().next().unwrap_or(detail).trim().to_string();
    let replacement = bracketed(&detail, "Unknown column [").and_then(|unknown| {
        let suggestion = bracketed(&detail, "did you mean [")
            .or_else(|| bracketed(&detail, "did you mean any of ["))?;
        let suggestion = suggestion.split(',').next()?.trim().to_string();
        Some((unknown, suggestion))
    });
    Some(ErrorLocation {
        line,
        column,
        message: detail,
        replacement,
    })
}

fn bracketed(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)? + prefix.len();
    let end = text[start..].find(']')? + start;
    Some(text[start..end].to_string())
}

/// The byte offset of a 1-based line and character column inside `query`.
pub fn offset_of(query: &str, line: usize, column: usize) -> Option<usize> {
    let mut offset = 0;
    for (index, text) in query.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            let within = text
                .char_indices()
                .nth(column.saturating_sub(1))
                .map_or(text.trim_end_matches('\n').len(), |(byte, _)| byte);
            return Some(offset + within);
        }
        offset += text.len();
    }
    None
}

/// Quotes a value as an ES|QL string literal.
pub fn string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn blocks_split_on_blank_lines_and_skip_notes() {
        let text = "// erros\nFROM logs-*\n| LIMIT 5\n\n// só comentário\n\nFROM traces-apm*\n";
        let blocks = blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            &text[blocks[0].range.clone()],
            "// erros\nFROM logs-*\n| LIMIT 5"
        );
        assert_eq!(blocks[0].first_line, 0);
        assert_eq!(&text[blocks[1].range.clone()], "FROM traces-apm*");
        assert_eq!(blocks[1].first_line, 6);
        let cursor = text.find("traces").unwrap_or_default();
        assert_eq!(block_at(&blocks, cursor), Some(&blocks[1]));
    }

    #[test]
    fn commands_ignore_pipes_in_strings_and_comments() {
        let query = "FROM logs-* // | STATS\n| WHERE message RLIKE \".*(a|b).*\"\n| LIMIT 10";
        assert_eq!(
            commands(query),
            vec![
                "FROM logs-*",
                "WHERE message RLIKE \".*(a|b).*\"",
                "LIMIT 10"
            ]
        );
        assert_eq!(sources("FROM a, b METADATA _id | LIMIT 1"), vec!["a", "b"]);
        assert!(!aggregates(query));
    }

    #[test]
    fn histogram_drops_order_and_columns() {
        let from = Utc.with_ymd_and_hms(2026, 9, 30, 13, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 9, 30, 13, 30, 0).unwrap();
        let histogram = histogram_query(
            "FROM logs-*\n| WHERE log.level == \"error\"\n| KEEP message\n| SORT @timestamp DESC\n| LIMIT 200",
            from,
            to,
            30,
        );
        assert_eq!(
            histogram.as_deref(),
            Some(
                "FROM logs-* | WHERE log.level == \"error\" | STATS docs = COUNT(*) BY bucket = \
                 BUCKET(@timestamp, 30, \"2026-09-30T13:00:00.000Z\", \"2026-09-30T13:30:00.000Z\") \
                 | SORT bucket"
            )
        );
        assert_eq!(
            histogram_query("FROM logs-* | STATS c = COUNT(*)", from, to, 30),
            None
        );
    }

    #[test]
    fn error_location_reads_the_suggestion() {
        let location = error_location(
            "Found 1 problem\nline 3:9: Unknown column [level], did you mean [log.level]?",
        )
        .expect("location");
        assert_eq!((location.line, location.column), (3, 9));
        assert_eq!(
            location.replacement,
            Some(("level".to_string(), "log.level".to_string()))
        );
        let query = "// x\nFROM logs-*\n| WHERE level == 1";
        let offset = offset_of(query, 3, 9).expect("offset");
        assert_eq!(&query[offset..offset + 5], "level");
    }
}
