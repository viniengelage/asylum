//! The virtual devices and system images on disk, read and changed without `avdmanager` (and so
//! without a JVM): an AVD is a `<name>.ini` marker plus a `<name>.avd` directory whose
//! `config.ini` holds its settings.

use crate::catalog::{SdkComponentKind, SystemImageId, parse_package_revision};
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// A virtual device that exists under Zed's AVD home.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AndroidAvd {
    /// Name the emulator is launched with, e.g. `zed-pixel_7`.
    pub name: String,
    /// `hw.device.name` from the AVD's config, e.g. `pixel_7`.
    pub device_profile: Option<String>,
    /// `avd.ini.displayname`, what the user named it. AVDs created before the device manager
    /// existed don't have one.
    pub display_name: Option<String>,
    pub system_image: Option<SystemImageId>,
    pub image_installed: bool,
    pub ram_mb: Option<u32>,
    pub storage_bytes: Option<u64>,
    pub orientation: Orientation,
    pub hardware_keyboard: bool,
    /// What the AVD's directory takes on disk, counting allocated blocks so sparse disk images
    /// aren't counted at their full size.
    pub disk_bytes: u64,
}

impl AndroidAvd {
    /// Label for the device picker: the name the user gave, else the device profile, since the
    /// AVD name is derived from it and reads worse.
    pub fn label(&self) -> String {
        if let Some(display_name) = &self.display_name {
            return display_name.clone();
        }
        match &self.device_profile {
            Some(profile) => humanize_device_profile_id(profile),
            None => self.name.clone(),
        }
    }

    /// `Android 15 · API 35`.
    pub fn os_label(&self) -> Option<String> {
        self.system_image.as_ref().map(SystemImageId::short_label)
    }

    pub fn image_missing(&self) -> bool {
        self.system_image.is_some() && !self.image_installed
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

impl Orientation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Retrato",
            Self::Landscape => "Paisagem",
        }
    }

    fn config_value(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::Landscape => "landscape",
        }
    }
}

/// What the new-device wizard and Editar write into an AVD's `config.ini`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvdSettings {
    pub display_name: String,
    pub device_profile: String,
    pub image: SystemImageId,
    pub ram_mb: u32,
    pub storage_gb: u32,
    pub orientation: Orientation,
    pub hardware_keyboard: bool,
}

impl AvdSettings {
    /// The `config.ini` entries these settings own. The GPU is always the host's: without it
    /// the guest renders in software at ~12 fps.
    pub(crate) fn config_entries(&self) -> Vec<(&'static str, String)> {
        vec![
            ("avd.ini.displayname", self.display_name.clone()),
            ("hw.ramSize", self.ram_mb.to_string()),
            (
                "disk.dataPartition.size",
                (u64::from(self.storage_gb) * 1024 * 1024 * 1024).to_string(),
            ),
            (
                "hw.initialOrientation",
                self.orientation.config_value().to_string(),
            ),
            ("hw.keyboard", yes_no(self.hardware_keyboard).to_string()),
            ("hw.gpu.enabled", "yes".to_string()),
            ("hw.gpu.mode", "host".to_string()),
        ]
    }

    /// The entries that point an existing AVD at another system image.
    pub(crate) fn image_entries(&self) -> Vec<(&'static str, String)> {
        let mut sysdir = self.image.relative_dir().to_string_lossy().into_owned();
        sysdir.push('/');
        let play_store = self
            .image
            .flavor()
            .is_some_and(|flavor| flavor.has_play_store());
        vec![
            ("image.sysdir.1", sysdir),
            ("tag.id", self.image.tag.clone()),
            ("tag.ids", self.image.tag.clone()),
            ("abi.type", self.image.abi.clone()),
            ("PlayStore.enabled", yes_no(play_store).to_string()),
        ]
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// An installed system image and the AVDs that boot from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledSystemImage {
    pub id: SystemImageId,
    pub disk_bytes: u64,
    /// Labels of the AVDs that use it.
    pub used_by: Vec<String>,
}

pub(crate) fn avd_dir(avd_home: &Path, name: &str) -> PathBuf {
    avd_home.join(format!("{name}.avd"))
}

pub(crate) fn avd_marker(avd_home: &Path, name: &str) -> PathBuf {
    avd_home.join(format!("{name}.ini"))
}

pub(crate) fn avd_config_ini(avd_home: &Path, name: &str) -> PathBuf {
    avd_dir(avd_home, name).join("config.ini")
}

fn config_value<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    config.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key.trim() == key).then(|| value.trim())
    })
}

