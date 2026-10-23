//! A `.sql` file from the project, open against a connection: the file is an ordinary editor
//! (saved like any other), with the results of what ran underneath. Each tab has its own
//! session, so a transaction opened here stays here.

use crate::{
    CancelQuery, ExplainStatement, RunScript, RunStatement, catalog,
    completion::{SchemaCache, SqlCompletionProvider},
    connection::SavedConnection,
    grid::{GridColumn, GridEvent, ResultGrid},
    panel::{chip, environment_color, segment},
    session::{QueryOutcome, ServerError, Session},
    statements::{self, Statement, StatementKind, Transaction},
    write_guard::{Decision, WriteGuard},
};
use editor::{
    Editor, EditorEvent, HighlightKey, MultiBufferOffset, SelectionEffects, ToOffset as _,
};
use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyWindowHandle, ClipboardItem, DragMoveEvent, Empty,
    Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, Global, Pixels,
    Subscription, Task, WeakEntity, px, relative,
};
use project::Project;
use std::{
    cell::RefCell,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{
    Indicator, KeyBinding, Tooltip, prelude::*, primary_button, secondary_button, segmented_control,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, SaveOptions, TabContentParams},
};

const ROW_LIMIT: usize = 1000;
const MAX_HISTORY: usize = 50;
const CANCEL_GRACE: Duration = Duration::from_secs(3);
const MIN_RESULTS_HEIGHT: Pixels = px(160.);
const MIN_EDITOR_HEIGHT: Pixels = px(80.);

/// Dragged by the bar between the editor and the results.
struct DraggedResultsDivider;
/// How much of the editor goes to the agent when the tab is mentioned.
const MAX_AGENT_SQL_CHARS: usize = 20_000;

/// Every open query tab, so the agent (which only knows its project) can list them and write
/// into one.
#[derive(Default)]
struct OpenQueryViews(Vec<WeakEntity<SqlQueryView>>);

impl Global for OpenQueryViews {}

/// Where the agent's SQL goes in the editor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentWriteMode {
    /// A new statement after what is already there.
    #[default]
    Append,
    /// In place of the statement under the cursor.
    ReplaceStatement,
    /// In place of the whole file.
    ReplaceAll,
}

/// The query tabs of a project, most recently opened first.
pub(crate) fn query_views(project: &Entity<Project>, cx: &App) -> Vec<Entity<SqlQueryView>> {
    let Some(open) = cx.try_global::<OpenQueryViews>() else {
        return Vec::new();
    };
    open.0
        .iter()
        .rev()
        .filter_map(|view| view.upgrade())
        .filter(|view| view.read(cx).project_id == project.entity_id())
        .collect()
}

