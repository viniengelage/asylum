//! Asks before a statement that could do real damage: every row of a table, a schema change, or
//! any write on production. Production also asks for the database name to be typed.

use crate::{
    connection::{Environment, SavedConnection},
    session::Session,
    statements::Risk,
};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task, px,
};
use std::sync::Arc;
use ui::{TintColor, prelude::*};
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
                cx.spawn(async move |this, cx| {
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
            confirm_input,
            on_decision: Some(on_decision),
            _estimate_task: estimate_task,
            _subscriptions: subscriptions,
        }
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
        let title = match self.risk {
            Risk::EveryRow if production => "Escrita em todas as linhas, em prod",
            Risk::EveryRow => "Escrita em todas as linhas",
            Risk::Ddl if production => "Mudança de estrutura em prod",
            Risk::Ddl => "Mudança de estrutura",
            Risk::ProductionWrite => "Escrita em prod",
        };
        let reason = match self.risk {
            Risk::EveryRow => "Não há WHERE: a tabela inteira é afetada.",
            Risk::Ddl => "O schema muda para todo mundo que usa este banco.",
            Risk::ProductionWrite => "Esta conexão está marcada como produção.",
        };
        let confirmed = self.confirmed(cx);
        let preview = self
            .statement
            .lines()
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        v_flex()
            .key_context("DatabaseWriteGuard")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .w(px(520.))
            .elevation_3(cx)
            .p_4()
            .gap_3()
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
                            .rounded_md()
                            .bg(Color::Error.color(cx).opacity(0.12))
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Medium)
                                    .color(Color::Error),
                            ),
                    )
                    .child(
                        v_flex()
                            .child(Label::new(title).weight(FontWeight::SEMIBOLD))
                            .child(
                                Label::new(format!(
                                    "{} · {} · {}",
                                    self.connection.name,
                                    self.connection.environment.label(),
                                    self.connection.address()
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .p_2p5()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .children(preview.lines().map(|line| {
                        Label::new(line.to_owned())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                    })),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(Label::new(reason).size(LabelSize::Small)),
                    )
                    .when_some(self.estimate.clone(), |this, estimate| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Table)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(match estimate {
                                        Ok(rows) => {
                                            format!(
                                                "{rows}, pela estimativa do planner (sem rodar)"
                                            )
                                        }
                                        Err(error) => format!("Sem estimativa: {error}"),
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::RotateCcw)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(match &self.note {
                                    Some(note) => note.clone(),
                                    None if self.in_transaction => {
                                        "Já existe uma transação aberta nesta aba: nada é gravado \
                                         até o COMMIT."
                                            .into()
                                    }
                                    None => "Numa transação você vê quantas linhas mudaram antes \
                                             de decidir entre COMMIT e ROLLBACK."
                                        .into(),
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .when_some(self.confirm_input.clone(), |this, input| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(format!("Digite {} para liberar", self.connection.database))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(input),
                )
            })
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(
                        Button::new("db-guard-cancel", "Cancelar")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(div().flex_1())
                    .when(!self.in_transaction, |this| {
                        this.child(
                            Button::new("db-guard-transaction", "Rodar numa transação")
                                .style(ButtonStyle::Outlined)
                                .disabled(!confirmed)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.decide(Decision::RunInTransaction, window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(
                            "db-guard-run",
                            if self.in_transaction || !production {
                                "Rodar"
                            } else {
                                "Rodar e commitar"
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .disabled(!confirmed)
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.decide(Decision::Run, window, cx)
                            }),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
