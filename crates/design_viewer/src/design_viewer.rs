mod agent_toolkit;
mod render;
mod view;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fs::{Fs, MTime, RemoveOptions};
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{AsyncWriteExt as _, FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, IntoElement, PathPromptOptions,
    Render, SharedString, Task, TaskExt as _, WeakEntity, Window, actions,
};
use i18n::t;
use project::DirectoryLister;
use serde::{Deserialize, Serialize};
use settings::{DesignCanvasBackground, RegisterSetting, Settings};
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{
    HideStatusItem, ItemHandle, OpenOptions, StatusItemView, Toast, ToolkitEntry, Workspace,
};

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use render::{Bounds, Layer, ScreenLayers};
pub use view::DesignView;

actions!(
    design,
    [
        /// Opens a .fig design file in the design viewer.
        Open,
        /// Zooms the design canvas in.
        ZoomIn,
        /// Zooms the design canvas out.
        ZoomOut,
        /// Fits the focused screen, or the whole page, in the canvas.
        FitToScreen,
        /// Focuses the next screen of the page.
        NextScreen,
        /// Focuses the previous screen of the page.
        PreviousScreen,
        /// Leaves the focused screen and shows the whole page.
        ExitFocus,
        /// Moves focus to the screen search field.
        FocusSearch,
        /// Mentions the selected layer, or the focused screen, in the agent.
        MentionLayer,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_project_item::<DesignView>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(open);
    })
    .detach();
}

fn open(workspace: &mut Workspace, _: &Open, window: &mut Window, cx: &mut Context<Workspace>) {
    let paths = workspace.prompt_for_open_path(
        PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("design_viewer.open.prompt")),
        },
        DirectoryLister::Local(
            workspace.project().clone(),
            workspace.app_state().fs.clone(),
        ),
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(path) = paths.await?.and_then(|paths| paths.into_iter().next()) else {
            return Ok(());
        };
        if !is_fig(&path) {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<DesignView>(),
                        t!("design_viewer.open.not_a_fig", file = path.display()).to_string(),
                    )
                    .autohide(),
                    cx,
                );
            })?;
            return Ok(());
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, OpenOptions::default(), window, cx)
            })?
            .await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// The Design row of the status bar's toolkit menu, which asks for a .fig to open.
pub struct DesignToolkitButton {
    workspace: WeakEntity<Workspace>,
}

impl DesignToolkitButton {
    pub fn new(workspace: &Workspace, _cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
        }
    }
}

impl Render for DesignToolkitButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The status bar lists it in the toolkit menu instead; see `toolkit_entry`.
        gpui::Empty
    }
}

impl StatusItemView for DesignToolkitButton {
    fn toolkit_entry(&self, _cx: &App) -> Option<ToolkitEntry> {
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            id: task_agents::toolkit_ids::DESIGN,
            // Designs open as tabs, so there is no dock for the row to reflect.
            is_open: false,
            action: Box::new(Open),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| open(workspace, &Open, window, cx))
                    .log_err();
            }),
        })
    }

    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            !task_agents::toolkit_enabled(
                task_agents::toolkit_ids::DESIGN,
                workspace.read(cx).project(),
                cx,
            )
        })
    }

    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The toolkit settings turn it off; it has no status bar button of its own.
        None
    }
}

fn is_fig(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("fig"))
}

#[derive(Clone, Debug, RegisterSetting)]
pub struct DesignViewerSettings {
    pub canvas_background: DesignCanvasBackground,
}

impl Settings for DesignViewerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            canvas_background: content
                .design
                .as_ref()
                .and_then(|design| design.canvas_background)
                .unwrap_or_default(),
        }
    }
}

pub struct FigFile {
    abs_path: PathBuf,
    project_path: project::ProjectPath,
    entry_id: Option<project::ProjectEntryId>,
}

impl project::ProjectItem for FigFile {
    fn try_open(
        project: &Entity<project::Project>,
        path: &project::ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        if !is_fig(path.path.as_std_path()) {
            return None;
        }
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let project_path = path.clone();
        Some(Task::ready(Ok(cx.new(|_| FigFile {
            abs_path,
            project_path,
            entry_id,
        }))))
    }

