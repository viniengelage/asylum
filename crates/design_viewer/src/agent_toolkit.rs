//! The agent's Design tools. They read the viewer's cache and only run the OpenPencil CLI for
//! what isn't cached yet. Node ids always come from the manifest: the ids `openpencil tree --page`
//! prints are local to the page and `--node` rejects them.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task, TaskExt as _, Window};
use i18n::t;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use task_agents::{
    DESIGN_MENTION_PREFIX, DesignSource, LogMention, ToolAccess, Toolkit, ToolkitCall,
    ToolkitOutput, ToolkitTool, toolkit_ids,
};
use workspace::{AppState, OpenOptions, Workspace};

use crate::render::{self, LayerKind, Tokened, UnboundProperty};
use crate::{
    DesignView, DesignViewerError, Layer, Manifest, PageEntry, PageId, Screen, ScreenLayers,
    cache_dir_for, cached_page_layers, file_stem, is_fig, known_manifests, layer_reference,
    manifest_for, remember_manifest, run_openpencil, screen_reference,
    view::{layer_path, layers_named, split_page_name},
};

const DESCRIBE_SCRIPT: &str = include_str!("../assets/describe.js");
/// Change whenever describe.js changes its output: the cache is keyed by the page's content,
/// so summaries from an older script would otherwise be served forever.
const STRUCTURE_CACHE_EXTENSION: &str = "v2.txt";
const SCREENS_DIR_NAME: &str = "screens";
const MAX_JSX_CHARS: usize = 30_000;

#[derive(Deserialize, JsonSchema)]
struct IndexInput {
    /// The .fig file, relative to the project or absolute. Defaults to the one open in the
    /// Design tab, else the project's only .fig.
    #[serde(default)]
    file: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ScreenInput {
    /// The .fig file, relative to the project or absolute. Defaults to the one open in the
    /// Design tab, else the project's only .fig.
    #[serde(default)]
    file: Option<String>,
    /// The screen: `Page/Screen` as design_index lists it, a screen name, or its id. A layer
    /// (`Page/Screen/layer`, a layer name or its id) renders just that layer.
    screen: String,
}

#[derive(Deserialize, JsonSchema)]
struct NodeInput {
    /// The .fig file, relative to the project or absolute. Defaults to the one open in the
    /// Design tab, else the project's only .fig.
    #[serde(default)]
    file: Option<String>,
    /// The layer: `Page/Screen/layer` path, a layer name or its id.
    node: String,
}

/// Mention ids carry the screen's global id and the .fig it belongs to.
pub(crate) fn mention_id(screen_id: &str, fig_path: &Path) -> String {
    format!(
        "{DESIGN_MENTION_PREFIX}{screen_id}|{}",
        fig_path.to_string_lossy()
    )
}

/// Layer ids never contain `#` or `|`, so they can follow the screen id.
pub(crate) fn layer_mention_id(screen_id: &str, layer_id: &str, fig_path: &Path) -> String {
    mention_id(&format!("{screen_id}#{layer_id}"), fig_path)
}

/// The screen id, the layer id when the mention is of a layer, and the .fig.
fn parse_mention_id(id: &str) -> Option<(&str, Option<&str>, PathBuf)> {
    let (node, path) = id.strip_prefix(DESIGN_MENTION_PREFIX)?.split_once('|')?;
    let (screen_id, layer_id) = match node.split_once('#') {
        Some((screen_id, layer_id)) => (screen_id, Some(layer_id)),
        None => (node, None),
    };
    Some((screen_id, layer_id, PathBuf::from(path)))
}

fn workspace_for(project: &Entity<Project>, cx: &App) -> Option<Entity<Workspace>> {
    let app_state = AppState::try_global(cx)?;
    let workspaces: Vec<_> = app_state
        .workspace_store
        .read(cx)
        .workspaces()
        .filter_map(|workspace| workspace.upgrade())
        .collect();
    workspaces
        .into_iter()
        .find(|workspace| workspace.read(cx).project() == project)
}

fn project_roots(project: &Entity<Project>, cx: &App) -> Vec<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

fn fig_files(project: &Entity<Project>, cx: &App) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    for worktree in project.read(cx).visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path().to_path_buf();
        for entry in worktree.files(false, 0) {
            if is_fig(entry.path.as_std_path()) {
                files.push((
                    entry.path.as_unix_str().to_string(),
                    root.join(entry.path.as_std_path()),
                ));
            }
        }
    }
    files
}

fn active_design_file(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    let workspace = workspace_for(project, cx)?;
    let view = workspace.read(cx).active_item_as::<DesignView>(cx)?;
    Some(view.read(cx).path(cx))
}

