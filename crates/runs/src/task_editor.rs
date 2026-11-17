use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, WeakEntity, Window,
};
use serde_json::{Map, Value};
use task::{IfRunning, ReadyWhen, StepAction, TaskStep, TaskTemplate};
use ui::{
    pill_button, prelude::*, secondary_button, segment_option, segmented_control, text_field,
};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    item::{Item, ItemEvent},
};

const SAVE_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyChoice {
    None,
    Exit,
    Port,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Selected,
    Ask,
    Fixed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepForm {
    pub label: String,
    pub command_line: String,
    /// `exit`, `port 8081`, or a regex the output has to match; empty for none.
    pub ready: String,
    pub action: Option<StepAction>,
}

/// What the task editor shows, as plain text; [`apply_form`] writes it back into the task's
/// JSON object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskForm {
    pub label: String,
    pub command_line: String,
    pub cwd: String,
    /// `KEY=value`, one per line.
    pub env: String,
    pub ready: ReadyChoice,
    pub ready_value: String,
    pub if_running: Option<IfRunning>,
    pub device: DeviceKind,
    pub device_id: String,
    pub favorite: Option<u8>,
    pub steps: Vec<StepForm>,
}

fn join_command(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().map(|arg| {
            shlex::try_quote(arg)
                .map(|quoted| quoted.into_owned())
                .unwrap_or_else(|_| arg.clone())
        }))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_command(command_line: &str) -> (String, Vec<String>) {
    let parts = shlex::split(command_line).unwrap_or_else(|| {
        command_line
            .split_whitespace()
            .map(str::to_string)
            .collect()
    });
    let mut parts = parts.into_iter();
    let command = parts.next().unwrap_or_default();
    (command, parts.collect())
}

fn ready_text(ready_when: Option<&ReadyWhen>) -> String {
    match ready_when {
        None => String::new(),
        Some(ReadyWhen::Exit(_)) => "exit".to_string(),
        Some(ReadyWhen::Port { port }) => format!("port {port}"),
        Some(ReadyWhen::Output { output }) => output.clone(),
    }
}

pub fn parse_ready_text(text: &str) -> Option<ReadyWhen> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text == "exit" {
        return Some(ReadyWhen::exit());
    }
    let port_text = text
        .strip_prefix("port")
        .map(str::trim)
        .or_else(|| text.strip_prefix(':'))
        .unwrap_or(text);
    if let Ok(port) = port_text.parse::<u16>() {
        return Some(ReadyWhen::Port { port });
    }
    Some(ReadyWhen::Output {
        output: text.to_string(),
    })
}

pub fn template_to_form(template: &TaskTemplate) -> TaskForm {
    let (ready, ready_value) = match &template.run.ready_when {
        None => (ReadyChoice::None, String::new()),
        Some(ReadyWhen::Exit(_)) => (ReadyChoice::Exit, String::new()),
        Some(ReadyWhen::Port { port }) => (ReadyChoice::Port, port.to_string()),
        Some(ReadyWhen::Output { output }) => (ReadyChoice::Output, output.clone()),
    };
    let (device, device_id) = match template.run.device.as_deref().map(str::trim) {
        None | Some("") | Some("selected") => (DeviceKind::Selected, String::new()),
        Some("ask") => (DeviceKind::Ask, String::new()),
        Some(id) => (DeviceKind::Fixed, id.to_string()),
    };
    let mut env: Vec<String> = template
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    env.sort();
    TaskForm {
        label: template.label.clone(),
        command_line: join_command(&template.command, &template.args),
        cwd: template.cwd.clone().unwrap_or_default(),
        env: env.join("\n"),
        ready,
        ready_value,
        if_running: template.run.if_running,
        device,
        device_id,
        favorite: template.run.favorite,
        steps: template
            .run
            .steps
            .iter()
            .map(|step| StepForm {
                label: step.label.clone().unwrap_or_default(),
                command_line: step
                    .command
                    .as_deref()
                    .map(|command| join_command(command, &step.args))
                    .or_else(|| step.task.as_ref().map(|task| format!("@{task}")))
                    .unwrap_or_default(),
                ready: ready_text(step.ready_when.as_ref()),
                action: step.action,
            })
            .collect(),
    }
}

