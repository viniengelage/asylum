use std::path::PathBuf;
use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Task, WeakEntity,
};
use i18n::t;
use picker::{Picker, PickerDelegate};
use ui::{
    HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*, segment_option,
    segmented_control,
};
use util::ResultExt as _;
use workspace::{ModalView, OpenOptions, OpenVisible, Workspace};

use crate::store::{
    NewNote, Note, NoteKind, NoteScope, NotesStore, first_free_slot, title_from_text,
};
use crate::{
    CopyNote, CreateNote, DeleteNote, EditNote, InsertText, TogglePin, relative_time,
    worktree_roots,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotesFilter {
    #[default]
    All,
    Pinned,
    Drafts,
}

impl NotesFilter {
    fn keeps(self, note: &Note) -> bool {
        match self {
            Self::All => true,
            Self::Pinned => note.pin.is_some(),
            Self::Drafts => note.kind == NoteKind::Draft,
        }
    }
}

pub(crate) fn open(
    workspace: &mut Workspace,
    filter: Option<NotesFilter>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = window.focused(cx);
    let roots = worktree_roots(workspace, cx);
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        NotesPalette::new(
            workspace_handle,
            roots,
            target,
            filter.unwrap_or_default(),
            window,
            cx,
        )
    });
}

/// Pastes `text` into the field that had the focus before a notes surface took it.
pub(crate) fn paste_into(
    target: Option<FocusHandle>,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    match target {
        Some(target) => {
            window.focus(&target, cx);
            window.dispatch_action(Box::new(InsertText { text }), cx);
        }
        None => cx.write_to_clipboard(ClipboardItem::new_string(text)),
    }
}

pub struct NotesPalette {
    picker: Entity<Picker<NotesDelegate>>,
    _store_subscription: Subscription,
}

impl NotesPalette {
    fn new(
        workspace: WeakEntity<Workspace>,
        roots: Vec<PathBuf>,
        target: Option<FocusHandle>,
        filter: NotesFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = NotesStore::global(cx);
        let delegate = NotesDelegate {
            palette: cx.weak_entity(),
            workspace,
            store: store.clone(),
            notes: store.read(cx).visible_notes(&roots),
            roots,
            target,
            filter,
            rows: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        let store_subscription = cx.observe_in(&store, window, |this, store, window, cx| {
            this.picker.update(cx, |picker, cx| {
                picker.delegate.notes = store.read(cx).visible_notes(&picker.delegate.roots);
                picker.refresh(window, cx);
            });
        });
        Self {
            picker,
            _store_subscription: store_subscription,
        }
    }

    fn selected_note(&self, cx: &App) -> Option<Note> {
        self.picker.read(cx).delegate.selected_note().cloned()
    }

    fn copy(&mut self, _: &CopyNote, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.selected_note(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(note.body));
            cx.emit(DismissEvent);
        }
    }

