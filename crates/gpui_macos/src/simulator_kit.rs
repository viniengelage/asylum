use crate::ns_string;
use anyhow::{Context as _, Result};
use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::{Pixels, Size};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use objc2_app_kit::{
    NSWorkspaceApplicationKey, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification,
};
use std::{
    ffi::{CStr, c_void},
    sync::Once,
};

const SIMULATOR_KIT_PATH: &str =
    "/Applications/Xcode.app/Contents/SharedFrameworks/SimulatorKit.framework";
const OBJC_ASSOCIATION_RETAIN_NONATOMIC: usize = 1;
const MAIN_DEVICE_SCREEN_ID: u32 = 1;
const CONNECTED: i32 = 0;
const INVALID_SIMULATOR_KIT_OBJECTS: i32 = 1;
const CONNECTION_FAILED: i32 = 2;

static DEVICE_SCREEN_ASSOCIATION_KEY: u8 = 0;

/// DeviceHub replaced Simulator.app in Xcode 27; both are listed so older Xcodes behave the same.
const DEVICE_HUB_BUNDLE_IDENTIFIERS: [&[u8]; 2] =
    [b"com.apple.dt.Devices", b"com.apple.iphonesimulator"];

unsafe extern "C" {
    fn objc_setAssociatedObject(
        object: *mut Object,
        key: *const c_void,
        value: *mut Object,
        policy: usize,
    );

    fn zed_simulator_kit_connect(display_view: *mut c_void, device_screen: *mut c_void) -> i32;
    fn zed_simulator_kit_resize(display_view: *mut c_void, width: f64, height: f64) -> i32;
    fn zed_simulator_kit_set_show_device_chrome(display_view: *mut c_void, show: bool);
}

pub(crate) fn sim_display_view_class() -> Result<&'static Class> {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), bundleWithPath: ns_string(SIMULATOR_KIT_PATH)];
        anyhow::ensure!(
            bundle != nil,
            "SimulatorKit was not found at {SIMULATOR_KIT_PATH}"
        );

        let loaded: BOOL = msg_send![bundle, load];
        anyhow::ensure!(loaded != NO, "SimulatorKit could not be loaded");

        Class::get("SimulatorKit.SimDisplayView")
            .context("SimulatorKit did not register SimulatorKit.SimDisplayView")
    }
}

pub(crate) fn create_sim_display_view(device: id, size: Size<Pixels>) -> Result<id> {
    unsafe {
        let display_view_class = sim_display_view_class()?;
        let display_view: id = msg_send![display_view_class, alloc];
        let display_view: id = msg_send![
            display_view,
            initWithFrame: NSRect::new(
                NSPoint::new(0., 0.),
                NSSize::new(size.width.to_f64(), size.height.to_f64())
            )
        ];
        anyhow::ensure!(
            display_view != nil,
            "SimulatorKit could not create SimDisplayView"
        );
        let device_screen_class = Class::get("SimulatorKit.SimDeviceScreen")
            .context("SimulatorKit did not register SimulatorKit.SimDeviceScreen")?;
        let device_screen: id = msg_send![device_screen_class, alloc];
        // CoreSimulator reserves screen ID 0; the integrated device display is ID 1.
        let device_screen: id = msg_send![
            device_screen,
            initWithDevice: device
            screenID: MAIN_DEVICE_SCREEN_ID
        ];
        anyhow::ensure!(
            device_screen != nil,
            "SimulatorKit could not create SimDeviceScreen"
        );

        if let Err(error) = connect_sim_display_view(display_view, device_screen) {
            let _: () = msg_send![device_screen, release];
            let _: () = msg_send![display_view, release];
            return Err(error);
        }
        zed_simulator_kit_set_show_device_chrome(display_view.cast(), false);
        objc_setAssociatedObject(
            display_view,
            (&raw const DEVICE_SCREEN_ASSOCIATION_KEY).cast(),
            device_screen,
            OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
        let _: () = msg_send![device_screen, release];

        hide_device_hub_while_embedded();
        Ok(display_view)
    }
}

/// `expo run:ios` and `react-native run-ios` open DeviceHub on every build and launch, on top
/// of the simulator already embedded here. They only check that the app is running, so it is
/// hidden rather than quit: quitting makes Expo wait for it until it times out, and the next
/// launch would open it again anyway.
fn hide_device_hub_while_embedded() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| unsafe {
        let Some(mut decl) = ClassDecl::new("GPUIDeviceHubObserver", class!(NSObject)) else {
            log::error!("could not declare the DeviceHub observer class");
            return;
        };
        decl.add_method(
            sel!(applicationDidChange:),
            hide_embedded_device_hub as extern "C" fn(&Object, Sel, id),
        );
        let observer_class = decl.register();
        // Lives as long as the process, like the notification registrations pointing at it.
        let observer: id = msg_send![observer_class, new];

        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let center: id = msg_send![workspace, notificationCenter];
        for name in [
            NSWorkspaceDidLaunchApplicationNotification,
            NSWorkspaceDidActivateApplicationNotification,
        ] {
            let name: *const objc2_foundation::NSString = name;
            let _: () = msg_send![
                center,
                addObserver: observer
                selector: sel!(applicationDidChange:)
                name: name as id
                object: nil
            ];
        }
    });
}

