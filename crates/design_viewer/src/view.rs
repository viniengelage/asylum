use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    App, Bounds, ClipboardItem, Context, DispatchPhase, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels, Point, Render, SharedString, Subscription,
    Task, WeakEntity, Window, canvas, div, px,
};
use i18n::t;
use settings::{DesignCanvasBackground, Settings as _, SettingsStore};
use ui::{
    Button, ButtonSize, Callout, Color, CommonAnimationExt as _, EmptyState, EmptyStateTone, Icon,
    IconButton, IconName, IconSize, KeyBinding, Label, LabelSize, ListItem, ListItemSpacing,
    Tooltip, prelude::*,
};
use web_preview::{WebPreviewEvent, WebPreviewView};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

use crate::agent_toolkit::{layer_mention_id, mention_id, node_jsx};
use crate::render::{self, LayerKind, UnboundProperty};
use crate::{
    DesignDocument, DesignDocumentEvent, DesignDocumentState, DesignViewerSettings, ExitFocus,
    FigFile, FitToScreen, FocusSearch, Layer, MentionLayer, NextScreen, PageEntry, PageId,
    PreviousScreen, Screen, ScreenLayers, ZoomIn, ZoomOut, layer_mention_title, layer_reference,
    screen_reference,
};

const NAV_WIDTH: Pixels = px(252.);
const NAV_DISCLOSURE_INSET: Pixels = px(16.);
const INSPECTOR_WIDTH: Pixels = px(300.);
const DESIGN_SYSTEM_PREFIX: &str = "00";
const HOVER_OUTLINE: u32 = 0x60A5FA;
const SELECTION_OUTLINE: u32 = 0x8B5CF6;
const DISTANCE_COLOR: u32 = 0xFB7185;
const TOKEN_COLOR: u32 = 0xC4B5FD;
/// A press that moves further than this is a pan of the canvas, not a click on a layer.
const CLICK_SLOP: f32 = 3.;
const NO_VALUE: &str = "—";

/// The page's pan and zoom as the viewer script reports them: a page point `p` is drawn at
/// `(x, y) + p * zoom` from the canvas origin.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Viewport {
    x: f32,
    y: f32,
    zoom: f32,
}

impl Viewport {
    fn project(&self, bounds: &crate::Bounds) -> (Pixels, Pixels, Pixels, Pixels) {
        (
            px(self.x + bounds.x as f32 * self.zoom),
            px(self.y + bounds.y as f32 * self.zoom),
            px(bounds.width as f32 * self.zoom),
            px(bounds.height as f32 * self.zoom),
        )
    }

    fn point(&self, x: f64, y: f64) -> (Pixels, Pixels) {
        (
            px(self.x + x as f32 * self.zoom),
            px(self.y + y as f32 * self.zoom),
        )
    }
}

struct NavScope<'a> {
    page_id: &'a PageId,
    screen_id: &'a str,
    query: Option<&'a str>,
}

pub struct DesignView {
    document: Entity<DesignDocument>,
    workspace: Option<WeakEntity<Workspace>>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    search_editor: Entity<Editor>,
    browser: Entity<WebPreviewView>,
    expanded_pages: HashSet<PageId>,
    current_page: Option<PageId>,
    focused_screen: Option<String>,
    /// A screen asked for (e.g. from an agent mention) before the manifest was read.
    pending_screen: Option<String>,
    /// A layer of the pending or focused screen to select once its layers are read.
    pending_layer: Option<String>,
    /// The layer whose JSX is being exported for the clipboard.
    copying_jsx: Option<Task<()>>,
    zoom_percent: Option<f32>,
    viewport: Option<Viewport>,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    canvas_background: DesignCanvasBackground,
    layers: HashMap<PageId, Arc<Vec<ScreenLayers>>>,
    layer_loads: HashMap<PageId, Task<()>>,
    /// Screen and layer ids open in the navigator.
    expanded_layers: HashSet<String>,
    selected_layer: Option<String>,
    hovered_layer: Option<String>,
    option_held: bool,
    pointer_down: Option<Point<Pixels>>,
    _subscriptions: Vec<Subscription>,
}

