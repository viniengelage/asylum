use anyhow::{Context, Result, anyhow};
use cef::*;
use core_foundation::base::{TCFType, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_video::pixel_buffer::{
    CVPixelBuffer, CVPixelBufferRef, kCVPixelFormatType_32BGRA,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
};
use core_video::pixel_buffer_io_surface::CVPixelBufferCreateWithIOSurface;
use core_video::r#return::kCVReturnSuccess;
use gpui::{Bounds, Pixels};
use crate::cef_paths;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const REMOTE_DEBUGGING_PORT: i32 = 9223;

/// Chromium's own name for the framework bundle location, the command line equivalent of
/// `Settings::framework_dir_path`.
const FRAMEWORK_DIR_PATH_SWITCH: &str = "framework-dir-path";

/// Frames CEF paints per second. Offscreen rendering is capped by this rather than by
/// the display, and CEF's default of 30 is visible as stutter while scrolling.
const WINDOWLESS_FRAME_RATE: i32 = 60;

/// Surfaces the software paint path cycles through. CEF writes into one while the
/// renderer may still be sampling the previous frames, so overwriting the buffer it
/// just handed over would tear.
const SOFTWARE_SURFACE_COUNT: usize = 3;

/// Cap on the delay CEF asks us to wait before the next `do_message_loop_work()`.
/// Matches cefclient's external pump: a pending-work floor of 30fps, because CEF
/// does not reliably re-schedule work it has already asked for.
const MAX_PUMP_DELAY_MS: u64 = 1000 / 30;
/// Pump at least this often even when CEF claims to have nothing pending, so a
/// missed `OnScheduleMessagePumpWork` cannot freeze the page indefinitely.
const IDLE_PUMP_INTERVAL_MS: u64 = 100;
/// Bounds on how long the caller waits between pump checks. The upper bound also
/// bounds input latency, since queued input is only delivered on these ticks.
const MIN_POLL_MS: u64 = 1;
const MAX_POLL_MS: u64 = 16;

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

/// The framework bundle `cef_initialize` was pointed at. Child processes have to be told
/// the same path on their command line: `browser_subprocess_path` only says which
/// executable to launch, and the Chromium inside it looks for `icudtl.dat` and the `.pak`
/// files relative to a framework bundle it would otherwise expect next to itself.
static FRAMEWORK_BUNDLE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set once `cef_initialize` has returned successfully, after which the framework's entry
/// points are safe to call.
static CEF_READY: AtomicBool = AtomicBool::new(false);

/// Set before `cef_initialize` when no helper executable is available to host the
/// render/GPU/utility processes.
static SINGLE_PROCESS: AtomicBool = AtomicBool::new(false);

/// Set once a browser that asked for GPU frames never produced one, so browsers created
/// afterwards ask CEF to rasterize into memory we copy from instead.
static FORCE_SOFTWARE_PAINT: AtomicBool = AtomicBool::new(false);

/// Whether new browsers should ask the GPU process for frames rather than have Chromium
/// rasterize into system memory for us to copy.
pub fn shared_texture_enabled() -> bool {
    !SINGLE_PROCESS.load(Ordering::Relaxed) && !FORCE_SOFTWARE_PAINT.load(Ordering::Relaxed)
}

pub fn force_software_paint() {
    FORCE_SOFTWARE_PAINT.store(true, Ordering::Relaxed);
}

/// When the next `do_message_loop_work()` is due, in milliseconds since [`pump_epoch`].
/// `u64::MAX` means CEF has told us it has nothing pending.
static PUMP_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_PUMP_MS: AtomicU64 = AtomicU64::new(0);

fn pump_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    pump_epoch().elapsed().as_millis() as u64
}

fn pump_due_at() -> u64 {
    let scheduled = PUMP_DEADLINE_MS.load(Ordering::Relaxed);
    let idle_deadline = LAST_PUMP_MS
        .load(Ordering::Relaxed)
        .saturating_add(IDLE_PUMP_INTERVAL_MS);
    scheduled.min(idle_deadline)
}

/// Ask for `do_message_loop_work()` to run within `delay_ms`. Called by CEF from
/// arbitrary threads, and by us after handing CEF work it should act on right away.
fn schedule_pump(delay_ms: i64) {
    let delay = delay_ms.clamp(0, MAX_PUMP_DELAY_MS as i64) as u64;
    let deadline = now_ms().saturating_add(delay);
    PUMP_DEADLINE_MS.fetch_min(deadline, Ordering::Relaxed);
}

/// Requests a pump on the next tick of the poll loop.
pub fn request_pump() {
    schedule_pump(0);
}

