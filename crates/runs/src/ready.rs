use std::{
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use gpui::{AsyncApp, Entity};
use regex::Regex;
use task::ReadyWhen;
use terminal::{TaskStatus, Terminal};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// How a task that was waited on became ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// It ended with exit code 0.
    Exited,
    /// Its port answered, or its output matched, while it keeps running.
    Ready,
}

/// What the terminal of a task looks like at one poll, for [`check_readiness`].
pub struct TerminalSnapshot<'a> {
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub output: &'a str,
    pub port_open: bool,
}

/// One poll of a task against its `ready_when`: `Some(Ok)` once ready, `Some(Err)` once it
/// can't become ready any more (it ended without getting there), `None` to keep waiting.
pub fn check_readiness(
    ready_when: &ReadyWhen,
    output_regex: Option<&Regex>,
    snapshot: &TerminalSnapshot,
) -> Option<Result<Readiness>> {
    let ended = snapshot.status != TaskStatus::Running;
    let succeeded = matches!(snapshot.status, TaskStatus::Completed { success: true });
    let ready = match ready_when {
        ReadyWhen::Exit(_) => false,
        ReadyWhen::Port { .. } => snapshot.port_open,
        ReadyWhen::Output { .. } => {
            output_regex.is_some_and(|regex| regex.is_match(snapshot.output))
        }
    };
    if ready {
        return Some(Ok(Readiness::Ready));
    }
    if !ended {
        return None;
    }
    if succeeded && ready_when.waits_for_exit() {
        return Some(Ok(Readiness::Exited));
    }
    let reason = match snapshot.exit_code {
        Some(code) => i18n::t!("runs.ready.ended_with_code", code = code).to_string(),
        None => i18n::t!("runs.ready.ended").to_string(),
    };
    Some(Err(anyhow!(reason)))
}

pub fn output_regex(ready_when: &ReadyWhen) -> Result<Option<Regex>> {
    match ready_when {
        ReadyWhen::Output { output } => Regex::new(output)
            .map(Some)
            .with_context(|| i18n::t!("runs.ready.bad_regex", regex = output.as_str()).to_string()),
        _ => Ok(None),
    }
}

pub fn port_is_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        PORT_CONNECT_TIMEOUT,
    )
    .is_ok()
}

/// Waits for a task terminal to be ready by its `ready_when`, polling its state rather than
/// its completion channel, which has a single consumer that the terminal's owner waits on.
pub async fn wait_until_ready(
    ready_when: &ReadyWhen,
    terminal: &Entity<Terminal>,
    cx: &mut AsyncApp,
) -> Result<Readiness> {
    let regex = output_regex(ready_when)?;
    let port = match ready_when {
        ReadyWhen::Port { port } => Some(*port),
        _ => None,
    };
    loop {
        let port_open = match port {
            Some(port) => {
                cx.background_executor()
                    .spawn(async move { port_is_open(port) })
                    .await
            }
            None => false,
        };
        let outcome = terminal.read_with(cx, |terminal, _| {
            let Some(task) = terminal.task() else {
                return Some(Err(anyhow!(i18n::t!("runs.ready.ended").to_string())));
            };
            let output = if regex.is_some() {
                terminal.get_content()
            } else {
                String::new()
            };
            check_readiness(
                ready_when,
                regex.as_ref(),
                &TerminalSnapshot {
                    status: task.status,
                    exit_code: terminal.exit_status().and_then(|status| status.code()),
                    output: &output,
                    port_open,
                },
            )
        });
        if let Some(outcome) = outcome {
            return outcome;
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: TaskStatus, output: &str, port_open: bool) -> TerminalSnapshot<'_> {
        TerminalSnapshot {
            status,
            exit_code: match status {
                TaskStatus::Completed { success: true } => Some(0),
                TaskStatus::Completed { success: false } => Some(1),
                _ => None,
            },
            output,
            port_open,
        }
    }

    #[test]
    fn exit_waits_for_a_clean_end() {
        let ready_when = ReadyWhen::exit();
        assert!(
            check_readiness(&ready_when, None, &snapshot(TaskStatus::Running, "", true)).is_none()
        );
        assert_eq!(
            check_readiness(
                &ready_when,
                None,
                &snapshot(TaskStatus::Completed { success: true }, "", false)
            )
            .and_then(Result::ok),
            Some(Readiness::Exited)
        );
        assert!(
            check_readiness(
                &ready_when,
                None,
                &snapshot(TaskStatus::Completed { success: false }, "", false)
            )
            .is_some_and(|outcome| outcome.is_err())
        );
    }

    #[test]
    fn port_is_ready_while_running() {
        let ready_when = ReadyWhen::Port { port: 8081 };
        assert!(
            check_readiness(&ready_when, None, &snapshot(TaskStatus::Running, "", false)).is_none()
        );
        assert_eq!(
            check_readiness(&ready_when, None, &snapshot(TaskStatus::Running, "", true))
                .and_then(Result::ok),
            Some(Readiness::Ready)
        );
        assert!(
            check_readiness(
                &ready_when,
                None,
                &snapshot(TaskStatus::Completed { success: true }, "", false)
            )
            .is_some_and(|outcome| outcome.is_err()),
            "a server that exits before listening never got ready"
        );
    }

    #[test]
    fn output_matches_regex() {
        let ready_when = ReadyWhen::Output {
            output: r"Waiting on http://localhost:\d+".to_string(),
        };
        let regex = output_regex(&ready_when).expect("valid").expect("regex");
        assert!(
            check_readiness(
                &ready_when,
                Some(&regex),
                &snapshot(TaskStatus::Running, "Starting Metro Bundler", false)
            )
            .is_none()
        );
        assert_eq!(
            check_readiness(
                &ready_when,
                Some(&regex),
                &snapshot(
                    TaskStatus::Running,
                    "Waiting on http://localhost:8081",
                    false
                )
            )
            .and_then(Result::ok),
            Some(Readiness::Ready)
        );
        assert!(
            output_regex(&ReadyWhen::Output {
                output: "(".to_string()
            })
            .is_err()
        );
    }
}
