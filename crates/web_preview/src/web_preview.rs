#[cfg(target_os = "macos")]
mod cef_browser;
mod runtime_discovery;

use editor::Editor;
use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, RenderImage, ScrollWheelEvent, SharedString,
    Styled, Subscription, Task, Window, actions, canvas, div, img, px,
};
use smallvec::SmallVec;
use ui::{
    h_flex, prelude::*, v_flex, Color, Icon, IconButton, IconName, IconSize, Label, Tooltip,
};
use util::ResultExt as _;
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
    ]
);

/// How often the poll loop looks for a URL dropped by the `zed browse` utility.
const BROWSE_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, _cx| {
        workspace.register_action(WebPreviewView::open_action);

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
    _poll_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    #[cfg(target_os = "macos")]
    browser: Option<cef_browser::CefBrowserInstance>,
    #[cfg(target_os = "macos")]
    latest_frame: Option<std::sync::Arc<RenderImage>>,
    /// Frames the renderer has uploaded to the sprite atlas. Held one generation past
    /// use because the atlas entry must outlive the last scene that referenced it.
    current_rendered_frame: Option<std::sync::Arc<RenderImage>>,
    previous_rendered_frame: Option<std::sync::Arc<RenderImage>>,
    /// Whether the pane is showing another item, in which case nothing can see what CEF
    /// paints.
    hidden: bool,
    cef_error: Option<String>,
}

