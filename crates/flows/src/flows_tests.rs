use super::*;
use std::collections::BTreeMap;
use task_agents::TaskAgentSource;

/// The flow drawn in the prototype ("I · Salvar e ver .flow.md").
const FEATURE_DO_ZERO: &str = r#"---
name: "Feature do zero"
input: "Criar um todo list na área logada do app"
steps:
  - agent: produto
    on_fail: { retry: 2, then: ask_you }
  - agent: ux
    where: { tool: pencil, file: "design/app.pen" }
    criteria:
      - text: "Cobre os critérios da spec com uma tela ou estado"
        required: true
        verify: { agent: review }
      - text: "Usa só componentes do design system ou marca o novo"
        required: true
        verify: self
      - text: "Estados vazio, erro e carregando desenhados"
        required: false
        verify: you
    on_fail: { retry: 2, then: ask_you }
    limits: { turns: 30, minutes: 20, usd: 1 }
  - gate: you
    note: "as telas do Designer UX"
  - agent: dev-rn
    criteria:
      - text: "Testes passam"
        required: true
        verify: { command: "npm test" }
  - agent: qa
    instructions: "Teste também com 100 tarefas."
    on_fail: { back_to: dev-rn, max: 2, then: ask_you }
  - agent: docs
  - gate: you
    note: "o PR antes de marcar a tarefa como pronta"
---

Esteira usada para features novas do app.
"#;

fn agent(command: &str, takes: &[&str], gives: &[&str]) -> TaskAgent {
    TaskAgent {
        id: command.to_string(),
        name: command.to_string(),
        command: command.to_string(),
        description: String::new(),
        profile: None,
        model: None,
        subagent_model: None,
        toolkits: Vec::new(),
        tools: BTreeMap::new(),
        permissions: BTreeMap::new(),
        takes: takes.iter().map(|name| name.to_string()).collect(),
        gives: gives.iter().map(|name| name.to_string()).collect(),
        context_servers: Vec::new(),
        instructions: String::new(),
        source: TaskAgentSource::Personal,
        file_path: PathBuf::new(),
    }
}

fn agents() -> Vec<TaskAgent> {
    vec![
        agent("produto", &["pedido"], &["spec.md"]),
        agent("ux", &["spec.md"], &["telas", "tokens"]),
        agent("dev-rn", &["spec.md", "telas"], &["PR"]),
        agent("qa", &["PR", "spec.md"], &["relatório"]),
        agent("docs", &["PR"], &["docs"]),
        agent("review", &["PR"], &["revisão"]),
    ]
}

fn parse(content: &str) -> Flow {
    parse_flow(
        Path::new("/app/.asylum/flows/feature-do-zero.flow.md"),
        content,
        FlowSource::Personal,
    )
    .expect("the example flow parses")
}

#[test]
fn test_parses_the_designed_flow() {
    let flow = parse(FEATURE_DO_ZERO);
    assert_eq!(flow.id, "feature-do-zero");
    assert_eq!(flow.name, "Feature do zero");
    assert_eq!(flow.input, "Criar um todo list na área logada do app");
    assert_eq!(flow.notes, "Esteira usada para features novas do app.");
    let ids: Vec<&str> = flow.steps.iter().map(|step| step.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "produto",
            "ux",
            "aprovacao",
            "dev-rn",
            "qa",
            "docs",
            "aprovacao-2"
        ]
    );

    let StepKind::Agent(ux) = &flow.steps[1].kind else {
        panic!("step 2 is an agent");
    };
    assert_eq!(
        ux.where_,
        Some(DesignTarget {
            tool: DesignTool::Pencil,
            file: Some("design/app.pen".to_string()),
        })
    );
    assert_eq!(ux.criteria.len(), 3);
    assert_eq!(ux.criteria[0].verify, Verifier::Agent("review".to_string()));
    assert_eq!(ux.criteria[1].verify, Verifier::SelfCheck);
    assert_eq!(ux.criteria[2].verify, Verifier::You);
    assert!(!ux.criteria[2].required);
    assert_eq!(ux.on_fail, OnFail::Retry { max: 2 });

    let StepKind::Agent(qa) = &flow.steps[4].kind else {
        panic!("step 5 is an agent");
    };
    assert_eq!(
        qa.on_fail,
        OnFail::BackTo {
            step: "dev-rn".to_string(),
            max: 2,
        }
    );
    assert_eq!(qa.instructions, "Teste também com 100 tarefas.");

    let StepKind::Gate(gate) = &flow.steps[2].kind else {
        panic!("step 3 is a gate");
    };
    assert_eq!(gate.note, "as telas do Designer UX");
    assert!(validate_flow(&flow, &agents()).is_empty());
}

