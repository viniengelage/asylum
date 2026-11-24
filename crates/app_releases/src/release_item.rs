//! A version opened from the dock, as an editor tab: the iOS and Android timelines side by side,
//! TestFlight groups, Play tracks and release notes.

use crate::{
    app_store_connect::{self, Localization, PhasedReleaseState, ReleaseType, VersionPreparation},
    google_play::{self, LocalizedText, PRODUCTION_TRACK, TrackChange},
    insights::{AfterLaunch, FeedbackKind, Review, Section},
    model::{
        self, CheckItem, CheckKind, CheckState, Platform, PlatformTimeline, Release, StepKind,
        StepState, VersionOptions,
    },
    state_tone, status_label, step_title,
    store::{ReleasesStore, StoreWrite},
};
use chrono::Local;
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task,
    WeakEntity, px,
};
use i18n::t;
use std::{path::Path, sync::Arc};
use ui::{
    CommonAnimationExt, ContextMenu, EmptyState, HeaderBar, PopoverMenu, PopoverTriggerArea,
    StatusTone, card, choice_chip, error_callout, modal_footer, modal_surface, prelude::*,
    primary_button, secondary_button, section_label, segment_option, segmented_control,
    status_pill,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    ModalView, Workspace,
    item::{Item as WorkspaceItem, ItemEvent},
};

/// The rollout steps offered for a staged release on production.
const ROLLOUT_STEPS: [f64; 4] = [0.1, 0.2, 0.5, 1.0];
const TESTING_TRACKS: [&str; 3] = ["internal", "alpha", "beta"];

fn percent_label(fraction: f64) -> String {
    format!("{}%", (fraction * 100.0).round() as i64)
}

