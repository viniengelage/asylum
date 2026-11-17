use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashSet;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Editor, EditorEvent, MarkdownImagePasteHandler};
use fs::Fs;
use gpui::{App, Context, Entity, ObjectFit, Subscription, Task, img};
use i18n::t;
use text::Point;
use ui::prelude::*;
use util::ResultExt as _;

use crate::store::{NOTES_FOLDER_NAME, Note, NotesStore, global_notes_dir};

pub(crate) const ASSETS_FOLDER: &str = "assets";
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const IMAGE_BLOCK_ROWS: u32 = 12;

/// Whether a file is a note: a markdown file directly inside the user's or a project's notes folder.
pub fn is_note_path(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    parent == global_notes_dir()
        || parent.ends_with(Path::new(paths::local_settings_folder_name()).join(NOTES_FOLDER_NAME))
}

/// Saves an image under the note's `assets/` folder as `<note>-<n>.<ext>` and returns the
/// relative link to put in the markdown.
pub async fn save_note_image(
    fs: Arc<dyn Fs>,
    note_path: &Path,
    bytes: &[u8],
    extension: &str,
) -> Result<String> {
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!(t!(
            "notes.images.too_large",
            limit = MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let directory = note_path
        .parent()
        .context("note has no folder")?
        .join(ASSETS_FOLDER);
    let stem = note_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("note");
    fs.create_dir(&directory).await?;
    let mut index = 1u32;
    loop {
        let name = format!("{stem}-{index}.{extension}");
        let target = directory.join(&name);
        if !fs.is_file(&target).await {
            fs.write(&target, bytes).await?;
            return Ok(format!("{ASSETS_FOLDER}/{name}"));
        }
        index += 1;
    }
}

struct NoteImagePasteError;

pub(crate) fn init(cx: &mut App) {
    let fs = <dyn Fs>::global(cx);
    cx.set_global(MarkdownImagePasteHandler(Arc::new(
        move |path: &Path, image: &gpui::Image, cx: &mut App| {
            if !is_note_path(path) {
                return None;
            }
            let fs = fs.clone();
            let note_path = path.to_path_buf();
            let bytes = image.bytes.clone();
            let extension = image.format.extension().to_string();
            let saved = cx.background_spawn(async move {
                save_note_image(fs, &note_path, &bytes, &extension).await
            });
            // The editor only logs a failed paste; say it where the user is looking.
            Some(cx.spawn(async move |cx| {
                let result = saved.await;
                if let Err(error) = &result {
                    let message = format!("{error:#}");
                    cx.update(|cx| {
                        workspace::notifications::show_app_notification(
                            workspace::notifications::NotificationId::unique::<NoteImagePasteError>(),
                            cx,
                            move |cx| {
                                cx.new(|cx| {
                                    workspace::notifications::simple_message_notification::MessageNotification::new(
                                        t!("notes.images.paste_failed", error = message.clone()),
                                        cx,
                                    )
                                })
                            },
                        )
                    });
                }
                result
            }))
        },
    )));
}

/// A local image a note links to, found on one line of the note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedImage {
    pub row: u32,
    pub path: PathBuf,
}

/// The local images linked with `![…](…)`, one per link, skipping web addresses.
pub(crate) fn linked_images(markdown: &str, note_directory: &Path) -> Vec<LinkedImage> {
    let mut images = Vec::new();
    for (row, line) in markdown.lines().enumerate() {
        let mut rest = line;
        while let Some(start) = rest.find("![") {
            rest = &rest[start + 2..];
            let Some(close) = rest.find("](") else {
                break;
            };
            rest = &rest[close + 2..];
            let Some(end) = rest.find(')') else {
                break;
            };
            let target = rest[..end].split_whitespace().next().unwrap_or_default();
            rest = &rest[end + 1..];
            if target.is_empty() || target.contains("://") || target.starts_with("data:") {
                continue;
            }
            let path = Path::new(target);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                note_directory.join(path)
            };
            images.push(LinkedImage {
                row: row as u32,
                path,
            });
        }
    }
    images
}

