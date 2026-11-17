use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use collections::HashSet;
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Task, TaskExt as _,
    WeakEntity, Window,
};
use regex::Regex;
use task::{SharedTaskContext, SpawnInTerminal, TaskContext, TaskId};
use terminal::{TaskStatus, Terminal};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use util::ResultExt as _;
use workspace::{Pane, Toast, Workspace, notifications::NotificationId};

use crate::sources::{RunCandidate, load_candidates, project_key};

const SCAN_INTERVAL: Duration = Duration::from_millis(800);
const RECENT_LIMIT: usize = 30;
const EXCERPT_LINES: usize = 6;
pub const FAVORITE_SLOTS: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Succeeded,
    Failed {
        exit_code: Option<i32>,
    },
    StoppedByUser,
    /// `if_running: reuse` found it already running, so nothing new was started.
    Reused,
}

impl RunState {
    pub fn is_running(self) -> bool {
        self == RunState::Running
    }
}

/// A place in the output that looks like `path:line[:column]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLocation {
    pub path: String,
    pub line: u32,
    pub column: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ExcerptLine {
    pub text: SharedString,
    pub location: Option<OutputLocation>,
}

#[derive(Clone)]
pub struct RunRecord {
    pub id: usize,
    pub label: SharedString,
    pub command_label: SharedString,
    pub task_id: TaskId,
    pub spawn: Option<SpawnInTerminal>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub state: RunState,
    pub terminal: WeakEntity<Terminal>,
    terminal_entity_id: Option<EntityId>,
    stop_requested: bool,
    pub excerpt: Vec<ExcerptLine>,
    pub output_tail: Vec<String>,
}

impl RunRecord {
    pub fn duration(&self) -> Duration {
        self.finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started_at)
    }

    pub fn cwd(&self) -> Option<PathBuf> {
        self.spawn.as_ref().and_then(|spawn| spawn.cwd.clone())
    }
}

pub enum RunStoreEvent {
    Finished { record_id: usize },
}

impl gpui::EventEmitter<RunStoreEvent> for RunStore {}

/// The runs of one workspace: the task terminals it has, running and finished, the project's
/// favorites and the run F5 starts.
pub struct RunStore {
    workspace: WeakEntity<Workspace>,
    records: Vec<RunRecord>,
    next_id: usize,
    favorites: Vec<SharedString>,
    active: Option<SharedString>,
    candidates: Vec<RunCandidate>,
    project_key: Option<String>,
    _scan: Task<()>,
}

#[derive(Default)]
struct RunStores(Vec<(WeakEntity<Workspace>, Entity<RunStore>)>);

impl Global for RunStores {}

impl RunStore {
    pub fn for_workspace(workspace: &Entity<Workspace>, cx: &mut App) -> Entity<Self> {
        let handle = workspace.downgrade();
        let stores = cx.default_global::<RunStores>();
        stores
            .0
            .retain(|(workspace, _)| workspace.upgrade().is_some());
        if let Some((_, store)) = stores.0.iter().find(|(existing, _)| *existing == handle) {
            return store.clone();
        }
        let store = cx.new(|cx| Self::new(handle.clone(), cx));
        cx.default_global::<RunStores>()
            .0
            .push((handle, store.clone()));
        store
    }

