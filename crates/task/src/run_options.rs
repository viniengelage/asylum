use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Asylum's options for running a task: a chain of steps, when it counts as ready, what to do
/// when it is already running, which device it targets and its favorite slot. They sit at the
/// top level of the task in tasks.json.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct RunOptions {
    /// Steps run in order instead of `command`: each one starts when the previous one is ready.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<TaskStep>,
    /// When the task counts as ready, so the next step (or a debug session) can start without
    /// waiting for it to end: `{ "port": 8081 }`, `{ "output": "regex" }` or `"exit"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_when: Option<ReadyWhen>,
    /// What to do when the task is started while it already runs: `reuse` keeps the running
    /// one, `restart` stops it and starts again, `parallel` runs another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_running: Option<IfRunning>,
    /// The device the task targets: `selected` (the Devices dock's, the default), `ask`, or a
    /// device id. It reaches the command as `ASYLUM_DEVICE_ID` and `ASYLUM_DEVICE_PLATFORM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The favorite slot (1–9) the task takes, run with ctrl-alt-<slot>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 9))]
    pub favorite: Option<u8>,
}

impl RunOptions {
    pub fn is_empty(&self) -> bool {
        self == &RunOptions::default()
    }

    pub fn device_choice(&self) -> DeviceChoice {
        DeviceChoice::parse(self.device.as_deref())
    }
}

/// When a running task counts as ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReadyWhen {
    /// `"exit"`: when it ends with exit code 0.
    Exit(ReadyOnExit),
    /// `{ "port": 8081 }`: when something listens on the port on localhost.
    Port { port: u16 },
    /// `{ "output": "regex" }`: when a line of its output matches the regex.
    Output { output: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadyOnExit {
    Exit,
}

impl ReadyWhen {
    pub fn exit() -> Self {
        ReadyWhen::Exit(ReadyOnExit::Exit)
    }

    /// Whether the task has to end before it counts as ready.
    pub fn waits_for_exit(&self) -> bool {
        matches!(self, ReadyWhen::Exit(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IfRunning {
    Reuse,
    Restart,
    Parallel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceChoice {
    Selected,
    Ask,
    Id(String),
}

impl DeviceChoice {
    pub fn parse(device: Option<&str>) -> Self {
        match device.map(str::trim) {
            None | Some("") | Some("selected") => DeviceChoice::Selected,
            Some("ask") => DeviceChoice::Ask,
            Some(id) => DeviceChoice::Id(id.to_string()),
        }
    }
}

/// One step of a task with `steps`: a command, another task of the project by label, or an
/// Asylum action.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct TaskStep {
    /// The step's name in the run card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Another task of the project, by its label, to run as this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Defaults to the task's `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Added to the task's `env`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// When the step counts as ready; without it the step has to end with exit code 0, except
    /// the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_when: Option<ReadyWhen>,
    /// An Asylum action instead of a command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<StepAction>,
}

impl TaskStep {
    pub fn display_label(&self, index: usize) -> String {
        if let Some(label) = self.label.as_ref().filter(|label| !label.trim().is_empty()) {
            return label.clone();
        }
        if let Some(task) = &self.task {
            return task.clone();
        }
        if let Some(action) = self.action {
            return match action {
                StepAction::OpenApp => "open app".to_string(),
            };
        }
        match &self.command {
            Some(command) => std::iter::once(command.as_str())
                .chain(self.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            None => format!("step {}", index + 1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepAction {
    /// Opens the project's app on the task's device.
    OpenApp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskTemplate;

    #[test]
    fn parses_ready_when() {
        let exit: ReadyWhen = serde_json::from_str(r#""exit""#).expect("exit");
        assert_eq!(exit, ReadyWhen::exit());
        let port: ReadyWhen = serde_json::from_str(r#"{ "port": 8081 }"#).expect("port");
        assert_eq!(port, ReadyWhen::Port { port: 8081 });
        let output: ReadyWhen =
            serde_json::from_str(r#"{ "output": "Waiting on http://localhost:\\d+" }"#)
                .expect("output");
        assert_eq!(
            output,
            ReadyWhen::Output {
                output: "Waiting on http://localhost:\\d+".to_string()
            }
        );
        assert!(serde_json::from_str::<ReadyWhen>(r#""soon""#).is_err());
        assert!(serde_json::from_str::<ReadyWhen>(r#"{ "port": "x" }"#).is_err());
    }

    #[test]
    fn reads_run_options_from_a_task() {
        let task: TaskTemplate = serde_json::from_str(
            r#"{
                "label": "Trix no iPhone",
                "command": "",
                "if_running": "reuse",
                "device": "selected",
                "favorite": 1,
                "steps": [
                    { "label": "Metro", "command": "yarn", "args": ["start"], "ready_when": { "port": 8081 } },
                    { "command": "yarn", "args": ["ios", "--device", "$ASYLUM_DEVICE_ID"] },
                    { "action": "open_app" }
                ]
            }"#,
        )
        .expect("task");
        assert_eq!(task.run.if_running, Some(IfRunning::Reuse));
        assert_eq!(task.run.device_choice(), DeviceChoice::Selected);
        assert_eq!(task.run.favorite, Some(1));
        assert_eq!(task.run.steps.len(), 3);
        assert_eq!(
            task.run.steps[0].ready_when,
            Some(ReadyWhen::Port { port: 8081 })
        );
        assert_eq!(
            task.run.steps[1].display_label(1),
            "yarn ios --device $ASYLUM_DEVICE_ID"
        );
        assert_eq!(task.run.steps[2].action, Some(StepAction::OpenApp));

        let plain: TaskTemplate =
            serde_json::from_str(r#"{ "label": "build", "command": "make" }"#).expect("plain");
        assert!(plain.run.is_empty());
        let written = serde_json::to_value(&plain).expect("serialize");
        assert!(written.get("steps").is_none());
        assert!(written.get("ready_when").is_none());
    }

    #[test]
    fn parses_device_choice() {
        assert_eq!(DeviceChoice::parse(None), DeviceChoice::Selected);
        assert_eq!(DeviceChoice::parse(Some("ask")), DeviceChoice::Ask);
        assert_eq!(
            DeviceChoice::parse(Some("emulator-5554")),
            DeviceChoice::Id("emulator-5554".into())
        );
    }
}