    fn edit(&mut self, _: &EditNote, window: &mut Window, cx: &mut Context<Self>) {
        let Some(note) = self.selected_note(cx) else {
            return;
        };
        let workspace = self.picker.read(cx).delegate.workspace.clone();
        cx.emit(DismissEvent);
        workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(
                        note.path,
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn toggle_pin(&mut self, _: &TogglePin, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(note) = self.selected_note(cx) else {
            return;
        };
        let pin = match note.pin {
            Some(_) => None,
            None => {
                let notes = &self.picker.read(cx).delegate.notes;
                let Some(slot) = first_free_slot(notes) else {
                    return;
                };
                Some(slot)
            }
        };
        NotesStore::global(cx)
            .update(cx, |store, cx| store.set_pin(&note, pin, cx))
            .detach_and_log_err(cx);
    }

    fn delete(&mut self, _: &DeleteNote, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.selected_note(cx) {
            NotesStore::global(cx)
                .update(cx, |store, cx| store.delete(&note, cx))
                .detach_and_log_err(cx);
        }
    }

    fn new_note(&mut self, _: &CreateNote, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.picker.read(cx).query(cx);
        let workspace = self.picker.read(cx).delegate.workspace.clone();
        cx.emit(DismissEvent);
        workspace
            .update(cx, |workspace, cx| {
                crate::quick_note::open(workspace, query, None, window, cx);
            })
            .log_err();
    }

    fn render_preview(&self, cx: &App) -> AnyElement {
        let Some(note) = self.picker.read(cx).delegate.selected_note() else {
            return div().into_any_element();
        };
        let mut meta = Vec::new();
        if let Some(slot) = note.pin {
            meta.push(t!("notes.palette.pinned_at", slot = slot).to_string());
        }
        meta.push(match &note.scope {
            NoteScope::Global => t!("notes.scope.all_projects").to_string(),
            NoteScope::Project(_) => t!("notes.scope.this_project").to_string(),
        });
        meta.push(relative_time(note.modified).to_string());
        let destination = if note.kind == NoteKind::Draft {
            t!("notes.palette.resume_hint")
        } else {
            t!("notes.palette.paste_hint")
        };
        let lines: Vec<String> = note.body.lines().take(14).map(str::to_string).collect();

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2p5()
            .p_4()
            .child(
                Label::new(note.title.clone())
                    .weight(gpui::FontWeight::BOLD)
                    .single_line()
                    .truncate(),
            )
            .child(
                Label::new(meta.join(" · "))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(note.invalid, |this| {
                this.child(
                    Label::new(t!("notes.palette.invalid"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .p_2p5()
                    .gap_0p5()
                    .rounded(px(8.))
                    .bg(cx.theme().colors().editor_background)
                    .overflow_hidden()
                    .children(lines.into_iter().map(|line| {
                        Label::new(line)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .single_line()
                            .truncate()
                    })),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(destination)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &App) -> impl IntoElement {
        let focus = self.picker.focus_handle(cx);
        let hint = |action: &dyn Action, label: SharedString| {
            h_flex()
                .gap_1p5()
                .child(KeyBinding::for_action_in(action, &focus, cx).size(rems_from_px(11_f32)))
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        };
        h_flex()
            .w_full()
            .h(px(36.))
            .gap_3p5()
            .px_4()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(hint(&menu::Confirm, t!("notes.palette.paste")))
            .child(hint(&CopyNote, t!("notes.palette.copy")))
            .child(hint(&EditNote, t!("notes.palette.edit")))
            .child(hint(&TogglePin, t!("notes.palette.pin")))
            .child(hint(&DeleteNote, t!("notes.palette.delete")))
            .child(hint(&CreateNote, t!("notes.palette.new")))
    }
}

impl EventEmitter<DismissEvent> for NotesPalette {}

impl ModalView for NotesPalette {}

impl Focusable for NotesPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for NotesPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ui::modal_surface(cx)
            .key_context("NotesPalette")
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::edit))
            .on_action(cx.listener(Self::toggle_pin))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::new_note))
            .w(px(680.))
            .child(
                h_flex()
                    .w_full()
                    .h(px(360.))
                    .items_start()
                    .child(
                        div()
                            .w(px(320.))
                            .h_full()
                            .flex_none()
                            .border_r_1()
                            .border_color(cx.theme().colors().border)
                            .child(self.picker.clone()),
                    )
                    .child(self.render_preview(cx)),
            )
            .child(self.render_footer(cx))
    }
}

enum Row {
    Header(SharedString),
    Item { note: usize, positions: Vec<usize> },
}

pub struct NotesDelegate {
    palette: WeakEntity<NotesPalette>,
    workspace: WeakEntity<Workspace>,
    store: Entity<NotesStore>,
    notes: Vec<Note>,
    roots: Vec<PathBuf>,
    target: Option<FocusHandle>,
    filter: NotesFilter,
    rows: Vec<Row>,
    selected_index: usize,
}

impl NotesDelegate {
    fn selected_note(&self) -> Option<&Note> {
        match self.rows.get(self.selected_index)? {
            Row::Item { note, .. } => self.notes.get(*note),
            Row::Header(_) => None,
        }
    }

    fn set_matches(&mut self, matches: Vec<StringMatch>) {
        let mut pinned = Vec::new();
        let mut recent = Vec::new();
        let mut drafts = Vec::new();
        for string_match in matches {
            let Some(note) = self.notes.get(string_match.candidate_id) else {
                continue;
            };
            let title_len = note.title.len();
            let row = Row::Item {
                note: string_match.candidate_id,
                positions: string_match
                    .positions
                    .into_iter()
                    .filter(|position| *position < title_len)
                    .collect(),
            };
            if note.pin.is_some() {
                pinned.push(row);
            } else if note.kind == NoteKind::Draft {
                drafts.push(row);
            } else {
                recent.push(row);
            }
        }
        self.rows.clear();
        for (label, group) in [
            (t!("notes.section.pinned"), pinned),
            (t!("notes.section.recent"), recent),
            (t!("notes.section.drafts"), drafts),
        ] {
            if !group.is_empty() {
                self.rows.push(Row::Header(label));
                self.rows.extend(group);
            }
        }
        self.selected_index = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Item { .. }))
            .unwrap_or(0);
    }

