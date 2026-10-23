//! What can be installed and created: system images, SDK components and device definitions.
//!
//! `sdkmanager --list` prints neither download sizes nor screen sizes, so the system images and
//! components come from the XML repository `sdkmanager` itself reads, and the device definitions
//! from the XML files bundled in the command-line tools' `sdklib` jar.

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use quick_xml::events::Event;
use std::path::{Path, PathBuf};

const REPOSITORY_BASE_URL: &str = "https://dl.google.com/android/repository";

/// A system image `sdkmanager` can install, identified the way its package path spells it:
/// `system-images;android-<platform>;<tag>;<abi>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SystemImageId {
    /// What follows `android-` in the package path: `36`, `36.1` or `37.0`.
    pub platform: String,
    /// The repository's tag id, e.g. `google_apis_playstore` or `android-wear-signed`.
    pub tag: String,
    pub abi: String,
}

impl SystemImageId {
    pub fn new(platform: impl Into<String>, flavor: ImageFlavor, abi: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            tag: flavor.tag_id().to_string(),
            abi: abi.into(),
        }
    }

    pub fn package(&self) -> String {
        format!(
            "system-images;android-{};{};{}",
            self.platform, self.tag, self.abi
        )
    }

    /// The directory under the SDK root the image is installed to.
    pub fn relative_dir(&self) -> PathBuf {
        Path::new("system-images")
            .join(format!("android-{}", self.platform))
            .join(&self.tag)
            .join(&self.abi)
    }

    pub fn parse_package(package: &str) -> Option<Self> {
        let mut parts = package.split(';');
        if parts.next()? != "system-images" {
            return None;
        }
        let platform = parts.next()?.strip_prefix("android-")?;
        let tag = parts.next()?;
        let abi = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        Self::from_parts(platform, tag, abi)
    }

    /// Parses an AVD's `image.sysdir.1`, e.g. `system-images/android-35/google_apis/arm64-v8a/`.
    pub fn parse_sysdir(sysdir: &str) -> Option<Self> {
        let mut parts = sysdir.trim().trim_end_matches('/').split('/');
        if parts.next()? != "system-images" {
            return None;
        }
        let platform = parts.next()?.strip_prefix("android-")?;
        let tag = parts.next()?;
        let abi = parts.next()?;
        Self::from_parts(platform, tag, abi)
    }

    fn from_parts(platform: &str, tag: &str, abi: &str) -> Option<Self> {
        if platform.is_empty() || tag.is_empty() || abi.is_empty() {
            return None;
        }
        Some(Self {
            platform: platform.to_string(),
            tag: tag.to_string(),
            abi: abi.to_string(),
        })
    }

    pub fn flavor(&self) -> Option<ImageFlavor> {
        ImageFlavor::from_tag_id(&self.tag)
    }

    /// The platform as a sortable `(major, minor)`, or `None` for previews such as `CANARY`,
    /// `36-ext19` or `37.2-beta1`.
    pub fn api(&self) -> Option<(u32, u32)> {
        parse_platform(&self.platform)
    }

    /// `API 36`, or `API 36.1` for a minor release. `37.0` reads as `API 37`.
    pub fn api_label(&self) -> String {
        match self.api() {
            Some((major, 0)) => format!("API {major}"),
            Some((major, minor)) => format!("API {major}.{minor}"),
            None => format!("API {}", self.platform),
        }
    }

    /// `Android 16`, `Android 12L`.
    pub fn android_version(&self) -> String {
        match self.api() {
            Some((major, _)) => android_version_for_api(major),
            None => format!("Android {}", self.platform),
        }
    }

    pub fn codename(&self) -> Option<&'static str> {
        codename_for_api(self.api()?.0)
    }

    /// `Android 15 · API 35`, as the device picker shows it.
    pub fn short_label(&self) -> String {
        format!("{} · {}", self.android_version(), self.api_label())
    }
}