/// Runs CEF's pending work if any is due and returns how long to wait before
/// checking again. Must only be called from the thread that ran `cef_initialize`.
pub fn pump_message_loop() -> Duration {
    // The CEF entry points are resolved out of a framework that is only loaded once
    // initialization runs, so calling one before that jumps through a null symbol. A tab
    // opened before the framework is installed polls in this state the whole time.
    if !CEF_READY.load(Ordering::Relaxed) {
        return Duration::from_millis(MAX_POLL_MS);
    }

    if now_ms() >= pump_due_at() {
        // Cleared before pumping: CEF calls `OnScheduleMessagePumpWork` from inside
        // `do_message_loop_work`, and that request must survive this reset.
        PUMP_DEADLINE_MS.store(u64::MAX, Ordering::Relaxed);
        do_message_loop_work();
        LAST_PUMP_MS.store(now_ms(), Ordering::Relaxed);
    }

    let wait = pump_due_at()
        .saturating_sub(now_ms())
        .clamp(MIN_POLL_MS, MAX_POLL_MS);
    Duration::from_millis(wait)
}

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


fn try_init_cef() -> Result<()> {
    // Patch GPUIApplication with CrAppProtocol methods before CEF touches it
    unsafe { install_cef_app_protocol_methods() };

    let (cef_dir, source) = cef_paths::find_framework().ok_or_else(|| {
        anyhow!(
            "Chromium Embedded Framework not installed. Install it from the Web Preview tab, \
             set CEF_PATH, or run `cargo build -p web_preview` to download it."
        )
    })?;

    log::info!(
        "web_preview: found CEF framework at {} ({source:?})",
        cef_dir.display()
    );

    let exe_path = std::env::current_exe().context("Failed to get current executable path")?;
    let exe_dir = exe_path.parent().context("Failed to get executable directory")?;

    // CEF looks for libGLESv2.dylib and libEGL.dylib next to the executable for
    // ANGLE/SwiftShader rendering. Inside an `.app` the executable's directory is sealed by
    // the code signature, so there they have to be found through `framework_dir_path`.
    if !cef_paths::running_from_bundle() {
        let libraries_dir = cef_dir
            .join(cef_paths::FRAMEWORK_NAME)
            .join("Libraries");
        for lib_name in ["libGLESv2.dylib", "libEGL.dylib", "vk_swiftshader_icd.json"] {
            let target = exe_dir.join(lib_name);
            if !target.exists() {
                let source = libraries_dir.join(lib_name);
                if source.exists() {
                    std::os::unix::fs::symlink(&source, &target).ok();
                }
            }
        }
    }

    if !cef_paths::load_cef_library(&cef_dir) {
        return Err(anyhow!(
            "Failed to load CEF library from {}",
            cef_dir.display()
        ));
    }

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();

    let cache_path = profile_directory();
    std::fs::create_dir_all(&cache_path).ok();

    // Inside an `.app` the helper has to be a nested app bundle. Chromium derives the
    // executable for renderer and other child flavors from this path by stripping
    // "<name>.app/Contents/MacOS/<name>" and appending "<name> (Renderer).app/Contents/
    // MacOS/<name> (Renderer)"; pointed at a bare executable in `Contents/MacOS` those
    // four levels land outside the bundle, and Chromium then launches the main Zed
    // executable instead — which macOS refuses to start as a child process, leaving a
    // browser with a GPU process but no renderer, painting nothing. Out of `target/` the
    // flavors are never used (`base::apple::AmIBundled()` is false there), so the plain
    // executable next to Zed is enough.
    let helper_path = match cef_paths::running_from_bundle() {
        true => exe_dir
            .parent()
            .map(|contents| {
                contents
                    .join("Frameworks")
                    .join("web_preview_helper.app")
                    .join("Contents")
                    .join("MacOS")
                    .join("web_preview_helper")
            })
            .unwrap_or_default(),
        false => exe_dir.join("web_preview_helper"),
    };

    // Without the helper there is nowhere to put the render/GPU/utility processes, so
    // CEF has to run everything inside Zed — where any Chromium `CHECK` kills the
    // editor and Chromium's GPU thread would fight GPUI for a Metal device.
    let single_process = !helper_path.is_file();
    if single_process {
        log::warn!(
            "web_preview: {} not found, falling back to CEF single-process mode \
             (a crash anywhere in Chromium will take Zed down, and pages render in \
             software). Build it with `cargo build -p web_preview --bin web_preview_helper`.",
            helper_path.display()
        );
    }
    SINGLE_PROCESS.store(single_process, Ordering::Relaxed);

    // Inside an app it has to be the .app itself — CEF reads an Info.plist from here, and
    // `Contents/MacOS` has none. Out of `target/` there is no bundle, so the executable's
    // directory is the closest thing.
    let main_bundle_path = match cef_paths::running_from_bundle() {
        true => exe_dir
            .parent()
            .and_then(|contents| contents.parent())
            .unwrap_or(exe_dir),
        false => exe_dir,
    };

    // Point CEF to the framework directory so it finds icudtl.dat, pak files, etc.
    let framework_path = cef_dir.join("Chromium Embedded Framework.framework");
    FRAMEWORK_BUNDLE_PATH.set(framework_path.clone()).ok();
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
            main_bundle_path.to_str().unwrap_or(""),
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

    CEF_READY.store(true, Ordering::Relaxed);

    log::info!(
        "web_preview: CEF initialized — CDP on port {REMOTE_DEBUGGING_PORT}, {} process mode",
        if single_process { "single" } else { "multi" }
    );

    Ok(())
}