fn images_in_assets(note: &Note) -> Vec<PathBuf> {
    let Some(directory) = note.path.parent() else {
        return Vec::new();
    };
    let assets = directory.join(ASSETS_FOLDER);
    linked_images(&note.body, directory)
        .into_iter()
        .map(|image| image.path)
        .filter(|path| path.starts_with(&assets))
        .collect()
}

/// Deletes a note and, when it had images in `assets/` that no other note uses, asks whether
/// to delete them too.
pub(crate) fn delete_note(
    store: Entity<NotesStore>,
    note: Note,
    window: &mut Window,
    cx: &mut App,
) {
    let referenced_elsewhere: HashSet<PathBuf> = store
        .read(cx)
        .notes_by_scope()
        .flat_map(|(_, notes)| notes.iter())
        .filter(|other| other.path != note.path)
        .flat_map(images_in_assets)
        .collect();
    let mut images: Vec<PathBuf> = images_in_assets(&note)
        .into_iter()
        .filter(|path| path.is_file() && !referenced_elsewhere.contains(path))
        .collect();
    images.dedup();
    let deleted = store.update(cx, |store, cx| store.delete(&note, cx));
    if images.is_empty() {
        deleted.detach_and_log_err(cx);
        return;
    }
    let answer = window.prompt(
        gpui::PromptLevel::Info,
        &t!("notes.images.delete_prompt", count = images.len()),
        None,
        &[
            t!("notes.images.delete_images").as_ref(),
            t!("notes.images.keep_images").as_ref(),
        ],
        cx,
    );
    let fs = <dyn Fs>::global(cx);
    cx.spawn(async move |_| {
        deleted.await?;
        if answer.await? == 0 {
            for image in images {
                fs.remove_file(&image, fs::RemoveOptions::default())
                    .await
                    .log_err();
            }
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Deletes the files in each notes folder's `assets/` that no note links to, returning how many.
pub(crate) fn remove_unused_images(cx: &mut App) -> Task<Result<usize>> {
    let fs = <dyn Fs>::global(cx);
    let folders: Vec<(PathBuf, HashSet<PathBuf>)> = NotesStore::global(cx)
        .read(cx)
        .notes_by_scope()
        .map(|(scope, notes)| {
            let assets = NotesStore::notes_dir(scope).join(ASSETS_FOLDER);
            let referenced = notes.iter().flat_map(images_in_assets).collect();
            (assets, referenced)
        })
        .collect();
    cx.background_spawn(async move {
        let mut removed = 0;
        for (assets, referenced) in folders {
            let Ok(mut entries) = fs.read_dir(&assets).await else {
                continue;
            };
            while let Some(entry) = futures::StreamExt::next(&mut entries).await {
                let Some(path) = entry.log_err() else {
                    continue;
                };
                if referenced.contains(&path) || !fs.is_file(&path).await {
                    continue;
                }
                if fs
                    .remove_file(&path, fs::RemoveOptions::default())
                    .await
                    .log_err()
                    .is_some()
                {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    })
}

/// Splits a draft into its text and the local images it links to, so resuming it puts the
/// images back as attachments instead of markdown links.
pub fn split_draft_images(markdown: &str, note_directory: &Path) -> (String, Vec<PathBuf>) {
    let mut paths = Vec::new();
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let images = linked_images(line, note_directory);
        let existing: Vec<PathBuf> = images
            .into_iter()
            .map(|image| image.path)
            .filter(|path| path.is_file())
            .collect();
        if existing.is_empty() || !line.trim_start().starts_with("![") {
            lines.push(line);
        }
        paths.extend(existing);
    }
    (lines.join("\n").trim_end().to_string(), paths)
}

/// Shows the images a note links to as blocks below their lines, in the note's editor.
struct NoteImageBlocks {
    note_directory: PathBuf,
    shown: Vec<LinkedImage>,
    block_ids: HashSet<CustomBlockId>,
    _subscription: Subscription,
}

impl editor::Addon for NoteImageBlocks {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

pub(crate) fn attach_to_editor(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(note_path) = editor.buffer().read(cx).as_singleton().and_then(|buffer| {
        buffer
            .read(cx)
            .file()?
            .as_local()
            .map(|file| file.abs_path(cx))
    }) else {
        return;
    };
    if !is_note_path(&note_path) {
        return;
    }
    let Some(note_directory) = note_path.parent().map(Path::to_path_buf) else {
        return;
    };
    let subscription = cx.subscribe_in(
        &cx.entity(),
        window,
        |editor, _, event: &EditorEvent, _window, cx| {
            if matches!(event, EditorEvent::BufferEdited | EditorEvent::Saved) {
                refresh_blocks(editor, cx);
            }
        },
    );
    editor.register_addon(NoteImageBlocks {
        note_directory,
        shown: Vec::new(),
        block_ids: HashSet::default(),
        _subscription: subscription,
    });
    refresh_blocks(editor, cx);
}

fn refresh_blocks(editor: &mut Editor, cx: &mut Context<Editor>) {
    let text = editor.buffer().read(cx).snapshot(cx).text();
    let Some(state) = editor.addon::<NoteImageBlocks>() else {
        return;
    };
    let images = linked_images(&text, &state.note_directory);
    if images == state.shown {
        return;
    }
    let old_blocks = state.block_ids.clone();
    editor.remove_blocks(old_blocks, None, cx);

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let blocks = images.iter().map(|image| {
        let anchor = snapshot.anchor_after(Point::new(image.row, 0));
        let path = image.path.clone();
        let exists = path.is_file();
        BlockProperties {
            placement: BlockPlacement::Below(anchor),
            height: Some(if exists { IMAGE_BLOCK_ROWS } else { 1 }),
            style: BlockStyle::Flex,
            priority: 0,
            render: Arc::new(move |block_cx| {
                let name: SharedString = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into();
                let max_height = block_cx.line_height * (IMAGE_BLOCK_ROWS as f32) - px(24.);
                let content = if exists {
                    v_flex()
                        .gap_0p5()
                        .child(
                            div().rounded(px(6.)).overflow_hidden().child(
                                img(path.clone())
                                    .max_h(max_height)
                                    .max_w(block_cx.max_width - block_cx.anchor_x - px(24.))
                                    .object_fit(ObjectFit::Contain),
                            ),
                        )
                        .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
                        .into_any_element()
                } else {
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(t!("notes.images.missing", name = name))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element()
                };
                div()
                    .pl(block_cx.anchor_x)
                    .py_1()
                    .child(content)
                    .into_any_element()
            }),
        }
    });
    let block_ids: HashSet<CustomBlockId> =
        editor.insert_blocks(blocks, None, cx).into_iter().collect();
    if let Some(state) = editor.addon_mut::<NoteImageBlocks>() {
        state.shown = images;
        state.block_ids = block_ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_images_finds_local_links_per_line() {
        let directory = Path::new("/notes");
        let markdown = "Intro\n![tela](assets/a-1.png) e ![](assets/a-2.png \"title\")\n\
                        ![web](https://example.com/x.png)\n![abs](/tmp/b.png)";
        assert_eq!(
            linked_images(markdown, directory),
            vec![
                LinkedImage {
                    row: 1,
                    path: PathBuf::from("/notes/assets/a-1.png"),
                },
                LinkedImage {
                    row: 1,
                    path: PathBuf::from("/notes/assets/a-2.png"),
                },
                LinkedImage {
                    row: 3,
                    path: PathBuf::from("/tmp/b.png"),
                },
            ]
        );
    }

    #[test]
    fn note_paths_are_markdown_inside_notes_folders() {
        assert!(is_note_path(&global_notes_dir().join("ideia.md")));
        assert!(is_note_path(Path::new("/projeto/.zed/notes/ideia.md")));
        assert!(!is_note_path(Path::new(
            "/projeto/.zed/notes/assets/ideia-1.png"
        )));
        assert!(!is_note_path(Path::new("/projeto/docs/ideia.md")));
    }
}