fn set_or_remove(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            object.insert(key.to_string(), value);
        }
        None => {
            object.remove(key);
        }
    }
}

fn non_empty(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

/// Writes the form into the task's JSON object, keeping the keys the editor doesn't show.
pub fn apply_form(form: &TaskForm, object: &mut Map<String, Value>) -> Result<()> {
    object.insert("label".to_string(), Value::from(form.label.trim()));
    let (command, args) = split_command(&form.command_line);
    object.insert("command".to_string(), Value::from(command));
    set_or_remove(
        object,
        "args",
        (!args.is_empty()).then(|| Value::from(args)),
    );
    set_or_remove(object, "cwd", non_empty(&form.cwd).map(Value::from));
    let mut env = Map::new();
    for line in form.env.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!(i18n::t!("runs.editor.bad_env", line = line).to_string()))?;
        env.insert(key.trim().to_string(), Value::from(value.trim()));
    }
    set_or_remove(
        object,
        "env",
        (!env.is_empty()).then_some(Value::Object(env)),
    );
    let ready_when = match form.ready {
        ReadyChoice::None => None,
        ReadyChoice::Exit => Some(ReadyWhen::exit()),
        ReadyChoice::Port => Some(ReadyWhen::Port {
            port: form
                .ready_value
                .trim()
                .parse()
                .map_err(|_| anyhow!(i18n::t!("runs.editor.bad_port").to_string()))?,
        }),
        ReadyChoice::Output => non_empty(&form.ready_value).map(|output| ReadyWhen::Output {
            output: output.to_string(),
        }),
    };
    set_or_remove(
        object,
        "ready_when",
        ready_when.map(serde_json::to_value).transpose()?,
    );
    set_or_remove(
        object,
        "if_running",
        form.if_running.map(serde_json::to_value).transpose()?,
    );
    let device = match form.device {
        DeviceKind::Selected => None,
        DeviceKind::Ask => Some("ask".to_string()),
        DeviceKind::Fixed => non_empty(&form.device_id).map(str::to_string),
    };
    set_or_remove(object, "device", device.map(Value::from));
    set_or_remove(object, "favorite", form.favorite.map(Value::from));
    let steps: Vec<TaskStep> = form
        .steps
        .iter()
        .map(|step| {
            let mut task_step = TaskStep {
                label: non_empty(&step.label).map(str::to_string),
                ready_when: parse_ready_text(&step.ready),
                action: step.action,
                ..TaskStep::default()
            };
            if step.action.is_none() {
                let command_line = step.command_line.trim();
                if let Some(task) = command_line.strip_prefix('@') {
                    task_step.task = Some(task.trim().to_string());
                } else if !command_line.is_empty() {
                    let (command, args) = split_command(command_line);
                    task_step.command = Some(command);
                    task_step.args = args;
                }
            }
            task_step
        })
        .collect();
    set_or_remove(
        object,
        "steps",
        (!steps.is_empty())
            .then(|| serde_json::to_value(&steps))
            .transpose()?,
    );
    Ok(())
}