fn profile_directory() -> PathBuf {
    cef_paths::support_dir().join("Profiles/default")
}

// CEF App implementation
wrap_app! {
    struct WebPreviewApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };

            // Pages that probe for a camera (Google's voice search and Lens buttons do)
            // reach media/capture/video/apple/video_capture_device_factory_apple.mm,
            // which trapped on `brk` there and took the whole editor down. The fake
            // factory keeps that file out of the request; a preview needs no camera.
            command_line.append_switch(Some(&CefString::from(
                "use-fake-device-for-media-stream",
            )));
            // macOS defaults capture buffers to GpuMemoryBuffers, which need a viz
            // context provider we never hand them ("Bind context provider failed.").
            command_line.append_switch(Some(&CefString::from(
                "disable-video-capture-use-gpu-memory-buffer",
            )));

            // The switches below describe how *this* process is laid out; children
            // inherit what they need from the browser process command line.
            let is_browser_process = process_type.is_none_or(|type_| type_.to_string().is_empty());
            if is_browser_process && SINGLE_PROCESS.load(Ordering::Relaxed) {
                command_line.append_switch(Some(&CefString::from("single-process")));
                // In-process, Chromium's GPU thread builds a second Metal device inside
                // Zed and contends with GPUI's renderer, so fall back to software.
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

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            // Called from arbitrary threads; the GPUI poll task performs the actual
            // `do_message_loop_work()` once this deadline comes due.
            schedule_pump(delay_ms);
        }

        fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
            // The framework lives in the support directory, not next to the helper, so a
            // child left to find it on its own dies in PreSandboxStartup with "icudtl.dat
            // not found in bundle" — before it can render anything, which read as a
            // browser that simply never painted.
            let Some(command_line) = command_line else {
                return;
            };
            let Some(framework_path) = FRAMEWORK_BUNDLE_PATH.get() else {
                return;
            };
            let Some(framework_path) = framework_path.to_str() else {
                return;
            };

            let switch = CefString::from(FRAMEWORK_DIR_PATH_SWITCH);
            if command_line.has_switch(Some(&switch)) == 0 {
                command_line
                    .append_switch_with_value(
                        Some(&switch),
                        Some(&CefString::from(framework_path)),
                    );
            }
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
    /// Current page selection, mirrored from `on_text_selection_changed` so the macOS input
    /// system can be answered synchronously. CEF cannot be queried for it.
    pub selected_text: String,
    pub selected_range: Option<std::ops::Range<usize>>,
    /// Caret / composition rectangles in logical pixels relative to the page, used to place
    /// the IME candidate window.
    pub composition_bounds: Vec<gpui::Bounds<Pixels>>,
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
            selected_text: String::new(),
            selected_range: None,
            composition_bounds: Vec::new(),
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

/// A painted page, in the form the Metal renderer can sample directly: BGRA pixels in
/// an IOSurface, which on Apple silicon is memory both the CPU and GPU address, so no
/// upload stands between CEF and the screen.
#[derive(Clone)]
pub struct BrowserFrame {
    pub pixel_buffer: CVPixelBuffer,
    /// Size of the page content within `pixel_buffer`. The compositor pads its textures
    /// to its own alignment, so the surface can be larger than what was painted.
    pub content_width: u32,
    pub content_height: u32,
    /// Scale the page was rasterized at. Carried on the frame rather than read from the
    /// window so that a frame painted before a resize is still drawn at its own size,
    /// instead of being stretched to bounds it was never laid out for.
    pub scale_factor: f32,
}

// Frames are produced on the CEF UI thread, which under the external message pump is
// GPUI's foreground thread — the same thread that later composites them.
unsafe impl Send for BrowserFrame {}

/// Ring of IOSurface-backed buffers the software paint path copies into, so a steady
/// stream of frames costs one memcpy each instead of an allocation and a GPU upload.
#[derive(Default)]
struct SurfacePool {
    surfaces: Vec<CVPixelBuffer>,
    next: usize,
    width: u32,
    height: u32,
}

// Only ever touched from the CEF UI thread, which is GPUI's foreground thread.
unsafe impl Send for SurfacePool {}

