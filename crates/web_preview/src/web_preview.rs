#[cfg(target_os = "macos")]
mod cef_browser;
#[cfg(target_os = "macos")]
mod cef_install;
#[cfg(target_os = "macos")]
mod cef_paths;
mod runtime_discovery;

use editor::Editor;
use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollWheelEvent, SharedString, Styled,
    Subscription, Task, Window, actions, canvas, div, px,
};
use ui::{
    h_flex, prelude::*, v_flex, Color, HeaderBar, HeaderBarLevel, Icon, IconButton, IconName,
    IconSize, Label, Tooltip,
};
use workspace::{
    StatusItemView, Workspace,
    item::{Item, ItemEvent, ItemHandle},
};

actions!(
    web_preview,
    [
        /// Opens a new web preview tab.
        OpenWebPreview,
        /// Reloads the active web preview.
        Reload,
        /// Navigates back in the active web preview.
        GoBack,
        /// Navigates forward in the active web preview.
        GoForward,
        /// Focuses the URL bar.
        FocusUrlBar,
        /// Opens DevTools for the active web preview.
        OpenDevTools,
        /// Submits the URL bar content for navigation.
        SubmitUrl,
        /// Selects all content in the page.
        SelectAll,
        /// Copies the page selection to the clipboard.
        Copy,
        /// Cuts the page selection to the clipboard.
        Cut,
        /// Pastes the clipboard into the focused field in the page.
        Paste,
        /// Undoes the last edit in the page.
        Undo,
        /// Redoes the last undone edit in the page.
        Redo,
    ]
);

/// How often the poll loop looks for a URL dropped by the `zed browse` utility.
const BROWSE_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a browser gets to paint its first frame before we stop trusting the GPU to
/// deliver them. Even `about:blank` paints immediately, so this is generous.
#[cfg(target_os = "macos")]
const FIRST_PAINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a browser that cannot use the GPU path either gets before the tab reports
/// failure. Software paint is the last resort, so there is nothing left to fall back to —
/// this only has to outlast a cold Chromium renderer's startup.
#[cfg(target_os = "macos")]
const PAINT_GIVE_UP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Why the tab is showing a message where the page would go.
struct PreviewError {
    headline: SharedString,
    detail: SharedString,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        workspace.register_action(WebPreviewView::open_action);
        workspace.register_action(WebPreviewView::open_url_action);

        if let Some(window) = window {
            let button = _cx.new(|_cx| WebPreviewButton);
            workspace.status_bar().update(_cx, |status_bar, cx| {
                status_bar.add_right_item(button, window, cx);
            });
        }
    })
    .detach();
}

struct WebPreviewButton;

impl Render for WebPreviewButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("web-preview-button", IconName::ToolWeb)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Open Web Preview"))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(OpenWebPreview), cx);
            })
    }
}

impl StatusItemView for WebPreviewButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

pub struct WebPreviewView {
    focus_handle: FocusHandle,
    browser_focus: FocusHandle,
    url_editor: Entity<Editor>,
    url: String,
    title: String,
    loading: bool,
    can_go_back: bool,
    can_go_forward: bool,
    last_bounds: Option<Bounds<Pixels>>,
    scale_factor: f32,
    /// Buttons the renderer believes are held, as `event_flags` bits. Chromium only treats a
    /// move as a drag when the button bit is set on the move event, and it needs a matching
    /// up for every down or the page stays stuck mid-selection.
    #[cfg(target_os = "macos")]
    pressed_buttons: u32,
    /// Last modifier state reported to the page, to diff bare modifier presses against.
    #[cfg(target_os = "macos")]
    last_modifiers: gpui::Modifiers,
    _poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    #[cfg(target_os = "macos")]
    browser: Option<cef_browser::CefBrowserInstance>,
    #[cfg(target_os = "macos")]
    latest_frame: Option<cef_browser::BrowserFrame>,
    /// Whether the pane is showing another item, in which case nothing can see what CEF
    /// paints.
    hidden: bool,
    error: Option<PreviewError>,
    #[cfg(target_os = "macos")]
    cef_installer: Entity<cef_install::CefInstaller>,
}

impl WebPreviewView {
    pub fn open_action(
        workspace: &mut Workspace,
        _action: &OpenWebPreview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let view = cx.new(|cx| Self::new(window, cx));
        Self::add_to_workspace(view, workspace, window, cx);
    }

    pub fn open_url_action(
        workspace: &mut Workspace,
        action: &zed_actions::web_preview::OpenUrlInWebPreview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let url = action.url.clone();
        let view = cx.new(|cx| Self::new_with_url(url, window, cx));
        Self::add_to_workspace(view, workspace, window, cx);
    }

    fn add_to_workspace(
        view: Entity<Self>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let item = Box::new(view) as Box<dyn workspace::ItemHandle>;
        // Every page opens in the slot the layout assigned to the browser; without a
        // declared slot this keeps opening next to the editor as before.
        if !workspace.add_item_to_kind_slot(
            &Self::content_kind(),
            item.boxed_clone(),
            true,
            window,
            cx,
        ) {
            workspace.add_item_to_active_pane(item, None, true, window, cx);
        }
    }

