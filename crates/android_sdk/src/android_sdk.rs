//! Self-contained Android SDK provisioning for Zed.
//!
//! Downloads a portable JDK and Google's commandline-tools, installs the
//! emulator, a system image and an AVD under Zed's data directory, and boots
//! the emulator. Everything lives under `paths::android_dir()`, so
//! uninstalling is a matter of deleting that directory.

use anyhow::{Context as _, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::channel::mpsc;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, SinkExt as _, StreamExt as _};
use gpui::{AppContext as _, AsyncApp, Context, RenderImage, SharedString, Task, WeakEntity};
use http_client::HttpClient;
use smol::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;

const CMDLINE_TOOLS_URL: &str =
    "https://dl.google.com/android/repository/commandlinetools-mac-13114758_latest.zip";
const JDK_VERSION: &str = "21.0.5+11";
const JDK_URL_AARCH64: &str = "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.5_11.tar.gz";
const JDK_URL_X64: &str = "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_x64_mac_hotspot_21.0.5_11.tar.gz";
const API_LEVEL: &str = "android-35";
const AVD_NAME: &str = "zed-android";
const AVD_DEVICE: &str = "pixel_7";
const EMULATOR_BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const ADB_POLL_INTERVAL: Duration = Duration::from_secs(2);
const DOWNLOAD_PROGRESS_GRANULARITY: u64 = 1024 * 1024;
const SCREEN_FRAME_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Android's `PixelFormat.RGBA_8888`, the only format `screencap` emits for
/// the main display.
const SCREENCAP_FORMAT_RGBA_8888: u32 = 1;
/// Half of the AVD's 1080x2400 panel. The emulator scales frames server-side
/// (preserving aspect ratio), keeping them small enough to stream, convert,
/// and upload at ~30fps.
const GRPC_STREAM_MAX_WIDTH: u32 = 540;
const GRPC_STREAM_MAX_HEIGHT: u32 = 1200;
/// After this many gRPC attempts that produced no frame, fall back to the
/// slower `screencap` streaming (e.g. stale discovery file, gRPC disabled).
const GRPC_STREAM_MAX_FAILURES: u32 = 3;

fn jdk_url() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        JDK_URL_AARCH64
    } else {
        JDK_URL_X64
    }
}

fn system_image_abi() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        "arm64-v8a"
    } else {
        "x86_64"
    }
}

fn system_image_package() -> String {
    format!("system-images;{API_LEVEL};google_apis;{}", system_image_abi())
}

#[derive(Clone)]
struct AndroidPaths {
    jdk_dir: PathBuf,
    sdk_dir: PathBuf,
    avd_home: PathBuf,
    user_home: PathBuf,
    downloads_dir: PathBuf,
}

impl AndroidPaths {
    fn new() -> Self {
        let root = paths::android_dir().clone();
        Self {
            jdk_dir: root.join("jdk"),
            sdk_dir: root.join("sdk"),
            avd_home: root.join("avd_home"),
            user_home: root.join("user_home"),
            downloads_dir: root.join("downloads"),
        }
    }

    fn jdk_version_marker(&self) -> PathBuf {
        self.jdk_dir.join(".version")
    }

    fn sdkmanager(&self) -> PathBuf {
        self.sdk_dir.join("cmdline-tools/latest/bin/sdkmanager")
    }

    fn avdmanager(&self) -> PathBuf {
        self.sdk_dir.join("cmdline-tools/latest/bin/avdmanager")
    }

    fn adb(&self) -> PathBuf {
        self.sdk_dir.join("platform-tools/adb")
    }

    fn emulator(&self) -> PathBuf {
        self.sdk_dir.join("emulator/emulator")
    }

    fn platform_dir(&self) -> PathBuf {
        self.sdk_dir.join("platforms").join(API_LEVEL)
    }

    fn system_image_dir(&self) -> PathBuf {
        self.sdk_dir
            .join("system-images")
            .join(API_LEVEL)
            .join("google_apis")
            .join(system_image_abi())
    }

    fn avd_ini(&self) -> PathBuf {
        self.avd_home.join(format!("{AVD_NAME}.ini"))
    }
}