    fn entry_id(&self, _cx: &App) -> Option<project::ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<project::ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

/// Bump whenever the dump script or the renderer changes their output, so cached
/// pages from an older converter are regenerated.
pub const CONVERTER_VERSION: u32 = 4;

/// The CLI version the dump script was validated against. Cached pages record it, so
/// changing the pin regenerates them.
pub const OPENPENCIL_CLI: &str = "@open-pencil/cli@0.16.0";

const DUMP_SCRIPT: &str = include_str!("../assets/dump.js");
const VIEWER_SCRIPT: &str = include_str!("../assets/viewer.js");
const WATCH_LATENCY: Duration = Duration::from_millis(500);
const MANIFEST_FILE_NAME: &str = "manifest.json";
const PAGES_DIR_NAME: &str = "pages";

#[derive(Clone, Debug, PartialEq)]
pub enum DesignViewerError {
    NodeNotFound,
    CliFailed { status: Option<i32>, stderr: String },
    InvalidDump(String),
    ReadFailed { path: PathBuf, detail: String },
    CacheFailed(String),
}

impl DesignViewerError {
    pub fn message(&self) -> SharedString {
        match self {
            DesignViewerError::NodeNotFound => t!("design_viewer.error.node_not_found"),
            DesignViewerError::CliFailed { status, stderr } => {
                let status = status.map_or_else(|| "-".to_string(), |code| code.to_string());
                t!(
                    "design_viewer.error.cli_failed",
                    status = status,
                    detail = stderr.trim()
                )
            }
            DesignViewerError::InvalidDump(detail) => {
                t!("design_viewer.error.invalid_dump", detail = detail)
            }
            DesignViewerError::ReadFailed { path, detail } => t!(
                "design_viewer.error.read_failed",
                path = path.display(),
                detail = detail
            ),
            DesignViewerError::CacheFailed(detail) => {
                t!("design_viewer.error.cache_failed", detail = detail)
            }
        }
    }

    fn cache(error: anyhow::Error) -> Self {
        DesignViewerError::CacheFailed(format!("{error:#}"))
    }
}

impl fmt::Display for DesignViewerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for DesignViewerError {}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageId(pub String);

impl PageId {
    fn file_name(&self) -> String {
        format!("{}.html", file_stem(&self.0))
    }

    fn layers_file_name(&self) -> String {
        format!("{}.layers.json", file_stem(&self.0))
    }
}

fn file_stem(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

impl fmt::Display for PageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub id: String,
    pub name: String,
    /// Relative to the top-left of the page's rendered canvas, not to the document.
    #[serde(rename = "box")]
    pub bounds: Bounds,
    #[serde(default)]
    pub node_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageEntry {
    pub id: PageId,
    pub name: String,
    pub hash: String,
    pub width: f64,
    pub height: f64,
    pub fonts: Vec<String>,
    pub screens: Vec<Screen>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub converter_version: u32,
    #[serde(default)]
    pub converter_cli: String,
    pub mtime: MTime,
    pub size: u64,
    pub sha256: String,
    pub pages: Vec<PageEntry>,
}

impl Manifest {
    pub fn page(&self, id: &PageId) -> Option<&PageEntry> {
        self.pages.iter().find(|page| &page.id == id)
    }

    pub fn screen(&self, id: &str) -> Option<(&PageEntry, &Screen)> {
        self.pages.iter().find_map(|page| {
            page.screens
                .iter()
                .find(|screen| screen.id == id)
                .map(|screen| (page, screen))
        })
    }
}

/// How a screen is referred to in the agent's composer, e.g. `design:Faturas/fatura-detalhe`.
pub fn screen_reference(page: &PageEntry, screen: &Screen) -> String {
    format!(
        "{}{}/{}",
        task_agents::DESIGN_MENTION_PREFIX,
        view::split_page_name(&page.name).1,
        screen.name
    )
}

/// A layer of a screen, e.g. `design:Faturas/fatura-detalhe/card/botão pagar`.
pub fn layer_reference(page: &PageEntry, screen: &Screen, path: &[&str]) -> String {
    let mut reference = screen_reference(page, screen);
    for name in path {
        reference.push('/');
        reference.push_str(name);
    }
    reference
}

/// How a layer mention reads in the agent's composer, e.g. `fatura-detalhe › botão pagar`.
pub fn layer_mention_title(screen: &Screen, layer: &Layer) -> String {
    format!("{} › {}", screen.name, layer.name)
}

/// The manifests read in this session, so the agent's `@` menu can list screens without
/// running the CLI.
#[derive(Default)]
struct DesignManifests(HashMap<PathBuf, Manifest>);

impl Global for DesignManifests {}

fn remember_manifest(path: PathBuf, manifest: Manifest, cx: &mut App) {
    cx.default_global::<DesignManifests>()
        .0
        .insert(path, manifest);
}

fn known_manifests(cx: &App) -> impl Iterator<Item = (&PathBuf, &Manifest)> {
    cx.try_global::<DesignManifests>()
        .into_iter()
        .flat_map(|manifests| manifests.0.iter())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadingProgress {
    pub done: usize,
    pub total: usize,
}

impl LoadingProgress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.done as f32 / self.total as f32
        }
    }

