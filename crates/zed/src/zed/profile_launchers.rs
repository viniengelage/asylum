//! Profile launchers: one `.app` per profile in `~/Applications`, so that each profile has
//! its own icon in the Dock and can be kept there while it isn't running.
//!
//! The Dock keeps apps by bundle, and every profile runs from the same `Asylum.app`, so
//! without them all profiles share one icon. A launcher is a bundle of its own (identifier,
//! name, icon and an `AsylumProfile` key) whose executables, frameworks and resources are
//! symlinks into the real app, started by a script that `exec`s the `zed` symlink. AppKit
//! takes the main bundle from the executable's path, which is the symlink inside the launcher,
//! so the process belongs to the launcher; the code signature is the real executable's, since
//! the symlink resolves to it. Running the real executable's path would make the process
//! belong to `Asylum.app` again.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use util::ResultExt as _;

/// The Info.plist key that holds the profile a launcher opens.
const PROFILE_KEY: &str = "AsylumProfile";
const ICON_FILE_NAME: &str = "profile";
const LAUNCH_SCRIPT_NAME: &str = "launch";
// Launch Services resolves a symlinked `CFBundleExecutable` before running it, which would
// start the real app instead. A script inside the launcher keeps the path: `exec` through the
// sibling symlink leaves the process's executable path, and so its main bundle, in the launcher.
const LAUNCH_SCRIPT: &str = "#!/bin/sh\nexec \"$(dirname \"$0\")/zed\" \"$@\"\n";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// What a profile's launcher is made from.
pub struct LauncherProfile {
    pub id: String,
    pub name: String,
    pub icon: &'static [u8],
}

/// The profile of the launcher this process was started from, or `None` when it runs from
/// the real app (or from no bundle at all).
pub fn launcher_profile_id() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let macos_dir = executable.parent()?;
    if !macos_dir.ends_with("Contents/MacOS") {
        return None;
    }
    profile_id_in(&macos_dir.parent()?.join("Info.plist"))
}

/// Where launchers are kept. `~/Applications` is where macOS puts per-user apps, and
/// Spotlight and Launchpad list what's there.
fn launchers_dir() -> PathBuf {
    util::paths::home_dir().join("Applications")
}

fn profile_id_in(info_plist: &Path) -> Option<String> {
    let info = plist::Value::from_file(info_plist).ok()?;
    let profile_id = info.as_dictionary()?.get(PROFILE_KEY)?.as_string()?;
    paths::is_valid_profile_id(profile_id).then(|| profile_id.to_string())
}

/// The launchers that exist now, by profile id.
fn existing_launchers(launchers_dir: &Path) -> HashMap<String, PathBuf> {
    let Ok(entries) = std::fs::read_dir(launchers_dir) else {
        return HashMap::default();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "app" {
                return None;
            }
            let profile_id = profile_id_in(&path.join("Contents").join("Info.plist"))?;
            Some((profile_id, path))
        })
        .collect()
}

/// The bundle to open `profile_id` with: its launcher when it has one, otherwise the real
/// app. `None` when this process doesn't run from a bundle, such as a `target/` build.
pub fn app_for_profile(profile_id: &str) -> Option<PathBuf> {
    let app = util::app_bundle_path()?;
    Some(
        existing_launchers(&launchers_dir())
            .remove(profile_id)
            .unwrap_or(app),
    )
}

/// Makes `~/Applications` hold exactly one launcher per profile in `profiles`, pointing at
/// the app this process runs from. Launchers of profiles that no longer exist are removed,
/// and a renamed profile's launcher is renamed, which keeps it in the Dock. Returns where
/// each profile's launcher is.
pub async fn sync_launchers(profiles: &[LauncherProfile]) -> Result<HashMap<String, PathBuf>> {
    // A `target/` build has no bundle to link to.
    let Some(app) = util::app_bundle_path() else {
        return Ok(HashMap::default());
    };
    sync_launchers_in(&launchers_dir(), &app, profiles).await
}

