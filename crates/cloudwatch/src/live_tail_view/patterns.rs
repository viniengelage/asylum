use std::collections::{HashMap, HashSet, VecDeque};

/// A template seen this many times inside [`NEW_PATTERN_WINDOW_MS`], and absent from the
/// baseline, is announced as new.
pub(crate) const NEW_PATTERN_HITS: usize = 3;
pub(crate) const NEW_PATTERN_WINDOW_MS: i64 = 2 * 60 * 1000;
/// Bounds the memory spent on templates that never repeat (one-off messages are the norm).
const MAX_CANDIDATES: usize = 2_000;

/// The shape of a message with numbers, ids, hex and quoted values taken out, so repeats of one
/// event share it.
pub(crate) fn template(message: &str) -> String {
    let line = message.lines().next().unwrap_or_default();
    let mut template = String::with_capacity(line.len());
    let mut characters = line.char_indices().peekable();
    let mut previous: Option<char> = None;
    while let Some((start, character)) = characters.next() {
        let opens_quote = character == '"'
            || character == '`'
            || (character == '\'' && !previous.is_some_and(char::is_alphanumeric));
        if opens_quote {
            if let Some(end) = line[start + 1..].find(character) {
                template.push(character);
                template.push_str("<s>");
                template.push(character);
                let close = start + 1 + end;
                while characters.peek().is_some_and(|(index, _)| *index <= close) {
                    characters.next();
                }
                previous = Some(character);
                continue;
            }
        }
        if is_word_character(character) {
            let mut end = start + character.len_utf8();
            while let Some((index, next)) = characters.peek().copied() {
                if !is_word_character(next) {
                    break;
                }
                end = index + next.len_utf8();
                characters.next();
            }
            template.push_str(&word_template(&line[start..end]));
            previous = line[..end].chars().next_back();
            continue;
        }
        if character.is_whitespace() {
            if !template.ends_with(' ') {
                template.push(' ');
            }
        } else {
            template.push(character);
        }
        previous = Some(character);
    }
    template.trim().to_string()
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '-'
}