#[test]
fn test_round_trips_through_markdown() {
    let flow = parse(FEATURE_DO_ZERO);
    let markdown = flow.to_markdown().expect("serializes");
    let again = parse(&markdown);
    assert_eq!(again, flow);
    assert!(markdown.contains("Esteira usada para features novas do app."));
}

#[test]
fn test_round_trips_explicit_ids_and_commands() {
    let mut flow = parse(FEATURE_DO_ZERO);
    flow.steps[0].id = "levantar".to_string();
    flow.steps.insert(
        4,
        FlowStep {
            id: "testes".to_string(),
            kind: StepKind::Run(RunStep {
                command: "npm test".to_string(),
                on_fail: OnFail::BackTo {
                    step: "dev-rn".to_string(),
                    max: 1,
                },
            }),
        },
    );
    let again = parse(&flow.to_markdown().expect("serializes"));
    assert_eq!(again, flow);
}

#[test]
fn test_reports_missing_input_when_a_step_moves_before_its_source() {
    let mut flow = parse(FEATURE_DO_ZERO);
    let docs = flow.steps.remove(5);
    flow.steps.insert(1, docs);
    let problems = validate_flow(&flow, &agents());
    assert_eq!(
        problems,
        [FlowProblem::MissingInput {
            step: 1,
            input: "PR".to_string(),
        }]
    );
    assert_eq!(
        problems[0].to_string(),
        "Nenhum passo antes entrega PR. Mova o passo ou adicione quem entrega."
    );
}

#[test]
fn test_reports_invalid_references() {
    let mut flow = parse(FEATURE_DO_ZERO);
    let StepKind::Agent(produto) = &mut flow.steps[0].kind else {
        panic!("step 1 is an agent");
    };
    produto.on_fail = OnFail::BackTo {
        step: "qa".to_string(),
        max: 2,
    };
    produto.criteria.push(Criterion {
        text: String::new(),
        required: true,
        verify: Verifier::Agent("ninguem".to_string()),
    });
    produto.model = Some("opus".to_string());
    let StepKind::Agent(docs) = &mut flow.steps[5].kind else {
        panic!("step 6 is an agent");
    };
    docs.on_fail = OnFail::Continue;
    docs.criteria.push(Criterion {
        text: "Links existem".to_string(),
        required: true,
        verify: Verifier::Command("npm run docs:check".to_string()),
    });
    flow.steps.push(FlowStep {
        id: "dev-rn".to_string(),
        kind: StepKind::Agent(AgentStep::new("inexistente")),
    });

    let problems = validate_flow(&flow, &agents());
    assert!(problems.contains(&FlowProblem::BackToNotEarlier {
        step: 0,
        target: "qa".to_string(),
    }));
    assert!(problems.contains(&FlowProblem::EmptyCriterion {
        step: 0,
        criterion: 0,
    }));
    assert!(problems.contains(&FlowProblem::UnknownVerifierAgent {
        step: 0,
        agent: "ninguem".to_string(),
    }));
    assert!(problems.contains(&FlowProblem::InvalidModel {
        step: 0,
        model: "opus".to_string(),
    }));
    assert!(problems.contains(&FlowProblem::ContinueWithRequiredCriteria { step: 5 }));
    assert!(problems.contains(&FlowProblem::DuplicateStepId {
        step: 7,
        id: "dev-rn".to_string(),
    }));
    assert!(problems.contains(&FlowProblem::UnknownAgent {
        step: 7,
        agent: "inexistente".to_string(),
    }));
}

