//! The agent's Docker tools. Reading (list, logs, inspect, stats) is free; anything that changes a
//! container asks first, and removing one is only possible from the dock. Env values never reach
//! the model.

use crate::{
    DockerPanel,
    client::{Container, ContainerDetails, Docker, LogLine, LogStream},
    format_bytes, format_percent, panel,
    store::{ContainerAction, DockerStore},
};
use anyhow::{Context as _, anyhow};
use gpui::{App, AsyncApp, Entity, Task, TaskExt as _, Window};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use task_agents::{
    CONTAINER_MENTION_PREFIX, ContainerSource, DOCKER_INSPECT_TOOL, DOCKER_LOGS_TOOL,
    DockerContainerResult, DockerLogLine, LogMention, ToolAccess, Toolkit, ToolkitOutput,
    ToolkitTool,
};
use workspace::{AppState, Workspace};

const DEFAULT_TAIL: usize = 200;
const MAX_TAIL: usize = 1000;
const CARD_LOG_LINES: usize = 6;
const DEFAULT_EXEC_TIMEOUT: u64 = 30;
const MAX_EXEC_TIMEOUT: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 40_000;
/// How long a tool waits for the dock's first connection and container list.
const CONNECT_WAIT: Duration = Duration::from_secs(5);

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct ContainerInput {
    /// The container: compose service (`api`), container name or id.
    container: String,
}

#[derive(Deserialize, JsonSchema)]
struct LogsInput {
    /// The container: compose service (`api`), container name or id.
    container: String,
    /// How many of the last lines to read. Defaults to 200, at most 1000.
    #[serde(default)]
    tail: Option<usize>,
    /// Only lines containing this text (case-insensitive).
    #[serde(default)]
    contains: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct StatsInput {
    /// One container; all running ones when omitted.
    #[serde(default)]
    container: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ExecInput {
    /// The container: compose service (`api`), container name or id.
    container: String,
    /// One shell command, run with `sh -c` without a TTY (e.g. `env | grep REDIS`,
    /// `nc -zv redis 6379`). Nothing interactive.
    command: String,
    /// Seconds before giving up. Defaults to 30, at most 120.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// The store once it is connected and has listed the containers, waiting a little for a dock
/// that was never opened in this session.
async fn ready_store(cx: &mut AsyncApp) -> anyhow::Result<(Entity<DockerStore>, Docker)> {
    let store = cx.update(|cx| DockerStore::global(cx));
    let started = std::time::Instant::now();
    loop {
        let state = store.read_with(cx, |store, _| {
            (
                store.docker().cloned(),
                store.is_loaded(),
                match store.connection() {
                    crate::store::Connection::Unavailable { socket, reason } => {
                        Some(format!("{} ({reason})", socket.display()))
                    }
                    _ => None,
                },
            )
        });
        match state {
            (Some(docker), true, _) => return Ok((store, docker)),
            (_, _, Some(unavailable)) => {
                return Err(anyhow!(
                    "O Docker não está rodando nesta máquina: nenhum daemon respondeu em \
                     {unavailable}. Peça para abrir o Docker Desktop (ou Colima/OrbStack)."
                ));
            }
            _ if started.elapsed() > CONNECT_WAIT => {
                return Err(anyhow!("O Docker ainda está conectando. Tente de novo."));
            }
            _ => {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await
            }
        }
    }
}

fn find_container(
    store: &Entity<DockerStore>,
    name: &str,
    cx: &mut AsyncApp,
) -> anyhow::Result<Container> {
    store
        .read_with(cx, |store, _| store.find(name).cloned())
        .with_context(|| {
            format!("Nenhum container chamado {name}. Use docker_list para ver os nomes.")
        })
}

fn container_line(container: &Container, store: &DockerStore) -> String {
    let mut line = format!(
        "- {} ({}) · {} · {}",
        container.display_name(),
        container.short_id(),
        container.image,
        container.state.label()
    );
    if let Some(project) = container.compose_project() {
        line.push_str(&format!(" · compose {project}"));
    }
    let ports = container.published_ports();
    if !ports.is_empty() {
        let ports = ports
            .iter()
            .map(|(public, private)| format!("{public}→{private}"))
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" · portas {ports}"));
    }
    if let Some(stats) = store.stats(&container.id) {
        line.push_str(&format!(
            " · CPU {} · memória {}",
            format_percent(stats.cpu_percent),
            format_bytes(stats.memory_bytes)
        ));
    }
    if let Some(count) = store.restart_count(&container.id) {
        line.push_str(&format!(" · {count} reinícios"));
    }
    line.push_str(&format!(" · {}", container.status));
    line
}

fn list_containers(
    _project: Entity<Project>,
    _input: NoInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let (store, _) = ready_store(cx).await?;
        let text = store.read_with(cx, |store, _| {
            let containers = store.containers();
            if containers.is_empty() {
                return "Nenhum container nesta máquina.".to_string();
            }
            let mut text = format!("{} containers:\n", containers.len());
            for container in containers {
                text.push_str(&container_line(container, store));
                text.push('\n');
            }
            text
        });
        Ok(ToolkitOutput::text(text))
    })
}