    /// The layout slot browser pages are routed to.
    pub fn content_kind() -> workspace::ContentKind {
        workspace::ContentKind::new("Browser")
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let browser_focus = cx.focus_handle();

        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter URL...", window, cx);
            editor.set_text("https://", window, cx);
            editor
        });

        let submit_subscription = cx.subscribe_in(&url_editor, window, |_this, _editor, _event: &editor::EditorEvent, _window, _cx| {
        });

        // The pane focuses the item's own handle, which is the outer container. Send that on
        // to the page so the keyboard works the moment the tab is activated, instead of only
        // after the first click. Guarded on `is_focused` so focusing the URL bar — a
        // descendant — is left alone.
        let focus_redirect = cx.on_focus_in(&focus_handle, window, |this, window, cx| {
            if this.focus_handle.is_focused(window) {
                this.browser_focus.focus(window, cx);
            }
        });

        let browser_focus_in = cx.on_focus_in(&browser_focus, window, |this, _window, _cx| {
            #[cfg(target_os = "macos")]
            if let Some(browser) = &this.browser {
                browser.set_focus(true);
            }
        });

        let browser_focus_out =
            cx.on_focus_out(&browser_focus, window, |this, _event, _window, _cx| {
                #[cfg(target_os = "macos")]
                {
                    this.release_mouse_capture();
                    if let Some(browser) = &this.browser {
                        browser.set_focus(false);
                    }
                }
            });

        // A drag interrupted by cmd-tab never produces a mouse-up, so tell the renderer the
        // button is gone rather than leaving the page selecting forever.
        let window_activation = cx.observe_window_activation(window, |this, window, _cx| {
            #[cfg(target_os = "macos")]
            if !window.is_window_active() {
                this.release_mouse_capture();
            }
        });

        let poll_task = cx.spawn(async move |this, cx| {
            let mut delay = std::time::Duration::from_millis(1);
            let mut last_browse_check = std::time::Instant::now();
            #[cfg(target_os = "macos")]
            let mut channel: Option<cef_browser::BrowserChannel> = None;
            #[cfg(target_os = "macos")]
            let mut synced_state_version = 0;
            #[cfg(target_os = "macos")]
            let mut browser_started_at: Option<std::time::Instant> = None;
            #[cfg(target_os = "macos")]
            let mut painted_once = false;

            loop {
                cx.background_executor().timer(delay).await;

                // Step 1: Flush input + pump CEF OUTSIDE entity update
                // to avoid re-entrancy crashes
                #[cfg(target_os = "macos")]
                {
                    // The browser is created the first time the element lays out, so
                    // until then this is the only step that needs the entity.
                    if channel.is_none() {
                        channel = this
                            .update(cx, |this, _cx| {
                                this.browser.as_ref().map(|browser| browser.channel())
                            })
                            .ok()
                            .flatten();
                        if channel.is_some() {
                            browser_started_at = Some(std::time::Instant::now());
                        }
                    }

                    if let Some(channel) = channel.as_ref()
                        && channel.flush_input()
                    {
                        cef_browser::request_pump();
                    }

                    delay = cef_browser::pump_message_loop();
                }
                #[cfg(not(target_os = "macos"))]
                {
                    delay = std::time::Duration::from_millis(50);
                }

                // Check for browse requests from the terminal utility. This is a miss
                // on nearly every tick, so it runs far slower than the pump.
                if last_browse_check.elapsed() >= BROWSE_REQUEST_INTERVAL {
                    last_browse_check = std::time::Instant::now();
                    let browse_path = browse_request_path();
                    if let Ok(url) = std::fs::read_to_string(&browse_path) {
                        let url = url.trim().to_string();
                        if !url.is_empty() {
                            std::fs::remove_file(&browse_path).ok();
                            this.update(cx, |this, cx| {
                                log::info!("web_preview: browse request for {url}");
                                // Can't set editor text without Window in this context
                                #[cfg(target_os = "macos")]
                                if let Some(ref browser) = this.browser {
                                    browser.navigate_to(&url);
                                }
                                runtime_discovery::write_runtime_discovery(&url, None);
                                cx.notify();
                            }).ok();
                        }
                    }
                }

                // Step 2: Sync state inside entity update. This loop ticks up to a
                // thousand times a second, so a tick that has nothing to hand the view
                // must not touch the entity at all, let alone redraw the pane.
                #[cfg(target_os = "macos")]
                {
                    let Some(current_channel) = channel.as_ref() else {
                        continue;
                    };
                    let has_frame = current_channel.has_frame();
                    painted_once |= has_frame;

                    // Asking the GPU process for frames can leave us with a browser that
                    // paints nowhere at all, which is worse than copying pixels around.
                    if !painted_once
                        && cef_browser::shared_texture_enabled()
                        && browser_started_at
                            .is_some_and(|started_at| started_at.elapsed() > FIRST_PAINT_TIMEOUT)
                    {
                        log::warn!(
                            "web_preview: no GPU frame within {:?}, falling back to \
                             software paint",
                            FIRST_PAINT_TIMEOUT
                        );
                        cef_browser::force_software_paint();
                        channel = None;
                        browser_started_at = None;
                        if this.update(cx, |this, cx| this.restart_browser(cx)).is_err() {
                            break;
                        }
                        continue;
                    }

                    // Software paint is the last resort, so a browser still not painting
                    // through it is never going to. Saying so beats leaving the tab on
                    // "Waiting for browser..." for the rest of the session.
                    if !painted_once
                        && browser_started_at
                            .is_some_and(|started_at| started_at.elapsed() > PAINT_GIVE_UP_TIMEOUT)
                    {
                        log::error!(
                            "web_preview: browser painted no frame within {:?}; giving up",
                            PAINT_GIVE_UP_TIMEOUT
                        );
                        channel = None;
                        browser_started_at = None;
                        if this
                            .update(cx, |this, cx| this.report_paint_failure(cx))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }

                    let state_version = current_channel.state_version();
                    let state_changed = state_version != synced_state_version;
                    if !state_changed && !has_frame {
                        continue;
                    }
                    synced_state_version = state_version;

                    let should_continue = this.update(cx, |this, cx| {
                        let old_title = this.title.clone();
                        let old_loading = this.loading;

                        this.sync_browser_state();

                        if this.title != old_title || this.loading != old_loading {
                            cx.emit(ItemEvent::UpdateTab);
                        }
                        cx.notify();
                    });
                    if should_continue.is_err() {
                        break;
                    }
                }
                #[cfg(not(target_os = "macos"))]
                if this.update(cx, |_this, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });

        // Redraw when the framework finishes downloading so the tab moves on from the
        // install prompt on its own.
        #[cfg(target_os = "macos")]
        let cef_installer = cef_install::CefInstaller::global(cx);
        #[cfg(target_os = "macos")]
        let installer_subscription = cx.observe(&cef_installer, |_this, _installer, cx| cx.notify());

        Self {
            focus_handle,
            browser_focus,
            url_editor,
            url: String::from("about:blank"),
            title: String::from("Web Preview"),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            last_bounds: None,
            scale_factor: window.scale_factor(),
            #[cfg(target_os = "macos")]
            pressed_buttons: 0,
            #[cfg(target_os = "macos")]
            last_modifiers: gpui::Modifiers::default(),
            _poll_task: Some(poll_task),
            _subscriptions: {
                let mut subscriptions = vec![
                    submit_subscription,
                    focus_redirect,
                    browser_focus_in,
                    browser_focus_out,
                    window_activation,
                ];
                #[cfg(target_os = "macos")]
                subscriptions.push(installer_subscription);
                subscriptions
            },
            #[cfg(target_os = "macos")]
            browser: None,
            #[cfg(target_os = "macos")]
            latest_frame: None,
            hidden: false,
            error: None,
            #[cfg(target_os = "macos")]
            cef_installer,
        }
    }

    /// A tab that starts out on `url` instead of blank. The browser is only created on the
    /// first layout, and `ensure_browser` navigates to `self.url` when it is, so there is
    /// nothing to drive here beyond seeding the state.
    pub fn new_with_url(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(window, cx);
        this.url_editor.update(cx, |editor, cx| {
            editor.set_text(url.as_str(), window, cx);
        });
        this.url = url;
        this
    }

    /// Offers to download Chromium, and reports how that download is going. Shown instead
    /// of the page until the framework is on disk.
    #[cfg(target_os = "macos")]
    fn render_cef_install(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use cef_install::CefInstallState;

        const MEGABYTE: f32 = 1024.0 * 1024.0;

        let state = self.cef_installer.read(cx).state().clone();
        let installer = self.cef_installer.clone();

        let heading = match &state {
            CefInstallState::Missing => "Web Preview needs Chromium",
            CefInstallState::Downloading { .. } => "Downloading Chromium",
            CefInstallState::Extracting => "Installing Chromium",
            CefInstallState::Installed => "Chromium installed",
            CefInstallState::Failed(_) => "Could not install Chromium",
        };

        let detail: SharedString = match &state {
            CefInstallState::Missing => {
                SharedString::from("About 120 MB is downloaded once and kept for later.")
            }
            CefInstallState::Downloading { received, total } => SharedString::from(match total {
                0 => format!("{:.0} MB", *received as f32 / MEGABYTE),
                total => format!(
                    "{:.0} MB of {:.0} MB",
                    *received as f32 / MEGABYTE,
                    *total as f32 / MEGABYTE
                ),
            }),
            CefInstallState::Extracting => SharedString::from("Unpacking the framework…"),
            CefInstallState::Installed => SharedString::from("Open a page to get started."),
            CefInstallState::Failed(error) => error.clone(),
        };

        let progress = match &state {
            CefInstallState::Downloading { received, total } if *total > 0 => {
                Some(*received as f32 / *total as f32)
            }
            CefInstallState::Extracting => Some(1.0),
            _ => None,
        };

        v_flex()
            .flex_1()
            .w_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Label::new(heading).size(ui::LabelSize::Large))
            .child(
                Label::new(detail)
                    .size(ui::LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(progress, |this, fraction| {
                this.child(
                    div()
                        .w(px(280.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction.clamp(0.0, 1.0)))
                                .rounded_full()
                                .bg(cx.theme().colors().element_selected),
                        ),
                )
            })
            .when(
                matches!(
                    state,
                    CefInstallState::Missing | CefInstallState::Failed(_)
                ),
                |this| {
                    this.child(
                        ui::Button::new(
                            "install-cef",
                            match state {
                                CefInstallState::Failed(_) => "Try Again",
                                _ => "Install",
                            },
                        )
                        .style(ui::ButtonStyle::Filled)
                        .on_click(move |_, _window, cx| {
                            installer.update(cx, |installer, cx| installer.install(cx));
                        }),
                    )
                },
            )
            .into_any_element()
    }

    /// Pulls state and any newly painted frame out of CEF.
    #[cfg(target_os = "macos")]
    fn sync_browser_state(&mut self) {
        let Some(ref browser) = self.browser else {
            return;
        };

        if let Some(state) = browser.current_state() {
            self.url = state.url;
            if !state.title.is_empty() {
                self.title = state.title;
            }
            self.loading = state.loading;
            self.can_go_back = state.can_go_back;
            self.can_go_forward = state.can_go_forward;
        }

        if let Some(frame) = browser.take_frame() {
            self.latest_frame = Some(frame);
        }
    }

    /// Draws a painted frame at exactly one surface pixel per physical pixel. The
    /// compositor pads its surfaces, so the element is stretched by however much padding
    /// the surface carries and the container clips the overhang away.
    #[cfg(target_os = "macos")]
    /// Places a painted frame at the size it was painted for, which is not necessarily the
    /// size the page occupies now: CEF repaints a frame or two behind a drag-resize, and
    /// scaling those to the current bounds distorts the page's aspect ratio, squashing
    /// text and images until the next paint catches up. Drawn at its own size a stale
    /// frame is instead cropped or leaves a sliver uncovered, both of which read as the
    /// page simply lagging rather than as the page deforming.
    ///
    /// The outer element crops to the painted content because the compositor pads its
    /// textures, and the padding holds whatever the surface was last used for.
    fn render_frame(frame: &cef_browser::BrowserFrame) -> gpui::AnyElement {
        // Device pixels, which is what CEF rasterizes and reports in.
        let surface_width = frame.pixel_buffer.get_width() as f32;
        let surface_height = frame.pixel_buffer.get_height() as f32;
        let scale = if frame.scale_factor > 0. {
            frame.scale_factor
        } else {
            1.
        };

        div()
            .absolute()
            .left_0()
            .top_0()
            .w(px(frame.content_width as f32 / scale))
            .h(px(frame.content_height as f32 / scale))
            .overflow_hidden()
            .child(
                gpui::surface(frame.pixel_buffer.clone())
                    .object_fit(gpui::ObjectFit::Fill)
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(px(surface_width / scale))
                    .h(px(surface_height / scale)),
            )
            .into_any_element()
    }

    /// Drops the browser so the next layout builds a fresh one, keeping the page the old
    /// one was showing.
    #[cfg(target_os = "macos")]
    fn restart_browser(&mut self, cx: &mut Context<Self>) {
        self.browser = None;
        self.latest_frame = None;
        cx.notify();
    }

    /// Replaces the page with a message after Chromium accepted a browser but never
    /// rendered it. The renderer process failing to launch looks exactly like this from
    /// here, and its own diagnostics only reach the log.
    #[cfg(target_os = "macos")]
    fn report_paint_failure(&mut self, cx: &mut Context<Self>) {
        self.browser = None;
        self.latest_frame = None;
        self.error = Some(PreviewError {
            headline: "The browser never rendered a frame".into(),
            detail: format!(
                "Chromium started but painted nothing within {} seconds, which usually \
                 means its renderer process failed to launch. The Zed log has the \
                 web_preview entries with the reason.",
                PAINT_GIVE_UP_TIMEOUT.as_secs()
            )
            .into(),
        });
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    fn ensure_browser(&mut self, bounds: Bounds<Pixels>) {
        if let Some(browser) = &self.browser {
            let size_changed = self.last_bounds.is_none_or(|last| {
                let dw: f32 = (last.size.width - bounds.size.width).into();
                let dh: f32 = (last.size.height - bounds.size.height).into();
                dw.abs() > 0.5 || dh.abs() > 0.5
            });
            if size_changed {
                let width: f32 = bounds.size.width.into();
                let height: f32 = bounds.size.height.into();
                browser.resize(width as i32, height as i32);
            }
            // A resize can also move the page across displays, and the scale factor is
            // what decides the zoom the page is rasterized at.
            browser.set_scale_factor(self.scale_factor);
            // Tracked even when only the origin moved: mouse positions are translated
            // against this, so a stale origin offsets every click.
            self.last_bounds = Some(bounds);
            return;
        }

        if self.error.is_some() {
            return;
        }

        match cef_browser::CefBrowserInstance::new(bounds, self.scale_factor) {
            Ok(instance) => {
                if self.hidden {
                    instance.set_hidden(true);
                }
                // A browser built to replace one that could not paint has to pick the
                // page back up; a first browser starts on the blank page it was created
                // with.
                if self.url != "about:blank" && !self.url.is_empty() {
                    instance.navigate_to(&self.url);
                }
                self.last_bounds = Some(bounds);
                self.browser = Some(instance);
            }
            Err(error) => {
                log::error!("web_preview: {error}");
                self.error = Some(PreviewError {
                    headline: "Chromium Embedded Framework not available".into(),
                    detail: error.to_string().into(),
                });
            }
        }
    }

    /// Page-relative position in logical pixels, which is the unit CEF is sized in. Values
    /// outside the page are passed through unclamped: Chromium uses them during a drag to
    /// auto-scroll the selection.
    #[cfg(target_os = "macos")]
    fn browser_point(&self, position: gpui::Point<Pixels>) -> Option<(i32, i32)> {
        let bounds = self.last_bounds?;
        Some((
            f32::from(position.x - bounds.origin.x).round() as i32,
            f32::from(position.y - bounds.origin.y).round() as i32,
        ))
    }

    #[cfg(target_os = "macos")]
    fn holds(&self, button: MouseButton) -> bool {
        cef_mouse_button(button)
            .is_some_and(|(_, held_flag)| self.pressed_buttons & held_flag != 0)
    }

    #[cfg(target_os = "macos")]
    fn forward_mouse_down(
        &mut self,
        button: MouseButton,
        position: gpui::Point<Pixels>,
        modifiers: &gpui::Modifiers,
        click_count: usize,
        window: &Window,
    ) {
        let Some((cef_button, held_flag)) = cef_mouse_button(button) else { return };
        let Some((x, y)) = self.browser_point(position) else { return };

        // Set before building the flags: Chromium expects the button bit on its own down event.
        self.pressed_buttons |= held_flag;
        let modifiers =
            gpui_modifiers_to_cef(modifiers, window.capslock().on, self.pressed_buttons);

        if let Some(browser) = &self.browser {
            browser.send_mouse_click(
                x,
                y,
                cef_button,
                false,
                click_count.clamp(1, 3) as i32,
                modifiers,
            );
        }
    }

    #[cfg(target_os = "macos")]
    fn forward_mouse_up(
        &mut self,
        button: MouseButton,
        position: gpui::Point<Pixels>,
        modifiers: &gpui::Modifiers,
        click_count: usize,
        window: &Window,
    ) {
        let Some((cef_button, held_flag)) = cef_mouse_button(button) else { return };
        let Some((x, y)) = self.browser_point(position) else { return };

        let modifiers =
            gpui_modifiers_to_cef(modifiers, window.capslock().on, self.pressed_buttons);
        self.pressed_buttons &= !held_flag;

        if let Some(browser) = &self.browser {
            browser.send_mouse_click(
                x,
                y,
                cef_button,
                true,
                click_count.clamp(1, 3) as i32,
                modifiers,
            );
        }
    }

    #[cfg(target_os = "macos")]
    fn forward_mouse_move(
        &self,
        position: gpui::Point<Pixels>,
        modifiers: &gpui::Modifiers,
        mouse_leave: bool,
        window: &Window,
    ) {
        let Some((x, y)) = self.browser_point(position) else { return };
        let modifiers =
            gpui_modifiers_to_cef(modifiers, window.capslock().on, self.pressed_buttons);

        if let Some(browser) = &self.browser {
            browser.send_mouse_move(x, y, modifiers, mouse_leave);
        }
    }

    #[cfg(target_os = "macos")]
    fn release_mouse_capture(&mut self) {
        if self.pressed_buttons == 0 {
            return;
        }
        self.pressed_buttons = 0;
        if let Some(browser) = &self.browser {
            browser.send_capture_lost();
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.browser_focus.focus(window, cx);
        #[cfg(target_os = "macos")]
        self.forward_mouse_down(
            event.button,
            event.position,
            &event.modifiers,
            event.click_count,
            window,
        );
        cx.notify();
    }

    /// Window-level move and up listeners, so a drag that leaves the page keeps selecting and
    /// a release anywhere still reaches the renderer. Mirrors `TerminalElement`.
    ///
    /// Must be called during paint: `Window::on_mouse_event` asserts the paint phase.
    #[cfg(target_os = "macos")]
    fn register_drag_listeners(
        entity: gpui::WeakEntity<Self>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        window.on_mouse_event({
            let entity = entity.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != gpui::DispatchPhase::Bubble || event.pressed_button.is_none() {
                    return;
                }
                // Inside the page the element-level `on_mouse_move` already reported this;
                // only the outside-the-page half of a drag is this listener's job.
                if cx.has_active_drag() || bounds.contains(&event.position) {
                    return;
                }
                entity
                    .update(cx, |this, _cx| {
                        if this.pressed_buttons != 0 {
                            this.forward_mouse_move(
                                event.position,
                                &event.modifiers,
                                false,
                                window,
                            );
                        }
                    })
                    .ok();
            }
        });

        window.on_mouse_event({
            move |event: &MouseUpEvent, phase, window, cx| {
                if phase != gpui::DispatchPhase::Bubble {
                    return;
                }
                entity
                    .update(cx, |this, _cx| {
                        // `holds` keeps a click that began elsewhere from injecting a
                        // phantom release into the page.
                        if this.holds(event.button) {
                            this.forward_mouse_up(
                                event.button,
                                event.position,
                                &event.modifiers,
                                event.click_count,
                                window,
                            );
                        }
                    })
                    .ok();
            }
        });
    }

    #[cfg(target_os = "macos")]
    fn frame_command(&self, command: cef_browser::FrameCommand) {
        if let Some(browser) = &self.browser {
            browser.frame_command(command);
        }
    }

    /// Translates a GPUI keystroke into the CEF key events Chromium expects, for both press
    /// and release.
    #[cfg(target_os = "macos")]
    fn forward_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        is_held: bool,
        pressed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        use cef_browser::event_flags as flag;

        let Some(browser) = &self.browser else { return };
        let Some(key) = KeyCodes::for_key(&keystroke.key) else { return };

        let mut modifiers =
            gpui_modifiers_to_cef(&keystroke.modifiers, window.capslock().on, self.pressed_buttons);
        // `parse_keystroke` folds shift into the key name for uppercase letters and for
        // shifted punctuation, clearing the modifier; put it back or the page sees ":" typed
        // with no shift held.
        if key.implies_shift {
            modifiers |= flag::SHIFT_DOWN;
        }
        if key.is_keypad {
            modifiers |= flag::IS_KEY_PAD;
        }
        if is_held {
            modifiers |= flag::IS_REPEAT;
        }

        // Cmd and Alt combinations are menu equivalents rather than text; without this
        // Chromium inserts the character and beeps.
        let is_system_key = (keystroke.modifiers.platform || keystroke.modifiers.alt) as i32;

        let character = key.character.or_else(|| {
            keystroke
                .key_char
                .as_ref()
                .and_then(|key_char| key_char.chars().next())
                .filter(|ch| !ch.is_control())
                .and_then(|ch| u16::try_from(ch as u32).ok())
        });

        let event = |type_: cef::KeyEventType, windows_key_code: i32| cef::KeyEvent {
            type_,
            windows_key_code,
            native_key_code: key.native,
            modifiers,
            is_system_key,
            character: character.unwrap_or(0),
            unmodified_character: character.unwrap_or(0),
            ..Default::default()
        };

        if !pressed {
            browser.send_key(event(cef::KeyEventType::KEYUP, key.windows));
            return;
        }

        browser.send_key(event(cef::KeyEventType::RAWKEYDOWN, key.windows));

        // Plain text keys are left to the macOS input system. Not consuming the event here is
        // what lets `handle_key_event` hand the NSEvent to the input context, which composes
        // dead keys and IME candidates and delivers the finished text through
        // `WebPreviewInputHandler` as an `ime_commit_text`. Emitting a CHAR as well would
        // insert every character twice.
        //
        // Keys with a fixed `character` (enter, tab, space, backspace) are excluded: routing
        // space through the IME would insert a space instead of scrolling the page.
        let goes_through_ime = key.character.is_none()
            && character.is_some()
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.control;
        if goes_through_ime {
            return;
        }

        // A CHAR event while Cmd or Ctrl is held is interpreted as literal text, so cmd-a
        // would type an "a" as well as selecting.
        if !keystroke.modifiers.platform
            && !keystroke.modifiers.control
            && let Some(character) = character
        {
            browser.send_key(event(cef::KeyEventType::CHAR, character as i32));
        }

        cx.stop_propagation();
    }

    /// Bare modifier presses and releases, so a page that keys off `shiftKey`/`altKey` sees
    /// the real state instead of whatever it was at the last character.
    #[cfg(target_os = "macos")]
    fn forward_modifiers(&mut self, event: &gpui::ModifiersChangedEvent, window: &Window) {
        let previous = std::mem::replace(&mut self.last_modifiers, event.modifiers);
        let Some(browser) = &self.browser else { return };

        let changes = [
            ("shift", previous.shift, event.modifiers.shift),
            ("control", previous.control, event.modifiers.control),
            ("alt", previous.alt, event.modifiers.alt),
            ("platform", previous.platform, event.modifiers.platform),
        ];

        for (name, was_down, is_down) in changes {
            if was_down == is_down {
                continue;
            }
            let Some(key) = KeyCodes::for_key(name) else { continue };
            browser.send_key(cef::KeyEvent {
                type_: if is_down {
                    cef::KeyEventType::RAWKEYDOWN
                } else {
                    cef::KeyEventType::KEYUP
                },
                windows_key_code: key.windows,
                native_key_code: key.native,
                modifiers: gpui_modifiers_to_cef(
                    &event.modifiers,
                    window.capslock().on,
                    self.pressed_buttons,
                ),
                ..Default::default()
            });
        }
    }

    fn set_browser_hidden(&mut self, hidden: bool) {
        if self.hidden == hidden {
            return;
        }
        self.hidden = hidden;
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.set_hidden(hidden);
        }
    }

    fn submit_url(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_editor.read(cx).text(cx);
        if !url.is_empty() {
            log::info!("web_preview: navigating to {url}");
            #[cfg(target_os = "macos")]
            if let Some(ref browser) = self.browser {
                browser.navigate_to(&url);
            }
            runtime_discovery::write_runtime_discovery(&url, None);
        }
        cx.notify();
    }

    fn handle_reload(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.reload();
        }
        cx.notify();
    }

    fn handle_go_back(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.go_back();
        }
        cx.notify();
    }

    fn handle_go_forward(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.go_forward();
        }
        cx.notify();
    }

    fn handle_open_devtools(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.open_devtools();
        }
        cx.notify();
    }

    fn handle_stop(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(ref browser) = self.browser {
            browser.stop();
        }
        cx.notify();
    }

}