impl SurfacePool {
    /// Returns the next surface to paint into, rebuilding the ring when the page size
    /// changed.
    fn next_surface(&mut self, width: u32, height: u32) -> Option<CVPixelBuffer> {
        if self.width != width || self.height != height || self.surfaces.is_empty() {
            self.surfaces.clear();
            self.next = 0;
            self.width = width;
            self.height = height;

            for _ in 0..SOFTWARE_SURFACE_COUNT {
                match create_bgra_surface(width, height) {
                    Ok(surface) => self.surfaces.push(surface),
                    Err(error) => {
                        log::error!("web_preview: failed to create paint surface: {error}");
                        self.surfaces.clear();
                        return None;
                    }
                }
            }
        }

        let surface = self.surfaces.get(self.next)?.clone();
        self.next = (self.next + 1) % self.surfaces.len();
        Some(surface)
    }
}

fn create_bgra_surface(width: u32, height: u32) -> Result<CVPixelBuffer> {
    let attributes = CFDictionary::from_CFType_pairs(&[
        (
            unsafe { CFString::wrap_under_get_rule(kCVPixelBufferMetalCompatibilityKey) },
            CFBoolean::true_value().as_CFType(),
        ),
        (
            // An empty properties dictionary is what asks for IOSurface backing; without
            // it the pixels live in ordinary memory the GPU cannot address.
            unsafe { CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey) },
            CFDictionary::<CFString, CFNumber>::from_CFType_pairs(&[]).as_CFType(),
        ),
    ]);

    CVPixelBuffer::new(
        kCVPixelFormatType_32BGRA,
        width as usize,
        height as usize,
        Some(&attributes),
    )
    .map_err(|status| anyhow!("CVPixelBufferCreate failed with status {status}"))
}

/// Copies tightly packed BGRA pixels into a surface, whose rows the system pads to its
/// own alignment.
fn copy_pixels_into_surface(surface: &CVPixelBuffer, source: &[u8], width: u32, height: u32) {
    if surface.lock_base_address(0) != kCVReturnSuccess {
        log::error!("web_preview: could not lock paint surface");
        return;
    }

    let destination = unsafe { surface.get_base_address() };
    let destination_stride = surface.get_bytes_per_row();
    let source_stride = width as usize * 4;

    if !destination.is_null() && destination_stride >= source_stride {
        for row in 0..height as usize {
            let source_offset = row * source_stride;
            let Some(source_row) = source.get(source_offset..source_offset + source_stride) else {
                break;
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source_row.as_ptr(),
                    (destination as *mut u8).add(row * destination_stride),
                    source_stride,
                );
            }
        }
    }

    if surface.unlock_base_address(0) != kCVReturnSuccess {
        log::error!("web_preview: could not unlock paint surface");
    }
}

fn surface_from_io_surface(handle: *mut c_void) -> Option<CVPixelBuffer> {
    if handle.is_null() {
        return None;
    }
    let mut pixel_buffer: CVPixelBufferRef = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreateWithIOSurface(
            kCFAllocatorDefault,
            handle as *const _,
            std::ptr::null(),
            &mut pixel_buffer,
        )
    };
    if status != kCVReturnSuccess || pixel_buffer.is_null() {
        log::error!("web_preview: CVPixelBufferCreateWithIOSurface failed with status {status}");
        return None;
    }
    Some(unsafe { CVPixelBuffer::wrap_under_create_rule(pixel_buffer) })
}

pub(crate) struct WebPreviewHandler {
    state: Arc<Mutex<BrowserState>>,
    /// Bumped whenever CEF reports new page state, so the poll loop can tell there is
    /// nothing to sync without locking and cloning the state on every tick.
    state_version: Arc<AtomicU64>,
    browser: Arc<Mutex<Option<Browser>>>,
    /// A URL requested before CEF finished creating the browser, loaded by
    /// `on_after_created`.
    pending_url: Arc<Mutex<Option<String>>>,
    frame: Arc<Mutex<Option<BrowserFrame>>>,
    software_surfaces: Arc<Mutex<SurfacePool>>,
    /// Set once CEF hands over a GPU texture, after which software paints — which CEF
    /// still emits for popups — must not overwrite the frame.
    accelerated: Arc<AtomicBool>,
    view_width: Arc<Mutex<i32>>,
    view_height: Arc<Mutex<i32>>,
    scale_factor: Arc<Mutex<f32>>,
}