pub fn open(
    workspace: &mut Workspace,
    store: Entity<ReleasesStore>,
    folder: Arc<Path>,
    version: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<ReleaseView>(cx).find(|view| {
        let view = view.read(cx);
        view.folder == folder && view.version == version
    });
    if let Some(view) = existing {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let weak_workspace = workspace.weak_handle();
    let view = cx.new(|cx| ReleaseView::new(store, weak_workspace, folder, version, window, cx));
    view.update(cx, |view, cx| view.ensure_review_notes(cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// What's New of the version being prepared, read so the checklist can say which languages
/// still lack it.
enum ReviewNotes {
    NotLoaded,
    Loading(String),
    Loaded {
        version_id: String,
        localizations: Vec<Localization>,
    },
    Failed(String),
}

enum WriteState {
    Idle,
    Sending,
    Done(SharedString),
    Failed(SharedString),
}

pub struct ReleaseView {
    focus_handle: FocusHandle,
    pub(crate) store: Entity<ReleasesStore>,
    workspace: WeakEntity<Workspace>,
    pub(crate) folder: Arc<Path>,
    pub(crate) version: String,
    what_to_test: Entity<InputField>,
    what_to_test_locale: Entity<InputField>,
    write_state: WriteState,
    write_task: Option<Task<()>>,
    ios_notes: NotesEditor,
    android_notes: NotesEditor,
    notes_task: Option<Task<()>>,
    after_launch: AfterLaunchState,
    after_launch_task: Option<Task<()>>,
    prepare_build: Option<String>,
    prepare_release_type: Option<ReleaseType>,
    prepare_encryption: bool,
    review_notes: ReviewNotes,
    review_notes_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReleaseView {
    fn new(
        store: Entity<ReleasesStore>,
        workspace: WeakEntity<Workspace>,
        folder: Arc<Path>,
        version: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&store, |this, _, cx| {
            this.ensure_review_notes(cx);
            cx.notify()
        })];
        let what_to_test = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &t!("app_releases.write.what_to_test_placeholder"),
            )
        });
        let what_to_test_locale = cx.new(|cx| InputField::new(window, cx, "en-US"));
        Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace,
            folder,
            version,
            what_to_test,
            what_to_test_locale,
            write_state: WriteState::Idle,
            write_task: None,
            ios_notes: NotesEditor::Closed,
            android_notes: NotesEditor::Closed,
            notes_task: None,
            after_launch: AfterLaunchState::Closed,
            after_launch_task: None,
            prepare_build: None,
            prepare_release_type: None,
            prepare_encryption: false,
            review_notes: ReviewNotes::NotLoaded,
            review_notes_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn version_options(&self, cx: &App) -> Option<VersionOptions> {
        self.store
            .read(cx)
            .app_for_folder(&self.folder)?
            .version_options(&self.version)
    }

    /// Loads What's New of the version once it exists, for the checklist.
    fn ensure_review_notes(&mut self, cx: &mut Context<Self>) {
        let Some(options) = self.version_options(cx) else {
            return;
        };
        let Some(version) = options.version.filter(|_| !options.first_release) else {
            return;
        };
        let current = match &self.review_notes {
            ReviewNotes::NotLoaded => None,
            ReviewNotes::Loading(version_id) | ReviewNotes::Failed(version_id) => {
                Some(version_id)
            }
            ReviewNotes::Loaded { version_id, .. } => Some(version_id),
        };
        if current == Some(&version.id) {
            return;
        }
        let version_id = version.id;
        self.review_notes = ReviewNotes::Loading(version_id.clone());
        let task = self.store.update(cx, |store, cx| {
            store.fetch_version_notes(version_id.clone(), cx)
        });
        self.review_notes_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.review_notes = match result {
                    Ok(localizations) => ReviewNotes::Loaded {
                        version_id,
                        localizations,
                    },
                    Err(error) => {
                        log::warn!("Lançamentos: What's New da versão: {error:#}");
                        ReviewNotes::Failed(version_id)
                    }
                };
                cx.notify();
            })
            .log_err();
        }));
    }

    fn review_localizations(&self, version_id: &str) -> Option<&[Localization]> {
        match &self.review_notes {
            ReviewNotes::Loaded {
                version_id: loaded,
                localizations,
            } if loaded == version_id => Some(localizations),
            _ => None,
        }
    }

    fn checklist(&self, options: &VersionOptions, cx: &App) -> Option<Vec<CheckItem>> {
        let data = self
            .store
            .read(cx)
            .app_for_folder(&self.folder)?
            .app_store
            .as_ref()?;
        let localizations = options
            .version
            .as_ref()
            .and_then(|version| self.review_localizations(&version.id));
        Some(model::review_checklist(data, options, localizations))
    }

    /// Every store write goes through here: nothing is sent until the modal is confirmed.
    pub(crate) fn confirm_write(
        &mut self,
        title: SharedString,
        detail: SharedString,
        write: StoreWrite,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.write_state, WriteState::Sending) {
            return;
        }
        let this = cx.weak_entity();
        let service = write.service().label();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |_window, cx| {
                    ConfirmWrite::new(
                        service,
                        title,
                        detail,
                        move |_window, cx| {
                            this.update(cx, |this, cx| this.start_write(write, cx))
                                .log_err();
                        },
                        cx,
                    )
                })
            })
            .log_err();
    }

    fn start_write(&mut self, write: StoreWrite, cx: &mut Context<Self>) {
        self.write_state = WriteState::Sending;
        cx.notify();
        let folder = self.folder.clone();
        let task = self
            .store
            .update(cx, |store, cx| store.run_write(&folder, write, cx));
        self.write_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // The write may have changed the version or its notes; read them again.
                this.review_notes = ReviewNotes::NotLoaded;
                this.write_state = match result {
                    Ok(()) => WriteState::Done(t!("app_releases.write.done")),
                    Err(error) => WriteState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        }));
    }

    fn render_write_state(&self, cx: &Context<Self>) -> Option<AnyElement> {
        match &self.write_state {
            WriteState::Idle => None,
            WriteState::Sending => Some(
                status_pill(t!("app_releases.write.sending"), StatusTone::Info, cx)
                    .into_any_element(),
            ),
            WriteState::Done(message) => {
                Some(status_pill(message.clone(), StatusTone::Success, cx).into_any_element())
            }
            WriteState::Failed(message) => Some(
                error_callout(t!("app_releases.write.failed"), message.clone(), cx)
                    .into_any_element(),
            ),
        }
    }

    fn render_ios_actions(
        &self,
        timeline: &PlatformTimeline,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let build_id = timeline.ids.asc_build_id.clone()?;
        let build = timeline.build.clone().unwrap_or_default();
        let groups: Vec<(String, String)> = self
            .store
            .read(cx)
            .app_for_folder(&self.folder)
            .map(|app| {
                app.beta_groups
                    .iter()
                    .filter(|group| !timeline.testflight_groups.contains(&group.name))
                    .map(|group| (group.id.clone(), group.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let processed =
            timeline.step(StepKind::Processing).map(|step| step.state) == Some(StepState::Done);
        let testflight_state = timeline.step(StepKind::TestFlight).map(|step| step.state);
        let can_submit_beta =
            processed && matches!(testflight_state, Some(StepState::Pending | StepState::Done));
        let view = cx.weak_entity();
        let version = self.version.clone();

        let group_menu = (!groups.is_empty()).then(|| {
            let view = view.clone();
            let build_id = build_id.clone();
            let build = build.clone();
            PopoverMenu::new("release-add-group")
                .trigger(PopoverTriggerArea::new(secondary_button(
                    "release-add-group-trigger",
                    Some((IconName::UserGroup, Color::Muted)),
                    t!("app_releases.write.add_to_group"),
                    true,
                    cx,
                )))
                .menu(move |window, cx| {
                    let groups = groups.clone();
                    let view = view.clone();
                    let build_id = build_id.clone();
                    let build = build.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            for (group_id, group_name) in groups {
                                let view = view.clone();
                                let write = StoreWrite::AddBuildToGroup {
                                    build_id: build_id.clone(),
                                    group_id,
                                };
                                let title = t!(
                                    "app_releases.write.add_to_group_title",
                                    build = build,
                                    group = group_name
                                );
                                menu = menu.entry(group_name, None, move |window, cx| {
                                    let write = write.clone();
                                    let title = title.clone();
                                    view.update(cx, |view, cx| {
                                        view.confirm_write(
                                            title,
                                            t!("app_releases.write.add_to_group_detail"),
                                            write,
                                            window,
                                            cx,
                                        )
                                    })
                                    .log_err();
                                });
                            }
                            menu
                        },
                    ))
                })
                .into_any_element()
        });

        let beta_review = can_submit_beta.then(|| {
            let build_id = build_id.clone();
            let title = t!(
                "app_releases.write.beta_review_title",
                version = version,
                build = build
            );
            secondary_button(
                "release-beta-review",
                Some((IconName::Send, Color::Muted)),
                t!("app_releases.write.beta_review"),
                true,
                cx,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.confirm_write(
                    title.clone(),
                    t!("app_releases.write.beta_review_detail"),
                    StoreWrite::SubmitForBetaReview {
                        build_id: build_id.clone(),
                    },
                    window,
                    cx,
                )
            }))
            .into_any_element()
        });

        let save_what_to_test = secondary_button(
            "release-what-to-test",
            None,
            t!("app_releases.write.save_what_to_test"),
            true,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            let text = this.what_to_test.read(cx).text(cx).trim().to_string();
            let locale = this
                .what_to_test_locale
                .read(cx)
                .text(cx)
                .trim()
                .to_string();
            let locale = if locale.is_empty() {
                "en-US".to_string()
            } else {
                locale
            };
            if text.is_empty() {
                this.write_state = WriteState::Failed(t!("app_releases.write.what_to_test_empty"));
                cx.notify();
                return;
            }
            let title = t!("app_releases.write.what_to_test_title", locale = locale);
            this.confirm_write(
                title,
                text.clone().into(),
                StoreWrite::WhatToTest {
                    build_id: build_id.clone(),
                    locale,
                    text,
                },
                window,
                cx,
            )
        }));

        Some(
            v_flex()
                .gap_2()
                .pt_2()
                .child(section_label(t!("app_releases.write.testflight_actions")))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(group_menu)
                        .children(beta_review),
                )
                .child(section_label(t!("app_releases.write.what_to_test")))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(80.)).child(self.what_to_test_locale.clone()))
                        .child(div().flex_1().child(self.what_to_test.clone())),
                )
                .child(h_flex().justify_end().child(save_what_to_test))
                .into_any_element(),
        )
    }

    fn render_android_actions(
        &self,
        timeline: &PlatformTimeline,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        self.package(cx)?;
        let version_code = timeline.build.clone()?;
        let source_track = timeline
            .tracks
            .iter()
            .filter(|track| track.track != PRODUCTION_TRACK)
            .map(|track| track.track.clone())
            .next_back();
        let production = timeline
            .tracks
            .iter()
            .find(|track| track.track == PRODUCTION_TRACK)
            .cloned();
        let view = cx.weak_entity();
        let version = self.version.clone();

        let promote_menu =
            source_track.map(|from_track| {
                let view = view.clone();
                let version_code = version_code.clone();
                let version = version.clone();
                PopoverMenu::new("release-promote")
                    .trigger(PopoverTriggerArea::new(secondary_button(
                        "release-promote-trigger",
                        Some((IconName::ArrowUp, Color::Muted)),
                        t!("app_releases.write.promote"),
                        true,
                        cx,
                    )))
                    .menu(move |window, cx| {
                        let view = view.clone();
                        let version_code = version_code.clone();
                        let from_track = from_track.clone();
                        let version = version.clone();
                        Some(ContextMenu::build(
                            window,
                            cx,
                            move |mut menu, _window, _cx| {
                                let mut targets: Vec<(String, Option<f64>)> = TESTING_TRACKS
                                    .iter()
                                    .filter(|track| **track != from_track)
                                    .map(|track| (track.to_string(), None))
                                    .collect();
                                targets.extend(ROLLOUT_STEPS.iter().map(|fraction| {
                                    (PRODUCTION_TRACK.to_string(), Some(*fraction))
                                }));
                                for (to_track, fraction) in targets {
                                    let label = match fraction {
                                        Some(fraction) => {
                                            format!("{to_track} · {}", percent_label(fraction))
                                        }
                                        None => to_track.clone(),
                                    };
                                    let view = view.clone();
                                    let change = TrackChange::Promote {
                                        from_track: from_track.clone(),
                                        to_track,
                                        version_code: version_code.clone(),
                                        user_fraction: fraction,
                                    };
                                    let title = t!(
                                        "app_releases.write.promote_title",
                                        version = version,
                                        version_code = version_code,
                                        track = label
                                    );
                                    menu = menu.entry(label, None, move |window, cx| {
                                        let change = change.clone();
                                        let title = title.clone();
                                        view.update(cx, |view, cx| {
                                            view.confirm_write(
                                                title,
                                                t!("app_releases.write.commit_detail"),
                                                StoreWrite::Play(change),
                                                window,
                                                cx,
                                            )
                                        })
                                        .log_err();
                                    });
                                }
                                menu
                            },
                        ))
                    })
                    .into_any_element()
            });

        let mut rollout_controls: Vec<AnyElement> = Vec::new();
        if let Some(production) = production {
            let halted = production.status == "halted";
            let staged = production.status == "inProgress";
            if staged {
                let current = production.user_fraction.unwrap_or(0.0);
                for (index, fraction) in ROLLOUT_STEPS
                    .iter()
                    .copied()
                    .filter(|fraction| *fraction > current)
                    .enumerate()
                {
                    let version_code = version_code.clone();
                    let label = percent_label(fraction);
                    let title = t!(
                        "app_releases.write.rollout_title",
                        version_code = version_code,
                        percent = label
                    );
                    rollout_controls.push(
                        secondary_button(("release-rollout", index), None, label, true, cx)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_write(
                                    title.clone(),
                                    t!("app_releases.write.commit_detail"),
                                    StoreWrite::Play(TrackChange::SetRollout {
                                        track: PRODUCTION_TRACK.to_string(),
                                        version_code: version_code.clone(),
                                        user_fraction: fraction,
                                    }),
                                    window,
                                    cx,
                                )
                            }))
                            .into_any_element(),
                    );
                }
            }
            if staged || halted {
                let (label, title, change) = if halted {
                    (
                        t!("app_releases.write.resume_rollout"),
                        t!(
                            "app_releases.write.resume_rollout_title",
                            version_code = version_code
                        ),
                        TrackChange::Resume {
                            track: PRODUCTION_TRACK.to_string(),
                            version_code: version_code.clone(),
                        },
                    )
                } else {
                    (
                        t!("app_releases.write.halt_rollout"),
                        t!(
                            "app_releases.write.halt_rollout_title",
                            version_code = version_code
                        ),
                        TrackChange::Halt {
                            track: PRODUCTION_TRACK.to_string(),
                            version_code: version_code.clone(),
                        },
                    )
                };
                rollout_controls.push(
                    secondary_button("release-halt-resume", None, label, true, cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_write(
                                title.clone(),
                                t!("app_releases.write.commit_detail"),
                                StoreWrite::Play(change.clone()),
                                window,
                                cx,
                            )
                        }))
                        .into_any_element(),
                );
            }
        }

        if promote_menu.is_none() && rollout_controls.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .gap_2()
                .pt_2()
                .child(section_label(t!("app_releases.write.play_actions")))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(promote_menu)
                        .children(rollout_controls),
                )
                .into_any_element(),
        )
    }

    fn release<'a>(&self, cx: &'a App) -> Option<&'a Release> {
        self.store
            .read(cx)
            .find_release(&self.folder, &self.version)
    }

    fn app_store_app_id(&self, cx: &App) -> Option<String> {
        self.store
            .read(cx)
            .app_for_folder(&self.folder)?
            .app_store_app_id
            .clone()
    }

    fn package(&self, cx: &App) -> Option<String> {
        self.store
            .read(cx)
            .app_for_folder(&self.folder)?
            .link
            .package
            .clone()
    }

    fn render_rejection(&self, cx: &Context<Self>) -> AnyElement {
        let app_id = self.app_store_app_id(cx);
        v_flex()
            .gap_2()
            .child(error_callout(
                t!("app_releases.release.rejected_title"),
                t!("app_releases.release.rejected_detail"),
                cx,
            ))
            .children(app_id.map(|app_id| {
                h_flex().justify_end().child(
                    primary_button(
                        "release-resolution-center",
                        Some(IconName::ArrowUpRight),
                        t!("app_releases.release.resolution_center"),
                        None,
                        cx,
                    )
                    .on_click(move |_, _window, cx| {
                        cx.open_url(&app_store_connect::resolution_center_url(&app_id))
                    }),
                )
            }))
            .into_any_element()
    }

    fn render_timeline(&self, timeline: &PlatformTimeline, cx: &Context<Self>) -> AnyElement {
        let current_kind = timeline.current_step().map(|step| step.kind);
        let colors = cx.theme().colors();
        let (label, tone) = status_label(timeline);
        let steps = timeline.steps.iter().enumerate().map(|(index, step)| {
            let is_current = Some(step.kind) == current_kind && !timeline.is_live();
            let tone_color = state_tone(step.state).color(cx);
            let icon = match step.state {
                StepState::Done => Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success)
                    .into_any_element(),
                StepState::Running => Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Info)
                    .with_rotate_animation(2)
                    .into_any_element(),
                StepState::Attention => Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning)
                    .into_any_element(),
                StepState::Failed => Icon::new(IconName::XCircle)
                    .size(IconSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
                StepState::Pending | StepState::Unknown => Icon::new(IconName::Circle)
                    .size(IconSize::Small)
                    .color(Color::Placeholder)
                    .into_any_element(),
            };
            let detail = match step.state {
                StepState::Unknown => Some(t!("app_releases.step.no_account")),
                _ => step.detail.clone(),
            };
            h_flex()
                .id(("release-step", index))
                .w_full()
                .items_start()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(px(8.))
                .when(is_current, |this| this.bg(tone_color.opacity(0.1)))
                .child(div().pt_0p5().child(icon))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(step_title(step.kind))
                                .size(LabelSize::Small)
                                .weight(if is_current {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::NORMAL
                                })
                                .color(if step.state == StepState::Unknown {
                                    Color::Placeholder
                                } else {
                                    Color::Default
                                }),
                        )
                        .children(detail.map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        })),
                )
                .children(step.at.map(|at| {
                    Label::new(at.with_timezone(&Local).format("%d/%m %H:%M").to_string())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Placeholder)
                }))
        });

        let build_line = match (&timeline.build, &timeline.commit) {
            (Some(build), Some(commit)) => Some(t!(
                "app_releases.release.build_with_commit",
                build = build,
                commit = commit.chars().take(7).collect::<String>(),
                profile = timeline.profile.clone().unwrap_or_default()
            )),
            (Some(build), None) if timeline.outside_eas => {
                Some(t!("app_releases.release.build_outside_eas", build = build))
            }
            (Some(build), None) => Some(t!("app_releases.release.build", build = build)),
            _ => None,
        };

        let is_clean_build = timeline.ids.eas_build_id.as_ref().is_some_and(|id| {
            crate::clean_build::clean_build_ids(&db::kvp::KeyValueStore::global(cx)).contains(id)
        });
        card(cx)
            .flex_1()
            .min_w(px(280.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(timeline.platform.label())
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(status_pill(label, tone, cx))
                    .when(is_clean_build, |this| {
                        this.child(status_pill(
                            t!("app_releases.clean_build.clean_chip"),
                            ui::StatusTone::Info,
                            cx,
                        ))
                    }),
            )
            .children(
                build_line.map(|line| Label::new(line).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .children(timeline.commit_message.clone().map(|message| {
                Label::new(message)
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder)
                    .truncate()
            }))
            .child(
                v_flex()
                    .gap_0p5()
                    .pt_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .children(steps),
            )
            .child(self.render_platform_details(timeline, cx))
            .when_some(
                timeline.ids.eas_build_id.clone().filter(|_| is_clean_build),
                |this, build_id| {
                    let platform = timeline.platform;
                    this.child(
                        ui::secondary_button(
                            SharedString::from(format!("clean-build-submit-{build_id}")),
                            Some((IconName::ArrowUpRight, Color::Muted)),
                            t!("app_releases.clean_build.submit"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                crate::clean_build::submit_build(
                                    this.workspace.clone(),
                                    this.store.clone(),
                                    build_id.clone(),
                                    platform,
                                    window,
                                    cx,
                                );
                            },
                        )),
                    )
                },
            )
            .children(match timeline.platform {
                Platform::Ios => self.render_ios_actions(timeline, cx),
                Platform::Android => self.render_android_actions(timeline, cx),
            })
            .children(match timeline.platform {
                Platform::Ios => self.render_prepare_version(cx),
                Platform::Android => None,
            })
            .children(match timeline.platform {
                Platform::Ios => self.render_app_store_launch(timeline, cx),
                Platform::Android => self.render_play_notes(timeline, cx),
            })
            .into_any_element()
    }

    fn render_platform_details(
        &self,
        timeline: &PlatformTimeline,
        cx: &Context<Self>,
    ) -> AnyElement {
        match timeline.platform {
            Platform::Ios => {
                let app_id = self.app_store_app_id(cx);
                v_flex()
                    .gap_1()
                    .pt_2()
                    .child(section_label(t!("app_releases.release.testflight_groups")))
                    .when(timeline.testflight_groups.is_empty(), |this| {
                        this.child(
                            Label::new(t!("app_releases.release.no_groups"))
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        )
                    })
                    .children(
                        timeline
                            .testflight_groups
                            .iter()
                            .map(|group| Label::new(group.clone()).size(LabelSize::Small)),
                    )
                    .children(app_id.map(|app_id| {
                        h_flex().pt_1().child(
                            secondary_button(
                                "release-open-testflight",
                                Some((IconName::ArrowUpRight, Color::Muted)),
                                t!("app_releases.release.open_testflight"),
                                true,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                cx.open_url(&app_store_connect::testflight_url(&app_id))
                            }),
                        )
                    }))
                    .into_any_element()
            }
            Platform::Android => {
                let package = self.package(cx);
                v_flex()
                    .gap_1()
                    .pt_2()
                    .child(section_label(t!("app_releases.release.tracks")))
                    .when(timeline.tracks.is_empty(), |this| {
                        this.child(
                            Label::new(t!("app_releases.release.no_tracks"))
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        )
                    })
                    .children(timeline.tracks.iter().map(|track| {
                        let status = match track.user_fraction {
                            Some(fraction) => {
                                format!("{} · {}%", track.status, (fraction * 100.0).round() as i64)
                            }
                            None => track.status.clone(),
                        };
                        h_flex()
                            .gap_2()
                            .child(Label::new(track.track.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(status)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    }))
                    .when(!timeline.notes.is_empty(), |this| {
                        this.child(
                            div()
                                .pt_2()
                                .child(section_label(t!("app_releases.release.notes"))),
                        )
                        .children(timeline.notes.iter().map(|note| {
                            v_flex()
                                .child(
                                    Label::new(note.language.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(note.text.clone()).size(LabelSize::Small))
                        }))
                    })
                    .children(package.map(|package| {
                        h_flex().pt_1().child(
                            secondary_button(
                                "release-open-play",
                                Some((IconName::ArrowUpRight, Color::Muted)),
                                t!("app_releases.release.open_play"),
                                true,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                cx.open_url(&google_play::console_url(&package))
                            }),
                        )
                    }))
                    .into_any_element()
            }
        }
    }
}

