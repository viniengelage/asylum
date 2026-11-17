use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use collections::HashSet;
use fs::Fs;
use gpui::{App, AppContext as _, AsyncWindowContext, SharedString, Task, WeakEntity};
use project::{Project, TaskContexts, TaskSourceKind, WorktreeId};
use serde::Deserialize;
use task::{DebugScenario, TaskTemplate};
use workspace::Workspace;

/// Where a run in the picker comes from, shown as the chip next to its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunOrigin {
    PackageScript,
    Eas,
    ProjectTasks,
    VsCodeTasks,
    GlobalTasks,
    Debug,
}

impl RunOrigin {
    pub fn chip_label(self) -> SharedString {
        match self {
            RunOrigin::PackageScript => "package.json".into(),
            RunOrigin::Eas => "eas.json".into(),
            RunOrigin::ProjectTasks => ".zed/tasks.json".into(),
            RunOrigin::VsCodeTasks => ".vscode/tasks.json".into(),
            RunOrigin::GlobalTasks => i18n::t!("runs.origin.global"),
            RunOrigin::Debug => "debug.json".into(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            RunOrigin::PackageScript => "package",
            RunOrigin::Eas => "eas",
            RunOrigin::ProjectTasks => "tasks",
            RunOrigin::VsCodeTasks => "vscode",
            RunOrigin::GlobalTasks => "global",
            RunOrigin::Debug => "debug",
        }
    }
}

#[derive(Clone, Debug)]
pub enum RunKind {
    Task {
        source_kind: TaskSourceKind,
        template: TaskTemplate,
    },
    Debug {
        scenario: DebugScenario,
        worktree_id: Option<WorktreeId>,
    },
}

/// Something the picker can run: a task from any of the project's sources, or a debug scenario.
#[derive(Clone, Debug)]
pub struct RunCandidate {
    /// Stable across reloads, what favorites and the active run are stored by.
    pub key: SharedString,
    pub label: SharedString,
    /// The command line, or the adapter for a debug scenario.
    pub detail: SharedString,
    pub origin: RunOrigin,
    pub kind: RunKind,
    /// The tasks file the task was read from, for "Edit".
    pub file: Option<PathBuf>,
}

impl RunCandidate {
    pub fn is_debug(&self) -> bool {
        matches!(self.kind, RunKind::Debug { .. })
    }

