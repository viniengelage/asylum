//! CloudWatch Logs in the dock: Logs Insights tabs, live tail and the agent's read-only tools, over
//! the credentials in `~/.aws` resolved by the official SDK.

mod agent_bridge;
mod agent_toolkit;
mod aws_login;
mod client;
mod live_tail_view;
mod llm;
mod panel;
mod profiles;
mod query_view;
mod store;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use client::{
    CloudWatch, CloudWatchError, LiveTail, LogEvent, LogGroup, QueryResult, TailUpdate, Target,
    connect,
};
pub use live_tail_view::{LiveTailView, open_live_tail};
pub use panel::{CloudWatchPanel, CloudWatchToolkitButton};
pub use profiles::{AwsProfile, ProfileKind, load_profiles};
pub use query_view::{QueryView, open_query};
pub use store::{
    AGENT_FREE_WINDOW, CloudWatchStore, Connection, QUERIES_DIR, QUERY_EXTENSION, SavedQuery,
    StoreEvent, Usage,
};

use gpui::{App, actions};

actions!(
    cloudwatch,
    [
        /// Opens the CloudWatch dock, or hands focus back if it already has it.
        ToggleFocus,
        /// Creates an .insights file in .asylum/cloudwatch/queries and opens it.
        NewQuery,
        /// Runs the Logs Insights query in the focused tab.
        RunQuery,
        /// Stops the running Logs Insights query.
        CancelQuery,
        /// Opens a live tail of the project's log groups.
        OpenLiveTail,
        /// Pauses or resumes scrolling in the live tail.
        TogglePause,
        /// Asks the agent to write or change the query in the focused tab.
        AskAgent,
        /// Accepts the agent's proposal or the suggested fix.
        AcceptSuggestion,
        /// Accepts the agent's proposal or the suggested fix and runs the query.
        AcceptSuggestionAndRun,
        /// Discards the agent's proposal.
        DiscardSuggestion,
        /// Summarizes the query results with the agent.
        SummarizeResults,
        /// Runs `aws login` for the current profile in a terminal.
        RunAwsLogin,
    ]
);

pub fn init(cx: &mut App) {
    panel::init(cx);
    query_view::init(cx);
}

#[cfg(test)]
mod live_tests {
    //! Runs against a real AWS account with the credentials in `~/.aws`:
    //!
    //! ```sh
    //! cargo test -p cloudwatch -- --ignored --nocapture
    //! ```
    //!
    //! `CLOUDWATCH_PROFILE` / `CLOUDWATCH_REGION` pick the target,
    //! `CLOUDWATCH_LOG_GROUP` the group (otherwise the first one listed), and
    //! `CLOUDWATCH_TAIL_SECONDS` how long to keep the tail open (default 10),
    //! and `CLOUDWATCH_TAIL_FILTER` a filter pattern for it (one that matches
    //! nothing keeps the session quiet, to see whether idle tails survive).

    use std::{
        sync::Arc,
        time::{Duration, Instant, SystemTime},
    };

    use http_client::{AsyncBody, HttpClient};
    use reqwest_client::ReqwestClient;

    use super::*;

    #[test]
    #[ignore = "needs AWS credentials and network"]
    fn talks_to_cloudwatch_logs() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let http_client: Arc<dyn HttpClient> = {
            let _guard = runtime.enter();
            Arc::new(
                ReqwestClient::proxy_and_user_agent(None, "Asylum/cloudwatch-live-test")
                    .expect("http client"),
            )
        };
        runtime.block_on(run(http_client));
    }

    async fn run(http_client: Arc<dyn HttpClient>) {
        let target = Target {
            profile: std::env::var("CLOUDWATCH_PROFILE").ok(),
            region: std::env::var("CLOUDWATCH_REGION").ok(),
        };
        let cloudwatch = connect(&target, http_client.clone())
            .await
            .expect("connect");
        println!(
            "perfil {:?}, região {}",
            cloudwatch.profile(),
            cloudwatch.region()
        );

        let probe = http_client
            .get(
                &format!(
                    "https://streaming-logs.{}.amazonaws.com/",
                    cloudwatch.region()
                ),
                AsyncBody::empty(),
                false,
            )
            .await
            .expect("probe streaming-logs endpoint");
        println!(
            "streaming-logs responde {} via {:?}",
            probe.status(),
            probe.version()
        );

        let groups = match cloudwatch.list_log_groups(None, 50).await {
            Ok(groups) => groups,
            Err(error) => panic!("DescribeLogGroups falhou: {error} ({error:?})"),
        };
        println!("{} log groups (até 50)", groups.len());
        for group in groups.iter().take(10) {
            println!(
                "  {} · {:?} bytes · {:?} dias",
                group.name, group.stored_bytes, group.retention_days
            );
        }
        let group = match std::env::var("CLOUDWATCH_LOG_GROUP") {
            Ok(name) => groups
                .iter()
                .find(|group| group.name == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} não está entre os grupos listados")),
            Err(_) => groups.first().cloned().expect("a conta não tem log groups"),
        };
        println!("usando {}", group.name);

        let now = SystemTime::now();
        let five_minutes_ago = now - Duration::from_secs(5 * 60);
        let started = Instant::now();
        let result = cloudwatch
            .run_insights_query(
                std::slice::from_ref(&group.name),
                "fields @timestamp, @message | sort @timestamp desc | limit 5",
                five_minutes_ago,
                now,
                5,
                Duration::from_secs(1),
            )
            .await
            .expect("insights query");
        println!(
            "insights: {} linhas em {:?} · {} registros lidos · {} bytes",
            result.rows.len(),
            started.elapsed(),
            result.records_scanned,
            result.bytes_scanned
        );

        let events = cloudwatch
            .filter_log_events(&group.name, None, five_minutes_ago, 5)
            .await
            .expect("filter log events");
        println!("filter_log_events: {} eventos", events.len());

        let arn = group.arn.clone().expect("log group sem ARN");
        let tail_seconds = std::env::var("CLOUDWATCH_TAIL_SECONDS")
            .ok()
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(10);
        let tail_filter = std::env::var("CLOUDWATCH_TAIL_FILTER").ok();
        let mut tail = cloudwatch
            .start_live_tail(&[arn], tail_filter.as_deref())
            .await
            .expect("start live tail");
        let tail_started = Instant::now();
        let deadline = tail_started + Duration::from_secs(tail_seconds);
        let (mut updates, mut empty_updates, mut tail_events) = (0, 0, 0);
        let mut longest_gap = Duration::ZERO;
        let mut last_message = Instant::now();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, tail.next_update()).await {
                Err(_) => break,
                Ok(Ok(None)) => {
                    println!("o servidor fechou o tail após {:?}", tail_started.elapsed());
                    break;
                }
                Ok(Ok(Some(update))) => {
                    longest_gap = longest_gap.max(last_message.elapsed());
                    last_message = Instant::now();
                    match update {
                        TailUpdate::Started { session_id } => {
                            println!("tail iniciado: {session_id:?}")
                        }
                        TailUpdate::Events { events, sampled } => {
                            updates += 1;
                            if events.is_empty() {
                                empty_updates += 1;
                            }
                            tail_events += events.len();
                            if sampled {
                                println!("atualização amostrada pelo servidor");
                            }
                        }
                    }
                }
                Ok(Err(error)) => panic!("tail falhou após {:?}: {error}", tail_started.elapsed()),
            }
        }
        println!(
            "tail: {updates} atualizações ({empty_updates} vazias), {tail_events} eventos em \
             {:?}; maior intervalo sem mensagem {longest_gap:?}",
            tail_started.elapsed()
        );
    }
}