impl DesignView {
    pub fn new(
        path: PathBuf,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let document = cx.new(|cx| DesignDocument::new(path, fs.clone(), cx));
        let browser = cx.new(|cx| WebPreviewView::embedded("about:blank".to_string(), window, cx));
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&t!("design_viewer.nav.search_placeholder"), window, cx);
            editor
        });

        let subscriptions = vec![
            cx.subscribe(&document, Self::handle_document_event),
            cx.subscribe(
                &browser,
                |this, _, event: &WebPreviewEvent, cx| match event {
                    WebPreviewEvent::UrlChanged(url) => this.sync_from_url(url, cx),
                },
            ),
            cx.subscribe(&search_editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.load_layers_for_search(cx);
                    cx.notify();
                }
            }),
            // Theme changes arrive here too and change the theme color behind the screens.
            cx.observe_global::<SettingsStore>(|this, cx| {
                this.canvas_background = DesignViewerSettings::get_global(cx).canvas_background;
                this.apply_canvas_background(cx);
                cx.notify();
            }),
        ];

        let mut this = Self {
            document,
            workspace: None,
            fs,
            focus_handle: cx.focus_handle(),
            search_editor,
            browser,
            expanded_pages: HashSet::default(),
            current_page: None,
            focused_screen: None,
            pending_screen: None,
            pending_layer: None,
            copying_jsx: None,
            zoom_percent: None,
            viewport: None,
            canvas_bounds: Rc::new(Cell::new(None)),
            canvas_background: DesignViewerSettings::get_global(cx).canvas_background,
            layers: HashMap::default(),
            layer_loads: HashMap::default(),
            expanded_layers: HashSet::default(),
            selected_layer: None,
            hovered_layer: None,
            option_held: false,
            pointer_down: None,
            _subscriptions: subscriptions,
        };
        this.show_initial_page(cx);
        this
    }

    pub fn path(&self, cx: &App) -> PathBuf {
        self.document.read(cx).path().to_path_buf()
    }

    fn file_name(&self, cx: &App) -> SharedString {
        file_name(self.document.read(cx).path())
    }

    fn handle_document_event(
        &mut self,
        _: Entity<DesignDocument>,
        event: &DesignDocumentEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            DesignDocumentEvent::StateChanged => {
                self.forget_current_page_if_gone(cx);
                self.show_pending_screen(cx);
                self.show_initial_page(cx);
            }
            DesignDocumentEvent::PagesChanged(pages) => {
                for page in pages {
                    self.layers.remove(page);
                    self.layer_loads.remove(page);
                }
                if let Some(current) = self.current_page.clone()
                    && pages.contains(&current)
                {
                    self.ensure_layers(&current, cx);
                }
                self.load_layers_for_search(cx);
                self.forget_current_page_if_gone(cx);
                if self.current_page.is_none() {
                    self.show_initial_page(cx);
                } else if self
                    .current_page
                    .as_ref()
                    .is_some_and(|page| pages.contains(page))
                {
                    // The hash carries the zoom and scroll, so a reload keeps the view in place.
                    self.browser.update(cx, |browser, cx| browser.reload(cx));
                }
            }
        }
        cx.notify();
    }

    fn forget_current_page_if_gone(&mut self, cx: &App) {
        let current_page_gone = self.current_page.as_ref().is_some_and(|page| {
            self.document
                .read(cx)
                .manifest()
                .is_none_or(|manifest| manifest.page(page).is_none())
        });
        if current_page_gone {
            self.current_page = None;
            self.set_focused_screen(None);
        }
    }

    /// A layer belongs to one screen, so a new focus starts with nothing selected.
    fn set_focused_screen(&mut self, screen: Option<String>) {
        if self.focused_screen != screen {
            self.selected_layer = None;
            self.hovered_layer = None;
        }
        self.focused_screen = screen;
    }

    fn ensure_layers(&mut self, page_id: &PageId, cx: &mut Context<Self>) {
        if self.layers.contains_key(page_id) || self.layer_loads.contains_key(page_id) {
            return;
        }
        let load = self.document.read(cx).load_layers(page_id, cx);
        let page = page_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.layer_loads.remove(&page);
                match result {
                    Ok(layers) => {
                        this.layers.insert(page, layers);
                        this.show_pending_layer();
                        cx.notify();
                    }
                    Err(error) => {
                        log::warn!(
                            "design viewer: couldn't read the layers of page {page}: {error:#}"
                        )
                    }
                }
            })
            .ok();
        });
        self.layer_loads.insert(page_id.clone(), task);
    }

    fn search_query(&self, cx: &App) -> String {
        self.search_editor.read(cx).text(cx).trim().to_lowercase()
    }

    fn load_layers_for_search(&mut self, cx: &mut Context<Self>) {
        if self.search_query(cx).is_empty() {
            return;
        }
        let pages: Vec<PageId> = self
            .document
            .read(cx)
            .manifest()
            .map(|manifest| manifest.pages.iter().map(|page| page.id.clone()).collect())
            .unwrap_or_default();
        for page in &pages {
            self.ensure_layers(page, cx);
        }
    }

    fn screen_layers(&self, page_id: &PageId, screen_id: &str) -> Option<&[Layer]> {
        self.layers
            .get(page_id)?
            .iter()
            .find(|screen| screen.screen_id == screen_id)
            .map(|screen| screen.layers.as_slice())
    }

    fn focused_layers(&self) -> Option<&[Layer]> {
        self.screen_layers(self.current_page.as_ref()?, self.focused_screen.as_ref()?)
    }

    /// The selected layer and its ancestors under the screen, outermost first.
    fn selected_path(&self) -> Option<Vec<&Layer>> {
        layer_path(self.focused_layers()?, self.selected_layer.as_ref()?)
    }

    fn page_point(&self, position: Point<Pixels>) -> Option<(f64, f64)> {
        let bounds = self.canvas_bounds.get()?;
        let viewport = self.viewport?;
        let x = (f32::from(position.x - bounds.origin.x) - viewport.x) / viewport.zoom;
        let y = (f32::from(position.y - bounds.origin.y) - viewport.y) / viewport.zoom;
        Some((f64::from(x), f64::from(y)))
    }

    fn hover_at(&mut self, position: Option<Point<Pixels>>, deep: bool, cx: &mut Context<Self>) {
        let hovered = position
            .and_then(|position| self.page_point(position))
            .and_then(|point| layer_at(self.focused_layers()?, point, deep))
            .map(|layer| layer.id.clone());
        if self.hovered_layer != hovered {
            self.hovered_layer = hovered;
            cx.notify();
        }
    }

    fn release_at(&mut self, position: Point<Pixels>, deep: bool, cx: &mut Context<Self>) {
        let Some(start) = self.pointer_down.take() else {
            return;
        };
        let moved =
            f32::from((position.x - start.x).abs()).max(f32::from((position.y - start.y).abs()));
        if moved >= CLICK_SLOP {
            return;
        }
        let Some(point) = self.page_point(position) else {
            return;
        };
        let Some(screen_bounds) = self
            .focused_screen_entry(cx)
            .map(|(_, _, screen)| screen.bounds)
        else {
            return;
        };
        // Outside the focused screen the viewer script focuses whatever screen was clicked.
        if !contains(&screen_bounds, point) {
            return;
        }
        let selected = self
            .focused_layers()
            .and_then(|layers| layer_at(layers, point, deep))
            .map(|layer| layer.id.clone());
        if let Some(id) = &selected {
            self.reveal_layer(id);
        }
        self.selected_layer = selected;
        cx.notify();
    }

    fn reveal_layer(&mut self, layer_id: &str) {
        let ancestors: Vec<String> = self
            .focused_layers()
            .and_then(|layers| layer_path(layers, layer_id))
            .map(|path| {
                path.iter()
                    .take(path.len().saturating_sub(1))
                    .map(|layer| layer.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(screen) = self.focused_screen.clone() {
            self.expanded_layers.insert(screen);
        }
        self.expanded_layers.extend(ancestors);
    }

    fn select_layer(
        &mut self,
        page_id: PageId,
        screen_id: String,
        layer_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.current_page.as_ref() != Some(&page_id)
            || self.focused_screen.as_ref() != Some(&screen_id)
        {
            self.show_screen(page_id.clone(), screen_id, cx);
        }
        self.ensure_layers(&page_id, cx);
        self.selected_layer = Some(layer_id);
        cx.notify();
    }

    fn select_parent(&mut self, cx: &mut Context<Self>) {
        let parent = self.selected_path().and_then(|path| {
            path.len()
                .checked_sub(2)
                .and_then(|index| path.get(index))
                .map(|layer| layer.id.clone())
        });
        self.selected_layer = parent;
        cx.notify();
    }

    fn exit_level(&mut self, cx: &mut Context<Self>) {
        if self.selected_layer.is_some() {
            self.select_parent(cx);
        } else {
            self.exit_focus(cx);
        }
    }

    fn show_initial_page(&mut self, cx: &mut Context<Self>) {
        if self.current_page.is_some() {
            return;
        }
        let Some(manifest) = self.document.read(cx).manifest() else {
            return;
        };
        let first = manifest
            .pages
            .iter()
            .find(|page| !is_design_system(page))
            .or_else(|| manifest.pages.first())
            .map(|page| page.id.clone());
        if let Some(page) = first {
            self.expanded_pages.insert(page.clone());
            self.show_page(page, None, cx);
        }
    }

    /// Focuses a screen by its id, as soon as the manifest is read.
    pub fn focus_screen(&mut self, screen_id: String, cx: &mut Context<Self>) {
        self.pending_screen = Some(screen_id);
        self.show_pending_screen(cx);
    }

    /// Focuses a screen and selects one of its layers, as soon as both are read.
    pub fn focus_layer(&mut self, screen_id: String, layer_id: String, cx: &mut Context<Self>) {
        self.pending_layer = Some(layer_id);
        self.focus_screen(screen_id, cx);
        self.show_pending_layer();
        cx.notify();
    }

    fn show_pending_layer(&mut self) {
        let Some(layer_id) = self.pending_layer.as_ref() else {
            return;
        };
        if self.pending_screen.is_some()
            || self
                .focused_layers()
                .and_then(|layers| layer_path(layers, layer_id))
                .is_none()
        {
            return;
        }
        let Some(layer_id) = self.pending_layer.take() else {
            return;
        };
        self.reveal_layer(&layer_id);
        self.selected_layer = Some(layer_id);
    }

    fn show_pending_screen(&mut self, cx: &mut Context<Self>) {
        let Some(screen_id) = self.pending_screen.as_ref() else {
            return;
        };
        let Some(page) = self
            .document
            .read(cx)
            .manifest()
            .and_then(|manifest| manifest.screen(screen_id))
            .map(|(page, _)| page.id.clone())
        else {
            return;
        };
        let Some(screen_id) = self.pending_screen.take() else {
            return;
        };
        self.show_screen(page, screen_id, cx);
    }

    fn show_page(&mut self, page: PageId, screen: Option<String>, cx: &mut Context<Self>) {
        let Some(html_path) = self.document.read(cx).page_html_path(&page) else {
            return;
        };
        let Ok(mut url) = url::Url::from_file_path(&html_path) else {
            log::error!(
                "design viewer: {} is not an absolute path",
                html_path.display()
            );
            return;
        };
        let mut fragment = url::form_urlencoded::Serializer::new(String::new());
        fragment.append_pair("bg", &self.background_css(cx));
        fragment.append_pair("accent", &css_color(cx.theme().colors().text_accent));
        if let Some(screen) = &screen {
            fragment.append_pair("screen", screen);
        }
        url.set_fragment(Some(&fragment.finish()));

        self.ensure_layers(&page, cx);
        self.current_page = Some(page);
        self.set_focused_screen(screen);
        self.zoom_percent = None;
        self.viewport = None;
        self.browser
            .update(cx, |browser, cx| browser.navigate(url.to_string(), cx));
        cx.notify();
    }

    fn show_screen(&mut self, page: PageId, screen: String, cx: &mut Context<Self>) {
        if self.current_page.as_ref() == Some(&page) {
            // Only the hash would change, which doesn't reload the page or rerun its script.
            self.run_viewer(&format!("focus({})", js_string(&screen)), cx);
            self.set_focused_screen(Some(screen));
            cx.notify();
        } else {
            self.expanded_pages.insert(page.clone());
            self.show_page(page, Some(screen), cx);
        }
    }

    fn run_viewer(&self, call: &str, cx: &App) {
        self.browser.read(cx).execute_javascript(&format!(
            "window.asylumDesign && window.asylumDesign.{call}"
        ));
    }

    fn background_css(&self, cx: &App) -> String {
        match self.canvas_background {
            DesignCanvasBackground::Theme => {
                css_color(cx.theme().colors().editor_subheader_background)
            }
            DesignCanvasBackground::Document => "document".to_string(),
        }
    }

    fn apply_canvas_background(&self, cx: &App) {
        self.run_viewer(
            &format!("setBackground({})", js_string(&self.background_css(cx))),
            cx,
        );
    }

    fn set_canvas_background(
        &mut self,
        background: DesignCanvasBackground,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.design.get_or_insert_default().canvas_background = Some(background);
        });
    }

    fn sync_from_url(&mut self, url: &str, cx: &mut Context<Self>) {
        let Some((_, fragment)) = url.split_once('#') else {
            return;
        };
        let mut screen = None;
        let mut zoom = None;
        let mut x = None;
        let mut y = None;
        for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            match key.as_ref() {
                "screen" => screen = Some(value.into_owned()),
                "zoom" => zoom = value.parse::<f32>().ok(),
                "x" => x = value.parse::<f32>().ok(),
                "y" => y = value.parse::<f32>().ok(),
                _ => {}
            }
        }
        let viewport = match (x, y, zoom) {
            (Some(x), Some(y), Some(percent)) if percent > 0. => Some(Viewport {
                x,
                y,
                zoom: percent / 100.,
            }),
            _ => None,
        };
        if self.focused_screen != screen || self.zoom_percent != zoom || self.viewport != viewport {
            self.set_focused_screen(screen);
            self.zoom_percent = zoom;
            self.viewport = viewport;
            cx.notify();
        }
    }

    fn current_page_entry<'a>(&self, cx: &'a App) -> Option<&'a PageEntry> {
        let page = self.current_page.as_ref()?;
        self.document.read(cx).manifest()?.page(page)
    }

    fn focused_screen_entry<'a>(&self, cx: &'a App) -> Option<(&'a PageEntry, usize, &'a Screen)> {
        let page = self.current_page_entry(cx)?;
        let focused = self.focused_screen.as_ref()?;
        let (index, screen) = page
            .screens
            .iter()
            .enumerate()
            .find(|(_, screen)| &screen.id == focused)?;
        Some((page, index, screen))
    }

    fn copy_reference(&mut self, cx: &mut Context<Self>) {
        let Some((page, _, screen)) = self.focused_screen_entry(cx) else {
            return;
        };
        let reference = match self.selected_path() {
            Some(path) => {
                let names: Vec<&str> = path.iter().map(|layer| layer.name.as_str()).collect();
                format!("@{}", layer_reference(page, screen, &names))
            }
            None => format!("@{}", screen_reference(page, screen)),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(reference));
        self.show_toast(t!("design_viewer.focus.reference_copied").to_string(), cx);
    }

    fn mention_in_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let screen = self
            .focused_screen_entry(cx)
            .map(|(page, _, screen)| (page, screen))
            .or_else(|| {
                let page = self.current_page_entry(cx)?;
                Some((page, page.screens.first()?))
            });
        let Some((page, screen)) = screen else {
            return;
        };
        let (id, title) = match self.selected_path().and_then(|path| path.last().copied()) {
            Some(layer) if self.focused_screen.as_ref() == Some(&screen.id) => (
                layer_mention_id(&screen.id, &layer.id, &self.path(cx)),
                layer_mention_title(screen, layer),
            ),
            _ => (
                mention_id(&screen.id, &self.path(cx)),
                screen_reference(page, screen),
            ),
        };
        let project = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
            .map(|workspace| workspace.read(cx).project().clone());
        if let Some(project) = project
            && !task_agents::toolkit_enabled(task_agents::toolkit_ids::DESIGN, &project, cx)
        {
            self.show_toast(task_agents::toolkit_disabled_message("Design"), cx);
            return;
        }
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id,
                title,
                prompt: None,
                submit: false,
            }),
            cx,
        );
    }

    fn copy_jsx(&mut self, cx: &mut Context<Self>) {
        if self.copying_jsx.is_some() {
            return;
        }
        let Some((page, _, _)) = self.focused_screen_entry(cx) else {
            return;
        };
        let page = page.clone();
        let Some(layer_id) = self
            .selected_path()
            .and_then(|path| path.last().map(|layer| layer.id.clone()))
        else {
            return;
        };
        let export = cx.background_spawn(node_jsx(self.fs.clone(), self.path(cx), page, layer_id));
        let task = cx.spawn(async move |this, cx| {
            let result = export.await;
            // The view may have closed while the CLI ran; there is nothing left to tell.
            this.update(cx, |this, cx| {
                this.copying_jsx = None;
                match result {
                    Ok(jsx) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(jsx));
                        this.show_toast(t!("design_viewer.inspector.jsx_copied").to_string(), cx);
                    }
                    Err(error) => this.show_toast(
                        t!(
                            "design_viewer.error.copy_jsx_failed",
                            detail = format!("{error:#}")
                        )
                        .to_string(),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        });
        self.copying_jsx = Some(task);
        cx.notify();
    }

    fn open_in_openpencil(&mut self, cx: &mut Context<Self>) {
        let path = self.path(cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = launch_openpencil(&path).await {
                this.update(cx, |this, cx| {
                    this.show_toast(
                        t!(
                            "design_viewer.error.open_in_openpencil_failed",
                            detail = format!("{error:#}")
                        )
                        .to_string(),
                        cx,
                    );
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_toast(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                workspace::Toast::new(NotificationId::unique::<Self>(), message).autohide(),
                cx,
            );
        });
    }

    fn step_screen(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(page) = self.current_page_entry(cx) else {
            return;
        };
        let current = self
            .focused_screen
            .as_ref()
            .and_then(|focused| page.screens.iter().position(|screen| &screen.id == focused));
        let next = match current {
            Some(index) => index.checked_add_signed(delta),
            None if delta > 0 => Some(0),
            None => page.screens.len().checked_sub(1),
        };
        let Some(screen) = next.and_then(|index| page.screens.get(index)) else {
            return;
        };
        let page_id = page.id.clone();
        let screen_id = screen.id.clone();
        self.show_screen(page_id, screen_id, cx);
    }

    fn exit_focus(&mut self, cx: &mut Context<Self>) {
        self.run_viewer("exit()", cx);
        self.set_focused_screen(None);
        cx.notify();
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        let factor = 1.0 + event.delta;
        match self.canvas_bounds.get() {
            Some(bounds) => {
                let x = f32::from(event.position.x - bounds.origin.x);
                let y = f32::from(event.position.y - bounds.origin.y);
                self.run_viewer(&format!("zoomBy({factor}, {x}, {y})"), cx);
            }
            None => self.run_viewer(&format!("zoomBy({factor})"), cx),
        }
    }

    fn render_status_chip(&self, cx: &App) -> AnyElement {
        match self.document.read(cx).state() {
            DesignDocumentState::Loading { .. } => ui::status_chip_spinning(
                IconName::LoadCircle,
                t!("design_viewer.status.updating"),
                Color::Accent,
                cx,
            )
            .into_any_element(),
            DesignDocumentState::Failed(error) => div()
                .id("design-status-error")
                .tooltip(Tooltip::text(error.message()))
                .child(ui::status_chip(
                    Some(IconName::Warning),
                    t!("design_viewer.status.failed"),
                    Color::Error,
                    cx,
                ))
                .into_any_element(),
            DesignDocumentState::Ready => ui::status_chip(
                Some(IconName::Check),
                t!("design_viewer.status.updated"),
                Color::Success,
                cx,
            )
            .into_any_element(),
        }
    }

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border_variant;
        let file_name = self.file_name(cx);
        let search_focused = self
            .search_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        let query = self.search_query(cx);

        let mut design_system = Vec::new();
        let mut screens = Vec::new();
        let mut any_match = false;
        if let Some(manifest) = self.document.read(cx).manifest().cloned() {
            for page in &manifest.pages {
                let matching: Vec<(&Screen, bool)> = page
                    .screens
                    .iter()
                    .filter_map(|screen| {
                        if query.is_empty() {
                            return Some((screen, false));
                        }
                        let layers_match =
                            self.screen_layers(&page.id, &screen.id)
                                .is_some_and(|layers| {
                                    layers.iter().any(|layer| layer_matches(layer, &query))
                                });
                        (layers_match || screen.name.to_lowercase().contains(&query))
                            .then_some((screen, layers_match))
                    })
                    .collect();
                if !query.is_empty() && matching.is_empty() {
                    continue;
                }
                any_match = true;
                let rows = self.render_page_rows(page, &matching, &query, cx);
                if is_design_system(page) {
                    design_system.push(rows);
                } else {
                    screens.push(rows);
                }
            }
        }

        v_flex()
            .w(NAV_WIDTH)
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(ui::initial_tile(&file_name, cx))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(file_name.clone())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                    )
                    .child(self.render_status_chip(cx)),
            )
            .child(
                div().px_2().pb_2().child(
                    ui::text_field(search_focused, cx)
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::MagnifyingGlass)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().min_w_0().child(self.search_editor.clone()))
                        .when(!search_focused && query.is_empty(), |this| {
                            this.child(KeyBinding::for_action_in(
                                &FocusSearch,
                                &self.focus_handle,
                                cx,
                            ))
                        }),
                ),
            )
            .child(
                v_flex()
                    .id("design-nav-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    .gap_3()
                    .when(!design_system.is_empty(), |this| {
                        this.child(grouped_section(
                            t!(
                                "design_viewer.nav.section_design_system",
                                count = design_system.len()
                            ),
                            design_system,
                            cx,
                        ))
                    })
                    .when(!screens.is_empty(), |this| {
                        this.child(grouped_section(
                            t!("design_viewer.nav.section_screens", count = screens.len()),
                            screens,
                            cx,
                        ))
                    })
                    .when(!query.is_empty() && !any_match, |this| {
                        this.child(
                            div().px_1().child(
                                Label::new(t!("design_viewer.nav.no_matches"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    }),
            )
    }

    fn render_page_rows(
        &self,
        page: &PageEntry,
        screens: &[(&Screen, bool)],
        query: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filtering = !query.is_empty();
        let expanded = filtering || self.expanded_pages.contains(&page.id);
        let (number, title) = split_page_name(&page.name);
        let page_id = page.id.clone();
        let is_current = self.current_page.as_ref() == Some(&page.id);

        // The disclosure is drawn one rem left of the item, so the row is indented by that much
        // to keep it inside the grouped block.
        let page_row = ListItem::new(SharedString::from(format!("page-{}", page.id)))
            .indent_level(1)
            .indent_step_size(NAV_DISCLOSURE_INSET)
            .spacing(ListItemSpacing::Sparse)
            .toggle(expanded)
            .toggle_state(is_current && self.focused_screen.is_none())
            .on_toggle(cx.listener({
                let page_id = page_id.clone();
                move |this, _, _, cx| {
                    if !this.expanded_pages.remove(&page_id) {
                        this.expanded_pages.insert(page_id.clone());
                    }
                    cx.notify();
                }
            }))
            .on_click(cx.listener({
                move |this, _, _, cx| {
                    this.expanded_pages.insert(page_id.clone());
                    if this.current_page.as_ref() == Some(&page_id) {
                        this.exit_focus(cx);
                    } else {
                        this.show_page(page_id.clone(), None, cx);
                    }
                }
            }))
            .end_slot::<Label>(number.map(|number| {
                Label::new(SharedString::from(number.to_string()))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .child(
                Label::new(SharedString::from(title.to_string()))
                    .size(LabelSize::Small)
                    .truncate(),
            );

        v_flex()
            .child(page_row)
            .when(expanded, |this| {
                if screens.is_empty() {
                    this.child(
                        ListItem::new(SharedString::from(format!("page-{}-empty", page.id)))
                            .indent_level(2)
                            .indent_step_size(NAV_DISCLOSURE_INSET)
                            .selectable(false)
                            .child(
                                Label::new(t!("design_viewer.nav.no_screens"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                } else {
                    let mut rows = Vec::new();
                    for (screen, layers_match) in screens {
                        let filtered = filtering && *layers_match;
                        let expanded = filtered || self.expanded_layers.contains(&screen.id);
                        rows.push(self.render_screen_row(page, screen, is_current, expanded, cx));
                        if expanded && let Some(layers) = self.screen_layers(&page.id, &screen.id) {
                            let scope = NavScope {
                                page_id: &page.id,
                                screen_id: &screen.id,
                                query: filtered.then_some(query),
                            };
                            self.push_layer_rows(&mut rows, &scope, layers, 0, cx);
                        }
                    }
                    this.children(rows)
                }
            })
            .into_any_element()
    }

    fn render_screen_row(
        &self,
        page: &PageEntry,
        screen: &Screen,
        is_current: bool,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = is_current
            && self.focused_screen.as_ref() == Some(&screen.id)
            && self.selected_layer.is_none();
        let page_id = page.id.clone();
        let screen_id = screen.id.clone();
        ListItem::new(SharedString::from(format!("screen-{}", screen.id)))
            .indent_level(2)
            .indent_step_size(NAV_DISCLOSURE_INSET)
            .toggle_state(selected)
            .when(screen.node_count > 1, |item| {
                item.toggle(expanded).on_toggle(cx.listener({
                    let page_id = page_id.clone();
                    let screen_id = screen_id.clone();
                    move |this, _, _, cx| {
                        if !this.expanded_layers.remove(&screen_id) {
                            this.expanded_layers.insert(screen_id.clone());
                        }
                        this.ensure_layers(&page_id, cx);
                        cx.notify();
                    }
                }))
            })
            .start_slot(
                Icon::new(IconName::Smartphone)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.show_screen(page_id.clone(), screen_id.clone(), cx);
                this.selected_layer = None;
            }))
            .child(
                Label::new(SharedString::from(screen.name.clone()))
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .into_any_element()
    }

    fn push_layer_rows(
        &self,
        rows: &mut Vec<AnyElement>,
        scope: &NavScope,
        layers: &[Layer],
        depth: usize,
        cx: &mut Context<Self>,
    ) {
        for layer in layers {
            if scope
                .query
                .is_some_and(|query| !layer_matches(layer, query))
            {
                continue;
            }
            let has_children = !layer.children.is_empty();
            let expanded =
                has_children && (scope.query.is_some() || self.expanded_layers.contains(&layer.id));
            let selected = self.selected_layer.as_ref() == Some(&layer.id)
                && self.focused_screen.as_deref() == Some(scope.screen_id);
            let (label_color, icon_color) = if layer.visible {
                (Color::Default, Color::Muted)
            } else {
                (Color::Disabled, Color::Disabled)
            };
            let row_id = format!("layer-{}-{}", scope.screen_id, layer.id);
            let page_id = scope.page_id.clone();
            let screen_id = scope.screen_id.to_string();
            let layer_id = layer.id.clone();
            let item = ListItem::new(SharedString::from(row_id.clone()))
                .indent_level(3 + depth)
                .indent_step_size(NAV_DISCLOSURE_INSET)
                .toggle_state(selected)
                .when(has_children, |item| {
                    item.toggle(expanded).on_toggle(cx.listener({
                        let layer_id = layer_id.clone();
                        move |this, _, _, cx| {
                            if !this.expanded_layers.remove(&layer_id) {
                                this.expanded_layers.insert(layer_id.clone());
                            }
                            cx.notify();
                        }
                    }))
                })
                .start_slot(
                    Icon::new(layer_icon(layer.kind))
                        .size(IconSize::Small)
                        .color(icon_color),
                )
                .when(!layer.visible, |item| {
                    item.end_slot(hidden_marker(format!("{row_id}-hidden")))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_layer(page_id.clone(), screen_id.clone(), layer_id.clone(), cx);
                }))
                .child(
                    Label::new(SharedString::from(layer.name.clone()))
                        .size(LabelSize::Small)
                        .color(label_color)
                        .truncate(),
                );
            rows.push(item.into_any_element());
            if expanded {
                self.push_layer_rows(rows, scope, &layer.children, depth + 1, cx);
            }
        }
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.current_page_entry(cx);
        let focused = self
            .focused_screen_entry(cx)
            .map(|(_, _, screen)| screen.clone());
        let page_title = page.map(|page| split_page_name(&page.name).1.to_string());
        let layer = self
            .selected_path()
            .and_then(|path| path.last().map(|layer| (layer.name.clone(), layer.bounds)));
        let size = match (&layer, &focused, page) {
            (Some((_, bounds)), _, _) => Some((bounds.width, bounds.height)),
            (None, Some(screen), _) => Some((screen.bounds.width, screen.bounds.height)),
            (None, None, Some(page)) => Some((page.width, page.height)),
            (None, None, None) => None,
        };
        let screen_color = if layer.is_some() {
            Color::Muted
        } else {
            Color::Default
        };
        let zoom = self
            .zoom_percent
            .map(|percent| {
                t!(
                    "design_viewer.toolbar.zoom_value",
                    percent = percent.round()
                )
            })
            .unwrap_or_else(|| SharedString::from("–"));
        let background = self.canvas_background;

        ui::toolbar_rail(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_x_hidden()
                    .children(page_title.map(|title| {
                        Label::new(SharedString::from(title))
                            .size(LabelSize::Small)
                            .color(if focused.is_some() {
                                Color::Muted
                            } else {
                                Color::Default
                            })
                            .truncate()
                    }))
                    .children(focused.map(|screen| {
                        h_flex()
                            .gap_1()
                            .child(Label::new("/").size(LabelSize::Small).color(Color::Muted))
                            .child(
                                Label::new(screen.name)
                                    .size(LabelSize::Small)
                                    .color(screen_color)
                                    .truncate(),
                            )
                    }))
                    .children(layer.map(|(name, _)| {
                        h_flex()
                            .gap_1()
                            .child(Label::new("/").size(LabelSize::Small).color(Color::Muted))
                            .child(Label::new(name).size(LabelSize::Small).truncate())
                    }))
                    .children(size.map(|(width, height)| {
                        Label::new(format_size(width, height))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    })),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(
                        ui::segmented_control(cx).p(px(2.)).children(
                            [
                                (
                                    "design-canvas-theme",
                                    t!("design_viewer.toolbar.canvas_theme"),
                                    DesignCanvasBackground::Theme,
                                ),
                                (
                                    "design-canvas-document",
                                    t!("design_viewer.toolbar.canvas_document"),
                                    DesignCanvasBackground::Document,
                                ),
                            ]
                            .into_iter()
                            .map(|(id, label, value)| {
                                ui::segment_option(id, label, background == value, true, cx)
                                    .h(px(22.))
                                    .px_2()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_canvas_background(value, cx);
                                    }))
                            }),
                        ),
                    )
                    .child(ui::button_row(
                        [
                            h_flex()
                                .gap_1()
                                .child(
                                    ui::group_icon_button("design-zoom-out", IconName::Dash, 24.)
                                        .tooltip(Tooltip::for_action_title_in(
                                            t!("design_viewer.toolbar.zoom_out"),
                                            &ZoomOut,
                                            &self.focus_handle,
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.run_viewer("zoomOut()", cx);
                                        })),
                                )
                                .child(div().min_w(px(40.)).flex().justify_center().child(
                                    Label::new(zoom).size(LabelSize::XSmall).color(Color::Muted),
                                ))
                                .child(
                                    ui::group_icon_button("design-zoom-in", IconName::Plus, 24.)
                                        .tooltip(Tooltip::for_action_title_in(
                                            t!("design_viewer.toolbar.zoom_in"),
                                            &ZoomIn,
                                            &self.focus_handle,
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.run_viewer("zoomIn()", cx);
                                        })),
                                )
                                .into_any_element(),
                            Button::new("design-fit", t!("design_viewer.toolbar.fit"))
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .key_binding(KeyBinding::for_action_in(
                                    &FitToScreen,
                                    &self.focus_handle,
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_viewer("fit()", cx);
                                }))
                                .into_any_element(),
                            ui::group_icon_button("design-mention", IconName::Sparkle, 24.)
                                .tooltip(Tooltip::text(t!("design_viewer.toolbar.mention_agent")))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.mention_in_agent(window, cx);
                                }))
                                .into_any_element(),
                            ui::group_icon_button(
                                "design-open-openpencil",
                                IconName::ArrowUpRight,
                                24.,
                            )
                            .tooltip(Tooltip::text(t!(
                                "design_viewer.toolbar.open_in_openpencil"
                            )))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_in_openpencil(cx);
                            }))
                            .into_any_element(),
                        ],
                        cx,
                    )),
            )
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.focused_screen_entry(cx)?;
        let body = match self.selected_path() {
            Some(path) => self.render_layer_inspector(&path, cx).into_any_element(),
            None => self.render_screen_inspector(cx)?.into_any_element(),
        };
        Some(
            v_flex()
                .id("design-inspector")
                .w(INSPECTOR_WIDTH)
                .flex_none()
                .h_full()
                .p_3()
                .gap_3()
                .border_l_1()
                .border_color(cx.theme().colors().border_variant)
                .overflow_y_scroll()
                .child(body),
        )
    }

    fn render_screen_inspector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (page, index, screen) = self.focused_screen_entry(cx)?;
        let total = page.screens.len();
        let name = SharedString::from(screen.name.clone());
        let details = format!(
            "{} · {}",
            format_size(screen.bounds.width, screen.bounds.height),
            t!("design_viewer.focus.nodes", count = screen.node_count)
        );

        Some(
            ui::card(cx)
                .p_3()
                .gap_2()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(name)
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            ),
                        )
                        .child(
                            IconButton::new("design-previous-screen", IconName::ChevronLeft)
                                .icon_size(IconSize::Small)
                                .disabled(index == 0)
                                .tooltip(Tooltip::for_action_title_in(
                                    t!("design_viewer.focus.previous"),
                                    &PreviousScreen,
                                    &self.focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.step_screen(-1, cx))),
                        )
                        .child(
                            Label::new(t!(
                                "design_viewer.focus.position",
                                index = index + 1,
                                total = total
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            IconButton::new("design-next-screen", IconName::ChevronRight)
                                .icon_size(IconSize::Small)
                                .disabled(index + 1 >= total)
                                .tooltip(Tooltip::for_action_title_in(
                                    t!("design_viewer.focus.next"),
                                    &NextScreen,
                                    &self.focus_handle,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.step_screen(1, cx))),
                        ),
                )
                .child(
                    Label::new(details)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .gap_1p5()
                        .child(
                            ui::primary_button(
                                "design-focus-mention",
                                Some(IconName::Sparkle),
                                t!("design_viewer.focus.mention"),
                                None,
                                cx,
                            )
                            .justify_center()
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.mention_in_agent(window, cx);
                                },
                            )),
                        )
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1p5()
                                .child(self.copy_reference_button(cx))
                                .child(
                                    ui::secondary_button(
                                        "design-focus-openpencil",
                                        Some((IconName::ArrowUpRight, Color::Muted)),
                                        t!("design_viewer.toolbar.open_in_openpencil"),
                                        true,
                                        cx,
                                    )
                                    .flex_grow(1.)
                                    .justify_center()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.open_in_openpencil(cx);
                                        },
                                    )),
                                ),
                        ),
                ),
        )
    }

    fn copy_reference_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ui::secondary_button(
            "design-focus-copy",
            Some((IconName::Copy, Color::Muted)),
            t!("design_viewer.focus.copy_reference"),
            true,
            cx,
        )
        .flex_grow(1.)
        .justify_center()
        .on_click(cx.listener(|this, _, _, cx| {
            this.copy_reference(cx);
        }))
    }

    fn copy_jsx_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.copying_jsx.is_some();
        let label = if busy {
            t!("design_viewer.inspector.copying_jsx")
        } else {
            t!("design_viewer.inspector.copy_jsx")
        };
        ui::secondary_button(
            "design-layer-copy-jsx",
            Some((IconName::Code, Color::Muted)),
            label,
            !busy,
            cx,
        )
        .flex_grow(1.)
        .justify_center()
        .when(busy, |this| {
            this.child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
        })
        .when(!busy, |this| {
            this.on_click(cx.listener(|this, _, _, cx| this.copy_jsx(cx)))
        })
    }

    fn render_layer_inspector(&self, path: &[&Layer], cx: &mut Context<Self>) -> impl IntoElement {
        let mut breadcrumb: Vec<String> = Vec::new();
        let mut screen_origin = (0., 0.);
        if let Some((page, _, screen)) = self.focused_screen_entry(cx) {
            breadcrumb.push(split_page_name(&page.name).1.to_string());
            breadcrumb.push(screen.name.clone());
            screen_origin = (screen.bounds.x, screen.bounds.y);
        }
        breadcrumb.extend(path.iter().map(|layer| layer.name.clone()));
        let Some(layer) = path.last().copied() else {
            return v_flex().into_any_element();
        };

        let header = h_flex()
            .gap_1p5()
            .child(
                Icon::new(layer_icon(layer.kind))
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(SharedString::from(layer.name.clone()))
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(if layer.visible {
                            Color::Default
                        } else {
                            Color::Disabled
                        })
                        .truncate(),
                ),
            )
            .when(!layer.visible, |this| {
                this.child(hidden_marker("design-inspector-hidden".to_string()))
            })
            .child(
                ui::group_icon_button("design-select-parent", IconName::ArrowUp, 24.)
                    .tooltip(Tooltip::for_action_title_in(
                        t!("design_viewer.inspector.select_parent"),
                        &ExitFocus,
                        &self.focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.select_parent(cx))),
            )
            .child(
                ui::group_icon_button("design-layer-openpencil", IconName::ArrowUpRight, 24.)
                    .tooltip(Tooltip::text(t!(
                        "design_viewer.toolbar.open_in_openpencil"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.open_in_openpencil(cx))),
            );

        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(header)
                    .child(
                        Label::new(SharedString::from(breadcrumb.join(" › ")))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .children(layer.component.as_ref().map(|component| {
                        h_flex()
                            .gap_1p5()
                            .child(ui::status_pill(
                                t!("design_viewer.inspector.instance"),
                                ui::StatusTone::Info,
                                cx,
                            ))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(SharedString::from(component.clone()))
                                        .size(LabelSize::Small)
                                        .truncate(),
                                ),
                            )
                    })),
            )
            .child(grouped_section(
                t!("design_viewer.inspector.section_layout"),
                layout_rows(layer, screen_origin, cx),
                cx,
            ))
            .when_some(
                Some(appearance_rows(layer, cx)).filter(|rows| !rows.is_empty()),
                |this, rows| {
                    this.child(grouped_section(
                        t!("design_viewer.inspector.section_appearance"),
                        rows,
                        cx,
                    ))
                },
            )
            .when_some(layer.text.as_ref(), |this, text| {
                this.child(grouped_section(
                    t!("design_viewer.inspector.section_text"),
                    text_rows(text, cx),
                    cx,
                ))
            })
            .when(!layer.unbound.is_empty(), |this| {
                this.child(unbound_callout(layer))
            })
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        ui::primary_button(
                            "design-layer-mention",
                            Some(IconName::Sparkle),
                            t!("design_viewer.inspector.mention_layer"),
                            Some(KeyBinding::for_action_in(
                                &MentionLayer,
                                &self.focus_handle,
                                cx,
                            )),
                            cx,
                        )
                        .justify_center()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.mention_in_agent(window, cx);
                        })),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1p5()
                            .child(self.copy_reference_button(cx))
                            .child(self.copy_jsx_button(cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_overlay(&self, cx: &App) -> Option<impl IntoElement> {
        let viewport = self.viewport?;
        let layers = self.focused_layers()?;
        let (_, _, screen) = self.focused_screen_entry(cx)?;
        let hover_color: Hsla = gpui::rgb(HOVER_OUTLINE).into();
        let selection_color: Hsla = gpui::rgb(SELECTION_OUTLINE).into();
        let distance_color: Hsla = gpui::rgb(DISTANCE_COLOR).into();

        let selected_path = self
            .selected_layer
            .as_ref()
            .and_then(|id| layer_path(layers, id));
        let selected = selected_path.as_ref().and_then(|path| path.last().copied());
        let hovered = self
            .hovered_layer
            .as_ref()
            .filter(|id| Some(*id) != self.selected_layer.as_ref())
            .and_then(|id| layer_path(layers, id))
            .and_then(|path| path.last().copied());

        let mut overlay = div().absolute().inset_0().overflow_hidden();
        if let Some(layer) = hovered {
            overlay = overlay.child(outline_box(
                viewport,
                &layer.bounds,
                false,
                hover_color,
                !layer.visible,
            ));
        }
        if let Some(layer) = selected {
            let (left, top, _, _) = viewport.project(&layer.bounds);
            overlay = overlay
                .child(outline_box(
                    viewport,
                    &layer.bounds,
                    true,
                    selection_color,
                    !layer.visible,
                ))
                .child(
                    h_flex()
                        .absolute()
                        .left(left - px(2.))
                        .top(top - px(22.))
                        .h(px(18.))
                        .px_1p5()
                        .rounded(Radius::Xs.rems())
                        .bg(selection_color)
                        .child(
                            Label::new(format!(
                                "{}  {}",
                                layer.name,
                                format_size(layer.bounds.width, layer.bounds.height)
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Custom(gpui::white()))
                            .single_line(),
                        ),
                );
            if self.option_held {
                let (target, inside) = match hovered {
                    Some(hovered) => (hovered.bounds, encloses(&hovered.bounds, &layer.bounds)),
                    None => {
                        let parent = selected_path.as_ref().and_then(|path| {
                            path.len()
                                .checked_sub(2)
                                .and_then(|index| path.get(index))
                                .map(|parent| parent.bounds)
                        });
                        (parent.unwrap_or(screen.bounds), true)
                    }
                };
                for mark in distance_marks(&layer.bounds, &target, inside) {
                    overlay = overlay.child(distance_mark(viewport, &mark, distance_color));
                }
            }
        }
        Some(overlay)
    }

    fn render_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas_bounds = self.canvas_bounds.clone();
        let view = cx.entity().downgrade();
        let hint = if self.focused_screen.is_some() {
            t!("design_viewer.hint_focused")
        } else {
            t!("design_viewer.hint")
        };
        div()
            .id("design-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .rounded(ui::TAB_STRIP_RADIUS)
            .on_pinch(cx.listener(Self::handle_pinch))
            .child(
                canvas(
                    move |bounds, _, _| canvas_bounds.set(Some(bounds)),
                    move |bounds, _, window, _| register_canvas_listeners(view, bounds, window),
                )
                .absolute()
                .inset_0(),
            )
            .child(self.browser.clone())
            .children(self.render_overlay(cx))
            .child(
                h_flex()
                    .absolute()
                    .bottom_3()
                    .left_0()
                    .right_0()
                    .justify_center()
                    .child(ui::status_chip(None, hint, Color::Muted, cx)),
            )
    }

    fn render_loading(&self, progress: Option<crate::LoadingProgress>, cx: &App) -> EmptyState {
        let file_name = self.file_name(cx);
        EmptyState::new(
            IconName::LoadCircle,
            t!("design_viewer.loading.title", file = file_name),
        )
        .spinning(true)
        .description(match progress {
            Some(progress) => progress.label(),
            None => t!("design_viewer.loading.reading"),
        })
        .when_some(progress, |this, progress| {
            this.action(
                div()
                    .w(px(240.))
                    .child(ui::progress_line(progress.fraction(), cx)),
            )
        })
    }

    fn render_failure(
        &self,
        error: &crate::DesignViewerError,
        cx: &mut Context<Self>,
    ) -> EmptyState {
        EmptyState::new(IconName::Warning, t!("design_viewer.error.title"))
            .tone(EmptyStateTone::Error)
            .description(t!("design_viewer.error.description"))
            .detail(error.message())
            .action(
                h_flex()
                    .gap_2()
                    .child(
                        ui::secondary_button(
                            "design-view-log",
                            None,
                            t!("design_viewer.error.view_log"),
                            true,
                            cx,
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(workspace::OpenLog), cx);
                        }),
                    )
                    .child(
                        ui::primary_button(
                            "design-try-again",
                            None,
                            t!("design_viewer.error.try_again"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.document
                                .update(cx, |document, cx| document.revalidate(cx));
                        })),
                    ),
            )
    }
}

