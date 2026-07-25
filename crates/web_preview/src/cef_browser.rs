use anyhow::{Context, Result, anyhow};
use cef::*;
use gpui::{Bounds, Pixels};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const REMOTE_DEBUGGING_PORT: i32 = 9223;

// CEF in single-process mode calls isHandlingSendEvent / setHandlingSendEvent:
// on NSApplication. Zed's GPUIApplication doesn't implement these (CrAppProtocol).
// We inject them at runtime so CEF doesn't crash.
static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

unsafe fn install_cef_app_protocol_methods() {
    unsafe {
        // Use the raw ObjC runtime C API to add methods to GPUIApplication
        unsafe extern "C" {
            fn objc_getClass(name: *const std::os::raw::c_char) -> *mut c_void;
            fn sel_registerName(name: *const std::os::raw::c_char) -> *mut c_void;
            fn class_addMethod(
                cls: *mut c_void,
                sel: *mut c_void,
                imp: *const c_void,
                types: *const std::os::raw::c_char,
            ) -> bool;
            fn class_getInstanceMethod(cls: *mut c_void, sel: *mut c_void) -> *mut c_void;
        }

        extern "C" fn is_handling_send_event(
            _self: *mut c_void,
            _cmd: *mut c_void,
        ) -> bool {
            HANDLING_SEND_EVENT.load(Ordering::Relaxed)
        }

        extern "C" fn set_handling_send_event(
            _self: *mut c_void,
            _cmd: *mut c_void,
            value: bool,
        ) {
            HANDLING_SEND_EVENT.store(value, Ordering::Relaxed);
        }

        let cls = objc_getClass(c"GPUIApplication".as_ptr());
        if cls.is_null() {
            log::warn!("web_preview: GPUIApplication not found, skipping CrAppProtocol patch");
            return;
        }

        let is_sel = sel_registerName(c"isHandlingSendEvent".as_ptr());
        let set_sel = sel_registerName(c"setHandlingSendEvent:".as_ptr());

        if class_getInstanceMethod(cls, is_sel).is_null() {
            class_addMethod(
                cls,
                is_sel,
                is_handling_send_event as *const c_void,
                c"B@:".as_ptr(),
            );
            log::info!("web_preview: added isHandlingSendEvent to GPUIApplication");
        }

        if class_getInstanceMethod(cls, set_sel).is_null() {
            class_addMethod(
                cls,
                set_sel,
                set_handling_send_event as *const c_void,
                c"v@:B".as_ptr(),
            );
            log::info!("web_preview: added setHandlingSendEvent: to GPUIApplication");
        }
    }
}

static CEF_INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

fn ensure_cef_initialized() -> Result<()> {
    let result = CEF_INITIALIZED.get_or_init(|| match try_init_cef() {
        Ok(()) => Ok(()),
        Err(error) => {
            log::error!("web_preview: CEF initialization failed: {error}");
            Err(error.to_string())
        }
    });

    match result {
        Ok(()) => Ok(()),
        Err(message) => Err(anyhow!("{}", message)),
    }
}

fn find_cef_framework_path() -> Option<PathBuf> {
    // 1. Check CEF_PATH env var
    if let Ok(cef_path) = std::env::var("CEF_PATH") {
        let framework = PathBuf::from(&cef_path).join("Chromium Embedded Framework.framework");
        if framework.exists() {
            return Some(PathBuf::from(cef_path));
        }
    }

    // 2. Check relative to executable (bundled app)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // Standard bundle: ../Frameworks/
            let bundle_path = parent.join("../Frameworks");
            let framework = bundle_path.join("Chromium Embedded Framework.framework");
            if framework.exists() {
                return Some(bundle_path);
            }
        }
    }

    // 3. Scan build artifacts for dev builds
    let cwd = std::env::current_dir().ok()?;
    for profile in ["debug", "release"] {
        let build_dir = cwd.join(format!("target/{}/build", profile));
        if let Ok(entries) = std::fs::read_dir(&build_dir) {
            for entry in entries.flatten() {
                let cef_dir = entry.path().join("out/cef_macos_aarch64");
                let framework = cef_dir.join("Chromium Embedded Framework.framework");
                if framework.exists() {
                    return Some(cef_dir);
                }
            }
        }
    }

    None
}

