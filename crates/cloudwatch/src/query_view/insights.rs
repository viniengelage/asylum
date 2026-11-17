//! The Logs Insights query language as far as the tab needs it without asking AWS: tokens for
//! highlighting, a validator for the status line, quick fixes for the usual mistakes, and the
//! rewrites behind the histogram and "Filtrar por".

use std::ops::Range;

pub const COMMANDS: [&str; 11] = [
    "fields", "filter", "stats", "sort", "limit", "parse", "dedup", "display", "unmask", "pattern",
    "diff",
];

const KEYWORDS: [&str; 11] = [
    "and", "or", "not", "like", "in", "as", "by", "asc", "desc", "is", "with",
];

/// Commands that keep one row per event, so a count by bin still means "events per bin".
const EVENT_PRESERVING: [&str; 5] = ["fields", "filter", "parse", "dedup", "unmask"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    Command,
    Keyword,
    Function,
    Field,
    Identifier,
    String,
    Regex,
    Number,
    Pipe,
    Punctuation,
    Operator,
}

impl TokenKind {
    /// The theme's syntax style for the kind, or `None` to keep the editor's text color.
    pub fn style_name(self) -> Option<&'static str> {
        Some(match self {
            TokenKind::Comment => "comment",
            TokenKind::Command | TokenKind::Keyword => "keyword",
            TokenKind::Function => "function",
            TokenKind::Field => "type",
            TokenKind::String => "string",
            TokenKind::Regex => "string.regex",
            TokenKind::Number => "number",
            TokenKind::Pipe | TokenKind::Punctuation => "punctuation",
            TokenKind::Identifier | TokenKind::Operator => return None,
        })
    }

    pub const HIGHLIGHTED: [TokenKind; 9] = [
        TokenKind::Comment,
        TokenKind::Command,
        TokenKind::Keyword,
        TokenKind::Function,
        TokenKind::Field,
        TokenKind::String,
        TokenKind::Regex,
        TokenKind::Number,
        TokenKind::Pipe,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    UnexpectedSymbol(String),
    UnterminatedString,
    UnterminatedRegex,
    UnbalancedParenthesis,
    EmptyCommand,
    MissingArgument(String),
    InvalidLimit(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub problem: Problem,
    pub range: Range<usize>,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub command: Option<String>,
    /// From the command to the end of the stage, without the pipe.
    pub range: Range<usize>,
    pub command_range: Option<Range<usize>>,
}

fn is_word_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == '@' || character == '$'
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '.' | '@' | '$' | '-')
}

/// A `/` opens a regex after these; anywhere else it divides.
fn regex_can_follow(previous: Option<(&Token, &str)>) -> bool {
    match previous {
        None => true,
        Some((token, text)) => match token.kind {
            TokenKind::Keyword | TokenKind::Command => true,
            TokenKind::Punctuation | TokenKind::Operator | TokenKind::Pipe => text != ")",
            _ => false,
        },
    }
}

pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut expect_command = true;
    let mut chars = text.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        let previous = tokens
            .iter()
            .rev()
            .find(|token| token.kind != TokenKind::Comment)
            .map(|token| (token, &text[token.range.clone()]));
        let kind;
        let mut end = start + character.len_utf8();
        match character {
            '#' => {
                while let Some(&(index, next)) = chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    end = index + next.len_utf8();
                    chars.next();
                }
                kind = TokenKind::Comment;
            }
            '"' | '\'' | '`' => {
                let quote = character;
                let mut escaped = false;
                while let Some((index, next)) = chars.next() {
                    end = index + next.len_utf8();
                    if escaped {
                        escaped = false;
                    } else if next == '\\' {
                        escaped = true;
                    } else if next == quote || next == '\n' {
                        break;
                    }
                }
                kind = TokenKind::String;
            }
            '/' if regex_can_follow(previous) => {
                let mut escaped = false;
                while let Some((index, next)) = chars.next() {
                    end = index + next.len_utf8();
                    if escaped {
                        escaped = false;
                    } else if next == '\\' {
                        escaped = true;
                    } else if next == '/' || next == '\n' {
                        break;
                    }
                }
                kind = TokenKind::Regex;
            }
            '|' => {
                kind = TokenKind::Pipe;
            }
            ',' | '(' | ')' | '[' | ']' => {
                kind = TokenKind::Punctuation;
            }
            character if character.is_ascii_digit() => {
                while let Some(&(index, next)) = chars.peek() {
                    if !(next.is_ascii_alphanumeric() || next == '.') {
                        break;
                    }
                    end = index + next.len_utf8();
                    chars.next();
                }
                kind = TokenKind::Number;
            }
            character if is_word_start(character) => {
                while let Some(&(index, next)) = chars.peek() {
                    if !is_word_char(next) {
                        break;
                    }
                    end = index + next.len_utf8();
                    chars.next();
                }
                let word = &text[start..end];
                let next_is_paren = text[end..].trim_start().starts_with('(');
                kind = if expect_command {
                    TokenKind::Command
                } else if word.starts_with('@') {
                    TokenKind::Field
                } else if next_is_paren {
                    TokenKind::Function
                } else if KEYWORDS.contains(&word.to_ascii_lowercase().as_str()) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Identifier
                };
            }
            _ => {
                while let Some(&(index, next)) = chars.peek() {
                    if !matches!(next, '=' | '!' | '<' | '>' | '~' | '+' | '-' | '*' | '%') {
                        break;
                    }
                    end = index + next.len_utf8();
                    chars.next();
                }
                kind = TokenKind::Operator;
            }
        }
        match kind {
            TokenKind::Comment => {}
            TokenKind::Pipe => expect_command = true,
            _ => expect_command = false,
        }
        tokens.push(Token {
            kind,
            range: start..end,
        });
    }
    tokens
}

pub fn stages(text: &str, tokens: &[Token]) -> Vec<Stage> {
    let mut stages = Vec::new();
    let mut current: Option<Stage> = None;
    let mut stage_start = 0;
    for token in tokens {
        match token.kind {
            TokenKind::Comment => {}
            TokenKind::Pipe => {
                stages.push(current.take().unwrap_or(Stage {
                    command: None,
                    range: stage_start..token.range.start,
                    command_range: None,
                }));
                stage_start = token.range.end;
            }
            _ => match &mut current {
                Some(stage) => stage.range.end = token.range.end,
                None => {
                    current = Some(Stage {
                        command: (token.kind == TokenKind::Command)
                            .then(|| text[token.range.clone()].to_ascii_lowercase()),
                        range: token.range.clone(),
                        command_range: (token.kind == TokenKind::Command)
                            .then(|| token.range.clone()),
                    });
                }
            },
        }
    }
    if let Some(stage) = current {
        stages.push(stage);
    } else if tokens.iter().any(|token| token.kind == TokenKind::Pipe) {
        stages.push(Stage {
            command: None,
            range: stage_start..text.len(),
            command_range: None,
        });
    }
    stages
}