fn grouped_section(title: SharedString, rows: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().px_1().child(ui::section_header(title, None)))
        .child(ui::grouped_block(cx).children(rows))
}

/// The page itself handles drags and wheel events, so these only observe the pointer: in the
/// capture phase, without stopping propagation.
fn register_canvas_listeners(
    view: WeakEntity<DesignView>,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseMoveEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture || event.pressed_button.is_some() {
                return;
            }
            let position = bounds.contains(&event.position).then_some(event.position);
            view.update(cx, |this, cx| {
                if this.option_held != event.modifiers.alt {
                    this.option_held = event.modifiers.alt;
                    cx.notify();
                }
                this.hover_at(position, event.modifiers.platform, cx);
            })
            .ok();
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseDownEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture
                || event.button != MouseButton::Left
                || !bounds.contains(&event.position)
            {
                return;
            }
            view.update(cx, |this, _| this.pointer_down = Some(event.position))
                .ok();
        }
    });
    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
        if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
            return;
        }
        view.update(cx, |this, cx| {
            this.release_at(event.position, event.modifiers.platform, cx)
        })
        .ok();
    });
}

fn contains(bounds: &crate::Bounds, (x, y): (f64, f64)) -> bool {
    x >= bounds.x && y >= bounds.y && x <= bounds.x + bounds.width && y <= bounds.y + bounds.height
}

