//! The Lançamentos toolkit for agents: read where each version is, and promote, roll out or
//! submit with the accounts of the dock. Every write tool always asks before it runs.

use crate::{
    app_store_connect::{self as asc, PhasedReleaseState, ReleaseType, VersionPreparation},
    clean_build::{self, BuildPlatform, BuildRequest},
    google_play::{LocalizedText, PRODUCTION_TRACK, TrackChange},
    model::{self, CheckItem, CheckState, Platform, PlatformTimeline, Release},
    status_label, step_title,
    store::{AccountsState, AppReleases, Detection, ReleasesStore, StoreWrite},
};
use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, TaskExt as _, Window, WindowHandle};
use i18n::t;
use project::Project;
use workspace::{MultiWorkspace, OpenOptions, Workspace};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};

const READY_TIMEOUT: Duration = Duration::from_secs(45);
const NO_APP: &str = "This project has no linked app; link it in Lojas e EAS.";
/// App Store Connect rate-limits; waiting re-reads no faster than this.
const WAIT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_WAIT_MINUTES: u64 = 10;

/// Waits until the project's store has read the services at least once.
async fn ready_store(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Entity<ReleasesStore>> {
    let store = cx.update(|cx| ReleasesStore::for_project(project, cx));
    let started = Instant::now();
    let mut asked_refresh = false;
    loop {
        let (ready, needs_refresh) = cx.update(|cx| {
            let store_state = store.read(cx);
            let accounts_ready = !matches!(
                store_state.accounts().read(cx).state(),
                AccountsState::Loading
            );
            let detected = matches!(store_state.detection(), Detection::Done);
            let refreshed = store_state
                .apps()
                .iter()
                .all(|app| app.refreshed_at.is_some());
            (
                accounts_ready && detected && refreshed && !store_state.is_refreshing(),
                accounts_ready && detected && !refreshed && !store_state.is_refreshing(),
            )
        });
        if ready {
            return Ok(store);
        }
        if needs_refresh && !asked_refresh {
            asked_refresh = true;
            store.update(cx, |store, cx| store.refresh(cx));
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err(anyhow!(
                "Lançamentos didn't finish reading the stores in time; try again."
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(200))
            .await;
    }
}

fn describe_release(release: &Release) -> String {
    let mut text = format!("{}\n", release.title());
    for timeline in &release.platforms {
        let (label, _) = status_label(timeline);
        text.push_str(&format!(
            "  {} build {}: {}\n",
            timeline.platform.label(),
            timeline.build.as_deref().unwrap_or("?"),
            label
        ));
        for step in &timeline.steps {
            text.push_str(&format!(
                "    - {}: {:?}{}\n",
                step_title(step.kind),
                step.state,
                step.detail
                    .as_ref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            ));
        }
        if !timeline.testflight_groups.is_empty() {
            text.push_str(&format!(
                "    TestFlight groups: {}\n",
                timeline.testflight_groups.join(", ")
            ));
        }
        for track in &timeline.tracks {
            text.push_str(&format!(
                "    Play track {}: {}{}\n",
                track.track,
                track.status,
                track
                    .user_fraction
                    .map(|fraction| format!(" {}%", (fraction * 100.0).round() as i64))
                    .unwrap_or_default()
            ));
        }
    }
    text
}

fn describe_checklist(checklist: &[CheckItem]) -> String {
    let mut text = String::new();
    for item in checklist {
        let state = match item.state {
            CheckState::Passed => "ok",
            CheckState::Missing => "missing",
            CheckState::Unknown => "not checked",
        };
        text.push_str(&format!(
            "      - {:?}: {state}{}\n",
            item.kind,
            item.detail
                .as_ref()
                .filter(|detail| !detail.is_empty())
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default()
        ));
    }
    text
}

/// For an iOS version that is not in review: what App Store Connect has and what is missing.
fn describe_preparation(app: &AppReleases, version: &str) -> Option<String> {
    let data = app.app_store.as_ref()?;
    let options = model::version_options(data, version);
    if !options.can_prepare() {
        return None;
    }
    let mut text = match (&options.version, &options.editable_version) {
        (Some(current), _) => format!(
            "    App Store version {version}: {} (release {:?})\n",
            current.state,
            current.release_type.unwrap_or_default()
        ),
        (None, Some(editable)) => format!(
            "    App Store version {version}: doesn't exist; release_prepare_version renames the open {}\n",
            editable.version
        ),
        (None, None) => format!(
            "    App Store version {version}: doesn't exist; release_prepare_version creates it\n"
        ),
    };
    let builds: Vec<&str> = options
        .ready_builds
        .iter()
        .map(|build| build.build_number.as_str())
        .collect();
    text.push_str(&format!(
        "    Builds ready for it: {}{}\n",
        if builds.is_empty() {
            "none".to_string()
        } else {
            builds.join(", ")
        },
        if options.processing_builds > 0 {
            format!(" ({} still processing)", options.processing_builds)
        } else {
            String::new()
        }
    ));
    if options.version.is_some() {
        text.push_str("    Before App Review:\n");
        text.push_str(&describe_checklist(&model::review_checklist(
            data, &options, None,
        )));
    }
    Some(text)
}

#[derive(Deserialize, JsonSchema)]
struct StatusInput {
    /// Only this version (e.g. `2.4.0`). Defaults to the most recent versions.
    #[serde(default)]
    version: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct PromoteInput {
    /// The Android version code to promote, e.g. `128`.
    version_code: String,
    /// The Play track to put it on: `internal`, `alpha`, `beta` or `production`.
    to_track: String,
    /// The track it is on now. Defaults to the testing track that has it.
    #[serde(default)]
    from_track: Option<String>,
    /// For production, a staged rollout percentage (1–100). Defaults to 100.
    #[serde(default)]
    rollout_percent: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RolloutAction {
    Set,
    Halt,
    Resume,
}

#[derive(Deserialize, JsonSchema)]
struct RolloutInput {
    /// The Android version code on production.
    version_code: String,
    action: RolloutAction,
    /// With `set`: the new rollout percentage (1–100).
    #[serde(default)]
    percent: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct SubmitInput {
    /// The iOS version to send to App Review, e.g. `2.4.0`.
    version: String,
}

/// The app folder the write applies to: the one selected in the dock.
fn selected_folder(store: &Entity<ReleasesStore>, cx: &AsyncApp) -> Result<Arc<Path>> {
    cx.update(|cx| {
        store
            .read(cx)
            .selected_app()
            .map(|app| app.folder.clone())
            .context("This project has no linked app; link it in Lojas e EAS.")
    })
}

fn release_status(
    project: Entity<Project>,
    input: StatusInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let text = cx.update(|cx| {
            let mut text = String::new();
            for app in store.read(cx).apps() {
                text.push_str(&format!("App {}\n", app.name()));
                for kind in crate::accounts::AccountKind::ALL {
                    let status = app.sources.get(kind);
                    if status.is_problem() {
                        text.push_str(&format!("  {}: {:?}\n", kind.label(), status));
                    }
                }
                let releases: Vec<&Release> = match &input.version {
                    Some(version) => app
                        .releases
                        .iter()
                        .filter(|release| &release.version == version)
                        .collect(),
                    None => app.releases.iter().take(3).collect(),
                };
                if releases.is_empty() {
                    text.push_str("  No matching versions.\n");
                }
                for release in releases {
                    text.push_str(&describe_release(release));
                    if release.platform(Platform::Ios).is_some()
                        && let Some(preparation) = describe_preparation(app, &release.version)
                    {
                        text.push_str(&preparation);
                    }
                }
            }
            text
        });
        Ok(ToolkitOutput::text(text))
    })
}

async fn write(
    project: &Entity<Project>,
    write: StoreWrite,
    cx: &mut AsyncApp,
) -> Result<Entity<ReleasesStore>> {
    let store = ready_store(project, cx).await?;
    let folder = selected_folder(&store, cx)?;
    store
        .update(cx, |store, cx| store.run_write(&folder, write, cx))
        .await?;
    Ok(store)
}

fn fraction_from_percent(percent: Option<f64>) -> Result<Option<f64>> {
    match percent {
        None => Ok(None),
        Some(percent) if percent > 0.0 && percent <= 100.0 => Ok(Some(percent / 100.0)),
        Some(_) => Err(anyhow!("The rollout percentage must be between 1 and 100.")),
    }
}

fn release_promote(
    project: Entity<Project>,
    input: PromoteInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let from_track = match input.from_track {
            Some(track) => track,
            None => cx
                .update(|cx| {
                    store.read(cx).selected_app().and_then(|app| {
                        app.google_play_tracks
                            .iter()
                            .filter(|track| track.track != input.to_track)
                            .filter(|track| track.track != PRODUCTION_TRACK)
                            .find(|track| {
                                track.releases.iter().any(|release| {
                                    release.version_codes.contains(&input.version_code)
                                })
                            })
                            .map(|track| track.track.clone())
                    })
                })
                .with_context(|| {
                    format!(
                        "Version code {} isn't on any testing track; say which track it is on.",
                        input.version_code
                    )
                })?,
        };
        let user_fraction = if input.to_track == PRODUCTION_TRACK {
            fraction_from_percent(input.rollout_percent)?
        } else {
            None
        };
        write(
            &project,
            StoreWrite::Play(TrackChange::Promote {
                from_track: from_track.clone(),
                to_track: input.to_track.clone(),
                version_code: input.version_code.clone(),
                user_fraction,
            }),
            cx,
        )
        .await?;
        Ok(ToolkitOutput::text(format!(
            "Promoted version code {} from {from_track} to {}.",
            input.version_code, input.to_track
        )))
    })
}

fn release_rollout(
    project: Entity<Project>,
    input: RolloutInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let track = PRODUCTION_TRACK.to_string();
        let version_code = input.version_code.clone();
        let change = match input.action {
            RolloutAction::Set => TrackChange::SetRollout {
                track,
                version_code,
                user_fraction: fraction_from_percent(input.percent)?
                    .context("Say the new rollout percentage.")?,
            },
            RolloutAction::Halt => TrackChange::Halt {
                track,
                version_code,
            },
            RolloutAction::Resume => TrackChange::Resume {
                track,
                version_code,
            },
        };
        write(&project, StoreWrite::Play(change), cx).await?;
        Ok(ToolkitOutput::text(format!(
            "Updated the production rollout of {}.",
            input.version_code
        )))
    })
}

