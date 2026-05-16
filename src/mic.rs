use anyhow::{anyhow, Context, Result};
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
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ptr::{null, NonNull};

const SYSTEM_OBJECT_ID: AudioDeviceID = 1;
const VOLUME_MUTED_EPSILON: f32 = 0.000_001;

fn is_volume_muted(volume: f32) -> bool {
    volume <= VOLUME_MUTED_EPSILON
}

fn target_state(state: Option<bool>, desired_muted: bool) -> bool {
    state.unwrap_or(!desired_muted)
}

fn status_result(status: i32, operation: &str, audio_device_id: AudioDeviceID) -> Result<()> {
    if status == kAudioHardwareNoError {
        Ok(())
    } else {
        Err(anyhow!(
            "{} failed for audio device {} with OSStatus {}",
            operation,
            audio_device_id,
            status
        ))
    }
}

struct AudioBufferListAllocation {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AudioBufferListAllocation {
    fn new(size: u32) -> Result<Self> {
        let layout = Layout::from_size_align(size as usize, mem::align_of::<AudioBufferList>())
            .context("invalid AudioBufferList allocation layout")?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or_else(|| anyhow!("failed to allocate AudioBufferList"))?;
        Ok(Self { ptr, layout })
    }

    fn as_mut_void(&mut self) -> *mut c_void {
        self.ptr.as_ptr().cast()
    }

    unsafe fn as_list(&self) -> &AudioBufferList {
        &*self.ptr.as_ptr().cast::<AudioBufferList>()
    }
}

impl Drop for AudioBufferListAllocation {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

pub trait AudioBackend {
    fn device_ids(&self) -> Result<Vec<AudioDeviceID>>;
    fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String>;
    fn has_input_channels(&self, audio_device_id: AudioDeviceID) -> Result<bool>;
    fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>>;
    fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>>;
    fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<f32>;
    fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<()>;
    fn default_input_device(&self) -> Result<Option<AudioDeviceID>>;
}

#[derive(Default)]
pub struct CoreAudioBackend;

impl CoreAudioBackend {
    fn mute_address() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn volume_address() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyVolumeScalar,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        }
    }
}

impl AudioBackend for CoreAudioBackend {
    fn device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        get_audio_device_ids().map_err(anyhow::Error::msg)
    }

    fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String> {
        get_device_name(audio_device_id).map_err(anyhow::Error::msg)
    }

    fn has_input_channels(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreamConfiguration,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut data_size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
            )
        };
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(false);
        }
        status_result(
            status,
            "read input stream configuration size",
            audio_device_id,
        )?;
        if data_size < mem::size_of::<u32>() as u32 {
            return Ok(false);
        }

        let mut buffer_list = AudioBufferListAllocation::new(data_size)?;
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(buffer_list.as_mut_void()),
            )
        };
        status_result(status, "read input stream configuration", audio_device_id)?;

        let list = unsafe { buffer_list.as_list() };
        let buffer_count = list.mNumberBuffers as usize;
        if buffer_count == 0 {
            return Ok(false);
        }
        let minimum_size = mem::offset_of!(AudioBufferList, mBuffers)
            + buffer_count * mem::size_of::<AudioBuffer>();
        if (data_size as usize) < minimum_size {
            return Err(anyhow!(
                "input stream configuration for audio device {} was truncated",
                audio_device_id
            ));
        }
        let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), buffer_count) };
        Ok(buffers.iter().any(|buffer| buffer.mNumberChannels > 0))
    }

    fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>> {
        let mut property_address = Self::mute_address();
        let mut muted = 0_u32;
        let mut data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&mut muted as *mut u32 as *mut c_void),
            )
        };
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(None);
        }
        status_result(status, "read mute", audio_device_id)?;
        Ok(Some(muted == 1))
    }

    fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>> {
        let mut property_address = Self::mute_address();
        let data = state as u32;
        let data_size = mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectSetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                data_size,
                NonNull::new_unchecked(&data as *const u32 as *mut c_void),
            )
        };
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(None);
        }
        status_result(status, "set mute", audio_device_id)?;
        Ok(Some(()))
    }

    fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<f32> {
        let mut property_address = Self::volume_address();
        let mut volume = 0_f32;
        let mut data_size = mem::size_of::<f32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&mut volume as *mut f32 as *mut c_void),
            )
        };
        status_result(status, "read input volume", audio_device_id)?;
        Ok(volume)
    }

    fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<()> {
        let mut property_address = Self::volume_address();
        let data_size = mem::size_of::<f32>() as u32;
        let status = unsafe {
            AudioObjectSetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                data_size,
                NonNull::new_unchecked(&volume as *const f32 as *mut c_void),
            )
        };
        status_result(status, "set input volume", audio_device_id)
    }

    fn default_input_device(&self) -> Result<Option<AudioDeviceID>> {
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut device_id: AudioDeviceID = 0;
        let mut data_size = mem::size_of::<AudioDeviceID>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT_ID,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&mut device_id as *mut AudioDeviceID as *mut c_void),
            )
        };
        if status != kAudioHardwareNoError || device_id == 0 {
            return Ok(None);
        }
        Ok(Some(device_id))
    }
}

