//! Where the Chromium Embedded Framework is found at runtime.
//!
//! Shared by the library and the `web_preview_helper` binary through `#[path]`: the helper
//! has to resolve the exact same framework the browser process loaded, and pulling in the
//! library for that would drag gpui and the editor into a process that only forwards
//! Chromium's subprocess entry point.

use std::path::{Path, PathBuf};

/// The CEF build the `cef` crate generated its bindings against. An install of any other
/// version would load, then disagree about struct layouts.
pub const CEF_VERSION: &str = "150.0.14+g7c1aa68+chromium-150.0.7871.129";

pub const FRAMEWORK_NAME: &str = "Chromium Embedded Framework.framework";

/// How the framework we are about to load got onto this machine, which decides whether
/// writing next to it is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameworkSource {
    /// `CEF_PATH`, pointing at a distribution the user manages.
    Configured,
    /// Inside the `.app`, sealed by its code signature.
    Bundle,
    /// A `cargo build` artifact, so we are running out of `target/`.
    Build,
    /// Downloaded on demand into the support directory.
    Installed,
}

pub fn support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(home).join("Library/Application Support/Zed Workstation/WebPreview")
}

/// Where [`crate::cef_install`] unpacks the framework it downloads. Versioned, so a Zed
/// built against a newer CEF downloads its own instead of loading a stale one.
pub fn install_dir() -> PathBuf {
    support_dir().join("cef").join(CEF_VERSION)
}

pub fn is_installed() -> bool {
    framework_in(&install_dir()).is_some()
}

/// The directory *containing* the framework, plus where it came from.
pub fn find_framework() -> Option<(PathBuf, FrameworkSource)> {
    if let Ok(configured) = std::env::var("CEF_PATH") {
        if let Some(dir) = framework_in(Path::new(&configured)) {
            return Some((dir, FrameworkSource::Configured));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            if let Some(dir) = framework_in(&exe_dir.join("../Frameworks")) {
                return Some((dir, FrameworkSource::Bundle));
            }

            // The cef crate downloads the framework into its build script's output
            // directory, which sits next to the executable being run. Looking there rather
            // than under the working directory also covers `--target` builds, whose
            // artifacts land in `target/<triple>/<profile>`.
            if let Some(dir) = scan_build_dir(&exe_dir.join("build")) {
                return Some((dir, FrameworkSource::Build));
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        for profile in ["debug", "release"] {
            if let Some(dir) = scan_build_dir(&cwd.join(format!("target/{profile}/build"))) {
                return Some((dir, FrameworkSource::Build));
            }
        }
    }

    framework_in(&install_dir()).map(|dir| (dir, FrameworkSource::Installed))
}

/// Whether this process runs from an `.app`, where anything written next to the executable
/// would break the bundle's code signature.
pub fn running_from_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.ends_with("Contents/MacOS")))
        .unwrap_or(false)
}

fn framework_in(dir: &Path) -> Option<PathBuf> {
    dir.join(FRAMEWORK_NAME)
        .is_dir()
        .then(|| dir.to_path_buf())
}

fn scan_build_dir(build_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(build_dir).ok()?;
    for entry in entries.flatten() {
        let Ok(candidates) = std::fs::read_dir(entry.path().join("out")) else {
            continue;
        };
        for candidate in candidates.flatten() {
            let is_cef_dir = candidate
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("cef_macos_"));
            if is_cef_dir {
                if let Some(dir) = framework_in(&candidate.path()) {
                    return Some(dir);
                }
            }
        }
    }
    None
}

/// Loads the framework so the `cef` entry points resolve. Must succeed before anything
/// else in the crate touches CEF.
pub fn load_cef_library(framework_dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let binary = framework_dir
        .join(FRAMEWORK_NAME)
        .join("Chromium Embedded Framework");
    let Ok(binary) = std::ffi::CString::new(binary.as_os_str().as_bytes()) else {
        return false;
    };
    // Deliberately not `cef::library_loader::LibraryLoader`: it derives the path from the
    // executable's own directory, which no longer holds once the framework is installed
    // outside the bundle, and unloads the library when dropped.
    unsafe { cef::load_library(Some(&*binary.as_ptr().cast())) == 1 }
}
