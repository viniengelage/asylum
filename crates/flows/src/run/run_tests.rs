use super::*;
use crate::{AgentStep, Criterion, FlowSource, FlowStep, GateStep, OnFail, RunStep, StepKind};
use std::path::PathBuf;

fn agent(id: &str, criteria: Vec<Criterion>, on_fail: OnFail) -> FlowStep {
    let mut step = AgentStep::new(id);
    step.criteria = criteria;
    step.on_fail = on_fail;
    FlowStep {
        id: id.to_string(),
        kind: StepKind::Agent(step),
    }
}

fn gate(id: &str) -> FlowStep {
    FlowStep {
        id: id.to_string(),
        kind: StepKind::Gate(GateStep {
            note: "as telas".to_string(),
            adjust_to: None,
        }),
    }
}

fn criterion(text: &str, required: bool, verify: Verifier) -> Criterion {
    Criterion {
        text: text.to_string(),
        required,
        verify,
    }
}

fn flow(steps: Vec<FlowStep>) -> Flow {
    Flow {
        id: "feature".to_string(),
        name: "Feature do zero".to_string(),
        input: String::new(),
        steps,
        notes: String::new(),
        source: FlowSource::Personal,
        file_path: PathBuf::from("feature.flow.md"),
    }
}

fn submission(outputs: &[&str], self_checks: &[(usize, bool)]) -> Submission {
    Submission {
        summary: String::new(),
        outputs: outputs
            .iter()
            .map(|name| (name.to_string(), "ok".to_string()))
            .collect(),
        self_checks: self_checks
            .iter()
            .map(|(index, passed)| (*index, (*passed, String::new())))
            .collect(),
    }
}

#[test]
fn test_runs_agents_and_gate_in_order() {
    let mut run = FlowRun::new(
        flow(vec![
            agent("produto", vec![], OnFail::default_for_agent()),
            gate("aprovacao"),
            agent("dev", vec![], OnFail::default_for_agent()),
        ]),
        "Criar um todo list".to_string(),
    );
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 0 });
    run.step_started(0);
    assert_eq!(run.next_action(), RunAction::Wait);
    assert!(
        run.agent_submitted(0, "Produto", submission(&["spec.md"], &[]))
            .is_empty()
    );
    assert_eq!(run.steps[0].state, StepState::Passed);
    assert_eq!(
        run.state,
        RunState::WaitingForYou(Waiting::Gate { step: 1 })
    );
    assert_eq!(run.next_action(), RunAction::Wait);

    run.answer_gate(true, "");
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 2 });
    run.step_started(2);
    run.agent_submitted(2, "Dev", submission(&["PR"], &[]));
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.next_action(), RunAction::Done);
    assert_eq!(run.passed_steps(), 3);
}

#[test]
fn test_failed_required_criterion_retries_with_the_reason() {
    let mut run = FlowRun::new(
        flow(vec![agent(
            "ux",
            vec![criterion("Cobre a spec", true, Verifier::SelfCheck)],
            OnFail::Retry { max: 1 },
        )]),
        String::new(),
    );
    run.step_started(0);
    run.agent_submitted(0, "Designer UX", submission(&["telas"], &[(0, false)]));
    assert_eq!(run.steps[0].state, StepState::Failed);
    assert_eq!(run.steps[0].feedback.as_deref(), Some("Cobre a spec"));
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 0 });

    run.step_started(0);
    run.agent_submitted(0, "Designer UX", submission(&["telas"], &[(0, false)]));
    assert!(matches!(
        run.state,
        RunState::WaitingForYou(Waiting::Exhausted { step: 0, .. })
    ));
    run.answer_exhausted(true);
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 0 });
    run.step_started(0);
    run.agent_submitted(0, "Designer UX", submission(&["telas"], &[(0, true)]));
    assert_eq!(run.state, RunState::Succeeded);
}

#[test]
fn test_back_to_requeues_the_steps_in_between_and_respects_max() {
    let mut run = FlowRun::new(
        flow(vec![
            agent("dev", vec![], OnFail::default_for_agent()),
            FlowStep {
                id: "testes".to_string(),
                kind: StepKind::Run(RunStep {
                    command: "npm test".to_string(),
                    on_fail: OnFail::AskYou,
                }),
            },
            agent(
                "qa",
                vec![criterion("Persiste ao reabrir", true, Verifier::SelfCheck)],
                OnFail::BackTo {
                    step: "dev".to_string(),
                    max: 1,
                },
            ),
        ]),
        String::new(),
    );
    run.step_started(0);
    run.agent_submitted(0, "Dev", submission(&["PR"], &[]));
    assert_eq!(run.next_action(), RunAction::RunCommand { step: 1 });
    run.step_started(1);
    run.command_finished(1, true, "saiu 0".to_string());
    run.step_started(2);
    run.agent_submitted(2, "QA", submission(&["relatório"], &[(0, false)]));

    assert_eq!(run.cursor, 0);
    assert_eq!(run.steps[0].state, StepState::Queued);
    assert_eq!(run.steps[1].state, StepState::Queued);
    assert_eq!(run.steps[2].state, StepState::Failed);
    assert_eq!(
        run.steps[0].feedback.as_deref(),
        Some("Persiste ao reabrir")
    );
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 0 });

    run.step_started(0);
    run.agent_submitted(0, "Dev", submission(&["PR"], &[]));
    run.step_started(1);
    run.command_finished(1, true, "saiu 0".to_string());
    run.step_started(2);
    run.agent_submitted(2, "QA", submission(&["relatório"], &[(0, false)]));
    assert!(matches!(
        run.state,
        RunState::WaitingForYou(Waiting::Exhausted { step: 2, .. })
    ));
}

