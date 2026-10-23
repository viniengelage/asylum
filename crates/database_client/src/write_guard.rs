//! Asks before a statement that could do real damage: every row of a table, a schema change, or
//! any write on production. Production also asks for the database name to be typed.

use crate::{
    catalog,
    connection::{Environment, SavedConnection},
    panel::sql_line,
    session::Session,
    statements::Risk,
};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task, px,
};
use std::sync::Arc;
use ui::{prelude::*, secondary_button};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::ModalView;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run it as is (auto-commit, or inside the transaction already open).
    Run,
    /// Open a transaction first, so the result can still be rolled back.
    RunInTransaction,
}

type OnDecision = Box<dyn FnOnce(Decision, &mut Window, &mut App)>;

pub struct WriteGuard {
    focus_handle: FocusHandle,
    connection: SavedConnection,
    statement: String,
    risk: Risk,
    in_transaction: bool,
    note: Option<SharedString>,
    estimate: Option<Result<String, String>>,
    /// Tables whose foreign keys can stop a DELETE: `(table, ON DELETE action)`.
    references: Vec<(String, String)>,
    /// The line the statement starts on in the editor.
    line: Option<usize>,
    confirm_input: Option<Entity<InputField>>,
    on_decision: Option<OnDecision>,
    _estimate_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for WriteGuard {}

impl ModalView for WriteGuard {}

impl Focusable for WriteGuard {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.confirm_input {
            Some(input) => input.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

/// Foreign keys pointing at `table` that make the server refuse a DELETE of referenced rows.
async fn blocking_references(
    session: &Session,
    table: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let sql = format!(
        "select c.conrelid::regclass::text,
                case c.confdeltype when 'r' then 'RESTRICT' else 'NO ACTION' end
           from pg_catalog.pg_constraint c
          where c.contype = 'f' and c.confdeltype in ('r', 'a')
            and c.confrelid = {}::regclass
          order by 1",
        catalog::quote_literal(table)
    );
    let rows = session.query_text(&sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            Some((values.next().flatten()?, values.next().flatten()?))
        })
        .collect())
}

/// The planner's row estimate from the top line of `EXPLAIN` (`… rows=12480 width=6)`).
pub fn estimated_rows(plan_first_line: &str) -> Option<u64> {
    let start = plan_first_line.find("rows=")? + "rows=".len();
    let digits: String = plan_first_line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A DELETE or UPDATE node reports `rows=0` for itself; the scan under it holds the estimate.
fn plan_estimate(lines: impl Iterator<Item = String>) -> Option<u64> {
    lines
        .filter_map(|line| estimated_rows(&line))
        .find(|rows| *rows > 0)
}

impl WriteGuard {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection: SavedConnection,
        statement: String,
        risk: Risk,
        in_transaction: bool,
        note: Option<SharedString>,
        session: Option<Arc<Session>>,
        on_decision: OnDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let production = connection.environment == Environment::Prod;
        let mut subscriptions = Vec::new();
        let confirm_input = production.then(|| {
            let input = cx.new(|cx| InputField::new(window, cx, &connection.database));
            let editor = input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |_, cx| cx.notify()).log_err();
                    }
                }),
                window,
                cx,
            ));
            input
        });
        let estimate_task = match session {
            // DDL has nothing to estimate, and EXPLAIN never runs the statement itself.
            Some(session) if risk != Risk::Ddl => {
                let explain = format!("explain {statement}");
                let deleted_table = statement
                    .trim_start()
                    .to_lowercase()
                    .starts_with("delete")
                    .then(|| target_table(&statement))
                    .flatten();
                cx.spawn(async move |this, cx| {
                    let references = match deleted_table {
                        Some(table) => blocking_references(&session, &table)
                            .await
                            .log_err()
                            .unwrap_or_default(),
                        None => Vec::new(),
                    };
                    this.update(cx, |this, _| this.references = references)
                        .log_err();
                    let result = session.run(&explain, 50).await;
                    this.update(cx, |this, cx| {
                        this.estimate = Some(match result {
                            Ok(outcome) => outcome
                                .result_sets
                                .first()
                                .and_then(|result_set| {
                                    plan_estimate(
                                        result_set
                                            .rows
                                            .iter()
                                            .filter_map(|row| row.first().cloned().flatten()),
                                    )
                                })
                                .map(|rows| match rows {
                                    1 => "≈ 1 linha".to_owned(),
                                    rows => format!(
                                        "≈ {} linhas",
                                        crate::panel::format_count(rows as i64)
                                    ),
                                })
                                .ok_or_else(|| "sem estimativa".to_owned()),
                            Err(error) => Err(format!("{error:#}")),
                        });
                        cx.notify();
                    })
                    .log_err();
                })
            }
            _ => Task::ready(()),
        };
        Self {
            focus_handle: cx.focus_handle(),
            connection,
            statement,
            risk,
            in_transaction,
            note,
            estimate: None,
            references: Vec::new(),
            line: None,
            confirm_input,
            on_decision: Some(on_decision),
            _estimate_task: estimate_task,
            _subscriptions: subscriptions,
        }
    }

    pub fn at_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    fn confirmed(&self, cx: &App) -> bool {
        match &self.confirm_input {
            Some(input) => input.read(cx).text(cx).trim() == self.connection.database,
            None => true,
        }
    }

    fn decide(&mut self, decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirmed(cx) {
            return;
        }
        if let Some(on_decision) = self.on_decision.take() {
            on_decision(decision, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.decide(Decision::Run, window, cx);
    }
}

impl Render for WriteGuard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let production = self.connection.environment == Environment::Prod;
        let keyword = self
            .statement
            .split_whitespace()
            .next()
            .unwrap_or("SQL")
            .to_uppercase();
        let title = match self.risk {
            Risk::EveryRow if production => format!("{keyword} sem WHERE em prod"),
            Risk::EveryRow => format!("{keyword} sem WHERE"),
            Risk::Ddl if production => "Mudança de estrutura em prod".to_owned(),
            Risk::Ddl => "Mudança de estrutura".to_owned(),
            Risk::ProductionWrite => "Escrita em prod".to_owned(),
        };
        let table = target_table(&self.statement);
        let reach = match (&self.estimate, self.risk) {
            (_, Risk::Ddl) => (
                "O schema muda para todo mundo que usa este banco.".to_owned(),
                None,
            ),
            (Some(Ok(rows)), Risk::EveryRow) => (
                match (keyword.as_str(), &table) {
                    ("DELETE" | "TRUNCATE", Some(table)) => {
                        format!("Apaga todas as {rows} de {table}")
                    }
                    ("UPDATE", Some(table)) => format!("Altera todas as {rows} de {table}"),
                    _ => format!("Afeta todas as {rows}"),
                },
                Some("estimativa do planner, sem rodar".to_owned()),
            ),
            (Some(Ok(rows)), _) => (
                format!("Afeta {rows}"),
                Some("estimativa do planner, sem rodar".to_owned()),
            ),
            (Some(Err(error)), _) => ("Sem estimativa de linhas".to_owned(), Some(error.clone())),
            (None, Risk::EveryRow) => (
                "Não há WHERE: a tabela inteira é afetada.".to_owned(),
                Some("estimando…".to_owned()),
            ),
            (None, _) => (
                "Esta conexão está marcada como produção.".to_owned(),
                Some("estimando…".to_owned()),
            ),
        };
        let transaction_note: SharedString = match &self.note {
            Some(note) => note.clone(),
            None if self.in_transaction => {
                "Já existe uma transação aberta nesta aba: nada é gravado até o COMMIT.".into()
            }
            None => "você vê o total afetado e decide COMMIT ou ROLLBACK".into(),
        };
        let confirmed = self.confirmed(cx);
        let colors = cx.theme().colors();
        let error = Color::Error.color(cx);
        let preview = self.statement.lines().take(6).collect::<Vec<_>>();
        let consequence = |icon: IconName, color: Color, text: String, detail: Option<String>| {
            h_flex()
                .gap_2p5()
                .items_start()
                .child(
                    div()
                        .pt_0p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(color)),
                )
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(text).size(LabelSize::Small))
                        .children(detail.map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        })),
                )
        };
        let run_label = if self.in_transaction || !production {
            "Rodar"
        } else {
            "Rodar e commitar"
        };
        let actions = h_flex()
            .gap_2()
            .child(
                h_flex()
                    .id("db-guard-cancel")
                    .h(px(30.))
                    .px_3()
                    .gap_2()
                    .cursor_pointer()
                    .child(Label::new("Cancelar").size(LabelSize::Small))
                    .child(
                        Label::new("esc")
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(div().flex_1())
            .when(!self.in_transaction, |this| {
                this.child(
                    secondary_button(
                        "db-guard-transaction",
                        None,
                        "Rodar e decidir depois",
                        confirmed,
                        cx,
                    )
                    .when(confirmed, |this| {
                        this.on_click(cx.listener(|this, _, window, cx| {
                            this.decide(Decision::RunInTransaction, window, cx)
                        }))
                    }),
                )
            })
            .child(
                h_flex()
                    .id("db-guard-run")
                    .h(px(30.))
                    .px_3()
                    .rounded_md()
                    .bg(if confirmed {
                        error.opacity(0.85)
                    } else {
                        error.opacity(0.22)
                    })
                    .when(confirmed, |this| {
                        this.cursor_pointer()
                            .hover(move |style| style.bg(error))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide(Decision::Run, window, cx)
                            }))
                    })
                    .child(
                        Label::new(run_label)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(if confirmed {
                                Color::Custom(gpui::white())
                            } else {
                                Color::Custom(error.opacity(0.75))
                            }),
                    ),
            );
        let (inline_actions, bottom_actions) = if self.confirm_input.is_none() {
            (Some(actions), None)
        } else {
            (None, Some(actions))
        };
        v_flex()
            .key_context("DatabaseWriteGuard")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .w(px(520.))
            .rounded_xl()
            .border_1()
            .border_color(colors.border_selected)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .overflow_hidden()
            .child(
                v_flex()
                    .px_6()
                    .pt(px(22.))
                    .pb(px(18.))
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .size_9()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .rounded_lg()
                                    .bg(error.opacity(0.12))
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::Medium)
                                            .color(Color::Error),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        Label::new(title)
                                            .size(LabelSize::Large)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "{} · {} · {}",
                                            self.connection.name,
                                            self.connection.environment.label(),
                                            self.connection.address()
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .min_h(px(40.))
                            .px_3p5()
                            .py_2()
                            .gap_3()
                            .rounded_lg()
                            .bg(colors.editor_background)
                            .border_1()
                            .border_color(colors.border)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .children(preview.into_iter().map(|line| sql_line(line, cx))),
                            )
                            .when_some(self.line, |this, line| {
                                this.child(
                                    Label::new(format!("linha {line}"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .gap_3()
                            .child(consequence(IconName::Table, Color::Error, reach.0, reach.1))
                            .children(self.references.iter().map(|(table, on_delete)| {
                                consequence(
                                    IconName::Link,
                                    Color::Muted,
                                    format!(
                                        "{table} referencia esta tabela (ON DELETE {on_delete})"
                                    ),
                                    Some("o Postgres pode recusar no meio".to_owned()),
                                )
                            }))
                            .child(consequence(
                                IconName::RotateCcw,
                                Color::Muted,
                                if self.in_transaction {
                                    "Roda dentro da transação aberta".to_owned()
                                } else {
                                    "Pode rodar numa transação aberta".to_owned()
                                },
                                Some(transaction_note.to_string()),
                            )),
                    )
                    .when_some(inline_actions, |this, actions| {
                        this.child(div().pt_1().child(actions))
                    }),
            )
            .when_some(self.confirm_input.clone(), |this, input| {
                this.child(
                    v_flex()
                        .px_6()
                        .pt_4()
                        .pb_5()
                        .gap_2()
                        .border_t_1()
                        .border_color(colors.border)
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new("Digite").size(LabelSize::Small))
                                .child(
                                    Label::new(self.connection.database.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .weight(FontWeight::SEMIBOLD),
                                )
                                .child(Label::new("para liberar").size(LabelSize::Small)),
                        )
                        .child(input)
                        .children(bottom_actions.map(|actions| div().pt_2().child(actions))),
                )
            })
    }
}

