use crate::device_records::{
    DeviceKey, InputLevels, RestoreJournal, JOURNAL_FILE_NAME, LEVELS_FILE_NAME,
};
use crate::store::state_file;
use anyhow::{anyhow, Context, Result};
use coreaudio::audio_unit::macos_helpers::{get_audio_device_ids, get_device_name};
use log::{error, info, trace, warn};
use objc2_core_audio::{
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyMute, kAudioDevicePropertyScopeInput,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyVolumeScalar,
    kAudioHardwareNoError, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwareUnknownPropertyError, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, AudioDeviceID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectIsPropertySettable, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::{CFRetained, CFString};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ptr::{null, NonNull};
use std::time::Instant;

const SYSTEM_OBJECT_ID: AudioDeviceID = 1;
const VOLUME_MUTED_EPSILON: f32 = 0.000_001;
/// Unmute level for a volume-controlled device Mic Mute has never seen audible.
const UNMUTE_FALLBACK_VOLUME: f32 = 0.5;
const AUDIO_HARDWARE_SERVICE_DEVICE_PROPERTY_VIRTUAL_MAIN_VOLUME: AudioObjectPropertySelector =
    0x766d7663; // 'vmvc'

fn is_volume_muted(volume: f32) -> bool {
    volume <= VOLUME_MUTED_EPSILON
}

fn target_state(state: Option<bool>, desired_muted: bool) -> bool {
    state.unwrap_or(!desired_muted)
}

fn state_name(state: bool) -> &'static str {
    if state {
        "muted"
    } else {
        "unmuted"
    }
}

fn action_name(state: bool) -> &'static str {
    if state {
        "mute"
    } else {
        "unmute"
    }
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
    /// The persistent device UID. Unlike the `AudioDeviceID`, it survives reconnects and
    /// restarts, so the app's records are keyed by it.
    fn device_uid(&self, audio_device_id: AudioDeviceID) -> Result<String>;
    fn has_input_channels(&self, audio_device_id: AudioDeviceID) -> Result<bool>;
    /// Whether the device has a native input mute the app can set.
    fn is_mute_settable(&self, audio_device_id: AudioDeviceID) -> Result<bool>;
    fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>>;
    fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>>;
    fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<Option<f32>>;
    fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<Option<()>>;
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

    fn volume_addresses() -> [AudioObjectPropertyAddress; 2] {
        [
            AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            },
            AudioObjectPropertyAddress {
                mSelector: AUDIO_HARDWARE_SERVICE_DEVICE_PROPERTY_VIRTUAL_MAIN_VOLUME,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            },
        ]
    }

    fn is_property_settable(
        audio_device_id: AudioDeviceID,
        mut property_address: AudioObjectPropertyAddress,
        operation: &str,
    ) -> Result<bool> {
        let mut is_settable = 0u8;
        let status = unsafe {
            AudioObjectIsPropertySettable(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                NonNull::new_unchecked(&mut is_settable),
            )
        };
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(false);
        }
        status_result(status, operation, audio_device_id)?;
        Ok(is_settable != 0)
    }

    fn get_settable_volume(
        audio_device_id: AudioDeviceID,
        mut property_address: AudioObjectPropertyAddress,
    ) -> Result<Option<f32>> {
        if !Self::is_property_settable(
            audio_device_id,
            property_address,
            "check input volume settable",
        )? {
            return Ok(None);
        }

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
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(None);
        }
        status_result(status, "read input volume", audio_device_id)?;
        Ok(Some(volume))
    }

    fn set_settable_volume(
        audio_device_id: AudioDeviceID,
        mut property_address: AudioObjectPropertyAddress,
        volume: f32,
    ) -> Result<Option<()>> {
        if !Self::is_property_settable(
            audio_device_id,
            property_address,
            "check input volume settable",
        )? {
            return Ok(None);
        }

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
        if status == kAudioHardwareUnknownPropertyError {
            return Ok(None);
        }
        status_result(status, "set input volume", audio_device_id)?;
        Ok(Some(()))
    }
}

