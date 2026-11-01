use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Window,
};
use i18n::t;
use ui::{Divider, prelude::*};
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
        self.sync.update(cx, |sync, cx| sync.cancel_sign_in(cx));
        cx.emit(DismissEvent);
    }

    fn render_code(
        &self,
        user_code: SharedString,
        verification_uri: SharedString,
        cx: &App,
    ) -> AnyElement {
        v_flex()
            .gap_3()
            .child(Label::new(t!("settings_sync.sign_in.enter_code")).color(Color::Muted))
            .child(
                h_flex().justify_center().py_2().child(
                    Label::new(user_code.clone())
                        .size(LabelSize::Large)
                        .buffer_font(cx)
                        .weight(gpui::FontWeight::BOLD),
                ),
            )
            .child(
                Button::new(
                    "settings-sync-copy-and-open",
                    t!("settings_sync.sign_in.copy_and_open"),
                )
                .full_width()
                .style(ButtonStyle::Filled)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(user_code.to_string()));
                    cx.open_url(&verification_uri);
                }),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("settings_sync.sign_in.waiting"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }
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
        let sync = self.sync.read(cx);
        let body = match sync.state() {
            State::AwaitingAuthorization { code } => self.render_code(
                code.user_code.clone().into(),
                code.verification_uri.clone().into(),
                cx,
            ),
            State::SignedOut { error: Some(error) } => v_flex()
                .gap_3()
                .child(Label::new(error.clone()).color(Color::Error))
                .child(
                    Button::new("settings-sync-retry", t!("settings_sync.sign_in.try_again"))
                        .full_width()
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sync.update(cx, |sync, cx| sync.sign_in(cx));
                        })),
                )
                .into_any_element(),
            State::SignedIn { .. } | State::SignedOut { error: None } | State::Restoring => {
                Label::new(t!("settings_sync.sign_in.contacting"))
                    .color(Color::Muted)
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("SettingsSyncSignIn")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(26.))
            .p_4()
            .gap_3()
            .child(
                h_flex().gap_2().child(Icon::new(IconName::Github)).child(
                    Headline::new(t!("settings_sync.sign_in.title")).size(HeadlineSize::Small),
                ),
            )
            .child(
                Label::new(t!("settings_sync.sign_in.description"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Divider::horizontal())
            .child(body)
    }
}