fn pick_file(
    explicit: Option<&str>,
    active: Option<PathBuf>,
    candidates: &[(String, PathBuf)],
    roots: &[PathBuf],
) -> Result<PathBuf, String> {
    if let Some(file) = explicit.map(str::trim).filter(|file| !file.is_empty()) {
        if !is_fig(Path::new(file)) {
            return Err(t!("design_viewer.agent.not_fig", file = file).to_string());
        }
        let found = candidates.iter().find(|(relative, absolute)| {
            relative == file
                || absolute.as_path() == Path::new(file)
                || absolute
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == file)
        });
        if let Some((_, absolute)) = found {
            return Ok(absolute.clone());
        }
        return explicit_file_in_roots(Path::new(file), roots)
            .ok_or_else(|| t!("design_viewer.agent.outside_project", file = file).to_string());
    }
    if let Some(active) = active {
        return Ok(active);
    }
    match candidates {
        [] => Err(t!("design_viewer.agent.no_fig").to_string()),
        [(_, only)] => Ok(only.clone()),
        _ => Err(t!(
            "design_viewer.agent.which_file",
            files = candidates
                .iter()
                .map(|(relative, _)| relative.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .to_string()),
    }
}

fn explicit_file_in_roots(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let stays_inside = path.components().all(|component| {
        matches!(
            component,
            Component::Normal(_) | Component::CurDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if !stays_inside {
        return None;
    }
    if path.is_absolute() {
        return roots
            .iter()
            .any(|root| path.starts_with(root))
            .then(|| path.to_path_buf());
    }
    roots
        .iter()
        .map(|root| root.join(path))
        .find(|candidate| candidate.is_file())
        .or_else(|| roots.first().map(|root| root.join(path)))
}

fn resolve_file(
    project: &Entity<Project>,
    file: Option<&str>,
    cx: &App,
) -> anyhow::Result<PathBuf> {
    let roots = project_roots(project, cx);
    pick_file(
        file,
        active_design_file(project, cx),
        &fig_files(project, cx),
        &roots,
    )
    .map_err(|message| anyhow!(message))
}

/// Matches a screen id, `Page/Screen` (with or without the page's number), or a screen name
/// that only one page has.
fn find_screen<'a>(manifest: &'a Manifest, query: &str) -> Option<(&'a PageEntry, &'a Screen)> {
    let query = query.trim().trim_start_matches('@');
    let query = query.strip_prefix(DESIGN_MENTION_PREFIX).unwrap_or(query);
    if let Some(found) = manifest.screen(query) {
        return Some(found);
    }
    let page_matches = |page: &PageEntry, name: &str| {
        page.name.eq_ignore_ascii_case(name)
            || split_page_name(&page.name).1.eq_ignore_ascii_case(name)
    };
    for (index, _) in query.match_indices('/') {
        let (page_name, screen_name) = (&query[..index], &query[index + 1..]);
        let found = manifest
            .pages
            .iter()
            .filter(|page| page_matches(page, page_name))
            .find_map(|page| {
                page.screens
                    .iter()
                    .find(|screen| screen.name.eq_ignore_ascii_case(screen_name))
                    .map(|screen| (page, screen))
            });
        if found.is_some() {
            return found;
        }
    }
    let mut by_name = manifest.pages.iter().flat_map(|page| {
        page.screens
            .iter()
            .filter(|screen| screen.name.eq_ignore_ascii_case(query))
            .map(move |screen| (page, screen))
    });
    let first = by_name.next()?;
    by_name.next().is_none().then_some(first)
}

fn size(width: f64, height: f64) -> String {
    format!("{}×{}", width.round(), height.round())
}

fn index_text(fig_path: &Path, manifest: &Manifest) -> String {
    let mut text = format!("{}\n", fig_path.display());
    for page in &manifest.pages {
        text.push_str(&format!(
            "{} · {}\n",
            page.name,
            size(page.width, page.height)
        ));
        for screen in &page.screens {
            text.push_str(&format!(
                "  {} · {} · {}\n",
                screen.name,
                screen.id,
                size(screen.bounds.width, screen.bounds.height)
            ));
        }
    }
    text
}

fn truncate_jsx(jsx: &str) -> String {
    let total = jsx.chars().count();
    if total <= MAX_JSX_CHARS {
        return jsx.to_string();
    }
    let mut text: String = jsx.chars().take(MAX_JSX_CHARS).collect();
    text.push_str("\n\n");
    text.push_str(&t!(
        "design_viewer.agent.jsx_truncated",
        shown = MAX_JSX_CHARS,
        total = total
    ));
    text
}

fn node_cache_path(fig_path: &Path, page: &PageEntry, node_id: &str, extension: &str) -> PathBuf {
    cache_dir_for(fig_path)
        .join(SCREENS_DIR_NAME)
        .join(&page.hash)
        .join(format!("{}.{extension}", file_stem(node_id)))
}

async fn exported(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
    node_id: &str,
    format: &str,
    extra_args: &[&str],
) -> Result<PathBuf, DesignViewerError> {
    let path = node_cache_path(fig_path, page, node_id, format);
    if fs.is_file(&path).await {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs.create_dir(parent)
            .await
            .map_err(DesignViewerError::cache)?;
    }
    let mut args = vec![
        OsStr::new("export"),
        fig_path.as_os_str(),
        OsStr::new("--node"),
        OsStr::new(node_id),
        OsStr::new("-f"),
        OsStr::new(format),
        OsStr::new("-o"),
        path.as_os_str(),
    ];
    args.extend(extra_args.iter().map(OsStr::new));
    run_openpencil(&args, None).await?;
    Ok(path)
}

async fn structure(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
    screen: &Screen,
) -> anyhow::Result<String> {
    let path = node_cache_path(fig_path, page, &screen.id, STRUCTURE_CACHE_EXTENSION);
    if fs.is_file(&path).await {
        return fs.load(&path).await;
    }
    let script = format!(
        "const NODE_ID = {};\n{DESCRIBE_SCRIPT}",
        serde_json::to_string(&screen.id)?
    );
    let output = run_openpencil(
        &[
            OsStr::new("eval"),
            fig_path.as_os_str(),
            OsStr::new("--json"),
            OsStr::new("--stdin"),
        ],
        Some(&script),
    )
    .await?;
    let text = match serde_json::from_slice::<serde_json::Value>(&output)? {
        serde_json::Value::String(text) => text,
        _ => return Err(DesignViewerError::InvalidDump(screen.id.clone()).into()),
    };
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    fs.atomic_write(path, text.clone()).await?;
    Ok(text)
}

async fn screen_text(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
    screen: &Screen,
) -> anyhow::Result<String> {
    let structure = structure(fs, fig_path, page, screen).await?;
    let jsx_path = exported(fs, fig_path, page, &screen.id, "jsx", &[]).await?;
    let jsx = fs.load(&jsx_path).await?;
    Ok(format!(
        "{} · {} · {}\n\nStructure:\n{structure}\n\nJSX:\n{}",
        screen_reference(page, screen),
        screen.id,
        size(screen.bounds.width, screen.bounds.height),
        truncate_jsx(&jsx)
    ))
}

/// The JSX of one node (a screen or a layer), exported by the CLI once per page version.
pub(crate) async fn node_jsx(
    fs: Arc<dyn Fs>,
    fig_path: PathBuf,
    page: PageEntry,
    node_id: String,
) -> anyhow::Result<String> {
    let path = exported(fs.as_ref(), &fig_path, &page, &node_id, "jsx", &[]).await?;
    let jsx = fs.load(&path).await?;
    Ok(truncate_jsx(&jsx))
}

fn kind_name(kind: LayerKind) -> &'static str {
    match kind {
        LayerKind::AutoLayout => "auto layout frame",
        LayerKind::Frame => "frame",
        LayerKind::Group => "group",
        LayerKind::Text => "text",
        LayerKind::Instance => "instance",
        LayerKind::Image => "image",
        LayerKind::Shape => "shape",
    }
}

fn unbound_name(property: UnboundProperty) -> &'static str {
    match property {
        UnboundProperty::Fill => "fill",
        UnboundProperty::Radius => "radius",
        UnboundProperty::Gap => "gap",
        UnboundProperty::Padding => "padding",
        UnboundProperty::Stroke => "stroke",
        UnboundProperty::FontSize => "font size",
        UnboundProperty::TextColor => "text color",
    }
}

fn tokened(value: String, token: Option<&String>) -> String {
    match token {
        Some(token) => format!("{value} [{token}]"),
        None => value,
    }
}

fn color_value(color: &Tokened<render::Color>) -> String {
    tokened(render::hex_color(&color.value), color.token.as_ref())
}

const MAX_SUMMARY_TEXT_CHARS: usize = 120;
const MAX_SUMMARY_CHILDREN: usize = 40;

fn layer_title(page: &PageEntry, screen: &Screen, path: &[&Layer]) -> String {
    let names: Vec<&str> = path.iter().map(|layer| layer.name.as_str()).collect();
    format!("{} › {}", screen_reference(page, screen), names.join(" › "))
}

/// What the inspector shows for the last layer of `path`, as text for the agent.
fn layer_summary(page: &PageEntry, screen: &Screen, path: &[&Layer]) -> String {
    let Some(layer) = path.last() else {
        return screen_reference(page, screen);
    };
    let mut lines = vec![
        format!(
            "{} · {} · {}",
            layer_title(page, screen, path),
            layer.id,
            kind_name(layer.kind)
        ),
        format!("Size: {}", size(layer.bounds.width, layer.bounds.height)),
        format!(
            "Position in the screen: x {}, y {}",
            render::number(layer.bounds.x - screen.bounds.x),
            render::number(layer.bounds.y - screen.bounds.y)
        ),
    ];
    if !layer.visible {
        lines.push("Hidden".to_string());
    }
    if let Some(sizing) = &layer.sizing {
        lines.push(format!(
            "Sizing: horizontal {}, vertical {}",
            sizing.horizontal.as_deref().unwrap_or("fixed"),
            sizing.vertical.as_deref().unwrap_or("fixed")
        ));
    }
    if let Some(auto_layout) = &layer.auto_layout {
        let mut line = format!(
            "Auto layout: {}, gap {}",
            if auto_layout.horizontal {
                "row"
            } else {
                "column"
            },
            tokened(
                render::number(auto_layout.gap.value),
                auto_layout.gap.token.as_ref()
            )
        );
        if let Some(primary) = &auto_layout.primary_align {
            line.push_str(&format!(", main axis {primary}"));
        }
        if let Some(counter) = &auto_layout.counter_align {
            line.push_str(&format!(", cross axis {counter}"));
        }
        lines.push(line);
        lines.push(format!(
            "Padding (top right bottom left): {}",
            tokened(
                render::padding_value(&auto_layout.padding.value),
                auto_layout.padding.token.as_ref()
            )
        ));
    }
    if let Some(fill) = &layer.fill {
        lines.push(format!("Fill: {}", color_value(fill)));
    }
    if let Some(radius) = &layer.radius {
        lines.push(format!(
            "Radius: {}",
            tokened(radius.value.clone(), radius.token.as_ref())
        ));
    }
    if let Some(stroke) = &layer.stroke {
        lines.push(format!(
            "Stroke: {} {}",
            render::number(stroke.weight),
            color_value(&stroke.color)
        ));
    }
    if let Some(shadow) = &layer.shadow {
        lines.push(format!(
            "Shadow: {}",
            tokened(shadow.value.clone(), shadow.token.as_ref())
        ));
    }
    if let Some(text) = &layer.text {
        let mut characters: String = text
            .characters
            .chars()
            .take(MAX_SUMMARY_TEXT_CHARS)
            .collect();
        if text.characters.chars().count() > MAX_SUMMARY_TEXT_CHARS {
            characters.push('…');
        }
        let mut parts = vec![format!("{characters:?}")];
        if let Some(family) = &text.family {
            parts.push(tokened(family.value.clone(), family.token.as_ref()));
        }
        if let Some(font_size) = &text.size {
            parts.push(format!(
                "size {}",
                tokened(render::number(font_size.value), font_size.token.as_ref())
            ));
        }
        parts.push(format!("weight {}", text.weight));
        if let Some(color) = &text.color {
            parts.push(format!("color {}", color_value(color)));
        }
        lines.push(format!("Text: {}", parts.join(" · ")));
    }
    if let Some(component) = &layer.component {
        lines.push(format!(
            "Instance of: {component}. Look for an existing code component matching it before \
             creating one."
        ));
    }
    if !layer.unbound.is_empty() {
        let values: Vec<String> = layer
            .unbound
            .iter()
            .map(|unbound| format!("{} {}", unbound_name(unbound.property), unbound.value))
            .collect();
        lines.push(format!(
            "Values without token (warnings): {}",
            values.join(", ")
        ));
    }
    if !layer.children.is_empty() {
        lines.push(format!(
            "Children ({}), readable with design_node by appending their name to this path:",
            layer.children.len()
        ));
        lines.extend(
            layer
                .children
                .iter()
                .take(MAX_SUMMARY_CHILDREN)
                .map(|child| {
                    let mut line = format!(
                        "- {} · {} · {}",
                        child.name,
                        kind_name(child.kind),
                        size(child.bounds.width, child.bounds.height)
                    );
                    if !child.children.is_empty() {
                        line.push_str(&format!(" · {} children", child.children.len()));
                    }
                    if !child.visible {
                        line.push_str(" · hidden");
                    }
                    line
                }),
        );
        if layer.children.len() > MAX_SUMMARY_CHILDREN {
            lines.push(format!(
                "- … {} more",
                layer.children.len() - MAX_SUMMARY_CHILDREN
            ));
        }
    }
    lines.join("\n")
}

type PageLayers = HashMap<PageId, Vec<ScreenLayers>>;

async fn load_all_layers(
    fs: &dyn Fs,
    fig_path: &Path,
    manifest: &Manifest,
) -> anyhow::Result<PageLayers> {
    let mut layers = HashMap::default();
    for page in &manifest.pages {
        layers.insert(
            page.id.clone(),
            cached_page_layers(fs, fig_path, page).await?,
        );
    }
    Ok(layers)
}

fn screen_layer_tree<'a>(layers: &'a PageLayers, page: &PageEntry, screen: &Screen) -> &'a [Layer] {
    layers
        .get(&page.id)
        .and_then(|screens| {
            screens
                .iter()
                .find(|screen_layers| screen_layers.screen_id == screen.id)
        })
        .map_or(&[], |screen_layers| screen_layers.layers.as_slice())
}