pub struct MicController<B = CoreAudioBackend> {
    pub muted: bool,
    desired_muted: bool,
    /// Saved input volume per device for devices that don't support kAudioDevicePropertyMute.
    /// Keyed by AudioDeviceID; value is the volume scalar (0.0–1.0) before muting.
    saved_volumes: HashMap<AudioDeviceID, f32>,
    volume_fallback_devices: HashSet<AudioDeviceID>,
    backend: B,
}

impl<B: Default> Default for MicController<B> {
    fn default() -> Self {
        Self {
            muted: false,
            desired_muted: false,
            saved_volumes: HashMap::new(),
            volume_fallback_devices: HashSet::new(),
            backend: B::default(),
        }
    }
}

impl<B: AudioBackend> Debug for MicController<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("MicController")
            .field("names", &self.names().unwrap_or_default())
            .field("muted", &self.muted)
            .field("desired_muted", &self.desired_muted)
            .finish()
    }
}

impl MicController<CoreAudioBackend> {
    pub fn new() -> Result<Self> {
        Self::with_backend(CoreAudioBackend)
    }
}

impl<B: AudioBackend> MicController<B> {
    fn with_backend(backend: B) -> Result<Self> {
        let mut controller = Self {
            muted: false,
            desired_muted: false,
            saved_volumes: HashMap::new(),
            volume_fallback_devices: HashSet::new(),
            backend,
        };
        trace!("Creating audio controller");
        let names = controller.names()?;
        trace!("Found {} devices: {}", names.len(), names.join(", "));
        controller.muted = controller.is_muted_all()?;
        controller.desired_muted = controller.muted;
        Ok(controller)
    }

    fn names(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        let ids = self.backend.device_ids()?;
        for id in ids {
            let name = self.backend.device_name(id)?;
            names.push(format!("{}: {}", id, name));
        }
        Ok(names)
    }