impl WebPreviewHandler {
    fn new(width: i32, height: i32, scale_factor: f32) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            state: Arc::new(Mutex::new(BrowserState::default())),
            state_version: Arc::new(AtomicU64::new(0)),
            browser: Arc::new(Mutex::new(None)),
            pending_url: Arc::new(Mutex::new(None)),
            frame: Arc::new(Mutex::new(None)),
            software_surfaces: Arc::new(Mutex::new(SurfacePool::default())),
            accelerated: Arc::new(AtomicBool::new(false)),
            view_width: Arc::new(Mutex::new(width)),
            view_height: Arc::new(Mutex::new(height)),
            scale_factor: Arc::new(Mutex::new(scale_factor)),
        }))
    }

    /// Applies a CEF state change. The closure reports whether anything actually moved,
    /// so state CEF re-reports unchanged does not cost the view a redraw.
    fn update_state(
        handler: &Arc<Mutex<Self>>,
        update: impl FnOnce(&mut BrowserState) -> bool,
    ) {
        let Ok(inner) = handler.lock() else {
            return;
        };
        let Ok(mut state) = inner.state.lock() else {
            return;
        };
        if update(&mut state) {
            inner.state_version.fetch_add(1, Ordering::Relaxed);
        }
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
            let title = title.map(|title| title.to_string()).unwrap_or_default();
            WebPreviewHandler::update_state(&self.inner, |state| {
                let changed = state.title != title;
                state.title = title;
                changed
            });
        }

        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let url = url.map(|url| url.to_string()).unwrap_or_default();
            WebPreviewHandler::update_state(&self.inner, |state| {
                let changed = state.url != url;
                state.url = url;
                changed
            });
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            let cursor = cef_cursor_to_gpui(type_);
            WebPreviewHandler::update_state(&self.inner, |state| {
                let changed = state.cursor != cursor;
                state.cursor = cursor;
                changed
            });
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
                    let pending_url = inner
                        .pending_url
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.take());
                    if let Some(url) = pending_url
                        && let Some(frame) = browser.main_frame()
                    {
                        frame.load_url(Some(&CefString::from(url.as_str())));
                    }
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
            WebPreviewHandler::update_state(&self.inner, |state| {
                let changed = state.loading != (is_loading != 0)
                    || state.can_go_back != (can_go_back != 0)
                    || state.can_go_forward != (can_go_forward != 0);
                state.loading = is_loading != 0;
                state.can_go_back = can_go_back != 0;
                state.can_go_forward = can_go_forward != 0;
                changed
            });
        }
    }
}

wrap_render_handler! {
    struct WebPreviewRenderHandler {
        inner: Arc<Mutex<WebPreviewHandler>>,
    }

    impl RenderHandler {
        fn on_text_selection_changed(
            &self,
            _browser: Option<&mut Browser>,
            selected_text: Option<&CefString>,
            selected_range: Option<&Range>,
        ) {
            let text = selected_text.map(|text| text.to_string()).unwrap_or_default();
            let range = selected_range.and_then(|range| {
                let from = usize::try_from(range.from).ok()?;
                let to = usize::try_from(range.to).ok()?;
                (from <= to).then_some(from..to)
            });
            WebPreviewHandler::update_state(&self.inner, |state| {
                let changed = state.selected_text != text || state.selected_range != range;
                state.selected_text = text;
                state.selected_range = range;
                changed
            });
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            let bounds: Vec<gpui::Bounds<Pixels>> = character_bounds
                .unwrap_or(&[])
                .iter()
                .map(|rect| gpui::Bounds {
                    origin: gpui::point(gpui::px(rect.x as f32), gpui::px(rect.y as f32)),
                    size: gpui::size(gpui::px(rect.width as f32), gpui::px(rect.height as f32)),
                })
                .collect();
            WebPreviewHandler::update_state(&self.inner, |state| {
                state.composition_bounds = bounds;
                false
            });
        }

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
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            if type_ != PaintElementType::VIEW {
                return;
            }

            let Ok(inner) = self.inner.lock() else {
                return;
            };
            // Once the GPU path is live CEF keeps this callback for popups only, and
            // painting those over the page would replace it with a mostly empty frame.
            if inner.accelerated.load(Ordering::Relaxed) {
                return;
            }

            let width = width as u32;
            let height = height as u32;
            let Some(surface) = inner
                .software_surfaces
                .lock()
                .ok()
                .and_then(|mut pool| pool.next_surface(width, height))
            else {
                return;
            };

            let source =
                unsafe { std::slice::from_raw_parts(buffer, width as usize * height as usize * 4) };
            copy_pixels_into_surface(&surface, source, width, height);

            let scale_factor = inner.scale_factor.lock().map(|v| *v).unwrap_or(1.0);
            if let Ok(mut slot) = inner.frame.lock() {
                *slot = Some(BrowserFrame {
                    pixel_buffer: surface,
                    content_width: width,
                    content_height: height,
                    scale_factor,
                });
            }
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if type_ != PaintElementType::VIEW {
                return;
            }
            let Some(info) = info else {
                return;
            };
            let visible = &info.extra.visible_rect;
            if visible.width <= 0 || visible.height <= 0 {
                return;
            }
            let Some(pixel_buffer) = surface_from_io_surface(info.shared_texture_io_surface) else {
                return;
            };

            let Ok(inner) = self.inner.lock() else {
                return;
            };
            if !inner.accelerated.swap(true, Ordering::Relaxed) {
                log::info!("web_preview: compositing GPU frames from CEF without a copy");
                // The buffers the software path was cycling through are dead weight now.
                if let Ok(mut pool) = inner.software_surfaces.lock() {
                    *pool = SurfacePool::default();
                }
            }

            let scale_factor = inner.scale_factor.lock().map(|v| *v).unwrap_or(1.0);
            if let Ok(mut slot) = inner.frame.lock() {
                *slot = Some(BrowserFrame {
                    pixel_buffer,
                    content_width: visible.width as u32,
                    content_height: visible.height as u32,
                    scale_factor,
                });
            }
        }
    }
}

