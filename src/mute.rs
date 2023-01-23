extern crate core_foundation_sys;
extern crate coreaudio;

use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringGetCString, CFStringRef};
use coreaudio::audio_unit::macos_helpers::get_audio_device_ids;
use coreaudio::sys::{
    kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyMute,
    kAudioDevicePropertyScopeOutput, kAudioHardwareNoError, kAudioObjectPropertyElementMaster,
    AudioDeviceID, AudioDeviceSetProperty, AudioObjectGetPropertyData, AudioObjectPropertyAddress,
};
use std::ffi::CStr;
use std::mem;
use std::ptr::null;

use crate::audio_controller::{AudioControllerTrait, AudioError, AudioInputDeviceTrait};

pub struct AudioController {}

pub struct AudioInputDevice {
    audio_device_ids: Vec<AudioDeviceID>,
}

macro_rules! try_cf {
    ($expr:expr) => {
        #[allow(non_upper_case_globals)]
        match $expr as u32 {
            kAudioHardwareNoError => (),
            _ => {
                return Err(AudioError {
                    msg: format!(
                        "Error: {}",
                        coreaudio::Error::from_os_status($expr).err().unwrap()
                    ),
                })
            }
        }
    };
}

impl AudioControllerTrait for AudioController {
    fn new() -> Box<dyn AudioControllerTrait> {
        Box::new(AudioController {})
    }

    fn get_audio_units(&self) -> Result<Box<dyn AudioInputDeviceTrait>, AudioError> {
        // let property_address = AudioObjectPropertyAddress {
        //     mSelector: kAudioHardwarePropertyDefaultInputDevice,
        //     mScope: kAudioObjectPropertyScopeGlobal,
        //     mElement: kAudioObjectPropertyElementMaster,
        // };

        match get_audio_device_ids() {
            Ok(audio_device_ids) => {
                // let data_size = mem::size_of::<AudioDeviceID>();
                // let status = unsafe {
                //     AudioObjectGetPropertyData(
                //         kAudioObjectSystemObject,
                //         &property_address as *const _,
                //         0,
                //         null(),
                //         &data_size as *const _ as *mut _,
                //         &audio_device_id as *const _ as *mut _,
                //     )
                // };
                // log::trace!("{:?}", status);
                // if status != kAudioHardwareNoError as i32 {
                //     return Err(AudioError {
                //         msg: format!("Error: 0x{:X}", status),
                //     });
                // }

                Ok(Box::new(AudioInputDevice { audio_device_ids }))
            }
            Err(err) => {
                panic!("Failed to find audio device IDs {:?}", err);
            }
        }
    }
}

impl AudioInputDeviceTrait for AudioInputDevice {
    fn names(&self) -> Result<Vec<String>, AudioError> {
        Ok(self
            .audio_device_ids
            .iter()
            .map(|id| self.name(*id).unwrap())
            .collect())

        // for id in self.audio_device_ids.iter() {
        //     match self.name(id) {
        //         Ok(_) => {}
        //         Err(err) => {
        //             error!("Unable ");
        //             Ok([])
        //         }
        //     }
        // }
    }

    fn set_mute(&self, audio_device_id: AudioDeviceID, state: bool) -> Result<bool, AudioError> {
        let cf_state = state as u32;
        let data_size = mem::size_of::<u32>() as u32;
        unsafe {
            try_cf!(AudioDeviceSetProperty(
                audio_device_id,
                /* when */ null(),
                /* channel */ 0,
                /* is_input */ 1,
                kAudioDevicePropertyMute,
                data_size,
                &cf_state as *const _ as _,
            ));
        }

        Ok(state)
    }

    fn set_mute_all(&self, state: bool) -> Result<bool, AudioError> {
        for id in self.audio_device_ids.iter() {
            match self.set_mute(*id, state) {
                Ok(_) => (),
                Err(e) => panic!("Failed to set state for some device {:?}", e),
            }
        }
        Ok(state)
    }
}
