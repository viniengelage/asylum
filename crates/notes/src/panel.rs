use std::path::PathBuf;

use editor::{Editor, EditorEvent};
use gpui::{
    Action as _, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    WeakEntity,
};
use i18n::t;
use ui::EmptyState;
use ui::{
    KeyBinding, Tooltip, prelude::*, primary_button, secondary_button, segment_option,
    segmented_control, text_field,
};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::palette::{NotesFilter, paste_into};
use crate::store::{Note, NoteKind, NoteScope, NotesStore, first_free_slot};
use crate::{InsertText, OpenPalette, Save, ToggleFocus, project_roots, relative_time};

pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    restore_panel(workspace, window, cx);
    workspace.focus_panel::<NotesPanel>(window, cx);
}

pub(crate) fn restore_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.panel::<NotesPanel>(cx).is_none() {
        let workspace_handle = cx.weak_entity();
        let panel = cx.new(|cx| NotesPanel::new(workspace_handle, window, cx));
        workspace.add_panel(panel, window, cx);
    }
}

pub(crate) fn toggle_focus(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.panel::<NotesPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<NotesPanel>(window, cx);
}

pub struct NotesPanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<NotesStore>,
    search: Entity<Editor>,
    filter: NotesFilter,
    project_only: bool,
    selected: Option<PathBuf>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl NotesPanel {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = NotesStore::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("notes.panel.search_placeholder"), window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        Self {
            workspace,
            store,
            search,
            filter: NotesFilter::All,
            project_only: false,
            selected: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn roots(&self, cx: &mut App) -> Vec<PathBuf> {
        self.workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone())
            .map(|project| project_roots(&project, cx))
            .unwrap_or_default()
    }

    fn visible(&self, roots: &[PathBuf], cx: &App) -> Vec<Note> {
        let query = self.search.read(cx).text(cx).to_lowercase();
        self.store
            .read(cx)
            .visible_notes(roots)
            .into_iter()
            .filter(|note| !self.project_only || matches!(note.scope, NoteScope::Project(_)))
            .filter(|note| match self.filter {
                NotesFilter::All => true,
                NotesFilter::Pinned => note.pin.is_some(),
                NotesFilter::Drafts => note.kind == NoteKind::Draft,
            })
            .filter(|note| {
                query.is_empty()
                    || note.title.to_lowercase().contains(&query)
                    || note.body.to_lowercase().contains(&query)
                    || note
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&query))
            })
            .collect()
    }

    /// Pastes into the active item of the center, the place a note is usually headed.
    fn paste(&self, note: &Note, window: &mut Window, cx: &mut App) {
        let target = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .map(|item| item.item_focus_handle(cx))
        });
        paste_into(target, note.body.clone(), window, cx);
    }

    fn resume(&self, note: &Note, window: &mut Window, cx: &mut App) {
        let text = note.body.clone();
        self.store
            .update(cx, |store, cx| store.delete(note, cx))
            .detach_and_log_err(cx);
        window.dispatch_action(Box::new(zed_actions::assistant::FocusAgent), cx);
        // The composer takes the focus while FocusAgent runs; the text has to go there after.
        window.defer(cx, move |window, cx| {
            window.dispatch_action(Box::new(InsertText { text }), cx);
        });
    }

    fn edit(&self, note: &Note, window: &mut Window, cx: &mut App) {
        let path = note.path.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(
                        path,
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

    fn toggle_pin(&self, note: &Note, notes: &[Note], cx: &mut App) {
        let pin = match note.pin {
            Some(_) => None,
            None => match first_free_slot(notes) {
                Some(slot) => Some(slot),
                None => return,
            },
        };
        self.store
            .update(cx, |store, cx| store.set_pin(note, pin, cx))
            .detach_and_log_err(cx);
    }

    fn render_note(&self, note: &Note, all_notes: &[Note], cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let selected = self.selected.as_ref() == Some(&note.path);
        let path = note.path.clone();
        let preview = note
            .body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_string();

        let leading: AnyElement = match note.pin {
            Some(slot) => h_flex()
                .flex_none()
                .h(px(18.))
                .px(px(5.))
                .rounded(px(4.))
                .bg(if selected {
                    colors.element_selected
                } else {
                    colors.elevated_surface_background
                })
                .child(
                    Label::new(format!("⌥⌘{slot}"))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(if selected {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                )
                .into_any_element(),
            None => Icon::new(match (note.kind, note.invalid) {
                (_, true) => IconName::Warning,
                (NoteKind::Draft, _) => IconName::QueueMessage,
                (NoteKind::Note, _) => IconName::Notepad,
            })
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        };

        let trailing: AnyElement = if note.kind == NoteKind::Draft {
            secondary_button(
                SharedString::from(format!("notes-resume-{}", note.path.display())),
                Some((IconName::ReplyArrowRight, Color::Muted)),
                t!("notes.panel.resume"),
                true,
                cx,
            )
            .on_click({
                let note = note.clone();
                cx.listener(move |this, _, window, cx| this.resume(&note, window, cx))
            })
            .into_any_element()
        } else {
            Label::new(relative_time(note.modified))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element()
        };

        let header = h_flex()
            .w_full()
            .gap_2()
            .child(leading)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Label::new(note.title.clone())
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::MEDIUM)
                                    .single_line()
                                    .truncate(),
                            )
                            .when(matches!(note.scope, NoteScope::Project(_)), |this| {
                                this.child(
                                    Label::new(t!("notes.scope.project_chip"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                            }),
                    )
                    .when(!selected, |this| {
                        this.child(
                            Label::new(match note.kind {
                                NoteKind::Draft => relative_time(note.modified),
                                NoteKind::Note => preview.clone().into(),
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                        )
                    }),
            )
            .child(trailing);

        let body_lines: Vec<String> = note.body.lines().take(6).map(str::to_string).collect();
        let details = selected.then(|| {
            let note = note.clone();
            let notes = all_notes.to_vec();
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    v_flex()
                        .w_full()
                        .p_2()
                        .gap_0p5()
                        .rounded(px(6.))
                        .bg(colors.editor_background)
                        .children(body_lines.into_iter().map(|line| {
                            Label::new(line)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .single_line()
                                .truncate()
                        })),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .child(
                            primary_button(
                                "notes-paste",
                                None,
                                t!("notes.palette.paste"),
                                None,
                                cx,
                            )
                            .on_click({
                                let note = note.clone();
                                cx.listener(move |this, _, window, cx| {
                                    this.paste(&note, window, cx)
                                })
                            }),
                        )
                        .child(
                            secondary_button(
                                "notes-copy",
                                None,
                                t!("notes.palette.copy"),
                                true,
                                cx,
                            )
                            .on_click({
                                let body = note.body.clone();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))
                                }
                            }),
                        )
                        .child(
                            secondary_button(
                                "notes-edit",
                                None,
                                t!("notes.palette.edit"),
                                true,
                                cx,
                            )
                            .on_click({
                                let note = note.clone();
                                cx.listener(move |this, _, window, cx| this.edit(&note, window, cx))
                            }),
                        )
                        .child(div().flex_1())
                        .child(
                            IconButton::new(
                                "notes-pin",
                                if note.pin.is_some() {
                                    IconName::Unpin
                                } else {
                                    IconName::Pin
                                },
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if note.pin.is_some() {
                                t!("notes.panel.unpin")
                            } else {
                                t!("notes.palette.pin")
                            }))
                            .on_click({
                                let note = note.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_pin(&note, &notes, cx)
                                })
                            }),
                        )
                        .child(
                            IconButton::new("notes-delete", IconName::Trash)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(t!("notes.palette.delete")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .update(cx, |store, cx| store.delete(&note, cx))
                                        .detach_and_log_err(cx);
                                })),
                        ),
                )
        });

        v_flex()
            .id(SharedString::from(format!("note-{}", note.path.display())))
            .w_full()
            .gap_2()
            .py(px(7.))
            .px_2()
            .rounded(px(8.))
            .cursor_pointer()
            .when(selected, |this| this.bg(colors.element_selected))
            .when(!selected, |this| {
                this.hover(|this| this.bg(colors.element_hover))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(path.clone());
                cx.notify();
            }))
            .child(header)
            .children(details)
            .into_any_element()
    }

    fn render_section(
        &self,
        label: SharedString,
        trailer: Option<SharedString>,
        notes: Vec<Note>,
        all_notes: &[Note],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if notes.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap_0p5()
                .child(
                    h_flex()
                        .w_full()
                        .pt_1()
                        .pb_1()
                        .px_2()
                        .child(
                            Label::new(label.to_uppercase())
                                .size(LabelSize::XSmall)
                                .weight(gpui::FontWeight::BOLD)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .children(trailer.map(|trailer| {
                            Label::new(trailer)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                        })),
                )
                .children(
                    notes
                        .iter()
                        .map(|note| self.render_note(note, all_notes, cx)),
                )
                .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let hint = |key: &'static str, text: SharedString| {
            h_flex()
                .gap_2()
                .child(
                    h_flex()
                        .w(px(56.))
                        .h(px(20.))
                        .flex_none()
                        .justify_center()
                        .rounded(Radius::Xs.rems())
                        .bg(cx.theme().colors().element_background)
                        .child(Label::new(key).size(LabelSize::XSmall).buffer_font(cx)),
                )
                .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        };
        EmptyState::new(IconName::Notepad, t!("notes.empty.title"))
            .description(t!("notes.empty.body"))
            .action(
                v_flex()
                    .gap_1p5()
                    .child(hint("⌥⌘N", t!("notes.empty.hint_save")))
                    .child(hint("⌥⌘V", t!("notes.empty.hint_palette")))
                    .child(hint("⌥⌘1–9", t!("notes.empty.hint_pinned"))),
            )
            .into_any_element()
    }
}