fn apply_env(command: &mut smol::process::Command, paths: &AndroidPaths, java_home: Option<&Path>) {
    if let Some(java_home) = java_home {
        command.env("JAVA_HOME", java_home);
    }
    command
        .env("ANDROID_HOME", &paths.sdk_dir)
        .env("ANDROID_SDK_ROOT", &paths.sdk_dir)
        .env("ANDROID_AVD_HOME", &paths.avd_home)
        .env("ANDROID_USER_HOME", &paths.user_home);
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallStep {
    pub label: SharedString,
    /// `(downloaded_bytes, total_bytes)`; `total_bytes` may be zero when the
    /// server does not report a content length.
    pub progress: Option<(u64, u64)>,
}

impl InstallStep {
    fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            progress: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AndroidSdkState {
    Unknown,
    NotInstalled,
    Installing(InstallStep),
    /// SDK and emulator installed, but no AVD created yet.
    Installed,
    AvdReady,
    EmulatorBooting,
    EmulatorRunning { serial: String },
    Failed(SharedString),
}

pub struct AndroidSdkManager {
    state: AndroidSdkState,
    install_task: Option<Task<()>>,
    emulator_process: Option<smol::process::Child>,
    adb_poll_task: Option<Task<()>>,
    screen_frame: Option<Arc<RenderImage>>,
    screen_size: Option<(u32, u32)>,
    screen_stream_task: Option<Task<()>>,
    touch_sender: Option<mpsc::UnboundedSender<TouchMessage>>,
}

/// A single-finger touch event in the coordinate space of the streamed
/// screen frame. Scaled to the device's native resolution by the touch
/// worker before being sent to the emulator.
struct TouchMessage {
    x: u32,
    y: u32,
    frame_size: (u32, u32),
    pressed: bool,
}

impl AndroidSdkManager {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: AndroidSdkState::Unknown,
            install_task: None,
            emulator_process: None,
            adb_poll_task: None,
            screen_frame: None,
            screen_size: None,
            screen_stream_task: None,
            touch_sender: None,
        };
        this.detect_state(cx);
        this
    }

    pub fn state(&self) -> &AndroidSdkState {
        &self.state
    }

    /// Latest frame captured from the running emulator's screen, in BGRA.
    pub fn screen_frame(&self) -> Option<&Arc<RenderImage>> {
        self.screen_frame.as_ref()
    }

    /// Size of the emulator's screen in device pixels.
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        self.screen_size
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.detect_state(cx);
    }

    fn set_state(&mut self, state: AndroidSdkState, cx: &mut Context<Self>) {
        self.state = state;
        match &self.state {
            AndroidSdkState::EmulatorRunning { serial } => {
                self.ensure_screen_stream(serial.clone(), cx);
            }
            _ => {
                self.screen_stream_task = None;
                self.touch_sender = None;
                // The sidebar retires rendered frames from the sprite atlas;
                // dropping the Arc here is enough.
                self.screen_frame = None;
                self.screen_size = None;
            }
        }
        cx.notify();
    }

    fn detect_state(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            AndroidSdkState::Installing(_) | AndroidSdkState::EmulatorBooting
        ) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let state = cx.background_spawn(detect_state_on_disk()).await;
            this.update(cx, |this, cx| {
                if !matches!(
                    this.state,
                    AndroidSdkState::Installing(_) | AndroidSdkState::EmulatorBooting
                ) {
                    this.set_state(state, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn install(&mut self, cx: &mut Context<Self>) {
        if self.install_task.is_some() {
            return;
        }
        let http_client = cx.http_client();
        self.state = AndroidSdkState::Installing(InstallStep::new("Preparando instalação…"));
        cx.notify();
        self.install_task = Some(cx.spawn(async move |this, cx| {
            let result = run_install_pipeline(&this, http_client, cx).await;
            this.update(cx, |this, cx| {
                this.install_task = None;
                this.state = match result {
                    Ok(()) => AndroidSdkState::AvdReady,
                    Err(error) => AndroidSdkState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Creates just the AVD. Fallback used when the SDK is installed but the
    /// AVD is missing; `install()` normally chains this step itself.
    pub fn create_avd(&mut self, cx: &mut Context<Self>) {
        if self.install_task.is_some() {
            return;
        }
        self.state = AndroidSdkState::Installing(InstallStep::new("Criando dispositivo virtual…"));
        cx.notify();
        self.install_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let paths = AndroidPaths::new();
                    let java_home = installed_java_home(&paths)
                        .await
                        .context("JDK gerenciado pelo Zed não encontrado")?;
                    create_avd_on_disk(&paths, &java_home).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.install_task = None;
                this.state = match result {
                    Ok(()) => AndroidSdkState::AvdReady,
                    Err(error) => AndroidSdkState::Failed(
                        format!("Não foi possível criar o dispositivo virtual: {error:#}").into(),
                    ),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn boot_emulator(&mut self, cx: &mut Context<Self>) {
        if self.emulator_process.is_some() {
            return;
        }
        let paths = AndroidPaths::new();
        let mut command = smol::process::Command::new(paths.emulator());
        // Headless: the emulator's screen is streamed into Zed's devices view
        // instead of opening the emulator's own window.
        command.args(emulator_args(true));
        apply_env(&mut command, &paths, None);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match command.spawn() {
            Ok(child) => {
                self.emulator_process = Some(child);
                self.state = AndroidSdkState::EmulatorBooting;
                self.spawn_adb_poll(cx);
            }
            Err(error) => {
                self.state = AndroidSdkState::Failed(
                    format!("Não foi possível iniciar o emulador: {error:#}").into(),
                );
            }
        }
        cx.notify();
    }

    pub fn stop_emulator(&mut self, cx: &mut Context<Self>) {
        self.adb_poll_task = None;
        if let Some(mut child) = self.emulator_process.take() {
            child.kill().log_err();
        } else if let AndroidSdkState::EmulatorRunning { serial } = &self.state {
            // The emulator was started outside this Zed instance, so it is
            // not our child process; ask it to shut down through adb.
            let serial = serial.clone();
            cx.background_spawn(async move {
                let paths = AndroidPaths::new();
                let mut command = smol::process::Command::new(paths.adb());
                command.args(["-s", &serial, "emu", "kill"]);
                apply_env(&mut command, &paths, None);
                command.stdin(Stdio::null());
                match command.output().await {
                    Ok(output) if !output.status.success() => log::warn!(
                        "adb emu kill failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                    Ok(_) => {}
                    Err(error) => log::warn!("failed to stop emulator via adb: {error:#}"),
                }
            })
            .detach();
        }
        self.set_state(AndroidSdkState::AvdReady, cx);
    }

    /// Sends a touch down/move (`pressed`) or up event at the given
    /// frame-space coordinates through the emulator's gRPC `sendTouch`
    /// endpoint. Returns false when the gRPC input channel is unavailable,
    /// in which case callers should fall back to the adb-based
    /// [`Self::send_tap`]/[`Self::send_swipe`].
    pub fn send_touch(&mut self, x: u32, y: u32, pressed: bool) -> bool {
        let Some(frame_size) = self.screen_size else {
            return false;
        };
        let Some(sender) = &self.touch_sender else {
            return false;
        };
        let message = TouchMessage {
            x,
            y,
            frame_size,
            pressed,
        };
        if sender.unbounded_send(message).is_err() {
            // The touch worker died (for example, the gRPC connection broke);
            // the screen stream loop will recreate it on its next iteration.
            self.touch_sender = None;
            return false;
        }
        true
    }

    /// Sends a tap at the given device-pixel coordinates to the running
    /// emulator.
    pub fn send_tap(&mut self, x: u32, y: u32, cx: &mut Context<Self>) {
        self.send_input(vec!["tap".into(), x.to_string(), y.to_string()], cx);
    }

    /// Sends a swipe between the given device-pixel coordinates to the
    /// running emulator.
    pub fn send_swipe(&mut self, from: (u32, u32), to: (u32, u32), duration: Duration, cx: &mut Context<Self>) {
        self.send_input(
            vec![
                "swipe".into(),
                from.0.to_string(),
                from.1.to_string(),
                to.0.to_string(),
                to.1.to_string(),
                duration.as_millis().to_string(),
            ],
            cx,
        );
    }

    /// Sends an Android key event (for example 3 = Home, 4 = Back,
    /// 187 = Recents) to the running emulator.
    pub fn send_keyevent(&mut self, keycode: u32, cx: &mut Context<Self>) {
        self.send_input(vec!["keyevent".into(), keycode.to_string()], cx);
    }

    fn send_input(&mut self, arguments: Vec<String>, cx: &mut Context<Self>) {
        let AndroidSdkState::EmulatorRunning { serial } = &self.state else {
            return;
        };
        let serial = serial.clone();
        cx.background_spawn(async move {
            let paths = AndroidPaths::new();
            let mut command = smol::process::Command::new(paths.adb());
            command.args(["-s", &serial, "shell", "input"]);
            command.args(&arguments);
            apply_env(&mut command, &paths, None);
            command.stdin(Stdio::null());
            match command.output().await {
                Ok(output) if !output.status.success() => log::warn!(
                    "adb input {} failed: {}",
                    arguments.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                Ok(_) => {}
                Err(error) => log::warn!("failed to run adb input: {error:#}"),
            }
        })
        .detach();
    }

    fn ensure_screen_stream(&mut self, serial: String, cx: &mut Context<Self>) {
        if self.screen_stream_task.is_some() {
            return;
        }
        self.screen_stream_task = Some(cx.spawn(async move |this, cx| {
            let mut grpc_failures = 0;
            loop {
                let (frame_sender, mut frame_receiver) = mpsc::channel(1);
                let endpoint = if grpc_failures < GRPC_STREAM_MAX_FAILURES {
                    cx.background_spawn(async { discover_grpc_endpoint() }).await
                } else {
                    None
                };
                let via_grpc = endpoint.is_some();
                let touch_sender = match &endpoint {
                    Some(endpoint) => {
                        let native_size = cx
                            .background_spawn({
                                let serial = serial.clone();
                                async move { fetch_native_screen_size(&serial).await }
                            })
                            .await;
                        match native_size {
                            Ok(native_size) => {
                                let (sender, receiver) = mpsc::unbounded();
                                // Dropping the JoinHandle detaches the task; the
                                // worker exits once the sender is dropped.
                                drop(reqwest_client::runtime().spawn(run_touch_worker(
                                    endpoint.clone(),
                                    native_size,
                                    receiver,
                                )));
                                Some(sender)
                            }
                            Err(error) => {
                                log::warn!(
                                    "não foi possível obter a resolução nativa do emulador: {error:#}"
                                );
                                None
                            }
                        }
                    }
                    None => None,
                };
                if this
                    .update(cx, |this, _| this.touch_sender = touch_sender)
                    .is_err()
                {
                    return;
                }
                let reader = match endpoint {
                    Some(endpoint) => {
                        // reqwest requires a tokio runtime; use the shared one.
                        let handle = reqwest_client::runtime()
                            .spawn(stream_screen_frames_grpc(endpoint, frame_sender));
                        cx.background_spawn(async move {
                            handle.await.map_err(|error| {
                                anyhow::anyhow!("gRPC screen stream task failed: {error}")
                            })?
                        })
                    }
                    None => cx.background_spawn({
                        let serial = serial.clone();
                        async move { stream_screen_frames(&serial, frame_sender).await }
                    }),
                };
                let mut received_any_frame = false;
                while let Some((image, width, height)) = frame_receiver.next().await {
                    received_any_frame = true;
                    // Do not free the previous frame's atlas texture here: the
                    // last-painted scene may still reference it, and the Metal
                    // renderer panics on dangling atlas texture ids. The
                    // sidebar drops frames once they are two renders old.
                    let updated = this.update(cx, |this, cx| {
                        this.screen_size = Some((width, height));
                        this.screen_frame = Some(image);
                        cx.notify();
                    });
                    if updated.is_err() {
                        return;
                    }
                }
                if let Err(error) = reader.await {
                    log::warn!("android emulator screen stream failed: {error:#}");
                }
                // The emulator connection is suspect once the stream ends;
                // drop the touch worker so input falls back to adb until the
                // next successful reconnect.
                this.update(cx, |this, _| this.touch_sender = None).ok();
                if received_any_frame {
                    grpc_failures = 0;
                } else if via_grpc {
                    grpc_failures += 1;
                }
                cx.background_executor()
                    .timer(SCREEN_FRAME_RETRY_INTERVAL)
                    .await;
                let still_running = this.update(cx, |this, _| {
                    matches!(this.state, AndroidSdkState::EmulatorRunning { .. })
                });
                if !matches!(still_running, Ok(true)) {
                    return;
                }
            }
        }));
    }

    fn spawn_adb_poll(&mut self, cx: &mut Context<Self>) {
        self.adb_poll_task = Some(cx.spawn(async move |this, cx| {
            let attempts =
                (EMULATOR_BOOT_TIMEOUT.as_secs() / ADB_POLL_INTERVAL.as_secs()).max(1);
            for _ in 0..attempts {
                cx.background_executor().timer(ADB_POLL_INTERVAL).await;

                let exit_status = match this.update(cx, |this, _| {
                    this.emulator_process.as_mut().and_then(|child| {
                        match child.try_status() {
                            Ok(status) => status,
                            Err(error) => {
                                log::warn!("failed to poll emulator process status: {error}");
                                None
                            }
                        }
                    })
                }) {
                    Ok(exit_status) => exit_status,
                    Err(_) => return,
                };
                if let Some(status) = exit_status {
                    this.update(cx, |this, cx| {
                        this.emulator_process = None;
                        this.state = AndroidSdkState::Failed(
                            format!("O emulador encerrou inesperadamente ({status}).").into(),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }

                match cx
                    .background_spawn(async move {
                        let paths = AndroidPaths::new();
                        adb_devices(&paths).await
                    })
                    .await
                {
                    Ok(serials) => {
                        if let Some(serial) = serials.into_iter().next() {
                            this.update(cx, |this, cx| {
                                this.set_state(AndroidSdkState::EmulatorRunning { serial }, cx);
                            })
                            .ok();
                            return;
                        }
                    }
                    Err(error) => {
                        // adb may not be responsive while the emulator is
                        // still starting up; keep polling.
                        log::debug!("adb devices failed while waiting for emulator: {error:#}");
                    }
                }
            }

            this.update(cx, |this, cx| {
                if let Some(mut child) = this.emulator_process.take() {
                    child.kill().log_err();
                }
                this.state = AndroidSdkState::Failed(
                    "O emulador não ficou online em 120 segundos.".into(),
                );
                cx.notify();
            })
            .ok();
        }));
    }
}

fn emulator_args(headless: bool) -> Vec<String> {
    let mut args = vec!["-avd".to_string(), AVD_NAME.to_string()];
    if headless {
        args.push("-no-window".to_string());
    }
    // avdmanager creates AVDs with `hw.gpu.enabled = no`, which makes the
    // guest render in software (~12fps animations). `-gpu host` overrides the
    // AVD config and uses the host GPU.
    args.push("-gpu".to_string());
    args.push("host".to_string());
    args
}

async fn exists(path: &Path) -> bool {
    smol::fs::metadata(path).await.is_ok()
}

/// Moves an installation from the legacy location under the data directory
/// (which contains a space on macOS, breaking sdkmanager) to the current
/// `paths::android_dir()` location.
async fn migrate_legacy_android_dir() {
    let android_dir = paths::android_dir();
    let legacy_dir = paths::data_dir().join("android");
    if legacy_dir == *android_dir || exists(android_dir).await || !exists(&legacy_dir).await {
        return;
    }
    if let Some(parent) = android_dir.parent()
        && let Err(error) = smol::fs::create_dir_all(parent).await
    {
        log::warn!(
            "failed to create parent of {} for Android SDK migration: {error:#}",
            android_dir.display()
        );
        return;
    }
    match smol::fs::rename(&legacy_dir, android_dir).await {
        Ok(()) => log::info!(
            "migrated Android SDK from {} to {}",
            legacy_dir.display(),
            android_dir.display()
        ),
        Err(error) => log::warn!(
            "failed to migrate Android SDK from {} to {}: {error:#}",
            legacy_dir.display(),
            android_dir.display()
        ),
    }
}

async fn detect_state_on_disk() -> AndroidSdkState {
    migrate_legacy_android_dir().await;
    let paths = AndroidPaths::new();
    if installed_java_home(&paths).await.is_none()
        || !exists(&paths.sdkmanager()).await
        || !exists(&paths.adb()).await
        || !exists(&paths.emulator()).await
        || !exists(&paths.platform_dir()).await
        || !exists(&paths.system_image_dir()).await
    {
        return AndroidSdkState::NotInstalled;
    }
    if !exists(&paths.avd_ini()).await {
        return AndroidSdkState::Installed;
    }
    match adb_devices(&paths).await {
        Ok(serials) => match serials.into_iter().next() {
            Some(serial) => AndroidSdkState::EmulatorRunning { serial },
            None => AndroidSdkState::AvdReady,
        },
        Err(error) => {
            log::warn!("failed to query adb devices: {error:#}");
            AndroidSdkState::AvdReady
        }
    }
}

fn set_step(
    this: &WeakEntity<AndroidSdkManager>,
    cx: &mut AsyncApp,
    label: &'static str,
) -> Result<()> {
    this.update(cx, |this, cx| {
        this.state = AndroidSdkState::Installing(InstallStep::new(label));
        cx.notify();
    })
}

async fn run_install_pipeline(
    this: &WeakEntity<AndroidSdkManager>,
    http_client: Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let paths = AndroidPaths::new();

    let java_home = match cx
        .background_spawn({
            let paths = paths.clone();
            async move { installed_java_home(&paths).await }
        })
        .await
    {
        Some(java_home) => java_home,
        None => {
            set_step(this, cx, "Baixando JDK…")?;
            let archive = paths.downloads_dir.join("jdk.tar.gz");
            download_step(this, cx, http_client.clone(), jdk_url(), &archive)
                .await
                .context("Não foi possível baixar o JDK")?;
            set_step(this, cx, "Instalando JDK…")?;
            cx.background_spawn({
                let paths = paths.clone();
                let archive = archive.clone();
                async move { install_jdk(&paths, &archive).await }
            })
            .await
            .context("Não foi possível instalar o JDK")?
        }
    };

    if !exists(&paths.sdkmanager()).await {
        set_step(this, cx, "Baixando ferramentas do Android SDK…")?;
        let archive = paths.downloads_dir.join("cmdline-tools.zip");
        download_step(this, cx, http_client.clone(), CMDLINE_TOOLS_URL, &archive)
            .await
            .context("Não foi possível baixar as ferramentas do Android SDK")?;
        set_step(this, cx, "Instalando ferramentas do Android SDK…")?;
        cx.background_spawn({
            let paths = paths.clone();
            let archive = archive.clone();
            async move { install_cmdline_tools(&paths, &archive).await }
        })
        .await
        .context("Não foi possível instalar as ferramentas do Android SDK")?;
    }

    set_step(this, cx, "Aceitando licenças do Android SDK…")?;
    cx.background_spawn({
        let paths = paths.clone();
        let java_home = java_home.clone();
        async move { run_sdkmanager(&paths, &java_home, &["--licenses"]).await }
    })
    .await
    .context("Não foi possível aceitar as licenças do Android SDK")?;

    if !exists(&paths.adb()).await
        || !exists(&paths.emulator()).await
        || !exists(&paths.platform_dir()).await
    {
        set_step(this, cx, "Instalando platform-tools e emulador…")?;
        cx.background_spawn({
            let paths = paths.clone();
            let java_home = java_home.clone();
            async move {
                let platform = format!("platforms;{API_LEVEL}");
                run_sdkmanager(&paths, &java_home, &["platform-tools", "emulator", &platform])
                    .await
            }
        })
        .await
        .context("Não foi possível instalar o platform-tools e o emulador")?;
    }

    if !exists(&paths.system_image_dir()).await {
        set_step(this, cx, "Baixando imagem do sistema Android…")?;
        cx.background_spawn({
            let paths = paths.clone();
            let java_home = java_home.clone();
            async move {
                let package = system_image_package();
                run_sdkmanager(&paths, &java_home, &[&package]).await
            }
        })
        .await
        .context("Não foi possível baixar a imagem do sistema Android")?;
    }

    if !exists(&paths.avd_ini()).await {
        set_step(this, cx, "Criando dispositivo virtual…")?;
        cx.background_spawn({
            let paths = paths.clone();
            let java_home = java_home.clone();
            async move { create_avd_on_disk(&paths, &java_home).await }
        })
        .await
        .context("Não foi possível criar o dispositivo virtual")?;
    }

    Ok(())
}

/// Downloads `url` to `destination` on a background thread while forwarding
/// progress updates to the entity's `Installing` state.
async fn download_step(
    this: &WeakEntity<AndroidSdkManager>,
    cx: &mut AsyncApp,
    http_client: Arc<dyn HttpClient>,
    url: &str,
    destination: &Path,
) -> Result<()> {
    let (progress_tx, mut progress_rx) = mpsc::unbounded::<(u64, u64)>();
    let download = cx.background_spawn({
        let url = url.to_string();
        let destination = destination.to_path_buf();
        async move { download_file(http_client, &url, &destination, progress_tx).await }
    });
    let progress = cx.spawn({
        let this = this.clone();
        async move |cx| {
            while let Some((downloaded, total)) = progress_rx.next().await {
                let updated = this.update(cx, |this, cx| {
                    if let AndroidSdkState::Installing(step) = &mut this.state {
                        step.progress = Some((downloaded, total));
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
        }
    });
    let result = download.await;
    progress.await;
    result
}

async fn download_file(
    http_client: Arc<dyn HttpClient>,
    url: &str,
    destination: &Path,
    progress: mpsc::UnboundedSender<(u64, u64)>,
) -> Result<()> {
    if exists(destination).await {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        smol::fs::create_dir_all(parent).await?;
    }

    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("requisição para {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "download de {url} falhou com status {}",
        response.status()
    );
    let total = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .unwrap_or(0);

    let partial_name = destination
        .file_name()
        .map(|name| format!("{}.partial", name.to_string_lossy()))
        .context("destino do download sem nome de arquivo")?;
    let partial_path = destination.with_file_name(partial_name);

    let mut file = smol::fs::File::create(&partial_path).await?;
    let body = response.body_mut();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut downloaded = 0u64;
    let mut last_reported = 0u64;
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read]).await?;
        downloaded += bytes_read as u64;
        if downloaded - last_reported >= DOWNLOAD_PROGRESS_GRANULARITY {
            last_reported = downloaded;
            progress.unbounded_send((downloaded, total)).ok();
        }
    }
    file.flush().await?;
    drop(file);
    smol::fs::rename(&partial_path, destination).await?;
    Ok(())
}

async fn installed_java_home(paths: &AndroidPaths) -> Option<PathBuf> {
    let marker = smol::fs::read_to_string(paths.jdk_version_marker())
        .await
        .ok()?;
    if marker.trim() != JDK_VERSION {
        return None;
    }
    find_java_home(&paths.jdk_dir).await
}

async fn find_java_home(jdk_dir: &Path) -> Option<PathBuf> {
    let mut entries = smol::fs::read_dir(jdk_dir).await.ok()?;
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        let java_home = entry.path().join("Contents/Home");
        if exists(&java_home.join("bin/java")).await {
            return Some(java_home);
        }
    }
    None
}

async fn install_jdk(paths: &AndroidPaths, archive: &Path) -> Result<PathBuf> {
    if exists(&paths.jdk_dir).await {
        smol::fs::remove_dir_all(&paths.jdk_dir).await?;
    }
    smol::fs::create_dir_all(&paths.jdk_dir).await?;
    let file = smol::fs::File::open(archive).await?;
    let decompressed = GzipDecoder::new(BufReader::new(file));
    Archive::new(decompressed)
        .unpack(&paths.jdk_dir)
        .await
        .context("extração do JDK")?;
    let java_home = find_java_home(&paths.jdk_dir)
        .await
        .context("JAVA_HOME não encontrado após a extração do JDK")?;
    smol::fs::write(paths.jdk_version_marker(), JDK_VERSION).await?;
    smol::fs::remove_file(archive).await.log_err();
    Ok(java_home)
}

async fn install_cmdline_tools(paths: &AndroidPaths, archive: &Path) -> Result<()> {
    // Google's zip has a top-level `cmdline-tools/` directory, but sdkmanager
    // requires the layout `cmdline-tools/latest/`, so extract into a staging
    // directory and rename.
    let staging = paths.sdk_dir.join("cmdline-tools-staging");
    if exists(&staging).await {
        smol::fs::remove_dir_all(&staging).await?;
    }
    smol::fs::create_dir_all(&staging).await?;
    let file = smol::fs::File::open(archive).await?;
    util::archive::extract_zip(&staging, file)
        .await
        .context("extração das ferramentas do Android SDK")?;

    let latest_parent = paths.sdk_dir.join("cmdline-tools");
    smol::fs::create_dir_all(&latest_parent).await?;
    let latest = latest_parent.join("latest");
    if exists(&latest).await {
        smol::fs::remove_dir_all(&latest).await?;
    }
    smol::fs::rename(staging.join("cmdline-tools"), &latest).await?;
    smol::fs::remove_dir_all(&staging).await?;
    smol::fs::remove_file(archive).await.log_err();
    Ok(())
}

async fn run_sdkmanager(paths: &AndroidPaths, java_home: &Path, args: &[&str]) -> Result<()> {
    smol::fs::create_dir_all(&paths.avd_home).await?;
    smol::fs::create_dir_all(&paths.user_home).await?;
    let mut command = smol::process::Command::new(paths.sdkmanager());
    command.arg(format!("--sdk_root={}", paths.sdk_dir.display()));
    command.args(args);
    apply_env(&mut command, paths, Some(java_home));
    run_command_accepting_prompts(command, &"y\n".repeat(100))
        .await
        .with_context(|| format!("sdkmanager {}", args.join(" ")))
}

async fn create_avd_on_disk(paths: &AndroidPaths, java_home: &Path) -> Result<()> {
    smol::fs::create_dir_all(&paths.avd_home).await?;
    let mut command = smol::process::Command::new(paths.avdmanager());
    command.args(["create", "avd", "--name", AVD_NAME, "--package"]);
    command.arg(system_image_package());
    command.args(["--device", AVD_DEVICE, "--force"]);
    apply_env(&mut command, paths, Some(java_home));
    // avdmanager asks whether to create a custom hardware profile.
    run_command_accepting_prompts(command, "no\n")
        .await
        .context("avdmanager create avd")
}

/// Runs a command writing `input` to its stdin to answer interactive prompts
/// (for example, sdkmanager's license prompts).
async fn run_command_accepting_prompts(
    mut command: smol::process::Command,
    input: &str,
) -> Result<()> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(input.as_bytes()).await {
            // The process may exit before consuming all responses; a broken
            // pipe here is expected.
            log::debug!("failed to write prompt responses to child stdin: {error}");
        }
    }
    let mut stdout = child.stdout.take().context("stdout do processo ausente")?;
    let mut stderr = child.stderr.take().context("stderr do processo ausente")?;
    let mut stdout_content = String::new();
    let mut stderr_content = String::new();
    futures::try_join!(
        stdout.read_to_string(&mut stdout_content),
        stderr.read_to_string(&mut stderr_content)
    )?;
    let status = child.status().await?;
    if !status.success() {
        let details = if stderr_content.trim().is_empty() {
            stdout_content
        } else {
            stderr_content
        };
        anyhow::bail!("processo terminou com {status}: {}", details.trim());
    }
    Ok(())
}

struct KillOnDrop(smol::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            log::warn!("failed to kill adb screencap stream: {error}");
        }
    }
}

#[derive(Clone)]
struct EmulatorGrpcEndpoint {
    port: u16,
    token: Option<String>,
}

/// Finds the gRPC endpoint of our running AVD via the discovery files the
/// emulator writes on startup (the same mechanism Android Studio uses). Files
/// from dead emulators can linger, so the newest matching file wins and
/// callers fall back to `screencap` streaming when the endpoint is stale.
fn discover_grpc_endpoint() -> Option<EmulatorGrpcEndpoint> {
    let running_dir = paths::home_dir().join("Library/Caches/TemporaryItems/avd/running");
    let entries = std::fs::read_dir(running_dir).ok()?;
    let mut newest: Option<(std::time::SystemTime, EmulatorGrpcEndpoint)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "ini") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut avd_name = None;
        let mut port = None;
        let mut token = None;
        for line in contents.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "avd.name" => avd_name = Some(value),
                    "grpc.port" => port = value.parse::<u16>().ok(),
                    "grpc.token" => token = Some(value.to_string()),
                    _ => {}
                }
            }
        }
        if avd_name == Some(AVD_NAME)
            && let Some(port) = port
        {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest
                .as_ref()
                .is_none_or(|(newest_modified, _)| modified > *newest_modified)
            {
                newest = Some((modified, EmulatorGrpcEndpoint { port, token }));
            }
        }
    }
    newest.map(|(_, endpoint)| endpoint)
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Encodes a gRPC-framed `ImageFormat { format: RGBA8888, width, height }`
/// request for `EmulatorController.streamScreenshot`.
fn encode_stream_screenshot_request(width: u32, height: u32) -> Vec<u8> {
    let mut message = Vec::new();
    message.push(0x08); // field 1 (format), varint
    encode_varint(1, &mut message); // ImgFormat::RGBA8888
    message.push(0x18); // field 3 (width), varint
    encode_varint(width as u64, &mut message);
    message.push(0x20); // field 4 (height), varint
    encode_varint(height as u64, &mut message);
    let mut framed = vec![0u8]; // uncompressed
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);
    framed
}

struct ProtoReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> ProtoReader<'a> {
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self
                .data
                .get(self.offset)
                .context("protobuf varint truncado")?;
            self.offset += 1;
            value |= u64::from(byte & 0x7f)
                .checked_shl(shift)
                .context("protobuf varint longo demais")?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.varint()? as usize;
        let end = self
            .offset
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .context("protobuf bytes truncados")?;
        let bytes = &self.data[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn skip(&mut self, wire_type: u64) -> Result<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => self.offset += 8,
            2 => {
                self.bytes()?;
            }
            5 => self.offset += 4,
            wire_type => anyhow::bail!("wire type desconhecido no protobuf ({wire_type})"),
        }
        anyhow::ensure!(self.offset <= self.data.len(), "protobuf truncado");
        Ok(())
    }
}

/// Decodes an `Image` message from the emulator, returning its RGBA pixels
/// and actual dimensions (which reflect server-side scaling and rotation).
fn decode_image_message(data: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>> {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut image: Option<&[u8]> = None;
    let mut reader = ProtoReader { data, offset: 0 };
    while reader.offset < data.len() {
        let tag = reader.varint()?;
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                // ImageFormat
                let format = reader.bytes()?;
                let mut format_reader = ProtoReader {
                    data: format,
                    offset: 0,
                };
                while format_reader.offset < format.len() {
                    let tag = format_reader.varint()?;
                    match (tag >> 3, tag & 7) {
                        (3, 0) => width = format_reader.varint()? as u32,
                        (4, 0) => height = format_reader.varint()? as u32,
                        (_, wire_type) => format_reader.skip(wire_type)?,
                    }
                }
            }
            (4, 2) => image = Some(reader.bytes()?),
            (_, wire_type) => reader.skip(wire_type)?,
        }
    }
    match image {
        Some(image) if width > 0 && height > 0 => Ok(Some((image.to_vec(), width, height))),
        _ => Ok(None),
    }
}

/// Converts RGBA pixels from the emulator into the BGRA `RenderImage` GPUI's
/// renderer expects.
fn bgra_render_image(width: u32, height: u32, mut data: Vec<u8>) -> Result<Arc<RenderImage>> {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, data)
        .context("frame com menos pixels que o esperado")?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Queries the emulator's native display resolution via `adb shell wm size`.
/// `sendTouch` expects coordinates in this space, while the streamed frames
/// are scaled down server-side.
async fn fetch_native_screen_size(serial: &str) -> Result<(u32, u32)> {
    let paths = AndroidPaths::new();
    let mut command = smol::process::Command::new(paths.adb());
    command.args(["-s", serial, "shell", "wm", "size"]);
    apply_env(&mut command, &paths, None);
    command.stdin(Stdio::null());
    let output = command.output().await.context("adb shell wm size")?;
    anyhow::ensure!(
        output.status.success(),
        "wm size falhou: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // "Physical size: 1080x2400", possibly followed by an "Override size:"
    // line which takes precedence; the last parseable line wins.
    let mut size = None;
    for line in stdout.lines() {
        if let Some((_, value)) = line.split_once(':')
            && let Some((width, height)) = value.trim().split_once('x')
            && let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>())
        {
            size = Some((width, height));
        }
    }
    size.context("não foi possível interpretar a saída de wm size")
}

/// Encodes a gRPC-framed `TouchEvent { touches: [Touch { x, y, pressure }] }`
/// request for `EmulatorController.sendTouch`. Pressure > 0 is touch
/// down/move, 0 is touch up; the identifier is left at 0 (single finger).
fn encode_send_touch_request(x: u64, y: u64, pressure: u64) -> Vec<u8> {
    let mut touch = Vec::new();
    touch.push(0x08); // field 1 (x), varint
    encode_varint(x, &mut touch);
    touch.push(0x10); // field 2 (y), varint
    encode_varint(y, &mut touch);
    if pressure > 0 {
        touch.push(0x20); // field 4 (pressure), varint
        encode_varint(pressure, &mut touch);
    }
    let mut message = Vec::new();
    message.push(0x0a); // field 1 (touches), length-delimited
    encode_varint(touch.len() as u64, &mut message);
    message.extend_from_slice(&touch);
    let mut framed = vec![0u8]; // uncompressed
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);
    framed
}

/// Forwards touch events to the emulator's gRPC `sendTouch` endpoint over a
/// single pooled HTTP/2 connection, taking a few milliseconds per event
/// versus ~300ms for spawning `adb shell input`. Coordinates arrive in the
/// streamed frame's space and are scaled to the device's native resolution.
/// Must run on a tokio runtime (a reqwest requirement).
async fn run_touch_worker(
    endpoint: EmulatorGrpcEndpoint,
    native_size: (u32, u32),
    mut events: mpsc::UnboundedReceiver<TouchMessage>,
) {
    let client = match reqwest::Client::builder().http2_prior_knowledge().build() {
        Ok(client) => client,
        Err(error) => {
            log::warn!("não foi possível criar o cliente HTTP/2 para touch: {error:#}");
            return;
        }
    };
    let url = format!(
        "http://127.0.0.1:{}/android.emulation.control.EmulatorController/sendTouch",
        endpoint.port
    );
    while let Some(event) = events.next().await {
        let (frame_width, frame_height) = event.frame_size;
        if frame_width == 0 || frame_height == 0 {
            continue;
        }
        let x = u64::from(event.x) * u64::from(native_size.0) / u64::from(frame_width);
        let y = u64::from(event.y) * u64::from(native_size.1) / u64::from(frame_height);
        // 1024 is the maximum of Android's pressure range for touch screens.
        let pressure = if event.pressed { 1024 } else { 0 };
        if let Err(error) =
            send_touch_request(&client, &url, endpoint.token.as_deref(), x, y, pressure).await
        {
            log::warn!("gRPC sendTouch falhou: {error:#}");
            // Exiting closes the channel, so the manager falls back to adb.
            return;
        }
    }
}

async fn send_touch_request(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    x: u64,
    y: u64,
    pressure: u64,
) -> Result<()> {
    let mut request = client
        .post(url)
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(encode_send_touch_request(x, y, pressure));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .context("não foi possível conectar ao gRPC do emulador")?;
    anyhow::ensure!(
        response.status().is_success(),
        "sendTouch retornou HTTP {}",
        response.status()
    );
    // gRPC errors come back as HTTP 200 with a grpc-status header
    // (trailers-only response); success statuses live in real trailers,
    // which reqwest does not expose.
    if let Some(grpc_status) = response.headers().get("grpc-status")
        && grpc_status != "0"
    {
        let message = response
            .headers()
            .get("grpc-message")
            .and_then(|message| message.to_str().ok())
            .unwrap_or("");
        anyhow::bail!(
            "sendTouch falhou (grpc-status {}): {message}",
            String::from_utf8_lossy(grpc_status.as_bytes())
        );
    }
    response
        .bytes()
        .await
        .context("resposta do sendTouch interrompida")?;
    Ok(())
}

/// Streams frames from the emulator's gRPC `streamScreenshot` endpoint, the
/// same API Android Studio's embedded emulator window uses. The emulator only
/// sends frames when the screen changes and drops frames when the client lags
/// behind, so this is both faster (~30fps vs ~9fps for `screencap`) and idles
/// at zero cost. Must run on a tokio runtime (a reqwest requirement).
async fn stream_screen_frames_grpc(
    endpoint: EmulatorGrpcEndpoint,
    mut frames: mpsc::Sender<(Arc<RenderImage>, u32, u32)>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .context("não foi possível criar o cliente HTTP/2")?;
    let mut request = client
        .post(format!(
            "http://127.0.0.1:{}/android.emulation.control.EmulatorController/streamScreenshot",
            endpoint.port
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(encode_stream_screenshot_request(
            GRPC_STREAM_MAX_WIDTH,
            GRPC_STREAM_MAX_HEIGHT,
        ));
    if let Some(token) = &endpoint.token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .send()
        .await
        .context("não foi possível conectar ao gRPC do emulador")?;
    anyhow::ensure!(
        response.status().is_success(),
        "streamScreenshot retornou HTTP {}",
        response.status()
    );
    // gRPC errors come back as HTTP 200 with a grpc-status header
    // (trailers-only response).
    if let Some(grpc_status) = response.headers().get("grpc-status")
        && grpc_status != "0"
    {
        let message = response
            .headers()
            .get("grpc-message")
            .and_then(|message| message.to_str().ok())
            .unwrap_or("");
        anyhow::bail!(
            "streamScreenshot falhou (grpc-status {}): {message}",
            String::from_utf8_lossy(grpc_status.as_bytes())
        );
    }

    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("stream gRPC do emulador interrompido")?
    {
        buffer.extend_from_slice(&chunk);
        loop {
            if buffer.len() < 5 {
                break;
            }
            anyhow::ensure!(buffer[0] == 0, "frame gRPC comprimido não suportado");
            let length =
                u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
            if buffer.len() < 5 + length {
                break;
            }
            if let Some((data, width, height)) = decode_image_message(&buffer[5..5 + length])? {
                let image = bgra_render_image(width, height, data)?;
                if frames.send((image, width, height)).await.is_err() {
                    // Receiver dropped; the stream is no longer wanted.
                    return Ok(());
                }
            }
            buffer.drain(..5 + length);
        }
    }
    Ok(())
}

/// Fallback screen streaming for when the emulator's gRPC endpoint is not
/// reachable: frames of the emulator's screen over a single long-lived adb
/// connection, converting each raw framebuffer to a BGRA [`RenderImage`].
///
/// Spawning `adb exec-out screencap` per frame costs ~0.5s of process and
/// connection setup for each capture (~2 fps); a `while` loop over one
/// connection streams consecutive frames at ~9 fps. Backpressure from the
/// bounded channel and the adb pipe throttles the device-side loop.
async fn stream_screen_frames(
    serial: &str,
    mut frames: mpsc::Sender<(Arc<RenderImage>, u32, u32)>,
) -> Result<()> {
    let paths = AndroidPaths::new();
    let mut command = smol::process::Command::new(paths.adb());
    // `exec-out` (not `shell`) so the device shell has no pty, which would
    // mangle the binary output.
    command.args([
        "-s",
        serial,
        "exec-out",
        "while true; do screencap; done",
    ]);
    apply_env(&mut command, &paths, None);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = KillOnDrop(command.spawn()?);
    let mut stdout = child
        .0
        .stdout
        .take()
        .context("adb screencap stream sem stdout")?;

    loop {
        // Our AVD runs API 35, whose screencap header is always
        // w/h/format/colorspace (4 u32 LE values); older releases omitted the
        // colorspace field.
        let mut header = [0u8; 16];
        if stdout.read_exact(&mut header).await.is_err() {
            // Stream ended (emulator stopped or adb died).
            return Ok(());
        }
        let read_u32 = |offset: usize| -> u32 {
            let mut value = [0u8; 4];
            value.copy_from_slice(&header[offset..offset + 4]);
            u32::from_le_bytes(value)
        };
        let width = read_u32(0);
        let height = read_u32(4);
        let format = read_u32(8);
        anyhow::ensure!(
            format == SCREENCAP_FORMAT_RGBA_8888,
            "screencap retornou um formato de pixel não suportado ({format})"
        );
        let pixel_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("dimensões inválidas do screencap")?;
        let mut data = vec![0u8; pixel_bytes];
        stdout
            .read_exact(&mut data)
            .await
            .context("screencap retornou um frame truncado")?;
        let image = bgra_render_image(width, height, data)?;
        if frames.send((image, width, height)).await.is_err() {
            // Receiver dropped; the stream is no longer wanted.
            return Ok(());
        }
    }
}

async fn adb_devices(paths: &AndroidPaths) -> Result<Vec<String>> {
    let mut command = smol::process::Command::new(paths.adb());
    command.arg("devices");
    apply_env(&mut command, paths, None);
    command.stdin(Stdio::null());
    let output = command.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "adb devices falhou: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next()?;
            (serial.starts_with("emulator-") && state == "device").then(|| serial.to_string())
        })
        .collect())
}
