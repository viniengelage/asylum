use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use aws_config::{BehaviorVersion, Region, stalled_stream_protection::StalledStreamProtectionConfig};
use aws_http_client::AwsHttpClient;
use aws_sdk_cloudwatchlogs::{
    Client,
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::event_stream::EventReceiver,
    types::{
        QueryStatus, StartLiveTailResponseStream, error::StartLiveTailResponseStreamError,
    },
};
use http_client::HttpClient;

/// Which `~/.aws` profile and region to talk to. `None` defers to the same
/// resolution the AWS CLI uses (`AWS_PROFILE`, then `default`; the profile's
/// region, then `AWS_REGION`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub profile: Option<String>,
    pub region: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CloudWatchError {
    #[error("a sessão do aws login expirou; rode `{}`", login_command(.profile))]
    SessionExpired {
        profile: Option<String>,
        message: String,
    },
    #[error("nenhuma credencial da AWS encontrada: {message}")]
    NoCredentials { message: String },
    #[error("nenhuma região configurada para o perfil; defina `region` no ~/.aws/config")]
    NoRegion,
    #[error("a AWS negou {action}: {message}")]
    AccessDenied {
        action: &'static str,
        message: String,
    },
    #[error("a AWS limitou as chamadas de {action}: {message}")]
    Throttled {
        action: &'static str,
        message: String,
    },
    #[error("consulta inválida: {message}")]
    MalformedQuery { message: String },
    #[error("a consulta terminou com status {status}")]
    QueryDidNotComplete { status: String },
    #[error("a sessão do live tail terminou: {message}")]
    TailSessionEnded { message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

fn login_command(profile: &Option<String>) -> String {
    match profile {
        Some(profile) if profile != "default" => format!("aws login --profile {profile}"),
        _ => "aws login".to_string(),
    }
}

#[derive(Clone)]
pub struct CloudWatch {
    client: Client,
    profile: Option<String>,
    region: String,
}

impl fmt::Debug for CloudWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudWatch")
            .field("profile", &self.profile)
            .field("region", &self.region)
            .finish()
    }
}

