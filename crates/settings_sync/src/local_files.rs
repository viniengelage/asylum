use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use i18n::t;
use serde::{Deserialize, Serialize};

use crate::sync_plan::Hashes;

/// Marks the gist as Asylum's and keeps it from ever being empty, which GitHub rejects.
pub const MANIFEST_NAME: &str = "asylum-sync.json";
pub const MANIFEST_CONTENT: &str = "{\n  \"version\": 1\n}\n";

/// Gists have no folders, so a file inside a synced folder is named `<folder>--<file>`.
const FOLDER_SEPARATOR: &str = "--";

/// Larger files are skipped: everything synced is hand-written configuration.
const MAX_FILE_SIZE: u64 = 1024 * 1024;

enum Location {
    File {
        name: &'static str,
        path: PathBuf,
    },
    Folder {
        prefix: &'static str,
        path: PathBuf,
        suffix: &'static str,
    },
}

fn locations() -> Vec<Location> {
    vec![
        Location::File {
            name: "settings.json",
            path: paths::settings_file().clone(),
        },
        Location::File {
            name: "keymap.json",
            path: paths::keymap_file().clone(),
        },
        Location::File {
            name: "tasks.json",
            path: paths::tasks_file().clone(),
        },
        Location::File {
            name: "debug.json",
            path: paths::debug_scenarios_file().clone(),
        },
        Location::File {
            name: "AGENTS.md",
            path: paths::agents_file().clone(),
        },
        Location::Folder {
            prefix: "snippets",
            path: paths::snippets_dir().clone(),
            suffix: ".json",
        },
        Location::Folder {
            prefix: "themes",
            path: paths::themes_dir().clone(),
            suffix: ".json",
        },
        // The same folders as `task_agents::personal_agents_dir` and `flows::personal_flows_dir`,
        // which this crate doesn't depend on to stay out of the title bar's dependency graph.
        Location::Folder {
            prefix: "agents",
            path: paths::data_dir().join("agents"),
            suffix: ".md",
        },
        Location::Folder {
            prefix: "flows",
            path: paths::data_dir().join("flows"),
            suffix: ".flow.md",
        },
    ]
}

/// Reads every synced file that exists and isn't empty, keyed by its name in the gist.
pub fn read_local_files() -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for location in locations() {
        match location {
            Location::File { name, path } => {
                if let Some(content) = read_text_file(&path)? {
                    files.insert(name.to_string(), content);
                }
            }
            Location::Folder {
                prefix,
                path,
                suffix,
            } => {
                let entries = match std::fs::read_dir(&path) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error).with_context(|| reading_error(&path));
                    }
                };
                for entry in entries {
                    let entry = entry?;
                    let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if !is_syncable_file_name(&file_name, suffix) || !entry.file_type()?.is_file() {
                        continue;
                    }
                    if let Some(content) = read_text_file(&entry.path())? {
                        files.insert(format!("{prefix}{FOLDER_SEPARATOR}{file_name}"), content);
                    }
                }
            }
        }
    }
    Ok(files)
}

fn is_syncable_file_name(file_name: &str, suffix: &str) -> bool {
    file_name.ends_with(suffix)
        && !file_name.starts_with('.')
        && !file_name.contains(FOLDER_SEPARATOR)
        && !file_name.contains(['/', '\\'])
}

fn read_text_file(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| reading_error(path)),
    };
    if metadata.len() > MAX_FILE_SIZE {
        log::warn!(
            "settings sync: skipping {} because it is larger than 1 MB",
            path.display()
        );
        return Ok(None);
    }
    let bytes = std::fs::read(path).with_context(|| reading_error(path))?;
    let Ok(content) = String::from_utf8(bytes) else {
        log::warn!(
            "settings sync: skipping {} because it isn't UTF-8",
            path.display()
        );
        return Ok(None);
    };
    Ok((!content.trim().is_empty()).then_some(content))
}

/// Where a file from the gist lives locally, or `None` for names Asylum doesn't sync.
pub fn local_path(name: &str) -> Option<PathBuf> {
    for location in locations() {
        match location {
            Location::File {
                name: file_name,
                path,
            } if file_name == name => return Some(path),
            Location::Folder {
                prefix,
                path,
                suffix,
            } => {
                let Some(file_name) = name
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix(FOLDER_SEPARATOR))
                else {
                    continue;
                };
                if is_syncable_file_name(file_name, suffix) {
                    return Some(path.join(file_name));
                }
            }
            Location::File { .. } => {}
        }
    }
    None
}

/// Writes through a temporary file and a rename, so the settings watchers never read a
/// half-written file.
pub fn write_local_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        t!(
            "settings_sync.error.no_parent_folder",
            path = path.display()
        )
        .to_string()
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary_name = path.file_name().unwrap_or_default().to_os_string();
    temporary_name.push(".sync-tmp");
    let temporary_path = path.with_file_name(temporary_name);
    std::fs::write(&temporary_path, content).with_context(|| writing_error(&temporary_path))?;
    std::fs::rename(&temporary_path, path).with_context(|| writing_error(path))
}

fn reading_error(path: &Path) -> String {
    t!("settings_sync.error.reading", path = path.display()).to_string()
}

fn writing_error(path: &Path) -> String {
    t!("settings_sync.error.writing", path = path.display()).to_string()
}

pub fn delete_local_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error)
            .with_context(|| t!("settings_sync.error.deleting", path = path.display()).to_string()),
        _ => Ok(()),
    }
}

/// Keeps the copy that lost a conflict, so nothing is lost when both sides changed.
pub fn write_backup(name: &str, content: &str, side: &str) -> Result<PathBuf> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = backups_dir().join(format!("{timestamp}-{side}-{name}"));
    write_local_file(&path, content)?;
    Ok(path)
}

pub fn backups_dir() -> PathBuf {
    paths::data_dir().join("settings_sync").join("backups")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncRecord {
    pub gist_id: Option<String>,
    /// The hash of every file as of the last successful sync.
    pub base: Hashes,
}

fn record_path() -> PathBuf {
    paths::data_dir().join("settings_sync").join("state.json")
}

pub fn read_record() -> Result<SyncRecord> {
    match std::fs::read_to_string(record_path()) {
        Ok(content) => serde_json::from_str(&content)
            .with_context(|| t!("settings_sync.error.read_state").to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SyncRecord::default()),
        Err(error) => Err(error).with_context(|| t!("settings_sync.error.read_state").to_string()),
    }
}

pub fn write_record(record: &SyncRecord) -> Result<()> {
    write_local_file(&record_path(), &serde_json::to_string_pretty(record)?)
}

pub fn delete_record() -> Result<()> {
    delete_local_file(&record_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_folder_file_names() {
        assert!(is_syncable_file_name("rust.json", ".json"));
        assert!(is_syncable_file_name("review.flow.md", ".flow.md"));
        assert!(!is_syncable_file_name("review.md", ".flow.md"));
        assert!(!is_syncable_file_name(".hidden.json", ".json"));
        assert!(!is_syncable_file_name("a--b.json", ".json"));
        assert!(!is_syncable_file_name("../escape.json", ".json"));
    }
}
