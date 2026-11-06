mod palette;
mod panel;
mod quick_note;
mod store;

use std::path::PathBuf;
use std::time::SystemTime;

use editor::Editor;
use gpui::{Action, App, Entity, SharedString, Window, actions};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

pub use palette::NotesPalette;
pub use panel::NotesPanel;
pub use quick_note::QuickNote;
pub use store::{
    NewNote, Note, NoteKind, NoteScope, NotesStore, PIN_SLOTS, global_notes_dir, title_from_text,
};
pub use zed_actions::notes::InsertText;

actions!(
    notes,
    [
        /// Searches the notes and pastes one where the cursor was.
        OpenPalette,
        /// Saves the selection as a note. In the agent's composer, parks the prompt as a draft.
        Save,
        /// Shows or hides the Notes panel.
        ToggleFocus,
        /// Copies the selected note to the clipboard.
        CopyNote,
        /// Opens the selected note's file in the editor.
        EditNote,
        /// Pins the selected note to the first free slot, or unpins it.
        TogglePin,
        /// Deletes the selected note.
        DeleteNote,
        /// Creates a note from what was typed in the search.
        CreateNote,
        /// Saves the quick note.
        SaveQuickNote,
    ]
);

/// Pastes the note pinned to a slot where the cursor is.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = notes)]
#[serde(deny_unknown_fields)]
pub struct PastePinned {
    pub slot: u8,
}

pub fn init(cx: &mut App) {
    workspace::register_lazy_panel::<NotesPanel>(panel::restore_panel, cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenPalette, window, cx| {
            palette::open(workspace, None, window, cx);
        });
        workspace.register_action(|workspace, _: &Save, window, cx| {
            quick_note::open(workspace, String::new(), None, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            panel::toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &PastePinned, window, cx| {
            let roots = worktree_roots(workspace, cx);
            let note = NotesStore::global(cx).read(cx).pinned(action.slot, &roots);
            if let Some(note) = note {
                window.dispatch_action(Box::new(InsertText { text: note.body }), cx);
            }
        });
    })
    .detach();

    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() {
            return;
        }
        let handle = cx.weak_entity();
        editor
            .register_action({
                let handle = handle.clone();
                move |action: &InsertText, window, cx| {
                    handle
                        .update(cx, |editor, cx| editor.insert(&action.text, window, cx))
                        .ok();
                }
            })
            .detach();
        // The agent's composer isn't a full editor, so Save reaches it and parks the prompt.
        if !editor.mode().is_full() {
            return;
        }
        editor
            .register_action(move |_: &Save, window, cx| {
                let Some((workspace, text, source)) = handle
                    .update(cx, |editor, cx| {
                        let workspace = editor.workspace()?;
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let selection = editor.selections.newest_anchor();
                        let text: String = snapshot
                            .text_for_range(selection.start..selection.end)
                            .collect();
                        let source = editor
                            .active_buffer(cx)
                            .and_then(|buffer| {
                                buffer
                                    .read(cx)
                                    .file()
                                    .map(|file| file.file_name(cx).to_string())
                            })
                            .map(SharedString::from);
                        Some((workspace, text, source))
                    })
                    .ok()
                    .flatten()
                else {
                    return;
                };
                open_quick_note(&workspace, text, source, window, cx);
            })
            .detach();
    })
    .detach();
}

/// The roots of the workspace's visible worktrees, whose `.zed/notes` start being read.
pub fn worktree_roots(workspace: &Workspace, cx: &mut App) -> Vec<PathBuf> {
    let project = workspace.project().clone();
    project_roots(&project, cx)
}

pub(crate) fn project_roots(project: &Entity<Project>, cx: &mut App) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| worktree.read(cx).is_local())
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    let store = NotesStore::global(cx);
    store.update(cx, |store, cx| {
        for root in &roots {
            store.watch_project(root.clone(), cx);
        }
    });
    roots
}

/// The notes a project can see, without starting to read anything new.
pub fn notes_for_project(project: &Entity<Project>, cx: &App) -> Vec<Note> {
    let Some(store) = NotesStore::try_global(cx) else {
        return Vec::new();
    };
    let roots: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    store.read(cx).visible_notes(&roots)
}

pub fn relative_time(modified: SystemTime) -> SharedString {
    let elapsed = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs();
    let minutes = elapsed / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        i18n::t!("notes.time.now")
    } else if hours < 1 {
        i18n::t!("notes.time.minutes", count = minutes)
    } else if days < 1 {
        i18n::t!("notes.time.hours", count = hours)
    } else {
        i18n::t!("notes.time.days", count = days)
    }
}

pub fn open_quick_note(
    workspace: &Entity<Workspace>,
    text: String,
    source: Option<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        quick_note::open(workspace, text, source, window, cx);
    });
}
