//! Downloads the Chromium Embedded Framework on demand.
//!
//! The framework is 300 MB unpacked, so shipping it inside the `.app` triples the download
//! everyone pays for whether or not they ever open a preview. Instead Zed fetches the same
//! archive the crate's build script uses and unpacks the framework into the support
//! directory, where [`crate::cef_paths::find_framework`] picks it up.

use anyhow::{Context as _, Result};
use async_compression::futures::bufread::BzDecoder;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _, channel::mpsc};
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task, WeakEntity};
use http_client::HttpClient;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt as _;

use crate::cef_paths;

const CDN_URL: &str = "https://cef-builds.spotifycdn.com";

/// How much has to be downloaded before the progress bar is moved again.
const PROGRESS_GRANULARITY: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CefInstallState {
    Missing,
    Downloading { received: u64, total: u64 },
    Extracting,
    Installed,
    Failed(SharedString),
}

pub struct CefInstaller {
    state: CefInstallState,
    _install_task: Option<Task<()>>,
}

struct GlobalCefInstaller(Entity<CefInstaller>);

impl Global for GlobalCefInstaller {}

impl CefInstaller {
    /// One installer per process: every preview tab watches the same download.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(installer) = cx.try_global::<GlobalCefInstaller>() {
            return installer.0.clone();
        }

        let installed = cef_paths::find_framework().is_some();
        let installer = cx.new(|_| Self {
            state: match installed {
                true => CefInstallState::Installed,
                false => CefInstallState::Missing,
            },
            _install_task: None,
        });
        cx.set_global(GlobalCefInstaller(installer.clone()));
        installer
    }

    pub fn state(&self) -> &CefInstallState {
        &self.state
    }

    pub fn is_installed(&self) -> bool {
        self.state == CefInstallState::Installed
    }

    pub fn install(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            CefInstallState::Downloading { .. }
                | CefInstallState::Extracting
                | CefInstallState::Installed
        ) {
            return;
        }

        let http_client = cx.http_client();
        self.state = CefInstallState::Downloading {
            received: 0,
            total: 0,
        };
        cx.notify();

        self._install_task = Some(cx.spawn(async move |this, cx| {
            let result = run_install(&this, http_client, cx).await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(()) => CefInstallState::Installed,
                    Err(error) => {
                        log::error!("web_preview: CEF install failed: {error:#}");
                        CefInstallState::Failed(SharedString::from(format!("{error:#}")))
                    }
                };
                cx.notify();
            })
            .log_err();
        }));
    }
}

async fn run_install(
    this: &WeakEntity<CefInstaller>,
    http_client: Arc<dyn HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let install_dir = cef_paths::install_dir();
    let archive = cef_paths::support_dir()
        .join("downloads")
        .join(archive_name());

    download_archive(this, cx, http_client, &archive).await?;

    this.update(cx, |this, cx| {
        this.state = CefInstallState::Extracting;
        cx.notify();
    })?;

    let extract = cx.background_spawn({
        let install_dir = install_dir.clone();
        async move { extract_framework(&archive, &install_dir).await }
    });
    extract.await?;

    anyhow::ensure!(
        cef_paths::is_installed(),
        "framework missing from {} after extraction",
        install_dir.display()
    );
    Ok(())
}

fn archive_name() -> String {
    let platform = match std::env::consts::ARCH {
        "x86_64" => "macosx64",
        _ => "macosarm64",
    };
    format!(
        "cef_binary_{}_{platform}_minimal.tar.bz2",
        cef_paths::CEF_VERSION
    )
}

