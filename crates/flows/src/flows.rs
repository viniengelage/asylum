//! Esteiras: an ordered list of steps (agents, approval gates, commands) that hand work to one
//! another. A flow lives in a `.flow.md` file: the steps in the YAML frontmatter and free notes
//! for people in the body.

pub mod run;

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use task_agents::{ModelReference, TaskAgent, split_frontmatter};

/// Where flows that ship with a project live, relative to a worktree root.
pub const PROJECT_FLOWS_DIR: &str = ".asylum/flows";
pub const FLOW_FILE_SUFFIX: &str = ".flow.md";
pub const MAX_FLOW_FILE_SIZE: u64 = 100 * 1024;
/// Every run's input, available to the first step and every later one.
pub const RUN_INPUT: &str = "pedido";

/// Flows that belong to the user rather than a repository, kept per app profile like personal
/// agents.
pub fn personal_flows_dir() -> PathBuf {
    paths::data_dir().join("flows")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowSource {
    Project { worktree_root_name: Arc<str> },
    Personal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flow {
    /// The file name without `.flow.md`.
    pub id: String,
    pub name: String,
    /// "O que construir": what the last run was asked to do. Each run may change it.
    pub input: String,
    pub steps: Vec<FlowStep>,
    /// The Markdown body, kept for people and never read by the runner.
    pub notes: String,
    pub source: FlowSource,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlowStep {
    /// Stable within the flow; what `back_to` and `adjust_to` point at.
    pub id: String,
    pub kind: StepKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepKind {
    Agent(AgentStep),
    Gate(GateStep),
    Run(RunStep),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStep {
    /// The task agent's command, without the slash.
    pub agent: String,
    /// `provider/model`, over the agent's own model for this flow only.
    pub model: Option<String>,
    /// Added to the agent's instructions for this flow only.
    pub instructions: String,
    pub where_: Option<DesignTarget>,
    pub criteria: Vec<Criterion>,
    pub on_fail: OnFail,
    pub limits: StepLimits,
}

impl AgentStep {
    pub fn new(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            model: None,
            instructions: String::new(),
            where_: None,
            criteria: Vec::new(),
            on_fail: OnFail::default_for_agent(),
            limits: StepLimits::default(),
        }
    }
}

/// "Você aprova": the run pauses until the user approves or asks for an adjustment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GateStep {
    /// What the user approves, e.g. "as telas do Designer UX".
    pub note: String,
    /// The step an adjustment goes back to; the previous agent step when unset.
    pub adjust_to: Option<String>,
}

/// A shell command in the project; it passes when it exits with 0.
#[derive(Debug, Clone, PartialEq)]
pub struct RunStep {
    pub command: String,
    pub on_fail: OnFail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesignTool {
    Pencil,
    Penpot,
    Figma,
    #[serde(alias = "codigo", alias = "código")]
    Code,
}

impl DesignTool {
    pub const ALL: [DesignTool; 4] = [Self::Pencil, Self::Penpot, Self::Figma, Self::Code];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pencil => "Pencil",
            Self::Penpot => "Penpot",
            Self::Figma => "Figma",
            Self::Code => "Só código",
        }
    }
}

/// "Onde acontece".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignTarget {
    pub tool: DesignTool,
    /// A file or link, optionally followed by a page.
    pub file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criterion {
    pub text: String,
    /// A required criterion that fails applies the step's `on_fail`; an optional one becomes a
    /// question for the user.
    pub required: bool,
    pub verify: Verifier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verifier {
    /// The user, in the run panel.
    You,
    /// The step's own agent, in what it submits.
    SelfCheck,
    /// Another task agent, by command.
    Agent(String),
    /// A shell command that must exit with 0.
    Command(String),
}

impl Verifier {
    pub fn label(&self) -> &'static str {
        match self {
            Self::You => "você",
            Self::SelfCheck => "o próprio agente",
            Self::Agent(_) => "outro agente",
            Self::Command(_) => "um comando",
        }
    }
}

/// "Se não passar". Retries and returns end by asking the user once they run out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnFail {
    Retry {
        max: u8,
    },
    BackTo {
        step: String,
        max: u8,
    },
    AskYou,
    /// Moves on; only allowed when every criterion of the step is optional.
    Continue,
}

impl OnFail {
    pub const DEFAULT_MAX: u8 = 2;

    pub fn default_for_agent() -> Self {
        Self::Retry {
            max: Self::DEFAULT_MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepLimits {
    pub turns: u32,
    pub minutes: u32,
    pub usd: f64,
}

impl Default for StepLimits {
    fn default() -> Self {
        Self {
            turns: 30,
            minutes: 20,
            usd: 1.0,
        }
    }
}

impl FlowStep {
    /// The name shown for the step: the agent's name when it is known, else what the step does.
    pub fn label(&self, agents: &[TaskAgent]) -> String {
        match &self.kind {
            StepKind::Agent(step) => agents
                .iter()
                .find(|agent| agent.command == step.agent)
                .map(|agent| agent.name.clone())
                .unwrap_or_else(|| format!("/{}", step.agent)),
            StepKind::Gate(_) => "Você aprova".to_string(),
            StepKind::Run(_) => "Rodar comando".to_string(),
        }
    }

    fn default_id(&self) -> &str {
        match &self.kind {
            StepKind::Agent(step) => &step.agent,
            StepKind::Gate(_) => "aprovacao",
            StepKind::Run(_) => "comando",
        }
    }
}

/// Gives each step an id from what it is (`dev-rn`, `aprovacao`, `comando`), suffixed when it
/// repeats, so a new step never collides with one that exists.
pub fn unique_step_id(steps: &[FlowStep], base: &str) -> String {
    let taken = |candidate: &str| steps.iter().any(|step| step.id == candidate);
    if !taken(base) {
        return base.to_string();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// One thing a step can receive: the run's input or what an earlier agent step delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableInput {
    pub name: String,
    /// The index of the step that delivers it; `None` for the run's input.
    pub from_step: Option<usize>,
}

impl Flow {
    /// What step `index` can receive, in step order.
    pub fn available_inputs(&self, index: usize, agents: &[TaskAgent]) -> Vec<AvailableInput> {
        let mut inputs = vec![AvailableInput {
            name: RUN_INPUT.to_string(),
            from_step: None,
        }];
        for (step_index, step) in self.steps.iter().take(index).enumerate() {
            let StepKind::Agent(agent_step) = &step.kind else {
                continue;
            };
            let Some(agent) = agents
                .iter()
                .find(|agent| agent.command == agent_step.agent)
            else {
                continue;
            };
            for output in &agent.gives {
                inputs.push(AvailableInput {
                    name: output.clone(),
                    from_step: Some(step_index),
                });
            }
        }
        inputs
    }

    pub fn step_index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|step| step.id == id)
    }

    pub fn to_markdown(&self) -> Result<String> {
        let mut auto_ids: Vec<FlowStep> = Vec::new();
        let mut steps = Vec::new();
        for step in &self.steps {
            let auto_id = unique_step_id(&auto_ids, step.default_id());
            auto_ids.push(FlowStep {
                id: auto_id.clone(),
                kind: step.kind.clone(),
            });
            let mut raw = RawStep::from_step(step);
            if step.id != auto_id {
                raw.id = Some(step.id.clone());
            }
            steps.push(raw);
        }
        let frontmatter = RawFlow {
            name: self.name.clone(),
            input: self.input.clone(),
            steps,
        };
        let yaml = serde_yaml_ng::to_string(&frontmatter)?;
        let notes = self.notes.trim();
        Ok(if notes.is_empty() {
            format!("---\n{yaml}---\n")
        } else {
            format!("---\n{yaml}---\n\n{notes}\n")
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RawFlow {
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    input: String,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adjust_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    where_: Option<RawWhere>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    criteria: Vec<RawCriterion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_fail: Option<RawOnFail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limits: Option<RawLimits>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWhere {
    tool: DesignTool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

fn required_default() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCriterion {
    text: String,
    #[serde(default = "required_default", skip_serializing_if = "is_true")]
    required: bool,
    verify: RawVerify,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum RawVerify {
    Word(String),
    Agent { agent: String },
    Command { command: String },
}

fn max_default() -> u8 {
    OnFail::DEFAULT_MAX
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum RawOnFail {
    Word(String),
    Retry {
        retry: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        then: Option<String>,
    },
    BackTo {
        back_to: String,
        #[serde(default = "max_default")]
        max: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        then: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd: Option<f64>,
}

const ASK_YOU: &str = "ask_you";
const CONTINUE: &str = "continue";

impl RawOnFail {
    fn into_on_fail(self) -> Result<OnFail> {
        let check_then = |then: &Option<String>| match then.as_deref() {
            None | Some(ASK_YOU) => Ok(()),
            Some(other) => anyhow::bail!("`then: {other}` não existe; use `then: ask_you`"),
        };
        Ok(match self {
            Self::Word(word) => match word.as_str() {
                ASK_YOU => OnFail::AskYou,
                CONTINUE => OnFail::Continue,
                other => anyhow::bail!("`on_fail: {other}` não existe; use ask_you ou continue"),
            },
            Self::Retry { retry, then } => {
                check_then(&then)?;
                OnFail::Retry { max: retry.max(1) }
            }
            Self::BackTo { back_to, max, then } => {
                check_then(&then)?;
                OnFail::BackTo {
                    step: back_to,
                    max: max.max(1),
                }
            }
        })
    }

    fn from_on_fail(on_fail: &OnFail) -> Self {
        match on_fail {
            OnFail::Retry { max } => Self::Retry {
                retry: *max,
                then: Some(ASK_YOU.to_string()),
            },
            OnFail::BackTo { step, max } => Self::BackTo {
                back_to: step.clone(),
                max: *max,
                then: Some(ASK_YOU.to_string()),
            },
            OnFail::AskYou => Self::Word(ASK_YOU.to_string()),
            OnFail::Continue => Self::Word(CONTINUE.to_string()),
        }
    }
}

impl RawVerify {
    fn into_verifier(self) -> Result<Verifier> {
        Ok(match self {
            Self::Word(word) => match word.as_str() {
                "you" | "voce" | "você" => Verifier::You,
                "self" => Verifier::SelfCheck,
                other => anyhow::bail!(
                    "`verify: {other}` não existe; use you, self, {{ agent }} ou {{ command }}"
                ),
            },
            Self::Agent { agent } => Verifier::Agent(task_agents::command_slug(&agent)),
            Self::Command { command } => Verifier::Command(command),
        })
    }

    fn from_verifier(verifier: &Verifier) -> Self {
        match verifier {
            Verifier::You => Self::Word("you".to_string()),
            Verifier::SelfCheck => Self::Word("self".to_string()),
            Verifier::Agent(agent) => Self::Agent {
                agent: agent.clone(),
            },
            Verifier::Command(command) => Self::Command {
                command: command.clone(),
            },
        }
    }
}

impl RawStep {
    fn into_kind(self) -> Result<StepKind> {
        let kinds = [
            self.agent.is_some(),
            self.gate.is_some(),
            self.run.is_some(),
        ];
        anyhow::ensure!(
            kinds.iter().filter(|is_set| **is_set).count() == 1,
            "cada passo tem exatamente um de `agent`, `gate` ou `run`"
        );
        if let Some(gate) = self.gate {
            anyhow::ensure!(gate == "you", "`gate: {gate}` não existe; use `gate: you`");
            return Ok(StepKind::Gate(GateStep {
                note: self.note.unwrap_or_default(),
                adjust_to: self.adjust_to,
            }));
        }
        if let Some(command) = self.run {
            let on_fail = match self.on_fail {
                Some(on_fail) => on_fail.into_on_fail()?,
                None => OnFail::AskYou,
            };
            return Ok(StepKind::Run(RunStep { command, on_fail }));
        }
        let agent = task_agents::command_slug(self.agent.as_deref().unwrap_or_default());
        anyhow::ensure!(!agent.is_empty(), "passo com `agent` vazio");
        let criteria = self
            .criteria
            .into_iter()
            .map(|criterion| {
                Ok(Criterion {
                    text: criterion.text.trim().to_string(),
                    required: criterion.required,
                    verify: criterion.verify.into_verifier()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let defaults = StepLimits::default();
        let limits = self
            .limits
            .map(|limits| StepLimits {
                turns: limits.turns.unwrap_or(defaults.turns),
                minutes: limits.minutes.unwrap_or(defaults.minutes),
                usd: limits.usd.unwrap_or(defaults.usd),
            })
            .unwrap_or(defaults);
        Ok(StepKind::Agent(AgentStep {
            agent,
            model: self.model.filter(|model| !model.trim().is_empty()),
            instructions: self.instructions.unwrap_or_default().trim().to_string(),
            where_: self.where_.map(|target| DesignTarget {
                tool: target.tool,
                file: target.file.filter(|file| !file.trim().is_empty()),
            }),
            criteria,
            on_fail: match self.on_fail {
                Some(on_fail) => on_fail.into_on_fail()?,
                None => OnFail::default_for_agent(),
            },
            limits,
        }))
    }

    fn from_step(step: &FlowStep) -> Self {
        match &step.kind {
            StepKind::Agent(agent) => {
                let defaults = StepLimits::default();
                let limits = (agent.limits != defaults).then(|| RawLimits {
                    turns: (agent.limits.turns != defaults.turns).then_some(agent.limits.turns),
                    minutes: (agent.limits.minutes != defaults.minutes)
                        .then_some(agent.limits.minutes),
                    usd: (agent.limits.usd != defaults.usd).then_some(agent.limits.usd),
                });
                Self {
                    agent: Some(agent.agent.clone()),
                    model: agent.model.clone(),
                    instructions: (!agent.instructions.is_empty())
                        .then(|| agent.instructions.clone()),
                    where_: agent.where_.as_ref().map(|target| RawWhere {
                        tool: target.tool,
                        file: target.file.clone(),
                    }),
                    criteria: agent
                        .criteria
                        .iter()
                        .map(|criterion| RawCriterion {
                            text: criterion.text.clone(),
                            required: criterion.required,
                            verify: RawVerify::from_verifier(&criterion.verify),
                        })
                        .collect(),
                    on_fail: Some(RawOnFail::from_on_fail(&agent.on_fail)),
                    limits,
                    ..Default::default()
                }
            }
            StepKind::Gate(gate) => Self {
                gate: Some("you".to_string()),
                note: (!gate.note.is_empty()).then(|| gate.note.clone()),
                adjust_to: gate.adjust_to.clone(),
                ..Default::default()
            },
            StepKind::Run(run) => Self {
                run: Some(run.command.clone()),
                on_fail: (run.on_fail != OnFail::AskYou)
                    .then(|| RawOnFail::from_on_fail(&run.on_fail)),
                ..Default::default()
            },
        }
    }
}

/// The flow's id from its file name: `feature-do-zero.flow.md` → `feature-do-zero`.
pub fn flow_id_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let id = file_name.strip_suffix(FLOW_FILE_SUFFIX)?;
    (!id.is_empty()).then(|| id.to_string())
}

pub fn parse_flow(path: &Path, content: &str, source: FlowSource) -> Result<Flow> {
    let id =
        flow_id_from_path(path).context("o arquivo da esteira precisa terminar em .flow.md")?;
    let (frontmatter, body) = split_frontmatter(content)?;
    let raw: RawFlow = match frontmatter {
        Some(yaml) if !yaml.trim().is_empty() => {
            serde_yaml_ng::from_str(yaml).context("YAML da esteira inválido")?
        }
        _ => anyhow::bail!("a esteira não tem frontmatter com `name` e `steps`"),
    };
    let mut steps: Vec<FlowStep> = Vec::new();
    for (index, raw_step) in raw.steps.into_iter().enumerate() {
        let explicit_id = raw_step
            .id
            .as_deref()
            .map(task_agents::command_slug)
            .filter(|id| !id.is_empty());
        let kind = raw_step
            .into_kind()
            .with_context(|| format!("passo {}", index + 1))?;
        let mut step = FlowStep {
            id: String::new(),
            kind,
        };
        step.id = explicit_id.unwrap_or_else(|| unique_step_id(&steps, step.default_id()));
        steps.push(step);
    }
    let name = if raw.name.trim().is_empty() {
        id.clone()
    } else {
        raw.name.trim().to_string()
    };
    Ok(Flow {
        id,
        name,
        input: raw.input.trim().to_string(),
        steps,
        notes: body.trim().to_string(),
        source,
        file_path: path.to_path_buf(),
    })
}

/// Something in a flow that would stop it from running as intended. Shown next to the step it
/// belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowProblem {
    NoSteps,
    DuplicateStepId { step: usize, id: String },
    UnknownAgent { step: usize, agent: String },
    MissingInput { step: usize, input: String },
    BackToNotEarlier { step: usize, target: String },
    AdjustToNotEarlier { step: usize, target: String },
    ContinueWithRequiredCriteria { step: usize },
    EmptyCommand { step: usize },
    EmptyCriterion { step: usize, criterion: usize },
    UnknownVerifierAgent { step: usize, agent: String },
    InvalidModel { step: usize, model: String },
}

impl FlowProblem {
    /// The step the problem belongs to, when it belongs to one.
    pub fn step(&self) -> Option<usize> {
        match self {
            Self::NoSteps => None,
            Self::DuplicateStepId { step, .. }
            | Self::UnknownAgent { step, .. }
            | Self::MissingInput { step, .. }
            | Self::BackToNotEarlier { step, .. }
            | Self::AdjustToNotEarlier { step, .. }
            | Self::ContinueWithRequiredCriteria { step }
            | Self::EmptyCommand { step }
            | Self::EmptyCriterion { step, .. }
            | Self::UnknownVerifierAgent { step, .. }
            | Self::InvalidModel { step, .. } => Some(*step),
        }
    }
}

impl fmt::Display for FlowProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSteps => write!(f, "A esteira não tem passos."),
            Self::DuplicateStepId { id, .. } => write!(f, "Outro passo já usa o id `{id}`."),
            Self::UnknownAgent { agent, .. } => {
                write!(f, "Nenhum agente /{agent} nos seus agentes nem no projeto.")
            }
            Self::MissingInput { input, .. } => write!(
                f,
                "Nenhum passo antes entrega {input}. Mova o passo ou adicione quem entrega."
            ),
            Self::BackToNotEarlier { target, .. } => write!(
                f,
                "“Se não passar” volta para `{target}`, que não é um agente antes deste passo."
            ),
            Self::AdjustToNotEarlier { target, .. } => write!(
                f,
                "Pedir ajuste volta para `{target}`, que não é um passo antes deste."
            ),
            Self::ContinueWithRequiredCriteria { .. } => write!(
                f,
                "“Seguir mesmo assim” só vale quando todos os critérios são opcionais."
            ),
            Self::EmptyCommand { .. } => write!(f, "O comando está vazio."),
            Self::EmptyCriterion { criterion, .. } => {
                write!(f, "O critério {} está vazio.", criterion + 1)
            }
            Self::UnknownVerifierAgent { agent, .. } => {
                write!(f, "O verificador /{agent} não existe.")
            }
            Self::InvalidModel { model, .. } => write!(
                f,
                "O modelo `{model}` precisa ser provedor/modelo, ex.: anthropic/claude-opus-5-5."
            ),
        }
    }
}

pub fn validate_flow(flow: &Flow, agents: &[TaskAgent]) -> Vec<FlowProblem> {
    let mut problems = Vec::new();
    if flow.steps.is_empty() {
        problems.push(FlowProblem::NoSteps);
    }
    let known_agent = |command: &str| agents.iter().any(|agent| agent.command == command);
    let mut seen_ids = BTreeSet::new();
    for (index, step) in flow.steps.iter().enumerate() {
        if !seen_ids.insert(step.id.as_str()) {
            problems.push(FlowProblem::DuplicateStepId {
                step: index,
                id: step.id.clone(),
            });
        }
        let earlier_agent = |target: &str| {
            flow.steps[..index]
                .iter()
                .any(|earlier| earlier.id == target && matches!(earlier.kind, StepKind::Agent(_)))
        };
        let check_back_to = |on_fail: &OnFail, problems: &mut Vec<FlowProblem>| {
            if let OnFail::BackTo { step: target, .. } = on_fail
                && !earlier_agent(target)
            {
                problems.push(FlowProblem::BackToNotEarlier {
                    step: index,
                    target: target.clone(),
                });
            }
        };
        match &step.kind {
            StepKind::Agent(agent_step) => {
                match agents
                    .iter()
                    .find(|agent| agent.command == agent_step.agent)
                {
                    None => problems.push(FlowProblem::UnknownAgent {
                        step: index,
                        agent: agent_step.agent.clone(),
                    }),
                    Some(agent) => {
                        let available = flow.available_inputs(index, agents);
                        for input in &agent.takes {
                            if !available.iter().any(|available| &available.name == input) {
                                problems.push(FlowProblem::MissingInput {
                                    step: index,
                                    input: input.clone(),
                                });
                            }
                        }
                    }
                }
                if let Some(model) = &agent_step.model
                    && ModelReference::parse(model).is_none()
                {
                    problems.push(FlowProblem::InvalidModel {
                        step: index,
                        model: model.clone(),
                    });
                }
                for (criterion_index, criterion) in agent_step.criteria.iter().enumerate() {
                    if criterion.text.is_empty() {
                        problems.push(FlowProblem::EmptyCriterion {
                            step: index,
                            criterion: criterion_index,
                        });
                    }
                    if let Verifier::Agent(verifier) = &criterion.verify
                        && !known_agent(verifier)
                    {
                        problems.push(FlowProblem::UnknownVerifierAgent {
                            step: index,
                            agent: verifier.clone(),
                        });
                    }
                }
                if agent_step.on_fail == OnFail::Continue
                    && agent_step
                        .criteria
                        .iter()
                        .any(|criterion| criterion.required)
                {
                    problems.push(FlowProblem::ContinueWithRequiredCriteria { step: index });
                }
                check_back_to(&agent_step.on_fail, &mut problems);
            }
            StepKind::Gate(gate) => {
                if let Some(target) = &gate.adjust_to
                    && !flow.steps[..index]
                        .iter()
                        .any(|earlier| &earlier.id == target)
                {
                    problems.push(FlowProblem::AdjustToNotEarlier {
                        step: index,
                        target: target.clone(),
                    });
                }
            }
            StepKind::Run(run) => {
                if run.command.trim().is_empty() {
                    problems.push(FlowProblem::EmptyCommand { step: index });
                }
                check_back_to(&run.on_fail, &mut problems);
            }
        }
    }
    problems
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowLoadError {
    pub path: PathBuf,
    pub message: String,
}

async fn load_flows_from_directory(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    source: FlowSource,
) -> Vec<Result<Flow, FlowLoadError>> {
    let Ok(mut entries) = fs.read_dir(directory).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else {
            continue;
        };
        if flow_id_from_path(&path).is_some() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut results = Vec::new();
    for path in paths {
        let load_error = |message: String| FlowLoadError {
            path: path.clone(),
            message,
        };
        match fs.metadata(&path).await {
            Ok(Some(metadata)) if metadata.is_dir => continue,
            Ok(Some(metadata)) if metadata.len > MAX_FLOW_FILE_SIZE => {
                results.push(Err(load_error(format!(
                    "a esteira passa de {}KB",
                    MAX_FLOW_FILE_SIZE / 1024
                ))));
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                results.push(Err(load_error(error.to_string())));
                continue;
            }
        }
        let result = match fs.load(&path).await {
            Ok(content) => parse_flow(&path, &content, source.clone())
                .map_err(|error| load_error(format!("{error:#}"))),
            Err(error) => Err(load_error(error.to_string())),
        };
        results.push(result);
    }
    results
}

/// Loads the flows of a set of worktree roots, then the personal ones.
pub async fn load_flows(
    fs: Arc<dyn Fs>,
    worktree_roots: Vec<(Arc<str>, PathBuf)>,
) -> (Vec<Flow>, Vec<FlowLoadError>) {
    let mut results = Vec::new();
    for (root_name, root) in &worktree_roots {
        results.extend(
            load_flows_from_directory(
                &fs,
                &root.join(PROJECT_FLOWS_DIR),
                FlowSource::Project {
                    worktree_root_name: root_name.clone(),
                },
            )
            .await,
        );
    }
    results
        .extend(load_flows_from_directory(&fs, &personal_flows_dir(), FlowSource::Personal).await);
    let mut flows = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(flow) => flows.push(flow),
            Err(error) => errors.push(error),
        }
    }
    (flows, errors)
}

/// Bumped whenever a flow file is written from the app, so views that list flows reload.
#[derive(Default)]
pub struct FlowsRevision(pub usize);

impl gpui::Global for FlowsRevision {}

pub fn notify_flows_changed(cx: &mut gpui::App) {
    cx.default_global::<FlowsRevision>().0 += 1;
}

#[cfg(test)]
mod flows_tests;
