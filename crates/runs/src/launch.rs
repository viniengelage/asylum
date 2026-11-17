use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use gpui::{App, AsyncWindowContext, Entity, SharedString, TaskExt as _, WeakEntity, Window};
use project::TaskSourceKind;
use task::{DebugScenario, DeviceChoice, IfRunning, TaskContext, TaskTemplate};
use terminal::TaskStatus;
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::{
    chain::run_chain,
    sources::{RunCandidate, RunKind},
    store::{RunStore, default_task_context},
};

const REACT_NATIVE_ADAPTER: &str = "React Native / Expo";
const METRO_PORT: &str = "8081";
const RESTART_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The device a run targets, as its command sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEnvironment {
    pub id: String,
    /// `ios` or `android`.
    pub platform: String,
}

/// A device id a task names: simulator UDIDs are UUIDs, everything else is taken for an adb
/// serial.
pub fn platform_of_device_id(id: &str) -> &'static str {
    let is_uuid = id.len() == 36
        && id
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            });
    if is_uuid { "ios" } else { "android" }
}

/// Puts the device `owner` targets (its `device`, the Devices dock's selection by default)
/// into `template`'s environment as `ASYLUM_DEVICE_ID` and `ASYLUM_DEVICE_PLATFORM`, with
/// `ASYLUM_METRO_PORT`. Fails when the task asks for a device and none is selected.
pub fn apply_device(
    template: &mut TaskTemplate,
    owner: &TaskTemplate,
    cx: &App,
) -> Result<Option<DeviceEnvironment>> {
    let device = match owner.run.device_choice() {
        DeviceChoice::Selected => task_agents::active_device(cx).map(|device| DeviceEnvironment {
            id: device.id.to_string(),
            platform: device.platform.to_string(),
        }),
        DeviceChoice::Ask => Some(
            task_agents::active_device(cx)
                .map(|device| DeviceEnvironment {
                    id: device.id.to_string(),
                    platform: device.platform.to_string(),
                })
                .ok_or_else(|| anyhow!(i18n::t!("runs.device.pick_one").to_string()))?,
        ),
        DeviceChoice::Id(id) => Some(DeviceEnvironment {
            platform: platform_of_device_id(&id).to_string(),
            id,
        }),
    };
    if let Some(device) = &device {
        template
            .env
            .entry("ASYLUM_DEVICE_ID".to_string())
            .or_insert_with(|| device.id.clone());
        template
            .env
            .entry("ASYLUM_DEVICE_PLATFORM".to_string())
            .or_insert_with(|| device.platform.clone());
    }
    template
        .env
        .entry("ASYLUM_METRO_PORT".to_string())
        .or_insert_with(|| METRO_PORT.to_string());
    Ok(device)
}

/// Starts a candidate: a task through the workspace's task scheduling (history, toasts,
/// rerun), a chain of steps through the chain runner, a debug scenario through the debugger.
/// Debugging a task runs it and attaches the React Native debugger to the app on the device,
/// since that is what a task can be debugged with here.
pub fn launch(
    store: Entity<RunStore>,
    workspace: Entity<Workspace>,
    candidate: RunCandidate,
    debug: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let task_contexts = workspace.update(cx, |workspace, cx| {
        tasks_ui::task_contexts(workspace, window, cx)
    });
    let workspace_handle = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            let task_contexts = task_contexts.await;
            let shared_context = default_task_context(&task_contexts);
            let worktree_id = task_contexts.worktree();
            match candidate.kind {
                RunKind::Debug {
                    scenario,
                    worktree_id,
                } => workspace_handle.update_in(cx, |workspace, window, cx| {
                    start_debug_session(
                        workspace,
                        scenario,
                        shared_context,
                        worktree_id,
                        window,
                        cx,
                    )
                }),
                RunKind::Task {
                    source_kind,
                    template,
                } => {
                    let task_context: TaskContext = (*shared_context).clone();
                    let attach_device = cx
                        .update(|_, cx| task_agents::active_device(cx).map(|device| device.name))?;
                    let started = start_task(
                        &store,
                        &workspace_handle,
                        source_kind,
                        template.clone(),
                        task_context,
                        cx,
                    )
                    .await;
                    if let Err(error) = &started {
                        show_error(&workspace_handle, error.to_string(), cx);
                    }
                    if debug && started.is_ok() {
                        let scenario =
                            react_native_attach_scenario(&template.label, attach_device.as_deref());
                        workspace_handle.update_in(cx, |workspace, window, cx| {
                            start_debug_session(
                                workspace,
                                scenario,
                                shared_context,
                                worktree_id,
                                window,
                                cx,
                            )
                        })?;
                    }
                    Ok(())
                }
            }
        })
        .detach_and_log_err(cx);
}

fn show_error(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncWindowContext) {
    struct RunFailedToStart;
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<RunFailedToStart>(), message),
                cx,
            );
        })
        .log_err();
}