fn release_submit_review(
    project: Entity<Project>,
    input: SubmitInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let version_id = cx
            .update(|cx| {
                store.read(cx).selected_app().and_then(|app| {
                    app.releases
                        .iter()
                        .find(|release| release.version == input.version)?
                        .platform(Platform::Ios)?
                        .ids
                        .asc_version_id
                        .clone()
                })
            })
            .with_context(|| {
                format!(
                    "App Store Connect has no version {}; create it there first.",
                    input.version
                )
            })?;
        let checklist = review_checklist(&store, &input.version, cx).await?;
        if !model::checklist_passed(&checklist) {
            bail!(
                "{} isn't ready for App Review yet:\n{}",
                input.version,
                describe_checklist(&checklist)
            );
        }
        write(&project, StoreWrite::SubmitForReview { version_id }, cx).await?;
        Ok(ToolkitOutput::text(format!(
            "Sent {} to App Review.",
            input.version
        )))
    })
}

/// The checklist of an iOS version, with What's New read from App Store Connect.
async fn review_checklist(
    store: &Entity<ReleasesStore>,
    version: &str,
    cx: &mut AsyncApp,
) -> Result<Vec<CheckItem>> {
    let (data, options) = cx.update(|cx| -> Result<_> {
        let app = store.read(cx).selected_app().context(NO_APP)?;
        let data = app
            .app_store
            .clone()
            .context("App Store Connect wasn't read; check the account in Lojas e EAS.")?;
        let options = model::version_options(&data, version);
        Ok((data, options))
    })?;
    let localizations = match options.version.as_ref() {
        Some(current) if !options.first_release => Some(
            store
                .update(cx, |store, cx| store.fetch_version_notes(current.id.clone(), cx))
                .await?,
        ),
        _ => None,
    };
    Ok(model::review_checklist(
        &data,
        &options,
        localizations.as_deref(),
    ))
}