/// `cef_event_flags_t`, which the `cef` crate does not re-export as named constants.
pub mod event_flags {
    pub const CAPS_LOCK_ON: u32 = 1 << 0;
    pub const SHIFT_DOWN: u32 = 1 << 1;
    pub const CONTROL_DOWN: u32 = 1 << 2;
    pub const ALT_DOWN: u32 = 1 << 3;
    pub const LEFT_MOUSE_BUTTON: u32 = 1 << 4;
    pub const MIDDLE_MOUSE_BUTTON: u32 = 1 << 5;
    pub const RIGHT_MOUSE_BUTTON: u32 = 1 << 6;
    pub const COMMAND_DOWN: u32 = 1 << 7;
    pub const IS_KEY_PAD: u32 = 1 << 9;
    pub const IS_REPEAT: u32 = 1 << 13;
    pub const PRECISION_SCROLLING_DELTA: u32 = 1 << 14;
}

/// Editing commands the renderer runs against the focused field or the current selection.
/// In windowless mode on macOS these never arrive on their own: the Cmd shortcuts that
/// normally trigger them come from AppKit's responder chain, which offscreen rendering
/// bypasses, so the client has to drive them.
#[derive(Clone, Copy, Debug)]
pub enum FrameCommand {
    SelectAll,
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
}

pub(crate) enum QueuedInput {
    MouseClick { x: i32, y: i32, button: MouseButtonType, mouse_up: bool, click_count: i32, modifiers: u32 },
    MouseMove { x: i32, y: i32, modifiers: u32, mouse_leave: bool },
    MouseWheel { x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32 },
    Key(KeyEvent),
    Focus(bool),
    CaptureLost,
    Frame(FrameCommand),
    ImeCommitText { text: String, relative_cursor_pos: i32 },
    ImeSetComposition { text: String, selection_start: u32, selection_end: u32 },
    ImeCancelComposition,
}

/// Wraps a CEF browser instance with offscreen rendering.
pub struct CefBrowserInstance {
    handler: Arc<Mutex<WebPreviewHandler>>,
    input_queue: Arc<Mutex<Vec<QueuedInput>>>,
}

unsafe impl Send for CefBrowserInstance {}

/// Everything the poll loop needs from a browser. Held separately from the view so that
/// pumping CEF, delivering input and picking up frames — which happens hundreds of times
/// a second — does not have to borrow the entity to find the browser first.
#[derive(Clone)]
pub struct BrowserChannel {
    handler: Arc<Mutex<WebPreviewHandler>>,
    input_queue: Arc<Mutex<Vec<QueuedInput>>>,
}

unsafe impl Send for BrowserChannel {}

impl BrowserChannel {
    /// Hands queued input to CEF, returning whether anything was delivered. Must run
    /// outside a GPUI entity update: CEF dispatches input synchronously and calls back
    /// into the handlers while doing so.
    pub fn flush_input(&self) -> bool {
        CefBrowserInstance::flush_queued(&self.input_queue, &self.handler)
    }