    pub fn template(&self) -> Option<&TaskTemplate> {
        match &self.kind {
            RunKind::Task { template, .. } => Some(template),
            RunKind::Debug { .. } => None,
        }
    }
}

pub fn candidate_key(origin: RunOrigin, label: &str) -> SharedString {
    format!("{}:{label}", origin.key()).into()
}

fn command_line(template: &TaskTemplate) -> SharedString {
    std::iter::once(template.command.as_str())
        .chain(template.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

fn task_candidate(
    origin: RunOrigin,
    source_kind: TaskSourceKind,
    template: TaskTemplate,
    file: Option<PathBuf>,
) -> RunCandidate {
    RunCandidate {
        key: candidate_key(origin, &template.label),
        label: template.label.clone().into(),
        detail: if template.command.is_empty() {
            i18n::t!("runs.picker.steps_detail")
        } else {
            command_line(&template)
        },
        origin,
        kind: RunKind::Task {
            source_kind,
            template,
        },
        file,
    }
}

/// The package manager a JavaScript project uses, from its lockfile.
pub async fn package_manager(fs: &dyn Fs, directory: &Path) -> &'static str {
    if fs.is_file(&directory.join("yarn.lock")).await {
        "yarn"
    } else if fs.is_file(&directory.join("pnpm-lock.yaml")).await {
        "pnpm"
    } else if fs.is_file(&directory.join("bun.lockb")).await
        || fs.is_file(&directory.join("bun.lock")).await
    {
        "bun"
    } else {
        "npm"
    }
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    scripts: serde_json::Map<String, serde_json::Value>,
}

pub fn package_scripts(package_json: &str) -> Vec<String> {
    serde_json::from_str::<PackageJson>(package_json)
        .map(|package| package.scripts.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn package_script_template(
    package_manager: &str,
    script: &str,
    directory: &Path,
) -> TaskTemplate {
    let args = match package_manager {
        "yarn" => vec![script.to_string()],
        _ => vec!["run".to_string(), script.to_string()],
    };
    TaskTemplate {
        label: script.to_string(),
        command: package_manager.to_string(),
        args,
        cwd: Some(directory.to_string_lossy().into_owned()),
        use_new_terminal: true,
        tags: vec!["package-script".to_string()],
        ..TaskTemplate::default()
    }
}

#[derive(Deserialize)]
struct EasJson {
    #[serde(default)]
    build: serde_json::Map<String, serde_json::Value>,
}

/// The build profiles an `eas.json` declares, in file order.
pub fn eas_build_profiles(eas_json: &str) -> Vec<String> {
    serde_json::from_str::<EasJson>(eas_json)
        .map(|eas| eas.build.keys().cloned().collect())
        .unwrap_or_default()
}

pub const EAS_PLATFORMS: [&str; 2] = ["ios", "android"];

pub fn eas_build_template(profile: &str, platform: &str, directory: &Path) -> TaskTemplate {
    TaskTemplate {
        label: format!("eas build · {profile} · {platform}"),
        command: "npx".to_string(),
        args: vec![
            "eas-cli".to_string(),
            "build".to_string(),
            "--profile".to_string(),
            profile.to_string(),
            "--platform".to_string(),
            platform.to_string(),
        ],
        cwd: Some(directory.to_string_lossy().into_owned()),
        use_new_terminal: true,
        ..TaskTemplate::default()
    }
}

fn asylum_source_kind(id_base: &'static str, abs_path: PathBuf) -> TaskSourceKind {
    TaskSourceKind::AbsPath {
        id_base: Cow::Borrowed(id_base),
        abs_path,
    }
}

/// Everything the picker offers for the workspace: tasks.json (project, .vscode and global),
/// the package.json scripts and eas.json profiles of each project root, and debug scenarios.
pub fn load_candidates(
    workspace: WeakEntity<Workspace>,
    task_contexts: Arc<TaskContexts>,
    cx: &mut AsyncWindowContext,
) -> Task<Vec<RunCandidate>> {
    let Ok((fs, roots, inventory_tasks, debug_scenarios)) =
        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().read(cx);
            let fs = project.fs().clone();
            let roots: Vec<PathBuf> = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect();
            let inventory = project.task_store().read(cx).task_inventory().cloned();
            let (tasks, scenarios) = match inventory {
                Some(inventory) => {
                    let tasks =
                        inventory
                            .read(cx)
                            .list_tasks(None, None, task_contexts.worktree(), cx);
                    let scenarios = inventory.update(cx, |inventory, cx| {
                        inventory.list_debug_scenarios(
                            &task_contexts,
                            Vec::new(),
                            Vec::new(),
                            false,
                            cx,
                        )
                    });
                    (tasks, scenarios)
                }
                None => (
                    Task::ready(Vec::new()),
                    Task::ready((Vec::new(), Vec::new())),
                ),
            };
            (fs, roots, tasks, scenarios)
        })
    else {
        return Task::ready(Vec::new());
    };
    let worktree_id = task_contexts.worktree();
    let worktree_roots: Vec<(WorktreeId, PathBuf)> = workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| {
                    let worktree = worktree.read(cx);
                    (worktree.id(), worktree.abs_path().to_path_buf())
                })
                .collect()
        })
        .unwrap_or_default();
    cx.background_spawn(async move {
        let mut candidates = Vec::new();
        let mut seen = HashSet::default();
        for root in &roots {
            let package_json = root.join("package.json");
            if let Some(text) = fs.load(&package_json).await.ok() {
                let manager = package_manager(fs.as_ref(), root).await;
                for script in package_scripts(&text) {
                    let template = package_script_template(manager, &script, root);
                    let candidate = task_candidate(
                        RunOrigin::PackageScript,
                        asylum_source_kind("asylum_package_json", package_json.clone()),
                        template,
                        Some(package_json.clone()),
                    );
                    if seen.insert(candidate.key.clone()) {
                        candidates.push(candidate);
                    }
                }
            }
            let eas_json = root.join("eas.json");
            if let Some(text) = fs.load(&eas_json).await.ok() {
                for profile in eas_build_profiles(&text) {
                    for platform in EAS_PLATFORMS {
                        let template = eas_build_template(&profile, platform, root);
                        let candidate = task_candidate(
                            RunOrigin::Eas,
                            asylum_source_kind("asylum_eas_json", eas_json.clone()),
                            template,
                            Some(eas_json.clone()),
                        );
                        if seen.insert(candidate.key.clone()) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
        for (source_kind, template) in inventory_tasks.await {
            if template.tags.iter().any(|tag| tag == "package-script") {
                continue;
            }
            let (origin, file) = match &source_kind {
                TaskSourceKind::Worktree {
                    id,
                    directory_in_worktree,
                    ..
                } => {
                    let directory = directory_in_worktree.as_unix_str().to_string();
                    let origin = if directory.ends_with(".vscode") {
                        RunOrigin::VsCodeTasks
                    } else {
                        RunOrigin::ProjectTasks
                    };
                    let file = worktree_roots
                        .iter()
                        .find(|(worktree_id, _)| worktree_id == id)
                        .map(|(_, root)| root.join(&directory).join("tasks.json"));
                    (origin, file)
                }
                TaskSourceKind::AbsPath { abs_path, .. } => {
                    (RunOrigin::GlobalTasks, Some(abs_path.clone()))
                }
                _ => continue,
            };
            let candidate = task_candidate(origin, source_kind, template, file);
            if seen.insert(candidate.key.clone()) {
                candidates.push(candidate);
            }
        }
        let (_, scenarios) = debug_scenarios.await;
        for (_, scenario) in scenarios {
            let candidate = RunCandidate {
                key: candidate_key(RunOrigin::Debug, &scenario.label),
                label: scenario.label.clone(),
                detail: scenario.adapter.clone(),
                origin: RunOrigin::Debug,
                kind: RunKind::Debug {
                    scenario,
                    worktree_id,
                },
                file: None,
            };
            if seen.insert(candidate.key.clone()) {
                candidates.push(candidate);
            }
        }
        candidates
    })
}

/// The first visible worktree's root, which per-project state (favorites, the active run) is
/// keyed by.
pub fn project_key(project: &Project, cx: &App) -> Option<String> {
    project
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_eas_build_profiles_in_order() {
        let eas = r#"{
            "cli": { "version": ">= 5.0.0" },
            "build": {
                "development": { "developmentClient": true, "distribution": "internal" },
                "preview": { "distribution": "internal" },
                "production": {}
            },
            "submit": { "production": {} }
        }"#;
        assert_eq!(
            eas_build_profiles(eas),
            vec!["development", "preview", "production"]
        );
        assert!(eas_build_profiles("{}").is_empty());
        assert!(eas_build_profiles("not json").is_empty());
    }

    #[test]
    fn builds_eas_command() {
        let template = eas_build_template("preview", "ios", Path::new("/project"));
        assert_eq!(template.command, "npx");
        assert_eq!(
            template.args,
            vec![
                "eas-cli",
                "build",
                "--profile",
                "preview",
                "--platform",
                "ios"
            ]
        );
        assert_eq!(template.cwd.as_deref(), Some("/project"));
    }

    #[test]
    fn package_scripts_use_the_lockfile_manager() {
        let scripts =
            package_scripts(r#"{ "scripts": { "start": "expo start", "ios": "expo run:ios" } }"#);
        assert_eq!(scripts, vec!["start", "ios"]);
        let yarn = package_script_template("yarn", "ios", Path::new("/p"));
        assert_eq!(yarn.args, vec!["ios"]);
        let npm = package_script_template("npm", "ios", Path::new("/p"));
        assert_eq!(npm.args, vec!["run", "ios"]);
    }
}
