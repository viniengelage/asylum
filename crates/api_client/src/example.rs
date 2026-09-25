//! Writes a response into the YAML spec as the `example` of that status, touching only the
//! lines of that example so the rest of the file keeps its formatting.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::ops::Range;

struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

impl Line<'_> {
    fn indent(&self) -> usize {
        self.text.len() - self.text.trim_start().len()
    }

    fn content(&self) -> &str {
        self.text.trim()
    }

    fn is_blank(&self) -> bool {
        let content = self.content();
        content.is_empty() || content.starts_with('#')
    }

    /// The mapping key on this line, without quotes, and whether a value follows it inline.
    fn key(&self) -> Option<(&str, bool)> {
        let content = self.content();
        let (key, rest) = content.split_once(':')?;
        let key = key
            .trim()
            .trim_matches(|character| character == '\'' || character == '"');
        let rest = rest.trim();
        let inline = !rest.is_empty() && !rest.starts_with('#');
        Some((key, inline))
    }
}

fn lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for text_line in text.split_inclusive('\n') {
        let end = start + text_line.len();
        lines.push(Line {
            start,
            end,
            text: text_line.trim_end_matches(['\n', '\r']),
        });
        start = end;
    }
    lines
}

/// The index just past the block that starts at `row` (its more indented lines).
fn block_end(lines: &[Line], row: usize) -> usize {
    let indent = lines[row].indent();
    let mut end = row + 1;
    for (index, line) in lines.iter().enumerate().skip(row + 1) {
        if line.is_blank() {
            continue;
        }
        if line.indent() <= indent {
            break;
        }
        end = index + 1;
    }
    end
}

/// The direct child of the block at `row` whose key matches.
fn child(lines: &[Line], row: usize, matches: impl Fn(&str) -> bool) -> Option<usize> {
    let end = block_end(lines, row);
    let child_indent = lines[row + 1..end]
        .iter()
        .find(|line| !line.is_blank())
        .map(Line::indent)?;
    (row + 1..end).find(|&index| {
        let line = &lines[index];
        !line.is_blank()
            && line.indent() == child_indent
            && line.key().is_some_and(|(key, _)| matches(key))
    })
}

fn child_indent(lines: &[Line], row: usize) -> usize {
    let end = block_end(lines, row);
    lines[row + 1..end]
        .iter()
        .find(|line| !line.is_blank())
        .map(Line::indent)
        .unwrap_or(lines[row].indent() + 2)
}

fn yaml_block(value: &Value, indent: usize) -> Result<String> {
    let yaml = serde_yaml::to_string(value)?;
    let padding = " ".repeat(indent);
    Ok(yaml
        .lines()
        .map(|line| format!("{padding}{line}\n"))
        .collect())
}

fn refuse_inline(lines: &[Line], row: usize, what: &str) -> Result<()> {
    match lines[row].key() {
        Some((_, true)) => Err(anyhow!(
            "{what} está escrito numa linha só (`{}`); edite o exemplo à mão",
            lines[row].content()
        )),
        _ => Ok(()),
    }
}

/// The edit that sets `example` for the response: the byte range to replace and the text.
pub fn example_edit(
    text: &str,
    path: &str,
    method: &str,
    status: u16,
    content_type: &str,
    example: &Value,
) -> Result<(Range<usize>, String)> {
    let lines = lines(text);
    let paths_row = (0..lines.len())
        .find(|&index| {
            lines[index].indent() == 0 && lines[index].key().is_some_and(|(key, _)| key == "paths")
        })
        .ok_or_else(|| anyhow!("o spec não tem `paths:` em YAML de bloco"))?;
    let path_row = child(&lines, paths_row, |key| key == path)
        .ok_or_else(|| anyhow!("{path} não foi achado no spec"))?;
    let method_lower = method.to_ascii_lowercase();
    let method_row = child(&lines, path_row, |key| key == method_lower)
        .ok_or_else(|| anyhow!("{method} {path} não foi achado no spec"))?;
    refuse_inline(&lines, method_row, "A operação")?;
    let responses_row = child(&lines, method_row, |key| key == "responses")
        .ok_or_else(|| anyhow!("{method} {path} não tem `responses`"))?;
    let status_text = status.to_string();
    let range_text = format!("{}XX", status / 100);
    let status_row = child(&lines, responses_row, |key| key == status_text)
        .or_else(|| {
            child(&lines, responses_row, |key| {
                key.eq_ignore_ascii_case(&range_text)
            })
        })
        .ok_or_else(|| anyhow!("o spec não lista a resposta {status} de {method} {path}"))?;
    refuse_inline(&lines, status_row, "A resposta")?;
    if child(&lines, status_row, |key| key == "$ref").is_some() {
        return Err(anyhow!(
            "a resposta {status} vem de um $ref; ponha o exemplo em components/responses"
        ));
    }

    let Some(content_row) = child(&lines, status_row, |key| key == "content") else {
        // No `content` yet: it goes at the end of the status block.
        let indent = child_indent(&lines, status_row);
        let end = block_end(&lines, status_row);
        let offset = lines[end - 1].end;
        let mut insertion = String::new();
        if !text[..offset].ends_with('\n') {
            insertion.push('\n');
        }
        insertion.push_str(&format!(
            "{}content:\n{}{content_type}:\n{}example:\n",
            " ".repeat(indent),
            " ".repeat(indent + 2),
            " ".repeat(indent + 4)
        ));
        insertion.push_str(&yaml_block(example, indent + 6)?);
        return Ok((offset..offset, insertion));
    };
    refuse_inline(&lines, content_row, "O content")?;
    let media_row = child(&lines, content_row, |key| {
        key.split(';').next().unwrap_or(key).trim() == content_type
    })
    .or_else(|| child(&lines, content_row, |key| key.contains("json")))
    .ok_or_else(|| anyhow!("a resposta {status} não declara {content_type}"))?;
    refuse_inline(&lines, media_row, "O media type")?;
    let indent = child_indent(&lines, media_row);
    let mut replacement = format!("{}example:\n", " ".repeat(indent));
    replacement.push_str(&yaml_block(example, indent + 2)?);
    match child(&lines, media_row, |key| key == "example") {
        Some(example_row) => {
            let end = block_end(&lines, example_row);
            Ok((lines[example_row].start..lines[end - 1].end, replacement))
        }
        None => {
            let offset = lines[media_row].end;
            let replacement = if text[..offset].ends_with('\n') {
                replacement
            } else {
                format!("\n{replacement}")
            };
            Ok((offset..offset, replacement))
        }
    }
}