    fn get_input_device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        let audio_device_ids = self.backend.device_ids()?;
        trace!(
            "All {} audio device IDs {:?}",
            audio_device_ids.len(),
            audio_device_ids
        );
        let mut input_device_ids = vec![];
        for id in audio_device_ids {
            if self.backend.has_input_channels(id)? {
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
        let name = self.backend.device_name(audio_device_id)?;
        trace!(
            "Reading mute state for device {} - {}",
            audio_device_id,
            name
        );

        match self.backend.get_mute(audio_device_id)? {
            Some(muted) => Ok(muted),
            None => match self.backend.get_volume(audio_device_id) {
                Ok(volume) => Ok(is_volume_muted(volume)),
                Err(err) => {
                    trace!(
                        "Unable to read fallback volume for audio device {}: {}",
                        audio_device_id,
                        err
                    );
                    Ok(false)
                }
            },
        }
    }

    fn is_muted_all(&self) -> Result<bool> {
        for id in &self.get_input_device_ids()? {
            let state = self.is_muted(*id)?;
            trace!(
                "Input device {} is {}",
                id,
                if state { "muted" } else { "unmuted" },
            );
            if !state {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn all_devices_match_state(&self, ids: &[AudioDeviceID], state: bool) -> Result<bool> {
        for id in ids {
            if self.is_muted(*id)? != state {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn wait_for_device_state(&self, audio_device_id: AudioDeviceID, state: bool) -> Result<bool> {
        for attempt in 0..5 {
            if self.is_muted(audio_device_id)? == state {
                return Ok(true);
            }
            if attempt != 4 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        Ok(false)
    }

    fn mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<AudioDeviceID> {
        let set_result = self.backend.set_mute(audio_device_id, state)?;
        if set_result.is_none() {
            trace!(
                "Device {} doesn't support mute property; falling back to volume scalar",
                audio_device_id
            );
            self.mute_via_volume(audio_device_id, state)?;
        } else {
            self.volume_fallback_devices.remove(&audio_device_id);
            if !self.wait_for_device_state(audio_device_id, state)? {
                return Err(anyhow!(
                    "audio device {} did not reach requested mute state {} after native mute set",
                    audio_device_id,
                    state
                ));
            }
        }

        trace!(
            "Device {} now registers as {}",
            audio_device_id,
            if self.is_muted(audio_device_id)? {
                "muted"
            } else {
                "unmuted"
            }
        );
        Ok(audio_device_id)
    }

    fn mute_via_volume(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<()> {
        if state {
            if !self.saved_volumes.contains_key(&audio_device_id) {
                let current_vol = self.backend.get_volume(audio_device_id)?;
                trace!(
                    "Saving volume {:.3} for device {} before muting",
                    current_vol,
                    audio_device_id
                );
                self.saved_volumes.insert(audio_device_id, current_vol);
            }
            self.backend.set_volume(audio_device_id, 0.0)?;
            let volume = self.backend.get_volume(audio_device_id)?;
            if !is_volume_muted(volume) {
                return Err(anyhow!(
                    "audio device {} input volume remained {:.3} after fallback mute",
                    audio_device_id,
                    volume
                ));
            }
            self.volume_fallback_devices.insert(audio_device_id);
        } else {
            let restore_vol = self
                .saved_volumes
                .remove(&audio_device_id)
                .filter(|volume| !is_volume_muted(*volume))
                .unwrap_or(1.0_f32);
            trace!(
                "Restoring volume {:.3} for device {}",
                restore_vol,
                audio_device_id
            );
            self.backend.set_volume(audio_device_id, restore_vol)?;
            let volume = self.backend.get_volume(audio_device_id)?;
            if is_volume_muted(volume) {
                return Err(anyhow!(
                    "audio device {} input volume remained muted after fallback unmute",
                    audio_device_id
                ));
            }
            self.volume_fallback_devices.remove(&audio_device_id);
        }
        Ok(())
    }

    pub fn mute_all(&mut self, state: bool) -> Result<&Self> {
        self.desired_muted = state;
        let ids = self.get_input_device_ids()?;
        let mut failures = Vec::new();
        for id in &ids {
            let name = self.backend.device_name(*id)?;
            trace!("Setting mute={} for {}", state, name);
            match self.mute(*id, state) {
                Ok(_) => trace!(
                    "Successfully {} audio device {}: {}",
                    if state { "muted" } else { "unmuted" },
                    id,
                    name
                ),
                Err(err) => {
                    error!(
                        "Failed to {} audio device {}: {}: {}",
                        if state { "mute" } else { "unmute" },
                        id,
                        name,
                        err
                    );
                    failures.push(format!("{} ({})", name, err));
                }
            }
        }

        self.muted = self.is_muted_all().unwrap_or(false);
        if !failures.is_empty() {
            return Err(anyhow!(
                "failed to {} {} input device(s): {}",
                if state { "mute" } else { "unmute" },
                failures.len(),
                failures.join("; ")
            ));
        }
        if !self.all_devices_match_state(&ids, state)? {
            self.muted = self.is_muted_all().unwrap_or(false);
            return Err(anyhow!(
                "one or more input devices did not reach requested mute state {}",
                state
            ));
        }
        self.muted = state || self.is_muted_all()?;
        Ok(self)
    }

    pub fn toggle(&mut self, state: Option<bool>) -> Result<&Self> {
        let state = target_state(state, self.desired_muted);
        self.mute_all(state)
    }

    pub fn should_enforce_mute(&self) -> bool {
        self.desired_muted
    }

    /// Returns the name of the default system input device, if available.
    pub fn active_device_name(&self) -> Option<String> {
        self.backend
            .default_input_device()
            .ok()
            .flatten()
            .and_then(|device_id| self.backend.device_name(device_id).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Clone)]
    struct Device {
        name: String,
        input: bool,
        mute: Option<bool>,
        volume: f32,
        fail_set_mute: bool,
        fail_set_volume: bool,
        ignore_set_mute: bool,
    }

    impl Device {
        fn native(name: &str, muted: bool) -> Self {
            Self {
                name: name.to_string(),
                input: true,
                mute: Some(muted),
                volume: 1.0,
                fail_set_mute: false,
                fail_set_volume: false,
                ignore_set_mute: false,
            }
        }

        fn fallback(name: &str, volume: f32) -> Self {
            Self {
                name: name.to_string(),
                input: true,
                mute: None,
                volume,
                fail_set_mute: false,
                fail_set_volume: false,
                ignore_set_mute: false,
            }
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        devices: HashMap<AudioDeviceID, Device>,
        ids: Vec<AudioDeviceID>,
        default_input: Option<AudioDeviceID>,
    }

    impl FakeBackend {
        fn with_devices(devices: Vec<(AudioDeviceID, Device)>) -> Self {
            let default_input = devices.first().map(|(id, _)| *id);
            let ids = devices.iter().map(|(id, _)| *id).collect();
            let devices = devices.into_iter().collect();
            Self {
                devices,
                ids,
                default_input,
            }
        }

        fn device(&self, id: AudioDeviceID) -> Result<&Device> {
            self.devices
                .get(&id)
                .ok_or_else(|| anyhow!("missing fake device {}", id))
        }

        fn device_mut(&mut self, id: AudioDeviceID) -> Result<&mut Device> {
            self.devices
                .get_mut(&id)
                .ok_or_else(|| anyhow!("missing fake device {}", id))
        }
    }

    impl AudioBackend for FakeBackend {
        fn device_ids(&self) -> Result<Vec<AudioDeviceID>> {
            Ok(self.ids.clone())
        }

        fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String> {
            Ok(self.device(audio_device_id)?.name.clone())
        }

        fn has_input_channels(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
            Ok(self.device(audio_device_id)?.input)
        }

        fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>> {
            Ok(self.device(audio_device_id)?.mute)
        }

        fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>> {
            let device = self.device_mut(audio_device_id)?;
            if device.fail_set_mute {
                return Err(anyhow!("fake native mute failure"));
            }
            if let Some(muted) = &mut device.mute {
                if !device.ignore_set_mute {
                    *muted = state;
                }
                Ok(Some(()))
            } else {
                Ok(None)
            }
        }

        fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<f32> {
            Ok(self.device(audio_device_id)?.volume)
        }

        fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<()> {
            let device = self.device_mut(audio_device_id)?;
            if device.fail_set_volume {
                return Err(anyhow!("fake volume failure"));
            }
            device.volume = volume;
            Ok(())
        }

        fn default_input_device(&self) -> Result<Option<AudioDeviceID>> {
            Ok(self.default_input)
        }
    }

    #[test]
    fn test_mic_controller_new() {
        // Should succeed (even if no input devices)
        let result = MicController::new();
        assert!(result.is_ok());
    }

    #[test]
    fn test_mic_controller_default() {
        let c: MicController = MicController::default();
        assert!(!c.muted);
        assert!(!c.should_enforce_mute());
    }

    #[test]
    fn test_toggle_logic_uses_desired_state() {
        assert!(target_state(None, false));
        assert!(!target_state(None, true));
        assert!(!target_state(Some(false), true));
    }

    #[test]
    fn native_mute_requires_readback_confirmation() {
        let backend = FakeBackend::with_devices(vec![(1, Device::native("Built-in", false))]);
        let mut controller = MicController::with_backend(backend).unwrap();

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn native_mute_failure_does_not_claim_muted_but_keeps_enforcing() {
        let mut device = Device::native("Built-in", false);
        device.fail_set_mute = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = MicController::with_backend(backend).unwrap();

        let result = controller.mute_all(true);

        assert!(result.is_err());
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn native_mute_readback_mismatch_does_not_claim_muted() {
        let mut device = Device::native("Built-in", false);
        device.ignore_set_mute = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = MicController::with_backend(backend).unwrap();

        let result = controller.mute_all(true);

        assert!(result.is_err());
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn unsupported_native_mute_uses_verified_volume_fallback() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.65))]);
        let mut controller = MicController::with_backend(backend).unwrap();

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, 0.0);
        assert_eq!(controller.saved_volumes.get(&1), Some(&0.65));
        assert!(controller.volume_fallback_devices.contains(&1));
    }

    #[test]
    fn volume_fallback_failure_does_not_claim_muted() {
        let mut device = Device::fallback("Continuity", 0.65);
        device.fail_set_volume = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = MicController::with_backend(backend).unwrap();

        let result = controller.mute_all(true);

        assert!(result.is_err());
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn startup_detects_volume_muted_fallback_device_as_muted() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.0))]);
        let controller = MicController::with_backend(backend).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn fallback_unmute_restores_audible_volume() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.65))]);
        let mut controller = MicController::with_backend(backend).unwrap();
        controller.mute_all(true).unwrap();

        controller.mute_all(false).unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, 0.65);
        assert!(controller.saved_volumes.is_empty());
        assert!(controller.volume_fallback_devices.is_empty());
    }

    #[test]
    fn fallback_unmute_uses_audible_default_when_saved_volume_was_zero() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.0))]);
        let mut controller = MicController::with_backend(backend).unwrap();
        controller.saved_volumes.insert(1, 0.0);
        controller.volume_fallback_devices = HashSet::from([1]);

        controller.mute_all(false).unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, 1.0);
    }
}
