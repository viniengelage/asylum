//! A `.sql` file from the project, open against a connection: the file is an ordinary editor
//! (saved like any other), with the results of what ran underneath. Each tab has its own
//! session, so a transaction opened here stays here.

use crate::{
    CancelQuery, ExplainStatement, RunScript, RunStatement, catalog,
    completion::{SchemaCache, SqlCompletionProvider},
    connection::SavedConnection,
    grid::{GridColumn, ResultGrid},
    panel::environment_chip,
    session::{QueryOutcome, ServerError, Session},
    statements::{self, Statement, StatementKind, Transaction},
    write_guard::{Decision, WriteGuard},
};
use editor::{Editor, EditorEvent, HighlightKey, MultiBufferOffset, SelectionEffects};
use gpui::{
    AnyElement, ClipboardItem, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight,
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
    ToggleButtonGroup, ToggleButtonGroupSize, ToggleButtonGroupStyle, ToggleButtonSimple, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, SaveOptions, TabContentParams},
};

const ROW_LIMIT: usize = 1000;
const MAX_HISTORY: usize = 50;

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
    History,
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
    Running { started: Instant, total: usize },
    Done { summary: String, truncated: bool },
    Failed(Failure),
}

struct Failure {
    message: SharedString,
    server: Option<ServerError>,
    /// Buffer range of the identifier the server pointed at.
    range: Option<Range<usize>>,
    line: Option<usize>,
    suggestion: Option<String>,
}

struct Executed {
    outcomes: Vec<(Planned, QueryOutcome)>,
    error: Option<(Planned, anyhow::Error)>,
    in_transaction: bool,
    elapsed: Duration,
}

