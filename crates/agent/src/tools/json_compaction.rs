//! Shrinks large JSON tool results before they are sent to the model.
//!
//! Long arrays are cut down to their first and last items plus any item that
//! looks like an error, and very long strings are shortened. The model is told
//! how much was left out so it can ask for a narrower range when it needs the
//! rest.

use crate::ToolCallEventStream;
use gpui::AsyncApp;
use serde_json::Value;

/// Small payloads are left untouched: there is little to save and the model
/// may need every field.
const MIN_BYTES_TO_COMPACT: usize = 2 * 1024;
const MAX_ARRAY_ITEMS: usize = 30;
const KEPT_HEAD_ITEMS: usize = 10;
const KEPT_TAIL_ITEMS: usize = 3;
const MAX_KEPT_ERROR_ITEMS: usize = 10;
const MAX_STRING_BYTES: usize = 4 * 1024;
const KEPT_STRING_BYTES: usize = 1024;

const ERROR_KEYS: &[&str] = &["error", "errors", "exception", "failure"];
const LEVEL_KEYS: &[&str] = &["level", "severity", "status", "state", "result", "outcome"];
const ERROR_LEVELS: &[&str] = &[
    "error", "fatal", "critical", "failed", "failure", "fail", "panic",
];
const SUCCESS_KEYS: &[&str] = &["success", "ok", "passed"];
const STATUS_CODE_KEYS: &[&str] = &["status", "status_code", "statuscode", "code"];

#[derive(Debug)]
pub(super) struct CompactedJson {
    pub text: String,
    pub omitted_items: usize,
    pub shortened_strings: usize,
}

impl CompactedJson {
    /// Explains what was left out, or `None` when the JSON was only
    /// re-serialized without whitespace.
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.omitted_items > 0 {
            parts.push(format!(
                "{} items of long arrays were omitted (the first and last items and any items that look like errors were kept)",
                self.omitted_items
            ));
        }
        if self.shortened_strings > 0 {
            parts.push(format!(
                "{} very long strings were shortened",
                self.shortened_strings
            ));
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "(This JSON was compacted to save context: {}. If you need the omitted data, request a narrower range, e.g. with pagination or filters.)",
            parts.join("; ")
        ))
    }
}

/// The copy of a tool's text result to send to the model: compacted, with a
/// note about what was left out, when it is large JSON.
pub(super) fn model_text(text: &str, event_stream: &ToolCallEventStream, cx: &AsyncApp) -> String {
    let Some(compacted) = compact_json(text) else {
        return text.to_string();
    };
    event_stream.record_compacted_output(text.len().saturating_sub(compacted.text.len()), cx);
    match compacted.note() {
        Some(note) => format!("{}\n\n{note}", compacted.text),
        None => compacted.text,
    }
}