/// Must run inside a Tokio runtime: the SDK resolves credentials and signs
/// requests on it.
pub async fn connect(
    target: &Target,
    http_client: Arc<dyn HttpClient>,
) -> Result<CloudWatch, CloudWatchError> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .http_client(AwsHttpClient::new(http_client))
        // Live Tail sessions can go quiet for minutes; stalled-stream
        // protection would otherwise abort them as stuck downloads.
        .stalled_stream_protection(StalledStreamProtectionConfig::disabled());
    if let Some(profile) = &target.profile {
        loader = loader.profile_name(profile);
    }
    if let Some(region) = &target.region {
        loader = loader.region(Region::new(region.clone()));
    }
    let config = loader.load().await;
    let region = config
        .region()
        .map(|region| region.to_string())
        .ok_or(CloudWatchError::NoRegion)?;
    Ok(CloudWatch {
        client: Client::new(&config),
        profile: target.profile.clone(),
        region,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogGroup {
    pub name: String,
    /// The ARN without the trailing `:*`, which is what Live Tail expects.
    pub arn: Option<String>,
    pub stored_bytes: Option<i64>,
    pub retention_days: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResult {
    /// Each row keeps the field order CloudWatch returned; `@ptr` is dropped.
    pub rows: Vec<Vec<(String, String)>>,
    pub records_matched: f64,
    pub records_scanned: f64,
    pub bytes_scanned: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogEvent {
    pub timestamp_ms: Option<i64>,
    pub log_group: Option<String>,
    pub log_stream: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TailUpdate {
    Started { session_id: Option<String> },
    Events { events: Vec<LogEvent>, sampled: bool },
}

pub struct LiveTail {
    stream: EventReceiver<StartLiveTailResponseStream, StartLiveTailResponseStreamError>,
    profile: Option<String>,
}

impl CloudWatch {
    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub async fn list_log_groups(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LogGroup>, CloudWatchError> {
        let mut pages = self
            .client
            .describe_log_groups()
            .set_log_group_name_prefix(prefix.map(str::to_string))
            .into_paginator()
            .items()
            .send();
        let mut groups = Vec::new();
        while groups.len() < limit {
            let Some(group) = pages.next().await else {
                break;
            };
            let group = group.map_err(|error| self.classify("DescribeLogGroups", error))?;
            let Some(name) = group.log_group_name() else {
                continue;
            };
            groups.push(LogGroup {
                name: name.to_string(),
                arn: group.log_group_arn().map(str::to_string),
                stored_bytes: group.stored_bytes(),
                retention_days: group.retention_in_days(),
            });
        }
        Ok(groups)
    }

    /// Runs a Logs Insights query to completion, polling every
    /// `poll_interval`. Dropping the future stops the query on the server so
    /// an abandoned query does not keep scanning (and billing).
    pub async fn run_insights_query(
        &self,
        log_groups: &[String],
        query: &str,
        start: SystemTime,
        end: SystemTime,
        limit: i32,
        poll_interval: Duration,
    ) -> Result<QueryResult, CloudWatchError> {
        let started = self
            .client
            .start_query()
            .set_log_group_names(Some(log_groups.to_vec()))
            .query_string(query)
            .start_time(unix_seconds(start))
            .end_time(unix_seconds(end))
            .limit(limit)
            .send()
            .await
            .map_err(|error| self.classify("StartQuery", error))?;
        let query_id = started
            .query_id()
            .ok_or_else(|| anyhow!("StartQuery não devolveu um queryId"))?
            .to_string();
        let mut stop_guard = StopQueryOnDrop {
            client: self.client.clone(),
            query_id: Some(query_id.clone()),
        };

        loop {
            let output = self
                .client
                .get_query_results()
                .query_id(&query_id)
                .send()
                .await
                .map_err(|error| self.classify("GetQueryResults", error))?;
            match output.status() {
                Some(QueryStatus::Complete) => {
                    stop_guard.query_id = None;
                    let statistics = output.statistics();
                    return Ok(QueryResult {
                        rows: output
                            .results()
                            .iter()
                            .map(|row| {
                                row.iter()
                                    .filter_map(|field| {
                                        let name = field.field()?;
                                        (name != "@ptr").then(|| {
                                            (
                                                name.to_string(),
                                                field.value().unwrap_or_default().to_string(),
                                            )
                                        })
                                    })
                                    .collect()
                            })
                            .collect(),
                        records_matched: statistics.map_or(0.0, |s| s.records_matched()),
                        records_scanned: statistics.map_or(0.0, |s| s.records_scanned()),
                        bytes_scanned: statistics.map_or(0.0, |s| s.bytes_scanned()),
                    });
                }
                Some(QueryStatus::Scheduled | QueryStatus::Running) | None => {
                    tokio::time::sleep(poll_interval).await;
                }
                Some(status) => {
                    stop_guard.query_id = None;
                    return Err(CloudWatchError::QueryDidNotComplete {
                        status: status.as_str().to_string(),
                    });
                }
            }
        }
    }

    pub async fn filter_log_events(
        &self,
        log_group: &str,
        filter_pattern: Option<&str>,
        start: SystemTime,
        limit: i32,
    ) -> Result<Vec<LogEvent>, CloudWatchError> {
        let output = self
            .client
            .filter_log_events()
            .log_group_name(log_group)
            .set_filter_pattern(filter_pattern.map(str::to_string))
            .start_time(unix_millis(start))
            .limit(limit)
            .send()
            .await
            .map_err(|error| self.classify("FilterLogEvents", error))?;
        Ok(output
            .events()
            .iter()
            .map(|event| LogEvent {
                timestamp_ms: event.timestamp(),
                log_group: Some(log_group.to_string()),
                log_stream: event.log_stream_name().map(str::to_string),
                message: event.message().unwrap_or_default().to_string(),
            })
            .collect())
    }

    /// `log_group_arns` must be ARNs without the trailing `:*` (see
    /// [`LogGroup::arn`]); Live Tail rejects plain names.
    pub async fn start_live_tail(
        &self,
        log_group_arns: &[String],
        filter_pattern: Option<&str>,
    ) -> Result<LiveTail, CloudWatchError> {
        let output = self
            .client
            .start_live_tail()
            .set_log_group_identifiers(Some(log_group_arns.to_vec()))
            .set_log_event_filter_pattern(filter_pattern.map(str::to_string))
            .send()
            .await
            .map_err(|error| self.classify("StartLiveTail", error))?;
        Ok(LiveTail {
            stream: output.response_stream,
            profile: self.profile.clone(),
        })
    }

    fn classify<E, R>(&self, action: &'static str, error: SdkError<E, R>) -> CloudWatchError
    where
        E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
        R: fmt::Debug + Send + Sync + 'static,
    {
        classify(action, &self.profile, error)
    }
}

impl LiveTail {
    /// Waits for the next message on the session. `Ok(None)` means the server
    /// closed the stream normally.
    pub async fn next_update(&mut self) -> Result<Option<TailUpdate>, CloudWatchError> {
        loop {
            let message = self
                .stream
                .recv()
                .await
                .map_err(|error| classify("StartLiveTail", &self.profile, error))?;
            let Some(message) = message else {
                return Ok(None);
            };
            match message {
                StartLiveTailResponseStream::SessionStart(start) => {
                    return Ok(Some(TailUpdate::Started {
                        session_id: start.session_id().map(str::to_string),
                    }));
                }
                StartLiveTailResponseStream::SessionUpdate(update) => {
                    return Ok(Some(TailUpdate::Events {
                        sampled: update
                            .session_metadata()
                            .is_some_and(|metadata| metadata.sampled()),
                        events: update
                            .session_results()
                            .iter()
                            .map(|event| LogEvent {
                                timestamp_ms: event.timestamp(),
                                log_group: event.log_group_identifier().map(str::to_string),
                                log_stream: event.log_stream_name().map(str::to_string),
                                message: event.message().unwrap_or_default().to_string(),
                            })
                            .collect(),
                    }));
                }
                _ => continue,
            }
        }
    }
}

struct StopQueryOnDrop {
    client: Client,
    query_id: Option<String>,
}

impl Drop for StopQueryOnDrop {
    fn drop(&mut self) {
        let Some(query_id) = self.query_id.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            log::warn!("consulta {query_id} abandonada fora do runtime; ela vai rodar até o fim");
            return;
        };
        let client = self.client.clone();
        runtime.spawn(async move {
            if let Err(error) = client.stop_query().query_id(&query_id).send().await {
                log::warn!(
                    "falha ao parar a consulta {query_id}: {}",
                    DisplayErrorContext(&error)
                );
            }
        });
    }
}

fn classify<E, R>(
    action: &'static str,
    profile: &Option<String>,
    error: SdkError<E, R>,
) -> CloudWatchError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: fmt::Debug + Send + Sync + 'static,
{
    let message = error
        .message()
        .map(str::to_string)
        .unwrap_or_else(|| DisplayErrorContext(&error).to_string());
    match error.code() {
        Some("AccessDeniedException" | "UnrecognizedClientException") => {
            return CloudWatchError::AccessDenied { action, message };
        }
        Some("ThrottlingException" | "LimitExceededException" | "TooManyRequestsException") => {
            return CloudWatchError::Throttled { action, message };
        }
        Some("MalformedQueryException") => return CloudWatchError::MalformedQuery { message },
        Some("ExpiredTokenException") => {
            return CloudWatchError::SessionExpired {
                profile: profile.clone(),
                message,
            };
        }
        Some("SessionTimeoutException" | "SessionStreamingException") => {
            return CloudWatchError::TailSessionEnded { message };
        }
        _ => {}
    }

    // Credential failures happen before any request is sent, so they carry no
    // service error code; the provider chain only reports them as text.
    let chain = DisplayErrorContext(&error).to_string();
    if chain.contains("session has expired") || chain.contains("Login token is expired") {
        return CloudWatchError::SessionExpired {
            profile: profile.clone(),
            message: chain,
        };
    }
    if chain.contains("no providers in chain provided credentials") {
        return CloudWatchError::NoCredentials { message: chain };
    }
    CloudWatchError::Other(anyhow!("{action}: {chain}"))
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn unix_millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}
