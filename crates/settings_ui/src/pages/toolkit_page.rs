use std::sync::Arc;

use gpui::{ReadGlobal as _, ScrollHandle, prelude::*};
use settings::{SettingsStore, ToolkitDisplay};
use ui::{
    grouped_block, prelude::*, section_header, section_label, segment_option, segmented_control,
    toggle_switch,
};
use util::ResultExt as _;
use workspace::{TOOLKITS, ToolkitDomain};

use crate::{SettingsUiFile, SettingsWindow, update_settings_file};

pub(crate) fn render_toolkit_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let file = settings_window.current_file.clone();
    let store = SettingsStore::global(cx);
    let (_, display) = store.get_value_from_file(file.to_settings(), |content| {
        content.toolkit.as_ref()?.display
    });
    let display = display.unwrap_or_default();
    let (_, hidden) = store.get_value_from_file(file.to_settings(), |content| {
        content.toolkit.as_ref()?.hidden.as_ref()
    });
    let hidden: Vec<Arc<str>> = hidden.cloned().unwrap_or_default();

    let display_option = |id: &'static str, label: SharedString, value: ToolkitDisplay| {
        let file = file.clone();
        segment_option(id, label, display == value, true, cx).on_click(move |_event, window, cx| {
            update_settings_file(
                file.clone(),
                Some("toolkit.display"),
                window,
                cx,
                move |settings, _cx| {
                    settings.toolkit.get_or_insert_default().display = Some(value);
                },
            )
            .log_err();
        })
    };

    v_flex()
        .id("toolkit-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_6()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            v_flex()
                .gap_2()
                .child(section_label(i18n::t!(
                    "workspace.toolkit.settings.display"
                )))
                .child(
                    segmented_control(cx)
                        .child(display_option(
                            "toolkit-display-menu",
                            i18n::t!("workspace.toolkit.settings.display_menu"),
                            ToolkitDisplay::Menu,
                        ))
                        .child(display_option(
                            "toolkit-display-inline",
                            i18n::t!("workspace.toolkit.settings.display_inline"),
                            ToolkitDisplay::Inline,
                        )),
                ),
        )
        .children(
            ToolkitDomain::ALL
                .into_iter()
                .map(|domain| render_domain(domain, &file, &hidden, cx)),
        )
        .into_any_element()
}

fn render_domain(
    domain: ToolkitDomain,
    file: &SettingsUiFile,
    hidden: &[Arc<str>],
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_2()
        .child(section_header(domain.label(), None))
        .child(
            grouped_block(cx).children(TOOLKITS.iter().filter(|info| info.domain == domain).map(
                |info| {
                    let visible = !hidden.iter().any(|id| &**id == info.id);
                    let file = file.clone();
                    let id = info.id;
                    let mut new_hidden: Vec<Arc<str>> = hidden
                        .iter()
                        .filter(|hidden_id| &***hidden_id != id)
                        .cloned()
                        .collect();
                    if visible {
                        new_hidden.push(id.into());
                    }
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .child(
                            Icon::new(info.icon)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().child(Label::new((info.label)())))
                        .child(
                            toggle_switch(
                                SharedString::from(format!("toolkit-visible-{id}")),
                                visible,
                                true,
                                cx,
                            )
                            .on_click(move |_event, window, cx| {
                                let new_hidden = new_hidden.clone();
                                update_settings_file(
                                    file.clone(),
                                    Some("toolkit.hidden"),
                                    window,
                                    cx,
                                    move |settings, _cx| {
                                        settings.toolkit.get_or_insert_default().hidden =
                                            Some(new_hidden);
                                    },
                                )
                                .log_err();
                            }),
                        )
                },
            )),
        )
}
