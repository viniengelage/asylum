//! A version opened from the dock, as an editor tab: the iOS and Android timelines side by side,
//! TestFlight groups, Play tracks and release notes.

use crate::{
    app_store_connect, google_play,
    model::{Platform, PlatformTimeline, Release, StepState},
    state_tone, status_label, step_title,
    store::ReleasesStore,
};
use chrono::Local;
use gpui::{Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, px};
use i18n::t;
use std::{path::Path, sync::Arc};
use ui::{
    CommonAnimationExt, EmptyState, HeaderBar, card, error_callout, prelude::*, primary_button,
    secondary_button, section_label, status_pill,
};
use workspace::{
    Workspace,
    item::{Item as WorkspaceItem, ItemEvent},
};

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
    let view = cx.new(|cx| ReleaseView::new(store, folder, version, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

pub struct ReleaseView {
    focus_handle: FocusHandle,
    pub(crate) store: Entity<ReleasesStore>,
    pub(crate) folder: Arc<Path>,
    pub(crate) version: String,
    _subscriptions: Vec<Subscription>,
}

impl ReleaseView {
    fn new(
        store: Entity<ReleasesStore>,
        folder: Arc<Path>,
        version: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            focus_handle: cx.focus_handle(),
            store,
            folder,
            version,
            _subscriptions: subscriptions,
        }
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
