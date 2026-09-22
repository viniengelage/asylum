use crate::ns_string;
use anyhow::{Context as _, Result};
use cocoa::base::{id, nil};
use objc::{
    class, msg_send,
    runtime::{BOOL, Class, NO, YES},
    sel, sel_impl,
};

const CORE_SIMULATOR_PATH: &str = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework";
/// The layout CoreSimulator reports for the attached keyboard, in `LMGetKbdType` terms;
/// 40 is the ANSI layout every current Apple keyboard reports.
const ANSI_KEYBOARD_TYPE: u8 = 40;

pub(crate) fn sim_device_for_udid(udid: &str) -> Result<id> {
    unsafe {
        load_framework(CORE_SIMULATOR_PATH)?;

        let service_context_class = Class::get("SimServiceContext")
            .context("CoreSimulator did not register SimServiceContext")?;
        let mut error: id = nil;
        let service_context: id = msg_send![
            service_context_class,
            sharedServiceContextForDeveloperDir: nil
            error: &mut error
        ];
        anyhow::ensure!(
            service_context != nil,
            "CoreSimulator could not create a shared service context"
        );

        let device_set: id = msg_send![service_context, defaultDeviceSetWithError: &mut error];
        anyhow::ensure!(
            device_set != nil,
            "CoreSimulator could not load the default device set"
        );

        let devices_by_udid: id = msg_send![device_set, devicesByUDID];
        let device: id = msg_send![devices_by_udid, objectForKey: ns_string(udid)];
        if device != nil {
            return Ok(device);
        }

        // `devicesByUDID` is private and its key type has changed across Xcode releases.
        // Match the documented runtime property on each device instead of assuming NSString keys.
        let devices: id = msg_send![devices_by_udid, allValues];
        let device_count: usize = msg_send![devices, count];
        for device_index in 0..device_count {
            let device: id = msg_send![devices, objectAtIndex: device_index];
            let device_udid: id = msg_send![device, UDID];
            let device_udid_description: id = msg_send![device_udid, description];
            let matches: BOOL =
                msg_send![device_udid_description, isEqualToString: ns_string(udid)];
            if matches != NO {
                return Ok(device);
            }
        }

        anyhow::bail!(
            "CoreSimulator could not find device {udid} in the default device set ({device_count} devices)"
        );
    }
}

/// Attaches or detaches the guest's hardware keyboard. iOS only draws its on-screen keyboard
/// while it believes no hardware keyboard is attached, so detaching is how Simulator.app's
/// "Connect Hardware Keyboard" reveals it.
pub(crate) fn set_hardware_keyboard_enabled(udid: &str, enabled: bool) -> Result<()> {
    unsafe {
        let device = sim_device_for_udid(udid)?;
        let mut error: id = nil;
        let changed: BOOL = msg_send![
            device,
            setHardwareKeyboardEnabled: if enabled { YES } else { NO }
            keyboardType: ANSI_KEYBOARD_TYPE
            error: &mut error
        ];
        anyhow::ensure!(
            changed != NO,
            "CoreSimulator could not {} the hardware keyboard of {udid}: {}",
            if enabled { "attach" } else { "detach" },
            describe_error(error)
        );
        Ok(())
    }
}

unsafe fn describe_error(error: id) -> String {
    unsafe {
        if error == nil {
            return "no error reported".to_string();
        }
        let description: id = msg_send![error, localizedDescription];
        if description == nil {
            return "no error reported".to_string();
        }
        let utf8: *const std::os::raw::c_char = msg_send![description, UTF8String];
        if utf8.is_null() {
            return "no error reported".to_string();
        }
        std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

unsafe fn load_framework(path: &str) -> Result<()> {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), bundleWithPath: ns_string(path)];
        anyhow::ensure!(bundle != nil, "CoreSimulator was not found at {path}");

        let loaded: BOOL = msg_send![bundle, load];
        anyhow::ensure!(loaded != NO, "CoreSimulator could not be loaded");
        Ok(())
    }
}
