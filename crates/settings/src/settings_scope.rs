use crate::{ParseStatus, RootUserSettings as _, UserSettingsContent};
use anyhow::{Context as _, Result};
use serde_json::{Map, Value};
use settings_json::{infer_json_indent_size, update_value_in_json_text};
use std::path::Path;

/// The JSON paths of the settings each profile keeps to itself (accounts, models, MCP servers
/// and the agent's toolkits), so personal and work don't mix. Everything else is shared by
/// every profile unless a profile overrides it.
pub const PROFILE_SCOPED_SETTINGS: &[&str] = &[
    "language_models",
    "agent_servers",
    "context_servers",
    "agent_toolkits",
    "toolkit",
    "edit_predictions.provider",
    "agent.default_model",
    "agent.subagent_model",
    "agent.inline_assistant_model",
    "agent.commit_message_model",
    "agent.thread_summary_model",
    "agent.compaction_model",
    "agent.inline_alternatives",
    "agent.favorite_models",
    "agent.model_parameters",
    "agent.default_profile",
    "agent.profiles",
    "agent.on_demand_context_servers",
];

/// Where a setting is written: the file every profile shares or the current profile's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsScope {
    Shared,
    Profile,
}

/// Whether the setting at `path` (keys from the root) belongs to each profile.
pub fn is_profile_scoped(path: &[impl AsRef<str>]) -> bool {
    PROFILE_SCOPED_SETTINGS.iter().any(|scoped| {
        let scoped: Vec<&str> = scoped.split('.').collect();
        scoped.len() <= path.len()
            && scoped
                .iter()
                .zip(path.iter())
                .all(|(scoped, key)| *scoped == key.as_ref())
    })
}

/// The scope a setting is written to when nothing says otherwise.
pub fn natural_scope(path: &[impl AsRef<str>]) -> SettingsScope {
    if is_profile_scoped(path) {
        SettingsScope::Profile
    } else {
        SettingsScope::Shared
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    pub path: Vec<String>,
    /// `None` removes the key.
    pub value: Option<Value>,
}

/// The leaves that differ between two settings values. Objects are compared key by key, so a
/// change is never wider than the setting that changed; anything else is a leaf.
pub(crate) fn diff(old: &Value, new: &Value) -> Vec<Change> {
    let mut changes = Vec::new();
    diff_into(old, new, &mut Vec::new(), &mut changes);
    changes
}

fn diff_into(old: &Value, new: &Value, path: &mut Vec<String>, changes: &mut Vec<Change>) {
    let empty = Map::new();
    match (old, new) {
        (Value::Object(old_object), Value::Object(new_object)) => {
            for (key, old_value) in old_object {
                path.push(key.clone());
                match new_object.get(key) {
                    Some(new_value) => diff_into(old_value, new_value, path, changes),
                    None => changes.push(Change {
                        path: path.clone(),
                        value: None,
                    }),
                }
                path.pop();
            }
            for (key, new_value) in new_object {
                if !old_object.contains_key(key) {
                    path.push(key.clone());
                    diff_into(&Value::Null, new_value, path, changes);
                    path.pop();
                }
            }
        }
        (Value::Null, Value::Object(new_object)) if !path.is_empty() => {
            diff_into(
                &Value::Object(empty),
                &Value::Object(new_object.clone()),
                path,
                changes,
            );
        }
        _ if old != new => changes.push(Change {
            path: path.clone(),
            value: Some(new.clone()),
        }),
        _ => {}
    }
}

pub(crate) fn lookup<'a>(value: &'a Value, path: &[impl AsRef<str>]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |value, key| value.as_object()?.get(key.as_ref()))
}

/// Sets or removes the value at the change's path, creating the objects on the way.
pub(crate) fn apply(target: &mut Value, change: &Change) {
    let Some((last, parents)) = change.path.split_last() else {
        return;
    };
    let mut current = target;
    for key in parents {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        let Some(object) = current.as_object_mut() else {
            return;
        };
        current = object
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !current.is_object() {
        *current = Value::Object(Map::new());
    }
    if let Some(object) = current.as_object_mut() {
        match &change.value {
            Some(value) => {
                object.insert(last.clone(), value.clone());
            }
            None => {
                object.remove(last);
            }
        }
    }
}

/// Splits what an update changed between the profile's file and the shared one: a change goes to
/// the profile when the setting is one of each profile's own or the profile already overrides it.
pub(crate) fn route(changes: Vec<Change>, profile: &Value) -> (Vec<Change>, Vec<Change>) {
    changes
        .into_iter()
        .partition(|change| is_profile_scoped(&change.path) || has_path(profile, &change.path))
}

/// Whether `value` sets `path` or something that contains it.
fn has_path(value: &Value, path: &[String]) -> bool {
    let mut current = value;
    for key in path {
        match current.as_object() {
            Some(object) => match object.get(key) {
                Some(next) => current = next,
                None => return false,
            },
            None => return is_set(current),
        }
    }
    is_set(current)
}

/// Serializing the settings writes the collections that aren't optional even when they are
/// empty, so an empty one says nothing about the file.
fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Object(object) => !object.is_empty(),
        Value::Array(array) => !array.is_empty(),
        _ => true,
    }
}

pub(crate) fn parse_settings_value(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let (content, status) = UserSettingsContent::parse_json(text);
    if let ParseStatus::Failed { error } = &status {
        anyhow::bail!("{error}");
    }
    let content = content.context("Settings file could not be parsed")?;
    Ok(serde_json::to_value(content)?)
}

