use std::path::{Path, PathBuf};

use anyhow::Result;
use collections::HashSet;
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Task, WeakEntity,
};
use i18n::t;
use project::TaskSourceKind;
use task::{TaskContext, TaskTemplate};
use ui::{
    modal_footer, modal_surface, prelude::*, primary_button, secondary_button, segment_option,
    segmented_control, text_field,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, tasks::ScheduledTaskResult};

use crate::store::ReleasesStore;

const CLEAN_BUILD_IDS_KEY: &str = "app_releases__clean_build_ids";
const CLEAN_BUILD_MESSAGE: &str = "asylum clean build";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildPlatform {
    Ios,
    Android,
    All,
}

impl BuildPlatform {
    fn flag(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanBuildPlan {
    pub repository_root: PathBuf,
    /// Where `app.json` sits, relative to the repository root.
    pub app_subfolder: PathBuf,
    /// `origin/<branch>`, or the local branch when the user chose to build what isn't pushed.
    pub commit_ref: String,
    pub branch: String,
    pub fetch: bool,
    pub profile: String,
    pub platform: BuildPlatform,
    pub local: bool,
    pub clear_cache: bool,
    pub install_command: String,
    pub result_file: PathBuf,
    pub worktrees_dir: PathBuf,
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r#"'\''"#))
}

/// The build profiles in `eas.json`, in file order.
pub fn eas_profiles(eas_json: &str) -> Vec<String> {
    let Some(value) = serde_json_lenient::from_str::<serde_json::Value>(eas_json).log_err() else {
        return Vec::new();
    };
    value
        .get("build")
        .and_then(serde_json::Value::as_object)
        .map(|build| build.keys().cloned().collect())
        .unwrap_or_default()
}

/// A fresh install that fails instead of updating the lockfile, so the build matches the commit.
pub fn install_command(app_folder: &Path) -> String {
    let has = |name: &str| app_folder.join(name).exists();
    if has("bun.lockb") || has("bun.lock") {
        "bun install --frozen-lockfile".into()
    } else if has("pnpm-lock.yaml") {
        "pnpm install --frozen-lockfile".into()
    } else if has("yarn.lock") {
        "yarn install --frozen-lockfile".into()
    } else {
        "npm ci".into()
    }
}

/// The steps as one shell script: a worktree of the commit, a clean install, the build, and the
/// worktree removed only when everything worked (a failure leaves it to look at).
pub fn clean_build_script(plan: &CleanBuildPlan) -> String {
    let mut lines = vec![
        "set -euo pipefail".to_string(),
        format!("cd {}", quote(&plan.repository_root.to_string_lossy())),
    ];
    if plan.fetch {
        lines.push(format!("git fetch origin {}", quote(&plan.branch)));
    }
    lines.push(format!(
        "sha=$(git rev-parse {})",
        quote(&format!("{}^{{commit}}", plan.commit_ref))
    ));
    lines.push(format!(
        "worktree={}/build-${{sha:0:12}}",
        quote(&plan.worktrees_dir.to_string_lossy())
    ));
    lines.push(format!(
        "mkdir -p {}",
        quote(&plan.worktrees_dir.to_string_lossy())
    ));
    lines.push("git worktree remove --force \"$worktree\" 2>/dev/null || true".into());
    lines.push("git worktree add --detach \"$worktree\" \"$sha\"".into());
    lines.push(format!(
        "cd \"$worktree\"/{}",
        quote(&plan.app_subfolder.to_string_lossy())
    ));
    lines.push(plan.install_command.clone());
    let mut build = format!(
        "npx --yes eas-cli build --profile {} --platform {} --non-interactive --json",
        quote(&plan.profile),
        plan.platform.flag()
    );
    if plan.local {
        build.push_str(" --local");
    } else {
        build.push_str(&format!(
            " --no-wait --message \"{CLEAN_BUILD_MESSAGE} $sha\""
        ));
    }
    if plan.clear_cache {
        build.push_str(" --clear-cache");
    }
    build.push_str(&format!(
        " > {}",
        quote(&plan.result_file.to_string_lossy())
    ));
    lines.push(build);
    lines.push(format!(
        "cd {}",
        quote(&plan.repository_root.to_string_lossy())
    ));
    lines.push("git worktree remove --force \"$worktree\"".into());
    lines.join("\n")
}

/// The build ids in `eas build --json` output.
pub fn parse_build_ids(json: &str) -> Vec<String> {
    let Some(value) = serde_json::from_str::<serde_json::Value>(json).log_err() else {
        return Vec::new();
    };
    let builds = match value {
        serde_json::Value::Array(builds) => builds,
        build @ serde_json::Value::Object(_) => vec![build],
        _ => Vec::new(),
    };
    builds
        .iter()
        .filter_map(|build| build.get("id")?.as_str().map(str::to_string))
        .collect()
}

pub fn clean_build_ids(kvp: &KeyValueStore) -> HashSet<String> {
    kvp.read_kvp(CLEAN_BUILD_IDS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err())
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default()
}

async fn remember_clean_builds(kvp: KeyValueStore, ids: Vec<String>) -> Result<()> {
    let mut all = clean_build_ids(&kvp);
    all.extend(ids);
    let json = serde_json::to_string(&all.into_iter().collect::<Vec<_>>())?;
    kvp.write_kvp(CLEAN_BUILD_IDS_KEY.to_string(), json).await
}

async fn confirmed(answer: Result<futures::channel::oneshot::Receiver<usize>>) -> bool {
    match answer {
        Ok(answer) => answer.await.ok() == Some(0),
        Err(_) => false,
    }
}

async fn git(folder: &Path, args: &[&str]) -> Option<String> {
    let output = util::command::new_command("git")
        .args(args)
        .current_dir(folder)
        .output()
        .await
        .log_err()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Clone, Debug)]
struct BranchState {
    repository_root: PathBuf,
    on_origin: bool,
    ahead: usize,
}

async fn branch_state(app_folder: &Path, branch: &str) -> Option<BranchState> {
    let repository_root = PathBuf::from(git(app_folder, &["rev-parse", "--show-toplevel"]).await?);
    // Offline is fine: origin is then judged by what was fetched last.
    git(&repository_root, &["fetch", "origin", branch]).await;
    let remote = format!("origin/{branch}");
    let on_origin = git(&repository_root, &["rev-parse", "--verify", &remote])
        .await
        .is_some();
    let ahead = if on_origin {
        git(
            &repository_root,
            &["rev-list", "--count", &format!("{remote}..{branch}")],
        )
        .await
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
    } else {
        0
    };
    Some(BranchState {
        repository_root,
        on_origin,
        ahead,
    })
}

/// "Nova build limpa": builds on EAS what a branch has on origin, from a throwaway worktree.
pub struct CleanBuildModal {
    workspace: WeakEntity<Workspace>,
    store: Entity<ReleasesStore>,
    app_folder: PathBuf,
    branch: Entity<Editor>,
    profiles: Vec<String>,
    profile: Option<String>,
    platform: BuildPlatform,
    local: bool,
    clear_cache: bool,
    has_expo_account: bool,
    error: Option<SharedString>,
    starting: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl CleanBuildModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<ReleasesStore>,
        app_folder: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch = cx.new(|cx| Editor::single_line(window, cx));
        cx.spawn_in(window, {
            let app_folder = app_folder.clone();
            let branch = branch.clone();
            async move |_, cx| {
                let current = git(&app_folder, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .await
                    .unwrap_or_else(|| "main".to_string());
                branch
                    .update_in(cx, |editor, window, cx| {
                        if editor.text(cx).is_empty() {
                            editor.set_text(current, window, cx);
                        }
                    })
                    .log_err();
            }
        })
        .detach();
        let profiles = std::fs::read_to_string(app_folder.join("eas.json"))
            .map(|text| eas_profiles(&text))
            .unwrap_or_default();
        let profile = profiles
            .iter()
            .find(|profile| profile.as_str() == "preview")
            .or_else(|| profiles.first())
            .cloned();
        let has_expo_account = store
            .read(cx)
            .accounts()
            .read(cx)
            .accounts()
            .is_some_and(|accounts| accounts.expo.is_some());
        Self {
            workspace,
            store,
            app_folder,
            branch,
            profiles,
            profile,
            platform: BuildPlatform::All,
            local: false,
            clear_cache: false,
            has_expo_account,
            error: None,
            starting: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let branch = self.branch.read(cx).text(cx).trim().to_string();
        if branch.is_empty() || self.starting.is_some() {
            return;
        }
        let token = self
            .store
            .read(cx)
            .accounts()
            .read(cx)
            .accounts()
            .and_then(|accounts| accounts.expo.as_ref().map(|expo| expo.token.clone()));
        let Some(token) = token else {
            self.error = Some(t!("app_releases.clean_build.no_expo"));
            cx.notify();
            return;
        };
        let app_folder = self.app_folder.clone();
        let platform = self.platform;
        let local = self.local;
        let clear_cache = self.clear_cache;
        let workspace = self.workspace.clone();
        let store = self.store.clone();
        self.error = None;
        self.starting = Some(cx.spawn_in(window, async move |this, cx| {
            let state = cx
                .background_spawn({
                    let app_folder = app_folder.clone();
                    let branch = branch.clone();
                    async move { branch_state(&app_folder, &branch).await }
                })
                .await;
            let Some(state) = state else {
                this.update(cx, |this, cx| {
                    this.starting = None;
                    this.error = Some(t!("app_releases.clean_build.not_git"));
                    cx.notify();
                })
                .log_err();
                return;
            };
            let mut commit_ref = format!("origin/{branch}");
            let mut fetch = true;
            if !state.on_origin {
                let answer = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &t!(
                            "app_releases.clean_build.local_only",
                            branch = branch.clone()
                        ),
                        Some(&t!("app_releases.clean_build.local_only_detail")),
                        &[
                            t!("app_releases.clean_build.use_local").as_ref(),
                            t!("app_releases.clean_build.cancel").as_ref(),
                        ],
                        cx,
                    )
                });
                if !confirmed(answer).await {
                    this.update(cx, |this, cx| {
                        this.starting = None;
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
                commit_ref = branch.clone();
                fetch = false;
            } else if state.ahead > 0 {
                let answer = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &t!("app_releases.clean_build.ahead", count = state.ahead),
                        Some(&t!("app_releases.clean_build.ahead_detail")),
                        &[
                            t!("app_releases.clean_build.build_origin").as_ref(),
                            t!("app_releases.clean_build.cancel").as_ref(),
                        ],
                        cx,
                    )
                });
                if !confirmed(answer).await {
                    this.update(cx, |this, cx| {
                        this.starting = None;
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            }
            if profile == "production" {
                let answer = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &t!("app_releases.clean_build.production"),
                        Some(&t!("app_releases.clean_build.production_detail")),
                        &[
                            t!("app_releases.clean_build.build_production").as_ref(),
                            t!("app_releases.clean_build.cancel").as_ref(),
                        ],
                        cx,
                    )
                });
                if !confirmed(answer).await {
                    this.update(cx, |this, cx| {
                        this.starting = None;
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            }

            let app_subfolder = app_folder
                .strip_prefix(&state.repository_root)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let result_file = paths::temp_dir().join(format!(
                "asylum-clean-build-{}.json",
                chrono::Utc::now().timestamp()
            ));
            let plan = CleanBuildPlan {
                repository_root: state.repository_root.clone(),
                app_subfolder,
                commit_ref,
                branch: branch.clone(),
                fetch,
                profile: profile.clone(),
                platform,
                local,
                clear_cache,
                install_command: install_command(&app_folder),
                result_file: result_file.clone(),
                worktrees_dir: paths::temp_dir().join("asylum-clean-builds"),
            };
            let mut template = TaskTemplate {
                label: t!(
                    "app_releases.clean_build.task_label",
                    branch = branch.clone(),
                    profile = profile.clone()
                )
                .to_string(),
                command: "bash".to_string(),
                args: vec!["-c".to_string(), clean_build_script(&plan)],
                cwd: Some(state.repository_root.to_string_lossy().into_owned()),
                ..TaskTemplate::default()
            };
            template
                .env
                .insert("EXPO_TOKEN".to_string(), token.to_string());
            let Some(resolved) =
                template.resolve_task("asylum-clean-build", &TaskContext::default())
            else {
                return;
            };
            let kvp = cx.update(|_, cx| KeyValueStore::global(cx)).ok();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.schedule_resolved_task_with_completion(
                        TaskSourceKind::UserInput,
                        resolved,
                        false,
                        move |result, cx| {
                            if result != ScheduledTaskResult::Success {
                                return;
                            }
                            let ids = std::fs::read_to_string(&result_file)
                                .map(|json| parse_build_ids(&json))
                                .unwrap_or_default();
                            std::fs::remove_file(&result_file).log_err();
                            if let Some(kvp) = kvp {
                                cx.background_spawn(async move {
                                    remember_clean_builds(kvp, ids).await.log_err();
                                })
                                .detach();
                            }
                            store.update(cx, |store, cx| store.refresh(cx));
                        },
                        window,
                        cx,
                    );
                })
                .log_err();
            this.update(cx, |_, cx| cx.emit(DismissEvent)).log_err();
        }));
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_platforms(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = [
            (BuildPlatform::All, t!("app_releases.clean_build.both")),
            (BuildPlatform::Ios, SharedString::from("iOS")),
            (BuildPlatform::Android, SharedString::from("Android")),
        ];
        segmented_control(cx).children(options.into_iter().map(|(platform, label)| {
            segment_option(
                SharedString::from(format!("clean-build-platform-{}", platform.flag())),
                label,
                self.platform == platform,
                true,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.platform = platform;
                cx.notify();
            }))
        }))
    }

    fn render_profiles(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.profiles.is_empty() {
            return Label::new(t!("app_releases.clean_build.no_eas_json"))
                .size(LabelSize::Small)
                .color(Color::Warning)
                .into_any_element();
        }
        segmented_control(cx)
            .children(self.profiles.iter().cloned().map(|profile| {
                let selected = self.profile.as_deref() == Some(profile.as_str());
                segment_option(
                    SharedString::from(format!("clean-build-profile-{profile}")),
                    profile.clone(),
                    selected,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.profile = Some(profile.clone());
                    cx.notify();
                }))
            }))
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for CleanBuildModal {}
impl ModalView for CleanBuildModal {}

