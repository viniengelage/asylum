//! The part of JSONPath that picking a token out of a login response needs: `$.a.b`,
//! `$['a-b']` and `$.items[0]`. Filters, wildcards and recursive descent are not supported.

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(usize),
}

fn parse(path: &str) -> Option<Vec<Segment>> {
    let path = path.trim();
    let mut rest = path.strip_prefix('$').unwrap_or(path);
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            let end = after_dot.find(['.', '[']).unwrap_or(after_dot.len());
            let key = &after_dot[..end];
            if key.is_empty() {
                return None;
            }
            segments.push(Segment::Key(key.to_string()));
            rest = &after_dot[end..];
        } else if let Some(after_bracket) = rest.strip_prefix('[') {
            let end = after_bracket.find(']')?;
            let inside = after_bracket[..end].trim();
            let quoted = inside
                .strip_prefix('\'')
                .and_then(|inner| inner.strip_suffix('\''))
                .or_else(|| {
                    inside
                        .strip_prefix('"')
                        .and_then(|inner| inner.strip_suffix('"'))
                });
            match quoted {
                Some(key) => segments.push(Segment::Key(key.to_string())),
                None => segments.push(Segment::Index(inside.parse().ok()?)),
            }
            rest = &after_bracket[end + 1..];
        } else if segments.is_empty() {
            // A bare `session.token`, as people often type it.
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            segments.push(Segment::Key(rest[..end].to_string()));
            rest = &rest[end..];
        } else {
            return None;
        }
    }
    Some(segments)
}

pub fn is_valid(path: &str) -> bool {
    parse(path).is_some()
}

pub fn select<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in parse(path)? {
        current = match segment {
            Segment::Key(key) => current.get(key.as_str())?,
            Segment::Index(index) => current.get(index)?,
        };
    }
    Some(current)
}

/// The selected value as text: strings without quotes, other values as JSON.
pub fn select_string(value: &Value, path: &str) -> Option<String> {
    match select(value, path)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_values() {
        let response = json!({
            "session": { "accessToken": "abc", "expiresIn": 3600 },
            "items": [{ "id": 7 }],
            "x-meta": { "ok": true }
        });
        assert_eq!(
            select_string(&response, "$.session.accessToken").as_deref(),
            Some("abc")
        );
        assert_eq!(
            select_string(&response, "session.expiresIn").as_deref(),
            Some("3600")
        );
        assert_eq!(
            select_string(&response, "$.items[0].id").as_deref(),
            Some("7")
        );
        assert_eq!(
            select_string(&response, "$['x-meta'].ok").as_deref(),
            Some("true")
        );
        assert_eq!(select(&response, "$.missing"), None);
        assert!(!is_valid("$..token"));
    }
}
