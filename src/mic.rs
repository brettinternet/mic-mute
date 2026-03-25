use anyhow::{anyhow, Result};
use coreaudio::audio_unit::macos_helpers::{get_audio_device_ids, get_device_name};
use log::{error, trace};
use objc2_core_audio::{
    kAudioDevicePropertyMute, kAudioDevicePropertyScopeInput,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyVolumeScalar,
    kAudioHardwareNoError, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwareUnknownPropertyError, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, AudioDeviceID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
};
use objc2_core_audio_types::AudioBufferList;
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ptr::{null, NonNull};

macro_rules! try_status_or_return {
    ($status:expr) => {
        match $status {
            status if status == kAudioHardwareUnknownPropertyError as i32 => {}
            status if status == kAudioHardwareNoError as i32 => {}
            status => {
                return Err(anyhow!("Error: {}", status));
            }
        }
    };
}

#[derive(Default)]
pub struct MicController {
    pub muted: bool,
    /// Saved input volume per device for devices that don't support kAudioDevicePropertyMute.
    /// Keyed by AudioDeviceID; value is the volume scalar (0.0–1.0) before muting.
    saved_volumes: HashMap<AudioDeviceID, f32>,
}

impl Debug for MicController {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("MicController")
            .field("names", &self.names().unwrap())
            .field("muted", &self.muted)
            .finish()
    }
}

