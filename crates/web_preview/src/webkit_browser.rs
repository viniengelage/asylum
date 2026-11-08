use crate::browser_import::{ImportedCookie, SameSite};
use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, Pixels, Window};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, extern_class, msg_send, sel};
use objc2_app_kit::{NSResponder, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::rc::Rc;

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

const COMMAND_FLAG: usize = 1 << 20;
const SHIFT_FLAG: usize = 1 << 17;
const CONTROL_FLAG: usize = 1 << 18;
const OPTION_FLAG: usize = 1 << 19;
/// `NSViewWidthSizable | NSViewHeightSizable`: the page follows the host window's size.
const RESIZE_WITH_HOST: usize = 2 | 16;
/// `NSURLErrorCancelled`: the navigation was stopped or replaced.
const URL_ERROR_CANCELLED: isize = -999;
/// `NSURLErrorCannotConnectToHost`, which is what a dev server that isn't running gives.
const URL_ERROR_CANNOT_CONNECT: isize = -1004;
/// `WebKitErrorFrameLoadInterruptedByPolicyChange`: a download or a page WebKit won't show,
/// not a failure to load.
const WEBKIT_ERROR_POLICY_CHANGE: isize = 102;

extern_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WKWebView"]
    struct WKWebView;
);

define_class!(
    // SAFETY: `WKWebView` may be subclassed, and this subclass adds no ivars.
    #[unsafe(super(WKWebView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AsylumWebView"]
    struct AsylumWebView;

    impl AsylumWebView {
        /// Asylum's menus run GPUI actions rather than the standard editing selectors, so
        /// while the page is key nothing would send it `copy:` or `paste:`.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &AnyObject) -> Bool {
            if let Some(action) = editing_action(event) {
                let handled: Bool = unsafe {
                    msg_send![self, tryToPerform: action, with: std::ptr::null_mut::<AnyObject>()]
                };
                if handled.as_bool() {
                    return Bool::YES;
                }
            }
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }
    }
);

fn editing_action(event: &AnyObject) -> Option<Sel> {
    let flags: usize = unsafe { msg_send![event, modifierFlags] };
    let characters: Option<Retained<NSString>> =
        unsafe { msg_send![event, charactersIgnoringModifiers] };
    let key = characters?.to_string().to_lowercase();
    let modifiers = flags & (COMMAND_FLAG | SHIFT_FLAG | CONTROL_FLAG | OPTION_FLAG);
    match (key.as_str(), modifiers) {
        ("c", COMMAND_FLAG) => Some(sel!(copy:)),
        ("x", COMMAND_FLAG) => Some(sel!(cut:)),
        ("v", COMMAND_FLAG) => Some(sel!(paste:)),
        ("a", COMMAND_FLAG) => Some(sel!(selectAll:)),
        ("z", COMMAND_FLAG) => Some(sel!(undo:)),
        ("z", modifiers) if modifiers == COMMAND_FLAG | SHIFT_FLAG => Some(sel!(redo:)),
        _ => None,
    }
}

/// A main-frame navigation that failed.
#[derive(Clone, Debug, PartialEq)]
pub struct WebKitLoadError {
    pub connection_refused: bool,
    pub url: String,
    pub text: String,
}

#[derive(Default)]
struct NavigationEvents {
    load_error: Option<WebKitLoadError>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and `define_class!` drops the ivars.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AsylumWebKitNavigationDelegate"]
    #[ivars = Rc<RefCell<NavigationEvents>>]
    struct NavigationDelegate;

    unsafe impl NSObjectProtocol for NavigationDelegate {}

    impl NavigationDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start_provisional_navigation(&self, _web_view: &AnyObject, _navigation: *mut AnyObject) {
            self.ivars().borrow_mut().load_error = None;
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            _web_view: &AnyObject,
            _navigation: *mut AnyObject,
            error: &AnyObject,
        ) {
            self.record_failure(error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(&self, _web_view: &AnyObject, _navigation: *mut AnyObject, error: &AnyObject) {
            self.record_failure(error);
        }

        /// Links with `target="_blank"` and `window.open` ask for a new window; the tab
        /// follows them instead, the way the Chromium engine's tab does.
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            web_view: &AnyObject,
            _configuration: &AnyObject,
            navigation_action: &AnyObject,
            _window_features: &AnyObject,
        ) -> *mut AnyObject {
            let request: *mut AnyObject = unsafe { msg_send![navigation_action, request] };
            if !request.is_null() {
                let _navigation: *mut AnyObject = unsafe { msg_send![web_view, loadRequest: request] };
            }
            std::ptr::null_mut()
        }
    }
);

