//! Self-contained Android SDK provisioning for Zed.
//!
//! Downloads a portable JDK and Google's commandline-tools, installs the
//! emulator, a system image and an AVD under Zed's data directory, and boots
//! the emulator. Everything lives under `paths::android_dir()`, so
//! uninstalling is a matter of deleting that directory.

use anyhow::{Context as _, Result, anyhow};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::channel::mpsc;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, SinkExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Keystroke, RenderImage, SharedString, Task, WeakEntity,
};
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
const DEFAULT_AVD_DEVICE: &str = "pixel_7";
/// AVDs Zed creates are prefixed so they are recognisable in `avd_home`,
/// which is Zed's own directory and holds nothing else.
const AVD_NAME_PREFIX: &str = "zed-";
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
/// Keycodes from `android.view.KeyEvent`.
pub const KEYCODE_HOME: u32 = 3;
const KEYCODE_COPY: u32 = 278;

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
    format!(
        "system-images;{API_LEVEL};google_apis;{}",
        system_image_abi()
    )
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

    fn avd_config_ini(&self, avd_name: &str) -> PathBuf {
        self.avd_home
            .join(format!("{avd_name}.avd"))
            .join("config.ini")
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

/// A virtual device that exists under Zed's AVD home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndroidAvd {
    /// Name the emulator is launched with, e.g. `zed-pixel_7`.
    pub name: String,
    /// `hw.device.name` from the AVD's config, e.g. `pixel_7`. Absent when the
    /// config cannot be read, which only costs a nicer label.
    pub device_profile: Option<String>,
}

impl AndroidAvd {
    /// Label for the device picker: the device profile when it is known,
    /// since the AVD name is derived from it and reads worse.
    pub fn label(&self) -> String {
        match &self.device_profile {
            Some(profile) => humanize_device_profile_id(profile),
            None => self.name.clone(),
        }
    }
}

/// A device definition `avdmanager` can create an AVD from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndroidDeviceProfile {
    /// Identifier passed to `avdmanager create avd --device`.
    pub id: String,
    pub name: String,
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
    EmulatorRunning {
        serial: String,
    },
    Failed(SharedString),
}

pub struct AndroidSdkManager {
    state: AndroidSdkState,
    avds: Vec<AndroidAvd>,
    /// AVD the UI acts on. Kept pointing at an existing AVD by
    /// [`Self::apply_installation`].
    selected_avd: Option<String>,
    device_profiles: Vec<AndroidDeviceProfile>,
    device_profiles_task: Option<Task<()>>,
    install_task: Option<Task<()>>,
    boot_task: Option<Task<()>>,
    emulator_process: Option<smol::process::Child>,
    adb_poll_task: Option<Task<()>>,
    screen_frame: Option<Arc<RenderImage>>,
    screen_size: Option<(u32, u32)>,
    screen_stream_task: Option<Task<()>>,
    touch_sender: Option<mpsc::UnboundedSender<TouchMessage>>,
    key_sender: Option<mpsc::UnboundedSender<EmulatorKey>>,
}