/// Replaces the task labelled `original_label` in a tasks.json text with the form (appending
/// it when it isn't there). The file is rewritten as plain JSON, so comments in it are lost.
pub fn replace_task_in_text(text: &str, original_label: &str, form: &TaskForm) -> Result<String> {
    let mut tasks: Vec<Value> = if text.trim().is_empty() {
        Vec::new()
    } else {
        serde_json_lenient::from_str(text).context("tasks.json is not a JSON array")?
    };
    let index = tasks
        .iter()
        .position(|task| task.get("label").and_then(Value::as_str) == Some(original_label));
    let mut object = index
        .and_then(|index| tasks.get(index))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    apply_form(form, &mut object)?;
    match index.and_then(|index| tasks.get_mut(index)) {
        Some(task) => *task = Value::Object(object),
        None => tasks.push(Value::Object(object)),
    }
    let mut text = serde_json::to_string_pretty(&tasks)?;
    text.push('\n');
    Ok(text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveStatus {
    Saved,
    Saving,
    Failed(SharedString),
}

struct StepEditors {
    label: Entity<Editor>,
    command: Entity<Editor>,
    ready: Entity<Editor>,
    action: Option<StepAction>,
    _subscriptions: Vec<Subscription>,
}

/// A task of a tasks.json file as a form, in an editor tab; every change saves the file.
pub struct TaskEditor {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    file: PathBuf,
    original_label: String,
    label: Entity<Editor>,
    command: Entity<Editor>,
    cwd: Entity<Editor>,
    env: Entity<Editor>,
    ready_value: Entity<Editor>,
    device_id: Entity<Editor>,
    ready: ReadyChoice,
    if_running: Option<IfRunning>,
    device: DeviceKind,
    favorite: Option<u8>,
    steps: Vec<StepEditors>,
    status: SaveStatus,
    save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

fn new_editor(
    text: &str,
    placeholder: &str,
    multi_line: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = if multi_line {
            Editor::auto_height(2, 8, window, cx)
        } else {
            Editor::single_line(window, cx)
        };
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

impl TaskEditor {
    fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        file: PathBuf,
        template: &TaskTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let form = template_to_form(template);
        let label = new_editor(&form.label, "Metro + iOS", false, window, cx);
        let command = new_editor(&form.command_line, "yarn start", false, window, cx);
        let cwd = new_editor(&form.cwd, "$ZED_WORKTREE_ROOT", false, window, cx);
        let env = new_editor(&form.env, "KEY=value", true, window, cx);
        let ready_value = new_editor(&form.ready_value, "8081", false, window, cx);
        let device_id = new_editor(&form.device_id, "emulator-5554", false, window, cx);
        let subscriptions = [&label, &command, &cwd, &env, &ready_value, &device_id]
            .into_iter()
            .map(|editor| Self::subscribe_editor(editor, cx))
            .collect();
        let mut this = Self {
            workspace,
            fs,
            file,
            original_label: form.label.clone(),
            label,
            command,
            cwd,
            env,
            ready_value,
            device_id,
            ready: form.ready,
            if_running: form.if_running,
            device: form.device,
            favorite: form.favorite,
            steps: Vec::new(),
            status: SaveStatus::Saved,
            save_task: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        for step in &form.steps {
            this.push_step(step, window, cx);
        }
        this
    }

    fn subscribe_editor(editor: &Entity<Editor>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited { .. }) {
                this.schedule_save(cx);
            }
        })
    }

    fn push_step(&mut self, step: &StepForm, window: &mut Window, cx: &mut Context<Self>) {
        let label = new_editor(&step.label, "Metro", false, window, cx);
        let command = new_editor(&step.command_line, "yarn start", false, window, cx);
        let ready = new_editor(&step.ready, "port 8081", false, window, cx);
        let subscriptions = [&label, &command, &ready]
            .into_iter()
            .map(|editor| Self::subscribe_editor(editor, cx))
            .collect();
        self.steps.push(StepEditors {
            label,
            command,
            ready,
            action: step.action,
            _subscriptions: subscriptions,
        });
    }

    fn form(&self, cx: &App) -> TaskForm {
        TaskForm {
            label: self.label.read(cx).text(cx),
            command_line: self.command.read(cx).text(cx),
            cwd: self.cwd.read(cx).text(cx),
            env: self.env.read(cx).text(cx),
            ready: self.ready,
            ready_value: self.ready_value.read(cx).text(cx),
            if_running: self.if_running,
            device: self.device,
            device_id: self.device_id.read(cx).text(cx),
            favorite: self.favorite,
            steps: self
                .steps
                .iter()
                .map(|step| StepForm {
                    label: step.label.read(cx).text(cx),
                    command_line: step.command.read(cx).text(cx),
                    ready: step.ready.read(cx).text(cx),
                    action: step.action,
                })
                .collect(),
        }
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.status = SaveStatus::Saving;
        cx.notify();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok((fs, file, original_label, form)) = this.read_with(cx, |this, cx| {
                (
                    this.fs.clone(),
                    this.file.clone(),
                    this.original_label.clone(),
                    this.form(cx),
                )
            }) else {
                return;
            };
            let result = async {
                let text = if fs.is_file(&file).await {
                    fs.load(&file).await?
                } else {
                    String::new()
                };
                let new_text = replace_task_in_text(&text, &original_label, &form)?;
                fs.atomic_write(file, new_text).await?;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.original_label = form.label.trim().to_string();
                        this.status = SaveStatus::Saved;
                    }
                    Err(error) => {
                        this.status = SaveStatus::Failed(error.to_string().into());
                    }
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open_json(&self, window: &mut Window, cx: &mut App) {
        let file = self.file.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(
                        file,
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..OpenOptions::default()
                        },
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn render_field(
        &self,
        title: SharedString,
        editor: &Entity<Editor>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        v_flex().gap_1().child(field_title(title)).child(
            text_field(editor.focus_handle(cx).contains_focused(window, cx), cx)
                .child(div().flex_1().min_w_0().child(editor.clone())),
        )
    }

    fn render_steps(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1p5()
            .child(field_title(i18n::t!("runs.editor.steps")))
            .when(self.steps.is_empty(), |this| {
                this.child(
                    Label::new(i18n::t!("runs.editor.steps_empty"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(self.steps.iter().enumerate().map(|(index, step)| {
                let remove = IconButton::new(("remove-step", index), IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(ui::Tooltip::text(i18n::t!("runs.editor.remove_step")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if index < this.steps.len() {
                            this.steps.remove(index);
                            this.schedule_save(cx);
                        }
                    }));
                let number = Label::new(format!("{}", index + 1))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted);
                match step.action {
                    Some(StepAction::OpenApp) => h_flex()
                        .w_full()
                        .gap_2()
                        .child(number)
                        .child(
                            Icon::new(IconName::Smartphone)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(i18n::t!("runs.editor.open_app_step"))
                                .size(LabelSize::Small),
                        )
                        .child(div().flex_1())
                        .child(remove)
                        .into_any_element(),
                    None => h_flex()
                        .w_full()
                        .gap_2()
                        .child(number)
                        .child(
                            div().w(px(130.)).flex_none().child(
                                text_field(
                                    step.label.focus_handle(cx).contains_focused(window, cx),
                                    cx,
                                )
                                .child(step.label.clone()),
                            ),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                text_field(
                                    step.command.focus_handle(cx).contains_focused(window, cx),
                                    cx,
                                )
                                .child(step.command.clone()),
                            ),
                        )
                        .child(
                            div().w(px(150.)).flex_none().child(
                                text_field(
                                    step.ready.focus_handle(cx).contains_focused(window, cx),
                                    cx,
                                )
                                .child(step.ready.clone()),
                            ),
                        )
                        .child(remove)
                        .into_any_element(),
                }
            }))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        pill_button(
                            "add-step",
                            Some(IconName::Plus),
                            i18n::t!("runs.editor.add_step"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.push_step(&StepForm::default(), window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        pill_button(
                            "add-open-app-step",
                            Some(IconName::Smartphone),
                            i18n::t!("runs.editor.add_open_app"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.push_step(
                                &StepForm {
                                    action: Some(StepAction::OpenApp),
                                    ..StepForm::default()
                                },
                                window,
                                cx,
                            );
                            this.schedule_save(cx);
                        })),
                    ),
            )
            .child(
                Label::new(i18n::t!("runs.editor.steps_hint"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

fn field_title(title: SharedString) -> impl IntoElement {
    Label::new(title)
        .size(LabelSize::XSmall)
        .weight(gpui::FontWeight::SEMIBOLD)
        .color(Color::Muted)
}

impl Render for TaskEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready_options = [
            (ReadyChoice::None, i18n::t!("runs.editor.ready_none")),
            (ReadyChoice::Exit, i18n::t!("runs.editor.ready_exit")),
            (ReadyChoice::Port, i18n::t!("runs.editor.ready_port")),
            (ReadyChoice::Output, i18n::t!("runs.editor.ready_output")),
        ];
        let if_running_options = [
            (None, i18n::t!("runs.editor.if_running_wait")),
            (
                Some(IfRunning::Reuse),
                i18n::t!("runs.editor.if_running_reuse"),
            ),
            (
                Some(IfRunning::Restart),
                i18n::t!("runs.editor.if_running_restart"),
            ),
            (
                Some(IfRunning::Parallel),
                i18n::t!("runs.editor.if_running_parallel"),
            ),
        ];
        let device_options = [
            (
                DeviceKind::Selected,
                i18n::t!("runs.editor.device_selected"),
            ),
            (DeviceKind::Ask, i18n::t!("runs.editor.device_ask")),
            (DeviceKind::Fixed, i18n::t!("runs.editor.device_fixed")),
        ];
        let status = match &self.status {
            SaveStatus::Saved => Label::new(i18n::t!("runs.editor.saved"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            SaveStatus::Saving => Label::new(i18n::t!("runs.editor.saving"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            SaveStatus::Failed(error) => Label::new(error.clone())
                .size(LabelSize::XSmall)
                .color(Color::Error),
        };
        v_flex()
            .id("task-editor")
            .key_context("TaskEditor")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w(px(760.))
                    .w_full()
                    .p_6()
                    .gap_4()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Label::new(i18n::t!("runs.editor.title"))
                                    .size(LabelSize::Large)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .child(div().flex_1())
                            .child(status)
                            .child(
                                secondary_button(
                                    "task-editor-json",
                                    Some((IconName::Code, Color::Muted)),
                                    i18n::t!("runs.editor.edit_json"),
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.open_json(window, cx);
                                    },
                                )),
                            ),
                    )
                    .child(
                        Label::new(self.file.to_string_lossy().into_owned())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(self.render_field(
                        i18n::t!("runs.editor.label"),
                        &self.label,
                        window,
                        cx,
                    ))
                    .child(self.render_field(
                        i18n::t!("runs.editor.command"),
                        &self.command,
                        window,
                        cx,
                    ))
                    .child(self.render_steps(window, cx))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_title(i18n::t!("runs.editor.ready_when")))
                            .child(
                                segmented_control(cx).children(ready_options.into_iter().map(
                                    |(choice, label)| {
                                        segment_option(
                                            SharedString::from(format!("ready-{choice:?}")),
                                            label,
                                            self.ready == choice,
                                            true,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.ready = choice;
                                                this.schedule_save(cx);
                                            }),
                                        )
                                    },
                                )),
                            )
                            .when(
                                matches!(self.ready, ReadyChoice::Port | ReadyChoice::Output),
                                |this| {
                                    this.child(
                                        text_field(
                                            self.ready_value
                                                .focus_handle(cx)
                                                .contains_focused(window, cx),
                                            cx,
                                        )
                                        .child(self.ready_value.clone()),
                                    )
                                },
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_title(i18n::t!("runs.editor.if_running")))
                            .child(segmented_control(cx).children(
                                if_running_options.into_iter().map(|(choice, label)| {
                                    segment_option(
                                        SharedString::from(format!("if-running-{choice:?}")),
                                        label,
                                        self.if_running == choice,
                                        true,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.if_running = choice;
                                            this.schedule_save(cx);
                                        },
                                    ))
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_title(i18n::t!("runs.editor.device")))
                            .child(
                                segmented_control(cx).children(device_options.into_iter().map(
                                    |(choice, label)| {
                                        segment_option(
                                            SharedString::from(format!("device-{choice:?}")),
                                            label,
                                            self.device == choice,
                                            true,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.device = choice;
                                                this.schedule_save(cx);
                                            }),
                                        )
                                    },
                                )),
                            )
                            .when(self.device == DeviceKind::Fixed, |this| {
                                this.child(
                                    text_field(
                                        self.device_id
                                            .focus_handle(cx)
                                            .contains_focused(window, cx),
                                        cx,
                                    )
                                    .child(self.device_id.clone()),
                                )
                            })
                            .child(
                                Label::new(i18n::t!("runs.editor.device_hint"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.render_field(i18n::t!("runs.editor.cwd"), &self.cwd, window, cx))
                    .child(self.render_field(i18n::t!("runs.editor.env"), &self.env, window, cx))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_title(i18n::t!("runs.editor.favorite")))
                            .child(segmented_control(cx).children(
                                std::iter::once(None).chain((1..=9).map(Some)).map(|slot| {
                                    let label: SharedString = match slot {
                                        Some(slot) => slot.to_string().into(),
                                        None => "—".into(),
                                    };
                                    segment_option(
                                        SharedString::from(format!("favorite-{slot:?}")),
                                        label,
                                        self.favorite == slot,
                                        true,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.favorite = slot;
                                            this.schedule_save(cx);
                                        },
                                    ))
                                }),
                            )),
                    ),
            )
    }
}

impl Focusable for TaskEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TaskEditor {}

impl Item for TaskEditor {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        i18n::t!("runs.editor.tab", label = self.original_label.as_str())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayOutlined).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens the task labelled `label` of a tasks.json file in the task editor, or brings back the
/// tab already editing it.
pub fn open(
    workspace: &mut Workspace,
    file: PathBuf,
    template: TaskTemplate,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<TaskEditor>(cx).find(|editor| {
        let editor = editor.read(cx);
        editor.file == file && editor.original_label == template.label
    });
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let fs = workspace.project().read(cx).fs().clone();
    let handle = workspace.weak_handle();
    let editor = cx.new(|cx| TaskEditor::new(handle, fs, file, &template, window, cx));
    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ready_text() {
        assert_eq!(parse_ready_text(""), None);
        assert_eq!(parse_ready_text("exit"), Some(ReadyWhen::exit()));
        assert_eq!(
            parse_ready_text("port 8081"),
            Some(ReadyWhen::Port { port: 8081 })
        );
        assert_eq!(
            parse_ready_text(":3000"),
            Some(ReadyWhen::Port { port: 3000 })
        );
        assert_eq!(
            parse_ready_text("Logs for your project"),
            Some(ReadyWhen::Output {
                output: "Logs for your project".into()
            })
        );
    }

    #[test]
    fn round_trips_a_task_through_the_form() {
        let text = r#"[
            // comments are allowed in tasks.json
            { "label": "other", "command": "make" },
            {
                "label": "Trix",
                "command": "",
                "reveal": "never",
                "steps": [
                    { "label": "Metro", "command": "yarn", "args": ["start"], "ready_when": { "port": 8081 } },
                    { "action": "open_app" }
                ]
            }
        ]"#;
        let tasks: Vec<TaskTemplate> = serde_json_lenient::from_str(text).expect("tasks");
        let mut form = template_to_form(&tasks[1]);
        assert_eq!(form.steps.len(), 2);
        assert_eq!(form.steps[0].ready, "port 8081");
        form.label = "Trix no iPhone".into();
        form.if_running = Some(IfRunning::Reuse);
        form.favorite = Some(2);
        form.env = "EXPO_NO_TELEMETRY=1".into();
        form.steps[0].command_line = "yarn start --dev-client".into();

        let written = replace_task_in_text(text, "Trix", &form).expect("written");
        let tasks: Vec<Value> = serde_json::from_str(&written).expect("json");
        assert_eq!(tasks.len(), 2);
        let task = &tasks[1];
        assert_eq!(task["label"], "Trix no iPhone");
        assert_eq!(
            task["reveal"], "never",
            "keys the editor doesn't show are kept"
        );
        assert_eq!(task["if_running"], "reuse");
        assert_eq!(task["favorite"], 2);
        assert_eq!(task["env"]["EXPO_NO_TELEMETRY"], "1");
        assert_eq!(
            task["steps"][0]["args"],
            serde_json::json!(["start", "--dev-client"])
        );
        assert_eq!(task["steps"][0]["ready_when"]["port"], 8081);
        assert_eq!(task["steps"][1]["action"], "open_app");

        let template: TaskTemplate = serde_json::from_value(task.clone()).expect("template");
        assert_eq!(template.run.steps.len(), 2);
    }

    #[test]
    fn rejects_bad_env_lines() {
        let mut form = template_to_form(&TaskTemplate {
            label: "x".into(),
            command: "y".into(),
            ..TaskTemplate::default()
        });
        form.env = "NO_EQUALS_SIGN".into();
        assert!(replace_task_in_text("[]", "x", &form).is_err());
    }
}
