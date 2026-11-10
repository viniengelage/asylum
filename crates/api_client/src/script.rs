//! The part of a Postman script that maps onto the response without a JavaScript engine:
//! `pm.test` blocks made of `pm.response.to.have.status(N)` and `pm.expect(<body path>)` with
//! `equal`/`eql`, `match` and `exist`, plus `pm.environment.set` from the body, which becomes a
//! capture. Everything else is kept and shown, but off.

use crate::{config, jsonpath, send::ReceivedResponse};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum LineKind {
    Plain,
    /// The line can't run here. `call` names what stops it on the line that starts it, and is
    /// `None` on the lines inside a block that is off as a whole.
    Off {
        call: Option<String>,
    },
    Capture {
        variable: String,
        path: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptLine {
    pub number: usize,
    pub text: String,
    pub kind: LineKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub source: String,
    literal: String,
    anchored_start: bool,
    anchored_end: bool,
}

impl Pattern {
    fn is_match(&self, text: &str) -> bool {
        match (self.anchored_start, self.anchored_end) {
            (true, true) => text == self.literal,
            (true, false) => text.starts_with(&self.literal),
            (false, true) => text.ends_with(&self.literal),
            (false, false) => text.contains(&self.literal),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Assertion {
    Status(u16),
    Equal {
        subject: String,
        path: String,
        expected: Value,
    },
    Match {
        subject: String,
        path: String,
        pattern: Pattern,
    },
    Exist {
        subject: String,
        path: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Test {
    pub name: String,
    pub line: usize,
    pub assertions: Vec<Assertion>,
    pub runnable: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    pub lines: Vec<ScriptLine>,
    pub tests: Vec<Test>,
}

impl Script {
    /// Nothing in the script maps, so it is off as a whole.
    pub fn is_off(&self) -> bool {
        let runs = self.tests.iter().any(|test| test.runnable)
            || self
                .lines
                .iter()
                .any(|line| matches!(line.kind, LineKind::Capture { .. }));
        !runs
            && self
                .lines
                .iter()
                .any(|line| matches!(line.kind, LineKind::Off { .. }))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    Status {
        expected: u16,
        actual: u16,
    },
    Equal {
        subject: String,
        expected: String,
        actual: String,
    },
    Match {
        subject: String,
        pattern: String,
    },
    Exist {
        subject: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Passed,
    Failed(Failure),
    Off,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    pub name: String,
    pub line: usize,
    pub outcome: Outcome,
    pub elapsed: Duration,
}

/// `after_response` is false for a pre-request script: nothing there runs before sending.
pub fn parse(source: &str, after_response: bool) -> Script {
    let mut aliases: Vec<String> = config::RESPONSE_EXPRESSIONS
        .iter()
        .map(|expression| expression.to_string())
        .collect();
    let mut script = Script::default();
    let mut open_test: Option<Test> = None;
    let mut off_block = false;
    let mut depth = 0;
    for (index, text) in source.lines().enumerate() {
        let number = index + 1;
        let trimmed = text.trim();
        let change = depth_change(trimmed);
        let kind = if off_block {
            depth += change;
            off_block = depth > 0;
            LineKind::Off { call: None }
        } else if let Some(mut test) = open_test.take() {
            depth += change;
            let body = if depth <= 0 {
                before_last_closing_brace(trimmed)
            } else {
                trimmed
            };
            let kind = classify_test_body(body, &mut test, &mut aliases);
            if depth <= 0 {
                script.tests.push(test);
            } else {
                open_test = Some(test);
            }
            kind
        } else if let Some((name, body)) = test_opener(trimmed).filter(|_| after_response) {
            let mut test = Test {
                name,
                line: number,
                assertions: Vec::new(),
                runnable: true,
            };
            depth = change;
            let kind = classify_test_body(body, &mut test, &mut aliases);
            if depth <= 0 {
                script.tests.push(test);
            } else {
                open_test = Some(test);
            }
            kind
        } else {
            let kind = classify_top_level(trimmed, after_response, &mut aliases);
            if matches!(kind, LineKind::Off { .. }) && change > 0 {
                off_block = true;
                depth = change;
            }
            kind
        };
        script.lines.push(ScriptLine {
            number,
            text: text.to_string(),
            kind,
        });
    }
    script.tests.extend(open_test);
    script
}

pub fn run(script: &Script, response: &ReceivedResponse) -> Vec<TestResult> {
    let body = response.json();
    script
        .tests
        .iter()
        .map(|test| {
            let started = Instant::now();
            let outcome = if test.runnable {
                test.assertions
                    .iter()
                    .find_map(|assertion| check(assertion, response.status, body.as_ref()))
                    .map_or(Outcome::Passed, Outcome::Failed)
            } else {
                Outcome::Off
            };
            TestResult {
                name: test.name.clone(),
                line: test.line,
                outcome,
                elapsed: started.elapsed(),
            }
        })
        .collect()
}

fn check(assertion: &Assertion, status: u16, body: Option<&Value>) -> Option<Failure> {
    let select = |path: &str| body.and_then(|body| jsonpath::select(body, path));
    match assertion {
        Assertion::Status(expected) => (*expected != status).then(|| Failure::Status {
            expected: *expected,
            actual: status,
        }),
        Assertion::Equal {
            subject,
            path,
            expected,
        } => {
            let actual = select(path);
            (actual != Some(expected)).then(|| Failure::Equal {
                subject: subject.clone(),
                expected: expected.to_string(),
                actual: actual.map_or_else(|| "undefined".to_string(), Value::to_string),
            })
        }
        Assertion::Match {
            subject,
            path,
            pattern,
        } => {
            let matches = match select(path) {
                Some(Value::String(text)) => pattern.is_match(text),
                _ => false,
            };
            (!matches).then(|| Failure::Match {
                subject: subject.clone(),
                pattern: pattern.source.clone(),
            })
        }
        Assertion::Exist { subject, path } => {
            select(path)
                .is_none_or(Value::is_null)
                .then(|| Failure::Exist {
                    subject: subject.clone(),
                })
        }
    }
}

fn classify_top_level(line: &str, after_response: bool, aliases: &mut Vec<String>) -> LineKind {
    for statement in statements(line) {
        if !after_response {
            return off(statement);
        }
        if let Some(alias) = config::response_alias(statement, aliases) {
            aliases.push(alias);
        } else if let Some((variable, path)) = config::capture_from_line(statement, aliases) {
            return LineKind::Capture { variable, path };
        } else {
            return off(statement);
        }
    }
    LineKind::Plain
}

fn classify_test_body(body: &str, test: &mut Test, aliases: &mut Vec<String>) -> LineKind {
    let mut kind = LineKind::Plain;
    for statement in statements(body) {
        if let Some(alias) = config::response_alias(statement, aliases) {
            aliases.push(alias);
        } else if let Some(assertion) = assertion(statement, aliases) {
            test.assertions.push(assertion);
        } else {
            test.runnable = false;
            if kind == LineKind::Plain {
                kind = off(statement);
            }
        }
    }
    kind
}

fn off(statement: &str) -> LineKind {
    LineKind::Off {
        call: Some(call_name(statement)),
    }
}

/// What to name in "off: … doesn't run here".
fn call_name(statement: &str) -> String {
    if statement.contains("pm.sendRequest") {
        return "pm.sendRequest".to_string();
    }
    if statement.contains("require(") {
        return "require".to_string();
    }
    if let Some(close) = statement
        .strip_prefix("pm.expect(")
        .and_then(|_| matching_paren(statement, "pm.expect".len()))
    {
        let chain = &statement[close + 1..];
        let end = chain.find('(').unwrap_or(chain.len());
        return format!("pm.expect(…){}", &chain[..end]);
    }
    let head: String = statement
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '$')
        })
        .collect();
    if !head.is_empty() && statement[head.len()..].starts_with('(') {
        return head;
    }
    const LONGEST: usize = 32;
    if statement.chars().count() > LONGEST {
        let shortened: String = statement.chars().take(LONGEST).collect();
        format!("{shortened}…")
    } else {
        statement.to_string()
    }
}

/// `pm.test("name", function () { …` gives the name and what follows the opening brace, or
/// the expression of an arrow function without braces.
fn test_opener(line: &str) -> Option<(String, &str)> {
    let rest = line.strip_prefix("pm.test(")?;
    let (name, after_name) = quoted(rest)?;
    let after_comma = after_name.trim_start().strip_prefix(',')?;
    if let Some(brace) = after_comma.find('{') {
        let body = &after_comma[brace + 1..];
        let body = if depth_change(line) <= 0 {
            before_last_closing_brace(body)
        } else {
            body
        };
        return Some((name, body));
    }
    let arrow = after_comma.find("=>")?;
    let expression = after_comma[arrow + 2..].trim();
    let expression = expression
        .strip_suffix(';')
        .unwrap_or(expression)
        .trim_end();
    Some((name, expression.strip_suffix(')').unwrap_or(expression)))
}

fn quoted(text: &str) -> Option<(String, &str)> {
    let quote = text
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\'' | '`'))?;
    let inside = &text[1..];
    let end = inside.find(quote)?;
    Some((inside[..end].to_string(), &inside[end + 1..]))
}

fn assertion(statement: &str, aliases: &[String]) -> Option<Assertion> {
    if let Some(code) = statement
        .strip_prefix("pm.response.to.have.status(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return code.trim().parse().ok().map(Assertion::Status);
    }
    statement.strip_prefix("pm.expect(")?;
    let close = matching_paren(statement, "pm.expect".len())?;
    let subject = statement["pm.expect(".len()..close].trim();
    let mut chain = statement[close + 1..].strip_prefix(".to")?;
    'filler: loop {
        for word in [".been", ".be", ".deep", ".is", ".that", ".have"] {
            if let Some(rest) = chain.strip_prefix(word)
                && (rest.is_empty() || rest.starts_with('.'))
            {
                chain = rest;
                continue 'filler;
            }
        }
        break;
    }
    if chain == ".exist" {
        return Some(Assertion::Exist {
            subject: subject.to_string(),
            path: subject_path(subject, aliases)?,
        });
    }
    if let Some(argument) = [".equal(", ".eql("]
        .iter()
        .find_map(|matcher| chain.strip_prefix(matcher))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let expected = literal(argument.trim())?;
        if subject == "pm.response.code" {
            return expected
                .as_u64()
                .and_then(|code| u16::try_from(code).ok())
                .map(Assertion::Status);
        }
        return Some(Assertion::Equal {
            subject: subject.to_string(),
            path: subject_path(subject, aliases)?,
            expected,
        });
    }
    let argument = chain
        .strip_prefix(".match(")
        .and_then(|rest| rest.strip_suffix(')'))?;
    Some(Assertion::Match {
        subject: subject.to_string(),
        path: subject_path(subject, aliases)?,
        pattern: pattern(argument.trim())?,
    })
}

fn subject_path(subject: &str, aliases: &[String]) -> Option<String> {
    aliases.iter().find_map(|alias| {
        let accessor = subject.strip_prefix(alias.as_str())?;
        if !(accessor.is_empty() || accessor.starts_with('.') || accessor.starts_with('[')) {
            return None;
        }
        let path = format!("${accessor}");
        jsonpath::is_valid(&path).then_some(path)
    })
}

fn literal(text: &str) -> Option<Value> {
    if let Some(inner) = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return (!inner.contains('\'')).then(|| Value::String(inner.to_string()));
    }
    serde_json::from_str(text).ok()
}

/// A regex literal that is plain text with optional `^`/`$` anchors; anything with real regex
/// syntax would need an engine to mean the same thing.
fn pattern(text: &str) -> Option<Pattern> {
    let inner = text.strip_prefix('/')?.strip_suffix('/')?;
    let (inner, anchored_start) = match inner.strip_prefix('^') {
        Some(rest) => (rest, true),
        None => (inner, false),
    };
    let (inner, anchored_end) = match inner.strip_suffix('$') {
        Some(rest) if !rest.ends_with('\\') => (rest, true),
        _ => (inner, false),
    };
    let mut literal = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                let escaped = characters.next()?;
                if escaped.is_ascii_alphanumeric() {
                    return None;
                }
                literal.push(escaped);
            }
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                return None;
            }
            other => literal.push(other),
        }
    }
    Some(Pattern {
        source: text.to_string(),
        literal,
        anchored_start,
        anchored_end,
    })
}