/// The window and workspace showing this project, to run tasks where Execuções shows them.
fn workspace_window(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .find_map(|window| {
            let workspace = window
                .read(cx)
                .ok()?
                .workspaces()
                .find(|workspace| workspace.read(cx).project() == project)?
                .clone();
            Some((window, workspace))
        })
}

fn in_workspace<R>(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
    run: impl FnOnce(&mut Workspace, &mut Window, &mut gpui::Context<Workspace>) -> R,
) -> Result<R> {
    cx.update(|cx| {
        let (window, workspace) = workspace_window(project, cx)
            .context("Open this project in a window so the task can run there.")?;
        window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| run(workspace, window, cx))
        })
    })
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StorePlatform {
    Ios,
    Android,
}

impl From<StorePlatform> for Platform {
    fn from(platform: StorePlatform) -> Self {
        match platform {
            StorePlatform::Ios => Platform::Ios,
            StorePlatform::Android => Platform::Android,
        }
    }
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BuildTarget {
    Ios,
    Android,
    All,
}

#[derive(Deserialize, JsonSchema)]
struct BuildInput {
    /// The branch to build. Its commit on origin is built, from a clean worktree.
    branch: String,
    /// The build profile of `eas.json`, e.g. `preview` or `production`.
    profile: String,
    platform: BuildTarget,
    /// Builds without EAS's cache.
    #[serde(default)]
    clear_cache: bool,
    /// Build the local branch when it isn't on origin. Only when the user asked for it.
    #[serde(default)]
    allow_unpushed: bool,
}

fn eas_build(
    project: Entity<Project>,
    input: BuildInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let (app_folder, token) = cx.update(|cx| -> Result<_> {
            let store = store.read(cx);
            let app = store.selected_app().context(NO_APP)?;
            let token = store
                .accounts()
                .read(cx)
                .accounts()
                .and_then(|accounts| accounts.expo.as_ref().map(|expo| expo.token.clone()))
                .context("Connect an Expo account in Lojas e EAS first.")?;
            Ok((app.folder.to_path_buf(), token))
        })?;
        let profiles = std::fs::read_to_string(app_folder.join("eas.json"))
            .map(|text| clean_build::eas_profiles(&text))
            .unwrap_or_default();
        if !profiles.contains(&input.profile) {
            bail!(
                "eas.json has no build profile {:?}; it has: {}.",
                input.profile,
                profiles.join(", ")
            );
        }
        let state = cx
            .background_spawn({
                let app_folder = app_folder.clone();
                let branch = input.branch.clone();
                async move { clean_build::branch_state(&app_folder, &branch).await }
            })
            .await
            .context("The app isn't in a git repository.")?;
        let mut notes = Vec::new();
        let (commit_ref, fetch) = if !state.on_origin {
            if !input.allow_unpushed {
                bail!(
                    "Branch {} isn't on origin. Push it first, or build the local branch only if \
                     the user asks (allow_unpushed). Never push on your own.",
                    input.branch
                );
            }
            notes.push("Built the local branch: it isn't on origin.".to_string());
            (input.branch.clone(), false)
        } else {
            if state.ahead > 0 {
                notes.push(format!(
                    "The local branch has {} commits not on origin; they are not in this build.",
                    state.ahead
                ));
            }
            (format!("origin/{}", input.branch), true)
        };
        let request = BuildRequest {
            app_folder,
            branch: input.branch.clone(),
            profile: input.profile.clone(),
            platform: match input.platform {
                BuildTarget::Ios => BuildPlatform::Ios,
                BuildTarget::Android => BuildPlatform::Android,
                BuildTarget::All => BuildPlatform::All,
            },
            local: false,
            clear_cache: input.clear_cache,
        };
        let plan = clean_build::plan_for(&state, &request, commit_ref, fetch);
        let receiver = in_workspace(&project, cx, |workspace, window, cx| {
            clean_build::schedule_clean_build(workspace, store.clone(), plan, token, window, cx)
        })?;
        let ids = receiver.await.ok().flatten().context(
            "The clean build stopped before reaching EAS; its task in Execuções has the output.",
        )?;
        let mut text = format!(
            "Started the {} build of {} on EAS: {}. It runs in EAS's cloud; follow it with \
             release_wait or release_status.",
            input.profile,
            input.branch,
            ids.join(", ")
        );
        for note in notes {
            text.push_str(&format!("\n{note}"));
        }
        Ok(ToolkitOutput::text(text))
    })
}