impl Render for ReleaseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refreshing = self.store.read(cx).is_refreshing();
        let content = match self.release(cx) {
            None if refreshing => {
                EmptyState::new(IconName::LoadCircle, t!("app_releases.dock.refreshing"))
                    .spinning(true)
                    .into_any_element()
            }
            None => {
                EmptyState::new(IconName::Info, t!("app_releases.release.gone")).into_any_element()
            }
            Some(release) => {
                let release = release.clone();
                let rejected = release
                    .platform(Platform::Ios)
                    .is_some_and(|timeline| timeline.rejected);
                v_flex()
                    .id("release-content")
                    .size_full()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_4()
                    .when(rejected, |this| this.child(self.render_rejection(cx)))
                    .children(self.render_write_state(cx))
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .flex_wrap()
                            .gap_4()
                            .children(
                                release
                                    .platforms
                                    .iter()
                                    .map(|timeline| self.render_timeline(timeline, cx)),
                            ),
                    )
                    .child(self.render_after_launch(&release, cx))
                    .into_any_element()
            }
        };
        let title = self
            .release(cx)
            .map(Release::title)
            .unwrap_or_else(|| self.version.clone());
        v_flex()
            .key_context("ReleaseView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                HeaderBar::new("release-header")
                    .start_child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .end_child(
                        IconButton::new("release-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(refreshing)
                            .tooltip(ui::Tooltip::text(t!("app_releases.dock.refresh")))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.store.update(cx, |store, cx| store.refresh(cx));
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

/// A language of the release notes being edited.
struct NoteRow {
    /// The App Store localization id, or the Play language code.
    id: String,
    locale: String,
    original: String,
    input: Entity<InputField>,
}

enum NotesEditor {
    Closed,
    Loading,
    Open(Vec<NoteRow>),
    Failed(SharedString),
}

/// App Store states from which a version can be sent to App Review.
const SUBMITTABLE_STATES: [&str; 4] = [
    "PREPARE_FOR_SUBMISSION",
    "DEVELOPER_REJECTED",
    "REJECTED",
    "METADATA_REJECTED",
];

fn check_title(kind: CheckKind) -> SharedString {
    match kind {
        CheckKind::Version => t!("app_releases.checklist.version"),
        CheckKind::Build => t!("app_releases.checklist.build"),
        CheckKind::Encryption => t!("app_releases.checklist.encryption"),
        CheckKind::WhatsNew => t!("app_releases.checklist.whats_new"),
    }
}

fn render_checklist(checklist: &[CheckItem], cx: &App) -> AnyElement {
    v_flex()
        .gap_1()
        .pt_1()
        .child(section_label(t!("app_releases.checklist.title")))
        .children(checklist.iter().map(|item| {
            let (label, tone) = match item.state {
                CheckState::Passed => (t!("app_releases.checklist.passed"), StatusTone::Success),
                CheckState::Missing => (t!("app_releases.checklist.missing"), StatusTone::Warning),
                CheckState::Unknown => (t!("app_releases.checklist.unknown"), StatusTone::Neutral),
            };
            h_flex()
                .gap_2()
                .child(status_pill(label, tone, cx))
                .child(Label::new(check_title(item.kind)).size(LabelSize::Small))
                .children(item.detail.clone().filter(|detail| !detail.is_empty()).map(
                    |detail| {
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                    },
                ))
        }))
        .into_any_element()
}

/// The confirmation text: what App Store Connect will get, in the order it is sent.
pub(crate) fn prepare_detail(
    options: &VersionOptions,
    preparation: &VersionPreparation,
) -> SharedString {
    let mut lines = Vec::new();
    match (&options.version, &options.editable_version) {
        (Some(_), _) => {}
        (None, Some(editable)) => lines.push(
            t!(
                "app_releases.prepare.detail_rename",
                from = editable.version.clone(),
                to = preparation.version_string.clone()
            )
            .to_string(),
        ),
        (None, None) => lines.push(
            t!(
                "app_releases.prepare.detail_create",
                version = preparation.version_string.clone()
            )
            .to_string(),
        ),
    }
    if let Some(uses) = preparation.uses_non_exempt_encryption {
        lines.push(
            if uses {
                t!("app_releases.prepare.detail_encryption_yes")
            } else {
                t!("app_releases.prepare.detail_encryption_exempt")
            }
            .to_string(),
        );
    }
    lines.push(
        match preparation.release_type {
            ReleaseType::Manual => t!("app_releases.prepare.detail_manual"),
            ReleaseType::AfterApproval => t!("app_releases.prepare.detail_after_approval"),
        }
        .to_string(),
    );
    lines.join("\n").into()
}

fn note_rows(
    notes: Vec<(String, String, String)>,
    window: &mut Window,
    cx: &mut Context<ReleaseView>,
) -> Vec<NoteRow> {
    notes
        .into_iter()
        .map(|(id, locale, text)| {
            let input = cx.new(|cx| InputField::new(window, cx, &locale));
            let editor = input.read(cx).editor().clone();
            editor.set_text(&text, window, cx);
            NoteRow {
                id,
                locale,
                original: text,
                input,
            }
        })
        .collect()
}

impl ReleaseView {
    fn open_ios_notes(&mut self, version_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.ios_notes = NotesEditor::Loading;
        cx.notify();
        let task = self
            .store
            .update(cx, |store, cx| store.fetch_version_notes(version_id, cx));
        self.notes_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.ios_notes = match result {
                    Ok(localizations) => NotesEditor::Open(note_rows(
                        localizations
                            .into_iter()
                            .map(|localization| {
                                (localization.id, localization.locale, localization.text)
                            })
                            .collect(),
                        window,
                        cx,
                    )),
                    Err(error) => NotesEditor::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open_android_notes(
        &mut self,
        timeline: &PlatformTimeline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut notes: Vec<(String, String, String)> = timeline
            .notes
            .iter()
            .map(|note| {
                (
                    note.language.clone(),
                    note.language.clone(),
                    note.text.clone(),
                )
            })
            .collect();
        if notes.is_empty() {
            notes.push(("en-US".into(), "en-US".into(), String::new()));
        }
        self.android_notes = NotesEditor::Open(note_rows(notes, window, cx));
        cx.notify();
    }

    fn save_ios_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let NotesEditor::Open(rows) = &self.ios_notes else {
            return;
        };
        let changed: Vec<(String, String, String)> = rows
            .iter()
            .filter_map(|row| {
                let text = row.input.read(cx).text(cx).trim().to_string();
                (text != row.original).then(|| (row.id.clone(), row.locale.clone(), text))
            })
            .collect();
        if changed.is_empty() {
            self.write_state = WriteState::Failed(t!("app_releases.launch.notes_unchanged"));
            cx.notify();
            return;
        }
        let locales = changed
            .iter()
            .map(|(_, locale, _)| locale.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.confirm_write(
            t!("app_releases.launch.notes_title", locales = locales),
            t!("app_releases.launch.notes_detail_ios"),
            StoreWrite::VersionNotes {
                notes: changed
                    .into_iter()
                    .map(|(id, _, text)| (id, text))
                    .collect(),
            },
            window,
            cx,
        );
    }

    fn save_android_notes(
        &mut self,
        track: String,
        version_code: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let NotesEditor::Open(rows) = &self.android_notes else {
            return;
        };
        let notes: Vec<LocalizedText> = rows
            .iter()
            .filter_map(|row| {
                let text = row.input.read(cx).text(cx).trim().to_string();
                (!text.is_empty()).then(|| LocalizedText {
                    language: row.locale.clone(),
                    text,
                })
            })
            .collect();
        let locales = notes
            .iter()
            .map(|note| note.language.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.confirm_write(
            t!("app_releases.launch.notes_title", locales = locales),
            t!("app_releases.write.commit_detail"),
            StoreWrite::Play(TrackChange::SetNotes {
                track,
                version_code,
                notes,
            }),
            window,
            cx,
        );
    }

    fn render_notes_editor(
        &self,
        editor: &NotesEditor,
        id: &'static str,
        on_save: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        match editor {
            NotesEditor::Closed => None,
            NotesEditor::Loading => Some(
                Label::new(t!("app_releases.launch.notes_loading"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            NotesEditor::Failed(message) => Some(
                error_callout(t!("app_releases.launch.notes_failed"), message.clone(), cx)
                    .into_any_element(),
            ),
            NotesEditor::Open(rows) => Some(
                v_flex()
                    .gap_2()
                    .children(rows.iter().map(|row| {
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new(row.locale.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(row.input.clone())
                    }))
                    .child(
                        h_flex().justify_end().child(
                            secondary_button(
                                id,
                                None,
                                t!("app_releases.launch.save_notes"),
                                true,
                                cx,
                            )
                            .on_click(
                                cx.listener(move |this, _, window, cx| on_save(this, window, cx)),
                            ),
                        ),
                    )
                    .into_any_element(),
            ),
        }
    }

    fn render_prepare_version(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.app_store_app_id(cx)?;
        let options = self.version_options(cx)?;
        // A version Apple never heard of (an Android-only or abandoned one) has nothing to prepare.
        let nothing_uploaded = options.version.is_none()
            && options.ready_builds.is_empty()
            && options.processing_builds == 0;
        if !options.can_prepare() || nothing_uploaded {
            return None;
        }
        let version = self.version.clone();
        let selected_build = self
            .prepare_build
            .as_ref()
            .and_then(|build_id| options.ready_builds.iter().find(|build| &build.id == build_id))
            .or_else(|| options.suggested_build())
            .cloned();
        let release_type = self
            .prepare_release_type
            .or_else(|| options.version.as_ref().and_then(|version| version.release_type))
            .unwrap_or_default();

        let version_status = match (&options.version, &options.editable_version) {
            (Some(_), _) => status_pill(t!("app_releases.prepare.open"), StatusTone::Info, cx),
            (None, Some(editable)) => status_pill(
                t!("app_releases.prepare.renames", version = editable.version.clone()),
                StatusTone::Warning,
                cx,
            ),
            (None, None) => {
                status_pill(t!("app_releases.prepare.creates"), StatusTone::Neutral, cx)
            }
        };

        let builds: Vec<AnyElement> = options
            .ready_builds
            .iter()
            .map(|build| {
                let build_id = build.id.clone();
                let selected = selected_build
                    .as_ref()
                    .is_some_and(|selected| selected.id == build.id);
                choice_chip(
                    SharedString::from(format!("prepare-build-{}", build.id)),
                    None,
                    build.build_number.clone(),
                    selected,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.prepare_build = Some(build_id.clone());
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        let no_builds = builds.is_empty().then(|| {
            let text = if options.processing_builds > 0 {
                t!(
                    "app_releases.prepare.builds_processing",
                    count = options.processing_builds
                )
            } else {
                t!("app_releases.prepare.no_builds", version = version.clone())
            };
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
        });

        let release_types = segmented_control(cx).children(
            [
                (ReleaseType::Manual, t!("app_releases.prepare.release_manual")),
                (
                    ReleaseType::AfterApproval,
                    t!("app_releases.prepare.release_after_approval"),
                ),
            ]
            .into_iter()
            .map(|(option, label)| {
                segment_option(
                    SharedString::from(format!("prepare-release-type-{option:?}")),
                    label,
                    option == release_type,
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.prepare_release_type = Some(option);
                    cx.notify();
                }))
            }),
        );

        let needs_encryption_answer = selected_build
            .as_ref()
            .is_some_and(|build| build.uses_non_exempt_encryption.is_none());
        let encryption = needs_encryption_answer.then(|| {
            v_flex()
                .gap_1()
                .child(
                    Label::new(t!("app_releases.prepare.encryption"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children([false, true].into_iter().map(|uses| {
                            choice_chip(
                                SharedString::from(format!("prepare-encryption-{uses}")),
                                None,
                                if uses {
                                    t!("app_releases.prepare.encryption_yes")
                                } else {
                                    t!("app_releases.prepare.encryption_exempt")
                                },
                                self.prepare_encryption == uses,
                                true,
                                cx,
                            )
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.prepare_encryption = uses;
                                cx.notify();
                            }))
                        })),
                )
        });

        let already_prepared = options.version.as_ref().is_some_and(|current| {
            current.build_id.as_ref() == selected_build.as_ref().map(|build| &build.id)
                && current.release_type == Some(release_type)
        }) && !needs_encryption_answer;
        let prepare_button = selected_build
            .filter(|_| !already_prepared)
            .map(|build| {
                let preparation = VersionPreparation {
                    editable_version_id: options
                        .version
                        .as_ref()
                        .or(options.editable_version.as_ref())
                        .map(|version| version.id.clone()),
                    version_string: version.clone(),
                    build_id: build.id.clone(),
                    release_type,
                    uses_non_exempt_encryption: needs_encryption_answer
                        .then_some(self.prepare_encryption),
                };
                let title = t!(
                    "app_releases.prepare.title",
                    version = version.clone(),
                    build = build.build_number
                );
                let detail = prepare_detail(&options, &preparation);
                primary_button(
                    "release-prepare-version",
                    Some(IconName::Check),
                    t!("app_releases.prepare.save"),
                    None,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_write(
                        title.clone(),
                        detail.clone(),
                        StoreWrite::PrepareVersion(preparation.clone()),
                        window,
                        cx,
                    )
                }))
            });

        let checklist = self.checklist(&options, cx).filter(|_| options.version.is_some());

        Some(
            v_flex()
                .gap_2()
                .pt_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(section_label(t!("app_releases.prepare.section")))
                        .child(version_status),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(t!("app_releases.prepare.build"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(h_flex().gap_2().flex_wrap().children(builds))
                        .children(no_builds),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(t!("app_releases.prepare.release_type"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(h_flex().child(release_types)),
                )
                .children(encryption)
                .children(prepare_button.map(|button| h_flex().child(button)))
                .children(checklist.map(|checklist| render_checklist(&checklist, cx)))
                .into_any_element(),
        )
    }

    fn render_app_store_launch(
        &self,
        timeline: &PlatformTimeline,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let version_id = timeline.ids.asc_version_id.clone()?;
        let state = timeline.ids.asc_version_state.clone().unwrap_or_default();
        let version = self.version.clone();
        let has_app_id = self.app_store_app_id(cx).is_some();
        let mut buttons: Vec<AnyElement> = Vec::new();

        let checklist_passed = self
            .version_options(cx)
            .and_then(|options| self.checklist(&options, cx))
            .is_none_or(|checklist| model::checklist_passed(&checklist));
        if has_app_id && checklist_passed && SUBMITTABLE_STATES.contains(&state.as_str()) {
            let version_id = version_id.clone();
            let title = t!("app_releases.launch.submit_title", version = version);
            buttons.push(
                primary_button(
                    "release-submit-review",
                    Some(IconName::Send),
                    t!("app_releases.launch.submit"),
                    None,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_write(
                        title.clone(),
                        t!("app_releases.launch.submit_detail"),
                        StoreWrite::SubmitForReview {
                            version_id: version_id.clone(),
                        },
                        window,
                        cx,
                    )
                }))
                .into_any_element(),
            );
        }

        if state == "PENDING_DEVELOPER_RELEASE" {
            let version_id = version_id.clone();
            let title = t!("app_releases.launch.release_title", version = version);
            buttons.push(
                primary_button(
                    "release-release-version",
                    None,
                    t!("app_releases.launch.release"),
                    None,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_write(
                        title.clone(),
                        t!("app_releases.launch.release_detail"),
                        StoreWrite::ReleaseVersion {
                            version_id: version_id.clone(),
                        },
                        window,
                        cx,
                    )
                }))
                .into_any_element(),
            );
        }

        let phased_button = |id: &'static str,
                             label: SharedString,
                             title: SharedString,
                             write: StoreWrite,
                             buttons: &mut Vec<AnyElement>| {
            buttons.push(
                secondary_button(id, None, label, true, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_write(
                            title.clone(),
                            t!("app_releases.launch.phased_detail"),
                            write.clone(),
                            window,
                            cx,
                        )
                    }))
                    .into_any_element(),
            );
        };
        match timeline.ids.asc_phased_release.clone() {
            None if state != "READY_FOR_SALE" && state != "READY_FOR_DISTRIBUTION" => {
                phased_button(
                    "release-phased-start",
                    t!("app_releases.launch.phased_start"),
                    t!("app_releases.launch.phased_start_title", version = version),
                    StoreWrite::StartPhasedRelease {
                        version_id: version_id.clone(),
                    },
                    &mut buttons,
                );
            }
            Some((phased_release_id, PhasedReleaseState::Active)) => {
                phased_button(
                    "release-phased-pause",
                    t!("app_releases.launch.phased_pause"),
                    t!("app_releases.launch.phased_pause_title", version = version),
                    StoreWrite::SetPhasedRelease {
                        phased_release_id: phased_release_id.clone(),
                        state: PhasedReleaseState::Paused,
                    },
                    &mut buttons,
                );
                phased_button(
                    "release-phased-complete",
                    t!("app_releases.launch.phased_complete"),
                    t!(
                        "app_releases.launch.phased_complete_title",
                        version = version
                    ),
                    StoreWrite::SetPhasedRelease {
                        phased_release_id,
                        state: PhasedReleaseState::Complete,
                    },
                    &mut buttons,
                );
            }
            Some((phased_release_id, PhasedReleaseState::Paused)) => {
                phased_button(
                    "release-phased-resume",
                    t!("app_releases.launch.phased_resume"),
                    t!("app_releases.launch.phased_resume_title", version = version),
                    StoreWrite::SetPhasedRelease {
                        phased_release_id,
                        state: PhasedReleaseState::Active,
                    },
                    &mut buttons,
                );
            }
            _ => {}
        }

        let notes_button = matches!(self.ios_notes, NotesEditor::Closed | NotesEditor::Failed(_))
            .then(|| {
                let version_id = version_id.clone();
                secondary_button(
                    "release-edit-ios-notes",
                    Some((IconName::Pencil, Color::Muted)),
                    t!("app_releases.launch.edit_notes"),
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_ios_notes(version_id.clone(), window, cx)
                }))
                .into_any_element()
            });

        Some(
            v_flex()
                .gap_2()
                .pt_2()
                .child(section_label(t!("app_releases.launch.app_store")))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(buttons)
                        .children(notes_button),
                )
                .children(self.render_notes_editor(
                    &self.ios_notes,
                    "release-save-ios-notes",
                    |this, window, cx| this.save_ios_notes(window, cx),
                    cx,
                ))
                .into_any_element(),
        )
    }

    fn render_play_notes(
        &self,
        timeline: &PlatformTimeline,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        self.package(cx)?;
        let version_code = timeline.build.clone()?;
        let track = timeline
            .tracks
            .iter()
            .find(|track| track.track == PRODUCTION_TRACK)
            .or_else(|| timeline.tracks.last())?
            .track
            .clone();
        let notes_button = matches!(self.android_notes, NotesEditor::Closed).then(|| {
            let timeline = timeline.clone();
            secondary_button(
                "release-edit-play-notes",
                Some((IconName::Pencil, Color::Muted)),
                t!("app_releases.launch.edit_notes"),
                true,
                cx,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_android_notes(&timeline, window, cx)
            }))
            .into_any_element()
        });
        Some(
            v_flex()
                .gap_2()
                .pt_2()
                .child(section_label(t!(
                    "app_releases.launch.play_notes",
                    track = track
                )))
                .children(notes_button)
                .children(self.render_notes_editor(
                    &self.android_notes,
                    "release-save-play-notes",
                    move |this, window, cx| {
                        this.save_android_notes(track.clone(), version_code.clone(), window, cx)
                    },
                    cx,
                ))
                .into_any_element(),
        )
    }
}