async fn sync_launchers_in(
    launchers_dir: &Path,
    app: &Path,
    profiles: &[LauncherProfile],
) -> Result<HashMap<String, PathBuf>> {
    let app_info = plist::Value::from_file(app.join("Contents").join("Info.plist"))
        .context("failed to read the app's Info.plist")?;
    let app_name = app
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("the app's name is not valid UTF-8")?;

    std::fs::create_dir_all(launchers_dir)
        .with_context(|| format!("failed to create {}", launchers_dir.display()))?;
    let mut existing = existing_launchers(launchers_dir);
    let mut launchers = HashMap::default();

    // Removed first, so that a new profile can take the name of a deleted one.
    let deleted: Vec<String> = existing
        .keys()
        .filter(|profile_id| !profiles.iter().any(|profile| profile.id == **profile_id))
        .cloned()
        .collect();
    for profile_id in deleted {
        if let Some(launcher) = existing.remove(&profile_id) {
            unregister(&launcher).await;
            std::fs::remove_dir_all(&launcher)
                .with_context(|| format!("failed to remove {}", launcher.display()))
                .log_err();
        }
    }

    for profile in profiles {
        let mut launcher =
            launchers_dir.join(format!("{app_name} {}.app", file_name_safe(&profile.name)));
        let taken_by_other_profile = existing
            .iter()
            .chain(launchers.iter())
            .any(|(profile_id, path)| *path == launcher && *profile_id != profile.id);
        if taken_by_other_profile {
            launcher = launchers_dir.join(format!(
                "{app_name} {} ({}).app",
                file_name_safe(&profile.name),
                profile.id
            ));
        }
        if let Some(current) = existing.remove(&profile.id)
            && current != launcher
        {
            unregister(&current).await;
            std::fs::rename(&current, &launcher).with_context(|| {
                format!(
                    "failed to rename {} to {}",
                    current.display(),
                    launcher.display()
                )
            })?;
        }
        let changed = write_launcher(&launcher, app, &app_info, app_name, profile)
            .with_context(|| format!("failed to write {}", launcher.display()))?;
        if changed {
            register(&launcher).await;
        }
        launchers.insert(profile.id.clone(), launcher);
    }

    Ok(launchers)
}

fn file_name_safe(name: &str) -> String {
    name.trim().replace(['/', ':'], "-")
}

/// Writes whatever differs from what the launcher should be, returning whether anything did.
fn write_launcher(
    launcher: &Path,
    app: &Path,
    app_info: &plist::Value,
    app_name: &str,
    profile: &LauncherProfile,
) -> Result<bool> {
    let contents = launcher.join("Contents");
    let app_contents = app.join("Contents");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(&resources)?;

    let mut changed = false;
    // The executables have to be files inside the launcher's `Contents/MacOS` (symlinks to the
    // real ones), not a symlink of the whole directory: AppKit finds the bundle from the
    // executable's path.
    for entry in std::fs::read_dir(app_contents.join("MacOS"))? {
        let entry = entry?;
        changed |= link(
            &contents.join("MacOS").join(entry.file_name()),
            &entry.path(),
        )?;
    }
    let launch_script = contents.join("MacOS").join(LAUNCH_SCRIPT_NAME);
    changed |= write_if_changed(&launch_script, LAUNCH_SCRIPT.as_bytes())?;
    std::fs::set_permissions(&launch_script, std::fs::Permissions::from_mode(0o755))?;
    changed |= link(
        &contents.join("Frameworks"),
        &app_contents.join("Frameworks"),
    )?;
    let app_icon_file = app_info
        .as_dictionary()
        .and_then(|info| info.get("CFBundleIconFile"))
        .and_then(|icon| icon.as_string());
    for entry in std::fs::read_dir(app_contents.join("Resources"))? {
        let entry = entry?;
        let is_app_icon = app_icon_file.is_some_and(|icon| {
            let icon = icon.strip_suffix(".icns").unwrap_or(icon);
            entry.path().file_stem().is_some_and(|stem| stem == icon)
        });
        if !is_app_icon {
            changed |= link(&resources.join(entry.file_name()), &entry.path())?;
        }
    }
    changed |= write_if_changed(
        &resources.join(format!("{ICON_FILE_NAME}.icns")),
        profile.icon,
    )?;

    let mut info = app_info
        .as_dictionary()
        .cloned()
        .context("the app's Info.plist is not a dictionary")?;
    let identifier = info
        .get("CFBundleIdentifier")
        .and_then(|identifier| identifier.as_string())
        .context("the app's Info.plist has no CFBundleIdentifier")?;
    let display_name = format!("{app_name} {}", profile.name.trim());
    info.insert(
        "CFBundleIdentifier".into(),
        format!("{identifier}.profile.{}", profile.id).into(),
    );
    info.insert("CFBundleName".into(), display_name.clone().into());
    info.insert("CFBundleDisplayName".into(), display_name.into());
    info.insert("CFBundleExecutable".into(), LAUNCH_SCRIPT_NAME.into());
    info.insert("CFBundleIconFile".into(), ICON_FILE_NAME.into());
    info.remove("CFBundleIconName");
    // `zed://` links and the file types stay with the real app, so opening a file from the
    // Finder doesn't land in whichever launcher registered last.
    info.remove("CFBundleURLTypes");
    info.remove("CFBundleDocumentTypes");
    info.insert(PROFILE_KEY.into(), profile.id.clone().into());
    let mut info_bytes = Vec::new();
    plist::Value::Dictionary(info)
        .to_writer_xml(&mut info_bytes)
        .context("failed to encode the launcher's Info.plist")?;
    changed |= write_if_changed(&contents.join("Info.plist"), &info_bytes)?;

    Ok(changed)
}

