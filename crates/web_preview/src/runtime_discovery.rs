use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
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
    browser_cache_dir_for_profile(paths::active_profile_id().unwrap_or(paths::DEFAULT_PROFILE_ID))
}

/// Where the browser of `profile_id` keeps its cookies, storage and caches.
#[cfg(target_os = "macos")]
pub fn browser_cache_dir_for_profile(profile_id: &str) -> PathBuf {
    crate::cef_paths::support_dir().join("Profiles").join(profile_id)
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

pub fn write_runtime_discovery(active_url: &str, active_target_id: Option<&str>) {
    let info = RuntimeInfo {
        endpoint: endpoint(),
        active_target_id: active_target_id.map(str::to_string),
        active_url: active_url.to_string(),
        workspace: None,
    };
    write_json(&runtime_file_path(), &info);
}

fn endpoint() -> String {
    format!("http://127.0.0.1:{}", remote_debugging_port())
}

#[derive(serde::Serialize)]
struct OpenedPage<'a> {
    endpoint: String,
    #[serde(rename = "targetId")]
    target_id: &'a str,
    url: &'a str,
}

/// Only ids that are safe as a file name, since they come from outside through a URL.
pub fn is_valid_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= 64
        && request_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn request_file_path(request_id: &str) -> PathBuf {
    discovery_dir().join("requests").join(format!("{request_id}.json"))
}

/// Answers a `zed://browser?id=...` request with the page it opened, which is how the
/// script that asked finds the tab on the CDP port. The file only appears once the target
/// id is known, so its existence is the signal that the page can be attached to.
pub fn write_opened_page(request_id: &str, target_id: &str, url: &str) {
    let page = OpenedPage {
        endpoint: endpoint(),
        target_id,
        url,
    };
    write_json(&request_file_path(request_id), &page);
}

fn write_json(path: &Path, value: &impl serde::Serialize) {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        log::warn!(
            "web_preview: failed to create {}: {error}",
            parent.display()
        );
        return;
    }
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(error) => {
            log::warn!(
                "web_preview: failed to serialize {}: {error}",
                path.display()
            );
            return;
        }
    };
    // Written beside the destination and renamed over it, so a script polling for the
    // file never reads it half written.
    let temporary_path = path.with_extension("json.tmp");
    if let Err(error) =
        std::fs::write(&temporary_path, json).and_then(|()| std::fs::rename(&temporary_path, path))
    {
        log::warn!("web_preview: failed to write {}: {error}", path.display());
    }
}

#[allow(dead_code)]
pub fn remove_runtime_discovery() {
    let path = runtime_file_path();
    std::fs::remove_file(&path).ok();
}