impl EventEmitter<ItemEvent> for WebPreviewView {}

impl Focusable for WebPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WebPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_go_back = self.can_go_back;
        let can_go_forward = self.can_go_forward;
        let loading = self.loading;

        #[cfg(target_os = "macos")]
        let install_prompt = (!self.cef_installer.read(cx).is_installed())
            .then(|| self.render_cef_install(cx));
        #[cfg(not(target_os = "macos"))]
        let install_prompt: Option<gpui::AnyElement> = None;

        // The browser is an item inside a pane, so the pane's tab bar is the topmost strip here
        // and this row is a secondary strip belonging to the content, like the editor's
        // breadcrumb toolbar.
        let toolbar = HeaderBar::new("web-preview-toolbar")
            .level(HeaderBarLevel::Content)
            .start_children(vec![
                IconButton::new("back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_back)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_go_back(window, cx);
                    }))
                    .into_any_element(),
                IconButton::new("forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_forward)
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_go_forward(window, cx);
                    }))
                    .into_any_element(),
                if loading {
                    IconButton::new("stop", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Stop"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_stop(window, cx);
                        }))
                        .into_any_element()
                } else {
                    IconButton::new("reload", IconName::RotateCw)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Reload"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_reload(window, cx);
                        }))
                        .into_any_element()
                },
            ])
            .child(
                // Mirrors `ui_input::InputField`'s tokens so the URL bar reads like every other
                // input in the app. It cannot use `InputField` directly, because that builds its
                // own editor and this view owns and drives `url_editor`.
                h_flex()
                    .id("url-bar")
                    .flex_1()
                    .min_w_0()
                    .h(DynamicSpacing::Base24.px(cx))
                    .px_1()
                    .items_center()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .when(
                        self.url_editor.focus_handle(cx).contains_focused(window, cx),
                        |this| this.border_color(cx.theme().colors().border_focused),
                    )
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.submit_url(window, cx);
                    }))
                    .child(self.url_editor.clone()),
            )
            .end_children(vec![
                IconButton::new("go", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Navigate"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_url(window, cx);
                    }))
                    .into_any_element(),
                IconButton::new("devtools", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open DevTools"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_open_devtools(window, cx);
                    }))
                    .into_any_element(),
            ])
            .when(loading, |this| {
                this.end_child(
                    Label::new("Loading…")
                        .size(ui::LabelSize::XSmall)
                        .color(Color::Muted),
                )
            });

        v_flex()
            .id("web-preview-view")
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Reload, window, cx| {
                this.handle_reload(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoBack, window, cx| {
                this.handle_go_back(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoForward, window, cx| {
                this.handle_go_forward(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusUrlBar, window, cx| {
                this.url_editor.focus_handle(cx).focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenDevTools, window, cx| {
                this.handle_open_devtools(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SubmitUrl, window, cx| {
                this.submit_url(window, cx);
            }))
            .child(toolbar)
            .child(if let Some(prompt) = install_prompt {
                prompt
            } else if let Some(ref error) = self.error {
                v_flex()
                    .flex_1()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Label::new(error.headline.clone())
                            .size(ui::LabelSize::Large)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(error.detail.clone())
                            .size(ui::LabelSize::Small)
                            .color(Color::Disabled),
                    )
                    .into_any_element()
            } else {
                {
                    // Use a canvas to measure real size, then render the frame
                    let entity = cx.entity().downgrade();
                    #[cfg(target_os = "macos")]
                    let paint_entity = cx.entity().downgrade();
                    #[cfg(target_os = "macos")]
                    let input_focus = self.browser_focus.clone();
                    #[cfg(target_os = "macos")]
                    let frame = self.latest_frame.clone();
                    #[cfg(not(target_os = "macos"))]
                    let frame: Option<()> = None;
                    #[cfg(target_os = "macos")]
                    let frame_element = frame.as_ref().map(Self::render_frame);
                    #[cfg(not(target_os = "macos"))]
                    let frame_element: Option<gpui::AnyElement> = None;

                    #[cfg(target_os = "macos")]
                    let browser_cursor = self.browser.as_ref()
                        .and_then(|browser| browser.current_cursor())
                        .unwrap_or(CursorStyle::Arrow);
                    #[cfg(not(target_os = "macos"))]
                    let browser_cursor = CursorStyle::Arrow;

                    let mut content = div()
                        .id("browser-content")
                        .flex_1()
                        .w_full()
                        .overflow_hidden()
                        .track_focus(&self.browser_focus);
                    content.style().mouse_cursor = Some(browser_cursor);
                    content
                        .key_context("WebPreview")
                        .on_action(cx.listener(|this, _: &SelectAll, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::SelectAll);
                        }))
                        .on_action(cx.listener(|this, _: &Copy, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::Copy);
                        }))
                        .on_action(cx.listener(|this, _: &Cut, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::Cut);
                        }))
                        .on_action(cx.listener(|this, _: &Paste, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::Paste);
                        }))
                        .on_action(cx.listener(|this, _: &Undo, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::Undo);
                        }))
                        .on_action(cx.listener(|this, _: &Redo, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            this.frame_command(cef_browser::FrameCommand::Redo);
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            #[cfg(target_os = "macos")]
                            this.forward_key(&event.keystroke, event.is_held, true, window, cx);
                        }))
                        .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, window, cx| {
                            #[cfg(target_os = "macos")]
                            this.forward_key(&event.keystroke, false, false, window, cx);
                        }))
                        .on_modifiers_changed(cx.listener(
                            |this, event: &gpui::ModifiersChangedEvent, window, _cx| {
                                #[cfg(target_os = "macos")]
                                this.forward_modifiers(event, window);
                            },
                        ))
                        .child(
                            canvas(
                                move |bounds, window: &mut Window, cx: &mut App| {
                                    #[cfg(target_os = "macos")]
                                    entity
                                        .update(cx, |this, _cx| {
                                            this.scale_factor = window.scale_factor();
                                            this.ensure_browser(bounds);
                                        })
                                        .ok();
                                    bounds
                                },
                                move |_bounds, bounds: Bounds<Pixels>, _window, _cx| {
                                    #[cfg(target_os = "macos")]
                                    {
                                        Self::register_drag_listeners(
                                            paint_entity.clone(),
                                            bounds,
                                            _window,
                                        );
                                        // Re-registered every frame: input handlers live for
                                        // one frame only.
                                        _window.handle_input(
                                            &input_focus,
                                            WebPreviewInputHandler {
                                                view: paint_entity,
                                                bounds,
                                                marked_range: None,
                                            },
                                            _cx,
                                        );
                                    }
                                },
                            )
                            .absolute()
                            .size_full(),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.handle_mouse_down(event, window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.handle_mouse_down(event, window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.handle_mouse_down(event, window, cx);
                            }),
                        )
                        // Mouse-up is handled by a window-level listener registered during
                        // paint, because `Interactivity::on_mouse_up` only fires while the
                        // hitbox is hovered — releasing outside the page would otherwise leave
                        // the renderer believing the button is still down.
                        .on_mouse_move(
                            cx.listener(|this, event: &MouseMoveEvent, window, _cx| {
                                #[cfg(target_os = "macos")]
                                this.forward_mouse_move(
                                    event.position,
                                    &event.modifiers,
                                    false,
                                    window,
                                );
                            }),
                        )
                        .on_mouse_exit(
                            cx.listener(|this, event: &gpui::MouseExitEvent, window, _cx| {
                                #[cfg(target_os = "macos")]
                                // Mid-drag the pointer is still "in" the page as far as the
                                // renderer is concerned; telling it otherwise cancels the drag.
                                if this.pressed_buttons == 0 {
                                    this.forward_mouse_move(
                                        event.position,
                                        &event.modifiers,
                                        true,
                                        window,
                                    );
                                }
                            }),
                        )
                        .on_scroll_wheel(
                            cx.listener(|this, event: &ScrollWheelEvent, window, _cx| {
                                #[cfg(target_os = "macos")]
                                if let (Some(browser), Some((x, y))) =
                                    (&this.browser, this.browser_point(event.position))
                                {
                                    let delta_x = f32::from(event.delta.pixel_delta(px(20.0)).x) as i32;
                                    let delta_y = f32::from(event.delta.pixel_delta(px(20.0)).y) as i32;
                                    let mut modifiers = gpui_modifiers_to_cef(
                                        &event.modifiers,
                                        window.capslock().on,
                                        this.pressed_buttons,
                                    );
                                    // Trackpads report pixel deltas; without this Chromium
                                    // steps by lines and the scroll feels notched.
                                    if matches!(event.delta, gpui::ScrollDelta::Pixels(_)) {
                                        modifiers |=
                                            cef_browser::event_flags::PRECISION_SCROLLING_DELTA;
                                    }
                                    browser.send_mouse_wheel(x, y, delta_x, delta_y, modifiers);
                                }
                            }),
                        )
                        .when_some(frame_element, |this, element| this.child(element))
                        .when(frame.is_none() && self.error.is_none(), |this| {
                            this.child(
                                div()
                                    .size_full()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Label::new("Waiting for browser...")
                                            .size(ui::LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                        })
                        .into_any_element()
                }
            })
    }
}

