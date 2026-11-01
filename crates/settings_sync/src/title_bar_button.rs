use gpui::{Action, AnyElement, App};
use i18n::t;
use ui::{Avatar, ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};

use crate::{SettingsSync, SignIn, SignOut, State, SyncNow, SyncStatus};

/// The title bar's GitHub button: "Sign In" while signed out, the user's avatar with a sync menu
/// once signed in.
pub fn render_title_bar_button(cx: &App) -> Option<AnyElement> {
    let sync = SettingsSync::global(cx)?;
    let sync = sync.read(cx);
    match sync.state() {
        State::Restoring => None,
        State::SignedOut { .. } | State::AwaitingAuthorization { .. } => Some(
            Button::new(
                "settings-sync-sign-in",
                t!("settings_sync.title_bar.sign_in"),
            )
            .label_size(LabelSize::Small)
            .start_icon(
                Icon::new(IconName::Github)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text(t!("settings_sync.title_bar.sign_in_tooltip")))
            .on_click(|_, window, cx| window.dispatch_action(SignIn.boxed_clone(), cx))
            .into_any_element(),
        ),
        State::SignedIn { user, sync, .. } => {
            let login = user.login.clone();
            let status = status_label(sync);
            let failed = matches!(sync, SyncStatus::Failed(_));
            Some(
                PopoverMenu::new("settings-sync-menu")
                    .trigger_with_tooltip(
                        ButtonLike::new("settings-sync-avatar").child(
                            Avatar::new(user.avatar_url.clone())
                                .size(rems_from_px(18_f32))
                                .when(failed, |avatar| {
                                    avatar.indicator(ui::Indicator::dot().color(Color::Error))
                                }),
                        ),
                        Tooltip::text(t!("settings_sync.title_bar.signed_in_as", login = login)),
                    )
                    .anchor(gpui::Anchor::TopRight)
                    .menu(move |window, cx| {
                        let login = login.clone();
                        let status = status.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.header(format!("@{login}"))
                                .item(ui::ContextMenuEntry::new(status).disabled(true))
                                .separator()
                                .action(t!("settings_sync.menu.sync_now"), SyncNow.boxed_clone())
                                .action(t!("settings_sync.menu.sign_out"), SignOut.boxed_clone())
                        }))
                    })
                    .into_any_element(),
            )
        }
    }
}

fn status_label(sync: &SyncStatus) -> SharedString {
    match sync {
        SyncStatus::NeverSynced => t!("settings_sync.status.never_synced"),
        SyncStatus::Syncing => t!("settings_sync.status.syncing"),
        SyncStatus::Synced { at, report } => {
            let time = at.format("%H:%M");
            match report.backups {
                0 => t!("settings_sync.status.synced", time = time),
                1 => t!("settings_sync.status.synced_with_backup.one", time = time),
                count => t!(
                    "settings_sync.status.synced_with_backup.other",
                    time = time,
                    count = count
                ),
            }
        }
        SyncStatus::Failed(error) => t!("settings_sync.status.failed", error = error),
    }
}