fn card_lines(lines: &[LogLine]) -> Vec<DockerLogLine> {
    lines
        .iter()
        .rev()
        .take(CARD_LOG_LINES)
        .rev()
        .map(|line| DockerLogLine {
            time: line
                .clock()
                .map(|clock| clock.chars().take(8).collect())
                .unwrap_or_default(),
            text: line.text.clone(),
            error: line.stream == LogStream::Stderr
                || line.text.to_ascii_lowercase().contains("error"),
        })
        .collect()
}

fn card(container: &Container, store: &DockerStore, lines: &[LogLine]) -> DockerContainerResult {
    let state_label = crate::panel::state_label(container, store);
    DockerContainerResult {
        id: container.id.clone(),
        name: container.display_name().to_string(),
        image: container.image.clone(),
        state: serde_json::to_value(container.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default(),
        state_label,
        log_lines: card_lines(lines),
    }
}

/// Reads the last lines without following.
async fn read_logs(docker: &Docker, id: &str, tail: usize) -> anyhow::Result<Vec<LogLine>> {
    let mut receiver = docker
        .logs(id, tail, false)
        .await
        .map_err(|error| anyhow!("{error}"))?;
    let mut lines = Vec::new();
    while let Some(line) = receiver.recv().await {
        lines.push(line);
    }
    Ok(lines)
}

fn logs_text(container: &Container, lines: &[LogLine]) -> String {
    let mut text = format!(
        "Últimas {} linhas de {} ({}):\n",
        lines.len(),
        container.display_name(),
        container.state.label()
    );
    let start = lines.len().saturating_sub(MAX_TAIL);
    for line in &lines[start..] {
        let entry = format!(
            "{} {}{}\n",
            line.clock().unwrap_or_default(),
            if line.stream == LogStream::Stderr {
                "[stderr] "
            } else {
                ""
            },
            line.text
        );
        if text.len() + entry.len() > MAX_OUTPUT_CHARS {
            text.push_str("… (saída cortada)\n");
            break;
        }
        text.push_str(&entry);
    }
    text
}

fn read_container_logs(
    _project: Entity<Project>,
    input: LogsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let (store, docker) = ready_store(cx).await?;
        let container = find_container(&store, &input.container, cx)?;
        let tail = input.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL);
        let mut lines = read_logs(&docker, &container.id, tail).await?;
        if let Some(contains) = input.contains.map(|text| text.to_lowercase()) {
            lines.retain(|line| line.text.to_lowercase().contains(&contains));
        }
        let text = logs_text(&container, &lines);
        let card = store.read_with(cx, |store, _| card(&container, store, &lines));
        Ok(ToolkitOutput::text(text).with_raw_output(card))
    })
}