async fn download_archive(
    this: &WeakEntity<CefInstaller>,
    cx: &mut gpui::AsyncApp,
    http_client: Arc<dyn HttpClient>,
    destination: &Path,
) -> Result<()> {
    let (progress_tx, mut progress_rx) = mpsc::unbounded::<(u64, u64)>();
    let download = cx.background_spawn({
        let destination = destination.to_path_buf();
        async move { download_file(http_client, &destination, progress_tx).await }
    });
    let progress = cx.spawn({
        let this = this.clone();
        async move |cx| {
            while let Some((received, total)) = progress_rx.next().await {
                let updated = this.update(cx, |this, cx| {
                    if let CefInstallState::Downloading { .. } = this.state {
                        this.state = CefInstallState::Downloading { received, total };
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
    destination: &Path,
    progress: mpsc::UnboundedSender<(u64, u64)>,
) -> Result<()> {
    if smol::fs::metadata(destination).await.is_ok() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        smol::fs::create_dir_all(parent).await?;
    }

    // The CDN serves the archive under its literal name, whose version contains `+`.
    let url = format!("{CDN_URL}/{}", archive_name().replace('+', "%2B"));
    let mut response = http_client
        .get(&url, Default::default(), true)
        .await
        .with_context(|| format!("requesting {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "downloading {url} failed with status {}",
        response.status()
    );
    let total = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .unwrap_or(0);

    let partial_path = destination.with_extension("partial");
    let mut file = smol::fs::File::create(&partial_path).await?;
    let body = response.body_mut();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    let mut last_reported = 0u64;
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read]).await?;
        received += bytes_read as u64;
        if received - last_reported >= PROGRESS_GRANULARITY {
            last_reported = received;
            progress.unbounded_send((received, total)).ok();
        }
    }
    file.flush().await?;
    drop(file);
    smol::fs::rename(&partial_path, destination).await?;
    Ok(())
}

/// Unpacks just the framework: the archive also carries the headers, the C++ wrapper
/// sources and a 19 MB credits page, none of which are needed to run a browser.
async fn extract_framework(archive: &Path, install_dir: &Path) -> Result<()> {
    let staging = install_dir.with_extension("partial");
    if smol::fs::metadata(&staging).await.is_ok() {
        smol::fs::remove_dir_all(&staging).await?;
    }
    smol::fs::create_dir_all(&staging).await?;

    let file = smol::fs::File::open(archive)
        .await
        .with_context(|| format!("opening {}", archive.display()))?;
    let decoder = BzDecoder::new(futures::io::BufReader::new(file));
    let mut entries = async_tar::Archive::new(decoder).entries()?;
    let mut unpacked = 0usize;
    while let Some(entry) = entries.next().await {
        let mut entry = entry?;
        let path = PathBuf::from(entry.path()?.into_owned().into_os_string());
        let Some(relative) = framework_relative_path(&path) else {
            continue;
        };
        let destination = staging.join(relative);
        if let Some(parent) = destination.parent() {
            smol::fs::create_dir_all(parent).await?;
        }
        entry.unpack(&destination).await?;
        unpacked += 1;
    }
    anyhow::ensure!(
        unpacked > 0,
        "{} contains no {}",
        archive.display(),
        cef_paths::FRAMEWORK_NAME
    );

    if smol::fs::metadata(install_dir).await.is_ok() {
        smol::fs::remove_dir_all(install_dir).await?;
    }
    if let Some(parent) = install_dir.parent() {
        smol::fs::create_dir_all(parent).await?;
    }
    smol::fs::rename(&staging, install_dir).await?;
    smol::fs::remove_file(archive).await.log_err();
    Ok(())
}

/// Rewrites `cef_binary_<version>_<platform>/Release/Chromium Embedded Framework.framework/X`
/// to `Chromium Embedded Framework.framework/X`, and skips everything else in the archive.
fn framework_relative_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == cef_paths::FRAMEWORK_NAME {
            return Some(Path::new(cef_paths::FRAMEWORK_NAME).join(components.as_path()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_framework_and_strips_the_archive_prefix() {
        let framework = format!("cef_binary_{}_macosarm64_minimal/Release/Chromium Embedded Framework.framework", cef_paths::CEF_VERSION);

        assert_eq!(
            framework_relative_path(Path::new(&format!(
                "{framework}/Resources/icudtl.dat"
            ))),
            Some(PathBuf::from(
                "Chromium Embedded Framework.framework/Resources/icudtl.dat"
            ))
        );
        assert_eq!(
            framework_relative_path(Path::new(&framework)),
            Some(PathBuf::from("Chromium Embedded Framework.framework"))
        );
        assert_eq!(
            framework_relative_path(Path::new(
                "cef_binary_150_macosarm64_minimal/include/cef_app.h"
            )),
            None
        );
    }

    /// Run with `cargo test -p web_preview -- --ignored` after a build has downloaded the
    /// archive. Unpacking 300 MB is too slow to belong in the default test run.
    #[test]
    #[ignore = "requires the CEF archive downloaded by a local build"]
    fn extracts_a_loadable_framework_from_the_archive() {
        let archive = local_archive().expect("no CEF archive under target/, run a build first");
        let destination = std::env::temp_dir().join("zed-cef-extract-test");
        std::fs::remove_dir_all(&destination).ok();

        smol::block_on(extract_framework(&archive, &destination)).expect("extraction failed");

        let framework = destination.join(cef_paths::FRAMEWORK_NAME);
        assert!(framework.join("Chromium Embedded Framework").is_file());
        assert!(framework.join("Resources/icudtl.dat").is_file());
        assert!(framework.join("Libraries/libEGL.dylib").is_file());
        std::fs::remove_dir_all(&destination).ok();
    }

    #[cfg(test)]
    fn local_archive() -> Option<PathBuf> {
        let name = archive_name();
        let mut targets = vec![PathBuf::from("../../target")];
        if let Ok(entries) = std::fs::read_dir("../../target") {
            targets.extend(entries.flatten().map(|entry| entry.path()));
        }
        for target in targets {
            for profile in ["debug", "release"] {
                let build_dir = target.join(profile).join("build");
                let Ok(entries) = std::fs::read_dir(&build_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let candidate = entry.path().join("out").join(&name);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
}