    pub fn try_for_workspace(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<Self>> {
        let handle = workspace.downgrade();
        cx.try_global::<RunStores>()?
            .0
            .iter()
            .find(|(existing, _)| *existing == handle)
            .map(|(_, store)| store.clone())
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let scan = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SCAN_INTERVAL).await;
                let Ok(()) = this.update(cx, |this, cx| this.scan(cx)) else {
                    break;
                };
            }
        });
        let mut this = Self {
            workspace,
            records: Vec::new(),
            next_id: 0,
            favorites: Vec::new(),
            active: None,
            candidates: Vec::new(),
            project_key: None,
            _scan: scan,
        };
        this.load_project_state(cx);
        this
    }

    fn load_project_state(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let key = project_key(workspace.read(cx), cx);
        if key == self.project_key {
            return;
        }
        self.project_key = key.clone();
        let Some(key) = key else {
            return;
        };
        let store = KeyValueStore::global(cx);
        self.favorites = store
            .read_kvp(&favorites_key(&key))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err())
            .unwrap_or_default()
            .into_iter()
            .map(SharedString::from)
            .collect();
        self.active = store
            .read_kvp(&active_key(&key))
            .log_err()
            .flatten()
            .map(SharedString::from);
    }

    pub fn records(&self) -> &[RunRecord] {
        &self.records
    }

    pub fn record(&self, record_id: usize) -> Option<&RunRecord> {
        self.records.iter().find(|record| record.id == record_id)
    }

    pub fn running(&self) -> impl Iterator<Item = &RunRecord> {
        self.records
            .iter()
            .rev()
            .filter(|record| record.state.is_running())
    }

    pub fn recent(&self) -> impl Iterator<Item = &RunRecord> {
        self.records
            .iter()
            .rev()
            .filter(|record| !record.state.is_running())
    }

    pub fn last_result(&self, label: &str) -> Option<&RunRecord> {
        self.records
            .iter()
            .rev()
            .find(|record| record.label.as_ref() == label)
    }

    pub fn favorites(&self) -> &[SharedString] {
        &self.favorites
    }

    pub fn favorite_slot(&self, key: &str) -> Option<usize> {
        self.favorites
            .iter()
            .position(|favorite| favorite.as_ref() == key)
            .map(|index| index + 1)
    }

    pub fn candidates(&self) -> &[RunCandidate] {
        &self.candidates
    }

    pub fn candidate(&self, key: &str) -> Option<&RunCandidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.key.as_ref() == key)
    }

    pub fn set_candidates(&mut self, candidates: Vec<RunCandidate>, cx: &mut Context<Self>) {
        self.candidates = candidates;
        cx.notify();
    }

    pub fn active(&self) -> Option<&RunCandidate> {
        let active = self.active.as_ref()?;
        self.candidate(active)
    }

    pub fn active_key(&self) -> Option<&SharedString> {
        self.active.as_ref()
    }

    pub fn set_active(&mut self, key: SharedString, cx: &mut Context<Self>) {
        if self.active.as_ref() == Some(&key) {
            return;
        }
        self.active = Some(key.clone());
        if let Some(project_key) = self.project_key.clone() {
            let store = KeyValueStore::global(cx);
            cx.background_spawn(async move {
                store
                    .write_kvp(active_key(&project_key), key.to_string())
                    .await
            })
            .detach_and_log_err(cx);
        }
        cx.notify();
    }

    pub fn toggle_favorite(&mut self, key: SharedString, cx: &mut Context<Self>) {
        if let Some(index) = self.favorites.iter().position(|favorite| *favorite == key) {
            self.favorites.remove(index);
        } else if self.favorites.len() < FAVORITE_SLOTS {
            self.favorites.push(key);
        } else {
            self.show_toast(i18n::t!("runs.favorites.full").to_string(), cx);
            return;
        }
        self.save_favorites(cx);
        cx.notify();
    }

    fn save_favorites(&self, cx: &mut Context<Self>) {
        let Some(project_key) = self.project_key.clone() else {
            return;
        };
        let favorites: Vec<String> = self
            .favorites
            .iter()
            .map(|favorite| favorite.to_string())
            .collect();
        match serde_json::to_string(&favorites) {
            Ok(json) => {
                let store = KeyValueStore::global(cx);
                cx.background_spawn(async move {
                    store.write_kvp(favorites_key(&project_key), json).await
                })
                .detach_and_log_err(cx);
            }
            Err(error) => log::error!("runs: favorites not saved: {error}"),
        }
    }

    fn show_toast(&self, message: String, cx: &mut App) {
        struct RunToast;
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<RunToast>(), message).autohide(),
                    cx,
                );
            })
            .log_err();
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        self.load_project_state(cx);
        let terminals = task_terminals(&workspace, cx);
        let mut changed = false;
        let mut seen = HashSet::default();
        for (_, terminal) in &terminals {
            let entity_id = terminal.entity_id();
            seen.insert(entity_id);
            let terminal_ref = terminal.read(cx);
            let Some(task) = terminal_ref.task() else {
                continue;
            };
            let status = task.status;
            let existing = self
                .records
                .iter()
                .rposition(|record| record.terminal_entity_id == Some(entity_id));
            match existing {
                Some(index) => {
                    let Some(record) = self.records.get_mut(index) else {
                        continue;
                    };
                    if record.state.is_running() && status != TaskStatus::Running {
                        finish_record(record, status, terminal_ref);
                        changed = true;
                        cx.emit(RunStoreEvent::Finished {
                            record_id: record.id,
                        });
                    }
                }
                None if status == TaskStatus::Running => {
                    let spawn = task.spawned_task.clone();
                    let record = RunRecord {
                        id: self.next_id,
                        label: spawn.full_label.clone().into(),
                        command_label: spawn.command_label.clone().into(),
                        task_id: spawn.id.clone(),
                        spawn: Some(spawn),
                        started_at: Instant::now(),
                        finished_at: None,
                        state: RunState::Running,
                        terminal: terminal.downgrade(),
                        terminal_entity_id: Some(entity_id),
                        stop_requested: false,
                        excerpt: Vec::new(),
                        output_tail: Vec::new(),
                    };
                    self.next_id += 1;
                    self.records.push(record);
                    changed = true;
                }
                None => {}
            }
        }
        for record in &mut self.records {
            if record.state.is_running()
                && record
                    .terminal_entity_id
                    .is_some_and(|entity_id| !seen.contains(&entity_id))
            {
                record.state = if record.stop_requested {
                    RunState::StoppedByUser
                } else {
                    RunState::Failed { exit_code: None }
                };
                record.finished_at = Some(Instant::now());
                changed = true;
            }
        }
        let finished = self
            .records
            .iter()
            .filter(|r| !r.state.is_running())
            .count();
        if finished > RECENT_LIMIT {
            let mut to_drop = finished - RECENT_LIMIT;
            self.records.retain(|record| {
                if to_drop > 0 && !record.state.is_running() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        if changed {
            cx.notify();
        }
    }

    /// Records a run that `if_running: reuse` didn't start because it was already running.
    pub fn record_reused(&mut self, label: SharedString, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.records.push(RunRecord {
            id: self.next_id,
            label: label.clone(),
            command_label: label,
            task_id: TaskId(String::new()),
            spawn: None,
            started_at: now,
            finished_at: Some(now),
            state: RunState::Reused,
            terminal: WeakEntity::new_invalid(),
            terminal_entity_id: None,
            stop_requested: false,
            excerpt: Vec::new(),
            output_tail: Vec::new(),
        });
        self.next_id += 1;
        cx.notify();
    }

    pub fn running_terminal_for(&self, label: &str, cx: &App) -> Option<Entity<Terminal>> {
        self.records
            .iter()
            .rev()
            .filter(|record| record.state.is_running() && record.label.as_ref() == label)
            .find_map(|record| record.terminal.upgrade())
            .filter(|terminal| {
                terminal
                    .read(cx)
                    .task()
                    .is_some_and(|task| task.status == TaskStatus::Running)
            })
    }

    pub fn stop(&mut self, record_id: usize, cx: &mut Context<Self>) {
        let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == record_id)
        else {
            return;
        };
        record.stop_requested = true;
        if let Some(terminal) = record.terminal.upgrade() {
            terminal.update(cx, |terminal, _| terminal.kill_active_task());
        }
        cx.notify();
    }

    pub fn stop_all(&mut self, cx: &mut Context<Self>) -> usize {
        let running: Vec<usize> = self.running().map(|record| record.id).collect();
        for record_id in &running {
            self.stop(*record_id, cx);
        }
        running.len()
    }

    /// Starts the record's command again, after stopping it when it still runs.
    pub fn restart(&mut self, record_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.records.iter().find(|record| record.id == record_id) else {
            return;
        };
        let Some(spawn) = record.spawn.clone() else {
            if let Some(key) = self
                .candidates
                .iter()
                .find(|candidate| candidate.label == record.label)
                .map(|candidate| candidate.key.clone())
            {
                self.run_key(key, false, window, cx);
            }
            return;
        };
        if record.state.is_running() {
            self.stop(record_id, cx);
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let run = workspace.update(cx, |workspace, cx| {
            workspace.spawn_in_terminal(spawn, window, cx)
        });
        let workspace = workspace.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(Err(error)) = run.await {
                workspace.update(cx, |workspace, cx| {
                    struct RestartFailed;
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<RestartFailed>(),
                            i18n::t!("runs.restart_failed", error = error.to_string()).to_string(),
                        ),
                        cx,
                    );
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Brings the record's terminal to the front.
    pub fn reveal(&mut self, record_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.record(record_id) else {
            return;
        };
        let Some(terminal) = record.terminal.upgrade() else {
            self.show_toast(i18n::t!("runs.output_closed").to_string(), cx);
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            reveal_terminal(workspace, &terminal, window, cx);
        });
    }

    /// Runs a candidate by key, refreshing the candidates first when it isn't known yet.
    pub fn run_key(
        &mut self,
        key: SharedString,
        debug: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(candidate) = self.candidate(&key).cloned() {
            self.run_candidate(candidate, debug, window, cx);
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let refresh = refresh_candidates(&workspace, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            refresh.await;
            this.update_in(cx, |this, window, cx| match this.candidate(&key).cloned() {
                Some(candidate) => this.run_candidate(candidate, debug, window, cx),
                None => this.show_toast(
                    i18n::t!("runs.not_found", label = key.as_ref()).to_string(),
                    cx,
                ),
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn run_candidate(
        &mut self,
        candidate: RunCandidate,
        debug: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active(candidate.key.clone(), cx);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        crate::launch::launch(cx.entity(), workspace, candidate, debug, window, cx);
    }
}

/// Reloads the picker's candidates into the workspace's store.
pub fn refresh_candidates(
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let store = RunStore::for_workspace(workspace, cx);
    let task_contexts = workspace.update(cx, |workspace, cx| {
        tasks_ui::task_contexts(workspace, window, cx)
    });
    let workspace = workspace.downgrade();
    window.spawn(cx, async move |cx| {
        let task_contexts = Arc::new(task_contexts.await);
        let candidates = load_candidates(workspace, task_contexts, cx).await;
        store.update(cx, |store, cx| store.set_candidates(candidates, cx));
    })
}

pub fn default_task_context(contexts: &project::TaskContexts) -> SharedTaskContext {
    SharedTaskContext::from(
        contexts
            .active_context()
            .cloned()
            .unwrap_or_else(TaskContext::default),
    )
}

fn finish_record(record: &mut RunRecord, status: TaskStatus, terminal: &Terminal) {
    let exit_code = terminal.exit_status().and_then(|status| status.code());
    record.state = match status {
        TaskStatus::Completed { success: true } => RunState::Succeeded,
        _ if record.stop_requested => RunState::StoppedByUser,
        _ => RunState::Failed { exit_code },
    };
    record.finished_at = Some(Instant::now());
    let tail = terminal.last_n_non_empty_lines(80);
    if matches!(record.state, RunState::Failed { .. }) {
        record.excerpt = error_excerpt(&tail);
    }
    record.output_tail = tail;
}

static LOCATION_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"((?:[A-Za-z]:)?[\w@~./\\-]*[\w-]\.[A-Za-z]{1,5}):(\d+)(?::(\d+))?").log_err()
});

pub fn parse_location(line: &str) -> Option<OutputLocation> {
    let regex = LOCATION_REGEX.as_ref()?;
    let captures = regex.captures(line)?;
    Some(OutputLocation {
        path: captures.get(1)?.as_str().to_string(),
        line: captures.get(2)?.as_str().parse().ok()?,
        column: captures
            .get(3)
            .and_then(|column| column.as_str().parse().ok()),
    })
}

/// The lines worth showing for a failure: the ones that point at code or say "error", or the
/// last lines when nothing does. The task summary Zed appends at the end is left out.
pub fn error_excerpt(lines: &[String]) -> Vec<ExcerptLine> {
    let lines: Vec<&String> = lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !trimmed.starts_with("⏵")
                && !trimmed.starts_with("Task `")
                && !trimmed.starts_with("Command `")
        })
        .collect();
    let interesting: Vec<&String> = lines
        .iter()
        .copied()
        .filter(|line| {
            let lower = line.to_lowercase();
            parse_location(line).is_some() || lower.contains("error") || lower.contains("failed")
        })
        .take(EXCERPT_LINES)
        .collect();
    let chosen: Vec<&String> = if interesting.is_empty() {
        let skip = lines.len().saturating_sub(EXCERPT_LINES);
        lines.into_iter().skip(skip).collect()
    } else {
        interesting
    };
    chosen
        .into_iter()
        .map(|line| ExcerptLine {
            text: line.trim_end().to_string().into(),
            location: parse_location(line),
        })
        .collect()
}

/// Every terminal tab in the workspace that runs or ran a task, in the center and in the
/// terminal dock.
pub fn task_terminals(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Vec<(Entity<TerminalView>, Entity<Terminal>)> {
    let workspace = workspace.read(cx);
    let mut panes: Vec<Entity<Pane>> = workspace.panes().to_vec();
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        for pane in panel.read(cx).panes() {
            if !panes.iter().any(|existing| existing == pane) {
                panes.push(pane.clone());
            }
        }
    }
    let mut seen = HashSet::default();
    panes
        .iter()
        .flat_map(|pane| {
            pane.read(cx)
                .items()
                .filter_map(|item| item.act_as::<TerminalView>(cx))
                .collect::<Vec<_>>()
        })
        .filter(|view| seen.insert(view.entity_id()))
        .filter_map(|view| {
            let terminal = view.read(cx).terminal().clone();
            terminal.read(cx).task()?;
            Some((view, terminal))
        })
        .collect()
}

pub fn reveal_terminal(
    workspace: &mut Workspace,
    terminal: &Entity<Terminal>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mut panes: Vec<(Entity<Pane>, bool)> = workspace
        .panes()
        .iter()
        .map(|pane| (pane.clone(), false))
        .collect();
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        panes.extend(
            panel
                .read(cx)
                .panes()
                .into_iter()
                .map(|pane| (pane.clone(), true)),
        );
    }
    for (pane, in_terminal_panel) in panes {
        let index = pane.read(cx).items().position(|item| {
            item.act_as::<TerminalView>(cx)
                .is_some_and(|view| view.read(cx).terminal() == terminal)
        });
        if let Some(index) = index {
            if in_terminal_panel {
                workspace.focus_panel::<TerminalPanel>(window, cx);
            }
            pane.update(cx, |pane, cx| {
                pane.activate_item(index, true, true, window, cx)
            });
            return;
        }
    }
}

fn favorites_key(project_key: &str) -> String {
    format!("runs-favorites-{project_key}")
}

fn active_key(project_key: &str) -> String {
    format!("runs-active-{project_key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_locations_in_output() {
        assert_eq!(
            parse_location("src/screens/PinScreen.tsx:42:7 - error TS2322"),
            Some(OutputLocation {
                path: "src/screens/PinScreen.tsx".into(),
                line: 42,
                column: Some(7),
            })
        );
        assert_eq!(
            parse_location("    at Object.<anonymous> (/app/index.js:10)"),
            Some(OutputLocation {
                path: "/app/index.js".into(),
                line: 10,
                column: None,
            })
        );
        assert_eq!(parse_location("listening on port 8081"), None);
    }

    #[test]
    fn excerpt_prefers_error_lines() {
        let lines: Vec<String> = [
            "Starting Metro",
            "Bundling 1200 modules",
            "error: Unable to resolve module ./Missing from src/App.tsx:3:1",
            "⏵ Task `start` finished with non-zero error code: 1",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let excerpt = error_excerpt(&lines);
        assert_eq!(excerpt.len(), 1);
        assert_eq!(
            excerpt[0].location.as_ref().map(|location| location.line),
            Some(3)
        );

        let quiet: Vec<String> = ["one", "two"].into_iter().map(String::from).collect();
        assert_eq!(error_excerpt(&quiet).len(), 2);
    }
}
