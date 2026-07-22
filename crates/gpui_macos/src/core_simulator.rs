use crate::ns_string;
use anyhow::{Context as _, Result};
use cocoa::base::{id, nil};
use objc::{
    class, msg_send,
    runtime::{BOOL, Class, NO},
    sel, sel_impl,
};

const CORE_SIMULATOR_PATH: &str = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework";

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

unsafe fn load_framework(path: &str) -> Result<()> {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), bundleWithPath: ns_string(path)];
        anyhow::ensure!(bundle != nil, "CoreSimulator was not found at {path}");

        let loaded: BOOL = msg_send![bundle, load];
        anyhow::ensure!(loaded != NO, "CoreSimulator could not be loaded");
        Ok(())
    }
}