/// The inspect output the model gets: what explains a container's behavior, env names without
/// their values.
fn inspect_text(container: &Container, details: &ContainerDetails) -> String {
    let mut text = format!(
        "{} ({}) · imagem {}\n",
        container.display_name(),
        container.short_id(),
        details.config.image
    );
    let state = &details.state;
    text.push_str(&format!(
        "estado: {} · código de saída {} · {} reinícios{}\n",
        state.status,
        state.exit_code,
        details.restart_count,
        if state.oom_killed {
            " · morto por falta de memória (OOM)"
        } else {
            ""
        }
    ));
    if !state.error.is_empty() {
        text.push_str(&format!("erro do daemon: {}\n", state.error));
    }
    if let Some(health) = &state.health {
        text.push_str(&format!(
            "health: {} ({} falhas seguidas)\n",
            health.status, health.failing_streak
        ));
    }
    text.push_str(&format!(
        "iniciado em {} · terminou em {}\n",
        state.started_at, state.finished_at
    ));
    if let Some(policy) = &details.host_config.restart_policy
        && !policy.name.is_empty()
    {
        text.push_str(&format!("restart: {}\n", policy.name));
    }
    let command: Vec<String> = details
        .config
        .entrypoint
        .iter()
        .flatten()
        .chain(details.config.cmd.iter().flatten())
        .cloned()
        .collect();
    if !command.is_empty() {
        text.push_str(&format!("comando: {}\n", command.join(" ")));
    }
    if !details.config.working_dir.is_empty() {
        text.push_str(&format!("diretório: {}\n", details.config.working_dir));
    }
    if !details.config.user.is_empty() {
        text.push_str(&format!("usuário: {}\n", details.config.user));
    }
    if !details.host_config.network_mode.is_empty() {
        text.push_str(&format!("rede: {}\n", details.host_config.network_mode));
    }
    let ports = container.published_ports();
    if !ports.is_empty() {
        let ports = ports
            .iter()
            .map(|(public, private)| format!("{public}→{private}"))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("portas: {ports}\n"));
    }
    if let Some(binds) = &details.host_config.binds
        && !binds.is_empty()
    {
        text.push_str(&format!("volumes: {}\n", binds.join(", ")));
    }
    let env_names: Vec<&str> = details
        .config
        .env
        .iter()
        .flatten()
        .filter_map(|entry| entry.split('=').next())
        .collect();
    if !env_names.is_empty() {
        text.push_str(&format!(
            "env (valores escondidos): {}\n",
            env_names
                .iter()
                .map(|name| format!("{name}=***"))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    text
}

fn inspect_container(
    _project: Entity<Project>,
    input: ContainerInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let (store, docker) = ready_store(cx).await?;
        let container = find_container(&store, &input.container, cx)?;
        let details = docker
            .inspect(&container.id)
            .await
            .map_err(|error| anyhow!("{error}"))?;
        let lines = read_logs(&docker, &container.id, CARD_LOG_LINES)
            .await
            .unwrap_or_default();
        let text = inspect_text(&container, &details);
        let card = store.read_with(cx, |store, _| card(&container, store, &lines));
        Ok(ToolkitOutput::text(text).with_raw_output(card))
    })
}

fn container_stats(
    _project: Entity<Project>,
    input: StatsInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let (store, docker) = ready_store(cx).await?;
        let containers: Vec<Container> = match &input.container {
            Some(name) => vec![find_container(&store, name, cx)?],
            None => store.read_with(cx, |store, _| {
                store
                    .containers()
                    .iter()
                    .filter(|container| container.state.is_running())
                    .cloned()
                    .collect()
            }),
        };
        if containers.is_empty() {
            return Ok(ToolkitOutput::text("Nenhum container rodando."));
        }
        let samples = futures::future::join_all(containers.iter().map(|container| {
            let docker = docker.clone();
            let id = container.id.clone();
            async move { docker.stats(&id).await }
        }))
        .await;
        let mut text = String::new();
        for (container, sample) in containers.iter().zip(samples) {
            match sample {
                Ok(stats) => {
                    text.push_str(&format!(
                        "{}: CPU {} · memória {}\n",
                        container.display_name(),
                        format_percent(stats.cpu_percent),
                        format_bytes(stats.memory_bytes)
                    ));
                }
                Err(error) => {
                    text.push_str(&format!(
                        "{}: sem stats ({error})\n",
                        container.display_name()
                    ));
                }
            }
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn lifecycle(
    action: ContainerAction,
) -> impl Fn(Entity<Project>, ContainerInput, &mut App) -> Task<anyhow::Result<ToolkitOutput>>
+ Send
+ Sync
+ 'static {
    move |_project, input, cx| {
        cx.spawn(async move |cx| {
            let (store, _) = ready_store(cx).await?;
            let container = find_container(&store, &input.container, cx)?;
            let run = store.update(cx, |store, cx| store.run(container.id.clone(), action, cx));
            run.await?;
            Ok(ToolkitOutput::text(format!(
                "{} {}.",
                container.display_name(),
                action.done()
            )))
        })
    }
}

fn exec_in_container(
    _project: Entity<Project>,
    input: ExecInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    cx.spawn(async move |cx| {
        let (store, docker) = ready_store(cx).await?;
        let container = find_container(&store, &input.container, cx)?;
        if !container.state.is_running() {
            return Err(anyhow!(
                "{} está {}; o comando precisa dele rodando.",
                container.display_name(),
                container.state.label()
            ));
        }
        let timeout = input
            .timeout_seconds
            .unwrap_or(DEFAULT_EXEC_TIMEOUT)
            .clamp(1, MAX_EXEC_TIMEOUT);
        let output = docker
            .exec(&container.id, &input.command, Duration::from_secs(timeout))
            .await
            .map_err(|error| anyhow!("{error}"))?;
        let mut text = format!("$ {}\n", input.command);
        if output.timed_out {
            text.push_str(&format!("(parou depois de {timeout} s sem terminar)\n"));
        }
        for (label, stream) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            if stream.is_empty() {
                continue;
            }
            let mut stream = stream.clone();
            if stream.len() > MAX_OUTPUT_CHARS {
                let mut cut = MAX_OUTPUT_CHARS;
                while !stream.is_char_boundary(cut) {
                    cut -= 1;
                }
                stream.truncate(cut);
                stream.push_str("\n… (saída cortada)");
            }
            text.push_str(&format!("[{label}]\n{stream}\n"));
        }
        if output.truncated {
            text.push_str("(saída cortada)\n");
        }
        if let Some(code) = output.exit_code {
            text.push_str(&format!("código de saída: {code}\n"));
        }
        Ok(ToolkitOutput::text(text))
    })
}

