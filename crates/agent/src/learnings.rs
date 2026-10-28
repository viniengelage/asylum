//! Learnings are short Markdown notes about a project or the user's
//! preferences that are shown to the model in every thread, so a new session
//! starts from what earlier sessions already worked out.
//!
//! Project learnings live in `.zed/learnings/` and are committed with the
//! code. Personal learnings live in the Zed config directory and apply to
//! every project.

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{AsyncApp, Entity};
use project::{Project, Worktree, WorktreeId};
use prompt_store::LearningContext;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use util::rel_path::RelPath;

pub const PROJECT_LEARNINGS_DIR: &str = ".zed/learnings";
const LEARNING_FILE_EXTENSION: &str = "md";
/// Learnings are meant to be a few lines each; larger files are skipped so a
/// stray document doesn't end up in every prompt.
pub const MAX_LEARNING_FILE_SIZE: usize = 8 * 1024;
/// Total size of learning text included in the system prompt. Learnings past
/// the budget are listed by location only.
const MAX_LEARNINGS_PROMPT_BYTES: usize = 16 * 1024;

pub static PROJECT_LEARNINGS_PREFIX: LazyLock<Option<Arc<RelPath>>> = LazyLock::new(|| {
    RelPath::from_unix_str(PROJECT_LEARNINGS_DIR)
        .ok()
        .map(|path| path.into_arc())
});

pub fn personal_learnings_dir() -> PathBuf {
    paths::config_dir().join("learnings")
}

pub struct LoadedLearning {
    pub location: String,
    pub text: String,
}

/// Loads `.zed/learnings/*.md` from the given worktrees. Callers must only
/// pass trusted worktrees: learnings go straight into the system prompt.
pub async fn load_project_learnings(
    project: &Entity<Project>,
    worktrees: &[(Entity<Worktree>, WorktreeId, Arc<str>)],
    cx: &mut AsyncApp,
) -> Vec<LoadedLearning> {
    let Some(prefix) = PROJECT_LEARNINGS_PREFIX.as_ref() else {
        return Vec::new();
    };

    let mut learnings = Vec::new();
    for (worktree, worktree_id, root_name) in worktrees {
        let expanded = async {
            if let Some(parent) = prefix.parent() {
                crate::expand_worktree_directory(worktree, parent, cx).await?;
            }
            crate::expand_worktree_directory(worktree, prefix, cx).await
        };
        if let Err(error) = expanded.await {
            log::warn!("Failed to scan {root_name}/{PROJECT_LEARNINGS_DIR}: {error:#}");
            continue;
        }

        let mut files = worktree.read_with(cx, |worktree, _| {
            worktree
                .child_entries(prefix)
                .filter(|entry| {
                    entry.is_file()
                        && entry.path.extension() == Some(LEARNING_FILE_EXTENSION)
                        && entry.size <= MAX_LEARNING_FILE_SIZE as u64
                })
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        });
        files.sort();

        for path in files {
            let location = format!("{root_name}/{}", path.as_unix_str());
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer((*worktree_id, path.clone()), cx)
                })
                .await;
            match buffer {
                Ok(buffer) => {
                    let text = buffer.read_with(cx, |buffer, _| buffer.text());
                    learnings.push(LoadedLearning {
                        location,
                        text: text.trim().to_string(),
                    });
                }
                Err(error) => log::warn!("Failed to read learning {location}: {error:#}"),
            }
        }
    }
    learnings
}

pub async fn load_personal_learnings(fs: &dyn Fs, directory: &Path) -> Vec<LoadedLearning> {
    let Ok(mut entries) = fs.read_dir(directory).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        match entry {
            Ok(path)
                if path.extension().and_then(|extension| extension.to_str())
                    == Some(LEARNING_FILE_EXTENSION) =>
            {
                paths.push(path)
            }
            Ok(_) => {}
            Err(error) => log::warn!("Failed to list personal learnings: {error:#}"),
        }
    }
    paths.sort();

    let mut learnings = Vec::new();
    for path in paths {
        match fs.load(&path).await {
            Ok(text) if text.len() <= MAX_LEARNING_FILE_SIZE => learnings.push(LoadedLearning {
                location: path.display().to_string(),
                text: text.trim().to_string(),
            }),
            Ok(_) => log::warn!("Skipping personal learning {path:?}: file is too large"),
            Err(error) => log::warn!("Failed to read personal learning {path:?}: {error:#}"),
        }
    }
    learnings
}

/// Personal learnings come first because, unlike project learnings, the
/// model can't read them with project tools when they are left out.
pub fn select_prompt_learnings(
    personal: Vec<LoadedLearning>,
    project: Vec<LoadedLearning>,
) -> Vec<LearningContext> {
    let mut remaining_budget = MAX_LEARNINGS_PROMPT_BYTES;
    personal
        .into_iter()
        .chain(project)
        .filter(|learning| !learning.text.is_empty())
        .map(|learning| {
            let text = if learning.text.len() <= remaining_budget {
                remaining_budget -= learning.text.len();
                Some(learning.text)
            } else {
                remaining_budget = 0;
                None
            };
            LearningContext {
                location: learning.location,
                text,
            }
        })
        .collect()
}

/// Turns a learning's title into its file name, so saving a learning with
/// the same title replaces it.
pub fn learning_file_name(title: &str) -> Result<String> {
    let mut slug = String::with_capacity(title.len());
    for character in title.trim().chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = truncate_on_char_boundary(slug, 60).trim_end_matches('-');
    anyhow::ensure!(
        !slug.is_empty(),
        "The learning title must contain letters or digits"
    );
    Ok(format!("{slug}.{LEARNING_FILE_EXTENSION}"))
}

fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn render_learning(title: &str, content: &str) -> String {
    format!("# {}\n\n{}\n", title.trim(), content.trim())
}

pub async fn save_learning_file(fs: &dyn Fs, path: &Path, text: &str) -> Result<()> {
    let directory = path.parent().context("learning path has no parent")?;
    fs.create_dir(directory).await?;
    fs.atomic_write(path.to_path_buf(), text.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learning(location: &str, text: &str) -> LoadedLearning {
        LoadedLearning {
            location: location.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn test_learning_file_name() {
        assert_eq!(
            learning_file_name("Use `cx.notify()` after updates!").ok(),
            Some("use-cx-notify-after-updates.md".to_string())
        );
        assert_eq!(
            learning_file_name("Não usar git apply -U0").ok(),
            Some("não-usar-git-apply-u0.md".to_string())
        );
        assert!(learning_file_name("  ---  ").is_err());
        let long_name = learning_file_name(&"a".repeat(100)).expect("valid title");
        assert_eq!(long_name.len(), 60 + ".md".len());
    }

    #[test]
    fn test_select_prompt_learnings_respects_budget() {
        let big = "x".repeat(MAX_LEARNINGS_PROMPT_BYTES - 10);
        let selected = select_prompt_learnings(
            vec![learning("~/personal.md", &big)],
            vec![
                learning("root/.zed/learnings/a.md", "short"),
                learning("root/.zed/learnings/b.md", "this one does not fit"),
                learning("root/.zed/learnings/empty.md", ""),
            ],
        );

        assert_eq!(
            selected,
            vec![
                LearningContext {
                    location: "~/personal.md".to_string(),
                    text: Some(big),
                },
                LearningContext {
                    location: "root/.zed/learnings/a.md".to_string(),
                    text: Some("short".to_string()),
                },
                LearningContext {
                    location: "root/.zed/learnings/b.md".to_string(),
                    text: None,
                },
            ]
        );
    }
}
