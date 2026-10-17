//! Prompts for "Sugerir passos": the model writes a `.flow.md` with the user's own agents, and
//! what the parser or the validation rejects goes back to it until the flow passes.

use crate::{FLOW_FILE_SUFFIX, Flow, FlowSource, parse_flow, validate_flow};
use std::path::Path;
use task_agents::TaskAgent;

pub fn suggestion_system_prompt(agents: &[TaskAgent]) -> String {
    let mut prompt = String::from(
        "Você monta esteiras de agentes do Asylum. Uma esteira é um arquivo .flow.md: frontmatter YAML \
         entre `---` e, depois, notas livres. Responda só com o arquivo, num bloco ```markdown.\n\n\
         ## Formato\n\
         ```\n---\nname: \"Nome curto\"\ninput: \"o pedido de exemplo\"\nsteps:\n  - agent: <comando>\n    \
         instructions: \"só se o pedido exigir algo além das instruções do agente\"\n    criteria:\n      \
         - text: \"critério verificável\"\n        required: true\n        verify: self   # ou you, \
         { agent: <comando> }, { command: \"npm test\" }\n    on_fail: { retry: 2, then: ask_you }   # ou \
         { back_to: <id de um passo anterior>, max: 2, then: ask_you }, ask_you, continue\n  \
         - gate: you\n    note: \"o que a pessoa aprova\"\n  - run: \"npm test\"\n    on_fail: ask_you\n---\n```\n\n\
         ## Regras\n\
         - Use só os agentes da lista, pelo comando exato. Não invente agentes.\n\
         - Cada agente precisa receber o que pede (`recebe`): ponha antes quem entrega isso, ou o pedido.\n\
         - Ponha um portão (`gate: you`) onde a pessoa deve olhar antes de seguir, como depois de telas ou antes do merge.\n\
         - `back_to` só aponta para um passo anterior; o id de um passo é o comando do agente.\n\
         - Critérios curtos e verificáveis; `continue` só em passos sem critério obrigatório.\n\n\
         ## Agentes disponíveis\n",
    );
    if agents.is_empty() {
        prompt.push_str("(nenhum; use só portões e comandos)\n");
    }
    for agent in agents {
        prompt.push_str(&format!("- `{}` · {}", agent.command, agent.name));
        if !agent.description.is_empty() {
            prompt.push_str(&format!(" — {}", agent.description));
        }
        if !agent.takes.is_empty() {
            prompt.push_str(&format!(" · recebe: {}", agent.takes.join(", ")));
        }
        if !agent.gives.is_empty() {
            prompt.push_str(&format!(" · entrega: {}", agent.gives.join(", ")));
        }
        prompt.push('\n');
    }
    prompt
}

pub fn suggestion_user_prompt(name: &str, request: &str) -> String {
    format!(
        "Monte a esteira “{name}” para este pedido:\n\n{}\n\nUse `input` com o próprio pedido.",
        request.trim()
    )
}

pub fn suggestion_repair_prompt(problems: &[String]) -> String {
    let mut prompt =
        String::from("A esteira não passou. Corrija e responda com o arquivo inteiro:\n");
    for problem in problems {
        prompt.push_str(&format!("- {problem}\n"));
    }
    prompt
}

/// Parses and validates a suggestion. The problems are phrased for the model to fix.
pub fn review_suggestion(
    response: &str,
    id: &str,
    source: FlowSource,
    agents: &[TaskAgent],
) -> Result<Flow, Vec<String>> {
    let markdown = task_agents::extract_agent_markdown(response);
    let path = format!("{id}{FLOW_FILE_SUFFIX}");
    let flow = parse_flow(Path::new(&path), &markdown, source)
        .map_err(|error| vec![format!("o arquivo não abre: {error:#}")])?;
    let problems: Vec<String> = validate_flow(&flow, agents)
        .iter()
        .map(|problem| match problem.step() {
            Some(step) => format!("passo {}: {problem}", step + 1),
            None => problem.to_string(),
        })
        .collect();
    if problems.is_empty() {
        Ok(flow)
    } else {
        Err(problems)
    }
}
