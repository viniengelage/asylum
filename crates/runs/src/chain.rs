use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use collections::HashSet;
use gpui::{AsyncWindowContext, Entity, EntityId, SharedString, WeakEntity};
use project::TaskSourceKind;
use serde::Deserialize;
use task::{ReadyWhen, StepAction, TaskContext, TaskStep, TaskTemplate};
use terminal::Terminal;
use workspace::Workspace;

use crate::{
    launch::{DeviceEnvironment, apply_device},
    ready::{Readiness, port_is_open, wait_until_ready},
    store::{RunStore, task_terminals},
};

const TERMINAL_APPEAR_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Starting,
    /// Running, not ready yet (the port hasn't opened, the output hasn't matched).
    Waiting,
    /// Ready, still running (Metro).
    Ready,
    /// Was already running, so it wasn't started again.
    Reused,
    /// Ended with exit code 0.
    Done,
    Failed(SharedString),
    Skipped,
}

impl StepState {
    pub fn is_active(&self) -> bool {
        matches!(self, StepState::Starting | StepState::Waiting)
    }
}

#[derive(Clone, Debug)]
pub struct ChainStepRecord {
    pub label: SharedString,
    pub state: StepState,
    pub terminal: Option<WeakEntity<Terminal>>,
}

#[derive(Clone, Debug)]
pub struct ChainRecord {
    pub id: usize,
    pub label: SharedString,
    pub steps: Vec<ChainStepRecord>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
}

impl ChainRecord {
    pub fn is_running(&self) -> bool {
        self.finished_at.is_none()
    }

    pub fn failed_step(&self) -> Option<(&ChainStepRecord, &SharedString)> {
        self.steps.iter().find_map(|step| match &step.state {
            StepState::Failed(reason) => Some((step, reason)),
            _ => None,
        })
    }
}

/// The ready condition of step `index`: its own, or, for every step but the last, a clean
/// exit, since the next one usually needs what it built.
pub fn step_ready_when(step: &TaskStep, index: usize, step_count: usize) -> Option<ReadyWhen> {
    step.ready_when.clone().or_else(|| {
        let is_last = index + 1 == step_count;
        (!is_last && step.action.is_none()).then(ReadyWhen::exit)
    })
}