extern "C" fn hide_embedded_device_hub(_: &Object, _: Sel, notification: id) {
    unsafe {
        let user_info: id = msg_send![notification, userInfo];
        let application_key: *const objc2_foundation::NSString = NSWorkspaceApplicationKey;
        let application: id = msg_send![user_info, objectForKey: application_key as id];
        if application == nil || !is_device_hub(application) || !has_embedded_simulator() {
            return;
        }

        let was_active: BOOL = msg_send![application, isActive];
        let _: BOOL = msg_send![application, hide];
        // Hiding the frontmost app does not hand focus back to the workspace it covered.
        if was_active != NO {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, activateIgnoringOtherApps: YES];
        }
    }
}

unsafe fn is_device_hub(application: id) -> bool {
    unsafe {
        let bundle_identifier: id = msg_send![application, bundleIdentifier];
        if bundle_identifier == nil {
            return false;
        }
        let utf8: *const std::os::raw::c_char = msg_send![bundle_identifier, UTF8String];
        if utf8.is_null() {
            return false;
        }
        let bundle_identifier = CStr::from_ptr(utf8).to_bytes();
        DEVICE_HUB_BUNDLE_IDENTIFIERS.contains(&bundle_identifier)
    }
}

/// A display view hidden along with its tab still counts: the device is in use here, and
/// DeviceHub coming forward would only cover the workspace.
unsafe fn has_embedded_simulator() -> bool {
    unsafe {
        let Some(display_view_class) = Class::get("SimulatorKit.SimDisplayView") else {
            return false;
        };
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let windows: id = msg_send![app, windows];
        let window_count: usize = msg_send![windows, count];
        (0..window_count).any(|window_index| {
            let window: id = msg_send![windows, objectAtIndex: window_index];
            let content_view: id = msg_send![window, contentView];
            if content_view == nil {
                return false;
            }
            let subviews: id = msg_send![content_view, subviews];
            let subview_count: usize = msg_send![subviews, count];
            (0..subview_count).any(|subview_index| {
                let subview: id = msg_send![subviews, objectAtIndex: subview_index];
                let is_display_view: BOOL = msg_send![subview, isKindOfClass: display_view_class];
                is_display_view != NO
            })
        })
    }
}

pub(crate) fn resize_sim_display_view(display_view: id, size: Size<Pixels>) {
    let status = unsafe {
        zed_simulator_kit_resize(
            display_view.cast(),
            size.width.to_f64(),
            size.height.to_f64(),
        )
    };
    // Status codes are documented on `zedSimulatorKitResize` in
    // simulator_kit_bridge.swift. Status 2 (no frame received yet) is expected
    // right after connecting, before the device produces its first frame.
    match status {
        0 | 2 => {}
        3 => log::warn!(
            "SimulatorKit did not adopt the requested display size; \
             the simulator may be rendering at its native resolution and \
             getting scaled by the window server on every frame"
        ),
        status => log::warn!("SimulatorKit display resize failed (status {status})"),
    }
}

/// Centers the display view inside its host container of `container_size`,
/// preserving the size SimulatorKit computed for the current display frame.
pub(crate) fn fit_sim_display_view(display_view: id, container_size: Size<Pixels>) {
    unsafe {
        let intrinsic_size: NSSize = msg_send![display_view, intrinsicContentSize];
        if intrinsic_size.width <= 0. || intrinsic_size.height <= 0. {
            return;
        }

        let frame = NSRect::new(
            NSPoint::new(
                (container_size.width.to_f64() - intrinsic_size.width) / 2.,
                (container_size.height.to_f64() - intrinsic_size.height) / 2.,
            ),
            intrinsic_size,
        );
        let _: () = msg_send![display_view, setFrame: frame];
    }
}

unsafe fn connect_sim_display_view(display_view: id, device_screen: id) -> Result<()> {
    unsafe {
        match zed_simulator_kit_connect(display_view.cast(), device_screen.cast()) {
            CONNECTED => Ok(()),
            INVALID_SIMULATOR_KIT_OBJECTS => anyhow::bail!(
                "SimulatorKit could not connect the display because its display or device screen is invalid"
            ),
            CONNECTION_FAILED => {
                anyhow::bail!("SimulatorKit could not connect the display to the device screen")
            }
            result => {
                anyhow::bail!("SimulatorKit returned an unknown connection result ({result})")
            }
        }
    }
}

