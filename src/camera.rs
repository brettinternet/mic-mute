use anyhow::Result;
use log::{error, trace};
use objc2::rc::Retained;
use objc2::{msg_send, ClassType};
use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeVideo};
use objc2_foundation::NSArray;
use std::ffi::c_void;
use std::mem;

// CMIO constants
const K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = 0x676c6f62; // 'glob'
const K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_CMIO_DEVICE_PROPERTY_IS_RUNNING_SOMEWHERE: u32 = 0x676F6E65; // 'gone'
                                                                     // System object holds the list of all CMIO devices
const K_CMIO_HARDWARE_OBJECT_SYSTEM: u32 = 1;
const K_CMIO_HARDWARE_PROPERTY_DEVICES: u32 = 0x64657623; // 'dev#'

type CMIOObjectID = u32;

#[repr(C)]
struct CMIOObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[link(name = "CoreMediaIO", kind = "framework")]
extern "C" {
    fn CMIOObjectGetPropertyData(
        object_id: CMIOObjectID,
        address: *const CMIOObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        in_data_size: u32,
        out_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> i32;

    fn CMIOObjectGetPropertyDataSize(
        object_id: CMIOObjectID,
        address: *const CMIOObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        out_data_size: *mut u32,
    ) -> i32;
}

/// AVFoundation's video capture devices, in one array; `None` if it returns nil.
///
/// Not the binding's `devicesWithMediaType`: it is deprecated, and it would panic on the nil
/// it declares impossible.
fn video_capture_devices() -> Option<Retained<NSArray<AVCaptureDevice>>> {
    // SAFETY: AVFoundation's media type constant, `None` only where it's missing.
    let media_type = unsafe { AVMediaTypeVideo }?;
    // SAFETY: a class method that takes a media type and returns an array of devices.
    unsafe { msg_send![AVCaptureDevice::class(), devicesWithMediaType: media_type] }
}

pub struct CameraController {
    pub muted: bool,
}

impl CameraController {
    pub fn new() -> Result<Self> {
        let mut controller = Self { muted: false };
        // muted=false means "camera is active"; muted=true means "camera is idle/off"
        controller.muted = !controller.is_running_anywhere().unwrap_or(false);
        Ok(controller)
    }

    /// Enumerate all CMIO device IDs directly from the CMIO system object.
    /// This does not require camera TCC permission.
    fn get_cmio_device_ids_system(&self) -> Vec<CMIOObjectID> {
        let address = CMIOObjectPropertyAddress {
            m_selector: K_CMIO_HARDWARE_PROPERTY_DEVICES,
            m_scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut data_size: u32 = 0;
        let status = unsafe {
            CMIOObjectGetPropertyDataSize(
                K_CMIO_HARDWARE_OBJECT_SYSTEM,
                &address,
                0,
                std::ptr::null(),
                &mut data_size,
            )
        };
        if status != 0 || data_size == 0 {
            trace!(
                "CMIO system device size query: status={} size={}",
                status,
                data_size
            );
            return vec![];
        }
        let count = data_size as usize / mem::size_of::<CMIOObjectID>();
        let mut ids = vec![0u32; count];
        let mut out_size = data_size;
        let status = unsafe {
            CMIOObjectGetPropertyData(
                K_CMIO_HARDWARE_OBJECT_SYSTEM,
                &address,
                0,
                std::ptr::null(),
                data_size,
                &mut out_size,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        trace!(
            "CMIO system device enumeration: status={} count={}",
            status,
            count
        );
        if status != 0 {
            vec![]
        } else {
            ids
        }
    }

    fn is_device_running_somewhere(&self, device_id: CMIOObjectID) -> Option<bool> {
        let address = CMIOObjectPropertyAddress {
            m_selector: K_CMIO_DEVICE_PROPERTY_IS_RUNNING_SOMEWHERE,
            m_scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut running: u32 = 0;
        let in_size = mem::size_of::<u32>() as u32;
        let mut out_size = in_size;
        let status = unsafe {
            CMIOObjectGetPropertyData(
                device_id,
                &address,
                0,
                std::ptr::null(),
                in_size,
                &mut out_size,
                &mut running as *mut u32 as *mut c_void,
            )
        };
        trace!(
            "CMIO device {} isRunningSomewhere: status={} running={}",
            device_id,
            status,
            running
        );
        if status == 0 {
            Some(running != 0)
        } else {
            None
        }
    }

    /// Returns true if any camera device is actively in use by any process.
    pub fn is_running_anywhere(&self) -> Result<bool> {
        // First: AVFoundation isInUseByAnotherApplication.
        // Only trusted if devicesWithMediaType: returns at least one device (requires TCC permission).
        let devices = video_capture_devices();
        let av_device_count = match &devices {
            Some(devices) => devices.count(),
            None => {
                error!(
                    "AVCaptureDevice.devicesWithMediaType: returned nil — no camera permission?"
                );
                0
            }
        };
        trace!("AVFoundation camera device count: {}", av_device_count);
        if let Some(devices) = &devices {
            // One array for the count and the devices, so an index can't outrun it.
            for i in 0..av_device_count {
                let device = devices.objectAtIndex(i);
                // SAFETY: a BOOL property of a capture device AVFoundation returned.
                let in_use = unsafe { device.isInUseByAnotherApplication() };
                trace!(
                    "AVCaptureDevice {} isInUseByAnotherApplication={}",
                    i,
                    in_use
                );
                if in_use {
                    return Ok(true);
                }
            }
        }
        // AVFoundation found no device in use. Also check CMIO — isInUseByAnotherApplication
        // can be unreliable on newer macOS.
        //
        // CMIO kCMIODevicePropertyDeviceIsRunningSomewhere — lower-level check that
        // runs regardless of AVFoundation result.
        let device_ids = self.get_cmio_device_ids_system();
        if device_ids.is_empty() {
            trace!("CMIO direct enumeration returned 0 devices");
        }
        for id in &device_ids {
            if let Some(true) = self.is_device_running_somewhere(*id) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_camera_controller_new() {
        // CameraController::new() should succeed even with no cameras
        let result = CameraController::new();
        assert!(result.is_ok());
    }
}