impl AudioBackend for CoreAudioBackend {
    fn device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        get_audio_device_ids().map_err(anyhow::Error::msg)
    }

    fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String> {
        get_device_name(audio_device_id).map_err(anyhow::Error::msg)
    }

    fn device_uid(&self, audio_device_id: AudioDeviceID) -> Result<String> {
        // The UID belongs to the whole device, so it lives on the global scope.
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceUID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut uid: *const CFString = null();
        let mut data_size = mem::size_of::<*const CFString>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::new_unchecked(&mut property_address),
                0,
                null(),
                NonNull::new_unchecked(&mut data_size),
                NonNull::new_unchecked(&mut uid as *mut *const CFString as *mut c_void),
            )
        };
        status_result(status, "read device UID", audio_device_id)?;
        if data_size as usize != mem::size_of::<*const CFString>() {
            return Err(anyhow!(
                "device UID of audio device {} has unexpected size {}",
                audio_device_id,
                data_size
            ));
        }
        let uid = NonNull::new(uid.cast_mut())
            .ok_or_else(|| anyhow!("audio device {} returned no device UID", audio_device_id))?;
        // CoreAudio returns the string retained (+1); CFRetained releases it on drop.
        let uid = unsafe { CFRetained::from_raw(uid) }.to_string();
        if uid.is_empty() {
            return Err(anyhow!(
                "audio device {} returned an empty device UID",
                audio_device_id
            ));
        }
        Ok(uid)
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

    fn is_mute_settable(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
        Self::is_property_settable(audio_device_id, Self::mute_address(), "check mute settable")
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
        if !self.is_mute_settable(audio_device_id)? {
            return Ok(None);
        }
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

    fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<Option<f32>> {
        for property_address in Self::volume_addresses() {
            if let Some(volume) = Self::get_settable_volume(audio_device_id, property_address)? {
                return Ok(Some(volume));
            }
        }
        Ok(None)
    }

    fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<Option<()>> {
        for property_address in Self::volume_addresses() {
            if Self::set_settable_volume(audio_device_id, property_address, volume)?.is_some() {
                return Ok(Some(()));
            }
        }
        Ok(None)
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

/// The control the app mutes a device with: native mute when it is settable, else input
/// volume. Chosen from capabilities alone, never from device names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuteControl {
    Native,
    Volume,
}

fn mute_control<B: AudioBackend>(
    backend: &B,
    audio_device_id: AudioDeviceID,
) -> Result<Option<MuteControl>> {
    if backend.is_mute_settable(audio_device_id)? {
        return Ok(Some(MuteControl::Native));
    }
    Ok(backend
        .get_volume(audio_device_id)?
        .map(|_| MuteControl::Volume))
}

fn device_key<B: AudioBackend>(backend: &B, audio_device_id: AudioDeviceID) -> DeviceKey {
    match backend.device_uid(audio_device_id) {
        Ok(uid) => DeviceKey::Uid(uid),
        Err(err) => {
            trace!(
                "Using a session-only record key for audio device {}: {}",
                audio_device_id,
                err
            );
            DeviceKey::Session(audio_device_id)
        }
    }
}

fn input_device_ids<B: AudioBackend>(backend: &B) -> Result<Vec<AudioDeviceID>> {
    let audio_device_ids = backend.device_ids()?;
    trace!(
        "All {} audio device IDs {:?}",
        audio_device_ids.len(),
        audio_device_ids
    );
    let mut input_device_ids = vec![];
    for id in audio_device_ids {
        if backend.has_input_channels(id)? {
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

fn read_muted<B: AudioBackend>(
    backend: &B,
    audio_device_id: AudioDeviceID,
) -> Result<Option<bool>> {
    let name = backend.device_name(audio_device_id)?;
    trace!(
        "Reading mute state for device {} - {}",
        audio_device_id,
        name
    );

    match backend.get_mute(audio_device_id)? {
        Some(muted) => Ok(Some(muted)),
        None => match backend.get_volume(audio_device_id)? {
            Some(volume) => Ok(Some(is_volume_muted(volume))),
            None => {
                trace!(
                    "Audio device {} has no supported mute or input volume control",
                    audio_device_id
                );
                Ok(None)
            }
        },
    }
}

fn wait_for_state<B: AudioBackend>(
    backend: &B,
    audio_device_id: AudioDeviceID,
    state: bool,
) -> Result<bool> {
    for attempt in 0..5 {
        if read_muted(backend, audio_device_id)? == Some(state) {
            return Ok(true);
        }
        if attempt != 4 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(false)
}

/// The connected devices that have journaled changes. Matching by UID alone means a device
/// whose stream configuration can't be read doesn't block restoring the others.
fn journaled_devices<B: AudioBackend>(
    backend: &B,
    journal: &RestoreJournal,
) -> Result<Vec<(AudioDeviceID, DeviceKey)>> {
    let mut devices = Vec::new();
    for id in backend.device_ids()? {
        let key = device_key(backend, id);
        if journal.get(&key).is_some() {
            devices.push((id, key));
        }
    }
    Ok(devices)
}

fn restore_native_mute<B: AudioBackend>(
    backend: &mut B,
    audio_device_id: AudioDeviceID,
) -> Result<()> {
    if backend.set_mute(audio_device_id, false)?.is_none() {
        return Err(anyhow!(
            "native mute of audio device {} is no longer settable",
            audio_device_id
        ));
    }
    if !wait_for_state(backend, audio_device_id, false)? {
        return Err(anyhow!(
            "audio device {} remained muted after native unmute",
            audio_device_id
        ));
    }
    Ok(())
}

/// Writes a saved level back to a device that is still volume-muted. Returns false when
/// there was nothing to write: the device is audible again, or the level isn't audible.
fn restore_saved_volume<B: AudioBackend>(
    backend: &mut B,
    audio_device_id: AudioDeviceID,
    volume: f32,
) -> Result<bool> {
    let Some(current_vol) = backend.get_volume(audio_device_id)? else {
        return Err(anyhow!(
            "input volume of audio device {} is no longer settable",
            audio_device_id
        ));
    };
    if !is_volume_muted(current_vol) || is_volume_muted(volume) {
        return Ok(false);
    }
    if backend.set_volume(audio_device_id, volume)?.is_none() {
        return Err(anyhow!(
            "input volume of audio device {} is no longer settable",
            audio_device_id
        ));
    }
    match backend.get_volume(audio_device_id)? {
        Some(restored) if !is_volume_muted(restored) => Ok(true),
        Some(_) => Err(anyhow!(
            "audio device {} input volume remained muted after restoring {:.3}",
            audio_device_id,
            volume
        )),
        None => Err(anyhow!(
            "input volume of audio device {} became unreadable",
            audio_device_id
        )),
    }
}

/// Undoes the journaled changes on the given connected devices. Restored changes leave the
/// journal; failed ones stay, and their failures are returned.
fn restore_devices<B: AudioBackend>(
    backend: &mut B,
    journal: &mut RestoreJournal,
    devices: &[(AudioDeviceID, DeviceKey)],
) -> Vec<String> {
    let mut failures = Vec::new();
    for (id, key) in devices {
        let Some(changes) = journal.get(key).cloned() else {
            continue;
        };
        let name = backend.device_name(*id).unwrap_or(changes.name);
        let mut restored = Vec::new();
        if changes.native_mute {
            match restore_native_mute(backend, *id) {
                Ok(()) => {
                    journal.clear_native_mute(key);
                    restored.push("native mute off".to_string());
                }
                Err(err) => failures.push(format!("{} ({})", name, err)),
            }
        }
        if let Some(volume) = changes.saved_volume {
            match restore_saved_volume(backend, *id, volume) {
                Ok(is_written) => {
                    journal.clear_saved_volume(key);
                    if is_written {
                        restored.push(format!("input volume {:.3}", volume));
                    }
                }
                Err(err) => failures.push(format!("{} ({})", name, err)),
            }
        }
        if !restored.is_empty() {
            info!(
                "Restored audio device {}: {}: {}",
                id,
                name,
                restored.join(", ")
            );
        }
    }
    failures
}

/// Undoes the journaled changes on every connected device. Devices that aren't connected stay
/// in the journal.
fn restore_journaled_devices<B: AudioBackend>(
    backend: &mut B,
    journal: &mut RestoreJournal,
) -> Vec<String> {
    if journal.is_empty() {
        return Vec::new();
    }
    match journaled_devices(backend, journal) {
        Ok(devices) => restore_devices(backend, journal, &devices),
        Err(err) => vec![format!("input devices ({})", err)],
    }
}

pub struct MicController<B = CoreAudioBackend> {
    pub muted: bool,
    desired_muted: bool,
    /// What the app changed per device and must undo, keyed by device UID and mirrored to
    /// `restore.json`.
    journal: RestoreJournal,
    /// The last audible level of each volume-controlled device, mirrored to
    /// `input-levels.json`.
    levels: InputLevels,
    /// Journaled devices that were connected at the last unmuted poll. A journaled device that
    /// joins this set gets restored once; later failures wait for the next unmute or exit.
    connected_journaled: HashSet<DeviceKey>,
    backend: B,
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
    /// A controller for the real devices, with its records in the settings folder.
    pub fn new() -> Result<Self> {
        Self::with_backend(
            CoreAudioBackend,
            RestoreJournal::load(state_file(JOURNAL_FILE_NAME)),
            InputLevels::load(state_file(LEVELS_FILE_NAME)),
        )
    }
}

impl<B: AudioBackend> MicController<B> {
    fn with_backend(backend: B, journal: RestoreJournal, levels: InputLevels) -> Result<Self> {
        let mut controller = Self {
            muted: false,
            desired_muted: false,
            journal,
            levels,
            connected_journaled: HashSet::new(),
            backend,
        };
        trace!("Creating audio controller");
        let names = controller.names()?;
        trace!("Found {} devices: {}", names.len(), names.join(", "));
        // Undo what an unclean exit left behind before the hardware state becomes user intent.
        controller.restore_after_unclean_exit();
        controller.record_audible_levels();
        controller.muted = controller.is_muted_all()?;
        controller.desired_muted = controller.muted;
        Ok(controller)
    }

    fn restore_after_unclean_exit(&mut self) {
        if !self.journal.is_empty() {
            info!(
                "{} lists {} device(s) left changed by an unclean exit; restoring them",
                JOURNAL_FILE_NAME,
                self.journal.len()
            );
            let failures = restore_journaled_devices(&mut self.backend, &mut self.journal);
            if !failures.is_empty() {
                error!(
                    "Failed to restore {} input device(s) after an unclean exit: {}",
                    failures.len(),
                    failures.join("; ")
                );
            }
            if !self.journal.is_empty() {
                info!(
                    "Keeping {} device(s) in {} until they can be restored",
                    self.journal.len(),
                    JOURNAL_FILE_NAME
                );
            }
        }
        self.journal.save_logged();
        self.connected_journaled = self.connected_journaled_keys();
    }

    fn connected_journaled_keys(&self) -> HashSet<DeviceKey> {
        if self.journal.is_empty() {
            return HashSet::new();
        }
        match journaled_devices(&self.backend, &self.journal) {
            Ok(devices) => devices.into_iter().map(|(_, key)| key).collect(),
            Err(err) => {
                trace!("Could not list journaled input devices: {}", err);
                HashSet::new()
            }
        }
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
        input_device_ids(&self.backend)
    }

    fn is_muted(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>> {
        read_muted(&self.backend, audio_device_id)
    }

    fn is_muted_all(&self) -> Result<bool> {
        let mut controllable = false;
        for id in &self.get_input_device_ids()? {
            match self.is_muted(*id)? {
                Some(state) => {
                    controllable = true;
                    trace!("Input device {} is {}", id, state_name(state));
                    if !state {
                        return Ok(false);
                    }
                }
                None => trace!(
                    "Input device {} is not controllable by CoreAudio mute or volume",
                    id
                ),
            }
        }
        Ok(controllable)
    }

    fn all_devices_match_state(&self, ids: &[AudioDeviceID], state: bool) -> Result<bool> {
        for id in ids {
            if let Some(actual) = self.is_muted(*id)? {
                if actual != state {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn wait_for_device_state(&self, audio_device_id: AudioDeviceID, state: bool) -> Result<bool> {
        wait_for_state(&self.backend, audio_device_id, state)
    }

    fn mute(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        state: bool,
    ) -> Result<Option<AudioDeviceID>> {
        let control = mute_control(&self.backend, audio_device_id)?;
        let mut is_handled = false;
        if let Some(control) = control {
            let key = device_key(&self.backend, audio_device_id);
            if control == MuteControl::Native {
                is_handled = self.mute_native(audio_device_id, name, &key, state)?;
            }
            if !is_handled {
                trace!(
                    "Device {} doesn't support mute property; falling back to input volume",
                    audio_device_id
                );
                is_handled = self.mute_via_volume(audio_device_id, name, &key, state)?;
            }
        }
        if !is_handled {
            trace!(
                "Skipping audio device {} because neither native mute nor input volume is controllable",
                audio_device_id
            );
            return Ok(None);
        }

        let state_text = match self.is_muted(audio_device_id)? {
            Some(true) => "muted",
            Some(false) => "unmuted",
            None => "uncontrollable",
        };
        trace!("Device {} now registers as {}", audio_device_id, state_text);
        Ok(Some(audio_device_id))
    }

    /// Sets native mute and confirms it by readback. Returns false when native mute turns out
    /// not to be settable.
    fn mute_native(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        state: bool,
    ) -> Result<bool> {
        let is_recorded = state
            && self.is_muted(audio_device_id)? == Some(false)
            && self.journal.record_native_mute(key, name);
        if is_recorded {
            // Journal the change before making it, so a crash can't leave it unrecorded.
            self.journal.save_logged();
        }
        if self.backend.set_mute(audio_device_id, state)?.is_none() {
            if is_recorded {
                self.journal.clear_native_mute(key);
            }
            return Ok(false);
        }
        if !self.wait_for_device_state(audio_device_id, state)? {
            return Err(anyhow!(
                "audio device {} did not reach requested mute state {} after native mute set",
                audio_device_id,
                state
            ));
        }
        if !state {
            self.journal.clear_native_mute(key);
        }
        Ok(true)
    }

    /// Mutes by setting input volume to 0, or unmutes by restoring a level. Returns false when
    /// the device has no settable input volume.
    fn mute_via_volume(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        state: bool,
    ) -> Result<bool> {
        if !state {
            return self.unmute_via_volume(audio_device_id, name, key);
        }
        let Some(current_vol) = self.backend.get_volume(audio_device_id)? else {
            return Ok(false);
        };
        if !is_volume_muted(current_vol) {
            self.levels.record(key, name, current_vol);
            if self.journal.record_saved_volume(key, name, current_vol) {
                trace!(
                    "Saving volume {:.3} for device {} before muting",
                    current_vol,
                    audio_device_id
                );
                // Journal the level before changing it, so a crash can't lose it.
                self.journal.save_logged();
            }
        }
        if self.backend.set_volume(audio_device_id, 0.0)?.is_none() {
            return Ok(false);
        }
        let Some(volume) = self.backend.get_volume(audio_device_id)? else {
            return Ok(false);
        };
        if !is_volume_muted(volume) {
            return Err(anyhow!(
                "audio device {} input volume remained {:.3} after fallback mute",
                audio_device_id,
                volume
            ));
        }
        Ok(true)
    }

    fn unmute_via_volume(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
    ) -> Result<bool> {
        let Some(current_vol) = self.backend.get_volume(audio_device_id)? else {
            return Ok(false);
        };
        if !is_volume_muted(current_vol) {
            // Already audible: nothing to restore, and any saved level is stale.
            self.journal.clear_saved_volume(key);
            return Ok(true);
        }
        let restore_vol = self.unmute_volume(audio_device_id, name, key);
        trace!(
            "Restoring volume {:.3} for device {}",
            restore_vol,
            audio_device_id
        );
        if self
            .backend
            .set_volume(audio_device_id, restore_vol)?
            .is_none()
        {
            return Ok(false);
        }
        let Some(volume) = self.backend.get_volume(audio_device_id)? else {
            return Ok(false);
        };
        if is_volume_muted(volume) {
            // The saved level stays journaled for the next attempt and for exit.
            return Err(anyhow!(
                "audio device {} input volume remained muted after fallback unmute",
                audio_device_id
            ));
        }
        self.journal.clear_saved_volume(key);
        self.levels.record(key, name, volume);
        Ok(true)
    }

    /// The level to unmute a volume-muted device at: the level saved when the app muted it,
    /// else the last audible level recorded, else [`UNMUTE_FALLBACK_VOLUME`].
    fn unmute_volume(&self, audio_device_id: AudioDeviceID, name: &str, key: &DeviceKey) -> f32 {
        let is_audible = |volume: &f32| !is_volume_muted(*volume);
        let known = self
            .journal
            .saved_volume(key)
            .filter(is_audible)
            .or_else(|| self.levels.get(key).filter(is_audible));
        known.unwrap_or_else(|| {
            warn!(
                "No audible input level is known for audio device {}: {}; unmuting at {:.2}",
                audio_device_id, name, UNMUTE_FALLBACK_VOLUME
            );
            UNMUTE_FALLBACK_VOLUME
        })
    }

    pub fn mute_all(&mut self, state: bool) -> Result<&Self> {
        self.desired_muted = state;
        let ids = self.get_input_device_ids()?;
        let action = action_name(state);
        let end_state = state_name(state);
        let mut failures = Vec::new();
        for id in &ids {
            let name = self.backend.device_name(*id)?;
            trace!("Setting mute={} for {}", state, name);
            match self.mute(*id, &name, state) {
                Ok(Some(_)) => trace!("Successfully {} audio device {}: {}", end_state, id, name),
                Ok(None) => trace!(
                    "Skipped audio device {}: {} because it has no supported mute control",
                    id,
                    name
                ),
                Err(err) => {
                    error!(
                        "Failed to {} audio device {}: {}: {}",
                        action, id, name, err
                    );
                    failures.push(format!("{} ({})", name, err));
                }
            }
        }
        // Drop the entries of devices that unmuting restored; a no-op when nothing changed.
        self.journal.save_logged();

        self.muted = self.is_muted_all().unwrap_or(false);
        if !failures.is_empty() {
            return Err(anyhow!(
                "failed to {} {} input device(s): {}",
                action,
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
        self.muted = self.is_muted_all()?;
        Ok(self)
    }

    /// Undoes every journaled change on connected devices and saves pending records.
    /// Restoring twice is a no-op, so every exit path can call it.
    pub fn restore_on_exit(&mut self) -> Result<()> {
        let failures = restore_journaled_devices(&mut self.backend, &mut self.journal);
        // Deletes restore.json after a clean, complete restore; failed and disconnected
        // devices stay listed for the next launch.
        self.journal.save_logged();
        self.levels.save();
        if failures.is_empty() && !self.journal.is_empty() {
            info!(
                "Keeping {} disconnected device(s) in {} for the next launch",
                self.journal.len(),
                JOURNAL_FILE_NAME
            );
        }

        self.muted = self.is_muted_all().unwrap_or(false);
        self.desired_muted = self.muted;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "failed to restore {} input device(s) on exit: {}",
                failures.len(),
                failures.join("; ")
            ))
        }
    }

    /// Runs on every poll while the user's intent is unmuted.
    pub fn watch_unmuted(&mut self) {
        self.restore_reconnected_devices();
        self.record_audible_levels();
    }

    /// Restores journaled devices that joined the connected set since the last poll: devices
    /// that were away when they should have been restored, or whose unmute just failed.
    fn restore_reconnected_devices(&mut self) {
        if self.journal.is_empty() {
            self.connected_journaled.clear();
            return;
        }
        let devices = match journaled_devices(&self.backend, &self.journal) {
            Ok(devices) => devices,
            Err(err) => {
                trace!("Could not list journaled input devices: {}", err);
                return;
            }
        };
        let reconnected: Vec<_> = devices
            .iter()
            .filter(|(_, key)| !self.connected_journaled.contains(key))
            .cloned()
            .collect();
        self.connected_journaled = devices.into_iter().map(|(_, key)| key).collect();
        if reconnected.is_empty() {
            return;
        }
        let failures = restore_devices(&mut self.backend, &mut self.journal, &reconnected);
        if !failures.is_empty() {
            error!(
                "Failed to restore {} reconnected input device(s): {}",
                failures.len(),
                failures.join("; ")
            );
        }
        self.journal.save_logged();
    }

    /// Records the level of every volume-controlled device that is audible now.
    fn record_audible_levels(&mut self) {
        let ids = match self.get_input_device_ids() {
            Ok(ids) => ids,
            Err(err) => {
                trace!("Could not list input devices to record levels: {}", err);
                return;
            }
        };
        for id in ids {
            if let Err(err) = self.record_audible_level(id) {
                trace!("Could not record the input level of device {}: {}", id, err);
            }
        }
    }

    fn record_audible_level(&mut self, audio_device_id: AudioDeviceID) -> Result<()> {
        if mute_control(&self.backend, audio_device_id)? != Some(MuteControl::Volume) {
            return Ok(());
        }
        let Some(volume) = self.backend.get_volume(audio_device_id)? else {
            return Ok(());
        };
        if is_volume_muted(volume) {
            return Ok(());
        }
        let key = device_key(&self.backend, audio_device_id);
        let name = self.backend.device_name(audio_device_id)?;
        self.levels.record(&key, &name, volume);
        Ok(())
    }

    /// Saves recorded input levels at most once per second; call on every poll.
    pub fn save_input_levels_if_due(&mut self) {
        self.levels.save_if_due(Instant::now());
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
    use crate::store::MemoryStore;
    use std::collections::HashMap;

    #[derive(Clone)]
    struct Device {
        name: String,
        uid: String,
        input: bool,
        mute: Option<bool>,
        volume: Option<f32>,
        fail_uid: bool,
        fail_input_channels: bool,
        fail_set_mute: bool,
        fail_set_volume: bool,
        ignore_set_mute: bool,
        ignore_set_volume: bool,
    }

    impl Device {
        fn new(name: &str, mute: Option<bool>, volume: Option<f32>) -> Self {
            Self {
                name: name.to_string(),
                uid: uid_of(name),
                input: true,
                mute,
                volume,
                fail_uid: false,
                fail_input_channels: false,
                fail_set_mute: false,
                fail_set_volume: false,
                ignore_set_mute: false,
                ignore_set_volume: false,
            }
        }

        fn native(name: &str, muted: bool) -> Self {
            Self::new(name, Some(muted), Some(1.0))
        }

        fn fallback(name: &str, volume: f32) -> Self {
            Self::new(name, None, Some(volume))
        }

        fn no_control(name: &str) -> Self {
            Self::new(name, None, None)
        }
    }

    fn uid_of(name: &str) -> String {
        format!("{} UID", name)
    }

    fn key(name: &str) -> DeviceKey {
        DeviceKey::Uid(uid_of(name))
    }

    #[derive(Default)]
    struct FakeBackend {
        devices: HashMap<AudioDeviceID, Device>,
        ids: Vec<AudioDeviceID>,
        default_input: Option<AudioDeviceID>,
        /// When set, each device write records whether this journal store named the device.
        journal_probe: Option<MemoryStore>,
        journaled_at_write: Vec<(AudioDeviceID, bool)>,
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
                ..Self::default()
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

        fn unplug(&mut self, id: AudioDeviceID) -> Device {
            self.ids.retain(|existing| *existing != id);
            self.devices.remove(&id).unwrap()
        }

        /// Connects a device, e.g. one unplugged earlier, under a new ID.
        fn plug(&mut self, id: AudioDeviceID, device: Device) {
            self.ids.push(id);
            self.devices.insert(id, device);
        }

        fn observe_write(&mut self, id: AudioDeviceID) {
            if let Some(store) = &self.journal_probe {
                let uid = self.devices[&id].uid.clone();
                let is_journaled = store
                    .contents()
                    .is_some_and(|contents| contents.contains(&uid));
                self.journaled_at_write.push((id, is_journaled));
            }
        }
    }

    impl AudioBackend for FakeBackend {
        fn device_ids(&self) -> Result<Vec<AudioDeviceID>> {
            Ok(self.ids.clone())
        }

        fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String> {
            Ok(self.device(audio_device_id)?.name.clone())
        }

        fn device_uid(&self, audio_device_id: AudioDeviceID) -> Result<String> {
            let device = self.device(audio_device_id)?;
            if device.fail_uid {
                return Err(anyhow!("fake device UID failure"));
            }
            Ok(device.uid.clone())
        }

        fn has_input_channels(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
            let device = self.device(audio_device_id)?;
            if device.fail_input_channels {
                return Err(anyhow!("fake stream configuration failure"));
            }
            Ok(device.input)
        }

        fn is_mute_settable(&self, audio_device_id: AudioDeviceID) -> Result<bool> {
            Ok(self.device(audio_device_id)?.mute.is_some())
        }

        fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>> {
            Ok(self.device(audio_device_id)?.mute)
        }

        fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>> {
            self.observe_write(audio_device_id);
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

        fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<Option<f32>> {
            Ok(self.device(audio_device_id)?.volume)
        }

        fn set_volume(
            &mut self,
            audio_device_id: AudioDeviceID,
            volume: f32,
        ) -> Result<Option<()>> {
            self.observe_write(audio_device_id);
            let device = self.device_mut(audio_device_id)?;
            if device.fail_set_volume {
                return Err(anyhow!("fake volume failure"));
            }
            if device.volume.is_none() {
                return Ok(None);
            }
            if !device.ignore_set_volume {
                device.volume = Some(volume);
            }
            Ok(Some(()))
        }

        fn default_input_device(&self) -> Result<Option<AudioDeviceID>> {
            Ok(self.default_input)
        }
    }

    /// In-memory stand-ins for restore.json and input-levels.json, shared across "launches".
    #[derive(Default)]
    struct Stores {
        journal: MemoryStore,
        levels: MemoryStore,
    }

    impl Stores {
        fn launch(&self, backend: FakeBackend) -> MicController<FakeBackend> {
            MicController::with_backend(
                backend,
                RestoreJournal::load(self.journal.boxed()),
                InputLevels::load(self.levels.boxed()),
            )
            .unwrap()
        }

        fn journal_json(&self) -> serde_json::Value {
            serde_json::from_str(&self.journal.contents().unwrap()).unwrap()
        }
    }

    fn controller(backend: FakeBackend) -> MicController<FakeBackend> {
        Stores::default().launch(backend)
    }

    #[test]
    fn test_mic_controller_new() {
        // Reads the real devices; in-memory records keep the real files out of it.
        let stores = Stores::default();
        let result = MicController::with_backend(
            CoreAudioBackend,
            RestoreJournal::load(stores.journal.boxed()),
            InputLevels::load(stores.levels.boxed()),
        );

        assert!(result.is_ok());
        assert_eq!(stores.journal.saves(), 0);
        assert_eq!(stores.levels.saves(), 0);
    }

    #[test]
    fn core_audio_reads_a_device_uid_for_every_input_device() {
        let backend = CoreAudioBackend;
        for id in backend.device_ids().unwrap() {
            if backend.has_input_channels(id).unwrap() {
                assert!(!backend.device_uid(id).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn controller_without_devices_starts_unmuted() {
        let c = controller(FakeBackend::default());
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
    fn state_and_action_names_are_consistent() {
        assert_eq!(state_name(true), "muted");
        assert_eq!(state_name(false), "unmuted");
        assert_eq!(action_name(true), "mute");
        assert_eq!(action_name(false), "unmute");
    }

    #[test]
    fn core_audio_volume_fallback_checks_virtual_main_volume() {
        let addresses = CoreAudioBackend::volume_addresses();

        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].mSelector, kAudioDevicePropertyVolumeScalar);
        assert_eq!(
            addresses[1].mSelector,
            AUDIO_HARDWARE_SERVICE_DEVICE_PROPERTY_VIRTUAL_MAIN_VOLUME
        );
        assert_eq!(addresses[1].mScope, kAudioDevicePropertyScopeInput);
        assert_eq!(addresses[1].mElement, kAudioObjectPropertyElementMain);
    }

    #[test]
    fn native_mute_requires_readback_confirmation() {
        let backend = FakeBackend::with_devices(vec![(1, Device::native("Built-in", false))]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn restore_on_exit_unmutes_native_devices_muted_by_app() {
        let backend = FakeBackend::with_devices(vec![(1, Device::native("Built-in", false))]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.restore_on_exit().unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn restore_on_exit_leaves_preexisting_native_mute_unchanged() {
        let backend = FakeBackend::with_devices(vec![(1, Device::native("Built-in", true))]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.restore_on_exit().unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn native_mute_failure_does_not_claim_muted_but_keeps_enforcing() {
        let mut device = Device::native("Built-in", false);
        device.fail_set_mute = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = controller(backend);

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
        let mut controller = controller(backend);

        let result = controller.mute_all(true);

        assert!(result.is_err());
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn unsupported_native_mute_uses_verified_volume_fallback() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.65))]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.0));
        assert_eq!(
            controller.journal.saved_volume(&key("Continuity")),
            Some(0.65)
        );
    }

    #[test]
    fn restore_on_exit_restores_volume_fallback_devices_muted_by_app() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.65))]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.restore_on_exit().unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn restore_on_exit_leaves_preexisting_volume_mute_unchanged() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.0))]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.restore_on_exit().unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.0));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn volume_fallback_failure_does_not_claim_muted() {
        let mut device = Device::fallback("Continuity", 0.65);
        device.fail_set_volume = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = controller(backend);

        let result = controller.mute_all(true);

        assert!(result.is_err());
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn startup_detects_volume_muted_fallback_device_as_muted() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.0))]);
        let controller = controller(backend);

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn fallback_unmute_restores_audible_volume() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.65))]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.mute_all(false).unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn fallback_unmute_uses_audible_default_when_saved_volume_was_zero() {
        let backend = FakeBackend::with_devices(vec![(1, Device::fallback("Continuity", 0.0))]);
        let mut controller = controller(backend);
        controller
            .journal
            .record_saved_volume(&key("Continuity"), "Continuity", 0.0);

        controller.mute_all(false).unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.5));
    }

    #[test]
    fn device_without_native_mute_or_volume_does_not_block_other_devices() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::no_control("B iPhone Microphone")),
            (2, Device::native("Built-in", false)),
        ]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn only_uncontrollable_devices_do_not_claim_muted() {
        let backend =
            FakeBackend::with_devices(vec![(1, Device::no_control("B iPhone Microphone"))]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    fn native_and_volume_devices() -> FakeBackend {
        FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::fallback("USB Mic", 0.65)),
        ])
    }

    #[test]
    fn journal_is_written_on_mute_and_deleted_after_a_clean_restore() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());

        controller.mute_all(true).unwrap();

        let journal = stores.journal_json();
        assert_eq!(journal["devices"]["Built-in UID"]["native_mute"], true);
        assert_eq!(journal["devices"]["USB Mic UID"]["saved_volume"], 0.65);

        controller.restore_on_exit().unwrap();

        assert_eq!(stores.journal.contents(), None);
        assert_eq!(stores.journal.removes(), 1);
    }

    #[test]
    fn journal_names_each_device_before_the_app_changes_it() {
        let stores = Stores::default();
        let mut backend = native_and_volume_devices();
        backend.journal_probe = Some(stores.journal.clone());
        let mut controller = stores.launch(backend);

        controller.mute_all(true).unwrap();

        let writes = &controller.backend.journaled_at_write;
        assert!(writes.iter().any(|(id, _)| *id == 1));
        assert!(writes.iter().any(|(id, _)| *id == 2));
        assert!(writes.iter().all(|(_, is_journaled)| *is_journaled));
    }

    #[test]
    fn mute_polls_do_not_rewrite_an_unchanged_journal() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        assert_eq!(stores.journal.saves(), 2);

        for _ in 0..5 {
            controller.toggle(Some(true)).unwrap();
        }

        assert_eq!(stores.journal.saves(), 2);
    }

    #[test]
    fn restore_on_exit_twice_is_a_no_op() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.restore_on_exit().unwrap();
        let writes = (stores.journal.saves(), stores.journal.removes());

        controller.restore_on_exit().unwrap();

        assert_eq!((stores.journal.saves(), stores.journal.removes()), writes);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
    }

    #[test]
    fn stale_journal_is_restored_at_startup_and_not_adopted_as_intent() {
        let stores = Stores::default();
        let mut crashed = stores.launch(native_and_volume_devices());
        crashed.mute_all(true).unwrap();

        // A crash: the controller is gone without restoring, and the devices stay muted.
        let relaunched = stores.launch(crashed.backend);

        assert!(!relaunched.muted);
        assert!(!relaunched.should_enforce_mute());
        assert_eq!(relaunched.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(relaunched.backend.device(2).unwrap().volume, Some(0.65));
        assert!(relaunched.journal.is_empty());
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn disconnected_device_stays_journaled_and_is_restored_when_it_reconnects() {
        let stores = Stores::default();
        let mut crashed = stores.launch(native_and_volume_devices());
        crashed.mute_all(true).unwrap();
        let mut backend = crashed.backend;
        let usb = backend.unplug(2);

        let mut relaunched = stores.launch(backend);

        assert_eq!(relaunched.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(relaunched.journal.saved_volume(&key("USB Mic")), Some(0.65));
        let journal = stores.journal_json();
        assert!(journal["devices"].get("Built-in UID").is_none());
        assert_eq!(journal["devices"]["USB Mic UID"]["saved_volume"], 0.65);

        relaunched.backend.plug(9, usb);
        relaunched.watch_unmuted();

        assert_eq!(relaunched.backend.device(9).unwrap().volume, Some(0.65));
        assert!(relaunched.journal.is_empty());
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn device_reconnecting_while_muted_keeps_its_saved_level_for_the_next_unmute() {
        let stores = Stores::default();
        let mut crashed = stores.launch(native_and_volume_devices());
        crashed.mute_all(true).unwrap();
        let mut backend = crashed.backend;
        let usb = backend.unplug(2);
        let mut relaunched = stores.launch(backend);
        relaunched.mute_all(true).unwrap();

        relaunched.backend.plug(9, usb);
        relaunched.toggle(Some(true)).unwrap();

        assert_eq!(relaunched.backend.device(9).unwrap().volume, Some(0.0));
        assert_eq!(relaunched.journal.saved_volume(&key("USB Mic")), Some(0.65));

        relaunched.mute_all(false).unwrap();

        assert_eq!(relaunched.backend.device(9).unwrap().volume, Some(0.65));
        assert!(relaunched.journal.is_empty());
    }

    #[test]
    fn failed_restore_keeps_its_journal_entries() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().fail_set_mute = true;

        assert!(controller.restore_on_exit().is_err());

        assert!(
            controller
                .journal
                .get(&key("Built-in"))
                .unwrap()
                .native_mute
        );
        assert!(controller.journal.get(&key("USB Mic")).is_none());
        let journal = stores.journal_json();
        assert_eq!(journal["devices"]["Built-in UID"]["native_mute"], true);
        assert!(journal["devices"].get("USB Mic UID").is_none());
    }

    #[test]
    fn failed_unmute_write_keeps_the_saved_level() {
        let stores = Stores::default();
        let mut controller = stores.launch(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("USB Mic", 0.65),
        )]));
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().fail_set_volume = true;

        assert!(controller.mute_all(false).is_err());

        assert_eq!(controller.journal.saved_volume(&key("USB Mic")), Some(0.65));
        assert_eq!(
            stores.journal_json()["devices"]["USB Mic UID"]["saved_volume"],
            0.65
        );

        controller.backend.device_mut(1).unwrap().fail_set_volume = false;
        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn unconfirmed_unmute_keeps_the_saved_level_for_the_next_attempt() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("USB Mic", 0.65),
        )]));
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().ignore_set_volume = true;

        assert!(controller.mute_all(false).is_err());
        assert_eq!(controller.journal.saved_volume(&key("USB Mic")), Some(0.65));

        controller.backend.device_mut(1).unwrap().ignore_set_volume = false;
        controller.mute_all(false).unwrap();

        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn failed_unmute_is_retried_once_on_the_next_unmuted_poll() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("USB Mic", 0.65),
        )]));
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().fail_set_volume = true;
        assert!(controller.mute_all(false).is_err());
        controller.backend.device_mut(1).unwrap().fail_set_volume = false;

        controller.watch_unmuted();

        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn replugged_device_restores_its_level_under_a_new_id() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("USB Mic", 0.65),
        )]));
        controller.mute_all(true).unwrap();
        let usb = controller.backend.unplug(1);
        controller.backend.plug(7, usb);

        controller.mute_all(false).unwrap();

        assert_eq!(controller.backend.device(7).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn restart_unmutes_at_the_level_recorded_in_an_earlier_run() {
        let stores = Stores::default();
        let mut first = stores.launch(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("USB Mic", 0.65),
        )]));
        first.restore_on_exit().unwrap();
        assert!(stores.levels.contents().unwrap().contains("0.65"));

        // Something else muted the mic by volume before the next launch.
        let mut backend = first.backend;
        backend.device_mut(1).unwrap().volume = Some(0.0);
        let mut second = stores.launch(backend);
        assert!(second.muted);

        second.mute_all(false).unwrap();

        assert_eq!(second.backend.device(1).unwrap().volume, Some(0.65));
    }

    #[test]
    fn unmute_uses_the_recorded_level_when_none_was_saved_at_mute() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::fallback("Continuity", 0.65),
        )]));
        controller.backend.device_mut(1).unwrap().volume = Some(0.0);
        controller.mute_all(true).unwrap();
        assert_eq!(controller.journal.saved_volume(&key("Continuity")), None);

        controller.mute_all(false).unwrap();

        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.65));
    }

    #[test]
    fn levels_of_volume_controlled_devices_are_recorded_while_unmuted() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.backend.device_mut(2).unwrap().volume = Some(0.4);

        controller.watch_unmuted();
        controller.levels.save();

        assert_eq!(controller.levels.get(&key("USB Mic")), Some(0.4));
        assert_eq!(controller.levels.get(&key("Built-in")), None);
        assert!(stores.levels.contents().unwrap().contains("USB Mic UID"));
    }

    #[test]
    fn hot_plugged_device_level_is_recorded_while_muted() {
        let mut controller = controller(FakeBackend::default());
        controller.mute_all(true).unwrap();

        controller.backend.plug(4, Device::fallback("USB Mic", 0.8));
        controller.toggle(Some(true)).unwrap();

        assert_eq!(controller.levels.get(&key("USB Mic")), Some(0.8));
        assert_eq!(controller.journal.saved_volume(&key("USB Mic")), Some(0.8));
    }

    #[test]
    fn device_without_a_uid_is_restored_in_session_but_never_saved() {
        let stores = Stores::default();
        let mut device = Device::native("No UID", false);
        device.fail_uid = true;
        let mut controller = stores.launch(FakeBackend::with_devices(vec![(1, device)]));

        controller.mute_all(true).unwrap();
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(stores.journal.contents(), None);

        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
    }

    #[test]
    fn restore_on_exit_does_not_depend_on_reading_stream_configurations() {
        let mut controller = controller(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller
            .backend
            .device_mut(2)
            .unwrap()
            .fail_input_channels = true;

        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }
}