    fn render_filters(&self, cx: &mut Context<Picker<Self>>) -> AnyElement {
        let option = |id: &'static str, label: SharedString, filter: NotesFilter| {
            segment_option(id, label, self.filter == filter, true, cx).on_click(cx.listener(
                move |picker, _, window, cx| {
                    picker.delegate.filter = filter;
                    picker.refresh(window, cx);
                },
            ))
        };
        segmented_control(cx)
            .w(px(220.))
            .child(option(
                "notes-filter-all",
                t!("notes.filter.all"),
                NotesFilter::All,
            ))
            .child(option(
                "notes-filter-pinned",
                t!("notes.filter.pinned"),
                NotesFilter::Pinned,
            ))
            .child(option(
                "notes-filter-drafts",
                t!("notes.filter.drafts"),
                NotesFilter::Drafts,
            ))
            .into_any_element()
    }
}

impl PickerDelegate for NotesDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "notes palette"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        t!("notes.palette.placeholder").to_string().into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(t!("notes.palette.no_matches"))
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        self.palette.update(cx, |_, cx| cx.notify()).ok();
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        matches!(self.rows.get(ix), Some(Row::Item { .. }))
    }

    fn searchbar_trailer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(self.render_filters(cx))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<StringMatchCandidate> = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| self.filter.keeps(note))
            .map(|(ix, note)| {
                let body: String = note.body.chars().take(400).collect();
                StringMatchCandidate::new(
                    ix,
                    &format!("{} {} {}", note.title, note.tags.join(" "), body),
                )
            })
            .collect();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.trim().is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        score: 0.,
                        positions: Vec::new(),
                        string: candidate.string,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    200,
                    &Default::default(),
                    executor,
                )
                .await
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.set_matches(matches);
                    picker.delegate.palette.update(cx, |_, cx| cx.notify()).ok();
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(note) = self.selected_note().cloned() else {
            let query = cx.entity().read(cx).query(cx);
            if query.trim().is_empty() {
                return;
            }
            let new_note = NewNote {
                title: title_from_text(&query),
                body: query,
                kind: NoteKind::Note,
                scope: NoteScope::Global,
                pin: None,
                thread: None,
            };
            self.store
                .update(cx, |store, cx| store.create(new_note, cx))
                .detach_and_log_err(cx);
            self.dismissed(window, cx);
            return;
        };
        let target = self.target.clone();
        if note.kind == NoteKind::Draft {
            self.store
                .update(cx, |store, cx| store.delete(&note, cx))
                .detach_and_log_err(cx);
        }
        self.dismissed(window, cx);
        paste_into(target, note.body, window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.palette.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.rows.get(ix)? {
            Row::Header(label) => Some(
                h_flex()
                    .w_full()
                    .pt_2()
                    .pb_0p5()
                    .px_3()
                    .child(
                        Label::new(label.to_uppercase())
                            .size(LabelSize::XSmall)
                            .weight(gpui::FontWeight::BOLD)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            Row::Item { note, positions } => {
                let note = self.notes.get(*note)?;
                let icon = match (note.kind, note.invalid) {
                    (_, true) => IconName::Warning,
                    (NoteKind::Draft, _) => IconName::QueueMessage,
                    (NoteKind::Note, _) => IconName::Notepad,
                };
                let end = match note.pin {
                    Some(slot) => Label::new(format!("⌥⌘{slot}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Accent)
                        .buffer_font(cx),
                    None => Label::new(relative_time(note.modified))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                };
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(
                            HighlightedLabel::new(note.title.clone(), positions.clone())
                                .single_line()
                                .truncate(),
                        )
                        .end_slot(end)
                        .into_any_element(),
                )
            }
        }
    }
}