/// Returns `None` when `text` isn't JSON or compacting it wouldn't remove
/// anything.
pub(super) fn compact_json(text: &str) -> Option<CompactedJson> {
    if text.len() < MIN_BYTES_TO_COMPACT {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(text.trim()).ok()?;
    if !matches!(value, Value::Array(_) | Value::Object(_)) {
        return None;
    }

    let mut compacted = CompactedJson {
        text: String::new(),
        omitted_items: 0,
        shortened_strings: 0,
    };
    compact_value(&mut value, &mut compacted);
    compacted.text = serde_json::to_string(&value).ok()?;

    let removed_content = compacted.omitted_items > 0 || compacted.shortened_strings > 0;
    // Re-serializing pretty-printed JSON compactly already saves tokens, so
    // it's worth returning even when no content was removed.
    let saves_bytes = compacted.text.len() + MIN_BYTES_TO_COMPACT / 4 < text.len();
    (removed_content || saves_bytes).then_some(compacted)
}

fn compact_value(value: &mut Value, compacted: &mut CompactedJson) {
    match value {
        Value::Array(items) => {
            if items.len() > MAX_ARRAY_ITEMS {
                shorten_array(items, compacted);
            }
            for item in items.iter_mut() {
                compact_value(item, compacted);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                compact_value(field, compacted);
            }
        }
        Value::String(text) => {
            if text.len() > MAX_STRING_BYTES {
                let mut end = KEPT_STRING_BYTES;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let omitted = text.len() - end;
                text.truncate(end);
                text.push_str(&format!("… [{omitted} bytes omitted]"));
                compacted.shortened_strings += 1;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn shorten_array(items: &mut Vec<Value>, compacted: &mut CompactedJson) {
    let total = items.len();
    let tail_start = total - KEPT_TAIL_ITEMS;
    let mut kept_errors = 0;
    let keep = (0..total)
        .map(|index| {
            if index < KEPT_HEAD_ITEMS || index >= tail_start {
                true
            } else if kept_errors < MAX_KEPT_ERROR_ITEMS && looks_like_error(&items[index]) {
                kept_errors += 1;
                true
            } else {
                false
            }
        })
        .collect::<Vec<_>>();

    let mut shortened = Vec::with_capacity(KEPT_HEAD_ITEMS + KEPT_TAIL_ITEMS + kept_errors + 4);
    let mut omitted_run = 0;
    for (item, keep) in std::mem::take(items).into_iter().zip(keep) {
        if keep {
            if omitted_run > 0 {
                shortened.push(omission_marker(omitted_run, total));
                omitted_run = 0;
            }
            shortened.push(item);
        } else {
            omitted_run += 1;
            compacted.omitted_items += 1;
        }
    }
    *items = shortened;
}

fn omission_marker(omitted: usize, total: usize) -> Value {
    Value::String(format!("… {omitted} of {total} items omitted …"))
}

fn looks_like_error(item: &Value) -> bool {
    let Value::Object(fields) = item else {
        return false;
    };
    fields.iter().any(|(key, value)| {
        let key = key.to_lowercase();
        let key = key.as_str();
        if ERROR_KEYS.contains(&key) {
            return !matches!(value, Value::Null | Value::Bool(false))
                && value.as_array().is_none_or(|errors| !errors.is_empty())
                && value.as_str().is_none_or(|error| !error.is_empty());
        }
        if SUCCESS_KEYS.contains(&key) && value == &Value::Bool(false) {
            return true;
        }
        if LEVEL_KEYS.contains(&key)
            && let Some(level) = value.as_str()
            && ERROR_LEVELS.contains(&level.to_lowercase().as_str())
        {
            return true;
        }
        STATUS_CODE_KEYS.contains(&key)
            && value
                .as_u64()
                .is_some_and(|status| (400..600).contains(&status))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_small_or_non_json_text_is_untouched() {
        assert!(compact_json("not json").is_none());
        assert!(compact_json(&"x".repeat(5000)).is_none());
        assert!(compact_json(r#"{"a": 1}"#).is_none());
    }

    #[test]
    fn test_long_arrays_keep_head_tail_and_errors() {
        let items = (0..100)
            .map(|index| {
                if index == 50 {
                    json!({ "id": index, "level": "ERROR", "message": "boom" })
                } else if index == 60 {
                    json!({ "id": index, "status": 503 })
                } else {
                    json!({ "id": index, "level": "info", "message": "fine" })
                }
            })
            .collect::<Vec<_>>();
        let text = serde_json::to_string_pretty(&json!({ "items": items })).unwrap();

        let compacted = compact_json(&text).expect("JSON should be compacted");
        let value: Value = serde_json::from_str(&compacted.text).unwrap();
        let items = value["items"].as_array().unwrap();
        let ids = items
            .iter()
            .map(|item| match item {
                Value::String(marker) => marker.clone(),
                item => item["id"].to_string(),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "0",
                "1",
                "2",
                "3",
                "4",
                "5",
                "6",
                "7",
                "8",
                "9",
                "… 40 of 100 items omitted …",
                "50",
                "… 9 of 100 items omitted …",
                "60",
                "… 36 of 100 items omitted …",
                "97",
                "98",
                "99",
            ]
        );
        assert_eq!(compacted.omitted_items, 85);
        assert!(
            compacted
                .note()
                .is_some_and(|note| note.contains("85 items"))
        );
    }

    #[test]
    fn test_long_strings_are_shortened() {
        let text = serde_json::to_string(&json!({ "log": "é".repeat(5000) })).unwrap();

        let compacted = compact_json(&text).expect("JSON should be compacted");

        assert_eq!(compacted.shortened_strings, 1);
        assert!(compacted.text.contains("bytes omitted]"));
        assert!(compacted.text.len() < 1200);
    }

    #[test]
    fn test_pretty_json_is_minified_even_without_omissions() {
        let rows = (0..20)
            .map(|index| {
                json!({
                    "id": index,
                    "name": format!("row {index}"),
                    "tags": ["alpha", "beta", "gamma"],
                    "owner": { "name": "someone", "team": "platform" },
                })
            })
            .collect::<Vec<_>>();
        let text = serde_json::to_string_pretty(&json!({ "rows": rows })).unwrap();

        let compacted = compact_json(&text).expect("pretty JSON should be minified");

        assert_eq!(compacted.omitted_items, 0);
        assert_eq!(compacted.note(), None);
        assert_eq!(
            serde_json::from_str::<Value>(&compacted.text).unwrap(),
            serde_json::from_str::<Value>(&text).unwrap()
        );
    }

    #[test]
    fn test_error_detection() {
        assert!(looks_like_error(&json!({ "error": "denied" })));
        assert!(looks_like_error(&json!({ "errors": [{ "message": "x" }] })));
        assert!(looks_like_error(&json!({ "success": false })));
        assert!(looks_like_error(&json!({ "statusCode": 404 })));
        assert!(!looks_like_error(&json!({ "error": null })));
        assert!(!looks_like_error(&json!({ "errors": [] })));
        assert!(!looks_like_error(&json!({ "status": "ok" })));
        assert!(!looks_like_error(&json!({ "code": 200 })));
    }
}
