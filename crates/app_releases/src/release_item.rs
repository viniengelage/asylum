//! A version opened from the dock, as an editor tab: the iOS and Android timelines side by side,
//! TestFlight groups, Play tracks and release notes.

use crate::{
    app_store_connect,
    google_play::{self, PRODUCTION_TRACK, TrackChange},
    model::{Platform, PlatformTimeline, Release, StepKind, StepState},
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
    StatusTone, card, error_callout, modal_footer, modal_surface, prelude::*, primary_button,
    secondary_button, section_label, status_pill,
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
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
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
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
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
            _subscriptions: subscriptions,
        }
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
                    .child(status_pill(label, tone, cx)),
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
            .children(match timeline.platform {
                Platform::Ios => self.render_ios_actions(timeline, cx),
                Platform::Android => self.render_android_actions(timeline, cx),
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