fn try_init_cef() -> Result<()> {
    // Patch GPUIApplication with CrAppProtocol methods before CEF touches it
    unsafe { install_cef_app_protocol_methods() };

    let cef_dir = find_cef_framework_path().ok_or_else(|| {
        anyhow!(
            "Chromium Embedded Framework not found. Set CEF_PATH environment variable or \
             run `cargo build -p web_preview` to download it."
        )
    })?;

    log::info!("web_preview: found CEF framework at {}", cef_dir.display());

    let exe_path = std::env::current_exe().context("Failed to get current executable path")?;

    // LibraryLoader expects the framework at {exe_dir}/../Frameworks/
    // Create a symlink so it can find it during development.
    let exe_dir = exe_path.parent().context("Failed to get executable directory")?;
    let frameworks_dir = exe_dir.join("../Frameworks");
    let framework_link = frameworks_dir.join("Chromium Embedded Framework.framework");

    if !framework_link.exists() {
        let source_framework = cef_dir.join("Chromium Embedded Framework.framework");
        std::fs::create_dir_all(&frameworks_dir).ok();
        std::os::unix::fs::symlink(&source_framework, &framework_link)
            .with_context(|| format!(
                "Failed to symlink CEF framework from {} to {}",
                source_framework.display(),
                framework_link.display(),
            ))?;
        log::info!(
            "web_preview: symlinked CEF framework → {}",
            framework_link.display()
        );
    }

    // CEF looks for libGLESv2.dylib and libEGL.dylib next to the executable
    // for ANGLE/SwiftShader rendering. Symlink them from the framework.
    let libraries_dir = cef_dir.join("Chromium Embedded Framework.framework/Libraries");
    for lib_name in ["libGLESv2.dylib", "libEGL.dylib", "vk_swiftshader_icd.json"] {
        let target = exe_dir.join(lib_name);
        if !target.exists() {
            let source = libraries_dir.join(lib_name);
            if source.exists() {
                std::os::unix::fs::symlink(&source, &target).ok();
            }
        }
    }

    let loader = library_loader::LibraryLoader::new(&exe_path, false);
    if !loader.load() {
        return Err(anyhow!(
            "Failed to load CEF library from {}",
            cef_dir.display()
        ));
    }

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();

    let cache_path = profile_directory();
    std::fs::create_dir_all(&cache_path).ok();

    let helper_path = exe_path
        .parent()
        .map(|dir| dir.join("web_preview_helper"))
        .unwrap_or_default();

    // Point CEF to the framework directory so it finds icudtl.dat, pak files, etc.
    let framework_path = cef_dir.join("Chromium Embedded Framework.framework");
    let resources_path = framework_path.join("Resources");
    let locales_path = resources_path.clone();

    let settings = Settings {
        no_sandbox: 1,
        external_message_pump: 1,
        multi_threaded_message_loop: 0,
        windowless_rendering_enabled: 1,
        remote_debugging_port: REMOTE_DEBUGGING_PORT,
        persist_session_cookies: 1,
        cache_path: CefString::from(cache_path.to_str().unwrap_or("")),
        root_cache_path: CefString::from(cache_path.to_str().unwrap_or("")),
        browser_subprocess_path: CefString::from(
            helper_path.to_str().unwrap_or("web_preview_helper"),
        ),
        framework_dir_path: CefString::from(
            framework_path.to_str().unwrap_or(""),
        ),
        main_bundle_path: CefString::from(
            exe_dir.to_str().unwrap_or(""),
        ),
        resources_dir_path: CefString::from(
            resources_path.to_str().unwrap_or(""),
        ),
        locales_dir_path: CefString::from(
            locales_path.to_str().unwrap_or(""),
        ),
        ..Default::default()
    };

    let mut app = WebPreviewApp::new();

    let result = initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if result != 1 {
        return Err(anyhow!("cef_initialize returned {result}"));
    }

    log::info!(
        "web_preview: CEF initialized — CDP on port {REMOTE_DEBUGGING_PORT}"
    );

    Ok(())
}

fn profile_directory() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(format!(
        "{}/Library/Application Support/Zed Workstation/WebPreview/Profiles/default",
        home
    ))
}

// CEF App implementation
wrap_app! {
    struct WebPreviewApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            if let Some(command_line) = command_line {
                // Single process mode — all rendering happens in-process.
                command_line.append_switch(Some(&CefString::from("single-process")));
                // Disable GPU — use software rendering to avoid Metal conflicts.
                command_line.append_switch(Some(&CefString::from("disable-gpu")));
                command_line.append_switch(Some(&CefString::from("disable-gpu-compositing")));
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(WebPreviewBrowserProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    struct WebPreviewBrowserProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            log::info!("web_preview: CEF context initialized");
        }

        fn on_schedule_message_pump_work(&self, _delay_ms: i64) {
            // Work is driven by our GPUI timer calling do_message_loop_work()
        }
    }
}

