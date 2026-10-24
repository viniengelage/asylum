use crate::{DeviceInstanceKind, DevicePlatform, Sidebar};
use android_sdk::{AndroidSdkManager, AndroidSdkState, EmulatorKey, KEYCODE_HOME};
use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, SimulatorInput,
    SimulatorPointerPhase, WeakEntity,
};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;
use std::time::Duration;
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool, scale_screenshot};
use util::ResultExt as _;
use workspace::dock::DockPosition;

const ANDROID_KEYCODE_BACK: u32 = 4;
const SWIPE_STEPS: u32 = 12;
const LOG_LINES_LIMIT: usize = 400;
/// A cold boot of a simulator that was never started can take a while on first launch.
const IOS_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
/// An emulator cold boot is slow, more so the first time an AVD starts.
const ANDROID_OPEN_TIMEOUT: Duration = Duration::from_secs(300);
/// Reading the Android installation from disk is quick; this only bounds a stuck scan.
const ANDROID_DETECT_TIMEOUT: Duration = Duration::from_secs(20);
const OPEN_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Every devices sidebar (the panel and the instances opened with +), with the window it
/// lives in, so agent tools can reach the simulator view and the emulator manager.
#[derive(Default)]
struct DeviceHosts(Vec<(WeakEntity<Sidebar>, AnyWindowHandle)>);

impl Global for DeviceHosts {}

pub(crate) fn register_device_host(
    sidebar: WeakEntity<Sidebar>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let hosts = cx.default_global::<DeviceHosts>();
    hosts.0.retain(|(host, _)| host.upgrade().is_some());
    hosts.0.push((sidebar, window));
}

enum DeviceTarget {
    Ios {
        udid: String,
        name: String,
        view: Option<(u64, AnyWindowHandle)>,
    },
    Android {
        manager: Entity<AndroidSdkManager>,
        serial: String,
    },
}

impl DeviceTarget {
    fn label(&self) -> String {
        match self {
            DeviceTarget::Ios { name, .. } => format!("iOS · {name}"),
            DeviceTarget::Android { serial, .. } => format!("Android · {serial}"),
        }
    }
}

fn wants(device: Option<&str>, platform: DevicePlatform) -> bool {
    let Some(device) = device.map(str::to_lowercase) else {
        return true;
    };
    match platform {
        DevicePlatform::Ios => {
            device.contains("ios") || device.contains("iphone") || device.contains("ipad")
        }
        DevicePlatform::Android => {
            device.contains("android") || device.contains("pixel") || device.contains("emulator")
        }
    }
}

fn resolve_device(device: Option<&str>, cx: &App) -> Result<DeviceTarget> {
    let hosts = cx
        .try_global::<DeviceHosts>()
        .map(|hosts| hosts.0.clone())
        .unwrap_or_default();
    let mut ios = None;
    let mut android = None;
    for (sidebar, window) in hosts {
        let Some(sidebar) = sidebar.upgrade() else {
            continue;
        };
        let sidebar = sidebar.read(cx);
        if let Some(udid) = sidebar.selected_ios_device_udid.clone() {
            let name = sidebar
                .ios_devices
                .iter()
                .find(|candidate| candidate.udid == udid)
                .map(|candidate| candidate.name.clone())
                .unwrap_or_else(|| udid.clone());
            let view = sidebar
                .ios_native_subview_id
                .filter(|_| sidebar.ios_native_subview_udid.as_deref() == Some(udid.as_str()))
                .map(|subview_id| (subview_id, window));
            let candidate = DeviceTarget::Ios { udid, name, view };
            let is_better = match &ios {
                None => true,
                Some(DeviceTarget::Ios { view, .. }) => view.is_none(),
                Some(DeviceTarget::Android { .. }) => false,
            };
            if is_better && (device.is_none() || wants(device, DevicePlatform::Ios)) {
                ios = Some(candidate);
            }
        }
        if let Some(manager) = sidebar.android_sdk_manager.clone()
            && let Some(serial) = manager.read(cx).running_serial().map(str::to_string)
            && android.is_none()
            && (device.is_none() || wants(device, DevicePlatform::Android))
        {
            android = Some(DeviceTarget::Android { manager, serial });
        }
    }

    let prefers_android = device.is_some_and(|device| wants(Some(device), DevicePlatform::Android));
    let target = if prefers_android {
        android.or(ios)
    } else {
        ios.or(android)
    };
    target
        .context("Nenhum device aberto. Veja as opções com device_list e abra um com device_open.")
}

/// A device an agent asked to see, resolved against what can be booted.
pub(crate) enum AgentDevice {
    Ios { udid: String },
    Android { avd: String },
}

impl AgentDevice {
    fn kind(&self) -> DeviceInstanceKind {
        match self {
            AgentDevice::Ios { .. } => DeviceInstanceKind::Ios,
            AgentDevice::Android { .. } => DeviceInstanceKind::Android,
        }
    }
}

/// The Devices panel of the window showing `project`, with that window. Instances opened
/// with + register as hosts too, but only the panel lays them out.
fn devices_panel(
    project: &Entity<Project>,
    cx: &App,
) -> Result<(Entity<Sidebar>, AnyWindowHandle)> {
    let panels = cx
        .try_global::<DeviceHosts>()
        .map(|hosts| hosts.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(sidebar, window)| Some((sidebar.upgrade()?, window)))
        .filter(|(sidebar, _)| sidebar.read(cx).device_instance_host.is_none())
        .collect::<Vec<_>>();
    let shows_project = |sidebar: &Entity<Sidebar>| {
        sidebar
            .read(cx)
            .active_workspace(cx)
            .is_some_and(|workspace| workspace.read(cx).project() == project)
    };
    panels
        .iter()
        .find(|(sidebar, _)| shows_project(sidebar))
        .or_else(|| panels.first())
        .cloned()
        .context("Esta janela não tem o dock de Devices.")
}

