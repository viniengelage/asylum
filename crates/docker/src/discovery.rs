//! Where the daemon's socket is: `DOCKER_HOST`, the active `docker context`, then the places
//! Docker Desktop, Linux, Colima and OrbStack put it.

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketCandidate {
    /// "Docker Desktop", "Colima", "OrbStack"… as the empty state names them.
    pub runtime: String,
    pub path: PathBuf,
    pub exists: bool,
}

impl SocketCandidate {
    /// `~/.colima/…/docker.sock`, short enough for the empty state's list.
    pub fn short_path(&self) -> String {
        let display = home_relative(&self.path);
        let parts: Vec<&str> = display.split('/').collect();
        if parts.len() <= 3 {
            return display;
        }
        format!(
            "{}/{}/…/{}",
            parts.first().copied().unwrap_or_default(),
            parts.get(1).copied().unwrap_or_default(),
            parts.last().copied().unwrap_or_default()
        )
    }
}

pub fn home_relative(path: &Path) -> String {
    match path.strip_prefix(paths::home_dir()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Every place worth trying, in order, without duplicates. `exists` says whether the socket
/// file is there; a stale file can still refuse connections.
pub fn candidates() -> Vec<SocketCandidate> {
    let home = paths::home_dir();
    let mut found: Vec<(Option<String>, PathBuf)> = Vec::new();
    if let Some(path) = std::env::var("DOCKER_HOST")
        .ok()
        .as_deref()
        .and_then(unix_path)
    {
        found.push((None, path));
    }
    if let Some(path) = active_context_socket(&home.join(".docker")) {
        found.push((None, path));
    }
    found.push((
        Some("Docker Desktop".into()),
        home.join(".docker/run/docker.sock"),
    ));
    found.push((None, PathBuf::from("/var/run/docker.sock")));
    found.push((
        Some("Colima".into()),
        home.join(".colima/default/docker.sock"),
    ));
    found.push((
        Some("OrbStack".into()),
        home.join(".orbstack/run/docker.sock"),
    ));

    let mut candidates: Vec<SocketCandidate> = Vec::new();
    for (runtime, path) in found {
        if candidates.iter().any(|candidate| candidate.path == path) {
            continue;
        }
        let exists = path.exists();
        candidates.push(SocketCandidate {
            runtime: runtime.unwrap_or_else(|| runtime_for(&path, exists)),
            path,
            exists,
        });
    }
    candidates
}

/// The runtime behind a socket, by where it lives. `/var/run/docker.sock` on macOS is Docker
/// Desktop's symlink unless it points somewhere else.
pub fn runtime_for(path: &Path, exists: bool) -> String {
    let resolved = if exists {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let text = resolved.to_string_lossy().to_lowercase();
    if text.contains("colima") {
        "Colima".into()
    } else if text.contains("orbstack") {
        "OrbStack".into()
    } else if text.contains("rancher") || text.contains(".rd/") {
        "Rancher Desktop".into()
    } else if text.contains("podman") {
        "Podman".into()
    } else if text.contains(".docker/run") || cfg!(target_os = "macos") {
        "Docker Desktop".into()
    } else {
        "Docker Engine".into()
    }
}

fn unix_path(host: &str) -> Option<PathBuf> {
    host.strip_prefix("unix://").map(PathBuf::from)
}

#[derive(Deserialize)]
struct CliConfig {
    #[serde(default, rename = "currentContext")]
    current_context: Option<String>,
}

#[derive(Deserialize)]
struct ContextMeta {
    #[serde(rename = "Endpoints")]
    endpoints: ContextEndpoints,
}

#[derive(Deserialize)]
struct ContextEndpoints {
    docker: ContextEndpoint,
}

#[derive(Deserialize)]
struct ContextEndpoint {
    #[serde(rename = "Host")]
    host: String,
}

/// The CLI keeps each context in `contexts/meta/<sha256 of the name>/meta.json`.
fn active_context_socket(docker_dir: &Path) -> Option<PathBuf> {
    let config: CliConfig =
        serde_json::from_slice(&std::fs::read(docker_dir.join("config.json")).ok()?).ok()?;
    let name = config.current_context.filter(|name| name != "default")?;
    let digest = Sha256::digest(name.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let meta_path = docker_dir.join("contexts/meta").join(hex).join("meta.json");
    let meta: ContextMeta = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
    unix_path(&meta.endpoints.docker.host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_active_context() {
        let directory =
            std::env::temp_dir().join(format!("asylum-docker-context-{}", std::process::id()));
        let digest = Sha256::digest(b"colima");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let meta_dir = directory.join("contexts/meta").join(hex);
        std::fs::create_dir_all(&meta_dir).expect("dir");
        std::fs::write(
            directory.join("config.json"),
            r#"{"auths":{},"currentContext":"colima"}"#,
        )
        .expect("config");
        std::fs::write(
            meta_dir.join("meta.json"),
            r#"{"Name":"colima","Endpoints":{"docker":{"Host":"unix:///Users/v/.colima/default/docker.sock","SkipTLSVerify":false}}}"#,
        )
        .expect("meta");
        assert_eq!(
            active_context_socket(&directory),
            Some(PathBuf::from("/Users/v/.colima/default/docker.sock"))
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn names_runtimes_by_path() {
        assert_eq!(
            runtime_for(Path::new("/Users/v/.colima/default/docker.sock"), false),
            "Colima"
        );
        assert_eq!(
            runtime_for(Path::new("/Users/v/.orbstack/run/docker.sock"), false),
            "OrbStack"
        );
        let candidates = candidates();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.path == Path::new("/var/run/docker.sock"))
        );
    }
}