struct FoundNode<'a> {
    page: &'a PageEntry,
    screen: &'a Screen,
    /// The layer and its ancestors under the screen, outermost first.
    path: Vec<&'a Layer>,
}

#[derive(Debug, PartialEq)]
enum NodeLookupError {
    NotFound,
    Ambiguous(Vec<String>),
}

/// Matches a layer id, `Page/Screen/layer/sub-layer` by names, or a layer name that only one
/// layer of the file has.
fn find_node<'a>(
    manifest: &'a Manifest,
    layers: &'a PageLayers,
    query: &str,
) -> Result<FoundNode<'a>, NodeLookupError> {
    let query = query.trim().trim_start_matches('@');
    let query = query.strip_prefix(DESIGN_MENTION_PREFIX).unwrap_or(query);
    let screens = || {
        manifest
            .pages
            .iter()
            .flat_map(|page| page.screens.iter().map(move |screen| (page, screen)))
    };

    for (page, screen) in screens() {
        if let Some(path) = layer_path(screen_layer_tree(layers, page, screen), query) {
            return Ok(FoundNode { page, screen, path });
        }
    }

    let indices: Vec<usize> = query.match_indices('/').map(|(index, _)| index).collect();
    for index in indices.into_iter().rev() {
        let (screen_query, layer_names) = (&query[..index], &query[index + 1..]);
        let Some((page, screen)) = find_screen(manifest, screen_query) else {
            continue;
        };
        let mut children = screen_layer_tree(layers, page, screen);
        let mut path = Vec::new();
        for name in layer_names.split('/') {
            let Some(layer) = children
                .iter()
                .find(|layer| layer.name.eq_ignore_ascii_case(name))
            else {
                break;
            };
            path.push(layer);
            children = &layer.children;
        }
        if path.len() == layer_names.split('/').count() {
            return Ok(FoundNode { page, screen, path });
        }
    }

    let mut matches: Vec<FoundNode<'a>> = screens()
        .flat_map(|(page, screen)| {
            layers_named(screen_layer_tree(layers, page, screen), query)
                .into_iter()
                .map(move |path| FoundNode { page, screen, path })
        })
        .collect();
    match matches.len() {
        0 => Err(NodeLookupError::NotFound),
        1 => Ok(matches.remove(0)),
        _ => Err(NodeLookupError::Ambiguous(
            matches
                .iter()
                .map(|found| {
                    let names: Vec<&str> =
                        found.path.iter().map(|layer| layer.name.as_str()).collect();
                    layer_reference(found.page, found.screen, &names)
                })
                .collect(),
        )),
    }
}

