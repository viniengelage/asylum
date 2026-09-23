use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The port the default profile has always exposed CDP on, which MCP configs point at.
const DEFAULT_REMOTE_DEBUGGING_PORT: u16 = 9223;

#[derive(serde::Serialize)]
struct RuntimeInfo {
    endpoint: String,
    #[serde(rename = "activeTargetId")]
    active_target_id: Option<String>,
    #[serde(rename = "activeUrl")]
    active_url: String,
    workspace: Option<String>,
}

/// The port Chromium serves CDP on. Profiles run their own CEF side by side, so
/// a non-default profile takes whichever port is free and publishes it in its
/// `runtime.json`.
pub fn remote_debugging_port() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        if paths::active_profile_id().is_none() {
            return DEFAULT_REMOTE_DEBUGGING_PORT;
        }
        match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).and_then(|listener| listener.local_addr())
        {
            Ok(address) => address.port(),
            Err(error) => {
                log::warn!("web_preview: no free port for CDP, using the default one: {error}");
                DEFAULT_REMOTE_DEBUGGING_PORT
            }
        }
    })
}

/// Where Chromium keeps cookies, storage and caches. Two CEF instances can't share one.
#[cfg(target_os = "macos")]
pub fn browser_cache_dir() -> PathBuf {
    crate::cef_paths::support_dir()
        .join("Profiles")
        .join(paths::active_profile_id().unwrap_or(paths::DEFAULT_PROFILE_ID))
}

/// Where the files other tools use to find and drive this browser live.
pub fn discovery_dir() -> PathBuf {
    match paths::active_profile_id() {
        Some(_) => paths::data_dir().join("web_preview"),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
            PathBuf::from(home).join("Library/Application Support/Zed Workstation/WebPreview")
        }
    }
}

fn runtime_file_path() -> PathBuf {
    discovery_dir().join("runtime.json")
}

pub fn write_runtime_discovery(active_url: &str, workspace_path: Option<&str>) {
    let info = RuntimeInfo {
        endpoint: format!("http://127.0.0.1:{}", remote_debugging_port()),
        active_target_id: None,
        active_url: active_url.to_string(),
        workspace: workspace_path.map(|s| s.to_string()),
    };

    let path = runtime_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    match serde_json::to_string_pretty(&info) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&path, json) {
                log::warn!("web_preview: failed to write runtime.json: {error}");
            }
        }
        Err(error) => {
            log::warn!("web_preview: failed to serialize runtime.json: {error}");
        }
    }
}

#[allow(dead_code)]
pub fn remove_runtime_discovery() {
    let path = runtime_file_path();
    std::fs::remove_file(&path).ok();
}
