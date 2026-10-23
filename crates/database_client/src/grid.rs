//! The result grid shared by the table tab and the SQL editor: a virtualized `ui::Table` with a
//! pinned row-number column, resizable columns, one selected cell and a strip that shows the
//! whole value of that cell.

use crate::edits::PendingEdits;
use editor::{Editor, MultiBuffer};
use gpui::{
    AbsoluteLength, Anchor, AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, WeakEntity, actions, anchored, deferred, px,
};
use std::ops::Range;
use ui::{
    ColumnWidthConfig, ResizableColumnsState, Table, TableInteractionState, TableResizeBehavior,
    Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    database_client,
    [
        /// Copies the value of the selected cell.
        CopyCell,
        /// Selects the cell above.
        SelectCellAbove,
        /// Selects the cell below.
        SelectCellBelow,
        /// Selects the cell to the left.
        SelectCellLeft,
        /// Selects the cell to the right.
        SelectCellRight,
        /// Starts editing the selected cell.
        EditCell,
        /// Keeps the value typed in the cell.
        ConfirmCellEdit,
        /// Leaves the cell as it was.
        CancelCellEdit,
    ]
);

/// Longer values are cut in the cell; the detail strip and Copy keep the whole thing.
const MAX_CELL_CHARS: usize = 200;
const MIN_COLUMN_WIDTH: f32 = 72.;
const MAX_COLUMN_WIDTH: f32 = 360.;
const CHAR_WIDTH: f32 = 7.6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridColumn {
    pub name: String,
    pub type_name: Option<String>,
}

impl GridColumn {
    fn is_numeric(&self) -> bool {
        matches!(
            self.type_name.as_deref(),
            Some(
                "int2"
                    | "int4"
                    | "int8"
                    | "smallint"
                    | "integer"
                    | "bigint"
                    | "numeric"
                    | "float4"
                    | "float8"
                    | "real"
                    | "double precision"
                    | "oid"
                    | "money"
            )
        ) || self
            .type_name
            .as_deref()
            .is_some_and(|type_name| type_name.starts_with("numeric("))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: usize,
    pub descending: bool,
}

pub enum GridEvent {
    /// A header was clicked; the owner reorders and reloads.
    SortRequested(usize),
    /// A cell got a new value (`None` is NULL); the owner keeps it as a pending change.
    CellEdited {
        row: usize,
        column: usize,
        value: Option<String>,
    },
    /// "Abrir no editor" on the selected cell: the owner opens the value in a buffer.
    OpenValue { column: GridColumn, value: String },
}

struct CellEditor {
    row: usize,
    column: usize,
    editor: Entity<Editor>,
}

pub struct ResultGrid {
    columns: Vec<GridColumn>,
    rows: Vec<Vec<Option<String>>>,
    display: Vec<Vec<Option<SharedString>>>,
    first_row_number: usize,
    sort: Option<Sort>,
    sortable: bool,
    selected: Option<(usize, usize)>,
    /// The value popover under the selected cell; a click opens it, the keyboard closes it.
    detail_open: bool,
    editable: bool,
    pending: PendingEdits,
    editing: Option<CellEditor>,
    widths: Entity<ResizableColumnsState>,
    interaction: Entity<TableInteractionState>,
    focus_handle: FocusHandle,
}

impl EventEmitter<GridEvent> for ResultGrid {}

impl ResultGrid {
    pub fn new(sortable: bool, cx: &mut Context<Self>) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            display: Vec::new(),
            first_row_number: 1,
            sort: None,
            sortable,
            selected: None,
            detail_open: false,
            editable: false,
            pending: PendingEdits::default(),
            editing: None,
            widths: cx.new(|_| {
                ResizableColumnsState::new(1, vec![px(48.)], vec![TableResizeBehavior::None])
            }),
            interaction: cx.new(|cx| TableInteractionState::new(cx)),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_editable(&mut self, editable: bool, cx: &mut Context<Self>) {
        self.editable = editable;
        cx.notify();
    }