/// 1-based line and column (in characters) of a byte offset.
pub fn position(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// Byte offset of a 1-based line and column, as AWS reports them.
pub fn offset_of(text: &str, line: usize, column: usize) -> Option<usize> {
    let line_start = if line <= 1 {
        0
    } else {
        text.match_indices('\n').nth(line - 2)?.0 + 1
    };
    let line_text = text[line_start..].split('\n').next().unwrap_or_default();
    let column_offset = line_text
        .char_indices()
        .nth(column.saturating_sub(1))
        .map_or(line_text.len(), |(index, _)| index);
    Some(line_start + column_offset)
}

fn diagnostic(text: &str, problem: Problem, range: Range<usize>) -> Diagnostic {
    let (line, column) = position(text, range.start);
    Diagnostic {
        problem,
        range,
        line,
        column,
    }
}

/// The first problem in the query, or `None` when it reads as valid Logs Insights.
pub fn validate(text: &str) -> Option<Diagnostic> {
    let tokens = tokenize(text);
    for token in &tokens {
        let token_text = &text[token.range.clone()];
        let unterminated = match token.kind {
            TokenKind::String => {
                let quote = token_text.chars().next();
                token_text.chars().count() < 2 || token_text.chars().last() != quote
            }
            TokenKind::Regex => token_text.chars().count() < 2 || !token_text.ends_with('/'),
            _ => false,
        };
        if unterminated {
            let problem = if token.kind == TokenKind::String {
                Problem::UnterminatedString
            } else {
                Problem::UnterminatedRegex
            };
            return Some(diagnostic(text, problem, token.range.clone()));
        }
    }
    let stages = stages(text, &tokens);
    for (index, stage) in stages.iter().enumerate() {
        let Some(command) = &stage.command else {
            if index == 0 && stage.range.is_empty() {
                continue;
            }
            let range = tokens
                .iter()
                .find(|token| token.range.start >= stage.range.start)
                .map_or(stage.range.clone(), |token| token.range.clone());
            let problem = if stage.range.is_empty() || text[stage.range.clone()].trim().is_empty() {
                Problem::EmptyCommand
            } else {
                Problem::UnexpectedSymbol(text[range.clone()].to_string())
            };
            return Some(diagnostic(text, problem, range));
        };
        let command_range = stage.command_range.clone().unwrap_or(stage.range.clone());
        if !COMMANDS.contains(&command.as_str()) {
            return Some(diagnostic(
                text,
                Problem::UnexpectedSymbol(text[command_range.clone()].to_string()),
                command_range,
            ));
        }
        let arguments: Vec<&Token> = tokens
            .iter()
            .filter(|token| {
                token.kind != TokenKind::Comment
                    && token.range.start >= command_range.end
                    && token.range.end <= stage.range.end
            })
            .collect();
        if arguments.is_empty() {
            return Some(diagnostic(
                text,
                Problem::MissingArgument(command.clone()),
                command_range,
            ));
        }
        if command == "limit" {
            let argument = &text[arguments[0].range.clone()];
            let valid = arguments.len() == 1
                && argument
                    .parse::<u32>()
                    .is_ok_and(|limit| (1..=10_000).contains(&limit));
            if !valid {
                return Some(diagnostic(
                    text,
                    Problem::InvalidLimit(argument.to_string()),
                    arguments[0].range.clone(),
                ));
            }
        }
        let mut depth = 0i32;
        for token in &arguments {
            match &text[token.range.clone()] {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return Some(diagnostic(
                    text,
                    Problem::UnbalancedParenthesis,
                    token.range.clone(),
                ));
            }
        }
        if depth != 0 {
            let last = arguments
                .last()
                .map_or(command_range.clone(), |token| token.range.clone());
            return Some(diagnostic(text, Problem::UnbalancedParenthesis, last));
        }
    }
    None
}

pub fn has_stats(text: &str) -> bool {
    let tokens = tokenize(text);
    stages(text, &tokens)
        .iter()
        .any(|stage| matches!(stage.command.as_deref(), Some("stats" | "pattern" | "diff")))
}

/// Field names the query reads, without the `@` fields every group has and without the names
/// it creates with `as`.
pub fn referenced_fields(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut created = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Identifier {
            continue;
        }
        let name = &text[token.range.clone()];
        let after_as = index
            .checked_sub(1)
            .and_then(|previous| tokens.get(previous))
            .is_some_and(|previous| text[previous.range.clone()].eq_ignore_ascii_case("as"));
        if after_as {
            created.push(name.to_string());
        } else if !fields.iter().any(|field| field == name) {
            fields.push(name.to_string());
        }
    }
    fields.retain(|field| !created.contains(field));
    fields
}

/// The query's filters counted per bin, for the histogram above the events. `None` when the
/// query already aggregates.
pub fn histogram_query(text: &str, bin: &str) -> Option<String> {
    let tokens = tokenize(text);
    let stages = stages(text, &tokens);
    let mut kept = Vec::new();
    for stage in &stages {
        match stage.command.as_deref() {
            Some(command) if EVENT_PRESERVING.contains(&command) => {
                kept.push(strip_comments(
                    &text[stage.range.clone()],
                    &tokens,
                    stage.range.start,
                ));
            }
            Some("sort" | "limit" | "display") => {}
            _ => return None,
        }
    }
    kept.push(format!("stats count(*) as events by bin({bin})"));
    Some(kept.join("\n| "))
}

