use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::{cmp::Ordering, collections::HashMap, path::PathBuf};

/// Apple's index of downloadable simulator runtimes, the same one Xcode's Components settings
/// read. It carries each runtime's size, which neither `simctl` nor `xcodebuild` reports.
pub const CATALOG_URL: &str =
    "https://devimages-cdn.apple.com/downloads/xcode/simulators/index2.dvtdownloadableindex";

const IOS_RUNTIME_PREFIX: &str = "com.apple.CoreSimulator.SimRuntime.iOS-";

/// `"26.2"` → `[26, 2, 0]`, so versions order numerically and `"26"` equals `"26.0"`.
pub fn version_key(version: &str) -> [u32; 3] {
    let mut key = [0; 3];
    for (slot, part) in key.iter_mut().zip(version.trim().split('.')) {
        *slot = part
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0);
    }
    key
}

pub fn compare_versions(left: &str, right: &str) -> Ordering {
    version_key(left).cmp(&version_key(right))
}

/// `"26.2.0"` → `"26.2"`, `"18.0.1"` stays, the way the UI names iOS versions.
pub fn display_version(version: &str) -> String {
    let [major, minor, patch] = version_key(version);
    if patch == 0 {
        format!("{major}.{minor}")
    } else {
        format!("{major}.{minor}.{patch}")
    }
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-18-6` → `18.6`. Devices whose runtime was deleted are
/// still listed under that identifier, and it is the only place their iOS version survives.
pub fn runtime_version_from_identifier(identifier: &str) -> Option<String> {
    let version = identifier.strip_prefix(IOS_RUNTIME_PREFIX)?;
    Some(version.replace('-', "."))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProductFamily {
    IPhone,
    IPad,
}

impl ProductFamily {
    pub const ALL: [Self; 2] = [Self::IPhone, Self::IPad];

    pub fn label(self) -> &'static str {
        match self {
            Self::IPhone => "iPhone",
            Self::IPad => "iPad",
        }
    }

    fn parse(family: &str) -> Option<Self> {
        match family {
            "iPhone" => Some(Self::IPhone),
            "iPad" => Some(Self::IPad),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
}

/// A model the simulator can emulate, from `simctl list devicetypes`.
#[derive(Clone, Debug, PartialEq)]
pub struct SimDeviceType {
    pub identifier: String,
    pub name: String,
    pub family: ProductFamily,
    pub min_runtime: Option<String>,
    pub max_runtime: Option<String>,
    pub bundle_path: PathBuf,
    pub screen: Option<Screen>,
}

impl SimDeviceType {
    pub fn supports(&self, version: &str) -> bool {
        self.min_runtime
            .as_deref()
            .is_none_or(|min| compare_versions(version, min) != Ordering::Less)
            && self
                .max_runtime
                .as_deref()
                .is_none_or(|max| compare_versions(version, max) != Ordering::Greater)
    }

    /// `1206×2622 · @3x · iOS 26+`, the second line of a model card.
    pub fn screen_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(screen) = self.screen {
            parts.push(format!("{}×{}", screen.width, screen.height));
            parts.push(format!("@{}x", screen.scale));
        }
        if let Some(min) = &self.min_runtime {
            parts.push(format!("iOS {}+", short_version(min)));
        }
        parts.join(" · ")
    }
}

/// `"26.0.0"` → `"26"`, `"18.3.0"` → `"18.3"`: how a minimum iOS reads on a card.
fn short_version(version: &str) -> String {
    match version_key(version) {
        [major, 0, 0] => major.to_string(),
        [major, minor, 0] => format!("{major}.{minor}"),
        [major, minor, patch] => format!("{major}.{minor}.{patch}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimRuntime {
    pub identifier: String,
    pub version: String,
    pub build: String,
    pub supported_device_types: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimState {
    Booted,
    Booting,
    ShuttingDown,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    pub state: SimState,
    pub runtime_identifier: String,
    pub device_type_identifier: String,
    /// False when the runtime it was created with is no longer installed.
    pub is_available: bool,
    pub data_path: Option<PathBuf>,
    pub disk_bytes: u64,
}

impl SimDevice {
    pub fn is_booted(&self) -> bool {
        matches!(self.state, SimState::Booted)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, SimState::Booted | SimState::Booting)
    }

    pub fn runtime_version(&self) -> Option<String> {
        runtime_version_from_identifier(&self.runtime_identifier)
    }

    /// The device's folder, which holds `data/` and its `device.plist`.
    pub fn directory(&self) -> Option<PathBuf> {
        self.data_path
            .as_ref()
            .and_then(|data| data.parent())
            .map(PathBuf::from)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimctlInventory {
    pub devices: Vec<SimDevice>,
    pub runtimes: Vec<SimRuntime>,
    pub device_types: Vec<SimDeviceType>,
}

#[derive(Deserialize)]
struct SimctlList {
    #[serde(default)]
    devices: HashMap<String, Vec<SimctlDevice>>,
    #[serde(default)]
    runtimes: Vec<SimctlRuntime>,
    #[serde(default)]
    devicetypes: Vec<SimctlDeviceType>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(default)]
    is_available: bool,
    #[serde(default)]
    device_type_identifier: String,
    data_path: Option<PathBuf>,
    #[serde(default)]
    data_path_size: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlRuntime {
    identifier: String,
    version: String,
    #[serde(default)]
    buildversion: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    is_available: bool,
    #[serde(default)]
    supported_device_types: Vec<SimctlSupportedDeviceType>,
}

#[derive(Deserialize)]
struct SimctlSupportedDeviceType {
    identifier: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlDeviceType {
    identifier: String,
    name: String,
    #[serde(default)]
    product_family: String,
    min_runtime_version_string: Option<String>,
    max_runtime_version_string: Option<String>,
    #[serde(default)]
    bundle_path: PathBuf,
}

/// The iOS part of `simctl list -j devices runtimes devicetypes`. Device types come back in
/// simctl's order; screens are read separately from each bundle.
pub fn parse_simctl_list(json: &[u8]) -> Result<SimctlInventory> {
    let list: SimctlList =
        serde_json::from_slice(json).context("xcrun simctl devolveu um JSON inesperado")?;
    let mut devices: Vec<SimDevice> = list
        .devices
        .into_iter()
        .filter(|(runtime, _)| runtime.starts_with(IOS_RUNTIME_PREFIX))
        .flat_map(|(runtime, devices)| {
            devices.into_iter().map(move |device| SimDevice {
                state: match device.state.as_str() {
                    "Booted" => SimState::Booted,
                    "Booting" => SimState::Booting,
                    "Shutting Down" => SimState::ShuttingDown,
                    _ => SimState::Shutdown,
                },
                udid: device.udid,
                name: device.name,
                runtime_identifier: runtime.clone(),
                device_type_identifier: device.device_type_identifier,
                is_available: device.is_available,
                data_path: device.data_path,
                disk_bytes: device.data_path_size,
            })
        })
        .collect();
    devices.sort_by(|left, right| {
        right
            .is_running()
            .cmp(&left.is_running())
            .then_with(|| left.name.cmp(&right.name))
    });
    let runtimes = list
        .runtimes
        .into_iter()
        .filter(|runtime| runtime.platform == "iOS" && runtime.is_available)
        .map(|runtime| SimRuntime {
            identifier: runtime.identifier,
            version: runtime.version,
            build: runtime.buildversion,
            supported_device_types: runtime
                .supported_device_types
                .into_iter()
                .map(|device_type| device_type.identifier)
                .collect(),
        })
        .collect();
    let device_types = list
        .devicetypes
        .into_iter()
        .filter_map(|device_type| {
            Some(SimDeviceType {
                family: ProductFamily::parse(&device_type.product_family)?,
                identifier: device_type.identifier,
                name: device_type.name,
                min_runtime: device_type.min_runtime_version_string,
                max_runtime: device_type.max_runtime_version_string,
                bundle_path: device_type.bundle_path,
                screen: None,
            })
        })
        .collect();
    Ok(SimctlInventory {
        devices,
        runtimes,
        device_types,
    })
}

/// The screen a device type's `capabilities.plist` describes, in pixels.
pub fn parse_screen(capabilities: &plist::Value) -> Option<Screen> {
    let dimensions = capabilities
        .as_dictionary()?
        .get("capabilities")?
        .as_dictionary()?
        .get("ScreenDimensionsCapability")?
        .as_dictionary()?;
    // Some keys are stored as reals (`main-screen-scale` is `3.0`).
    let number = |key: &str| {
        let value = dimensions.get(key)?;
        value
            .as_unsigned_integer()
            .and_then(|value| u32::try_from(value).ok())
            .or_else(|| value.as_real().map(|value| value.round() as u32))
    };
    Some(Screen {
        width: number("main-screen-width")?,
        height: number("main-screen-height")?,
        scale: number("main-screen-scale").unwrap_or(1),
    })
}

/// A runtime disk image from `simctl runtime list -j`: what deleting it frees and the id
/// `simctl runtime delete` takes.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeImage {
    pub image_identifier: String,
    pub build: String,
    pub disk_bytes: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlRuntimeImage {
    identifier: String,
    #[serde(default)]
    build: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    platform_identifier: String,
}

pub fn parse_runtime_images(json: &[u8]) -> Result<Vec<RuntimeImage>> {
    let images: HashMap<String, SimctlRuntimeImage> = serde_json::from_slice(json)
        .context("xcrun simctl runtime list devolveu um JSON inesperado")?;
    Ok(images
        .into_values()
        .filter(|image| image.platform_identifier == "com.apple.platform.iphonesimulator")
        .map(|image| RuntimeImage {
            image_identifier: image.identifier,
            build: image.build,
            disk_bytes: image.size_bytes,
        })
        .collect())
}

/// The iOS SDK the selected Xcode builds against, from `simctl runtime match list -j`: the
/// runtime it wants is the one Apple's catalog maps that SDK build to.
#[derive(Clone, Debug, PartialEq)]
pub struct SdkRequest {
    pub sdk_identifier: String,
    pub sdk_version: String,
    pub sdk_build: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlRuntimeMatch {
    #[serde(default)]
    platform: String,
    #[serde(default)]
    sdk_version: String,
    #[serde(default)]
    sdk_build: String,
}

pub fn parse_runtime_match(json: &[u8]) -> Result<Option<SdkRequest>> {
    let matches: HashMap<String, SimctlRuntimeMatch> = serde_json::from_slice(json)
        .context("xcrun simctl runtime match list devolveu um JSON inesperado")?;
    Ok(matches
        .into_iter()
        .find(|(_, entry)| entry.platform == "com.apple.platform.iphoneos")
        .map(|(sdk_identifier, entry)| SdkRequest {
            sdk_identifier,
            sdk_version: entry.sdk_version,
            sdk_build: entry.sdk_build,
        }))
}

/// `xcodebuild -version`: `Xcode 27.0` / `Build version 27A266a`.
pub fn parse_xcode_version(output: &str) -> Option<(String, String)> {
    let mut lines = output.lines();
    let version = lines.next()?.strip_prefix("Xcode ")?.trim().to_string();
    let build = lines
        .find_map(|line| line.strip_prefix("Build version "))
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((version, build))
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogRuntime {
    pub version: String,
    pub build: String,
    pub download_bytes: u64,
    /// Betas and release candidates, which step D keeps behind its "Betas" segment.
    pub prerelease: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// Newest first, one per build.
    pub runtimes: Vec<CatalogRuntime>,
    sdk_mappings: Vec<SdkMapping>,
}

#[derive(Clone, Debug, PartialEq)]
struct SdkMapping {
    sdk_identifier: String,
    sdk_build: String,
    simulator_build: String,
}

impl Catalog {
    pub fn runtime_for_build(&self, build: &str) -> Option<&CatalogRuntime> {
        self.runtimes.iter().find(|runtime| runtime.build == build)
    }

    /// The runtime Xcode asks for, from the SDK it builds against.
    pub fn requested_runtime(&self, request: &SdkRequest) -> Option<&CatalogRuntime> {
        let build = self
            .sdk_mappings
            .iter()
            .rev()
            .find(|mapping| {
                mapping.sdk_identifier == request.sdk_identifier
                    && mapping.sdk_build == request.sdk_build
            })
            .map(|mapping| mapping.simulator_build.as_str())?;
        self.runtime_for_build(build)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadableIndex {
    #[serde(default)]
    downloadables: Vec<Downloadable>,
    #[serde(default)]
    sdk_to_simulator_mappings: Vec<IndexSdkMapping>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Downloadable {
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    file_size: u64,
    #[serde(default)]
    architectures: Vec<String>,
    simulator_version: Option<IndexSimulatorVersion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexSimulatorVersion {
    version: String,
    build_update: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexSdkMapping {
    sdk_identifier: String,
    sdk_build_update: String,
    simulator_build_update: String,
}

/// The iOS runtimes of Apple's index. Recent runtimes come once per architecture variant; on
/// Apple silicon the arm64-only one is what `-architectureVariant arm64` downloads.
pub fn parse_catalog(bytes: &[u8], apple_silicon: bool) -> Result<Catalog> {
    let index: DownloadableIndex = plist::from_bytes(bytes)
        .context("o índice de runtimes da Apple veio num formato inesperado")?;
    let mut runtimes: Vec<CatalogRuntime> = Vec::new();
    let mut is_arm64_only: Vec<bool> = Vec::new();
    for downloadable in index.downloadables {
        if downloadable.platform != "com.apple.platform.iphoneos" {
            continue;
        }
        let Some(version) = downloadable.simulator_version else {
            continue;
        };
        let arm64_only = downloadable.architectures == ["arm64"];
        let lower_name = downloadable.name.to_lowercase();
        let runtime = CatalogRuntime {
            version: version.version,
            build: version.build_update,
            download_bytes: downloadable.file_size,
            prerelease: lower_name.contains("beta") || lower_name.contains("release candidate"),
        };
        match runtimes.iter().position(|existing| {
            existing.version == runtime.version && existing.prerelease == runtime.prerelease
        }) {
            Some(index) => {
                // Same release twice: keep the newer build, and on Apple silicon prefer the
                // arm64-only variant of it.
                let existing = &runtimes[index];
                let newer = runtime.build > existing.build;
                let better_variant = runtime.build == existing.build
                    && apple_silicon
                    && arm64_only
                    && !is_arm64_only[index];
                if newer || better_variant {
                    runtimes[index] = runtime;
                    is_arm64_only[index] = arm64_only;
                }
            }
            None => {
                runtimes.push(runtime);
                is_arm64_only.push(arm64_only);
            }
        }
    }
    runtimes.sort_by(|left, right| {
        compare_versions(&right.version, &left.version)
            .then_with(|| left.prerelease.cmp(&right.prerelease))
    });
    Ok(Catalog {
        runtimes,
        sdk_mappings: index
            .sdk_to_simulator_mappings
            .into_iter()
            .map(|mapping| SdkMapping {
                sdk_identifier: mapping.sdk_identifier,
                sdk_build: mapping.sdk_build_update,
                simulator_build: mapping.simulator_build_update,
            })
            .collect(),
    })
}

/// What a line of `xcodebuild -downloadPlatform` output says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DownloadLine {
    /// Share of the download done, 0 to 1.
    Downloading(f32),
    Installing,
}

/// `Downloading iOS 27.0 Simulator (24A434): 12.5% (1.03 GB of 8.25 GB)` and the
/// `Installing …` lines that follow once the image is down.
pub fn parse_download_line(line: &str) -> Option<DownloadLine> {
    let line = line.trim();
    if line.starts_with("Installing")
        || line.starts_with("Verifying")
        || line.starts_with("Mounting")
    {
        return Some(DownloadLine::Installing);
    }
    if !line.starts_with("Downloading") {
        return None;
    }
    let percent_end = line.find('%')?;
    let number_start = line[..percent_end]
        .rfind(|character: char| !(character.is_ascii_digit() || character == '.'))
        .map_or(0, |index| index + 1);
    let percent: f32 = line[number_start..percent_end].parse().ok()?;
    Some(DownloadLine::Downloading((percent / 100.).clamp(0., 1.)))
}

/// A language the new-simulator form offers: what goes in `AppleLanguages` and `AppleLocale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimLanguage {
    pub language: &'static str,
    pub locale: &'static str,
    pub label: &'static str,
}

pub const LANGUAGES: [SimLanguage; 7] = [
    SimLanguage {
        language: "pt-BR",
        locale: "pt_BR",
        label: "Português (Brasil)",
    },
    SimLanguage {
        language: "en-US",
        locale: "en_US",
        label: "English (US)",
    },
    SimLanguage {
        language: "es-ES",
        locale: "es_ES",
        label: "Español (España)",
    },
    SimLanguage {
        language: "es-419",
        locale: "es_MX",
        label: "Español (Latinoamérica)",
    },
    SimLanguage {
        language: "pt-PT",
        locale: "pt_PT",
        label: "Português (Portugal)",
    },
    SimLanguage {
        language: "fr-FR",
        locale: "fr_FR",
        label: "Français",
    },
    SimLanguage {
        language: "de-DE",
        locale: "de_DE",
        label: "Deutsch",
    },
];

impl SimLanguage {
    pub fn for_language(language: &str) -> Option<Self> {
        LANGUAGES
            .into_iter()
            .find(|option| option.language.eq_ignore_ascii_case(language))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Claro",
            Self::Dark => "Escuro",
        }
    }

    pub fn simctl_argument(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// `UserInterfaceStyleMode` in the device's
    /// `com.apple.uikitservices.userInterfaceStyleMode.plist`, which is what `simctl ui
    /// appearance` writes and the only way to set it while the device is off.
    pub fn style_mode(self) -> u64 {
        match self {
            Self::Light => 1,
            Self::Dark => 2,
        }
    }

    pub fn from_style_mode(mode: u64) -> Self {
        if mode == 2 { Self::Dark } else { Self::Light }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_versions_numerically() {
        assert_eq!(compare_versions("26.10", "26.2"), Ordering::Greater);
        assert_eq!(compare_versions("26", "26.0.0"), Ordering::Equal);
        assert_eq!(display_version("26.2.0"), "26.2");
        assert_eq!(display_version("18.0.1"), "18.0.1");
        assert_eq!(
            runtime_version_from_identifier("com.apple.CoreSimulator.SimRuntime.iOS-18-6")
                .as_deref(),
            Some("18.6")
        );
    }

    #[test]
    fn parses_simctl_list() {
        let json = br#"{
          "devicetypes": [
            {"productFamily": "iPhone", "bundlePath": "/p/iPhone 17 Pro.simdevicetype",
             "identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro",
             "minRuntimeVersionString": "26.0.0", "maxRuntimeVersionString": "65535.255.255",
             "name": "iPhone 17 Pro"},
            {"productFamily": "Apple Watch", "bundlePath": "/p/w.simdevicetype",
             "identifier": "watch", "name": "Apple Watch"}
          ],
          "runtimes": [
            {"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-2", "version": "26.2",
             "buildversion": "23C54", "platform": "iOS", "isAvailable": true,
             "supportedDeviceTypes": [{"identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"}]},
            {"identifier": "com.apple.CoreSimulator.SimRuntime.watchOS-26-0", "version": "26.0",
             "buildversion": "x", "platform": "watchOS", "isAvailable": true}
          ],
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-2": [
              {"udid": "B", "name": "iPhone Air", "state": "Shutdown", "isAvailable": true,
               "deviceTypeIdentifier": "air", "dataPath": "/d/B/data", "dataPathSize": 840},
              {"udid": "A", "name": "iPhone 17 Pro", "state": "Booted", "isAvailable": true,
               "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro",
               "dataPath": "/d/A/data", "dataPathSize": 1602723840}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-18-6": [
              {"udid": "C", "name": "iPhone 16e", "state": "Shutdown", "isAvailable": false,
               "availabilityError": "runtime profile not found", "deviceTypeIdentifier": "e"}
            ],
            "com.apple.CoreSimulator.SimRuntime.watchOS-26-0": [
              {"udid": "W", "name": "Watch", "state": "Shutdown", "isAvailable": true}
            ]
          }
        }"#;
        let inventory = parse_simctl_list(json).unwrap();
        let names: Vec<&str> = inventory
            .devices
            .iter()
            .map(|device| device.name.as_str())
            .collect();
        assert_eq!(names, ["iPhone 17 Pro", "iPhone 16e", "iPhone Air"]);
        let missing = &inventory.devices[1];
        assert!(!missing.is_available);
        assert_eq!(missing.runtime_version().as_deref(), Some("18.6"));
        assert_eq!(
            inventory.devices[0].directory(),
            Some(PathBuf::from("/d/A"))
        );
        assert_eq!(inventory.runtimes.len(), 1);
        assert_eq!(inventory.runtimes[0].build, "23C54");
        assert_eq!(inventory.device_types.len(), 1);
        let pro = &inventory.device_types[0];
        assert!(pro.supports("26.2"));
        assert!(!pro.supports("18.6"));
    }

    #[test]
    fn labels_device_type_screens() {
        let device_type = SimDeviceType {
            identifier: "pro".into(),
            name: "iPhone 17 Pro".into(),
            family: ProductFamily::IPhone,
            min_runtime: Some("26.0.0".into()),
            max_runtime: None,
            bundle_path: PathBuf::new(),
            screen: Some(Screen {
                width: 1206,
                height: 2622,
                scale: 3,
            }),
        };
        assert_eq!(device_type.screen_label(), "1206×2622 · @3x · iOS 26+");
    }

    #[test]
    fn parses_catalog_and_requested_runtime() {
        let index = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>downloadables</key><array>
  <dict><key>name</key><string>iOS 27.0 Simulator Runtime</string>
    <key>platform</key><string>com.apple.platform.iphoneos</string>
    <key>fileSize</key><integer>8053150208</integer>
    <key>architectures</key><array><string>arm64</string></array>
    <key>simulatorVersion</key><dict><key>version</key><string>27.0</string><key>buildUpdate</key><string>24A434</string></dict></dict>
  <dict><key>name</key><string>iOS 27.0 Simulator Runtime</string>
    <key>platform</key><string>com.apple.platform.iphoneos</string>
    <key>fileSize</key><integer>9000000000</integer>
    <key>architectures</key><array><string>arm64</string><string>x86_64</string></array>
    <key>simulatorVersion</key><dict><key>version</key><string>27.0</string><key>buildUpdate</key><string>24A434</string></dict></dict>
  <dict><key>name</key><string>iOS 27.2 beta 4 Simulator Runtime</string>
    <key>platform</key><string>com.apple.platform.iphoneos</string>
    <key>fileSize</key><integer>8254478336</integer>
    <key>simulatorVersion</key><dict><key>version</key><string>27.2</string><key>buildUpdate</key><string>24B5110a</string></dict></dict>
  <dict><key>name</key><string>iOS 18.6 Simulator Runtime</string>
    <key>platform</key><string>com.apple.platform.iphoneos</string>
    <key>fileSize</key><integer>7700000000</integer>
    <key>simulatorVersion</key><dict><key>version</key><string>18.6</string><key>buildUpdate</key><string>22G86</string></dict></dict>
  <dict><key>name</key><string>watchOS 27.0 Simulator Runtime</string>
    <key>platform</key><string>com.apple.platform.watchos</string>
    <key>simulatorVersion</key><dict><key>version</key><string>27.0</string><key>buildUpdate</key><string>w</string></dict></dict>
</array>
<key>sdkToSimulatorMappings</key><array>
  <dict><key>sdkIdentifier</key><string>iphoneos27.0</string><key>sdkBuildUpdate</key><string>24A430</string><key>simulatorBuildUpdate</key><string>24A434</string></dict>
</array>
</dict></plist>"#;
        let catalog = parse_catalog(index.as_bytes(), true).unwrap();
        let versions: Vec<(&str, bool)> = catalog
            .runtimes
            .iter()
            .map(|runtime| (runtime.version.as_str(), runtime.prerelease))
            .collect();
        assert_eq!(versions, [("27.2", true), ("27.0", false), ("18.6", false)]);
        assert_eq!(catalog.runtimes[1].download_bytes, 8053150208);
        let request = SdkRequest {
            sdk_identifier: "iphoneos27.0".into(),
            sdk_version: "27.0".into(),
            sdk_build: "24A430".into(),
        };
        assert_eq!(
            catalog
                .requested_runtime(&request)
                .map(|runtime| runtime.build.as_str()),
            Some("24A434")
        );
    }

    #[test]
    fn parses_download_lines() {
        assert_eq!(
            parse_download_line(
                "Downloading iOS 27.0 Simulator (24A434): 12.5% (1.03 GB of 8.25 GB)"
            ),
            Some(DownloadLine::Downloading(0.125))
        );
        assert_eq!(
            parse_download_line("Installing iOS 27.0 Simulator (24A434)"),
            Some(DownloadLine::Installing)
        );
        assert_eq!(parse_download_line("Finding content..."), None);
    }

    #[test]
    fn parses_xcode_version_and_runtime_match() {
        assert_eq!(
            parse_xcode_version("Xcode 27.0\nBuild version 27A266a\n"),
            Some(("27.0".to_string(), "27A266a".to_string()))
        );
        let json = br#"{
          "appletvos27.0": {"platform": "com.apple.platform.appletvos", "sdkVersion": "27.0", "sdkBuild": "24J360"},
          "iphoneos27.0": {"platform": "com.apple.platform.iphoneos", "sdkVersion": "27.0", "sdkBuild": "24A430"}
        }"#;
        assert_eq!(
            parse_runtime_match(json).unwrap(),
            Some(SdkRequest {
                sdk_identifier: "iphoneos27.0".into(),
                sdk_version: "27.0".into(),
                sdk_build: "24A430".into(),
            })
        );
    }
}
