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
use util::ResultExt;

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
            if let Some(ws_url) = targets.iter().find_map(|target| {
                target
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }) {
                return Some(ws_url);
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
            if let Some(websocket_url) =
                Self::discover_hermes_websocket_url(delegate, &metro_address, metro_port, executor)
                    .await
            {
                delegate.output_to_console(format!(
                    "Discovered Hermes inspector at {websocket_url}"
                ));
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
            if let Some(version) = JsDebugAdapter::default()
                .fetch_latest_adapter_version(delegate)
                .await
                .log_err()
            {
                adapters::download_adapter_from_github(
                    JsDebugAdapter::ADAPTER_NAME.into(),
                    version,
                    adapters::DownloadedFileType::GzipTar,
                    delegate.as_ref(),
                )
                .await?;
            } else {
                delegate.output_to_console("JavaScript debug adapter is up to date".to_owned());
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
}
