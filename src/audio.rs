use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringGetCString, CFStringRef};
use coreaudio::audio_unit::macos_helpers::{
    get_audio_device_ids, get_device_name, get_supported_physical_stream_formats,
};
use coreaudio::sys::{
    kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyMute,
    kAudioDevicePropertyScopeInput, kAudioDevicePropertyScopeOutput,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwareNoError, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioQueueDeviceProperty_NumberChannels, kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyVirtualFormat, AudioBufferList,
    AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectHasProperty, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    AudioStreamBasicDescription, AudioStreamRangedDescription,
};
use log::{debug, error, trace};
use std::mem;
use std::ptr::null;

macro_rules! try_status_or_return {
    ($status:expr) => {
        if $status != kAudioHardwareNoError as i32 {
            return Err(format!(
                "Error: {}",
                coreaudio::Error::from_os_status($status).err().unwrap()
            ));
        }
    };
}

pub struct AudioController<F>
where
    F: Fn(String),
{
    handle_error: F,
}

impl<F> AudioController<F>
where
    F: Fn(String),
{
    pub fn new(handle_error: F) -> Self {
        let mut names = vec![];
        // match AudioController::<F>::get_input_device_ids() {
        match get_audio_device_ids() {
            Ok(ids) => {
                for id in ids {
                    match get_device_name(id) {
                        Ok(name) => {
                            names.push(format!("{}: {}", id, name));
                            // match get_supported_physical_stream_formats(id) {
                            match AudioController::<F>::get_asbd(id) {
                                Ok(formats) => {
                                    for fmt in formats {
                                        trace!("{}: {} - {:?}", id, name, fmt);
                                    }
                                }
                                _ => log::warn!(
                                    "didn't find get_supported_physical_stream_formats for {}",
                                    id
                                ),
                            }
                        }
                        Err(_) => error!("Unable to collect name for device {}", id),
                    }
                }
            }
            Err(_) => error!("Failed to get device IDs"),
        }
        trace!("Found {} devices: {}", names.len(), names.join(", "));
        Self { handle_error }
    }

    fn get_asbd(id: AudioDeviceID) -> Result<Vec<AudioStreamRangedDescription>, String> {
        // Get available formats.
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioStreamPropertyAvailablePhysicalFormats,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let allformats = unsafe {
            // property_address.mSelector = kAudioStreamPropertyAvailablePhysicalFormats;
            let mut data_size = 0u32;
            let status = AudioObjectGetPropertyDataSize(
                id,
                &property_address as *const _,
                0,
                null(),
                &mut data_size as *mut _,
            );
            try_status_or_return!(status);
            let n_formats = data_size as usize / mem::size_of::<AudioStreamRangedDescription>();
            let mut formats: Vec<AudioStreamRangedDescription> = vec![];
            formats.reserve_exact(n_formats as usize);
            formats.set_len(n_formats);

            let status = AudioObjectGetPropertyData(
                id,
                &property_address as *const _,
                0,
                null(),
                &data_size as *const _ as *mut _,
                formats.as_mut_ptr() as *mut _,
            );
            try_status_or_return!(status);
            formats
        };
        Ok(allformats)
    }

    fn get_input_device_ids() -> Result<Vec<AudioDeviceID>, String> {
        let audio_device_ids = get_audio_device_ids().unwrap();
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

    fn is_muted(&self, audio_device_id: AudioDeviceID) -> Result<bool, String> {
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

    fn mute(&self, audio_device_id: AudioDeviceID, state: bool) -> Result<AudioDeviceID, String> {
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
        try_status_or_return!(status);

        trace!(
            "Device {} now registers as {}",
            audio_device_id,
            match AudioController::is_muted(self, audio_device_id) {
                Ok(is_muted) if is_muted => "muted",
                _ => "unmuted",
            }
        );
        Ok(audio_device_id)
    }

    pub fn mute_all(&self, state: bool) -> &Self {
        for id in &mut AudioController::<F>::get_input_device_ids().unwrap() {
            let name = get_device_name(*id).unwrap_or("unknown".to_string());
            match AudioController::mute(self, *id, state) {
                Ok(_) => {
                    trace!(
                        "Successfully {} audio device {}: {}",
                        if state { "muted" } else { "unmuted" },
                        id,
                        name
                    )
                }
                Err(_) => (self.handle_error)(format!(
                    "Failed to toggle mute to {} for audio {}: {}",
                    state, id, name
                )),
            };
        }
        self
    }

    // fn add_input_property_listener()
    // fn add hardware_listener()
}
