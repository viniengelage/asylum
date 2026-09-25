//! The API toolkit for agents: list the operations of the project's collections and send
//! requests through them, with the same environments, login and headers as the API panel.

use crate::collection::{self, Collection};
use crate::config::{HeaderEntry, ParamEntry};
use crate::schema::Validation;
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, Task};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};

const SPEC_LOAD_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OPERATIONS_LISTED: usize = 400;
const MAX_BODY_CHARS: usize = 20_000;

/// Collections the agent opened, by file, so the login session and cookies carry over between
/// calls in the same way they do in the panel.
#[derive(Default)]
struct AgentCollections(HashMap<PathBuf, Entity<Collection>>);

impl Global for AgentCollections {}

async fn project_collections(
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Vec<Entity<Collection>>> {
    let (fs, roots) = cx.update(|cx| {
        let project = project.read(cx);
        let roots = project
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                worktree
                    .read(cx)
                    .as_local()
                    .map(|local| local.abs_path().to_path_buf())
            })
            .collect::<Vec<_>>();
        (project.fs().clone(), roots)
    });
    let mut files = Vec::new();
    for root in roots {
        for file in collection::discover(&fs, &root).await {
            files.push((root.clone(), file));
        }
    }
    anyhow::ensure!(
        !files.is_empty(),
        "Este projeto não tem coleção de API. Abra o painel API e vincule o arquivo OpenAPI/Swagger."
    );

    let collections = cx.update(|cx| {
        files
            .into_iter()
            .map(|(root, file)| {
                let existing = cx
                    .try_global::<AgentCollections>()
                    .and_then(|collections| collections.0.get(&file).cloned());
                existing.unwrap_or_else(|| {
                    let fs = fs.clone();
                    let entity =
                        cx.new(|cx| Collection::new(root, file.clone(), fs, cx));
                    cx.default_global::<AgentCollections>()
                        .0
                        .insert(file, entity.clone());
                    entity
                })
            })
            .collect::<Vec<_>>()
    });

    let started = Instant::now();
    loop {
        let pending = cx.update(|cx| {
            collections.iter().any(|collection| {
                let collection = collection.read(cx);
                collection.spec.is_none() && collection.spec_error.is_none()
            })
        });
        if !pending || started.elapsed() > SPEC_LOAD_TIMEOUT {
            break;
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    Ok(collections)
}

#[derive(Deserialize, JsonSchema)]
struct OperationsInput {
    /// Only operations whose method, path, summary, tag or operationId contain this text.
    #[serde(default)]
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SendInput {
    /// The operation as `METHOD /path` (e.g. `GET /users/{id}`) or its operationId, as listed
    /// by api_operations.
    operation: String,
    /// Values for `{placeholders}` in the path.
    #[serde(default)]
    path_params: BTreeMap<String, String>,
    #[serde(default)]
    query: BTreeMap<String, String>,
    /// Extra headers; the collection's headers and the login token are added automatically.
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// The request body, usually JSON text.
    #[serde(default)]
    body: Option<String>,
}

fn operations(project: Entity<Project>, input: OperationsInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let collections = project_collections(&project, cx).await?;
        let filter = input.filter.map(|filter| filter.to_lowercase());
        let text = cx.update(|cx| {
            let mut text = String::new();
            let mut listed = 0;
            for collection in &collections {
                let collection = collection.read(cx);
                text.push_str(&format!("## {}\n", collection.title()));
                if let Some(environment) = collection.active_environment() {
                    text.push_str(&format!(
                        "Ambiente: {} · baseUrl = {}\n",
                        environment.name,
                        collection.variable("baseUrl").unwrap_or_default()
                    ));
                }
                text.push_str(if collection.session.is_logged_in() {
                    "Login: com sessão ativa\n"
                } else {
                    "Login: sem sessão (o envio faz login sozinho se o spec tiver login)\n"
                });
                if let Some(error) = &collection.spec_error {
                    text.push_str(&format!("Erro no spec: {error}\n\n"));
                    continue;
                }
                let Some(spec) = &collection.spec else {
                    text.push_str("O spec ainda está carregando.\n\n");
                    continue;
                };
                for operation in &spec.operations {
                    let haystack = format!(
                        "{} {} {} {}",
                        operation.key,
                        operation.summary.clone().unwrap_or_default(),
                        operation.operation_id.clone().unwrap_or_default(),
                        operation.tags.join(" ")
                    )
                    .to_lowercase();
                    if filter.as_ref().is_some_and(|filter| !haystack.contains(filter)) {
                        continue;
                    }
                    if listed == MAX_OPERATIONS_LISTED {
                        text.push_str("… (use filter para ver o resto)\n");
                        break;
                    }
                    listed += 1;
                    let parameters = operation
                        .parameters
                        .iter()
                        .map(|parameter| {
                            format!(
                                "{}{}",
                                parameter.name,
                                if parameter.required { "*" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>();
                    text.push_str(&format!("- {}", operation.key));
                    if let Some(summary) = &operation.summary {
                        text.push_str(&format!(" — {summary}"));
                    }
                    if let Some(operation_id) = &operation.operation_id {
                        text.push_str(&format!(" [{operation_id}]"));
                    }
                    if !parameters.is_empty() {
                        text.push_str(&format!(" · params: {}", parameters.join(", ")));
                    }
                    if operation.request_body.is_some() {
                        text.push_str(" · com body");
                    }
                    if operation.deprecated {
                        text.push_str(" · deprecated");
                    }
                    text.push('\n');
                }
                text.push('\n');
            }
            text
        });
        Ok(ToolkitOutput::text(text))
    })
}

fn set_entry(entries: &mut Vec<ParamEntry>, name: String, value: String) {
    match entries.iter_mut().find(|entry| entry.name == name) {
        Some(entry) => {
            entry.value = value;
            entry.enabled = true;
        }
        None => entries.push(ParamEntry {
            name,
            value,
            enabled: true,
        }),
    }
}

fn send(project: Entity<Project>, input: SendInput, cx: &mut App) -> Task<Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let collections = project_collections(&project, cx).await?;
        let wanted = input.operation.trim().to_string();
        let found = cx.update(|cx| {
            collections.iter().find_map(|collection| {
                let spec = collection.read(cx).spec.clone()?;
                let operation = spec.operations.iter().find(|operation| {
                    operation.key.eq_ignore_ascii_case(&wanted)
                        || operation.operation_id.as_deref() == Some(wanted.as_str())
                })?;
                Some((collection.clone(), operation.key.clone()))
            })
        });
        let (collection, operation_key) = found.with_context(|| {
            format!("A operação `{wanted}` não existe no spec. Use api_operations para ver as disponíveis.")
        })?;

        let exchange = collection
            .update(cx, |collection, cx| {
                let mut draft = collection.draft(&operation_key);
                for (name, value) in input.path_params {
                    set_entry(&mut draft.path_params, name, value);
                }
                for (name, value) in input.query {
                    set_entry(&mut draft.query, name, value);
                }
                for (name, value) in input.headers {
                    draft.headers.retain(|header| !header.name.eq_ignore_ascii_case(&name));
                    draft.headers.push(HeaderEntry {
                        name,
                        value,
                        enabled: true,
                    });
                }
                if input.body.is_some() {
                    draft.body = input.body;
                }
                collection.send(operation_key.clone(), draft, cx)
            })
            .await;

        let mut text = String::new();
        if let Some(request) = &exchange.request {
            let request = request.masked();
            text.push_str(&format!("{} {}\n", request.method, request.url));
            if !request.missing.is_empty() {
                text.push_str(&format!(
                    "Variáveis sem valor: {}\n",
                    request.missing.join(", ")
                ));
            }
        }
        for step in &exchange.timeline {
            text.push_str(&format!(
                "· {} {}{} {}\n",
                step.method,
                step.path,
                step.status
                    .map(|status| format!(" → {status}"))
                    .unwrap_or_default(),
                step.note
            ));
        }
        if let Some(error) = &exchange.error {
            return Err(anyhow!("{text}{error}"));
        }
        let response = exchange
            .response
            .as_ref()
            .context("a requisição não teve resposta")?;
        text.push_str(&format!(
            "HTTP {}{} · {} ms\n",
            response.status,
            response
                .reason
                .as_ref()
                .map(|reason| format!(" {reason}"))
                .unwrap_or_default(),
            response.elapsed.as_millis()
        ));
        match &exchange.validation {
            Some(Validation::Invalid { issues, more }) => {
                text.push_str(&format!(
                    "A resposta não bate com o schema ({} problema(s)):\n",
                    issues.len() + more
                ));
                for issue in issues {
                    text.push_str(&format!("  - {} {}\n", issue.location, issue.message));
                }
            }
            Some(Validation::Valid) => text.push_str("A resposta bate com o schema.\n"),
            _ => {}
        }
        let body = response.display_body();
        if body.chars().count() > MAX_BODY_CHARS {
            text.push_str(&body.chars().take(MAX_BODY_CHARS).collect::<String>());
            text.push_str("\n… (corpo cortado)");
        } else {
            text.push_str(&body);
        }
        Ok(ToolkitOutput::text(text))
    })
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: "api".into(),
            name: "API".into(),
            description: "Operações do OpenAPI do projeto, pelo painel API".into(),
            icon: "arrow_right_left".into(),
            tools: vec![
                ToolkitTool::new(
                    "api_operations",
                    "Listou as operações da API",
                    "Lists the operations of the project's OpenAPI/Swagger collections (method, \
                     path, summary, parameters), with the active environment and login state.",
                    ToolAccess::Read,
                    operations,
                ),
                ToolkitTool::new(
                    "api_send",
                    "Chamou a API",
                    "Sends a request for one operation of the project's API collection, using its \
                     environment, headers and login (renewing the token and retrying once on 401). \
                     Returns the status, the schema check and the body.",
                    ToolAccess::Act,
                    send,
                ),
            ],
        },
        cx,
    );
}
