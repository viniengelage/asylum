use crate::{PermissionMode, TaskAgent, ToolAccess, ToolPermissionRules, Toolkit};
use std::collections::BTreeSet;

pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub enabled_tools: Vec<String>,
}

pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub access: ToolAccess,
}

pub struct ToolkitSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tools: Vec<ToolSummary>,
}

impl From<&Toolkit> for ToolkitSummary {
    fn from(toolkit: &Toolkit) -> Self {
        Self {
            id: toolkit.id.to_string(),
            name: toolkit.name.to_string(),
            description: toolkit.description.to_string(),
            tools: toolkit
                .tools
                .iter()
                .map(|tool| ToolSummary {
                    name: tool.name.to_string(),
                    description: tool.description.to_string(),
                    access: tool.access,
                })
                .collect(),
        }
    }
}

/// What exists in this install, so the model only names profiles, toolkits and tools that are
/// real, and the answer can be checked against the same list.
pub struct AuthoringCatalog {
    pub profiles: Vec<ProfileSummary>,
    pub toolkits: Vec<ToolkitSummary>,
    pub existing_commands: Vec<String>,
    /// Finished agents the model can imitate.
    pub examples: Vec<String>,
}

impl AuthoringCatalog {
    fn builtin_tools(&self) -> BTreeSet<&str> {
        self.profiles
            .iter()
            .flat_map(|profile| profile.enabled_tools.iter().map(String::as_str))
            .collect()
    }

    fn is_known_tool(&self, name: &str) -> bool {
        self.builtin_tools().contains(name)
            || self
                .toolkits
                .iter()
                .any(|toolkit| toolkit.tools.iter().any(|tool| tool.name == name))
    }

    fn is_known_toolkit(&self, id: &str) -> bool {
        self.toolkits.iter().any(|toolkit| toolkit.id == id)
    }
}

