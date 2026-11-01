//! What differs between the hosts the Android SDK runs on: which archives to download, how they
//! are laid out once extracted, and what the SDK's executables are called.

use anyhow::{Result, anyhow};
use gpui::SharedString;
use i18n::t;
use std::path::{Path, PathBuf};

const CMDLINE_TOOLS_BASE_URL: &str = "https://dl.google.com/android/repository";
const CMDLINE_TOOLS_BUILD: &str = "13114758";
const JDK_BASE_URL: &str =
    "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11";
const JDK_FILE_VERSION: &str = "21.0.5_11";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    Windows,
    Linux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostArch {
    X64,
    Aarch64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPlatform {
    pub os: HostOs,
    pub arch: HostArch,
}

impl HostPlatform {
    /// The platform Zed was built for. Hosts the SDK isn't published for get the Linux x64
    /// layout, which only matters until [`Self::supported_current`] rejects them.
    pub fn current() -> Self {
        Self::from_target(std::env::consts::OS, std::env::consts::ARCH).unwrap_or(Self {
            os: HostOs::Linux,
            arch: HostArch::X64,
        })
    }

    /// The current platform, or why the Android emulator can't run on it.
    pub fn supported_current() -> Result<Self> {
        let platform =
            Self::from_target(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
                anyhow!(t!(
                    "android_sdk.host.unsupported_platform",
                    os = std::env::consts::OS,
                    arch = std::env::consts::ARCH
                ))
            })?;
        match platform.emulator_unsupported_reason() {
            Some(reason) => Err(anyhow!(reason)),
            None => Ok(platform),
        }
    }

    pub fn from_target(os: &str, arch: &str) -> Option<Self> {
        let os = match os {
            "macos" => HostOs::MacOs,
            "windows" => HostOs::Windows,
            "linux" => HostOs::Linux,
            _ => return None,
        };
        let arch = match arch {
            "x86_64" => HostArch::X64,
            "aarch64" => HostArch::Aarch64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// Google publishes the emulator for macOS (both architectures), Linux x64 and Windows x64
    /// only. Under Windows on ARM the x64 build has no hypervisor to run on.
    pub fn emulator_unsupported_reason(self) -> Option<SharedString> {
        match (self.os, self.arch) {
            (HostOs::MacOs, _)
            | (HostOs::Linux, HostArch::X64)
            | (HostOs::Windows, HostArch::X64) => None,
            (HostOs::Linux, HostArch::Aarch64) => {
                Some(t!("android_sdk.host.unsupported_linux_arm64"))
            }
            (HostOs::Windows, HostArch::Aarch64) => {
                Some(t!("android_sdk.host.unsupported_windows_arm64"))
            }
        }
    }

    pub fn cmdline_tools_url(self) -> String {
        let os = match self.os {
            HostOs::MacOs => "mac",
            HostOs::Windows => "win",
            HostOs::Linux => "linux",
        };
        format!("{CMDLINE_TOOLS_BASE_URL}/commandlinetools-{os}-{CMDLINE_TOOLS_BUILD}_latest.zip")
    }

    pub fn jdk_archive_format(self) -> ArchiveFormat {
        match self.os {
            HostOs::Windows => ArchiveFormat::Zip,
            HostOs::MacOs | HostOs::Linux => ArchiveFormat::TarGz,
        }
    }

    pub fn jdk_url(self) -> String {
        let arch = match self.arch {
            HostArch::X64 => "x64",
            HostArch::Aarch64 => "aarch64",
        };
        let os = match self.os {
            HostOs::MacOs => "mac",
            HostOs::Windows => "windows",
            HostOs::Linux => "linux",
        };
        format!(
            "{JDK_BASE_URL}/OpenJDK21U-jdk_{arch}_{os}_hotspot_{JDK_FILE_VERSION}.{}",
            self.jdk_archive_format().extension()
        )
    }

    /// `JAVA_HOME` inside the directory the JDK archive extracts to: macOS ships a bundle.
    pub fn java_home_in(self, extracted_jdk: &Path) -> PathBuf {
        match self.os {
            HostOs::MacOs => extracted_jdk.join("Contents").join("Home"),
            HostOs::Windows | HostOs::Linux => extracted_jdk.to_path_buf(),
        }
    }

    /// The `host-os` the repository XML tags each archive with.
    pub fn catalog_host_os(self) -> &'static str {
        match self.os {
            HostOs::MacOs => "macosx",
            HostOs::Windows => "windows",
            HostOs::Linux => "linux",
        }
    }

    /// The `host-arch` the repository XML tags each archive with.
    pub fn catalog_host_arch(self) -> &'static str {
        match self.arch {
            HostArch::X64 => "x64",
            HostArch::Aarch64 => "aarch64",
        }
    }

    /// The ABI of the system images the emulator can run without translating instructions.
    pub fn system_image_abi(self) -> &'static str {
        match self.arch {
            HostArch::X64 => "x86_64",
            HostArch::Aarch64 => "arm64-v8a",
        }
    }

    /// A native executable shipped in the SDK, such as `adb` or `emulator`.
    pub fn executable_name(self, name: &str) -> String {
        match self.os {
            HostOs::Windows => format!("{name}.exe"),
            HostOs::MacOs | HostOs::Linux => name.to_string(),
        }
    }

    /// A launcher script from the command-line tools, such as `sdkmanager`.
    pub fn script_name(self, name: &str) -> String {
        match self.os {
            HostOs::Windows => format!("{name}.bat"),
            HostOs::MacOs | HostOs::Linux => name.to_string(),
        }
    }

    /// macOS gets Hypervisor.framework with the OS; Windows needs WHPX turned on and Linux needs
    /// access to `/dev/kvm`, and without them the emulator dies without saying why.
    pub fn needs_acceleration_check(self) -> bool {
        !matches!(self.os, HostOs::MacOs)
    }

    /// Where a running emulator writes the discovery file with its gRPC port. Linux falls back to
    /// the user directory when `XDG_RUNTIME_DIR` isn't set, which depends on
    /// `ANDROID_USER_HOME`, so every candidate is listed.
    pub fn emulator_discovery_dirs(
        self,
        home_dir: &Path,
        android_user_home: &Path,
        environment_variable: impl Fn(&str) -> Option<String>,
    ) -> Vec<PathBuf> {
        let running = |root: PathBuf| root.join("avd").join("running");
        match self.os {
            HostOs::MacOs => vec![
                home_dir
                    .join("Library")
                    .join("Caches")
                    .join("TemporaryItems")
                    .join("avd")
                    .join("running"),
            ],
            HostOs::Windows => {
                let local_app_data = environment_variable("LOCALAPPDATA")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home_dir.join("AppData").join("Local"));
                vec![running(local_app_data.join("Temp"))]
            }
            HostOs::Linux => {
                let mut directories = Vec::new();
                if let Some(runtime_dir) =
                    environment_variable("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())
                {
                    directories.push(running(PathBuf::from(runtime_dir)));
                }
                directories.push(running(android_user_home.to_path_buf()));
                directories.push(running(home_dir.join(".android")));
                directories
            }
        }
    }
}