impl NavigationDelegate {
    fn new(main_thread: MainThreadMarker, events: Rc<RefCell<NavigationEvents>>) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(events);
        unsafe { msg_send![super(this), init] }
    }

    fn record_failure(&self, error: &AnyObject) {
        let code: isize = unsafe { msg_send![error, code] };
        let domain: Option<Retained<NSString>> = unsafe { msg_send![error, domain] };
        let domain = domain.map(|domain| domain.to_string()).unwrap_or_default();
        if code == URL_ERROR_CANCELLED
            || (domain == "WebKitErrorDomain" && code == WEBKIT_ERROR_POLICY_CHANGE)
        {
            return;
        }
        let text: Option<Retained<NSString>> = unsafe { msg_send![error, localizedDescription] };
        let user_info: Option<Retained<AnyObject>> = unsafe { msg_send![error, userInfo] };
        let url = user_info
            .and_then(|user_info| {
                let key = NSString::from_str("NSErrorFailingURLStringKey");
                let value: Option<Retained<AnyObject>> =
                    unsafe { msg_send![&*user_info, objectForKey: &*key] };
                value
            })
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|url| url.to_string())
            .unwrap_or_default();
        self.ivars().borrow_mut().load_error = Some(WebKitLoadError {
            connection_refused: code == URL_ERROR_CANNOT_CONNECT,
            url,
            text: text.map(|text| text.to_string()).unwrap_or_default(),
        });
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebKitState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub load_progress: f64,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub load_error: Option<WebKitLoadError>,
}

/// A page shown by the WebKit that ships with macOS. It is a native view, so it is hosted
/// above GPUI's own drawing and gets mouse and keyboard input straight from AppKit.
pub struct WebKitPage {
    web_view: Retained<AsylumWebView>,
    // WebKit keeps only a weak reference to its delegates.
    _delegate: Retained<NavigationDelegate>,
    events: Rc<RefCell<NavigationEvents>>,
    subview_id: Option<u64>,
    bounds: Option<Bounds<Pixels>>,
    hidden: bool,
}