/// Reads `hw.ramSize`, which is either megabytes or a number with a `M`/`G` suffix.
fn parse_megabytes(value: &str) -> Option<u32> {
    let value = value.trim();
    if let Some(gigabytes) = value.strip_suffix(['G', 'g']) {
        return gigabytes.trim().parse::<u32>().ok().map(|gb| gb * 1024);
    }
    value
        .strip_suffix(['M', 'm'])
        .unwrap_or(value)
        .trim()
        .parse()
        .ok()
}

/// Reads `disk.dataPartition.size`: bytes, or a number with a `K`/`M`/`G` suffix.
fn parse_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    let (number, multiplier) = match value.chars().last()? {
        'G' | 'g' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        'M' | 'm' => (&value[..value.len() - 1], 1024 * 1024),
        'K' | 'k' => (&value[..value.len() - 1], 1024),
        _ => (value, 1),
    };
    number
        .trim()
        .parse::<u64>()
        .ok()
        .map(|number| number * multiplier)
}

/// Lists the AVDs under the AVD home by reading the `<name>.ini` markers `avdmanager` writes.
pub(crate) fn list_avds(avd_home: &Path, sdk_dir: &Path) -> Vec<AndroidAvd> {
    let Ok(entries) = std::fs::read_dir(avd_home) else {
        return Vec::new();
    };
    let mut avds: Vec<AndroidAvd> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().is_none_or(|extension| extension != "ini") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            // `.ini` markers are only meaningful with their `.avd` payload; a half-deleted AVD
            // would otherwise show up as bootable.
            let config = std::fs::read_to_string(avd_config_ini(avd_home, &name)).ok()?;
            Some(read_avd(avd_home, sdk_dir, name, &config))
        })
        .collect();
    avds.sort_by_key(|avd| avd.label().to_lowercase());
    avds
}

fn read_avd(avd_home: &Path, sdk_dir: &Path, name: String, config: &str) -> AndroidAvd {
    let system_image = config_value(config, "image.sysdir.1").and_then(SystemImageId::parse_sysdir);
    let image_installed = system_image
        .as_ref()
        .is_some_and(|image| system_image_installed(sdk_dir, image));
    AndroidAvd {
        device_profile: config_value(config, "hw.device.name").map(str::to_string),
        display_name: config_value(config, "avd.ini.displayname")
            .filter(|display_name| !display_name.is_empty())
            .map(str::to_string),
        system_image,
        image_installed,
        ram_mb: config_value(config, "hw.ramSize").and_then(parse_megabytes),
        storage_bytes: config_value(config, "disk.dataPartition.size").and_then(parse_bytes),
        orientation: match config_value(config, "hw.initialOrientation") {
            Some("landscape") => Orientation::Landscape,
            _ => Orientation::Portrait,
        },
        hardware_keyboard: config_value(config, "hw.keyboard") == Some("yes"),
        disk_bytes: allocated_bytes(&avd_dir(avd_home, &name)),
        name,
    }
}

pub(crate) fn system_image_installed(sdk_dir: &Path, image: &SystemImageId) -> bool {
    // sdkmanager writes `source.properties` last, so a cancelled download doesn't count.
    sdk_dir
        .join(image.relative_dir())
        .join("source.properties")
        .exists()
}

/// Rewrites `config` with `entries` set: existing keys keep their line, missing ones are appended.
pub(crate) fn apply_config_entries(config: &str, entries: &[(&str, String)]) -> String {
    let mut written = vec![false; entries.len()];
    let mut updated = String::with_capacity(config.len() + 64);
    for line in config.lines() {
        let key = line.split_once('=').map(|(key, _)| key.trim());
        match key.and_then(|key| entries.iter().position(|(entry_key, _)| *entry_key == key)) {
            Some(index) => {
                if !written[index] {
                    let (entry_key, value) = &entries[index];
                    updated.push_str(&format!("{entry_key} = {value}"));
                    updated.push('\n');
                    written[index] = true;
                }
            }
            None => {
                updated.push_str(line);
                updated.push('\n');
            }
        }
    }
    for ((key, value), written) in entries.iter().zip(written) {
        if !written {
            updated.push_str(&format!("{key} = {value}\n"));
        }
    }
    updated
}