/// The index of the `)` closing the `(` at `open`, skipping quoted text.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quote = None;
    for (index, character) in text.char_indices().skip_while(|(index, _)| *index < open) {
        match (quote, character) {
            (Some(open_quote), _) if character == open_quote => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(character),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn depth_change(line: &str) -> i32 {
    let mut change = 0;
    for_each_unquoted(line, |_, character| match character {
        '{' => change += 1,
        '}' => change -= 1,
        _ => {}
    });
    change
}

fn before_last_closing_brace(text: &str) -> &str {
    let mut last = None;
    for_each_unquoted(text, |index, character| {
        if character == '}' {
            last = Some(index);
        }
    });
    last.map_or(text, |index| &text[..index])
}

fn statements(text: &str) -> Vec<&str> {
    let mut boundaries = Vec::new();
    for_each_unquoted(text, |index, character| {
        if character == ';' {
            boundaries.push(index);
        }
    });
    let mut statements = Vec::new();
    let mut start = 0;
    for end in boundaries.into_iter().chain([text.len()]) {
        let statement = text[start..end].trim();
        start = (end + 1).min(text.len());
        let is_noise = statement.starts_with("//")
            || statement
                .chars()
                .all(|character| matches!(character, '{' | '}' | '(' | ')' | ' ' | ','));
        if !is_noise {
            statements.push(statement);
        }
    }
    statements
}

fn for_each_unquoted(text: &str, mut visit: impl FnMut(usize, char)) {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if let Some(open_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open_quote {
                quote = None;
            }
        } else if matches!(character, '"' | '\'' | '`') {
            quote = Some(character);
        } else {
            visit(index, character);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> ReceivedResponse {
        ReceivedResponse {
            status,
            reason: None,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: body.as_bytes().to_vec(),
            truncated: false,
            elapsed: Duration::ZERO,
        }
    }

    fn outcomes(source: &str, response: &ReceivedResponse) -> Vec<Outcome> {
        run(&parse(source, true), response)
            .into_iter()
            .map(|result| result.outcome)
            .collect()
    }

    #[test]
    fn checks_the_status() {
        let source =
            "pm.test(\"Status is 201\", function () {\n    pm.response.to.have.status(201);\n});";
        assert_eq!(
            outcomes(source, &response(201, "{}")),
            vec![Outcome::Passed]
        );
        assert_eq!(
            outcomes(source, &response(400, "{}")),
            vec![Outcome::Failed(Failure::Status {
                expected: 201,
                actual: 400
            })]
        );
        assert_eq!(
            outcomes(
                "pm.test('ok', () => pm.expect(pm.response.code).to.eql(200));",
                &response(200, "{}")
            ),
            vec![Outcome::Passed]
        );
    }

    #[test]
    fn checks_equal_on_a_nested_path_through_an_alias() {
        let source = "const data = pm.response.json();\npm.test(\"pix\", () => {\n  pm.expect(data.pix.status).to.eql('PENDING');\n  pm.expect(data.items[0].amount).to.equal(10);\n});";
        let script = parse(source, true);
        assert_eq!(script.lines[0].kind, LineKind::Plain);
        assert_eq!(script.tests.len(), 1);
        assert!(script.tests[0].runnable);
        assert_eq!(script.tests[0].line, 2);
        let body = r#"{"pix":{"status":"PENDING"},"items":[{"amount":10}]}"#;
        assert_eq!(
            outcomes(source, &response(200, body)),
            vec![Outcome::Passed]
        );
        let body = r#"{"pix":{"status":"PAID"},"items":[{"amount":10}]}"#;
        assert_eq!(
            outcomes(source, &response(200, body)),
            vec![Outcome::Failed(Failure::Equal {
                subject: "data.pix.status".to_string(),
                expected: "\"PENDING\"".to_string(),
                actual: "\"PAID\"".to_string(),
            })]
        );
    }

    #[test]
    fn checks_match_and_exist() {
        let source = "pm.test(\"id\", function () {\n  var json = JSON.parse(responseBody);\n  pm.expect(json.id).to.exist;\n  pm.expect(json.code).to.match(/^PIX-/);\n});";
        assert_eq!(
            outcomes(source, &response(200, r#"{"id":1,"code":"PIX-9"}"#)),
            vec![Outcome::Passed]
        );
        assert_eq!(
            outcomes(source, &response(200, r#"{"id":null,"code":"PIX-9"}"#)),
            vec![Outcome::Failed(Failure::Exist {
                subject: "json.id".to_string()
            })]
        );
        assert_eq!(
            outcomes(source, &response(200, r#"{"id":1,"code":"X-PIX-9"}"#)),
            vec![Outcome::Failed(Failure::Match {
                subject: "json.code".to_string(),
                pattern: "/^PIX-/".to_string(),
            })]
        );
    }

    #[test]
    fn environment_set_becomes_a_capture() {
        let script = parse(
            "const body = pm.response.json();\npm.environment.set(\"pixId\", body.id);",
            true,
        );
        assert_eq!(
            script.lines[1].kind,
            LineKind::Capture {
                variable: "pixId".to_string(),
                path: "$.id".to_string(),
            }
        );
        assert!(!script.is_off());
    }

    #[test]
    fn send_request_and_require_are_off() {
        let source = "const moment = require('moment');\npm.sendRequest(\"https://x\", function (err, res) {\n  console.log(res);\n});\npm.test(\"later\", function () {\n  pm.expect(moment()).to.be.ok;\n});";
        let script = parse(source, true);
        let kinds: Vec<&LineKind> = script.lines.iter().map(|line| &line.kind).collect();
        assert_eq!(
            kinds[0],
            &LineKind::Off {
                call: Some("require".to_string())
            }
        );
        assert_eq!(
            kinds[1],
            &LineKind::Off {
                call: Some("pm.sendRequest".to_string())
            }
        );
        assert_eq!(kinds[2], &LineKind::Off { call: None });
        assert_eq!(kinds[3], &LineKind::Off { call: None });
        assert_eq!(
            kinds[5],
            &LineKind::Off {
                call: Some("pm.expect(…).to.be.ok".to_string())
            }
        );
        assert!(!script.tests[0].runnable);
        assert!(script.is_off());
        assert_eq!(
            run(&script, &response(200, "{}"))
                .into_iter()
                .map(|result| result.outcome)
                .collect::<Vec<_>>(),
            vec![Outcome::Off]
        );
    }

    #[test]
    fn pre_request_scripts_never_run() {
        let script = parse("pm.environment.set(\"ts\", Date.now());", false);
        assert_eq!(
            script.lines[0].kind,
            LineKind::Off {
                call: Some("pm.environment.set".to_string())
            }
        );
        assert!(script.tests.is_empty());
    }
}
