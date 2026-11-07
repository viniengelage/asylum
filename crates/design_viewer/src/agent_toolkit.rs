//! The agent's Design tools. They read the viewer's cache and only run the OpenPencil CLI for
//! what isn't cached yet. Node ids always come from the manifest: the ids `openpencil tree --page`
//! prints are local to the page and `--node` rejects them.

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
    DESIGN_MENTION_PREFIX, DesignSource, LogMention, ToolAccess, Toolkit, ToolkitOutput,
    ToolkitTool,
};
use workspace::{AppState, OpenOptions, Workspace};

use crate::{
    DesignView, DesignViewerError, Manifest, PageEntry, Screen, cache_dir_for, file_stem, is_fig,
    known_manifests, manifest_for, remember_manifest, run_openpencil, screen_reference,
    view::split_page_name,
};

const DESCRIBE_SCRIPT: &str = include_str!("../assets/describe.js");
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
    /// The screen: `Page/Screen` as design_index lists it, a screen name, or its id.
    screen: String,
}

/// Mention ids carry the screen's global id and the .fig it belongs to.
pub(crate) fn mention_id(screen_id: &str, fig_path: &Path) -> String {
    format!(
        "{DESIGN_MENTION_PREFIX}{screen_id}|{}",
        fig_path.to_string_lossy()
    )
}

fn parse_mention_id(id: &str) -> Option<(&str, PathBuf)> {
    let (screen_id, path) = id.strip_prefix(DESIGN_MENTION_PREFIX)?.split_once('|')?;
    Some((screen_id, PathBuf::from(path)))
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
    let roots: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
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

fn screen_cache_path(
    fig_path: &Path,
    page: &PageEntry,
    screen: &Screen,
    extension: &str,
) -> PathBuf {
    cache_dir_for(fig_path)
        .join(SCREENS_DIR_NAME)
        .join(&page.hash)
        .join(format!("{}.{extension}", file_stem(&screen.id)))
}

async fn exported(
    fs: &dyn Fs,
    fig_path: &Path,
    page: &PageEntry,
    screen: &Screen,
    format: &str,
    extra_args: &[&str],
) -> Result<PathBuf, DesignViewerError> {
    let path = screen_cache_path(fig_path, page, screen, format);
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
        OsStr::new(&screen.id),
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
    let path = screen_cache_path(fig_path, page, screen, "txt");
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
    let jsx_path = exported(fs, fig_path, page, screen, "jsx", &[]).await?;
    let jsx = fs.load(&jsx_path).await?;
    Ok(format!(
        "{} · {} · {}\n\nStructure:\n{structure}\n\nJSX:\n{}",
        screen_reference(page, screen),
        screen.id,
        size(screen.bounds.width, screen.bounds.height),
        truncate_jsx(&jsx)
    ))
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
    find_screen(manifest, query).with_context(|| {
        t!(
            "design_viewer.agent.screen_not_found",
            screen = query,
            file = fig_path.display()
        )
        .to_string()
    })
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
            let (page, screen) = screen_or_error(&manifest, &fig_path, &input.screen)?;
            let png_path =
                exported(fs.as_ref(), &fig_path, page, screen, "png", &["-s", "2"]).await?;
            let png = fs.load_bytes(&png_path).await?;
            let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
            let screenshot = task_agents::scale_screenshot(image)?;
            let mut output = ToolkitOutput::text(format!(
                "{} · {} · {} (PNG {}×{})",
                screen_reference(page, screen),
                screen.id,
                size(screen.bounds.width, screen.bounds.height),
                screenshot.width,
                screenshot.height
            ));
            output.push_png(&screenshot.png);
            Ok(output)
        })
        .await
    })
}

fn list_mentions(project: &Entity<Project>, cx: &App) -> Vec<LogMention> {
    let roots: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
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
    let Some((screen_id, fig_path)) = parse_mention_id(id) else {
        return Task::ready(Err(anyhow!("invalid design mention: {id}")));
    };
    let screen_id = screen_id.to_string();
    let fs = project.read(cx).fs().clone();
    cx.background_spawn(async move {
        let manifest = manifest_for(fs.as_ref(), &fig_path).await?;
        let (page, screen) = screen_or_error(&manifest, &fig_path, &screen_id)?;
        screen_text(fs.as_ref(), &fig_path, page, screen).await
    })
}

fn open_mention(
    project: &Entity<Project>,
    id: &str,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    let (screen_id, fig_path) =
        parse_mention_id(id).with_context(|| format!("invalid design mention: {id}"))?;
    let screen_id = screen_id.to_string();
    let workspace = workspace_for(project, cx)
        .with_context(|| t!("design_viewer.agent.project_window_closed").to_string())?;
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(fig_path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            if let Some(view) = item.downcast::<DesignView>() {
                view.update(cx, |view, cx| view.focus_screen(screen_id, cx));
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
                     take. Without `file`, uses the .fig open in the Design tab, else the \
                     project's only .fig.",
                    ToolAccess::Read,
                    design_index,
                ),
                ToolkitTool::new(
                    "design_screen",
                    t!("design_viewer.agent.tool.screen"),
                    "Reads one screen of a .fig design: a structural summary (layers with type, \
                     name, size, auto layout and text) and its JSX. Use it to implement or \
                     compare a screen with code. `screen` takes `Page/Screen` from \
                     design_index, a screen name or its id.",
                    ToolAccess::Read,
                    design_screen,
                ),
                ToolkitTool::new(
                    "design_render",
                    t!("design_viewer.agent.tool.render"),
                    "Renders one screen of a .fig design to an image, to see how it looks. \
                     `screen` takes `Page/Screen` from design_index, a screen name or its id.",
                    ToolAccess::Read,
                    design_render,
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
        assert_eq!(parse_mention_id(&id), Some(("0:10", path.to_path_buf())));
        assert_eq!(parse_mention_id("container:abc"), None);
    }
}