fn node_lookup_message(error: NodeLookupError, fig_path: &Path, query: &str) -> anyhow::Error {
    match error {
        NodeLookupError::NotFound => anyhow!(
            t!(
                "design_viewer.agent.layer_not_found",
                node = query,
                file = fig_path.display()
            )
            .to_string()
        ),
        NodeLookupError::Ambiguous(matches) => anyhow!(
            t!(
                "design_viewer.agent.layer_ambiguous",
                node = query,
                matches = matches.join(", ")
            )
            .to_string()
        ),
    }
}

/// Cuts the layer out of its screen's PNG, whatever scale the PNG was exported at.
fn crop_to_layer(
    image: image::DynamicImage,
    screen: &Screen,
    layer: &Layer,
) -> image::DynamicImage {
    if screen.bounds.width <= 0.0 {
        return image;
    }
    let scale = f64::from(image.width()) / screen.bounds.width;
    // `as u32` saturates, so negative or huge offsets land on the image's edges.
    let x = (((layer.bounds.x - screen.bounds.x) * scale).round() as u32)
        .min(image.width().saturating_sub(1));
    let y = (((layer.bounds.y - screen.bounds.y) * scale).round() as u32)
        .min(image.height().saturating_sub(1));
    let width = ((layer.bounds.width * scale).round() as u32)
        .clamp(1, image.width().saturating_sub(x).max(1));
    let height = ((layer.bounds.height * scale).round() as u32)
        .clamp(1, image.height().saturating_sub(y).max(1));
    image.crop_imm(x, y, width, height)
}

/// The file the call is about and its manifest, which the `@` menu then knows too.
fn load_manifest_for(
    project: &Entity<Project>,
    file: Option<&str>,
    cx: &mut App,
) -> Task<anyhow::Result<(Arc<dyn Fs>, PathBuf, Manifest)>> {
    let fig_path = match resolve_file(project, file, cx) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    let fs = project.read(cx).fs().clone();
    cx.spawn(async move |cx| {
        let manifest = cx
            .background_spawn({
                let fs = fs.clone();
                let fig_path = fig_path.clone();
                async move { manifest_for(fs.as_ref(), &fig_path).await }
            })
            .await?;
        cx.update(|cx| remember_manifest(fig_path.clone(), manifest.clone(), cx));
        Ok((fs, fig_path, manifest))
    })
}

fn screen_or_error<'a>(
    manifest: &'a Manifest,
    fig_path: &Path,
    query: &str,
) -> anyhow::Result<(&'a PageEntry, &'a Screen)> {
    find_screen(manifest, query).ok_or_else(|| screen_not_found(fig_path, query))
}

fn screen_not_found(fig_path: &Path, query: &str) -> anyhow::Error {
    anyhow!(
        t!(
            "design_viewer.agent.screen_not_found",
            screen = query,
            file = fig_path.display()
        )
        .to_string()
    )
}

fn design_index(
    project: Entity<Project>,
    input: IndexInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let manifest = load_manifest_for(&project, input.file.as_deref(), cx);
    cx.spawn(async move |_| {
        let (_, fig_path, manifest) = manifest.await?;
        Ok(ToolkitOutput::text(index_text(&fig_path, &manifest)))
    })
}

fn design_screen(
    project: Entity<Project>,
    input: ScreenInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let manifest = load_manifest_for(&project, input.file.as_deref(), cx);
    cx.spawn(async move |cx| {
        let (fs, fig_path, manifest) = manifest.await?;
        cx.background_spawn(async move {
            let (page, screen) = screen_or_error(&manifest, &fig_path, &input.screen)?;
            let text = screen_text(fs.as_ref(), &fig_path, page, screen).await?;
            Ok(ToolkitOutput::text(text))
        })
        .await
    })
}

fn design_render(
    project: Entity<Project>,
    input: ScreenInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let manifest = load_manifest_for(&project, input.file.as_deref(), cx);
    cx.spawn(async move |cx| {
        let (fs, fig_path, manifest) = manifest.await?;
        cx.background_spawn(async move {
            let layers;
            let (page, screen, path) = match find_screen(&manifest, &input.screen) {
                Some((page, screen)) => (page, screen, Vec::new()),
                None => {
                    layers = load_all_layers(fs.as_ref(), &fig_path, &manifest).await?;
                    match find_node(&manifest, &layers, &input.screen) {
                        Ok(found) => (found.page, found.screen, found.path),
                        Err(NodeLookupError::NotFound) => {
                            return Err(screen_not_found(&fig_path, &input.screen));
                        }
                        Err(error) => {
                            return Err(node_lookup_message(error, &fig_path, &input.screen));
                        }
                    }
                }
            };
            let image = layer_image(fs.as_ref(), &fig_path, page, screen, &path).await?;
            let mut output = ToolkitOutput::text(image.heading());
            output.push_png(&image.screenshot.png);
            Ok(output)
        })
        .await
    })
}

struct LayerImage {
    screenshot: task_agents::ScaledScreenshot,
    title: String,
    id: String,
    bounds: render::Bounds,
}

impl LayerImage {
    fn heading(&self) -> String {
        format!(
            "{} · {} · {} (PNG {}×{})",
            self.title,
            self.id,
            size(self.bounds.width, self.bounds.height),
            self.screenshot.width,
            self.screenshot.height
        )
    }
}

/// The screen exported at 2x, cropped to the last layer of `path` when there is one.
async fn layer_image(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
    screen: &Screen,
    path: &[&Layer],
) -> anyhow::Result<LayerImage> {
    let png_path = exported(fs, fig_path, page, &screen.id, "png", &["-s", "2"]).await?;
    let png = fs.load_bytes(&png_path).await?;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
    let (image, title, id, bounds) = match path.last() {
        Some(layer) => (
            crop_to_layer(image, screen, layer),
            layer_title(page, screen, path),
            layer.id.clone(),
            layer.bounds,
        ),
        None => (
            image,
            screen_reference(page, screen),
            screen.id.clone(),
            screen.bounds,
        ),
    };
    Ok(LayerImage {
        screenshot: task_agents::scale_screenshot(image)?,
        title,
        id,
        bounds,
    })
}