impl Focusable for CleanBuildModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.branch.focus_handle(cx)
    }
}

fn field(label: SharedString, content: impl IntoElement) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(content)
}

impl Render for CleanBuildModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let branch_focused = self.branch.focus_handle(cx).is_focused(window);
        let can_start = self.has_expo_account && self.profile.is_some() && self.starting.is_none();
        let where_options = [
            (false, t!("app_releases.clean_build.cloud")),
            (true, t!("app_releases.clean_build.local")),
        ];
        modal_surface(cx)
            .key_context("CleanBuildModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .w(px(480.))
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Box).color(Color::Accent))
                            .child(
                                Label::new(t!("app_releases.clean_build.title"))
                                    .weight(gpui::FontWeight::MEDIUM),
                            ),
                    )
                    .child(
                        Label::new(t!("app_releases.clean_build.subtitle"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(field(
                        t!("app_releases.clean_build.branch"),
                        text_field(branch_focused, cx).child(self.branch.clone()),
                    ))
                    .child(field(
                        t!("app_releases.clean_build.profile"),
                        self.render_profiles(cx),
                    ))
                    .child(field(
                        t!("app_releases.clean_build.platform"),
                        self.render_platforms(cx),
                    ))
                    .child(field(
                        t!("app_releases.clean_build.where"),
                        segmented_control(cx).children(where_options.into_iter().map(
                            |(local, label)| {
                                segment_option(
                                    SharedString::from(format!("clean-build-where-{local}")),
                                    label,
                                    self.local == local,
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.local = local;
                                        cx.notify();
                                    },
                                ))
                            },
                        )),
                    ))
                    .child(
                        ui::Checkbox::new(
                            "clean-build-clear-cache",
                            if self.clear_cache {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label(t!("app_releases.clean_build.clear_cache"))
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.clear_cache = state.selected();
                                cx.notify();
                            },
                        )),
                    )
                    .when(!self.has_expo_account, |this| {
                        this.child(
                            Label::new(t!("app_releases.clean_build.no_expo"))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    })
                    .when_some(self.error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }),
            )
            .child(
                modal_footer(cx)
                    .when(!self.has_expo_account, |this| {
                        this.child(
                            secondary_button(
                                "clean-build-connect-expo",
                                None,
                                t!("app_releases.clean_build.connect_expo"),
                                true,
                                cx,
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::OpenAccounts), cx)
                            }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        primary_button(
                            "clean-build-start",
                            Some(IconName::PlayFilled),
                            if self.starting.is_some() {
                                t!("app_releases.clean_build.starting")
                            } else {
                                t!("app_releases.clean_build.start")
                            },
                            None,
                            cx,
                        )
                        .when(!can_start, |this| this.opacity(0.5))
                        .when(can_start, |this| {
                            this.on_click(cx.listener(|this, _, window, cx| this.start(window, cx)))
                        }),
                    ),
            )
    }
}