    pub fn label(&self) -> SharedString {
        t!(
            "design_viewer.loading.progress",
            done = self.done,
            total = self.total
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DesignDocumentState {
    /// `progress` is `None` while OpenPencil is still reading the file.
    Loading {
        progress: Option<LoadingProgress>,
    },
    Ready,
    Failed(DesignViewerError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DesignDocumentEvent {
    PagesChanged(Vec<PageId>),
    StateChanged,
}

pub trait Converter: Send + Sync + 'static {
    /// Returns the JSON produced by the dump script for every page of `fig_path`.
    fn dump(&self, fig_path: &Path) -> BoxFuture<'static, Result<Vec<u8>, DesignViewerError>>;
}

/// Runs the OpenPencil CLI with `args`, writing `stdin` to it when given, and returns its stdout.
async fn run_openpencil(
    args: &[&OsStr],
    stdin: Option<&str>,
) -> Result<Vec<u8>, DesignViewerError> {
    // The login shell's PATH (nvm, asdf, mise) is already in the process
    // environment, loaded at startup by `util::load_login_shell_environment`.
    let npx = which::which("npx").map_err(|_| DesignViewerError::NodeNotFound)?;
    let spawn_failed = |error: io::Error| DesignViewerError::CliFailed {
        status: None,
        stderr: error.to_string(),
    };
    let mut child = util::command::new_command(npx)
        .args(["-y", OPENPENCIL_CLI])
        .args(args)
        .stdin(if stdin.is_some() {
            util::command::Stdio::piped()
        } else {
            util::command::Stdio::null()
        })
        .stdout(util::command::Stdio::piped())
        .stderr(util::command::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(spawn_failed)?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(input.as_bytes())
            .await
            .map_err(spawn_failed)?;
        pipe.close().await.map_err(spawn_failed)?;
    }
    let output = child.output().await.map_err(spawn_failed)?;
    if !output.status.success() {
        return Err(DesignViewerError::CliFailed {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

pub struct OpenPencilConverter;

impl Converter for OpenPencilConverter {
    fn dump(&self, fig_path: &Path) -> BoxFuture<'static, Result<Vec<u8>, DesignViewerError>> {
        let fig_path = fig_path.to_path_buf();
        async move {
            run_openpencil(
                &[
                    OsStr::new("eval"),
                    fig_path.as_os_str(),
                    OsStr::new("--json"),
                    OsStr::new("--stdin"),
                ],
                Some(DUMP_SCRIPT),
            )
            .await
        }
        .boxed()
    }
}

pub fn cache_dir_for(fig_path: &Path) -> PathBuf {
    let key = hex::encode(&Sha256::digest(fig_path.to_string_lossy().as_bytes())[..16]);
    paths::design_cache_dir().join(key)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn page_path(cache_dir: &Path, id: &PageId) -> PathBuf {
    cache_dir.join(PAGES_DIR_NAME).join(id.file_name())
}

/// Kept out of the manifest, which is cloned on every navigator render and kept for the
/// agent's `@` menu.
fn page_layers_path(cache_dir: &Path, id: &PageId) -> PathBuf {
    cache_dir.join(PAGES_DIR_NAME).join(id.layers_file_name())
}

async fn load_page_layers(fs: &dyn Fs, path: &Path) -> anyhow::Result<Vec<ScreenLayers>> {
    let text = fs.load(path).await?;
    Ok(serde_json::from_str(&text)?)
}

/// The layer trees of `page` as the last revalidation of `fig_path` cached them.
pub(crate) async fn cached_page_layers(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
) -> anyhow::Result<Vec<ScreenLayers>> {
    load_page_layers(fs, &page_layers_path(&cache_dir_for(fig_path), &page.id)).await
}

async fn load_manifest(fs: &dyn Fs, cache_dir: &Path) -> Option<Manifest> {
    let path = cache_dir.join(MANIFEST_FILE_NAME);
    if !fs.is_file(&path).await {
        return None;
    }
    let text = fs.load(&path).await.log_err()?;
    serde_json::from_str(&text).log_err()
}

async fn write_manifest(
    fs: &dyn Fs,
    cache_dir: &Path,
    manifest: &Manifest,
) -> Result<(), DesignViewerError> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| DesignViewerError::cache(error.into()))?;
    fs.create_dir(cache_dir)
        .await
        .map_err(DesignViewerError::cache)?;
    fs.atomic_write(cache_dir.join(MANIFEST_FILE_NAME), text)
        .await
        .map_err(DesignViewerError::cache)
}

async fn write_viewer_script(fs: &dyn Fs, cache_dir: &Path) -> Result<(), DesignViewerError> {
    let path = cache_dir.join(render::VIEWER_SCRIPT_FILE_NAME);
    if fs.is_file(&path).await
        && fs
            .load(&path)
            .await
            .is_ok_and(|current| current == VIEWER_SCRIPT)
    {
        return Ok(());
    }
    fs.create_dir(cache_dir)
        .await
        .map_err(DesignViewerError::cache)?;
    fs.atomic_write(path, VIEWER_SCRIPT.to_string())
        .await
        .map_err(DesignViewerError::cache)
}

#[derive(Debug)]
struct Revalidation {
    manifest: Manifest,
    changed_pages: Vec<PageId>,
}

async fn revalidate_cache(
    fs: &dyn Fs,
    converter: &dyn Converter,
    fig_path: &Path,
    cache_dir: &Path,
    cached: Option<&Manifest>,
    report: &(dyn Fn(LoadingProgress) + Send + Sync),
) -> Result<Revalidation, DesignViewerError> {
    let read_failed = |error: anyhow::Error| DesignViewerError::ReadFailed {
        path: fig_path.to_path_buf(),
        detail: format!("{error:#}"),
    };
    let metadata = fs
        .metadata(fig_path)
        .await
        .map_err(read_failed)?
        .ok_or_else(|| read_failed(io::Error::from(io::ErrorKind::NotFound).into()))?;

    write_viewer_script(fs, cache_dir).await?;

    let current = cached.filter(|manifest| {
        manifest.converter_version == CONVERTER_VERSION && manifest.converter_cli == OPENPENCIL_CLI
    });
    if let Some(manifest) = current
        && manifest.mtime == metadata.mtime
        && manifest.size == metadata.len
    {
        return Ok(Revalidation {
            manifest: manifest.clone(),
            changed_pages: Vec::new(),
        });
    }

    let bytes = fs.load_bytes(fig_path).await.map_err(read_failed)?;
    let sha256 = sha256_hex(&bytes);
    drop(bytes);
    if let Some(manifest) = current
        && manifest.sha256 == sha256
    {
        let manifest = Manifest {
            mtime: metadata.mtime,
            size: metadata.len,
            ..manifest.clone()
        };
        write_manifest(fs, cache_dir, &manifest).await?;
        return Ok(Revalidation {
            manifest,
            changed_pages: Vec::new(),
        });
    }

    let output = converter.dump(fig_path).await?;
    let dump: render::Dump = serde_json::from_slice(&output)
        .map_err(|error| DesignViewerError::InvalidDump(error.to_string()))?;
    drop(output);

    fs.create_dir(&cache_dir.join(PAGES_DIR_NAME))
        .await
        .map_err(DesignViewerError::cache)?;

    let total = dump.pages.len();
    report(LoadingProgress { done: 0, total });
    let mut pages = Vec::with_capacity(total);
    let mut changed_pages = Vec::new();
    for (index, page) in dump.pages.iter().enumerate() {
        let rendered = render::render_page(page);
        let layers = serde_json::to_string(&render::page_layers(page, rendered.origin))
            .map_err(|error| DesignViewerError::cache(error.into()))?;
        // Bound variables change only the layers, so they are part of what makes a page new.
        let hash = hex::encode(
            Sha256::new()
                .chain_update(rendered.html.as_bytes())
                .chain_update(layers.as_bytes())
                .finalize(),
        );
        let id = PageId(page.id.clone());
        let path = page_path(cache_dir, &id);
        let layers_path = page_layers_path(cache_dir, &id);
        let unchanged = cached
            .and_then(|manifest| manifest.page(&id))
            .is_some_and(|entry| entry.hash == hash)
            && fs.is_file(&path).await
            && fs.is_file(&layers_path).await;
        if !unchanged {
            fs.atomic_write(path, rendered.html)
                .await
                .map_err(DesignViewerError::cache)?;
            fs.atomic_write(layers_path, layers)
                .await
                .map_err(DesignViewerError::cache)?;
            changed_pages.push(id.clone());
        }
        let (origin_x, origin_y) = rendered.origin;
        pages.push(PageEntry {
            id,
            name: page.name.clone(),
            hash,
            width: rendered.width,
            height: rendered.height,
            fonts: rendered.fonts.into_iter().collect(),
            screens: page
                .screens
                .iter()
                .map(|screen| Screen {
                    id: screen.id.clone(),
                    name: screen.name.clone(),
                    bounds: Bounds {
                        x: screen.bounds.x - origin_x,
                        y: screen.bounds.y - origin_y,
                        ..screen.bounds
                    },
                    node_count: screen.node_count,
                })
                .collect(),
        });
        report(LoadingProgress {
            done: index + 1,
            total,
        });
    }

    if let Some(manifest) = cached {
        for removed in manifest
            .pages
            .iter()
            .filter(|old| !pages.iter().any(|page| page.id == old.id))
        {
            for path in [
                page_path(cache_dir, &removed.id),
                page_layers_path(cache_dir, &removed.id),
            ] {
                fs.remove_file(
                    &path,
                    RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .map_err(DesignViewerError::cache)?;
            }
            changed_pages.push(removed.id.clone());
        }
    }

    let manifest = Manifest {
        converter_version: CONVERTER_VERSION,
        converter_cli: OPENPENCIL_CLI.to_string(),
        mtime: metadata.mtime,
        size: metadata.len,
        sha256,
        pages,
    };
    write_manifest(fs, cache_dir, &manifest).await?;
    Ok(Revalidation {
        manifest,
        changed_pages,
    })
}

/// The manifest of `fig_path`, from the cache when the file hasn't changed since it was written.
async fn manifest_for(fs: &dyn Fs, fig_path: &Path) -> Result<Manifest, DesignViewerError> {
    let cache_dir = cache_dir_for(fig_path);
    let cached = load_manifest(fs, &cache_dir).await;
    revalidate_cache(
        fs,
        &OpenPencilConverter,
        fig_path,
        &cache_dir,
        cached.as_ref(),
        &|_| {},
    )
    .await
    .map(|revalidation| revalidation.manifest)
}

pub struct DesignDocument {
    path: PathBuf,
    cache_dir: PathBuf,
    fs: Arc<dyn Fs>,
    converter: Arc<dyn Converter>,
    state: DesignDocumentState,
    manifest: Option<Manifest>,
    _watch_task: Task<()>,
    revalidate_task: Option<Task<anyhow::Result<()>>>,
}

impl EventEmitter<DesignDocumentEvent> for DesignDocument {}

impl DesignDocument {
    pub fn new(path: PathBuf, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let cache_dir = cache_dir_for(&path);
        Self::with_converter(path, fs, Arc::new(OpenPencilConverter), cache_dir, cx)
    }

    pub fn with_converter(
        path: PathBuf,
        fs: Arc<dyn Fs>,
        converter: Arc<dyn Converter>,
        cache_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let watch_task = Self::watch(path.clone(), fs.clone(), cx);
        let mut this = Self {
            path,
            cache_dir,
            fs,
            converter,
            state: DesignDocumentState::Loading { progress: None },
            manifest: None,
            _watch_task: watch_task,
            revalidate_task: None,
        };
        this.revalidate(cx);
        this
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn state(&self) -> &DesignDocumentState {
        &self.state
    }

    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    pub fn page_html_path(&self, page_id: &PageId) -> Option<PathBuf> {
        self.manifest
            .as_ref()?
            .page(page_id)
            .map(|_| page_path(&self.cache_dir, page_id))
    }

    pub fn load_layers(
        &self,
        page_id: &PageId,
        cx: &App,
    ) -> Task<anyhow::Result<Arc<Vec<ScreenLayers>>>> {
        if self
            .manifest
            .as_ref()
            .is_none_or(|manifest| manifest.page(page_id).is_none())
        {
            return Task::ready(Err(anyhow::anyhow!(
                "page {page_id} is not in the manifest"
            )));
        }
        let fs = self.fs.clone();
        let path = page_layers_path(&self.cache_dir, page_id);
        cx.background_spawn(async move { load_page_layers(fs.as_ref(), &path).await.map(Arc::new) })
    }

    pub fn revalidate(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let converter = self.converter.clone();
        let path = self.path.clone();
        let cache_dir = self.cache_dir.clone();
        let cached = self.manifest.clone();
        self.revalidate_task = Some(cx.spawn(async move |this, cx| {
            let cached = match cached {
                Some(cached) => Some(cached),
                None => {
                    let loaded = cx
                        .background_spawn({
                            let fs = fs.clone();
                            let cache_dir = cache_dir.clone();
                            async move { load_manifest(fs.as_ref(), &cache_dir).await }
                        })
                        .await;
                    if let Some(loaded) = &loaded {
                        this.update(cx, |this, cx| {
                            remember_manifest(this.path.clone(), loaded.clone(), cx);
                            this.manifest = Some(loaded.clone());
                            this.set_state(DesignDocumentState::Ready, cx);
                        })?;
                    }
                    loaded
                }
            };

            let (progress_tx, mut progress_rx) = mpsc::unbounded();
            let revalidation = cx.background_spawn(async move {
                revalidate_cache(
                    fs.as_ref(),
                    converter.as_ref(),
                    &path,
                    &cache_dir,
                    cached.as_ref(),
                    &|progress| {
                        progress_tx.unbounded_send(progress).ok();
                    },
                )
                .await
            });
            while let Some(progress) = progress_rx.next().await {
                this.update(cx, |this, cx| this.set_progress(progress, cx))?;
            }
            let result = revalidation.await;
            this.update(cx, |this, cx| this.apply_revalidation(result, cx))
        }));
    }

    fn watch(path: PathBuf, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let Some(parent) = path.parent().map(Path::to_path_buf) else {
                return;
            };
            // Editors save by replacing the file, which would end a watch on the file
            // itself, so the parent directory is watched instead.
            let (mut events, _watcher) = fs.watch(&parent, WATCH_LATENCY).await;
            let canonical_parent = fs.canonicalize(&parent).await.log_err().unwrap_or(parent);
            let canonical_path = path
                .file_name()
                .map(|name| canonical_parent.join(name))
                .unwrap_or_else(|| path.clone());
            while let Some(events) = events.next().await {
                let touches_file = events
                    .iter()
                    .any(|event| event.path == path || event.path == canonical_path);
                if touches_file && this.update(cx, |this, cx| this.revalidate(cx)).is_err() {
                    break;
                }
            }
        })
    }

    fn set_state(&mut self, state: DesignDocumentState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.emit(DesignDocumentEvent::StateChanged);
            cx.notify();
        }
    }

    fn set_progress(&mut self, progress: LoadingProgress, cx: &mut Context<Self>) {
        if matches!(self.state, DesignDocumentState::Loading { .. }) {
            self.set_state(
                DesignDocumentState::Loading {
                    progress: Some(progress),
                },
                cx,
            );
        }
    }

    fn apply_revalidation(
        &mut self,
        result: Result<Revalidation, DesignViewerError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(revalidation) => {
                remember_manifest(self.path.clone(), revalidation.manifest.clone(), cx);
                self.manifest = Some(revalidation.manifest);
                self.set_state(DesignDocumentState::Ready, cx);
                if !revalidation.changed_pages.is_empty() {
                    cx.emit(DesignDocumentEvent::PagesChanged(
                        revalidation.changed_pages,
                    ));
                }
                cx.notify();
            }
            Err(error) => {
                log::error!("design viewer: {}: {error}", self.path.display());
                self.set_state(DesignDocumentState::Failed(error), cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeConverter {
        output: Mutex<Vec<u8>>,
        calls: AtomicUsize,
    }

    impl FakeConverter {
        fn new(output: serde_json::Value) -> Arc<Self> {
            Arc::new(Self {
                output: Mutex::new(output.to_string().into_bytes()),
                calls: AtomicUsize::new(0),
            })
        }

        fn set_output(&self, output: serde_json::Value) {
            *self
                .output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = output.to_string().into_bytes();
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Converter for FakeConverter {
        fn dump(&self, _fig_path: &Path) -> BoxFuture<'static, Result<Vec<u8>, DesignViewerError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let output = self
                .output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            async move { Ok(output) }.boxed()
        }
    }

    fn dump(first_label: &str) -> serde_json::Value {
        let page = |id: &str, label: &str| {
            serde_json::json!({
                "id": id,
                "name": format!("Page {id}"),
                "background": { "r": 0, "g": 0, "b": 0, "a": 1 },
                "children": [{
                    "id": format!("{id}-screen"),
                    "type": "FRAME",
                    "name": "Screen",
                    "box": { "x": 100, "y": 50, "width": 300, "height": 200 },
                    "children": [{
                        "id": format!("{id}-label"),
                        "type": "TEXT",
                        "name": "label",
                        "box": { "x": 110, "y": 60, "width": 100, "height": 20 },
                        "characters": label,
                    }],
                }],
                "screens": [{
                    "id": format!("{id}-screen"),
                    "name": "Screen",
                    "box": { "x": 100, "y": 50, "width": 300, "height": 200 },
                }],
            })
        };
        serde_json::json!({ "pages": [page("0:1", first_label), page("0:2", "Second")] })
    }

    const FIG: &str = "/project/design.fig";
    const CACHE: &str = "/cache/design";

    async fn run(
        fs: &Arc<FakeFs>,
        converter: &Arc<FakeConverter>,
        cached: Option<&Manifest>,
    ) -> Revalidation {
        revalidate_cache(
            fs.as_ref(),
            converter.as_ref(),
            Path::new(FIG),
            Path::new(CACHE),
            cached,
            &|_| {},
        )
        .await
        .expect("revalidation succeeds")
    }

    async fn fake_fs(cx: &mut TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/project"))
            .await
            .expect("directory is created");
        fs.insert_file(FIG, b"fig-v1".to_vec()).await;
        fs
    }

    async fn page_mtime(fs: &Arc<FakeFs>, id: &str) -> Option<MTime> {
        let path = page_path(Path::new(CACHE), &PageId(id.to_string()));
        fs.metadata(&path)
            .await
            .ok()
            .flatten()
            .map(|metadata| metadata.mtime)
    }

    #[gpui::test]
    async fn same_file_is_a_cache_hit(cx: &mut TestAppContext) {
        let fs = fake_fs(cx).await;
        let converter = FakeConverter::new(dump("First"));

        let first = run(&fs, &converter, None).await;
        assert_eq!(converter.calls(), 1);
        assert_eq!(
            first.changed_pages,
            vec![PageId("0:1".into()), PageId("0:2".into())]
        );
        let screen = &first.manifest.pages[0].screens[0];
        assert_eq!((screen.bounds.x, screen.bounds.y), (0.0, 0.0));
        let layers = load_page_layers(
            fs.as_ref(),
            &page_layers_path(Path::new(CACHE), &PageId("0:1".into())),
        )
        .await
        .expect("layers were written");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].screen_id, "0:1-screen");
        let label = &layers[0].layers[0];
        assert_eq!(label.name, "label");
        assert_eq!((label.bounds.x, label.bounds.y), (10.0, 10.0));

        let loaded = load_manifest(fs.as_ref(), Path::new(CACHE))
            .await
            .expect("manifest was written");
        assert_eq!(loaded, first.manifest);

        let second = run(&fs, &converter, Some(&loaded)).await;
        assert_eq!(converter.calls(), 1);
        assert!(second.changed_pages.is_empty());
    }

    #[gpui::test]
    async fn touched_file_with_same_content_is_a_hit(cx: &mut TestAppContext) {
        let fs = fake_fs(cx).await;
        let converter = FakeConverter::new(dump("First"));
        let first = run(&fs, &converter, None).await;
        let page_before = page_mtime(&fs, "0:1").await;

        fs.touch_path(FIG).await;
        let second = run(&fs, &converter, Some(&first.manifest)).await;
        assert_eq!(converter.calls(), 1);
        assert!(second.changed_pages.is_empty());
        assert_ne!(second.manifest.mtime, first.manifest.mtime);
        assert_eq!(page_mtime(&fs, "0:1").await, page_before);
    }

    #[gpui::test]
    async fn changed_content_rewrites_only_changed_pages(cx: &mut TestAppContext) {
        let fs = fake_fs(cx).await;
        let converter = FakeConverter::new(dump("First"));
        let first = run(&fs, &converter, None).await;
        let unchanged_before = page_mtime(&fs, "0:2").await;

        fs.insert_file(FIG, b"fig-v2".to_vec()).await;
        converter.set_output(dump("Edited"));
        let second = run(&fs, &converter, Some(&first.manifest)).await;
        assert_eq!(converter.calls(), 2);
        assert_eq!(second.changed_pages, vec![PageId("0:1".into())]);
        assert_eq!(page_mtime(&fs, "0:2").await, unchanged_before);
        let html = fs
            .load(&page_path(Path::new(CACHE), &PageId("0:1".into())))
            .await
            .expect("page exists");
        assert!(html.contains("Edited"));
    }

    #[gpui::test]
    async fn document_opens_from_cache_and_reports_changes(cx: &mut TestAppContext) {
        let fs = fake_fs(cx).await;
        let converter = FakeConverter::new(dump("First"));
        run(&fs, &converter, None).await;

        let document = cx.new(|cx| {
            DesignDocument::with_converter(
                PathBuf::from(FIG),
                fs.clone(),
                converter.clone(),
                PathBuf::from(CACHE),
                cx,
            )
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&document, move |_, event: &DesignDocumentEvent, _| {
                events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event.clone());
            })
            .detach();
        });
        cx.run_until_parked();
        document.read_with(cx, |document, _| {
            assert_eq!(document.state(), &DesignDocumentState::Ready);
            assert!(
                document
                    .page_html_path(&PageId("0:1".into()))
                    .is_some_and(|path| path.ends_with("pages/0-1.html"))
            );
        });
        assert_eq!(converter.calls(), 1);

        converter.set_output(dump("Edited"));
        fs.insert_file(FIG, b"fig-v2".to_vec()).await;
        cx.executor().advance_clock(WATCH_LATENCY * 2);
        cx.run_until_parked();
        assert_eq!(converter.calls(), 2);
        let events = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            events.contains(&DesignDocumentEvent::PagesChanged(vec![PageId(
                "0:1".into()
            )]))
        );
    }

    #[test]
    fn errors_have_messages() {
        let error = DesignViewerError::CliFailed {
            status: Some(2),
            stderr: "boom\n".into(),
        };
        assert!(!error.message().is_empty());
        assert_ne!(
            DesignViewerError::NodeNotFound.message().as_ref(),
            "design_viewer.error.node_not_found"
        );
    }
}
