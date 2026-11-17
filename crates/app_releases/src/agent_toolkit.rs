//! The Lançamentos toolkit for agents: read where each version is, and promote, roll out or
//! submit with the accounts of the dock. Every write tool always asks before it runs.

use crate::{
    google_play::{PRODUCTION_TRACK, TrackChange},
    model::{Platform, Release},
    status_label, step_title,
    store::{AccountsState, Detection, ReleasesStore, StoreWrite},
};
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AsyncApp, Entity, Task};
use i18n::t;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};

const READY_TIMEOUT: Duration = Duration::from_secs(45);

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
        write(&project, StoreWrite::SubmitForReview { version_id }, cx).await?;
        Ok(ToolkitOutput::text(format!(
            "Sent {} to App Review.",
            input.version
        )))
    })
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
                    "Submits an iOS App Store version to App Review.",
                    ToolAccess::AlwaysConfirm,
                    release_submit_review,
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
}