impl Render for NotesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = self.roots(cx);
        let all_notes = self.store.read(cx).visible_notes(&roots);
        let notes = self.visible(&roots, cx);
        let count = |filter: NotesFilter| {
            all_notes
                .iter()
                .filter(|note| match filter {
                    NotesFilter::All => true,
                    NotesFilter::Pinned => note.pin.is_some(),
                    NotesFilter::Drafts => note.kind == NoteKind::Draft,
                })
                .count()
        };
        let (pinned, rest): (Vec<Note>, Vec<Note>) =
            notes.into_iter().partition(|note| note.pin.is_some());
        let (drafts, recent): (Vec<Note>, Vec<Note>) = rest
            .into_iter()
            .partition(|note| note.kind == NoteKind::Draft);
        let search_focused = self.search.focus_handle(cx).is_focused(window);
        let draft_count = count(NotesFilter::Drafts);
        let note_count = all_notes.len() - draft_count;
        let sections: Vec<AnyElement> = [
            self.render_section(
                t!("notes.section.pinned"),
                Some(t!("notes.panel.pinned_hint")),
                pinned,
                &all_notes,
                cx,
            ),
            self.render_section(t!("notes.section.parked"), None, drafts, &all_notes, cx),
            self.render_section(t!("notes.section.recent"), None, recent, &all_notes, cx),
        ]
        .into_iter()
        .flatten()
        .collect();

        let empty = all_notes.is_empty().then(|| self.render_empty(cx));
        let filter_option = |id: &'static str, label: SharedString, filter: NotesFilter| {
            segment_option(
                id,
                format!("{label} {}", count(filter)),
                self.filter == filter,
                true,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.filter = filter;
                cx.notify();
            }))
        };

        v_flex()
            .key_context("NotesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .children(empty)
            .when(!all_notes.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_2p5()
                        .pt_2p5()
                        .px_2()
                        .child(
                            text_field(search_focused, cx)
                                .gap_2()
                                .child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1().child(self.search.clone()))
                                .child(
                                    KeyBinding::for_action_in(&OpenPalette, &self.focus_handle, cx)
                                        .size(rems_from_px(11_f32)),
                                ),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    segmented_control(cx)
                                        .flex_1()
                                        .child(filter_option(
                                            "notes-panel-all",
                                            t!("notes.filter.all"),
                                            NotesFilter::All,
                                        ))
                                        .child(filter_option(
                                            "notes-panel-pinned",
                                            t!("notes.filter.pinned"),
                                            NotesFilter::Pinned,
                                        ))
                                        .child(filter_option(
                                            "notes-panel-drafts",
                                            t!("notes.filter.drafts"),
                                            NotesFilter::Drafts,
                                        )),
                                )
                                .child(
                                    secondary_button(
                                        "notes-panel-scope",
                                        Some((IconName::Folder, Color::Muted)),
                                        if self.project_only {
                                            t!("notes.scope.this_project")
                                        } else {
                                            t!("notes.scope.all")
                                        },
                                        !roots.is_empty(),
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.project_only = !this.project_only;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                )
                .child(
                    v_flex()
                        .id("notes-panel-list")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_y_scroll()
                        .gap_2p5()
                        .pt_2p5()
                        .px_2()
                        .children(sections),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .h(px(44.))
                    .flex_none()
                    .gap_2()
                    .px_3()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(t!(
                            "notes.panel.count",
                            notes = note_count,
                            drafts = draft_count
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        primary_button(
                            "notes-panel-new",
                            Some(IconName::Plus),
                            t!("notes.panel.new"),
                            Some(KeyBinding::for_action_in(&Save, &self.focus_handle, cx)),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.workspace
                                .update(cx, |workspace, cx| {
                                    crate::quick_note::open(
                                        workspace,
                                        String::new(),
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        })),
                    ),
            )
    }
}

impl Focusable for NotesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for NotesPanel {}

impl Panel for NotesPanel {
    fn persistent_name() -> &'static str {
        "NotesPanel"
    }

    fn panel_key() -> &'static str {
        "NotesPanel"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(384.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Notepad)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some(match i18n::current_language() {
            i18n::Language::English => "Notes",
            i18n::Language::PortugueseBrazil => "Notas",
        })
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}