#[derive(Deserialize, JsonSchema)]
struct SubmitBuildInput {
    /// The version whose EAS build to send, e.g. `2.4.0`.
    version: String,
    platform: StorePlatform,
    /// An EAS build id, when it isn't the version's newest build.
    #[serde(default)]
    build_id: Option<String>,
}

fn eas_submit(
    project: Entity<Project>,
    input: SubmitBuildInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let platform = Platform::from(input.platform);
        let plan = cx.update(|cx| -> Result<_> {
            let build_id = match input.build_id.clone() {
                Some(build_id) => build_id,
                None => store
                    .read(cx)
                    .selected_app()
                    .context(NO_APP)?
                    .releases
                    .iter()
                    .find(|release| release.version == input.version)
                    .and_then(|release| release.platform(platform))
                    .and_then(|timeline| timeline.ids.eas_build_id.clone())
                    .with_context(|| {
                        format!(
                            "No EAS build of {} for {}; build it with eas_build first.",
                            input.version,
                            platform.label()
                        )
                    })?,
            };
            clean_build::prepare_submit(store.read(cx), build_id, platform, cx)
        })?;
        let uses_asylum_credentials = plan.uses_asylum_credentials;
        let receiver = in_workspace(&project, cx, |workspace, window, cx| {
            clean_build::schedule_submit(workspace, store.clone(), plan, window, cx)
        })?;
        if !receiver.await.unwrap_or(false) {
            bail!("eas submit failed; its task in Execuções has the output.");
        }
        Ok(ToolkitOutput::text(format!(
            "Sent the {} build of {} to {} ({}). The store still has to process it; follow \
             with release_wait.",
            platform.label(),
            input.version,
            match platform {
                Platform::Ios => "App Store Connect",
                Platform::Android => "Google Play",
            },
            if uses_asylum_credentials {
                "with the keys of Lojas e EAS"
            } else {
                "with the credentials saved on EAS"
            }
        )))
    })
}

#[derive(Deserialize, JsonSchema)]
struct PrepareInput {
    /// The iOS version, e.g. `2.4.0`.
    version: String,
    /// The build number to attach. Defaults to the newest processed build of the version.
    #[serde(default)]
    build_number: Option<String>,
    /// Defaults to what the version has, or manual.
    #[serde(default)]
    release_type: Option<ReleaseType>,
    /// Export compliance, asked only when the build has no answer. `false` for apps that use
    /// only HTTPS and the system's standard encryption. Ask the user when unsure.
    #[serde(default)]
    uses_non_exempt_encryption: Option<bool>,
}