impl WebPreviewView {
    pub fn open_action(
        workspace: &mut Workspace,
        _action: &OpenWebPreview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let view = cx.new(|cx| Self::new(window, cx));
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

        let poll_task = cx.spawn(async move |this, cx| {
            let mut delay = std::time::Duration::from_millis(1);
            let mut last_browse_check = std::time::Instant::now();

            loop {
                cx.background_executor().timer(delay).await;

                // Step 1: Flush input + pump CEF OUTSIDE entity update
                // to avoid re-entrancy crashes
                #[cfg(target_os = "macos")]
                {
                    let queues = this
                        .update(cx, |this, _cx| {
                            this.browser
                                .as_ref()
                                .map(|browser| (browser.input_queue_arc(), browser.handler_arc()))
                        })
                        .ok()
                        .flatten();

                    if let Some((input_queue, handler)) = queues
                        && cef_browser::CefBrowserInstance::flush_queued(&input_queue, &handler)
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

                // Step 2: Sync state inside entity update
                let should_continue = this.update(cx, |this, cx| {
                    #[cfg(target_os = "macos")]
                    {
                        let old_title = this.title.clone();
                        let old_loading = this.loading;

                        let got_frame = this.sync_browser_state();

                        let title_changed = this.title != old_title;
                        let loading_changed = this.loading != old_loading;

                        if title_changed || loading_changed {
                            cx.emit(ItemEvent::UpdateTab);
                        }
                        // Only a new frame or a state change is worth a redraw: this loop
                        // ticks up to 60 times a second, and notifying unconditionally
                        // re-rendered the whole pane on a page that had not moved.
                        if title_changed || loading_changed || got_frame {
                            cx.notify();
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        cx.notify();
                    }
                });
                if should_continue.is_err() {
                    break;
                }
            }
        });

        // Every frame the view showed holds a sprite-atlas texture that only an explicit
        // drop releases, so closing the tab has to hand back the ones still retained.
        cx.on_release_in(window, |this, window, cx| {
            for frame in [
                this.current_rendered_frame.take(),
                this.previous_rendered_frame.take(),
            ]
            .into_iter()
            .flatten()
            {
                cx.drop_image(frame, Some(&mut *window));
            }
        })
        .detach();

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
            _poll_task: Some(poll_task),
            _subscriptions: vec![submit_subscription],
            #[cfg(target_os = "macos")]
            browser: None,
            #[cfg(target_os = "macos")]
            latest_frame: None,
            current_rendered_frame: None,
            previous_rendered_frame: None,
            hidden: false,
            cef_error: None,
        }
    }

    /// Pulls state and any newly painted frame out of CEF. Returns whether a new frame
    /// arrived.
    #[cfg(target_os = "macos")]
    fn sync_browser_state(&mut self) -> bool {
        let Some(ref browser) = self.browser else {
            return false;
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

        let Some(frame_buffer) = browser.take_frame() else {
            return false;
        };
        // CEF paints BGRA, which is the order `RenderImage` expects, so the pixels go
        // straight through without a pass over the buffer.
        let Some(buffer) = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
            frame_buffer.width,
            frame_buffer.height,
            frame_buffer.pixels,
        ) else {
            return false;
        };

        self.latest_frame = Some(std::sync::Arc::new(RenderImage::new(SmallVec::from_elem(
            image::Frame::new(buffer),
            1,
        ))));
        true
    }

    /// Releases the sprite-atlas entry of frames that are two generations old. Dropping
    /// the frame the renderer just drew from would leave the atlas without the texture
    /// the last scene still references.
    fn track_rendered_frame(&mut self, window: &mut Window) {
        #[cfg(target_os = "macos")]
        let latest_frame = self.latest_frame.clone();
        #[cfg(not(target_os = "macos"))]
        let latest_frame: Option<std::sync::Arc<RenderImage>> = None;

        let Some(latest_frame) = latest_frame else {
            return;
        };
        if self
            .current_rendered_frame
            .as_ref()
            .is_some_and(|frame| frame.id == latest_frame.id)
        {
            return;
        }
        if let Some(previous) = self.previous_rendered_frame.take()
            && previous.id != latest_frame.id
        {
            window.drop_image(previous).log_err();
        }
        self.previous_rendered_frame = self.current_rendered_frame.take();
        self.current_rendered_frame = Some(latest_frame);
    }

    #[cfg(target_os = "macos")]
    fn ensure_browser(&mut self, bounds: Bounds<Pixels>) {
        if self.browser.is_some() {
            // Resize if bounds changed
            let bounds_changed = self.last_bounds.map_or(true, |last| {
                let dw: f32 = (last.size.width - bounds.size.width).into();
                let dh: f32 = (last.size.height - bounds.size.height).into();
                dw.abs() > 0.5 || dh.abs() > 0.5
            });
            if bounds_changed {
                if let Some(ref browser) = self.browser {
                    let w: f32 = bounds.size.width.into();
                    let h: f32 = bounds.size.height.into();
                    browser.resize(w as i32, h as i32);
                }
                self.last_bounds = Some(bounds);
            }
            return;
        }

        if self.cef_error.is_some() {
            return;
        }

        match cef_browser::CefBrowserInstance::new(bounds, self.scale_factor) {
            Ok(instance) => {
                if self.hidden {
                    instance.set_hidden(true);
                }
                self.last_bounds = Some(bounds);
                self.browser = Some(instance);
            }
            Err(error) => {
                log::error!("web_preview: {error}");
                self.cef_error = Some(error.to_string());
            }
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
        self.track_rendered_frame(window);

        let can_go_back = self.can_go_back;
        let can_go_forward = self.can_go_forward;
        let loading = self.loading;

        let title_bar_bg = cx.theme().colors().title_bar_background;
        let border = cx.theme().colors().border;

        let toolbar = h_flex()
            .id("web-preview-toolbar")
            .w_full()
            .h(px(36.0))
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .bg(title_bar_bg)
            .border_b_1()
            .border_color(border)
            .when(loading, |this| {
                this.child(
                    Label::new("Loading...")
                        .size(ui::LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(
                IconButton::new("back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_back)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_go_back(window, cx);
                    })),
            )
            .child(
                IconButton::new("forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!can_go_forward)
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_go_forward(window, cx);
                    })),
            )
            .child(if loading {
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
            })
            .child(
                div()
                    .id("url-bar")
                    .flex_1()
                    .h(px(26.0))
                    .px_1()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(border)
                    .flex()
                    .items_center()
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.submit_url(window, cx);
                    }))
                    .child(self.url_editor.clone()),
            )
            .child(
                IconButton::new("go", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Navigate"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_url(window, cx);
                    })),
            )
            .child(
                IconButton::new("devtools", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open DevTools"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_open_devtools(window, cx);
                    })),
            );

        v_flex()
            .id("web-preview-view")
            .size_full()
            .track_focus(&self.focus_handle(cx))
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
            .child(if let Some(ref error) = self.cef_error {
                v_flex()
                    .flex_1()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Label::new("Chromium Embedded Framework not available")
                            .size(ui::LabelSize::Large)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(SharedString::from(error.clone()))
                            .size(ui::LabelSize::Small)
                            .color(Color::Disabled),
                    )
                    .into_any_element()
            } else {
                {
                    // Use a canvas to measure real size, then render the frame
                    let entity = cx.entity().downgrade();
                    let frame_image = self.current_rendered_frame.clone();

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
                        .track_focus(&self.browser_focus);
                    content.style().mouse_cursor = Some(browser_cursor);
                    content
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, _cx| {
                            #[cfg(target_os = "macos")]
                            if let Some(browser) = &this.browser {
                                let modifiers = gpui_modifiers_to_cef(&event.keystroke.modifiers);
                                let key = &event.keystroke.key;

                                // (windows_vk, mac_native_keycode) for special keys
                                let special = match key.as_str() {
                                    "enter"     => Some((0x0D, 0x24)),
                                    "tab"       => Some((0x09, 0x30)),
                                    "backspace" => Some((0x08, 0x33)),
                                    "escape"    => Some((0x1B, 0x35)),
                                    "left"      => Some((0x25, 0x7B)),
                                    "up"        => Some((0x26, 0x7E)),
                                    "right"     => Some((0x27, 0x7C)),
                                    "down"      => Some((0x28, 0x7D)),
                                    "delete"    => Some((0x2E, 0x75)),
                                    "home"      => Some((0x24, 0x73)),
                                    "end"       => Some((0x23, 0x77)),
                                    "pageup"    => Some((0x21, 0x74)),
                                    "pagedown"  => Some((0x22, 0x79)),
                                    "space"     => Some((0x20, 0x31)),
                                    "f1"  => Some((0x70, 0x7A)),
                                    "f2"  => Some((0x71, 0x78)),
                                    "f3"  => Some((0x72, 0x63)),
                                    "f4"  => Some((0x73, 0x76)),
                                    "f5"  => Some((0x74, 0x60)),
                                    _ => None,
                                };

                                if let Some((vk, native)) = special {
                                    browser.send_key(cef::KeyEvent {
                                        type_: cef::KeyEventType::RAWKEYDOWN,
                                        windows_key_code: vk,
                                        native_key_code: native,
                                        modifiers,
                                        ..Default::default()
                                    });
                                    let char_code = match vk {
                                        0x0D => Some(0x0D_u16),
                                        0x09 => Some(0x09),
                                        0x08 => Some(0x08),
                                        0x20 => Some(0x20),
                                        _ => None,
                                    };
                                    if let Some(ch) = char_code {
                                        browser.send_key(cef::KeyEvent {
                                            type_: cef::KeyEventType::CHAR,
                                            windows_key_code: ch as i32,
                                            native_key_code: native,
                                            character: ch,
                                            unmodified_character: ch,
                                            modifiers,
                                            ..Default::default()
                                        });
                                    }
                                    browser.send_key(cef::KeyEvent {
                                        type_: cef::KeyEventType::KEYUP,
                                        windows_key_code: vk,
                                        native_key_code: native,
                                        modifiers,
                                        ..Default::default()
                                    });
                                    return;
                                }

                                // Printable characters
                                if let Some(key_char) = &event.keystroke.key_char {
                                    for ch in key_char.chars() {
                                        let vk = if ch.is_ascii_alphabetic() {
                                            ch.to_ascii_uppercase() as i32
                                        } else {
                                            ch as i32
                                        };
                                        // macOS native keycode for letters: a=0x00, s=0x01, d=0x02...
                                        let native = mac_native_keycode_for_char(ch);
                                        browser.send_key(cef::KeyEvent {
                                            type_: cef::KeyEventType::RAWKEYDOWN,
                                            windows_key_code: vk,
                                            native_key_code: native,
                                            character: ch as u16,
                                            unmodified_character: ch as u16,
                                            modifiers,
                                            ..Default::default()
                                        });
                                        browser.send_key(cef::KeyEvent {
                                            type_: cef::KeyEventType::CHAR,
                                            windows_key_code: ch as i32,
                                            native_key_code: native,
                                            character: ch as u16,
                                            unmodified_character: ch as u16,
                                            modifiers,
                                            ..Default::default()
                                        });
                                        browser.send_key(cef::KeyEvent {
                                            type_: cef::KeyEventType::KEYUP,
                                            windows_key_code: vk,
                                            native_key_code: native,
                                            character: ch as u16,
                                            unmodified_character: ch as u16,
                                            modifiers,
                                            ..Default::default()
                                        });
                                    }
                                }
                            }
                        }))
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
                                },
                                |_bounds, _: (), _window, _cx| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.browser_focus.focus(window, cx);
                                #[cfg(target_os = "macos")]
                                if let (Some(browser), Some(bounds)) = (&this.browser, &this.last_bounds) {
                                    let x = f32::from(event.position.x - bounds.origin.x) as i32;
                                    let y = f32::from(event.position.y - bounds.origin.y) as i32;
                                    browser.set_focus(true);
                                    browser.send_mouse_click(x, y, cef::MouseButtonType::LEFT, false, 1);
                                }
                                cx.notify();
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                                #[cfg(target_os = "macos")]
                                if let (Some(browser), Some(bounds)) = (&this.browser, &this.last_bounds) {
                                    let x = f32::from(event.position.x - bounds.origin.x) as i32;
                                    let y = f32::from(event.position.y - bounds.origin.y) as i32;
                                    browser.send_mouse_click(x, y, cef::MouseButtonType::LEFT, true, 1);
                                }
                            }),
                        )
                        .on_mouse_move(
                            cx.listener(|this, event: &MouseMoveEvent, _window, _cx| {
                                #[cfg(target_os = "macos")]
                                if let (Some(browser), Some(bounds)) = (&this.browser, &this.last_bounds) {
                                    let x = f32::from(event.position.x - bounds.origin.x) as i32;
                                    let y = f32::from(event.position.y - bounds.origin.y) as i32;
                                    browser.send_mouse_move(x, y);
                                }
                            }),
                        )
                        .on_scroll_wheel(
                            cx.listener(|this, event: &ScrollWheelEvent, _window, _cx| {
                                #[cfg(target_os = "macos")]
                                if let (Some(browser), Some(bounds)) = (&this.browser, &this.last_bounds) {
                                    let x = f32::from(event.position.x - bounds.origin.x) as i32;
                                    let y = f32::from(event.position.y - bounds.origin.y) as i32;
                                    let delta_x = f32::from(event.delta.pixel_delta(px(20.0)).x) as i32;
                                    let delta_y = f32::from(event.delta.pixel_delta(px(20.0)).y) as i32;
                                    browser.send_mouse_wheel(x, y, delta_x, delta_y);
                                }
                            }),
                        )
                        .when_some(frame_image.clone(), |this, image| {
                            this.child(
                                img(image)
                                    .size_full()
                                    .object_fit(gpui::ObjectFit::Fill),
                            )
                        })
                        .when(frame_image.is_none() && self.cef_error.is_none(), |this| {
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
fn gpui_modifiers_to_cef(modifiers: &gpui::Modifiers) -> u32 {
    // CEF modifier flags (from cef_event_flags_t)
    const SHIFT: u32 = 1 << 1;
    const CONTROL: u32 = 1 << 2;
    const ALT: u32 = 1 << 3;
    const COMMAND: u32 = 1 << 7;

    let mut flags = 0u32;
    if modifiers.shift { flags |= SHIFT; }
    if modifiers.control { flags |= CONTROL; }
    if modifiers.alt { flags |= ALT; }
    if modifiers.platform { flags |= COMMAND; }
    flags
}

#[cfg(target_os = "macos")]
fn mac_native_keycode_for_char(ch: char) -> i32 {
    match ch.to_ascii_lowercase() {
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
        'm' => 0x2E, '.' => 0x2F, '`' => 0x32, ' ' => 0x31,
        _ => 0,
    }
}