/// Points `link_path` at `target`, returning whether it had to change.
fn link(link_path: &Path, target: &Path) -> Result<bool> {
    if std::fs::read_link(link_path).is_ok_and(|current| current == target) {
        return Ok(false);
    }
    match std::fs::symlink_metadata(link_path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(link_path)?,
        Ok(_) => std::fs::remove_file(link_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::os::unix::fs::symlink(target, link_path)
        .with_context(|| format!("failed to link {}", link_path.display()))?;
    Ok(true)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    // Other profiles sync the same launchers, so a reader must never see half a file.
    let temporary = path.with_extension("partial");
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// Tells Launch Services about a new or changed launcher, which is what makes the Dock and
/// the Finder pick up a new icon or name.
async fn register(launcher: &Path) {
    run_lsregister("-f", launcher).await;
}

async fn unregister(launcher: &Path) {
    run_lsregister("-u", launcher).await;
}

async fn run_lsregister(flag: &str, launcher: &Path) {
    // Tests write launchers into temporary directories, which must not end up in the Launch
    // Services database.
    if cfg!(test) {
        return;
    }
    let status = util::command::new_command(LSREGISTER)
        .arg(flag)
        .arg(launcher)
        .status()
        .await;
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => log::warn!(
            "lsregister {flag} {} exited with {status}",
            launcher.display()
        ),
        Err(error) => log::warn!(
            "failed to run lsregister for {}: {error}",
            launcher.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_app(root: &Path) -> Result<PathBuf> {
        let app = root.join("Fake.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS"))?;
        std::fs::write(contents.join("MacOS").join("zed"), "zed")?;
        std::fs::write(contents.join("MacOS").join("cli"), "cli")?;
        std::fs::create_dir_all(contents.join("Frameworks").join("helper.app"))?;
        std::fs::create_dir_all(contents.join("Resources"))?;
        std::fs::write(contents.join("Resources").join("Fake.icns"), "app icon")?;
        std::fs::write(
            contents.join("Resources").join("Document.icns"),
            "document icon",
        )?;
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "dev.test.Fake".into());
        info.insert("CFBundleName".into(), "Fake".into());
        info.insert("CFBundleExecutable".into(), "zed".into());
        info.insert("CFBundleIconFile".into(), "Fake.icns".into());
        info.insert(
            "CFBundleURLTypes".into(),
            plist::Value::Array(vec!["zed".into()]),
        );
        plist::Value::Dictionary(info).to_file_xml(contents.join("Info.plist"))?;
        Ok(app)
    }

    fn launcher_info(launcher: &Path) -> Result<plist::Dictionary> {
        plist::Value::from_file(launcher.join("Contents").join("Info.plist"))?
            .into_dictionary()
            .context("Info.plist is not a dictionary")
    }

    #[test]
    fn test_sync_creates_renames_and_removes_launchers() -> Result<()> {
        let root = tempfile::tempdir()?;
        let app = fake_app(root.path())?;
        let launchers_dir = root.path().join("Applications");
        let sync = |profiles: &[LauncherProfile]| {
            futures::executor::block_on(sync_launchers_in(&launchers_dir, &app, profiles))
        };

        let launchers = sync(&[LauncherProfile {
            id: "work".into(),
            name: "Trabalho".into(),
            icon: b"teal",
        }])?;
        let launcher = launchers_dir.join("Fake Trabalho.app");
        assert_eq!(launchers.get("work"), Some(&launcher));
        let contents = launcher.join("Contents");
        let app_contents = app.join("Contents");
        assert_eq!(
            std::fs::read_link(contents.join("MacOS").join("zed"))?,
            app_contents.join("MacOS").join("zed")
        );
        assert_eq!(
            std::fs::read_link(contents.join("MacOS").join("cli"))?,
            app_contents.join("MacOS").join("cli")
        );
        assert_eq!(
            std::fs::read_link(contents.join("Frameworks"))?,
            app_contents.join("Frameworks")
        );
        assert_eq!(
            std::fs::read_link(contents.join("Resources").join("Document.icns"))?,
            app_contents.join("Resources").join("Document.icns")
        );
        assert!(!contents.join("Resources").join("Fake.icns").exists());
        assert_eq!(
            std::fs::read(contents.join("Resources").join("profile.icns"))?,
            b"teal"
        );
        let info = launcher_info(&launcher)?;
        let string = |key: &str| info.get(key).and_then(|value| value.as_string());
        assert_eq!(
            string("CFBundleIdentifier"),
            Some("dev.test.Fake.profile.work")
        );
        assert_eq!(string("CFBundleName"), Some("Fake Trabalho"));
        assert_eq!(string("CFBundleExecutable"), Some("launch"));
        let launch_script = contents.join("MacOS").join("launch");
        assert_eq!(std::fs::read_to_string(&launch_script)?, LAUNCH_SCRIPT);
        assert_eq!(
            std::fs::metadata(&launch_script)?.permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(string("CFBundleIconFile"), Some("profile"));
        assert_eq!(string(PROFILE_KEY), Some("work"));
        assert!(info.get("CFBundleURLTypes").is_none());

        let launchers = sync(&[LauncherProfile {
            id: "work".into(),
            name: "Empresa".into(),
            icon: b"rose",
        }])?;
        let renamed = launchers_dir.join("Fake Empresa.app");
        assert_eq!(launchers.get("work"), Some(&renamed));
        assert!(!launcher.exists());
        assert_eq!(
            std::fs::read(
                renamed
                    .join("Contents")
                    .join("Resources")
                    .join("profile.icns")
            )?,
            b"rose"
        );

        sync(&[])?;
        assert!(!renamed.exists());
        assert!(app.join("Contents").join("MacOS").join("zed").exists());
        Ok(())
    }

    #[test]
    fn test_sync_keeps_profiles_with_the_same_name_apart() -> Result<()> {
        let root = tempfile::tempdir()?;
        let app = fake_app(root.path())?;
        let launchers_dir = root.path().join("Applications");
        let launchers = futures::executor::block_on(sync_launchers_in(
            &launchers_dir,
            &app,
            &[
                LauncherProfile {
                    id: "work".into(),
                    name: "Trabalho".into(),
                    icon: b"teal",
                },
                LauncherProfile {
                    id: "work-2".into(),
                    name: "Trabalho".into(),
                    icon: b"rose",
                },
            ],
        ))?;
        assert_eq!(
            launchers.get("work"),
            Some(&launchers_dir.join("Fake Trabalho.app"))
        );
        assert_eq!(
            launchers.get("work-2"),
            Some(&launchers_dir.join("Fake Trabalho (work-2).app"))
        );
        Ok(())
    }
}