pub fn authoring_system_prompt(catalog: &AuthoringCatalog) -> String {
    let mut prompt = String::from(
        "Você escreve agentes de tarefa para o Asylum, um editor de código. Um agente de tarefa é um arquivo Markdown com frontmatter YAML. O usuário chama o agente com `/comando <pedido>` no chat; a partir daí as instruções do arquivo viram parte do system prompt, as ferramentas passam a ser as do perfil somadas às dos toolkits, e as permissões do arquivo decidem o que roda sem perguntar.

Responda APENAS com o conteúdo do arquivo, começando pela linha `---`. Sem cercas de código, sem comentários antes ou depois.

## Formato

---
name: Nome curto e legível
command: comando-sem-barra
description: Uma frase que diz o que o agente entrega
profile: <id de um perfil da lista>
toolkits: [<ids de toolkits da lista>]
tools:
  <ferramenta>: false
permissions:
  <ferramenta ou toolkit>: allow | confirm | deny
  <ferramenta>:
    default: confirm
    allow: ['<regex sobre a entrada da ferramenta>']
    deny: ['<regex>']
---

<instruções em Markdown>

## Regras dos campos

- `command`: minúsculas, dígitos e hífens; curto e fácil de digitar. Não reutilize um comando que já existe.
- `profile`: o ponto de partida das ferramentas. Use o perfil mais restrito que ainda deixa o agente terminar o trabalho: um agente que só lê e relata não precisa de um perfil que edita arquivos.
- `toolkits`: liga todas as ferramentas do toolkit. Ligue só os que as instruções realmente usam.
- `tools`: exceções por ferramenta sobre o perfil e os toolkits. Omita o campo quando não houver exceção.
- `permissions`: chave é o nome de uma ferramenta ou o id de um toolkit (vale para todas as ferramentas dele). `allow` roda sem perguntar, `confirm` pede confirmação, `deny` nunca roda. Ferramentas de leitura já rodam sem perguntar; dê `allow` a ferramentas que agem só quando o trabalho depende de muitas chamadas seguras (ex.: tocar na tela de um simulador), e `confirm` para o que publica, apaga, envia ou é irreversível. Para o terminal, prefira regras com regex (`allow: ['^npm test']`) a liberar tudo. Omita o campo quando não houver regra.
- Não escreva `model`: o agente usa o modelo da thread.

## Instruções

Escreva em português do Brasil, na segunda pessoa (\"Você revisa…\"). Um bom agente entrega um trabalho completo sem precisar de acompanhamento:
1. Uma frase de papel: o que ele faz e para quem.
2. Os passos, na ordem, citando as ferramentas pelo nome exato (`clickup_task`, `device_tap`…), e onde olhar no projeto quando o contexto disser.
3. O que priorizar e o que ignorar.
4. Quando parar e perguntar ao usuário (antes de ações irreversíveis ou quando faltar informação).
5. O formato da entrega final.
Seja concreto e use o contexto que o usuário deu (caminhos, convenções, links, nomes). Não invente ferramentas, arquivos ou fatos que não estão no contexto.

## Perfis disponíveis\n",
    );
    for profile in &catalog.profiles {
        let tools = if profile.enabled_tools.is_empty() {
            "nenhuma ferramenta".to_string()
        } else {
            profile.enabled_tools.join(", ")
        };
        prompt.push_str(&format!(
            "- `{}` ({}): {}\n",
            profile.id, profile.name, tools
        ));
    }
    prompt.push_str("\n## Toolkits disponíveis\n");
    for toolkit in &catalog.toolkits {
        prompt.push_str(&format!(
            "\n### `{}` — {}\n{}\n",
            toolkit.id, toolkit.name, toolkit.description
        ));
        for tool in &toolkit.tools {
            let access = match tool.access {
                ToolAccess::Read => "lê",
                ToolAccess::Act => "age",
                ToolAccess::AlwaysConfirm => "age, sempre confirma",
            };
            let description = tool.description.lines().next().unwrap_or_default();
            prompt.push_str(&format!("- `{}` ({access}): {description}\n", tool.name));
        }
    }
    if !catalog.existing_commands.is_empty() {
        prompt.push_str(&format!(
            "\n## Comandos já usados\n{}\n",
            catalog
                .existing_commands
                .iter()
                .map(|command| format!("/{command}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (index, example) in catalog.examples.iter().enumerate() {
        prompt.push_str(&format!("\n## Exemplo {}\n\n{example}\n", index + 1));
    }
    prompt
}

pub fn authoring_user_prompt(description: &str, context: &str) -> String {
    let mut prompt = format!(
        "Crie um agente de tarefa.\n\n## O que ele deve fazer\n{}\n",
        description.trim()
    );
    if !context.trim().is_empty() {
        prompt.push_str(&format!("\n## Contexto\n{}\n", context.trim()));
    }
    prompt
}

pub fn authoring_repair_prompt(problems: &[String]) -> String {
    format!(
        "O arquivo tem estes problemas:\n{}\n\nResponda de novo com o arquivo inteiro corrigido, começando por `---`.",
        problems
            .iter()
            .map(|problem| format!("- {problem}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The file inside a response, tolerating a code fence or a sentence before the frontmatter.
pub fn extract_agent_markdown(response: &str) -> String {
    let mut text = response.trim();
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        let body_start = after_fence
            .find('\n')
            .map_or(after_fence.len(), |index| index + 1);
        let body = &after_fence[body_start..];
        let fenced = match body.rfind("```") {
            Some(end) => &body[..end],
            None => body,
        };
        if fenced.trim_start().starts_with("---") {
            text = fenced.trim();
        }
    }
    if !text.starts_with("---")
        && let Some(start) = text.find("\n---\n")
    {
        text = &text[start + 1..];
    }
    format!("{}\n", text.trim())
}

/// What is wrong with a generated agent, phrased so the model can fix it.
pub fn review_generated_agent(agent: &TaskAgent, catalog: &AuthoringCatalog) -> Vec<String> {
    let mut problems = Vec::new();
    if catalog.existing_commands.contains(&agent.command) {
        problems.push(format!(
            "o comando `{}` já existe; escolha outro",
            agent.command
        ));
    }
    if agent.description.is_empty() {
        problems.push("falta `description`".to_string());
    }
    if agent.instructions.trim().len() < 80 {
        problems
            .push("as instruções estão curtas demais para o agente trabalhar sozinho".to_string());
    }
    match &agent.profile {
        Some(profile) if !catalog.profiles.iter().any(|known| known.id == *profile) => {
            problems.push(format!("o perfil `{profile}` não existe"));
        }
        Some(_) => {}
        None => problems.push("falta `profile`".to_string()),
    }
    for toolkit in &agent.toolkits {
        if !catalog.is_known_toolkit(toolkit) {
            problems.push(format!("o toolkit `{toolkit}` não existe"));
        }
    }
    for tool in agent.tools.keys() {
        if !catalog.is_known_tool(tool) {
            problems.push(format!("a ferramenta `{tool}` em `tools` não existe"));
        }
    }
    for key in agent.permissions.keys() {
        if !catalog.is_known_tool(key) && !catalog.is_known_toolkit(key) {
            problems.push(format!(
                "`{key}` em `permissions` não é uma ferramenta nem um toolkit"
            ));
        }
    }
    problems
}

/// Drops what still names something that doesn't exist after the model had its chance to fix
/// it, so the saved file loads cleanly and the form shows only real options.
pub fn drop_unknown_references(agent: &mut TaskAgent, catalog: &AuthoringCatalog) {
    if agent
        .profile
        .as_ref()
        .is_some_and(|profile| !catalog.profiles.iter().any(|known| known.id == *profile))
    {
        agent.profile = None;
    }
    agent
        .toolkits
        .retain(|toolkit| catalog.is_known_toolkit(toolkit));
    agent.tools.retain(|tool, _| catalog.is_known_tool(tool));
    agent
        .permissions
        .retain(|key, _| catalog.is_known_tool(key) || catalog.is_known_toolkit(key));
    agent.model = None;
}

/// A template rendered as a finished file, for the prompt's examples.
pub fn example_markdown(
    name: &str,
    command: &str,
    description: &str,
    profile: &str,
    toolkits: &[&str],
    permissions: &[(&str, PermissionMode)],
    instructions: &str,
) -> Option<String> {
    let agent = TaskAgent {
        id: command.to_string(),
        name: name.to_string(),
        command: command.to_string(),
        description: description.to_string(),
        profile: Some(profile.to_string()),
        model: None,
        toolkits: toolkits.iter().map(|id| id.to_string()).collect(),
        tools: Default::default(),
        permissions: permissions
            .iter()
            .map(|(key, mode)| {
                (
                    key.to_string(),
                    ToolPermissionRules {
                        default: Some(*mode),
                        ..Default::default()
                    },
                )
            })
            .collect(),
        instructions: instructions.to_string(),
        source: crate::TaskAgentSource::Personal,
        file_path: Default::default(),
    };
    agent.to_markdown().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskAgentSource, parse_task_agent};
    use std::path::Path;

    fn catalog() -> AuthoringCatalog {
        AuthoringCatalog {
            profiles: vec![ProfileSummary {
                id: "ask".into(),
                name: "Ask".into(),
                enabled_tools: vec!["read_file".into(), "grep".into(), "terminal".into()],
            }],
            toolkits: vec![ToolkitSummary {
                id: "clickup".into(),
                name: "ClickUp".into(),
                description: "Tarefas".into(),
                tools: vec![ToolSummary {
                    name: "clickup_task".into(),
                    description: "Lê uma tarefa.\nDetalhes.".into(),
                    access: ToolAccess::Read,
                }],
            }],
            existing_commands: vec!["revisar".into()],
            examples: Vec::new(),
        }
    }

    fn parse(markdown: &str) -> TaskAgent {
        parse_task_agent(
            Path::new("/x/gerado.md"),
            markdown,
            TaskAgentSource::Personal,
        )
        .expect("agent should parse")
    }

    #[test]
    fn test_extract_agent_markdown_strips_fences_and_preamble() {
        let fenced = "Aqui está:\n\n```markdown\n---\nname: A\n---\nFaça.\n```\n";
        assert_eq!(extract_agent_markdown(fenced), "---\nname: A\n---\nFaça.\n");
        let preamble = "Claro!\n---\nname: A\n---\nFaça.";
        assert_eq!(
            extract_agent_markdown(preamble),
            "---\nname: A\n---\nFaça.\n"
        );
        let plain = "---\nname: A\n---\nFaça.";
        assert_eq!(extract_agent_markdown(plain), "---\nname: A\n---\nFaça.\n");
    }

    #[test]
    fn test_review_flags_unknown_references_and_taken_command() {
        let agent = parse(
            "---\nname: Revisor\ncommand: revisar\ndescription: Revisa\nprofile: write\ntoolkits: [clickup, jira]\ntools:\n  edit_file: false\npermissions:\n  clickup: allow\n  slack_send: confirm\n  terminal:\n    allow: ['^npm test']\n---\nCurto.",
        );
        let problems = review_generated_agent(&agent, &catalog());
        let joined = problems.join("\n");
        assert!(joined.contains("`revisar` já existe"), "{joined}");
        assert!(joined.contains("perfil `write`"), "{joined}");
        assert!(joined.contains("toolkit `jira`"), "{joined}");
        assert!(joined.contains("`edit_file` em `tools`"), "{joined}");
        assert!(joined.contains("`slack_send`"), "{joined}");
        assert!(!joined.contains("`clickup` em"), "{joined}");
        assert!(!joined.contains("`terminal`"), "{joined}");
        assert!(joined.contains("curtas demais"), "{joined}");
    }

    #[test]
    fn test_drop_unknown_references_keeps_real_ones() {
        let mut agent = parse(
            "---\nname: US\ncommand: us\ndescription: Cria US\nprofile: write\nmodel: anthropic/claude-opus-5-5\ntoolkits: [clickup, jira]\npermissions:\n  clickup_task: allow\n  slack_send: confirm\n---\nInstruções.",
        );
        drop_unknown_references(&mut agent, &catalog());
        assert_eq!(agent.profile, None);
        assert_eq!(agent.model, None);
        assert_eq!(agent.toolkits, vec!["clickup".to_string()]);
        assert_eq!(
            agent.permissions.keys().collect::<Vec<_>>(),
            vec!["clickup_task"]
        );
    }

    #[test]
    fn test_system_prompt_lists_catalog() {
        let prompt = authoring_system_prompt(&catalog());
        assert!(prompt.contains("- `ask` (Ask): read_file, grep, terminal"));
        assert!(prompt.contains("- `clickup_task` (lê): Lê uma tarefa."));
        assert!(!prompt.contains("Detalhes."));
        assert!(prompt.contains("/revisar"));
    }
}