impl MicController {
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
        let ids = get_audio_device_ids().map_err(anyhow::Error::msg)?;
        for id in ids {
            let name = get_device_name(id).map_err(anyhow::Error::msg)?;
            names.push(format!("{}: {}", id, name));
        }
        Ok(names)
    }

    fn get_input_device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        let audio_device_ids = get_audio_device_ids().map_err(anyhow::Error::msg)?;
        trace!(
            "All {} audio device IDs {:?}",
            audio_device_ids.len(),
            audio_device_ids
        );
        let mut input_device_ids = vec![];
        for id in audio_device_ids {
            let mut property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyStreamConfiguration,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut data_size = 0u32;
            let status = unsafe {
                AudioObjectGetPropertyDataSize(
                    id,
                    NonNull::new_unchecked(&mut property_address),
                    0,
                    null(),
                    NonNull::new_unchecked(&mut data_size),
                )
            };
            try_status_or_return!(status);

            // To determine if a device is an input device check if it has input channels
            // https://stackoverflow.com/a/4577271
            let mut audio_buffer_list = AudioBufferList {
                mNumberBuffers: data_size,
                mBuffers: [objc2_core_audio_types::AudioBuffer {
                    mNumberChannels: 0,
                    mDataByteSize: 0,
                    mData: std::ptr::null_mut(),
                }],
            };
            let status = unsafe {
                AudioObjectGetPropertyData(
                    id,
                    NonNull::new_unchecked(&mut property_address),
                    0,
                    null(),
                    NonNull::new_unchecked(&mut data_size),
                    NonNull::new_unchecked(&mut audio_buffer_list as *mut _ as *mut _),
                )
            };
            trace!(
                "Buffer list mNumberBuffers {} for device {}",
                audio_buffer_list.mNumberBuffers,
                id
            );
            try_status_or_return!(status);
            if audio_buffer_list.mNumberBuffers > 0 {
                input_device_ids.push(id);
            }
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

        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let muted = 0_u32;
        let mut data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&muted as *const u32 as *mut _),
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

    fn mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<AudioDeviceID> {
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let data = state as u32;
        let data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectSetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                data_size,
                NonNull::new_unchecked(&data as *const u32 as *mut _),
            )
        };

        trace!("RESULT FROM STATUS: {}", status);

        match status {
            status if status == kAudioHardwareNoError => {}
            status if status == kAudioHardwareUnknownPropertyError => {
                // Device doesn't support kAudioDevicePropertyMute (e.g. iPhone Continuity mic).
                // Fall back to setting the input volume scalar to 0.
                trace!(
                    "Device {} doesn't support mute property; falling back to volume scalar",
                    audio_device_id
                );
                self.mute_via_volume(audio_device_id, state);
            }
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

    fn mute_via_volume(&mut self, audio_device_id: AudioDeviceID, state: bool) {
        let mut vol_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyVolumeScalar,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let vol_data_size = mem::size_of::<f32>() as u32;

        if state {
            // Save current volume before muting, if not already saved.
            if let std::collections::hash_map::Entry::Vacant(e) =
                self.saved_volumes.entry(audio_device_id)
            {
                let current_vol = 0_f32;
                let mut read_size = vol_data_size;
                let read_status = unsafe {
                    AudioObjectGetPropertyData(
                        audio_device_id,
                        NonNull::new_unchecked(&mut vol_address),
                        0,
                        null(),
                        NonNull::new_unchecked(&mut read_size),
                        NonNull::new_unchecked(&current_vol as *const f32 as *mut _),
                    )
                };
                if read_status == kAudioHardwareNoError {
                    trace!(
                        "Saving volume {:.3} for device {} before muting",
                        current_vol,
                        audio_device_id
                    );
                    e.insert(current_vol);
                }
            }
            let zero: f32 = 0.0;
            let set_status = unsafe {
                AudioObjectSetPropertyData(
                    audio_device_id,
                    NonNull::new_unchecked(&mut vol_address),
                    0,
                    null(),
                    vol_data_size,
                    NonNull::new_unchecked(&zero as *const f32 as *mut _),
                )
            };
            if set_status != kAudioHardwareNoError {
                trace!(
                    "Volume scalar mute failed for device {} with status {}",
                    audio_device_id,
                    set_status
                );
            }
        } else {
            // Restore saved volume, defaulting to 1.0 if none was saved.
            let restore_vol = self
                .saved_volumes
                .remove(&audio_device_id)
                .unwrap_or(1.0_f32);
            trace!(
                "Restoring volume {:.3} for device {}",
                restore_vol,
                audio_device_id
            );
            let set_status = unsafe {
                AudioObjectSetPropertyData(
                    audio_device_id,
                    NonNull::new_unchecked(&mut vol_address),
                    0,
                    null(),
                    vol_data_size,
                    NonNull::new_unchecked(&restore_vol as *const f32 as *mut _),
                )
            };
            if set_status != kAudioHardwareNoError {
                trace!(
                    "Volume scalar unmute failed for device {} with status {}",
                    audio_device_id,
                    set_status
                );
            }
        }
    }

    pub fn mute_all(&mut self, state: bool) -> Result<&Self> {
        let ids = self.get_input_device_ids()?;
        for id in &ids {
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

    /// Returns the name of the default system input device, if available.
    pub fn active_device_name(&self) -> Option<String> {
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut device_id: AudioDeviceID = 0;
        let mut data_size = mem::size_of::<AudioDeviceID>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                1, // kAudioObjectSystemObject
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&mut device_id as *mut _ as *mut _),
            )
        };
        if status != kAudioHardwareNoError || device_id == 0 {
            return None;
        }
        get_device_name(device_id).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mic_controller_new() {
        // Should succeed (even if no input devices)
        let result = MicController::new();
        assert!(result.is_ok());
    }

    #[test]
    fn test_mic_controller_default() {
        let c = MicController::default();
        assert!(!c.muted);
    }

    #[test]
    fn test_toggle_logic() {
        // Test the toggle state logic without touching hardware
        let mut c = MicController { muted: false };
        let new_state = None::<bool>.unwrap_or(!c.muted);
        assert!(new_state, "Toggle from false should give true");

        c.muted = true;
        let new_state = None::<bool>.unwrap_or(!c.muted);
        assert!(!new_state, "Toggle from true should give false");

        // Explicit state override
        let new_state = Some(false).unwrap_or(!c.muted);
        assert!(!new_state);
    }
}