fn encloses(outer: &crate::Bounds, inner: &crate::Bounds) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// The topmost visible layer under `point`, or with `deep` the innermost one.
fn layer_at(layers: &[Layer], point: (f64, f64), deep: bool) -> Option<&Layer> {
    // Later siblings are painted on top.
    let hit = layers
        .iter()
        .rev()
        .find(|layer| layer.visible && contains(&layer.bounds, point))?;
    if deep {
        Some(layer_at(&hit.children, point, true).unwrap_or(hit))
    } else {
        Some(hit)
    }
}

/// Every layer named `name` (ignoring ASCII case), each with its ancestors, outermost first.
pub(crate) fn layers_named<'a>(layers: &'a [Layer], name: &str) -> Vec<Vec<&'a Layer>> {
    let mut found = Vec::new();
    for layer in layers {
        if layer.name.eq_ignore_ascii_case(name) {
            found.push(vec![layer]);
        }
        for mut path in layers_named(&layer.children, name) {
            path.insert(0, layer);
            found.push(path);
        }
    }
    found
}

pub(crate) fn layer_path<'a>(layers: &'a [Layer], id: &str) -> Option<Vec<&'a Layer>> {
    layers.iter().find_map(|layer| {
        if layer.id == id {
            return Some(vec![layer]);
        }
        let mut path = layer_path(&layer.children, id)?;
        path.insert(0, layer);
        Some(path)
    })
}