fn parse_platform(platform: &str) -> Option<(u32, u32)> {
    let (major, minor) = match platform.split_once('.') {
        Some((major, minor)) => (major, minor),
        None => (platform, "0"),
    };
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn android_version_for_api(api: u32) -> String {
    let version = match api {
        21 => "5.0",
        22 => "5.1",
        23 => "6.0",
        24 => "7.0",
        25 => "7.1",
        26 => "8.0",
        27 => "8.1",
        28 => "9",
        29 => "10",
        30 => "11",
        31 => "12",
        32 => "12L",
        // From Android 13 on, the version is the API level minus 20.
        api if api >= 33 => return format!("Android {}", api - 20),
        _ => return format!("API {api}"),
    };
    format!("Android {version}")
}

fn codename_for_api(api: u32) -> Option<&'static str> {
    Some(match api {
        21 | 22 => "Lollipop",
        23 => "Marshmallow",
        24 | 25 => "Nougat",
        26 | 27 => "Oreo",
        28 => "Pie",
        29 => "Quince Tart",
        30 => "Red Velvet Cake",
        31 => "Snow Cone",
        32 => "Snow Cone v2",
        33 => "Tiramisu",
        34 => "Upside Down Cake",
        35 => "Vanilla Ice Cream",
        36 => "Baklava",
        _ => return None,
    })
}

/// The kinds of system image the device manager offers, grouped by the device they run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFlavor {
    GooglePlay,
    GoogleApis,
    Aosp,
    GoogleTv,
    AndroidTv,
    WearOs,
}

impl ImageFlavor {
    pub const HANDHELD: &[ImageFlavor] = &[Self::GooglePlay, Self::GoogleApis, Self::Aosp];
    pub const TV: &[ImageFlavor] = &[Self::GoogleTv, Self::AndroidTv];
    pub const WEAR: &[ImageFlavor] = &[Self::WearOs];

    pub fn label(self) -> &'static str {
        match self {
            Self::GooglePlay => "Google Play",
            Self::GoogleApis => "Google APIs",
            Self::Aosp => "AOSP",
            Self::GoogleTv => "Google TV",
            Self::AndroidTv => "Android TV",
            Self::WearOs => "Wear OS",
        }
    }

    /// The tag id new images of this flavor are installed with.
    pub fn tag_id(self) -> &'static str {
        match self {
            Self::GooglePlay => "google_apis_playstore",
            Self::GoogleApis => "google_apis",
            Self::Aosp => "default",
            Self::GoogleTv => "google-tv",
            Self::AndroidTv => "android-tv",
            Self::WearOs => "android-wear",
        }
    }

    pub fn from_tag_id(tag: &str) -> Option<Self> {
        Some(match tag {
            "google_apis_playstore" => Self::GooglePlay,
            "google_apis" => Self::GoogleApis,
            "default" => Self::Aosp,
            "google-tv" => Self::GoogleTv,
            "android-tv" => Self::AndroidTv,
            // Wear OS 6 and later are only published signed.
            "android-wear" | "android-wear-signed" => Self::WearOs,
            _ => return None,
        })
    }

    pub fn has_play_store(self) -> bool {
        matches!(self, Self::GooglePlay | Self::GoogleTv | Self::WearOs)
    }

    pub fn for_category(category: DeviceCategory) -> &'static [ImageFlavor] {
        match category {
            DeviceCategory::Phone | DeviceCategory::Tablet | DeviceCategory::Foldable => {
                Self::HANDHELD
            }
            DeviceCategory::Tv => Self::TV,
            DeviceCategory::Wear => Self::WEAR,
        }
    }

    /// The repository directory that lists this flavor's images.
    fn repository_directory(self) -> &'static str {
        match self {
            Self::GooglePlay => "google_apis_playstore",
            Self::GoogleApis => "google_apis",
            Self::Aosp => "android",
            Self::GoogleTv => "google-tv",
            Self::AndroidTv => "android-tv",
            Self::WearOs => "android-wear",
        }
    }

    const ALL: [ImageFlavor; 6] = [
        Self::GooglePlay,
        Self::GoogleApis,
        Self::Aosp,
        Self::GoogleTv,
        Self::AndroidTv,
        Self::WearOs,
    ];
}

/// A system image the repository offers for this Mac's ABI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogImage {
    pub id: SystemImageId,
    pub download_bytes: u64,
}

/// The newest stable version of an SDK component the repository offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogComponent {
    pub kind: SdkComponentKind,
    pub version: String,
    pub download_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SdkCatalog {
    /// Newest first.
    pub images: Vec<CatalogImage>,
    pub components: Vec<CatalogComponent>,
}

impl SdkCatalog {
    pub fn image(&self, id: &SystemImageId) -> Option<&CatalogImage> {
        self.images.iter().find(|image| &image.id == id)
    }

    pub fn component(&self, kind: SdkComponentKind) -> Option<&CatalogComponent> {
        self.components
            .iter()
            .find(|component| component.kind == kind)
    }
}