fn release_prepare_version(
    project: Entity<Project>,
    input: PrepareInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let (preparation, build_number) = cx.update(|cx| -> Result<_> {
            let app = store.read(cx).selected_app().context(NO_APP)?;
            let options = app.version_options(&input.version).context(
                "App Store Connect wasn't read; check the account in Lojas e EAS.",
            )?;
            if let Some(current) = options.version.as_ref().filter(|_| !options.can_prepare()) {
                bail!(
                    "{} is {} in App Store Connect and can't be changed anymore.",
                    input.version,
                    current.state
                );
            }
            let build = match &input.build_number {
                Some(build_number) => options.ready_build(build_number).with_context(|| {
                    format!(
                        "Build {build_number} of {} isn't processed in App Store Connect.",
                        input.version
                    )
                })?,
                None => options.suggested_build().with_context(|| {
                    if options.processing_builds > 0 {
                        format!(
                            "App Store Connect is still processing {} build(s) of {}; wait with \
                             release_wait.",
                            options.processing_builds, input.version
                        )
                    } else {
                        format!(
                            "No build of {} reached App Store Connect; send one with eas_submit.",
                            input.version
                        )
                    }
                })?,
            };
            let uses_non_exempt_encryption = match build.uses_non_exempt_encryption {
                Some(_) => None,
                None => Some(input.uses_non_exempt_encryption.with_context(|| {
                    format!(
                        "Build {} has no export compliance answer; ask the user and pass \
                         uses_non_exempt_encryption.",
                        build.build_number
                    )
                })?),
            };
            let release_type = input
                .release_type
                .or_else(|| options.version.as_ref().and_then(|version| version.release_type))
                .unwrap_or_default();
            Ok((
                VersionPreparation {
                    editable_version_id: options
                        .version
                        .as_ref()
                        .or(options.editable_version.as_ref())
                        .map(|version| version.id.clone()),
                    version_string: input.version.clone(),
                    build_id: build.id.clone(),
                    release_type,
                    uses_non_exempt_encryption,
                },
                build.build_number.clone(),
            ))
        })?;
        let release_type = preparation.release_type;
        write(&project, StoreWrite::PrepareVersion(preparation), cx).await?;
        Ok(ToolkitOutput::text(format!(
            "App Store version {} now has build {build_number} (release {release_type:?}). \
             Next: release_notes for What's New, then release_submit_review.",
            input.version
        )))
    })
}

