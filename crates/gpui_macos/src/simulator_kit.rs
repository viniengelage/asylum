use crate::ns_string;
use anyhow::{Context as _, Result};
use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::{Pixels, Size};
use objc::{
    class, msg_send,
    runtime::{BOOL, Class, NO, Object},
    sel, sel_impl,
};
use std::ffi::c_void;

const SIMULATOR_KIT_PATH: &str =
    "/Applications/Xcode.app/Contents/SharedFrameworks/SimulatorKit.framework";
const OBJC_ASSOCIATION_RETAIN_NONATOMIC: usize = 1;
const MAIN_DEVICE_SCREEN_ID: u32 = 1;
const CONNECTED: i32 = 0;
const INVALID_SIMULATOR_KIT_OBJECTS: i32 = 1;
const CONNECTION_FAILED: i32 = 2;

static DEVICE_SCREEN_ASSOCIATION_KEY: u8 = 0;

unsafe extern "C" {
    fn objc_setAssociatedObject(
        object: *mut Object,
        key: *const c_void,
        value: *mut Object,
        policy: usize,
    );

    fn zed_simulator_kit_connect(display_view: *mut c_void, device_screen: *mut c_void) -> i32;
    fn zed_simulator_kit_resize(display_view: *mut c_void, width: f64, height: f64) -> i32;
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
        objc_setAssociatedObject(
            display_view,
            (&raw const DEVICE_SCREEN_ASSOCIATION_KEY).cast(),
            device_screen,
            OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
        let _: () = msg_send![device_screen, release];

        Ok(display_view)
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