#[test]
fn test_rejects_malformed_steps() {
    let both = "---\nname: x\nsteps:\n  - agent: ux\n    gate: you\n---\n";
    assert!(parse_flow(Path::new("x.flow.md"), both, FlowSource::Personal).is_err());
    let bad_verify = "---\nname: x\nsteps:\n  - agent: ux\n    criteria:\n      - text: a\n        verify: talvez\n---\n";
    assert!(parse_flow(Path::new("x.flow.md"), bad_verify, FlowSource::Personal).is_err());
    let not_a_flow = "---\nname: x\n---\n";
    assert!(parse_flow(Path::new("x.md"), not_a_flow, FlowSource::Personal).is_err());
}

#[test]
fn test_available_inputs_follow_step_order() {
    let flow = parse(FEATURE_DO_ZERO);
    let names = |index: usize| -> Vec<String> {
        flow.available_inputs(index, &agents())
            .into_iter()
            .map(|input| input.name)
            .collect()
    };
    assert_eq!(names(0), ["pedido"]);
    assert_eq!(names(3), ["pedido", "spec.md", "telas", "tokens"]);
}

#[gpui::test]
async fn test_loads_project_flows(cx: &mut gpui::TestAppContext) {
    let fs = fs::FakeFs::new(cx.executor());
    fs.insert_tree(
        "/app",
        serde_json::json!({
            ".asylum": {
                "flows": {
                    "feature-do-zero.flow.md": FEATURE_DO_ZERO,
                    "quebrada.flow.md": "---\nname: x\nsteps:\n  - gate: talvez\n---\n",
                    "notas.md": "não é esteira",
                }
            }
        }),
    )
    .await;
    let (flows, errors) = load_flows(fs, vec![(Arc::from("app"), PathBuf::from("/app"))]).await;
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].name, "Feature do zero");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].path.ends_with("quebrada.flow.md"));
}

#[test]
fn test_reviews_suggestions() {
    let response = format!("Aqui está:\n```markdown\n{FEATURE_DO_ZERO}```\n");
    let flow = review_suggestion(
        &response,
        "feature-do-zero",
        FlowSource::Personal,
        &agents(),
    )
    .expect("the example passes");
    assert_eq!(flow.steps.len(), 7);

    let problems = review_suggestion(
        "---\nname: x\nsteps:\n  - agent: inventado\n---\n",
        "x",
        FlowSource::Personal,
        &agents(),
    )
    .expect_err("unknown agent");
    assert!(problems[0].starts_with("passo 1:"));
    assert!(
        suggestion_system_prompt(&agents())
            .contains("`ux` · ux · recebe: spec.md · entrega: telas, tokens")
    );
}

#[test]
fn test_parses_branches_and_parallel_steps() {
    let content = "---\nname: x\nsteps:\n  - agent: qa\n  - if: { output: relatório, contains: falhou }\n    then: dev-rn\n  - parallel: [docs, review]\n  - agent: dev-rn\n---\n";
    let flow = parse_flow(Path::new("x.flow.md"), content, FlowSource::Personal).expect("parses");
    assert_eq!(
        flow.steps[1].kind,
        StepKind::Branch(BranchStep {
            output: "relatório".to_string(),
            contains: "falhou".to_string(),
            then_to: Some("dev-rn".to_string()),
            else_to: None,
        })
    );
    assert_eq!(
        flow.steps[2].kind,
        StepKind::Parallel(ParallelStep {
            agents: vec!["docs".to_string(), "review".to_string()],
            read_only: true,
            on_fail: OnFail::AskYou,
        })
    );
    let again = parse_flow(
        Path::new("x.flow.md"),
        &flow.to_markdown().expect("serializes"),
        FlowSource::Personal,
    )
    .expect("parses again");
    assert_eq!(again, flow);
    let mut agents = agents();
    agents.retain(|agent| agent.command != "review");
    let problems = validate_flow(&flow, &agents);
    assert!(problems.contains(&FlowProblem::UnknownAgent {
        step: 2,
        agent: "review".to_string(),
    }));
    assert!(!problems.contains(&FlowProblem::MissingInput {
        step: 1,
        input: "relatório".to_string(),
    }));
}
