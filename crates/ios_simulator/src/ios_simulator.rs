//! Creates, edits and removes iOS simulators and their runtimes through `xcrun simctl` and
//! `xcodebuild`, for the iOS side of the devices dock. Showing a simulator stays with the
//! sidebar, which embeds it through SimulatorKit.

mod catalog;

pub use android_sdk::{DownloadProgress, PrepareJob, PrepareStep, PrepareStepKind, format_bytes};
pub use catalog::{
    Appearance, Catalog, CatalogRuntime, LANGUAGES, ProductFamily, RuntimeImage, Screen,
    SdkRequest, SimDevice, SimDeviceType, SimLanguage, SimRuntime, SimState, SimctlInventory,
    compare_versions, display_version, runtime_version_from_identifier, version_key,
};

use anyhow::{Context as _, Result, anyhow};
use catalog::DownloadLine;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, AsyncApp, Context, EventEmitter, SharedString, Task, WeakEntity};
use http_client::HttpClient;
use smol::process::Command;
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

const CATALOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Runtimes are installed on the system volume, under `/Library/Developer/CoreSimulator`.
const RUNTIME_VOLUME: &str = "/Library/Developer";

/// Where Xcode stands on this Mac, which decides whether the iOS tab can manage simulators at
/// all (board J) and which runtime it asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XcodeStatus {
    /// What `xcode-select -p` points at.
    pub developer_dir: Option<PathBuf>,
    /// An Xcode installed anywhere Spotlight or `/Applications` finds it.
    pub xcode_app: Option<PathBuf>,
    /// `(version, build)` of the selected Xcode.
    pub xcode_version: Option<(String, String)>,
    pub license_accepted: bool,
    pub sdk_request: Option<SdkRequest>,
}

impl XcodeStatus {
    pub fn command_line_tools_found(&self) -> bool {
        self.developer_dir.is_some()
    }

    /// `xcode-select` still points at the Command Line Tools, which have no simulator.
    pub fn selects_command_line_tools(&self) -> bool {
        self.developer_dir
            .as_deref()
            .is_some_and(|dir| dir.starts_with("/Library/Developer/CommandLineTools"))
    }

    pub fn is_ready(&self) -> bool {
        self.xcode_version.is_some() && !self.selects_command_line_tools() && self.license_accepted
    }

    pub fn version(&self) -> Option<&str> {
        self.xcode_version
            .as_ref()
            .map(|(version, _)| version.as_str())
    }
}

/// A row of step D: a runtime that is installed, downloadable, or listed but unusable.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeOption {
    pub version: String,
    pub build: String,
    pub installed: bool,
    pub download_bytes: Option<u64>,
    pub prerelease: bool,
    /// The runtime the selected Xcode builds against.
    pub requested: bool,
    /// Why it cannot be picked, shown in place of the build line.
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimulatorSettings {
    pub name: String,
    pub device_type: String,
    pub runtime_version: String,
    pub runtime_build: String,
    pub language: SimLanguage,
    pub appearance: Appearance,
}

/// What the device manager asks [`IosSimulatorManager::prepare`] to do.
#[derive(Clone, Debug, PartialEq)]
pub enum PrepareRequest {
    Create {
        settings: SimulatorSettings,
        boot_when_ready: bool,
    },
    DownloadRuntime {
        version: String,
        build: String,
    },
}

pub enum IosSimulatorEvent {
    /// A simulator the manager booted, or the user asked to start, that the dock should show.
    OpenDevice(String),
    /// The user asked to stop a simulator; the dock owns the one it shows, so it stops it.
    StopDevice(String),
    /// Simulators were created, removed or changed on disk.
    DevicesChanged,
}

pub struct IosSimulatorManager {
    xcode: Option<XcodeStatus>,
    inventory: SimctlInventory,
    runtime_images: Vec<RuntimeImage>,
    available_bytes: Option<u64>,
    catalog: Option<Catalog>,
    loaded: bool,
    refresh_task: Option<Task<()>>,
    catalog_task: Option<Task<()>>,
    prepare_job: Option<PrepareJob>,
    prepare_task: Option<Task<()>>,
    /// Why the last job failed, shown in the dock until the next one starts.
    last_error: Option<SharedString>,
}

impl EventEmitter<IosSimulatorEvent> for IosSimulatorManager {}

struct Snapshot {
    xcode: XcodeStatus,
    inventory: SimctlInventory,
    runtime_images: Vec<RuntimeImage>,
    available_bytes: Option<u64>,
}