/// Starts a task by its `if_running`, `device` and `steps`. A chain resolves once its steps
/// have run; a plain task once it is scheduled.
pub async fn start_task(
    store: &Entity<RunStore>,
    workspace: &WeakEntity<Workspace>,
    source_kind: TaskSourceKind,
    mut template: TaskTemplate,
    task_context: TaskContext,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let label = SharedString::from(template.label.clone());
    let if_running = template.run.if_running;
    if !template.run.steps.is_empty() {
        let running_chain = store.read_with(cx, |store, _| {
            store.running_chain(&label).map(|chain| chain.id)
        });
        if let Some(chain_id) = running_chain {
            match if_running {
                Some(IfRunning::Reuse) | None => {
                    store.update(cx, |store, cx| store.record_reused(label.clone(), cx));
                    return Ok(());
                }
                Some(IfRunning::Restart) => {
                    store.update(cx, |store, cx| store.stop_chain(chain_id, cx));
                }
                Some(IfRunning::Parallel) => {}
            }
        }
        let step_labels = template
            .run
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| SharedString::from(step.display_label(index)))
            .collect();
        let chain_id = store.update(cx, |store, cx| {
            store.start_chain(label.clone(), step_labels, cx)
        });
        let task = cx.spawn({
            let store = store.clone();
            let workspace = workspace.clone();
            async move |cx| {
                run_chain(
                    store,
                    workspace,
                    chain_id,
                    source_kind,
                    template,
                    task_context,
                    cx,
                )
                .await
                .log_err();
            }
        });
        store.update(cx, |store, _| store.set_chain_task(chain_id, task));
        return Ok(());
    }

    let running = store.read_with(cx, |store, cx| store.running_terminal_for(&label, cx));
    if let Some(terminal) = running {
        match if_running {
            Some(IfRunning::Reuse) => {
                store.update(cx, |store, cx| store.record_reused(label.clone(), cx));
                workspace.update_in(cx, |workspace, window, cx| {
                    crate::store::reveal_terminal(workspace, &terminal, window, cx);
                })?;
                return Ok(());
            }
            Some(IfRunning::Restart) => {
                terminal.update(cx, |terminal, _| terminal.kill_active_task());
                let started = Instant::now();
                while terminal.read_with(cx, |terminal, _| {
                    terminal
                        .task()
                        .is_some_and(|task| task.status == TaskStatus::Running)
                }) && started.elapsed() < RESTART_STOP_TIMEOUT
                {
                    cx.background_executor().timer(RESTART_POLL_INTERVAL).await;
                }
            }
            Some(IfRunning::Parallel) => {
                template.allow_concurrent_runs = true;
                template.use_new_terminal = true;
            }
            None => {}
        }
    } else if if_running == Some(IfRunning::Parallel) {
        template.allow_concurrent_runs = true;
        template.use_new_terminal = true;
    }
    let owner = template.clone();
    cx.update(|_, cx| apply_device(&mut template, &owner, cx))??;
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.schedule_task(source_kind, &template, &task_context, false, window, cx);
    })?;
    Ok(())
}

fn react_native_attach_scenario(label: &str, device_name: Option<&str>) -> DebugScenario {
    let mut config = serde_json::json!({ "request": "attach", "port": 8081 });
    if let (Some(device_name), Some(object)) = (device_name, config.as_object_mut()) {
        object.insert("deviceName".to_string(), device_name.into());
    }
    DebugScenario {
        adapter: REACT_NATIVE_ADAPTER.into(),
        label: SharedString::from(i18n::t!("runs.debug_label", label = label).to_string()),
        build: None,
        config,
        tcp_connection: None,
    }
}

fn start_debug_session(
    workspace: &mut Workspace,
    scenario: DebugScenario,
    task_context: task::SharedTaskContext,
    worktree_id: Option<project::WorktreeId>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let Some(provider) = workspace.debugger_provider() else {
        struct NoDebugger;
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<NoDebugger>(),
                i18n::t!("runs.no_debugger").to_string(),
            )
            .autohide(),
            cx,
        );
        return;
    };
    provider.start_session(scenario, task_context, None, worktree_id, window, cx);
}

/// Dispatches an action of a crate this one doesn't depend on (the debugger's), by name.
pub fn dispatch_by_name(name: &str, window: &mut Window, cx: &mut App) {
    if let Some(action) = cx.build_action(name, None).log_err() {
        window.dispatch_action(action, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_simulators_from_emulators() {
        assert_eq!(
            platform_of_device_id("8F1C2A4E-1B2C-4D5E-9F00-112233445566"),
            "ios"
        );
        assert_eq!(platform_of_device_id("emulator-5554"), "android");
        assert_eq!(platform_of_device_id("R58M12345"), "android");
    }
}
