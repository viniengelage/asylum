//! A table opened from the dock: its rows a page at a time (with a free WHERE and ORDER BY),
//! its columns, indexes, foreign keys and a rebuilt DDL. Each tab has its own read-only
//! connection, so a typed filter can't write and a slow page doesn't hold up the dock.

use crate::{
    ApplyEdits, ApplyTableFilter,
    catalog::{
        self, ColumnInfo, ForeignKey, ForeignKeyDirection, IndexInfo, RelationKind, qualified_name,
        quote_ident,
    },
    connection::{Environment, SavedConnection},
    edits::{self, PendingEdits},
    grid::{GridColumn, GridEvent, ResultGrid, Sort},
    panel::{DatabasePanel, segment, sql_line, sql_tag},
    session::{ServerError, Session},
    statements::Risk,
    write_guard::WriteGuard,
};
use editor::Editor;
use gpui::{
    AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Subscription, Task, WeakEntity, px,
};
use i18n::t;
use std::{sync::Arc, time::Duration};
use ui::{
    Divider, KeyBinding, StatusTone, Tooltip, button_group, group_icon_button, prelude::*,
    primary_button, secondary_button, segmented_control, status_chip,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

const PAGE_SIZE: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Data,
    Structure,
    Indexes,
    Relations,
    Ddl,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Data,
        Self::Structure,
        Self::Indexes,
        Self::Relations,
        Self::Ddl,
    ];

    /// The name the debug hook selects the tab by, independent of the interface language.
    fn debug_name(self) -> &'static str {
        match self {
            Self::Data => "Dados",
            Self::Structure => "Estrutura",
            Self::Indexes => "Índices",
            Self::Relations => "Relações",
            Self::Ddl => "DDL",
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Data => t!("database_client.table.tab_data"),
            Self::Structure => t!("database_client.table.tab_structure"),
            Self::Indexes => t!("database_client.table.tab_indexes"),
            Self::Relations => t!("database_client.table.tab_relations"),
            Self::Ddl => t!("database_client.table.tab_ddl"),
        }
    }
}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(SharedString),
}

struct Page {
    elapsed: Duration,
    shown: usize,
    has_more: bool,
}

struct Structure {
    indexes: Vec<IndexInfo>,
    foreign_keys: Vec<ForeignKey>,
    ddl: String,
}

