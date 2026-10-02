use crate::{DevicePlatform, Sidebar};
use android_sdk::{AndroidSdkManager, EmulatorKey, KEYCODE_HOME};
use anyhow::{Context as _, Result, anyhow};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, SimulatorInput,
    SimulatorPointerPhase, WeakEntity,
};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool, scale_screenshot};

const ANDROID_KEYCODE_BACK: u32 = 4;
const SWIPE_STEPS: u32 = 12;
const LOG_LINES_LIMIT: usize = 400;

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
    target.context(
        "Nenhum device aberto. Abra o simulador iOS ou o emulador Android no dock de Devices e tente de novo.",
    )
}

#[derive(Deserialize, JsonSchema)]
struct DeviceInput {
    /// Which device: "ios" or "android". Leave empty for the one open in the Devices dock
    /// (iOS first when both are open).
    #[serde(default)]
    device: Option<String>,
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
async fn ios_fractions(target: &DeviceTarget, points: &[(f64, f64)], cx: &mut AsyncApp) -> Result<Vec<(f64, f64)>> {
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

async fn android_native_scale(manager: &Entity<AndroidSdkManager>, cx: &mut AsyncApp) -> Result<f64> {
    let (width, height) = cx.update(|cx| manager.read(cx).native_screen_size(cx)).await?;
    Ok(1.0 / task_agents::screenshot_scale(width, height))
}

fn ios_view(target: &DeviceTarget) -> Result<(u64, AnyWindowHandle)> {
    match target {
        DeviceTarget::Ios { view: Some(view), .. } => Ok(*view),
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
        .update(cx, |_, window, _cx| window.send_simulator_input(subview_id, input))
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
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7),
        ('c', 8), ('v', 9), ('b', 11), ('q', 12), ('w', 13), ('e', 14), ('r', 15), ('y', 16),
        ('t', 17), ('1', 18), ('2', 19), ('3', 20), ('4', 21), ('6', 22), ('5', 23), ('=', 24),
        ('9', 25), ('7', 26), ('-', 27), ('8', 28), ('0', 29), (']', 30), ('o', 31), ('u', 32),
        ('[', 33), ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('\'', 39), ('k', 40), (';', 41),
        ('\\', 42), (',', 43), ('/', 44), ('n', 45), ('m', 46), ('.', 47), (' ', 49), ('`', 50),
    ];
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'), ('@', '2'), ('#', '3'), ('$', '4'), ('%', '5'), ('^', '6'), ('&', '7'),
        ('*', '8'), ('(', '9'), (')', '0'), ('_', '-'), ('+', '='), ('{', '['), ('}', ']'),
        ('|', '\\'), (':', ';'), ('"', '\''), ('<', ','), ('>', '.'), ('?', '/'), ('~', '`'),
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
    if text.chars().all(|character| mac_key_for_character(character).is_some()) {
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
    run: impl AsyncFn(DeviceTarget, I, &mut AsyncApp) -> Result<ToolkitOutput> + Clone + Send + Sync + 'static,
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

device_selector!(DeviceInput, TapInput, SwipeInput, TypeInput, KeyInput, LogsInput, OpenUrlInput);

pub fn register_toolkit(cx: &mut App) {
    let tools = vec![
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
                let duration = Duration::from_millis(input.duration_ms.unwrap_or(300).clamp(50, 5000));
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
                        manager.update(cx, |manager, cx| manager.send_swipe(from, to, duration, cx));
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
                        let named = |w3c: &'static str, keycode: u32| EmulatorKey::Named { w3c, keycode };
                        match key.as_str() {
                            "back" => manager.update(cx, |manager, cx| {
                                manager.send_keyevent(ANDROID_KEYCODE_BACK, cx)
                            }),
                            "home" => manager.update(cx, |manager, cx| {
                                manager.send_keyevent(KEYCODE_HOME, cx)
                            }),
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
                Ok(ToolkitOutput::text(format!("Apertou {key} no {}.", target.label())))
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
                Ok(ToolkitOutput::text(format!("Abriu {} no {}.", input.url, target.label())))
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
            id: "devices".into(),
            name: "Dispositivos".into(),
            description: "Simulador iOS e emulador Android do dock de Devices".into(),
            icon: "smartphone".into(),
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
}