// Browser state that's shared between CEF callbacks and the Rust view
#[derive(Debug, Clone)]
pub struct BrowserState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub cursor: gpui::CursorStyle,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            url: String::from("about:blank"),
            title: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            cursor: gpui::CursorStyle::Arrow,
        }
    }
}

fn cef_cursor_to_gpui(cursor_type: CursorType) -> gpui::CursorStyle {
    use gpui::CursorStyle;
    match cursor_type {
        CursorType::POINTER => CursorStyle::Arrow,
        CursorType::CROSS => CursorStyle::Crosshair,
        CursorType::HAND => CursorStyle::PointingHand,
        CursorType::IBEAM => CursorStyle::IBeam,
        CursorType::EASTRESIZE => CursorStyle::ResizeRight,
        CursorType::WESTRESIZE => CursorStyle::ResizeLeft,
        CursorType::NORTHRESIZE => CursorStyle::ResizeUp,
        CursorType::SOUTHRESIZE => CursorStyle::ResizeDown,
        CursorType::NORTHSOUTHRESIZE => CursorStyle::ResizeUpDown,
        CursorType::EASTWESTRESIZE => CursorStyle::ResizeLeftRight,
        CursorType::NORTHEASTRESIZE | CursorType::SOUTHWESTRESIZE => CursorStyle::ResizeUpRightDownLeft,
        CursorType::NORTHWESTRESIZE | CursorType::SOUTHEASTRESIZE => CursorStyle::ResizeUpLeftDownRight,
        CursorType::NORTHEASTSOUTHWESTRESIZE => CursorStyle::ResizeUpRightDownLeft,
        CursorType::NORTHWESTSOUTHEASTRESIZE => CursorStyle::ResizeUpLeftDownRight,
        CursorType::COLUMNRESIZE => CursorStyle::ResizeColumn,
        CursorType::ROWRESIZE => CursorStyle::ResizeRow,
        CursorType::MOVE | CursorType::MIDDLEPANNING => CursorStyle::OpenHand,
        CursorType::GRAB | CursorType::GRABBING => CursorStyle::ClosedHand,
        CursorType::NOTALLOWED | CursorType::NODROP => CursorStyle::OperationNotAllowed,
        _ => CursorStyle::Arrow,
    }
}

// CEF Client implementation
/// BGRA pixel buffer from CEF's offscreen rendering.
#[derive(Clone)]
pub struct FrameBuffer {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(crate) struct WebPreviewHandler {
    state: Arc<Mutex<BrowserState>>,
    browser: Arc<Mutex<Option<Browser>>>,
    frame: Arc<Mutex<Option<FrameBuffer>>>,
    view_width: Arc<Mutex<i32>>,
    view_height: Arc<Mutex<i32>>,
    scale_factor: Arc<Mutex<f32>>,
}

impl WebPreviewHandler {
    fn new(width: i32, height: i32, scale_factor: f32) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            state: Arc::new(Mutex::new(BrowserState::default())),
            browser: Arc::new(Mutex::new(None)),
            frame: Arc::new(Mutex::new(None)),
            view_width: Arc::new(Mutex::new(width)),
            view_height: Arc::new(Mutex::new(height)),
            scale_factor: Arc::new(Mutex::new(scale_factor)),
        }))
    }
}

wrap_client! {
    struct WebPreviewClient {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(WebPreviewDisplayHandler::new(self.inner.clone()))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(WebPreviewLifeSpanHandler::new(self.inner.clone()))
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(WebPreviewLoadHandler::new(self.inner.clone()))
        }

        fn render_handler(&self) -> Option<RenderHandler> {
            Some(WebPreviewRenderHandler::new(self.inner.clone()))
        }
    }
}

wrap_display_handler! {
    struct WebPreviewDisplayHandler {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Ok(inner) = self.inner.lock() {
                if let Ok(mut state) = inner.state.lock() {
                    state.title = title
                        .map(|t| t.to_string())
                        .unwrap_or_default();
                }
            }
        }

        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if let Ok(inner) = self.inner.lock() {
                if let Ok(mut state) = inner.state.lock() {
                    state.url = url
                        .map(|u| u.to_string())
                        .unwrap_or_default();
                }
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            if let Ok(inner) = self.inner.lock() {
                if let Ok(mut state) = inner.state.lock() {
                    state.cursor = cef_cursor_to_gpui(type_);
                }
            }
            0
        }
    }
}

