use anyhow::{anyhow, Result};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringGetCString, CFStringRef};
use coreaudio::audio_unit::macos_helpers::{
    get_audio_device_ids, get_device_name, get_supported_physical_stream_formats,
};
use coreaudio::sys::{
    kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyMute,
    kAudioDevicePropertyScopeInput, kAudioDevicePropertyScopeOutput,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwareNoError, kAudioHardwarePropertyDevices,
    kAudioHardwareUnknownPropertyError, kAudioObjectPropertyElementMaster,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioQueueDeviceProperty_NumberChannels, kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyVirtualFormat, AudioBufferList,
    AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectHasProperty, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    AudioStreamBasicDescription, AudioStreamRangedDescription,
};
use log::{error, trace};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ptr::null;

#[derive(Debug)]
pub struct AudioError {
    pub msg: String,
}

// /// Trait that describes an audio input device.
// pub trait AudioInputDeviceTrait {
//     /// Collect names of audio devices
//     fn names(&self) -> Result<Vec<String>>;

//     /// Sets the mute state of the audio device.
//     fn set_mute(&self, audio_device_id: u32, state: bool) -> Result<bool>;

//     /// Set the mute state for all audio devices.
//     fn set_mute_all(&self, state: bool) -> Result<bool>;
// }

// impl Debug for dyn AudioInputDeviceTrait {
//     fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
//         f.debug_struct("AudioInputDevice")
//             .field("names", &self.names().unwrap())
//             .finish()
//     }
// }

macro_rules! try_status_or_return {
    ($status:expr) => {
        match $status {
            status if status == kAudioHardwareUnknownPropertyError as i32 => {}
            status if status == kAudioHardwareNoError as i32 => {}
            status => {
                return Err(anyhow!(
                    "Error: {}",
                    coreaudio::Error::from_os_status(status).err().unwrap()
                ));
            }
        }
    };
}

pub struct AudioController {
    pub muted: bool,
}

impl Default for AudioController {
    fn default() -> Self {
        Self { muted: false }
    }
}

impl Debug for AudioController {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("AudioController")
            .field("names", &self.names().unwrap())
            .field("muted", &self.muted)
            .finish()
    }
}

impl AudioController {
    pub fn new() -> Result<Self> {
        let mut controller = Self {
            ..Default::default()
        };
        trace!("Creating audio controller");
        let names = controller.names()?;
        trace!("Found {} devices: {}", names.len(), names.join(", "));
        controller.muted = controller.is_muted_all()?;
        Ok(controller)
    }

    fn names(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        // match AudioController::<F>::get_input_device_ids() {
        let ids = get_audio_device_ids().map_err(anyhow::Error::msg)?;
        for id in ids {
            let name = get_device_name(id).map_err(anyhow::Error::msg)?;
            names.push(format!("{}: {}", id, name));
            // let formats = AudioController::get_asbd(id)?;
            // for fmt in formats {
            //     trace!("{}: {} - {:?}", id, name, fmt);
            // }
        }
        Ok(names)
    }

    // fn get_asbd(&self, id: AudioDeviceID) -> Result<Vec<AudioStreamRangedDescription>> {
    //     // Get available formats.
    //     let property_address = AudioObjectPropertyAddress {
    //         mSelector: kAudioStreamPropertyAvailablePhysicalFormats,
    //         mScope: kAudioDevicePropertyScopeInput,
    //         mElement: kAudioObjectPropertyElementMaster,
    //     };
    //     let allformats = unsafe {
    //         // property_address.mSelector = kAudioStreamPropertyAvailablePhysicalFormats;
    //         let mut data_size = 0u32;
    //         let status = AudioObjectGetPropertyDataSize(
    //             id,
    //             &property_address as *const _,
    //             0,
    //             null(),
    //             &mut data_size as *mut _,
    //         );
    //         try_status_or_return!(status);
    //         let n_formats = data_size as usize / mem::size_of::<AudioStreamRangedDescription>();
    //         let mut formats: Vec<AudioStreamRangedDescription> = vec![];
    //         formats.reserve_exact(n_formats as usize);
    //         formats.set_len(n_formats);

    //         let status = AudioObjectGetPropertyData(
    //             id,
    //             &property_address as *const _,
    //             0,
    //             null(),
    //             &data_size as *const _ as *mut _,
    //             formats.as_mut_ptr() as *mut _,
    //         );
    //         try_status_or_return!(status);
    //         formats
    //     };
    //     Ok(allformats)
    // }