/// What a disk scan found: which AVDs exist, and which one (if any) is
/// already running.
struct AndroidInstallation {
    state: AndroidSdkState,
    avds: Vec<AndroidAvd>,
    running_avd: Option<String>,
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

/// A key press to deliver to the emulator's keyboard. Both variants carry
/// what the gRPC `sendKey` endpoint needs plus what the adb fallback needs,
/// because the transport is only decided at send time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmulatorKey {
    /// Printable text produced by the keystroke, already resolved against the
    /// keyboard layout and modifiers (so shift-a arrives as "A").
    Text(String),
    /// A non-printable key, identified by its W3C `KeyboardEvent.key` name for
    /// gRPC and by its Android keycode for the adb fallback.
    Named { w3c: &'static str, keycode: u32 },
}

/// Translates a GPUI keystroke into the key to deliver to the emulator, or
/// `None` when the keystroke is a Zed shortcut that should not be forwarded.
///
/// `command`/`control` combinations are left to Zed so that the surrounding
/// editor keybindings keep working; `alt` is forwarded when it produced a
/// character, since that is how layouts such as ABNT reach `ç` and friends.
pub fn emulator_key_for_keystroke(keystroke: &Keystroke) -> Option<EmulatorKey> {
    if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.function {
        return None;
    }

    // Android keycodes from `android.view.KeyEvent`, used by the adb fallback.
    let named = match keystroke.key.as_str() {
        "backspace" => Some(("Backspace", 67)),
        "enter" => Some(("Enter", 66)),
        "tab" => Some(("Tab", 61)),
        // Matches Android Studio's embedded emulator, where Esc is the Back button.
        "escape" => Some(("GoBack", 4)),
        "up" => Some(("ArrowUp", 19)),
        "down" => Some(("ArrowDown", 20)),
        "left" => Some(("ArrowLeft", 21)),
        "right" => Some(("ArrowRight", 22)),
        "home" => Some(("Home", 122)),
        "end" => Some(("End", 123)),
        "pageup" => Some(("PageUp", 92)),
        "pagedown" => Some(("PageDown", 93)),
        "delete" => Some(("Delete", 112)),
        _ => None,
    };
    if let Some((w3c, keycode)) = named {
        return Some(EmulatorKey::Named { w3c, keycode });
    }

    // Everything else is only forwarded when the keystroke produced text.
    // Control characters would be interpreted as evdev codes by the emulator.
    let key_char = keystroke.key_char.as_deref()?;
    if key_char.is_empty() || key_char.chars().any(|character| character.is_control()) {
        return None;
    }
    Some(EmulatorKey::Text(key_char.to_string()))
}

impl AndroidSdkManager {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: AndroidSdkState::Unknown,
            avds: Vec::new(),
            selected_avd: None,
            device_profiles: Vec::new(),
            device_profiles_task: None,
            install_task: None,
            boot_task: None,
            emulator_process: None,
            adb_poll_task: None,
            screen_frame: None,
            screen_size: None,
            screen_stream_task: None,
            touch_sender: None,
            key_sender: None,
        };
        this.detect_state(cx);
        this
    }

    pub fn state(&self) -> &AndroidSdkState {
        &self.state
    }

    /// Virtual devices available to boot, in the order they should be listed.
    pub fn avds(&self) -> &[AndroidAvd] {
        &self.avds
    }

    /// AVD the boot/stop controls act on.
    pub fn selected_avd(&self) -> Option<&str> {
        self.selected_avd.as_deref()
    }

    /// Device definitions a new AVD can be created from. Empty until
    /// [`Self::refresh_device_profiles`] has run.
    pub fn device_profiles(&self) -> &[AndroidDeviceProfile] {
        &self.device_profiles
    }

    pub fn is_loading_device_profiles(&self) -> bool {
        self.device_profiles_task.is_some()
    }

    /// Switches the emulator to `name`. A different AVD cannot share the
    /// running emulator, so the current one is shut down and the new one
    /// booted in its place.
    pub fn select_avd(&mut self, name: String, cx: &mut Context<Self>) {
        if self.selected_avd.as_deref() == Some(name.as_str()) {
            return;
        }
        if !self.avds.iter().any(|avd| avd.name == name) {
            return;
        }
        let was_running = matches!(
            self.state,
            AndroidSdkState::EmulatorRunning { .. } | AndroidSdkState::EmulatorBooting
        );
        if was_running {
            self.stop_emulator(cx);
        }
        self.selected_avd = Some(name);
        cx.notify();
        if was_running {
            self.boot_emulator(cx);
        }
    }

    /// Reads the device definitions `avdmanager` knows about. Requires the
    /// managed JDK, so it does nothing until the SDK is installed.
    pub fn refresh_device_profiles(&mut self, cx: &mut Context<Self>) {
        if self.device_profiles_task.is_some() || !self.device_profiles.is_empty() {
            return;
        }
        self.device_profiles_task = Some(cx.spawn(async move |this, cx| {
            let profiles = cx
                .background_spawn(async {
                    let paths = AndroidPaths::new();
                    let java_home = installed_java_home(&paths)
                        .await
                        .context("JDK gerenciado pelo Zed não encontrado")?;
                    list_device_profiles(&paths, &java_home).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.device_profiles_task = None;
                match profiles {
                    Ok(profiles) => this.device_profiles = profiles,
                    Err(error) => {
                        log::warn!("não foi possível listar os perfis de device: {error:#}")
                    }
                }
                cx.notify();
            })
            .ok();
        }));
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
                let serial = serial.clone();
                let avd_name = self.selected_avd.clone();
                self.ensure_screen_stream(serial, avd_name, cx);
            }
            _ => {
                self.screen_stream_task = None;
                self.touch_sender = None;
                self.key_sender = None;
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
            let installation = cx.background_spawn(detect_installation_on_disk()).await;
            this.update(cx, |this, cx| {
                if !matches!(
                    this.state,
                    AndroidSdkState::Installing(_) | AndroidSdkState::EmulatorBooting
                ) {
                    this.apply_installation(installation, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Adopts a disk scan, keeping the selection pointed at an AVD that still
    /// exists and preferring whichever one is already running.
    fn apply_installation(&mut self, installation: AndroidInstallation, cx: &mut Context<Self>) {
        self.avds = installation.avds;
        let selection_is_gone = self
            .selected_avd
            .as_ref()
            .is_none_or(|selected| !self.avds.iter().any(|avd| &avd.name == selected));
        if selection_is_gone {
            self.selected_avd = installation
                .running_avd
                .filter(|running| self.avds.iter().any(|avd| &avd.name == running))
                .or_else(|| self.avds.first().map(|avd| avd.name.clone()));
        }
        // The device definitions come from `avdmanager`, which only exists
        // once the SDK is installed; this is the first point where that is
        // known, and the picker needs the list before the user opens it.
        if !matches!(
            installation.state,
            AndroidSdkState::Unknown | AndroidSdkState::NotInstalled
        ) {
            self.refresh_device_profiles(cx);
        }
        self.set_state(installation.state, cx);
    }

    /// Rescans the AVDs on disk without disturbing a running emulator.
    fn refresh_avds(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let avds = cx
                .background_spawn(async { list_avds(&AndroidPaths::new()).await })
                .await;
            this.update(cx, |this, cx| {
                this.avds = avds;
                let selection_is_gone = this
                    .selected_avd
                    .as_ref()
                    .is_none_or(|selected| !this.avds.iter().any(|avd| &avd.name == selected));
                if selection_is_gone {
                    this.selected_avd = this.avds.first().map(|avd| avd.name.clone());
                }
                cx.notify();
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
                match result {
                    Ok(()) => {
                        this.state = AndroidSdkState::AvdReady;
                        this.refresh_avds(cx);
                        this.refresh_device_profiles(cx);
                    }
                    Err(error) => {
                        this.state = AndroidSdkState::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Creates an AVD for the default device profile. Fallback used when the
    /// SDK is installed but no AVD exists; `install()` chains this itself.
    pub fn create_avd(&mut self, cx: &mut Context<Self>) {
        self.create_avd_for_device(DEFAULT_AVD_DEVICE.to_string(), cx);
    }

    /// Creates an AVD for `device_profile` and makes it the selection. A
    /// running emulator belongs to another AVD, so it is shut down first.
    pub fn create_avd_for_device(&mut self, device_profile: String, cx: &mut Context<Self>) {
        if self.install_task.is_some() {
            return;
        }
        // One AVD per profile: picking a device that already has one just
        // switches to it, so the list never grows duplicates.
        if let Some(existing) = self
            .avds
            .iter()
            .find(|avd| avd.device_profile.as_deref() == Some(device_profile.as_str()))
            .map(|avd| avd.name.clone())
        {
            self.select_avd(existing, cx);
            return;
        }
        if matches!(
            self.state,
            AndroidSdkState::EmulatorRunning { .. } | AndroidSdkState::EmulatorBooting
        ) {
            self.stop_emulator(cx);
        }
        let avd_name = avd_name_for_device(&device_profile);
        self.state = AndroidSdkState::Installing(InstallStep::new("Criando dispositivo virtual…"));
        cx.notify();
        self.install_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let avd_name = avd_name.clone();
                    async move {
                        let paths = AndroidPaths::new();
                        let java_home = installed_java_home(&paths)
                            .await
                            .context("JDK gerenciado pelo Zed não encontrado")?;
                        create_avd_on_disk(&paths, &java_home, &avd_name, &device_profile).await
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.install_task = None;
                match result {
                    Ok(()) => {
                        this.selected_avd = Some(avd_name);
                        this.state = AndroidSdkState::AvdReady;
                        this.refresh_avds(cx);
                    }
                    Err(error) => {
                        this.state = AndroidSdkState::Failed(
                            format!("Não foi possível criar o dispositivo virtual: {error:#}")
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn boot_emulator(&mut self, cx: &mut Context<Self>) {
        if self.emulator_process.is_some() || matches!(self.state, AndroidSdkState::EmulatorBooting)
        {
            return;
        }
        let Some(avd_name) = self.selected_avd.clone() else {
            self.state =
                AndroidSdkState::Failed("Nenhum dispositivo virtual selecionado.".into());
            cx.notify();
            return;
        };
        // Marking the state before spawning keeps a second call from launching
        // a second emulator while the config below is being repaired.
        self.state = AndroidSdkState::EmulatorBooting;
        cx.notify();
        self.boot_task = Some(cx.spawn(async move |this, cx| {
            // Repairs AVDs created before the hardware keyboard was enabled.
            // Must happen before the emulator starts, which is the only time
            // the setting is read.
            cx.background_spawn({
                let avd_name = avd_name.clone();
                async move {
                    enable_avd_hardware_keyboard(&AndroidPaths::new(), &avd_name)
                        .await
                        .context("não foi possível habilitar o teclado físico do AVD")
                        .log_err();
                }
            })
            .await;
            this.update(cx, |this, cx| this.spawn_emulator_process(avd_name, cx))
                .ok();
        }));
    }

    fn spawn_emulator_process(&mut self, avd_name: String, cx: &mut Context<Self>) {
        let paths = AndroidPaths::new();
        let mut command = smol::process::Command::new(paths.emulator());
        // Headless: the emulator's screen is streamed into Zed's devices view
        // instead of opening the emulator's own window.
        command.args(emulator_args(&avd_name, true));
        apply_env(&mut command, &paths, None);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match command.spawn() {
            Ok(child) => {
                self.emulator_process = Some(child);
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
        self.boot_task = None;
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
    pub fn send_swipe(
        &mut self,
        from: (u32, u32),
        to: (u32, u32),
        duration: Duration,
        cx: &mut Context<Self>,
    ) {
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

    /// Delivers a keystroke to the emulator's keyboard, preferring the gRPC
    /// `sendKey` endpoint and falling back to `adb shell input` when it is
    /// unavailable. The fallback takes ~300ms per key, which is too slow to
    /// type with, so it exists only to keep the keyboard working at all.
    pub fn send_key(&mut self, key: EmulatorKey, cx: &mut Context<Self>) {
        if let Some(sender) = &self.key_sender {
            if sender.unbounded_send(key.clone()).is_ok() {
                return;
            }
            // The key worker died (for example, the gRPC connection broke);
            // the screen stream loop recreates it on its next iteration.
            self.key_sender = None;
        }
        match key {
            EmulatorKey::Named { keycode, .. } => self.send_keyevent(keycode, cx),
            EmulatorKey::Text(text) => {
                self.send_input(vec!["text".into(), escape_adb_text(&text)], cx)
            }
        }
    }

    /// The serial of the running emulator, if there is one.
    pub fn running_serial(&self) -> Option<&str> {
        match &self.state {
            AndroidSdkState::EmulatorRunning { serial } => Some(serial),
            _ => None,
        }
    }

    /// Captures the emulator screen to a PNG file and resolves with its path.
    pub fn screenshot(&self, cx: &App) -> Task<Result<PathBuf>> {
        let Some(serial) = self.running_serial().map(str::to_string) else {
            return Task::ready(Err(anyhow!("nenhum emulador Android em execução")));
        };
        cx.background_spawn(async move {
            // `exec-out` keeps the PNG bytes binary-clean, unlike `shell`, which would
            // translate line endings.
            let png = run_adb(&serial, &["exec-out", "screencap", "-p"]).await?;
            anyhow::ensure!(!png.is_empty(), "screencap não retornou nenhuma imagem");
            let path = std::env::temp_dir().join("zed_emulator_screenshot.png");
            smol::fs::write(&path, png)
                .await
                .with_context(|| format!("não foi possível gravar {}", path.display()))?;
            Ok(path)
        })
    }

    /// Force-stops and relaunches the given package's launcher activity.
    pub fn launch_package(&self, package: String, cx: &App) -> Task<Result<()>> {
        let Some(serial) = self.running_serial().map(str::to_string) else {
            return Task::ready(Err(anyhow!("nenhum emulador Android em execução")));
        };
        cx.background_spawn(async move {
            // Stopping first is best-effort: the app may simply not be running.
            run_adb(&serial, &["shell", "am", "force-stop", &package])
                .await
                .log_err();
            run_adb(
                &serial,
                &[
                    "shell",
                    "monkey",
                    "-p",
                    &package,
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1",
                ],
            )
            .await?;
            Ok(())
        })
    }

    /// Moves the emulator's simulated GPS position.
    pub fn set_location(&self, latitude: f64, longitude: f64, cx: &App) -> Task<Result<()>> {
        let Some(serial) = self.running_serial().map(str::to_string) else {
            return Task::ready(Err(anyhow!("nenhum emulador Android em execução")));
        };
        cx.background_spawn(async move {
            // `geo fix` takes longitude before latitude, unlike every other API here.
            // `adb emu` resolves the emulator console auth token on its own.
            run_adb(
                &serial,
                &[
                    "emu",
                    "geo",
                    "fix",
                    &longitude.to_string(),
                    &latitude.to_string(),
                ],
            )
            .await?;
            Ok(())
        })
    }

    /// Android hides the on-screen IME while a hardware keyboard is attached, and the host
    /// keyboard the emulator forwards always counts as one. This secure setting brings the
    /// IME back without giving up physical typing, which is what makes it a usable toggle
    /// rather than a trade.
    pub fn set_software_keyboard_visible(&self, visible: bool, cx: &App) -> Task<Result<()>> {
        let Some(serial) = self.running_serial().map(str::to_string) else {
            return Task::ready(Err(anyhow!("nenhum emulador Android em execução")));
        };
        cx.background_spawn(async move {
            run_adb(
                &serial,
                &[
                    "shell",
                    "settings",
                    "put",
                    "secure",
                    "show_ime_with_hard_keyboard",
                    if visible { "1" } else { "0" },
                ],
            )
            .await?;
            Ok(())
        })
    }

    /// Types `text` into whatever the device has focused, which is what pasting into the
    /// device looks like from the host's side.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.send_input(vec!["text".into(), escape_adb_text(text)], cx);
    }

    /// Copies on the device and resolves with the device clipboard so the host can mirror
    /// it. Reading the clipboard needs `cmd clipboard`, which Android denies to a shell in
    /// the background on API 29+, so the read half is best-effort and resolves to `None`.
    pub fn copy_from_device(&mut self, cx: &mut Context<Self>) -> Task<Option<String>> {
        self.send_keyevent(KEYCODE_COPY, cx);
        let Some(serial) = self.running_serial().map(str::to_string) else {
            return Task::ready(None);
        };
        cx.background_spawn(async move {
            match run_adb(&serial, &["shell", "cmd", "clipboard", "get-text"]).await {
                Ok(stdout) => {
                    let text = String::from_utf8_lossy(&stdout).trim_end().to_string();
                    (!text.is_empty()).then_some(text)
                }
                Err(error) => {
                    log::warn!("não foi possível ler o clipboard do device: {error:#}");
                    None
                }
            }
        })
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

    fn ensure_screen_stream(
        &mut self,
        serial: String,
        avd_name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.screen_stream_task.is_some() {
            return;
        }
        self.screen_stream_task = Some(cx.spawn(async move |this, cx| {
            let mut grpc_failures = 0;
            loop {
                let (frame_sender, mut frame_receiver) = mpsc::channel(1);
                let endpoint = if grpc_failures < GRPC_STREAM_MAX_FAILURES {
                    let avd_name = avd_name.clone();
                    cx.background_spawn(async move {
                        discover_grpc_endpoint(avd_name.as_deref())
                    })
                    .await
                } else {
                    None
                };
                let via_grpc = endpoint.is_some();
                let key_sender = endpoint.as_ref().map(|endpoint| {
                    let (sender, receiver) = mpsc::unbounded();
                    // Dropping the JoinHandle detaches the task; the worker
                    // exits once the sender is dropped.
                    drop(
                        reqwest_client::runtime()
                            .spawn(run_key_worker(endpoint.clone(), receiver)),
                    );
                    sender
                });
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
                    .update(cx, |this, _| {
                        this.touch_sender = touch_sender;
                        this.key_sender = key_sender;
                    })
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
                this.update(cx, |this, _| {
                    this.touch_sender = None;
                    this.key_sender = None;
                })
                .ok();
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
            let attempts = (EMULATOR_BOOT_TIMEOUT.as_secs() / ADB_POLL_INTERVAL.as_secs()).max(1);
            for _ in 0..attempts {
                cx.background_executor().timer(ADB_POLL_INTERVAL).await;

                let exit_status = match this.update(cx, |this, _| {
                    this.emulator_process
                        .as_mut()
                        .and_then(|child| match child.try_status() {
                            Ok(status) => status,
                            Err(error) => {
                                log::warn!("failed to poll emulator process status: {error}");
                                None
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
                this.state =
                    AndroidSdkState::Failed("O emulador não ficou online em 120 segundos.".into());
                cx.notify();
            })
            .ok();
        }));
    }
}

fn emulator_args(avd_name: &str, headless: bool) -> Vec<String> {
    let mut args = vec!["-avd".to_string(), avd_name.to_string()];
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

async fn detect_installation_on_disk() -> AndroidInstallation {
    migrate_legacy_android_dir().await;
    let paths = AndroidPaths::new();
    if installed_java_home(&paths).await.is_none()
        || !exists(&paths.sdkmanager()).await
        || !exists(&paths.adb()).await
        || !exists(&paths.emulator()).await
        || !exists(&paths.platform_dir()).await
        || !exists(&paths.system_image_dir()).await
    {
        return AndroidInstallation {
            state: AndroidSdkState::NotInstalled,
            avds: Vec::new(),
            running_avd: None,
        };
    }

    let avds = list_avds(&paths).await;
    if avds.is_empty() {
        return AndroidInstallation {
            state: AndroidSdkState::Installed,
            avds,
            running_avd: None,
        };
    }

    let serial = match adb_devices(&paths).await {
        Ok(serials) => serials.into_iter().next(),
        Err(error) => {
            log::warn!("failed to query adb devices: {error:#}");
            None
        }
    };
    let Some(serial) = serial else {
        return AndroidInstallation {
            state: AndroidSdkState::AvdReady,
            avds,
            running_avd: None,
        };
    };
    let running_avd = adb_avd_name(&paths, &serial).await;
    AndroidInstallation {
        state: AndroidSdkState::EmulatorRunning { serial },
        avds,
        running_avd,
    }
}

/// Lists the AVDs under Zed's AVD home by reading the `<name>.ini` markers
/// `avdmanager` writes, which is both faster and less fragile than parsing
/// `avdmanager list avd` (and needs no JVM).
async fn list_avds(paths: &AndroidPaths) -> Vec<AndroidAvd> {
    let Ok(mut entries) = smol::fs::read_dir(&paths.avd_home).await else {
        return Vec::new();
    };
    let mut avds = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "ini") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // `.ini` markers are only meaningful with their `.avd` payload; a
        // half-deleted AVD would otherwise show up as bootable.
        if !exists(&paths.avd_config_ini(name)).await {
            continue;
        }
        avds.push(AndroidAvd {
            name: name.to_string(),
            device_profile: read_avd_device_profile(paths, name).await,
        });
    }
    avds.sort_by_key(|avd| avd.label());
    avds
}

async fn read_avd_device_profile(paths: &AndroidPaths, avd_name: &str) -> Option<String> {
    let config = smol::fs::read_to_string(paths.avd_config_ini(avd_name))
        .await
        .ok()?;
    config.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "hw.device.name").then(|| value.trim().to_string())
    })
}

/// Asks the emulator console which AVD a running instance was launched with,
/// so a pre-existing emulator can be matched to an entry in the device list.
async fn adb_avd_name(paths: &AndroidPaths, serial: &str) -> Option<String> {
    let mut command = smol::process::Command::new(paths.adb());
    command.args(["-s", serial, "emu", "avd", "name"]);
    apply_env(&mut command, paths, None);
    command.stdin(Stdio::null());
    let output = command.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    // The console echoes the name followed by an `OK` acknowledgement.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "OK")
        .map(str::to_string)
}

/// Reads the device definitions `avdmanager` can create AVDs from, keeping
/// only the handheld and tablet ones. The tagged definitions (Wear, TV,
/// Automotive, Desktop, XR) need their own system images, and Zed only
/// installs the `google_apis` phone image.
async fn list_device_profiles(
    paths: &AndroidPaths,
    java_home: &Path,
) -> Result<Vec<AndroidDeviceProfile>> {
    let mut command = smol::process::Command::new(paths.avdmanager());
    command.args(["list", "device"]);
    apply_env(&mut command, paths, Some(java_home));
    command.stdin(Stdio::null());
    let output = command.output().await.context("avdmanager list device")?;
    anyhow::ensure!(
        output.status.success(),
        "avdmanager list device falhou: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_device_profiles(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parses `avdmanager list device` output, whose records look like
///
/// ```text
/// id: 39 or "pixel_7"
///     Name: Pixel 7
///     OEM : Google
///     Tag : android-wear
/// ---------
/// ```
fn parse_device_profiles(output: &str) -> Vec<AndroidDeviceProfile> {
    let mut profiles = Vec::new();
    let mut id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut tagged = false;

    let mut flush = |id: &mut Option<String>, name: &mut Option<String>, tagged: &mut bool| {
        let (Some(id), Some(name)) = (id.take(), name.take()) else {
            *tagged = false;
            return;
        };
        if !*tagged {
            profiles.push(AndroidDeviceProfile { id, name });
        }
        *tagged = false;
    };

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---") {
            flush(&mut id, &mut name, &mut tagged);
        } else if let Some(rest) = trimmed.strip_prefix("id:") {
            flush(&mut id, &mut name, &mut tagged);
            id = rest
                .split_once('"')
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(identifier, _)| identifier.to_string());
        } else if let Some(rest) = trimmed.strip_prefix("Name:") {
            name = Some(rest.trim().to_string());
        } else if trimmed.starts_with("Tag ") || trimmed.starts_with("Tag:") {
            tagged = true;
        }
    }
    flush(&mut id, &mut name, &mut tagged);
    profiles
}

/// Name for the AVD backing `device_profile`. One AVD per profile keeps the
/// device list free of duplicates that only differ by a counter.
fn avd_name_for_device(device_profile: &str) -> String {
    let sanitized: String = device_profile
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("{AVD_NAME_PREFIX}{sanitized}")
}

/// Turns a device profile id such as `pixel_9_pro` into `Pixel 9 Pro`. The
/// profile list carries proper names, but AVDs on disk only remember the id.
fn humanize_device_profile_id(id: &str) -> String {
    let mut label = String::with_capacity(id.len());
    for word in id.split(['_', '-']).filter(|word| !word.is_empty()) {
        if !label.is_empty() {
            label.push(' ');
        }
        let mut characters = word.chars();
        match characters.next() {
            Some(first) => {
                label.extend(first.to_uppercase());
                label.push_str(characters.as_str());
            }
            None => {}
        }
    }
    if label.is_empty() {
        id.to_string()
    } else {
        label
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
                run_sdkmanager(
                    &paths,
                    &java_home,
                    &["platform-tools", "emulator", &platform],
                )
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

    if list_avds(&paths).await.is_empty() {
        set_step(this, cx, "Criando dispositivo virtual…")?;
        cx.background_spawn({
            let paths = paths.clone();
            let java_home = java_home.clone();
            async move {
                let avd_name = avd_name_for_device(DEFAULT_AVD_DEVICE);
                create_avd_on_disk(&paths, &java_home, &avd_name, DEFAULT_AVD_DEVICE).await
            }
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

async fn create_avd_on_disk(
    paths: &AndroidPaths,
    java_home: &Path,
    avd_name: &str,
    device_profile: &str,
) -> Result<()> {
    smol::fs::create_dir_all(&paths.avd_home).await?;
    let mut command = smol::process::Command::new(paths.avdmanager());
    command.args(["create", "avd", "--name", avd_name, "--package"]);
    command.arg(system_image_package());
    command.args(["--device", device_profile, "--force"]);
    apply_env(&mut command, paths, Some(java_home));
    // avdmanager asks whether to create a custom hardware profile.
    run_command_accepting_prompts(command, "no\n")
        .await
        .context("avdmanager create avd")?;
    enable_avd_hardware_keyboard(paths, avd_name).await
}

/// avdmanager creates AVDs with `hw.keyboard = no`, which leaves the guest
/// without a keyboard input device, so every key forwarded from the host is
/// silently dropped and only the on-screen keyboard works. The emulator reads
/// this only at boot and has no command line override (unlike `hw.gpu.enabled`,
/// which `-gpu host` overrides), so the AVD config is rewritten in place.
async fn enable_avd_hardware_keyboard(paths: &AndroidPaths, avd_name: &str) -> Result<()> {
    const SETTING: &str = "hw.keyboard";

    let config_path = paths.avd_config_ini(avd_name);
    let config = match smol::fs::read_to_string(&config_path).await {
        Ok(config) => config,
        // The AVD has not been created yet; creating it sets the value.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("lendo {}", config_path.display()));
        }
    };

    fn setting_value(line: &str) -> Option<&str> {
        let (key, value) = line.split_once('=')?;
        (key.trim() == SETTING).then(|| value.trim())
    }

    if config
        .lines()
        .any(|line| setting_value(line) == Some("yes"))
    {
        return Ok(());
    }

    let mut updated = String::with_capacity(config.len() + SETTING.len() + 8);
    let mut replaced = false;
    for line in config.lines() {
        if setting_value(line).is_some() {
            updated.push_str("hw.keyboard = yes");
            replaced = true;
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }
    if !replaced {
        updated.push_str("hw.keyboard = yes\n");
    }

    smol::fs::write(&config_path, updated)
        .await
        .with_context(|| format!("gravando {}", config_path.display()))
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
fn discover_grpc_endpoint(wanted_avd: Option<&str>) -> Option<EmulatorGrpcEndpoint> {
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
        if avd_name == wanted_avd
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
        let body = encode_send_touch_request(x, y, pressure);
        if let Err(error) =
            send_unary_grpc(&client, &url, endpoint.token.as_deref(), "sendTouch", body).await
        {
            log::warn!("gRPC sendTouch falhou: {error:#}");
            // Exiting closes the channel, so the manager falls back to adb.
            return;
        }
    }
}

/// `adb shell input text` is parsed twice before reaching the keyboard: once
/// by the device's shell, and once by `input`, which reads `%s` as a space.
fn escape_adb_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            ' ' => escaped.push_str("%s"),
            '\\' | '"' | '\'' | '`' | '$' | '&' | '|' | ';' | '<' | '>' | '(' | ')' | '*' | '?'
            | '[' | ']' | '{' | '}' | '~' | '#' | '!' | '%' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Encodes a gRPC-framed `KeyboardEvent` request for
/// `EmulatorController.sendKey`. `key` (field 4) takes priority over `keyCode`
/// and `text`, and the emulator expands a printable character into the evdev
/// sequence a shifted character needs. `eventType` must be `keypress` so that
/// named keys such as `Backspace` are also released, not just pressed.
fn encode_send_key_request(key: &str) -> Vec<u8> {
    let mut message = Vec::new();
    message.push(0x10); // field 2 (eventType), varint
    message.push(0x02); // KeyEventType::keypress
    message.push(0x22); // field 4 (key), length-delimited
    encode_varint(key.len() as u64, &mut message);
    message.extend_from_slice(key.as_bytes());
    let mut framed = vec![0u8]; // uncompressed
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);
    framed
}

/// Forwards keystrokes to the emulator's gRPC `sendKey` endpoint, the same
/// path Android Studio's embedded emulator uses. Spawning `adb shell input`
/// per key takes ~300ms, which is far too slow to type with.
/// Must run on a tokio runtime (a reqwest requirement).
async fn run_key_worker(
    endpoint: EmulatorGrpcEndpoint,
    mut events: mpsc::UnboundedReceiver<EmulatorKey>,
) {
    let client = match reqwest::Client::builder().http2_prior_knowledge().build() {
        Ok(client) => client,
        Err(error) => {
            log::warn!("não foi possível criar o cliente HTTP/2 para teclado: {error:#}");
            return;
        }
    };
    let url = format!(
        "http://127.0.0.1:{}/android.emulation.control.EmulatorController/sendKey",
        endpoint.port
    );
    while let Some(event) = events.next().await {
        // The emulator translates a multi-character `key` as a single named
        // key, so text is sent one character at a time.
        let keys: Vec<String> = match &event {
            EmulatorKey::Named { w3c, .. } => vec![(*w3c).to_string()],
            EmulatorKey::Text(text) => text.chars().map(String::from).collect(),
        };
        for key in keys {
            let body = encode_send_key_request(&key);
            if let Err(error) =
                send_unary_grpc(&client, &url, endpoint.token.as_deref(), "sendKey", body).await
            {
                log::warn!("gRPC sendKey falhou: {error:#}");
                // Exiting closes the channel, so the manager falls back to adb.
                return;
            }
        }
    }
}

/// Performs a unary gRPC call whose response body is ignored. `method` is only
/// used for error messages.
async fn send_unary_grpc(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    method: &str,
    body: Vec<u8>,
) -> Result<()> {
    let mut request = client
        .post(url)
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(body);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .context("não foi possível conectar ao gRPC do emulador")?;
    anyhow::ensure!(
        response.status().is_success(),
        "{method} retornou HTTP {}",
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
            "{method} falhou (grpc-status {}): {message}",
            String::from_utf8_lossy(grpc_status.as_bytes())
        );
    }
    response
        .bytes()
        .await
        .with_context(|| format!("resposta do {method} interrompida"))?;
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
            let length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
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
    command.args(["-s", serial, "exec-out", "while true; do screencap; done"]);
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

/// Runs `adb -s <serial> <arguments>` and returns its stdout.
async fn run_adb(serial: &str, arguments: &[&str]) -> Result<Vec<u8>> {
    let paths = AndroidPaths::new();
    let mut command = smol::process::Command::new(paths.adb());
    command.arg("-s").arg(serial).args(arguments);
    apply_env(&mut command, &paths, None);
    command.stdin(Stdio::null());
    let output = command
        .output()
        .await
        .with_context(|| format!("não foi possível executar adb {}", arguments.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "adb {} falhou: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim records from `avdmanager list device` (cmdline-tools 13114758).
    const DEVICE_LIST: &str = r#"Available devices definitions:
id: 0 or "automotive_1024p_landscape"
    Name: Automotive (1024p landscape)
    OEM : Google
    Tag : android-automotive-playstore
---------
id: 9 or "Galaxy Nexus"
    Name: Galaxy Nexus
    OEM : Google
---------
id: 39 or "pixel_7"
    Name: Pixel 7
    OEM : Google
---------
id: 51 or "pixel_tablet"
    Name: Pixel Tablet
    OEM : Google
---------
id: 60 or "wearos_square"
    Name: Wear OS Square
    OEM : Google
    Tag : android-wear
---------
"#;

    #[test]
    fn parses_handheld_device_profiles_and_skips_tagged_ones() {
        let profiles = parse_device_profiles(DEVICE_LIST);

        assert_eq!(
            profiles,
            vec![
                AndroidDeviceProfile {
                    id: "Galaxy Nexus".into(),
                    name: "Galaxy Nexus".into(),
                },
                AndroidDeviceProfile {
                    id: "pixel_7".into(),
                    name: "Pixel 7".into(),
                },
                AndroidDeviceProfile {
                    id: "pixel_tablet".into(),
                    name: "Pixel Tablet".into(),
                },
            ]
        );
    }

    #[test]
    fn avd_names_are_derived_from_the_device_profile() {
        assert_eq!(avd_name_for_device("pixel_7"), "zed-pixel_7");
        // Profile ids are free-form enough to contain spaces and quotes.
        assert_eq!(avd_name_for_device("Galaxy Nexus"), "zed-Galaxy_Nexus");
        assert_eq!(avd_name_for_device("7in WSVGA (Tablet)"), "zed-7in_WSVGA__Tablet_");
    }

    #[test]
    fn device_profile_ids_are_humanized_for_labels() {
        assert_eq!(humanize_device_profile_id("pixel_9_pro"), "Pixel 9 Pro");
        assert_eq!(humanize_device_profile_id("medium_tablet"), "Medium Tablet");
        assert_eq!(humanize_device_profile_id("pixel"), "Pixel");
    }
}