/// The SDK packages the device manager keeps up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SdkComponentKind {
    Emulator,
    PlatformTools,
    CommandLineTools,
}

impl SdkComponentKind {
    pub const ALL: [SdkComponentKind; 3] =
        [Self::Emulator, Self::PlatformTools, Self::CommandLineTools];

    pub fn label(self) -> &'static str {
        match self {
            Self::Emulator => "Android Emulator",
            Self::PlatformTools => "Platform-Tools (adb)",
            Self::CommandLineTools => "Command-line Tools",
        }
    }

    /// The package `sdkmanager --install` takes to update it.
    pub fn package(self) -> &'static str {
        match self {
            Self::Emulator => "emulator",
            Self::PlatformTools => "platform-tools",
            Self::CommandLineTools => "cmdline-tools;latest",
        }
    }

    /// Where its `source.properties` lives, relative to the SDK root.
    pub fn relative_dir(self) -> &'static str {
        match self {
            Self::Emulator => "emulator",
            Self::PlatformTools => "platform-tools",
            Self::CommandLineTools => "cmdline-tools/latest",
        }
    }
}

pub(crate) fn system_image_catalog_urls() -> impl Iterator<Item = (ImageFlavor, String)> {
    ImageFlavor::ALL.into_iter().map(|flavor| {
        (
            flavor,
            format!(
                "{REPOSITORY_BASE_URL}/sys-img/{}/sys-img2-3.xml",
                flavor.repository_directory()
            ),
        )
    })
}

pub(crate) fn repository_url() -> String {
    format!("{REPOSITORY_BASE_URL}/repository2-3.xml")
}

/// One `<remotePackage>` of a repository XML, with only the fields the catalog reads.
#[derive(Default)]
struct RemotePackage {
    path: String,
    channel: Option<String>,
    revision: [Option<String>; 3],
    archives: Vec<RemoteArchive>,
}

#[derive(Default)]
struct RemoteArchive {
    size: u64,
    host_os: Option<String>,
    host_arch: Option<String>,
}