impl WebKitPage {
    pub fn new() -> Result<Self> {
        let main_thread =
            MainThreadMarker::new().context("WebKit pages can only be made on the main thread")?;
        let events = Rc::new(RefCell::new(NavigationEvents::default()));
        let delegate = NavigationDelegate::new(main_thread, events.clone());
        let web_view = unsafe {
            let configuration: Retained<AnyObject> =
                msg_send![class!(WKWebViewConfiguration), new];
            let store = website_data_store()?;
            let _: () = msg_send![&*configuration, setWebsiteDataStore: &*store];
            // The Web Inspector's "Inspect Element" item in the page's context menu.
            let preferences: Retained<AnyObject> = msg_send![&*configuration, preferences];
            let enabled: Retained<AnyObject> = msg_send![class!(NSNumber), numberWithBool: Bool::YES];
            let key = NSString::from_str("developerExtrasEnabled");
            let _: () = msg_send![&*preferences, setValue: &*enabled, forKey: &*key];

            let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(800., 600.));
            let allocated: Allocated<AsylumWebView> = AsylumWebView::alloc(main_thread);
            let web_view: Retained<AsylumWebView> =
                msg_send![allocated, initWithFrame: frame, configuration: &*configuration];
            let _: () = msg_send![&*web_view, setAutoresizingMask: RESIZE_WITH_HOST];
            let _: () = msg_send![&*web_view, setAllowsBackForwardNavigationGestures: Bool::YES];
            let _: () = msg_send![&*web_view, setNavigationDelegate: &*delegate];
            let _: () = msg_send![&*web_view, setUIDelegate: &*delegate];
            if responds_to(&web_view, sel!(setInspectable:)) {
                let _: () = msg_send![&*web_view, setInspectable: Bool::YES];
            }
            web_view
        };
        Ok(Self {
            web_view,
            _delegate: delegate,
            events,
            subview_id: None,
            bounds: None,
            hidden: false,
        })
    }

    /// Mounts the page over `bounds` the first time, and follows the bounds afterwards.
    pub fn place(&mut self, bounds: Bounds<Pixels>, window: &Window) {
        match self.subview_id {
            Some(subview_id) => {
                if self.bounds != Some(bounds) {
                    window.update_native_subview_bounds(subview_id, bounds);
                    self.bounds = Some(bounds);
                }
            }
            None => {
                // The host takes over one reference; this page keeps its own.
                let view = Retained::into_raw(self.web_view.clone());
                // SAFETY: `view` is an owned, initialized NSView.
                self.subview_id = unsafe { window.add_native_subview(view.cast(), bounds) };
                if self.subview_id.is_none() {
                    // SAFETY: the host refused the view, so the reference is still ours.
                    drop(unsafe { Retained::from_raw(view) });
                }
                self.bounds = Some(bounds);
            }
        }
    }

    pub fn unmount(&mut self, window: &Window) {
        if let Some(subview_id) = self.subview_id.take() {
            window.remove_native_subview(subview_id);
        }
        self.bounds = None;
    }

    /// Hides the page without a window at hand. The host window it sits in is transparent,
    /// so with the page hidden clicks fall through to GPUI underneath.
    pub fn set_hidden(&mut self, hidden: bool) {
        if self.hidden != hidden {
            self.hidden = hidden;
            let _: () = unsafe { msg_send![&*self.web_view, setHidden: Bool::new(hidden)] };
        }
    }

    pub fn navigate_to(&self, url: &str) {
        let address = if !url.contains("://") && !url.starts_with("about:") {
            format!("https://{url}")
        } else {
            url.to_string()
        };
        let address = NSString::from_str(&address);
        unsafe {
            let url: Option<Retained<AnyObject>> =
                msg_send![class!(NSURL), URLWithString: &*address];
            let Some(url) = url else {
                log::warn!("web_preview: WebKit could not parse {address}");
                return;
            };
            let request: Retained<AnyObject> =
                msg_send![class!(NSURLRequest), requestWithURL: &*url];
            let _navigation: *mut AnyObject = msg_send![&*self.web_view, loadRequest: &*request];
        }
    }

    pub fn reload(&self) {
        let _navigation: *mut AnyObject = unsafe { msg_send![&*self.web_view, reload] };
    }

    pub fn stop(&self) {
        let _: () = unsafe { msg_send![&*self.web_view, stopLoading] };
    }

    pub fn go_back(&self) {
        let _navigation: *mut AnyObject = unsafe { msg_send![&*self.web_view, goBack] };
    }

    pub fn go_forward(&self) {
        let _navigation: *mut AnyObject = unsafe { msg_send![&*self.web_view, goForward] };
    }

    pub fn execute_javascript(&self, code: &str) {
        let code = NSString::from_str(code);
        let _: () = unsafe {
            msg_send![
                &*self.web_view,
                evaluateJavaScript: &*code,
                completionHandler: std::ptr::null_mut::<AnyObject>()
            ]
        };
    }

    /// Takes Chromium's zoom level, where each step is a factor of 1.2 and 0 is 100%, so the
    /// tab's zoom controls work the same with both engines.
    pub fn set_zoom_level(&self, level: f64) {
        if responds_to(&self.web_view, sel!(setPageZoom:)) {
            let factor = 1.2f64.powf(level);
            let _: () = unsafe { msg_send![&*self.web_view, setPageZoom: factor] };
        }
    }

    /// Opens the Web Inspector. WebKit has no public call for this, so it is skipped where
    /// the private one is missing; "Inspect Element" in the page's menu still works.
    pub fn open_inspector(&self) {
        if !responds_to(&self.web_view, sel!(_inspector)) {
            return;
        }
        unsafe {
            let inspector: *mut AnyObject = msg_send![&*self.web_view, _inspector];
            if !inspector.is_null() {
                let _: () = msg_send![inspector, show];
            }
        }
    }

    pub fn state(&self) -> WebKitState {
        unsafe {
            let url: Option<Retained<AnyObject>> = msg_send![&*self.web_view, URL];
            let url = url
                .and_then(|url| {
                    let text: Option<Retained<NSString>> = msg_send![&*url, absoluteString];
                    text
                })
                .map(|text| text.to_string())
                .unwrap_or_default();
            let title: Option<Retained<NSString>> = msg_send![&*self.web_view, title];
            let loading: Bool = msg_send![&*self.web_view, isLoading];
            let load_progress: f64 = msg_send![&*self.web_view, estimatedProgress];
            let can_go_back: Bool = msg_send![&*self.web_view, canGoBack];
            let can_go_forward: Bool = msg_send![&*self.web_view, canGoForward];
            WebKitState {
                url,
                title: title.map(|title| title.to_string()).unwrap_or_default(),
                loading: loading.as_bool(),
                load_progress,
                can_go_back: can_go_back.as_bool(),
                can_go_forward: can_go_forward.as_bool(),
                load_error: self.events.borrow().load_error.clone(),
            }
        }
    }
}

fn responds_to(object: &AnyObject, selector: Sel) -> bool {
    let responds: Bool = unsafe { msg_send![object, respondsToSelector: selector] };
    responds.as_bool()
}

/// The data store of the active Asylum profile, so each profile keeps its own logins the
/// way it does with Chromium. Before macOS 14 every profile shares the default store.
fn website_data_store() -> Result<Retained<AnyObject>> {
    let store_class: &AnyClass = class!(WKWebsiteDataStore);
    let per_profile: Bool =
        unsafe { msg_send![store_class, respondsToSelector: sel!(dataStoreForIdentifier:)] };
    if per_profile.as_bool() {
        let identifier = NSString::from_str(&profile_store_identifier());
        let allocated: Allocated<AnyObject> = unsafe { msg_send![class!(NSUUID), alloc] };
        let uuid: Option<Retained<AnyObject>> =
            unsafe { msg_send![allocated, initWithUUIDString: &*identifier] };
        if let Some(uuid) = uuid {
            let store: Option<Retained<AnyObject>> =
                unsafe { msg_send![store_class, dataStoreForIdentifier: &*uuid] };
            if let Some(store) = store {
                return Ok(store);
            }
        }
    }
    let store: Option<Retained<AnyObject>> = unsafe { msg_send![store_class, defaultDataStore] };
    store.ok_or_else(|| anyhow!("WebKit has no website data store"))
}

