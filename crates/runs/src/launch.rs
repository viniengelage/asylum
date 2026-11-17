use gpui::{App, Entity, SharedString, TaskExt as _, Window};
use project::TaskSourceKind;
use task::{DebugScenario, TaskTemplate};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::{
    sources::{RunCandidate, RunKind},
    store::{RunStore, default_task_context},
};

const REACT_NATIVE_ADAPTER: &str = "React Native / Expo";

/// Starts a candidate: a task through the workspace's task scheduling (history, toasts,
/// rerun), a debug scenario through the debugger. Debugging a task runs it and attaches the
/// React Native debugger to the app, since that is what a task can be debugged with here.
pub fn launch(
    _store: Entity<RunStore>,
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
            let task_context = default_task_context(&task_contexts);
            workspace_handle.update_in(cx, |workspace, window, cx| match candidate.kind {
                RunKind::Debug {
                    scenario,
                    worktree_id,
                } => {
                    start_debug_session(workspace, scenario, task_context, worktree_id, window, cx)
                }
                RunKind::Task {
                    source_kind,
                    template,
                } => {
                    run_task(
                        workspace,
                        source_kind,
                        template.clone(),
                        &task_context,
                        window,
                        cx,
                    );
                    if debug {
                        let scenario = react_native_attach_scenario(&template.label);
                        start_debug_session(
                            workspace,
                            scenario,
                            task_context,
                            task_contexts.worktree(),
                            window,
                            cx,
                        );
                    }
                }
            })
        })
        .detach_and_log_err(cx);
}

pub fn run_task(
    workspace: &mut Workspace,
    source_kind: TaskSourceKind,
    mut template: TaskTemplate,
    task_context: &task::TaskContext,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    if let Some(device) = task_agents::active_device(cx) {
        template
            .env
            .entry("ASYLUM_DEVICE_ID".to_string())
            .or_insert_with(|| device.id.to_string());
        template
            .env
            .entry("ASYLUM_DEVICE_PLATFORM".to_string())
            .or_insert_with(|| device.platform.to_string());
    }
    template
        .env
        .entry("ASYLUM_METRO_PORT".to_string())
        .or_insert_with(|| "8081".to_string());
    workspace.schedule_task(source_kind, &template, task_context, false, window, cx);
}

fn react_native_attach_scenario(label: &str) -> DebugScenario {
    DebugScenario {
        adapter: REACT_NATIVE_ADAPTER.into(),
        label: SharedString::from(i18n::t!("runs.debug_label", label = label).to_string()),
        build: None,
        config: serde_json::json!({ "request": "attach", "port": 8081 }),
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