    fn get_input_device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        let audio_device_ids = get_audio_device_ids().map_err(anyhow::Error::msg)?;
        trace!(
            "All {} audio device IDs {:?}",
            audio_device_ids.len(),
            audio_device_ids
        );
        let mut input_device_ids = vec![];
        for id in audio_device_ids.clone() {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyStreamConfiguration,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMaster,
            };
            let data_size = 0u32;
            let status = unsafe {
                AudioObjectGetPropertyDataSize(
                    id,
                    &property_address as *const _,
                    0,
                    null(),
                    &data_size as *const _ as *mut _,
                )
            };
            try_status_or_return!(status);

            // To determine if a device is an input device you need to check and see if it has any input channels
            // https://stackoverflow.com/a/4577271
            let audio_buffer_list = AudioBufferList {
                mNumberBuffers: data_size,
                ..Default::default()
            };
            let status = unsafe {
                AudioObjectGetPropertyData(
                    id,
                    &property_address as *const _,
                    0,
                    null(),
                    &data_size as *const _ as *mut _,
                    &audio_buffer_list as *const _ as *mut _,
                )
            };
            trace!(
                "Buffer list mNumberBuffers {} for device {}",
                audio_buffer_list.mNumberBuffers,
                id
            );
            try_status_or_return!(status);
            if audio_buffer_list.mNumberBuffers > 0 {
                // kAudioQueueDeviceProperty_NumberChannels
                input_device_ids.push(id);
            }
            // for i in 1..audio_buffer_list.mNumberBuffers {
            //     let audio_buffer = audio_buffer_list[i];
            //     channel_count = channel_count + audio_buffer.ch
            // }
        }

        trace!(
            "Filtered list to {} input device IDs {:?}",
            input_device_ids.len(),
            input_device_ids
        );
        Ok(input_device_ids)
    }

    fn is_muted(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
        let name = get_device_name(audio_device_id).map_err(anyhow::Error::msg)?;
        trace!("BEFORE Device {} - {}", audio_device_id, name);

        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let muted = 0 as u32;
        let data_size = mem::size_of::<u32>();
        let status = unsafe {
            // AudioObjectHasProperty(
            //     audio_device_id,
            //     &property_address as *const _,
            //     0,
            //     null(),
            //     &data_size as *const _ as *mut _,
            //     &muted as *const _ as *mut _,
            // )
            AudioObjectGetPropertyData(
                audio_device_id,
                &property_address as *const _,
                0,
                null(),
                &data_size as *const _ as *mut _,
                &muted as *const _ as *mut _,
            )
        };
        try_status_or_return!(status);
        trace!("Mute result: {}", muted);
        Ok(muted == 1)
    }

    fn is_muted_all(&self) -> Result<bool> {
        for id in &self.get_input_device_ids()? {
            let state = self.is_muted(*id)?;
            trace!(
                "Input device {} is {}",
                id,
                if state { "muted" } else { "unmuted" },
            );
            if state {
                continue;
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn mute(&self, audio_device_id: AudioDeviceID, state: bool) -> Result<AudioDeviceID> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let data = state as u32;
        let data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectSetPropertyData(
                audio_device_id,
                &property_address as *const _,
                0,
                null(),
                data_size,
                &data as *const _ as _,
            )
        };

        trace!("RESULT FROM STATUS: {}", status);

        // try_status_or_return!(status);
        match status {
            status if status == kAudioHardwareNoError as i32 => {}
            status if status == kAudioHardwareUnknownPropertyError as i32 => {}
            result => {
                error!("RESULT: {}", result);
            }
        }

        trace!(
            "Device {} now registers as {}",
            audio_device_id,
            match self.is_muted(audio_device_id) {
                Ok(is_muted) if is_muted => "muted",
                _ => "unmuted",
            }
        );
        Ok(audio_device_id)
    }

    pub fn mute_all(&mut self, state: bool) -> Result<&Self> {
        for id in &self.get_input_device_ids()? {
            let name = get_device_name(*id).map_err(anyhow::Error::msg)?;
            trace!("Muting {}", name);
            self.mute(*id, state)?;
            trace!(
                "Successfully {} audio device {}: {}",
                if state { "muted" } else { "unmuted" },
                id,
                name
            )
        }
        self.muted = state;
        Ok(self)
    }

    pub fn toggle(&mut self, state: Option<bool>) -> Result<&Self> {
        let state = state.unwrap_or(!self.muted);
        self.mute_all(state)
    }

    // fn add_input_property_listener()
    // fn add hardware_listener()
}