/// The values a layer should show in the app, one row per property, with their tokens.
fn check_rows(layer: &Layer) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Size", size(layer.bounds.width, layer.bounds.height))];
    if let Some(fill) = &layer.fill {
        rows.push(("Fill", color_value(fill)));
    }
    if let Some(stroke) = &layer.stroke {
        rows.push((
            "Stroke",
            format!(
                "{} {}",
                render::number(stroke.weight),
                color_value(&stroke.color)
            ),
        ));
    }
    if let Some(radius) = &layer.radius {
        rows.push((
            "Radius",
            tokened(radius.value.clone(), radius.token.as_ref()),
        ));
    }
    if let Some(auto_layout) = &layer.auto_layout {
        rows.push((
            "Padding (top right bottom left)",
            tokened(
                render::padding_value(&auto_layout.padding.value),
                auto_layout.padding.token.as_ref(),
            ),
        ));
        rows.push((
            "Gap",
            tokened(
                render::number(auto_layout.gap.value),
                auto_layout.gap.token.as_ref(),
            ),
        ));
    }
    if let Some(text) = &layer.text {
        if let Some(family) = &text.family {
            rows.push((
                "Font family",
                tokened(family.value.clone(), family.token.as_ref()),
            ));
        }
        if let Some(font_size) = &text.size {
            rows.push((
                "Font size",
                tokened(render::number(font_size.value), font_size.token.as_ref()),
            ));
        }
        rows.push(("Font weight", text.weight.to_string()));
        if let Some(color) = &text.color {
            rows.push(("Text color", color_value(color)));
        }
    }
    if let Some(shadow) = &layer.shadow {
        rows.push((
            "Shadow",
            tokened(shadow.value.clone(), shadow.token.as_ref()),
        ));
    }
    rows
}

fn check_text(page: &PageEntry, screen: &Screen, path: &[&Layer]) -> String {
    let mut text = layer_summary(page, screen, path);
    text.push_str(
        "\n\nCompare the two images. For every row, answer `igual` when the app matches, or \
         `app: <value>` with what the app shows. Read values from the code when the screenshot \
         can't show them (tokens, exact padding). Do not change code in this step.\n\nChecklist:",
    );
    if let Some(layer) = path.last() {
        for (property, expected) in check_rows(layer) {
            text.push_str(&format!("\n- {property}: {expected} → "));
        }
    }
    text
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CheckTarget {
    Simulator,
    Android,
    WebPreview,
}

impl CheckTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Simulator => "simulator",
            Self::Android => "android",
            Self::WebPreview => "web_preview",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CheckInput {
    /// The .fig file, relative to the project or absolute. Defaults to the one open in the
    /// Design tab, else the project's only .fig.
    #[serde(default)]
    file: Option<String>,
    /// The layer: `Page/Screen/layer` path, a layer name or its id.
    node: String,
    /// A screenshot of the app (PNG or JPEG), absolute or relative to the project. When set,
    /// nothing is captured.
    #[serde(default)]
    screenshot: Option<String>,
    /// Where the app runs: the iOS simulator or Android emulator in the Devices dock, or the
    /// embedded browser. Leave empty for the device open in the Devices dock.
    #[serde(default)]
    target: Option<CheckTarget>,
}

/// Runs another toolkit's capture tool, so the user's toolkit switches still apply.
fn run_capture_tool(
    project: Entity<Project>,
    toolkit_id: &str,
    tool_name: &str,
    target_name: &str,
    input: serde_json::Value,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let toolkit = task_agents::toolkits(cx)
        .into_iter()
        .find(|toolkit| toolkit.id == toolkit_id);
    let Some(tool) = toolkit
        .as_ref()
        .and_then(|toolkit| toolkit.tools.iter().find(|tool| tool.name == tool_name))
    else {
        return Task::ready(Err(anyhow!(
            t!(
                "design_viewer.agent.capture_unavailable",
                target = target_name
            )
            .to_string()
        )));
    };
    if !task_agents::toolkit_enabled(toolkit_id, &project, cx) {
        let name = toolkit
            .as_ref()
            .map_or(toolkit_id, |toolkit| toolkit.name.as_ref());
        return Task::ready(Err(anyhow!(task_agents::toolkit_disabled_message(name))));
    }
    tool.run(ToolkitCall { project, input }, cx)
}

fn screenshot_file(project: &Entity<Project>, file: &str, cx: &App) -> PathBuf {
    let path = Path::new(file.trim());
    if path.is_absolute() {
        return path.to_path_buf();
    }
    explicit_file_in_roots(path, &project_roots(project, cx)).unwrap_or_else(|| path.to_path_buf())
}

fn app_screenshot(
    project: Entity<Project>,
    screenshot: Option<&str>,
    target: Option<CheckTarget>,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    if let Some(file) = screenshot.filter(|file| !file.trim().is_empty()) {
        let path = screenshot_file(&project, file, cx);
        let fs = project.read(cx).fs().clone();
        return cx.background_spawn(async move {
            let screenshot = async {
                let bytes = fs.load_bytes(&path).await?;
                task_agents::scale_screenshot(image::load_from_memory(&bytes)?)
            }
            .await
            .map_err(|error| {
                anyhow!(
                    t!(
                        "design_viewer.agent.screenshot_unreadable",
                        file = path.display(),
                        error = error
                    )
                    .to_string()
                )
            })?;
            let mut output = ToolkitOutput::text(format!(
                "App (screenshot {}) · {}×{}",
                path.display(),
                screenshot.width,
                screenshot.height
            ));
            output.push_png(&screenshot.png);
            Ok(output)
        });
    }
    let (toolkit_id, tool_name, input) = match target {
        Some(CheckTarget::Simulator) => (
            toolkit_ids::DEVICES,
            "device_screenshot",
            serde_json::json!({ "device": "ios" }),
        ),
        Some(CheckTarget::Android) => (
            toolkit_ids::DEVICES,
            "device_screenshot",
            serde_json::json!({ "device": "android" }),
        ),
        Some(CheckTarget::WebPreview) => (
            toolkit_ids::BROWSER,
            "browser_screenshot",
            serde_json::json!({}),
        ),
        None => (
            toolkit_ids::DEVICES,
            "device_screenshot",
            serde_json::json!({ "device": null }),
        ),
    };
    let target_name = target.map_or_else(
        || t!("design_viewer.agent.target_devices").to_string(),
        |target| target.name().to_string(),
    );
    let capture = run_capture_tool(project, toolkit_id, tool_name, &target_name, input, cx);
    if target.is_some() {
        return capture;
    }
    cx.background_spawn(async move {
        capture
            .await
            .map_err(|error| anyhow!("{error}\n{}", t!("design_viewer.agent.check_needs_source")))
    })
}

fn design_check(
    project: Entity<Project>,
    input: CheckInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let manifest = load_manifest_for(&project, input.file.as_deref(), cx);
    let app = app_screenshot(project, input.screenshot.as_deref(), input.target, cx);
    let node = input.node;
    cx.spawn(async move |cx| {
        let (fs, fig_path, manifest) = manifest.await?;
        let design = cx.background_spawn(async move {
            let layers = load_all_layers(fs.as_ref(), &fig_path, &manifest).await?;
            let found = find_node(&manifest, &layers, &node)
                .map_err(|error| node_lookup_message(error, &fig_path, &node))?;
            let image = layer_image(
                fs.as_ref(),
                &fig_path,
                found.page,
                found.screen,
                &found.path,
            )
            .await?;
            let checklist = check_text(found.page, found.screen, &found.path);
            anyhow::Ok((image, checklist))
        });
        let (design, app) = futures::join!(design, app);
        let (image, checklist) = design?;
        let app = app?;
        let mut output = ToolkitOutput::text(format!("Design: {}", image.heading()));
        output.push_png(&image.screenshot.png);
        output.content.extend(app.content);
        output.push_text(checklist);
        Ok(output)
    })
}

fn design_node(
    project: Entity<Project>,
    input: NodeInput,
    cx: &mut App,
) -> Task<anyhow::Result<ToolkitOutput>> {
    let manifest = load_manifest_for(&project, input.file.as_deref(), cx);
    cx.spawn(async move |cx| {
        let (fs, fig_path, manifest) = manifest.await?;
        cx.background_spawn(async move {
            let layers = load_all_layers(fs.as_ref(), &fig_path, &manifest).await?;
            let found = find_node(&manifest, &layers, &input.node)
                .map_err(|error| node_lookup_message(error, &fig_path, &input.node))?;
            let summary = layer_summary(found.page, found.screen, &found.path);
            let layer_id = found
                .path
                .last()
                .map(|layer| layer.id.clone())
                .context("the layer path is empty")?;
            let jsx = node_jsx(fs.clone(), fig_path.clone(), found.page.clone(), layer_id).await?;
            Ok(ToolkitOutput::text(format!("{summary}\n\nJSX:\n{jsx}")))
        })
        .await
    })
}

fn list_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    let roots = project_roots(project, cx);
    known_manifests(cx)
        .filter(|(path, _)| roots.iter().any(|root| path.starts_with(root)))
        .flat_map(|(path, manifest)| {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            manifest.pages.iter().flat_map(move |page| {
                let file_name = file_name.clone();
                page.screens.iter().map(move |screen| LogMention {
                    id: mention_id(&screen.id, path).into(),
                    title: screen_reference(page, screen).into(),
                    detail: format!(
                        "{file_name} · {}",
                        size(screen.bounds.width, screen.bounds.height)
                    )
                    .into(),
                })
            })
        })
        .collect()
}