fn word_template(word: &str) -> String {
    let digits = word.chars().filter(char::is_ascii_digit).count();
    if digits == 0 {
        // Ids such as `req_abcdEF` carry no digits but mix case after a prefix.
        if let Some((prefix, rest)) = word.split_once('_')
            && !prefix.is_empty()
            && rest.len() >= 4
            && rest.chars().any(char::is_uppercase)
            && rest.chars().any(char::is_lowercase)
        {
            return "<id>".to_string();
        }
        return word.to_string();
    }
    if digits == word.len() {
        return "<n>".to_string();
    }
    // Names like `v1`, `s3` and `ec2` are part of the shape, not ids.
    if word.len() <= 3 && word.starts_with(char::is_alphabetic) {
        return word.to_string();
    }
    let number_end = word
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(word.len(), |(index, _)| index);
    let unit = &word[number_end..];
    if number_end > 0 && unit.len() <= 3 && unit.chars().all(char::is_alphabetic) {
        return format!("<n>{unit}");
    }
    if word.len() >= 8
        && word
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
    {
        return "<hex>".to_string();
    }
    "<id>".to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NewPattern {
    pub template: String,
    pub example: String,
    pub first_seen_ms: i64,
    pub hits: usize,
}

struct Candidate {
    hits: VecDeque<i64>,
    example: String,
}

/// Flags message templates the baseline (the last 24 h) never had once they repeat. Runs
/// locally; the model only writes the sentence of the warning.
#[derive(Default)]
pub(crate) struct PatternDetector {
    baseline: Option<HashSet<String>>,
    silenced: HashSet<String>,
    announced: HashSet<String>,
    candidates: HashMap<String, Candidate>,
}

impl PatternDetector {
    pub fn set_baseline<'a>(&mut self, messages: impl IntoIterator<Item = &'a str>) {
        self.baseline = Some(messages.into_iter().map(template).collect());
        self.candidates.clear();
    }

    pub fn is_watching(&self) -> bool {
        self.baseline.is_some()
    }

    pub fn silence(&mut self, template: &str) {
        self.silenced.insert(template.to_string());
        self.candidates.remove(template);
    }

    /// Returns the pattern the moment it crosses the threshold, once per template.
    pub fn observe(&mut self, message: &str, timestamp_ms: i64) -> Option<NewPattern> {
        let baseline = self.baseline.as_ref()?;
        if message.trim().is_empty() {
            return None;
        }
        let template = template(message);
        if baseline.contains(&template)
            || self.silenced.contains(&template)
            || self.announced.contains(&template)
        {
            return None;
        }
        if self.candidates.len() >= MAX_CANDIDATES && !self.candidates.contains_key(&template) {
            self.prune(timestamp_ms);
            if self.candidates.len() >= MAX_CANDIDATES {
                return None;
            }
        }
        let candidate = self
            .candidates
            .entry(template.clone())
            .or_insert_with(|| Candidate {
                hits: VecDeque::new(),
                example: message.lines().next().unwrap_or_default().to_string(),
            });
        candidate.hits.push_back(timestamp_ms);
        while candidate
            .hits
            .front()
            .is_some_and(|first| timestamp_ms - *first > NEW_PATTERN_WINDOW_MS)
        {
            candidate.hits.pop_front();
        }
        if candidate.hits.len() < NEW_PATTERN_HITS {
            return None;
        }
        let pattern = NewPattern {
            example: candidate.example.clone(),
            first_seen_ms: candidate.hits.front().copied().unwrap_or(timestamp_ms),
            hits: candidate.hits.len(),
            template: template.clone(),
        };
        self.candidates.remove(&template);
        self.announced.insert(template);
        Some(pattern)
    }

    fn prune(&mut self, now_ms: i64) {
        self.candidates.retain(|_, candidate| {
            candidate
                .hits
                .back()
                .is_some_and(|last| now_ms - *last <= NEW_PATTERN_WINDOW_MS)
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let text = text.trim_matches(|character: char| !character.is_alphanumeric());
        match text.to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" | "information" | "notice" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emergency" => {
                Some(Level::Error)
            }
            _ => None,
        }
    }

    /// pino and bunyan write the level as a number.
    fn from_number(number: i64) -> Option<Self> {
        match number {
            ..=10 => Some(Level::Trace),
            11..=20 => Some(Level::Debug),
            21..=30 => Some(Level::Info),
            31..=40 => Some(Level::Warn),
            _ => Some(Level::Error),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParsedMessage {
    pub level: Option<Level>,
    pub message: String,
    pub stack: Vec<String>,
}

const MAX_STACK_LINES: usize = 12;
const LEVEL_FIELDS: &[&str] = &["level", "severity", "lvl", "levelname", "log.level"];
const MESSAGE_FIELDS: &[&str] = &["msg", "message", "@message"];
const STACK_FIELDS: &[&str] = &["stack", "stack_trace", "stackTrace", "exception"];

/// Splits a raw event into level, the line shown and the stack trace under it. Structured (JSON)
/// events are read by field; plain ones by their first line and a level word near the start.
pub(crate) fn parse_message(raw: &str) -> ParsedMessage {
    let trimmed = raw.trim();
    if trimmed.starts_with('{')
        && let Ok(serde_json::Value::Object(object)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let level = LEVEL_FIELDS
            .iter()
            .find_map(|field| object.get(*field))
            .and_then(|value| match value {
                serde_json::Value::String(text) => Level::parse(text),
                serde_json::Value::Number(number) => number.as_i64().and_then(Level::from_number),
                _ => None,
            });
        let error = ["err", "error"]
            .iter()
            .find_map(|field| object.get(*field).and_then(|value| value.as_object()));
        let message = MESSAGE_FIELDS
            .iter()
            .find_map(|field| object.get(*field).and_then(|value| value.as_str()))
            .or_else(|| error.and_then(|error| error.get("message")?.as_str()))
            .map(str::to_string)
            .unwrap_or_else(|| trimmed.to_string());
        let stack = STACK_FIELDS
            .iter()
            .find_map(|field| object.get(*field).and_then(|value| value.as_str()))
            .or_else(|| {
                error.and_then(|error| {
                    STACK_FIELDS
                        .iter()
                        .find_map(|field| error.get(*field)?.as_str())
                })
            })
            .map(|stack| stack_lines(stack, true))
            .unwrap_or_default();
        let mut lines = message.lines();
        let first = lines.next().unwrap_or_default().to_string();
        let stack = if stack.is_empty() {
            stack_lines(&lines.collect::<Vec<_>>().join("\n"), false)
        } else {
            stack
        };
        return ParsedMessage {
            level,
            message: first,
            stack,
        };
    }
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default();
    let level = first
        .split(|character: char| character.is_whitespace() || character == '\t')
        .filter(|word| !word.is_empty())
        .take(6)
        .find_map(Level::parse);
    ParsedMessage {
        level,
        message: first.trim_end().to_string(),
        stack: stack_lines(&lines.collect::<Vec<_>>().join("\n"), false),
    }
}

fn stack_lines(stack: &str, repeats_message: bool) -> Vec<String> {
    let mut lines = stack
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .peekable();
    // A stack field usually repeats the message on its first line (`Error: …`).
    if repeats_message
        && lines
            .peek()
            .is_some_and(|line| !line.starts_with("at ") && !line.starts_with("File "))
        && stack.lines().count() > 1
    {
        lines.next();
    }
    lines.take(MAX_STACK_LINES).map(str::to_string).collect()
}

/// A `file:line[:column]` inside a stack frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameLink {
    pub path: String,
    /// 1-based.
    pub line: u32,
    pub column: Option<u32>,
    /// Where the link sits in the frame's text.
    pub range: std::ops::Range<usize>,
}

pub(crate) fn frame_link(frame: &str) -> Option<FrameLink> {
    // Frames inside dependencies and the runtime don't point at the project.
    if frame.contains("node_modules") || frame.contains("node:internal") {
        return None;
    }
    for token in frame.split(|character: char| {
        character.is_whitespace() || matches!(character, '(' | ')' | '"' | '\'' | ',' | '[' | ']')
    }) {
        let token = token.strip_prefix("file://").unwrap_or(token);
        // `split` yields subslices of `frame`, so the pointer difference is the byte offset.
        let token_start = token.as_ptr() as usize - frame.as_ptr() as usize;
        let token_trimmed = token.trim_end_matches(':');
        let mut parts = token_trimmed.rsplitn(3, ':');
        let Some(last) = parts.next() else {
            continue;
        };
        let Some(middle) = parts.next() else {
            continue;
        };
        let (path, line, column) = match parts.next() {
            Some(path) if middle.parse::<u32>().is_ok() && last.parse::<u32>().is_ok() => {
                (path, middle.parse::<u32>().ok()?, last.parse::<u32>().ok())
            }
            _ => match last.parse::<u32>() {
                Ok(line) => {
                    let path = token_trimmed.rsplit_once(':')?.0;
                    (path, line, None)
                }
                Err(_) => continue,
            },
        };
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let has_extension = file_name.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.chars().all(char::is_alphanumeric)
        });
        if !has_extension || path.contains("://") {
            continue;
        }
        return Some(FrameLink {
            path: path.to_string(),
            line,
            column,
            range: token_start..token_start + token_trimmed.len(),
        });
    }
    None
}

/// The log group name in a Live Tail event, which may come as the group's ARN.
pub(crate) fn group_name(identifier: &str) -> &str {
    let name = identifier
        .split_once(":log-group:")
        .map_or(identifier, |(_, name)| name);
    name.strip_suffix(":*").unwrap_or(name)
}

/// `/ecs/trix-api-prod` and `/aws/lambda/trix-webhooks` → `api-prod` and `webhooks`: the last
/// path segment, without the dash-separated prefix every group shares.
pub(crate) fn short_names(groups: &[String]) -> Vec<String> {
    let last_segments: Vec<&str> = groups
        .iter()
        .map(|group| {
            group
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(group)
        })
        .collect();
    if last_segments.len() < 2 {
        return last_segments.into_iter().map(str::to_string).collect();
    }
    let split: Vec<Vec<&str>> = last_segments
        .iter()
        .map(|segment| segment.split('-').collect())
        .collect();
    let shortest = split.iter().map(Vec::len).min().unwrap_or(0);
    let mut shared = 0;
    while shared + 1 < shortest
        && split
            .iter()
            .all(|parts| parts.get(shared) == split[0].get(shared))
    {
        shared += 1;
    }
    split
        .iter()
        .map(|parts| parts.get(shared..).unwrap_or_default().join("-"))
        .collect()
}

/// The longest literal stretch of a template, to look the pattern up in Logs Insights.
pub(crate) fn literal_fragment(template: &str) -> String {
    template
        .split(['<', '>'])
        .enumerate()
        .filter(|(index, _)| index % 2 == 0)
        .map(|(_, part)| part.trim().trim_matches(|character: char| character == '"'))
        .max_by_key(|part| part.len())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Escapes text for a `/regex/` in a Logs Insights `filter`.
pub(crate) fn insights_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '/'
                | '.'
                | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '^'
                | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Byte ranges of `query` in `text`, ignoring ASCII case.
pub(crate) fn find_matches(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = haystack.get(from..).and_then(|rest| rest.find(&needle)) {
        let start = from + found;
        ranges.push(start..start + needle.len());
        from = start + needle.len();
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_group_repeats_of_one_event() {
        assert_eq!(
            template("Retentativa 1/3 no adquirente: ECONNRESET req_T81sLm"),
            template("Retentativa 2/3 no adquirente: ECONNRESET req_Vn30Ka")
        );
        assert_eq!(
            template("Retentativa 1/3 no adquirente: ECONNRESET req_T81sLm"),
            "Retentativa <n>/<n> no adquirente: ECONNRESET <id>"
        );
        assert_eq!(
            template("POST /v1/checkout/pay 200 812ms userId=51902"),
            "POST /v1/checkout/pay <n> <n>ms userId=<n>"
        );
        assert_eq!(
            template("POST /v1/checkout/pay 200 1204ms userId=77120"),
            template("POST /v1/checkout/pay 200 812ms userId=51902")
        );
        assert_eq!(template("trace 3f2a9c1e0b7d4e6f done"), "trace <hex> done");
        assert_eq!(
            template("user \"ana@example.com\" not found in 'tenant-a'"),
            "user \"<s>\" not found in '<s>'"
        );
        assert_eq!(template("it's   fine"), "it's fine");
        assert_ne!(
            template("Webhook fora de ordem: pagamento.estornado antes de capturado pay_0Rk2"),
            template("Adquirente respondeu 502 ao capturar o pagamento req_T81sLm")
        );
        assert_eq!(
            template("Webhook fora de ordem: pagamento.estornado antes de capturado pay_0Rk2"),
            template("Webhook fora de ordem: pagamento.estornado antes de capturado pay_9LmX")
        );
    }

    #[test]
    fn detector_flags_only_new_templates_that_repeat() {
        let mut detector = PatternDetector::default();
        assert_eq!(detector.observe("Webhook fora de ordem pay_0Rk2", 0), None);
        detector.set_baseline(["Retentativa 1/3 no adquirente: ECONNRESET req_T81sLm"]);
        assert!(detector.is_watching());

        for second in 0..5 {
            assert_eq!(
                detector.observe(
                    "Retentativa 2/3 no adquirente: ECONNRESET req_Vn30Ka",
                    second * 1000
                ),
                None
            );
        }

        let base = 1_000_000;
        assert_eq!(
            detector.observe("Webhook fora de ordem pay_0Rk2", base),
            None
        );
        assert_eq!(
            detector.observe("Webhook fora de ordem pay_1Ab3", base + 30_000),
            None
        );
        let pattern = detector
            .observe("Webhook fora de ordem pay_2Cd4", base + 60_000)
            .expect("third hit inside two minutes");
        assert_eq!(pattern.example, "Webhook fora de ordem pay_0Rk2");
        assert_eq!(pattern.first_seen_ms, base);
        assert_eq!(pattern.hits, 3);
        assert_eq!(
            detector.observe("Webhook fora de ordem pay_3Ef5", base + 61_000),
            None,
            "announced once"
        );
    }

    #[test]
    fn detector_needs_the_hits_inside_the_window() {
        let mut detector = PatternDetector::default();
        detector.set_baseline(std::iter::empty());
        let minute = 60_000;
        assert_eq!(detector.observe("cache miss key=1", 0), None);
        assert_eq!(detector.observe("cache miss key=2", 3 * minute), None);
        assert_eq!(detector.observe("cache miss key=3", 4 * minute), None);
        assert!(detector.observe("cache miss key=4", 5 * minute).is_some());

        detector.silence(&template("silenced 1"));
        for index in 0..5 {
            assert_eq!(detector.observe(&format!("silenced {index}"), index), None);
        }
    }

    #[test]
    fn parses_structured_and_plain_events() {
        let parsed = parse_message(
            r#"{"level":"error","msg":"Adquirente respondeu 502 req_T81sLm","err":{"stack":"Error: 502\n    at capturePayment (src/modules/payments/checkout.ts:142:11)\n    at async PaymentsController.pay (src/modules/payments/payments.controller.ts:58:5)"}}"#,
        );
        assert_eq!(parsed.level, Some(Level::Error));
        assert_eq!(parsed.message, "Adquirente respondeu 502 req_T81sLm");
        assert_eq!(
            parsed.stack,
            vec![
                "at capturePayment (src/modules/payments/checkout.ts:142:11)",
                "at async PaymentsController.pay (src/modules/payments/payments.controller.ts:58:5)"
            ]
        );

        let parsed = parse_message(r#"{"level":40,"message":"Latência alta"}"#);
        assert_eq!(parsed.level, Some(Level::Warn));
        assert_eq!(parsed.message, "Latência alta");

        let parsed = parse_message(
            "2026-09-28T16:42:03.911Z\tc0ffee\tERROR\tFalha ao capturar\n    at handler (/var/task/index.js:10:3)",
        );
        assert_eq!(parsed.level, Some(Level::Error));
        assert_eq!(
            parsed.stack,
            vec!["at handler (/var/task/index.js:10:3)".to_string()]
        );

        assert_eq!(parse_message("GET /health 200").level, None);
    }

    #[test]
    fn finds_links_in_stack_frames() {
        let frame = "at capturePayment (src/modules/payments/checkout.ts:142:11)";
        let link = frame_link(frame).expect("link");
        assert_eq!(link.path, "src/modules/payments/checkout.ts");
        assert_eq!(link.line, 142);
        assert_eq!(link.column, Some(11));
        assert_eq!(
            &frame[link.range],
            "src/modules/payments/checkout.ts:142:11"
        );

        let link = frame_link("File \"/app/worker.py\", line 3").map(|link| link.path);
        assert_eq!(link, None);
        let link = frame_link("at main (file:///app/dist/main.js:7)").expect("link");
        assert_eq!(link.path, "/app/dist/main.js");
        assert_eq!(link.column, None);
        assert_eq!(
            frame_link("at x (node_modules/express/lib/router.js:1:2)"),
            None
        );
        assert_eq!(frame_link("at http://localhost:3000/a"), None);
    }

    #[test]
    fn short_names_drop_the_shared_prefix() {
        assert_eq!(
            short_names(&[
                "/ecs/trix-api-prod".into(),
                "/aws/lambda/trix-webhooks".into()
            ]),
            vec!["api-prod", "webhooks"]
        );
        assert_eq!(
            short_names(&["/ecs/trix-api-prod".into()]),
            vec!["trix-api-prod"]
        );
        assert_eq!(
            group_name("arn:aws:logs:us-east-1:123456789012:log-group:/ecs/trix-api-prod:*"),
            "/ecs/trix-api-prod"
        );
        assert_eq!(group_name("/ecs/trix-api-prod"), "/ecs/trix-api-prod");
    }

    #[test]
    fn literal_fragments_and_matches() {
        assert_eq!(
            literal_fragment(&template(
                "Webhook fora de ordem: pagamento.estornado antes de capturado pay_0Rk2"
            )),
            "Webhook fora de ordem: pagamento.estornado antes de capturado"
        );
        assert_eq!(insights_regex("a.b/c"), "a\\.b\\/c");
        assert_eq!(
            find_matches("Pagamento e pagamento", "pagamento"),
            vec![0..9, 12..21]
        );
        assert!(find_matches("abc", "").is_empty());
    }
}
