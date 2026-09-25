//! "▶ Enviar · Abrir no painel API" above each operation of a linked spec open in the
//! editor. The editor's own code lenses only come from language servers, so these are
//! plain blocks that follow the text.

use crate::{collection::Collection, example, request_view, request_view::ApiRequestView};
use collections::{HashMap, HashSet};
use editor::{
    Addon, Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock},
};
use gpui::{
    App, AppContext as _, Context, Entity, Global, MouseButton, Subscription, Task, WeakEntity,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::prelude::*;
use util::ResultExt as _;

/// Edits settle this long before the lenses move, so typing doesn't reflow them each key.
const REFRESH_DELAY: Duration = Duration::from_millis(300);

/// The specs linked to a collection, by absolute path.
#[derive(Default)]
struct SpecRegistry {
    specs: HashMap<PathBuf, WeakEntity<Collection>>,
}

impl Global for SpecRegistry {}

pub fn register_spec(path: PathBuf, collection: WeakEntity<Collection>, cx: &mut App) {
    // Taking the global mutably tells every lens to refresh, so only when something changed.
    let unchanged = cx.try_global::<SpecRegistry>().is_some_and(|registry| {
        registry
            .specs
            .get(&path)
            .is_some_and(|existing| existing == &collection)
    });
    if !unchanged {
        cx.default_global::<SpecRegistry>()
            .specs
            .insert(path, collection);
    }
}

fn collection_for(path: &Path, cx: &App) -> Option<Entity<Collection>> {
    cx.try_global::<SpecRegistry>()?.specs.get(path)?.upgrade()
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || !editor.mode().is_full() {
            return;
        }
        let editor_entity = cx.entity();
        let lens = cx.new(|cx| SpecLens::new(editor_entity, cx));
        editor.register_addon(SpecLensAddon { _lens: lens });
    })
    .detach();
}

/// Ties the lens to the editor's lifetime.
struct SpecLensAddon {
    _lens: Entity<SpecLens>,
}

impl Addon for SpecLensAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct SpecLens {
    editor: WeakEntity<Editor>,
    blocks: HashSet<CustomBlockId>,
    refresh_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SpecLens {
    fn new(editor: Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
                if matches!(
                    event,
                    EditorEvent::BufferEdited | EditorEvent::Saved | EditorEvent::TitleChanged
                ) {
                    this.schedule_refresh(cx);
                }
            }),
            cx.observe_global::<SpecRegistry>(|this, cx| this.schedule_refresh(cx)),
        ];
        let mut this = Self {
            editor: editor.downgrade(),
            blocks: HashSet::default(),
            refresh_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.schedule_refresh(cx);
        this
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DELAY).await;
            this.update(cx, |this, cx| this.refresh(cx)).log_err();
        });
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let path = editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| {
                let file = buffer.read(cx).file()?;
                Some(file.as_local()?.abs_path(cx))
            });
        let collection = path.as_deref().and_then(|path| collection_for(path, cx));
        let operations: Vec<(u32, String)> = match &collection {
            Some(collection) => {
                let spec = collection.read(cx).spec.clone();
                let text = editor.read(cx).text(cx);
                example::operation_rows(&text)
                    .into_iter()
                    .map(|(row, method, path)| (row, format!("{method} {path}")))
                    .filter(|(_, key)| {
                        spec.as_ref()
                            .is_some_and(|spec| spec.operation(key).is_some())
                    })
                    .collect()
            }
            None => Vec::new(),
        };

        let old_blocks = std::mem::take(&mut self.blocks);
        let editor_handle = self.editor.clone();
        let new_blocks = editor.update(cx, |editor, cx| {
            if !old_blocks.is_empty() {
                editor.remove_blocks(old_blocks, None, cx);
            }
            let Some(collection) = collection else {
                return Vec::new();
            };
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let text = snapshot.text();
            let line_indents: Vec<usize> = text
                .lines()
                .map(|line| line.len() - line.trim_start().len())
                .collect();
            let blocks = operations.into_iter().map(|(row, key)| {
                let indent = line_indents.get(row as usize).copied().unwrap_or(0);
                BlockProperties {
                    placement: BlockPlacement::Above(
                        snapshot.anchor_before(language::Point::new(row, 0)),
                    ),
                    height: Some(1),
                    style: BlockStyle::Spacer,
                    render: render_lens(key, indent, collection.downgrade(), editor_handle.clone()),
                    priority: 0,
                }
            });
            editor.insert_blocks(blocks, None, cx)
        });
        self.blocks = new_blocks.into_iter().collect();
    }
}

fn render_lens(
    operation_key: String,
    indent: usize,
    collection: WeakEntity<Collection>,
    editor: WeakEntity<Editor>,
) -> RenderBlock {
    Arc::new(move |cx| {
        let text_style = &cx.editor_style.text;
        let font = text_style.font();
        let font_size = text_style.font_size.to_pixels(cx.window.rem_size()) * 0.85;
        let colors = cx.app.theme().colors();
        let accent = colors.text_accent;
        let muted = colors.text_muted;
        let action = |id: &'static str, label: &'static str, send: bool| {
            let operation_key = operation_key.clone();
            let collection = collection.clone();
            let editor = editor.clone();
            div()
                .id(id)
                .cursor_pointer()
                .text_color(accent)
                .hover(|style| style.text_color(colors.text))
                .child(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    open_operation(
                        &editor,
                        &collection,
                        operation_key.clone(),
                        send,
                        window,
                        cx,
                    );
                })
        };
        div()
            .id(cx.block_id)
            .pl(cx.em_width * (indent as f32))
            .h_full()
            .flex()
            .flex_row()
            .items_end()
            .gap_1p5()
            .font(font)
            .text_size(font_size)
            .child(action("api-lens-send", "▶ Enviar", true))
            .child(div().text_color(muted).child("·"))
            .child(action("api-lens-open", "Abrir no painel API", false))
            .into_any_element()
    })
}

fn open_operation(
    editor: &WeakEntity<Editor>,
    collection: &WeakEntity<Collection>,
    operation_key: String,
    send: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor
        .upgrade()
        .and_then(|editor| editor.read(cx).workspace())
    else {
        return;
    };
    let Some(collection) = collection.upgrade() else {
        return;
    };
    request_view::open(
        workspace.downgrade(),
        collection,
        operation_key,
        None,
        window,
        cx,
    );
    if send && let Some(view) = workspace.read(cx).active_item_as::<ApiRequestView>(cx) {
        view.update(cx, |view, cx| view.send(window, cx));
    }
}