/// The line the example starts at after the edit, to put the cursor there.
pub fn edited_row(text: &str, range: &Range<usize>, replacement: &str) -> u32 {
    let leading_newlines = replacement.len() - replacement.trim_start_matches('\n').len();
    (text[..range.start].matches('\n').count() + leading_newlines) as u32
}

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Every operation in a block-style YAML spec: its 0-based line, method and path.
pub fn operation_rows(text: &str) -> Vec<(u32, String, String)> {
    let lines = lines(text);
    let Some(paths_row) = (0..lines.len()).find(|&index| {
        lines[index].indent() == 0 && lines[index].key().is_some_and(|(key, _)| key == "paths")
    }) else {
        return Vec::new();
    };
    let mut operations = Vec::new();
    let paths_end = block_end(&lines, paths_row);
    let path_indent = child_indent(&lines, paths_row);
    for path_row in paths_row + 1..paths_end {
        let line = &lines[path_row];
        if line.is_blank() || line.indent() != path_indent {
            continue;
        }
        let Some((path, false)) = line.key() else {
            continue;
        };
        if !path.starts_with('/') {
            continue;
        }
        let method_indent = child_indent(&lines, path_row);
        for method_row in path_row + 1..block_end(&lines, path_row) {
            let method_line = &lines[method_row];
            if method_line.is_blank() || method_line.indent() != method_indent {
                continue;
            }
            if let Some((method, _)) = method_line.key()
                && METHODS.contains(&method)
            {
                operations.push((
                    method_row as u32,
                    method.to_ascii_uppercase(),
                    path.to_string(),
                ));
            }
        }
    }
    operations
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const SPEC: &str = "\
paths:
  /auth/:
    post:
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/AccessToken'
        '401':
          description: Credenciais inválidas
  /auth/me:
    get:
      responses:
        '200':
          content:
            application/json:
              example:
                old: true
              schema: { type: object }
        '404': { description: nope }
  /users:
    get:
      responses:
        '200':
          $ref: '#/components/responses/Users'
";

    fn apply(range: Range<usize>, replacement: &str) -> String {
        let mut text = SPEC.to_string();
        text.replace_range(range, replacement);
        text
    }

    #[test]
    fn inserts_an_example_under_the_media_type() {
        let (range, replacement) = example_edit(
            SPEC,
            "/auth/",
            "POST",
            200,
            "application/json",
            &json!({ "accessToken": "abc", "refreshToken": "r1" }),
        )
        .unwrap();
        let edited = apply(range.clone(), &replacement);
        assert!(edited.contains(
            "            application/json:\n              example:\n                accessToken: abc\n                refreshToken: r1\n              schema:\n"
        ), "{edited}");
        assert_eq!(edited_row(SPEC, &range, &replacement), 8);
        crate::spec::parse_document(&edited).unwrap();
    }

    #[test]
    fn replaces_an_existing_example_and_adds_content_when_missing() {
        let (range, replacement) = example_edit(
            SPEC,
            "/auth/me",
            "GET",
            200,
            "application/json",
            &json!({ "id": 7 }),
        )
        .unwrap();
        let edited = apply(range, &replacement);
        assert!(edited.contains("              example:\n                id: 7\n              schema: { type: object }"), "{edited}");
        assert!(!edited.contains("old: true"));

        let (range, replacement) = example_edit(
            SPEC,
            "/auth/",
            "POST",
            401,
            "application/json",
            &json!({ "message": "x" }),
        )
        .unwrap();
        let edited = apply(range, &replacement);
        assert!(edited.contains(
            "          description: Credenciais inválidas\n          content:\n            application/json:\n              example:\n                message: x\n  /auth/me:"
        ), "{edited}");
        crate::spec::parse_document(&edited).unwrap();
    }

    #[test]
    fn lists_operations_with_their_lines() {
        let rows = operation_rows(SPEC);
        assert_eq!(
            rows,
            vec![
                (2, "POST".to_string(), "/auth/".to_string()),
                (13, "GET".to_string(), "/auth/me".to_string()),
                (23, "GET".to_string(), "/users".to_string()),
            ]
        );
    }

    #[test]
    fn refuses_what_it_cannot_edit_safely() {
        let error = |path: &str, method: &str, status: u16| {
            example_edit(SPEC, path, method, status, "application/json", &json!({}))
                .unwrap_err()
                .to_string()
        };
        assert!(error("/users", "GET", 200).contains("$ref"));
        assert!(error("/auth/me", "GET", 404).contains("numa linha só"));
        assert!(error("/auth/me", "GET", 500).contains("não lista a resposta 500"));
        assert!(error("/nope", "GET", 200).contains("não foi achado"));
    }
}