fn workspace_for(project: &Entity<Project>, cx: &App) -> Option<Entity<Workspace>> {
    let app_state = AppState::try_global(cx)?;
    let workspaces: Vec<_> = app_state
        .workspace_store
        .read(cx)
        .workspaces()
        .filter_map(|workspace| workspace.upgrade())
        .collect();
    workspaces
        .into_iter()
        .find(|workspace| workspace.read(cx).project() == project)
}

fn list_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    let Some(store) = DockerStore::try_global(cx) else {
        return Vec::new();
    };
    let root = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
    store
        .read(cx)
        .groups(root.as_deref())
        .into_iter()
        .flat_map(|group| group.containers)
        .map(|container| LogMention {
            id: format!("{CONTAINER_MENTION_PREFIX}{}", container.id).into(),
            title: container.display_name().to_string().into(),
            detail: format!("{} · {}", container.image, container.state.label()).into(),
        })
        .collect()
}

/// What the agent receives for `@container`: state, image, ports and the last 200 log lines.
fn describe_mention(
    _project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let id = id
        .strip_prefix(CONTAINER_MENTION_PREFIX)
        .unwrap_or(id)
        .to_string();
    cx.spawn(async move |cx| {
        let (store, docker) = ready_store(cx).await?;
        let container = find_container(&store, &id, cx)?;
        let mut text = store.read_with(cx, |store, _| container_line(&container, store));
        text.push('\n');
        if let Ok(details) = docker.inspect(&container.id).await {
            text.push_str(&inspect_text(&container, &details));
        }
        let lines = read_logs(&docker, &container.id, DEFAULT_TAIL)
            .await
            .unwrap_or_default();
        text.push_str(&logs_text(&container, &lines));
        Ok(text)
    })
}

fn open_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let id = id
        .strip_prefix(CONTAINER_MENTION_PREFIX)
        .unwrap_or(id)
        .to_string();
    let workspace = workspace_for(project, cx).context("A janela do projeto foi fechada.")?;
    workspace.update(cx, |workspace, cx| {
        crate::open(workspace, window, cx);
        if let Some(panel) = workspace.panel::<DockerPanel>(cx) {
            panel.update(cx, |panel, cx| panel.select(id, cx));
        }
    });
    Ok(())
}

