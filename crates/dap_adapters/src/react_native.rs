use anyhow::Context as _;
use async_trait::async_trait;
use collections::HashMap;
use dap::{
    StartDebuggingRequestArguments, StartDebuggingRequestArgumentsRequest,
    adapters::DebugTaskDefinition,
};
use gpui::AsyncApp;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::OnceLock};
use task::{DebugScenario, ZedDebugConfig};

use crate::{javascript::JsDebugAdapter, *};

#[derive(Debug, Default)]
pub(crate) struct ReactNativeExpoDebugAdapter {
    checked: OnceLock<()>,
}

impl ReactNativeExpoDebugAdapter {
    const ADAPTER_NAME: &'static str = "React Native / Expo";

    const DISCOVERY_MAX_ATTEMPTS: u32 = 30;
    const DISCOVERY_INTERVAL_SECS: u64 = 2;

    /// Query Metro's `/json/list` endpoint to discover the Hermes inspector
    /// WebSocket URL. Retries waiting for Metro to start and for the app to
    /// register its Hermes inspector (~60s total).
    async fn discover_hermes_websocket_url(
        delegate: &Arc<dyn DapDelegate>,
        metro_address: &str,
        metro_port: u64,
        device_name: Option<&str>,
        executor: &gpui::BackgroundExecutor,
    ) -> Option<String> {
        let url = format!("http://{}:{}/json/list", metro_address, metro_port);
        let max = Self::DISCOVERY_MAX_ATTEMPTS;
        for attempt in 0..max {
            if attempt > 0 {
                executor
                    .timer(std::time::Duration::from_secs(
                        Self::DISCOVERY_INTERVAL_SECS,
                    ))
                    .await;
            }
            let output = match smol::process::Command::new("curl")
                .args(["-s", "--max-time", "3", &url])
                .output()
                .await
            {
                Ok(output) if output.status.success() => output,
                _ => {
                    delegate.output_to_console(format!(
                        "Waiting for Metro at {url} (attempt {}/{max})...",
                        attempt + 1
                    ));
                    continue;
                }
            };
            let body = match String::from_utf8(output.stdout) {
                Ok(body) => body,
                Err(_) => continue,
            };
            let targets: Vec<Value> = match serde_json::from_str(&body) {
                Ok(targets) => targets,
                Err(_) => continue,
            };
            match pick_hermes_target(&targets, device_name) {
                TargetChoice::Found {
                    websocket_url,
                    other_devices,
                } => {
                    if !other_devices.is_empty() {
                        delegate.output_to_console(format!(
                            "More than one app is connected to Metro ({}); attaching to the first. \
                             Set \"deviceName\" in the debug scenario to pick another.",
                            other_devices.join(", ")
                        ));
                    }
                    return Some(websocket_url);
                }
                TargetChoice::NoMatch { available } => {
                    delegate.output_to_console(format!(
                        "No app on {} is connected to Metro yet; connected: {} (attempt {}/{max})...",
                        device_name.unwrap_or_default(),
                        available.join(", "),
                        attempt + 1
                    ));
                    continue;
                }
                TargetChoice::Empty => {}
            }
            delegate.output_to_console(format!(
                "Metro responded but no Hermes targets yet (attempt {}/{max})...",
                attempt + 1
            ));
        }
        None
    }

