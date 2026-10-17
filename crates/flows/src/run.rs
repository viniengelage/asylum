//! The state of one run of a flow. Pure: the app's runner performs each [`RunAction`] (start an
//! agent thread, run a command, verify criteria) and reports back through the `FlowRun` methods,
//! which decide what comes next, including retries, returns to an earlier step and pauses for the
//! user.

use crate::{Criterion, Flow, OnFail, StepKind, Verifier};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunState {
    Running,
    WaitingForYou(Waiting),
    Succeeded,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Waiting {
    /// A "você aprova" step.
    Gate { step: usize },
    /// Criteria verified by the user.
    Criteria { step: usize, criteria: Vec<usize> },
    /// The step failed and has no retries or returns left.
    Exhausted { step: usize, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Queued,
    Running,
    Verifying,
    WaitingForYou,
    Passed,
    /// Failed and was sent back; it runs again when the run reaches it.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CriterionOutcome {
    Passed,
    Failed,
    /// Waiting for the user.
    WithYou,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionResult {
    pub index: usize,
    pub outcome: CriterionOutcome,
    /// What proves the outcome: a command's exit, the agent's own note, the verifier's reply.
    pub evidence: String,
}

/// What an agent step delivered through `submit_step`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub summary: String,
    pub outputs: BTreeMap<String, String>,
    /// The agent's own check of each criterion it verifies, by criterion index.
    pub self_checks: BTreeMap<usize, (bool, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    pub state: StepState,
    /// How many times the step started in this run.
    pub attempts: u8,
    pub submission: Option<Submission>,
    pub criteria: Vec<CriterionResult>,
    /// Why the step runs again: failed criteria or the user's adjustment note.
    pub feedback: Option<String>,
    /// The step's last outcome note, e.g. "saiu 0" or "aprovado".
    pub note: Option<String>,
}

impl StepRun {
    fn new() -> Self {
        Self {
            state: StepState::Queued,
            attempts: 0,
            submission: None,
            criteria: Vec::new(),
            feedback: None,
            note: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunEventKind {
    You,
    Delivered,
    Verified,
    Failed,
    Back,
    Gate,
    Finished,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub kind: RunEventKind,
    pub title: String,
    pub message: String,
    pub step: Option<usize>,
    pub at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAction {
    StartAgent {
        step: usize,
    },
    RunCommand {
        step: usize,
    },
    /// Nothing to start: the run waits for the user or for work already started.
    Wait,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlowRun {
    /// The flow as it was when the run started; editing the file doesn't change a running flow.
    pub flow: Flow,
    pub input: String,
    pub steps: Vec<StepRun>,
    pub cursor: usize,
    pub state: RunState,
    pub events: Vec<RunEvent>,
    /// Returns taken from each step, by step index.
    backs: BTreeMap<usize, u8>,
    /// Retries taken by each step since it last started fresh.
    retries: BTreeMap<usize, u8>,
}

/// Everything a run knows besides its flow, to keep it across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub input: String,
    pub steps: Vec<StepRun>,
    pub cursor: usize,
    pub state: RunState,
    pub events: Vec<RunEvent>,
    backs: BTreeMap<usize, u8>,
    retries: BTreeMap<usize, u8>,
}

impl FlowRun {
    pub fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            input: self.input.clone(),
            steps: self.steps.clone(),
            cursor: self.cursor,
            state: self.state.clone(),
            events: self.events.clone(),
            backs: self.backs.clone(),
            retries: self.retries.clone(),
        }
    }

    /// Brings back a run saved with [`FlowRun::snapshot`]. A step that was working when the app
    /// closed lost its thread's turn, so it runs again when the run resumes.
    pub fn restore(flow: Flow, snapshot: RunSnapshot) -> Option<Self> {
        if snapshot.steps.len() != flow.steps.len() {
            return None;
        }
        let mut run = Self {
            flow,
            input: snapshot.input,
            steps: snapshot.steps,
            cursor: snapshot.cursor,
            state: snapshot.state,
            events: snapshot.events,
            backs: snapshot.backs,
            retries: snapshot.retries,
        };
        for step in &mut run.steps {
            if matches!(step.state, StepState::Running | StepState::Verifying) {
                step.state = StepState::Queued;
            }
        }
        Some(run)
    }

    /// Continues a restored run.
    pub fn resume(&mut self) {
        if self.state == RunState::Running {
            self.log(
                RunEventKind::You,
                "Você retomou",
                "a esteira continua do passo em que parou.",
                None,
            );
        }
    }

    /// Whether the run stopped midway because the app closed, rather than finishing or waiting.
    pub fn was_interrupted(&self) -> bool {
        self.state == RunState::Running
    }

    pub fn new(flow: Flow, input: String) -> Self {
        let steps = flow.steps.iter().map(|_| StepRun::new()).collect();
        let mut run = Self {
            flow,
            input,
            steps,
            cursor: 0,
            state: RunState::Running,
            events: Vec::new(),
            backs: BTreeMap::new(),
            retries: BTreeMap::new(),
        };
        let message = format!("rodou “{}” com: {}", run.flow.name, run.input);
        run.log(RunEventKind::You, "Você", message, None);
        run.enter_cursor();
        run
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, RunState::Succeeded | RunState::Cancelled)
    }

    pub fn passed_steps(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.state == StepState::Passed)
            .count()
    }

    fn log(
        &mut self,
        kind: RunEventKind,
        title: impl Into<String>,
        message: impl Into<String>,
        step: Option<usize>,
    ) {
        self.events.push(RunEvent {
            kind,
            title: title.into(),
            message: message.into(),
            step,
            at: SystemTime::now(),
        });
    }

    fn step_title(&self, step: usize, label: &str) -> String {
        format!("Passo {} · {label}", step + 1)
    }

    /// Starts gates as soon as the run reaches them; agents and commands wait for the runner.
    fn enter_cursor(&mut self) {
        if self.cursor >= self.flow.steps.len() {
            self.state = RunState::Succeeded;
            self.log(
                RunEventKind::Finished,
                "Esteira concluída",
                "tudo entregue; revisão e merge ficam com você.",
                None,
            );
            return;
        }
        let step = self.cursor;
        if let StepKind::Gate(gate) = &self.flow.steps[step].kind {
            let note = if gate.note.is_empty() {
                "o que o passo anterior entregou".to_string()
            } else {
                gate.note.clone()
            };
            self.steps[step].state = StepState::WaitingForYou;
            self.state = RunState::WaitingForYou(Waiting::Gate { step });
            let title = self.step_title(step, "Você aprova");
            self.log(RunEventKind::Gate, title, note, Some(step));
        } else {
            self.state = RunState::Running;
        }
    }

    pub fn next_action(&self) -> RunAction {
        match &self.state {
            RunState::Succeeded | RunState::Cancelled => RunAction::Done,
            RunState::WaitingForYou(_) => RunAction::Wait,
            RunState::Running => {
                let step = self.cursor;
                match self.steps.get(step).map(|step| step.state) {
                    Some(StepState::Queued | StepState::Failed) => {
                        match &self.flow.steps[step].kind {
                            StepKind::Agent(_) => RunAction::StartAgent { step },
                            StepKind::Run(_) => RunAction::RunCommand { step },
                            StepKind::Gate(_) => RunAction::Wait,
                        }
                    }
                    _ => RunAction::Wait,
                }
            }
        }
    }

    /// The runner started the step's agent thread or command.
    pub fn step_started(&mut self, step: usize) {
        if let Some(run) = self.steps.get_mut(step) {
            run.state = StepState::Running;
            run.attempts += 1;
            run.submission = None;
            run.criteria.clear();
            run.note = None;
        }
    }

    /// The agent called `submit_step`. Returns the criteria the runner verifies itself: by
    /// command and by another agent. Self checks come from the submission; the user's come
    /// from the run panel.
    pub fn agent_submitted(
        &mut self,
        step: usize,
        label: &str,
        submission: Submission,
    ) -> Vec<(usize, Verifier)> {
        let StepKind::Agent(agent_step) = &self.flow.steps[step].kind else {
            return Vec::new();
        };
        let criteria = agent_step.criteria.clone();
        let delivered = if submission.outputs.is_empty() {
            submission.summary.clone()
        } else {
            format!(
                "entregou {}",
                submission
                    .outputs
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" + ")
            )
        };
        let title = self.step_title(step, label);
        self.log(RunEventKind::Delivered, title, delivered, Some(step));
        let run = &mut self.steps[step];
        run.state = StepState::Verifying;
        run.feedback = None;
        let mut external = Vec::new();
        run.criteria = criteria
            .iter()
            .enumerate()
            .map(|(index, criterion)| match &criterion.verify {
                Verifier::SelfCheck => match submission.self_checks.get(&index) {
                    Some((passed, note)) => CriterionResult {
                        index,
                        outcome: if *passed {
                            CriterionOutcome::Passed
                        } else {
                            CriterionOutcome::Failed
                        },
                        evidence: if note.is_empty() {
                            "autoavaliação no submit".to_string()
                        } else {
                            note.clone()
                        },
                    },
                    None => CriterionResult {
                        index,
                        outcome: CriterionOutcome::Failed,
                        evidence: "o agente não avaliou este critério no submit".to_string(),
                    },
                },
                Verifier::You => CriterionResult {
                    index,
                    outcome: CriterionOutcome::WithYou,
                    evidence: "com você".to_string(),
                },
                verifier @ (Verifier::Agent(_) | Verifier::Command(_)) => {
                    external.push((index, verifier.clone()));
                    CriterionResult {
                        index,
                        outcome: CriterionOutcome::WithYou,
                        evidence: "verificando".to_string(),
                    }
                }
            })
            .collect();
        run.submission = Some(submission);
        if external.is_empty() {
            self.settle_criteria(step, label);
        }
        external
    }

    /// The runner finished verifying the criteria it was given by `agent_submitted`.
    pub fn criteria_verified(&mut self, step: usize, label: &str, results: Vec<CriterionResult>) {
        let run = &mut self.steps[step];
        for result in results {
            if let Some(existing) = run
                .criteria
                .iter_mut()
                .find(|existing| existing.index == result.index)
            {
                *existing = result;
            }
        }
        self.settle_criteria(step, label);
    }

    fn criterion(&self, step: usize, index: usize) -> Option<&Criterion> {
        match &self.flow.steps[step].kind {
            StepKind::Agent(agent_step) => agent_step.criteria.get(index),
            _ => None,
        }
    }

    fn settle_criteria(&mut self, step: usize, label: &str) {
        let results = self.steps[step].criteria.clone();
        let passed = results
            .iter()
            .filter(|result| result.outcome == CriterionOutcome::Passed)
            .count();
        let failed_required: Vec<String> = results
            .iter()
            .filter(|result| result.outcome == CriterionOutcome::Failed)
            .filter_map(|result| self.criterion(step, result.index))
            .filter(|criterion| criterion.required)
            .map(|criterion| criterion.text.clone())
            .collect();
        let with_you: Vec<usize> = results
            .iter()
            .filter(|result| result.outcome == CriterionOutcome::WithYou)
            .map(|result| result.index)
            .collect();
        if !results.is_empty() {
            let kind = if failed_required.is_empty() {
                RunEventKind::Verified
            } else {
                RunEventKind::Failed
            };
            self.log(
                kind,
                format!("Critérios · {label}"),
                format!("{passed} de {} conferidos", results.len()),
                Some(step),
            );
        }
        if !failed_required.is_empty() {
            self.fail_step(step, label, failed_required.join("; "));
        } else if !with_you.is_empty() {
            self.steps[step].state = StepState::WaitingForYou;
            self.state = RunState::WaitingForYou(Waiting::Criteria {
                step,
                criteria: with_you,
            });
        } else {
            self.pass_step(step, None);
        }
    }

    fn pass_step(&mut self, step: usize, note: Option<String>) {
        self.steps[step].state = StepState::Passed;
        if note.is_some() {
            self.steps[step].note = note;
        }
        self.retries.remove(&step);
        self.cursor = step + 1;
        self.enter_cursor();
    }

    /// Applies the step's "se não passar".
    fn fail_step(&mut self, step: usize, label: &str, reason: String) {
        let on_fail = match &self.flow.steps[step].kind {
            StepKind::Agent(agent_step) => agent_step.on_fail.clone(),
            StepKind::Run(run) => run.on_fail.clone(),
            StepKind::Gate(_) => OnFail::AskYou,
        };
        match on_fail {
            OnFail::Continue => {
                self.pass_step(step, Some("seguiu mesmo assim".to_string()));
            }
            OnFail::Retry { max } => {
                let taken = self.retries.entry(step).or_default();
                if *taken >= max {
                    self.exhaust(step, label, reason);
                    return;
                }
                *taken += 1;
                let count = *taken;
                self.steps[step].state = StepState::Failed;
                self.steps[step].feedback = Some(reason.clone());
                self.steps[step].note = Some(format!("refazendo {count} de {max}"));
                self.log(
                    RunEventKind::Back,
                    format!("Refazendo {label}"),
                    format!("{count} de {max} · motivo: {reason}"),
                    Some(step),
                );
                self.cursor = step;
                self.state = RunState::Running;
            }
            OnFail::BackTo { step: target, max } => {
                let Some(target_index) = self.flow.step_index(&target) else {
                    self.exhaust(step, label, reason);
                    return;
                };
                let taken = self.backs.entry(step).or_default();
                if *taken >= max {
                    self.exhaust(step, label, reason);
                    return;
                }
                *taken += 1;
                let count = *taken;
                self.steps[step].state = StepState::Failed;
                self.steps[step].note = Some(format!("volta {count} de {max}"));
                for later in target_index..step {
                    self.steps[later].state = StepState::Queued;
                }
                self.steps[target_index].feedback = Some(reason.clone());
                self.log(
                    RunEventKind::Back,
                    format!("{label} → {}", self.target_label(target_index)),
                    format!("volta {count} de {max} · motivo: {reason}"),
                    Some(step),
                );
                self.cursor = target_index;
                self.state = RunState::Running;
            }
            OnFail::AskYou => self.exhaust(step, label, reason),
        }
    }

    fn target_label(&self, step: usize) -> String {
        match &self.flow.steps[step].kind {
            StepKind::Agent(agent_step) => format!("/{}", agent_step.agent),
            StepKind::Gate(_) => "Você aprova".to_string(),
            StepKind::Run(run) => run.command.clone(),
        }
    }

    fn exhaust(&mut self, step: usize, label: &str, reason: String) {
        self.steps[step].state = StepState::WaitingForYou;
        self.steps[step].note = Some("reprovado".to_string());
        self.log(
            RunEventKind::Failed,
            "Esperando você",
            format!("{label}: “{reason}” reprovou. Decida como seguir."),
            Some(step),
        );
        self.state = RunState::WaitingForYou(Waiting::Exhausted { step, reason });
    }

    /// The agent step ended with an error, or without calling `submit_step` after a reminder.
    pub fn step_errored(&mut self, step: usize, label: &str, error: String) {
        self.fail_step(step, label, error);
    }

    pub fn command_finished(&mut self, step: usize, passed: bool, evidence: String) {
        let label = self.target_label(step);
        let title = self.step_title(step, &label);
        if passed {
            self.log(RunEventKind::Delivered, title, evidence.clone(), Some(step));
            self.pass_step(step, Some(evidence));
        } else {
            self.log(RunEventKind::Failed, title, evidence.clone(), Some(step));
            self.fail_step(step, &label, evidence);
        }
    }

    /// The user answered the gate the run waits on. An adjustment goes back with the note.
    pub fn answer_gate(&mut self, approve: bool, note: &str) {
        let RunState::WaitingForYou(Waiting::Gate { step }) = self.state.clone() else {
            return;
        };
        if approve {
            let message = match &self.flow.steps[step].kind {
                StepKind::Gate(gate) => gate.note.clone(),
                _ => String::new(),
            };
            self.log(RunEventKind::You, "Você aprovou", message, Some(step));
            self.pass_step(step, Some("aprovado".to_string()));
            return;
        }
        let target = match &self.flow.steps[step].kind {
            StepKind::Gate(gate) => gate
                .adjust_to
                .as_deref()
                .and_then(|id| self.flow.step_index(id)),
            _ => None,
        }
        .or_else(|| {
            (0..step)
                .rev()
                .find(|index| matches!(self.flow.steps[*index].kind, StepKind::Agent(_)))
        });
        let Some(target) = target else {
            self.cancel("não há passo antes do portão para ajustar.");
            return;
        };
        for later in target..=step {
            self.steps[later].state = StepState::Queued;
        }
        let note = note.trim();
        self.steps[target].feedback = Some(if note.is_empty() {
            "Você pediu um ajuste sem detalhar; revise o que entregou.".to_string()
        } else {
            note.to_string()
        });
        let label = self.target_label(target);
        self.log(
            RunEventKind::Back,
            format!("Ajuste → {label}"),
            if note.is_empty() { "sem texto" } else { note },
            Some(step),
        );
        self.cursor = target;
        self.state = RunState::Running;
    }

    /// The user accepted or refused the criteria waiting on them.
    pub fn answer_criteria(&mut self, step: usize, label: &str, accept: bool) {
        let RunState::WaitingForYou(Waiting::Criteria {
            step: waiting_step,
            criteria,
        }) = self.state.clone()
        else {
            return;
        };
        if waiting_step != step {
            return;
        }
        for index in &criteria {
            if let Some(result) = self.steps[step]
                .criteria
                .iter_mut()
                .find(|result| result.index == *index)
            {
                result.outcome = if accept {
                    CriterionOutcome::Passed
                } else {
                    CriterionOutcome::Failed
                };
                result.evidence = if accept {
                    "você aceitou".to_string()
                } else {
                    "você recusou".to_string()
                };
            }
        }
        if accept {
            self.log(
                RunEventKind::You,
                "Você aceitou",
                label.to_string(),
                Some(step),
            );
            self.state = RunState::Running;
            self.pass_step(step, None);
            return;
        }
        let texts: Vec<String> = criteria
            .iter()
            .filter_map(|index| self.criterion(step, *index))
            .map(|criterion| criterion.text.clone())
            .collect();
        self.log(
            RunEventKind::Back,
            format!("Você pediu: refazer {label}"),
            texts.join("; "),
            Some(step),
        );
        self.state = RunState::Running;
        // Refusing an optional criterion asks for a redo even when the step would let it pass.
        let any_required = criteria
            .iter()
            .filter_map(|index| self.criterion(step, *index))
            .any(|criterion| criterion.required);
        if any_required {
            self.fail_step(step, label, texts.join("; "));
        } else {
            self.steps[step].state = StepState::Failed;
            self.steps[step].feedback = Some(texts.join("; "));
            self.cursor = step;
        }
    }

    /// The user decided what to do with a step that ran out of retries.
    pub fn answer_exhausted(&mut self, retry: bool) {
        let RunState::WaitingForYou(Waiting::Exhausted { step, reason }) = self.state.clone()
        else {
            return;
        };
        if retry {
            self.retries.remove(&step);
            self.backs.remove(&step);
            self.steps[step].state = StepState::Failed;
            self.steps[step].feedback = Some(reason);
            self.log(
                RunEventKind::You,
                "Você mandou refazer",
                String::new(),
                Some(step),
            );
            self.cursor = step;
            self.state = RunState::Running;
        } else {
            self.cancel("você encerrou a esteira depois da reprovação.");
        }
    }

    pub fn cancel(&mut self, reason: &str) {
        if self.is_finished() {
            return;
        }
        self.state = RunState::Cancelled;
        self.log(RunEventKind::Stopped, "Parado", reason.to_string(), None);
    }
}

#[cfg(test)]
mod run_tests;
