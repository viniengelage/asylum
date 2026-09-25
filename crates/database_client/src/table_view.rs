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
    panel::environment_chip,
    session::{ServerError, Session},
    statements::Risk,
    write_guard::WriteGuard,
};
use gpui::{
    AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Subscription, Task, WeakEntity, px,
};
use std::{sync::Arc, time::Duration};
use ui::{
    Indicator, ToggleButtonGroup, ToggleButtonGroupSize, ToggleButtonGroupStyle,
    ToggleButtonSimple, Tooltip, prelude::*,
};
use ui_input::InputField;
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

    fn label(self) -> &'static str {
        match self {
            Self::Data => "Dados",
            Self::Structure => "Estrutura",
            Self::Indexes => "Índices",
            Self::Relations => "Relações",
            Self::Ddl => "DDL",
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
    where_input: Entity<InputField>,
    order_input: Entity<InputField>,
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
        let where_input = cx.new(|cx| {
            InputField::new(window, cx, "created_at > now() - interval '7 days'").label("WHERE")
        });
        let order_input =
            cx.new(|cx| InputField::new(window, cx, "id desc").label("ORDER BY"));
        let grid = cx.new(|cx| ResultGrid::new(true, cx));
        let subscriptions = vec![cx.subscribe(&grid, |this, _, event, cx| match event {
            GridEvent::SortRequested(column) => this.sort_by(*column, cx),
            GridEvent::CellEdited { row, column, value } => {
                this.cell_edited(*row, *column, value.clone(), cx)
            }
        })];
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

    fn open_session(&mut self, panel_session: Arc<Session>, cx: &mut Context<Self>) {
        let schema = self.schema.clone();
        let name = self.name.clone();
        self.data_task = cx.spawn(async move |this, cx| {
            let result = async {
                let session = Arc::new(panel_session.connect_read_only().await?);
                let columns = catalog::list_columns(&session, &schema, &name).await?;
                anyhow::Ok((session, columns))
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok((session, columns)) => {
                    this.session = Some(session);
                    this.columns = columns;
                    let editable = this.can_edit();
                    this.grid.update(cx, |grid, cx| grid.set_editable(editable, cx));
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
        let mut sql = format!(
            "select * from {}",
            qualified_name(&self.schema, &self.name)
        );
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
                            this.data = Load::Failed("A consulta não devolveu linhas.".into());
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
        self.notice = Some((
            "Aplique ou descarte as alterações antes de trocar de página, ordenar ou filtrar."
                .into(),
            Color::Warning,
        ));
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
        self.grid.update(cx, |grid, cx| grid.set_pending(pending, cx));
        cx.notify();
    }

    fn discard_edits(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.show_sql = false;
        self.notice = None;
        self.grid
            .update(cx, |grid, cx| grid.set_pending(PendingEdits::default(), cx));
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
                        this.update(cx, |this, cx| this.write(updates, cx)).log_err();
                    }),
                    window,
                    cx,
                )
            });
        });
        if opened.is_err() {
            self.notice = Some(("A janela deste workspace foi fechada.".into(), Color::Error));
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
                        this.notice = Some((
                            if count == 1 {
                                "1 linha atualizada.".into()
                            } else {
                                format!("{count} linhas atualizadas.").into()
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
        if tab != Tab::Data && self.structure.is_none() {
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
            .find(|tab| tab.label().eq_ignore_ascii_case(label))
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
        let tab_index = Tab::ALL
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap_or(0);
        let tabs = Tab::ALL.map(|tab| {
            ToggleButtonSimple::new(
                tab.label(),
                cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)),
            )
        });
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                Icon::new(IconName::Database)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(self.connection.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(environment_chip(self.connection.environment, cx))
            .child(
                Icon::new(IconName::ChevronRight)
                    .size(IconSize::XSmall)
                    .color(Color::Disabled),
            )
            .child(
                Label::new(self.schema.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Icon::new(IconName::ChevronRight)
                    .size(IconSize::XSmall)
                    .color(Color::Disabled),
            )
            .child(Label::new(self.name.clone()).weight(FontWeight::SEMIBOLD))
            .when(
                matches!(
                    self.kind,
                    RelationKind::View | RelationKind::MaterializedView
                ),
                |this| {
                    this.child(
                        Label::new("view")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                },
            )
            .when_some(self.estimated_rows, |this, rows| {
                this.child(
                    Label::new(format!("~{} linhas", crate::panel::format_count(rows)))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .child(
                ToggleButtonGroup::single_row("db-table-tabs", tabs)
                    .style(ToggleButtonGroupStyle::Outlined)
                    .size(ToggleButtonGroupSize::Custom(rems_from_px(28_f32)))
                    .label_size(LabelSize::Small)
                    .auto_width()
                    .selected_index(tab_index),
            )
            .into_any_element()
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .key_context("DatabaseTableFilter")
            .px_3()
            .pb_2()
            .gap_3()
            .items_end()
            .child(div().flex_1().child(self.where_input.clone()))
            .child(div().w(px(260.)).child(self.order_input.clone()))
            .child(
                Button::new("db-table-apply", "Aplicar")
                    .style(ButtonStyle::Outlined)
                    .key_binding(ui::KeyBinding::for_action_in(
                        &ApplyTableFilter,
                        &self.focus_handle,
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_filter(&ApplyTableFilter, window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let (summary, has_more) = match &self.data {
            Load::Loaded(page) => {
                let first = self.page * PAGE_SIZE + 1;
                let last = self.page * PAGE_SIZE + page.shown;
                let range = if page.shown == 0 {
                    "nenhuma linha".to_owned()
                } else {
                    format!("linhas {first}–{last}")
                };
                let mode = if self.can_edit() {
                    "duplo clique ou Enter edita a célula"
                } else if self.columns.iter().any(|column| column.primary_key) {
                    "somente leitura"
                } else {
                    "somente leitura: sem chave primária"
                };
                (
                    format!("{range} · {} ms · {mode}", page.elapsed.as_millis()),
                    page.has_more,
                )
            }
            Load::Loading => ("carregando…".to_owned(), false),
            Load::Failed(_) => (String::new(), false),
        };
        let page = self.page;
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(summary)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("db-table-previous", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .disabled(page == 0)
                    .tooltip(Tooltip::text("Página anterior"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go_to_page(page.saturating_sub(1), cx)
                    })),
            )
            .child(
                Label::new(format!("página {}", page + 1))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("db-table-next", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(!has_more)
                    .tooltip(Tooltip::text("Próxima página"))
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to_page(page + 1, cx))),
            )
            .into_any_element()
    }

    fn render_message(&self, message: SharedString, error: bool, cx: &App) -> AnyElement {
        let color = if error { Color::Error } else { Color::Muted };
        v_flex()
            .m_3()
            .p_3()
            .gap_1()
            .rounded_md()
            .when(error, |this| {
                this.bg(Color::Error.color(cx).opacity(0.08))
                    .border_1()
                    .border_color(Color::Error.color(cx).opacity(0.3))
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
            .flex_1()
            .min_h_0()
            .child(self.render_filters(cx))
            .child(match &self.data {
                Load::Failed(error) => self.render_message(error.clone(), true, cx),
                Load::Loading if self.grid.read(cx).row_count() == 0 => {
                    self.render_message("Carregando…".into(), false, cx)
                }
                _ => div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.grid.clone())
                    .into_any_element(),
            })
            .children(self.render_pending(cx))
            .child(self.render_footer(cx))
            .into_any_element()
    }

    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending.is_empty() && self.notice.is_none() {
            return None;
        }
        let preview = self
            .show_sql
            .then(|| self.pending_updates(cx).map(|updates| edits::preview(&updates)));
        let count = self.pending.len();
        Some(
            v_flex()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .when_some(preview, |this, preview| {
                    let text = match &preview {
                        Ok(text) => text.clone(),
                        Err(error) => error.clone(),
                    };
                    let copy = text.clone();
                    this.child(
                        v_flex()
                            .max_h(px(260.))
                            .id("db-edits-preview")
                            .overflow_y_scroll()
                            .px_3()
                            .py_2()
                            .gap_0p5()
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                h_flex()
                                    .pb_1()
                                    .child(
                                        Label::new("SQL que vai rodar")
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Label::new(
                                            " · as condições com ::text conferem o valor antigo",
                                        )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        IconButton::new("db-copy-edits-sql", IconName::Copy)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("Copiar o SQL"))
                                            .on_click(move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy.clone(),
                                                ))
                                            }),
                                    ),
                            )
                            .children(text.lines().map(|line| {
                                Label::new(line.to_owned())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(if line.contains("is not distinct from") {
                                        Color::Warning
                                    } else {
                                        Color::Default
                                    })
                            })),
                    )
                })
                .child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .when(count > 0, |this| {
                            this.bg(Color::Warning.color(cx).opacity(0.08))
                                .child(Indicator::dot().color(Color::Warning))
                                .child(
                                    Label::new(if count == 1 {
                                        "1 alteração pendente".to_owned()
                                    } else {
                                        format!("{count} alterações pendentes")
                                    })
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD)
                                    .color(Color::Warning),
                                )
                                .child(
                                    Label::new("· numa transação")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                        })
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
                        .child(div().flex_1())
                        .when(count > 0, |this| {
                            this.child(
                                Button::new(
                                    "db-edits-sql",
                                    if self.show_sql { "Ocultar SQL" } else { "Ver SQL" },
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_sql = !this.show_sql;
                                    cx.notify();
                                })),
                            )
                            .child(
                                Button::new("db-edits-discard", "Descartar")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .disabled(self.applying)
                                    .on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))),
                            )
                            .child(
                                Button::new(
                                    "db-edits-apply",
                                    if self.applying { "Aplicando…" } else { "Aplicar" },
                                )
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .label_size(LabelSize::Small)
                                .disabled(self.applying)
                                .key_binding(ui::KeyBinding::for_action_in(
                                    &ApplyEdits,
                                    &self.focus_handle,
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.apply_edits(&ApplyEdits, window, cx)
                                })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_rows(
        &self,
        headers: &[&'static str],
        widths: &[f32],
        rows: Vec<Vec<(String, Color)>>,
        cx: &App,
    ) -> AnyElement {
        let row = |cells: Vec<AnyElement>| {
            h_flex()
                .px_3()
                .py_1p5()
                .gap_3()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .children(cells)
        };
        let cell = |index: usize, child: AnyElement| {
            let width = widths.get(index).copied().unwrap_or(0.);
            if width > 0. {
                div().w(px(width)).flex_none().child(child).into_any_element()
            } else {
                div().flex_1().min_w_0().child(child).into_any_element()
            }
        };
        v_flex()
            .child(
                row(headers
                    .iter()
                    .enumerate()
                    .map(|(index, header)| {
                        cell(
                            index,
                            Label::new(*header)
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::SEMIBOLD)
                                .color(Color::Muted)
                                .into_any_element(),
                        )
                    })
                    .collect())
                .bg(cx.theme().colors().element_background),
            )
            .children(rows.into_iter().map(|values| {
                row(values
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, color))| {
                        cell(
                            index,
                            Label::new(value)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(color)
                                .into_any_element(),
                        )
                    })
                    .collect())
            }))
            .into_any_element()
    }

    fn render_structure_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let content = match (&self.tab, &self.structure) {
            (Tab::Structure, _) => self.render_rows(
                &["Coluna", "Tipo", "Null", "Default", "Restrições"],
                &[200., 200., 60., 240., 0.],
                self.columns
                    .iter()
                    .map(|column| {
                        let restrictions = [
                            column.primary_key.then_some("PK"),
                            column.unique.then_some("único"),
                        ]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                        vec![
                            (column.name.clone(), Color::Default),
                            (column.type_name.clone(), Color::Accent),
                            (
                                if column.not_null { "não" } else { "sim" }.to_owned(),
                                Color::Muted,
                            ),
                            (
                                column.default.clone().unwrap_or_else(|| "—".to_owned()),
                                if column.default.is_some() {
                                    Color::Warning
                                } else {
                                    Color::Disabled
                                },
                            ),
                            (
                                restrictions,
                                if column.primary_key {
                                    Color::Warning
                                } else {
                                    Color::Muted
                                },
                            ),
                        ]
                    })
                    .collect(),
                cx,
            ),
            (_, None | Some(Load::Loading)) => {
                self.render_message("Lendo o catálogo…".into(), false, cx)
            }
            (_, Some(Load::Failed(error))) => self.render_message(error.clone(), true, cx),
            (Tab::Indexes, Some(Load::Loaded(structure))) => {
                if structure.indexes.is_empty() {
                    self.render_message("Esta tabela não tem índices.".into(), false, cx)
                } else {
                    self.render_rows(
                        &["Nome", "Definição", "Tamanho", "Uso"],
                        &[240., 0., 90., 220.],
                        structure
                            .indexes
                            .iter()
                            .map(|index| {
                                let unused = index.scans == Some(0) && !index.primary;
                                vec![
                                    (index.name.clone(), Color::Default),
                                    (
                                        index
                                            .definition
                                            .split_once(" USING ")
                                            .map(|(_, rest)| rest.to_owned())
                                            .unwrap_or_else(|| index.definition.clone()),
                                        Color::Muted,
                                    ),
                                    (index.size.clone(), Color::Muted),
                                    (
                                        match index.scans {
                                            Some(0) if unused => {
                                                "0 scans · candidato a remover".to_owned()
                                            }
                                            Some(scans) => format!("{scans} scans"),
                                            None => "sem estatística".to_owned(),
                                        },
                                        if unused { Color::Warning } else { Color::Muted },
                                    ),
                                ]
                            })
                            .collect(),
                        cx,
                    )
                }
            }
            (Tab::Relations, Some(Load::Loaded(structure))) => {
                if structure.foreign_keys.is_empty() {
                    self.render_message(
                        "Nenhuma chave estrangeira aponta para esta tabela ou sai dela.".into(),
                        false,
                        cx,
                    )
                } else {
                    self.render_rows(
                        &["Direção", "Tabela", "Definição", "Ao apagar"],
                        &[140., 220., 0., 110.],
                        structure
                            .foreign_keys
                            .iter()
                            .map(|foreign_key| {
                                vec![
                                    (
                                        match foreign_key.direction {
                                            ForeignKeyDirection::References => "referencia",
                                            ForeignKeyDirection::ReferencedBy => {
                                                "referenciada por"
                                            }
                                        }
                                        .to_owned(),
                                        Color::Muted,
                                    ),
                                    (foreign_key.other_table.clone(), Color::Default),
                                    (foreign_key.definition.clone(), Color::Muted),
                                    (
                                        foreign_key.on_delete.clone(),
                                        if foreign_key.on_delete == "CASCADE" {
                                            Color::Error
                                        } else {
                                            Color::Accent
                                        },
                                    ),
                                ]
                            })
                            .collect(),
                        cx,
                    )
                }
            }
            (Tab::Ddl, Some(Load::Loaded(structure))) => {
                let ddl = structure.ddl.clone();
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(
                                    "Montado a partir do catálogo: colunas, defaults, restrições \
                                     e índices.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("db-copy-ddl", "Copiar")
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Copy).size(IconSize::Small))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            ddl.clone(),
                                        ))
                                    }),
                            ),
                    )
                    .child(
                        v_flex()
                            .p_3()
                            .rounded_md()
                            .bg(cx.theme().colors().editor_background)
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .children(structure.ddl.lines().map(|line| {
                                Label::new(line.to_owned())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                            })),
                    )
                    .into_any_element()
            }
            (Tab::Data, _) => div().into_any_element(),
        };
        v_flex()
            .id("db-table-structure")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(content)
            .into_any_element()
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

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
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
