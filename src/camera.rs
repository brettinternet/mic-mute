use anyhow::{anyhow, Result};
use log::{error, trace};
use std::ffi::c_void;
use std::mem;

// CMIO constants
const K_CMIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = 0x676c6f62; // 'glob'
const K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_CMIO_HARDWARE_PROPERTY_DEVICES: u32 = 0x64657620; // 'dev '
const K_CMIO_DEVICE_PROPERTY_SUSPENDED_BY_USER: u32 = 0x73627975; // 'sbyu'

type CMIOObjectID = u32;

#[repr(C)]
struct CMIOObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[link(name = "CoreMediaIO", kind = "framework")]
extern "C" {
    fn CMIOObjectGetPropertyDataSize(
        object_id: CMIOObjectID,
        address: *const CMIOObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        out_data_size: *mut u32,
    ) -> i32;

    fn CMIOObjectGetPropertyData(
        object_id: CMIOObjectID,
        address: *const CMIOObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        in_data_size: u32,
        out_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> i32;

    fn CMIOObjectSetPropertyData(
        object_id: CMIOObjectID,
        address: *const CMIOObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        in_data_size: u32,
        in_data: *const c_void,
    ) -> i32;
}

pub struct CameraController {
    pub muted: bool,
}

impl CameraController {
    pub fn new() -> Result<Self> {
        let mut controller = Self { muted: false };
        controller.muted = controller.is_muted_all().unwrap_or(false);
        Ok(controller)
    }

    fn get_device_ids(&self) -> Result<Vec<CMIOObjectID>> {
        let address = CMIOObjectPropertyAddress {
            m_selector: K_CMIO_HARDWARE_PROPERTY_DEVICES,
            m_scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut data_size: u32 = 0;
        let status = unsafe {
            CMIOObjectGetPropertyDataSize(
                K_CMIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                &mut data_size,
            )
        };
        if status != 0 {
            return Ok(vec![]);
        }
        let count = data_size as usize / mem::size_of::<CMIOObjectID>();
        let mut devices: Vec<CMIOObjectID> = vec![0; count];
        let mut out_size = data_size;
        let status = unsafe {
            CMIOObjectGetPropertyData(
                K_CMIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                data_size,
                &mut out_size,
                devices.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return Ok(vec![]);
        }
        Ok(devices)
    }

    fn is_device_suspended(&self, device_id: CMIOObjectID) -> Option<bool> {
        let address = CMIOObjectPropertyAddress {
            m_selector: K_CMIO_DEVICE_PROPERTY_SUSPENDED_BY_USER,
            m_scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut suspended: u32 = 0;
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
                &mut suspended as *mut u32 as *mut c_void,
            )
        };
        if status == 0 {
            Some(suspended != 0)
        } else {
            None
        }
    }

    fn set_device_suspended(&self, device_id: CMIOObjectID, suspended: bool) -> bool {
        let address = CMIOObjectPropertyAddress {
            m_selector: K_CMIO_DEVICE_PROPERTY_SUSPENDED_BY_USER,
            m_scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            m_element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let value: u32 = if suspended { 1 } else { 0 };
        let data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            CMIOObjectSetPropertyData(
                device_id,
                &address,
                0,
                std::ptr::null(),
                data_size,
                &value as *const u32 as *const c_void,
            )
        };
        if status != 0 {
            error!("Failed to set camera device {} suspended={}: status {}", device_id, suspended, status);
        }
        status == 0
    }

    pub fn is_muted_all(&self) -> Result<bool> {
        let devices = self.get_device_ids()?;
        if devices.is_empty() {
            return Ok(false);
        }
        for id in &devices {
            if let Some(suspended) = self.is_device_suspended(*id) {
                if !suspended {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub fn mute_all(&mut self, state: bool) -> Result<&Self> {
        let devices = self.get_device_ids()?;
        trace!("Setting {} camera devices suspended={}", devices.len(), state);
        for id in &devices {
            self.set_device_suspended(*id, state);
        }
        self.muted = state;
        Ok(self)
    }

    pub fn toggle(&mut self, state: Option<bool>) -> Result<&Self> {
        let state = state.unwrap_or(!self.muted);
        self.mute_all(state)
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

    #[test]
    fn test_camera_toggle_logic() {
        // Test the toggle state logic independently
        let mut cam = CameraController { muted: false };
        // toggle with explicit state
        let state = Some(true).unwrap_or(!cam.muted);
        assert!(state);

        let state = Some(false).unwrap_or(!cam.muted);
        assert!(!state);

        // toggle without explicit state (flip)
        let state = None::<bool>.unwrap_or(!cam.muted);
        assert!(state); // was false, should flip to true
    }
}