pub struct TableView {
    workspace: WeakEntity<Workspace>,
    connection: SavedConnection,
    panel_session: Arc<Session>,
    write_session: Option<Arc<Session>>,
    pending: PendingEdits,
    show_sql: bool,
    applying: bool,
    notice: Option<(SharedString, Color)>,
    apply_task: Task<()>,
    schema: String,
    name: String,
    kind: RelationKind,
    estimated_rows: Option<i64>,
    focus_handle: FocusHandle,
    tab: Tab,
    where_input: Entity<Editor>,
    order_input: Entity<Editor>,
    /// Table, indexes and TOAST, shown next to the row estimate.
    total_size: Option<String>,
    grid: Entity<ResultGrid>,
    session: Option<Arc<Session>>,
    columns: Vec<ColumnInfo>,
    page: usize,
    sort: Option<Sort>,
    data: Load<Page>,
    structure: Option<Load<Structure>>,
    data_task: Task<()>,
    structure_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

fn describe_error(error: &anyhow::Error) -> SharedString {
    match error.downcast_ref::<ServerError>() {
        Some(server_error) => match &server_error.hint {
            Some(hint) => format!("{server_error}\nHINT: {hint}").into(),
            None => server_error.to_string().into(),
        },
        None => format!("{error:#}").into(),
    }
}

impl TableView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        workspace: WeakEntity<Workspace>,
        connection: SavedConnection,
        panel_session: Arc<Session>,
        schema: String,
        name: String,
        kind: RelationKind,
        estimated_rows: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_editor = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let font_family = theme::theme_settings(cx).buffer_font(cx).family.clone();
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text_style_refinement(gpui::TextStyleRefinement {
                    font_family: Some(font_family),
                    font_size: Some(rems(0.75).into()),
                    ..Default::default()
                });
                editor
            })
        };
        let where_input = filter_editor("created_at > now() - interval '7 days'", window, cx);
        let order_input = filter_editor("id desc", window, cx);
        let grid = cx.new(|cx| ResultGrid::new(true, cx));
        let subscriptions = vec![
            cx.subscribe_in(&grid, window, |this, _, event, window, cx| match event {
                GridEvent::SortRequested(column) => this.sort_by(*column, cx),
                GridEvent::CellEdited { row, column, value } => {
                    this.cell_edited(*row, *column, value.clone(), cx)
                }
                GridEvent::OpenValue { column, value } => crate::grid::open_value(
                    this.workspace.clone(),
                    column.clone(),
                    value.clone(),
                    window,
                    cx,
                ),
            }),
        ];
        let mut this = Self {
            workspace,
            connection,
            panel_session: panel_session.clone(),
            write_session: None,
            pending: PendingEdits::default(),
            show_sql: false,
            applying: false,
            notice: None,
            apply_task: Task::ready(()),
            schema,
            name,
            kind,
            estimated_rows,
            focus_handle: cx.focus_handle(),
            tab: Tab::Data,
            where_input,
            order_input,
            total_size: None,
            grid,
            session: None,
            columns: Vec::new(),
            page: 0,
            sort: None,
            data: Load::Loading,
            structure: None,
            data_task: Task::ready(()),
            structure_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.open_session(panel_session, cx);
        this
    }

    /// `(connection id, schema, name)`, for the dock to mark the table it shows.
    pub(crate) fn relation(&self) -> (&str, &str, &str) {
        (&self.connection.id, &self.schema, &self.name)
    }

    pub(crate) fn has_pending_edits(&self) -> bool {
        !self.pending.is_empty()
    }

    fn pending_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ItemEvent::UpdateTab);
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                    panel.update(cx, |_, cx| cx.notify());
                }
            })
            .log_err();
    }

    fn open_session(&mut self, panel_session: Arc<Session>, cx: &mut Context<Self>) {
        let schema = self.schema.clone();
        let name = self.name.clone();
        self.data_task = cx.spawn(async move |this, cx| {
            let result = async {
                let session = Arc::new(panel_session.connect_read_only().await?);
                let columns = catalog::list_columns(&session, &schema, &name).await?;
                let (estimate, total_size) = catalog::relation_stats(&session, &schema, &name)
                    .await
                    .log_err()
                    .unwrap_or_default();
                anyhow::Ok((session, columns, estimate, total_size))
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok((session, columns, estimate, total_size)) => {
                    this.session = Some(session);
                    this.total_size = total_size;
                    if this.estimated_rows.is_none() {
                        this.estimated_rows = estimate;
                    }
                    this.columns = columns;
                    let editable = this.can_edit();
                    this.grid
                        .update(cx, |grid, cx| grid.set_editable(editable, cx));
                    this.load_page(cx);
                }
                Err(error) => {
                    this.data = Load::Failed(describe_error(&error));
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn order_clause(&self, cx: &App) -> Option<String> {
        if let Some(sort) = self.sort
            && let Some(column) = self.columns.get(sort.column)
        {
            return Some(format!(
                "{} {}",
                quote_ident(&column.name),
                if sort.descending { "desc" } else { "asc" }
            ));
        }
        let typed = self.order_input.read(cx).text(cx).trim().to_owned();
        if !typed.is_empty() {
            return Some(typed);
        }
        // Without an order Postgres may return pages that overlap; the primary key keeps them
        // stable.
        let primary_key = self
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| quote_ident(&column.name))
            .collect::<Vec<_>>();
        (!primary_key.is_empty()).then(|| primary_key.join(", "))
    }

    fn page_sql(&self, cx: &App) -> String {
        let mut sql = format!("select * from {}", qualified_name(&self.schema, &self.name));
        let filter = self.where_input.read(cx).text(cx).trim().to_owned();
        if !filter.is_empty() {
            sql.push_str(&format!(" where {filter}"));
        }
        if let Some(order) = self.order_clause(cx) {
            sql.push_str(&format!(" order by {order}"));
        }
        // One extra row tells whether there is a next page.
        sql.push_str(&format!(
            " limit {} offset {}",
            PAGE_SIZE + 1,
            self.page * PAGE_SIZE
        ));
        sql
    }

    fn load_page(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let sql = self.page_sql(cx);
        let first_row_number = self.page * PAGE_SIZE + 1;
        self.data = Load::Loading;
        cx.notify();
        self.data_task = cx.spawn(async move |this, cx| {
            let result = session.run(&sql, PAGE_SIZE + 2).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(outcome) => {
                        let Some(mut result_set) = outcome.result_sets.into_iter().next() else {
                            this.data = Load::Failed(t!("database_client.table.no_rows_returned"));
                            cx.notify();
                            return;
                        };
                        let has_more = result_set.rows.len() > PAGE_SIZE;
                        result_set.rows.truncate(PAGE_SIZE);
                        let shown = result_set.rows.len();
                        let columns = result_set
                            .columns
                            .into_iter()
                            .map(|column| {
                                // The catalog spells types the way the dock does (`varchar(14)`).
                                let type_name = this
                                    .columns
                                    .iter()
                                    .find(|info| info.name == column.name)
                                    .map(|info| info.type_name.clone())
                                    .or(column.type_name);
                                GridColumn {
                                    name: column.name,
                                    type_name,
                                }
                            })
                            .collect();
                        this.grid.update(cx, |grid, cx| {
                            grid.set_data(columns, result_set.rows, first_row_number, cx)
                        });
                        this.data = Load::Loaded(Page {
                            elapsed: outcome.elapsed,
                            shown,
                            has_more,
                        });
                    }
                    Err(error) => this.data = Load::Failed(describe_error(&error)),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn can_edit(&self) -> bool {
        matches!(
            self.kind,
            RelationKind::Table | RelationKind::PartitionedTable
        ) && !self.connection.read_only
            && self.columns.iter().any(|column| column.primary_key)
    }

    /// Paging, sorting or filtering would drop the edited page, so they wait.
    fn blocked_by_pending(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        self.notice = Some((t!("database_client.table.pending_blocks"), Color::Warning));
        cx.notify();
        true
    }

    fn cell_edited(
        &mut self,
        row: usize,
        column: usize,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let original = self
            .grid
            .read(cx)
            .rows()
            .get(row)
            .and_then(|values| values.get(column))
            .cloned()
            .flatten();
        if original == value {
            self.pending.remove(&(row, column));
        } else {
            self.pending.insert((row, column), value);
        }
        self.notice = None;
        let pending = self.pending.clone();
        self.grid
            .update(cx, |grid, cx| grid.set_pending(pending, cx));
        self.pending_changed(cx);
        cx.notify();
    }

    fn discard_edits(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.show_sql = false;
        self.notice = None;
        self.grid
            .update(cx, |grid, cx| grid.set_pending(PendingEdits::default(), cx));
        self.pending_changed(cx);
        cx.notify();
    }

    fn pending_updates(&self, cx: &App) -> Result<Vec<edits::RowUpdate>, String> {
        let grid = self.grid.read(cx);
        let names = grid
            .columns()
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        let primary_key = names
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                self.columns
                    .iter()
                    .any(|column| column.primary_key && &column.name == *name)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        edits::row_updates(
            &self.schema,
            &self.name,
            &names,
            &primary_key,
            grid.rows(),
            &self.pending,
        )
    }

    fn apply_edits(&mut self, _: &ApplyEdits, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_empty() || self.applying {
            return;
        }
        let updates = match self.pending_updates(cx) {
            Ok(updates) => updates,
            Err(error) => {
                self.notice = Some((error.into(), Color::Error));
                cx.notify();
                return;
            }
        };
        if updates.is_empty() {
            self.discard_edits(cx);
            return;
        }
        if self.connection.environment != Environment::Prod {
            self.write(updates, cx);
            return;
        }
        let this = cx.weak_entity();
        let connection = self.connection.clone();
        let preview = edits::preview(&updates);
        let opened = self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                WriteGuard::new(
                    connection,
                    preview,
                    Risk::ProductionWrite,
                    true,
                    Some(
                        "As alterações rodam numa transação: se alguma linha mudou desde que a \
                         página carregou, nada é gravado."
                            .into(),
                    ),
                    None,
                    Box::new(move |_, _window, cx| {
                        this.update(cx, |this, cx| this.write(updates, cx))
                            .log_err();
                    }),
                    window,
                    cx,
                )
            });
        });
        if opened.is_err() {
            self.notice = Some((t!("database_client.table.window_closed"), Color::Error));
            cx.notify();
        }
    }

    /// Runs every update in one transaction and rolls back if any of them doesn't match
    /// exactly one row.
    fn write(&mut self, updates: Vec<edits::RowUpdate>, cx: &mut Context<Self>) {
        self.applying = true;
        self.notice = None;
        cx.notify();
        let write_session = self.write_session.clone();
        let panel_session = self.panel_session.clone();
        let first_row_number = self.page * PAGE_SIZE + 1;
        self.apply_task = cx.spawn(async move |this, cx| {
            let result = async {
                let session = match write_session {
                    Some(session) => session,
                    None => Arc::new(panel_session.connect_again().await?),
                };
                this.update(cx, |this, _| this.write_session = Some(session.clone()))?;
                session.run("begin", 1).await?;
                for update in &updates {
                    let outcome = session.run(&update.sql, 1).await;
                    let changed = match outcome {
                        Ok(outcome) => outcome
                            .result_sets
                            .last()
                            .and_then(|result_set| result_set.rows_affected),
                        Err(error) => {
                            session.run("rollback", 1).await.log_err();
                            return Err(error);
                        }
                    };
                    if changed != Some(1) {
                        session.run("rollback", 1).await.log_err();
                        anyhow::bail!(
                            "A linha {} mudou ou sumiu desde que a página carregou. Nada foi \
                             gravado; recarregue a página para ver o valor atual.",
                            first_row_number + update.row
                        );
                    }
                }
                session.run("commit", 1).await?;
                anyhow::Ok(updates.len())
            }
            .await;
            this.update(cx, |this, cx| {
                this.applying = false;
                match result {
                    Ok(count) => {
                        this.pending.clear();
                        this.show_sql = false;
                        this.grid
                            .update(cx, |grid, cx| grid.set_pending(PendingEdits::default(), cx));
                        this.pending_changed(cx);
                        this.notice = Some((
                            if count == 1 {
                                t!("database_client.table.rows_updated.one")
                            } else {
                                t!("database_client.table.rows_updated.other", count = count)
                            },
                            Color::Success,
                        ));
                        this.load_page(cx);
                    }
                    Err(error) => {
                        this.notice = Some((describe_error(&error), Color::Error));
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn apply_filter(&mut self, _: &ApplyTableFilter, _window: &mut Window, cx: &mut Context<Self>) {
        if self.blocked_by_pending(cx) {
            return;
        }
        self.page = 0;
        self.sort = None;
        self.grid.update(cx, |grid, cx| grid.set_sort(None, cx));
        self.load_page(cx);
    }

    /// Ascending, then descending, then back to the default order.
    fn sort_by(&mut self, column: usize, cx: &mut Context<Self>) {
        if self.blocked_by_pending(cx) {
            return;
        }
        self.sort = match self.sort {
            Some(sort) if sort.column == column && !sort.descending => Some(Sort {
                column,
                descending: true,
            }),
            Some(sort) if sort.column == column => None,
            _ => Some(Sort {
                column,
                descending: false,
            }),
        };
        let sort = self.sort;
        self.grid.update(cx, |grid, cx| grid.set_sort(sort, cx));
        self.page = 0;
        self.load_page(cx);
    }

    fn go_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if self.blocked_by_pending(cx) {
            return;
        }
        self.page = page;
        self.load_page(cx);
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab != Tab::Data && !matches!(self.structure, Some(Load::Loaded(_) | Load::Loading)) {
            self.load_structure(cx);
        }
        cx.notify();
    }

    /// For the debug hook: `edit` fakes two cell edits and opens the SQL preview.
    pub(crate) fn select_tab_named(&mut self, label: &str, cx: &mut Context<Self>) {
        if label == "edit" {
            let email = self
                .grid
                .read(cx)
                .columns()
                .iter()
                .position(|column| column.name == "email");
            if let Some(email) = email {
                self.cell_edited(1, email, Some("ana.nova@exemplo.com".to_owned()), cx);
                self.cell_edited(4, email, None, cx);
                self.show_sql = true;
            }
            return;
        }
        if let Some(tab) = Tab::ALL
            .into_iter()
            .find(|tab| tab.debug_name().eq_ignore_ascii_case(label))
        {
            self.select_tab(tab, cx);
        }
    }

    fn load_structure(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let schema = self.schema.clone();
        let name = self.name.clone();
        let columns = self.columns.clone();
        self.structure = Some(Load::Loading);
        self.structure_task = cx.spawn(async move |this, cx| {
            let result = async {
                let indexes = catalog::list_indexes(&session, &schema, &name).await?;
                let foreign_keys = catalog::list_foreign_keys(&session, &schema, &name).await?;
                let constraints = catalog::list_constraints(&session, &schema, &name).await?;
                let ddl = catalog::table_ddl(&schema, &name, &columns, &constraints, &indexes);
                anyhow::Ok(Structure {
                    indexes,
                    foreign_keys,
                    ddl,
                })
            }
            .await;
            this.update(cx, |this, cx| {
                this.structure = Some(match result {
                    Ok(structure) => Load::Loaded(structure),
                    Err(error) => Load::Failed(describe_error(&error)),
                });
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let is_view = matches!(
            self.kind,
            RelationKind::View | RelationKind::MaterializedView
        );
        let facts = [
            self.estimated_rows.map(|rows| {
                t!(
                    "database_client.table.rows",
                    count = crate::panel::format_count(rows)
                )
                .to_string()
            }),
            self.total_size.clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        h_flex()
            .h(px(52.))
            .flex_none()
            .px_3()
            .gap_2()
            .child(status_chip(
                Some(if is_view {
                    IconName::Eye
                } else {
                    IconName::Database
                }),
                format!("{} · {} · {}", self.connection.name, self.schema, self.name),
                Color::Default,
                cx,
            ))
            .when(is_view, |this| {
                this.child(
                    Label::new(t!("database_client.table.view_badge"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(!facts.is_empty(), |this| {
                this.child(
                    div().min_w_0().child(
                        Label::new(facts)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
            })
            .child(div().flex_1())
            .child(segmented_control(cx).children(Tab::ALL.map(|tab| {
                segment(
                    ("db-table-tab", tab as usize),
                    tab.label(),
                    tab == self.tab,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
            })))
            .into_any_element()
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .key_context("DatabaseTableFilter")
            .h(px(40.))
            .flex_none()
            .px_3()
            .gap_2p5()
            .border_t_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(sql_tag("WHERE", cx))
            .child(div().flex_1().min_w_0().child(self.where_input.clone()))
            .child(Divider::vertical().h(px(20.)))
            .child(sql_tag("ORDER BY", cx))
            .child(div().w(px(180.)).child(self.order_input.clone()))
            .child(Divider::vertical().h(px(20.)))
            .child(status_chip(
                None,
                t!("database_client.query.limit_chip", limit = PAGE_SIZE),
                Color::Muted,
                cx,
            ))
            .into_any_element()
    }

    fn render_pending_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.pending.len();
        if count == 0 {
            return None;
        }
        let warning = Color::Warning.color(cx);
        Some(
            h_flex()
                .h(px(44.))
                .flex_none()
                .px_3()
                .gap_3()
                .bg(warning.opacity(0.08))
                .border_t_1()
                .border_color(warning.opacity(0.3))
                .child(StatusTone::Warning.dot(cx))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            Label::new(if count == 1 {
                                t!("database_client.table.pending_changes.one")
                            } else {
                                t!("database_client.table.pending_changes.other", count = count)
                            })
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Warning),
                        )
                        .child(
                            Label::new(format!(
                                "· {}",
                                t!("database_client.table.pending_bar_note")
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                )
                .child(
                    Button::new("db-edits-sql", t!("database_client.table.view_sql"))
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .color(if self.show_sql {
                            Color::Accent
                        } else {
                            Color::Default
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_sql = !this.show_sql;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("db-edits-discard", t!("database_client.table.discard"))
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .disabled(self.applying)
                        .on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))),
                )
                .child(
                    primary_button(
                        "db-edits-apply",
                        None,
                        if self.applying {
                            t!("database_client.table.applying")
                        } else {
                            t!("database_client.table.apply")
                        },
                        Some(KeyBinding::for_action_in(
                            &ApplyEdits,
                            &self.focus_handle,
                            cx,
                        )),
                        cx,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.apply_edits(&ApplyEdits, window, cx)
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let filtered = !self.where_input.read(cx).text(cx).trim().is_empty();
        let (summary, detail, range, has_more) = match &self.data {
            Load::Loaded(page) => {
                let first = self.page * PAGE_SIZE + 1;
                let last = self.page * PAGE_SIZE + page.shown;
                let summary = match self.estimated_rows {
                    Some(total) if !filtered && page.shown > 0 => t!(
                        "database_client.table.rows_of_total",
                        shown = page.shown,
                        total = crate::panel::format_count(total)
                    )
                    .to_string(),
                    _ if page.shown == 1 => t!("database_client.query.rows.one").to_string(),
                    _ => t!("database_client.query.rows.other", count = page.shown).to_string(),
                };
                let mode = if self.can_edit() {
                    t!("database_client.table.double_click_edits")
                } else if self.columns.iter().any(|column| column.primary_key) {
                    t!("database_client.panel.read_only")
                } else {
                    t!("database_client.table.read_only_no_primary_key")
                };
                let order = match self.default_order_label(cx) {
                    Some(order) => format!(" · {order}"),
                    None => String::new(),
                };
                (
                    summary,
                    format!("· {} ms{order} · {mode}", page.elapsed.as_millis()),
                    if page.shown == 0 {
                        "—".to_owned()
                    } else {
                        format!("{first}–{last}")
                    },
                    page.has_more,
                )
            }
            Load::Loading => (
                t!("database_client.table.loading_lowercase").to_string(),
                String::new(),
                "…".to_owned(),
                false,
            ),
            Load::Failed(_) => (String::new(), String::new(), "—".to_owned(), false),
        };
        let page = self.page;
        h_flex()
            .h(px(56.))
            .flex_none()
            .px_3()
            .gap_3()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(summary).size(LabelSize::Small))
            .child(
                Label::new(detail)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(button_group(
                [
                    group_icon_button("db-table-previous", IconName::ChevronLeft, 24.)
                        .disabled(page == 0)
                        .tooltip(Tooltip::text(t!("database_client.table.previous_page")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.go_to_page(page.saturating_sub(1), cx)
                        }))
                        .into_any_element(),
                    div()
                        .px_2()
                        .child(Label::new(range).size(LabelSize::Small).buffer_font(cx))
                        .into_any_element(),
                    group_icon_button("db-table-next", IconName::ChevronRight, 24.)
                        .disabled(!has_more)
                        .tooltip(Tooltip::text(t!("database_client.table.next_page")))
                        .on_click(cx.listener(move |this, _, _, cx| this.go_to_page(page + 1, cx)))
                        .into_any_element(),
                ],
                cx,
            ))
            .child(div().flex_1())
            .when_some(self.notice.clone(), |this, (notice, color)| {
                this.child(
                    div().min_w_0().child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(color)
                            .truncate(),
                    ),
                )
            })
            .into_any_element()
    }

    /// The order the page falls back to, when nothing was typed or clicked.
    fn default_order_label(&self, cx: &App) -> Option<String> {
        if self.sort.is_some() || !self.order_input.read(cx).text(cx).trim().is_empty() {
            return None;
        }
        let primary_key = self
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();
        (!primary_key.is_empty()).then(|| {
            t!(
                "database_client.table.ordered_by",
                columns = primary_key.join(", ")
            )
            .to_string()
        })
    }

    fn render_message(&self, message: SharedString, error: bool, cx: &App) -> AnyElement {
        let color = if error { Color::Error } else { Color::Muted };
        v_flex()
            .m_3()
            .p_3()
            .gap_1()
            .rounded_lg()
            .when(error, |this| {
                this.bg(Color::Error.color(cx).opacity(0.06))
                    .border_1()
                    .border_color(Color::Error.color(cx).opacity(0.4))
            })
            .children(message.split('\n').map(|line| {
                Label::new(line.to_owned())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .color(if line.starts_with("HINT") {
                        Color::Warning
                    } else {
                        color
                    })
            }))
            .into_any_element()
    }

    fn render_data(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.render_filters(cx))
            .child(match &self.data {
                Load::Failed(error) => self.render_message(error.clone(), true, cx),
                Load::Loading if self.grid.read(cx).row_count() == 0 => {
                    self.render_message(t!("database_client.table.loading"), false, cx)
                }
                _ => div()
                    .flex_1()
                    .min_h_0()
                    .child(self.grid.clone())
                    .into_any_element(),
            })
            .children(self.render_pending_bar(cx))
            .child(self.render_footer(cx))
            .children(self.render_sql_preview(cx))
            .into_any_element()
    }

    /// "Ver SQL": the updates that "Aplicar" will run, floating over the grid like the board.
    fn render_sql_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_sql || self.pending.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let preview = self
            .pending_updates(cx)
            .map(|updates| (edits::preview(&updates), updates.len()));
        let (text, updates) = match preview {
            Ok(preview) => preview,
            Err(error) => (error, 0),
        };
        let copy = text.clone();
        let open = text.clone();
        Some(
            v_flex()
                .id("db-edits-preview")
                .occlude()
                .absolute()
                .right_3()
                .bottom(px(64.))
                .w(px(516.))
                .max_h(px(420.))
                .rounded_lg()
                .border_1()
                .border_color(colors.border_selected)
                .bg(colors.elevated_surface_background)
                .shadow_lg()
                .child(
                    h_flex()
                        .px_3p5()
                        .py_3()
                        .gap_3()
                        .child(
                            Label::new(t!("database_client.table.sql_to_run"))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(if updates == 1 {
                                t!(
                                    "database_client.table.updates_summary.one",
                                    connection = self.connection.name,
                                    environment = self.connection.environment.label()
                                )
                            } else {
                                t!(
                                    "database_client.table.updates_summary.other",
                                    count = updates,
                                    connection = self.connection.name,
                                    environment = self.connection.environment.label()
                                )
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    v_flex()
                        .id("db-edits-preview-lines")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_1p5()
                        .py_2p5()
                        .children(text.lines().enumerate().map(|(index, line)| {
                            let guard = line.contains("is not distinct from");
                            h_flex()
                                .h(px(21.))
                                .px_2()
                                .gap_3()
                                .overflow_hidden()
                                .rounded_sm()
                                .when(guard, |this| this.bg(Color::Warning.color(cx).opacity(0.1)))
                                .child(
                                    div().w(px(16.)).flex().justify_end().child(
                                        Label::new((index + 1).to_string())
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    ),
                                )
                                .child(sql_line(line, cx))
                        })),
                )
                .child(
                    h_flex()
                        .px_3p5()
                        .py_2p5()
                        .gap_2()
                        .items_start()
                        .border_t_1()
                        .border_color(colors.border)
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(t!("database_client.table.amber_lines_note"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        ),
                )
                .child(
                    h_flex()
                        .px_3p5()
                        .pb_3()
                        .gap_4()
                        .child(
                            Button::new("db-copy-edits-sql", t!("database_client.grid.copy"))
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                }),
                        )
                        .child(
                            Button::new(
                                "db-open-edits-sql",
                                t!("database_client.table.open_in_sql_file"),
                            )
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.open_in_query(
                                        t!("database_client.table.pending_changes_header")
                                            .to_string(),
                                        open.clone(),
                                        window,
                                        cx,
                                    )
                                },
                            )),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("db-close-edits-sql", t!("database_client.table.close"))
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_sql = false;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// SQL that changes the table goes to a new `.sql` file instead of running from here, so it
    /// passes through the editor and its write confirmation.
    fn open_in_query(
        &mut self,
        what: String,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let header = format!(
            "-- {} · {} · {}.{}: {what}",
            self.connection.name,
            self.connection.environment.label(),
            self.schema,
            self.name
        );
        let panel = self
            .workspace
            .read_with(cx, |workspace, cx| workspace.panel::<DatabasePanel>(cx))
            .ok()
            .flatten();
        let result = match panel {
            Some(panel) => panel.update(cx, |panel, cx| {
                panel.open_query_with(header, sql, false, window, cx)
            }),
            None => Err(anyhow::anyhow!(
                "{}",
                t!("database_client.connect.error_panel_closed")
            )),
        };
        if let Err(error) = result {
            self.notice = Some((format!("{error:#}").into(), Color::Error));
            cx.notify();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_section(
        &self,
        title: SharedString,
        count: usize,
        headers: &[(SharedString, f32)],
        rows: Vec<Vec<Option<(String, Color, bool)>>>,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let cell = |index: usize| {
            let width = headers.get(index).map(|(_, width)| *width).unwrap_or(0.);
            let cell = h_flex().h_full().px_3().min_w_0();
            if width > 0. {
                cell.w(px(width)).flex_none()
            } else {
                cell.flex_1()
            }
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(title).weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                ui::card(cx)
                    .overflow_hidden()
                    .child(h_flex().h(px(32.)).bg(colors.element_background).children(
                        headers.iter().enumerate().map(|(index, (header, _))| {
                            cell(index).child(
                                Label::new(header.clone())
                                    .size(LabelSize::XSmall)
                                    .weight(FontWeight::MEDIUM)
                                    .color(Color::Muted),
                            )
                        }),
                    ))
                    .children(rows.into_iter().map(|values| {
                        h_flex()
                            .h(px(28.))
                            .border_t_1()
                            .border_color(colors.border)
                            .children(values.into_iter().enumerate().map(|(index, value)| {
                                cell(index).children(value.map(|(text, color, mono)| {
                                    Label::new(text)
                                        .size(LabelSize::Small)
                                        .color(color)
                                        .when(mono, |label| label.buffer_font(cx))
                                        .truncate()
                                }))
                            }))
                    })),
            )
            .into_any_element()
    }

    fn columns_section(&self, structure: Option<&Structure>, cx: &App) -> AnyElement {
        let mono = |text: String, color: Color| Some((text, color, true));
        let rows = self
            .columns
            .iter()
            .map(|column| {
                let unique = t!("database_client.table.unique").to_string();
                let mut restrictions = Vec::new();
                if column.primary_key {
                    restrictions.push("PK".to_owned());
                }
                for index in structure
                    .map(|structure| structure.indexes.as_slice())
                    .unwrap_or(&[])
                {
                    if index.primary {
                        continue;
                    }
                    let Some(expression) = index_columns(&index.definition) else {
                        continue;
                    };
                    let expression = expression.trim_start_matches('(').trim_end_matches(')');
                    if expression.split(',').count() > 1 || !mentions(expression, &column.name) {
                        continue;
                    }
                    restrictions.push(if index.unique {
                        format!("{unique} · {}", index.name)
                    } else if expression == column.name {
                        t!("database_client.table.index").to_string()
                    } else if expression.eq_ignore_ascii_case(&format!("{} desc", column.name)) {
                        t!("database_client.table.index_desc").to_string()
                    } else {
                        t!("database_client.table.index_by", expression = expression).to_string()
                    });
                }
                if column.unique
                    && !restrictions
                        .iter()
                        .any(|restriction| restriction.starts_with(&unique))
                {
                    restrictions.push(unique);
                }
                let restriction_color = if column.primary_key {
                    Color::Warning
                } else {
                    Color::Default
                };
                vec![
                    mono(column.name.clone(), Color::Default),
                    mono(column.type_name.clone(), Color::Accent),
                    mono(
                        if column.not_null {
                            t!("database_client.table.no").to_string()
                        } else {
                            t!("database_client.table.yes").to_string()
                        },
                        Color::Muted,
                    ),
                    match &column.default {
                        Some(default) => mono(default.clone(), Color::Warning),
                        None => mono("—".to_owned(), Color::Muted),
                    },
                    (!restrictions.is_empty()).then(|| {
                        (
                            restrictions.join(" · "),
                            restriction_color,
                            column.primary_key && restrictions.len() == 1,
                        )
                    }),
                ]
            })
            .collect();
        self.render_section(
            t!("database_client.table.columns"),
            self.columns.len(),
            &[
                (t!("database_client.table.column"), 180.),
                (t!("database_client.table.type"), 170.),
                ("Null".into(), 70.),
                ("Default".into(), 260.),
                (t!("database_client.table.constraints"), 0.),
            ],
            rows,
            cx,
        )
    }

    fn indexes_section(&self, structure: &Structure, cx: &App) -> AnyElement {
        let rows = structure
            .indexes
            .iter()
            .map(|index| {
                let unused = index.scans == Some(0) && !index.primary;
                let method = index
                    .definition
                    .split_once(" USING ")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .unwrap_or("btree")
                    .to_owned();
                let kind = if index.primary {
                    format!("{method} · PK")
                } else if index.unique {
                    format!("{method} · {}", t!("database_client.table.unique"))
                } else {
                    method
                };
                vec![
                    Some((index.name.clone(), Color::Default, true)),
                    Some((
                        index_columns(&index.definition)
                            .unwrap_or(&index.definition)
                            .to_owned(),
                        Color::Muted,
                        true,
                    )),
                    Some((kind, Color::Accent, true)),
                    Some((index.size.clone(), Color::Muted, true)),
                    Some((
                        match index.scans {
                            Some(0) if unused => {
                                t!("database_client.table.unused_index").to_string()
                            }
                            Some(scans) => format!("{} scans", format_scans(scans)),
                            None => t!("database_client.table.no_statistics").to_string(),
                        },
                        if unused {
                            Color::Warning
                        } else {
                            Color::Default
                        },
                        false,
                    )),
                ]
            })
            .collect();
        self.render_section(
            t!("database_client.table.tab_indexes"),
            structure.indexes.len(),
            &[
                (t!("database_client.table.name"), 260.),
                (t!("database_client.table.columns"), 240.),
                (t!("database_client.table.type"), 140.),
                (t!("database_client.table.size"), 100.),
                (t!("database_client.table.usage_since_reset"), 0.),
            ],
            rows,
            cx,
        )
    }

    fn relations_section(&self, structure: &Structure, cx: &App) -> AnyElement {
        let rows = structure
            .foreign_keys
            .iter()
            .map(|foreign_key| {
                vec![
                    Some((
                        match foreign_key.direction {
                            ForeignKeyDirection::References => {
                                t!("database_client.table.references")
                            }
                            ForeignKeyDirection::ReferencedBy => {
                                t!("database_client.table.referenced_by")
                            }
                        }
                        .to_string(),
                        Color::Muted,
                        true,
                    )),
                    Some((foreign_key.other_table.clone(), Color::Default, true)),
                    Some((
                        foreign_key_columns(&foreign_key.definition)
                            .unwrap_or_else(|| foreign_key.definition.clone()),
                        Color::Default,
                        true,
                    )),
                    Some((
                        foreign_key.on_delete.clone(),
                        if foreign_key.on_delete == "CASCADE" {
                            Color::Error
                        } else {
                            Color::Accent
                        },
                        true,
                    )),
                    foreign_key
                        .other_rows
                        .map(|rows| (crate::panel::format_count(rows), Color::Default, false)),
                ]
            })
            .collect();
        self.render_section(
            t!("database_client.table.tab_relations"),
            structure.foreign_keys.len(),
            &[
                (t!("database_client.table.direction"), 150.),
                (t!("database_client.table.table"), 220.),
                (t!("database_client.table.column"), 200.),
                (t!("database_client.table.on_delete"), 160.),
                (t!("database_client.table.row_count"), 0.),
            ],
            rows,
            cx,
        )
    }

    fn render_structure_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let table = qualified_name(&self.schema, &self.name);
        let new_column = format!("alter table {table}\n    add column nova_coluna text;");
        let new_index = format!(
            "create index concurrently {}\n    on {table} (coluna);",
            quote_ident(&format!("{}_coluna_idx", self.name))
        );
        v_flex()
            .gap_3()
            .pt_4()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(t!("database_client.table.structure_changes_note"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        secondary_button(
                            "db-open-ddl",
                            Some((IconName::FileCode, Color::Muted)),
                            t!("database_client.table.open_ddl"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.select_tab(Tab::Ddl, cx))),
                    )
                    .child(
                        secondary_button(
                            "db-new-column",
                            Some((IconName::Plus, Color::Muted)),
                            t!("database_client.table.new_column"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_in_query(
                                    t!("database_client.table.new_column_header").to_string(),
                                    new_column.clone(),
                                    window,
                                    cx,
                                )
                            },
                        )),
                    )
                    .child(
                        secondary_button(
                            "db-new-index",
                            Some((IconName::Plus, Color::Muted)),
                            t!("database_client.table.new_index"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_in_query(
                                    t!("database_client.table.new_index_header").to_string(),
                                    new_index.clone(),
                                    window,
                                    cx,
                                )
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_structure_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let structure = match &self.structure {
            Some(Load::Loaded(structure)) => Some(structure),
            _ => None,
        };
        let pending = match (&self.tab, &self.structure) {
            (Tab::Structure, _) => None,
            (_, None | Some(Load::Loading)) => {
                Some(self.render_message(t!("database_client.table.reading_catalog"), false, cx))
            }
            (_, Some(Load::Failed(error))) => Some(self.render_message(error.clone(), true, cx)),
            _ => None,
        };
        let content = if let Some(pending) = pending {
            pending
        } else {
            match (self.tab, structure) {
                (Tab::Structure, structure) => v_flex()
                    .gap_6()
                    .child(self.columns_section(structure, cx))
                    .when_some(structure, |this, structure| {
                        this.when(!structure.indexes.is_empty(), |this| {
                            this.child(self.indexes_section(structure, cx))
                        })
                        .when(!structure.foreign_keys.is_empty(), |this| {
                            this.child(self.relations_section(structure, cx))
                        })
                    })
                    .when_some(
                        match &self.structure {
                            Some(Load::Failed(error)) => Some(error.clone()),
                            _ => None,
                        },
                        |this, error| this.child(self.render_message(error, true, cx)),
                    )
                    .child(self.render_structure_actions(cx))
                    .into_any_element(),
                (Tab::Indexes, Some(structure)) if structure.indexes.is_empty() => {
                    self.render_message(t!("database_client.table.no_indexes"), false, cx)
                }
                (Tab::Indexes, Some(structure)) => self.indexes_section(structure, cx),
                (Tab::Relations, Some(structure)) if structure.foreign_keys.is_empty() => {
                    self.render_message(t!("database_client.table.no_foreign_keys"), false, cx)
                }
                (Tab::Relations, Some(structure)) => self.relations_section(structure, cx),
                (Tab::Ddl, Some(structure)) => {
                    let ddl = structure.ddl.clone();
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(t!("database_client.table.ddl_note"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1())
                                .child(
                                    secondary_button(
                                        "db-copy-ddl",
                                        Some((IconName::Copy, Color::Muted)),
                                        t!("database_client.grid.copy"),
                                        true,
                                        cx,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                ddl.clone(),
                                            ))
                                        },
                                    ),
                                ),
                        )
                        .child(
                            v_flex()
                                .p_3()
                                .rounded_lg()
                                .bg(cx.theme().colors().editor_background)
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .children(structure.ddl.lines().map(|line| sql_line(line, cx))),
                        )
                        .into_any_element()
                }
                _ => div().into_any_element(),
            }
        };
        v_flex()
            .id("db-table-structure")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pt_1()
            .pb_4()
            .child(content)
            .into_any_element()
    }
}

/// `CREATE INDEX … USING btree (lower(email))` → `(lower(email))`.
fn index_columns(definition: &str) -> Option<&str> {
    let (_, rest) = definition.split_once(" USING ")?;
    let start = rest.find('(')?;
    let end = rest.rfind(')')?;
    (start < end).then(|| &rest[start..=end])
}

/// Whether `expression` refers to `column` as a whole identifier, not inside a longer name.
fn mentions(expression: &str, column: &str) -> bool {
    expression
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word == column)
}

/// `FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE` → `user_id → users.id`.
fn foreign_key_columns(definition: &str) -> Option<String> {
    let rest = definition.strip_prefix("FOREIGN KEY (")?;
    let (local, rest) = rest.split_once(')')?;
    let rest = rest.trim_start().strip_prefix("REFERENCES ")?;
    let (table, rest) = rest.split_once('(')?;
    let (remote, _) = rest.split_once(')')?;
    let table = table.rsplit('.').next().unwrap_or(table);
    Some(format!("{local} → {table}.{remote}"))
}

/// `2412000` → `2,4 M`, `310245` → `310 k`, for index usage.
fn format_scans(scans: i64) -> String {
    if scans >= 1_000_000 {
        let (decimal_separator, _) = crate::panel::number_separators();
        format!("{:.1} M", scans as f64 / 1_000_000.0).replace('.', &decimal_separator.to_string())
    } else if scans >= 10_000 {
        format!("{} k", scans / 1_000)
    } else {
        crate::panel::format_count(scans)
    }
}

impl Render for TableView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("DatabaseTableView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::apply_filter))
            .on_action(cx.listener(Self::apply_edits))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(match self.tab {
                Tab::Data => self.render_data(cx),
                _ => self.render_structure_tab(cx),
            })
    }
}

impl Focusable for TableView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TableView {}

impl Item for TableView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.name.clone().into()
    }

    // Not `is_dirty`: the pane would treat the table as an unsaved file, and the edits are
    // applied with Aplicar, not saved.
    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        h_flex()
            .gap_1p5()
            .child(Label::new(self.tab_content_text(0, cx)).color(params.text_color()))
            .when(self.has_pending_edits(), |this| {
                this.child(StatusTone::Warning.dot(cx))
            })
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} · {}.{}", self.connection.name, self.schema, self.name).into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(
            Icon::new(match self.kind {
                RelationKind::View | RelationKind::MaterializedView => IconName::Eye,
                _ => IconName::Table,
            })
            .color(Color::Muted),
        )
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens the table, or focuses the tab that already shows it.
#[allow(clippy::too_many_arguments)]
pub fn open(
    workspace: WeakEntity<Workspace>,
    connection: SavedConnection,
    session: Arc<Session>,
    schema: String,
    name: String,
    kind: RelationKind,
    estimated_rows: Option<i64>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let existing = workspace.items_of_type::<TableView>(cx).find(|view| {
                let view = view.read(cx);
                view.connection.id == connection.id && view.schema == schema && view.name == name
            });
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let workspace_handle = workspace.weak_handle();
            let view = cx.new(|cx| {
                TableView::new(
                    workspace_handle,
                    connection,
                    session,
                    schema,
                    name,
                    kind,
                    estimated_rows,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .log_err();
}
