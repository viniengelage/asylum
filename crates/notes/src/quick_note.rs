use std::path::PathBuf;

use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use i18n::t;
use ui::{
    KeyBinding, modal_footer, modal_surface, prelude::*, primary_button, secondary_button,
    segment_option, segmented_control, text_field, toggle_switch,
};
use workspace::{ModalView, Workspace};

use crate::store::{NewNote, NoteKind, NoteScope, NotesStore, first_free_slot, title_from_text};
use crate::{SaveQuickNote, worktree_roots};

pub(crate) fn open(
    workspace: &mut Workspace,
    text: String,
    source: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let roots = worktree_roots(workspace, cx);
    let next_slot = first_free_slot(&NotesStore::global(cx).read(cx).visible_notes(&roots));
    let project_root = roots.into_iter().next();
    workspace.toggle_modal(window, cx, move |window, cx| {
        QuickNote::new(text, source, project_root, next_slot, window, cx)
    });
}

pub struct QuickNote {
    title: Entity<Editor>,
    body: Entity<Editor>,
    source: Option<SharedString>,
    line_count: usize,
    project_root: Option<PathBuf>,
    in_project: bool,
    pin: bool,
    next_slot: Option<u8>,
    focus_handle: FocusHandle,
}

impl QuickNote {
    fn new(
        text: String,
        source: Option<SharedString>,
        project_root: Option<PathBuf>,
        next_slot: Option<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let line_count = text.lines().count();
        let title = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("notes.quick.title_placeholder"), window, cx);
            editor.set_text(title_from_text(&text), window, cx);
            editor
        });
        let body = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 10, window, cx);
            editor.set_placeholder_text(&t!("notes.quick.body_placeholder"), window, cx);
            editor.set_text(text, window, cx);
            editor
        });
        // A snippet of the project's code belongs to the project; anything else to the user.
        let in_project = source.is_some() && project_root.is_some();
        let focus = if line_count == 0 {
            body.focus_handle(cx)
        } else {
            title.focus_handle(cx)
        };
        window.focus(&focus, cx);
        Self {
            title,
            body,
            source,
            line_count,
            project_root,
            in_project,
            pin: false,
            next_slot,
            focus_handle: cx.focus_handle(),
        }
    }

    fn save(&mut self, _: &SaveQuickNote, _window: &mut Window, cx: &mut Context<Self>) {
        let body = self.body.read(cx).text(cx);
        let title = self.title.read(cx).text(cx);
        if body.trim().is_empty() && title.trim().is_empty() {
            return;
        }
        let scope = match (&self.project_root, self.in_project) {
            (Some(root), true) => NoteScope::Project(root.clone()),
            _ => NoteScope::Global,
        };
        let new_note = NewNote {
            title,
            body,
            kind: NoteKind::Note,
            scope,
            pin: self.pin.then_some(self.next_slot).flatten(),
            thread: None,
        };
        NotesStore::global(cx)
            .update(cx, |store, cx| store.create(new_note, cx))
            .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for QuickNote {}

impl ModalView for QuickNote {}

impl Focusable for QuickNote {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for QuickNote {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_focused = self.title.focus_handle(cx).is_focused(window);
        let body_focused = self.body.focus_handle(cx).is_focused(window);
        let source = match (&self.source, self.line_count) {
            (Some(source), count) if count > 0 => {
                Some(t!("notes.quick.source", count = count, file = source))
            }
            _ => None,
        };
        let pin_label = match self.next_slot {
            Some(slot) => t!("notes.quick.pin", slot = slot),
            None => t!("notes.quick.pin_full"),
        };

        modal_surface(cx)
            .key_context("NotesQuickNote")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::cancel))
            .w(px(440.))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .pt_3()
                    .pb_2()
                    .px_3()
                    .child(
                        Icon::new(IconName::Notepad)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(t!("notes.quick.heading"))
                            .weight(gpui::FontWeight::MEDIUM)
                            .size(LabelSize::Default),
                    )
                    .child(div().flex_1())
                    .children(source.map(|source| {
                        Label::new(source)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                    })),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_2p5()
                    .px_3()
                    .pb_3()
                    .child(text_field(title_focused, cx).child(self.title.clone()))
                    .child(
                        div()
                            .w_full()
                            .p_2()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(if body_focused {
                                cx.theme().colors().border_focused
                            } else {
                                cx.theme().colors().border
                            })
                            .bg(cx.theme().colors().editor_background)
                            .child(self.body.clone()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2p5()
                            .child(
                                segmented_control(cx)
                                    .w(px(240.))
                                    .child(
                                        segment_option(
                                            "quick-note-global",
                                            t!("notes.scope.all_projects"),
                                            !self.in_project,
                                            true,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.in_project = false;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        segment_option(
                                            "quick-note-project",
                                            t!("notes.scope.this_project"),
                                            self.in_project,
                                            self.project_root.is_some(),
                                            cx,
                                        )
                                        .when(
                                            self.project_root.is_some(),
                                            |this| {
                                                this.on_click(cx.listener(|this, _, _, cx| {
                                                    this.in_project = true;
                                                    cx.notify();
                                                }))
                                            },
                                        ),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                toggle_switch(
                                    "quick-note-pin",
                                    self.pin,
                                    self.next_slot.is_some(),
                                    cx,
                                )
                                .when(
                                    self.next_slot.is_some(),
                                    |this| {
                                        this.on_click(cx.listener(|this, _, _, cx| {
                                            this.pin = !this.pin;
                                            cx.notify();
                                        }))
                                    },
                                ),
                            )
                            .child(Label::new(pin_label).size(LabelSize::Small).color(
                                if self.next_slot.is_some() {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )),
                    ),
            )
            .child(
                modal_footer(cx)
                    .child(
                        Label::new(t!("notes.quick.hint"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        secondary_button(
                            "quick-note-cancel",
                            None,
                            t!("notes.quick.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        primary_button(
                            "quick-note-save",
                            None,
                            t!("notes.quick.save"),
                            Some(KeyBinding::for_action_in(
                                &SaveQuickNote,
                                &self.focus_handle,
                                cx,
                            )),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save(&SaveQuickNote, window, cx);
                        })),
                    ),
            )
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
    }
}