impl Item for WebPreviewView {
    type Event = ItemEvent;

    fn content_kind(&self, _cx: &App) -> Option<workspace::ContentKind> {
        Some(Self::content_kind())
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.title.is_empty() || self.title == "Web Preview" {
            SharedString::from("Web Preview")
        } else {
            SharedString::from(self.title.clone())
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ToolWeb).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(self.url.clone()))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn activated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.set_browser_hidden(false);
    }

    fn deactivated(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.set_browser_hidden(true);
        #[cfg(target_os = "macos")]
        {
            self.release_mouse_capture();
            if let Some(browser) = &self.browser {
                browser.set_focus(false);
            }
        }
    }
}

fn browse_request_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    std::path::PathBuf::from(format!(
        "{}/Library/Application Support/Zed Workstation/WebPreview/browse-request.txt",
        home
    ))
}

#[cfg(target_os = "macos")]
fn gpui_modifiers_to_cef(modifiers: &gpui::Modifiers, capslock: bool, pressed_buttons: u32) -> u32 {
    use cef_browser::event_flags as flag;

    let mut flags = pressed_buttons;
    if modifiers.shift { flags |= flag::SHIFT_DOWN; }
    if modifiers.control { flags |= flag::CONTROL_DOWN; }
    if modifiers.alt { flags |= flag::ALT_DOWN; }
    if modifiers.platform { flags |= flag::COMMAND_DOWN; }
    if capslock { flags |= flag::CAPS_LOCK_ON; }
    flags
}

