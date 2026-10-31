use anyhow::Result;
use serde_json::Value;

/// Renames the font aliases settings carried over from Zed use to Asylum's.
pub fn rename_zed_font_aliases(value: &mut Value) -> Result<()> {
    rename_in(value);
    Ok(())
}

fn asylum_font_name(name: &str) -> Option<&'static str> {
    match name {
        ".ZedSans" | "Zed Plex Sans" => Some(".AsylumSans"),
        ".ZedMono" | "Zed Plex Mono" => Some(".AsylumMono"),
        _ => None,
    }
}

fn rename_font(value: &mut Value) {
    if let Some(renamed) = value.as_str().and_then(asylum_font_name) {
        *value = Value::String(renamed.to_string());
    }
}

/// Font settings live under many keys (`ui_font_family`, `terminal.font_family`,
/// `agent_ui_font_family`, per-platform and per-profile overrides…), so every key that names a
/// font family or its fallbacks is renamed wherever it appears.
fn rename_in(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key.ends_with("font_family") {
                    rename_font(value);
                } else if key.ends_with("font_fallbacks")
                    && let Value::Array(fallbacks) = value
                {
                    fallbacks.iter_mut().for_each(rename_font);
                } else {
                    rename_in(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(rename_in),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renames_font_aliases_everywhere() {
        let mut value = json!({
            "ui_font_family": ".ZedSans",
            "buffer_font_family": "Zed Plex Mono",
            "buffer_font_fallbacks": [".ZedMono", "Menlo"],
            "terminal": { "font_family": ".ZedMono" },
            "macos": { "agent_ui_font_family": "Zed Plex Sans" },
            "profiles": { "work": { "ui_font_family": ".ZedSans" } },
            "theme": ".ZedSans"
        });
        rename_zed_font_aliases(&mut value).expect("migration should succeed");
        assert_eq!(
            value,
            json!({
                "ui_font_family": ".AsylumSans",
                "buffer_font_family": ".AsylumMono",
                "buffer_font_fallbacks": [".AsylumMono", "Menlo"],
                "terminal": { "font_family": ".AsylumMono" },
                "macos": { "agent_ui_font_family": ".AsylumSans" },
                "profiles": { "work": { "ui_font_family": ".AsylumSans" } },
                "theme": ".ZedSans"
            })
        );
    }

    #[test]
    fn leaves_other_fonts_alone() {
        let mut value =
            json!({ "ui_font_family": "Bear Sans UI", "buffer_font_family": "IBM Plex Mono" });
        let expected = value.clone();
        rename_zed_font_aliases(&mut value).expect("migration should succeed");
        assert_eq!(value, expected);
    }
}
