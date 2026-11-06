use std::path::PathBuf;
use std::time::Duration;

use agent::model_benchmarks::{self, BenchmarkWarning, ModelBenchmarks};
use collections::HashSet;
use editor::{
    Addon, Editor, EditorEvent, RowHighlightOptions,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock},
};
use fs::Fs;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Hsla, Subscription, Task, WeakEntity,
};
use i18n::t;
use std::sync::Arc;
use ui::{prelude::*, secondary_button};
use util::ResultExt as _;
use workspace::{
    SplitDirection, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle,
};

/// Edits settle this long before the warnings move, so typing doesn't reflow them each key.
const REFRESH_DELAY: Duration = Duration::from_millis(300);

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if window.is_none() || !editor.mode().is_full() {
            return;
        }
        let editor_entity = cx.entity();
        let warnings = cx.new(|cx| BenchmarkWarnings::new(editor_entity, cx));
        editor.register_addon(BenchmarkWarningsAddon {
            _warnings: warnings,
        });
    })
    .detach();
}

fn benchmarks_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    let path = file.as_local()?.abs_path(cx);
    model_benchmarks::is_model_benchmarks_path(&path).then_some(path)
}

struct BenchmarkWarningsAddon {
    _warnings: Entity<BenchmarkWarnings>,
}

impl Addon for BenchmarkWarningsAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct BenchmarkWarnings {
    editor: WeakEntity<Editor>,
    blocks: HashSet<CustomBlockId>,
    warnings: Vec<BenchmarkWarning>,
    refresh_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl BenchmarkWarnings {
    fn new(editor: Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if matches!(
                event,
                EditorEvent::BufferEdited | EditorEvent::Saved | EditorEvent::TitleChanged
            ) {
                this.schedule_refresh(cx);
            }
        })];
        let mut this = Self {
            editor: editor.downgrade(),
            blocks: HashSet::default(),
            warnings: Vec::new(),
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
        let is_benchmarks = benchmarks_path(editor.read(cx), cx).is_some();
        let warnings: Vec<BenchmarkWarning> = if is_benchmarks {
            ModelBenchmarks::parse(&editor.read(cx).text(cx))
                .warnings
                .into_iter()
                .filter(|warning| warning.line.is_some())
                .collect()
        } else {
            Vec::new()
        };
        if warnings == self.warnings {
            return;
        }
        self.warnings = warnings.clone();

        let old_blocks = std::mem::take(&mut self.blocks);
        let new_blocks = editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<Self>();
            if !old_blocks.is_empty() {
                editor.remove_blocks(old_blocks, None, cx);
            }
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;
            let blocks: Vec<_> = warnings
                .iter()
                .filter_map(|warning| {
                    let row = u32::try_from(warning.line?.checked_sub(1)?).ok()?;
                    (row <= max_row).then_some((row, warning))
                })
                .map(|(row, warning)| {
                    let start = snapshot.anchor_before(language::Point::new(row, 0));
                    let end = snapshot.anchor_after(language::Point::new(
                        row,
                        snapshot.line_len(multi_buffer::MultiBufferRow(row)),
                    ));
                    editor.highlight_rows::<Self>(
                        start..end,
                        warning_row_background,
                        RowHighlightOptions::default(),
                        cx,
                    );
                    BlockProperties {
                        placement: BlockPlacement::Below(start),
                        height: Some(1),
                        style: BlockStyle::Flex,
                        render: render_warning(model_benchmarks::warning_message(warning)),
                        priority: 0,
                    }
                })
                .collect();
            editor.insert_blocks(blocks, None, cx)
        });
        self.blocks = new_blocks.into_iter().collect();
    }
}

fn warning_row_background(cx: &App) -> Hsla {
    cx.theme().status().warning.opacity(0.08)
}

fn render_warning(message: SharedString) -> RenderBlock {
    Arc::new(move |cx| {
        let colors = cx.app.theme().colors();
        let warning = cx.app.theme().status().warning;
        h_flex()
            .id(cx.block_id)
            .h(cx.line_height)
            .pl(cx.anchor_x)
            .child(
                h_flex()
                    .gap_1p5()
                    .px_2()
                    .rounded_md()
                    .bg(colors.editor_background.blend(warning.opacity(0.12)))
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(message.clone())
                            .size(LabelSize::Small)
                            .color(Color::Warning)
                            .single_line(),
                    ),
            )
            .into_any_element()
    })
}

pub struct ModelBenchmarksToolbar {
    editor: Option<WeakEntity<Editor>>,
    path: Option<PathBuf>,
}

impl ModelBenchmarksToolbar {
    pub fn new() -> Self {
        Self {
            editor: None,
            path: None,
        }
    }

    fn compare_with_default(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref().and_then(|editor| editor.upgrade()) else {
            return;
        };
        let (Some(project), Some(workspace)) = (
            editor.read(cx).project().cloned(),
            editor.read(cx).workspace(),
        ) else {
            return;
        };
        if !project.read(cx).is_local() {
            return;
        }
        let languages = project.read(cx).languages().clone();
        cx.spawn_in(window, async move |_, cx| {
            let markdown = languages.language_for_name("Markdown").await.log_err();
            workspace.update_in(cx, |workspace, window, cx| {
                let buffer = project.update(cx, |project, cx| {
                    project.create_local_buffer(
                        model_benchmarks::DEFAULT_MODEL_BENCHMARKS,
                        markdown,
                        false,
                        cx,
                    )
                });
                let default_editor = cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.split_item(SplitDirection::Right, Box::new(default_editor), window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn restore_default(&self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let fs = <dyn Fs>::global(cx);
        cx.background_spawn(model_benchmarks::restore_default_model_benchmarks(fs, path))
            .detach_and_log_err(cx);
    }
}

impl EventEmitter<ToolbarItemEvent> for ModelBenchmarksToolbar {}

impl ToolbarItemView for ModelBenchmarksToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.editor = None;
        self.path = None;
        let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) else {
            return ToolbarItemLocation::Hidden;
        };
        let Some(path) = benchmarks_path(editor.read(cx), cx) else {
            return ToolbarItemLocation::Hidden;
        };
        self.editor = Some(editor.downgrade());
        self.path = Some(path);
        cx.notify();
        ToolbarItemLocation::PrimaryRight
    }
}

impl Render for ModelBenchmarksToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_compare = self
            .editor
            .as_ref()
            .and_then(|editor| editor.upgrade())
            .and_then(|editor| editor.read(cx).project().cloned())
            .is_some_and(|project| project.read(cx).is_local());
        h_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Sparkle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("agent_ui.model_benchmarks.used_by_auto"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(can_compare, |this| {
                this.child(
                    secondary_button(
                        "model-benchmarks-compare",
                        None,
                        t!("agent_ui.model_benchmarks.compare"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.compare_with_default(window, cx);
                    })),
                )
            })
            .child(
                secondary_button(
                    "model-benchmarks-restore",
                    Some((IconName::RotateCcw, Color::Muted)),
                    t!("agent.model_benchmarks.restore"),
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.restore_default(cx))),
            )
    }
}
