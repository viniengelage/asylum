//! The Docker Engine API over the daemon's unix socket. Requests run on the shared tokio
//! runtime, since hyper and the socket need it, while callers await from GPUI's executors.

use bytes::{Buf as _, Bytes, BytesMut};
use collections::HashMap;
use http_body_util::{BodyExt as _, Full};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
pub const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
pub const COMPOSE_WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";
pub const COMPOSE_CONFIG_FILES_LABEL: &str = "com.docker.compose.project.config_files";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, thiserror::Error)]
pub enum DockerError {
    #[error("o Docker não respondeu em {socket}: {reason}")]
    DaemonUnavailable { socket: String, reason: String },
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("o Docker recusou ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("resposta inesperada do Docker: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docker {
    socket: Arc<Path>,
}

impl Docker {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into().into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn ping(&self) -> Result<(), DockerError> {
        self.send("GET", "/_ping", None).await.map(drop)
    }

    pub async fn version(&self) -> Result<Version, DockerError> {
        self.get_json("/version").await
    }

    pub async fn info(&self) -> Result<SystemInfo, DockerError> {
        self.get_json("/info").await
    }

    pub async fn containers(&self) -> Result<Vec<Container>, DockerError> {
        self.get_json("/containers/json?all=1").await
    }

    pub async fn inspect(&self, id: &str) -> Result<ContainerDetails, DockerError> {
        self.get_json(&format!("/containers/{}/json", encode(id)))
            .await
    }

    /// The inspect output as the daemon wrote it, indented.
    pub async fn raw_inspect(&self, id: &str) -> Result<String, DockerError> {
        let value: serde_json::Value = self
            .get_json(&format!("/containers/{}/json", encode(id)))
            .await?;
        serde_json::to_string_pretty(&value).map_err(|error| DockerError::Decode(error.to_string()))
    }

    /// One sample of CPU and memory. The daemon takes two readings about a second apart to
    /// compute the CPU delta, so this takes that long.
    pub async fn stats(&self, id: &str) -> Result<Stats, DockerError> {
        let raw: RawStats = self
            .get_json(&format!("/containers/{}/stats?stream=false", encode(id)))
            .await?;
        Ok(raw.into())
    }

    pub async fn start(&self, id: &str) -> Result<(), DockerError> {
        self.lifecycle(id, "start").await
    }

    pub async fn stop(&self, id: &str) -> Result<(), DockerError> {
        self.lifecycle(id, "stop").await
    }

    pub async fn restart(&self, id: &str) -> Result<(), DockerError> {
        self.lifecycle(id, "restart").await
    }

    pub async fn pause(&self, id: &str) -> Result<(), DockerError> {
        self.lifecycle(id, "pause").await
    }

    pub async fn unpause(&self, id: &str) -> Result<(), DockerError> {
        self.lifecycle(id, "unpause").await
    }

    pub async fn remove(&self, id: &str) -> Result<(), DockerError> {
        self.send(
            "DELETE",
            &format!("/containers/{}?force=1", encode(id)),
            None,
        )
        .await
        .map(drop)
    }

    async fn lifecycle(&self, id: &str, action: &str) -> Result<(), DockerError> {
        self.send(
            "POST",
            &format!("/containers/{}/{action}", encode(id)),
            None,
        )
        .await
        .map(drop)
    }

    pub async fn images(&self) -> Result<Vec<Image>, DockerError> {
        self.get_json("/images/json").await
    }

    pub async fn volumes(&self) -> Result<Vec<Volume>, DockerError> {
        let list: VolumeList = self.get_json("/volumes").await?;
        Ok(list.volumes.unwrap_or_default())
    }

    /// Disk use per volume. The daemon walks every volume for it, so it is slow on big ones.
    pub async fn volume_sizes(&self) -> Result<HashMap<String, i64>, DockerError> {
        let usage: DiskUsage = self.get_json("/system/df?type=volume").await?;
        Ok(usage
            .volumes
            .unwrap_or_default()
            .into_iter()
            .filter_map(|volume| Some((volume.name, volume.usage_data?.size)))
            .collect())
    }

    pub async fn networks(&self) -> Result<Vec<Network>, DockerError> {
        self.get_json("/networks").await
    }

    pub async fn create(
        &self,
        name: Option<&str>,
        spec: &CreateContainer,
    ) -> Result<String, DockerError> {
        let path = match name {
            Some(name) if !name.is_empty() => format!("/containers/create?name={}", encode(name)),
            _ => "/containers/create".to_string(),
        };
        let body =
            serde_json::to_vec(spec).map_err(|error| DockerError::Decode(error.to_string()))?;
        let created: Created = decode(&self.send("POST", &path, Some(body)).await?)?;
        Ok(created.id)
    }

    /// Pulls `image`, sending each progress line the daemon reports. Errors the daemon reports
    /// mid-stream (bad tag, no access) fail the pull.
    pub async fn pull(
        &self,
        image: &str,
        progress: mpsc::UnboundedSender<PullProgress>,
    ) -> Result<(), DockerError> {
        let (name, tag) = split_tag(image);
        let path = format!(
            "/images/create?fromImage={}&tag={}",
            encode(name),
            encode(tag)
        );
        let mut lines = self.stream("POST", &path).await?;
        let mut buffer = BytesMut::new();
        while let Some(chunk) = lines.recv().await {
            buffer.extend_from_slice(&chunk?);
            while let Some(line) = take_line(&mut buffer) {
                let Ok(event) = serde_json::from_slice::<PullProgress>(&line) else {
                    continue;
                };
                if let Some(error) = &event.error {
                    return Err(DockerError::Api {
                        status: 500,
                        message: error.clone(),
                    });
                }
                progress.send(event).ok();
            }
        }
        Ok(())
    }

    /// Runs `command` with `sh -c` inside the container without a TTY and returns its output and
    /// exit code, or fails after `timeout`.
    pub async fn exec(
        &self,
        id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<ExecOutput, DockerError> {
        let create = serde_json::json!({
            "AttachStdout": true,
            "AttachStderr": true,
            "Tty": false,
            "Cmd": ["sh", "-c", command],
        });
        let created: Created = decode(
            &self
                .send(
                    "POST",
                    &format!("/containers/{}/exec", encode(id)),
                    Some(create.to_string().into_bytes()),
                )
                .await?,
        )?;
        let start = serde_json::json!({ "Detach": false, "Tty": false });
        let mut chunks = self
            .stream_with_body(
                "POST",
                &format!("/exec/{}/start", encode(&created.id)),
                Some(start.to_string().into_bytes()),
            )
            .await?;
        let mut demuxer = Demuxer::default();
        let mut output = ExecOutput::default();
        let read = async {
            while let Some(chunk) = chunks.recv().await {
                for frame in demuxer.push(&chunk?) {
                    match frame.stream {
                        LogStream::Stderr => output.stderr.push_str(&frame.text),
                        _ => output.stdout.push_str(&frame.text),
                    }
                    if output.stdout.len() + output.stderr.len() > MAX_EXEC_OUTPUT {
                        output.truncated = true;
                        return Ok(());
                    }
                }
            }
            Ok::<_, DockerError>(())
        };
        match tokio_timeout(timeout, read).await {
            Some(result) => result?,
            None => {
                output.timed_out = true;
                return Ok(output);
            }
        }
        let details: ExecDetails = self
            .get_json(&format!("/exec/{}/json", encode(&created.id)))
            .await?;
        output.exit_code = details.exit_code;
        Ok(output)
    }

    /// Log lines of the container, the last `tail` ones and then, with `follow`, every new one
    /// until the receiver is dropped.
    pub async fn logs(
        &self,
        id: &str,
        tail: usize,
        follow: bool,
    ) -> Result<mpsc::UnboundedReceiver<LogLine>, DockerError> {
        let path = format!(
            "/containers/{}/logs?stdout=1&stderr=1&timestamps=1&tail={tail}&follow={}",
            encode(id),
            u8::from(follow)
        );
        let mut chunks = self.stream("GET", &path).await?;
        let (sender, receiver) = mpsc::unbounded_channel();
        on_runtime_detached(async move {
            let mut demuxer = Demuxer::default();
            let mut pending: HashMap<LogStream, String> = HashMap::default();
            while let Some(chunk) = chunks.recv().await {
                let Ok(chunk) = chunk else { break };
                for frame in demuxer.push(&chunk) {
                    let partial = pending.entry(frame.stream).or_default();
                    partial.push_str(&frame.text);
                    while let Some(newline) = partial.find('\n') {
                        let line: String = partial.drain(..=newline).collect();
                        if sender
                            .send(LogLine::parse(
                                frame.stream,
                                line.trim_end_matches(['\n', '\r']),
                            ))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            for (stream, rest) in pending {
                if !rest.is_empty() {
                    sender.send(LogLine::parse(stream, &rest)).ok();
                }
            }
        });
        Ok(receiver)
    }

    /// Container events as they happen, until the receiver is dropped or the daemon goes away
    /// (the receiver then ends).
    pub async fn events(&self) -> Result<mpsc::UnboundedReceiver<Event>, DockerError> {
        let filters = encode(r#"{"type":["container"]}"#);
        let mut chunks = self
            .stream("GET", &format!("/events?filters={filters}"))
            .await?;
        let (sender, receiver) = mpsc::unbounded_channel();
        on_runtime_detached(async move {
            let mut buffer = BytesMut::new();
            while let Some(chunk) = chunks.recv().await {
                let Ok(chunk) = chunk else { break };
                buffer.extend_from_slice(&chunk);
                while let Some(line) = take_line(&mut buffer) {
                    match serde_json::from_slice::<Event>(&line) {
                        Ok(event) => {
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                        Err(error) => log::debug!("evento do Docker ignorado: {error}"),
                    }
                }
            }
        });
        Ok(receiver)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, DockerError> {
        decode(&self.send("GET", path, None).await?)
    }

    async fn send(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Bytes, DockerError> {
        let socket = self.socket.clone();
        let path = path.to_string();
        on_runtime(async move {
            let request = async {
                let response = transport::send(&socket, method, &path, body).await?;
                let status = response.status().as_u16();
                let bytes = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|error| unavailable(&socket, error))?
                    .to_bytes();
                check_status(status, bytes)
            };
            tokio_timeout(REQUEST_TIMEOUT, request)
                .await
                .unwrap_or_else(|| Err(unavailable(&socket, "tempo esgotado")))
        })
        .await
    }

    async fn stream(
        &self,
        method: &'static str,
        path: &str,
    ) -> Result<mpsc::UnboundedReceiver<Result<Bytes, DockerError>>, DockerError> {
        self.stream_with_body(method, path, None).await
    }

    /// The response body as it arrives. The reading task stops when the receiver is dropped.
    async fn stream_with_body(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<mpsc::UnboundedReceiver<Result<Bytes, DockerError>>, DockerError> {
        let socket = self.socket.clone();
        let path = path.to_string();
        let (sender, receiver) = mpsc::unbounded_channel();
        on_runtime(async move {
            let response = tokio_timeout(
                REQUEST_TIMEOUT,
                transport::send(&socket, method, &path, body),
            )
            .await
            .unwrap_or_else(|| Err(unavailable(&socket, "tempo esgotado")))?;
            let status = response.status().as_u16();
            let mut body = response.into_body();
            if status >= 300 {
                let bytes = body
                    .collect()
                    .await
                    .map_err(|error| unavailable(&socket, error))?
                    .to_bytes();
                check_status(status, bytes)?;
                return Ok(());
            }
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        frame = body.frame() => match frame {
                            Some(Ok(frame)) => {
                                if let Ok(data) = frame.into_data()
                                    && sender.send(Ok(data)).is_err()
                                {
                                    return;
                                }
                            }
                            Some(Err(error)) => {
                                sender.send(Err(unavailable(&socket, error))).ok();
                                return;
                            }
                            None => return,
                        },
                        _ = sender.closed() => return,
                    }
                }
            });
            Ok(())
        })
        .await?;
        Ok(receiver)
    }
}

const MAX_EXEC_OUTPUT: usize = 200_000;

#[cfg(unix)]
mod transport {
    use super::*;
    use hyper::{Request, Response, body::Incoming};
    use hyper_util::rt::TokioIo;

    pub(super) async fn send(
        socket: &Path,
        method: &'static str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response<Incoming>, DockerError> {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|error| unavailable(socket, error))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| unavailable(socket, error))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                log::debug!("conexão com o Docker encerrada: {error}");
            }
        });
        let has_body = body.is_some();
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "docker")
            .header("User-Agent", "Asylum")
            .when_header(has_body, "Content-Type", "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|error| DockerError::Decode(error.to_string()))?;
        sender
            .send_request(request)
            .await
            .map_err(|error| unavailable(socket, error))
    }

    trait WhenHeader {
        fn when_header(self, condition: bool, name: &str, value: &str) -> Self;
    }

    impl WhenHeader for hyper::http::request::Builder {
        fn when_header(self, condition: bool, name: &str, value: &str) -> Self {
            if condition {
                self.header(name, value)
            } else {
                self
            }
        }
    }
}

#[cfg(not(unix))]
mod transport {
    use super::*;

    pub(super) async fn send(
        socket: &Path,
        _method: &'static str,
        _path: &str,
        _body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<Full<Bytes>>, DockerError> {
        Err(unavailable(
            socket,
            "o Docker por named pipe do Windows ainda não é suportado",
        ))
    }
}

fn unavailable(socket: &Path, reason: impl std::fmt::Display) -> DockerError {
    DockerError::DaemonUnavailable {
        socket: socket.display().to_string(),
        reason: reason.to_string(),
    }
}

fn check_status(status: u16, bytes: Bytes) -> Result<Bytes, DockerError> {
    if status < 300 {
        return Ok(bytes);
    }
    let message = serde_json::from_slice::<ErrorMessage>(&bytes)
        .map(|error| error.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_string());
    Err(match status {
        // 304: already started, already stopped. The container is where the caller wanted it.
        304 => return Ok(Bytes::new()),
        404 => DockerError::NotFound(message),
        409 => DockerError::Conflict(message),
        _ => DockerError::Api { status, message },
    })
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DockerError> {
    serde_json::from_slice(bytes).map_err(|error| DockerError::Decode(error.to_string()))
}

fn encode(text: &str) -> String {
    urlencoding::encode(text).into_owned()
}

async fn on_runtime<T: Send + 'static>(
    future: impl Future<Output = Result<T, DockerError>> + Send + 'static,
) -> Result<T, DockerError> {
    reqwest_client::runtime()
        .spawn(future)
        .await
        .map_err(|error| DockerError::Decode(format!("a tarefa do Docker parou: {error}")))?
}

fn on_runtime_detached(future: impl Future<Output = ()> + Send + 'static) {
    // Dropping the handle detaches the task; it ends when its receiver is dropped.
    drop(reqwest_client::runtime().spawn(future));
}

async fn tokio_timeout<T>(duration: Duration, future: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(duration, future).await.ok()
}

fn take_line(buffer: &mut BytesMut) -> Option<Bytes> {
    let newline = buffer.iter().position(|byte| *byte == b'\n')?;
    let line = buffer.split_to(newline + 1).freeze();
    let line = line.slice(..newline);
    if line.iter().all(u8::is_ascii_whitespace) {
        return take_line(buffer);
    }
    Some(line)
}

/// `postgres:16-alpine` → (`postgres`, `16-alpine`); a registry port isn't a tag.
pub fn split_tag(image: &str) -> (&str, &str) {
    let image = image.split('@').next().unwrap_or(image);
    match image.rfind(':') {
        Some(colon) if !image[colon..].contains('/') => (&image[..colon], &image[colon + 1..]),
        _ => (image, "latest"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogStream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct Frame {
    stream: LogStream,
    text: String,
}

/// Splits what the daemon sends for a container without a TTY: each frame starts with 8 bytes
/// (the stream, three zeros and the length, big-endian). A container with a TTY sends the raw
/// text instead, which is told apart by its first bytes.
#[derive(Default)]
struct Demuxer {
    buffer: BytesMut,
    raw: Option<bool>,
}

impl Demuxer {
    fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(chunk);
        let raw = *self.raw.get_or_insert_with(|| {
            !(self.buffer.len() >= 8 && self.buffer[0] <= 2 && self.buffer[1..4] == [0, 0, 0])
        });
        if raw {
            let bytes = self.buffer.split();
            return vec![Frame {
                stream: LogStream::Stdout,
                text: String::from_utf8_lossy(&bytes).into_owned(),
            }];
        }
        let mut frames = Vec::new();
        while self.buffer.len() >= 8 {
            let stream = match self.buffer[0] {
                0 => LogStream::Stdin,
                2 => LogStream::Stderr,
                _ => LogStream::Stdout,
            };
            let length = u32::from_be_bytes([
                self.buffer[4],
                self.buffer[5],
                self.buffer[6],
                self.buffer[7],
            ]) as usize;
            if self.buffer.len() < 8 + length {
                break;
            }
            self.buffer.advance(8);
            let payload = self.buffer.split_to(length);
            frames.push(Frame {
                stream,
                text: String::from_utf8_lossy(&payload).into_owned(),
            });
        }
        frames
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub stream: LogStream,
    /// RFC 3339 as the daemon wrote it, without the fraction past milliseconds.
    pub timestamp: Option<String>,
    pub text: String,
}

impl LogLine {
    fn parse(stream: LogStream, line: &str) -> Self {
        match line.split_once(' ') {
            Some((timestamp, text))
                if timestamp.len() >= 20 && timestamp.as_bytes().get(4) == Some(&b'-') =>
            {
                Self {
                    stream,
                    timestamp: Some(timestamp.to_string()),
                    text: text.to_string(),
                }
            }
            _ => Self {
                stream,
                timestamp: None,
                text: line.to_string(),
            },
        }
    }

    /// `14:02:09.881`, in local time.
    pub fn clock(&self) -> Option<String> {
        let timestamp = chrono::DateTime::parse_from_rfc3339(self.timestamp.as_deref()?).ok()?;
        Some(
            timestamp
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string(),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorMessage {
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Created {
    #[serde(rename = "Id")]
    id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Version {
    pub version: String,
    pub api_version: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SystemInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub operating_system: String,
    #[serde(default)]
    pub server_version: String,
    #[serde(default, rename = "NCPU")]
    pub cpus: u32,
    #[serde(default)]
    pub mem_total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    Running,
    Paused,
    Restarting,
    Exited,
    Created,
    Dead,
    Removing,
}

impl ContainerState {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Restarting)
    }

    /// Whether the "Rodando" segment shows it: it counts as up, even paused.
    pub fn is_up(self) -> bool {
        matches!(self, Self::Running | Self::Restarting | Self::Paused)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "rodando",
            Self::Paused => "pausado",
            Self::Restarting => "reiniciando",
            Self::Exited => "parado",
            Self::Created => "criado",
            Self::Dead => "morto",
            Self::Removing => "removendo",
        }
    }
}

fn unknown_state<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ContainerState, D::Error> {
    let text = String::deserialize(deserializer)?;
    Ok(
        serde_json::from_value(serde_json::Value::String(text.to_lowercase()))
            .unwrap_or(ContainerState::Created),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Port {
    #[serde(default, rename = "IP")]
    pub ip: Option<String>,
    pub private_port: u16,
    #[serde(default)]
    pub public_port: Option<u16>,
    #[serde(default, rename = "Type")]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Container {
    pub id: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub created: i64,
    #[serde(deserialize_with = "unknown_state")]
    pub state: ContainerState,
    /// What `docker ps` shows: "Up 2 hours (healthy)", "Exited (0) 2 hours ago".
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ports: Vec<Port>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

impl Container {
    pub fn name(&self) -> &str {
        self.names
            .first()
            .map(|name| name.trim_start_matches('/'))
            .unwrap_or(&self.id)
    }

    /// The compose service when it is one, which is how the boards name containers.
    pub fn display_name(&self) -> &str {
        self.labels
            .get(COMPOSE_SERVICE_LABEL)
            .map(String::as_str)
            .unwrap_or_else(|| self.name())
    }

    pub fn short_id(&self) -> &str {
        self.id.get(..12).unwrap_or(&self.id)
    }

    pub fn compose_project(&self) -> Option<&str> {
        self.labels.get(COMPOSE_PROJECT_LABEL).map(String::as_str)
    }

    pub fn compose_working_dir(&self) -> Option<&Path> {
        self.labels
            .get(COMPOSE_WORKING_DIR_LABEL)
            .map(|dir| Path::new(dir.as_str()))
    }

    pub fn compose_config_files(&self) -> Vec<PathBuf> {
        self.labels
            .get(COMPOSE_CONFIG_FILES_LABEL)
            .map(|files| files.split(',').map(PathBuf::from).collect())
            .unwrap_or_default()
    }

    /// Published ports, deduplicated across IPv4 and IPv6: `:3000→3000`.
    pub fn published_ports(&self) -> Vec<(u16, u16)> {
        let mut ports: Vec<(u16, u16)> = self
            .ports
            .iter()
            .filter_map(|port| Some((port.public_port?, port.private_port)))
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// The exit code in "Exited (137) 3 minutes ago".
    pub fn exit_code(&self) -> Option<i64> {
        let rest = self.status.strip_prefix("Exited (")?;
        rest.split(')').next()?.parse().ok()
    }

    /// "3 minutes ago" from the status, for "há 3 min".
    pub fn status_age(&self) -> Option<&str> {
        let (_, age) = self.status.split_once(") ")?;
        Some(age.trim_end_matches(" ago"))
    }

    pub fn health(&self) -> Option<&'static str> {
        if self.status.contains("(healthy)") {
            Some("healthy")
        } else if self.status.contains("(unhealthy)") {
            Some("unhealthy")
        } else if self.status.contains("(health: starting)") {
            Some("starting")
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerDetails {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub restart_count: i64,
    #[serde(default)]
    pub state: StateDetails,
    #[serde(default)]
    pub config: ConfigDetails,
    #[serde(default)]
    pub host_config: HostConfigDetails,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct StateDetails {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    #[serde(default)]
    pub oom_killed: bool,
    #[serde(default)]
    pub health: Option<HealthDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HealthDetails {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub failing_streak: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ConfigDetails {
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub tty: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfigDetails {
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub network_mode: String,
    #[serde(default)]
    pub binds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RestartPolicy {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
}

#[derive(Debug, Default, Deserialize)]
struct RawStats {
    #[serde(default)]
    cpu_stats: CpuStats,
    #[serde(default)]
    precpu_stats: CpuStats,
    #[serde(default)]
    memory_stats: MemoryStats,
}

#[derive(Debug, Default, Deserialize)]
struct CpuStats {
    #[serde(default)]
    cpu_usage: CpuUsage,
    #[serde(default)]
    system_cpu_usage: u64,
    #[serde(default)]
    online_cpus: u64,
}

#[derive(Debug, Default, Deserialize)]
struct CpuUsage {
    #[serde(default)]
    total_usage: u64,
    #[serde(default)]
    percpu_usage: Option<Vec<u64>>,
}

#[derive(Debug, Default, Deserialize)]
struct MemoryStats {
    #[serde(default)]
    usage: u64,
    #[serde(default)]
    stats: HashMap<String, u64>,
}

impl From<RawStats> for Stats {
    fn from(raw: RawStats) -> Self {
        let cpu_delta = raw
            .cpu_stats
            .cpu_usage
            .total_usage
            .saturating_sub(raw.precpu_stats.cpu_usage.total_usage) as f64;
        let system_delta = raw
            .cpu_stats
            .system_cpu_usage
            .saturating_sub(raw.precpu_stats.system_cpu_usage) as f64;
        let cpus = match raw.cpu_stats.online_cpus {
            0 => raw
                .cpu_stats
                .cpu_usage
                .percpu_usage
                .as_ref()
                .map_or(1, |usage| usage.len().max(1) as u64),
            cpus => cpus,
        } as f64;
        let cpu_percent = if system_delta > 0.0 {
            cpu_delta / system_delta * cpus * 100.0
        } else {
            0.0
        };
        // What `docker stats` shows: the page cache doesn't count (cgroup v2 calls it
        // inactive_file, v1 total_inactive_file).
        let cache = raw
            .memory_stats
            .stats
            .get("inactive_file")
            .or_else(|| raw.memory_stats.stats.get("total_inactive_file"))
            .copied()
            .unwrap_or(0);
        Self {
            cpu_percent,
            memory_bytes: raw.memory_stats.usage.saturating_sub(cache),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Image {
    pub id: String,
    #[serde(default)]
    pub repo_tags: Option<Vec<String>>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub created: i64,
}

impl Image {
    pub fn tags(&self) -> impl Iterator<Item = &str> {
        self.repo_tags
            .iter()
            .flatten()
            .map(String::as_str)
            .filter(|tag| *tag != "<none>:<none>")
    }
}

#[derive(Debug, Deserialize)]
struct VolumeList {
    #[serde(rename = "Volumes")]
    volumes: Option<Vec<Volume>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Volume {
    pub name: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub mountpoint: String,
}

#[derive(Debug, Deserialize)]
struct DiskUsage {
    #[serde(rename = "Volumes")]
    volumes: Option<Vec<VolumeUsage>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeUsage {
    name: String,
    usage_data: Option<UsageData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UsageData {
    size: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Network {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub driver: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Event {
    #[serde(default, rename = "Action")]
    pub action: String,
    #[serde(default, rename = "Actor")]
    pub actor: EventActor,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventActor {
    #[serde(default, rename = "ID")]
    pub id: String,
    #[serde(default, rename = "Attributes")]
    pub attributes: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PullProgress {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "progressDetail")]
    pub progress_detail: Option<ProgressDetail>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProgressDetail {
    #[serde(default)]
    pub current: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
struct ExecDetails {
    #[serde(rename = "ExitCode")]
    exit_code: Option<i64>,
}

/// The body of `POST /containers/create`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateContainer {
    pub image: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub exposed_ports: HashMap<String, serde_json::Value>,
    pub host_config: CreateHostConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateHostConfig {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub port_bindings: HashMap<String, Vec<PortBinding>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<String>,
    pub auto_remove: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PortBinding {
    pub host_port: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_containers_with_compose_labels() {
        let json = r#"[{
            "Id": "a1f3c9e2b7d4aa00ff",
            "Names": ["/trix_frontend_app-api-1"],
            "Image": "trix-api:dev",
            "Command": "node dist/main.js",
            "Created": 1700000000,
            "State": "running",
            "Status": "Up 2 hours (healthy)",
            "Ports": [
                {"IP": "0.0.0.0", "PrivatePort": 3000, "PublicPort": 3000, "Type": "tcp"},
                {"IP": "::", "PrivatePort": 3000, "PublicPort": 3000, "Type": "tcp"},
                {"PrivatePort": 9229, "Type": "tcp"}
            ],
            "Labels": {
                "com.docker.compose.project": "trix_frontend_app",
                "com.docker.compose.service": "api",
                "com.docker.compose.project.working_dir": "/Users/v/trix_frontend_app"
            }
        }, {
            "Id": "bb",
            "Names": ["/mailpit"],
            "Image": "axllent/mailpit",
            "State": "exited",
            "Status": "Exited (137) 2 hours ago",
            "Ports": [],
            "Labels": {}
        }, {
            "Id": "cc",
            "Names": ["/weird"],
            "State": "something-new",
            "Labels": null
        }]"#
        .replace("\"Labels\": null", "\"Labels\": {}");
        let containers: Vec<Container> = serde_json::from_str(&json).expect("decodes");
        let api = &containers[0];
        assert_eq!(api.display_name(), "api");
        assert_eq!(api.name(), "trix_frontend_app-api-1");
        assert_eq!(api.short_id(), "a1f3c9e2b7d4");
        assert_eq!(api.compose_project(), Some("trix_frontend_app"));
        assert_eq!(api.published_ports(), vec![(3000, 3000)]);
        assert_eq!(api.health(), Some("healthy"));
        let mailpit = &containers[1];
        assert_eq!(mailpit.state, ContainerState::Exited);
        assert_eq!(mailpit.exit_code(), Some(137));
        assert_eq!(mailpit.status_age(), Some("2 hours"));
        assert_eq!(containers[2].state, ContainerState::Created);
    }

    #[test]
    fn computes_stats_like_docker_stats() {
        let json = r#"{
            "cpu_stats": {"cpu_usage": {"total_usage": 2000000}, "system_cpu_usage": 100000000, "online_cpus": 4},
            "precpu_stats": {"cpu_usage": {"total_usage": 1000000}, "system_cpu_usage": 0},
            "memory_stats": {"usage": 200000000, "stats": {"inactive_file": 9000000}}
        }"#;
        let stats: Stats = serde_json::from_str::<RawStats>(json)
            .expect("decodes")
            .into();
        assert!((stats.cpu_percent - 4.0).abs() < 1e-9);
        assert_eq!(stats.memory_bytes, 191_000_000);
    }

    #[test]
    fn splits_multiplexed_frames_across_chunks() {
        let mut bytes = Vec::new();
        for (stream, text) in [(1u8, "out one\n"), (2, "err one\n")] {
            bytes.extend_from_slice(&[stream, 0, 0, 0]);
            bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        let mut demuxer = Demuxer::default();
        let mut frames = demuxer.push(&bytes[..11]);
        frames.extend(demuxer.push(&bytes[11..]));
        let collected: Vec<_> = frames
            .into_iter()
            .map(|frame| (frame.stream, frame.text))
            .collect();
        assert_eq!(
            collected,
            vec![
                (LogStream::Stdout, "out one\n".to_string()),
                (LogStream::Stderr, "err one\n".to_string())
            ]
        );

        let mut tty = Demuxer::default();
        let frames = tty.push(b"plain text from a tty\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].text, "plain text from a tty\n");
    }

    #[test]
    fn parses_timestamps_and_tags() {
        let line = LogLine::parse(
            LogStream::Stdout,
            "2026-10-31T14:02:09.881234567Z [boot] trix-api ouvindo em :3000",
        );
        assert_eq!(line.text, "[boot] trix-api ouvindo em :3000");
        assert!(line.clock().is_some());
        assert_eq!(
            LogLine::parse(LogStream::Stdout, "sem hora").timestamp,
            None
        );
        assert_eq!(split_tag("postgres:16-alpine"), ("postgres", "16-alpine"));
        assert_eq!(
            split_tag("localhost:5000/api"),
            ("localhost:5000/api", "latest")
        );
        assert_eq!(split_tag("redis"), ("redis", "latest"));
    }

    #[test]
    fn maps_status_codes_to_errors() {
        assert!(matches!(
            check_status(404, Bytes::from_static(br#"{"message":"No such container: x"}"#)),
            Err(DockerError::NotFound(message)) if message == "No such container: x"
        ));
        assert!(matches!(
            check_status(409, Bytes::from_static(b"{\"message\":\"is paused\"}")),
            Err(DockerError::Conflict(_))
        ));
        assert!(check_status(304, Bytes::new()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn talks_to_a_fake_daemon() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let runtime = reqwest_client::runtime();
        let directory = std::env::temp_dir().join(format!("asylum-docker-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let socket = directory.join("docker.sock");
        std::fs::remove_file(&socket).ok();
        let listener = {
            let _guard = runtime.enter();
            tokio::net::UnixListener::bind(&socket).expect("binds")
        };
        runtime.spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = vec![0u8; 4096];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..read]).to_string();
                    let (status, body) = if request.starts_with("GET /_ping") {
                        ("200 OK", "OK".to_string())
                    } else if request.starts_with("GET /containers/json") {
                        (
                            "200 OK",
                            r#"[{"Id":"abc","Names":["/redis"],"Image":"redis:7-alpine","State":"paused","Status":"Up 1 hour (Paused)","Ports":[],"Labels":{}}]"#.to_string(),
                        )
                    } else if request.starts_with("POST /containers/missing/start") {
                        (
                            "404 Not Found",
                            r#"{"message":"No such container: missing"}"#.to_string(),
                        )
                    } else {
                        ("204 No Content", String::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.ok();
                });
            }
        });

        let docker = Docker::new(&socket);
        futures::executor::block_on(async {
            docker.ping().await.expect("ping");
            let containers = docker.containers().await.expect("containers");
            assert_eq!(containers[0].state, ContainerState::Paused);
            docker.unpause("abc").await.expect("unpause");
            assert!(matches!(
                docker.start("missing").await,
                Err(DockerError::NotFound(_))
            ));
            let offline = Docker::new(directory.join("nobody.sock"));
            assert!(matches!(
                offline.ping().await,
                Err(DockerError::DaemonUnavailable { .. })
            ));
        });
        std::fs::remove_dir_all(&directory).ok();
    }
}