wrap_life_span_handler! {
    struct WebPreviewLifeSpanHandler {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            log::info!("web_preview: browser created");
            if let Some(browser) = browser.cloned() {
                if let Ok(inner) = self.inner.lock() {
                    if let Ok(mut stored) = inner.browser.lock() {
                        *stored = Some(browser);
                    }
                }
            }
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0 // allow close
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            log::info!("web_preview: browser closed");
        }
    }
}

wrap_load_handler! {
    struct WebPreviewLoadHandler {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            if let Ok(inner) = self.inner.lock() {
                if let Ok(mut state) = inner.state.lock() {
                    state.loading = is_loading != 0;
                    state.can_go_back = can_go_back != 0;
                    state.can_go_forward = can_go_forward != 0;
                }
            }
        }
    }
}

wrap_render_handler! {
    struct WebPreviewRenderHandler {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                if let Ok(inner) = self.inner.lock() {
                    let w = inner.view_width.lock().map(|v| *v).unwrap_or(800);
                    let h = inner.view_height.lock().map(|v| *v).unwrap_or(600);
                    rect.x = 0;
                    rect.y = 0;
                    rect.width = w;
                    rect.height = h;
                }
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            if let Some(info) = screen_info {
                if let Ok(inner) = self.inner.lock() {
                    let scale = inner.scale_factor.lock().map(|v| *v).unwrap_or(2.0);
                    info.device_scale_factor = scale;
                }
                return 1;
            }
            0
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            _type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let size = (width * height * 4) as usize;
            let pixels = unsafe { std::slice::from_raw_parts(buffer, size) }.to_vec();

            if let Ok(inner) = self.inner.lock() {
                if let Ok(mut frame) = inner.frame.lock() {
                    *frame = Some(FrameBuffer {
                        pixels,
                        width: width as u32,
                        height: height as u32,
                    });
                }
            }
        }
    }
}

pub(crate) enum QueuedInput {
    MouseClick { x: i32, y: i32, button: MouseButtonType, mouse_up: bool, click_count: i32 },
    MouseMove { x: i32, y: i32 },
    MouseWheel { x: i32, y: i32, delta_x: i32, delta_y: i32 },
    Key(KeyEvent),
    Focus(bool),
}

/// Wraps a CEF browser instance with offscreen rendering.
pub struct CefBrowserInstance {
    handler: Arc<Mutex<WebPreviewHandler>>,
    input_queue: Arc<Mutex<Vec<QueuedInput>>>,
}

unsafe impl Send for CefBrowserInstance {}

impl CefBrowserInstance {
    pub fn new(bounds: Bounds<Pixels>, scale_factor: f32) -> Result<Self> {
        ensure_cef_initialized()?;

        let width: f32 = bounds.size.width.into();
        let height: f32 = bounds.size.height.into();

        let handler = WebPreviewHandler::new(width as i32, height as i32, scale_factor);

        let mut client: Option<Client> = Some(WebPreviewClient::new(handler.clone()));

        // Configure WindowInfo for offscreen (windowless) rendering
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };

        let settings = BrowserSettings::default();
        let url = CefString::from("about:blank");

        browser_host_create_browser(
            Some(&window_info),
            client.as_mut(),
            Some(&url),
            Some(&settings),
            None,
            None,
        );

        // Pump CEF to let the browser creation complete
        for _ in 0..10 {
            do_message_loop_work();
        }