    /// Counter of page state changes CEF has reported, for skipping the sync when the
    /// page has not moved.
    pub fn state_version(&self) -> u64 {
        self.handler
            .lock()
            .ok()
            .map(|inner| inner.state_version.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn has_frame(&self) -> bool {
        self.handler
            .lock()
            .ok()
            .and_then(|inner| inner.frame.lock().ok().map(|frame| frame.is_some()))
            .unwrap_or(false)
    }
}

impl CefBrowserInstance {
    pub fn new(bounds: Bounds<Pixels>, scale_factor: f32) -> Result<Self> {
        ensure_cef_initialized()?;

        let width: f32 = bounds.size.width.into();
        let height: f32 = bounds.size.height.into();

        let handler = WebPreviewHandler::new(width as i32, height as i32, scale_factor);

        let mut client: Option<Client> = Some(WebPreviewClient::new(handler.clone()));

        // Configure WindowInfo for offscreen (windowless) rendering. Shared textures ask
        // the GPU process to hand us an IOSurface instead of rasterizing into system
        // memory for us to copy; there is no GPU process to ask in single-process mode.
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: shared_texture_enabled() as i32,
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };

        let settings = BrowserSettings {
            windowless_frame_rate: WINDOWLESS_FRAME_RATE,
            ..Default::default()
        };
        let url = CefString::from("about:blank");

        browser_host_create_browser(
            Some(&window_info),
            client.as_mut(),
            Some(&url),
            Some(&settings),
            None,
            None,
        );

        log::info!(
            "web_preview: browser created at {width}x{height} @{scale_factor}x, \
             shared textures {}, {WINDOWLESS_FRAME_RATE}fps",
            if window_info.shared_texture_enabled == 1 {
                "requested"
            } else {
                "off"
            }
        );

        // Creation completes on a pump. Deferring it to the poll task keeps CEF from
        // re-entering our handlers while GPUI is still laying this element out.
        request_pump();

        Ok(Self {
            handler,
            input_queue: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn channel(&self) -> BrowserChannel {
        BrowserChannel {
            handler: self.handler.clone(),
            input_queue: self.input_queue.clone(),
        }
    }

    /// Flush input and send to CEF. Called OUTSIDE of GPUI entity update
    /// to avoid re-entrancy with CEF's synchronous event processing.
    /// Returns whether any input reached the browser.
    fn flush_queued(
        input_queue: &Arc<Mutex<Vec<QueuedInput>>>,
        handler: &Arc<Mutex<WebPreviewHandler>>,
    ) -> bool {
        // Resolve the browser before draining the queue: input enqueued while CEF is still
        // starting up has to survive until there is somewhere to deliver it.
        //
        // The `Browser` is cloned out (a refcount bump) and the guard dropped before any CEF
        // call, because CEF dispatches input synchronously and re-enters our handlers while
        // doing so, and this is a non-reentrant `std::sync::Mutex`. Handlers may touch
        // `state` / `frame` / `pending_url`, never `browser`.
        let browser = {
            let browser_arc = {
                let Ok(inner) = handler.lock() else { return false };
                inner.browser.clone()
            };
            let Ok(browser_guard) = browser_arc.lock() else { return false };
            let Some(browser) = browser_guard.as_ref() else { return false };
            browser.clone()
        };
        let Some(host) = browser.host() else { return false };
        let frame = browser.main_frame();

        let events: Vec<QueuedInput> = {
            let Ok(mut queue) = input_queue.lock() else { return false };
            std::mem::take(&mut *queue)
        };

        if events.is_empty() {
            return false;
        }

        for input in &events {
            match input {
                QueuedInput::MouseClick { x, y, button, mouse_up, click_count, modifiers } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: *modifiers };
                    host.send_mouse_click_event(Some(&event), *button, *mouse_up as i32, *click_count);
                }
                QueuedInput::MouseMove { x, y, modifiers, mouse_leave } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: *modifiers };
                    host.send_mouse_move_event(Some(&event), *mouse_leave as i32);
                }
                QueuedInput::MouseWheel { x, y, delta_x, delta_y, modifiers } => {
                    let event = MouseEvent { x: *x, y: *y, modifiers: *modifiers };
                    host.send_mouse_wheel_event(Some(&event), *delta_x, *delta_y);
                }
                QueuedInput::Key(key_event) => {
                    host.send_key_event(Some(key_event));
                }
                QueuedInput::Focus(focused) => {
                    host.set_focus(*focused as i32);
                }
                QueuedInput::CaptureLost => {
                    host.send_capture_lost_event();
                }
                QueuedInput::Frame(command) => {
                    if let Some(frame) = frame.as_ref() {
                        match command {
                            FrameCommand::SelectAll => frame.select_all(),
                            FrameCommand::Copy => frame.copy(),
                            FrameCommand::Cut => frame.cut(),
                            FrameCommand::Paste => frame.paste(),
                            FrameCommand::Undo => frame.undo(),
                            FrameCommand::Redo => frame.redo(),
                        }
                    } else {
                        log::warn!("web_preview: dropping {command:?}, no main frame");
                    }
                }
                QueuedInput::ImeCommitText { text, relative_cursor_pos } => {
                    host.ime_commit_text(
                        Some(&CefString::from(text.as_str())),
                        None,
                        *relative_cursor_pos,
                    );
                }
                QueuedInput::ImeSetComposition { text, selection_start, selection_end } => {
                    let selection = Range { from: *selection_start, to: *selection_end };
                    host.ime_set_composition(
                        Some(&CefString::from(text.as_str())),
                        None,
                        None,
                        Some(&selection),
                    );
                }
                QueuedInput::ImeCancelComposition => {
                    host.ime_cancel_composition();
                }
            }
        }

        true
    }

    pub fn take_frame(&self) -> Option<BrowserFrame> {
        let inner = self.handler.lock().ok()?;
        let frame_arc = inner.frame.clone();
        drop(inner);
        let mut frame = frame_arc.lock().ok()?;
        frame.take()
    }