fn strip_comments(stage_text: &str, tokens: &[Token], offset: usize) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    for token in tokens
        .iter()
        .filter(|token| token.kind == TokenKind::Comment)
    {
        let Some(start) = token.range.start.checked_sub(offset) else {
            continue;
        };
        if start >= stage_text.len() {
            break;
        }
        let end = (token.range.end - offset).min(stage_text.len());
        result.push_str(&stage_text[cursor..start]);
        cursor = end;
    }
    result.push_str(&stage_text[cursor..]);
    result
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bin that gives about 30 bars over the window, rounded to one Insights accepts.
pub fn bin_for_window(seconds: u64) -> (u64, String) {
    const BINS: [(u64, &str); 16] = [
        (5, "5s"),
        (10, "10s"),
        (15, "15s"),
        (30, "30s"),
        (60, "1m"),
        (120, "2m"),
        (300, "5m"),
        (600, "10m"),
        (900, "15m"),
        (1800, "30m"),
        (3600, "1h"),
        (7200, "2h"),
        (10_800, "3h"),
        (21_600, "6h"),
        (43_200, "12h"),
        (86_400, "1d"),
    ];
    let wanted = seconds / 30;
    BINS.iter()
        .find(|(bin, _)| *bin >= wanted)
        .or(BINS.last())
        .map(|(bin, name)| (*bin, name.to_string()))
        .unwrap_or((60, "1m".to_string()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixRule {
    Where,
    Select,
    OrderBy,
    Head,
    Distinct,
}

impl FixRule {
    pub fn replacement(self) -> &'static str {
        match self {
            FixRule::Where => "filter",
            FixRule::Select => "fields",
            FixRule::OrderBy => "sort",
            FixRule::Head => "limit",
            FixRule::Distinct => "dedup",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineFix {
    /// 1-based.
    pub line: usize,
    pub old_line: String,
    pub new_line: String,
    /// The word AWS stumbled on (`where`).
    pub symbol: String,
    pub rule: FixRule,
}

/// The fix for a command borrowed from SQL or KQL, from the line the error points at.
pub fn local_fix(text: &str, line: usize) -> Option<LineFix> {
    let old_line = text.lines().nth(line.checked_sub(1)?)?.to_string();
    let tokens = tokenize(&old_line);
    let command = tokens
        .iter()
        .find(|token| token.kind == TokenKind::Command)?;
    let symbol = old_line[command.range.clone()].to_string();
    let rule = match symbol.to_ascii_lowercase().as_str() {
        "where" => FixRule::Where,
        "select" | "project" => FixRule::Select,
        "order" => FixRule::OrderBy,
        "head" | "top" | "take" => FixRule::Head,
        "distinct" => FixRule::Distinct,
        _ => return None,
    };
    let mut rest = &old_line[command.range.end..];
    if rule == FixRule::OrderBy {
        let trimmed = rest.trim_start();
        if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("by ") {
            rest = &trimmed[2..];
        }
    }
    let new_line = format!(
        "{}{}{}",
        &old_line[..command.range.start],
        rule.replacement(),
        rest
    );
    Some(LineFix {
        line,
        old_line,
        new_line,
        symbol,
        rule,
    })
}

/// `unexpected symbol: where at line 3, column 3` → `(3, 3)`.
pub fn error_position(message: &str) -> Option<(usize, usize)> {
    let lower = message.to_ascii_lowercase();
    let after_line = &lower[lower.find("line ")? + 5..];
    let line: usize = after_line
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let column = after_line
        .find("column ")
        .and_then(|index| {
            after_line[index + 7..]
                .split(|character: char| !character.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(1);
    Some((line, column))
}

/// The word AWS names in `unexpected symbol: where`.
pub fn error_symbol(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let start = lower.find("unexpected symbol:")? + "unexpected symbol:".len();
    message
        .get(start..)?
        .split_whitespace()
        .next()
        .map(|symbol| {
            symbol
                .trim_matches(|c: char| c == '\'' || c == '"')
                .to_string()
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Removed,
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// 1-based line number in the text the line belongs to (the new text for added lines).
    pub number: usize,
    pub text: String,
}

/// A line diff by longest common subsequence; queries are a handful of lines.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let (rows, columns) = (old_lines.len(), new_lines.len());
    let mut lengths = vec![vec![0usize; columns + 1]; rows + 1];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            lengths[row][column] = if old_lines[row] == new_lines[column] {
                lengths[row + 1][column + 1] + 1
            } else {
                lengths[row + 1][column].max(lengths[row][column + 1])
            };
        }
    }
    let mut diff = Vec::new();
    let (mut row, mut column) = (0, 0);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let flush =
        |diff: &mut Vec<DiffLine>, removed: &mut Vec<DiffLine>, added: &mut Vec<DiffLine>| {
            diff.append(removed);
            diff.append(added);
        };
    while row < rows || column < columns {
        if row < rows && column < columns && old_lines[row] == new_lines[column] {
            flush(&mut diff, &mut removed, &mut added);
            diff.push(DiffLine {
                kind: DiffKind::Same,
                number: column + 1,
                text: new_lines[column].to_string(),
            });
            row += 1;
            column += 1;
        } else if column < columns
            && (row == rows || lengths[row][column + 1] >= lengths[row + 1][column])
        {
            added.push(DiffLine {
                kind: DiffKind::Added,
                number: column + 1,
                text: new_lines[column].to_string(),
            });
            column += 1;
        } else {
            removed.push(DiffLine {
                kind: DiffKind::Removed,
                number: row + 1,
                text: old_lines[row].to_string(),
            });
            row += 1;
        }
    }
    flush(&mut diff, &mut removed, &mut added);
    diff
}

/// The smallest replacement that turns `old` into `new`, so accepting a proposal is one edit
/// that keeps the cursor and undo history around the untouched text.
pub fn minimal_edit(old: &str, new: &str) -> (Range<usize>, String) {
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .find(|((_, old_char), new_char)| old_char != new_char)
        .map_or(old.len().min(new.len()), |((index, _), _)| index);
    let prefix = (0..=prefix)
        .rev()
        .find(|index| old.is_char_boundary(*index) && new.is_char_boundary(*index))
        .unwrap_or(0);
    let old_rest = &old[prefix..];
    let new_rest = &new[prefix..];
    let suffix = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(old_char, new_char)| old_char == new_char)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    (
        prefix..old.len() - suffix,
        new_rest[..new_rest.len() - suffix].to_string(),
    )
}

/// The query with `| filter <field> = "<value>"` before its sort and limit, so the filter
/// applies to every event and not to the page that is left.
pub fn insert_filter(text: &str, field: &str, value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    let field = if field
        .chars()
        .all(|character| is_word_char(character) && character != '-')
    {
        field.to_string()
    } else {
        format!("`{field}`")
    };
    let filter = format!("| filter {field} = \"{escaped}\"");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let tail = lines.iter().position(|line| {
        let trimmed = line.trim_start().trim_start_matches('|').trim_start();
        let command = trimmed.split_whitespace().next().unwrap_or_default();
        matches!(command, "sort" | "limit" | "display" | "stats")
    });
    match tail {
        Some(index) if index > 0 => lines.insert(index, filter),
        _ => {
            let last_code = lines
                .iter()
                .rposition(|line| {
                    let trimmed = line.trim();
                    !trimmed.is_empty() && !trimmed.starts_with('#')
                })
                .map_or(lines.len(), |index| index + 1);
            lines.insert(last_code, filter);
        }
    }
    let mut result = lines.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// The console's Logs Insights page with the query, the groups and the window filled in. The
/// console reads its state from a hash in its own escaping: `*` for `%` inside values and `$`
/// for `%` over the whole hash.
pub fn console_url(region: &str, groups: &[String], query: &str, start: i64, end: i64) -> String {
    fn escape(value: &str, safe: &str) -> String {
        let mut result = String::new();
        for byte in value.bytes() {
            let character = byte as char;
            if character.is_ascii_alphanumeric() || safe.contains(character) {
                result.push(character);
            } else {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
        result
    }
    let value = |text: &str| format!("'{}", escape(text, "-_.").replace('%', "*"));
    let sources = groups
        .iter()
        .map(|group| format!("~{}", value(group)))
        .collect::<String>();
    let detail = format!(
        "~(end~{end}~start~{start}~timeType~'ABSOLUTE~tz~'Local~editorString~{}~source~({sources}))",
        value(query)
    );
    let hash = escape(&format!("?queryDetail={detail}"), "-_.~()'*!").replace('%', "$");
    format!(
        "https://{region}.console.aws.amazon.com/cloudwatch/home?region={region}#logsV2:logs-insights{hash}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD_QUERY: &str = "# erros de pagamento dos últimos 30 min, com o requestId\n\
        fields @timestamp, @logStream, level, msg, requestId, userId, err.stack\n\
        | filter level = \"error\" and msg like /pagamento|checkout/\n\
        | sort @timestamp desc\n\
        | limit 200\n\
        \n\
        # | stats count(*) as erros by bin(1m)\n";

    fn kinds(text: &str) -> Vec<(TokenKind, &str)> {
        tokenize(text)
            .into_iter()
            .map(|token| (token.kind, &text[token.range]))
            .collect()
    }

    #[test]
    fn tokens_follow_the_board_colors() {
        let line = "| filter level = \"error\" and msg like /pagamento|checkout/";
        assert_eq!(
            kinds(line),
            [
                (TokenKind::Pipe, "|"),
                (TokenKind::Command, "filter"),
                (TokenKind::Identifier, "level"),
                (TokenKind::Operator, "="),
                (TokenKind::String, "\"error\""),
                (TokenKind::Keyword, "and"),
                (TokenKind::Identifier, "msg"),
                (TokenKind::Keyword, "like"),
                (TokenKind::Regex, "/pagamento|checkout/"),
            ]
        );
        assert_eq!(
            kinds("stats count(*) as erros by bin(1m)")[..3],
            [
                (TokenKind::Command, "stats"),
                (TokenKind::Function, "count"),
                (TokenKind::Punctuation, "("),
            ]
        );
        assert_eq!(kinds("# | stats")[0], (TokenKind::Comment, "# | stats"));
        assert_eq!(
            kinds("sort @timestamp desc")[1],
            (TokenKind::Field, "@timestamp")
        );
    }

    #[test]
    fn validates_the_board_query_and_its_mistake() {
        assert_eq!(validate(BOARD_QUERY), None);
        let broken = BOARD_QUERY.replace("| filter", "| where");
        let diagnostic = validate(&broken).expect("where is not a command");
        assert_eq!(
            diagnostic.problem,
            Problem::UnexpectedSymbol("where".into())
        );
        assert_eq!((diagnostic.line, diagnostic.column), (3, 3));
    }

    #[test]
    fn catches_unfinished_pieces() {
        assert_eq!(
            validate("filter msg = \"abc").map(|diagnostic| diagnostic.problem),
            Some(Problem::UnterminatedString)
        );
        assert_eq!(
            validate("filter msg like /abc").map(|diagnostic| diagnostic.problem),
            Some(Problem::UnterminatedRegex)
        );
        assert_eq!(
            validate("stats count(* by bin(1m)").map(|diagnostic| diagnostic.problem),
            Some(Problem::UnbalancedParenthesis)
        );
        assert_eq!(
            validate("fields msg | | limit 5").map(|diagnostic| diagnostic.problem),
            Some(Problem::EmptyCommand)
        );
        assert_eq!(
            validate("fields msg | limit muitos").map(|diagnostic| diagnostic.problem),
            Some(Problem::InvalidLimit("muitos".into()))
        );
        assert_eq!(
            validate("fields msg | sort").map(|diagnostic| diagnostic.problem),
            Some(Problem::MissingArgument("sort".into()))
        );
        assert_eq!(
            validate("filter status >= 500 | stats count(*) by route"),
            None
        );
    }

    #[test]
    fn fixes_sql_habits_locally() {
        let broken = BOARD_QUERY.replace("| filter", "| where");
        let fix = local_fix(&broken, 3).expect("fix");
        assert_eq!(
            fix.old_line,
            "| where level = \"error\" and msg like /pagamento|checkout/"
        );
        assert_eq!(
            fix.new_line,
            "| filter level = \"error\" and msg like /pagamento|checkout/"
        );
        assert_eq!(fix.symbol, "where");
        assert_eq!(
            local_fix("fields a\n| order by a desc", 2).map(|fix| fix.new_line),
            Some("| sort a desc".to_string())
        );
        assert_eq!(local_fix("fields a\n| filter a = 1", 2), None);
    }

    #[test]
    fn reads_positions_from_aws_messages() {
        let message = "unexpected symbol: where at line 3, column 3";
        assert_eq!(error_position(message), Some((3, 3)));
        assert_eq!(error_symbol(message), Some("where".to_string()));
        let offset = offset_of(BOARD_QUERY, 3, 3).unwrap();
        assert_eq!(&BOARD_QUERY[offset..offset + 6], "filter");
        assert_eq!(position(BOARD_QUERY, offset), (3, 3));
    }

    #[test]
    fn histogram_keeps_filters_and_drops_paging() {
        assert_eq!(
            histogram_query(BOARD_QUERY, "1m").as_deref(),
            Some(
                "fields @timestamp, @logStream, level, msg, requestId, userId, err.stack\n\
                 | filter level = \"error\" and msg like /pagamento|checkout/\n\
                 | stats count(*) as events by bin(1m)"
            )
        );
        assert_eq!(histogram_query("stats count(*) by route", "1m"), None);
        assert!(!has_stats(BOARD_QUERY));
        assert!(has_stats("filter a = 1 | stats count(*)"));
        assert_eq!(bin_for_window(30 * 60), (60, "1m".to_string()));
        assert_eq!(bin_for_window(7 * 86_400).1, "6h");
    }

    #[test]
    fn lists_the_fields_a_query_reads() {
        assert_eq!(
            referenced_fields(
                "filter route like /^\\/v1\\/checkout/ and status >= 500\n\
                 | stats count(*) as erros, pct(durationMs, 95) as p95_ms by route\n\
                 | sort erros desc"
            ),
            ["route", "status", "durationMs"]
        );
    }

    #[test]
    fn diffs_by_line() {
        let diff = line_diff("a\nb\nc", "a\nx\nc");
        assert_eq!(
            diff.iter()
                .map(|line| (line.kind, line.number, line.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (DiffKind::Same, 1, "a"),
                (DiffKind::Removed, 2, "b"),
                (DiffKind::Added, 2, "x"),
                (DiffKind::Same, 3, "c"),
            ]
        );
        assert_eq!(
            minimal_edit("| where x", "| filter x"),
            (2..7, "filter".into())
        );
        assert_eq!(minimal_edit("ação", "ação!"), (6..6, "!".into()));
    }

    #[test]
    fn filters_go_before_paging() {
        assert_eq!(
            insert_filter(BOARD_QUERY, "requestId", "req_9HX1c2"),
            BOARD_QUERY.replace(
                "| sort @timestamp desc",
                "| filter requestId = \"req_9HX1c2\"\n| sort @timestamp desc"
            )
        );
        assert_eq!(
            insert_filter("fields msg", "err.code", "a\"b"),
            "fields msg\n| filter err.code = \"a\\\"b\""
        );
    }

    #[test]
    fn console_links_escape_like_the_console() {
        let url = console_url("us-east-1", &["/ecs/api".into()], "fields msg", 10, 20);
        assert!(url.starts_with(
            "https://us-east-1.console.aws.amazon.com/cloudwatch/home?region=us-east-1#logsV2:logs-insights$3FqueryDetail$3D"
        ));
        assert!(url.contains("editorString~'fields*20msg"));
        assert!(url.contains("source~(~'*2Fecs*2Fapi)"));
    }
}