pub struct SqlQueryView {
    workspace: WeakEntity<Workspace>,
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
    focus_handle: FocusHandle,
    run_task: Task<()>,
    tick_task: Task<()>,
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
        let subscriptions = vec![cx.subscribe(&editor, |this, _, event, cx| match event {
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
        })];
        Self {
            workspace,
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
            focus_handle: cx.focus_handle(),
            run_task: Task::ready(()),
            tick_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
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
            });
            cx.notify();
            return;
        }
        let risky = plan.iter().find_map(|planned| {
            statements::risk(planned.statement.kind, &self.connection)
                .map(|risk| (planned.sql.clone(), risk))
        });
        let Some((statement, risk)) = risky else {
            self.execute(plan, false, cx);
            return;
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
            });
        });
        if opened.is_err() {
            self.state = RunState::Failed(Failure {
                message: "A confirmação não abriu: a janela deste workspace foi fechada.".into(),
                server: None,
                range: None,
                line: None,
                suggestion: None,
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
        self.state = RunState::Running {
            started: Instant::now(),
            total: plan.len(),
        };
        self.tab = ResultTab::Result;
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
        let elapsed = executed.elapsed.as_millis();
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
        if let Some(result_set) = shown {
            let columns = result_set
                .columns
                .into_iter()
                .map(|column| GridColumn {
                    name: column.name,
                    type_name: column.type_name,
                })
                .collect();
            self.grid.update(cx, |grid, cx| {
                grid.set_data(columns, result_set.rows, 1, cx)
            });
            self.has_grid = true;
        } else if !executed.outcomes.is_empty() {
            self.has_grid = false;
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
                });
            }
            None => {
                let summary = match executed.outcomes.as_slice() {
                    [(planned, outcome)] => outcome_summary(planned, outcome),
                    outcomes => format!("{} statements", outcomes.len()),
                };
                self.state = RunState::Done {
                    summary: format!("{summary} · {elapsed} ms"),
                    truncated,
                };
            }
        }
        self.history.truncate(MAX_HISTORY);
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        let (true, Some(session)) = (self.is_running(), self.session.clone()) else {
            return;
        };
        cx.background_spawn(async move { session.cancel().await })
            .detach_and_log_err(cx);
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

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.is_running();
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Icon::new(IconName::Database)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(self.connection.name.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(environment_chip(self.connection.environment, cx))
            .child(
                Label::new(self.connection.address())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            )
            .child(
                Label::new(if self.in_transaction {
                    "transação aberta"
                } else {
                    "auto-commit"
                })
                .size(LabelSize::XSmall)
                .color(if self.in_transaction {
                    Color::Warning
                } else {
                    Color::Muted
                }),
            )
            .when(self.connection.read_only, |this| {
                this.child(
                    Label::new("somente leitura")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(
                Label::new(format!("limite {ROW_LIMIT}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(self.in_transaction && !running, |this| {
                this.child(
                    Button::new("db-rollback", "Rollback")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.end_transaction(Transaction::Rollback, cx)
                        })),
                )
                .child(
                    Button::new("db-commit", "Commit")
                        .style(ButtonStyle::Tinted(ui::TintColor::Warning))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.end_transaction(Transaction::Commit, cx)
                        })),
                )
            })
            .child(
                Button::new("db-explain", "EXPLAIN")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .disabled(running)
                    .start_icon(Icon::new(IconName::ListTree).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &ExplainStatement,
                        &self.editor.focus_handle(cx),
                        cx,
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run(Scope::Explain, window, cx)),
                    ),
            )
            .child(if running {
                Button::new("db-cancel-query", "Cancelar")
                    .style(ButtonStyle::Tinted(ui::TintColor::Error))
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Stop).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &CancelQuery,
                        &self.editor.focus_handle(cx),
                        cx,
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                    .into_any_element()
            } else {
                Button::new("db-run", "Executar")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(
                        &RunStatement,
                        &self.editor.focus_handle(cx),
                        cx,
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run(Scope::Statement, window, cx)),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_status(&self) -> AnyElement {
        let (icon, text, color): (Option<IconName>, String, Color) = match &self.state {
            RunState::Idle => (
                None,
                "⌘↵ roda o statement sob o cursor · ⌘⇧↵ o arquivo · ⌥↵ EXPLAIN".to_owned(),
                Color::Muted,
            ),
            RunState::Running { started, total } => (
                Some(IconName::LoadCircle),
                if *total > 1 {
                    format!(
                        "rodando {total} statements · {:.1} s",
                        started.elapsed().as_secs_f32()
                    )
                } else {
                    format!("rodando · {:.1} s", started.elapsed().as_secs_f32())
                },
                Color::Warning,
            ),
            RunState::Done { summary, truncated } => (
                Some(IconName::Check),
                if *truncated {
                    format!("{summary} · parou no limite de {ROW_LIMIT} linhas")
                } else {
                    summary.clone()
                },
                Color::Success,
            ),
            RunState::Failed(failure) => (
                Some(IconName::XCircle),
                failure
                    .server
                    .as_ref()
                    .map(|server| format!("erro {}", server.code))
                    .unwrap_or_else(|| "erro".to_owned()),
                Color::Error,
            ),
        };
        h_flex()
            .gap_1p5()
            .children(icon.map(|icon| Icon::new(icon).size(IconSize::Small).color(color)))
            .child(Label::new(text).size(LabelSize::Small).color(color))
            .into_any_element()
    }

    fn render_failure(&self, failure: &Failure, cx: &mut Context<Self>) -> AnyElement {
        let has_range = failure.range.is_some();
        v_flex()
            .m_3()
            .p_3()
            .gap_2()
            .rounded_md()
            .bg(Color::Error.color(cx).opacity(0.06))
            .border_1()
            .border_color(Color::Error.color(cx).opacity(0.3))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(failure.message.clone())
                            .buffer_font(cx)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
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
            .when(self.in_transaction, |this| {
                this.child(
                    Label::new("A transação desta aba foi abortada: rode Rollback para continuar.")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .when_some(failure.suggestion.clone(), |this, suggestion| {
                        this.child(
                            Button::new("db-apply-suggestion", format!("Trocar por {suggestion}"))
                                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.apply_suggestion(window, cx)
                                })),
                        )
                    })
                    .when(has_range, |this| {
                        this.child(
                            Button::new(
                                "db-go-to-error",
                                match failure.line {
                                    Some(line) => format!("Ir para a linha {line}"),
                                    None => "Ir para o erro".to_owned(),
                                },
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.go_to_error(window, cx)),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab_index = match self.tab {
            ResultTab::Result => 0,
            ResultTab::Messages => 1,
            ResultTab::History => 2,
        };
        let tabs = [
            ToggleButtonSimple::new(
                "Resultado",
                cx.listener(|this, _, _, cx| {
                    this.tab = ResultTab::Result;
                    cx.notify();
                }),
            ),
            ToggleButtonSimple::new(
                "Mensagens",
                cx.listener(|this, _, _, cx| {
                    this.tab = ResultTab::Messages;
                    cx.notify();
                }),
            ),
            ToggleButtonSimple::new(
                "Histórico",
                cx.listener(|this, _, _, cx| {
                    this.tab = ResultTab::History;
                    cx.notify();
                }),
            ),
        ];
        let content = match (self.tab, &self.state) {
            (ResultTab::Result, RunState::Failed(failure)) => self.render_failure(failure, cx),
            (ResultTab::Result, _) if self.has_grid => div()
                .size_full()
                .when(self.is_running(), |this| this.opacity(0.4))
                .child(self.grid.clone())
                .into_any_element(),
            (ResultTab::Result, _) => div()
                .p_3()
                .child(
                    Label::new(if self.is_running() {
                        "Aguardando o servidor…"
                    } else {
                        "Rode um statement para ver o resultado aqui."
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            (ResultTab::Messages, _) => v_flex()
                .id("db-messages")
                .size_full()
                .overflow_y_scroll()
                .p_3()
                .gap_1()
                .children(self.messages.iter().rev().map(|message| {
                    Label::new(message.text.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(message.color)
                }))
                .into_any_element(),
            (ResultTab::History, _) => v_flex()
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
                        .rounded_sm()
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
        v_flex()
            .h(relative(0.45))
            .min_h(px(160.))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .child(Label::new("Resultados").weight(FontWeight::SEMIBOLD))
                    .child(self.render_status())
                    .child(div().flex_1())
                    .child(
                        ToggleButtonGroup::single_row("db-result-tabs", tabs)
                            .style(ToggleButtonGroupStyle::Outlined)
                            .size(ToggleButtonGroupSize::Custom(rems_from_px(26_f32)))
                            .label_size(LabelSize::Small)
                            .auto_width()
                            .selected_index(tab_index),
                    )
                    .child(
                        IconButton::new("db-copy-csv", IconName::Copy)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .disabled(!self.has_grid)
                            .tooltip(Tooltip::text("Copiar o resultado como CSV"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_as_csv(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
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
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
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
) {
    let existing = workspace
        .update(cx, |workspace, cx| {
            let existing = workspace
                .items_of_type::<SqlQueryView>(cx)
                .find(|view| view.read(cx).path == path);
            if let Some(existing) = &existing {
                workspace.activate_item(existing, true, true, window, cx);
            }
            existing.is_some()
        })
        .unwrap_or(false);
    if existing {
        return;
    }
    let buffer = project.update(cx, |project, cx| project.open_local_buffer(&path, cx));
    window
        .spawn(cx, async move |cx| {
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
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
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
}