fn parse_remote_packages(xml: &str) -> Result<Vec<RemotePackage>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut packages = Vec::new();
    let mut current: Option<RemotePackage> = None;
    let mut path: Vec<String> = Vec::new();
    loop {
        match reader
            .read_event()
            .context("lendo o XML do repositório Android")?
        {
            Event::Start(element) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "remotePackage" => {
                        let package_path = element
                            .try_get_attribute("path")
                            .ok()
                            .flatten()
                            .map(|attribute| String::from_utf8_lossy(&attribute.value).into_owned())
                            .unwrap_or_default();
                        current = Some(RemotePackage {
                            path: package_path,
                            ..RemotePackage::default()
                        });
                    }
                    "archive" => {
                        if let Some(package) = current.as_mut() {
                            package.archives.push(RemoteArchive::default());
                        }
                    }
                    _ => {}
                }
                path.push(name);
            }
            Event::Empty(element) => {
                if element.local_name().as_ref() == b"channelRef"
                    && let Some(package) = current.as_mut()
                {
                    package.channel = element
                        .try_get_attribute("ref")
                        .ok()
                        .flatten()
                        .map(|attribute| String::from_utf8_lossy(&attribute.value).into_owned());
                }
            }
            Event::End(element) => {
                path.pop();
                if element.local_name().as_ref() == b"remotePackage"
                    && let Some(package) = current.take()
                {
                    packages.push(package);
                }
            }
            Event::Text(text) => {
                let Some(package) = current.as_mut() else {
                    continue;
                };
                let text = text.decode().unwrap_or_default().trim().to_string();
                let parent = path.iter().rev().nth(1).map(String::as_str);
                match (parent, path.last().map(String::as_str)) {
                    (Some("revision"), Some("major")) => package.revision[0] = Some(text),
                    (Some("revision"), Some("minor")) => package.revision[1] = Some(text),
                    (Some("revision"), Some("micro")) => package.revision[2] = Some(text),
                    (Some("complete"), Some("size")) => {
                        if let Some(archive) = package.archives.last_mut() {
                            archive.size = text.parse().unwrap_or(0);
                        }
                    }
                    (Some("archive"), Some("host-os")) => {
                        if let Some(archive) = package.archives.last_mut() {
                            archive.host_os = Some(text);
                        }
                    }
                    (Some("archive"), Some("host-arch")) => {
                        if let Some(archive) = package.archives.last_mut() {
                            archive.host_arch = Some(text);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(packages)
}

/// Reads one flavor's `sys-img2-3.xml`, keeping the released images for `abi`. Previews and
/// extension builds (`android-36-ext19`, `android-CANARY`, `google_apis_playstore_ps16k`) are
/// left out, as Android Studio's default list does.
pub(crate) fn parse_system_image_catalog(
    xml: &str,
    flavor: ImageFlavor,
    abi: &str,
) -> Result<Vec<CatalogImage>> {
    Ok(parse_remote_packages(xml)?
        .into_iter()
        .filter_map(|package| {
            let id = SystemImageId::parse_package(&package.path)?;
            if id.abi != abi || id.flavor() != Some(flavor) || id.api().is_none() {
                return None;
            }
            let download_bytes = package.archives.first().map_or(0, |archive| archive.size);
            Some(CatalogImage { id, download_bytes })
        })
        .collect())
}

/// Reads `repository2-3.xml` for the newest stable emulator, platform-tools and command-line
/// tools for this Mac.
pub(crate) fn parse_component_catalog(xml: &str, host_arch: &str) -> Result<Vec<CatalogComponent>> {
    let mut newest: Vec<(CatalogComponent, Vec<u64>)> = Vec::new();
    for package in parse_remote_packages(xml)? {
        if package
            .channel
            .as_deref()
            .is_some_and(|channel| channel != "channel-0")
        {
            continue;
        }
        let kind = match package.path.as_str() {
            "emulator" => SdkComponentKind::Emulator,
            "platform-tools" => SdkComponentKind::PlatformTools,
            path if path.starts_with("cmdline-tools;") => SdkComponentKind::CommandLineTools,
            _ => continue,
        };
        let Some(archive) = package.archives.iter().find(|archive| {
            archive.host_os.as_deref().is_none_or(|os| os == "macosx")
                && archive
                    .host_arch
                    .as_deref()
                    .is_none_or(|arch| arch == host_arch)
        }) else {
            continue;
        };
        let parts: Vec<String> = package.revision.iter().flatten().cloned().collect();
        if parts.is_empty() {
            continue;
        }
        let key = version_key(&parts.join("."));
        let candidate = CatalogComponent {
            kind,
            version: parts.join("."),
            download_bytes: archive.size,
        };
        match newest
            .iter_mut()
            .find(|(existing, _)| existing.kind == kind)
        {
            Some((existing, existing_key)) => {
                if key > *existing_key {
                    *existing = candidate;
                    *existing_key = key;
                }
            }
            None => newest.push((candidate, key)),
        }
    }
    Ok(newest.into_iter().map(|(component, _)| component).collect())
}

/// `35.3.11` → `[35, 3, 11]`, so versions compare numerically. Non-numeric parts count as zero.
pub fn version_key(version: &str) -> Vec<u64> {
    version
        .split(['.', '-', ' '])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Reads `Pkg.Revision` from a package's `source.properties`.
pub(crate) fn parse_package_revision(source_properties: &str) -> Option<String> {
    source_properties.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "Pkg.Revision").then(|| value.trim().to_string())
    })
}

/// What kind of device a hardware profile describes, as the new-device wizard groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceCategory {
    Phone,
    Tablet,
    Foldable,
    Tv,
    Wear,
}

impl DeviceCategory {
    pub const ALL: [DeviceCategory; 5] = [
        Self::Phone,
        Self::Tablet,
        Self::Foldable,
        Self::Tv,
        Self::Wear,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Phone => "Celular",
            Self::Tablet => "Tablet",
            Self::Foldable => "Dobrável",
            Self::Tv => "TV",
            Self::Wear => "Wear OS",
        }
    }
}

/// A device definition `avdmanager` can create an AVD from.
#[derive(Clone, Debug, PartialEq)]
pub struct AndroidDeviceProfile {
    /// Identifier passed to `avdmanager create avd --device`.
    pub id: String,
    pub name: String,
    pub category: DeviceCategory,
    pub diagonal_inches: Option<f32>,
    /// Portrait `(width, height)` in pixels.
    pub resolution: Option<(u32, u32)>,
}