fn layer_matches(layer: &Layer, query: &str) -> bool {
    layer.name.to_lowercase().contains(query)
        || layer
            .children
            .iter()
            .any(|child| layer_matches(child, query))
}

fn layer_icon(kind: LayerKind) -> IconName {
    match kind {
        LayerKind::AutoLayout => IconName::ListTree,
        LayerKind::Frame | LayerKind::Group => IconName::ChevronRight,
        LayerKind::Text => IconName::CaseSensitive,
        LayerKind::Instance => IconName::Blocks,
        LayerKind::Image => IconName::Eye,
        LayerKind::Shape => IconName::SquareDot,
    }
}

fn hidden_marker(id: String) -> impl IntoElement {
    div()
        .id(SharedString::from(id))
        .tooltip(Tooltip::text(t!("design_viewer.inspector.hidden")))
        .child(
            Icon::new(IconName::EyeOff)
                .size(IconSize::XSmall)
                .color(Color::Disabled),
        )
}

fn outline_box(
    viewport: Viewport,
    bounds: &crate::Bounds,
    thick: bool,
    color: Hsla,
    dashed: bool,
) -> impl IntoElement {
    let (left, top, width, height) = viewport.project(bounds);
    let border = if thick { px(2.) } else { px(1.) };
    div()
        .absolute()
        .left(left - border)
        .top(top - border)
        .w(width + border * 2.)
        .h(height + border * 2.)
        .map(|this| {
            if thick {
                this.border_2()
            } else {
                this.border_1()
            }
        })
        .when(dashed, |this| this.border_dashed())
        .border_color(color)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DistanceMark {
    from: (f64, f64),
    to: (f64, f64),
}

impl DistanceMark {
    fn length(&self) -> f64 {
        (self.to.0 - self.from.0).abs() + (self.to.1 - self.from.1).abs()
    }
}

/// The gaps between the selection and `target`: to its inner edges when it encloses the
/// selection, otherwise across the space between them.
fn distance_marks(
    selection: &crate::Bounds,
    target: &crate::Bounds,
    inside: bool,
) -> Vec<DistanceMark> {
    let (left, top) = (selection.x, selection.y);
    let (right, bottom) = (
        selection.x + selection.width,
        selection.y + selection.height,
    );
    let (target_right, target_bottom) = (target.x + target.width, target.y + target.height);
    let center_x = left + selection.width / 2.;
    let center_y = top + selection.height / 2.;
    let mut marks = Vec::new();
    if inside {
        marks.push(DistanceMark {
            from: (target.x, center_y),
            to: (left, center_y),
        });
        marks.push(DistanceMark {
            from: (right, center_y),
            to: (target_right, center_y),
        });
        marks.push(DistanceMark {
            from: (center_x, target.y),
            to: (center_x, top),
        });
        marks.push(DistanceMark {
            from: (center_x, bottom),
            to: (center_x, target_bottom),
        });
    } else {
        let overlap_y = (top.max(target.y) + bottom.min(target_bottom)) / 2.;
        let overlap_x = (left.max(target.x) + right.min(target_right)) / 2.;
        let y = if top.max(target.y) <= bottom.min(target_bottom) {
            overlap_y
        } else {
            center_y
        };
        let x = if left.max(target.x) <= right.min(target_right) {
            overlap_x
        } else {
            center_x
        };
        if right <= target.x {
            marks.push(DistanceMark {
                from: (right, y),
                to: (target.x, y),
            });
        }
        if target_right <= left {
            marks.push(DistanceMark {
                from: (target_right, y),
                to: (left, y),
            });
        }
        if bottom <= target.y {
            marks.push(DistanceMark {
                from: (x, bottom),
                to: (x, target.y),
            });
        }
        if target_bottom <= top {
            marks.push(DistanceMark {
                from: (x, target_bottom),
                to: (x, top),
            });
        }
    }
    marks.retain(|mark| mark.length() > 0.);
    marks
}

fn distance_mark(viewport: Viewport, mark: &DistanceMark, color: Hsla) -> impl IntoElement {
    let (from_x, from_y) = viewport.point(mark.from.0, mark.from.1);
    let (to_x, to_y) = viewport.point(mark.to.0, mark.to.1);
    let horizontal = mark.from.1 == mark.to.1;
    let line = if horizontal {
        div()
            .absolute()
            .left(from_x.min(to_x))
            .top(from_y)
            .w((to_x - from_x).abs())
            .h(px(1.))
    } else {
        div()
            .absolute()
            .left(from_x)
            .top(from_y.min(to_y))
            .w(px(1.))
            .h((to_y - from_y).abs())
    };
    let (tag_x, tag_y) = if horizontal {
        ((from_x + to_x) / 2. - px(10.), from_y + px(4.))
    } else {
        (from_x + px(4.), (from_y + to_y) / 2. - px(8.))
    };
    div().child(line.bg(color)).child(
        div()
            .absolute()
            .left(tag_x)
            .top(tag_y)
            .px_1()
            .rounded(Radius::Xs.rems())
            .bg(color)
            .child(
                Label::new(render::number(mark.length()))
                    .size(LabelSize::XSmall)
                    .color(Color::Custom(gpui::white()))
                    .single_line(),
            ),
    )
}

struct PropertyRow {
    label: SharedString,
    value: SharedString,
    detail: Option<SharedString>,
    swatch: Option<Hsla>,
    token: Option<SharedString>,
}

impl PropertyRow {
    fn new(label: SharedString, value: impl Into<SharedString>) -> Self {
        Self {
            label,
            value: value.into(),
            detail: None,
            swatch: None,
            token: None,
        }
    }

    fn detail(mut self, detail: Option<SharedString>) -> Self {
        self.detail = detail;
        self
    }

    fn swatch(mut self, color: &render::Color) -> Self {
        self.swatch = Some(hsla_color(color));
        self
    }

    fn token(mut self, token: Option<&String>) -> Self {
        self.token = token.map(|token| SharedString::from(token.clone()));
        self
    }

    fn render(self, cx: &App) -> AnyElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_start()
            .child(
                div().w(px(84.)).flex_none().child(
                    Label::new(self.label)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .children(self.swatch.map(|color| {
                                div()
                                    .flex_none()
                                    .size(px(12.))
                                    .rounded(Radius::Xs.rems())
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .bg(color)
                            }))
                            .child(
                                div().min_w_0().child(
                                    Label::new(self.value).size(LabelSize::Small).truncate(),
                                ),
                            ),
                    )
                    .children(self.token.map(|token| {
                        Label::new(token)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Custom(gpui::rgb(TOKEN_COLOR).into()))
                            .truncate()
                    }))
                    .children(self.detail.map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    })),
            )
            .into_any_element()
    }
}

