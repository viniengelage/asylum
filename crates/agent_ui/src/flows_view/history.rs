//! The recent runs of every esteira, kept across restarts: the sidebar lists them with their step
//! threads, and a run the app closed in the middle of can resume from the step it was on.

use crate::ThreadId;
use db::kvp::KeyValueStore;
use flows::run::{FlowRun, RunSnapshot, RunState};
use gpui::{App, AppContext as _, Global};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;
use util::ResultExt as _;

const NAMESPACE: &str = "flow_runs";
const RECENT_KEY: &str = "recent";
const MAX_RUNS: usize = 30;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowRunRecord {
    pub id: String,
    pub flow_id: String,
    pub flow_name: String,
    pub flow_path: PathBuf,
    /// The flow as it ran, so editing the file later doesn't change what the record shows.
    pub flow_markdown: String,
    pub started_at: SystemTime,
    pub updated_at: SystemTime,
    pub snapshot: RunSnapshot,
    pub step_labels: Vec<String>,
    pub step_threads: Vec<Vec<ThreadId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    Running,
    WaitingForYou,
    Succeeded,
    Cancelled,
    /// It was running when the app closed.
    Interrupted,
}

impl FlowRunRecord {
    pub fn status(&self, is_live: bool) -> RecordStatus {
        match &self.snapshot.state {
            RunState::Running if is_live => RecordStatus::Running,
            RunState::Running => RecordStatus::Interrupted,
            RunState::WaitingForYou(_) => RecordStatus::WaitingForYou,
            RunState::Succeeded => RecordStatus::Succeeded,
            RunState::Cancelled => RecordStatus::Cancelled,
        }
    }

    pub fn passed_steps(&self) -> usize {
        self.snapshot
            .steps
            .iter()
            .filter(|step| step.state == flows::run::StepState::Passed)
            .count()
    }

    pub fn restore(&self) -> Option<(flows::Flow, FlowRun)> {
        let flow = flows::parse_flow(
            &self.flow_path,
            &self.flow_markdown,
            flows::FlowSource::Personal,
        )
        .log_err()?;
        let run = FlowRun::restore(flow.clone(), self.snapshot.clone())?;
        Some((flow, run))
    }
}

#[derive(Default)]
pub struct FlowRunHistory {
    pub records: Vec<FlowRunRecord>,
    /// Runs with a tab in this app session, by id.
    pub live: Vec<String>,
}

impl Global for FlowRunHistory {}

pub fn init(cx: &mut App) {
    cx.set_global(FlowRunHistory::default());
    let kvp = KeyValueStore::global(cx);
    cx.spawn(async move |cx| {
        let stored = cx
            .background_spawn(async move { kvp.scoped(NAMESPACE).read(RECENT_KEY) })
            .await
            .log_err()
            .flatten();
        let records: Vec<FlowRunRecord> = stored
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default();
        cx.update(|cx| {
            let history = cx.default_global::<FlowRunHistory>();
            for record in records {
                if !history
                    .records
                    .iter()
                    .any(|existing| existing.id == record.id)
                {
                    history.records.push(record);
                }
            }
            history
                .records
                .sort_by_key(|record| std::cmp::Reverse(record.updated_at));
            super::notify_runs_changed(cx);
        });
    })
    .detach();
}

pub fn records(cx: &App) -> Vec<(FlowRunRecord, bool)> {
    cx.try_global::<FlowRunHistory>()
        .map(|history| {
            history
                .records
                .iter()
                .map(|record| (record.clone(), history.live.contains(&record.id)))
                .collect()
        })
        .unwrap_or_default()
}

/// Saves the run's latest state, newest first.
pub fn record(record: FlowRunRecord, cx: &mut App) {
    let history = cx.default_global::<FlowRunHistory>();
    if !history.live.contains(&record.id) {
        history.live.push(record.id.clone());
    }
    history.records.retain(|existing| existing.id != record.id);
    history.records.insert(0, record);
    history.records.truncate(MAX_RUNS);
    persist(cx);
}

/// The run's tab closed: it is no longer running in this session.
pub fn release(id: &str, cx: &mut App) {
    if let Some(history) = cx.try_global::<FlowRunHistory>()
        && history.live.iter().any(|live| live == id)
    {
        cx.default_global::<FlowRunHistory>()
            .live
            .retain(|live| live != id);
        super::notify_runs_changed(cx);
    }
}

pub fn forget(id: &str, cx: &mut App) {
    let history = cx.default_global::<FlowRunHistory>();
    history.records.retain(|record| record.id != id);
    history.live.retain(|live| live != id);
    persist(cx);
    super::notify_runs_changed(cx);
}

fn persist(cx: &mut App) {
    let json = match serde_json::to_string(&cx.default_global::<FlowRunHistory>().records) {
        Ok(json) => json,
        Err(error) => {
            log::error!("failed to serialize flow runs: {error:#}");
            return;
        }
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(NAMESPACE)
            .write(RECENT_KEY.to_string(), json)
            .await
            .log_err();
    })
    .detach();
}