fn describe_mention(
    project: &Entity<Project>,
    id: &str,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let Some((screen_id, layer_id, fig_path)) = parse_mention_id(id) else {
        return Task::ready(Err(anyhow!("invalid design mention: {id}")));
    };
    let screen_id = screen_id.to_string();
    let layer_id = layer_id.map(str::to_string);
    let fs = project.read(cx).fs().clone();
    cx.background_spawn(async move {
        let manifest = manifest_for(fs.as_ref(), &fig_path).await?;
        let (page, screen) = screen_or_error(&manifest, &fig_path, &screen_id)?;
        let Some(layer_id) = layer_id else {
            return screen_text(fs.as_ref(), &fig_path, page, screen).await;
        };
        // A layer mention carries the inspector summary only; design_node fetches its JSX.
        let screens = cached_page_layers(fs.as_ref(), &fig_path, page).await?;
        let path = screens
            .iter()
            .find(|screen_layers| screen_layers.screen_id == screen.id)
            .and_then(|screen_layers| layer_path(&screen_layers.layers, &layer_id))
            .ok_or_else(|| node_lookup_message(NodeLookupError::NotFound, &fig_path, &layer_id))?;
        Ok(layer_summary(page, screen, &path))
    })
}

fn open_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let (screen_id, layer_id, fig_path) =
        parse_mention_id(id).with_context(|| format!("invalid design mention: {id}"))?;
    let screen_id = screen_id.to_string();
    let layer_id = layer_id.map(str::to_string);
    let workspace = workspace_for(project, cx)
        .with_context(|| t!("design_viewer.agent.project_window_closed").to_string())?;
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(fig_path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            if let Some(view) = item.downcast::<DesignView>() {
                view.update(cx, |view, cx| match layer_id {
                    Some(layer_id) => view.focus_layer(screen_id, layer_id, cx),
                    None => view.focus_screen(screen_id, cx),
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    Ok(())
}

pub fn register_toolkit(cx: &mut App) {
    task_agents::register_design_source(
        DesignSource {
            list: Arc::new(list_mentions),
            describe: Arc::new(describe_mention),
            open: Arc::new(open_mention),
        },
        cx,
    );
    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::DESIGN.into(),
            name: "Design".into(),
            description: t!("design_viewer.agent.toolkit_description"),
            icon: "pencil".into(),
            mention: Some("@design".into()),
            tools: vec![
                ToolkitTool::new(
                    "design_index",
                    t!("design_viewer.agent.tool.index"),
                    "Lists the pages of a .fig design and the screens on each, with their ids \
                     and sizes. Call it first to learn the screen names the other design tools \
                     take; design_node and design_render also take the layers design_screen \
                     lists, as `Page/Screen/layer`. Without `file`, uses the .fig open in the \
                     Design tab, else the project's only .fig.",
                    ToolAccess::Read,
                    design_index,
                ),
                ToolkitTool::new(
                    "design_screen",
                    t!("design_viewer.agent.tool.screen"),
                    "Reads one screen of a .fig design: a structural summary (layers with type, \
                     name, size, auto layout and text) and its JSX. Use it to implement or \
                     compare a screen with code. `screen` takes `Page/Screen` from \
                     design_index, a screen name or its id. When the user mentioned a layer, \
                     call design_node instead.",
                    ToolAccess::Read,
                    design_screen,
                ),
                ToolkitTool::new(
                    "design_node",
                    t!("design_viewer.agent.tool.node"),
                    "Reads one layer of a .fig design: the inspector summary (size, auto layout, \
                     padding, fills, radius and text with their token names, the source \
                     component of an instance, values without a token as warnings) and the JSX \
                     of just that layer. Prefer it over design_screen when a layer is mentioned \
                     or you only need part of a screen. When the layer is an instance, look for \
                     an existing code component matching its source component before creating \
                     a new one. `node` takes `Page/Screen/layer` (names from design_screen), a \
                     layer name or its id.",
                    ToolAccess::Read,
                    design_node,
                ),
                ToolkitTool::new(
                    "design_render",
                    t!("design_viewer.agent.tool.render"),
                    "Renders a screen of a .fig design, or a 2x crop of one layer, to an image. \
                     `screen` takes `Page/Screen`, a screen name or id, or a layer as \
                     `Page/Screen/layer`, name or id.",
                    ToolAccess::Read,
                    design_render,
                ),
                ToolkitTool::new(
                    "design_check",
                    t!("design_viewer.agent.tool.check"),
                    "Compares one layer of a .fig design with the running app. Returns the \
                     layer's 2x crop, a screenshot of the app and a checklist of the expected \
                     values (size, colors, radius, padding, gap, font) with their tokens. The \
                     screenshot comes from `target` (simulator or android, captured like \
                     device_screenshot; web_preview, captured like browser_screenshot), from an \
                     explicit `screenshot` path, or from the device open in the Devices dock. \
                     Answer each checklist row with `igual` or `app: <value>`. Read-only: it \
                     doesn't change code. `node` takes `Page/Screen/layer`, a layer name or id.",
                    ToolAccess::Read,
                    design_check,
                ),
            ],
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bounds, PageId};

    fn screen(id: &str, name: &str) -> Screen {
        Screen {
            id: id.into(),
            name: name.into(),
            bounds: Bounds {
                x: 0.,
                y: 0.,
                width: 390.,
                height: 844.4,
            },
            node_count: 1,
        }
    }

    fn manifest() -> Manifest {
        let page = |id: &str, name: &str, screens: Vec<Screen>| PageEntry {
            id: PageId(id.into()),
            name: name.into(),
            hash: "hash".into(),
            width: 1000.,
            height: 900.,
            fonts: Vec::new(),
            screens,
        };
        Manifest {
            converter_version: crate::CONVERTER_VERSION,
            converter_cli: crate::OPENPENCIL_CLI.to_string(),
            mtime: fs::MTime::from_seconds_and_nanos(0, 0),
            size: 0,
            sha256: String::new(),
            pages: vec![
                page(
                    "0:1",
                    "08 · Faturas",
                    vec![screen("0:10", "fatura-detalhe"), screen("0:11", "lista")],
                ),
                page("0:2", "Rascunho", vec![screen("0:20", "lista")]),
            ],
        }
    }

    #[test]
    fn explicit_file_then_active_tab_then_only_fig() {
        let root = Path::new("/project");
        let roots = vec![root.to_path_buf()];
        let one = vec![("design/app.fig".to_string(), root.join("design/app.fig"))];
        let active = Some(PathBuf::from("/elsewhere/open.fig"));

        assert_eq!(
            pick_file(Some("app.fig"), active.clone(), &one, &roots),
            Ok(root.join("design/app.fig"))
        );
        assert_eq!(
            pick_file(Some("other.fig"), active.clone(), &one, &roots),
            Ok(root.join("other.fig"))
        );
        assert_eq!(
            pick_file(None, active, &one, &roots),
            Ok(PathBuf::from("/elsewhere/open.fig"))
        );
        assert_eq!(
            pick_file(None, None, &one, &roots),
            Ok(root.join("design/app.fig"))
        );
        assert!(pick_file(None, None, &[], &roots).is_err());

        let two = vec![
            ("a.fig".to_string(), root.join("a.fig")),
            ("b.fig".to_string(), root.join("b.fig")),
        ];
        let error = pick_file(None, None, &two, &roots).expect_err("asks which file");
        assert!(error.contains("a.fig") && error.contains("b.fig"));
    }

    #[test]
    fn explicit_file_must_be_a_fig_inside_the_project() {
        let root = Path::new("/project");
        let roots = vec![root.to_path_buf()];

        assert_eq!(
            pick_file(Some("/project/design/x.fig"), None, &[], &roots),
            Ok(root.join("design/x.fig"))
        );
        assert!(pick_file(Some("/etc/x.fig"), None, &[], &roots).is_err());
        assert!(pick_file(Some("../outside/x.fig"), None, &[], &roots).is_err());
        assert!(pick_file(Some("/project/../etc/x.fig"), None, &[], &roots).is_err());
        assert!(pick_file(Some("design/notes.txt"), None, &[], &roots).is_err());
        assert!(pick_file(Some("x.fig"), None, &[], &[]).is_err());
    }

    #[test]
    fn screens_match_by_reference_name_and_id() {
        let manifest = manifest();
        let id_of =
            |query: &str| find_screen(&manifest, query).map(|(_, screen)| screen.id.clone());
        assert_eq!(id_of("Faturas/fatura-detalhe").as_deref(), Some("0:10"));
        assert_eq!(
            id_of("08 · Faturas/fatura-detalhe").as_deref(),
            Some("0:10")
        );
        assert_eq!(
            id_of("@design:Faturas/fatura-detalhe").as_deref(),
            Some("0:10")
        );
        assert_eq!(id_of("fatura-detalhe").as_deref(), Some("0:10"));
        assert_eq!(id_of("0:20").as_deref(), Some("0:20"));
        assert_eq!(id_of("Rascunho/lista").as_deref(), Some("0:20"));
        assert_eq!(id_of("lista"), None);
        assert_eq!(id_of("nada"), None);
    }

    #[test]
    fn index_lists_pages_and_screens() {
        let text = index_text(Path::new("/project/app.fig"), &manifest());
        assert!(text.starts_with("/project/app.fig\n08 · Faturas · 1000×900\n"));
        assert!(text.contains("  fatura-detalhe · 0:10 · 390×844\n"));
    }

    #[test]
    fn long_jsx_is_truncated_with_a_note() {
        assert_eq!(truncate_jsx("<Frame />"), "<Frame />");
        let long = "a".repeat(MAX_JSX_CHARS + 10);
        let truncated = truncate_jsx(&long);
        assert!(truncated.starts_with(&"a".repeat(MAX_JSX_CHARS)));
        assert!(truncated.len() > MAX_JSX_CHARS + 2);
        assert!(!truncated.contains(&"a".repeat(MAX_JSX_CHARS + 1)));
    }

    #[test]
    fn mention_ids_round_trip() {
        let path = Path::new("/project/design/app.fig");
        let id = mention_id("0:10", path);
        assert_eq!(id, "design:0:10|/project/design/app.fig");
        assert_eq!(
            parse_mention_id(&id),
            Some(("0:10", None, path.to_path_buf()))
        );
        assert_eq!(parse_mention_id("container:abc"), None);
    }

    #[test]
    fn layer_mention_ids_round_trip() {
        let path = Path::new("/project/design/app.fig");
        let id = layer_mention_id("0:10", "I0:12;3:4", path);
        assert_eq!(id, "design:0:10#I0:12;3:4|/project/design/app.fig");
        assert_eq!(
            parse_mention_id(&id),
            Some(("0:10", Some("I0:12;3:4"), path.to_path_buf()))
        );
        assert_eq!(
            parse_mention_id(&mention_id("0:10", path)),
            Some(("0:10", None, path.to_path_buf()))
        );
    }

    fn layer(id: &str, name: &str, x: f64, y: f64, children: Vec<Layer>) -> Layer {
        Layer {
            id: id.into(),
            name: name.into(),
            kind: LayerKind::Frame,
            bounds: Bounds {
                x,
                y,
                width: 100.,
                height: 40.,
            },
            visible: true,
            component: None,
            auto_layout: None,
            sizing: None,
            fill: None,
            radius: None,
            stroke: None,
            shadow: None,
            text: None,
            unbound: Vec::new(),
            children,
        }
    }

    fn layers() -> PageLayers {
        let detail = ScreenLayers {
            screen_id: "0:10".into(),
            layers: vec![layer(
                "1:1",
                "card",
                10.,
                20.,
                vec![layer("1:2", "botão", 20., 30., Vec::new())],
            )],
        };
        let list = ScreenLayers {
            screen_id: "0:11".into(),
            layers: vec![layer("1:3", "botão", 0., 0., Vec::new())],
        };
        HashMap::from_iter([(PageId("0:1".into()), vec![detail, list])])
    }

    #[test]
    fn layers_match_by_id_path_and_unique_name() {
        let manifest = manifest();
        let layers = layers();
        let id_of = |query: &str| {
            find_node(&manifest, &layers, query).map(|found| {
                (
                    found.screen.id.clone(),
                    found.path.last().map(|layer| layer.id.clone()),
                )
            })
        };
        assert_eq!(id_of("1:2"), Ok(("0:10".into(), Some("1:2".into()))));
        assert_eq!(
            id_of("Faturas/fatura-detalhe/card/botão"),
            Ok(("0:10".into(), Some("1:2".into())))
        );
        assert_eq!(
            id_of("@design:08 · Faturas/fatura-detalhe/card"),
            Ok(("0:10".into(), Some("1:1".into())))
        );
        assert_eq!(id_of("card"), Ok(("0:10".into(), Some("1:1".into()))));
        assert_eq!(id_of("nada"), Err(NodeLookupError::NotFound));
        assert_eq!(
            id_of("Faturas/fatura-detalhe/card/nada"),
            Err(NodeLookupError::NotFound)
        );
        match id_of("botão") {
            Err(NodeLookupError::Ambiguous(matches)) => assert_eq!(
                matches,
                vec![
                    "design:Faturas/fatura-detalhe/card/botão".to_string(),
                    "design:Faturas/lista/botão".to_string(),
                ]
            ),
            other => panic!("expected an ambiguous match, got {other:?}"),
        }
    }

    #[test]
    fn layer_summary_lists_tokens_component_and_warnings() {
        let manifest = manifest();
        let page = &manifest.pages[0];
        let screen = &page.screens[0];
        let white = render::Color {
            r: 1.,
            g: 1.,
            b: 1.,
            a: 1.,
        };
        let mut button = layer("1:2", "botão", 20., 30., Vec::new());
        button.kind = LayerKind::Instance;
        button.component = Some("Button/Primary".into());
        button.fill = Some(Tokened {
            value: white,
            token: Some("color/surface".into()),
        });
        button.radius = Some(Tokened {
            value: "8".into(),
            token: None,
        });
        button.unbound = vec![render::Unbound {
            property: UnboundProperty::Radius,
            value: "8".into(),
        }];
        let card = layer("1:1", "card", 10., 20., vec![button.clone()]);
        let summary = layer_summary(page, screen, &[&card, &button]);
        assert!(
            summary.starts_with("design:Faturas/fatura-detalhe › card › botão · 1:2 · instance\n")
        );
        assert!(summary.contains("Size: 100×40\n"));
        assert!(summary.contains("Position in the screen: x 20, y 30\n"));
        assert!(summary.contains("Fill: #FFFFFF [color/surface]\n"));
        assert!(summary.contains("Radius: 8\n"));
        assert!(summary.contains("Instance of: Button/Primary."));
        assert!(summary.ends_with("Values without token (warnings): radius 8"));

        let card_summary = layer_summary(page, screen, &[&card]);
        assert!(card_summary.ends_with(
            "Children (1), readable with design_node by appending their name to this path:\n\
             - botão · instance · 100×40"
        ));
    }

    #[test]
    fn check_rows_list_expected_values_with_tokens() {
        let manifest = manifest();
        let page = &manifest.pages[0];
        let screen = &page.screens[0];
        let mut card = layer("1:1", "card", 10., 20., Vec::new());
        card.fill = Some(Tokened {
            value: render::Color {
                r: 1.,
                g: 1.,
                b: 1.,
                a: 1.,
            },
            token: Some("color/surface".into()),
        });
        card.radius = Some(Tokened {
            value: "12".into(),
            token: Some("radius/lg".into()),
        });
        card.auto_layout = Some(render::AutoLayout {
            horizontal: false,
            gap: Tokened {
                value: 8.,
                token: Some("space/2".into()),
            },
            primary_align: None,
            counter_align: None,
            padding: Tokened {
                value: [16., 16., 16., 16.],
                token: Some("space/4".into()),
            },
        });
        card.text = Some(render::TextStyle {
            characters: "Total".into(),
            family: Some(Tokened {
                value: "IBM Plex Mono".into(),
                token: Some("font/mono".into()),
            }),
            size: Some(Tokened {
                value: 13.,
                token: None,
            }),
            weight: 500,
            color: None,
        });
        assert_eq!(
            check_rows(&card),
            vec![
                ("Size", "100×40".to_string()),
                ("Fill", "#FFFFFF [color/surface]".to_string()),
                ("Radius", "12 [radius/lg]".to_string()),
                (
                    "Padding (top right bottom left)",
                    "16 16 16 16 [space/4]".to_string()
                ),
                ("Gap", "8 [space/2]".to_string()),
                ("Font family", "IBM Plex Mono [font/mono]".to_string()),
                ("Font size", "13".to_string()),
                ("Font weight", "500".to_string()),
            ]
        );
        let text = check_text(page, screen, &[&card]);
        assert!(text.starts_with("design:Faturas/fatura-detalhe › card · 1:1 · frame\n"));
        assert!(text.contains("\n- Radius: 12 [radius/lg] → "));
    }

    #[test]
    fn check_targets_deserialize_in_snake_case() {
        let target =
            |name: &str| serde_json::from_value::<CheckTarget>(serde_json::json!(name)).ok();
        assert_eq!(target("simulator"), Some(CheckTarget::Simulator));
        assert_eq!(target("android"), Some(CheckTarget::Android));
        assert_eq!(target("web_preview"), Some(CheckTarget::WebPreview));
        assert_eq!(target("browser"), None);
    }
}