pub(crate) fn write_avd_config(
    avd_home: &Path,
    name: &str,
    entries: &[(&str, String)],
) -> Result<()> {
    let path = avd_config_ini(avd_home, name);
    let config =
        std::fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
    std::fs::write(&path, apply_config_entries(&config, entries))
        .with_context(|| format!("gravando {}", path.display()))
}

/// The AVD name for a device the user called `display_name`: `zed-` plus a slug, with a counter
/// when another AVD already has it.
pub(crate) fn avd_name_for_display_name(display_name: &str, existing: &[String]) -> String {
    let mut slug = String::with_capacity(display_name.len());
    for character in display_name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('_') && !slug.is_empty() {
            slug.push('_');
        }
    }
    let slug = slug.trim_end_matches('_');
    let base = if slug.is_empty() {
        "zed-device".to_string()
    } else {
        format!("zed-{slug}")
    };
    if !existing.contains(&base) {
        return base;
    }
    (2..)
        .map(|counter| format!("{base}-{counter}"))
        .find(|candidate| !existing.contains(candidate))
        .unwrap_or(base)
}

pub(crate) fn delete_avd(avd_home: &Path, name: &str) -> Result<()> {
    let directory = avd_dir(avd_home, name);
    if directory.exists() {
        std::fs::remove_dir_all(&directory)
            .with_context(|| format!("apagando {}", directory.display()))?;
    }
    let marker = avd_marker(avd_home, name);
    if marker.exists() {
        std::fs::remove_file(&marker).with_context(|| format!("apagando {}", marker.display()))?;
    }
    Ok(())
}

/// Files the emulator recreates from the system image on the next boot, which is what
/// `-wipe-data` does at launch.
const USER_DATA_FILES: &[&str] = &[
    "userdata-qemu.img",
    "userdata-qemu.img.qcow2",
    "cache.img",
    "cache.img.qcow2",
];

pub(crate) fn wipe_avd_data(avd_home: &Path, name: &str) -> Result<()> {
    let directory = avd_dir(avd_home, name);
    for file in USER_DATA_FILES {
        let path = directory.join(file);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("apagando {}", path.display()))?;
        }
    }
    let snapshots = directory.join("snapshots");
    if snapshots.exists() {
        std::fs::remove_dir_all(&snapshots)
            .with_context(|| format!("apagando {}", snapshots.display()))?;
    }
    Ok(())
}

/// Copies an AVD under a new name. Snapshots and the running emulator's lock files stay behind;
/// the copy cold boots with the original's data. On APFS `std::fs::copy` clones, so the disk
/// images cost nothing until they diverge.
pub(crate) fn duplicate_avd(
    avd_home: &Path,
    name: &str,
    new_name: &str,
    new_display_name: &str,
) -> Result<()> {
    let source = avd_dir(avd_home, name);
    let destination = avd_dir(avd_home, new_name);
    anyhow::ensure!(
        !destination.exists(),
        "já existe um dispositivo em {}",
        destination.display()
    );
    copy_avd_directory(&source, &destination)?;

    let marker = std::fs::read_to_string(avd_marker(avd_home, name))
        .with_context(|| format!("lendo {name}.ini"))?;
    let new_marker = apply_config_entries(
        &marker,
        &[
            ("path", destination.to_string_lossy().into_owned()),
            ("path.rel", format!("avd/{new_name}.avd")),
        ],
    );
    std::fs::write(avd_marker(avd_home, new_name), new_marker)
        .with_context(|| format!("gravando {new_name}.ini"))?;
    write_avd_config(
        avd_home,
        new_name,
        &[("avd.ini.displayname", new_display_name.to_string())],
    )
}

fn copy_avd_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .with_context(|| format!("criando {}", destination.display()))?;
    for entry in std::fs::read_dir(source).with_context(|| format!("lendo {}", source.display()))? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name_text = file_name.to_string_lossy();
        if file_name_text == "snapshots" || file_name_text.ends_with(".lock") {
            continue;
        }
        let path = entry.path();
        let target = destination.join(&file_name);
        if entry.file_type()?.is_dir() {
            copy_avd_directory(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)
                .with_context(|| format!("copiando {}", path.display()))?;
        }
    }
    Ok(())
}