fn hsla_color(color: &render::Color) -> Hsla {
    gpui::Rgba {
        r: color.r as f32,
        g: color.g as f32,
        b: color.b as f32,
        a: color.a as f32,
    }
    .into()
}

fn sizing_label(sizing: &str) -> SharedString {
    match sizing {
        "FILL" => t!("design_viewer.inspector.sizing_fill"),
        "HUG" => t!("design_viewer.inspector.sizing_hug"),
        _ => t!("design_viewer.inspector.sizing_fixed"),
    }
}

fn alignment_label(alignment: Option<&str>) -> SharedString {
    match alignment {
        Some("CENTER") => t!("design_viewer.inspector.align_center"),
        Some("MAX") => t!("design_viewer.inspector.align_end"),
        Some("SPACE_BETWEEN") => t!("design_viewer.inspector.align_space_between"),
        _ => t!("design_viewer.inspector.align_start"),
    }
}

fn layout_rows(layer: &Layer, screen_origin: (f64, f64), cx: &App) -> Vec<AnyElement> {
    let sizing = layer.sizing.as_ref().map(|sizing| {
        let mut parts = Vec::new();
        if let Some(horizontal) = sizing.horizontal.as_deref() {
            parts.push(t!(
                "design_viewer.inspector.sizing_width",
                sizing = sizing_label(horizontal)
            ));
        }
        if let Some(vertical) = sizing.vertical.as_deref() {
            parts.push(t!(
                "design_viewer.inspector.sizing_height",
                sizing = sizing_label(vertical)
            ));
        }
        SharedString::from(parts.join(" · "))
    });
    let mut rows = vec![
        PropertyRow::new(
            t!("design_viewer.inspector.size"),
            format_size(layer.bounds.width, layer.bounds.height),
        )
        .detail(sizing)
        .render(cx),
        PropertyRow::new(
            t!("design_viewer.inspector.position"),
            t!(
                "design_viewer.inspector.position_value",
                x = render::number(layer.bounds.x - screen_origin.0),
                y = render::number(layer.bounds.y - screen_origin.1)
            ),
        )
        .render(cx),
    ];
    if let Some(auto_layout) = &layer.auto_layout {
        rows.push(
            PropertyRow::new(
                t!("design_viewer.inspector.auto_layout"),
                t!(
                    "design_viewer.inspector.auto_layout_value",
                    direction = if auto_layout.horizontal { "→" } else { "↓" },
                    gap = render::number(auto_layout.gap.value),
                    alignment = format!(
                        "{} / {}",
                        alignment_label(auto_layout.primary_align.as_deref()),
                        alignment_label(auto_layout.counter_align.as_deref())
                    )
                ),
            )
            .token(auto_layout.gap.token.as_ref())
            .render(cx),
        );
        rows.push(
            PropertyRow::new(
                t!("design_viewer.inspector.padding"),
                render::padding_value(&auto_layout.padding.value),
            )
            .token(auto_layout.padding.token.as_ref())
            .render(cx),
        );
    }
    rows
}