#[test]
fn test_external_and_user_criteria() {
    let mut run = FlowRun::new(
        flow(vec![agent(
            "ux",
            vec![
                criterion("Revisado", true, Verifier::Agent("review".to_string())),
                criterion("Testes", true, Verifier::Command("npm test".to_string())),
                criterion("Estados extras", false, Verifier::You),
            ],
            OnFail::default_for_agent(),
        )]),
        String::new(),
    );
    run.step_started(0);
    let external = run.agent_submitted(0, "Designer UX", submission(&["telas"], &[]));
    assert_eq!(external.len(), 2);
    assert_eq!(run.steps[0].state, StepState::Verifying);

    run.criteria_verified(
        0,
        "Designer UX",
        vec![
            CriterionResult {
                index: 0,
                outcome: CriterionOutcome::Passed,
                evidence: "/review conferiu".to_string(),
            },
            CriterionResult {
                index: 1,
                outcome: CriterionOutcome::Passed,
                evidence: "npm test saiu 0".to_string(),
            },
        ],
    );
    assert_eq!(
        run.state,
        RunState::WaitingForYou(Waiting::Criteria {
            step: 0,
            criteria: vec![2],
        })
    );
    run.answer_criteria(0, "Designer UX", false);
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 0 });
    assert_eq!(run.steps[0].feedback.as_deref(), Some("Estados extras"));

    run.step_started(0);
    run.agent_submitted(0, "Designer UX", submission(&["telas"], &[]));
    run.criteria_verified(
        0,
        "Designer UX",
        vec![
            CriterionResult {
                index: 0,
                outcome: CriterionOutcome::Passed,
                evidence: String::new(),
            },
            CriterionResult {
                index: 1,
                outcome: CriterionOutcome::Passed,
                evidence: String::new(),
            },
        ],
    );
    run.answer_criteria(0, "Designer UX", true);
    assert_eq!(run.state, RunState::Succeeded);
}

#[test]
fn test_gate_adjustment_goes_back_with_the_note() {
    let mut run = FlowRun::new(
        flow(vec![
            agent("produto", vec![], OnFail::default_for_agent()),
            agent("ux", vec![], OnFail::default_for_agent()),
            gate("aprovacao"),
        ]),
        String::new(),
    );
    run.step_started(0);
    run.agent_submitted(0, "Produto", submission(&[], &[]));
    run.step_started(1);
    run.agent_submitted(1, "Designer UX", submission(&[], &[]));
    run.answer_gate(false, "Falta o estado de erro");
    assert_eq!(run.cursor, 1);
    assert_eq!(
        run.steps[1].feedback.as_deref(),
        Some("Falta o estado de erro")
    );
    assert_eq!(run.steps[2].state, StepState::Queued);
    assert_eq!(run.next_action(), RunAction::StartAgent { step: 1 });
}

#[test]
fn test_cancel_stops_everything() {
    let mut run = FlowRun::new(
        flow(vec![agent("produto", vec![], OnFail::default_for_agent())]),
        String::new(),
    );
    run.step_started(0);
    run.cancel("você parou a esteira.");
    assert_eq!(run.state, RunState::Cancelled);
    assert_eq!(run.next_action(), RunAction::Done);
    assert_eq!(
        run.events.last().map(|event| event.kind),
        Some(RunEventKind::Stopped)
    );
}

#[test]
fn test_snapshot_round_trips_and_requeues_the_running_step() {
    let mut run = FlowRun::new(
        flow(vec![
            agent("produto", vec![], OnFail::default_for_agent()),
            agent("dev", vec![], OnFail::default_for_agent()),
        ]),
        "pedido".to_string(),
    );
    run.step_started(0);
    run.agent_submitted(0, "Produto", submission(&["spec.md"], &[]));
    run.step_started(1);
    let json = serde_json::to_string(&run.snapshot()).expect("serializes");
    let snapshot: RunSnapshot = serde_json::from_str(&json).expect("deserializes");
    let restored = FlowRun::restore(run.flow.clone(), snapshot).expect("same steps");
    assert!(restored.was_interrupted());
    assert_eq!(restored.steps[0].state, StepState::Passed);
    assert_eq!(restored.steps[1].state, StepState::Queued);
    assert_eq!(restored.next_action(), RunAction::StartAgent { step: 1 });
    assert_eq!(restored.events, run.events);
}