/// The system images under the SDK, with the AVDs that use each one.
pub(crate) fn installed_system_images(
    sdk_dir: &Path,
    avds: &[AndroidAvd],
) -> Vec<InstalledSystemImage> {
    let mut images = Vec::new();
    for platform in directory_entries(&sdk_dir.join("system-images")) {
        for tag in directory_entries(&platform) {
            for abi in directory_entries(&tag) {
                let relative = abi.strip_prefix(sdk_dir).unwrap_or(&abi);
                let Some(id) = SystemImageId::parse_sysdir(&relative.to_string_lossy()) else {
                    continue;
                };
                if !system_image_installed(sdk_dir, &id) {
                    continue;
                }
                let used_by = avds
                    .iter()
                    .filter(|avd| avd.system_image.as_ref() == Some(&id))
                    .map(AndroidAvd::label)
                    .collect();
                images.push(InstalledSystemImage {
                    disk_bytes: allocated_bytes(&abi),
                    id,
                    used_by,
                });
            }
        }
    }
    images.sort_by(|left, right| {
        right
            .id
            .api()
            .cmp(&left.id.api())
            .then_with(|| left.id.tag.cmp(&right.id.tag))
    });
    images
}

pub(crate) fn installed_component_version(
    sdk_dir: &Path,
    kind: SdkComponentKind,
) -> Option<String> {
    let properties =
        std::fs::read_to_string(sdk_dir.join(kind.relative_dir()).join("source.properties"))
            .ok()?;
    parse_package_revision(&properties)
}

fn directory_entries(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

/// Disk space a path takes, by allocated blocks: AVD disk images are sparse.
pub(crate) fn allocated_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| allocated_bytes(&entry.path()))
                    .sum()
            })
            .unwrap_or(0)
    } else {
        metadata.blocks() * 512
    }
}