    /// Values the owner hasn't written yet, shown in place of what was loaded.
    pub fn set_pending(&mut self, pending: PendingEdits, cx: &mut Context<Self>) {
        self.pending = pending;
        cx.notify();
    }

    pub fn rows(&self) -> &[Vec<Option<String>>] {
        &self.rows
    }

    pub fn columns(&self) -> &[GridColumn] {
        &self.columns
    }

    fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, column)) = self.selected.filter(|_| self.editable) else {
            return;
        };
        let current = match self.pending.get(&(row, column)) {
            Some(value) => value.clone(),
            None => self
                .rows
                .get(row)
                .and_then(|values| values.get(column))
                .cloned()
                .flatten(),
        };
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(current.unwrap_or_default(), window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.editing = Some(CellEditor {
            row,
            column,
            editor,
        });
        cx.notify();
    }

    fn confirm_edit(&mut self, _: &ConfirmCellEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let value = editing.editor.read(cx).text(cx);
        cx.emit(GridEvent::CellEdited {
            row: editing.row,
            column: editing.column,
            value: Some(value),
        });
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &CancelCellEdit, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn set_selected_null(&mut self, cx: &mut Context<Self>) {
        if let Some((row, column)) = self.selected.filter(|_| self.editable) {
            self.editing = None;
            cx.emit(GridEvent::CellEdited {
                row,
                column,
                value: None,
            });
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn set_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        self.sort = sort;
        cx.notify();
    }

    /// Replaces the contents. Column widths are kept while the columns stay the same, so paging
    /// through a table doesn't make them jump.
    pub fn set_data(
        &mut self,
        columns: Vec<GridColumn>,
        rows: Vec<Vec<Option<String>>>,
        first_row_number: usize,
        cx: &mut Context<Self>,
    ) {
        let same_columns = columns == self.columns;
        self.display = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| value.as_deref().map(display_value))
                    .collect()
            })
            .collect();
        if !same_columns {
            let number_width = px(((first_row_number + rows.len()).to_string().len() as f32
                * CHAR_WIDTH
                + 24.)
                .max(44.));
            let mut widths: Vec<AbsoluteLength> = vec![number_width.into()];
            widths.extend(columns.iter().enumerate().map(|(index, column)| {
                let header =
                    column.name.len() + column.type_name.as_ref().map_or(0, |name| name.len() + 1);
                let longest = self
                    .display
                    .iter()
                    .take(50)
                    .filter_map(|row| row.get(index).cloned().flatten())
                    .map(|value| value.chars().count())
                    .max()
                    .unwrap_or(4);
                let characters = header.max(longest) as f32;
                AbsoluteLength::from(px(
                    (characters * CHAR_WIDTH + 28.).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
                ))
            }));
            let mut behaviors = vec![TableResizeBehavior::Resizable; columns.len() + 1];
            behaviors[0] = TableResizeBehavior::None;
            let cols = columns.len() + 1;
            self.widths.update(cx, |state, _| {
                *state = ResizableColumnsState::new(cols, widths, behaviors)
            });
            self.selected = None;
        } else if self.selected.is_some_and(|(row, _)| row >= rows.len()) {
            self.selected = None;
        }
        self.editing = None;
        self.columns = columns;
        self.rows = rows;
        self.first_row_number = first_row_number;
        cx.notify();
    }

    /// The whole result as CSV (RFC 4180 quoting), NULL as an empty field.
    pub fn to_csv(&self) -> String {
        to_csv(&self.columns, &self.rows)
    }

    /// The whole result as a JSON array of objects, values as the server sent them (text).
    pub fn to_json(&self) -> String {
        let rows = self
            .rows
            .iter()
            .map(|row| {
                serde_json::Value::Object(
                    self.columns
                        .iter()
                        .zip(row)
                        .map(|(column, value)| {
                            (
                                column.name.clone(),
                                value
                                    .clone()
                                    .map_or(serde_json::Value::Null, serde_json::Value::String),
                            )
                        })
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        serde_json::to_string_pretty(&rows).unwrap_or_default()
    }

    /// One multi-row INSERT into `table`; numbers go unquoted, everything else as a literal.
    pub fn to_insert(&self, table: &str) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| crate::catalog::quote_ident(&column.name))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .rows
            .iter()
            .map(|row| {
                let values = self
                    .columns
                    .iter()
                    .zip(row)
                    .map(|(column, value)| match value {
                        None => "NULL".to_owned(),
                        Some(value) if column.is_numeric() => value.clone(),
                        Some(value) => crate::catalog::quote_literal(value),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("  ({values})")
            })
            .collect::<Vec<_>>()
            .join(",\n");
        format!("insert into {table} ({columns}) values\n{rows};\n")
    }

    pub fn selected_value(&self) -> Option<(&GridColumn, Option<&str>)> {
        let (row, column) = self.selected?;
        let value = self.rows.get(row)?.get(column)?;
        Some((self.columns.get(column)?, value.as_deref()))
    }

    fn copy_cell(&mut self, _: &CopyCell, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, value)) = self.selected_value() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                value.unwrap_or_default().to_owned(),
            ));
        }
    }

    fn move_selection(&mut self, rows: isize, columns: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() || self.columns.is_empty() {
            return;
        }
        let (row, column) = self.selected.unwrap_or((0, 0));
        let row = (row as isize + rows).clamp(0, self.rows.len() as isize - 1) as usize;
        let column = (column as isize + columns).clamp(0, self.columns.len() as isize - 1) as usize;
        self.selected = Some((row, column));
        self.detail_open = false;
        cx.notify();
    }

    fn open_value(&mut self, cx: &mut Context<Self>) {
        if let Some((column, value)) = self.selected_value() {
            let event = GridEvent::OpenValue {
                column: column.clone(),
                value: value.unwrap_or_default().to_owned(),
            };
            self.detail_open = false;
            cx.emit(event);
        }
    }

    fn render_header(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(column) = self.columns.get(index) else {
            return div().into_any_element();
        };
        let sorted = self
            .sort
            .filter(|sort| sort.column == index)
            .map(|sort| sort.descending);
        h_flex()
            .id(("db-grid-header", index))
            .w_full()
            .gap_1p5()
            .overflow_hidden()
            .when(self.sortable, |this| {
                this.cursor_pointer().on_click(
                    cx.listener(move |_, _, _, cx| cx.emit(GridEvent::SortRequested(index))),
                )
            })
            .child(
                Label::new(column.name.clone())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .weight(FontWeight::SEMIBOLD)
                    .single_line(),
            )
            .when_some(column.type_name.clone(), |this, type_name| {
                this.child(
                    Label::new(type_name)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Disabled)
                        .single_line(),
                )
            })
            .when_some(sorted, |this, descending| {
                this.child(
                    Icon::new(if descending {
                        IconName::ArrowDown
                    } else {
                        IconName::ArrowUp
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
                )
            })
            .into_any_element()
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Vec<AnyElement>> {
        let selected_border = cx.theme().colors().border_focused;
        let selected_row_background = cx.theme().colors().element_selected.opacity(0.35);
        range
            .filter_map(|row_index| {
                let row = self.display.get(row_index)?;
                let row_selected = self.selected.is_some_and(|(row, _)| row == row_index);
                let mut cells = Vec::with_capacity(row.len() + 1);
                cells.push(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .child(
                            Label::new((self.first_row_number + row_index).to_string())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Disabled),
                        )
                        .into_any_element(),
                );
                for (column_index, value) in row.iter().enumerate() {
                    let numeric = self
                        .columns
                        .get(column_index)
                        .is_some_and(GridColumn::is_numeric);
                    let cell_selected = self.selected == Some((row_index, column_index));
                    if let Some(editing) = self.editing.as_ref().filter(|editing| {
                        editing.row == row_index && editing.column == column_index
                    }) {
                        cells.push(
                            div()
                                .key_context("DatabaseCellEditor")
                                .w_full()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(selected_border)
                                .bg(cx.theme().colors().editor_background)
                                .child(editing.editor.clone())
                                .into_any_element(),
                        );
                        continue;
                    }
                    let pending = self.pending.get(&(row_index, column_index));
                    let pending_display = pending.map(|value| value.as_deref().map(display_value));
                    let value = match &pending_display {
                        Some(pending) => pending,
                        None => value,
                    };
                    cells.push(
                        div()
                            .id(("db-grid-cell", row_index * 4096 + column_index))
                            .w_full()
                            .h_full()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .when(numeric, |this| this.justify_end())
                            .when(row_selected, |this| this.bg(selected_row_background))
                            .when(pending.is_some(), |this| {
                                this.bg(Color::Warning.color(cx).opacity(0.14))
                                    .border_l_2()
                                    .border_color(Color::Warning.color(cx))
                            })
                            .when(cell_selected, |this| {
                                this.relative()
                                    .border_1()
                                    .border_color(selected_border)
                                    .rounded_sm()
                            })
                            .when(cell_selected && self.detail_open, |this| {
                                this.children(self.render_detail(row_index, cx).map(|detail| {
                                    div().absolute().left_0().top_full().child(
                                        deferred(
                                            anchored()
                                                .anchor(Anchor::TopLeft)
                                                .snap_to_window_with_margin(px(8.))
                                                .child(detail),
                                        )
                                        .with_priority(1),
                                    )
                                }))
                            })
                            .child(match value {
                                Some(value) => Label::new(value.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .when(pending.is_some(), |label| label.color(Color::Warning))
                                    .single_line()
                                    .truncate()
                                    .into_any_element(),
                                None => Label::new("NULL")
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .italic()
                                    .color(Color::Disabled)
                                    .into_any_element(),
                            })
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    let reselected =
                                        this.selected == Some((row_index, column_index));
                                    this.selected = Some((row_index, column_index));
                                    this.detail_open = !(reselected && this.detail_open);
                                    if event.click_count() >= 2 {
                                        this.detail_open = false;
                                        this.start_editing(window, cx);
                                    } else {
                                        this.editing = None;
                                        window.focus(&this.focus_handle, cx);
                                    }
                                    cx.notify();
                                },
                            ))
                            .into_any_element(),
                    );
                }
                Some(cells)
            })
            .collect()
    }

    /// The whole value of the selected cell, with what can be done with it.
    fn render_detail(&self, row_index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (column, value) = self.selected_value()?;
        let colors = cx.theme().colors();
        let type_name = column.type_name.clone().unwrap_or_default();
        let is_json = matches!(type_name.as_str(), "json" | "jsonb");
        let text = value.map(|value| {
            if is_json {
                serde_json::from_str::<serde_json::Value>(value)
                    .ok()
                    .and_then(|json| serde_json::to_string_pretty(&json).ok())
                    .unwrap_or_else(|| value.to_owned())
            } else {
                value.to_owned()
            }
        });
        let action = |id: &'static str, label: &'static str| {
            h_flex()
                .id(id)
                .gap_1()
                .cursor_pointer()
                .child(Label::new(label).size(LabelSize::Small))
        };
        Some(
            v_flex()
                .id("db-grid-detail")
                .occlude()
                .w(px(320.))
                .max_h(px(360.))
                .mt_1()
                .rounded_lg()
                .border_1()
                .border_color(colors.border_selected)
                .bg(colors.elevated_surface_background)
                .shadow_lg()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    h_flex()
                        .px_3()
                        .py_2p5()
                        .gap_2p5()
                        .border_b_1()
                        .border_color(colors.border)
                        .child(
                            Label::new(column.name.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(format!(
                                "{type_name} · linha {}",
                                self.first_row_number + row_index
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    v_flex()
                        .id("db-grid-detail-value")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_3()
                        .py_3()
                        .gap_0p5()
                        .map(|this| match &text {
                            Some(text) => this.children(
                                text.lines()
                                    .map(|line| json_line(line, is_json, cx).into_any_element()),
                            ),
                            None => this.child(
                                Label::new("NULL")
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .italic()
                                    .color(Color::Muted),
                            ),
                        }),
                )
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_4()
                        .border_t_1()
                        .border_color(colors.border)
                        .child(action("db-grid-copy", "Copiar").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.copy_cell(&CopyCell, window, cx)
                            }),
                        ))
                        .when(self.editable, |this| {
                            this.child(
                                action("db-grid-edit", "Editar")
                                    .child(
                                        Label::new("↵")
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.detail_open = false;
                                        this.start_editing(window, cx)
                                    })),
                            )
                        })
                        .child(
                            action("db-grid-open", "Abrir no editor")
                                .on_click(cx.listener(|this, _, _, cx| this.open_value(cx))),
                        )
                        .child(div().flex_1())
                        .when(self.editable, |this| {
                            this.child(
                                h_flex()
                                    .id("db-grid-null")
                                    .cursor_pointer()
                                    .tooltip(Tooltip::text("Deixar esta célula NULL"))
                                    .child(
                                        Label::new("NULL")
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.detail_open = false;
                                        this.set_selected_null(cx)
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

/// One line of the value popover. Pretty-printed JSON gets its keys, strings and literals
/// colored like the board; anything else is plain.
fn json_line(line: &str, is_json: bool, cx: &App) -> impl IntoElement {
    let label = |text: String, color: Color| {
        Label::new(text)
            .size(LabelSize::Small)
            .buffer_font(cx)
            .color(color)
            .single_line()
    };
    let string_color = Color::Custom(cx.theme().status().created);
    let trimmed = line.trim_start();
    let indent = line[..line.len() - trimmed.len()].to_owned();
    let row = h_flex().child(label(indent, Color::Default));
    if !is_json {
        return row.child(label(trimmed.to_owned(), Color::Default));
    }
    let value_color = |value: &str| {
        if value.starts_with('"') {
            string_color
        } else if matches!(
            value.trim_end_matches(','),
            "{" | "}" | "[" | "]" | "{}" | "[]"
        ) {
            Color::Muted
        } else {
            Color::Warning
        }
    };
    let key_end = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.find("\": "))
        .map(|end| end + 3);
    match key_end {
        Some(end) => row
            .child(label(trimmed[..end].to_owned(), Color::Accent))
            .child(label(
                trimmed[end..].to_owned(),
                value_color(&trimmed[end..]),
            )),
        None => row.child(label(trimmed.to_owned(), value_color(trimmed))),
    }
}

/// "Abrir no editor": the value in a new unsaved buffer, pretty-printed and highlighted when it
/// is JSON.
pub(crate) fn open_value(
    workspace: WeakEntity<Workspace>,
    column: GridColumn,
    value: String,
    window: &mut Window,
    cx: &mut App,
) {
    let is_json = matches!(column.type_name.as_deref(), Some("json" | "jsonb"));
    let text = if is_json {
        serde_json::from_str::<serde_json::Value>(&value)
            .ok()
            .and_then(|json| serde_json::to_string_pretty(&json).ok())
            .unwrap_or(value)
    } else {
        value
    };
    let Some((project, language)) = workspace
        .update(cx, |workspace, _| {
            let language =
                is_json.then(|| workspace.app_state().languages.language_for_name("JSON"));
            (workspace.project().clone(), language)
        })
        .log_err()
    else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            let language = match language {
                Some(language) => language.await.log_err(),
                None => None,
            };
            let buffer = project
                .update(cx, |project, cx| project.create_buffer(language, true, cx))
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
            workspace.update_in(cx, |workspace, window, cx| {
                let title = column.name.clone();
                let buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.clone()));
                let editor = cx.new(|cx| {
                    let mut editor = Editor::for_multibuffer(buffer, Some(project), window, cx);
                    editor.set_breadcrumb_header(title);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
}

fn to_csv(columns: &[GridColumn], rows: &[Vec<Option<String>>]) -> String {
    fn field(value: &str) -> String {
        if value.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.to_owned()
        }
    }
    let mut csv = columns
        .iter()
        .map(|column| field(&column.name))
        .collect::<Vec<_>>()
        .join(",");
    csv.push('\n');
    for row in rows {
        let line = row
            .iter()
            .map(|value| value.as_deref().map(field).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(",");
        csv.push_str(&line);
        csv.push('\n');
    }
    csv
}

/// One line, at most `MAX_CELL_CHARS` characters, so a huge jsonb value can't blow up layout.
fn display_value(value: &str) -> SharedString {
    let mut result = String::new();
    for (count, character) in value.chars().enumerate() {
        if count == MAX_CELL_CHARS {
            result.push('…');
            break;
        }
        result.push(match character {
            '\n' | '\r' | '\t' => ' ',
            character => character,
        });
    }
    result.into()
}

impl Focusable for ResultGrid {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cols = self.columns.len() + 1;
        let widths_match = self.widths.read(cx).cols() == cols;
        let mut headers = Vec::with_capacity(cols);
        headers.push(
            Label::new("#")
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Disabled)
                .into_any_element(),
        );
        for index in 0..self.columns.len() {
            headers.push(self.render_header(index, cx));
        }
        let row_count = self.rows.len();
        v_flex()
            .key_context("DatabaseGrid")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy_cell))
            .on_action(cx.listener(|this, _: &EditCell, window, cx| this.start_editing(window, cx)))
            .on_action(cx.listener(Self::confirm_edit))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(
                cx.listener(|this, _: &SelectCellBelow, _, cx| this.move_selection(1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectCellAbove, _, cx| this.move_selection(-1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectCellRight, _, cx| this.move_selection(0, 1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectCellLeft, _, cx| this.move_selection(0, -1, cx)),
            )
            .size_full()
            .child(div().flex_1().min_h_0().when(
                widths_match && !self.columns.is_empty(),
                |this| {
                    this.child(
                        Table::new(cols)
                            .interactable(&self.interaction)
                            .width_config(ColumnWidthConfig::Resizable(self.widths.clone()))
                            .header(headers)
                            .pin_cols(1)
                            .uniform_list(
                                "db-grid-rows",
                                row_count,
                                cx.processor(move |this, range: Range<usize>, _window, cx| {
                                    this.render_rows(range, cx)
                                }),
                            ),
                    )
                },
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_and_multiline_values_fit_one_line() {
        assert_eq!(display_value("a\nb\tc").as_ref(), "a b c");
        let long = "x".repeat(MAX_CELL_CHARS + 50);
        let shown = display_value(&long);
        assert_eq!(shown.chars().count(), MAX_CELL_CHARS + 1);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn csv_quotes_what_needs_quoting() {
        let columns = [
            GridColumn {
                name: "id".into(),
                type_name: None,
            },
            GridColumn {
                name: "note".into(),
                type_name: None,
            },
        ];
        let rows = [
            vec![Some("1".to_owned()), Some("a, \"b\"".to_owned())],
            vec![Some("2".to_owned()), None],
        ];
        assert_eq!(to_csv(&columns, &rows), "id,note\n1,\"a, \"\"b\"\"\"\n2,\n");
    }

    #[test]
    fn numeric_columns_align_right() {
        let column = |type_name: &str| GridColumn {
            name: "x".into(),
            type_name: Some(type_name.into()),
        };
        assert!(column("int8").is_numeric());
        assert!(column("numeric(14,2)").is_numeric());
        assert!(!column("text").is_numeric());
        assert!(!column("timestamptz").is_numeric());
    }
}