#[derive(Deserialize, JsonSchema)]
struct NoteInput {
    /// e.g. `pt-BR` or `en-US`.
    language: String,
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct NotesInput {
    version: String,
    platform: StorePlatform,
    /// One entry per language. On iOS, only languages the version already has.
    notes: Vec<NoteInput>,
    /// Android: the Play track whose release gets the notes. Defaults to the track that has the
    /// version code.
    #[serde(default)]
    track: Option<String>,
}

fn timeline_of(
    store: &Entity<ReleasesStore>,
    version: &str,
    platform: Platform,
    cx: &AsyncApp,
) -> Result<PlatformTimeline> {
    cx.update(|cx| {
        store
            .read(cx)
            .selected_app()
            .context(NO_APP)?
            .releases
            .iter()
            .find(|release| release.version == version)
            .and_then(|release| release.platform(platform))
            .cloned()
            .with_context(|| format!("No {} release {version}.", platform.label()))
    })
}

fn release_notes(
    project: Entity<Project>,
    input: NotesInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        if input.notes.is_empty() {
            bail!("Give at least one language.");
        }
        let platform = Platform::from(input.platform);
        let timeline = timeline_of(&store, &input.version, platform, cx)?;
        let write_to_send = match platform {
            Platform::Ios => {
                let version_id = timeline.ids.asc_version_id.clone().with_context(|| {
                    format!(
                        "App Store Connect has no version {}; create it with \
                         release_prepare_version.",
                        input.version
                    )
                })?;
                let localizations = store
                    .update(cx, |store, cx| store.fetch_version_notes(version_id, cx))
                    .await?;
                let mut notes = Vec::new();
                for note in &input.notes {
                    let localization = localizations
                        .iter()
                        .find(|localization| {
                            localization.locale.eq_ignore_ascii_case(&note.language)
                        })
                        .with_context(|| {
                            format!(
                                "The version has no {} localization; it has: {}.",
                                note.language,
                                localizations
                                    .iter()
                                    .map(|localization| localization.locale.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })?;
                    notes.push((localization.id.clone(), note.text.trim().to_string()));
                }
                StoreWrite::VersionNotes { notes }
            }
            Platform::Android => {
                let version_code = timeline
                    .build
                    .clone()
                    .context("This release has no version code.")?;
                let track = match input.track.clone() {
                    Some(track) => track,
                    None => cx
                        .update(|cx| {
                            store.read(cx).selected_app().and_then(|app| {
                                app.google_play_tracks
                                    .iter()
                                    .find(|track| {
                                        track.releases.iter().any(|release| {
                                            release.version_codes.contains(&version_code)
                                        })
                                    })
                                    .map(|track| track.track.clone())
                            })
                        })
                        .with_context(|| {
                            format!("Version code {version_code} isn't on any Play track yet.")
                        })?,
                };
                StoreWrite::Play(TrackChange::SetNotes {
                    track,
                    version_code,
                    notes: input
                        .notes
                        .iter()
                        .map(|note| LocalizedText {
                            language: note.language.clone(),
                            text: note.text.trim().to_string(),
                        })
                        .collect(),
                })
            }
        };
        write(&project, write_to_send, cx).await?;
        Ok(ToolkitOutput::text(format!(
            "Saved the {} notes of {} in {} languages.",
            platform.label(),
            input.version,
            input.notes.len()
        )))
    })
}

#[derive(Deserialize, JsonSchema)]
struct TestFlightInput {
    version: String,
    /// Defaults to the version's newest build in App Store Connect.
    #[serde(default)]
    build_number: Option<String>,
    /// A TestFlight group to add the build to, by name.
    #[serde(default)]
    group: Option<String>,
    /// What to Test for the testers.
    #[serde(default)]
    what_to_test: Option<String>,
    /// The language of `what_to_test`. Defaults to `en-US`.
    #[serde(default)]
    locale: Option<String>,
    /// Sends the build to Beta App Review, needed by external groups.
    #[serde(default)]
    beta_review: bool,
}

fn release_testflight(
    project: Entity<Project>,
    input: TestFlightInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        if input.group.is_none() && input.what_to_test.is_none() && !input.beta_review {
            bail!("Say a group, what_to_test or beta_review.");
        }
        let (build, group): (asc::Build, Option<asc::BetaGroup>) = cx.update(|cx| -> Result<_> {
            let app = store.read(cx).selected_app().context(NO_APP)?;
            let data = app
                .app_store
                .as_ref()
                .context("App Store Connect wasn't read; check the account in Lojas e EAS.")?;
            let mut builds: Vec<&asc::Build> = data
                .builds
                .iter()
                .filter(|build| build.version.as_deref() == Some(input.version.as_str()))
                .filter(|build| {
                    input
                        .build_number
                        .as_ref()
                        .is_none_or(|number| &build.build_number == number)
                })
                .collect();
            builds.sort_by(|left, right| {
                model::compare_build_numbers(&right.build_number, &left.build_number)
            });
            let build = (*builds.first().with_context(|| {
                format!("No build of {} in App Store Connect.", input.version)
            })?)
            .clone();
            let group = match &input.group {
                None => None,
                Some(name) => Some(
                    app.beta_groups
                        .iter()
                        .find(|group| group.name.eq_ignore_ascii_case(name))
                        .cloned()
                        .with_context(|| {
                            format!(
                                "No TestFlight group {name}; the app has: {}.",
                                app.beta_groups
                                    .iter()
                                    .map(|group| group.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })?,
                ),
            };
            Ok((build, group))
        })?;
        let mut done = Vec::new();
        if let Some(text) = &input.what_to_test {
            write(
                &project,
                StoreWrite::WhatToTest {
                    build_id: build.id.clone(),
                    locale: input.locale.clone().unwrap_or_else(|| "en-US".into()),
                    text: text.trim().to_string(),
                },
                cx,
            )
            .await?;
            done.push("saved What to Test".to_string());
        }
        if let Some(group) = &group {
            write(
                &project,
                StoreWrite::AddBuildToGroup {
                    build_id: build.id.clone(),
                    group_id: group.id.clone(),
                },
                cx,
            )
            .await?;
            done.push(format!("added it to {}", group.name));
        }
        if input.beta_review {
            write(
                &project,
                StoreWrite::SubmitForBetaReview {
                    build_id: build.id.clone(),
                },
                cx,
            )
            .await?;
            done.push("sent it to Beta App Review".to_string());
        }
        Ok(ToolkitOutput::text(format!(
            "Build {} of {}: {}.",
            build.build_number,
            input.version,
            done.join(", ")
        )))
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PhasedAction {
    /// Turns on the 7-day phased release (before or after approval).
    Start,
    Pause,
    Resume,
    /// Releases to everyone now.
    Complete,
    /// Puts an approved version waiting for the developer on sale.
    Release,
}

#[derive(Deserialize, JsonSchema)]
struct PhasedInput {
    version: String,
    action: PhasedAction,
}

fn release_phased(
    project: Entity<Project>,
    input: PhasedInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let timeline = timeline_of(&store, &input.version, Platform::Ios, cx)?;
        let version_id = timeline.ids.asc_version_id.clone().with_context(|| {
            format!("App Store Connect has no version {}.", input.version)
        })?;
        let phased = timeline.ids.asc_phased_release.clone();
        let phased_id = |expected: &[PhasedReleaseState]| -> Result<String> {
            match &phased {
                Some((id, state)) if expected.contains(state) => Ok(id.clone()),
                Some((_, state)) => bail!("The phased release is {state:?}."),
                None => bail!("{} has no phased release.", input.version),
            }
        };
        let (write_to_send, done) = match input.action {
            PhasedAction::Start => {
                if phased.is_some() {
                    bail!("{} already has a phased release.", input.version);
                }
                (
                    StoreWrite::StartPhasedRelease { version_id },
                    "turned on the phased release",
                )
            }
            PhasedAction::Pause => (
                StoreWrite::SetPhasedRelease {
                    phased_release_id: phased_id(&[PhasedReleaseState::Active])?,
                    state: PhasedReleaseState::Paused,
                },
                "paused the phased release",
            ),
            PhasedAction::Resume => (
                StoreWrite::SetPhasedRelease {
                    phased_release_id: phased_id(&[PhasedReleaseState::Paused])?,
                    state: PhasedReleaseState::Active,
                },
                "resumed the phased release",
            ),
            PhasedAction::Complete => (
                StoreWrite::SetPhasedRelease {
                    phased_release_id: phased_id(&[
                        PhasedReleaseState::Active,
                        PhasedReleaseState::Paused,
                    ])?,
                    state: PhasedReleaseState::Complete,
                },
                "released it to everyone",
            ),
            PhasedAction::Release => {
                if timeline.ids.asc_version_state.as_deref() != Some("PENDING_DEVELOPER_RELEASE") {
                    bail!(
                        "{} isn't approved and waiting for release.",
                        input.version
                    );
                }
                (StoreWrite::ReleaseVersion { version_id }, "put it on sale")
            }
        };
        write(&project, write_to_send, cx).await?;
        Ok(ToolkitOutput::text(format!("{}: {done}.", input.version)))
    })
}

#[derive(Deserialize, JsonSchema)]
struct WaitInput {
    /// Only this version. Defaults to the most recent versions.
    #[serde(default)]
    version: Option<String>,
    /// How long to wait at most, up to 10 minutes. Defaults to 10.
    #[serde(default)]
    minutes: Option<u64>,
}

fn describe_versions(store: &Entity<ReleasesStore>, version: Option<&str>, cx: &App) -> String {
    let mut text = String::new();
    for app in store.read(cx).apps() {
        let releases = app
            .releases
            .iter()
            .filter(|release| version.is_none_or(|version| release.version == version))
            .take(if version.is_some() { 1 } else { 3 });
        for release in releases {
            text.push_str(&describe_release(release));
        }
    }
    text
}

fn release_wait(
    project: Entity<Project>,
    input: WaitInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let store = ready_store(&project, cx).await?;
        let minutes = input
            .minutes
            .unwrap_or(MAX_WAIT_MINUTES)
            .clamp(1, MAX_WAIT_MINUTES);
        let before = cx.update(|cx| describe_versions(&store, input.version.as_deref(), cx));
        let started = Instant::now();
        loop {
            cx.background_executor().timer(WAIT_INTERVAL).await;
            store.update(cx, |store, cx| store.refresh(cx));
            while cx.update(|cx| store.read(cx).is_refreshing()) {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
            }
            let now = cx.update(|cx| describe_versions(&store, input.version.as_deref(), cx));
            if now != before {
                return Ok(ToolkitOutput::text(format!("Something moved:\n{now}")));
            }
            if started.elapsed() >= Duration::from_secs(minutes * 60) {
                return Ok(ToolkitOutput::text(format!(
                    "Nothing changed in {minutes} minutes:\n{now}"
                )));
            }
        }
    })
}