/// The reason `emulator -accel-check` gives, without the `accel:` framing and exit code lines.
pub fn acceleration_check_message(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && *line != "accel:"
                && *line != "accel"
                && !line.chars().all(|character| character.is_ascii_digit())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_ARM: HostPlatform = HostPlatform {
        os: HostOs::MacOs,
        arch: HostArch::Aarch64,
    };
    const MAC_X64: HostPlatform = HostPlatform {
        os: HostOs::MacOs,
        arch: HostArch::X64,
    };
    const WINDOWS_X64: HostPlatform = HostPlatform {
        os: HostOs::Windows,
        arch: HostArch::X64,
    };
    const WINDOWS_ARM: HostPlatform = HostPlatform {
        os: HostOs::Windows,
        arch: HostArch::Aarch64,
    };
    const LINUX_X64: HostPlatform = HostPlatform {
        os: HostOs::Linux,
        arch: HostArch::X64,
    };
    const LINUX_ARM: HostPlatform = HostPlatform {
        os: HostOs::Linux,
        arch: HostArch::Aarch64,
    };

    #[test]
    fn targets_map_to_platforms() {
        assert_eq!(HostPlatform::from_target("macos", "aarch64"), Some(MAC_ARM));
        assert_eq!(
            HostPlatform::from_target("windows", "x86_64"),
            Some(WINDOWS_X64)
        );
        assert_eq!(
            HostPlatform::from_target("linux", "aarch64"),
            Some(LINUX_ARM)
        );
        assert_eq!(HostPlatform::from_target("freebsd", "x86_64"), None);
        assert_eq!(HostPlatform::from_target("linux", "riscv64"), None);
    }

    #[test]
    fn macos_downloads_are_unchanged() {
        assert_eq!(
            MAC_ARM.cmdline_tools_url(),
            "https://dl.google.com/android/repository/commandlinetools-mac-13114758_latest.zip"
        );
        assert_eq!(
            MAC_ARM.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.5_11.tar.gz"
        );
        assert_eq!(
            MAC_X64.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_x64_mac_hotspot_21.0.5_11.tar.gz"
        );
        assert_eq!(MAC_ARM.jdk_archive_format(), ArchiveFormat::TarGz);
        assert_eq!(
            MAC_ARM.java_home_in(Path::new("/jdk/jdk-21.0.5+11")),
            Path::new("/jdk/jdk-21.0.5+11/Contents/Home")
        );
    }

    #[test]
    fn windows_downloads_zips() {
        assert_eq!(
            WINDOWS_X64.cmdline_tools_url(),
            "https://dl.google.com/android/repository/commandlinetools-win-13114758_latest.zip"
        );
        assert_eq!(
            WINDOWS_X64.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_x64_windows_hotspot_21.0.5_11.zip"
        );
        assert_eq!(
            WINDOWS_ARM.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_aarch64_windows_hotspot_21.0.5_11.zip"
        );
        assert_eq!(WINDOWS_X64.jdk_archive_format(), ArchiveFormat::Zip);
        let extracted = Path::new("jdk").join("jdk-21.0.5+11");
        assert_eq!(WINDOWS_X64.java_home_in(&extracted), extracted);
    }

    #[test]
    fn linux_downloads_tarballs() {
        assert_eq!(
            LINUX_X64.cmdline_tools_url(),
            "https://dl.google.com/android/repository/commandlinetools-linux-13114758_latest.zip"
        );
        assert_eq!(
            LINUX_X64.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_x64_linux_hotspot_21.0.5_11.tar.gz"
        );
        assert_eq!(
            LINUX_ARM.jdk_url(),
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.5%2B11/OpenJDK21U-jdk_aarch64_linux_hotspot_21.0.5_11.tar.gz"
        );
        assert_eq!(LINUX_X64.jdk_archive_format(), ArchiveFormat::TarGz);
        let extracted = Path::new("/jdk/jdk-21.0.5+11");
        assert_eq!(LINUX_X64.java_home_in(extracted), extracted);
    }

    #[test]
    fn catalog_tags_and_abis_follow_the_host() {
        assert_eq!(MAC_ARM.catalog_host_os(), "macosx");
        assert_eq!(WINDOWS_X64.catalog_host_os(), "windows");
        assert_eq!(LINUX_X64.catalog_host_os(), "linux");
        assert_eq!(MAC_ARM.catalog_host_arch(), "aarch64");
        assert_eq!(WINDOWS_X64.catalog_host_arch(), "x64");
        assert_eq!(MAC_ARM.system_image_abi(), "arm64-v8a");
        assert_eq!(LINUX_X64.system_image_abi(), "x86_64");
    }

    #[test]
    fn executables_have_windows_extensions() {
        assert_eq!(MAC_ARM.executable_name("adb"), "adb");
        assert_eq!(LINUX_X64.script_name("sdkmanager"), "sdkmanager");
        assert_eq!(WINDOWS_X64.executable_name("adb"), "adb.exe");
        assert_eq!(WINDOWS_X64.executable_name("emulator"), "emulator.exe");
        assert_eq!(WINDOWS_X64.script_name("sdkmanager"), "sdkmanager.bat");
        assert_eq!(WINDOWS_X64.script_name("avdmanager"), "avdmanager.bat");
    }

    #[test]
    fn only_hosts_with_an_emulator_build_are_supported() {
        assert_eq!(MAC_ARM.emulator_unsupported_reason(), None);
        assert_eq!(MAC_X64.emulator_unsupported_reason(), None);
        assert_eq!(WINDOWS_X64.emulator_unsupported_reason(), None);
        assert_eq!(LINUX_X64.emulator_unsupported_reason(), None);
        assert!(WINDOWS_ARM.emulator_unsupported_reason().is_some());
        assert!(LINUX_ARM.emulator_unsupported_reason().is_some());
        assert!(!MAC_ARM.needs_acceleration_check());
        assert!(WINDOWS_X64.needs_acceleration_check());
        assert!(LINUX_X64.needs_acceleration_check());
    }

    #[test]
    fn discovery_dirs_follow_the_emulator() {
        let home = Path::new("/home/user");
        let user_home = Path::new("/android/user_home");
        assert_eq!(
            MAC_ARM.emulator_discovery_dirs(home, user_home, |_| None),
            vec![PathBuf::from(
                "/home/user/Library/Caches/TemporaryItems/avd/running"
            )]
        );
        assert_eq!(
            LINUX_X64.emulator_discovery_dirs(home, user_home, |name| {
                (name == "XDG_RUNTIME_DIR").then(|| "/run/user/1000".to_string())
            }),
            vec![
                PathBuf::from("/run/user/1000/avd/running"),
                PathBuf::from("/android/user_home/avd/running"),
                PathBuf::from("/home/user/.android/avd/running"),
            ]
        );
        assert_eq!(
            WINDOWS_X64.emulator_discovery_dirs(home, user_home, |name| {
                (name == "LOCALAPPDATA").then(|| "/local".to_string())
            }),
            vec![Path::new("/local").join("Temp").join("avd").join("running")]
        );
    }

    #[test]
    fn acceleration_check_output_is_reduced_to_its_reason() {
        let output = "accel:\n0\nWHPX(10.0.22631) is installed and usable.\naccel\n";
        assert_eq!(
            acceleration_check_message(output),
            "WHPX(10.0.22631) is installed and usable."
        );
        let output = "accel:\n8\n/dev/kvm is not found: VT disabled in BIOS or KVM kernel module not loaded\naccel\n";
        assert_eq!(
            acceleration_check_message(output),
            "/dev/kvm is not found: VT disabled in BIOS or KVM kernel module not loaded"
        );
    }
}