fn act(
    project: &Entity<Project>,
    container_id: &str,
    action: task_agents::ContainerAction,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace_for(project, cx) else {
        return;
    };
    let store = DockerStore::global(cx);
    let (container, docker) = {
        let store = store.read(cx);
        (
            store.container(container_id).cloned(),
            store.docker().cloned(),
        )
    };
    let (Some(container), Some(docker)) = (container, docker) else {
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                workspace::Toast::new(
                    workspace::notifications::NotificationId::unique::<DockerPanel>(),
                    "Esse container não existe mais.",
                )
                .autohide(),
                cx,
            )
        });
        return;
    };
    match action {
        task_agents::ContainerAction::Logs => workspace.update(cx, |workspace, cx| {
            crate::logs_view::open(
                workspace,
                container.id.clone(),
                container.display_name().to_string().into(),
                window,
                cx,
            )
        }),
        task_agents::ContainerAction::Inspect => {
            panel::inspect(workspace.downgrade(), docker, container, window, cx)
        }
        task_agents::ContainerAction::Terminal => {
            let workspace = workspace.downgrade();
            window
                .spawn(cx, async move |cx| {
                    let details = docker.inspect(&container.id).await.ok();
                    workspace.update_in(cx, |workspace, window, cx| {
                        panel::open_terminal(
                            workspace,
                            &container,
                            details.as_ref(),
                            &docker,
                            window,
                            cx,
                        )
                    })
                })
                .detach_and_log_err(cx);
        }
    }
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_container_source(
        ContainerSource {
            list: Arc::new(list_mentions),
            describe: Arc::new(describe_mention),
            open: Arc::new(open_mention),
            act: Arc::new(act),
        },
        cx,
    );
    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::DOCKER.into(),
            name: "Docker".into(),
            description: "Containers da máquina: estado, logs e ciclo de vida".into(),
            icon: "box".into(),
            mention: Some("@container".into()),
            tools: vec![
                ToolkitTool::new(
                    "docker_list",
                    "Listou os containers",
                    "Lists the containers on this machine with state, image, compose project and \
                     service, published ports, CPU and memory when known, and restart counts. \
                     Call it first to learn the names the other docker tools take.",
                    ToolAccess::Read,
                    list_containers,
                ),
                ToolkitTool::new(
                    DOCKER_LOGS_TOOL,
                    "Leu os logs",
                    "Reads the last `tail` log lines of a container (default 200, at most 1000), \
                     stdout and stderr, with local times. `contains` keeps only matching lines. \
                     Use it to find why a container fails or restarts.",
                    ToolAccess::Read,
                    read_container_logs,
                ),
                ToolkitTool::new(
                    DOCKER_INSPECT_TOOL,
                    "Inspecionou o container",
                    "Reads a container's state, exit code, restart count, health, restart \
                     policy, command, working dir, user, network, ports and volumes. Env \
                     variables come with their names only.",
                    ToolAccess::Read,
                    inspect_container,
                ),
                ToolkitTool::new(
                    "docker_stats",
                    "Leu CPU e memória",
                    "Samples CPU and memory of one container, or of every running one.",
                    ToolAccess::Read,
                    container_stats,
                ),
                ToolkitTool::new(
                    "docker_start",
                    "Iniciou o container",
                    "Starts a stopped container. Asks the user first.",
                    ToolAccess::Act,
                    lifecycle(ContainerAction::Start),
                ),
                ToolkitTool::new(
                    "docker_stop",
                    "Parou o container",
                    "Stops a running container. Asks the user first.",
                    ToolAccess::Act,
                    lifecycle(ContainerAction::Stop),
                ),
                ToolkitTool::new(
                    "docker_restart",
                    "Reiniciou o container",
                    "Restarts a container. Asks the user first.",
                    ToolAccess::Act,
                    lifecycle(ContainerAction::Restart),
                ),
                ToolkitTool::new(
                    "docker_pause",
                    "Pausou o container",
                    "Pauses a running container (its processes freeze). Asks the user first.",
                    ToolAccess::Act,
                    lifecycle(ContainerAction::Pause),
                ),
                ToolkitTool::new(
                    "docker_unpause",
                    "Retomou o container",
                    "Resumes a paused container. Asks the user first.",
                    ToolAccess::Act,
                    lifecycle(ContainerAction::Unpause),
                ),
                ToolkitTool::new(
                    "docker_exec",
                    "Rodou um comando no container",
                    "Runs one non-interactive shell command inside a running container \
                     (`sh -c`, no TTY) and returns stdout, stderr and the exit code, giving up \
                     after `timeout_seconds` (default 30, at most 120). Asks the user first. Use \
                     it to check connectivity or config from inside; it can't open a shell.",
                    ToolAccess::Act,
                    exec_in_container,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ConfigDetails, ContainerState, StateDetails};

    #[test]
    fn inspect_text_hides_env_values() {
        let container: Container = serde_json::from_value(serde_json::json!({
            "Id": "abcdef1234567890",
            "Names": ["/worker"],
            "Image": "trix-worker:dev",
            "State": "restarting",
            "Status": "Restarting (1) 5 seconds ago",
            "Ports": [],
            "Labels": {}
        }))
        .expect("container");
        assert_eq!(container.state, ContainerState::Restarting);
        let details = ContainerDetails {
            id: container.id.clone(),
            restart_count: 14,
            state: StateDetails {
                status: "restarting".into(),
                exit_code: 1,
                ..Default::default()
            },
            config: ConfigDetails {
                image: "trix-worker:dev".into(),
                env: Some(vec![
                    "REDIS_URL=redis://redis:6379".into(),
                    "API_TOKEN=abc".into(),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        let text = inspect_text(&container, &details);
        assert!(text.contains("14 reinícios"));
        assert!(text.contains("REDIS_URL=*** API_TOKEN=***"));
        assert!(!text.contains("redis://"));
        assert!(!text.contains("abc\n"));
    }
}