/// The text edit that puts `sql` into `text`: the range it replaces, what goes there, and where
/// the statement starts afterwards, for the cursor.
fn agent_edit(
    text: &str,
    cursor: usize,
    sql: &str,
    mode: AgentWriteMode,
) -> (Range<usize>, String, usize) {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    let statement_at_cursor = match mode {
        AgentWriteMode::ReplaceStatement => {
            let all = statements::split(text);
            statements::statement_at(&all, cursor).map(|statement| statement.range.clone())
        }
        AgentWriteMode::Append | AgentWriteMode::ReplaceAll => None,
    };
    if let Some(range) = statement_at_cursor {
        // The statement's range stops before its `;`, which stays.
        let start = range.start;
        return (range, sql.to_owned(), start);
    }
    if mode == AgentWriteMode::ReplaceAll || text.trim().is_empty() {
        return (0..text.len(), format!("{sql};\n"), 0);
    }
    let separator = if text.ends_with("\n\n") {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    (
        text.len()..text.len(),
        format!("{separator}{sql};\n"),
        text.len() + separator.len(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Statement,
    Script,
    Explain,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResultTab {
    Result,
    Messages,
    Plan,
    History,
}

#[derive(Clone, Copy)]
enum Export {
    Csv,
    Json,
}

/// What goes to the server for one statement.
#[derive(Clone)]
struct Planned {
    sql: String,
    statement: Statement,
    /// Characters added in front of the statement (`explain (analyze) `), to map error
    /// positions back to the buffer.
    prefix_chars: u32,
}

struct Message {
    text: String,
    color: Color,
}

struct HistoryEntry {
    sql: String,
    summary: String,
    failed: bool,
}

enum RunState {
    Idle,
    Running {
        started: Instant,
        /// 1-based, among the statements of the file, when a single one runs.
        statement: Option<usize>,
        reads_only: bool,
    },
    Done {
        truncated: bool,
        /// Rows of the result shown in the grid.
        rows: Option<usize>,
        elapsed: Duration,
        finished: Instant,
    },
    Failed(Failure),
}

struct Failure {
    message: SharedString,
    server: Option<ServerError>,
    /// Buffer range of the identifier the server pointed at.
    range: Option<Range<usize>>,
    line: Option<usize>,
    suggestion: Option<String>,
    elapsed: Option<Duration>,
}

struct Executed {
    outcomes: Vec<(Planned, QueryOutcome)>,
    error: Option<(Planned, anyhow::Error)>,
    in_transaction: bool,
    elapsed: Duration,
}

pub struct SqlQueryView {
    workspace: WeakEntity<Workspace>,
    project_id: EntityId,
    window: AnyWindowHandle,
    editor: Entity<Editor>,
    path: PathBuf,
    connection: SavedConnection,
    panel_session: Arc<Session>,
    session: Option<Arc<Session>>,
    in_transaction: bool,
    grid: Entity<ResultGrid>,
    has_grid: bool,
    state: RunState,
    messages: Vec<Message>,
    history: Vec<HistoryEntry>,
    tab: ResultTab,
    /// The last EXPLAIN, one line per plan row, for the Plano tab.
    plan_lines: Vec<String>,
    /// Set once the divider above the results is dragged; until then they take 45% of the tab.
    results_height: Option<Pixels>,
    focus_handle: FocusHandle,
    run_task: Task<()>,
    tick_task: Task<()>,
    cancel_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// `column "t.status"` out of Postgres's "Perhaps you meant to reference the column …" hint.
fn suggested_column(hint: &str) -> Option<String> {
    let start = hint.find("column \"")? + "column \"".len();
    let end = hint[start..].find('"')? + start;
    Some(hint[start..end].to_owned())
}

fn outcome_summary(planned: &Planned, outcome: &QueryOutcome) -> String {
    let verb = planned
        .statement
        .first_keyword
        .as_deref()
        .unwrap_or("sql")
        .to_uppercase();
    let result_set = outcome.result_sets.last();
    let count = result_set.and_then(|result_set| result_set.rows_affected);
    let truncated = result_set.is_some_and(|result_set| result_set.truncated);
    match count {
        Some(count) if truncated => format!("{verb} · {count}+ linhas (limite de {ROW_LIMIT})"),
        Some(1) => format!("{verb} · 1 linha"),
        Some(count) => format!("{verb} · {count} linhas"),
        None => verb,
    }
}

impl SqlQueryView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        buffer: Entity<language::Buffer>,
        path: PathBuf,
        connection: SavedConnection,
        panel_session: Arc<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cache = Rc::new(RefCell::new(SchemaCache::default()));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, Some(project.clone()), window, cx);
            editor.set_completion_provider(Some(Rc::new(SqlCompletionProvider {
                cache: cache.clone(),
            })));
            editor
        });
        let catalog_session = panel_session.clone();
        cx.spawn(async move |_, _| {
            let columns = catalog::list_all_columns(&catalog_session).await?;
            *cache.borrow_mut() = SchemaCache::new(columns);
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        let grid = cx.new(|cx| ResultGrid::new(false, cx));
        let this = cx.weak_entity();
        let open = cx.default_global::<OpenQueryViews>();
        open.0.retain(|view| view.upgrade().is_some());
        open.0.push(this);
        let open_value = cx.subscribe_in(&grid, window, |this, _, event, window, cx| {
            if let GridEvent::OpenValue { column, value } = event {
                crate::grid::open_value(
                    this.workspace.clone(),
                    column.clone(),
                    value.clone(),
                    window,
                    cx,
                );
            }
        });
        let subscriptions = vec![
            open_value,
            cx.subscribe(&editor, |this, _, event, cx| match event {
                EditorEvent::DirtyChanged
                | EditorEvent::Saved
                | EditorEvent::TitleChanged
                | EditorEvent::FileHandleChanged => cx.emit(ItemEvent::UpdateTab),
                EditorEvent::Edited { .. } => {
                    // The server's position no longer points at the same text.
                    if matches!(this.state, RunState::Failed(_)) {
                        this.clear_highlight(HighlightKey::DatabaseError, cx);
                    }
                    cx.emit(ItemEvent::Edit);
                }
                _ => {}
            }),
        ];
        Self {
            workspace,
            project_id: project.entity_id(),
            window: window.window_handle(),
            editor,
            path,
            connection,
            panel_session,
            session: None,
            in_transaction: false,
            grid,
            has_grid: false,
            state: RunState::Idle,
            messages: Vec::new(),
            history: Vec::new(),
            tab: ResultTab::Result,
            plan_lines: Vec::new(),
            results_height: None,
            focus_handle: cx.focus_handle(),
            run_task: Task::ready(()),
            tick_task: Task::ready(()),
            cancel_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connection(&self) -> &SavedConnection {
        &self.connection
    }

    pub(crate) fn window(&self) -> AnyWindowHandle {
        self.window
    }

    /// What the agent gets when this tab is mentioned.
    pub(crate) fn describe_for_agent(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let text = editor.text(cx);
        let cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&editor.buffer().read(cx).snapshot(cx))
            .0
            .min(text.len());
        let cursor_line = text[..cursor].matches('\n').count() + 1;
        let connection = &self.connection;
        let mut sql = text;
        if sql.chars().count() > MAX_AGENT_SQL_CHARS {
            sql = format!(
                "{}\n-- … (cut)",
                sql.chars().take(MAX_AGENT_SQL_CHARS).collect::<String>()
            );
        }
        let path = self.path.display();
        format!(
            "SQL editor `{path}`, open against the connection \"{name}\" ({environment}, \
             {address}, PostgreSQL{read_only}).\n\n\
             When the user asks a question about the data (how many, which, show me), run a \
             read with `database_query` and answer from the rows in the chat; don't hand them \
             SQL to run. When they ask for SQL in this editor (write, change, fix a query), \
             follow these rules:\n\
             - Write the SQL with the `database_write_query` tool (path `{path}`). That is the \
             whole answer: reply with no text at all, no explanation, no summary, and never \
             repeat the SQL in the chat.\n\
             - When you don't already know the tables and columns, read them with \
             `database_schema` first so the names are right. Don't run SQL you wrote into the \
             editor: the user runs it from there.\n\
             - Mode `append` adds a new statement at the end, `replace_statement` rewrites the \
             statement under the cursor (to change or fix it), `replace_all` rewrites the file.\n\
             - Only add `--` comments when the user asks for an explanation.\n\
             - If the request can't be written (unknown table, ambiguous), answer with one short \
             sentence saying why, in the user's language.\n\
             - Without the `database_write_query` tool, edit the file at that path instead, \
             with the same rules.\n\n\
             Current contents (cursor on line {cursor_line}):\n```sql\n{sql}\n```",
            name = connection.name,
            environment = connection.environment.label(),
            address = connection.address_with_tunnel(),
            read_only = if connection.read_only {
                ", read-only"
            } else {
                ""
            },
        )
    }

    /// Puts the agent's SQL into the editor as one undoable edit, with the cursor on it so ⌘↵
    /// runs it. Returns the line the statement starts on.
    pub(crate) fn write_from_agent(
        &mut self,
        sql: &str,
        mode: AgentWriteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let cursor = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
                .head()
                .0;
            let (range, new_text, statement_start) = agent_edit(&text, cursor, sql, mode);
            editor.edit(
                [(
                    MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                    new_text,
                )],
                cx,
            );
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(statement_start)..MultiBufferOffset(statement_start)
                ])
            });
            editor.text(cx)[..statement_start].matches('\n').count() + 1
        })
    }

    fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running { .. })
    }

    fn clear_highlight(&mut self, key: HighlightKey, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.clear_background_highlights(key, cx);
        });
    }

    fn highlight(&mut self, key: HighlightKey, ranges: Vec<Range<usize>>, cx: &mut Context<Self>) {
        let error = key == HighlightKey::DatabaseError;
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchors = ranges
                .into_iter()
                .map(|range| {
                    snapshot.anchor_before(MultiBufferOffset(range.start))
                        ..snapshot.anchor_after(MultiBufferOffset(range.end))
                })
                .collect::<Vec<_>>();
            editor.highlight_background(
                key,
                &anchors,
                move |_, theme| {
                    if error {
                        theme.status().error_background
                    } else {
                        theme.colors().editor_active_line_background
                    }
                },
                cx,
            );
        });
    }

    /// The statements to run, from the selection when there is one, else from the cursor.
    fn plan(&self, scope: Scope, cx: &mut Context<Self>) -> Vec<Planned> {
        let (text, start, end) = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let selection = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
            (text, selection.start.0, selection.end.0)
        });
        let targets: Vec<Statement> = if start != end && scope != Scope::Script {
            let selected = &text[start.min(text.len())..end.min(text.len())];
            statements::split(selected)
                .into_iter()
                .map(|mut statement| {
                    statement.range = statement.range.start + start..statement.range.end + start;
                    statement
                })
                .collect()
        } else {
            let all = statements::split(&text);
            match scope {
                Scope::Script => all,
                Scope::Statement | Scope::Explain => statements::statement_at(&all, start)
                    .cloned()
                    .into_iter()
                    .collect(),
            }
        };
        targets
            .into_iter()
            .map(|statement| {
                let sql = text[statement.range.clone()].to_owned();
                if scope == Scope::Explain {
                    // ANALYZE runs the statement, so writes only get the plan.
                    let prefix = if statement.kind == StatementKind::Read {
                        "explain (analyze, buffers) "
                    } else {
                        "explain "
                    };
                    let (kind, first_keyword) = statements::classify(&format!("{prefix}{sql}"));
                    Planned {
                        sql: format!("{prefix}{sql}"),
                        statement: Statement {
                            kind,
                            first_keyword,
                            ..statement
                        },
                        prefix_chars: prefix.chars().count() as u32,
                    }
                } else {
                    Planned {
                        sql,
                        statement,
                        prefix_chars: 0,
                    }
                }
            })
            .collect()
    }

    /// For the debug hook, which can't press ⌘↵.
    pub(crate) fn run_statement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(Scope::Statement, window, cx);
    }

    /// For the debug hook: cursor at the end of the first line, completion menu open.
    pub(crate) fn show_completions_at_first_line(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.editor.read(cx).text(cx);
        let end = text.find('\n').unwrap_or(text.len());
        window.focus(&self.editor.focus_handle(cx), cx);
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(end)..MultiBufferOffset(end)])
            });
            editor.show_completions(&editor::actions::ShowCompletions, window, cx);
        });
    }

    fn run(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let plan = self.plan(scope, cx);
        if plan.is_empty() {
            self.state = RunState::Failed(Failure {
                message: "Nada para rodar: o cursor não está em um statement.".into(),
                server: None,
                range: None,
                line: None,
                suggestion: None,
                elapsed: None,
            });
            cx.notify();
            return;
        }
        let risky = plan.iter().find_map(|planned| {
            statements::risk(planned.statement.kind, &self.connection)
                .map(|risk| (planned.sql.clone(), risk, planned.statement.range.start))
        });
        let Some((statement, risk, start)) = risky else {
            self.execute(plan, false, cx);
            return;
        };
        let line = {
            let text = self.editor.read(cx).text(cx);
            text[..start.min(text.len())].matches('\n').count() + 1
        };
        let this = cx.weak_entity();
        let connection = self.connection.clone();
        let in_transaction = self.in_transaction;
        // EXPLAIN without ANALYZE never runs the statement, so before this tab has its own
        // session the dock's can estimate it.
        let session = Some(
            self.session
                .clone()
                .unwrap_or_else(|| self.panel_session.clone()),
        );
        let opened = self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                WriteGuard::new(
                    connection,
                    statement,
                    risk,
                    in_transaction,
                    None,
                    session,
                    Box::new(move |decision, _window, cx| {
                        this.update(cx, |this, cx| {
                            this.execute(plan, decision == Decision::RunInTransaction, cx)
                        })
                        .log_err();
                    }),
                    window,
                    cx,
                )
                .at_line(line)
            });
        });
        if opened.is_err() {
            self.state = RunState::Failed(Failure {
                message: "A confirmação não abriu: a janela deste workspace foi fechada.".into(),
                server: None,
                range: None,
                line: None,
                suggestion: None,
                elapsed: None,
            });
            cx.notify();
        }
    }

    fn execute(&mut self, plan: Vec<Planned>, begin_first: bool, cx: &mut Context<Self>) {
        self.clear_highlight(HighlightKey::DatabaseError, cx);
        self.highlight(
            HighlightKey::DatabaseStatement,
            plan.iter()
                .map(|planned| planned.statement.range.clone())
                .collect(),
            cx,
        );
        let statement = match plan.as_slice() {
            [planned] => {
                let text = self.editor.read(cx).text(cx);
                statements::split(&text)
                    .iter()
                    .position(|statement| statement.range == planned.statement.range)
                    .map(|index| index + 1)
            }
            _ => None,
        };
        self.state = RunState::Running {
            started: Instant::now(),
            statement,
            reads_only: plan
                .iter()
                .all(|planned| planned.statement.kind == StatementKind::Read),
        };
        self.tab = ResultTab::Result;
        self.cancel_task = Task::ready(());
        cx.notify();
        self.tick_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.is_running()
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        });
        // A connection that died (server restart, idle timeout) fails every statement sent to it,
        // so the tab opens a new one; whatever transaction it had is gone with it.
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.is_closed())
        {
            self.session = None;
            self.in_transaction = false;
        }
        let session = self.session.clone();
        let panel_session = self.panel_session.clone();
        let in_transaction = self.in_transaction;
        self.run_task = cx.spawn(async move |this, cx| {
            let session = match session {
                Some(session) => Ok(session),
                None => panel_session.connect_again().await.map(Arc::new),
            };
            let session = match session {
                Ok(session) => session,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.state = RunState::Failed(Failure {
                            message: format!("Não deu para abrir a sessão: {error:#}").into(),
                            server: None,
                            range: None,
                            line: None,
                            suggestion: None,
                            elapsed: None,
                        });
                        this.clear_highlight(HighlightKey::DatabaseStatement, cx);
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            this.update(cx, |this, _| this.session = Some(session.clone()))
                .log_err();
            let executed = execute_plan(&session, plan, begin_first, in_transaction).await;
            this.update(cx, |this, cx| this.finish(executed, cx))
                .log_err();
        });
    }

    fn finish(&mut self, executed: Executed, cx: &mut Context<Self>) {
        self.clear_highlight(HighlightKey::DatabaseStatement, cx);
        self.in_transaction = executed.in_transaction;
        for (planned, outcome) in &executed.outcomes {
            let summary = outcome_summary(planned, outcome);
            self.messages.push(Message {
                text: format!("{summary} · {} ms", outcome.elapsed.as_millis()),
                color: Color::Default,
            });
            self.history.insert(
                0,
                HistoryEntry {
                    sql: planned.sql.clone(),
                    summary,
                    failed: false,
                },
            );
        }
        let shown = executed.outcomes.iter().rev().find_map(|(_, outcome)| {
            outcome
                .result_sets
                .iter()
                .rev()
                .find(|result_set| !result_set.columns.is_empty())
                .cloned()
        });
        let truncated = shown
            .as_ref()
            .is_some_and(|result_set| result_set.truncated);
        let shown_rows = shown.as_ref().map(|result_set| result_set.rows.len());
        let explained = executed
            .outcomes
            .iter()
            .rev()
            .find(|(planned, _)| planned.sql.starts_with("explain "));
        if let Some((_, outcome)) = explained {
            self.plan_lines = outcome
                .result_sets
                .iter()
                .flat_map(|result_set| result_set.rows.iter())
                .filter_map(|row| row.first().cloned().flatten())
                .collect();
        }
        if let Some(result_set) = shown {
            let columns = result_set
                .columns
                .into_iter()
                .map(|column| GridColumn {
                    name: column.name,
                    type_name: column.type_name.as_deref().map(catalog::sql_type_name),
                })
                .collect();
            self.grid.update(cx, |grid, cx| {
                grid.set_data(columns, result_set.rows, 1, cx)
            });
            self.has_grid = true;
        } else if !executed.outcomes.is_empty() {
            self.has_grid = false;
        }
        if explained.is_some() && executed.error.is_none() {
            self.tab = ResultTab::Plan;
        }
        match executed.error {
            Some((planned, error)) => {
                let text = self.editor.read(cx).text(cx);
                let server = error.downcast_ref::<ServerError>().cloned();
                let range =
                    server
                        .as_ref()
                        .and_then(|server| server.position)
                        .and_then(|position| {
                            let position = position.checked_sub(planned.prefix_chars)?;
                            let statement_text = text.get(planned.statement.range.clone())?;
                            let offset = planned.statement.range.start
                                + statements::char_position_to_offset(statement_text, position);
                            let range = statements::identifier_at(&text, offset.min(text.len()));
                            Some(if range.is_empty() {
                                offset..(offset + 1).min(text.len())
                            } else {
                                range
                            })
                        });
                let line = range
                    .as_ref()
                    .map(|range| text[..range.start].matches('\n').count() + 1);
                let suggestion = server
                    .as_ref()
                    .and_then(|server| server.hint.as_deref())
                    .and_then(suggested_column);
                if let Some(range) = range.clone() {
                    self.highlight(HighlightKey::DatabaseError, vec![range], cx);
                }
                let message: SharedString = match &server {
                    Some(server) => server.to_string().into(),
                    None => format!("{error:#}").into(),
                };
                self.messages.push(Message {
                    text: message.to_string(),
                    color: Color::Error,
                });
                self.history.insert(
                    0,
                    HistoryEntry {
                        sql: planned.sql,
                        summary: message.to_string(),
                        failed: true,
                    },
                );
                self.state = RunState::Failed(Failure {
                    message,
                    server,
                    range,
                    line,
                    suggestion,
                    elapsed: Some(executed.elapsed),
                });
                self.tab = ResultTab::Messages;
            }
            None => {
                self.state = RunState::Done {
                    truncated,
                    rows: shown_rows,
                    elapsed: executed.elapsed,
                    finished: Instant::now(),
                };
            }
        }
        self.history.truncate(MAX_HISTORY);
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_running() {
            return;
        }
        let Some(session) = self.session.clone() else {
            self.abandon_run("Cancelado antes de a sessão abrir.", cx);
            return;
        };
        cx.background_spawn(async move { session.cancel().await })
            .detach_and_log_err(cx);
        // The server never answers a cancel request, so it looks delivered even when the
        // connection it targets is gone or half-open and the statement will never return; on a
        // stalled network the request itself can hang. Past the grace period the tab stops
        // waiting and drops that connection.
        self.cancel_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CANCEL_GRACE).await;
            this.update(cx, |this, cx| {
                if this.is_running() {
                    this.abandon_run(
                        "O servidor não respondeu ao cancelamento. A conexão desta aba foi \
                         descartada; a próxima execução abre outra.",
                        cx,
                    );
                }
            })
            .log_err();
        });
    }

    /// Stops waiting for the running statement and drops the tab's connection with it, which also
    /// makes the server roll back an open transaction once it notices.
    fn abandon_run(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let elapsed = match &self.state {
            RunState::Running { started, .. } => Some(started.elapsed()),
            _ => None,
        };
        self.run_task = Task::ready(());
        self.session = None;
        self.in_transaction = false;
        self.clear_highlight(HighlightKey::DatabaseStatement, cx);
        self.state = RunState::Failed(Failure {
            message: message.into(),
            server: None,
            range: None,
            line: None,
            suggestion: None,
            elapsed,
        });
        cx.notify();
    }

    fn end_transaction(&mut self, transaction: Transaction, cx: &mut Context<Self>) {
        let sql = match transaction {
            Transaction::Commit => "commit",
            Transaction::Rollback => "rollback",
            Transaction::Begin => return,
        };
        let (kind, first_keyword) = statements::classify(sql);
        self.execute(
            vec![Planned {
                sql: sql.to_owned(),
                statement: Statement {
                    range: 0..0,
                    kind,
                    first_keyword,
                },
                prefix_chars: 0,
            }],
            false,
            cx,
        );
    }

    fn apply_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RunState::Failed(Failure {
            range: Some(range),
            suggestion: Some(suggestion),
            ..
        }) = &self.state
        else {
            return;
        };
        let (range, suggestion) = (range.clone(), suggestion.clone());
        self.editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        suggestion.as_str(),
                    )],
                    None,
                    cx,
                )
            });
        });
        self.state = RunState::Idle;
        self.clear_highlight(HighlightKey::DatabaseError, cx);
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn go_to_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RunState::Failed(Failure {
            range: Some(range), ..
        }) = &self.state
        else {
            return;
        };
        let range = range.clone();
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections
                    .select_ranges([MultiBufferOffset(range.start)..MultiBufferOffset(range.end)])
            });
        });
        window.focus(&self.editor.focus_handle(cx), cx);
    }

    fn copy_as_csv(&self, cx: &mut Context<Self>) {
        let csv = self.grid.read(cx).to_csv();
        cx.write_to_clipboard(ClipboardItem::new_string(csv));
    }

    fn copy_as_insert(&self, cx: &mut Context<Self>) {
        let sql = self.grid.read(cx).to_insert("resultado");
        cx.write_to_clipboard(ClipboardItem::new_string(sql));
    }

    /// Saves the whole result next to the query, under a name the user confirms.
    fn export(&mut self, format: Export, _window: &mut Window, cx: &mut Context<Self>) {
        let grid = self.grid.read(cx);
        let (content, extension) = match format {
            Export::Csv => (grid.to_csv(), "csv"),
            Export::Json => (grid.to_json(), "json"),
        };
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        let stem = self
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "resultado".to_owned());
        let directory = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let receiver = cx.prompt_for_new_path(&directory, Some(&format!("{stem}.{extension}")));
        cx.spawn(async move |this, cx| {
            let result = async {
                let Some(path) = receiver.await.map_err(|_| anyhow::anyhow!("cancelado"))?? else {
                    return anyhow::Ok(None);
                };
                fs.atomic_write(path.clone(), content).await?;
                anyhow::Ok(Some(path))
            }
            .await;
            this.update(cx, |this, cx| {
                let (text, color) = match result {
                    Ok(Some(path)) => {
                        (format!("Exportado para {}", path.display()), Color::Default)
                    }
                    Ok(None) => return,
                    Err(error) => (format!("Não deu para exportar: {error:#}"), Color::Error),
                };
                this.messages.push(Message { text, color });
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.is_running();
        let connection = &self.connection;
        let environment = environment_color(connection.environment);
        let saved_in = self
            .path
            .to_string_lossy()
            .split_once(".asylum/")
            .map(|(_, rest)| format!("salvo em .asylum/{rest}"));
        h_flex()
            .h(px(52.))
            .flex_none()
            .px_3()
            .gap_2()
            .child(
                h_flex()
                    .h(px(32.))
                    .px_2p5()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_selected)
                    .child(Indicator::dot().color(environment))
                    .child(
                        Label::new(connection.name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(connection.environment.label())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                chip(cx).child(
                    Label::new(if self.in_transaction {
                        "transação aberta"
                    } else {
                        "auto-commit"
                    })
                    .size(LabelSize::XSmall)
                    .color(if self.in_transaction {
                        Color::Warning
                    } else {
                        Color::Default
                    }),
                ),
            )
            .when(connection.read_only, |this| {
                this.child(chip(cx).child(Label::new("somente leitura").size(LabelSize::XSmall)))
            })
            .child(
                chip(cx).child(Label::new(format!("limite {ROW_LIMIT}")).size(LabelSize::XSmall)),
            )
            .when_some(saved_in, |this, saved_in| {
                this.child(
                    div().min_w_0().child(
                        Label::new(saved_in)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
            })
            .child(div().flex_1())
            .when(self.in_transaction && !running, |this| {
                this.child(
                    secondary_button("db-rollback", None, "Rollback", true, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.end_transaction(Transaction::Rollback, cx)
                        }),
                    ),
                )
                .child(
                    secondary_button(
                        "db-commit",
                        Some((IconName::Check, Color::Warning)),
                        "Commit",
                        true,
                        cx,
                    )
                    .on_click(
                        cx.listener(|this, _, _, cx| this.end_transaction(Transaction::Commit, cx)),
                    ),
                )
            })
            .child(
                secondary_button(
                    "db-explain",
                    Some((IconName::ListTree, Color::Muted)),
                    "EXPLAIN",
                    !running,
                    cx,
                )
                .when(!running, |this| {
                    this.on_click(
                        cx.listener(|this, _, window, cx| this.run(Scope::Explain, window, cx)),
                    )
                }),
            )
            .child(if running {
                let error = Color::Error.color(cx);
                h_flex()
                    .id("db-cancel-query")
                    .h(px(30.))
                    .px_3()
                    .gap_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(error.opacity(0.06))
                    .border_1()
                    .border_color(error)
                    .child(
                        div()
                            .size(px(9.))
                            .rounded(px(2.))
                            .bg(cx.theme().colors().text),
                    )
                    .child(Label::new("Cancelar").size(LabelSize::Small))
                    .child(
                        KeyBinding::for_action_in(&CancelQuery, &self.editor.focus_handle(cx), cx)
                            .size(rems_from_px(12_f32)),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                    .into_any_element()
            } else {
                primary_button(
                    "db-run",
                    Some(IconName::PlayFilled),
                    "Executar",
                    Some(KeyBinding::for_action_in(
                        &RunStatement,
                        &self.editor.focus_handle(cx),
                        cx,
                    )),
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.run(Scope::Statement, window, cx)))
                .into_any_element()
            })
            .into_any_element()
    }

    /// The line under the editor: which statement ⌘↵ runs, how, and where the cursor is.
    fn render_editor_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.editor.read(cx);
        let text = editor.text(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let cursor = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0
            .min(text.len());
        let point = snapshot.offset_to_point(MultiBufferOffset(cursor));
        let all = statements::split(&text);
        let number_at = |offset: usize| {
            statements::statement_at(&all, offset)
                .and_then(|statement| all.iter().position(|other| other.range == statement.range))
                .map(|index| index + 1)
        };
        let (icon, message, color) = match &self.state {
            RunState::Running { statement, .. } => (
                IconName::LoadCircle,
                match statement {
                    Some(number) => {
                        format!("Rodando o statement {number} · o editor continua livre")
                    }
                    None => "Rodando · o editor continua livre".to_owned(),
                },
                Color::Warning,
            ),
            RunState::Failed(failure) => (
                IconName::XCircle,
                match failure
                    .range
                    .as_ref()
                    .and_then(|range| number_at(range.start))
                {
                    Some(number) => format!("1 erro no statement {number}"),
                    None => "1 erro".to_owned(),
                },
                Color::Error,
            ),
            _ => match number_at(cursor) {
                Some(number) => (
                    IconName::Check,
                    format!("Statement {number} de {} sob o cursor", all.len()),
                    Color::Success,
                ),
                None => (
                    IconName::Info,
                    "Nenhum statement sob o cursor".to_owned(),
                    Color::Muted,
                ),
            },
        };
        h_flex()
            .h(px(32.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Icon::new(icon).size(IconSize::Small).color(color))
            .child(Label::new(message).size(LabelSize::Small))
            .when(!self.is_running(), |this| {
                this.child(
                    div().pl_4().child(
                        Label::new("⌘↵ roda o statement · ⌘⇧↵ roda o arquivo · ⌥↵ EXPLAIN ANALYZE")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(div().flex_1())
            .child(
                Label::new(format!("Ln {}, Col {}", point.row + 1, point.column + 1))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_state_chip(&self, cx: &App) -> AnyElement {
        let (icon, label, color, mono) = match &self.state {
            RunState::Idle => return Empty.into_any_element(),
            RunState::Running { .. } => (
                Some(IconName::LoadCircle),
                "rodando".to_owned(),
                Color::Warning,
                false,
            ),
            RunState::Done {
                rows: Some(rows),
                truncated,
                ..
            } => (
                None,
                match (rows, truncated) {
                    (_, true) => format!("{rows}+ linhas"),
                    (1, false) => "1 linha".to_owned(),
                    (rows, false) => format!("{rows} linhas"),
                },
                Color::Success,
                false,
            ),
            RunState::Done { .. } => (None, "ok".to_owned(), Color::Success, false),
            RunState::Failed(failure) => (
                None,
                failure
                    .server
                    .as_ref()
                    .map(|server| format!("erro {}", server.code))
                    .unwrap_or_else(|| "erro".to_owned()),
                Color::Error,
                true,
            ),
        };
        h_flex()
            .h(px(22.))
            .px_2()
            .gap_1p5()
            .rounded_md()
            .bg(color.color(cx).opacity(0.1))
            .map(|this| match icon {
                Some(icon) => this.child(Icon::new(icon).size(IconSize::XSmall).color(color)),
                None => this.child(Indicator::dot().color(color)),
            })
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(color)
                    .when(mono, |label| label.buffer_font(cx)),
            )
            .into_any_element()
    }

    fn render_state_detail(&self) -> String {
        match &self.state {
            RunState::Idle => String::new(),
            RunState::Running { started, .. } => format!(
                "{:.1} s · aguardando o servidor",
                started.elapsed().as_secs_f32()
            )
            .replace('.', ","),
            RunState::Done {
                elapsed, finished, ..
            } => format!(
                "{} ms · rodou {}",
                elapsed.as_millis(),
                elapsed_label(finished.elapsed())
            ),
            RunState::Failed(failure) => match failure.elapsed {
                Some(elapsed) => {
                    format!("falhou em {} ms · nada foi alterado", elapsed.as_millis())
                }
                None => "nada foi alterado".to_owned(),
            },
        }
    }

    fn render_failure(&self, failure: &Failure, cx: &mut Context<Self>) -> AnyElement {
        let has_range = failure.range.is_some();
        let error = Color::Error.color(cx);
        let text = self.editor.read(cx).text(cx);
        // `LINE 11:  and t.stauts = …` with a caret under the column, as psql prints it.
        let pointer = failure.range.as_ref().and_then(|range| {
            let line_start = text[..range.start].rfind('\n').map_or(0, |index| index + 1);
            let line_end = text[range.start..]
                .find('\n')
                .map_or(text.len(), |index| range.start + index);
            let prefix = format!("LINE {}: ", failure.line?);
            let caret = prefix.chars().count() + text[line_start..range.start].chars().count();
            let before_caret = format!("{prefix}{}", &text[line_start..range.start]);
            debug_assert_eq!(before_caret.chars().count(), caret);
            Some((
                format!("{prefix}{}", &text[line_start..line_end]),
                before_caret,
            ))
        });
        let context = format!(
            "rodou como {} em {} {}",
            self.connection.user,
            self.connection.name,
            self.connection.environment.label()
        );
        v_flex()
            .rounded_lg()
            .bg(error.opacity(0.05))
            .border_1()
            .border_color(error.opacity(0.4))
            .child(
                v_flex()
                    .p_4()
                    .gap_2p5()
                    .child(
                        h_flex()
                            .gap_2p5()
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(
                                Label::new(failure.message.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        v_flex()
                            .pl(px(26.))
                            .gap_1()
                            .when_some(pointer, |this, (line, caret)| {
                                this.child(
                                    v_flex()
                                        .child(
                                            Label::new(line)
                                                .size(LabelSize::Small)
                                                .buffer_font(cx)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            // The text before the column, invisible, keeps the
                                            // caret under it whatever the whitespace.
                                            h_flex()
                                                .child(
                                                    Label::new(caret)
                                                        .size(LabelSize::Small)
                                                        .buffer_font(cx)
                                                        .color(Color::Custom(
                                                            gpui::transparent_black(),
                                                        )),
                                                )
                                                .child(
                                                    Label::new("^")
                                                        .size(LabelSize::Small)
                                                        .buffer_font(cx)
                                                        .color(Color::Error),
                                                ),
                                        ),
                                )
                            })
                            .when_some(
                                failure.server.as_ref().and_then(|s| s.detail.clone()),
                                |this, detail| {
                                    this.child(
                                        Label::new(format!("DETAIL: {detail}"))
                                            .size(LabelSize::Small)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    )
                                },
                            )
                            .when_some(
                                failure.server.as_ref().and_then(|s| s.hint.clone()),
                                |this, hint| {
                                    this.child(
                                        Label::new(format!("HINT: {hint}"))
                                            .size(LabelSize::Small)
                                            .buffer_font(cx)
                                            .color(Color::Warning),
                                    )
                                },
                            )
                            .when(failure.server.is_some(), |this| {
                                this.child(
                                    Label::new(context)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .when(self.in_transaction, |this| {
                                this.child(
                                    Label::new(
                                        "A transação desta aba foi abortada: rode Rollback para \
                                         continuar.",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                                )
                            }),
                    ),
            )
            .when(failure.suggestion.is_some() || has_range, |this| {
                this.child(
                    h_flex()
                        .p_4()
                        .gap_2()
                        .border_t_1()
                        .border_color(error.opacity(0.25))
                        .when_some(failure.suggestion.clone(), |this, suggestion| {
                            this.child(
                                primary_button(
                                    "db-apply-suggestion",
                                    Some(IconName::Check),
                                    format!("Trocar por {suggestion}"),
                                    None,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.apply_suggestion(window, cx),
                                )),
                            )
                        })
                        .when(has_range, |this| {
                            this.child(
                                secondary_button(
                                    "db-go-to-error",
                                    None,
                                    match failure.line {
                                        Some(line) => format!("Ir para a linha {line}"),
                                        None => "Ir para o erro".to_owned(),
                                    },
                                    true,
                                    cx,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.go_to_error(window, cx)),
                                ),
                            )
                        })
                        .when(failure.server.is_some(), |this| {
                            this.child(
                                div().pl_3().child(
                                    Label::new(
                                        "O texto original do Postgres fica aqui, sem tradução, \
                                         para dar para buscar.",
                                    )
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    /// While a statement runs: how long, and the way to stop it on the server.
    fn render_running(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let RunState::Running {
            started,
            reads_only,
            ..
        } = &self.state
        else {
            return None;
        };
        let colors = cx.theme().colors();
        let warning = Color::Warning.color(cx);
        let explanation = if self.in_transaction {
            "Dentro da transação aberta: cancelar aborta a transação."
        } else if *reads_only {
            "Nada muda: está em auto-commit e é SELECT."
        } else {
            "Em auto-commit: o que já terminou fica gravado."
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    v_flex()
                        .occlude()
                        .w(px(470.))
                        .p_5()
                        .gap_3p5()
                        .rounded_xl()
                        .border_1()
                        .border_color(colors.border_selected)
                        .bg(colors.elevated_surface_background)
                        .shadow_lg()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Icon::new(IconName::LoadCircle)
                                                .size(IconSize::Small)
                                                .color(Color::Warning),
                                        )
                                        .child(
                                            Label::new(
                                                format!(
                                                    "Rodando há {:.1} s",
                                                    started.elapsed().as_secs_f32()
                                                )
                                                .replace('.', ","),
                                            )
                                            .weight(FontWeight::SEMIBOLD),
                                        ),
                                )
                                .child(
                                    Label::new(format!(
                                        "{} · {}",
                                        self.connection.name,
                                        self.connection.address()
                                    ))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                                ),
                        )
                        .child(
                            div()
                                .relative()
                                .h(px(3.))
                                .w_full()
                                .rounded_sm()
                                .overflow_hidden()
                                .bg(colors.border_selected)
                                .child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .h_full()
                                        .w(px(110.))
                                        .rounded_sm()
                                        .bg(warning)
                                        .with_animation(
                                            "db-running-bar",
                                            Animation::new(Duration::from_millis(1400)).repeat(),
                                            |bar, delta| bar.left(px(-110. + delta * 540.)),
                                        ),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_3p5()
                                .child(
                                    h_flex()
                                        .id("db-cancel-running")
                                        .h(px(34.))
                                        .px_3()
                                        .gap_2()
                                        .flex_none()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .bg(colors.element_background)
                                        .border_1()
                                        .border_color(colors.border_selected)
                                        .hover(|style| style.bg(colors.element_hover))
                                        .child(div().size(px(9.)).rounded(px(2.)).bg(colors.text))
                                        .child(Label::new("Cancelar query").size(LabelSize::Small))
                                        .child(
                                            KeyBinding::for_action_in(
                                                &CancelQuery,
                                                &self.editor.focus_handle(cx),
                                                cx,
                                            )
                                            .size(rems_from_px(12_f32)),
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.cancel(&CancelQuery, window, cx)
                                        })),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .gap_0p5()
                                        .child(
                                            Label::new("Cancela no servidor, não só na tela.")
                                                .size(LabelSize::Small),
                                        )
                                        .child(
                                            Label::new(explanation)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [
            (ResultTab::Result, "Resultado"),
            (ResultTab::Messages, "Mensagens"),
            (ResultTab::Plan, "Plano"),
            (ResultTab::History, "Histórico"),
        ];
        let failure = match &self.state {
            RunState::Failed(failure) => Some(failure),
            _ => None,
        };
        let content = match self.tab {
            ResultTab::Result if self.has_grid => div()
                .size_full()
                .when(self.is_running(), |this| this.opacity(0.3))
                .child(self.grid.clone())
                .into_any_element(),
            ResultTab::Result => div()
                .p_3()
                .child(
                    Label::new(match &self.state {
                        RunState::Running { .. } => "Aguardando o servidor…",
                        RunState::Failed(_) => "O statement falhou: veja em Mensagens.",
                        RunState::Done { .. } => "O statement não devolveu linhas.",
                        RunState::Idle => "Rode um statement para ver o resultado aqui.",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            ResultTab::Messages => v_flex()
                .id("db-messages")
                .size_full()
                .overflow_y_scroll()
                .px_3()
                .pb_3()
                .gap_3()
                .when_some(failure, |this, failure| {
                    this.child(self.render_failure(failure, cx))
                })
                .child(
                    v_flex().gap_1().children(
                        self.messages
                            .iter()
                            .rev()
                            .filter(|message| {
                                failure
                                    .is_none_or(|failure| message.text != failure.message.as_ref())
                            })
                            .map(|message| {
                                Label::new(message.text.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(message.color)
                            }),
                    ),
                )
                .into_any_element(),
            ResultTab::Plan if self.plan_lines.is_empty() => div()
                .p_3()
                .child(
                    Label::new("Rode ⌥↵ para ver o plano do statement sob o cursor.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            ResultTab::Plan => v_flex()
                .id("db-plan")
                .size_full()
                .overflow_y_scroll()
                .px_3()
                .pb_3()
                .children(self.plan_lines.iter().map(|line| {
                    Label::new(line.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(if line.trim_start().starts_with("->") {
                            Color::Default
                        } else {
                            Color::Muted
                        })
                }))
                .into_any_element(),
            ResultTab::History => v_flex()
                .id("db-history")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .children(self.history.iter().enumerate().map(|(index, entry)| {
                    let sql = entry.sql.clone();
                    v_flex()
                        .id(("db-history-entry", index))
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .tooltip(Tooltip::text("Copiar o SQL"))
                        .child(
                            Label::new(sql.lines().next().unwrap_or_default().to_owned())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        )
                        .child(
                            Label::new(entry.summary.clone())
                                .size(LabelSize::XSmall)
                                .color(if entry.failed {
                                    Color::Error
                                } else {
                                    Color::Muted
                                }),
                        )
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()))
                        })
                }))
                .into_any_element(),
        };
        let footer = match (&self.state, self.tab) {
            (_, ResultTab::Result) if self.has_grid => {
                let rows = self.grid.read(cx).row_count();
                let truncated = matches!(
                    self.state,
                    RunState::Done {
                        truncated: true,
                        ..
                    }
                );
                Some(if self.is_running() {
                    "Abaixo, o resultado anterior, apagado até o novo chegar.".to_owned()
                } else if truncated {
                    format!("{rows} linhas · parou no limite de {ROW_LIMIT}")
                } else {
                    format!(
                        "{rows} {} · resultado completo (abaixo do limite de {ROW_LIMIT})",
                        if rows == 1 { "linha" } else { "linhas" }
                    )
                })
            }
            _ => None,
        };
        let export = self.has_grid && !self.is_running();
        v_flex()
            .relative()
            .map(|this| match self.results_height {
                Some(height) => this.h(height),
                None => this.h(relative(0.45)),
            })
            .min_h(MIN_RESULTS_HEIGHT)
            .child(
                div()
                    .id("db-results-resize-handle")
                    .h(px(12.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_row_resize()
                    .child(
                        div()
                            .w(px(40.))
                            .h(px(4.))
                            .rounded_sm()
                            .bg(cx.theme().colors().border_selected),
                    )
                    .on_drag(DraggedResultsDivider, |_, _, _, cx| cx.new(|_| Empty))
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.results_height = None;
                            cx.notify();
                        }
                    })),
            )
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px_3()
                    .gap_2p5()
                    .child(Label::new("Resultados").weight(FontWeight::SEMIBOLD))
                    .child(self.render_state_chip(cx))
                    .child(
                        Label::new(self.render_state_detail())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(segmented_control(cx).children(tabs.map(|(tab, label)| {
                        segment(("db-result-tab", tab as usize), label, tab == self.tab, cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tab = tab;
                                cx.notify();
                            }))
                    })))
                    .child(
                        IconButton::new("db-copy-csv", IconName::Copy)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(!export)
                            .tooltip(Tooltip::text("Copiar o resultado como CSV"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_as_csv(cx))),
                    )
                    .child(
                        IconButton::new("db-export-csv", IconName::Download)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(!export)
                            .tooltip(Tooltip::text("Exportar CSV"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.export(Export::Csv, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(content),
            )
            .when_some(footer, |this, footer| {
                this.child(
                    h_flex()
                        .h(px(40.))
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(footer)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .when(export, |this| {
                            let link = |id: &'static str, label: &'static str| {
                                Button::new(id, label)
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::XSmall)
                                    .color(Color::Accent)
                            };
                            this.child(link("db-export-csv-link", "Exportar CSV").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.export(Export::Csv, window, cx)
                                }),
                            ))
                            .child(link("db-export-json", "JSON").on_click(cx.listener(
                                |this, _, window, cx| this.export(Export::Json, window, cx),
                            )))
                            .child(
                                link("db-copy-insert", "Copiar como INSERT").on_click(
                                    cx.listener(|this, _, _, cx| this.copy_as_insert(cx)),
                                ),
                            )
                        }),
                )
            })
            .children(self.render_running(cx))
            .into_any_element()
    }
}

/// `há 12 s`, `há 3 min`, for when the shown result ran.
fn elapsed_label(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        0..5 => "agora".to_owned(),
        seconds @ 5..60 => format!("há {seconds} s"),
        seconds @ 60..3_600 => format!("há {} min", seconds / 60),
        seconds => format!("há {} h", seconds / 3_600),
    }
}

async fn execute_plan(
    session: &Session,
    plan: Vec<Planned>,
    begin_first: bool,
    mut in_transaction: bool,
) -> Executed {
    let started = Instant::now();
    let mut outcomes = Vec::new();
    if begin_first && !in_transaction {
        let (kind, first_keyword) = statements::classify("begin");
        let planned = Planned {
            sql: "begin".to_owned(),
            statement: Statement {
                range: 0..0,
                kind,
                first_keyword,
            },
            prefix_chars: 0,
        };
        match session.run("begin", 1).await {
            Ok(outcome) => {
                in_transaction = true;
                outcomes.push((planned, outcome));
            }
            Err(error) => {
                return Executed {
                    outcomes,
                    error: Some((planned, error)),
                    in_transaction,
                    elapsed: started.elapsed(),
                };
            }
        }
    }
    for planned in plan {
        let cursor = in_transaction
            && planned
                .statement
                .kind
                .is_cursor_safe(planned.statement.first_keyword.as_deref());
        let result = if cursor {
            session.run_in_cursor(&planned.sql, ROW_LIMIT).await
        } else {
            session.run(&planned.sql, ROW_LIMIT).await
        };
        match result {
            Ok(outcome) => {
                match planned.statement.kind {
                    StatementKind::Transaction(Transaction::Begin) => in_transaction = true,
                    StatementKind::Transaction(Transaction::Commit | Transaction::Rollback) => {
                        in_transaction = false
                    }
                    _ => {}
                }
                outcomes.push((planned, outcome));
            }
            Err(error) => {
                return Executed {
                    outcomes,
                    error: Some((planned, error)),
                    in_transaction,
                    elapsed: started.elapsed(),
                };
            }
        }
    }
    Executed {
        outcomes,
        error: None,
        in_transaction,
        elapsed: started.elapsed(),
    }
}

impl Render for SqlQueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SqlQueryView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &RunStatement, window, cx| {
                this.run(Scope::Statement, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &RunScript, window, cx| this.run(Scope::Script, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ExplainStatement, window, cx| {
                this.run(Scope::Explain, window, cx)
            }))
            .on_action(cx.listener(Self::cancel))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedResultsDivider>, _, cx| {
                    let bounds = event.bounds;
                    let max_height =
                        (bounds.size.height - MIN_EDITOR_HEIGHT).max(MIN_RESULTS_HEIGHT);
                    let height = (bounds.bottom() - event.event.position.y)
                        .clamp(MIN_RESULTS_HEIGHT, max_height);
                    this.results_height = Some(height);
                    cx.notify();
                },
            ))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
            .child(self.render_editor_status(cx))
            .child(self.render_results(cx))
    }
}

impl Focusable for SqlQueryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for SqlQueryView {}

impl Item for SqlQueryView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "query.sql".to_owned())
            .into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} · {}", self.path.display(), self.connection.name).into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileCode).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

/// Opens the `.sql` file against the connection active in the dock, or focuses the tab that
/// already has it.
pub fn open(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    connection: SavedConnection,
    session: Arc<Session>,
    path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<Entity<SqlQueryView>>> {
    let existing = workspace
        .update(cx, |workspace, cx| {
            let existing = workspace
                .items_of_type::<SqlQueryView>(cx)
                .find(|view| view.read(cx).path == path);
            if let Some(existing) = &existing {
                workspace.activate_item(existing, true, true, window, cx);
            }
            existing
        })
        .ok()
        .flatten();
    if let Some(existing) = existing {
        return Task::ready(Ok(existing));
    }
    let buffer = project.update(cx, |project, cx| project.open_local_buffer(&path, cx));
    window.spawn(cx, async move |cx| {
        let buffer = buffer.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            let view = cx.new(|cx| {
                SqlQueryView::new(
                    workspace_handle,
                    project,
                    buffer,
                    path,
                    connection,
                    session,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        })
    })
}

/// Brings the tab to the front of its pane.
pub(crate) fn activate(
    view: &Entity<SqlQueryView>,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let workspace = view.read(cx).workspace.clone();
    workspace.update(cx, |workspace, cx| {
        workspace.activate_item(view, true, true, window, cx);
    })
}

/// A new, empty file in `.asylum/db/queries`, named `query-N.sql`.
pub async fn create_query_file(
    fs: &dyn fs::Fs,
    root: &Path,
    header: &str,
) -> anyhow::Result<PathBuf> {
    let directory = root.join(".asylum/db/queries");
    fs.create_dir(&directory).await?;
    let mut number = 1;
    let path = loop {
        let candidate = directory.join(format!("query-{number}.sql"));
        if !fs.is_file(&candidate).await {
            break candidate;
        }
        number += 1;
    };
    fs.atomic_write(path.clone(), format!("{header}\n\n"))
        .await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_column_out_of_the_hint() {
        assert_eq!(
            suggested_column("Perhaps you meant to reference the column \"users.name\".")
                .as_deref(),
            Some("users.name")
        );
        assert_eq!(suggested_column("Something else."), None);
    }

    fn apply(text: &str, cursor: usize, sql: &str, mode: AgentWriteMode) -> (String, usize) {
        let (range, new_text, statement_start) = agent_edit(text, cursor, sql, mode);
        let mut result = text.to_owned();
        result.replace_range(range, &new_text);
        (result, statement_start)
    }

    #[test]
    fn agent_sql_goes_after_the_header() {
        let header = "-- Homologação · dev\n\n";
        let (text, start) = apply(header, 0, "select 1", AgentWriteMode::Append);
        assert_eq!(text, "-- Homologação · dev\n\nselect 1;\n");
        assert_eq!(&text[start..], "select 1;\n");

        let (text, start) = apply("select 1;", 0, " select 2; ", AgentWriteMode::Append);
        assert_eq!(text, "select 1;\n\nselect 2;\n");
        assert_eq!(&text[start..], "select 2;\n");

        let (text, _) = apply("  \n", 0, "select 1", AgentWriteMode::Append);
        assert_eq!(text, "select 1;\n");
    }

    #[test]
    fn agent_sql_replaces_the_statement_under_the_cursor() {
        let source = "select 1;\n\nselect * from users;\n\nselect 3;\n";
        let cursor = source.find("users").unwrap();
        let (text, start) = apply(
            source,
            cursor,
            "select * from users where active;",
            AgentWriteMode::ReplaceStatement,
        );
        assert_eq!(
            text,
            "select 1;\n\nselect * from users where active;\n\nselect 3;\n"
        );
        assert_eq!(start, source.find("select *").unwrap());

        let (text, start) = apply(source, 3, "select 9", AgentWriteMode::ReplaceAll);
        assert_eq!((text.as_str(), start), ("select 9;\n", 0));
    }
}