impl AndroidDeviceProfile {
    /// `6,3" · 1080×2424`, with the decimal comma the rest of the UI uses.
    pub fn screen_label(&self) -> Option<String> {
        let (width, height) = self.resolution?;
        let resolution = format!("{width}×{height}");
        Some(match self.diagonal_inches {
            Some(inches) => format!(
                "{}\" · {resolution}",
                format!("{inches:.1}").replace('.', ",")
            ),
            None => resolution,
        })
    }
}

/// The definition files inside the `sdklib` jar, in the order Android Studio lists them.
const DEVICE_DEFINITION_FILES: &[&str] = &[
    "com/android/sdklib/devices/nexus.xml",
    "com/android/sdklib/devices/devices.xml",
    "com/android/sdklib/devices/tv.xml",
    "com/android/sdklib/devices/wear.xml",
];

/// Reads the device definitions bundled with the command-line tools, without starting a JVM.
pub(crate) async fn read_device_definitions(
    cmdline_tools_dir: &Path,
) -> Result<Vec<AndroidDeviceProfile>> {
    let jar = cmdline_tools_dir.join("lib/sdklib/sdklib.core.jar");
    let file = smol::fs::File::open(&jar)
        .await
        .with_context(|| format!("abrindo {}", jar.display()))?;
    let mut reader =
        async_zip::base::read::seek::ZipFileReader::new(futures::io::BufReader::new(file))
            .await
            .context("lendo o jar do sdklib")?;
    let entries: Vec<(usize, String)> = reader
        .file()
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| Some((index, entry.filename().as_str().ok()?.to_string())))
        .collect();
    let mut profiles = Vec::new();
    for file_name in DEVICE_DEFINITION_FILES {
        let Some((index, _)) = entries.iter().find(|(_, name)| name == file_name) else {
            continue;
        };
        let mut entry = reader
            .reader_with_entry(*index)
            .await
            .with_context(|| format!("lendo {file_name}"))?;
        let mut xml = String::new();
        entry
            .read_to_string(&mut xml)
            .await
            .with_context(|| format!("lendo {file_name}"))?;
        profiles.extend(parse_device_definitions(&xml)?);
    }
    anyhow::ensure!(
        !profiles.is_empty(),
        "nenhuma definição de dispositivo em {}",
        jar.display()
    );
    Ok(profiles)
}

#[derive(Default)]
struct DeviceDefinition {
    id: Option<String>,
    name: Option<String>,
    deprecated: bool,
    tag: Option<String>,
    diagonal: Option<f32>,
    width: Option<u32>,
    height: Option<u32>,
    foldable: bool,
}

