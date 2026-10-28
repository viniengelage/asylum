use super::tool_permissions::{
    ResolvedProjectPath, canonicalize_worktree_roots, resolve_project_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, anyhow};
use futures::future;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use project::{Project, ProjectPath, Worktree, WorktreeSettings};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::fmt::Write;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;
use util::rel_path::RelPath;

const DEFAULT_DEPTH: usize = 2;
const MAX_DEPTH: usize = 6;
/// Upper bound on files and folders listed, so mapping a huge directory
/// doesn't flood the context.
const MAX_ENTRIES: usize = 400;
/// Files are opened and parsed to read their symbols, so only this many are
/// outlined per call.
const MAX_FILES_WITH_SYMBOLS: usize = 60;
const MAX_FILE_SIZE_FOR_SYMBOLS: u64 = 512 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 12;

/// Shows a compact map of a project directory: its folder tree, with file counts for folders beyond the requested depth, and the top-level symbols (types, functions, modules, ...) of each source file with their line numbers.
///
/// Use it to get oriented in an unfamiliar part of the codebase before reading files. It is much cheaper than listing directories one by one or reading whole files. Then read only the symbols you need with `read_file` and `start_line`/`end_line`.
///
/// Files ignored by Git and files excluded by the user's settings are not shown.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RepoMapToolInput {
    /// The directory to map. The first component must be a root directory of the project.
    ///
    /// <example>
    /// If the project has the root directory `my-project`, map its `src` folder with the path `my-project/src`, or the whole root with `my-project`.
    /// </example>
    pub path: String,
    /// How many folder levels below `path` to expand. Deeper folders are summarized by their file count. Defaults to 2.
    #[serde(default)]
    pub depth: Option<usize>,
}

pub struct RepoMapTool {
    project: Entity<Project>,
}

impl RepoMapTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MapEntry {
    Folder {
        path: Arc<RelPath>,
        level: usize,
        /// Set when the folder is beyond the requested depth and only its
        /// file count is shown.
        collapsed_file_count: Option<usize>,
    },
    File {
        path: Arc<RelPath>,
        level: usize,
        size: u64,
    },
}

struct DirectoryMap {
    entries: Vec<MapEntry>,
    truncated: bool,
}

impl AgentTool for RepoMapTool {
    type Input = RepoMapToolInput;
    type Output = String;

    const NAME: &'static str = "repo_map";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let path = MarkdownInlineCode(&input.path);
            format!("Map {path}").into()
        } else {
            "Map directory".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;
            build_repo_map(project, input, cx)
                .await
                .map_err(|error| format!("{error:#}"))
        })
    }
}

