mod toolkit;

pub use toolkit::*;

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where agents that ship with a project live, relative to a worktree root.
pub const PROJECT_AGENTS_DIR: &str = ".asylum/agents";
/// Claude Code keeps its subagents here; they are read as imported agents and never written.
pub const CLAUDE_AGENTS_DIR: &str = ".claude/agents";
pub const AGENT_FILE_EXTENSION: &str = "md";
pub const MAX_AGENT_FILE_SIZE: u64 = 100 * 1024;

/// Agents that belong to the user rather than a repository. They live in the data directory so
/// each app profile (Work, Personal) keeps its own set.
pub fn personal_agents_dir() -> PathBuf {
    paths::data_dir().join("agents")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskAgentSource {
    Project { worktree_root_name: Arc<str> },
    Personal,
    Imported { worktree_root_name: Arc<str> },
}

impl TaskAgentSource {
    pub fn is_editable(&self) -> bool {
        !matches!(self, Self::Imported { .. })
    }

    fn precedence(&self) -> u8 {
        match self {
            Self::Project { .. } => 2,
            Self::Personal => 1,
            Self::Imported { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[serde(alias = "livre")]
    Allow,
    #[serde(alias = "pede", alias = "ask")]
    Confirm,
    #[serde(alias = "nunca")]
    Deny,
}

/// Per-tool rules an agent adds on top of the user's tool permissions. Patterns are regexes
/// matched against the tool's inputs, the same way `agent.tool_permissions` works.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPermissionRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum PermissionEntry {
    Mode(PermissionMode),
    Rules(ToolPermissionRules),
}

impl From<PermissionEntry> for ToolPermissionRules {
    fn from(entry: PermissionEntry) -> Self {
        match entry {
            PermissionEntry::Mode(mode) => ToolPermissionRules {
                default: Some(mode),
                ..Default::default()
            },
            PermissionEntry::Rules(rules) => rules,
        }
    }
}

impl From<ToolPermissionRules> for PermissionEntry {
    fn from(rules: ToolPermissionRules) -> Self {
        if rules.allow.is_empty() && rules.confirm.is_empty() && rules.deny.is_empty() {
            if let Some(mode) = rules.default {
                return PermissionEntry::Mode(mode);
            }
        }
        PermissionEntry::Rules(rules)
    }
}

/// Claude Code writes `tools: Read, Grep`; our own files use a map of tool name to on/off.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ToolsEntry {
    Map(BTreeMap<String, bool>),
    List(Vec<String>),
    Text(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ToolkitsEntry {
    List(Vec<String>),
    Text(String),
}

fn split_names(text: &str) -> Vec<String> {
    text.split([',', ' '])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    command: Option<String>,
    #[serde(default)]
    description: String,
    profile: Option<String>,
    model: Option<String>,
    toolkits: Option<ToolkitsEntry>,
    tools: Option<ToolsEntry>,
    #[serde(default)]
    permissions: BTreeMap<String, PermissionEntry>,
}

#[derive(Serialize)]
struct FrontmatterOut<'a> {
    name: &'a str,
    command: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    toolkits: &'a Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    tools: &'a BTreeMap<String, bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    permissions: BTreeMap<&'a str, PermissionEntry>,
}

/// A `provider/model` pair, e.g. `anthropic/claude-opus-5-5`. Claude Code's aliases (`opus`,
/// `sonnet`, `inherit`) carry no provider and are ignored, so the thread keeps its model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReference {
    pub provider: String,
    pub model: String,
}

impl ModelReference {
    pub fn parse(text: &str) -> Option<Self> {
        let (provider, model) = text.trim().split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }
}

impl std::fmt::Display for ModelReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAgent {
    /// The file stem; stable across renames of `name`.
    pub id: String,
    pub name: String,
    /// Typed after `/` in the composer.
    pub command: String,
    pub description: String,
    /// The agent profile whose tools are the starting point.
    pub profile: Option<String>,
    pub model: Option<String>,
    /// Toolkit ids whose tools are all turned on.
    pub toolkits: Vec<String>,
    /// Per-tool overrides on top of the profile and the toolkits.
    pub tools: BTreeMap<String, bool>,
    /// Keyed by tool name or toolkit id.
    pub permissions: BTreeMap<String, ToolPermissionRules>,
    pub instructions: String,
    pub source: TaskAgentSource,
    pub file_path: PathBuf,
}

impl TaskAgent {
    pub fn model_reference(&self) -> Option<ModelReference> {
        self.model.as_deref().and_then(ModelReference::parse)
    }

    /// Whether the agent turns a tool on or off, given which toolkit the tool belongs to.
    /// `None` leaves the decision to the profile.
    pub fn tool_override(&self, tool_name: &str, toolkit_id: Option<&str>) -> Option<bool> {
        if let Some(enabled) = self.tools.get(tool_name) {
            return Some(*enabled);
        }
        if toolkit_id.is_some_and(|toolkit_id| self.toolkits.iter().any(|id| id == toolkit_id)) {
            return Some(true);
        }
        None
    }

    /// The agent's rules for a tool: the tool's own entry wins over its toolkit's.
    pub fn permission_rules(
        &self,
        tool_name: &str,
        toolkit_id: Option<&str>,
    ) -> Option<&ToolPermissionRules> {
        self.permissions
            .get(tool_name)
            .or_else(|| toolkit_id.and_then(|toolkit_id| self.permissions.get(toolkit_id)))
    }

    pub fn to_markdown(&self) -> Result<String> {
        let frontmatter = FrontmatterOut {
            name: &self.name,
            command: &self.command,
            description: &self.description,
            profile: self.profile.as_deref(),
            model: self.model.as_deref(),
            toolkits: &self.toolkits,
            tools: &self.tools,
            permissions: self
                .permissions
                .iter()
                .map(|(name, rules)| (name.as_str(), PermissionEntry::from(rules.clone())))
                .collect(),
        };
        let yaml = serde_yaml_ng::to_string(&frontmatter)?;
        let instructions = self.instructions.trim();
        Ok(format!("---\n{yaml}---\n\n{instructions}\n"))
    }
}

/// Lowercase ASCII letters, digits and dashes, so the command survives the slash parser (which
/// treats `.` and `:` as scope separators).
pub fn command_slug(text: &str) -> String {
    let mut slug = String::new();
    for character in text.trim().trim_start_matches('/').chars() {
        let character = match character {
            'á' | 'à' | 'ã' | 'â' | 'Á' | 'À' | 'Ã' | 'Â' => 'a',
            'é' | 'ê' | 'É' | 'Ê' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'õ' | 'ô' | 'Ó' | 'Õ' | 'Ô' => 'o',
            'ú' | 'Ú' => 'u',
            'ç' | 'Ç' => 'c',
            other => other,
        };
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

pub fn parse_task_agent(path: &Path, content: &str, source: TaskAgentSource) -> Result<TaskAgent> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("agent file has no name")?
        .to_string();
    let (frontmatter, body) = split_frontmatter(content)?;
    let frontmatter: Frontmatter = match frontmatter {
        Some(yaml) if !yaml.trim().is_empty() => {
            serde_yaml_ng::from_str(yaml).context("invalid YAML frontmatter")?
        }
        _ => Frontmatter::default(),
    };

    let name = frontmatter
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    let command = command_slug(frontmatter.command.as_deref().unwrap_or(&name));
    anyhow::ensure!(!command.is_empty(), "agent `{name}` has no usable command");

    let imported = matches!(source, TaskAgentSource::Imported { .. });
    let tools = match frontmatter.tools {
        // Claude Code tool names (Read, Bash, …) don't exist here; an imported agent keeps
        // the tools of its profile.
        _ if imported => BTreeMap::new(),
        Some(ToolsEntry::Map(map)) => map,
        Some(ToolsEntry::List(names)) => names.into_iter().map(|name| (name, true)).collect(),
        Some(ToolsEntry::Text(text)) => split_names(&text)
            .into_iter()
            .map(|name| (name, true))
            .collect(),
        None => BTreeMap::new(),
    };
    let toolkits = match frontmatter.toolkits {
        Some(ToolkitsEntry::List(names)) => names,
        Some(ToolkitsEntry::Text(text)) => split_names(&text),
        None => Vec::new(),
    };
    let model = frontmatter
        .model
        .filter(|model| ModelReference::parse(model).is_some());

    Ok(TaskAgent {
        id,
        name,
        command,
        description: frontmatter.description.trim().to_string(),
        profile: frontmatter.profile.filter(|profile| !profile.is_empty()),
        model,
        toolkits,
        tools,
        permissions: frontmatter
            .permissions
            .into_iter()
            .map(|(name, entry)| (name, entry.into()))
            .collect(),
        instructions: body.trim().to_string(),
        source,
        file_path: path.to_path_buf(),
    })
}

/// Returns the YAML between the leading `---` lines and the rest of the file. A file without
/// frontmatter is all instructions.
fn split_frontmatter(content: &str) -> Result<(Option<&str>, &str)> {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return Ok((None, content));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((Some(&rest[..offset]), &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    anyhow::bail!("frontmatter is missing its closing `---`")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAgentLoadError {
    pub path: PathBuf,
    pub message: String,
}

/// Reads every `*.md` directly inside `directory`. A missing directory yields nothing.
pub async fn load_task_agents_from_directory(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    source: TaskAgentSource,
) -> Vec<Result<TaskAgent, TaskAgentLoadError>> {
    let Ok(mut entries) = fs.read_dir(directory).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else {
            continue;
        };
        if path.extension().and_then(|extension| extension.to_str()) == Some(AGENT_FILE_EXTENSION)
        {
            paths.push(path);
        }
    }
    paths.sort();

    let mut results = Vec::new();
    for path in paths {
        let load_error = |message: String| TaskAgentLoadError {
            path: path.clone(),
            message,
        };
        match fs.metadata(&path).await {
            Ok(Some(metadata)) if metadata.is_dir => continue,
            Ok(Some(metadata)) if metadata.len > MAX_AGENT_FILE_SIZE => {
                results.push(Err(load_error(format!(
                    "agent file exceeds {}KB",
                    MAX_AGENT_FILE_SIZE / 1024
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
            Ok(content) => parse_task_agent(&path, &content, source.clone())
                .map_err(|error| load_error(format!("{error:#}"))),
            Err(error) => Err(load_error(error.to_string())),
        };
        results.push(result);
    }
    results
}

/// Loads the agents of a set of worktree roots plus the personal ones. When two agents claim the
/// same command, a project agent wins over a personal one, which wins over an imported one.
pub async fn load_task_agents(
    fs: Arc<dyn Fs>,
    worktree_roots: Vec<(Arc<str>, PathBuf)>,
) -> (Vec<TaskAgent>, Vec<TaskAgentLoadError>) {
    let mut results = Vec::new();
    for (root_name, root) in &worktree_roots {
        results.extend(
            load_task_agents_from_directory(
                &fs,
                &root.join(PROJECT_AGENTS_DIR),
                TaskAgentSource::Project {
                    worktree_root_name: root_name.clone(),
                },
            )
            .await,
        );
        results.extend(
            load_task_agents_from_directory(
                &fs,
                &root.join(CLAUDE_AGENTS_DIR),
                TaskAgentSource::Imported {
                    worktree_root_name: root_name.clone(),
                },
            )
            .await,
        );
    }
    results.extend(
        load_task_agents_from_directory(&fs, &personal_agents_dir(), TaskAgentSource::Personal)
            .await,
    );

    let mut agents: Vec<TaskAgent> = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(agent) => {
                if let Some(existing) = agents
                    .iter_mut()
                    .find(|existing| existing.command == agent.command)
                {
                    if agent.source.precedence() > existing.source.precedence() {
                        *existing = agent;
                    }
                } else {
                    agents.push(agent);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    (agents, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_source() -> TaskAgentSource {
        TaskAgentSource::Project {
            worktree_root_name: "app".into(),
        }
    }

    #[test]
    fn test_parse_full_agent() {
        let content = "---\nname: Revisor de PR\ncommand: /revisar\ndescription: Revisa o PR\nprofile: ask\nmodel: anthropic/claude-opus-5-5\ntoolkits: [repo, clickup]\ntools:\n  edit_file: false\npermissions:\n  repo_pr_comment: confirm\n  terminal:\n    default: confirm\n    allow: ['^npm test']\n    deny: ['rm -rf']\n---\n\nVocê revisa o PR.\n";
        let agent = parse_task_agent(
            Path::new("/app/.asylum/agents/revisar-pr.md"),
            content,
            project_source(),
        )
        .expect("agent should parse");
        assert_eq!(agent.id, "revisar-pr");
        assert_eq!(agent.name, "Revisor de PR");
        assert_eq!(agent.command, "revisar");
        assert_eq!(agent.profile.as_deref(), Some("ask"));
        assert_eq!(
            agent.model_reference(),
            Some(ModelReference {
                provider: "anthropic".into(),
                model: "claude-opus-5-5".into()
            })
        );
        assert_eq!(agent.toolkits, vec!["repo".to_string(), "clickup".to_string()]);
        assert_eq!(agent.tool_override("edit_file", None), Some(false));
        assert_eq!(agent.tool_override("repo_pr_diff", Some("repo")), Some(true));
        assert_eq!(agent.tool_override("read_file", None), None);
        assert_eq!(
            agent
                .permission_rules("repo_pr_comment", Some("repo"))
                .and_then(|rules| rules.default),
            Some(PermissionMode::Confirm)
        );
        let terminal = agent.permission_rules("terminal", None).expect("rules");
        assert_eq!(terminal.allow, vec!["^npm test".to_string()]);
        assert_eq!(terminal.deny, vec!["rm -rf".to_string()]);
        assert_eq!(agent.instructions, "Você revisa o PR.");
    }

    #[test]
    fn test_portuguese_permission_aliases() {
        let content = "---\nname: QA\npermissions:\n  devices: livre\n  api_send: pede\n  database_query: nunca\n---\nTeste o app.";
        let agent = parse_task_agent(Path::new("/x/qa.md"), content, project_source())
            .expect("agent should parse");
        let mode = |name: &str| {
            agent
                .permission_rules(name, None)
                .and_then(|rules| rules.default)
        };
        assert_eq!(mode("devices"), Some(PermissionMode::Allow));
        assert_eq!(mode("api_send"), Some(PermissionMode::Confirm));
        assert_eq!(mode("database_query"), Some(PermissionMode::Deny));
    }

    #[test]
    fn test_claude_code_agent_is_imported_without_tools() {
        let content = "---\nname: code-reviewer\ndescription: Reviews code\ntools: Read, Grep, Bash\nmodel: sonnet\n---\nYou review code.";
        let agent = parse_task_agent(
            Path::new("/app/.claude/agents/code-reviewer.md"),
            content,
            TaskAgentSource::Imported {
                worktree_root_name: "app".into(),
            },
        )
        .expect("agent should parse");
        assert_eq!(agent.command, "code-reviewer");
        assert!(agent.tools.is_empty());
        assert_eq!(agent.model, None);
        assert!(!agent.source.is_editable());
    }

    #[test]
    fn test_file_without_frontmatter_uses_file_name() {
        let agent = parse_task_agent(
            Path::new("/x/Escrever Testes.md"),
            "Escreva testes.",
            TaskAgentSource::Personal,
        )
        .expect("agent should parse");
        assert_eq!(agent.name, "Escrever Testes");
        assert_eq!(agent.command, "escrever-testes");
        assert_eq!(agent.instructions, "Escreva testes.");
    }

    #[test]
    fn test_unclosed_frontmatter_is_an_error() {
        assert!(
            parse_task_agent(Path::new("/x/a.md"), "---\nname: a\n", project_source()).is_err()
        );
    }

    #[test]
    fn test_command_slug() {
        assert_eq!(command_slug("/Virar US"), "virar-us");
        assert_eq!(command_slug("Descrição do PR"), "descricao-do-pr");
        assert_eq!(command_slug("qa.app:x"), "qa-app-x");
    }

    #[test]
    fn test_markdown_round_trip() {
        let content = "---\nname: QA do app\ncommand: qa\nprofile: write\ntoolkits: [devices]\npermissions:\n  devices: allow\n  terminal:\n    allow: ['^npx expo ']\n---\nSuba o app.";
        let agent = parse_task_agent(Path::new("/x/qa.md"), content, project_source())
            .expect("agent should parse");
        let written = agent.to_markdown().expect("agent should serialize");
        let reparsed = parse_task_agent(Path::new("/x/qa.md"), &written, project_source())
            .expect("written agent should parse");
        assert_eq!(agent, reparsed);
    }

    #[gpui::test]
    async fn test_load_prefers_project_agents(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/app",
            serde_json::json!({
                ".asylum": { "agents": {
                    "revisar.md": "---\nname: Revisor\ncommand: revisar\n---\nProjeto.",
                    "notes.txt": "ignored",
                    "broken.md": "---\nname: [\n---\n",
                }},
                ".claude": { "agents": {
                    "revisar.md": "---\nname: revisar\n---\nImportado.",
                    "explain.md": "---\nname: explain\n---\nExplique.",
                }},
            }),
        )
        .await;
        let fs: Arc<dyn Fs> = fs;
        let (agents, errors) =
            load_task_agents(fs, vec![("app".into(), PathBuf::from("/app"))]).await;
        let commands = agents
            .iter()
            .map(|agent| (agent.command.as_str(), agent.instructions.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(commands, vec![("revisar", "Projeto."), ("explain", "Explique.")]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].path.ends_with("broken.md"));
    }
}