/// The table an `UPDATE x`, `DELETE FROM x` or `TRUNCATE x` writes to, as written.
fn target_table(statement: &str) -> Option<String> {
    let mut words = statement.split_whitespace();
    let first = words.next()?.to_lowercase();
    let mut table = match first.as_str() {
        "update" => words.next()?,
        "delete" => {
            words
                .next()
                .filter(|word| word.eq_ignore_ascii_case("from"))?;
            words.next()?
        }
        "truncate" => match words.next()? {
            word if word.eq_ignore_ascii_case("table") => words.next()?,
            word => word,
        },
        _ => return None,
    };
    if table.eq_ignore_ascii_case("only") {
        table = words.next()?;
    }
    let table = table.trim_end_matches(';');
    Some(if table.contains('.') {
        table.to_owned()
    } else {
        format!("public.{table}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_table_a_write_targets() {
        assert_eq!(
            target_table("delete from pix_keys;").as_deref(),
            Some("public.pix_keys")
        );
        assert_eq!(
            target_table("UPDATE audit.events SET payload = null").as_deref(),
            Some("audit.events")
        );
        assert_eq!(
            target_table("truncate table only users").as_deref(),
            Some("public.users")
        );
        assert_eq!(target_table("select 1"), None);
    }

    #[test]
    fn reads_the_planner_estimate() {
        assert_eq!(
            estimated_rows("Delete on pix_keys  (cost=0.00..190.80 rows=12480 width=6)"),
            Some(12480)
        );
        assert_eq!(
            estimated_rows("Result  (cost=0.00..0.01 rows=1 width=0)"),
            Some(1)
        );
        assert_eq!(estimated_rows("Seq Scan on t"), None);
        let plan = [
            "Delete on pix_keys  (cost=0.00..190.80 rows=0 width=0)",
            "  ->  Seq Scan on pix_keys  (cost=0.00..190.80 rows=12480 width=6)",
        ];
        assert_eq!(
            plan_estimate(plan.into_iter().map(str::to_owned)),
            Some(12480)
        );
    }
}