async fn build_repo_map(
    project: Entity<Project>,
    input: RepoMapToolInput,
    cx: &mut AsyncApp,
) -> Result<String> {
    let depth = input.depth.unwrap_or(DEFAULT_DEPTH).min(MAX_DEPTH);
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

    let (project_path, map, root_name, path_style) =
        project.read_with(cx, |project, cx| -> Result<_> {
            let project_path = match input.path.as_str() {
                "." | "" | "./" => {
                    let mut worktrees = project.visible_worktrees(cx);
                    let worktree = worktrees.next().context("The project has no root directory")?;
                    anyhow::ensure!(
                        worktrees.next().is_none(),
                        "'.' is ambiguous in multi-root projects. Pass a root directory name instead."
                    );
                    ProjectPath {
                        worktree_id: worktree.read(cx).id(),
                        path: RelPath::empty().into(),
                    }
                }
                path => match resolve_project_path(project, path, &canonical_roots, cx)? {
                    ResolvedProjectPath::Safe(project_path) => project_path,
                    ResolvedProjectPath::SymlinkEscape { .. } => {
                        anyhow::bail!(
                            "{path} resolves outside the project through a symlink. Use `list_directory` for it instead."
                        )
                    }
                },
            };

            let worktree = project
                .worktree_for_id(project_path.worktree_id, cx)
                .with_context(|| format!("{} is not in a known worktree", input.path))?;
            let global_settings = WorktreeSettings::get_global(cx);
            let worktree_settings = WorktreeSettings::get(Some((&project_path).into()), cx);
            let is_hidden_by_settings = |path: &RelPath| {
                global_settings.is_path_excluded(path)
                    || global_settings.is_path_private(path)
                    || worktree_settings.is_path_excluded(path)
                    || worktree_settings.is_path_private(path)
            };
            if is_hidden_by_settings(&project_path.path) {
                anyhow::bail!(
                    "Cannot map {} because the user's settings exclude it from the agent",
                    input.path
                );
            }

            let worktree = worktree.read(cx);
            let entry = worktree
                .entry_for_path(&project_path.path)
                .ok_or_else(|| anyhow!("Path not found: {}", input.path))?;
            if !entry.is_dir() {
                anyhow::bail!(
                    "{} is not a directory. Use `read_file` to see a file's outline.",
                    input.path
                );
            }

            let map = map_directory(worktree, &project_path.path, depth, &is_hidden_by_settings);
            Ok((
                project_path,
                map,
                worktree.root_name().into_arc(),
                worktree.path_style(),
            ))
        })?;

    let languages = project.read_with(cx, |project, _| project.languages().clone());
    let symbol_candidates = map
        .entries
        .iter()
        .filter_map(|entry| match entry {
            MapEntry::File { path, size, .. } if *size <= MAX_FILE_SIZE_FOR_SYMBOLS => {
                let abs_like_path = path.as_std_path();
                languages
                    .language_for_file_path(abs_like_path)
                    .map(|_| path.clone())
            }
            _ => None,
        })
        .take(MAX_FILES_WITH_SYMBOLS)
        .collect::<Vec<_>>();
    let skipped_symbol_files = map
        .entries
        .iter()
        .filter(|entry| matches!(entry, MapEntry::File { .. }))
        .count()
        .saturating_sub(symbol_candidates.len());

    let symbols = future::join_all(symbol_candidates.iter().map(|path| {
        let project = project.clone();
        let project_path = ProjectPath {
            worktree_id: project_path.worktree_id,
            path: path.clone(),
        };
        let mut cx = cx.clone();
        async move {
            let symbols = file_symbols(project, project_path, &mut cx).await.ok()?;
            Some((path.clone(), symbols))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect::<collections::HashMap<_, _>>();

    let base = &project_path.path;
    let mut output = String::new();
    let display_root = root_name.join(base);
    writeln!(output, "# Map of {}", display_root.display(path_style))?;
    for entry in &map.entries {
        match entry {
            MapEntry::Folder {
                path,
                level,
                collapsed_file_count,
            } => {
                let name = path.file_name().unwrap_or_default();
                let indent = "  ".repeat(*level);
                match collapsed_file_count {
                    Some(count) => writeln!(output, "{indent}{name}/ ({count} files)")?,
                    None => writeln!(output, "{indent}{name}/")?,
                }
            }
            MapEntry::File { path, level, .. } => {
                let name = path.file_name().unwrap_or_default();
                let indent = "  ".repeat(*level);
                match symbols.get(path).filter(|symbols| !symbols.is_empty()) {
                    Some(symbols) => writeln!(output, "{indent}{name}: {symbols}")?,
                    None => writeln!(output, "{indent}{name}")?,
                }
            }
        }
    }
    if map.entries.is_empty() {
        writeln!(output, "(empty)")?;
    }
    if map.truncated {
        writeln!(
            output,
            "\nOnly the first {MAX_ENTRIES} entries are shown. Map a subfolder to see the rest."
        )?;
    }
    if skipped_symbol_files > 0 && symbol_candidates.len() == MAX_FILES_WITH_SYMBOLS {
        writeln!(
            output,
            "\nSymbols are shown for the first {MAX_FILES_WITH_SYMBOLS} source files only. Map a subfolder to see symbols for the rest."
        )?;
    }
    Ok(output)
}

fn map_directory(
    worktree: &Worktree,
    root: &RelPath,
    depth: usize,
    is_hidden_by_settings: &dyn Fn(&RelPath) -> bool,
) -> DirectoryMap {
    let mut map = DirectoryMap {
        entries: Vec::new(),
        truncated: false,
    };
    map_children(worktree, root, 0, depth, is_hidden_by_settings, &mut map);
    map
}

fn map_children(
    worktree: &Worktree,
    directory: &RelPath,
    level: usize,
    depth: usize,
    is_hidden_by_settings: &dyn Fn(&RelPath) -> bool,
    map: &mut DirectoryMap,
) {
    let mut children = worktree
        .child_entries(directory)
        .filter(|entry| !entry.is_ignored && !entry.is_private)
        .filter(|entry| !is_hidden_by_settings(&entry.path))
        .collect::<Vec<_>>();
    // Folders first, so a file's symbols never separate a folder from its
    // siblings.
    children.sort_by_key(|entry| (!entry.is_dir(), entry.path.clone()));

    for child in children {
        if map.entries.len() >= MAX_ENTRIES {
            map.truncated = true;
            return;
        }
        if child.is_dir() {
            if level < depth {
                map.entries.push(MapEntry::Folder {
                    path: child.path.clone(),
                    level,
                    collapsed_file_count: None,
                });
                map_children(
                    worktree,
                    &child.path,
                    level + 1,
                    depth,
                    is_hidden_by_settings,
                    map,
                );
            } else {
                map.entries.push(MapEntry::Folder {
                    path: child.path.clone(),
                    level,
                    collapsed_file_count: Some(count_files(
                        worktree,
                        &child.path,
                        is_hidden_by_settings,
                    )),
                });
            }
        } else {
            map.entries.push(MapEntry::File {
                path: child.path.clone(),
                level,
                size: child.size,
            });
        }
    }
}

fn count_files(
    worktree: &Worktree,
    directory: &RelPath,
    is_hidden_by_settings: &dyn Fn(&RelPath) -> bool,
) -> usize {
    worktree
        .child_entries(directory)
        .filter(|entry| !entry.is_ignored && !entry.is_private)
        .filter(|entry| !is_hidden_by_settings(&entry.path))
        .map(|entry| {
            if entry.is_dir() {
                count_files(worktree, &entry.path, is_hidden_by_settings)
            } else {
                1
            }
        })
        .sum()
}

async fn file_symbols(
    project: Entity<Project>,
    project_path: ProjectPath,
    cx: &mut AsyncApp,
) -> Result<String> {
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await?;
    buffer
        .read_with(cx, |buffer, _| buffer.parsing_idle())
        .await;
    Ok(buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let items = snapshot
            .outline(None)
            .items
            .into_iter()
            .filter(|item| item.depth == 0)
            .map(|item| {
                let item = item.to_point(&snapshot);
                format!("{} [L{}]", item.text, item.range.start.row + 1)
            })
            .collect::<Vec<_>>();
        format_symbols(items)
    }))
}

fn format_symbols(items: Vec<String>) -> String {
    let extra = items.len().saturating_sub(MAX_SYMBOLS_PER_FILE);
    let mut symbols = items
        .into_iter()
        .take(MAX_SYMBOLS_PER_FILE)
        .collect::<Vec<_>>()
        .join(", ");
    if extra > 0 {
        write!(symbols, ", +{extra} more").ok();
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn run_tool(
        project: Entity<Project>,
        input: RepoMapToolInput,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(RepoMapTool::new(project));
        cx.update(|cx| {
            tool.run(
                ToolInput::resolved(input),
                ToolCallEventStream::test().0,
                cx,
            )
        })
        .await
    }

    #[gpui::test]
    async fn test_repo_map_lists_tree_and_symbols(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".gitignore": "target\n",
                "Cargo.toml": "[package]",
                "src": {
                    "lib.rs": "struct Config {}\n\nfn load() {}\n",
                    "deep": {
                        "nested": {
                            "a.rs": "fn a() {}",
                            "b.rs": "fn b() {}",
                        }
                    }
                },
                "target": {
                    "debug": { "build.rs": "fn ignored() {}" }
                }
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language::rust_lang())
        });
        cx.run_until_parked();

        let output = run_tool(
            project,
            RepoMapToolInput {
                path: "root".to_string(),
                depth: Some(2),
            },
            cx,
        )
        .await
        .expect("repo_map should succeed");

        assert_eq!(
            output,
            "# Map of root\n\
             src/\n  \
               deep/\n    \
                 nested/ (2 files)\n  \
               lib.rs: struct Config [L1], fn load [L3]\n\
             .gitignore\n\
             Cargo.toml\n"
        );
    }

    #[gpui::test]
    async fn test_repo_map_rejects_files(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "main.rs": "fn main() {}" }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;

        let error = run_tool(
            project,
            RepoMapToolInput {
                path: "root/main.rs".to_string(),
                depth: None,
            },
            cx,
        )
        .await
        .expect_err("mapping a file should fail");

        assert!(error.contains("is not a directory"), "{error}");
    }

    #[test]
    fn test_format_symbols_limits_the_list() {
        let items = (0..15).map(|index| format!("fn f{index}")).collect();

        let symbols = format_symbols(items);

        assert!(symbols.starts_with("fn f0, fn f1"));
        assert!(symbols.ends_with("fn f11, +3 more"));
    }
}