/// Free space on the volume holding `path`.
pub fn available_disk_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stats` is only read after
    // `statvfs` reports success, at which point it has been filled in.
    let stats = unsafe {
        if libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) != 0 {
            return None;
        }
        stats.assume_init()
    };
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

/// Turns a device profile id such as `pixel_9_pro` into `Pixel 9 Pro`. The profile list carries
/// proper names, but AVDs on disk only remember the id.
pub(crate) fn humanize_device_profile_id(id: &str) -> String {
    let mut label = String::with_capacity(id.len());
    for word in id.split(['_', '-']).filter(|word| !word.is_empty()) {
        if !label.is_empty() {
            label.push(' ');
        }
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            label.extend(first.to_uppercase());
            label.push_str(characters.as_str());
        }
    }
    if label.is_empty() {
        id.to_string()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ImageFlavor;

    const CONFIG: &str = "PlayStore.enabled = no
abi.type = arm64-v8a
disk.dataPartition.size = 6442450944
hw.device.name = pixel_7
hw.initialOrientation = portrait
hw.keyboard = yes
hw.ramSize = 2G
image.sysdir.1 = system-images/android-35/google_apis/arm64-v8a/
tag.id = google_apis
";

    fn write_avd(avd_home: &Path, name: &str, config: &str) -> Result<()> {
        std::fs::create_dir_all(avd_dir(avd_home, name))?;
        std::fs::write(avd_config_ini(avd_home, name), config)?;
        std::fs::write(
            avd_marker(avd_home, name),
            format!(
                "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{name}.avd\ntarget=android-35\n",
                avd_dir(avd_home, name).display()
            ),
        )?;
        Ok(())
    }

    #[test]
    fn avds_are_read_from_their_config() -> Result<()> {
        let root = tempfile::tempdir()?;
        let avd_home = root.path().join("avd_home");
        let sdk_dir = root.path().join("sdk");
        write_avd(&avd_home, "zed-android", CONFIG)?;

        let avds = list_avds(&avd_home, &sdk_dir);
        assert_eq!(avds.len(), 1);
        let avd = &avds[0];
        assert_eq!(avd.label(), "Pixel 7");
        assert_eq!(avd.os_label().as_deref(), Some("Android 15 · API 35"));
        assert_eq!(avd.ram_mb, Some(2048));
        assert_eq!(avd.storage_bytes, Some(6442450944));
        assert!(avd.hardware_keyboard);
        assert!(avd.image_missing());

        let image_dir = sdk_dir.join("system-images/android-35/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image_dir)?;
        std::fs::write(image_dir.join("source.properties"), "Pkg.Revision=14\n")?;
        let avds = list_avds(&avd_home, &sdk_dir);
        assert!(!avds[0].image_missing());

        let images = installed_system_images(&sdk_dir, &avds);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].used_by, vec!["Pixel 7".to_string()]);
        Ok(())
    }

    #[test]
    fn config_entries_replace_existing_keys_and_append_new_ones() {
        let updated = apply_config_entries(
            "hw.keyboard = no\nhw.lcd.width = 1080\nhw.keyboard=no\n",
            &[
                ("hw.keyboard", "yes".to_string()),
                ("avd.ini.displayname", "Pixel 9".to_string()),
            ],
        );
        assert_eq!(
            updated,
            "hw.keyboard = yes\nhw.lcd.width = 1080\navd.ini.displayname = Pixel 9\n"
        );
    }

    #[test]
    fn avd_names_come_from_the_display_name() {
        let existing = vec![
            "zed-pixel_8_pro".to_string(),
            "zed-pixel_8_pro-2".to_string(),
        ];
        assert_eq!(
            avd_name_for_display_name("Pixel 9", &existing),
            "zed-pixel_9"
        );
        assert_eq!(
            avd_name_for_display_name("Pixel 8 Pro", &existing),
            "zed-pixel_8_pro-3"
        );
        assert_eq!(avd_name_for_display_name("  ", &existing), "zed-device");
    }

    #[test]
    fn avds_are_duplicated_wiped_and_deleted() -> Result<()> {
        let root = tempfile::tempdir()?;
        let avd_home = root.path().join("avd_home");
        write_avd(&avd_home, "zed-pixel_9", CONFIG)?;
        let directory = avd_dir(&avd_home, "zed-pixel_9");
        std::fs::write(directory.join("userdata-qemu.img"), "data")?;
        std::fs::write(directory.join("hardware-qemu.ini.lock"), "")?;
        std::fs::create_dir_all(directory.join("snapshots/default_boot"))?;

        duplicate_avd(&avd_home, "zed-pixel_9", "zed-pixel_9-2", "Pixel 9 (cópia)")?;
        let copy = avd_dir(&avd_home, "zed-pixel_9-2");
        assert!(copy.join("userdata-qemu.img").exists());
        assert!(!copy.join("hardware-qemu.ini.lock").exists());
        assert!(!copy.join("snapshots").exists());
        let marker = std::fs::read_to_string(avd_marker(&avd_home, "zed-pixel_9-2"))?;
        assert!(marker.contains(&format!("path = {}", copy.display())));
        assert!(marker.contains("path.rel = avd/zed-pixel_9-2.avd"));
        let config = std::fs::read_to_string(avd_config_ini(&avd_home, "zed-pixel_9-2"))?;
        assert!(config.contains("avd.ini.displayname = Pixel 9 (cópia)"));

        wipe_avd_data(&avd_home, "zed-pixel_9")?;
        assert!(!directory.join("userdata-qemu.img").exists());
        assert!(!directory.join("snapshots").exists());
        assert!(directory.join("config.ini").exists());

        delete_avd(&avd_home, "zed-pixel_9")?;
        assert!(!directory.exists());
        assert!(!avd_marker(&avd_home, "zed-pixel_9").exists());
        assert_eq!(
            list_avds(&avd_home, root.path())
                .iter()
                .map(|avd| avd.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zed-pixel_9-2"]
        );
        Ok(())
    }

    #[test]
    fn settings_point_the_avd_at_its_image() {
        let settings = AvdSettings {
            display_name: "Pixel 8 Pro".to_string(),
            device_profile: "pixel_8_pro".to_string(),
            image: SystemImageId::new("36", ImageFlavor::GooglePlay, "arm64-v8a"),
            ram_mb: 4096,
            storage_gb: 8,
            orientation: Orientation::Portrait,
            hardware_keyboard: true,
        };
        let config = apply_config_entries(
            CONFIG,
            &[settings.config_entries(), settings.image_entries()].concat(),
        );
        assert!(config.contains(
            "image.sysdir.1 = system-images/android-36/google_apis_playstore/arm64-v8a/"
        ));
        assert!(config.contains("PlayStore.enabled = yes"));
        assert!(config.contains("disk.dataPartition.size = 8589934592"));
        assert!(config.contains("hw.ramSize = 4096"));
        assert!(config.contains("hw.gpu.mode = host"));
    }
}