/// WebKit names data stores by UUID; this one is derived from the profile id so it stays the
/// same across launches.
fn profile_store_identifier() -> String {
    let profile = paths::active_profile_id().unwrap_or(paths::DEFAULT_PROFILE_ID);
    let digest = Sha256::digest(format!("asylum-browser:{profile}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Version 4 and the RFC 4122 variant, so the string parses as an ordinary UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn cookie_store() -> Result<Retained<AnyObject>> {
    let store = website_data_store()?;
    let cookies: Option<Retained<AnyObject>> = unsafe { msg_send![&*store, httpCookieStore] };
    cookies.ok_or_else(|| anyhow!("WebKit's data store has no cookie store"))
}

fn ns_cookie(cookie: &ImportedCookie) -> Option<Retained<AnyObject>> {
    unsafe {
        let properties: Retained<AnyObject> = msg_send![class!(NSMutableDictionary), new];
        let set = |key: &str, value: &AnyObject| {
            let key = NSString::from_str(key);
            let _: () = msg_send![&*properties, setObject: value, forKey: &*key];
        };
        let text = |value: &str| NSString::from_str(value);
        set("Name", &text(&cookie.name));
        set("Value", &text(&cookie.value));
        // NSHTTPCookie reads a domain without a leading dot as host-only, like the browsers
        // these cookies come from.
        set("Domain", &text(&cookie.domain));
        set("Path", &text(&cookie.path));
        if cookie.secure {
            set("Secure", &text("TRUE"));
        }
        if cookie.http_only {
            set("HttpOnly", &text("TRUE"));
        }
        match cookie.same_site {
            SameSite::Lax => set("SameSite", &text("lax")),
            SameSite::Strict => set("SameSite", &text("strict")),
            SameSite::None | SameSite::Unspecified => {}
        }
        if let Some(expires) = cookie.expires {
            let unix_seconds = expires as f64 / 1_000_000. - 11_644_473_600.;
            let date: Retained<AnyObject> =
                msg_send![class!(NSDate), dateWithTimeIntervalSince1970: unix_seconds];
            set("Expires", &date);
        }
        msg_send![class!(NSHTTPCookie), cookieWithProperties: &*properties]
    }
}

/// Writes imported cookies into the active profile's WebKit store and returns how many it
/// took.
pub fn import_cookies(cookies: &[ImportedCookie]) -> Result<usize> {
    let store = cookie_store()?;
    let mut accepted = 0;
    for cookie in cookies {
        let Some(ns_cookie) = ns_cookie(cookie) else {
            continue;
        };
        let _: () = unsafe {
            msg_send![
                &*store,
                setCookie: &*ns_cookie,
                completionHandler: std::ptr::null_mut::<AnyObject>()
            ]
        };
        accepted += 1;
    }
    Ok(accepted)
}

/// Deletes the named cookies of each address from the active profile's WebKit store.
pub fn delete_cookies(cookies: &[(String, String)]) -> Result<()> {
    let store = cookie_store()?;
    for (url, name) in cookies {
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            continue;
        };
        // Deleting takes the cookie object itself; one built from the same name, domain and
        // path matches the stored one.
        for domain in [host.clone(), format!(".{host}")] {
            let cookie = ImportedCookie {
                name: name.clone(),
                value: String::new(),
                domain,
                path: url::Url::parse(url)
                    .map(|url| url.path().to_string())
                    .unwrap_or_else(|_| "/".into()),
                secure: url.starts_with("https://"),
                http_only: false,
                expires: None,
                same_site: SameSite::Unspecified,
            };
            if let Some(ns_cookie) = ns_cookie(&cookie) {
                let _: () = unsafe {
                    msg_send![
                        &*store,
                        deleteCookie: &*ns_cookie,
                        completionHandler: std::ptr::null_mut::<AnyObject>()
                    ]
                };
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_store_identifier_is_a_stable_uuid() {
        let identifier = profile_store_identifier();
        assert_eq!(identifier, profile_store_identifier());
        let groups: Vec<usize> = identifier.split('-').map(str::len).collect();
        assert_eq!(groups, vec![8, 4, 4, 4, 12]);
        assert_eq!(identifier.chars().nth(14), Some('4'));
    }
}
