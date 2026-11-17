use std::rc::Rc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task};
use i18n::t;
use ui::{KeyBinding, modal_footer, modal_surface, prelude::*, primary_button, text_field};
use util::ResultExt as _;
use workspace::ModalView;

use crate::SaveQuickNote;
use crate::store::{NoteScope, NotesStore};

const SAVE_DELAY: Duration = Duration::from_millis(500);

type RenameThread = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;
type SummarizeThread = Rc<dyn Fn(&mut App) -> Task<anyhow::Result<String>>>;

/// "Edit thread": the thread's title and a markdown context the user writes for themselves.
/// The context is a note tied to the thread, so it also shows up in the Notes dock and in `@note`.
pub struct ThreadContextEditor {
    thread_key: String,
    scope: NoteScope,
    subtitle: SharedString,
    title: Entity<Editor>,
    body: Entity<Editor>,
    initial_title: String,
    rename_thread: RenameThread,
    summarize_thread: Option<SummarizeThread>,
    summarizing: Option<Task<()>>,
    summarize_error: Option<SharedString>,
    saved: bool,
    save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ThreadContextEditor {
    pub fn new(
        thread_key: String,
        scope: NoteScope,
        title: SharedString,
        subtitle: SharedString,
        rename_thread: impl Fn(SharedString, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let existing_body = NotesStore::global(cx)
            .read(cx)
            .note_for_thread(&thread_key)
            .map(|note| note.body.clone())
            .unwrap_or_default();
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(title.to_string(), window, cx);
            editor
        });
        let body = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 12, window, cx);
            editor.set_placeholder_text(&t!("notes.thread_context.placeholder"), window, cx);
            editor.set_text(existing_body, window, cx);
            editor
        });
        let subscriptions = vec![cx.subscribe(&body, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.schedule_save(cx);
            }
        })];
        window.focus(&body.focus_handle(cx), cx);
        Self {
            thread_key,
            scope,
            subtitle,
            title: title_editor,
            body,
            initial_title: title.to_string(),
            rename_thread: Rc::new(rename_thread),
            summarize_thread: None,
            summarizing: None,
            summarize_error: None,
            saved: false,
            save_task: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Offers "Summarize conversation". The summary lands in the context field for the user to
    /// edit; it is appended when the field already has text.
    pub fn summarize_with(
        mut self,
        summarize: impl Fn(&mut App) -> Task<anyhow::Result<String>> + 'static,
    ) -> Self {
        self.summarize_thread = Some(Rc::new(summarize));
        self
    }

    fn summarize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summarize) = self.summarize_thread.clone() else {
            return;
        };
        if self.summarizing.is_some() {
            return;
        }
        self.summarize_error = None;
        let summary = summarize(cx);
        self.summarizing = Some(cx.spawn_in(window, async move |this, cx| {
            let summary = summary.await;
            this.update_in(cx, |this, window, cx| {
                this.summarizing = None;
                match summary {
                    Ok(summary) => {
                        let existing = this.body.read(cx).text(cx);
                        let text = if existing.trim().is_empty() {
                            summary.trim().to_string()
                        } else {
                            format!("{}\n\n{}", existing.trim_end(), summary.trim())
                        };
                        this.body.update(cx, |editor, cx| {
                            editor.set_text(text, window, cx);
                            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
                        });
                    }
                    Err(error) => {
                        this.summarize_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.saved = false;
        cx.notify();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_now(cx)).log_err();
        }));
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        let body = self.body.read(cx).text(cx);
        let title = self.title.read(cx).text(cx);
        let save = NotesStore::global(cx).update(cx, |store, cx| {
            store.save_thread_context(self.thread_key.clone(), title, body, self.scope.clone(), cx)
        });
        cx.spawn(async move |this, cx| {
            let saved = save.await.log_err().is_some();
            this.update(cx, |this, cx| {
                this.saved = saved;
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.save_task.is_some() {
            self.save_now(cx);
        }
        let title = self.title.read(cx).text(cx);
        if !title.trim().is_empty() && title != self.initial_title {
            (self.rename_thread)(SharedString::from(title), window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn done(&mut self, _: &SaveQuickNote, window: &mut Window, cx: &mut Context<Self>) {
        self.finish(window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.finish(window, cx);
    }
}

impl EventEmitter<DismissEvent> for ThreadContextEditor {}

impl ModalView for ThreadContextEditor {}

impl Focusable for ThreadContextEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.body.focus_handle(cx)
    }
}

impl Render for ThreadContextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title_focused = self.title.focus_handle(cx).is_focused(window);
        let body_focused = self.body.focus_handle(cx).is_focused(window);
        let status = if self.saved {
            t!("notes.thread_context.saved")
        } else {
            t!("notes.thread_context.markdown")
        };

        modal_surface(cx)
            .key_context("NotesQuickNote")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::done))
            .on_action(cx.listener(Self::cancel))
            .w(px(420.))
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
                        Label::new(t!("notes.thread_context.heading"))
                            .weight(gpui::FontWeight::MEDIUM)
                            .size(LabelSize::Default),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(self.subtitle.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_2p5()
                    .px_3()
                    .pb_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new(t!("notes.thread_context.title"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(text_field(title_focused, cx).child(self.title.clone())),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .child(
                                        Label::new(t!("notes.thread_context.context"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Label::new(status)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
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
                                Label::new(t!("notes.thread_context.hint"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                modal_footer(cx)
                    .when(self.summarize_thread.is_some(), |this| {
                        let summarizing = self.summarizing.is_some();
                        this.child(
                            Button::new(
                                "thread-context-summarize",
                                if summarizing {
                                    t!("notes.thread_context.summarizing")
                                } else {
                                    t!("notes.thread_context.summarize")
                                },
                            )
                            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::XSmall))
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .disabled(summarizing)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.summarize(window, cx)),
                            ),
                        )
                    })
                    .when_some(self.summarize_error.clone(), |this, error| {
                        this.child(
                            div().min_w_0().flex_shrink(1.).child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error)
                                    .truncate(),
                            ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        primary_button(
                            "thread-context-done",
                            None,
                            t!("notes.thread_context.done"),
                            Some(KeyBinding::for_action_in(
                                &SaveQuickNote,
                                &self.focus_handle,
                                cx,
                            )),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.finish(window, cx))),
                    ),
            )
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.finish(window, cx)))
    }
}