const NS_EVENT_TYPE_LEFT_MOUSE_DOWN: u64 = 1;
const NS_EVENT_TYPE_LEFT_MOUSE_UP: u64 = 2;
const NS_EVENT_TYPE_LEFT_MOUSE_DRAGGED: u64 = 6;
const NS_EVENT_TYPE_KEY_DOWN: u64 = 10;
const NS_EVENT_TYPE_KEY_UP: u64 = 11;
const NS_EVENT_MODIFIER_FLAG_SHIFT: u64 = 1 << 17;
const NS_EVENT_MODIFIER_FLAG_COMMAND: u64 = 1 << 20;

/// Builds an `NSEvent` in the display view's window and hands it to the view's own handlers.
/// SimDisplayView turns mouse events into touches and key events into HID keys, which is the
/// same path a real click or keystroke on the simulator takes.
pub(crate) fn send_sim_display_input(display_view: id, input: gpui::SimulatorInput) -> Result<()> {
    unsafe {
        let window: id = msg_send![display_view, window];
        anyhow::ensure!(window != nil, "the simulator display has no window");
        let window_number: isize = msg_send![window, windowNumber];
        let process_info: id = msg_send![class!(NSProcessInfo), processInfo];
        let timestamp: f64 = msg_send![process_info, systemUptime];
        let bounds: NSRect = msg_send![display_view, bounds];
        anyhow::ensure!(
            bounds.size.width > 0. && bounds.size.height > 0.,
            "the simulator display has no size yet"
        );

        match input {
            gpui::SimulatorInput::Pointer { x, y, phase } => {
                let flipped: BOOL = msg_send![display_view, isFlipped];
                let local_x = bounds.origin.x + x.clamp(0., 1.) * bounds.size.width;
                let from_top = y.clamp(0., 1.) * bounds.size.height;
                let local_y = if flipped != NO {
                    bounds.origin.y + from_top
                } else {
                    bounds.origin.y + bounds.size.height - from_top
                };
                let location: NSPoint = msg_send![
                    display_view,
                    convertPoint: NSPoint::new(local_x, local_y)
                    toView: nil
                ];
                let (event_type, pressure) = match phase {
                    gpui::SimulatorPointerPhase::Down => (NS_EVENT_TYPE_LEFT_MOUSE_DOWN, 1.0f32),
                    gpui::SimulatorPointerPhase::Drag => (NS_EVENT_TYPE_LEFT_MOUSE_DRAGGED, 1.0f32),
                    gpui::SimulatorPointerPhase::Up => (NS_EVENT_TYPE_LEFT_MOUSE_UP, 0.0f32),
                };
                let event: id = msg_send![
                    class!(NSEvent),
                    mouseEventWithType: event_type
                    location: location
                    modifierFlags: 0u64
                    timestamp: timestamp
                    windowNumber: window_number
                    context: nil
                    eventNumber: 0isize
                    clickCount: 1isize
                    pressure: pressure
                ];
                anyhow::ensure!(event != nil, "could not create the pointer event");
                match phase {
                    gpui::SimulatorPointerPhase::Down => {
                        let _: () = msg_send![display_view, mouseDown: event];
                    }
                    gpui::SimulatorPointerPhase::Drag => {
                        let _: () = msg_send![display_view, mouseDragged: event];
                    }
                    gpui::SimulatorPointerPhase::Up => {
                        let _: () = msg_send![display_view, mouseUp: event];
                    }
                }
            }
            gpui::SimulatorInput::Key {
                key_code,
                characters,
                shift,
                command,
                down,
            } => {
                let mut flags = 0u64;
                if shift {
                    flags |= NS_EVENT_MODIFIER_FLAG_SHIFT;
                }
                if command {
                    flags |= NS_EVENT_MODIFIER_FLAG_COMMAND;
                }
                let characters = ns_string(&characters);
                let event: id = msg_send![
                    class!(NSEvent),
                    keyEventWithType: if down { NS_EVENT_TYPE_KEY_DOWN } else { NS_EVENT_TYPE_KEY_UP }
                    location: NSPoint::new(0., 0.)
                    modifierFlags: flags
                    timestamp: timestamp
                    windowNumber: window_number
                    context: nil
                    characters: characters
                    charactersIgnoringModifiers: characters
                    isARepeat: NO
                    keyCode: key_code
                ];
                anyhow::ensure!(event != nil, "could not create the key event");
                if down {
                    let _: () = msg_send![display_view, keyDown: event];
                } else {
                    let _: () = msg_send![display_view, keyUp: event];
                }
            }
        }
        Ok(())
    }
}