/// The instances the panel shows, in order, starting with the panel's own one.
fn visible_instances(panel: &Entity<Sidebar>, cx: &App) -> Vec<Entity<Sidebar>> {
    let sidebar = panel.read(cx);
    (!sidebar.own_device_instance_closed)
        .then(|| panel.clone())
        .into_iter()
        .chain(sidebar.device_instances.iter().cloned())
        .collect()
}

/// The instance that shows `kind`, or the first one, which is switched to it.
fn instance_for(
    panel: &Entity<Sidebar>,
    kind: DeviceInstanceKind,
    cx: &App,
) -> Result<Entity<Sidebar>> {
    let instances = visible_instances(panel, cx);
    instances
        .iter()
        .find(|instance| instance.read(cx).device_instance_kind() == kind)
        .or_else(|| instances.first())
        .cloned()
        .context("O dock de Devices não tem nenhuma instância aberta.")
}

/// The emulator manager of the instance Android runs in, once it knows what is installed.
async fn android_manager(
    panel: &Entity<Sidebar>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<Entity<AndroidSdkManager>> {
    let instance = cx.update(|cx| instance_for(panel, DeviceInstanceKind::Android, cx))?;
    let manager = window.update(cx, |_, _window, cx| {
        instance.update(cx, |instance, cx| {
            instance.ensure_android_sdk_manager(cx);
            instance.android_sdk_manager.clone()
        })
    })?;
    let manager = manager.context("o gerenciador do emulador não foi criado")?;
    wait_for(ANDROID_DETECT_TIMEOUT, cx, |cx| {
        Ok((!matches!(manager.read(cx).state(), AndroidSdkState::Unknown)).then_some(()))
    })
    .await
    .context("a instalação do Android não foi lida a tempo")?;
    Ok(manager)
}

/// Polls `check` until it returns a value, fails, or `timeout` runs out.
async fn wait_for<T>(
    timeout: Duration,
    cx: &mut AsyncApp,
    mut check: impl FnMut(&App) -> Result<Option<T>>,
) -> Result<T> {
    let started = std::time::Instant::now();
    loop {
        if let Some(value) = cx.update(|cx| check(cx))? {
            return Ok(value);
        }
        anyhow::ensure!(
            started.elapsed() < timeout,
            "tempo esgotado depois de {}s",
            timeout.as_secs()
        );
        cx.background_executor().timer(OPEN_POLL_INTERVAL).await;
    }
}

/// What the Android side can offer, or why it can't.
fn android_unavailable_reason(state: &AndroidSdkState) -> Option<String> {
    match state {
        AndroidSdkState::NotInstalled | AndroidSdkState::Unknown => Some(
            "o SDK Android não está instalado; o usuário instala pelo botão da aba Android no dock de Devices"
                .to_string(),
        ),
        AndroidSdkState::Installing(_) => Some("o SDK Android ainda está sendo instalado".to_string()),
        AndroidSdkState::Preparing(job) => Some(format!(
            "o dock de Devices está ocupado ({}); espere terminar",
            job.title
        )),
        AndroidSdkState::Installed => Some(
            "nenhum dispositivo virtual criado; o usuário cria um na aba Android do dock de Devices"
                .to_string(),
        ),
        AndroidSdkState::Failed(_)
        | AndroidSdkState::AvdReady
        | AndroidSdkState::EmulatorBooting
        | AndroidSdkState::EmulatorRunning { .. } => None,
    }
}

/// Matches what the agent typed against the simulators and AVDs: an id first, then a name,
/// then part of a name. "ios" and "android" stand for the device already chosen there.
fn choose_device(
    query: &str,
    ios: &[crate::IosSimulatorDevice],
    selected_udid: Option<&str>,
    avds: &[android_sdk::AndroidAvd],
    selected_avd: Option<&str>,
) -> Result<AgentDevice> {
    let query = query.trim();
    let lowercase = query.to_lowercase();
    let ios_device = |device: &crate::IosSimulatorDevice| AgentDevice::Ios {
        udid: device.udid.clone(),
    };
    let android_device = |avd: &android_sdk::AndroidAvd| AgentDevice::Android {
        avd: avd.name.clone(),
    };

    if matches!(
        lowercase.as_str(),
        "ios" | "iphone" | "simulador" | "simulator"
    ) {
        let device = selected_udid
            .and_then(|udid| ios.iter().find(|device| device.udid == udid))
            .or_else(|| crate::default_ios_device(ios))
            .context("Nenhum simulador iOS disponível.")?;
        return Ok(ios_device(device));
    }
    if matches!(lowercase.as_str(), "android" | "emulador" | "emulator") {
        let avd = selected_avd
            .and_then(|name| avds.iter().find(|avd| avd.name == name))
            .or_else(|| avds.first())
            .context("Nenhum dispositivo virtual Android disponível.")?;
        return Ok(android_device(avd));
    }

    if let Some(device) = ios
        .iter()
        .find(|device| device.udid.eq_ignore_ascii_case(query))
    {
        return Ok(ios_device(device));
    }
    if let Some(avd) = avds.iter().find(|avd| avd.name == query) {
        return Ok(android_device(avd));
    }

    let mut matches = ios
        .iter()
        .filter(|device| device.name.to_lowercase() == lowercase)
        .map(ios_device)
        .chain(
            avds.iter()
                .filter(|avd| avd.label().to_lowercase() == lowercase)
                .map(android_device),
        )
        .collect::<Vec<_>>();
    if matches.is_empty() {
        matches = ios
            .iter()
            .filter(|device| device.name.to_lowercase().contains(&lowercase))
            .map(ios_device)
            .chain(
                avds.iter()
                    .filter(|avd| {
                        avd.label().to_lowercase().contains(&lowercase)
                            || avd.name.to_lowercase().contains(&lowercase)
                    })
                    .map(android_device),
            )
            .collect();
    }
    match matches.len() {
        0 => anyhow::bail!(
            "Nenhum device corresponde a \"{query}\". Veja as opções com device_list."
        ),
        1 => Ok(matches.remove(0)),
        count => anyhow::bail!(
            "{count} devices correspondem a \"{query}\". Use o id que device_list mostra."
        ),
    }
}

#[derive(Deserialize, JsonSchema)]
struct DeviceInput {
    /// Which device: "ios" or "android". Leave empty for the one open in the Devices dock
    /// (iOS first when both are open).
    #[serde(default)]
    device: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListInput {}

#[derive(Deserialize, JsonSchema)]
struct OpenInput {
    /// The device to show: an id or name from device_list (e.g. "iPhone 16e", "Pixel 7"), or
    /// "ios" / "android" for the one already chosen on that platform.
    device: String,
}

#[derive(Deserialize, JsonSchema)]
struct TapInput {
    #[serde(default)]
    device: Option<String>,
    /// Horizontal position in the pixels of the latest `device_screenshot`.
    x: f64,
    /// Vertical position in the pixels of the latest `device_screenshot`.
    y: f64,
}

#[derive(Deserialize, JsonSchema)]
struct SwipeInput {
    #[serde(default)]
    device: Option<String>,
    /// Start, in the pixels of the latest `device_screenshot`.
    from_x: f64,
    from_y: f64,
    /// End, in the pixels of the latest `device_screenshot`.
    to_x: f64,
    to_y: f64,
    /// How long the finger takes to move, in milliseconds. Defaults to 300.
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct TypeInput {
    #[serde(default)]
    device: Option<String>,
    /// Text typed into whatever field has focus on the device.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct KeyInput {
    #[serde(default)]
    device: Option<String>,
    /// One of: enter, backspace, tab, escape, up, down, left, right, back (Android), home (Android).
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct LogsInput {
    #[serde(default)]
    device: Option<String>,
    /// How many lines to return, at most 400. Defaults to 150.
    #[serde(default)]
    lines: Option<usize>,
    /// Only lines containing this text (case-insensitive), e.g. the app name or a tag.
    #[serde(default)]
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct OpenUrlInput {
    #[serde(default)]
    device: Option<String>,
    /// A URL or deep link, e.g. `exp://192.168.0.12:8081` or `myapp://login`.
    url: String,
}

struct Screenshot {
    png: Vec<u8>,
    width: u32,
    height: u32,
    /// Native pixels per screenshot pixel.
    to_native: f64,
}

async fn capture(target: &DeviceTarget, cx: &mut AsyncApp) -> Result<Screenshot> {
    let png = match target {
        DeviceTarget::Ios { udid, .. } => {
            let path = std::env::temp_dir().join(format!("asylum_agent_ios_{udid}.png"));
            let output = smol::process::Command::new("xcrun")
                .args(["simctl", "io", udid, "screenshot", "--type=png"])
                .arg(&path)
                .output()
                .await
                .context("não foi possível executar xcrun simctl")?;
            anyhow::ensure!(
                output.status.success(),
                "simctl screenshot falhou: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            smol::fs::read(&path)
                .await
                .with_context(|| format!("não foi possível ler {}", path.display()))?
        }
        DeviceTarget::Android { manager, .. } => {
            cx.update(|cx| manager.read(cx).screenshot_png(cx)).await?
        }
    };
    let screenshot = cx
        .background_spawn(async move {
            let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .context("a captura não é um PNG válido")?;
            scale_screenshot(image)
        })
        .await?;
    Ok(Screenshot {
        png: screenshot.png,
        width: screenshot.width,
        height: screenshot.height,
        to_native: 1.0 / screenshot.scale,
    })
}

/// Maps a point in screenshot pixels to the fractions of the screen the simulator input wants.
async fn ios_fractions(
    target: &DeviceTarget,
    points: &[(f64, f64)],
    cx: &mut AsyncApp,
) -> Result<Vec<(f64, f64)>> {
    let screenshot = capture(target, cx).await?;
    Ok(points
        .iter()
        .map(|(x, y)| {
            (
                (x / f64::from(screenshot.width)).clamp(0.0, 1.0),
                (y / f64::from(screenshot.height)).clamp(0.0, 1.0),
            )
        })
        .collect())
}

async fn android_native_scale(
    manager: &Entity<AndroidSdkManager>,
    cx: &mut AsyncApp,
) -> Result<f64> {
    let (width, height) = cx
        .update(|cx| manager.read(cx).native_screen_size(cx))
        .await?;
    Ok(1.0 / task_agents::screenshot_scale(width, height))
}

fn ios_view(target: &DeviceTarget) -> Result<(u64, AnyWindowHandle)> {
    match target {
        DeviceTarget::Ios {
            view: Some(view), ..
        } => Ok(*view),
        DeviceTarget::Ios { .. } => Err(anyhow!(
            "O simulador iOS precisa estar visível no dock de Devices para receber toques e teclas."
        )),
        DeviceTarget::Android { .. } => Err(anyhow!("não é um simulador iOS")),
    }
}

#[cfg(target_os = "macos")]
fn send_ios_input(target: &DeviceTarget, input: SimulatorInput, cx: &mut AsyncApp) -> Result<()> {
    let (subview_id, window) = ios_view(target)?;
    window
        .update(cx, |_, window, _cx| {
            window.send_simulator_input(subview_id, input)
        })
        .context("a janela do simulador foi fechada")?
}

#[cfg(not(target_os = "macos"))]
fn send_ios_input(target: &DeviceTarget, _input: SimulatorInput, _cx: &mut AsyncApp) -> Result<()> {
    ios_view(target)?;
    Err(anyhow!("O simulador iOS só está disponível no macOS."))
}

async fn ios_pointer_path(
    target: &DeviceTarget,
    points: &[(f64, f64)],
    step_delay: Duration,
    cx: &mut AsyncApp,
) -> Result<()> {
    let fractions = ios_fractions(target, points, cx).await?;
    let last = fractions.len().saturating_sub(1);
    for (index, (x, y)) in fractions.into_iter().enumerate() {
        let phase = if index == 0 {
            SimulatorPointerPhase::Down
        } else if index == last {
            SimulatorPointerPhase::Up
        } else {
            SimulatorPointerPhase::Drag
        };
        send_ios_input(target, SimulatorInput::Pointer { x, y, phase }, cx)?;
        if index != last {
            cx.background_executor().timer(step_delay).await;
        }
    }
    Ok(())
}

/// macOS virtual key codes on the ANSI layout, with whether Shift is needed.
fn mac_key_for_character(character: char) -> Option<(u16, bool)> {
    const BASE: &[(char, u16)] = &[
        ('a', 0),
        ('s', 1),
        ('d', 2),
        ('f', 3),
        ('h', 4),
        ('g', 5),
        ('z', 6),
        ('x', 7),
        ('c', 8),
        ('v', 9),
        ('b', 11),
        ('q', 12),
        ('w', 13),
        ('e', 14),
        ('r', 15),
        ('y', 16),
        ('t', 17),
        ('1', 18),
        ('2', 19),
        ('3', 20),
        ('4', 21),
        ('6', 22),
        ('5', 23),
        ('=', 24),
        ('9', 25),
        ('7', 26),
        ('-', 27),
        ('8', 28),
        ('0', 29),
        (']', 30),
        ('o', 31),
        ('u', 32),
        ('[', 33),
        ('i', 34),
        ('p', 35),
        ('l', 37),
        ('j', 38),
        ('\'', 39),
        ('k', 40),
        (';', 41),
        ('\\', 42),
        (',', 43),
        ('/', 44),
        ('n', 45),
        ('m', 46),
        ('.', 47),
        (' ', 49),
        ('`', 50),
    ];
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('_', '-'),
        ('+', '='),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
        (':', ';'),
        ('"', '\''),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
        ('~', '`'),
    ];
    let lookup = |character: char| {
        BASE.iter()
            .find(|(candidate, _)| *candidate == character)
            .map(|(_, code)| *code)
    };
    if character.is_ascii_uppercase() {
        return lookup(character.to_ascii_lowercase()).map(|code| (code, true));
    }
    if let Some((_, base)) = SHIFTED.iter().find(|(shifted, _)| *shifted == character) {
        return lookup(*base).map(|code| (code, true));
    }
    lookup(character).map(|code| (code, false))
}

async fn ios_key(
    target: &DeviceTarget,
    key_code: u16,
    characters: &str,
    shift: bool,
    command: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    for down in [true, false] {
        send_ios_input(
            target,
            SimulatorInput::Key {
                key_code,
                characters: characters.to_string(),
                shift,
                command,
                down,
            },
            cx,
        )?;
    }
    cx.background_executor()
        .timer(Duration::from_millis(15))
        .await;
    Ok(())
}

async fn ios_type(target: &DeviceTarget, udid: &str, text: &str, cx: &mut AsyncApp) -> Result<()> {
    ios_view(target)?;
    if text
        .chars()
        .all(|character| mac_key_for_character(character).is_some())
    {
        for character in text.chars() {
            if let Some((key_code, shift)) = mac_key_for_character(character) {
                ios_key(target, key_code, &character.to_string(), shift, false, cx).await?;
            }
        }
        return Ok(());
    }
    // Accents and emoji have no key on the ANSI layout: put the text on the simulator's
    // pasteboard and paste it with ⌘V.
    let mut child = smol::process::Command::new("xcrun")
        .args(["simctl", "pbcopy", udid])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("não foi possível executar xcrun simctl pbcopy")?;
    if let Some(mut stdin) = child.stdin.take() {
        use smol::io::AsyncWriteExt as _;
        stdin.write_all(text.as_bytes()).await?;
    }
    let status = child.status().await?;
    anyhow::ensure!(status.success(), "simctl pbcopy falhou");
    let (paste_key, _) = mac_key_for_character('v').context("tecla v")?;
    ios_key(target, paste_key, "v", false, true, cx).await
}

fn device_input_tool<I>(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    access: ToolAccess,
    run: impl AsyncFn(DeviceTarget, I, &mut AsyncApp) -> Result<ToolkitOutput>
    + Clone
    + Send
    + Sync
    + 'static,
) -> ToolkitTool
where
    I: JsonSchema + serde::de::DeserializeOwned + DeviceSelector + 'static,
{
    ToolkitTool::new(
        name,
        title,
        description,
        access,
        move |_project: Entity<Project>, input: I, cx: &mut App| {
            let target = resolve_device(input.device(), cx);
            let run = run.clone();
            cx.spawn(async move |cx| run(target?, input, cx).await)
        },
    )
}

trait DeviceSelector {
    fn device(&self) -> Option<&str>;
}

macro_rules! device_selector {
    ($($input:ty),*) => {
        $(impl DeviceSelector for $input {
            fn device(&self) -> Option<&str> {
                self.device.as_deref().filter(|device| !device.trim().is_empty())
            }
        })*
    };
}

device_selector!(
    DeviceInput,
    TapInput,
    SwipeInput,
    TypeInput,
    KeyInput,
    LogsInput,
    OpenUrlInput
);

/// What the Devices dock can show: the simulators and AVDs, marked with the one on screen.
async fn list_devices(project: Entity<Project>, cx: &mut AsyncApp) -> Result<ToolkitOutput> {
    let (panel, window) = cx.update(|cx| devices_panel(&project, cx))?;
    let mut text = String::new();

    let shown_udids = cx.update(|cx| {
        visible_instances(&panel, cx)
            .into_iter()
            .filter_map(|instance| {
                let instance = instance.read(cx);
                instance
                    .ios_native_subview_udid
                    .clone()
                    .filter(|_| !instance.ios_native_subview_hidden)
            })
            .collect::<Vec<_>>()
    });
    writeln!(text, "## iOS (simuladores)")?;
    match crate::list_ios_simulators().await {
        Ok(devices) if devices.is_empty() => writeln!(text, "Nenhum simulador disponível.")?,
        Ok(devices) => {
            for device in devices {
                let os_version = device
                    .os_version
                    .as_deref()
                    .map(|version| format!(" ({version})"))
                    .unwrap_or_default();
                let on_screen = if shown_udids.contains(&device.udid) {
                    ", na tela"
                } else {
                    ""
                };
                writeln!(
                    text,
                    "- {}{os_version}: {}{on_screen}. id: {}",
                    device.name, device.state, device.udid
                )?;
            }
        }
        Err(error) => writeln!(text, "Indisponível: {error:#}")?,
    }

    writeln!(text, "\n## Android (emuladores)")?;
    match android_manager(&panel, window, cx).await {
        Ok(manager) => cx.update(|cx| -> Result<()> {
            let manager = manager.read(cx);
            if let Some(reason) = android_unavailable_reason(manager.state()) {
                writeln!(text, "Indisponível: {reason}.")?;
                return Ok(());
            }
            let running = match manager.state() {
                AndroidSdkState::EmulatorRunning { .. } => Some("rodando, na tela"),
                AndroidSdkState::EmulatorBooting => Some("inicializando"),
                _ => None,
            };
            for avd in manager.avds() {
                let state = running
                    .filter(|_| manager.selected_avd() == Some(avd.name.as_str()))
                    .unwrap_or("desligado");
                writeln!(text, "- {}: {state}. id: {}", avd.label(), avd.name)?;
            }
            if let AndroidSdkState::Failed(message) = manager.state() {
                writeln!(text, "Último erro: {message}")?;
            }
            Ok(())
        })?,
        Err(error) => writeln!(text, "Indisponível: {error:#}")?,
    }
    writeln!(
        text,
        "\nAbra um com device_open; só um emulador Android roda por vez."
    )?;
    Ok(ToolkitOutput::text(text))
}

/// Shows the device the agent named in the Devices dock and waits until its screen is up.
async fn open_device(
    project: Entity<Project>,
    query: String,
    cx: &mut AsyncApp,
) -> Result<ToolkitOutput> {
    let (panel, window) = cx.update(|cx| devices_panel(&project, cx))?;
    let wants_android = !matches!(
        query.trim().to_lowercase().as_str(),
        "ios" | "iphone" | "simulador" | "simulator"
    );
    let wants_ios = !matches!(
        query.trim().to_lowercase().as_str(),
        "android" | "emulador" | "emulator"
    );

    let ios = if wants_ios {
        crate::list_ios_simulators().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let android = if wants_android {
        android_manager(&panel, window, cx).await.ok()
    } else {
        None
    };
    let device = cx.update(|cx| {
        let selected_udid = visible_instances(&panel, cx)
            .iter()
            .find_map(|instance| instance.read(cx).selected_ios_device_udid.clone());
        let (avds, selected_avd) = match &android {
            Some(manager) => {
                let manager = manager.read(cx);
                (
                    manager.avds().to_vec(),
                    manager.selected_avd().map(str::to_string),
                )
            }
            None => (Vec::new(), None),
        };
        let device = choose_device(
            &query,
            &ios,
            selected_udid.as_deref(),
            &avds,
            selected_avd.as_deref(),
        );
        if device.is_err()
            && wants_android
            && let Some(reason) = android
                .as_ref()
                .and_then(|manager| android_unavailable_reason(manager.read(cx).state()))
        {
            return device.with_context(|| format!("Android: {reason}"));
        }
        device
    })?;

    let instance = cx.update(|cx| instance_for(&panel, device.kind(), cx))?;
    let workspace = cx
        .update(|cx| panel.read(cx).active_workspace(cx))
        .context("o dock de Devices não está em nenhum workspace")?;
    let label = match &device {
        AgentDevice::Ios { udid } => ios
            .iter()
            .find(|candidate| &candidate.udid == udid)
            .map(|candidate| format!("iOS · {}", candidate.name))
            .unwrap_or_else(|| format!("iOS · {udid}")),
        AgentDevice::Android { avd } => cx.update(|cx| {
            android
                .as_ref()
                .and_then(|manager| {
                    manager
                        .read(cx)
                        .avds()
                        .iter()
                        .find(|candidate| &candidate.name == avd)
                        .map(|candidate| format!("Android · {}", candidate.label()))
                })
                .unwrap_or_else(|| format!("Android · {avd}"))
        }),
    };

    window.update(cx, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            if !workspace
                .dock_at_position(DockPosition::Devices)
                .read(cx)
                .is_open()
            {
                workspace.toggle_dock(DockPosition::Devices, window, cx);
            }
        });
    })?;
    // Opening the dock reads every instance, so the device is picked once that settled.
    let udid = match &device {
        AgentDevice::Ios { udid } => Some(udid.clone()),
        AgentDevice::Android { .. } => None,
    };
    window.update(cx, |_, window, cx| {
        instance.update(cx, |instance, cx| {
            instance.open_device_for_agent(device, window, cx)
        })
    })?;

    match udid {
        Some(udid) => {
            wait_for(IOS_OPEN_TIMEOUT, cx, |cx| {
                let instance = instance.read(cx);
                if instance.ios_failed_device_udid.as_deref() == Some(udid.as_str()) {
                    anyhow::bail!("o simulador não conectou à tela");
                }
                if let Some(error) = &instance.ios_simulator_error {
                    anyhow::bail!("{error}");
                }
                Ok(
                    (instance.ios_native_subview_udid.as_deref() == Some(udid.as_str())
                        && !instance.ios_native_subview_hidden)
                        .then_some(()),
                )
            })
            .await
            .with_context(|| format!("{label} não abriu"))?;
        }
        None => {
            let manager = cx
                .update(|cx| instance.read(cx).android_sdk_manager.clone())
                .context("o gerenciador do emulador sumiu")?;
            wait_for(ANDROID_OPEN_TIMEOUT, cx, |cx| {
                let manager = manager.read(cx);
                match manager.state() {
                    AndroidSdkState::Failed(message) => anyhow::bail!("{message}"),
                    AndroidSdkState::EmulatorRunning { .. } => {
                        Ok(manager.screen_frame().is_some().then_some(()))
                    }
                    _ => Ok(None),
                }
            })
            .await
            .with_context(|| format!("{label} não abriu"))?;
        }
    }

    let mut output = ToolkitOutput::text(format!(
        "{label} está aberto no dock de Devices. Use device_screenshot para ver a tela."
    ));
    // The screen comes along so the agent doesn't need another call to see it.
    let platform = if label.starts_with("iOS") {
        "ios"
    } else {
        "android"
    };
    let target = cx.update(|cx| resolve_device(Some(platform), cx));
    if let Ok(target) = target
        && let Some(screenshot) = capture(&target, cx).await.log_err()
    {
        output = ToolkitOutput::text(format!(
            "{label} está aberto no dock de Devices. Tela: {}x{} px (1 px aqui = {:.2} px nativos).",
            screenshot.width, screenshot.height, screenshot.to_native
        ));
        output.push_png(&screenshot.png);
    }
    Ok(output)
}

/// Shuts the device down and takes it off the Devices dock.
async fn close_device(
    project: Entity<Project>,
    query: String,
    cx: &mut AsyncApp,
) -> Result<ToolkitOutput> {
    let (panel, window) = cx.update(|cx| devices_panel(&project, cx))?;
    let ios = crate::list_ios_simulators().await.unwrap_or_default();
    let android = android_manager(&panel, window, cx).await.ok();
    let (instances, device) = cx.update(|cx| {
        let instances = visible_instances(&panel, cx);
        let selected_udid = instances
            .iter()
            .find_map(|instance| instance.read(cx).selected_ios_device_udid.clone());
        let (avds, selected_avd) = match &android {
            Some(manager) => {
                let manager = manager.read(cx);
                (
                    manager.avds().to_vec(),
                    manager.selected_avd().map(str::to_string),
                )
            }
            None => (Vec::new(), None),
        };
        let device = choose_device(
            &query,
            &ios,
            selected_udid.as_deref(),
            &avds,
            selected_avd.as_deref(),
        );
        (instances, device)
    });
    match device? {
        AgentDevice::Ios { udid } => {
            let output = smol::process::Command::new("xcrun")
                .args(["simctl", "shutdown", &udid])
                .output()
                .await
                .context("não foi possível executar xcrun simctl shutdown")?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Shutting down a device that is already off is what was asked for.
            anyhow::ensure!(
                output.status.success() || stderr.contains("current state: Shutdown"),
                "simctl shutdown falhou: {}",
                stderr.trim()
            );
            window.update(cx, |_, window, cx| {
                for instance in &instances {
                    instance.update(cx, |instance, cx| {
                        if instance.selected_ios_device_udid.as_deref() == Some(udid.as_str()) {
                            // Back to the start screen, rather than reconnecting to a device
                            // that is going down.
                            instance.remove_ios_simulator_view(window);
                            instance.ios_simulator_started = false;
                            cx.notify();
                        }
                    });
                }
            })?;
            let name = ios
                .iter()
                .find(|device| device.udid == udid)
                .map_or(udid.as_str(), |device| device.name.as_str());
            Ok(ToolkitOutput::text(format!("Desligou o simulador {name}.")))
        }
        AgentDevice::Android { avd } => {
            let manager = android.context("o emulador Android não está disponível")?;
            let stopped = cx.update(|cx| {
                manager.update(cx, |manager, cx| {
                    let runs_this_avd = manager.selected_avd() == Some(avd.as_str())
                        && matches!(
                            manager.state(),
                            AndroidSdkState::EmulatorRunning { .. }
                                | AndroidSdkState::EmulatorBooting
                        );
                    if runs_this_avd {
                        manager.stop_emulator(cx);
                    }
                    runs_this_avd
                })
            });
            Ok(ToolkitOutput::text(if stopped {
                format!("Desligou o emulador {avd}.")
            } else {
                format!("O emulador {avd} já estava desligado.")
            }))
        }
    }
}

pub fn register_toolkit(cx: &mut App) {
    let tools = vec![
        ToolkitTool::new(
            "device_list",
            "Listou os devices",
            "Lists the iOS simulators and Android emulators the Devices dock can show, with their \
             ids and which one is on screen. Use this instead of running simctl or emulator in a \
             terminal: devices started there don't appear in the app.",
            ToolAccess::Read,
            |project: Entity<Project>, _input: ListInput, cx: &mut App| {
                cx.spawn(async move |cx| list_devices(project, cx).await)
            },
        ),
        ToolkitTool::new(
            "device_open",
            "Abriu um device",
            "Opens an iOS simulator or Android emulator in the Devices dock, boots it, waits until \
             its screen is up and returns a screenshot. Name it by an id or name from device_list, \
             or \"ios\" / \"android\". Only one Android emulator runs at a time: opening another \
             AVD replaces it.",
            ToolAccess::Act,
            |project: Entity<Project>, input: OpenInput, cx: &mut App| {
                cx.spawn(async move |cx| open_device(project, input.device, cx).await)
            },
        ),
        ToolkitTool::new(
            "device_close",
            "Desligou um device",
            "Shuts down an iOS simulator or the Android emulator, named like in device_open.",
            ToolAccess::Act,
            |project: Entity<Project>, input: OpenInput, cx: &mut App| {
                cx.spawn(async move |cx| close_device(project, input.device, cx).await)
            },
        ),
        device_input_tool(
            "device_screenshot",
            "Viu a tela do device",
            "Captures the screen of the iOS simulator or Android emulator open in the Devices dock. \
             Coordinates for device_tap and device_swipe are in the pixels of this image. Take a new \
             screenshot after acting to see the result.",
            ToolAccess::Read,
            async |target, _input: DeviceInput, cx| {
                let screenshot = capture(&target, cx).await?;
                let mut output = ToolkitOutput::text(format!(
                    "{}: {}x{} px (1 px aqui = {:.2} px nativos).",
                    target.label(),
                    screenshot.width,
                    screenshot.height,
                    screenshot.to_native
                ));
                output.push_png(&screenshot.png);
                Ok(output)
            },
        ),
        device_input_tool(
            "device_tap",
            "Tocou na tela do device",
            "Taps the device screen at a point given in the pixels of the latest device_screenshot.",
            ToolAccess::Act,
            async |target, input: TapInput, cx| {
                match &target {
                    DeviceTarget::Ios { .. } => {
                        let point = (input.x, input.y);
                        ios_pointer_path(&target, &[point, point], Duration::from_millis(60), cx)
                            .await?;
                    }
                    DeviceTarget::Android { manager, .. } => {
                        let to_native = android_native_scale(manager, cx).await?;
                        let x = (input.x * to_native).round().max(0.0) as u32;
                        let y = (input.y * to_native).round().max(0.0) as u32;
                        manager.update(cx, |manager, cx| manager.send_tap(x, y, cx));
                    }
                }
                Ok(ToolkitOutput::text(format!(
                    "Tocou em ({:.0}, {:.0}) no {}.",
                    input.x,
                    input.y,
                    target.label()
                )))
            },
        ),
        device_input_tool(
            "device_swipe",
            "Arrastou na tela do device",
            "Drags a finger across the device screen, e.g. to scroll a list or dismiss a sheet. \
             Points are in the pixels of the latest device_screenshot.",
            ToolAccess::Act,
            async |target, input: SwipeInput, cx| {
                let duration =
                    Duration::from_millis(input.duration_ms.unwrap_or(300).clamp(50, 5000));
                match &target {
                    DeviceTarget::Ios { .. } => {
                        let points = (0..=SWIPE_STEPS)
                            .map(|step| {
                                let progress = f64::from(step) / f64::from(SWIPE_STEPS);
                                (
                                    input.from_x + (input.to_x - input.from_x) * progress,
                                    input.from_y + (input.to_y - input.from_y) * progress,
                                )
                            })
                            .collect::<Vec<_>>();
                        ios_pointer_path(&target, &points, duration / SWIPE_STEPS, cx).await?;
                    }
                    DeviceTarget::Android { manager, .. } => {
                        let to_native = android_native_scale(manager, cx).await?;
                        let native = |value: f64| (value * to_native).round().max(0.0) as u32;
                        let from = (native(input.from_x), native(input.from_y));
                        let to = (native(input.to_x), native(input.to_y));
                        manager
                            .update(cx, |manager, cx| manager.send_swipe(from, to, duration, cx));
                        cx.background_executor().timer(duration).await;
                    }
                }
                Ok(ToolkitOutput::text(format!(
                    "Arrastou de ({:.0}, {:.0}) até ({:.0}, {:.0}) no {}.",
                    input.from_x,
                    input.from_y,
                    input.to_x,
                    input.to_y,
                    target.label()
                )))
            },
        ),
        device_input_tool(
            "device_type",
            "Digitou no device",
            "Types text into the focused field on the device. Tap the field first.",
            ToolAccess::Act,
            async |target, input: TypeInput, cx| {
                match &target {
                    DeviceTarget::Ios { udid, .. } => {
                        ios_type(&target, &udid.clone(), &input.text, cx).await?
                    }
                    DeviceTarget::Android { manager, .. } => manager.update(cx, |manager, cx| {
                        manager.send_key(EmulatorKey::Text(input.text.clone()), cx)
                    }),
                }
                Ok(ToolkitOutput::text(format!(
                    "Digitou {} caracteres no {}.",
                    input.text.chars().count(),
                    target.label()
                )))
            },
        ),
        device_input_tool(
            "device_key",
            "Apertou uma tecla no device",
            "Presses a key on the device: enter, backspace, tab, escape, up, down, left, right, \
             and on Android also back and home.",
            ToolAccess::Act,
            async |target, input: KeyInput, cx| {
                let key = input.key.trim().to_lowercase();
                match &target {
                    DeviceTarget::Ios { .. } => {
                        let key_code = match key.as_str() {
                            "enter" | "return" => 36,
                            "backspace" | "delete" => 51,
                            "tab" => 48,
                            "escape" | "esc" => 53,
                            "left" => 123,
                            "right" => 124,
                            "down" => 125,
                            "up" => 126,
                            "home" | "back" => anyhow::bail!(
                                "O iOS não tem tecla {key}; use device_swipe (de baixo para cima para ir ao início, da borda esquerda para voltar)."
                            ),
                            other => anyhow::bail!("tecla desconhecida: {other}"),
                        };
                        ios_key(&target, key_code, "", false, false, cx).await?;
                    }
                    DeviceTarget::Android { manager, .. } => {
                        let named =
                            |w3c: &'static str, keycode: u32| EmulatorKey::Named { w3c, keycode };
                        match key.as_str() {
                            "back" => manager.update(cx, |manager, cx| {
                                manager.send_keyevent(ANDROID_KEYCODE_BACK, cx)
                            }),
                            "home" => manager
                                .update(cx, |manager, cx| manager.send_keyevent(KEYCODE_HOME, cx)),
                            other => {
                                let key = match other {
                                    "enter" | "return" => named("Enter", 66),
                                    "backspace" | "delete" => named("Backspace", 67),
                                    "tab" => named("Tab", 61),
                                    "escape" | "esc" => named("Escape", 111),
                                    "left" => named("ArrowLeft", 21),
                                    "right" => named("ArrowRight", 22),
                                    "up" => named("ArrowUp", 19),
                                    "down" => named("ArrowDown", 20),
                                    other => anyhow::bail!("tecla desconhecida: {other}"),
                                };
                                manager.update(cx, |manager, cx| manager.send_key(key, cx));
                            }
                        }
                    }
                }
                Ok(ToolkitOutput::text(format!(
                    "Apertou {key} no {}.",
                    target.label()
                )))
            },
        ),
        device_input_tool(
            "device_open_url",
            "Abriu um link no device",
            "Opens a URL or deep link on the device, e.g. an Expo dev server URL or an app scheme.",
            ToolAccess::Act,
            async |target, input: OpenUrlInput, cx| {
                match &target {
                    DeviceTarget::Ios { udid, .. } => {
                        let output = smol::process::Command::new("xcrun")
                            .args(["simctl", "openurl", udid, &input.url])
                            .output()
                            .await
                            .context("não foi possível executar xcrun simctl openurl")?;
                        anyhow::ensure!(
                            output.status.success(),
                            "simctl openurl falhou: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                    }
                    DeviceTarget::Android { manager, .. } => {
                        cx.update(|cx| manager.read(cx).open_url(input.url.clone(), cx))
                            .await?
                    }
                }
                Ok(ToolkitOutput::text(format!(
                    "Abriu {} no {}.",
                    input.url,
                    target.label()
                )))
            },
        ),
        device_input_tool(
            "device_logs",
            "Leu os logs do device",
            "Reads recent log lines from the device (logcat on Android, the unified log on iOS), \
             optionally filtered by a text such as the app name.",
            ToolAccess::Read,
            async |target, input: LogsInput, cx| {
                let lines = input.lines.unwrap_or(150).clamp(1, LOG_LINES_LIMIT);
                let text = match &target {
                    DeviceTarget::Ios { udid, .. } => {
                        let mut arguments = vec![
                            "simctl".to_string(),
                            "spawn".to_string(),
                            udid.clone(),
                            "log".to_string(),
                            "show".to_string(),
                            "--last".to_string(),
                            "2m".to_string(),
                            "--style".to_string(),
                            "compact".to_string(),
                        ];
                        if let Some(filter) = &input.filter {
                            arguments.push("--predicate".to_string());
                            arguments.push(format!(
                                "eventMessage CONTAINS[c] \"{0}\" OR process CONTAINS[c] \"{0}\"",
                                filter.replace('"', "")
                            ));
                        }
                        let output = smol::process::Command::new("xcrun")
                            .args(&arguments)
                            .output()
                            .await
                            .context("não foi possível executar xcrun simctl spawn log")?;
                        anyhow::ensure!(
                            output.status.success(),
                            "log show falhou: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                        let text = String::from_utf8_lossy(&output.stdout).into_owned();
                        let all_lines = text.lines().collect::<Vec<_>>();
                        all_lines[all_lines.len().saturating_sub(lines)..].join("\n")
                    }
                    DeviceTarget::Android { manager, .. } => {
                        cx.update(|cx| manager.read(cx).logcat(lines, input.filter.clone(), cx))
                            .await?
                    }
                };
                Ok(ToolkitOutput::text(if text.trim().is_empty() {
                    format!("Nenhuma linha de log no {}.", target.label())
                } else {
                    text
                }))
            },
        ),
    ];

    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::DEVICES.into(),
            name: "Dispositivos".into(),
            description: "Abre e controla o simulador iOS e o emulador Android do dock de Devices"
                .into(),
            icon: "smartphone".into(),
            mention: None,
            tools,
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mac_key_for_character() {
        assert_eq!(mac_key_for_character('a'), Some((0, false)));
        assert_eq!(mac_key_for_character('A'), Some((0, true)));
        assert_eq!(mac_key_for_character('@'), Some((19, true)));
        assert_eq!(mac_key_for_character(' '), Some((49, false)));
        assert_eq!(mac_key_for_character('é'), None);
    }

    #[test]
    fn test_wants() {
        assert!(wants(None, DevicePlatform::Ios));
        assert!(wants(Some("iPhone 16e"), DevicePlatform::Ios));
        assert!(!wants(Some("android"), DevicePlatform::Ios));
        assert!(wants(Some("Pixel 9"), DevicePlatform::Android));
    }

    fn simulator(name: &str, udid: &str, state: &str) -> crate::IosSimulatorDevice {
        crate::IosSimulatorDevice {
            name: name.to_string(),
            udid: udid.to_string(),
            state: state.to_string(),
            os_version: None,
        }
    }

    fn avd(name: &str, device_profile: &str) -> android_sdk::AndroidAvd {
        android_sdk::AndroidAvd {
            name: name.to_string(),
            device_profile: Some(device_profile.to_string()),
            ..Default::default()
        }
    }

    fn chosen(device: Result<AgentDevice>) -> String {
        match device {
            Ok(AgentDevice::Ios { udid }) => format!("ios:{udid}"),
            Ok(AgentDevice::Android { avd }) => format!("android:{avd}"),
            Err(error) => format!("error:{error}"),
        }
    }

    #[test]
    fn test_choose_device() {
        let ios = [
            simulator("iPhone 16e", "AAA", "Shutdown"),
            simulator("iPhone 16 Pro", "BBB", "Booted"),
            simulator("iPad Air", "CCC", "Shutdown"),
        ];
        let avds = [avd("zed-pixel_7", "pixel_7"), avd("zed-pixel_9", "pixel_9")];
        let choose = |query: &str| chosen(choose_device(query, &ios, None, &avds, None));

        // A platform alone stands for the device the dock would pick.
        assert_eq!(choose("ios"), "ios:BBB");
        assert_eq!(choose("Android"), "android:zed-pixel_7");
        assert_eq!(
            chosen(choose_device(
                "android",
                &ios,
                None,
                &avds,
                Some("zed-pixel_9")
            )),
            "android:zed-pixel_9"
        );
        assert_eq!(
            chosen(choose_device("ios", &ios, Some("CCC"), &avds, None)),
            "ios:CCC"
        );

        // Ids, then exact names, then parts of names.
        assert_eq!(choose("aaa"), "ios:AAA");
        assert_eq!(choose("zed-pixel_9"), "android:zed-pixel_9");
        assert_eq!(choose("iphone 16e"), "ios:AAA");
        assert_eq!(choose("iPad"), "ios:CCC");
        assert_eq!(choose("pixel_7"), "android:zed-pixel_7");

        assert_eq!(choose("iPhone"), "ios:BBB");
        assert!(choose("iPhone 16").starts_with("error:2 devices"));
        assert!(choose("Galaxy").starts_with("error:Nenhum device"));
        assert!(chosen(choose_device("android", &ios, None, &[], None)).starts_with("error:"));
    }
}