/// Parses one of the `sdklib` device definition files. Deprecated definitions and the ones that
/// need images the manager doesn't offer (Automotive, Desktop, XR) are left out.
pub(crate) fn parse_device_definitions(xml: &str) -> Result<Vec<AndroidDeviceProfile>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut profiles = Vec::new();
    let mut current: Option<DeviceDefinition> = None;
    let mut path: Vec<String> = Vec::new();
    loop {
        match reader
            .read_event()
            .context("lendo as definições de dispositivo")?
        {
            Event::Start(element) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "device" => {
                        let deprecated = element
                            .try_get_attribute("deprecated")
                            .ok()
                            .flatten()
                            .is_some_and(|attribute| attribute.value.as_ref() == b"true");
                        current = Some(DeviceDefinition {
                            deprecated,
                            ..DeviceDefinition::default()
                        });
                    }
                    "foldable-region" | "hinge" => {
                        if let Some(device) = current.as_mut() {
                            device.foldable = true;
                        }
                    }
                    _ => {}
                }
                path.push(name);
            }
            Event::End(element) => {
                path.pop();
                if element.local_name().as_ref() == b"device"
                    && let Some(device) = current.take()
                    && let Some(profile) = device_profile_from_definition(device)
                {
                    profiles.push(profile);
                }
            }
            Event::Text(text) => {
                let Some(device) = current.as_mut() else {
                    continue;
                };
                let text = text.decode().unwrap_or_default().trim().to_string();
                let parent = path.iter().rev().nth(1).map(String::as_str);
                // Only the first screen counts; a foldable's inner screen comes first.
                match (parent, path.last().map(String::as_str)) {
                    (Some("device"), Some("id")) => device.id = Some(text),
                    (Some("device"), Some("name")) => device.name = Some(text),
                    (_, Some("tag-id")) => device.tag = Some(text),
                    (Some("screen"), Some("diagonal-length")) if device.diagonal.is_none() => {
                        device.diagonal = text.parse().ok()
                    }
                    (Some("dimensions"), Some("x-dimension")) if device.width.is_none() => {
                        device.width = text.parse().ok()
                    }
                    (Some("dimensions"), Some("y-dimension")) if device.height.is_none() => {
                        device.height = text.parse().ok()
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(profiles)
}

fn device_profile_from_definition(device: DeviceDefinition) -> Option<AndroidDeviceProfile> {
    if device.deprecated {
        return None;
    }
    let category = match device.tag.as_deref() {
        None => {
            if device.foldable {
                DeviceCategory::Foldable
            } else if device.diagonal.is_some_and(|inches| inches >= 7.0) {
                DeviceCategory::Tablet
            } else {
                DeviceCategory::Phone
            }
        }
        Some("android-tv") => DeviceCategory::Tv,
        Some("android-wear") => DeviceCategory::Wear,
        Some(_) => return None,
    };
    let resolution = match (device.width, device.height) {
        (Some(width), Some(height)) => Some((width.min(height), width.max(height))),
        _ => None,
    };
    Some(AndroidDeviceProfile {
        id: device.id?,
        name: device.name?,
        category,
        diagonal_inches: device.diagonal,
        resolution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_IMAGES: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<sys-img:sdk-sys-img xmlns:sys-img="http://schemas.android.com/sdk/android/repo/sys-img2/03" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <remotePackage path="system-images;android-36;google_apis_playstore;arm64-v8a">
    <type-details xsi:type="sys-img:sysImgDetailsType"><api-level>36</api-level><abi>arm64-v8a</abi></type-details>
    <revision><major>7</major></revision>
    <channelRef ref="channel-0"/>
    <archives><archive><complete><size>1886527965</size><url>arm64-v8a-36_r07.zip</url></complete></archive></archives>
  </remotePackage>
  <remotePackage path="system-images;android-36;google_apis_playstore;x86_64">
    <revision><major>7</major></revision>
    <archives><archive><complete><size>1</size></complete></archive></archives>
  </remotePackage>
  <remotePackage path="system-images;android-36-ext19;google_apis_playstore;arm64-v8a">
    <archives><archive><complete><size>2</size></complete></archive></archives>
  </remotePackage>
  <remotePackage path="system-images;android-36;google_apis_playstore_ps16k;arm64-v8a">
    <archives><archive><complete><size>3</size></complete></archive></archives>
  </remotePackage>
  <remotePackage path="system-images;android-37.0;google_apis_playstore;arm64-v8a">
    <archives><archive><complete><size>1500000000</size></complete></archive></archives>
  </remotePackage>
</sys-img:sdk-sys-img>"#;

    #[test]
    fn system_image_catalog_keeps_released_images_for_the_abi() -> Result<()> {
        let images =
            parse_system_image_catalog(SYSTEM_IMAGES, ImageFlavor::GooglePlay, "arm64-v8a")?;
        let packages: Vec<(String, u64)> = images
            .iter()
            .map(|image| (image.id.package(), image.download_bytes))
            .collect();
        assert_eq!(
            packages,
            vec![
                (
                    "system-images;android-36;google_apis_playstore;arm64-v8a".to_string(),
                    1886527965
                ),
                (
                    "system-images;android-37.0;google_apis_playstore;arm64-v8a".to_string(),
                    1500000000
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn component_catalog_picks_the_newest_stable_build_for_the_mac() -> Result<()> {
        let xml = r#"<sdk:sdk-repository xmlns:sdk="x">
  <remotePackage path="emulator">
    <revision><major>37</major><minor>3</minor><micro>5</micro></revision>
    <channelRef ref="channel-2"/>
    <archives><archive><complete><size>9</size></complete><host-os>macosx</host-os><host-arch>aarch64</host-arch></archive></archives>
  </remotePackage>
  <remotePackage path="emulator">
    <revision><major>36</major><minor>4</minor><micro>10</micro></revision>
    <channelRef ref="channel-0"/>
    <archives>
      <archive><complete><size>1</size></complete><host-os>linux</host-os><host-arch>x64</host-arch></archive>
      <archive><complete><size>423786469</size></complete><host-os>macosx</host-os><host-arch>aarch64</host-arch></archive>
    </archives>
  </remotePackage>
  <remotePackage path="emulator">
    <revision><major>36</major><minor>3</minor><micro>12</micro></revision>
    <channelRef ref="channel-0"/>
    <archives><archive><complete><size>5</size></complete><host-os>macosx</host-os><host-arch>aarch64</host-arch></archive></archives>
  </remotePackage>
  <remotePackage path="cmdline-tools;20.0">
    <revision><major>20</major><minor>0</minor></revision>
    <channelRef ref="channel-0"/>
    <archives><archive><complete><size>7</size></complete><host-os>macosx</host-os></archive></archives>
  </remotePackage>
</sdk:sdk-repository>"#;
        let components = parse_component_catalog(xml, "aarch64")?;
        assert_eq!(
            components,
            vec![
                CatalogComponent {
                    kind: SdkComponentKind::Emulator,
                    version: "36.4.10".to_string(),
                    download_bytes: 423786469,
                },
                CatalogComponent {
                    kind: SdkComponentKind::CommandLineTools,
                    version: "20.0".to_string(),
                    download_bytes: 7,
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn system_image_ids_read_packages_and_avd_sysdirs() {
        let id = SystemImageId::parse_sysdir("system-images/android-35/google_apis/arm64-v8a/");
        assert_eq!(
            id,
            Some(SystemImageId::new(
                "35",
                ImageFlavor::GoogleApis,
                "arm64-v8a"
            ))
        );
        let minor = SystemImageId::parse_package(
            "system-images;android-36.1;google_apis_playstore;arm64-v8a",
        );
        assert_eq!(
            minor.as_ref().map(SystemImageId::short_label).as_deref(),
            Some("Android 16 · API 36.1")
        );
        let major_only =
            SystemImageId::parse_package("system-images;android-37.0;default;arm64-v8a");
        assert_eq!(
            major_only.as_ref().map(SystemImageId::api_label).as_deref(),
            Some("API 37")
        );
        assert_eq!(
            SystemImageId::new("32", ImageFlavor::Aosp, "arm64-v8a").android_version(),
            "Android 12L"
        );
        assert_eq!(SystemImageId::parse_package("platforms;android-35"), None);
    }

    #[test]
    fn device_definitions_are_grouped_by_kind() -> Result<()> {
        let xml = r#"<d:devices xmlns:d="x">
  <d:device><d:name>Pixel 9</d:name><d:id>pixel_9</d:id>
    <d:hardware><d:screen><d:diagonal-length>6.24</d:diagonal-length>
      <d:dimensions><d:x-dimension>1080</d:x-dimension><d:y-dimension>2424</d:y-dimension></d:dimensions>
    </d:screen></d:hardware></d:device>
  <d:device><d:name>Pixel Tablet</d:name><d:id>pixel_tablet</d:id>
    <d:hardware><d:screen><d:diagonal-length>10.95</d:diagonal-length>
      <d:dimensions><d:x-dimension>2560</d:x-dimension><d:y-dimension>1600</d:y-dimension></d:dimensions>
    </d:screen></d:hardware></d:device>
  <d:device><d:name>Pixel Fold</d:name><d:id>pixel_fold</d:id>
    <d:hardware><d:screen><d:diagonal-length>7.58</d:diagonal-length>
      <d:dimensions><d:x-dimension>2208</d:x-dimension><d:y-dimension>1840</d:y-dimension></d:dimensions>
      <d:foldable-region><d:x-folded-dimension>1080</d:x-folded-dimension></d:foldable-region>
    </d:screen></d:hardware></d:device>
  <d:device deprecated="true"><d:name>Old</d:name><d:id>old</d:id></d:device>
  <d:device><d:name>Automotive</d:name><d:id>automotive_1024p_landscape</d:id><d:tag-id>android-automotive</d:tag-id></d:device>
  <d:device><d:name>Wear OS Large Round</d:name><d:id>wearos_large_round</d:id><d:tag-id>android-wear</d:tag-id></d:device>
</d:devices>"#;
        let profiles = parse_device_definitions(xml)?;
        let summary: Vec<(&str, DeviceCategory, Option<String>)> = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.as_str(),
                    profile.category,
                    profile.screen_label(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "pixel_9",
                    DeviceCategory::Phone,
                    Some("6,2\" · 1080×2424".to_string())
                ),
                (
                    "pixel_tablet",
                    DeviceCategory::Tablet,
                    Some("10,9\" · 1600×2560".to_string())
                ),
                (
                    "pixel_fold",
                    DeviceCategory::Foldable,
                    Some("7,6\" · 1840×2208".to_string())
                ),
                ("wearos_large_round", DeviceCategory::Wear, None),
            ]
        );
        Ok(())
    }
}
