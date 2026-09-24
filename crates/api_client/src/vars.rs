//! `{{name}}` placeholders in URLs, headers and bodies.

use std::ops::Range;

pub const SECRET_PREFIX: &str = "secret.";

/// The ranges of every `{{…}}` in the text, braces included.
pub fn placeholder_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find("{{") {
        let start = offset + start;
        let Some(end) = text[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + end + 2;
        ranges.push(start..end);
        offset = end;
    }
    ranges
}

pub fn placeholder_names(text: &str) -> Vec<String> {
    placeholder_ranges(text)
        .into_iter()
        .map(|range| text[range.start + 2..range.end - 2].trim().to_string())
        .collect()
}

pub struct Substitution {
    pub text: String,
    /// Placeholders left as they were because nothing defines them.
    pub missing: Vec<String>,
}

pub fn substitute(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Substitution {
    let mut result = String::with_capacity(text.len());
    let mut missing = Vec::new();
    let mut last = 0;
    for range in placeholder_ranges(text) {
        result.push_str(&text[last..range.start]);
        let name = text[range.start + 2..range.end - 2].trim();
        match lookup(name) {
            Some(value) => result.push_str(&value),
            None => {
                if !missing.iter().any(|existing| existing == name) {
                    missing.push(name.to_string());
                }
                result.push_str(&text[range.clone()]);
            }
        }
        last = range.end;
    }
    result.push_str(&text[last..]);
    Substitution {
        text: result,
        missing,
    }
}

/// `{{$uuid}}` and friends, fresh on every request.
pub fn dynamic_value(name: &str) -> Option<String> {
    match name {
        "$uuid" | "$guid" => Some(uuid::Uuid::new_v4().to_string()),
        "$timestamp" => Some(chrono::Utc::now().timestamp().to_string()),
        "$isoTimestamp" => Some(chrono::Utc::now().to_rfc3339()),
        "$randomInt" => Some((uuid::Uuid::new_v4().as_u128() % 1000).to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_known_placeholders() {
        let lookup = |name: &str| match name {
            "baseUrl" => Some("https://api.trix.com.br".to_string()),
            "token" => Some("abc".to_string()),
            _ => None,
        };
        let result = substitute("{{baseUrl}}/users/{{ id }}?t={{token}}{{id}}", &lookup);
        assert_eq!(
            result.text,
            "https://api.trix.com.br/users/{{ id }}?t=abc{{id}}"
        );
        assert_eq!(result.missing, vec!["id".to_string()]);
        assert_eq!(
            placeholder_names("a {{x}} b {{secret.password}}"),
            vec!["x", "secret.password"]
        );
        assert!(dynamic_value("$uuid").is_some());
        assert_eq!(placeholder_ranges("{{unclosed"), Vec::<Range<usize>>::new());
    }
}
