use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    Window,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    ToggleFavorite, record_pill,
    sources::{RunCandidate, RunOrigin},
    store::{RunStore, refresh_candidates},
};

enum Entry {
    Header(SharedString),
    Candidate {
        key: SharedString,
        positions: Vec<usize>,
    },
}

pub struct RunPickerDelegate {
    store: Entity<RunStore>,
    entries: Vec<Entry>,
    selected_index: usize,
}

impl RunPickerDelegate {
    fn selected_key(&self) -> Option<SharedString> {
        match self.entries.get(self.selected_index)? {
            Entry::Candidate { key, .. } => Some(key.clone()),
            Entry::Header(_) => None,
        }
    }
}

/// The run picker: the project's runs in sections (favorites, the project's, debugging),
/// opened from the status bar or with cmd-shift-r.
pub struct RunPickerView {
    picker: Entity<Picker<RunPickerDelegate>>,
    store: Entity<RunStore>,
    _subscriptions: Vec<Subscription>,
}

impl RunPickerView {
    pub fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = RunStore::for_workspace(workspace, cx);
        let delegate = RunPickerDelegate {
            store: store.clone(),
            entries: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .popover()
                .initial_width(rems(34.))
                .max_height(rems(28.))
        });
        refresh_candidates(workspace, window, cx).detach();
        let subscriptions = vec![
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.picker
                    .update(cx, |picker, cx| picker.refresh(window, cx));
            }),
        ];
        Self {
            picker,
            store,
            _subscriptions: subscriptions,
        }
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.picker.read(cx).delegate.selected_key() else {
            return;
        };
        self.store
            .update(cx, |store, cx| store.toggle_favorite(key, cx));
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
    }
}

impl EventEmitter<DismissEvent> for RunPickerView {}

impl Focusable for RunPickerView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RunPickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RunPicker")
            .on_action(cx.listener(Self::toggle_favorite))
            .child(self.picker.clone())
    }
}

impl PickerDelegate for RunPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "run picker"
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        matches!(self.entries.get(ix), Some(Entry::Candidate { .. }))
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from(i18n::t!("runs.picker.placeholder").to_string())
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(i18n::t!("runs.picker.empty"))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let store = self.store.read(cx);
        let candidates: Vec<RunCandidate> = store.candidates().to_vec();
        let favorites: Vec<SharedString> = store.favorites().to_vec();
        let match_candidates: Vec<StringMatchCandidate> = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| StringMatchCandidate::new(index, &candidate.label))
            .collect();
        cx.spawn_in(window, async move |picker, cx| {
            let matches: Vec<StringMatch> = if query.is_empty() {
                (0..candidates.len())
                    .map(|index| StringMatch {
                        candidate_id: index,
                        score: 0.0,
                        positions: Vec::new(),
                        string: String::new(),
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &match_candidates,
                    &query,
                    true,
                    true,
                    200,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            let entries = group_entries(&candidates, &favorites, matches);
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.entries = entries;
                    delegate.selected_index = delegate
                        .entries
                        .iter()
                        .position(|entry| matches!(entry, Entry::Candidate { .. }))
                        .unwrap_or(0);
                })
                .log_err();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(key) = self.selected_key() else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.run_key(key, secondary, window, cx);
        });
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(ix)? {
            Entry::Header(title) => Some(
                h_flex()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        Label::new(title.clone())
                            .size(LabelSize::XSmall)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            Entry::Candidate { key, positions } => {
                let store = self.store.read(cx);
                let candidate = store.candidate(key)?;
                let slot = store.favorite_slot(key);
                let last = store
                    .last_result(&candidate.label)
                    .map(|record| record_pill(record, cx));
                let icon = if candidate.is_debug() {
                    IconName::Debug
                } else {
                    IconName::PlayOutlined
                };
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            HighlightedLabel::new(
                                                candidate.label.clone(),
                                                positions.clone(),
                                            )
                                            .size(LabelSize::Small)
                                            .truncate(),
                                        )
                                        .child(
                                            Label::new(candidate.detail.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .buffer_font(cx)
                                                .single_line()
                                                .truncate(),
                                        ),
                                )
                                .children(last)
                                .child(origin_chip(candidate.origin, cx))
                                .when_some(slot, |this, slot| {
                                    this.child(
                                        Label::new(format!("⌃⌥{slot}"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                }),
                        )
                        .into_any_element(),
                )
            }
        }
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let hint = |keys: &str, label: SharedString| {
            h_flex()
                .gap_1()
                .child(
                    Label::new(keys.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_3()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(hint("↵", i18n::t!("runs.picker.hint_run")))
                .child(hint("⌘↵", i18n::t!("runs.picker.hint_debug")))
                .child(hint("⌘S", i18n::t!("runs.picker.hint_favorite")))
                .into_any_element(),
        )
    }
}

fn origin_chip(origin: RunOrigin, cx: &App) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .rounded(px(4.))
        .border_1()
        .border_color(colors.border_variant)
        .child(
            Label::new(origin.chip_label())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

fn group_entries(
    candidates: &[RunCandidate],
    favorites: &[SharedString],
    matches: Vec<StringMatch>,
) -> Vec<Entry> {
    let mut favorite_entries: Vec<(usize, Entry)> = Vec::new();
    let mut project_entries = Vec::new();
    let mut debug_entries = Vec::new();
    for string_match in matches {
        let Some(candidate) = candidates.get(string_match.candidate_id) else {
            continue;
        };
        let entry = Entry::Candidate {
            key: candidate.key.clone(),
            positions: string_match.positions,
        };
        if let Some(slot) = favorites.iter().position(|key| *key == candidate.key) {
            favorite_entries.push((slot, entry));
        } else if candidate.is_debug() {
            debug_entries.push(entry);
        } else {
            project_entries.push(entry);
        }
    }
    favorite_entries.sort_by_key(|(slot, _)| *slot);
    let mut entries = Vec::new();
    if !favorite_entries.is_empty() {
        entries.push(Entry::Header(i18n::t!("runs.picker.section_favorites")));
        entries.extend(favorite_entries.into_iter().map(|(_, entry)| entry));
    }
    if !project_entries.is_empty() {
        entries.push(Entry::Header(i18n::t!("runs.picker.section_project")));
        entries.extend(project_entries);
    }
    if !debug_entries.is_empty() {
        entries.push(Entry::Header(i18n::t!("runs.picker.section_debug")));
        entries.extend(debug_entries);
    }
    entries
}