        Ok(Self {
            handler,
            input_queue: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn input_queue_arc(&self) -> Arc<Mutex<Vec<QueuedInput>>> {
        self.input_queue.clone()
    }

    pub fn handler_arc(&self) -> Arc<Mutex<WebPreviewHandler>> {
        self.handler.clone()
    }

    /// Flush input and send to CEF. Called OUTSIDE of GPUI entity update
    /// to avoid re-entrancy with CEF's synchronous event processing.
    pub fn flush_queued(
        input_queue: &Arc<Mutex<Vec<QueuedInput>>>,
        handler: &Arc<Mutex<WebPreviewHandler>>,
    ) {
        let events: Vec<QueuedInput> = {
            let Ok(mut queue) = input_queue.lock() else { return };
            std::mem::take(&mut *queue)
        };

        if events.is_empty() {
            return;
        }

        let browser_arc = {
            let Ok(inner) = handler.lock() else { return };
            inner.browser.clone()
        };
        let Ok(browser_guard) = browser_arc.lock() else { return };
        let Some(browser) = browser_guard.as_ref() else { return };
        let Some(host) = browser.host() else { return };

        for input in &events {
            match input {
                QueuedInput::MouseClick { x, y, button, mouse_up, click_count } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: 0 };
                    host.send_mouse_click_event(Some(&event), *button, *mouse_up as i32, *click_count);
                }
                QueuedInput::MouseMove { x, y } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: 0 };
                    host.send_mouse_move_event(Some(&event), 0);
                }
                QueuedInput::MouseWheel { x, y, delta_x, delta_y } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: 0 };
                    host.send_mouse_wheel_event(Some(&event), *delta_x, *delta_y);
                }
                QueuedInput::Key(key_event) => {
                    host.send_key_event(Some(key_event));
                }
                QueuedInput::Focus(focused) => {
                    host.set_focus(*focused as i32);
                }
            }
        }
    }

    pub fn take_frame(&self) -> Option<FrameBuffer> {
        let inner = self.handler.lock().ok()?;
        let frame_arc = inner.frame.clone();
        drop(inner);
        let mut frame = frame_arc.lock().ok()?;
        frame.take()
    }

    pub fn resize(&self, width: i32, height: i32) {
        if let Ok(inner) = self.handler.lock() {
            if let Ok(mut w) = inner.view_width.lock() { *w = width; }
            if let Ok(mut h) = inner.view_height.lock() { *h = height; }
        }
        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                host.was_resized();
            }
        });
    }

    fn browser_arc(&self) -> Option<Arc<Mutex<Option<Browser>>>> {
        let inner = self.handler.lock().ok()?;
        Some(inner.browser.clone())
    }

    fn with_browser<R>(&self, f: impl FnOnce(&Browser) -> R) -> Option<R> {
        let browser_arc = self.browser_arc()?;
        let browser_guard = browser_arc.lock().ok()?;
        let browser = browser_guard.as_ref()?;
        Some(f(browser))
    }

    #[allow(dead_code)]
    pub fn has_browser(&self) -> bool {
        self.with_browser(|_| ()).is_some()
    }

    pub fn current_state(&self) -> Option<BrowserState> {
        let state_arc = {
            let inner = self.handler.lock().ok()?;
            inner.state.clone()
        };
        let state = state_arc.lock().ok()?;
        Some(state.clone())
    }

    pub fn navigate_to(&self, url: &str) {
        let url_string = if !url.contains("://") && !url.starts_with("about:") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };

        self.with_browser(|browser| {
            if let Some(frame) = browser.main_frame() {
                let cef_url = CefString::from(url_string.as_str());
                frame.load_url(Some(&cef_url));
            }
        });

        // Pump immediately to start processing the navigation
        for _ in 0..5 {
            do_message_loop_work();
        }
    }

    pub fn reload(&self) {
        self.with_browser(|browser| browser.reload());
    }

    pub fn stop(&self) {
        self.with_browser(|browser| browser.stop_load());
    }

    pub fn go_back(&self) {
        self.with_browser(|browser| browser.go_back());
    }

    pub fn go_forward(&self) {
        self.with_browser(|browser| browser.go_forward());
    }

    fn queue(&self, input: QueuedInput) {
        if let Ok(mut queue) = self.input_queue.lock() {
            queue.push(input);
        }
    }

    pub fn send_mouse_click(&self, x: i32, y: i32, button: MouseButtonType, mouse_up: bool, click_count: i32) {
        self.queue(QueuedInput::MouseClick { x, y, button, mouse_up, click_count });
    }

    pub fn send_mouse_move(&self, x: i32, y: i32) {
        self.queue(QueuedInput::MouseMove { x, y });
    }

    pub fn send_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32) {
        self.queue(QueuedInput::MouseWheel { x, y, delta_x, delta_y });
    }

    pub fn send_key(&self, key_event: KeyEvent) {
        self.queue(QueuedInput::Key(key_event));
    }

    pub fn set_focus(&self, focused: bool) {
        self.queue(QueuedInput::Focus(focused));
    }


    pub fn open_devtools(&self) {
        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                let window_info = WindowInfo::default();
                let settings = BrowserSettings::default();
                host.show_dev_tools(Some(&window_info), None, Some(&settings), None);
            }
        });
    }
}

impl Drop for CefBrowserInstance {
    fn drop(&mut self) {
        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        });
    }
}
