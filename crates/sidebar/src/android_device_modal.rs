use android_sdk::AndroidDeviceProfile;
use editor::Editor;
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
};
use ui::{Divider, Headline, HeadlineSize, Label, LabelSize, ListItem, WithScrollbar, prelude::*};
use workspace::ModalView;

/// Picks the device definition a new Android virtual device is created from.
///
/// `avdmanager` offers around sixty handheld definitions, so the list is
/// filtered as the user types rather than shown in a dropdown.
pub struct AndroidDeviceModal {
    profiles: Vec<AndroidDeviceProfile>,
    matches: Vec<usize>,
    selected_index: usize,
    query_editor: Entity<Editor>,
    scroll_handle: ScrollHandle,
    on_confirm: Box<dyn Fn(AndroidDeviceProfile, &mut Window, &mut App) + 'static>,
}

impl EventEmitter<DismissEvent> for AndroidDeviceModal {}
impl ModalView for AndroidDeviceModal {}

impl Focusable for AndroidDeviceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }
}

impl AndroidDeviceModal {
    pub fn new(
        profiles: Vec<AndroidDeviceProfile>,
        on_confirm: impl Fn(AndroidDeviceProfile, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pixel, tablet, fold…", window, cx);
            editor
        });
        cx.subscribe(&query_editor, |this, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                this.refresh_matches(cx);
            }
        })
        .detach();

        Self {
            matches: (0..profiles.len()).collect(),
            profiles,
            selected_index: 0,
            query_editor,
            scroll_handle: ScrollHandle::new(),
            on_confirm: Box::new(on_confirm),
        }
    }

    fn refresh_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query_editor.read(cx).text(cx).to_lowercase();
        self.matches = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile_matches(profile, &query))
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
        self.scroll_handle.scroll_to_item(0);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.matches.len();
        self.scroll_handle.scroll_to_item(self.selected_index);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.matches.is_empty() {
            return;
        }
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.matches.len() - 1);
        self.scroll_handle.scroll_to_item(self.selected_index);
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_index(self.selected_index, window, cx);
    }

    fn confirm_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .matches
            .get(index)
            .and_then(|profile_index| self.profiles.get(*profile_index))
            .cloned()
        else {
            return;
        };
        (self.on_confirm)(profile, window, cx);
        cx.emit(DismissEvent);
    }
}

impl Render for AndroidDeviceModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_index = self.selected_index;
        let matches: Vec<_> = self
            .matches
            .iter()
            .filter_map(|index| self.profiles.get(*index))
            .cloned()
            .collect();

        v_flex()
            .key_context("AndroidDeviceModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(30.))
            .overflow_hidden()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Screen)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Headline::new("Adicionar dispositivo Android")
                                    .size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(self.query_editor.clone()),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .id("android-device-profiles")
                    .p_1()
                    .gap_0p5()
                    .max_h_96()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .when(matches.is_empty(), |this| {
                        this.child(
                            div().p_2().child(
                                Label::new("Nenhum dispositivo corresponde à busca.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .children(matches.into_iter().enumerate().map(|(index, profile)| {
                        ListItem::new(SharedString::from(profile.id.clone()))
                            .rounded()
                            .toggle_state(index == selected_index)
                            .child(Label::new(profile.name))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_index(index, window, cx);
                            }))
                    })),
            )
            .child(Divider::horizontal())
            .child(
                h_flex().w_full().p_2().child(
                    Label::new("Enter para criar, Esc para cancelar.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}

/// Matches every whitespace-separated term against the definition's name and
/// id, so "pixel fold" finds "Pixel 9 Pro Fold".
fn profile_matches(profile: &AndroidDeviceProfile, query: &str) -> bool {
    let name = profile.name.to_lowercase();
    let id = profile.id.to_lowercase();
    query
        .split_whitespace()
        .all(|term| name.contains(term) || id.contains(term))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, name: &str) -> AndroidDeviceProfile {
        AndroidDeviceProfile {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn matches_every_term_against_name_or_id() {
        let fold = profile("pixel_9_pro_fold", "Pixel 9 Pro Fold");

        assert!(profile_matches(&fold, ""));
        assert!(profile_matches(&fold, "pixel fold"));
        assert!(profile_matches(&fold, "9 pro"));
        // Terms may come from the id even when the name spells things out.
        assert!(profile_matches(&fold, "pixel_9"));
        assert!(!profile_matches(&fold, "pixel tablet"));
    }
}