    /// Stops CEF from painting a browser nothing is showing. Offscreen rendering has no
    /// idea the tab went to the background, so it keeps rasterizing and handing us
    /// frames to convert and upload.
    pub fn set_hidden(&self, hidden: bool) {
        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                host.was_hidden(hidden as i32);
            }
        });
        request_pump();
    }

    pub fn resize(&self, width: i32, height: i32) {
        if let Ok(inner) = self.handler.lock() {
            if let Ok(mut w) = inner.view_width.lock() {
                *w = width;
            }
            if let Ok(mut h) = inner.view_height.lock() {
                *h = height;
            }
        }
        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                host.was_resized();
            }
        });
        // `was_resized` only marks the view dirty; CEF re-reads the view rect and
        // repaints on the next pump. Without asking for one the new size lands on
        // the idle pump instead, so the page visibly lags behind a drag-resize.
        request_pump();
    }

    /// Updates the scale factor Chromium renders the page at, which is what makes a
    /// page laid out for a Retina display look right on a 1x one and vice versa.
    ///
    /// Chromium caches the value from `screen_info` and only re-reads it when told, so
    /// without this a window moved to a display with a different scale keeps rendering
    /// the page at the zoom of the display its browser was created on.
    pub fn set_scale_factor(&self, scale_factor: f32) {
        let Some(current) = self
            .handler
            .lock()
            .ok()
            .map(|inner| inner.scale_factor.clone())
        else {
            return;
        };
        let Ok(mut current) = current.lock() else {
            return;
        };
        // Scale factors are display constants (1.0, 1.5, 2.0), not accumulated values.
        if (*current - scale_factor).abs() < 0.001 {
            return;
        }
        *current = scale_factor;
        drop(current);

        self.with_browser(|browser| {
            if let Some(host) = browser.host() {
                host.notify_screen_info_changed();
                // The view rect is unchanged in logical pixels but its pixel size is
                // not, so the frame CEF hands over has to be reallocated as well.
                host.was_resized();
            }
        });
        request_pump();
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

    /// Reads just the cursor, so rendering doesn't clone the URL and title strings.
    pub fn current_cursor(&self) -> Option<gpui::CursorStyle> {
        let state_arc = {
            let inner = self.handler.lock().ok()?;
            inner.state.clone()
        };
        let state = state_arc.lock().ok()?;
        Some(state.cursor)
    }

    pub fn navigate_to(&self, url: &str) {
        let url_string = if !url.contains("://") && !url.starts_with("about:") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };

        let loaded = self
            .with_browser(|browser| {
                let Some(frame) = browser.main_frame() else {
                    return false;
                };
                frame.load_url(Some(&CefString::from(url_string.as_str())));
                true
            })
            .unwrap_or(false);

        if !loaded {
            // The browser is still being created; `on_after_created` picks this up.
            if let Ok(inner) = self.handler.lock()
                && let Ok(mut pending) = inner.pending_url.lock()
            {
                *pending = Some(url_string);
            }
        }

        request_pump();
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

    pub fn send_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButtonType,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        self.queue(QueuedInput::MouseClick { x, y, button, mouse_up, click_count, modifiers });
    }

    pub fn send_mouse_move(&self, x: i32, y: i32, modifiers: u32, mouse_leave: bool) {
        self.queue(QueuedInput::MouseMove { x, y, modifiers, mouse_leave });
    }

    pub fn send_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        self.queue(QueuedInput::MouseWheel { x, y, delta_x, delta_y, modifiers });
    }

    pub fn send_key(&self, key_event: KeyEvent) {
        self.queue(QueuedInput::Key(key_event));
    }

    pub fn set_focus(&self, focused: bool) {
        self.queue(QueuedInput::Focus(focused));
    }

    /// Tells the renderer a held button will never be released, so a drag interrupted by the
    /// window losing focus does not leave the page stuck mid-selection.
    pub fn send_capture_lost(&self) {
        self.queue(QueuedInput::CaptureLost);
    }

    pub fn frame_command(&self, command: FrameCommand) {
        self.queue(QueuedInput::Frame(command));
    }

    /// Inserts finished text, i.e. what the macOS input system hands over once any dead-key
    /// or IME composition has resolved.
    pub fn ime_commit_text(&self, text: String, relative_cursor_pos: i32) {
        self.queue(QueuedInput::ImeCommitText { text, relative_cursor_pos });
    }

    /// Shows in-progress composition text (the underlined pending accent or candidate).
    pub fn ime_set_composition(&self, text: String, selection_start: u32, selection_end: u32) {
        self.queue(QueuedInput::ImeSetComposition { text, selection_start, selection_end });
    }

    pub fn ime_cancel_composition(&self) {
        self.queue(QueuedInput::ImeCancelComposition);
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