/// The CEF button type and the `event_flags` bit that says it is held.
#[cfg(target_os = "macos")]
fn cef_mouse_button(button: MouseButton) -> Option<(cef::MouseButtonType, u32)> {
    use cef_browser::event_flags as flag;

    match button {
        MouseButton::Left => Some((cef::MouseButtonType::LEFT, flag::LEFT_MOUSE_BUTTON)),
        MouseButton::Middle => Some((cef::MouseButtonType::MIDDLE, flag::MIDDLE_MOUSE_BUTTON)),
        MouseButton::Right => Some((cef::MouseButtonType::RIGHT, flag::RIGHT_MOUSE_BUTTON)),
        // Chromium has no mouse event for these; they are navigation gestures.
        MouseButton::Navigate(_) => None,
    }
}

/// Bridges the macOS input system to CEF's IME API, so dead keys (`´`+`a` → `á`), option
/// characters and CJK composition resolve the way they do in any other app. Without this the
/// page would receive the literal accent followed by the letter.
///
/// Implements [`gpui::InputHandler`] rather than [`gpui::EntityInputHandler`] because only the
/// former can turn off press-and-hold, which a browser needs so a held key repeats instead of
/// opening the macOS accent popover.
#[cfg(target_os = "macos")]
struct WebPreviewInputHandler {
    view: gpui::WeakEntity<WebPreviewView>,
    /// Page bounds in window coordinates, for placing the candidate window.
    bounds: Bounds<Pixels>,
    /// Composition range we reported to the system. CEF cannot be queried for it, so it is
    /// tracked here.
    marked_range: Option<std::ops::Range<usize>>,
}