/// The task template a command step runs as: the step's command with the parent's cwd and env
/// under its own.
pub fn command_step_template(parent: &TaskTemplate, step: &TaskStep, index: usize) -> TaskTemplate {
    let mut env = parent.env.clone();
    env.extend(step.env.clone());
    TaskTemplate {
        label: format!("{} · {}", parent.label, step.display_label(index)),
        command: step.command.clone().unwrap_or_default(),
        args: step.args.clone(),
        env,
        cwd: step.cwd.clone().or_else(|| parent.cwd.clone()),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: task::RevealStrategy::NoFocus,
        reveal_target: parent.reveal_target,
        shell: parent.shell.clone(),
        ..TaskTemplate::default()
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct AppIdentifiers {
    pub ios_bundle_identifier: Option<String>,
    pub android_package: Option<String>,
}

#[derive(Deserialize)]
struct AppJson {
    expo: Option<ExpoConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpoConfig {
    ios: Option<ExpoIos>,
    android: Option<ExpoAndroid>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpoIos {
    bundle_identifier: Option<String>,
}

#[derive(Deserialize)]
struct ExpoAndroid {
    package: Option<String>,
}

/// The app's ids from an Expo `app.json`.
pub fn app_identifiers(app_json: &str) -> AppIdentifiers {
    let Ok(app) = serde_json::from_str::<AppJson>(app_json) else {
        return AppIdentifiers::default();
    };
    let expo = app.expo;
    AppIdentifiers {
        ios_bundle_identifier: expo
            .as_ref()
            .and_then(|expo| expo.ios.as_ref())
            .and_then(|ios| ios.bundle_identifier.clone()),
        android_package: expo
            .as_ref()
            .and_then(|expo| expo.android.as_ref())
            .and_then(|android| android.package.clone()),
    }
}

/// The command that opens the installed app on the device: `xcrun simctl launch` on iOS,
/// `adb shell monkey` on Android.
pub fn open_app_command(
    device: &DeviceEnvironment,
    identifiers: &AppIdentifiers,
) -> Result<(String, Vec<String>)> {
    match device.platform.as_str() {
        "ios" => {
            let bundle = identifiers
                .ios_bundle_identifier
                .clone()
                .ok_or_else(|| anyhow!(i18n::t!("runs.chain.no_bundle_id").to_string()))?;
            Ok((
                "xcrun".to_string(),
                vec![
                    "simctl".to_string(),
                    "launch".to_string(),
                    device.id.clone(),
                    bundle,
                ],
            ))
        }
        "android" => {
            let package = identifiers
                .android_package
                .clone()
                .ok_or_else(|| anyhow!(i18n::t!("runs.chain.no_android_package").to_string()))?;
            Ok((
                "adb".to_string(),
                vec![
                    "-s".to_string(),
                    device.id.clone(),
                    "shell".to_string(),
                    "monkey".to_string(),
                    "-p".to_string(),
                    package,
                    "-c".to_string(),
                    "android.intent.category.LAUNCHER".to_string(),
                    "1".to_string(),
                ],
            ))
        }
        _ => Err(anyhow!(i18n::t!("runs.chain.no_device").to_string())),
    }
}

fn step_directory(
    parent: &TaskTemplate,
    step: &TaskStep,
    task_context: &TaskContext,
) -> Option<PathBuf> {
    let configured = step
        .cwd
        .clone()
        .or_else(|| parent.cwd.clone())
        .map(PathBuf::from);
    match configured {
        Some(path) if path.is_absolute() => Some(path),
        Some(path) => task_context.cwd.as_ref().map(|cwd| cwd.join(path)),
        None => task_context.cwd.clone(),
    }
}

/// Runs a task's steps in order, each starting once the previous one is ready, and records
/// their states in the store so the Executions dock can show them.
pub async fn run_chain(
    store: Entity<RunStore>,
    workspace: WeakEntity<Workspace>,
    chain_id: usize,
    source_kind: TaskSourceKind,
    parent: TaskTemplate,
    task_context: TaskContext,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let steps = parent.run.steps.clone();
    let step_count = steps.len();
    for (index, step) in steps.iter().enumerate() {
        store.update(cx, |store, cx| {
            store.set_step_state(chain_id, index, StepState::Starting, None, cx)
        });
        let ready_when = step_ready_when(step, index, step_count);
        if let Some(ReadyWhen::Port { port }) = ready_when
            && cx
                .background_executor()
                .spawn(async move { port_is_open(port) })
                .await
        {
            store.update(cx, |store, cx| {
                store.set_step_state(chain_id, index, StepState::Reused, None, cx)
            });
            continue;
        }
        let outcome = run_step(
            &store,
            &workspace,
            chain_id,
            index,
            &source_kind,
            &parent,
            step,
            ready_when.as_ref(),
            &task_context,
            cx,
        )
        .await;
        match outcome {
            Ok(state) => {
                store.update(cx, |store, cx| {
                    store.set_step_state(chain_id, index, state, None, cx)
                });
            }
            Err(error) => {
                let reason = SharedString::from(error.to_string());
                store.update(cx, |store, cx| {
                    store.fail_chain(chain_id, index, reason, cx);
                });
                return Ok(());
            }
        }
    }
    store.update(cx, |store, cx| store.finish_chain(chain_id, cx));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    store: &Entity<RunStore>,
    workspace: &WeakEntity<Workspace>,
    chain_id: usize,
    index: usize,
    source_kind: &TaskSourceKind,
    parent: &TaskTemplate,
    step: &TaskStep,
    ready_when: Option<&ReadyWhen>,
    task_context: &TaskContext,
    cx: &mut AsyncWindowContext,
) -> Result<StepState> {
    let mut template = match (&step.task, step.action) {
        (_, Some(StepAction::OpenApp)) => {
            let device = cx
                .update(|_, cx| apply_device(&mut TaskTemplate::default(), parent, cx))??
                .ok_or_else(|| anyhow!(i18n::t!("runs.chain.no_device").to_string()))?;
            let directory = step_directory(parent, step, task_context);
            let identifiers = match directory {
                Some(directory) => read_app_identifiers(workspace, &directory, cx).await,
                None => AppIdentifiers::default(),
            };
            let (command, args) = open_app_command(&device, &identifiers)?;
            let mut template = command_step_template(parent, step, index);
            template.command = command;
            template.args = args;
            template
        }
        (Some(task_label), None) => {
            let found = store.read_with(cx, |store, _| {
                store.candidates().iter().find_map(|candidate| {
                    candidate
                        .template()
                        .filter(|template| template.label == *task_label)
                        .cloned()
                })
            });
            let mut template = found.ok_or_else(|| {
                anyhow!(
                    i18n::t!("runs.chain.task_not_found", label = task_label.as_str()).to_string()
                )
            })?;
            template.use_new_terminal = true;
            template
        }
        (None, None) => {
            if step
                .command
                .as_deref()
                .is_none_or(|command| command.trim().is_empty())
            {
                return Err(anyhow!(i18n::t!("runs.chain.empty_step").to_string()));
            }
            command_step_template(parent, step, index)
        }
    };
    cx.update(|_, cx| apply_device(&mut template, parent, cx))??;
    let label = template.label.clone();
    let before: HashSet<EntityId> = match workspace.upgrade() {
        Some(workspace) => cx.update(|_, cx| {
            task_terminals(&workspace, cx)
                .into_iter()
                .map(|(_, terminal)| terminal.entity_id())
                .collect()
        })?,
        None => HashSet::default(),
    };
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.schedule_task(
            source_kind.clone(),
            &template,
            task_context,
            false,
            window,
            cx,
        );
    })?;
    let terminal = wait_for_terminal(workspace, &label, &before, cx).await?;
    store.update(cx, |store, cx| {
        store.set_step_state(
            chain_id,
            index,
            StepState::Waiting,
            Some(terminal.downgrade()),
            cx,
        )
    });
    let Some(ready_when) = ready_when else {
        return Ok(StepState::Ready);
    };
    match wait_until_ready(ready_when, &terminal, cx).await? {
        Readiness::Exited => Ok(StepState::Done),
        Readiness::Ready => Ok(StepState::Ready),
    }
}

async fn read_app_identifiers(
    workspace: &WeakEntity<Workspace>,
    directory: &Path,
    cx: &mut AsyncWindowContext,
) -> AppIdentifiers {
    let Ok(fs) = workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).fs().clone()
    }) else {
        return AppIdentifiers::default();
    };
    match fs.load(&directory.join("app.json")).await {
        Ok(text) => app_identifiers(&text),
        Err(_) => AppIdentifiers::default(),
    }
}