/// Opens "Nova build limpa" for the dock's selected app.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = ReleasesStore::for_project(workspace.project(), cx);
    let app_folder = store
        .read(cx)
        .selected_app()
        .map(|app| app.folder.to_path_buf())
        .or_else(|| store.read(cx).first_root().map(|root| root.to_path_buf()));
    let Some(app_folder) = app_folder else {
        return;
    };
    let weak_workspace = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        CleanBuildModal::new(weak_workspace, store, app_folder, window, cx)
    });
}

/// Worktrees of builds that stopped halfway (the app closed, or a step failed a while ago).
pub fn remove_stale_worktrees(cx: &mut App) {
    let directory = paths::temp_dir().join("asylum-clean-builds");
    cx.background_spawn(async move {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return;
        };
        let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > week);
            if stale {
                std::fs::remove_dir_all(entry.path()).log_err();
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> CleanBuildPlan {
        CleanBuildPlan {
            repository_root: PathBuf::from("/repo"),
            app_subfolder: PathBuf::from("apps/mobile"),
            commit_ref: "origin/feat/pix".into(),
            branch: "feat/pix".into(),
            fetch: true,
            profile: "preview".into(),
            platform: BuildPlatform::Ios,
            local: false,
            clear_cache: true,
            install_command: "yarn install --frozen-lockfile".into(),
            result_file: PathBuf::from("/tmp/result.json"),
            worktrees_dir: PathBuf::from("/tmp/builds"),
        }
    }

    #[test]
    fn script_builds_origin_in_a_worktree_and_cleans_up_on_success() {
        let script = clean_build_script(&plan());
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines[0], "set -euo pipefail");
        assert!(script.contains("git fetch origin 'feat/pix'"));
        assert!(script.contains("sha=$(git rev-parse 'origin/feat/pix^{commit}')"));
        assert!(script.contains("git worktree add --detach \"$worktree\" \"$sha\""));
        assert!(script.contains("cd \"$worktree\"/'apps/mobile'"));
        assert!(script.contains(
            "npx --yes eas-cli build --profile 'preview' --platform ios --non-interactive --json \
             --no-wait --message \"asylum clean build $sha\" --clear-cache > '/tmp/result.json'"
        ));
        assert_eq!(
            lines.last(),
            Some(&"git worktree remove --force \"$worktree\"")
        );
    }

    #[test]
    fn local_builds_skip_fetch_and_message() {
        let mut plan = plan();
        plan.local = true;
        plan.fetch = false;
        plan.commit_ref = "feat/pix".into();
        let script = clean_build_script(&plan);
        assert!(!script.contains("git fetch"));
        assert!(script.contains("--local"));
        assert!(!script.contains("--message"));
    }

    #[test]
    fn quotes_survive_single_quotes() {
        assert_eq!(quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn reads_profiles_and_build_ids() {
        let eas =
            r#"{ "cli": {}, "build": { "development": {}, "preview": {}, "production": {} } }"#;
        assert_eq!(
            eas_profiles(eas),
            vec!["development", "preview", "production"]
        );
        let output = r#"[{"id": "b1", "platform": "IOS"}, {"id": "b2"}]"#;
        assert_eq!(parse_build_ids(output), vec!["b1", "b2"]);
        assert_eq!(parse_build_ids(r#"{"id": "b3"}"#), vec!["b3"]);
    }
}
