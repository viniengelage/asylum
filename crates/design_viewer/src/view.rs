use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, PinchEvent, Pixels, Render, SharedString, Subscription, WeakEntity, Window, canvas, div,
    px,
};
use i18n::t;
use settings::{DesignCanvasBackground, Settings as _, SettingsStore};
use ui::{
    Button, ButtonSize, Color, EmptyState, EmptyStateTone, Icon, IconButton, IconName, IconSize,
    KeyBinding, Label, LabelSize, ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use web_preview::{WebPreviewEvent, WebPreviewView};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

use crate::agent_toolkit::mention_id;
use crate::{
    DesignDocument, DesignDocumentEvent, DesignDocumentState, DesignViewerSettings, ExitFocus,
    FigFile, FitToScreen, FocusSearch, NextScreen, PageEntry, PageId, PreviousScreen, Screen,
    ZoomIn, ZoomOut, screen_reference,
};

const NAV_WIDTH: Pixels = px(252.);
const DESIGN_SYSTEM_PREFIX: &str = "00";

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
    zoom_percent: Option<f32>,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    canvas_background: DesignCanvasBackground,
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
            cx.subscribe(&search_editor, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
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
            zoom_percent: None,
            canvas_bounds: Rc::new(Cell::new(None)),
            canvas_background: DesignViewerSettings::get_global(cx).canvas_background,
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
            self.focused_screen = None;
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

        self.current_page = Some(page);
        self.focused_screen = screen;
        self.zoom_percent = None;
        self.browser
            .update(cx, |browser, cx| browser.navigate(url.to_string(), cx));
        cx.notify();
    }

    fn show_screen(&mut self, page: PageId, screen: String, cx: &mut Context<Self>) {
        if self.current_page.as_ref() == Some(&page) {
            // Only the hash would change, which doesn't reload the page or rerun its script.
            self.run_viewer(&format!("focus({})", js_string(&screen)), cx);
            self.focused_screen = Some(screen);
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
        for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            match key.as_ref() {
                "screen" => screen = Some(value.into_owned()),
                "zoom" => zoom = value.parse::<f32>().ok(),
                _ => {}
            }
        }
        if self.focused_screen != screen || self.zoom_percent != zoom {
            self.focused_screen = screen;
            self.zoom_percent = zoom;
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
        let reference = format!("@{}", screen_reference(page, screen));
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
        let title = screen_reference(page, screen);
        let id = mention_id(&screen.id, &self.path(cx));
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
        self.focused_screen = None;
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
        let query = self.search_editor.read(cx).text(cx).trim().to_lowercase();

        let mut design_system = Vec::new();
        let mut screens = Vec::new();
        let mut any_match = false;
        if let Some(manifest) = self.document.read(cx).manifest().cloned() {
            for page in &manifest.pages {
                let matching: Vec<&Screen> = page
                    .screens
                    .iter()
                    .filter(|screen| {
                        query.is_empty() || screen.name.to_lowercase().contains(&query)
                    })
                    .collect();
                if !query.is_empty() && matching.is_empty() {
                    continue;
                }
                any_match = true;
                let rows = self.render_page_rows(page, &matching, !query.is_empty(), cx);
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
                        this.child(nav_section(
                            t!(
                                "design_viewer.nav.section_design_system",
                                count = design_system.len()
                            ),
                            design_system,
                            cx,
                        ))
                    })
                    .when(!screens.is_empty(), |this| {
                        this.child(nav_section(
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
        screens: &[&Screen],
        filtering: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = filtering || self.expanded_pages.contains(&page.id);
        let (number, title) = split_page_name(&page.name);
        let page_id = page.id.clone();
        let is_current = self.current_page.as_ref() == Some(&page.id);

        let page_row = ListItem::new(SharedString::from(format!("page-{}", page.id)))
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
                let page_id = page_id.clone();
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
                            .indent_level(1)
                            .selectable(false)
                            .child(
                                Label::new(t!("design_viewer.nav.no_screens"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                } else {
                    this.children(screens.iter().map(|screen| {
                        let selected =
                            is_current && self.focused_screen.as_ref() == Some(&screen.id);
                        let page_id = page_id.clone();
                        let screen_id = screen.id.clone();
                        ListItem::new(SharedString::from(format!("screen-{}", screen.id)))
                            .indent_level(1)
                            .toggle_state(selected)
                            .start_slot(
                                Icon::new(IconName::Smartphone)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.show_screen(page_id.clone(), screen_id.clone(), cx);
                            }))
                            .child(
                                Label::new(SharedString::from(screen.name.clone()))
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                    }))
                }
            })
            .into_any_element()
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.current_page_entry(cx);
        let focused = self
            .focused_screen_entry(cx)
            .map(|(_, _, screen)| screen.clone());
        let page_title = page.map(|page| split_page_name(&page.name).1.to_string());
        let size = match (&focused, page) {
            (Some(screen), _) => Some((screen.bounds.width, screen.bounds.height)),
            (None, Some(page)) => Some((page.width, page.height)),
            (None, None) => None,
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
                            .child(Label::new(screen.name).size(LabelSize::Small).truncate())
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

    fn render_focus_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
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
                .w(px(272.))
                .p_3()
                .gap_2()
                .occlude()
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
                                .gap_1p5()
                                .child(
                                    ui::secondary_button(
                                        "design-focus-copy",
                                        Some((IconName::Copy, Color::Muted)),
                                        t!("design_viewer.focus.copy_reference"),
                                        true,
                                        cx,
                                    )
                                    .flex_1()
                                    .justify_center()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.copy_reference(cx);
                                        },
                                    )),
                                )
                                .child(
                                    ui::secondary_button(
                                        "design-focus-openpencil",
                                        Some((IconName::ArrowUpRight, Color::Muted)),
                                        t!("design_viewer.toolbar.open_in_openpencil"),
                                        true,
                                        cx,
                                    )
                                    .flex_1()
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

    fn render_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas_bounds = self.canvas_bounds.clone();
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
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(self.browser.clone())
            .children(
                self.render_focus_card(cx)
                    .map(|card| div().absolute().top_3().right_3().child(card)),
            )
            .child(
                h_flex()
                    .absolute()
                    .bottom_3()
                    .left_0()
                    .right_0()
                    .justify_center()
                    .child(ui::status_chip(
                        None,
                        t!("design_viewer.hint"),
                        Color::Muted,
                        cx,
                    )),
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

fn nav_section(title: SharedString, rows: Vec<AnyElement>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().px_1().child(ui::section_header(title, None)))
        .child(ui::grouped_block(cx).children(rows))
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
            .on_action(cx.listener(|this, _: &ExitFocus, _, cx| this.exit_focus(cx)))
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