async fn wait_for_terminal(
    workspace: &WeakEntity<Workspace>,
    label: &str,
    before: &HashSet<EntityId>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Terminal>> {
    let started = Instant::now();
    loop {
        let Some(workspace_entity) = workspace.upgrade() else {
            return Err(anyhow!(i18n::t!("runs.chain.workspace_closed").to_string()));
        };
        let found = cx.update(|_, cx| {
            task_terminals(&workspace_entity, cx)
                .into_iter()
                .map(|(_, terminal)| terminal)
                .find(|terminal| {
                    !before.contains(&terminal.entity_id())
                        && terminal.read(cx).task().is_some_and(|task| {
                            task.spawned_task.full_label == label
                                || task.spawned_task.label == label
                        })
                })
        })?;
        if let Some(terminal) = found {
            return Ok(terminal);
        }
        if started.elapsed() > TERMINAL_APPEAR_TIMEOUT {
            return Err(anyhow!(
                i18n::t!("runs.chain.did_not_start", label = label).to_string()
            ));
        }
        cx.background_executor().timer(TERMINAL_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_steps_wait_for_exit() {
        let build = TaskStep {
            command: Some("yarn".into()),
            ..TaskStep::default()
        };
        let metro = TaskStep {
            command: Some("yarn".into()),
            ready_when: Some(ReadyWhen::Port { port: 8081 }),
            ..TaskStep::default()
        };
        assert_eq!(step_ready_when(&build, 0, 3), Some(ReadyWhen::exit()));
        assert_eq!(step_ready_when(&build, 2, 3), None);
        assert_eq!(
            step_ready_when(&metro, 0, 3),
            Some(ReadyWhen::Port { port: 8081 })
        );
        let open_app = TaskStep {
            action: Some(StepAction::OpenApp),
            ..TaskStep::default()
        };
        assert_eq!(step_ready_when(&open_app, 1, 3), None);
    }

    #[test]
    fn step_template_inherits_cwd_and_env() {
        let mut parent = TaskTemplate {
            label: "Trix".into(),
            cwd: Some("/trix".into()),
            ..TaskTemplate::default()
        };
        parent.env.insert("A".into(), "1".into());
        let mut step = TaskStep {
            label: Some("Metro".into()),
            command: Some("yarn".into()),
            args: vec!["start".into()],
            ..TaskStep::default()
        };
        step.env.insert("B".into(), "2".into());
        let template = command_step_template(&parent, &step, 0);
        assert_eq!(template.label, "Trix · Metro");
        assert_eq!(template.cwd.as_deref(), Some("/trix"));
        assert_eq!(template.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(template.env.get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn reads_expo_app_identifiers() {
        let identifiers = app_identifiers(
            r#"{ "expo": { "name": "Trix", "ios": { "bundleIdentifier": "com.trix.app" }, "android": { "package": "com.trix" } } }"#,
        );
        assert_eq!(
            identifiers.ios_bundle_identifier.as_deref(),
            Some("com.trix.app")
        );
        assert_eq!(identifiers.android_package.as_deref(), Some("com.trix"));
        assert_eq!(app_identifiers("{}"), AppIdentifiers::default());

        let ios = DeviceEnvironment {
            id: "UDID".into(),
            platform: "ios".into(),
        };
        let (command, args) = open_app_command(&ios, &identifiers).expect("ios");
        assert_eq!(command, "xcrun");
        assert_eq!(args, vec!["simctl", "launch", "UDID", "com.trix.app"]);
        assert!(open_app_command(&ios, &AppIdentifiers::default()).is_err());
    }
}
