use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Subscription, Window, px,
};
use i18n::t;
use ui::{
    CommonAnimationExt, error_callout, modal_footer, modal_surface, prelude::*, primary_button,
    rems_from_px, secondary_button,
};
use workspace::ModalView;

use crate::{SettingsSync, State};

pub struct SignInModal {
    sync: Entity<SettingsSync>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl SignInModal {
    pub fn new(sync: Entity<SettingsSync>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_in(&sync, window, |_, sync, _, cx| {
            if matches!(sync.read(cx).state(), State::SignedIn { .. }) {
                cx.emit(DismissEvent);
            }
            cx.notify();
        });
        Self {
            sync,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_sign_in(cx);
    }

    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.sync.update(cx, |sync, cx| sync.cancel_sign_in(cx));
        cx.emit(DismissEvent);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_start()
            .gap_3()
            .p(px(20.))
            .child(
                div()
                    .size(px(36.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(Radius::Lg.rems())
                    .bg(cx.theme().colors().ghost_element_hover)
                    .child(Icon::new(IconName::Github)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        Label::new(t!("settings_sync.sign_in.title")).weight(FontWeight::BOLD),
                    )
                    .child(
                        Label::new(t!("settings_sync.sign_in.description"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("settings-sync-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx))),
            )
    }

    fn render_code(
        &self,
        user_code: SharedString,
        verification_uri: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let footer_background = colors.surface_background;
        v_flex()
            .child(
                v_flex()
                    .items_center()
                    .gap_4()
                    .p(px(20.))
                    .child(
                        Label::new(t!("settings_sync.sign_in.steps"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(render_user_code(&user_code, cx))
                    .child(
                        primary_button(
                            "settings-sync-copy-and-open",
                            Some(IconName::Copy),
                            t!("settings_sync.sign_in.copy_and_open"),
                            None,
                            cx,
                        )
                        .w_full()
                        .h(px(32.))
                        .justify_center()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(user_code.to_string()));
                            cx.open_url(&verification_uri);
                        }),
                    ),
            )
            .child(
                modal_footer(cx)
                    .h_auto()
                    .px(px(20.))
                    .py_3()
                    .bg(footer_background)
                    .child(spinner())
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(t!("settings_sync.sign_in.waiting"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(
                        Button::new("settings-sync-cancel", t!("settings_sync.sign_in.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_error(&self, error: SharedString, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .p(px(20.))
            .child(error_callout(
                t!("settings_sync.sign_in.error_title"),
                error,
                cx,
            ))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        secondary_button(
                            "settings-sync-cancel-error",
                            None,
                            t!("settings_sync.sign_in.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx))),
                    )
                    .child(
                        primary_button(
                            "settings-sync-new-code",
                            Some(IconName::RotateCw),
                            t!("settings_sync.sign_in.new_code"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sync.update(cx, |sync, cx| sync.sign_in(cx));
                        })),
                    ),
            )
            .into_any_element()
    }
}

fn spinner() -> impl IntoElement {
    Icon::new(IconName::LoadCircle)
        .size(IconSize::Small)
        .color(Color::Muted)
        .with_rotate_animation(2)
}

fn render_user_code(user_code: &str, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    h_flex()
        .gap(px(6.))
        .children(user_code.chars().map(|character| {
            let label = Label::new(SharedString::from(character.to_string()))
                .size(LabelSize::Custom(rems_from_px(20_f32)))
                .buffer_font(cx)
                .weight(FontWeight::BOLD);
            if character == '-' {
                label.color(Color::Muted).into_any_element()
            } else {
                div()
                    .w(px(34.))
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(Radius::Md.rems())
                    .border_1()
                    .border_color(colors.border_selected)
                    .bg(colors.element_background)
                    .child(label)
                    .into_any_element()
            }
        }))
}

impl EventEmitter<DismissEvent> for SignInModal {}

impl ModalView for SignInModal {}

impl Focusable for SignInModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SignInModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.sync.read(cx).state() {
            State::AwaitingAuthorization { code } => {
                let user_code = code.user_code.clone().into();
                let verification_uri = code.verification_uri.clone().into();
                self.render_code(user_code, verification_uri, cx)
            }
            State::SignedOut { error: Some(error) } => {
                let error = error.clone();
                self.render_error(error, cx)
            }
            State::SignedIn { .. } | State::SignedOut { error: None } | State::Restoring => {
                h_flex()
                    .justify_center()
                    .gap_2()
                    .p(px(20.))
                    .child(spinner())
                    .child(Label::new(t!("settings_sync.sign_in.contacting")).color(Color::Muted))
                    .into_any_element()
            }
        };

        modal_surface(cx)
            .key_context("SettingsSyncSignIn")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(27.5))
            .child(self.render_header(cx))
            .child(body)
    }
}