enum AfterLaunchState {
    Closed,
    Loading,
    Loaded(AfterLaunch),
}

fn format_rate(rate: Option<f64>) -> String {
    match rate {
        Some(rate) => format!("{:.2}%", rate * 100.0),
        None => "—".to_string(),
    }
}

impl ReleaseView {
    fn load_after_launch(&mut self, cx: &mut Context<Self>) {
        self.after_launch = AfterLaunchState::Loading;
        cx.notify();
        let folder = self.folder.clone();
        let task = self
            .store
            .update(cx, |store, cx| store.fetch_after_launch(&folder, cx));
        self.after_launch_task = Some(cx.spawn(async move |this, cx| {
            let after_launch = task.await;
            this.update(cx, |this, cx| {
                this.after_launch = AfterLaunchState::Loaded(after_launch);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn render_section_body<T>(
        &self,
        section: &Section<T>,
        render: impl Fn(&T) -> AnyElement,
        cx: &Context<Self>,
    ) -> AnyElement {
        match section {
            None => Label::new(t!("app_releases.after.no_account"))
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
                .into_any_element(),
            Some(Err(message)) => {
                error_callout(t!("app_releases.after.failed"), message.clone(), cx)
                    .into_any_element()
            }
            Some(Ok(entries)) if entries.is_empty() => Label::new(t!("app_releases.after.empty"))
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
                .into_any_element(),
            Some(Ok(entries)) => v_flex()
                .gap_1p5()
                .children(entries.iter().map(&render))
                .into_any_element(),
        }
    }

    fn render_review(review: &Review, cx: &App) -> AnyElement {
        let stars = "★".repeat(usize::from(review.rating.min(5)));
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(stars)
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                    .children(review.title.clone().map(|title| {
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                    }))
                    .children(review.version_code.clone().map(|code| {
                        Label::new(code)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                    })),
            )
            .child(Label::new(review.body.clone()).size(LabelSize::Small))
            .child(
                Label::new(
                    [
                        review.author.clone(),
                        review
                            .created_at
                            .map(|at| at.with_timezone(&Local).format("%d/%m/%Y").to_string()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
                )
                .size(LabelSize::XSmall)
                .color(Color::Placeholder),
            )
            .into_any_element()
    }

    fn render_after_launch(&self, release: &Release, cx: &Context<Self>) -> AnyElement {
        let header = h_flex()
            .gap_2()
            .child(
                Label::new(t!("app_releases.after.title"))
                    .size(LabelSize::Default)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(div().flex_1())
            .child(
                secondary_button(
                    "release-load-after-launch",
                    Some((IconName::RotateCw, Color::Muted)),
                    match self.after_launch {
                        AfterLaunchState::Closed => t!("app_releases.after.load"),
                        _ => t!("app_releases.dock.refresh"),
                    },
                    !matches!(self.after_launch, AfterLaunchState::Loading),
                    cx,
                )
                .on_click(cx.listener(|this, _, _window, cx| this.load_after_launch(cx))),
            );
        let body = match &self.after_launch {
            AfterLaunchState::Closed => Label::new(t!("app_releases.after.description"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            AfterLaunchState::Loading => Label::new(t!("app_releases.after.loading"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            AfterLaunchState::Loaded(after_launch) => {
                let version_code = release
                    .platform(Platform::Android)
                    .and_then(|timeline| timeline.build.clone());
                let play_reviews: Section<Review> =
                    after_launch.play_reviews.clone().map(|result| {
                        result.map(|reviews| {
                            reviews
                                .into_iter()
                                .filter(|review| {
                                    version_code.is_none()
                                        || review.version_code.is_none()
                                        || review.version_code == version_code
                                })
                                .collect()
                        })
                    });
                let current_code = version_code;
                v_flex()
                    .gap_3()
                    .child(section_label(t!("app_releases.after.play_vitals")))
                    .child(self.render_section_body(
                        &after_launch.play_vitals,
                        |vitals| {
                            let is_current = current_code.as_deref() == Some(&vitals.version_code);
                            h_flex()
                                .gap_3()
                                .child(
                                    Label::new(vitals.version_code.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .weight(if is_current {
                                            FontWeight::SEMIBOLD
                                        } else {
                                            FontWeight::NORMAL
                                        }),
                                )
                                .child(
                                    Label::new(t!(
                                        "app_releases.after.crash_rate",
                                        rate = format_rate(vitals.crash_rate)
                                    ))
                                    .size(LabelSize::Small),
                                )
                                .child(
                                    Label::new(t!(
                                        "app_releases.after.anr_rate",
                                        rate = format_rate(vitals.anr_rate)
                                    ))
                                    .size(LabelSize::Small),
                                )
                                .into_any_element()
                        },
                        cx,
                    ))
                    .child(section_label(t!("app_releases.after.play_reviews")))
                    .child(self.render_section_body(
                        &play_reviews,
                        |review| Self::render_review(review, cx),
                        cx,
                    ))
                    .child(section_label(t!("app_releases.after.testflight_feedback")))
                    .child(self.render_section_body(
                        &after_launch.testflight_feedback,
                        |feedback| {
                            let kind = match feedback.kind {
                                FeedbackKind::Crash => t!("app_releases.after.crash"),
                                FeedbackKind::Screenshot => t!("app_releases.after.screenshot"),
                            };
                            let device = [feedback.device.clone(), feedback.os_version.clone()]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>()
                                .join(" · ");
                            v_flex()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(status_pill(
                                            kind,
                                            match feedback.kind {
                                                FeedbackKind::Crash => StatusTone::Error,
                                                FeedbackKind::Screenshot => StatusTone::Info,
                                            },
                                            cx,
                                        ))
                                        .child(
                                            Label::new(device)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                )
                                .children(
                                    feedback
                                        .comment
                                        .clone()
                                        .map(|comment| Label::new(comment).size(LabelSize::Small)),
                                )
                                .into_any_element()
                        },
                        cx,
                    ))
                    .child(section_label(t!("app_releases.after.app_store_reviews")))
                    .child(self.render_section_body(
                        &after_launch.app_store_reviews,
                        |review| Self::render_review(review, cx),
                        cx,
                    ))
                    .into_any_element()
            }
        };
        card(cx)
            .p_3()
            .gap_3()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

impl Focusable for ReleaseView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ReleaseView {}

impl WorkspaceItem for ReleaseView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.release(cx)
            .map(Release::title)
            .unwrap_or_else(|| self.version.clone())
            .into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Smartphone))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

type OnConfirm = Box<dyn FnOnce(&mut Window, &mut App)>;

/// Asks before anything is written to a store, naming the service, the action and the target.
pub struct ConfirmWrite {
    focus_handle: FocusHandle,
    service: SharedString,
    title: SharedString,
    detail: SharedString,
    on_confirm: Option<OnConfirm>,
}

impl ConfirmWrite {
    pub fn new(
        service: impl Into<SharedString>,
        title: SharedString,
        detail: SharedString,
        on_confirm: impl FnOnce(&mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            service: service.into(),
            title,
            detail,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ConfirmWrite {}

impl ModalView for ConfirmWrite {}

impl Focusable for ConfirmWrite {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConfirmWrite {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        modal_surface(cx)
            .key_context("ConfirmStoreWrite")
            .track_focus(&self.focus_handle)
            .w(px(440.))
            .child(
                v_flex()
                    .p_6()
                    .gap_2()
                    .child(
                        Label::new(t!("app_releases.confirm.writes_to", service = self.service))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(self.title.clone())
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(self.detail.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                modal_footer(cx)
                    .child(
                        div().flex_1().child(
                            Label::new(t!("app_releases.confirm.hint"))
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        ),
                    )
                    .child(
                        secondary_button(
                            "confirm-write-cancel",
                            None,
                            t!("app_releases.confirm.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.cancel(cx))),
                    )
                    .child(
                        primary_button(
                            "confirm-write-confirm",
                            None,
                            t!("app_releases.confirm.confirm"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}