#[cfg(target_os = "macos")]
impl WebPreviewInputHandler {
    fn with_browser<R>(
        &self,
        cx: &mut App,
        f: impl FnOnce(&cef_browser::CefBrowserInstance) -> R,
    ) -> Option<R> {
        self.view
            .read_with(cx, |view, _cx| view.browser.as_ref().map(f))
            .ok()
            .flatten()
    }

    fn state(&self, cx: &mut App) -> Option<cef_browser::BrowserState> {
        self.with_browser(cx, |browser| browser.current_state()).flatten()
    }
}

#[cfg(target_os = "macos")]
impl gpui::InputHandler for WebPreviewInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::UTF16Selection> {
        // A selection must always be reported, even an empty one, or macOS decides there is
        // no text context and refuses to start a composition.
        let range = self
            .state(cx)
            .and_then(|state| state.selected_range)
            .unwrap_or(0..0);
        Some(gpui::UTF16Selection { range, reversed: false })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        self.marked_range.clone()
    }

    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        // The selection is the only span of the document CEF reports, so any other range is
        // genuinely unknown — answering it with the selection would misinform the IME.
        let state = self.state(cx)?;
        (state.selected_range? == range_utf16 && !state.selected_text.is_empty())
            .then_some(state.selected_text)
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.marked_range = None;
        let text = text.to_string();
        self.with_browser(cx, |browser| {
            let cursor = text.chars().count() as i32;
            browser.ime_commit_text(text, cursor);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let length = new_text.encode_utf16().count();
        self.marked_range = Some(0..length);
        let selection = new_selected_range.unwrap_or(0..length);
        let text = new_text.to_string();
        self.with_browser(cx, |browser| {
            browser.ime_set_composition(text, selection.start as u32, selection.end as u32);
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.marked_range = None;
        self.with_browser(cx, |browser| browser.ime_cancel_composition());
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        // Chromium reports character rectangles relative to the page; the candidate window
        // wants them in window coordinates.
        let caret = self
            .state(cx)
            .and_then(|state| state.composition_bounds.first().copied());
        Some(match caret {
            Some(caret) => Bounds {
                origin: self.bounds.origin + caret.origin,
                size: caret.size,
            },
            None => Bounds {
                origin: self.bounds.origin,
                size: gpui::size(px(0.0), self.bounds.size.height.min(px(20.0))),
            },
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    /// Browsers repeat a held key rather than offering accented variants.
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

/// What Chromium needs to identify a key, derived from a GPUI key name.
///
/// `native` is the macOS virtual keycode. GPUI reads it from the NSEvent but does not carry it
/// on `Keystroke` (`gpui_macos/src/events.rs:434-467`), so it is reconstructed from a US layout
/// here. That makes `KeyboardEvent.code` approximate on other layouts; `key` and text input
/// are unaffected because they come from `key_char`.
#[cfg(target_os = "macos")]
struct KeyCodes {
    windows: i32,
    native: i32,
    /// Shift that `parse_keystroke` folded into the key name instead of the modifier.
    implies_shift: bool,
    is_keypad: bool,
    /// Fixed character for keys whose `key_char` GPUI leaves empty or unhelpful.
    character: Option<u16>,
}

#[cfg(target_os = "macos")]
impl KeyCodes {
    fn new(windows: i32, native: i32) -> Self {
        Self { windows, native, implies_shift: false, is_keypad: false, character: None }
    }

    fn shifted(windows: i32, native: i32) -> Self {
        Self { implies_shift: true, ..Self::new(windows, native) }
    }

    fn keypad(windows: i32, native: i32) -> Self {
        Self { is_keypad: true, ..Self::new(windows, native) }
    }

    fn with_char(windows: i32, native: i32, character: u16) -> Self {
        Self { character: Some(character), ..Self::new(windows, native) }
    }

    fn for_key(key: &str) -> Option<Self> {
        // Native keycodes for the US layout, keyed by the unshifted character.
        fn native_for(ch: char) -> Option<i32> {
            Some(match ch {
                'a' => 0x00, 's' => 0x01, 'd' => 0x02, 'f' => 0x03,
                'h' => 0x04, 'g' => 0x05, 'z' => 0x06, 'x' => 0x07,
                'c' => 0x08, 'v' => 0x09, 'b' => 0x0B, 'q' => 0x0C,
                'w' => 0x0D, 'e' => 0x0E, 'r' => 0x0F, 'y' => 0x10,
                't' => 0x11, '1' => 0x12, '2' => 0x13, '3' => 0x14,
                '4' => 0x15, '6' => 0x16, '5' => 0x17, '=' => 0x18,
                '9' => 0x19, '7' => 0x1A, '-' => 0x1B, '8' => 0x1C,
                '0' => 0x1D, ']' => 0x1E, 'o' => 0x1F, 'u' => 0x20,
                '[' => 0x21, 'i' => 0x22, 'p' => 0x23, 'l' => 0x25,
                'j' => 0x26, '\'' => 0x27, 'k' => 0x28, ';' => 0x29,
                '\\' => 0x2A, ',' => 0x2B, '/' => 0x2C, 'n' => 0x2D,
                'm' => 0x2E, '.' => 0x2F, '`' => 0x32,
                _ => return None,
            })
        }

        // `f1`..`f20`. Note the native codes are not contiguous.
        if let Some(number) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
            && (1..=20).contains(&number)
        {
            const NATIVE: [i32; 20] = [
                0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D,
                0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A,
            ];
            let native = *NATIVE.get(number as usize - 1)?;
            return Some(Self::new(0x70 + number as i32 - 1, native));
        }

        if let Some(digit) = key.strip_prefix("numpad").and_then(|n| n.parse::<u32>().ok())
            && digit <= 9
        {
            const NATIVE: [i32; 10] = [
                0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C,
            ];
            let native = *NATIVE.get(digit as usize)?;
            return Some(Self::keypad(0x60 + digit as i32, native));
        }

        // Named keys. `character` is set where Chromium expects a control character in the
        // CHAR event; enter/tab/space are handled here rather than as text so that space
        // scrolls the page instead of inserting a space.
        let named = match key {
            "enter" => Some(Self::with_char(0x0D, 0x24, 0x0D)),
            "tab" => Some(Self::with_char(0x09, 0x30, 0x09)),
            "space" => Some(Self::with_char(0x20, 0x31, 0x20)),
            "backspace" => Some(Self::with_char(0x08, 0x33, 0x08)),
            "escape" => Some(Self::new(0x1B, 0x35)),
            "left" => Some(Self::new(0x25, 0x7B)),
            "up" => Some(Self::new(0x26, 0x7E)),
            "right" => Some(Self::new(0x27, 0x7C)),
            "down" => Some(Self::new(0x28, 0x7D)),
            "delete" => Some(Self::new(0x2E, 0x75)),
            "insert" => Some(Self::new(0x2D, 0x72)),
            "home" => Some(Self::new(0x24, 0x73)),
            "end" => Some(Self::new(0x23, 0x77)),
            "pageup" => Some(Self::new(0x21, 0x74)),
            "pagedown" => Some(Self::new(0x22, 0x79)),
            "shift" => Some(Self::new(0x10, 0x38)),
            "control" => Some(Self::new(0x11, 0x3B)),
            "alt" => Some(Self::new(0x12, 0x3A)),
            "platform" => Some(Self::new(0x5B, 0x37)),
            "capslock" => Some(Self::new(0x14, 0x39)),
            _ => None,
        };
        if named.is_some() {
            return named;
        }

        let mut chars = key.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }

        // Uppercase letters and shifted punctuation reach us with the shift modifier already
        // cleared, so the base key is looked up and shift re-implied.
        if ch.is_ascii_uppercase() {
            let native = native_for(ch.to_ascii_lowercase())?;
            return Some(Self::shifted(ch as i32, native));
        }
        if ch.is_ascii_lowercase() {
            return Some(Self::new(ch.to_ascii_uppercase() as i32, native_for(ch)?));
        }
        if ch.is_ascii_digit() {
            return Some(Self::new(ch as i32, native_for(ch)?));
        }

        // Unshifted punctuation uses the VK_OEM_* codes; the shifted forms map back to the
        // same physical key.
        let (windows, base, shifted) = match ch {
            ';' => (0xBA, ';', false),
            ':' => (0xBA, ';', true),
            '=' => (0xBB, '=', false),
            '+' => (0xBB, '=', true),
            ',' => (0xBC, ',', false),
            '<' => (0xBC, ',', true),
            '-' => (0xBD, '-', false),
            '_' => (0xBD, '-', true),
            '.' => (0xBE, '.', false),
            '>' => (0xBE, '.', true),
            '/' => (0xBF, '/', false),
            '?' => (0xBF, '/', true),
            '`' => (0xC0, '`', false),
            '~' => (0xC0, '`', true),
            '[' => (0xDB, '[', false),
            '{' => (0xDB, '[', true),
            '\\' => (0xDC, '\\', false),
            '|' => (0xDC, '\\', true),
            ']' => (0xDD, ']', false),
            '}' => (0xDD, ']', true),
            '\'' => (0xDE, '\'', false),
            '"' => (0xDE, '\'', true),
            '!' => (0x31, '1', true),
            '@' => (0x32, '2', true),
            '#' => (0x33, '3', true),
            '$' => (0x34, '4', true),
            '%' => (0x35, '5', true),
            '^' => (0x36, '6', true),
            '&' => (0x37, '7', true),
            '*' => (0x38, '8', true),
            '(' => (0x39, '9', true),
            ')' => (0x30, '0', true),
            _ => return None,
        };
        let native = native_for(base)?;
        Some(if shifted {
            Self::shifted(windows, native)
        } else {
            Self::new(windows, native)
        })
    }
}
