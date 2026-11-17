//! One timeline per version, joining EAS, App Store Connect and Google Play. Everything here is
//! pure so the correlation can be tested with recorded responses.

use crate::{
    app_store_connect::{self as asc, PhasedReleaseState, ProcessingState},
    eas::{self, BuildStatus, SubmissionStatus},
    google_play::{self, PRODUCTION_TRACK},
};
use chrono::{DateTime, Utc};
use collections::{BTreeMap, HashMap};
use gpui::SharedString;
use i18n::t;
use serde::Serialize;
use std::cmp::Ordering;

/// Versions older than these are history nobody acts on.
const MAX_RELEASES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::Android => "Android",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum StepKind {
    Build,
    Submit,
    Processing,
    TestFlight,
    Review,
    Live,
    Testing,
    Production,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum StepState {
    Done,
    /// Nothing has happened yet; usually the next thing to do.
    Pending,
    Running,
    /// Needs a person: rejected, rollout halted, beta review refused.
    Attention,
    Failed,
    /// The store this step comes from has no account or refused it.
    Unknown,
}

impl StepState {
    /// Higher is more urgent, for the status bar chip.
    pub fn urgency(self) -> u8 {
        match self {
            Self::Failed | Self::Attention => 4,
            Self::Running => 3,
            Self::Pending => 2,
            Self::Unknown => 1,
            Self::Done => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub kind: StepKind,
    pub state: StepState,
    pub detail: Option<SharedString>,
    pub at: Option<DateTime<Utc>>,
}

impl Step {
    fn new(kind: StepKind, state: StepState) -> Self {
        Self {
            kind,
            state,
            detail: None,
            at: None,
        }
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn maybe_detail(mut self, detail: Option<SharedString>) -> Self {
        self.detail = detail;
        self
    }

    fn at(mut self, at: Option<DateTime<Utc>>) -> Self {
        self.at = at;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackEntry {
    pub track: String,
    pub status: String,
    pub user_fraction: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReleaseNote {
    pub language: String,
    pub text: String,
}

/// The ids a write action needs, so the release tab doesn't have to look them up again.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StoreIds {
    pub eas_build_id: Option<String>,
    pub asc_build_id: Option<String>,
    pub asc_version_id: Option<String>,
    pub asc_phased_release: Option<(String, PhasedReleaseState)>,
    pub asc_version_state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlatformTimeline {
    pub platform: Platform,
    /// The build number on iOS, the version code on Android.
    pub build: Option<String>,
    pub steps: Vec<Step>,
    pub profile: Option<String>,
    pub commit: Option<String>,
    pub commit_message: Option<String>,
    /// Uploaded without EAS, so there is no commit or profile to show.
    pub outside_eas: bool,
    pub testflight_groups: Vec<String>,
    pub tracks: Vec<TrackEntry>,
    pub notes: Vec<ReleaseNote>,
    pub rejected: bool,
    pub ids: StoreIds,
}

impl PlatformTimeline {
    /// The first step that isn't done, or the last one when everything is.
    pub fn current_step(&self) -> Option<&Step> {
        self.steps
            .iter()
            .find(|step| step.state != StepState::Done && step.state != StepState::Unknown)
            .or_else(|| {
                self.steps
                    .iter()
                    .rev()
                    .find(|step| step.state != StepState::Unknown)
            })
            .or_else(|| self.steps.last())
    }

    pub fn is_live(&self) -> bool {
        self.steps
            .last()
            .is_some_and(|step| step.state == StepState::Done)
    }

    pub fn is_moving(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.state == StepState::Running)
    }

    pub fn step(&self, kind: StepKind) -> Option<&Step> {
        self.steps.iter().find(|step| step.kind == kind)
    }

    pub fn urgency(&self) -> u8 {
        self.current_step()
            .map(|step| step.state.urgency())
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Release {
    pub version: String,
    pub platforms: Vec<PlatformTimeline>,
}

impl Release {
    /// `2.4.0 (128)`, as the tab and the dock call it.
    pub fn title(&self) -> String {
        match self.display_build() {
            Some(build) => format!("{} ({build})", self.version),
            None => self.version.clone(),
        }
    }

    pub fn display_build(&self) -> Option<&str> {
        self.platforms
            .iter()
            .filter_map(|timeline| timeline.build.as_deref())
            .max_by(|left, right| compare_build_numbers(left, right))
    }

    pub fn is_moving(&self) -> bool {
        self.platforms.iter().any(PlatformTimeline::is_moving)
    }

    pub fn needs_attention(&self) -> bool {
        self.platforms.iter().any(|timeline| {
            timeline
                .current_step()
                .is_some_and(|step| matches!(step.state, StepState::Attention | StepState::Failed))
        })
    }

    pub fn is_live_everywhere(&self) -> bool {
        !self.platforms.is_empty() && self.platforms.iter().all(PlatformTimeline::is_live)
    }

    pub fn platform(&self, platform: Platform) -> Option<&PlatformTimeline> {
        self.platforms
            .iter()
            .find(|timeline| timeline.platform == platform)
    }
}

/// Which services answered for this refresh. `None` means the service has no account or the
/// app has no id for it, so its steps are unknown rather than pending.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub eas: Option<eas::Activity>,
    pub app_store: Option<AppStoreData>,
    pub google_play: Option<Vec<google_play::Track>>,
    /// Which platforms the app ships, from `app.json`; a platform with no id is left out.
    pub platforms: Vec<Platform>,
}

#[derive(Clone, Debug, Default)]
pub struct AppStoreData {
    pub app_id: String,
    pub builds: Vec<asc::Build>,
    pub versions: Vec<asc::Version>,
}

/// Compares `2.10.0` after `2.9.1`; anything that isn't a number compares as text.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let parts = |version: &str| -> Vec<String> {
        version
            .split(|character: char| character == '.' || character == '-')
            .map(str::to_string)
            .collect()
    };
    let left_parts = parts(left);
    let right_parts = parts(right);
    for index in 0..left_parts.len().max(right_parts.len()) {
        let left_part = left_parts.get(index).map(String::as_str).unwrap_or("0");
        let right_part = right_parts.get(index).map(String::as_str).unwrap_or("0");
        let ordering = match (left_part.parse::<u64>(), right_part.parse::<u64>()) {
            (Ok(left_number), Ok(right_number)) => left_number.cmp(&right_number),
            _ => left_part.cmp(right_part),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

pub fn compare_build_numbers(left: &str, right: &str) -> Ordering {
    compare_versions(left, right)
}

/// Joins the three sources into releases, newest version first.
pub fn build_releases(snapshot: &Snapshot) -> Vec<Release> {
    let mut builds_by_version: BTreeMap<String, HashMap<Platform, String>> = BTreeMap::default();
    let mut consider = |platform: Platform, version: Option<&str>, build: Option<&str>| {
        let (Some(version), Some(build)) = (version, build) else {
            return;
        };
        if version.is_empty() || build.is_empty() || !snapshot.platforms.contains(&platform) {
            return;
        }
        let entry = builds_by_version.entry(version.to_string()).or_default();
        let replace = entry
            .get(&platform)
            .is_none_or(|current| compare_build_numbers(build, current) == Ordering::Greater);
        if replace {
            entry.insert(platform, build.to_string());
        }
    };

    if let Some(activity) = &snapshot.eas {
        for build in &activity.builds {
            if build.status == BuildStatus::Canceled {
                continue;
            }
            consider(
                build.platform,
                build.app_version.as_deref(),
                build.build_version.as_deref(),
            );
        }
    }
    if let Some(app_store) = &snapshot.app_store {
        for build in &app_store.builds {
            consider(
                Platform::Ios,
                build.version.as_deref(),
                Some(&build.build_number),
            );
        }
    }
    if let Some(tracks) = &snapshot.google_play {
        for track in tracks {
            for release in &track.releases {
                for version_code in &release.version_codes {
                    let version = snapshot
                        .eas
                        .as_ref()
                        .and_then(|activity| {
                            activity.builds.iter().find(|build| {
                                build.platform == Platform::Android
                                    && build.build_version.as_deref() == Some(version_code)
                            })
                        })
                        .and_then(|build| build.app_version.clone())
                        .or_else(|| release.version_name());
                    consider(Platform::Android, version.as_deref(), Some(version_code));
                }
            }
        }
    }

    let mut releases: Vec<Release> = builds_by_version
        .into_iter()
        .map(|(version, builds)| {
            let mut platforms: Vec<PlatformTimeline> = builds
                .into_iter()
                .map(|(platform, build)| match platform {
                    Platform::Ios => ios_timeline(snapshot, &version, &build),
                    Platform::Android => android_timeline(snapshot, &version, &build),
                })
                .collect();
            platforms.sort_by_key(|timeline| timeline.platform);
            Release { version, platforms }
        })
        .collect();
    releases.sort_by(|left, right| compare_versions(&right.version, &left.version));
    releases.truncate(MAX_RELEASES);
    releases
}

fn empty_timeline(platform: Platform, build: &str) -> PlatformTimeline {
    PlatformTimeline {
        platform,
        build: Some(build.to_string()),
        steps: Vec::new(),
        profile: None,
        commit: None,
        commit_message: None,
        outside_eas: false,
        testflight_groups: Vec::new(),
        tracks: Vec::new(),
        notes: Vec::new(),
        rejected: false,
        ids: StoreIds::default(),
    }
}

fn find_eas_build<'a>(
    snapshot: &'a Snapshot,
    platform: Platform,
    version: &str,
    build: &str,
) -> Option<&'a eas::Build> {
    snapshot
        .eas
        .as_ref()?
        .builds
        .iter()
        .filter(|candidate| {
            candidate.platform == platform
                && candidate.app_version.as_deref() == Some(version)
                && candidate.build_version.as_deref() == Some(build)
                && candidate.status != BuildStatus::Canceled
        })
        .max_by_key(|candidate| candidate.created_at)
}

fn find_eas_submission<'a>(
    snapshot: &'a Snapshot,
    platform: Platform,
    eas_build: Option<&eas::Build>,
    version: &str,
    build: &str,
) -> Option<&'a eas::Submission> {
    snapshot
        .eas
        .as_ref()?
        .submissions
        .iter()
        .filter(|submission| {
            submission.platform == platform
                && match (eas_build, submission.build_id.as_deref()) {
                    (Some(eas_build), Some(build_id)) => eas_build.id == build_id,
                    _ => {
                        submission.app_version.as_deref() == Some(version)
                            && submission.build_version.as_deref() == Some(build)
                    }
                }
        })
        .max_by_key(|submission| submission.created_at)
}

fn short_commit(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn build_step(eas_build: Option<&eas::Build>, store_has_build: bool) -> Step {
    let Some(eas_build) = eas_build else {
        return if store_has_build {
            Step::new(StepKind::Build, StepState::Done).detail(t!("app_releases.step.outside_eas"))
        } else {
            Step::new(StepKind::Build, StepState::Unknown)
        };
    };
    let state = match eas_build.status {
        BuildStatus::New | BuildStatus::InQueue | BuildStatus::InProgress => StepState::Running,
        BuildStatus::PendingCancel | BuildStatus::Canceled | BuildStatus::Errored => {
            StepState::Failed
        }
        BuildStatus::Finished => StepState::Done,
        BuildStatus::Unknown => StepState::Pending,
    };
    let detail = match eas_build.status {
        BuildStatus::InQueue | BuildStatus::New => Some(match eas_build.queue_position {
            Some(position) => t!("app_releases.step.queue_position", position = position),
            None => t!("app_releases.step.in_queue"),
        }),
        BuildStatus::Errored => eas_build.error.clone().map(SharedString::from),
        BuildStatus::Canceled | BuildStatus::PendingCancel => {
            Some(t!("app_releases.step.canceled"))
        }
        _ => {
            let profile = eas_build.profile.clone().unwrap_or_default();
            Some(match eas_build.commit.as_deref() {
                Some(commit) => format!("{profile} · {}", short_commit(commit)).into(),
                None => profile.into(),
            })
            .filter(|detail: &SharedString| !detail.is_empty())
        }
    };
    Step::new(StepKind::Build, state)
        .maybe_detail(detail)
        .at(eas_build.completed_at.or(eas_build.created_at))
}

fn submit_step(submission: Option<&eas::Submission>, store_has_build: bool) -> Step {
    match submission {
        Some(submission) => {
            let state = match submission.status {
                SubmissionStatus::AwaitingBuild
                | SubmissionStatus::InQueue
                | SubmissionStatus::InProgress => StepState::Running,
                SubmissionStatus::Finished => StepState::Done,
                SubmissionStatus::Errored | SubmissionStatus::Canceled => StepState::Failed,
                SubmissionStatus::Unknown => StepState::Pending,
            };
            Step::new(StepKind::Submit, state)
                .maybe_detail(submission.error.clone().map(SharedString::from))
                .at(submission.completed_at.or(submission.created_at))
        }
        None if store_has_build => Step::new(StepKind::Submit, StepState::Done),
        None => Step::new(StepKind::Submit, StepState::Pending),
    }
}

fn apply_eas_details(timeline: &mut PlatformTimeline, eas_build: Option<&eas::Build>) {
    match eas_build {
        Some(eas_build) => {
            timeline.profile = eas_build.profile.clone();
            timeline.commit = eas_build.commit.clone();
            timeline.commit_message = eas_build.commit_message.clone();
            timeline.ids.eas_build_id = Some(eas_build.id.clone());
        }
        None => timeline.outside_eas = true,
    }
}

fn ios_timeline(snapshot: &Snapshot, version: &str, build: &str) -> PlatformTimeline {
    let mut timeline = empty_timeline(Platform::Ios, build);
    let eas_build = find_eas_build(snapshot, Platform::Ios, version, build);
    let asc_build = snapshot.app_store.as_ref().and_then(|app_store| {
        app_store.builds.iter().find(|candidate| {
            candidate.build_number == build && candidate.version.as_deref() == Some(version)
        })
    });
    let asc_version = snapshot.app_store.as_ref().and_then(|app_store| {
        let same_version = app_store
            .versions
            .iter()
            .filter(|candidate| candidate.version == version);
        let with_this_build = same_version.clone().find(|candidate| {
            asc_build.is_some_and(|asc_build| candidate.build_id.as_deref() == Some(&asc_build.id))
        });
        with_this_build.or_else(|| same_version.max_by_key(|candidate| candidate.created_at))
    });
    apply_eas_details(&mut timeline, eas_build);
    if eas_build.is_none() && asc_build.is_none() {
        timeline.outside_eas = false;
    }

    timeline
        .steps
        .push(build_step(eas_build, asc_build.is_some()));
    let submission = find_eas_submission(snapshot, Platform::Ios, eas_build, version, build);
    timeline
        .steps
        .push(submit_step(submission, asc_build.is_some()));

    let app_store_known = snapshot.app_store.is_some();
    let unknown = |kind| Step::new(kind, StepState::Unknown);

    if !app_store_known {
        timeline.steps.push(unknown(StepKind::Processing));
        timeline.steps.push(unknown(StepKind::TestFlight));
        timeline.steps.push(unknown(StepKind::Review));
        timeline.steps.push(unknown(StepKind::Live));
        return timeline;
    }

    timeline.ids.asc_build_id = asc_build.map(|asc_build| asc_build.id.clone());
    timeline.steps.push(match asc_build {
        Some(asc_build) => {
            let state = match asc_build.processing_state {
                ProcessingState::Processing => StepState::Running,
                ProcessingState::Valid => StepState::Done,
                ProcessingState::Failed | ProcessingState::Invalid => StepState::Failed,
                ProcessingState::Unknown => StepState::Pending,
            };
            let detail = asc_build
                .expired
                .then(|| t!("app_releases.step.build_expired"));
            Step::new(StepKind::Processing, state)
                .maybe_detail(detail)
                .at(asc_build.uploaded_at)
        }
        None => Step::new(StepKind::Processing, StepState::Pending),
    });

    timeline.steps.push(match asc_build {
        Some(asc_build) => {
            timeline.testflight_groups = asc_build
                .groups
                .iter()
                .map(|group| group.name.clone())
                .collect();
            testflight_step(asc_build)
        }
        None => Step::new(StepKind::TestFlight, StepState::Pending),
    });

    match asc_version {
        Some(asc_version) => {
            timeline.ids.asc_version_id = Some(asc_version.id.clone());
            timeline.ids.asc_version_state = Some(asc_version.state.clone());
            timeline.ids.asc_phased_release = asc_version
                .phased_release
                .as_ref()
                .map(|phased| (phased.id.clone(), phased.state));
            let (review, live) = review_and_live_steps(asc_version);
            timeline.rejected = review.state == StepState::Attention;
            timeline.steps.push(review);
            timeline.steps.push(live);
        }
        None => {
            timeline
                .steps
                .push(Step::new(StepKind::Review, StepState::Pending));
            timeline
                .steps
                .push(Step::new(StepKind::Live, StepState::Pending));
        }
    }
    timeline
}

fn testflight_step(asc_build: &asc::Build) -> Step {
    let groups = asc_build.groups.len();
    let group_detail = (groups > 0).then(|| {
        asc_build
            .groups
            .iter()
            .map(|group| group.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
            .into()
    });
    if asc_build.expired {
        return Step::new(StepKind::TestFlight, StepState::Done)
            .detail(t!("app_releases.step.build_expired"));
    }
    match asc_build.external_state.as_deref() {
        Some("WAITING_FOR_BETA_REVIEW") | Some("IN_BETA_REVIEW") => {
            Step::new(StepKind::TestFlight, StepState::Running)
                .detail(t!("app_releases.step.beta_review"))
        }
        Some("BETA_REJECTED") => Step::new(StepKind::TestFlight, StepState::Attention)
            .detail(t!("app_releases.step.beta_rejected")),
        Some("PROCESSING_EXCEPTION") => Step::new(StepKind::TestFlight, StepState::Failed),
        Some("MISSING_EXPORT_COMPLIANCE") => Step::new(StepKind::TestFlight, StepState::Attention)
            .detail(t!("app_releases.step.export_compliance")),
        _ if groups > 0 || asc_build.internal_state.as_deref() == Some("IN_BETA_TESTING") => {
            Step::new(StepKind::TestFlight, StepState::Done).maybe_detail(group_detail)
        }
        _ => Step::new(StepKind::TestFlight, StepState::Pending),
    }
}

fn review_and_live_steps(asc_version: &asc::Version) -> (Step, Step) {
    let at = asc_version.created_at;
    let review = |state| Step::new(StepKind::Review, state).at(at);
    let live = |state| Step::new(StepKind::Live, state);
    match asc_version.state.as_str() {
        "WAITING_FOR_REVIEW" => (
            review(StepState::Running).detail(t!("app_releases.step.waiting_for_review")),
            live(StepState::Pending),
        ),
        "IN_REVIEW" => (
            review(StepState::Running).detail(t!("app_releases.step.in_review")),
            live(StepState::Pending),
        ),
        "REJECTED" | "METADATA_REJECTED" | "INVALID_BINARY" => (
            review(StepState::Attention).detail(t!("app_releases.step.rejected")),
            live(StepState::Pending),
        ),
        "PENDING_DEVELOPER_RELEASE" => (
            review(StepState::Done).detail(t!("app_releases.step.approved")),
            live(StepState::Pending).detail(t!("app_releases.step.waiting_for_release")),
        ),
        "PENDING_APPLE_RELEASE" | "PROCESSING_FOR_APP_STORE" | "PROCESSING_FOR_DISTRIBUTION" => (
            review(StepState::Done).detail(t!("app_releases.step.approved")),
            live(StepState::Running),
        ),
        "READY_FOR_SALE" | "READY_FOR_DISTRIBUTION" => {
            let live_step = match &asc_version.phased_release {
                Some(phased) if phased.state == PhasedReleaseState::Active => {
                    live(StepState::Running).detail(t!(
                        "app_releases.step.phased_day",
                        day = phased.current_day.unwrap_or(1)
                    ))
                }
                Some(phased) if phased.state == PhasedReleaseState::Paused => {
                    live(StepState::Attention).detail(t!("app_releases.step.phased_paused"))
                }
                _ => live(StepState::Done),
            };
            (review(StepState::Done), live_step)
        }
        "DEVELOPER_REJECTED" | "PREPARE_FOR_SUBMISSION" | "" => (
            review(StepState::Pending).detail(t!("app_releases.step.not_submitted")),
            live(StepState::Pending),
        ),
        "DEVELOPER_REMOVED_FROM_SALE" | "REMOVED_FROM_SALE" | "REPLACED_WITH_NEW_VERSION" => {
            (review(StepState::Done), live(StepState::Done))
        }
        other => (
            review(StepState::Pending).detail(SharedString::from(other.to_string())),
            live(StepState::Pending),
        ),
    }
}

fn track_state(status: &str) -> StepState {
    match status {
        "completed" => StepState::Done,
        "inProgress" => StepState::Running,
        "halted" => StepState::Attention,
        _ => StepState::Pending,
    }
}

fn percent(fraction: f64) -> String {
    format!("{}%", (fraction * 100.0).round() as i64)
}

fn android_timeline(snapshot: &Snapshot, version: &str, build: &str) -> PlatformTimeline {
    let mut timeline = empty_timeline(Platform::Android, build);
    let eas_build = find_eas_build(snapshot, Platform::Android, version, build);
    let mut placements: Vec<(&google_play::Track, &google_play::TrackRelease)> = Vec::new();
    if let Some(tracks) = &snapshot.google_play {
        for track in tracks {
            for release in &track.releases {
                if release.version_codes.iter().any(|code| code == build) {
                    placements.push((track, release));
                }
            }
        }
    }
    let in_play = !placements.is_empty();
    apply_eas_details(&mut timeline, eas_build);
    if eas_build.is_none() && !in_play {
        timeline.outside_eas = false;
    }
    timeline.tracks = placements
        .iter()
        .map(|(track, release)| TrackEntry {
            track: track.track.clone(),
            status: release.status.clone(),
            user_fraction: release.user_fraction,
        })
        .collect();
    timeline.notes = placements
        .iter()
        .flat_map(|(_, release)| release.release_notes.iter())
        .fold(Vec::<ReleaseNote>::new(), |mut notes, note| {
            if !notes.iter().any(|known| known.language == note.language) {
                notes.push(ReleaseNote {
                    language: note.language.clone(),
                    text: note.text.clone(),
                });
            }
            notes
        });

    timeline.steps.push(build_step(eas_build, in_play));
    let submission = find_eas_submission(snapshot, Platform::Android, eas_build, version, build);
    let submit = submit_step(submission, in_play);
    // Play has no review state: a finished submission shows as sent until a track lists it.
    let submit = if submit.state == StepState::Done && !in_play && snapshot.google_play.is_some() {
        submit.detail(t!("app_releases.step.sent_to_play"))
    } else {
        submit
    };
    timeline.steps.push(submit);

    if snapshot.google_play.is_none() {
        timeline
            .steps
            .push(Step::new(StepKind::Testing, StepState::Unknown));
        timeline
            .steps
            .push(Step::new(StepKind::Production, StepState::Unknown));
        return timeline;
    }

    let testing: Vec<_> = placements
        .iter()
        .filter(|(track, _)| track.track != PRODUCTION_TRACK)
        .collect();
    let production = placements
        .iter()
        .find(|(track, _)| track.track == PRODUCTION_TRACK);

    timeline.steps.push(if testing.is_empty() {
        if production.is_some() {
            Step::new(StepKind::Testing, StepState::Done)
        } else {
            Step::new(StepKind::Testing, StepState::Pending)
        }
    } else {
        let state = testing
            .iter()
            .map(|(_, release)| track_state(&release.status))
            .max_by_key(|state| match state {
                StepState::Done => 3,
                StepState::Running => 2,
                StepState::Attention => 1,
                _ => 0,
            })
            .unwrap_or(StepState::Pending);
        let names = testing
            .iter()
            .map(|(track, _)| track.track.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Step::new(StepKind::Testing, state).detail(names)
    });

    timeline.steps.push(match production {
        Some((_, release)) => {
            let state = track_state(&release.status);
            let detail = match (state, release.user_fraction) {
                (StepState::Running, Some(fraction)) => {
                    Some(t!("app_releases.step.rollout", percent = percent(fraction)))
                }
                (StepState::Attention, _) => Some(t!("app_releases.step.rollout_halted")),
                (StepState::Pending, _) => Some(t!("app_releases.step.draft")),
                _ => None,
            };
            Step::new(StepKind::Production, state).maybe_detail(detail)
        }
        None => Step::new(StepKind::Production, StepState::Pending),
    });
    timeline
}

/// Whether polling should go on: something is building, being submitted, in review or rolling
/// out.
pub fn anything_moving(releases: &[Release]) -> bool {
    releases.iter().any(Release::is_moving)
}

/// The most urgent platform line across releases, for the status bar chip. `None` when every
/// release is live and nothing is moving.
pub fn most_urgent(releases: &[Release]) -> Option<(&Release, &PlatformTimeline)> {
    let newest = releases.first()?;
    let candidates = releases.iter().flat_map(|release| {
        release
            .platforms
            .iter()
            .map(move |timeline| (release, timeline))
    });
    let (release, timeline) = candidates
        .filter(|(release, timeline)| {
            let urgency = timeline.urgency();
            urgency >= 3 || (urgency == 2 && release.version == newest.version)
        })
        .max_by(|(left_release, left), (right_release, right)| {
            left.urgency()
                .cmp(&right.urgency())
                .then_with(|| compare_versions(&left_release.version, &right_release.version))
        })?;
    Some((release, timeline))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum NoticeKind {
    Rejected,
    Approved,
    BuildFailed,
    RolloutHalted,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Notice {
    pub kind: NoticeKind,
    pub version: String,
    pub platform: Platform,
}

/// What changed between two refreshes that is worth a notification. Only transitions count, so
/// opening the dock on an old rejection doesn't notify again.
pub fn notable_changes(previous: &[Release], current: &[Release]) -> Vec<Notice> {
    let mut notices = Vec::new();
    for release in current {
        let Some(previous_release) = previous
            .iter()
            .find(|candidate| candidate.version == release.version)
        else {
            continue;
        };
        for timeline in &release.platforms {
            let Some(previous_timeline) = previous_release.platform(timeline.platform) else {
                continue;
            };
            let state_of =
                |timeline: &PlatformTimeline, kind| timeline.step(kind).map(|step| step.state);
            let changed_to = |kind, state| {
                state_of(timeline, kind) == Some(state)
                    && state_of(previous_timeline, kind) != Some(state)
            };
            let mut notify = |kind| {
                notices.push(Notice {
                    kind,
                    version: release.version.clone(),
                    platform: timeline.platform,
                })
            };
            if changed_to(StepKind::Build, StepState::Failed)
                && previous_timeline.build == timeline.build
            {
                notify(NoticeKind::BuildFailed);
            }
            match timeline.platform {
                Platform::Ios => {
                    if changed_to(StepKind::Review, StepState::Attention) {
                        notify(NoticeKind::Rejected);
                    }
                    if changed_to(StepKind::Review, StepState::Done)
                        && state_of(previous_timeline, StepKind::Review) == Some(StepState::Running)
                    {
                        notify(NoticeKind::Approved);
                    }
                }
                Platform::Android => {
                    if changed_to(StepKind::Production, StepState::Attention) {
                        notify(NoticeKind::RolloutHalted);
                    }
                }
            }
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app_store_connect::tests::{BUILDS_FIXTURE, VERSIONS_FIXTURE},
        eas::tests::ACTIVITY_FIXTURE,
        google_play::tests::TRACKS_FIXTURE,
    };

    fn snapshot() -> Snapshot {
        Snapshot {
            eas: Some(eas::parse_activity(ACTIVITY_FIXTURE).unwrap()),
            app_store: Some(AppStoreData {
                app_id: "1600000000".into(),
                builds: asc::parse_builds(BUILDS_FIXTURE).unwrap(),
                versions: asc::parse_versions(VERSIONS_FIXTURE).unwrap(),
            }),
            google_play: Some(google_play::parse_tracks(TRACKS_FIXTURE).unwrap()),
            platforms: vec![Platform::Ios, Platform::Android],
        }
    }

    fn states(timeline: &PlatformTimeline) -> Vec<(StepKind, StepState)> {
        timeline
            .steps
            .iter()
            .map(|step| (step.kind, step.state))
            .collect()
    }

    #[test]
    fn orders_versions_numerically() {
        assert_eq!(compare_versions("2.10.0", "2.9.1"), Ordering::Greater);
        assert_eq!(compare_versions("2.4", "2.4.0"), Ordering::Equal);
        assert_eq!(compare_build_numbers("128", "99"), Ordering::Greater);
    }

    #[test]
    fn correlates_the_current_version_on_both_platforms() {
        let releases = build_releases(&snapshot());
        let versions: Vec<&str> = releases
            .iter()
            .map(|release| release.version.as_str())
            .collect();
        assert_eq!(versions, ["2.4.0", "2.3.9"]);

        let current = &releases[0];
        assert_eq!(current.title(), "2.4.0 (128)");
        let ios = current.platform(Platform::Ios).unwrap();
        assert_eq!(ios.build.as_deref(), Some("128"), "the newer build wins");
        assert_eq!(
            states(ios),
            vec![
                (StepKind::Build, StepState::Done),
                (StepKind::Submit, StepState::Done),
                (StepKind::Processing, StepState::Done),
                (StepKind::TestFlight, StepState::Running),
                (StepKind::Review, StepState::Running),
                (StepKind::Live, StepState::Pending),
            ]
        );
        assert_eq!(ios.commit.as_deref(), Some("9f2c1ab77de"));
        assert_eq!(ios.testflight_groups, ["Time Trix", "QA externo"]);
        assert_eq!(ios.ids.asc_version_id.as_deref(), Some("v-240"));

        let android = current.platform(Platform::Android).unwrap();
        assert_eq!(
            states(android),
            vec![
                (StepKind::Build, StepState::Running),
                (StepKind::Submit, StepState::Done),
                (StepKind::Testing, StepState::Done),
                (StepKind::Production, StepState::Pending),
            ]
        );
        assert_eq!(android.notes.len(), 2);
        assert!(current.is_moving());
    }

    #[test]
    fn a_build_uploaded_outside_eas_has_no_commit() {
        let releases = build_releases(&snapshot());
        let previous = &releases[1];
        let ios = previous.platform(Platform::Ios).unwrap();
        assert!(ios.outside_eas);
        assert!(ios.commit.is_none());
        assert_eq!(
            ios.step(StepKind::Live).map(|step| step.state),
            Some(StepState::Running),
            "phased release on day 3"
        );
        let android = previous.platform(Platform::Android).unwrap();
        assert_eq!(android.build.as_deref(), Some("127"));
        assert_eq!(
            android.step(StepKind::Production).map(|step| step.state),
            Some(StepState::Running)
        );
        assert_eq!(android.tracks[0].user_fraction, Some(0.2));
    }

    #[test]
    fn a_missing_store_leaves_its_steps_unknown() {
        let mut snapshot = snapshot();
        snapshot.app_store = None;
        snapshot.google_play = None;
        let releases = build_releases(&snapshot);
        let ios = releases[0].platform(Platform::Ios).unwrap();
        assert_eq!(
            ios.step(StepKind::Review).map(|step| step.state),
            Some(StepState::Unknown)
        );
        assert_eq!(
            ios.current_step().map(|step| step.kind),
            Some(StepKind::Submit),
            "unknown steps never become the current one"
        );
    }

    #[test]
    fn platforms_without_an_id_are_left_out() {
        let mut snapshot = snapshot();
        snapshot.platforms = vec![Platform::Android];
        let releases = build_releases(&snapshot);
        assert!(
            releases
                .iter()
                .all(|release| release.platform(Platform::Ios).is_none())
        );
    }

    #[test]
    fn notifies_only_on_transitions() {
        let before = build_releases(&snapshot());
        let mut rejected = snapshot();
        if let Some(app_store) = rejected.app_store.as_mut() {
            app_store.versions[0].state = "REJECTED".into();
        }
        let after = build_releases(&rejected);
        let notices = notable_changes(&before, &after);
        assert_eq!(
            notices,
            vec![Notice {
                kind: NoticeKind::Rejected,
                version: "2.4.0".into(),
                platform: Platform::Ios
            }]
        );
        assert!(after[0].platform(Platform::Ios).unwrap().rejected);
        assert!(notable_changes(&after, &after).is_empty());
        assert!(notable_changes(&[], &after).is_empty());

        let mut approved = snapshot();
        if let Some(app_store) = approved.app_store.as_mut() {
            app_store.versions[0].state = "PENDING_DEVELOPER_RELEASE".into();
        }
        let notices = notable_changes(&before, &build_releases(&approved));
        assert_eq!(notices[0].kind, NoticeKind::Approved);
    }

    #[test]
    fn halted_rollout_is_the_most_urgent() {
        let mut halted = snapshot();
        if let Some(tracks) = halted.google_play.as_mut() {
            tracks[0].releases[0].status = "halted".into();
        }
        let releases = build_releases(&halted);
        let (release, timeline) = most_urgent(&releases).unwrap();
        assert_eq!(release.version, "2.3.9");
        assert_eq!(timeline.platform, Platform::Android);
    }

    #[test]
    fn nothing_urgent_when_everything_is_live() {
        let live = Snapshot {
            eas: None,
            app_store: Some(AppStoreData {
                app_id: "1".into(),
                builds: asc::parse_builds(BUILDS_FIXTURE)
                    .unwrap()
                    .into_iter()
                    .filter(|build| build.build_number == "126")
                    .collect(),
                versions: asc::parse_versions(VERSIONS_FIXTURE)
                    .unwrap()
                    .into_iter()
                    .filter(|version| version.version == "2.3.9")
                    .map(|mut version| {
                        version.phased_release = None;
                        version
                    })
                    .collect(),
            }),
            google_play: None,
            platforms: vec![Platform::Ios],
        };
        let releases = build_releases(&live);
        assert!(releases[0].is_live_everywhere());
        assert!(most_urgent(&releases).is_none());
        assert!(!anything_moving(&releases));
    }
}