fn appearance_rows(layer: &Layer, cx: &App) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    if let Some(fill) = &layer.fill {
        rows.push(
            PropertyRow::new(
                t!("design_viewer.inspector.fill"),
                render::hex_color(&fill.value),
            )
            .swatch(&fill.value)
            .token(fill.token.as_ref())
            .render(cx),
        );
    }
    if let Some(radius) = &layer.radius {
        rows.push(
            PropertyRow::new(t!("design_viewer.inspector.radius"), radius.value.clone())
                .token(radius.token.as_ref())
                .render(cx),
        );
    }
    if let Some(stroke) = &layer.stroke {
        rows.push(
            PropertyRow::new(
                t!("design_viewer.inspector.stroke"),
                format!(
                    "{} · {}",
                    render::number(stroke.weight),
                    render::hex_color(&stroke.color.value)
                ),
            )
            .swatch(&stroke.color.value)
            .token(stroke.color.token.as_ref())
            .render(cx),
        );
    }
    if let Some(shadow) = &layer.shadow {
        rows.push(
            PropertyRow::new(t!("design_viewer.inspector.shadow"), shadow.value.clone())
                .token(shadow.token.as_ref())
                .render(cx),
        );
    }
    rows
}

fn text_rows(text: &render::TextStyle, cx: &App) -> Vec<AnyElement> {
    let mut font = Vec::new();
    if let Some(family) = &text.family {
        font.push(family.value.clone());
    }
    if let Some(size) = &text.size {
        font.push(render::number(size.value));
    }
    let font = format!("{} · {}", font.join(" "), text.weight);
    let font_tokens: Vec<&str> = [
        text.family
            .as_ref()
            .and_then(|family| family.token.as_deref()),
        text.size.as_ref().and_then(|size| size.token.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let font_token = (!font_tokens.is_empty()).then(|| font_tokens.join(" · "));

    let mut rows = vec![
        PropertyRow::new(
            t!("design_viewer.inspector.characters"),
            if text.characters.is_empty() {
                SharedString::from(NO_VALUE)
            } else {
                SharedString::from(format!("“{}”", text.characters))
            },
        )
        .render(cx),
        PropertyRow::new(t!("design_viewer.inspector.font"), font)
            .token(font_token.as_ref())
            .render(cx),
    ];
    if let Some(color) = &text.color {
        rows.push(
            PropertyRow::new(
                t!("design_viewer.inspector.color"),
                render::hex_color(&color.value),
            )
            .swatch(&color.value)
            .token(color.token.as_ref())
            .render(cx),
        );
    }
    rows
}

fn unbound_property_label(property: UnboundProperty) -> SharedString {
    match property {
        UnboundProperty::Fill => t!("design_viewer.inspector.fill"),
        UnboundProperty::Radius => t!("design_viewer.inspector.radius"),
        UnboundProperty::Gap => t!("design_viewer.inspector.gap"),
        UnboundProperty::Padding => t!("design_viewer.inspector.padding"),
        UnboundProperty::Stroke => t!("design_viewer.inspector.stroke"),
        UnboundProperty::FontSize => t!("design_viewer.inspector.font"),
        UnboundProperty::TextColor => t!("design_viewer.inspector.color"),
    }
}

fn unbound_callout(layer: &Layer) -> impl IntoElement {
    Callout::new()
        .severity(Severity::Warning)
        .icon(IconName::Warning)
        .title(t!(
            "design_viewer.inspector.unbound_title",
            count = layer.unbound.len()
        ))
        .description_slot(
            v_flex()
                .gap_0p5()
                .children(layer.unbound.iter().map(|unbound| {
                    Label::new(t!(
                        "design_viewer.inspector.unbound_item",
                        property = unbound_property_label(unbound.property),
                        value = unbound.value.as_str()
                    ))
                    .size(LabelSize::XSmall)
                }))
                .child(
                    Label::new(t!("design_viewer.inspector.unbound_hint"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

fn is_design_system(page: &PageEntry) -> bool {
    split_page_name(&page.name).0 == Some(DESIGN_SYSTEM_PREFIX)
}

pub(crate) fn split_page_name(name: &str) -> (Option<&str>, &str) {
    match name.split_once('·') {
        Some((number, title))
            if !number.trim().is_empty()
                && number
                    .trim()
                    .chars()
                    .all(|character| character.is_ascii_digit()) =>
        {
            (Some(number.trim()), title.trim())
        }
        _ => (None, name),
    }
}

fn format_size(width: f64, height: f64) -> SharedString {
    SharedString::from(format!("{} × {}", width.round(), height.round()))
}

fn file_name(path: &Path) -> SharedString {
    path.file_name()
        .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

fn css_color(color: Hsla) -> String {
    let rgba = gpui::Rgba::from(color);
    format!(
        "rgba({},{},{},{:.3})",
        (rgba.r * 255.0).round(),
        (rgba.g * 255.0).round(),
        (rgba.b * 255.0).round(),
        rgba.a
    )
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

async fn launch_openpencil(path: &Path) -> anyhow::Result<()> {
    let mut command = match which::which("openpencil") {
        Ok(cli) => {
            let mut command = util::command::new_command(cli);
            command.args(["documents", "open"]).arg(path);
            command
        }
        Err(_) => {
            let mut command = util::command::new_command("open");
            command.args(["-a", "OpenPencil"]).arg(path);
            command
        }
    };
    let output = command.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

impl EventEmitter<ItemEvent> for DesignView {}

impl Focusable for DesignView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DesignView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.document.read(cx).state().clone();
        let has_manifest = self.document.read(cx).manifest().is_some();

        let root = h_flex()
            .id("design-view")
            .key_context("DesignViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background);

        if !has_manifest {
            let empty_state = match &state {
                DesignDocumentState::Failed(error) => self.render_failure(error, cx),
                DesignDocumentState::Loading { progress } => self.render_loading(*progress, cx),
                DesignDocumentState::Ready => self.render_loading(None, cx),
            };
            return root.child(empty_state);
        }

        root.on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.run_viewer("zoomIn()", cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.run_viewer("zoomOut()", cx)))
            .on_action(cx.listener(|this, _: &FitToScreen, _, cx| this.run_viewer("fit()", cx)))
            .on_action(cx.listener(|this, _: &NextScreen, _, cx| this.step_screen(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousScreen, _, cx| this.step_screen(-1, cx)))
            .on_action(cx.listener(|this, _: &ExitFocus, _, cx| this.exit_level(cx)))
            .on_action(
                cx.listener(|this, _: &MentionLayer, window, cx| this.mention_in_agent(window, cx)),
            )
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                if this.option_held != event.modifiers.alt {
                    this.option_held = event.modifiers.alt;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_editor.focus_handle(cx).focus(window, cx);
            }))
            .child(self.render_nav(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_2()
                    .gap_2()
                    .child(self.render_rail(cx))
                    .child(self.render_canvas(cx)),
            )
            .children(self.render_inspector(cx))
    }
}

impl Item for DesignView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.file_name(cx)
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Pencil).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(SharedString::from(
            self.document.read(cx).path().to_string_lossy().into_owned(),
        ))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = Some(workspace.weak_handle());
    }

    fn activated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.browser
            .update(cx, |browser, _| browser.set_browser_hidden(false));
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.browser
            .update(cx, |browser, _| browser.set_browser_hidden(true));
    }
}

impl workspace::item::ProjectItem for DesignView {
    type Item = FigFile;

    fn for_project_item(
        project: Entity<project::Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<FigFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = item.read(cx).abs_path.clone();
        let fs = project.read(cx).fs().clone();
        Self::new(path, fs, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::split_page_name;

    #[test]
    fn page_names_lose_their_number_prefix() {
        assert_eq!(split_page_name("00 · Cores"), (Some("00"), "Cores"));
        assert_eq!(split_page_name("16 · Design"), (Some("16"), "Design"));
        assert_eq!(split_page_name("Rascunho"), (None, "Rascunho"));
        assert_eq!(split_page_name("A · B"), (None, "A · B"));
    }
}