impl IosSimulatorManager {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            xcode: None,
            inventory: SimctlInventory::default(),
            runtime_images: Vec::new(),
            available_bytes: None,
            catalog: None,
            loaded: false,
            refresh_task: None,
            catalog_task: None,
            prepare_job: None,
            prepare_task: None,
            last_error: None,
        };
        this.refresh(cx);
        this
    }

    /// `None` until the first check of Xcode finishes.
    pub fn xcode(&self) -> Option<&XcodeStatus> {
        self.xcode.as_ref()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn is_refreshing(&self) -> bool {
        self.refresh_task.is_some()
    }

    pub fn devices(&self) -> &[SimDevice] {
        &self.inventory.devices
    }

    pub fn device(&self, udid: &str) -> Option<&SimDevice> {
        self.inventory
            .devices
            .iter()
            .find(|device| device.udid == udid)
    }

    pub fn runtimes(&self) -> &[SimRuntime] {
        &self.inventory.runtimes
    }

    pub fn device_types(&self) -> &[SimDeviceType] {
        &self.inventory.device_types
    }

    pub fn device_type(&self, identifier: &str) -> Option<&SimDeviceType> {
        self.inventory
            .device_types
            .iter()
            .find(|device_type| device_type.identifier == identifier)
    }

    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_ref()
    }

    pub fn is_loading_catalog(&self) -> bool {
        self.catalog_task.is_some()
    }

    pub fn available_bytes(&self) -> Option<u64> {
        self.available_bytes
    }

    pub fn prepare_job(&self) -> Option<&PrepareJob> {
        self.prepare_job.as_ref()
    }

    pub fn last_error(&self) -> Option<&SharedString> {
        self.last_error.as_ref()
    }

    pub fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.last_error = None;
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.prepare_job.is_some()
    }

    pub fn runtime(&self, identifier: &str) -> Option<&SimRuntime> {
        self.inventory
            .runtimes
            .iter()
            .find(|runtime| runtime.identifier == identifier)
    }

    pub fn runtime_disk_bytes(&self, runtime: &SimRuntime) -> Option<u64> {
        self.runtime_images
            .iter()
            .find(|image| image.build == runtime.build)
            .map(|image| image.disk_bytes)
    }

    /// Names of the simulators created with `runtime`.
    pub fn runtime_used_by(&self, runtime: &SimRuntime) -> Vec<String> {
        self.inventory
            .devices
            .iter()
            .filter(|device| device.runtime_identifier == runtime.identifier)
            .map(|device| device.name.clone())
            .collect()
    }

    pub fn runtime_used_by_other_device(&self, runtime_identifier: &str, except: &str) -> bool {
        self.inventory
            .devices
            .iter()
            .any(|device| device.udid != except && device.runtime_identifier == runtime_identifier)
    }

    /// Where every simulator's data lives.
    pub fn devices_dir(&self) -> PathBuf {
        paths::home_dir().join("Library/Developer/CoreSimulator/Devices")
    }

    pub fn devices_disk_bytes(&self) -> u64 {
        self.inventory
            .devices
            .iter()
            .map(|device| device.disk_bytes)
            .sum()
    }

    /// The runtime the selected Xcode builds against: its version and build, the catalog's
    /// download size, and whether it is already installed.
    pub fn requested_runtime(&self) -> Option<(CatalogRuntime, bool)> {
        let request = self.xcode.as_ref()?.sdk_request.as_ref()?;
        let runtime = self.catalog.as_ref()?.requested_runtime(request)?.clone();
        let installed = self.inventory.runtimes.iter().any(|installed| {
            installed.build == runtime.build
                || compare_versions(&installed.version, &request.sdk_version) == Ordering::Equal
        });
        Some((runtime, installed))
    }

    /// The runtimes step D lists: installed ones plus Apple's catalog, newest first, marking
    /// which the selected Xcode or `model` cannot run.
    pub fn runtime_options(&self, model: Option<&SimDeviceType>) -> Vec<RuntimeOption> {
        let sdk_version = self
            .xcode
            .as_ref()
            .and_then(|xcode| xcode.sdk_request.as_ref())
            .map(|request| request.sdk_version.clone());
        let requested_build = self.requested_runtime().map(|(runtime, _)| runtime.build);
        let mut options: Vec<RuntimeOption> = Vec::new();
        for runtime in &self.inventory.runtimes {
            options.push(RuntimeOption {
                version: display_version(&runtime.version),
                build: runtime.build.clone(),
                installed: true,
                download_bytes: None,
                prerelease: false,
                requested: false,
                unavailable: None,
            });
        }
        if let Some(catalog) = &self.catalog {
            for runtime in &catalog.runtimes {
                if let Some(installed) = options.iter_mut().find(|option| {
                    option.build == runtime.build
                        || (!runtime.prerelease
                            && compare_versions(&option.version, &runtime.version)
                                == Ordering::Equal)
                }) {
                    installed.prerelease = runtime.prerelease;
                    continue;
                }
                options.push(RuntimeOption {
                    version: display_version(&runtime.version),
                    build: runtime.build.clone(),
                    installed: false,
                    download_bytes: Some(runtime.download_bytes),
                    prerelease: runtime.prerelease,
                    requested: false,
                    unavailable: None,
                });
            }
        }
        for option in &mut options {
            option.requested = requested_build.as_deref() == Some(option.build.as_str());
            if !option.installed
                && let Some(sdk_version) = &sdk_version
                && compare_versions(&option.version, sdk_version) == Ordering::Greater
            {
                option.unavailable = Some(format!(
                    "exige Xcode {} ou mais novo",
                    display_version(&option.version)
                ));
            } else if let Some(model) = model
                && !model.supports(&option.version)
            {
                option.unavailable = Some(match (&model.min_runtime, &model.max_runtime) {
                    (Some(min), _) if compare_versions(&option.version, min) == Ordering::Less => {
                        format!(
                            "o {} exige iOS {} ou mais novo",
                            model.name,
                            display_version(min)
                                .strip_suffix(".0")
                                .unwrap_or(&display_version(min))
                        )
                    }
                    _ => format!("o {} não roda esta versão", model.name),
                });
            }
        }
        options.sort_by(|left, right| {
            compare_versions(&right.version, &left.version)
                .then_with(|| left.prerelease.cmp(&right.prerelease))
        });
        options
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            return;
        }
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = cx.background_spawn(read_snapshot()).await;
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.loaded = true;
                match snapshot {
                    Ok(snapshot) => {
                        this.xcode = Some(snapshot.xcode);
                        this.inventory = snapshot.inventory;
                        this.runtime_images = snapshot.runtime_images;
                        this.available_bytes = snapshot.available_bytes;
                    }
                    Err(error) => {
                        log::warn!("não foi possível listar os simuladores iOS: {error:#}");
                        this.xcode.get_or_insert_with(XcodeStatus::default);
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    pub fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.catalog_task.is_some() {
            return;
        }
        let http_client = cx.http_client();
        self.catalog_task = Some(cx.spawn(async move |this, cx| {
            let catalog = fetch_catalog(http_client).await;
            this.update(cx, |this, cx| {
                this.catalog_task = None;
                match catalog {
                    Ok(catalog) => this.catalog = Some(catalog),
                    Err(error) => log::warn!("catálogo de runtimes iOS indisponível: {error:#}"),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    pub fn prepare(&mut self, request: PrepareRequest, cx: &mut Context<Self>) {
        if self.prepare_job.is_some() {
            return;
        }
        let xcode_version = self
            .xcode
            .as_ref()
            .and_then(XcodeStatus::version)
            .unwrap_or_default()
            .to_string();
        let (download, settings, boot_when_ready) = match &request {
            PrepareRequest::Create {
                settings,
                boot_when_ready,
            } => {
                let installed = self.inventory.runtimes.iter().any(|runtime| {
                    runtime.build == settings.runtime_build
                        || compare_versions(&runtime.version, &settings.runtime_version)
                            == Ordering::Equal
                });
                let download = (!installed).then(|| {
                    (
                        settings.runtime_version.clone(),
                        settings.runtime_build.clone(),
                    )
                });
                (download, Some(settings.clone()), *boot_when_ready)
            }
            PrepareRequest::DownloadRuntime { version, build } => {
                (Some((version.clone(), build.clone())), None, false)
            }
        };
        let download_bytes = download.as_ref().and_then(|(_, build)| {
            self.catalog
                .as_ref()
                .and_then(|catalog| catalog.runtime_for_build(build))
                .map(|runtime| runtime.download_bytes)
        });
        let prerelease = download.as_ref().is_some_and(|(_, build)| {
            self.catalog
                .as_ref()
                .and_then(|catalog| catalog.runtime_for_build(build))
                .is_some_and(|runtime| runtime.prerelease)
        });

        let mut steps = Vec::new();
        if download.is_some() {
            steps.push(step(
                PrepareStepKind::Licenses,
                format!("Xcode {xcode_version} e licença verificados"),
            ));
            steps.push(step(
                PrepareStepKind::DiskSpace,
                "Espaço em disco verificado",
            ));
            steps.push(step(
                PrepareStepKind::Download,
                format!(
                    "Baixando o runtime iOS {}",
                    download
                        .as_ref()
                        .map(|(version, _)| display_version(version))
                        .unwrap_or_default()
                ),
            ));
        }
        if settings.is_some() {
            steps.push(step(PrepareStepKind::CreateAvd, "Criando o simulador"));
            if boot_when_ready {
                steps.push(step(PrepareStepKind::FirstBoot, "Primeira inicialização"));
            }
        }
        let version_label = download
            .as_ref()
            .map(|(version, _)| display_version(version))
            .or_else(|| {
                settings
                    .as_ref()
                    .map(|settings| display_version(&settings.runtime_version))
            })
            .unwrap_or_default();
        let job = PrepareJob {
            title: match &settings {
                Some(settings) => format!("Criando {}", settings.name).into(),
                None => format!("Baixando iOS {version_label}").into(),
            },
            description: download.as_ref().map(|_| {
                format!(
                    "O runtime do iOS {version_label} ainda não está no Mac. A Apple entrega o \
                     runtime inteiro de uma vez; depois ele serve para todos os simuladores \
                     dessa versão."
                )
                .into()
            }),
            steps,
            current: 0,
            progress: None,
            package: download
                .as_ref()
                .map(|(version, build)| download_command_label(version, build, prerelease)),
            device: settings.as_ref().map(|settings| {
                (
                    SharedString::from(settings.name.clone()),
                    SharedString::from(format!("iOS {version_label}")),
                )
            }),
        };
        self.prepare_job = Some(job);
        self.last_error = None;
        cx.notify();

        self.prepare_task = Some(cx.spawn(async move |this, cx| {
            let result = run_prepare(
                &this,
                download.map(|(version, build)| (version, build, download_bytes, prerelease)),
                settings,
                boot_when_ready,
                cx,
            )
            .await;
            this.update(cx, |this, cx| {
                this.prepare_job = None;
                this.prepare_task = None;
                if let Err(error) = &result {
                    log::error!("falha ao preparar o simulador iOS: {error:#}");
                }
                this.last_error = result.err().map(|error| format!("{error:#}").into());
                this.refresh(cx);
                cx.emit(IosSimulatorEvent::DevicesChanged);
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Stops the running job. A download is killed with its `xcodebuild`, which discards the
    /// partial image.
    pub fn cancel_prepare(&mut self, cx: &mut Context<Self>) {
        self.prepare_task = None;
        self.prepare_job = None;
        self.refresh(cx);
        cx.notify();
    }

    fn update_job(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut PrepareJob)) {
        if let Some(job) = &mut self.prepare_job {
            update(job);
            cx.notify();
        }
    }

    pub fn boot_device(&mut self, udid: String, cx: &mut Context<Self>) {
        cx.emit(IosSimulatorEvent::OpenDevice(udid));
    }

    pub fn stop_device(&mut self, udid: String, cx: &mut Context<Self>) {
        cx.emit(IosSimulatorEvent::StopDevice(udid));
    }

    pub fn shutdown_device(&mut self, udid: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.run_disk_action(cx, async move {
            simctl(&["shutdown", &udid]).await?;
            Ok(())
        })
    }

    /// Deletes a simulator and, with `remove_runtime`, the runtime it was the last user of.
    pub fn delete_device(
        &mut self,
        udid: String,
        remove_runtime: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let runtime_build = self
            .device(&udid)
            .filter(|_| remove_runtime)
            .and_then(|device| self.runtime(&device.runtime_identifier))
            .filter(|runtime| !self.runtime_used_by_other_device(&runtime.identifier, &udid))
            .map(|runtime| runtime.build.clone());
        let image = runtime_build.and_then(|build| {
            self.runtime_images
                .iter()
                .find(|image| image.build == build)
                .map(|image| image.image_identifier.clone())
        });
        self.run_disk_action(cx, async move {
            simctl(&["delete", &udid]).await?;
            if let Some(image) = image {
                simctl(&["runtime", "delete", &image]).await?;
            }
            Ok(())
        })
    }

    pub fn erase_device(&mut self, udid: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.run_disk_action(cx, async move {
            simctl(&["erase", &udid]).await?;
            Ok(())
        })
    }

    pub fn duplicate_device(&mut self, udid: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(device) = self.device(&udid) else {
            return Task::ready(Err(anyhow!("simulador não encontrado")));
        };
        let name = unique_name(
            &format!("{} (cópia)", device.name),
            self.inventory
                .devices
                .iter()
                .map(|device| device.name.as_str()),
        );
        self.run_disk_action(cx, async move {
            simctl(&["clone", &udid, &name]).await?;
            Ok(())
        })
    }

    /// Removes a runtime no simulator depends on.
    pub fn delete_runtime(&mut self, build: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(image) = self
            .runtime_images
            .iter()
            .find(|image| image.build == build)
            .map(|image| image.image_identifier.clone())
        else {
            return Task::ready(Err(anyhow!("runtime não encontrado")));
        };
        self.run_disk_action(cx, async move {
            simctl(&["runtime", "delete", &image]).await?;
            Ok(())
        })
    }

    /// Renames a simulator and rewrites its language and appearance. The appearance applies
    /// right away on a booted device; the language waits for its next boot.
    pub fn save_settings(
        &mut self,
        udid: String,
        name: String,
        language: SimLanguage,
        appearance: Appearance,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(device) = self.device(&udid).cloned() else {
            return Task::ready(Err(anyhow!("simulador não encontrado")));
        };
        self.run_disk_action(cx, async move {
            if device.name != name {
                simctl(&["rename", &udid, &name]).await?;
            }
            apply_preferences(&device, language, appearance).await
        })
    }

    /// The language and appearance stored in a simulator's preferences, which the edit form
    /// starts from.
    pub fn read_settings(&self, udid: &str, cx: &App) -> Task<(Option<SimLanguage>, Appearance)> {
        let directory = self.device(udid).and_then(SimDevice::directory);
        cx.background_spawn(async move {
            let Some(directory) = directory else {
                return (None, Appearance::Light);
            };
            let preferences = directory.join("data/Library/Preferences");
            let language = plist::Value::from_file(preferences.join(".GlobalPreferences.plist"))
                .ok()
                .and_then(|value| {
                    value
                        .as_dictionary()?
                        .get("AppleLanguages")?
                        .as_array()?
                        .first()?
                        .as_string()
                        .and_then(SimLanguage::for_language)
                });
            let appearance = plist::Value::from_file(preferences.join(STYLE_MODE_PLIST))
                .ok()
                .and_then(|value| {
                    value
                        .as_dictionary()?
                        .get("UserInterfaceStyleMode")?
                        .as_unsigned_integer()
                })
                .map_or(Appearance::Light, Appearance::from_style_mode);
            (language, appearance)
        })
    }

    fn run_disk_action(
        &mut self,
        cx: &mut Context<Self>,
        action: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) -> Task<Result<()>> {
        let action = cx.background_spawn(action);
        cx.spawn(async move |this, cx| {
            let result = action.await;
            this.update(cx, |this, cx| {
                this.refresh(cx);
                cx.emit(IosSimulatorEvent::DevicesChanged);
            })
            .log_err();
            result
        })
    }
}

const STYLE_MODE_PLIST: &str = "com.apple.uikitservices.userInterfaceStyleMode.plist";

fn step(kind: PrepareStepKind, label: impl Into<SharedString>) -> PrepareStep {
    PrepareStep {
        kind,
        label: label.into(),
        detail: None,
    }
}

fn unique_name<'a>(base: &str, existing: impl Iterator<Item = &'a str> + Clone) -> String {
    let taken = |name: &str| existing.clone().any(|existing| existing == name);
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|index| format!("{base} {index}"))
        .find(|name| !taken(name))
        .unwrap_or_else(|| base.to_string())
}

/// Release runtimes are asked for by version, which is what `-buildVersion` documents; a beta
/// shares its version with the release, so it goes by build.
fn download_arguments(version: &str, build: &str, prerelease: bool) -> Vec<String> {
    let mut arguments = vec![
        "-downloadPlatform".to_string(),
        "iOS".to_string(),
        "-buildVersion".to_string(),
        if prerelease {
            build.to_string()
        } else {
            display_version(version)
        },
    ];
    if cfg!(target_arch = "aarch64") {
        arguments.extend(["-architectureVariant".to_string(), "arm64".to_string()]);
    }
    arguments
}

fn download_command_label(version: &str, build: &str, prerelease: bool) -> String {
    let arguments = download_arguments(version, build, prerelease);
    let shown = arguments
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    format!("xcodebuild {shown}")
}

async fn run_prepare(
    this: &WeakEntity<IosSimulatorManager>,
    download: Option<(String, String, Option<u64>, bool)>,
    settings: Option<SimulatorSettings>,
    boot_when_ready: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    if let Some((version, build, download_bytes, prerelease)) = download {
        let xcode = cx.background_spawn(read_xcode_status()).await;
        anyhow::ensure!(
            xcode.is_ready(),
            "o Xcode precisa estar instalado, selecionado e com a licença aceita"
        );
        advance(this, cx, PrepareStepKind::DiskSpace, None)?;

        let available = cx
            .background_spawn(async {
                android_sdk::available_disk_bytes(Path::new(RUNTIME_VOLUME))
            })
            .await;
        if let (Some(available), Some(required)) = (available, download_bytes) {
            anyhow::ensure!(
                available > required,
                "espaço insuficiente: o runtime precisa de {} e há {} livres",
                format_bytes(required),
                format_bytes(available)
            );
        }
        let detail = match (available, download_bytes) {
            (Some(available), Some(required)) => Some(format!(
                "{} livres · o runtime ocupa {}",
                format_bytes(available),
                format_bytes(required)
            )),
            (Some(available), None) => Some(format!("{} livres", format_bytes(available))),
            _ => None,
        };
        advance(this, cx, PrepareStepKind::Download, detail)?;
        download_runtime(this, &version, &build, download_bytes, prerelease, cx).await?;
    }

    let Some(settings) = settings else {
        return Ok(());
    };
    advance(this, cx, PrepareStepKind::CreateAvd, None)?;
    let udid = cx
        .background_spawn({
            let settings = settings.clone();
            async move { create_simulator(&settings).await }
        })
        .await?;
    if boot_when_ready {
        advance(this, cx, PrepareStepKind::FirstBoot, None)?;
        cx.background_spawn({
            let udid = udid.clone();
            async move {
                simctl(&["boot", &udid]).await?;
                simctl(&["bootstatus", &udid, "-b"]).await?;
                anyhow::Ok(())
            }
        })
        .await?;
        this.update(cx, |_, cx| cx.emit(IosSimulatorEvent::OpenDevice(udid)))?;
    }
    Ok(())
}

/// Marks the steps before `kind` done and `kind` running, giving the step just finished its
/// detail line.
fn advance(
    this: &WeakEntity<IosSimulatorManager>,
    cx: &mut AsyncApp,
    kind: PrepareStepKind,
    finished_detail: Option<String>,
) -> Result<()> {
    this.update(cx, |this, cx| {
        this.update_job(cx, |job| {
            if let Some(index) = job.steps.iter().position(|step| step.kind == kind) {
                if let (Some(detail), Some(previous)) = (
                    finished_detail,
                    index
                        .checked_sub(1)
                        .and_then(|previous| job.steps.get_mut(previous)),
                ) {
                    previous.detail = Some(detail.into());
                }
                job.current = index;
                job.progress = None;
            }
        })
    })
}

async fn download_runtime(
    this: &WeakEntity<IosSimulatorManager>,
    version: &str,
    build: &str,
    download_bytes: Option<u64>,
    prerelease: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut child = Command::new("xcodebuild")
        .args(download_arguments(version, build, prerelease))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("não foi possível executar xcodebuild -downloadPlatform")?;
    let mut stdout = child.stdout.take().context("xcodebuild sem saída padrão")?;
    let mut stderr = child
        .stderr
        .take()
        .context("xcodebuild sem saída de erro")?;
    let total = download_bytes.unwrap_or(0);
    let started_at = Instant::now();
    let mut pending = String::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stdout.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        pending.push_str(&String::from_utf8_lossy(&buffer[..read]));
        // xcodebuild redraws its progress line with carriage returns.
        while let Some(end) = pending.find(['\r', '\n']) {
            let line: String = pending.drain(..=end).collect();
            let Some(parsed) = catalog::parse_download_line(&line) else {
                continue;
            };
            let progress = match parsed {
                DownloadLine::Downloading(fraction) => {
                    let downloaded = (total as f64 * f64::from(fraction)) as u64;
                    let elapsed = started_at.elapsed();
                    let remaining =
                        (fraction > 0.02).then(|| elapsed.mul_f32((1. - fraction) / fraction));
                    DownloadProgress {
                        downloaded,
                        total,
                        remaining,
                        installing: false,
                    }
                }
                DownloadLine::Installing => DownloadProgress {
                    downloaded: total,
                    total,
                    remaining: None,
                    installing: true,
                },
            };
            this.update(cx, |this, cx| {
                this.update_job(cx, |job| job.progress = Some(progress))
            })?;
        }
    }
    let status = child.status().await?;
    if !status.success() {
        let mut message = String::new();
        stderr.read_to_string(&mut message).await.log_err();
        anyhow::bail!(
            "xcodebuild -downloadPlatform falhou: {}",
            message.trim().lines().last().unwrap_or("sem detalhes")
        );
    }
    Ok(())
}

async fn create_simulator(settings: &SimulatorSettings) -> Result<String> {
    let inventory = read_inventory().await?;
    let runtime = inventory
        .runtimes
        .iter()
        .find(|runtime| runtime.build == settings.runtime_build)
        .or_else(|| {
            inventory.runtimes.iter().find(|runtime| {
                compare_versions(&runtime.version, &settings.runtime_version) == Ordering::Equal
            })
        })
        .with_context(|| {
            format!(
                "o runtime iOS {} não apareceu no simctl depois do download",
                display_version(&settings.runtime_version)
            )
        })?;
    let output = simctl(&[
        "create",
        &settings.name,
        &settings.device_type,
        &runtime.identifier,
    ])
    .await?;
    let udid = String::from_utf8_lossy(&output).trim().to_string();
    anyhow::ensure!(!udid.is_empty(), "xcrun simctl create não devolveu o UDID");
    let inventory = read_inventory().await?;
    if let Some(device) = inventory.devices.iter().find(|device| device.udid == udid) {
        apply_preferences(device, settings.language, settings.appearance).await?;
    }
    Ok(udid)
}

/// Writes the language into the device's global preferences and the appearance into its
/// UIKit style file. A booted device also gets the appearance through `simctl ui`, since it
/// would not reread the file until it restarts.
async fn apply_preferences(
    device: &SimDevice,
    language: SimLanguage,
    appearance: Appearance,
) -> Result<()> {
    let directory = device
        .directory()
        .context("o simctl não informou a pasta do simulador")?;
    let preferences = directory.join("data/Library/Preferences");
    smol::fs::create_dir_all(&preferences)
        .await
        .with_context(|| format!("criando {}", preferences.display()))?;
    let global = preferences.join(".GlobalPreferences.plist");
    update_plist(&global, |dictionary| {
        dictionary.insert(
            "AppleLanguages".into(),
            plist::Value::Array(vec![plist::Value::String(language.language.into())]),
        );
        dictionary.insert(
            "AppleLocale".into(),
            plist::Value::String(language.locale.into()),
        );
    })?;
    update_plist(&preferences.join(STYLE_MODE_PLIST), |dictionary| {
        dictionary.insert(
            "UserInterfaceStyleMode".into(),
            plist::Value::Integer(appearance.style_mode().into()),
        );
    })?;
    if device.is_booted() {
        simctl(&[
            "ui",
            &device.udid,
            "appearance",
            appearance.simctl_argument(),
        ])
        .await?;
        simctl(&[
            "spawn",
            &device.udid,
            "defaults",
            "write",
            "-g",
            "AppleLanguages",
            "-array",
            language.language,
        ])
        .await?;
        simctl(&[
            "spawn",
            &device.udid,
            "defaults",
            "write",
            "-g",
            "AppleLocale",
            language.locale,
        ])
        .await?;
    }
    Ok(())
}

fn update_plist(path: &Path, update: impl FnOnce(&mut plist::Dictionary)) -> Result<()> {
    let mut dictionary = plist::Value::from_file(path)
        .ok()
        .and_then(plist::Value::into_dictionary)
        .unwrap_or_default();
    update(&mut dictionary);
    plist::Value::Dictionary(dictionary)
        .to_file_binary(path)
        .with_context(|| format!("gravando {}", path.display()))
}

async fn simctl(arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("xcrun")
        .arg("simctl")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| {
            format!(
                "não foi possível executar xcrun simctl {}",
                arguments.join(" ")
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "xcrun simctl {} falhou: {}",
        arguments.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

async fn command_stdout(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn read_xcode_status() -> XcodeStatus {
    let developer_dir = command_stdout("xcode-select", &["-p"])
        .await
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let xcode_app = find_xcode_app().await;
    let selects_xcode = developer_dir
        .as_deref()
        .is_some_and(|dir| !dir.starts_with("/Library/Developer/CommandLineTools"));
    let xcode_version = if selects_xcode {
        command_stdout("xcodebuild", &["-version"])
            .await
            .as_deref()
            .and_then(catalog::parse_xcode_version)
    } else {
        None
    };
    let license_accepted = xcode_version.is_some()
        && Command::new("xcodebuild")
            .args(["-license", "check"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
    let sdk_request = if xcode_version.is_some() {
        match simctl(&["runtime", "match", "list", "-j"]).await {
            Ok(json) => catalog::parse_runtime_match(&json).log_err().flatten(),
            Err(error) => {
                log::warn!("{error:#}");
                None
            }
        }
    } else {
        None
    };
    XcodeStatus {
        developer_dir,
        xcode_app,
        xcode_version,
        license_accepted,
        sdk_request,
    }
}

async fn find_xcode_app() -> Option<PathBuf> {
    let default = PathBuf::from("/Applications/Xcode.app");
    if smol::fs::metadata(&default).await.is_ok() {
        return Some(default);
    }
    command_stdout(
        "mdfind",
        &["kMDItemCFBundleIdentifier == 'com.apple.dt.Xcode'"],
    )
    .await
    .and_then(|found| found.lines().next().map(PathBuf::from))
}

async fn read_inventory() -> Result<SimctlInventory> {
    let json = simctl(&["list", "-j", "devices", "runtimes", "devicetypes"]).await?;
    let mut inventory = catalog::parse_simctl_list(&json)?;
    for device_type in &mut inventory.device_types {
        let capabilities = device_type
            .bundle_path
            .join("Contents/Resources/capabilities.plist");
        device_type.screen = plist::Value::from_file(&capabilities)
            .ok()
            .as_ref()
            .and_then(catalog::parse_screen);
    }
    Ok(inventory)
}

async fn read_snapshot() -> Result<Snapshot> {
    let xcode = read_xcode_status().await;
    if !xcode.is_ready() {
        return Ok(Snapshot {
            xcode,
            inventory: SimctlInventory::default(),
            runtime_images: Vec::new(),
            available_bytes: None,
        });
    }
    let inventory = read_inventory().await?;
    let runtime_images = match simctl(&["runtime", "list", "-j"]).await {
        Ok(json) => catalog::parse_runtime_images(&json)
            .log_err()
            .unwrap_or_default(),
        Err(error) => {
            log::warn!("{error:#}");
            Vec::new()
        }
    };
    Ok(Snapshot {
        xcode,
        inventory,
        runtime_images,
        available_bytes: android_sdk::available_disk_bytes(Path::new(RUNTIME_VOLUME)),
    })
}

fn catalog_cache_path() -> PathBuf {
    paths::data_dir()
        .join("ios_simulator")
        .join("runtimes.plist")
}

async fn fetch_catalog(http_client: Arc<dyn HttpClient>) -> Result<Catalog> {
    let cache = catalog_cache_path();
    let apple_silicon = cfg!(target_arch = "aarch64");
    let is_fresh = smol::fs::metadata(&cache)
        .await
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < CATALOG_MAX_AGE);
    if is_fresh
        && let Ok(bytes) = smol::fs::read(&cache).await
        && let Some(catalog) = catalog::parse_catalog(&bytes, apple_silicon).log_err()
    {
        return Ok(catalog);
    }
    let mut response = http_client
        .get(catalog::CATALOG_URL, Default::default(), true)
        .await
        .context("requisição do índice de runtimes da Apple")?;
    anyhow::ensure!(
        response.status().is_success(),
        "o índice de runtimes respondeu {}",
        response.status()
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .context("lendo o índice de runtimes")?;
    let catalog = catalog::parse_catalog(&bytes, apple_silicon)?;
    if let Some(parent) = cache.parent() {
        smol::fs::create_dir_all(parent).await.log_err();
    }
    smol::fs::write(&cache, &bytes).await.log_err();
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_unique_copy_name() {
        let names = ["iPhone 17 Pro", "iPhone 17 Pro (cópia)"];
        assert_eq!(
            unique_name("iPhone 17 Pro (cópia)", names.iter().copied()),
            "iPhone 17 Pro (cópia) 2"
        );
        assert_eq!(unique_name("iPad", names.iter().copied()), "iPad");
    }

    #[test]
    fn download_asks_releases_by_version_and_betas_by_build() {
        let release = download_arguments("27.0", "24A434", false);
        assert_eq!(
            release[..4],
            ["-downloadPlatform", "iOS", "-buildVersion", "27.0"]
        );
        let beta = download_arguments("27.2", "24B5110a", true);
        assert_eq!(beta[3], "24B5110a");
        assert_eq!(
            download_command_label("27.0", "24A434", false),
            "xcodebuild -downloadPlatform iOS -buildVersion 27.0"
        );
    }
}