/// The file name of the ready-made "Lançar versão" agent in the personal agents folder.
const RELEASE_AGENT_FILE: &str = "lancar-versao.md";

const RELEASE_AGENT: &str = "---
name: Lançar versão
command: lancar
description: Leva uma versão do app da branch até as lojas, confirmando cada escrita.
profile: write
toolkits: [releases]
---

Você leva uma versão do app até a App Store e o Google Play com as tools de Lançamentos.
Cada tool que escreve pede confirmação; nunca tente contornar isso, nem faça push.

1. `release_status` para ver onde a versão está e o que falta.
2. Se não houver build da versão no EAS: `eas_build` da branch pedida, com o perfil de loja
   (normalmente `production`). Pergunte a branch e o perfil se não foram ditos.
3. `release_wait` até os builds terminarem. Build com falha: pare e mostre o motivo.
4. `eas_submit` de cada plataforma. Se o `eas.json` não tiver o perfil de submit, mostre o
   trecho que a tool devolveu e pergunte se pode adicioná-lo.
5. `release_wait` até o App Store Connect processar o build e o Play mostrar a versão na track.
6. iOS: `release_prepare_version`. Se faltar a resposta de criptografia, pergunte ao usuário.
   Depois `release_notes` com o What's New em todos os idiomas da versão e
   `release_submit_review`.
7. Android: `release_notes` e, quando pedirem, `release_promote` para `production` com
   lançamento gradual (comece em 10% se ninguém disser outra coisa).
8. Termine com um resumo por plataforma: o que foi feito e o que ainda espera (review da Apple,
   rollout do Play). Depois da aprovação, `release_phased` libera ou acompanha o gradual.

Se uma tool recusar, explique o motivo em uma frase e diga o que o usuário precisa fazer.
";