/// The text with `old` turned into `new`, keeping the comments and formatting of what didn't
/// change.
pub(crate) fn text_with_value(text: &str, old: &Value, new: &Value) -> String {
    if text.trim().is_empty() {
        let mut pretty = serde_json::to_string_pretty(new).unwrap_or_else(|_| "{}".to_string());
        pretty.push('\n');
        return pretty;
    }
    let mut text = text.to_string();
    let tab_size = infer_json_indent_size(&text);
    update_value_in_json_text(
        &mut text,
        &mut Vec::new(),
        tab_size,
        old,
        new,
        &mut Vec::new(),
    );
    text
}

/// What [`migrate_to_profile_scopes`] moved, kept so every profile can say it once.
pub fn migration_record_file() -> std::path::PathBuf {
    paths::profiles_dir().join("settings-scopes.json")
}

/// The settings each profile keeps to itself used to live in the default profile's
/// `settings.json`, which every profile inherited. With [`paths::profile_scoped_settings`] that
/// file is the shared one, so they move to the default profile's own file. Runs once for every
/// profile, from whichever starts first; returns the paths it moved.
pub fn migrate_to_profile_scopes() -> Result<Option<Vec<String>>> {
    if !paths::profile_scoped_settings() {
        return Ok(None);
    }
    let record = migration_record_file();
    if record.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(paths::profiles_dir())?;
    let lock = std::fs::File::create(paths::profiles_dir().join("settings-scopes.lock"))?;
    lock.lock()
        .context("waiting for another profile to migrate the settings")?;
    if record.exists() {
        return Ok(None);
    }

    let Some(shared_path) = paths::shared_settings_file() else {
        return Ok(None);
    };
    let default_path = paths::profile_settings_file_for(paths::DEFAULT_PROFILE_ID);
    let moved = move_profile_settings(shared_path, &default_path)?;

    let record_text = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "moved": moved,
    }))?;
    std::fs::write(&record, record_text)?;
    Ok(Some(moved))
}

fn move_profile_settings(shared_path: &Path, default_path: &Path) -> Result<Vec<String>> {
    let shared_text = match std::fs::read_to_string(shared_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let shared_old = parse_settings_value(&shared_text)
        .with_context(|| format!("parsing {}", shared_path.display()))?;

    let mut moved = Vec::new();
    let mut shared_new = shared_old.clone();
    let mut changes = Vec::new();
    for scoped in PROFILE_SCOPED_SETTINGS {
        let path: Vec<String> = scoped.split('.').map(str::to_string).collect();
        if let Some(value) = lookup(&shared_old, &path).filter(|value| is_set(value)) {
            changes.push(Change {
                path: path.clone(),
                value: Some(value.clone()),
            });
            apply(&mut shared_new, &Change { path, value: None });
            moved.push(scoped.to_string());
        }
    }
    if moved.is_empty() {
        return Ok(moved);
    }

    let default_text = match std::fs::read_to_string(default_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let default_old = parse_settings_value(&default_text)
        .with_context(|| format!("parsing {}", default_path.display()))?;
    let mut default_new = default_old.clone();
    for change in &changes {
        if !lookup(&default_old, &change.path).is_some_and(is_set) {
            apply(&mut default_new, change);
        }
    }

    if let Some(parent) = default_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        default_path,
        text_with_value(&default_text, &default_old, &default_new),
    )?;
    if let Some(parent) = shared_path.parent() {
        std::fs::write(parent.join("settings_backup.json"), &shared_text)?;
    }
    std::fs::write(
        shared_path,
        text_with_value(&shared_text, &shared_old, &shared_new),
    )?;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routes_profile_settings_and_existing_overrides_to_the_profile() {
        let profile = json!({ "tabs": { "git_status": true } });
        let updated = json!({
            "tabs": { "git_status": false },
            "theme": "Polar Dark",
            "agent": { "default_model": { "provider": "anthropic", "model": "claude-opus-5-5" } },
        });
        let (to_profile, to_shared) = route(diff(&profile, &updated), &profile);
        let paths = |changes: &[Change]| {
            changes
                .iter()
                .map(|change| change.path.join("."))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&to_profile),
            [
                "tabs.git_status",
                "agent.default_model.provider",
                "agent.default_model.model"
            ]
        );
        assert_eq!(paths(&to_shared), ["theme"]);
    }

    #[test]
    fn moves_profile_settings_out_of_the_shared_file() {
        let directory = tempfile::tempdir().unwrap();
        let shared = directory.path().join("settings.json");
        let default = directory.path().join("default/config/settings.json");
        std::fs::write(
            &shared,
            r#"{
  // the theme stays
  "theme": "Polar Dark",
  "agent_toolkits": { "clickup": false },
  "agent": { "dock": "right", "default_model": { "provider": "anthropic", "model": "claude-opus-5-5" } }
}"#,
        )
        .unwrap();

        let moved = move_profile_settings(&shared, &default).unwrap();
        assert_eq!(moved, ["agent_toolkits", "agent.default_model"]);

        let shared_value =
            parse_settings_value(&std::fs::read_to_string(&shared).unwrap()).unwrap();
        assert_eq!(shared_value["theme"], "Polar Dark");
        assert_eq!(shared_value["agent"]["dock"], "right");
        assert!(!is_set(&shared_value["agent_toolkits"]));
        assert!(!is_set(&shared_value["agent"]["default_model"]));
        let shared_text = std::fs::read_to_string(&shared).unwrap();
        assert!(shared_text.contains("// the theme stays"));
        assert!(!shared_text.contains("agent_toolkits"));
        assert!(!shared_text.contains("default_model"));

        let default_value =
            parse_settings_value(&std::fs::read_to_string(&default).unwrap()).unwrap();
        assert_eq!(default_value["agent_toolkits"]["clickup"], false);
        assert_eq!(
            default_value["agent"]["default_model"]["model"],
            "claude-opus-5-5"
        );
        assert!(directory.path().join("settings_backup.json").exists());
    }
}
