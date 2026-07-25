use std::path::PathBuf;

const REMOTE_DEBUGGING_PORT: u16 = 9223;

#[derive(serde::Serialize)]
struct RuntimeInfo {
    endpoint: String,
    #[serde(rename = "activeTargetId")]
    active_target_id: Option<String>,
    #[serde(rename = "activeUrl")]
    active_url: String,
    workspace: Option<String>,
}

fn runtime_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(format!(
        "{}/Library/Application Support/Zed Workstation/WebPreview/runtime.json",
        home
    ))
}

pub fn write_runtime_discovery(active_url: &str, workspace_path: Option<&str>) {
    let info = RuntimeInfo {
        endpoint: format!("http://127.0.0.1:{}", REMOTE_DEBUGGING_PORT),
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