/// Writes the ready-made release agent to the personal agents (keeping an edited one) and
/// opens it.
pub fn create_release_agent(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let fs = workspace.project().read(cx).fs().clone();
    let directory = task_agents::personal_agents_dir();
    let path = directory.join(RELEASE_AGENT_FILE);
    cx.spawn_in(window, async move |workspace, cx| {
        if !fs.is_file(&path).await {
            fs.create_dir(&directory).await?;
            fs.write(&path, RELEASE_AGENT.as_bytes()).await?;
        }
        cx.update(|_, cx| task_agents::notify_task_agents_changed(cx))?;
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, OpenOptions::default(), window, cx)
            })?
            .await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::RELEASES.into(),
            name: t!("app_releases.dock.title"),
            description: t!("app_releases.toolkit.description"),
            icon: "smartphone".into(),
            mention: None,
            tools: vec![
                ToolkitTool::new(
                    "release_status",
                    t!("app_releases.toolkit.read_status"),
                    "Shows where the app's recent versions are on EAS, App Store Connect and \
                     Google Play: build, submission, processing, TestFlight, review, tracks and \
                     rollout, per platform.",
                    ToolAccess::Read,
                    release_status,
                ),
                ToolkitTool::new(
                    "release_promote",
                    t!("app_releases.toolkit.promoted"),
                    "Promotes an Android version code to another Google Play track, optionally \
                     as a staged rollout on production.",
                    ToolAccess::AlwaysConfirm,
                    release_promote,
                ),
                ToolkitTool::new(
                    "release_rollout",
                    t!("app_releases.toolkit.rollout"),
                    "Changes, halts or resumes the staged rollout of an Android version code on \
                     the Google Play production track.",
                    ToolAccess::AlwaysConfirm,
                    release_rollout,
                ),
                ToolkitTool::new(
                    "release_submit_review",
                    t!("app_releases.toolkit.submitted"),
                    "Submits an iOS App Store version to App Review. Refuses, listing what is \
                     missing, while the version has no processed build, no export compliance \
                     answer or an empty What's New.",
                    ToolAccess::AlwaysConfirm,
                    release_submit_review,
                ),
                ToolkitTool::new(
                    "eas_build",
                    t!("app_releases.toolkit.built"),
                    "Starts an EAS cloud build of what a branch has on origin, from a throwaway \
                     worktree with a clean install. Returns the EAS build ids once the build is \
                     queued; the build itself takes a while on EAS.",
                    ToolAccess::AlwaysConfirm,
                    eas_build,
                ),
                ToolkitTool::new(
                    "eas_submit",
                    t!("app_releases.toolkit.sent_to_store"),
                    "Sends a finished EAS build to App Store Connect (TestFlight) or to its \
                     Google Play track with eas submit.",
                    ToolAccess::AlwaysConfirm,
                    eas_submit,
                ),
                ToolkitTool::new(
                    "release_prepare_version",
                    t!("app_releases.toolkit.prepared"),
                    "Creates the iOS App Store version (or renames the one open for changes), \
                     attaches a processed build, sets how it is released and answers export \
                     compliance when the build has no answer.",
                    ToolAccess::AlwaysConfirm,
                    release_prepare_version,
                ),
                ToolkitTool::new(
                    "release_notes",
                    t!("app_releases.toolkit.notes"),
                    "Writes the release notes per language: What's New of the iOS version, or \
                     the notes of the Android release on its Play track.",
                    ToolAccess::AlwaysConfirm,
                    release_notes,
                ),
                ToolkitTool::new(
                    "release_testflight",
                    t!("app_releases.toolkit.testflight"),
                    "Adds an iOS build to a TestFlight group, saves What to Test and sends it to \
                     Beta App Review.",
                    ToolAccess::AlwaysConfirm,
                    release_testflight,
                ),
                ToolkitTool::new(
                    "release_phased",
                    t!("app_releases.toolkit.phased"),
                    "Starts, pauses, resumes or completes the phased release of an iOS version, \
                     or releases an approved version that waits for the developer.",
                    ToolAccess::AlwaysConfirm,
                    release_phased,
                ),
                ToolkitTool::new(
                    "release_wait",
                    t!("app_releases.toolkit.waited"),
                    "Waits until a build, submission, processing or review of the app changes \
                     state, up to 10 minutes, and returns the new state.",
                    ToolAccess::Read,
                    release_wait,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_rollout_percentages() {
        assert_eq!(fraction_from_percent(Some(20.0)).unwrap(), Some(0.2));
        assert_eq!(fraction_from_percent(None).unwrap(), None);
        assert!(fraction_from_percent(Some(0.0)).is_err());
        assert!(fraction_from_percent(Some(150.0)).is_err());
    }

    #[test]
    fn the_release_agent_parses() {
        let agent = task_agents::parse_task_agent(
            std::path::Path::new("/agents/lancar-versao.md"),
            RELEASE_AGENT,
            task_agents::TaskAgentSource::Personal,
        )
        .unwrap();
        assert_eq!(agent.command, "lancar");
        assert_eq!(agent.toolkits, ["releases"]);
        assert!(agent.instructions.contains("release_prepare_version"));
    }
}