    async fn get_installed_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        task_definition: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        executor: &gpui::BackgroundExecutor,
    ) -> Result<DebugAdapterBinary> {
        let tcp_connection = task_definition.tcp_connection.clone().unwrap_or_default();
        let (host, port, timeout) = crate::configure_tcp_connection(tcp_connection).await?;

        let mut envs = user_env.unwrap_or_default();
        let mut configuration = task_definition
            .config
            .as_object()
            .cloned()
            .context("React Native / Expo debug configuration must be an object")?;

        let request = configuration
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or("attach");
        anyhow::ensure!(
            request == "attach",
            "React Native / Expo only supports attaching to a running Hermes app"
        );

        let metro_address = configuration
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1")
            .to_owned();
        let metro_port = configuration
            .get("port")
            .and_then(Value::as_u64)
            .unwrap_or(8081);
        let device_name = configuration
            .remove("deviceName")
            .and_then(|name| name.as_str().map(str::to_owned))
            .filter(|name| !name.trim().is_empty());

        configuration.insert("type".to_owned(), "pwa-node".into());
        configuration.insert("request".to_owned(), "attach".into());
        configuration
            .entry("continueOnAttach".to_owned())
            .or_insert(true.into());
        configuration
            .entry("restart".to_owned())
            .or_insert(true.into());
        configuration
            .entry("sourceMaps".to_owned())
            .or_insert(true.into());
        configuration
            .entry("pauseForSourceMap".to_owned())
            .or_insert(true.into());
        configuration
            .entry("sourceMapRenames".to_owned())
            .or_insert(true.into());
        configuration
            .entry("cwd".to_owned())
            .or_insert(delegate.worktree_root_path().to_string_lossy().into());

        if !configuration.contains_key("websocketAddress") {
            if let Some(websocket_url) = Self::discover_hermes_websocket_url(
                delegate,
                &metro_address,
                metro_port,
                device_name.as_deref(),
                executor,
            )
            .await
            {
                delegate
                    .output_to_console(format!("Discovered Hermes inspector at {websocket_url}"));
                configuration.insert("websocketAddress".to_owned(), websocket_url.into());
                configuration.remove("port");
                configuration.remove("address");
            } else {
                anyhow::bail!(
                    "Could not discover a Hermes inspector target at http://{}:{}. \
                     Make sure Metro is running and the app is loaded on the simulator.",
                    metro_address,
                    metro_port
                );
            }
        }

        if let Some(environment) = configuration.get("env").cloned()
            && let Ok(environment) = serde_json::from_value::<HashMap<String, String>>(environment)
        {
            envs.extend(environment);
        }

        let adapter_path = if let Some(user_installed_path) = user_installed_path {
            user_installed_path
        } else {
            let adapter_directory = paths::debug_adapters_dir().join(JsDebugAdapter::ADAPTER_NAME);
            let file_name_prefix = format!("{}_", JsDebugAdapter::ADAPTER_NAME);
            util::fs::find_file_name_in_dir(adapter_directory.as_path(), |file_name| {
                file_name.starts_with(&file_name_prefix)
            })
            .await
            .context("Couldn't find the JavaScript debug adapter directory")?
            .join(JsDebugAdapter::ADAPTER_PATH)
        };

        let arguments = if let Some(mut user_args) = user_args {
            user_args.insert(0, adapter_path.to_string_lossy().into_owned());
            user_args
        } else {
            vec![
                adapter_path.to_string_lossy().into_owned(),
                port.to_string(),
                host.to_string(),
            ]
        };

        Ok(DebugAdapterBinary {
            command: Some(
                delegate
                    .node_runtime()
                    .binary_path()
                    .await?
                    .to_string_lossy()
                    .into_owned(),
            ),
            arguments,
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            envs,
            connection: Some(adapters::TcpArguments {
                host,
                port,
                timeout,
            }),
            request_args: StartDebuggingRequestArguments {
                configuration: Value::Object(configuration),
                request: self.request_kind(&task_definition.config).await?,
            },
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TargetChoice {
    Found {
        websocket_url: String,
        /// The devices of the other apps connected, when there is more than one and no device
        /// was asked for.
        other_devices: Vec<String>,
    },
    /// A device was asked for and no connected app runs on it.
    NoMatch {
        available: Vec<String>,
    },
    Empty,
}

fn target_device(target: &Value) -> String {
    target
        .get("deviceName")
        .or_else(|| target.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Picks the Hermes target to attach to from Metro's `/json/list`. With iOS and Android apps
/// connected at once the first entry is a coin toss, so the device of the Devices dock decides
/// when it is known.
fn pick_hermes_target(targets: &[Value], device_name: Option<&str>) -> TargetChoice {
    let debuggable: Vec<(&Value, String)> = targets
        .iter()
        .filter_map(|target| {
            let url = target.get("webSocketDebuggerUrl")?.as_str()?.to_owned();
            Some((target, url))
        })
        .collect();
    let Some((_, first_url)) = debuggable.first() else {
        return TargetChoice::Empty;
    };
    if let Some(device_name) = device_name {
        let wanted = device_name.to_lowercase();
        return match debuggable
            .iter()
            .find(|(target, _)| target_device(target).to_lowercase().contains(&wanted))
        {
            Some((_, url)) => TargetChoice::Found {
                websocket_url: url.clone(),
                other_devices: Vec::new(),
            },
            None => TargetChoice::NoMatch {
                available: debuggable
                    .iter()
                    .map(|(target, _)| target_device(target))
                    .collect(),
            },
        };
    }
    let mut devices: Vec<String> = debuggable
        .iter()
        .map(|(target, _)| target_device(target))
        .collect();
    devices.dedup();
    let other_devices = if devices.len() > 1 {
        devices.into_iter().skip(1).collect()
    } else {
        Vec::new()
    };
    TargetChoice::Found {
        websocket_url: first_url.clone(),
        other_devices,
    }
}

#[async_trait(?Send)]
impl DebugAdapter for ReactNativeExpoDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(Self::ADAPTER_NAME.into())
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config: json!({
                "type": "pwa-node",
                "request": "attach",
                "address": "127.0.0.1",
                "port": 8081,
                "continueOnAttach": true,
                "restart": true,
            }),
            tcp_connection: None,
        })
    }

    fn dap_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["request"],
            "properties": {
                "request": {
                    "type": "string",
                    "enum": ["attach"],
                    "description": "Attach to the Hermes inspector exposed by Metro"
                },
                "address": {
                    "type": "string",
                    "description": "Metro host serving the Hermes inspector",
                    "default": "127.0.0.1"
                },
                "port": {
                    "type": "number",
                    "description": "Metro inspector port",
                    "default": 8081
                },
                "deviceName": {
                    "type": "string",
                    "description": "Attach to the app running on this device (part of its name, e.g. \"iPhone 16\") when more than one is connected to Metro"
                },
                "websocketAddress": {
                    "type": "string",
                    "description": "Exact Hermes Inspector WebSocket URL. When omitted, vscode-js-debug discovers a target through Metro's /json/list endpoint."
                },
                "cwd": {
                    "type": "string",
                    "description": "Workspace directory used to resolve source maps"
                },
                "continueOnAttach": {
                    "type": "boolean",
                    "description": "Continue execution instead of pausing when the debugger attaches",
                    "default": true
                },
                "restart": {
                    "type": ["boolean", "object"],
                    "description": "Reconnect after Fast Refresh or an app restart",
                    "default": true
                },
                "sourceMaps": {
                    "type": "boolean",
                    "description": "Use Metro source maps",
                    "default": true
                },
                "sourceMapPathOverrides": {
                    "type": "object",
                    "description": "Rewrite source-map paths to files in the workspace",
                    "default": {}
                },
                "skipFiles": {
                    "type": "array",
                    "description": "Files to skip while stepping",
                    "items": { "type": "string" },
                    "default": ["<node_internals>/**", "**/node_modules/**"]
                },
                "trace": {
                    "type": ["boolean", "object"],
                    "description": "Enable debug adapter diagnostics",
                    "default": false
                }
            }
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        config: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        cx: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        if self.checked.set(()).is_ok() {
            delegate.output_to_console(format!(
                "Checking latest version of {}...",
                JsDebugAdapter::ADAPTER_NPM_NAME
            ));
            match JsDebugAdapter::default()
                .fetch_latest_adapter_version(delegate)
                .await
            {
                Ok(version) => {
                    adapters::download_adapter_from_github(
                        JsDebugAdapter::ADAPTER_NAME.into(),
                        version,
                        adapters::DownloadedFileType::GzipTar,
                        delegate.as_ref(),
                    )
                    .await?;
                }
                // Not knowing the latest version is not the same as being up to date: say so,
                // and carry on with whatever version is installed.
                Err(error) => {
                    log::warn!("checking the JavaScript debug adapter version: {error:#}");
                    delegate.output_to_console(format!(
                        "Could not check for a newer JavaScript debug adapter ({error:#}); using the installed one"
                    ));
                }
            }
        }

        let executor = cx.background_executor().clone();
        self.get_installed_binary(
            delegate,
            config,
            user_installed_path,
            user_args,
            user_env,
            &executor,
        )
        .await
    }

    async fn request_kind(&self, config: &Value) -> Result<StartDebuggingRequestArgumentsRequest> {
        match config.get("request").and_then(Value::as_str) {
            Some("attach") | None => Ok(StartDebuggingRequestArgumentsRequest::Attach),
            Some(request) => anyhow::bail!(
                "React Native / Expo only supports an attach request, received {request:?}"
            ),
        }
    }

    fn prefer_thread_name(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hermes_attach_requests() {
        let adapter = ReactNativeExpoDebugAdapter::default();
        let request_kind = smol::block_on(adapter.request_kind(&json!({ "request": "attach" })));

        assert!(matches!(
            request_kind,
            Ok(StartDebuggingRequestArgumentsRequest::Attach)
        ));
    }

    #[test]
    fn rejects_launch_requests() {
        let adapter = ReactNativeExpoDebugAdapter::default();
        let request_kind = smol::block_on(adapter.request_kind(&json!({ "request": "launch" })));

        assert!(request_kind.is_err());
    }

    #[test]
    fn picks_the_target_of_the_selected_device() {
        let targets = vec![
            json!({ "deviceName": "Pixel 8", "webSocketDebuggerUrl": "ws://android" }),
            json!({ "deviceName": "iPhone 16 Pro", "webSocketDebuggerUrl": "ws://ios" }),
            json!({ "deviceName": "iPhone 16 Pro" }),
        ];
        assert_eq!(
            pick_hermes_target(&targets, Some("iphone 16")),
            TargetChoice::Found {
                websocket_url: "ws://ios".into(),
                other_devices: Vec::new(),
            }
        );
        assert_eq!(
            pick_hermes_target(&targets, None),
            TargetChoice::Found {
                websocket_url: "ws://android".into(),
                other_devices: vec!["iPhone 16 Pro".into()],
            }
        );
        assert_eq!(
            pick_hermes_target(&targets, Some("Galaxy")),
            TargetChoice::NoMatch {
                available: vec!["Pixel 8".into(), "iPhone 16 Pro".into()],
            }
        );
        assert_eq!(pick_hermes_target(&[], None), TargetChoice::Empty);
    }
}
