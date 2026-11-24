use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
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

use crate::{model::Platform, store::ReleasesStore};
use futures::channel::oneshot;
use std::sync::Arc;

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

pub(crate) async fn git(folder: &Path, args: &[&str]) -> Option<String> {
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
pub(crate) struct BranchState {
    pub repository_root: PathBuf,
    pub on_origin: bool,
    pub ahead: usize,
}

pub(crate) async fn branch_state(app_folder: &Path, branch: &str) -> Option<BranchState> {
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

/// What to build, as chosen in the modal or asked by an agent.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub app_folder: PathBuf,
    pub branch: String,
    pub profile: String,
    pub platform: BuildPlatform,
    pub local: bool,
    pub clear_cache: bool,
}

pub(crate) fn plan_for(
    state: &BranchState,
    request: &BuildRequest,
    commit_ref: String,
    fetch: bool,
) -> CleanBuildPlan {
    let app_subfolder = request
        .app_folder
        .strip_prefix(&state.repository_root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let result_file = paths::temp_dir().join(format!(
        "asylum-clean-build-{}.json",
        chrono::Utc::now().timestamp_millis()
    ));
    CleanBuildPlan {
        repository_root: state.repository_root.clone(),
        app_subfolder,
        commit_ref,
        branch: request.branch.clone(),
        fetch,
        profile: request.profile.clone(),
        platform: request.platform,
        local: request.local,
        clear_cache: request.clear_cache,
        install_command: install_command(&request.app_folder),
        result_file,
        worktrees_dir: paths::temp_dir().join("asylum-clean-builds"),
    }
}

/// Runs the clean build as a task (so it shows in Execuções). The receiver gets the EAS build
/// ids, or `None` when a step failed or the task was stopped.
pub(crate) fn schedule_clean_build(
    workspace: &mut Workspace,
    store: Entity<ReleasesStore>,
    plan: CleanBuildPlan,
    token: Arc<str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> oneshot::Receiver<Option<Vec<String>>> {
    let (sender, receiver) = oneshot::channel();
    let mut template = TaskTemplate {
        label: t!(
            "app_releases.clean_build.task_label",
            branch = plan.branch.clone(),
            profile = plan.profile.clone()
        )
        .to_string(),
        command: "bash".to_string(),
        args: vec!["-c".to_string(), clean_build_script(&plan)],
        cwd: Some(plan.repository_root.to_string_lossy().into_owned()),
        ..TaskTemplate::default()
    };
    template
        .env
        .insert("EXPO_TOKEN".to_string(), token.to_string());
    let Some(resolved) = template.resolve_task("asylum-clean-build", &TaskContext::default())
    else {
        sender.send(None).ok();
        return receiver;
    };
    let kvp = KeyValueStore::global(cx);
    let result_file = plan.result_file;
    workspace.schedule_resolved_task_with_completion(
        TaskSourceKind::UserInput,
        resolved,
        false,
        move |result, cx| {
            if result != ScheduledTaskResult::Success {
                sender.send(None).ok();
                return;
            }
            let ids = std::fs::read_to_string(&result_file)
                .map(|json| parse_build_ids(&json))
                .unwrap_or_default();
            std::fs::remove_file(&result_file).log_err();
            cx.background_spawn({
                let ids = ids.clone();
                async move {
                    remember_clean_builds(kvp, ids).await.log_err();
                }
            })
            .detach();
            store.update(cx, |store, cx| store.refresh(cx));
            sender.send(Some(ids)).ok();
        },
        window,
        cx,
    );
    receiver
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

            let request = BuildRequest {
                app_folder: app_folder.clone(),
                branch: branch.clone(),
                profile: profile.clone(),
                platform,
                local,
                clear_cache,
            };
            let plan = plan_for(&state, &request, commit_ref, fetch);
            workspace
                .update_in(cx, |workspace, window, cx| {
                    schedule_clean_build(workspace, store, plan, token, window, cx)
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

/// The EAS command that sends a finished build to its store. It doesn't wait: the dock follows
/// the submission on EAS like any other.
pub fn submit_command(build_id: &str, platform: Platform) -> Vec<String> {
    [
        "--yes",
        "eas-cli",
        "submit",
        "--platform",
        platform_flag(platform),
        "--id",
        build_id,
        "--non-interactive",
        "--no-wait",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn platform_flag(platform: Platform) -> &'static str {
    match platform {
        Platform::Ios => "ios",
        Platform::Android => "android",
    }
}

/// eas-cli reads submit credentials only from the submit profile of `eas.json` (which expands
/// `$VARS`) or from what EAS has saved. A profile that points at these variables gets the keys
/// of Lançamentos for the length of one submit.
pub const ASC_KEY_PATH_VAR: &str = "EXPO_ASC_API_KEY_PATH";
pub const ASC_KEY_ID_VAR: &str = "EXPO_ASC_KEY_ID";
pub const ASC_ISSUER_ID_VAR: &str = "EXPO_ASC_ISSUER_ID";
pub const PLAY_KEY_PATH_VAR: &str = "ASYLUM_GOOGLE_SERVICE_ACCOUNT_KEY_PATH";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitCheck {
    /// iOS: without `ascAppId` eas-cli refuses to submit when it can't ask.
    pub missing_asc_app_id: bool,
    /// The profile reads the variables above, so Asylum's keys are used instead of EAS's.
    pub uses_asylum_credentials: bool,
}

/// The submit profile eas-cli picks (`production`, as it does by default), with `extends`
/// followed, for one platform.
fn submit_profile(eas_json: &serde_json::Value, platform: Platform) -> serde_json::Map<String, serde_json::Value> {
    let profiles = eas_json.get("submit").and_then(serde_json::Value::as_object);
    let mut merged = serde_json::Map::new();
    let mut name = Some("production".to_string());
    let mut chain = Vec::new();
    while let Some(current) = name.take() {
        if chain.contains(&current) {
            break;
        }
        let Some(profile) = profiles.and_then(|profiles| profiles.get(&current)) else {
            break;
        };
        name = profile
            .get("extends")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        chain.push(current);
        if let Some(fields) = profile
            .get(platform_flag(platform))
            .and_then(serde_json::Value::as_object)
        {
            for (key, value) in fields {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    merged
}

pub fn check_submit_profile(eas_json: &str, platform: Platform) -> SubmitCheck {
    let value = serde_json_lenient::from_str::<serde_json::Value>(eas_json).unwrap_or_default();
    let profile = submit_profile(&value, platform);
    let field = |key: &str| profile.get(key).and_then(serde_json::Value::as_str);
    match platform {
        Platform::Ios => SubmitCheck {
            missing_asc_app_id: field("ascAppId").is_none_or(str::is_empty),
            uses_asylum_credentials: field("ascApiKeyPath")
                .is_some_and(|path| path.contains(ASC_KEY_PATH_VAR)),
        },
        Platform::Android => SubmitCheck {
            missing_asc_app_id: false,
            uses_asylum_credentials: field("serviceAccountKeyPath")
                .is_some_and(|path| path.contains(PLAY_KEY_PATH_VAR)),
        },
    }
}

/// What to add under `submit.production` in `eas.json` so a submit uses Lançamentos' keys.
pub fn submit_profile_snippet(platform: Platform, asc_app_id: Option<&str>) -> String {
    let fields = match platform {
        Platform::Ios => serde_json::json!({
            "ios": {
                "ascAppId": asc_app_id.unwrap_or("<App Store Connect app id>"),
                "ascApiKeyPath": format!("${ASC_KEY_PATH_VAR}"),
                "ascApiKeyId": format!("${ASC_KEY_ID_VAR}"),
                "ascApiKeyIssuerId": format!("${ASC_ISSUER_ID_VAR}"),
            }
        }),
        Platform::Android => serde_json::json!({
            "android": { "serviceAccountKeyPath": format!("${PLAY_KEY_PATH_VAR}") }
        }),
    };
    serde_json::to_string_pretty(&serde_json::json!({ "submit": { "production": fields } }))
        .unwrap_or_default()
}

/// Key files written for one submit. They are deleted when this is dropped, so a submit that
/// never starts (cancelled, no window) doesn't leave keys behind.
#[derive(Default)]
struct SecretFiles(Vec<PathBuf>);

impl Drop for SecretFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            std::fs::remove_file(file).log_err();
        }
    }
}

/// Everything one `eas submit` needs. The key files only live until the task ends.
pub(crate) struct SubmitPlan {
    build_id: String,
    platform: Platform,
    folder: PathBuf,
    env: Vec<(String, String)>,
    secret_files: SecretFiles,
    pub uses_asylum_credentials: bool,
}

fn write_secret_file(name: &str, contents: &str) -> Result<PathBuf> {
    let directory = paths::temp_dir().join("asylum-submit");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "{}-{name}",
        chrono::Utc::now().timestamp_millis()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    std::io::Write::write_all(&mut file, contents.as_bytes())?;
    Ok(path)
}

/// Checks `eas.json` and the accounts, and writes the keys the profile asks for.
pub(crate) fn prepare_submit(
    store: &ReleasesStore,
    build_id: String,
    platform: Platform,
    cx: &App,
) -> Result<SubmitPlan> {
    let accounts = store
        .accounts()
        .read(cx)
        .accounts()
        .cloned()
        .unwrap_or_default();
    let token = accounts
        .expo
        .as_ref()
        .map(|expo| expo.token.clone())
        .with_context(|| t!("app_releases.clean_build.no_expo"))?;
    let app = store
        .selected_app()
        .with_context(|| t!("app_releases.submit.no_app"))?;
    let folder = app.folder.to_path_buf();
    let eas_json = std::fs::read_to_string(folder.join("eas.json")).unwrap_or_default();
    let check = check_submit_profile(&eas_json, platform);
    if check.missing_asc_app_id {
        anyhow::bail!(
            "{}",
            t!(
                "app_releases.submit.missing_asc_app_id",
                snippet = submit_profile_snippet(platform, app.app_store_app_id.as_deref())
            )
        );
    }
    let mut env = vec![("EXPO_TOKEN".to_string(), token.to_string())];
    let mut secret_files = SecretFiles::default();
    if check.uses_asylum_credentials {
        match platform {
            Platform::Ios => {
                let account = accounts
                    .app_store
                    .as_ref()
                    .with_context(|| t!("app_releases.write.no_account", service = "App Store Connect"))?;
                let path = write_secret_file("AuthKey.p8", &account.private_key)?;
                env.push((ASC_KEY_PATH_VAR.into(), path.to_string_lossy().into_owned()));
                env.push((ASC_KEY_ID_VAR.into(), account.key_id.trim().to_string()));
                env.push((ASC_ISSUER_ID_VAR.into(), account.issuer_id.trim().to_string()));
                secret_files.0.push(path);
            }
            Platform::Android => {
                let account = accounts
                    .google_play
                    .as_ref()
                    .with_context(|| t!("app_releases.write.no_account", service = "Google Play"))?;
                let path = write_secret_file("service-account.json", &account.json)?;
                env.push((PLAY_KEY_PATH_VAR.into(), path.to_string_lossy().into_owned()));
                secret_files.0.push(path);
            }
        }
    }
    Ok(SubmitPlan {
        build_id,
        platform,
        folder,
        env,
        secret_files,
        uses_asylum_credentials: check.uses_asylum_credentials,
    })
}

/// Runs `eas submit` as a task; the receiver says whether it worked.
pub(crate) fn schedule_submit(
    workspace: &mut Workspace,
    store: Entity<ReleasesStore>,
    plan: SubmitPlan,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> oneshot::Receiver<bool> {
    let (sender, receiver) = oneshot::channel();
    let mut template = TaskTemplate {
        label: t!(
            "app_releases.clean_build.submit_label",
            build = plan.build_id.clone()
        )
        .to_string(),
        command: "npx".to_string(),
        args: submit_command(&plan.build_id, plan.platform),
        cwd: Some(plan.folder.to_string_lossy().into_owned()),
        ..TaskTemplate::default()
    };
    template.env.extend(plan.env);
    let Some(resolved) =
        template.resolve_task("asylum-clean-build-submit", &TaskContext::default())
    else {
        sender.send(false).ok();
        return receiver;
    };
    let secret_files = plan.secret_files;
    workspace.schedule_resolved_task_with_completion(
        TaskSourceKind::UserInput,
        resolved,
        false,
        move |result, cx| {
            drop(secret_files);
            store.update(cx, |store, cx| store.refresh(cx));
            sender.send(result == ScheduledTaskResult::Success).ok();
        },
        window,
        cx,
    );
    receiver
}

/// Sends a build to its store with `eas submit --id`, after the user confirms.
pub fn submit_build(
    workspace: WeakEntity<Workspace>,
    store: Entity<ReleasesStore>,
    build_id: String,
    platform: Platform,
    window: &mut Window,
    cx: &mut App,
) {
    let has_expo = store
        .read(cx)
        .accounts()
        .read(cx)
        .accounts()
        .is_some_and(|accounts| accounts.expo.is_some());
    if !has_expo {
        window.dispatch_action(Box::new(crate::OpenAccounts), cx);
        return;
    }
    let plan = match prepare_submit(store.read(cx), build_id, platform, cx) {
        Ok(plan) => plan,
        Err(error) => {
            let answer = window.prompt(
                PromptLevel::Critical,
                &t!("app_releases.submit.cannot", platform = platform.label()),
                Some(&format!("{error:#}")),
                &[t!("app_releases.submit.ok").as_ref()],
                cx,
            );
            cx.background_spawn(async move {
                answer.await.ok();
            })
            .detach();
            return;
        }
    };
    let detail = if plan.uses_asylum_credentials {
        t!("app_releases.submit.with_asylum_keys")
    } else {
        t!("app_releases.submit.with_eas_keys")
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &t!(
            "app_releases.clean_build.submit_prompt",
            platform = platform.label()
        ),
        Some(&format!(
            "{}\n\n{detail}",
            t!("app_releases.clean_build.submit_detail")
        )),
        &[
            t!("app_releases.clean_build.submit").as_ref(),
            t!("app_releases.clean_build.cancel").as_ref(),
        ],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    schedule_submit(workspace, store, plan, window, cx)
                })
                .log_err();
        })
        .detach();
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
    fn submit_uses_the_build_id_without_prompts() {
        assert_eq!(
            submit_command("b1", crate::model::Platform::Android).join(" "),
            "--yes eas-cli submit --platform android --id b1 --non-interactive --no-wait"
        );
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

    #[test]
    fn checks_the_submit_profile_eas_will_use() {
        let eas_json = r#"{
          // comments are fine in eas.json
          "submit": {
            "base": {"ios": {"ascAppId": "1600000000"}},
            "production": {
              "extends": "base",
              "ios": {"ascApiKeyPath": "$EXPO_ASC_API_KEY_PATH", "ascApiKeyId": "$EXPO_ASC_KEY_ID"},
              "android": {"serviceAccountKeyPath": "./keys/play.json"}
            }
          }
        }"#;
        assert_eq!(
            check_submit_profile(eas_json, Platform::Ios),
            SubmitCheck {
                missing_asc_app_id: false,
                uses_asylum_credentials: true,
            }
        );
        assert_eq!(
            check_submit_profile(eas_json, Platform::Android),
            SubmitCheck {
                missing_asc_app_id: false,
                uses_asylum_credentials: false,
            }
        );
        assert!(check_submit_profile("{}", Platform::Ios).missing_asc_app_id);
    }

    #[test]
    fn the_snippet_is_a_profile_eas_accepts() {
        let snippet = submit_profile_snippet(Platform::Ios, Some("1600000000"));
        let check = check_submit_profile(&snippet, Platform::Ios);
        assert!(!check.missing_asc_app_id);
        assert!(check.uses_asylum_credentials);
        let snippet = submit_profile_snippet(Platform::Android, None);
        assert!(check_submit_profile(&snippet, Platform::Android).uses_asylum_credentials);
    }

    #[test]
    fn key_files_are_private_and_go_away() {
        let path = write_secret_file("sample.p8", "secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(SecretFiles(vec![path.clone()]));
        assert!(!path.exists());
    }
}
