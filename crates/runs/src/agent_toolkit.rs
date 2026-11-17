use std::fmt::Write as _;

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AsyncApp, Entity, Task};
use project::{Project, debugger::session::ThreadStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool};
use workspace::{MultiWorkspace, Workspace};

use crate::{
    RunState,
    chain::StepState,
    format_duration,
    store::{RunStore, refresh_candidates},
};

pub const TOOLKIT_ID: &str = "runs";
const DEFAULT_OUTPUT_LINES: usize = 60;
const MAX_OUTPUT_LINES: usize = 400;

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct RunInput {
    /// The run's name as `runs_list` shows it (a tasks.json label, a package.json script, an
    /// eas.json profile). Matched without case; a unique part of the name is enough.
    name: String,
    /// Start it under the debugger (React Native: runs it and attaches to the app).
    #[serde(default)]
    debug: bool,
}

#[derive(Deserialize, JsonSchema)]
struct OutputInput {
    /// The run's name; the most recent run when left out.
    #[serde(default)]
    name: Option<String>,
    /// How many of the last output lines to return (default 60, at most 400).
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct StopInput {
    /// The running run's name.
    name: String,
}

fn workspace_for(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(gpui::WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .find_map(|window| {
            let workspace = window.read(cx).ok().and_then(|multi_workspace| {
                multi_workspace
                    .workspaces()
                    .find(|workspace| workspace.read(cx).project() == project)
                    .cloned()
            })?;
            Some((window, workspace))
        })
}

fn store_for(
    project: &Entity<Project>,
    cx: &mut App,
) -> Result<(
    gpui::WindowHandle<MultiWorkspace>,
    Entity<Workspace>,
    Entity<RunStore>,
)> {
    let (window, workspace) =
        workspace_for(project, cx).context("This project has no open window.")?;
    let store = RunStore::for_workspace(workspace.downgrade(), project.clone(), cx);
    Ok((window, workspace, store))
}

fn matches_name(label: &str, name: &str) -> bool {
    label.to_lowercase().contains(&name.trim().to_lowercase())
}

fn state_text(state: RunState) -> String {
    match state {
        RunState::Running => "running".to_string(),
        RunState::Succeeded => "ok".to_string(),
        RunState::Failed {
            exit_code: Some(code),
        } => format!("failed with exit {code}"),
        RunState::Failed { exit_code: None } => "failed".to_string(),
        RunState::StoppedByUser => "stopped by the user".to_string(),
        RunState::Reused => "already running, reused".to_string(),
    }
}

async fn reload_candidates(
    window: gpui::WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let refresh = window.update(cx, |_, window, cx| {
        refresh_candidates(&workspace, window, cx)
    })?;
    refresh.await;
    Ok(())
}

fn list_runs(
    project: Entity<Project>,
    _input: NoInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    let found = store_for(&project, cx);
    cx.spawn(async move |cx| {
        let (window, workspace, store) = found?;
        reload_candidates(window, workspace, cx).await?;
        let text = store.read_with(cx, |store, cx| {
            let mut text = String::new();
            text.push_str("Runs of the project (name · source · command):\n");
            for candidate in store.candidates() {
                let slot = store
                    .favorite_slot(&candidate.key)
                    .map(|slot| format!(" · favorite {slot}"))
                    .unwrap_or_default();
                writeln!(
                    text,
                    "- {} · {} · {}{slot}",
                    candidate.label,
                    candidate.origin.chip_label(),
                    candidate.detail
                )
                .ok();
            }
            let running: Vec<String> = store
                .running()
                .map(|record| {
                    format!(
                        "- {} ({})",
                        record.label,
                        format_duration(record.duration())
                    )
                })
                .collect();
            if !running.is_empty() {
                text.push_str("\nRunning now:\n");
                text.push_str(&running.join("\n"));
                text.push('\n');
            }
            if let Some(device) = task_agents::active_device(cx) {
                writeln!(
                    text,
                    "\nTarget device: {} ({}, {})",
                    device.name, device.platform, device.id
                )
                .ok();
            }
            text
        });
        Ok(ToolkitOutput::text(text))
    })
}

fn run_task(
    project: Entity<Project>,
    input: RunInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    let found = store_for(&project, cx);
    cx.spawn(async move |cx| {
        let (window, workspace, store) = found?;
        reload_candidates(window, workspace, cx).await?;
        let candidate = store.read_with(cx, |store, _| {
            let exact = store
                .candidates()
                .iter()
                .find(|candidate| candidate.label.eq_ignore_ascii_case(input.name.trim()));
            exact
                .or_else(|| {
                    store
                        .candidates()
                        .iter()
                        .find(|candidate| matches_name(&candidate.label, &input.name))
                })
                .cloned()
        });
        let candidate = candidate.ok_or_else(|| {
            anyhow!(
                "No run called “{}”. Call runs_list to see the names.",
                input.name
            )
        })?;
        let label = candidate.label.clone();
        let debug = input.debug;
        window.update(cx, |_, window, cx| {
            store.update(cx, |store, cx| {
                store.run_candidate(candidate, debug, window, cx)
            });
        })?;
        Ok(ToolkitOutput::text(format!(
            "Started “{label}”{}. Call runs_output with this name to read its output.",
            if debug { " under the debugger" } else { "" }
        )))
    })
}

fn run_output(
    project: Entity<Project>,
    input: OutputInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    let found = store_for(&project, cx);
    let result = found.and_then(|(_, _, store)| {
        let lines = input
            .lines
            .unwrap_or(DEFAULT_OUTPUT_LINES)
            .clamp(1, MAX_OUTPUT_LINES);
        let store = store.read(cx);
        let mut text = String::new();
        if let Some(name) = &input.name
            && let Some(chain) = store
                .chains()
                .iter()
                .rev()
                .find(|chain| matches_name(&chain.label, name))
        {
            writeln!(text, "“{}” has steps:", chain.label).ok();
            for step in &chain.steps {
                let state = match &step.state {
                    StepState::Pending => "pending".to_string(),
                    StepState::Starting | StepState::Waiting => "waiting to be ready".to_string(),
                    StepState::Ready => "ready".to_string(),
                    StepState::Reused => "already running, reused".to_string(),
                    StepState::Done => "done".to_string(),
                    StepState::Failed(reason) => format!("failed: {reason}"),
                    StepState::Skipped => "skipped".to_string(),
                };
                writeln!(text, "- {}: {state}", step.label).ok();
            }
            text.push('\n');
        }
        let record = store
            .records()
            .iter()
            .rev()
            .find(|record| {
                input
                    .name
                    .as_ref()
                    .is_none_or(|name| matches_name(&record.label, name))
            })
            .cloned();
        let Some(record) = record else {
            if text.is_empty() {
                return Err(anyhow!("No run with that name has started in this window."));
            }
            return Ok(text);
        };
        writeln!(
            text,
            "“{}” ({}): {} after {}",
            record.label,
            record.command_label,
            state_text(record.state),
            format_duration(record.duration())
        )
        .ok();
        let output = match record.terminal.upgrade() {
            Some(terminal) => terminal.read(cx).last_n_non_empty_lines(lines),
            None => {
                let skip = record.output_tail.len().saturating_sub(lines);
                record.output_tail.iter().skip(skip).cloned().collect()
            }
        };
        writeln!(text, "Last {} lines of output:", output.len()).ok();
        text.push_str(&output.join("\n"));
        Ok(text)
    });
    Task::ready(result.map(ToolkitOutput::text))
}

fn stop_run(
    project: Entity<Project>,
    input: StopInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    let result = store_for(&project, cx).and_then(|(_, _, store)| {
        store.update(cx, |store, cx| {
            let chain = store
                .chains()
                .iter()
                .rev()
                .find(|chain| chain.is_running() && matches_name(&chain.label, &input.name))
                .map(|chain| (chain.id, chain.label.clone()));
            if let Some((chain_id, label)) = chain {
                store.stop_chain(chain_id, cx);
                return Ok(format!("Stopped “{label}” and its steps."));
            }
            let record = store
                .running()
                .find(|record| matches_name(&record.label, &input.name))
                .map(|record| (record.id, record.label.clone()));
            match record {
                Some((record_id, label)) => {
                    store.stop(record_id, cx);
                    Ok(format!("Stopped “{label}”."))
                }
                None => Err(anyhow!("Nothing called “{}” is running.", input.name)),
            }
        })
    });
    Task::ready(result.map(ToolkitOutput::text))
}

fn debug_state(
    project: Entity<Project>,
    _input: NoInput,
    cx: &mut App,
) -> Task<Result<ToolkitOutput>> {
    let result = workspace_for(&project, cx)
        .context("This project has no open window.")
        .map(|(_, workspace)| {
            let status = workspace
                .read(cx)
                .debugger_provider()
                .and_then(|provider| provider.active_thread_state(cx));
            match status {
                None => "No debug session.".to_string(),
                Some(ThreadStatus::Stopped) => {
                    "The debug session is paused (stopped at a breakpoint or a step). The \
                     Debug dock shows where; the user continues with F5."
                        .to_string()
                }
                Some(ThreadStatus::Running) => "The debug session is running.".to_string(),
                Some(ThreadStatus::Stepping) => "The debug session is stepping.".to_string(),
                Some(ThreadStatus::Exited | ThreadStatus::Ended) => {
                    "The debug session ended.".to_string()
                }
            }
        });
    Task::ready(result.map(ToolkitOutput::text))
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_toolkit(
        Toolkit {
            id: TOOLKIT_ID.into(),
            name: i18n::t!("runs.agent.toolkit_name"),
            description: i18n::t!("runs.agent.toolkit_description"),
            icon: "play_outlined".into(),
            mention: None,
            tools: vec![
                ToolkitTool::new(
                    "runs_list",
                    i18n::t!("runs.agent.tool_list"),
                    "Lists what the project can run (tasks.json tasks, package.json scripts, \
                     eas.json build profiles, debug scenarios), what is running now and the \
                     device runs target. Use these names with runs_start, runs_output and \
                     runs_stop instead of starting dev servers in a terminal of your own.",
                    ToolAccess::Read,
                    list_runs,
                ),
                ToolkitTool::new(
                    "runs_start",
                    i18n::t!("runs.agent.tool_start"),
                    "Starts one of the project's runs by name, in the user's terminal, the same \
                     as the run picker does (honouring its steps, ready_when and if_running). \
                     With debug: true it runs under the debugger.",
                    ToolAccess::Act,
                    run_task,
                ),
                ToolkitTool::new(
                    "runs_output",
                    i18n::t!("runs.agent.tool_output"),
                    "Returns how a run is going or how it ended (state, exit code, steps) and the \
                     last lines of its output.",
                    ToolAccess::Read,
                    run_output,
                ),
                ToolkitTool::new(
                    "runs_stop",
                    i18n::t!("runs.agent.tool_stop"),
                    "Stops a running run (and the steps it started) by name.",
                    ToolAccess::Act,
                    stop_run,
                ),
                ToolkitTool::new(
                    "debug_state",
                    i18n::t!("runs.agent.tool_debug_state"),
                    "Says whether a debug session is running, paused or gone.",
                    ToolAccess::Read,
                    debug_state,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_names_loosely() {
        assert!(matches_name("eas build · preview · ios", "preview · IOS"));
        assert!(matches_name("start", " start "));
        assert!(!matches_name("start", "ios"));
    }
}
