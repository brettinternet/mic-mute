use crate::device_changes::{self, DeviceChangeSignal};
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
    kAudioHardwareNoError, kAudioHardwarePropertyDevices, kAudioHardwareUnknownPropertyError,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    AudioDeviceID, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectHasProperty, AudioObjectID,
    AudioObjectIsPropertySettable, AudioObjectPropertyAddress, AudioObjectPropertyListenerProc,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListener, AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::{CFRetained, CFString};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::c_void;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ptr::{null, NonNull};
use std::time::{Duration, Instant};

const VOLUME_MUTED_EPSILON: f32 = 0.000_001;
/// Unmute level for a volume-controlled device Mic Mute has never seen audible.
const UNMUTE_FALLBACK_VOLUME: f32 = 0.5;
/// Native mute readbacks after a write, [`AudioBackend::readback_retry_delay`] apart.
const NATIVE_READBACK_ATTEMPTS: u32 = 5;
/// How often a device whose accepted writes never take effect is written to again.
const IGNORED_WRITES_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// Identical repeats of a failure stay at `trace!` this long before an `error!` counts them.
const FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// While unmuted, how often the device states for the tray's Microphones menu are read
/// again; a mute poll reads them anyway, and a device that comes or goes shows at once.
const DEVICE_STATES_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const AUDIO_HARDWARE_SERVICE_DEVICE_PROPERTY_VIRTUAL_MAIN_VOLUME: AudioObjectPropertySelector =
    0x766d7663; // 'vmvc'
/// The object that holds the device list.
const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;
/// The listener proc of every registration. CoreAudio matches a removal by this pointer, so
/// both take it from here.
static PROPERTY_LISTENER: AudioObjectPropertyListenerProc = Some(device_changes::property_listener);

fn is_volume_muted(volume: f32) -> bool {
    volume <= VOLUME_MUTED_EPSILON
}

/// The state a request asks for: `state`, or for a toggle (`None`) the opposite of `is_muted`,
/// the state the tray and popup show.
fn target_state(state: Option<bool>, is_muted: bool) -> bool {
    state.unwrap_or(!is_muted)
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

/// [`status_result`] for the system object, which holds the device list.
fn system_status_result(status: i32, operation: &str) -> Result<()> {
    if status == kAudioHardwareNoError {
        Ok(())
    } else {
        Err(anyhow!(
            "{} failed for the audio system object with OSStatus {}",
            operation,
            status
        ))
    }
}

/// Joins per-device failures into one error; `None` when nothing failed.
fn combined_error(failures: Vec<anyhow::Error>) -> Option<anyhow::Error> {
    failures
        .into_iter()
        .reduce(|first, next| anyhow!("{}; {}", first, next))
}

/// The client data a listener registration passes CoreAudio, which hands it to
/// [`device_changes::property_listener`].
fn listener_client_data(signal: &'static DeviceChangeSignal) -> *mut c_void {
    (signal as *const DeviceChangeSignal).cast_mut().cast()
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
    /// How long to wait between native mute readbacks while a write takes effect.
    fn readback_retry_delay(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<Option<f32>>;
    fn set_volume(&mut self, audio_device_id: AudioDeviceID, volume: f32) -> Result<Option<()>>;
    /// Has CoreAudio call `signal` whenever a device comes or goes.
    fn listen_to_device_list(&mut self, signal: &'static DeviceChangeSignal) -> Result<()>;
    fn stop_listening_to_device_list(&mut self, signal: &'static DeviceChangeSignal) -> Result<()>;
    /// Has CoreAudio call `signal` whenever the native mute or an input volume of the device
    /// changes, on each of those controls it has. Returns the selectors it listens to, which
    /// [`Self::stop_listening_to_device`] takes back; none for a device without such controls.
    fn listen_to_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        signal: &'static DeviceChangeSignal,
    ) -> Result<Vec<AudioObjectPropertySelector>>;
    fn stop_listening_to_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        selectors: &[AudioObjectPropertySelector],
        signal: &'static DeviceChangeSignal,
    ) -> Result<()>;
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

    /// The controls a device listener watches: those the app mutes a device with and reads its
    /// state from.
    fn listened_addresses() -> [AudioObjectPropertyAddress; 3] {
        let [volume, virtual_main_volume] = Self::volume_addresses();
        [Self::mute_address(), volume, virtual_main_volume]
    }

    fn device_list_address() -> AudioObjectPropertyAddress {
        // The device list belongs to the system object, so it lives on the global scope.
        AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// Registers [`device_changes::property_listener`] with `signal` on one property of the
    /// object; returns the OSStatus.
    fn add_listener(
        object_id: AudioObjectID,
        mut property_address: AudioObjectPropertyAddress,
        signal: &'static DeviceChangeSignal,
    ) -> i32 {
        unsafe {
            AudioObjectAddPropertyListener(
                object_id,
                NonNull::from(&mut property_address),
                PROPERTY_LISTENER,
                listener_client_data(signal),
            )
        }
    }

    /// Removes what [`Self::add_listener`] registered; returns the OSStatus.
    fn remove_listener(
        object_id: AudioObjectID,
        mut property_address: AudioObjectPropertyAddress,
        signal: &'static DeviceChangeSignal,
    ) -> i32 {
        unsafe {
            AudioObjectRemovePropertyListener(
                object_id,
                NonNull::from(&mut property_address),
                PROPERTY_LISTENER,
                listener_client_data(signal),
            )
        }
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

    fn listen_to_device_list(&mut self, signal: &'static DeviceChangeSignal) -> Result<()> {
        let status = Self::add_listener(SYSTEM_OBJECT, Self::device_list_address(), signal);
        system_status_result(status, "listen for device list changes")
    }

    fn stop_listening_to_device_list(&mut self, signal: &'static DeviceChangeSignal) -> Result<()> {
        let status = Self::remove_listener(SYSTEM_OBJECT, Self::device_list_address(), signal);
        system_status_result(status, "stop listening for device list changes")
    }

    fn listen_to_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        signal: &'static DeviceChangeSignal,
    ) -> Result<Vec<AudioObjectPropertySelector>> {
        let mut selectors = Vec::new();
        for mut property_address in Self::listened_addresses() {
            // Only a property the device has can change.
            let has_property = unsafe {
                AudioObjectHasProperty(audio_device_id, NonNull::from(&mut property_address))
            };
            if !has_property {
                continue;
            }
            let status = Self::add_listener(audio_device_id, property_address, signal);
            let operation = "listen for mute and volume changes";
            if let Err(err) = status_result(status, operation, audio_device_id) {
                // Leave nothing half registered: the next poll registers the whole device again.
                if let Err(undo_err) =
                    self.stop_listening_to_device(audio_device_id, &selectors, signal)
                {
                    trace!(
                        "Could not undo the listeners of audio device {}: {}",
                        audio_device_id,
                        undo_err
                    );
                }
                return Err(err);
            }
            selectors.push(property_address.mSelector);
        }
        Ok(selectors)
    }

    fn stop_listening_to_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        selectors: &[AudioObjectPropertySelector],
        signal: &'static DeviceChangeSignal,
    ) -> Result<()> {
        let mut failures = Vec::new();
        for property_address in Self::listened_addresses() {
            if !selectors.contains(&property_address.mSelector) {
                continue;
            }
            let status = Self::remove_listener(audio_device_id, property_address, signal);
            let operation = "stop listening for mute and volume changes";
            if let Err(err) = status_result(status, operation, audio_device_id) {
                failures.push(err);
            }
        }
        combined_error(failures).map_or(Ok(()), Err)
    }
}

/// The control the app mutes a device with and reads its state from: native mute when it is
/// settable, else input volume. A native mute that can be read but not set counts as absent.
/// [`mute_control`] picks it from capabilities, and [`WriteBehavior`] adjusts it for devices
/// whose writes don't take effect; device names never play a part.
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

/// How a device took one mute or unmute write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    /// The control reads the requested state: it already did, or the write took effect.
    Confirmed,
    /// The control turned out not to be settable; nothing was written.
    NotSettable,
    /// The device accepted the write, but the control still reads the other state.
    Ignored,
}

/// What writes showed about a device whose controls don't do what they advertise.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WriteBehavior {
    /// Native mute accepts writes but never changes, while input volume works: the app mutes
    /// the device by volume and reads its state there.
    IgnoresNativeMute,
    /// Accepted writes take effect on no control: the device is left out of the mute state
    /// and of enforcement, like a device without controls, until its retry is due.
    IgnoresWrites(WriteRetry),
}

/// When to write again to a device that ignores writes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WriteRetry {
    /// The device's ID at the last attempt; another ID means it reconnected.
    audio_device_id: AudioDeviceID,
    /// Whether a poll has missed the device since; it reconnected once it is back.
    has_disconnected: bool,
    attempted_at: Instant,
}

impl WriteRetry {
    fn new(audio_device_id: AudioDeviceID, now: Instant) -> Self {
        Self {
            audio_device_id,
            has_disconnected: false,
            attempted_at: now,
        }
    }

    /// Whether to write again: [`IGNORED_WRITES_RETRY_INTERVAL`] has passed, or the device
    /// reconnected.
    fn is_due(&self, audio_device_id: AudioDeviceID, now: Instant) -> bool {
        self.has_disconnected
            || self.audio_device_id != audio_device_id
            || now.saturating_duration_since(self.attempted_at) >= IGNORED_WRITES_RETRY_INTERVAL
    }
}

/// What one read of a device found. The app reads a device's state from the control it mutes
/// the device with; the other cases are devices it leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// The user excluded the device: the app leaves it alone and doesn't read it.
    Excluded,
    /// Neither native mute nor input volume can be set.
    NoControl,
    /// Accepted writes take effect on no control ([`WriteBehavior::IgnoresWrites`]).
    IgnoresWrites,
    /// The state the control reads; `None` when it can't be read.
    State(MuteControl, Option<bool>),
}

impl Reading {
    /// The state that counts toward `muted`; `None` for a device the app leaves out.
    fn muted(self) -> Option<bool> {
        match self {
            Self::State(_, state) => state,
            Self::Excluded | Self::NoControl | Self::IgnoresWrites => None,
        }
    }
}

/// Whether the readings show at least one device the app controls, and every such device
/// muted.
fn are_controlled_devices_muted(readings: impl IntoIterator<Item = Reading>) -> bool {
    let mut is_controlling_any = false;
    for state in readings.into_iter().filter_map(Reading::muted) {
        if !state {
            return false;
        }
        is_controlling_any = true;
    }
    is_controlling_any
}

/// Whether `excluded`, a set of device UIDs, names the device with `key`. A device without a
/// readable UID is never excluded, so a failing UID read can't leave it live.
fn is_excluded_by(excluded: &BTreeSet<String>, key: &DeviceKey) -> bool {
    matches!(key, DeviceKey::Uid(uid) if excluded.contains(uid))
}

/// One connected input device as the app last read it, for the tray's Microphones menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    /// The device UID, which an exclusion names; `None` when it can't be read, so the device
    /// can't be excluded.
    pub uid: Option<String>,
    pub name: String,
    pub status: DeviceStatus,
}

/// What the app does with an input device, and the state it reads there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Its native mute is on.
    Muted,
    /// Its input volume is at zero: the fallback for a device without a working native mute.
    MutedByVolume,
    /// The control the app mutes it with reads unmuted.
    Unmuted,
    /// Skipped: it accepts mute writes, but they take effect on no control.
    IgnoresWrites,
    /// Skipped: neither native mute nor input volume can be set.
    NoControl,
    /// The last mute or unmute of it failed, or its state can't be read.
    Failed,
    /// The user excluded it, so the app leaves it alone.
    Excluded,
}

/// The status the Microphones menu shows for a device: `reading` is `None` when reading the
/// device failed, and `has_failed` says whether the last mute or unmute of it failed.
fn device_status(reading: Option<Reading>, has_failed: bool) -> DeviceStatus {
    match reading {
        Some(Reading::Excluded) => DeviceStatus::Excluded,
        _ if has_failed => DeviceStatus::Failed,
        None | Some(Reading::State(_, None)) => DeviceStatus::Failed,
        Some(Reading::NoControl) => DeviceStatus::NoControl,
        Some(Reading::IgnoresWrites) => DeviceStatus::IgnoresWrites,
        Some(Reading::State(MuteControl::Native, Some(true))) => DeviceStatus::Muted,
        Some(Reading::State(MuteControl::Volume, Some(true))) => DeviceStatus::MutedByVolume,
        Some(Reading::State(_, Some(false))) => DeviceStatus::Unmuted,
    }
}

/// The device states the Microphones menu shows, and when they were read.
#[derive(Debug, Default)]
struct DeviceSnapshot {
    states: Vec<DeviceState>,
    /// The input devices `states` lists, in order; another list means a device came or went.
    ids: Vec<AudioDeviceID>,
    read_at: Option<Instant>,
    /// Set when something the states show changed without a read, such as the exclusions.
    is_stale: bool,
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

/// What the app calls a device whose name can't be read.
fn unnamed_device(audio_device_id: AudioDeviceID) -> String {
    format!("Audio device {}", audio_device_id)
}

/// The devices a poll mutes and reads.
#[derive(Default)]
struct InputDevices {
    /// The devices with input streams, and those whose streams can't be read: they may be
    /// microphones, so the app mutes and reads them too.
    ids: Vec<AudioDeviceID>,
    /// Why the streams of those devices can't be read; a mute reports them as failed.
    unchecked: HashMap<AudioDeviceID, anyhow::Error>,
}

/// Lists the input devices. Only a device list that can't be read fails: a device whose
/// streams can't be read doesn't hide the others.
fn input_devices<B: AudioBackend>(backend: &B) -> Result<InputDevices> {
    let audio_device_ids = backend.device_ids()?;
    trace!(
        "All {} audio device IDs {:?}",
        audio_device_ids.len(),
        audio_device_ids
    );
    let mut devices = InputDevices::default();
    for id in audio_device_ids {
        match backend.has_input_channels(id) {
            Ok(true) => devices.ids.push(id),
            Ok(false) => {}
            Err(err) => {
                trace!("Treating audio device {} as an input device: {}", id, err);
                devices.ids.push(id);
                devices.unchecked.insert(id, err);
            }
        }
    }

    trace!(
        "Filtered list to {} input device IDs {:?}",
        devices.ids.len(),
        devices.ids
    );
    Ok(devices)
}

/// The device's state as `control` reads it: its native mute, or whether its input volume is
/// at zero. `None` when the control can't be read.
fn read_muted<B: AudioBackend>(
    backend: &B,
    audio_device_id: AudioDeviceID,
    control: MuteControl,
) -> Result<Option<bool>> {
    match control {
        MuteControl::Native => backend.get_mute(audio_device_id),
        MuteControl::Volume => Ok(backend.get_volume(audio_device_id)?.map(is_volume_muted)),
    }
}

/// Reads the native mute until it shows `state`; a write can take a moment to show.
fn wait_for_native_mute<B: AudioBackend>(
    backend: &B,
    audio_device_id: AudioDeviceID,
    state: bool,
) -> Result<bool> {
    for attempt in 1..=NATIVE_READBACK_ATTEMPTS {
        if backend.get_mute(audio_device_id)? == Some(state) {
            return Ok(true);
        }
        if attempt < NATIVE_READBACK_ATTEMPTS {
            std::thread::sleep(backend.readback_retry_delay());
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
    if !wait_for_native_mute(backend, audio_device_id, false)? {
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

/// Restores the devices `restore.json` lists with a fresh CoreAudio backend. Meant for the
/// panic hook: it never touches a controller, whose lock the panicking thread may hold.
pub fn restore_from_journal_file() {
    let mut journal = RestoreJournal::load(state_file(JOURNAL_FILE_NAME));
    if journal.is_empty() {
        return;
    }
    info!(
        "Restoring {} device(s) listed in {} before exiting",
        journal.len(),
        JOURNAL_FILE_NAME
    );
    let failures = restore_journaled_devices(&mut CoreAudioBackend, &mut journal);
    if !failures.is_empty() {
        error!(
            "Failed to restore {} input device(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    journal.save_logged();
}

/// Logs an operation that can fail on every poll without flooding the log: the first failure
/// at `error!`, identical repeats at `trace!` for [`FAILURE_REPORT_INTERVAL`], then one
/// `error!` with their count, and `info!` once the operation succeeds again.
pub struct FailureLog {
    /// Starts every failure line, e.g. "Failed to update microphone mute state".
    failure_text: &'static str,
    /// The line for the first success after failures.
    recovery_text: &'static str,
    streak: Option<FailureStreak>,
}

/// The failures since the last success.
struct FailureStreak {
    message: String,
    /// When `message` was last logged at `error!`.
    reported_at: Instant,
    /// Identical failures since then, logged at `trace!`.
    unreported: u32,
    /// Every failure since the last success.
    failures: u32,
}

/// What [`FailureLog`] logs for one failure.
#[derive(Debug, PartialEq, Eq)]
enum FailureReport {
    /// `error!`: a failure unlike the one before.
    New,
    /// `error!`: an identical failure after the quiet period, with the identical failures
    /// since the last report, this one included.
    Repeated { count: u32, since: Duration },
    /// `trace!`: an identical failure inside the quiet period.
    Suppressed,
}

impl FailureLog {
    pub fn new(failure_text: &'static str, recovery_text: &'static str) -> Self {
        Self {
            failure_text,
            recovery_text,
            streak: None,
        }
    }

    /// Logs a failure that happened at `now`.
    pub fn report(&mut self, err: &anyhow::Error, now: Instant) {
        let message = format!("{:#}", err);
        match self.record_failure(&message, now) {
            FailureReport::New => error!("{}: {}", self.failure_text, message),
            FailureReport::Repeated { count, since } => error!(
                "{}: {} (repeated {} times in the last {} s)",
                self.failure_text,
                message,
                count,
                since.as_secs()
            ),
            FailureReport::Suppressed => trace!("{} again: {}", self.failure_text, message),
        }
    }

    /// Notes a success, and logs the recovery when it ends a run of failures.
    pub fn clear(&mut self) {
        if let Some(failures) = self.record_success() {
            info!(
                "{} after {} failed attempt(s)",
                self.recovery_text, failures
            );
        }
    }

    fn record_failure(&mut self, message: &str, now: Instant) -> FailureReport {
        let failures = self
            .streak
            .as_ref()
            .map_or(0, |streak| streak.failures)
            .saturating_add(1);
        if let Some(streak) = self
            .streak
            .as_mut()
            .filter(|streak| streak.message == message)
        {
            streak.failures = failures;
            let since = now.saturating_duration_since(streak.reported_at);
            if since < FAILURE_REPORT_INTERVAL {
                streak.unreported = streak.unreported.saturating_add(1);
                return FailureReport::Suppressed;
            }
            let count = streak.unreported.saturating_add(1);
            streak.reported_at = now;
            streak.unreported = 0;
            return FailureReport::Repeated { count, since };
        }
        self.streak = Some(FailureStreak {
            message: message.to_string(),
            reported_at: now,
            unreported: 0,
            failures,
        });
        FailureReport::New
    }

    /// Ends a run of failures; returns how many there were.
    fn record_success(&mut self) -> Option<u32> {
        self.streak.take().map(|streak| streak.failures)
    }
}

/// The CoreAudio listeners a controller registered, which wake the event loop on device changes
/// (`device_changes.rs`).
struct DeviceListeners {
    signal: &'static DeviceChangeSignal,
    is_listening_to_device_list: bool,
    /// The input devices with registered listeners, and the selectors each listens to.
    devices: HashMap<AudioDeviceID, Vec<AudioObjectPropertySelector>>,
    /// Whether the last registration left an input device without listeners.
    has_unregistered_device: bool,
    /// Every poll retries a failed registration; this keeps the log quiet meanwhile.
    failures: FailureLog,
}

impl DeviceListeners {
    fn new(signal: &'static DeviceChangeSignal) -> Self {
        Self {
            signal,
            is_listening_to_device_list: false,
            devices: HashMap::new(),
            has_unregistered_device: false,
            failures: FailureLog::new(
                "Failed to listen for audio device changes",
                "Listening for audio device changes works again",
            ),
        }
    }

    /// Whether every listener is registered, so each change a poll looks for wakes the loop.
    fn is_complete(&self) -> bool {
        self.is_listening_to_device_list && !self.has_unregistered_device
    }
}

pub struct MicController<B = CoreAudioBackend> {
    pub muted: bool,
    desired_muted: bool,
    /// Rate-limits the log of failed updates; the event loop reports each `toggle` outcome.
    pub update_failures: FailureLog,
    /// What the app changed per device and must undo, keyed by device UID and mirrored to
    /// `restore.json`.
    journal: RestoreJournal,
    /// The last audible level of each volume-controlled device, mirrored to
    /// `input-levels.json`.
    levels: InputLevels,
    /// Journaled devices that were connected at the last unmuted poll. A journaled device that
    /// joins this set gets restored once; later failures wait for the next unmute or exit.
    connected_journaled: HashSet<DeviceKey>,
    /// What writes showed about devices whose controls don't do what they advertise, keyed
    /// like the journal. Kept for the session, also while a device is disconnected.
    write_behavior: HashMap<DeviceKey, WriteBehavior>,
    /// UIDs of the devices the user excluded (`excluded_devices` in settings.json): the app
    /// never mutes, unmutes, or enforces them, and they don't count toward `muted`.
    excluded: BTreeSet<String>,
    /// Connected excluded devices whose journaled changes the app has tried to undo since they
    /// connected or became excluded; a muted poll tries each only once.
    released_excluded: HashSet<DeviceKey>,
    /// Devices whose write failed in the last `mute_all`, shown as failed.
    failed_devices: HashSet<DeviceKey>,
    /// What the tray's Microphones menu shows; see [`Self::device_states`].
    snapshot: DeviceSnapshot,
    /// The CoreAudio listeners that wake the event loop on device changes; `None` until
    /// [`Self::start_listening`], and again once exit removes them.
    listeners: Option<DeviceListeners>,
    backend: B,
}

impl<B: AudioBackend> Debug for MicController<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("MicController")
            .field("names", &self.names())
            .field("muted", &self.muted)
            .field("desired_muted", &self.desired_muted)
            .finish()
    }
}

impl MicController<CoreAudioBackend> {
    /// A controller for the real devices, with its records in the settings folder. It leaves
    /// alone the devices whose UIDs `excluded` lists.
    pub fn new(excluded: BTreeSet<String>) -> Self {
        Self::with_backend(
            CoreAudioBackend,
            RestoreJournal::load(state_file(JOURNAL_FILE_NAME)),
            InputLevels::load(state_file(LEVELS_FILE_NAME)),
            excluded,
        )
    }
}

impl<B: AudioBackend> MicController<B> {
    /// Never fails, so the app starts with a working tray and shortcut: a device it can't read
    /// counts as unmuted, and a device list it can't read leaves it without devices. Every poll
    /// lists and reads them again.
    fn with_backend(
        backend: B,
        journal: RestoreJournal,
        levels: InputLevels,
        excluded: BTreeSet<String>,
    ) -> Self {
        let mut controller = Self {
            muted: false,
            desired_muted: false,
            update_failures: FailureLog::new(
                "Failed to update microphone mute state",
                "Microphone mute state updates work again",
            ),
            journal,
            levels,
            connected_journaled: HashSet::new(),
            write_behavior: HashMap::new(),
            excluded,
            released_excluded: HashSet::new(),
            failed_devices: HashSet::new(),
            snapshot: DeviceSnapshot::default(),
            listeners: None,
            backend,
        };
        trace!("Creating audio controller");
        let names = controller.names();
        trace!("Found {} devices: {}", names.len(), names.join(", "));
        // Undo what an unclean exit left behind before the hardware state becomes user intent.
        controller.restore_after_unclean_exit();
        let ids = match controller.get_input_device_ids() {
            Ok(ids) => ids,
            Err(err) => {
                // Every poll lists them again, and mutes or watches them once it can.
                error!("Failed to list the input devices at launch: {}", err);
                Vec::new()
            }
        };
        controller.record_audible_levels(&ids);
        controller.muted = controller.is_muted_all();
        controller.desired_muted = controller.muted;
        controller.refresh_device_states(&ids, Instant::now());
        controller
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

    /// "id: name" for every device, for the log; a name that can't be read gets a stand-in.
    fn names(&self) -> Vec<String> {
        let ids = match self.backend.device_ids() {
            Ok(ids) => ids,
            Err(err) => {
                trace!("Could not list audio devices: {}", err);
                return Vec::new();
            }
        };
        ids.into_iter()
            .map(|id| {
                let name = self
                    .backend
                    .device_name(id)
                    .unwrap_or_else(|_| unnamed_device(id));
                format!("{}: {}", id, name)
            })
            .collect()
    }

    fn get_input_device_ids(&self) -> Result<Vec<AudioDeviceID>> {
        Ok(input_devices(&self.backend)?.ids)
    }

    /// What writes showed about the device. Reads its UID only when anything is known.
    fn write_behavior_of(&self, audio_device_id: AudioDeviceID) -> Option<WriteBehavior> {
        if self.write_behavior.is_empty() {
            return None;
        }
        self.write_behavior
            .get(&device_key(&self.backend, audio_device_id))
            .copied()
    }

    fn is_excluded(&self, key: &DeviceKey) -> bool {
        is_excluded_by(&self.excluded, key)
    }

    /// Reads the device's state from the control the app mutes it with; `key` is its record
    /// key. An excluded device isn't read at all.
    fn read_device(&self, audio_device_id: AudioDeviceID, key: &DeviceKey) -> Result<Reading> {
        if self.is_excluded(key) {
            return Ok(Reading::Excluded);
        }
        let Some(control) = mute_control(&self.backend, audio_device_id)? else {
            return Ok(Reading::NoControl);
        };
        let control = match self.write_behavior.get(key) {
            None => control,
            Some(WriteBehavior::IgnoresNativeMute) => MuteControl::Volume,
            Some(WriteBehavior::IgnoresWrites(_)) => return Ok(Reading::IgnoresWrites),
        };
        let state = read_muted(&self.backend, audio_device_id, control)?;
        Ok(Reading::State(control, state))
    }

    /// Whether at least one device the app controls reads muted, and every such device does. A
    /// device or a device list it can't read makes it false: the app never shows muted when it
    /// can't tell.
    fn is_muted_all(&self) -> bool {
        let ids = match self.get_input_device_ids() {
            Ok(ids) => ids,
            Err(err) => {
                trace!("Could not list input devices to read them: {}", err);
                return false;
            }
        };
        let mut readings = Vec::new();
        for id in ids {
            match self.read_device(id, &device_key(&self.backend, id)) {
                Ok(reading) => {
                    trace!("Input device {} reads {:?}", id, reading);
                    readings.push(reading);
                }
                Err(err) => {
                    trace!("Could not read audio device {}: {}", id, err);
                    return false;
                }
            }
        }
        are_controlled_devices_muted(readings)
    }

    /// Mutes or unmutes one device the user hasn't excluded. Returns `None` when the device is
    /// left out: nothing is settable, or its accepted writes never take effect.
    fn mute(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        state: bool,
        now: Instant,
    ) -> Result<Option<AudioDeviceID>> {
        let Some(control) = mute_control(&self.backend, audio_device_id)? else {
            trace!(
                "Skipping audio device {} because neither native mute nor input volume is controllable",
                audio_device_id
            );
            return Ok(None);
        };
        let is_handled = if state {
            self.mute_device(audio_device_id, name, key, control, now)?
        } else {
            self.unmute_device(audio_device_id, name, key, control)?
        };
        Ok(is_handled.then_some(audio_device_id))
    }

    /// Mutes a device and learns from the readback. When native mute ignores an accepted write,
    /// input volume takes over for the session; when every control ignores its writes, the
    /// device is left out until its retry is due. Returns false when it is left out.
    fn mute_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        control: MuteControl,
        now: Instant,
    ) -> Result<bool> {
        let known = self.write_behavior.get(key).copied();
        if let Some(WriteBehavior::IgnoresWrites(retry)) = known {
            if !retry.is_due(audio_device_id, now) {
                trace!(
                    "Leaving out audio device {}: {} until its next write retry",
                    audio_device_id,
                    name
                );
                return Ok(false);
            }
        }
        let skips_native = known == Some(WriteBehavior::IgnoresNativeMute);
        let mut ignored = Vec::new();
        if control == MuteControl::Native && !skips_native {
            match self.mute_native(audio_device_id, name, key, true)? {
                WriteOutcome::Confirmed => {
                    self.learn(audio_device_id, name, key, None, &ignored);
                    return Ok(true);
                }
                WriteOutcome::NotSettable => trace!(
                    "Device {} doesn't support mute property; falling back to input volume",
                    audio_device_id
                ),
                WriteOutcome::Ignored => ignored.push("native mute"),
            }
        }
        match self.mute_via_volume(audio_device_id, name, key)? {
            WriteOutcome::Confirmed => {
                let behavior = (skips_native || !ignored.is_empty())
                    .then_some(WriteBehavior::IgnoresNativeMute);
                self.learn(audio_device_id, name, key, behavior, &ignored);
                return Ok(true);
            }
            WriteOutcome::Ignored => ignored.push("input volume"),
            WriteOutcome::NotSettable if skips_native => {
                // Its input volume is gone: try native mute again on the next poll.
                self.write_behavior.remove(key);
            }
            WriteOutcome::NotSettable => {}
        }
        if ignored.is_empty() {
            trace!(
                "Skipping audio device {} because neither native mute nor input volume is controllable",
                audio_device_id
            );
            return Ok(false);
        }
        let behavior = WriteBehavior::IgnoresWrites(WriteRetry::new(audio_device_id, now));
        self.learn(audio_device_id, name, key, Some(behavior), &ignored);
        Ok(false)
    }

    /// Remembers what a mute showed about the device and logs what changed. `ignored` names
    /// the controls whose accepted writes didn't take effect.
    fn learn(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        behavior: Option<WriteBehavior>,
        ignored: &[&str],
    ) {
        let previous = match behavior {
            Some(behavior) => self.write_behavior.insert(key.clone(), behavior),
            None => self.write_behavior.remove(key),
        };
        let was_ignoring_writes = matches!(previous, Some(WriteBehavior::IgnoresWrites(_)));
        let retry_secs = IGNORED_WRITES_RETRY_INTERVAL.as_secs();
        match behavior {
            Some(WriteBehavior::IgnoresWrites(_)) if was_ignoring_writes => trace!(
                "Audio device {}: {} still ignores {} writes; trying again in {} s",
                audio_device_id,
                name,
                ignored.join(" and "),
                retry_secs
            ),
            Some(WriteBehavior::IgnoresWrites(_)) => warn!(
                "Leaving out audio device {}: {}: it accepts {} writes, but they don't take effect; trying again every {} s and when it reconnects",
                audio_device_id,
                name,
                ignored.join(" and "),
                retry_secs
            ),
            _ if was_ignoring_writes => info!(
                "Audio device {}: {} responds to mute writes again",
                audio_device_id, name
            ),
            Some(WriteBehavior::IgnoresNativeMute) if previous.is_none() => info!(
                "Audio device {}: {} ignores native mute writes; muting it by input volume",
                audio_device_id, name
            ),
            _ => {}
        }
    }

    /// Unmutes a device on every control the app may have muted it with: native mute when it
    /// is settable, and input volume when that is the device's control or the journal holds a
    /// saved level. A failure on one control doesn't stop the other.
    fn unmute_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        control: MuteControl,
    ) -> Result<bool> {
        let mut uses_volume = control == MuteControl::Volume
            || self.write_behavior.get(key) == Some(&WriteBehavior::IgnoresNativeMute)
            || self.journal.saved_volume(key).is_some();
        let mut is_handled = false;
        let mut failures = Vec::new();
        if control == MuteControl::Native {
            match self.mute_native(audio_device_id, name, key, false) {
                Ok(WriteOutcome::Confirmed) => is_handled = true,
                Ok(WriteOutcome::NotSettable) => {
                    trace!(
                        "Device {} doesn't support mute property; falling back to input volume",
                        audio_device_id
                    );
                    uses_volume = true;
                }
                Ok(WriteOutcome::Ignored) => failures.push(anyhow!(
                    "audio device {} remained muted after native unmute",
                    audio_device_id
                )),
                Err(err) => failures.push(err),
            }
        }
        if uses_volume {
            match self.unmute_via_volume(audio_device_id, name, key) {
                Ok(is_settable) => is_handled |= is_settable,
                Err(err) => failures.push(err),
            }
        }
        match failures
            .into_iter()
            .reduce(|first, next| anyhow!("{}; {}", first, next))
        {
            Some(err) => Err(err),
            None => Ok(is_handled),
        }
    }

    /// Sets native mute unless it already reads `state`, and confirms a write by readback. A
    /// mute is journaled before the write and stays journaled when the device ignores it, so
    /// unmute and exit undo it should it take effect late.
    fn mute_native(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
        state: bool,
    ) -> Result<WriteOutcome> {
        let current = self.backend.get_mute(audio_device_id)?;
        if current == Some(state) {
            // Nothing to write; an unmuted device has no mute of the app's left to undo.
            if !state {
                self.journal.clear_native_mute(key);
            }
            return Ok(WriteOutcome::Confirmed);
        }
        let is_recorded =
            state && current == Some(false) && self.journal.record_native_mute(key, name);
        if is_recorded {
            // Journal the change before making it, so a crash can't leave it unrecorded.
            self.journal.save_logged();
        }
        if self.backend.set_mute(audio_device_id, state)?.is_none() {
            if is_recorded {
                self.journal.clear_native_mute(key);
            }
            return Ok(WriteOutcome::NotSettable);
        }
        if !wait_for_native_mute(&self.backend, audio_device_id, state)? {
            return Ok(WriteOutcome::Ignored);
        }
        if !state {
            self.journal.clear_native_mute(key);
        }
        Ok(WriteOutcome::Confirmed)
    }

    /// Mutes by setting input volume to 0 unless it already reads 0, and confirms the write by
    /// readback. The level is saved first and stays journaled when the device ignores the
    /// write, so unmute and exit restore it should the write take effect late.
    fn mute_via_volume(
        &mut self,
        audio_device_id: AudioDeviceID,
        name: &str,
        key: &DeviceKey,
    ) -> Result<WriteOutcome> {
        let Some(current_vol) = self.backend.get_volume(audio_device_id)? else {
            return Ok(WriteOutcome::NotSettable);
        };
        if is_volume_muted(current_vol) {
            // Muted already, by the app or before it ran: nothing to save or write.
            return Ok(WriteOutcome::Confirmed);
        }
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
        if self.backend.set_volume(audio_device_id, 0.0)?.is_none() {
            return Ok(WriteOutcome::NotSettable);
        }
        match self.backend.get_volume(audio_device_id)? {
            Some(volume) if is_volume_muted(volume) => Ok(WriteOutcome::Confirmed),
            Some(volume) => {
                trace!(
                    "Audio device {} input volume remained {:.3} after fallback mute",
                    audio_device_id,
                    volume
                );
                Ok(WriteOutcome::Ignored)
            }
            None => Ok(WriteOutcome::NotSettable),
        }
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
        self.mute_all_at(state, Instant::now())
    }

    /// [`Self::mute_all`] at `now`, which decides whether devices that ignore writes are due
    /// another write.
    fn mute_all_at(&mut self, state: bool, now: Instant) -> Result<&Self> {
        self.desired_muted = state;
        if let Some(listeners) = &self.listeners {
            listeners.signal.set_enforcing_mute(state);
        }
        let InputDevices { ids, mut unchecked } = input_devices(&self.backend)?;
        self.note_input_devices(&ids);
        // Before any write, so a change from here on wakes the loop.
        self.sync_listeners(&ids, now);
        let action = action_name(state);
        let end_state = state_name(state);
        let mut failures = Vec::new();
        let mut polled = Vec::with_capacity(ids.len());
        self.failed_devices.clear();
        for id in &ids {
            let (name, name_failure) = match self.backend.device_name(*id) {
                Ok(name) => (name, None),
                Err(err) => (
                    unnamed_device(*id),
                    Some(anyhow!(
                        "name of audio device {} is unreadable: {}",
                        id,
                        err
                    )),
                ),
            };
            let key = device_key(&self.backend, *id);
            if self.is_excluded(&key) {
                trace!(
                    "Leaving out audio device {}: {} because it is excluded",
                    id,
                    name
                );
                self.release_excluded_device(*id, &key, state);
                polled.push((*id, name, key));
                continue;
            }
            // A device whose input streams or name can't be read may still be a live
            // microphone: the app changes it like the others, and the pass reports it as failed.
            let mut device_failures: Vec<anyhow::Error> = unchecked
                .remove(id)
                .into_iter()
                .chain(name_failure)
                .collect();
            trace!("Setting mute={} for {}", state, name);
            match self.mute(*id, &name, &key, state, now) {
                Ok(Some(_)) => trace!("Successfully {} audio device {}: {}", end_state, id, name),
                Ok(None) => trace!("Skipped audio device {}: {}", id, name),
                Err(err) => device_failures.push(err),
            }
            if let Some(err) = combined_error(device_failures) {
                // The combined error below carries this failure, and the event loop logs it
                // through `update_failures`; per poll, this line is detail.
                trace!(
                    "Failed to {} audio device {}: {}: {}",
                    action,
                    id,
                    name,
                    err
                );
                failures.push(format!("{} ({})", name, err));
                self.failed_devices.insert(key.clone());
            }
            polled.push((*id, name, key));
        }
        // An excluded device that disconnects gets another restore attempt when it is back.
        self.released_excluded
            .retain(|key| polled.iter().any(|(_, _, polled_key)| polled_key == key));
        // Drop the entries of devices that unmuting restored; a no-op when nothing changed.
        self.journal.save_logged();

        // One reading per device decides `muted`, whether every device reached `state`, and
        // what the Microphones menu shows.
        let readings: Vec<Result<Reading>> = polled
            .iter()
            .map(|(id, _, key)| self.read_device(*id, key))
            .collect();
        self.muted = readings.iter().all(Result::is_ok)
            && are_controlled_devices_muted(
                readings.iter().filter_map(|r| r.as_ref().ok().copied()),
            );
        let states = polled
            .into_iter()
            .zip(&readings)
            .map(|((_, name, key), reading)| {
                self.device_state(name, &key, reading.as_ref().ok().copied())
            })
            .collect();
        self.snapshot = DeviceSnapshot {
            states,
            ids,
            read_at: Some(now),
            is_stale: false,
        };
        if !failures.is_empty() {
            return Err(anyhow!(
                "failed to {} {} input device(s): {}",
                action,
                failures.len(),
                failures.join("; ")
            ));
        }
        let readings = readings.into_iter().collect::<Result<Vec<_>>>()?;
        if readings
            .iter()
            .any(|reading| reading.muted().is_some_and(|muted| muted != state))
        {
            return Err(anyhow!(
                "one or more input devices did not reach requested mute state {}",
                state
            ));
        }
        Ok(self)
    }

    /// Undoes what the journal lists for an excluded device: once while it stays connected,
    /// and again on each user unmute (`state` false). A failed restore stays journaled, so exit
    /// tries again.
    fn release_excluded_device(
        &mut self,
        audio_device_id: AudioDeviceID,
        key: &DeviceKey,
        state: bool,
    ) {
        let is_first_attempt = self.released_excluded.insert(key.clone());
        if self.journal.get(key).is_none() || (state && !is_first_attempt) {
            return;
        }
        let device = [(audio_device_id, key.clone())];
        let failures = restore_devices(&mut self.backend, &mut self.journal, &device);
        if !failures.is_empty() {
            error!(
                "Failed to restore excluded input device {}",
                failures.join("; ")
            );
        }
    }

    /// Leaves the devices with these UIDs alone from now on, and undoes, by the journal, what
    /// the app changed on the ones it didn't exclude before. A device included again is muted by
    /// the next poll while the user wants mute. Returns whether the exclusions changed.
    pub fn set_excluded(&mut self, excluded: BTreeSet<String>) -> bool {
        if excluded == self.excluded {
            return false;
        }
        let newly_excluded: BTreeSet<String> =
            excluded.difference(&self.excluded).cloned().collect();
        if excluded.is_empty() {
            info!("No input device is excluded from muting any more");
        } else {
            let uids: Vec<&str> = excluded.iter().map(String::as_str).collect();
            info!(
                "Excluding {} input device(s) from muting: {}",
                uids.len(),
                uids.join(", ")
            );
        }
        self.excluded = excluded;
        let excluded = &self.excluded;
        self.released_excluded
            .retain(|key| is_excluded_by(excluded, key));
        self.release_newly_excluded(&newly_excluded);
        self.muted = self.is_muted_all();
        self.snapshot.is_stale = true;
        true
    }

    /// Each connected input device and its state for the tray's Microphones menu, as the last
    /// poll read them. While unmuted, they're read again every
    /// [`DEVICE_STATES_REFRESH_INTERVAL`], and when a device comes or goes.
    pub fn device_states(&self) -> &[DeviceState] {
        &self.snapshot.states
    }

    fn device_state(&self, name: String, key: &DeviceKey, reading: Option<Reading>) -> DeviceState {
        let uid = match key {
            DeviceKey::Uid(uid) => Some(uid.clone()),
            DeviceKey::Session(_) => None,
        };
        DeviceState {
            uid,
            name,
            status: device_status(reading, self.failed_devices.contains(key)),
        }
    }

    /// Reads the device states again when they may be out of date: a device came or went, the
    /// exclusions changed, or [`DEVICE_STATES_REFRESH_INTERVAL`] has passed.
    fn refresh_device_states_if_due(&mut self, ids: &[AudioDeviceID], now: Instant) {
        let snapshot = &self.snapshot;
        let is_due = snapshot.is_stale
            || snapshot.ids != ids
            || snapshot.read_at.is_none_or(|read_at| {
                now.saturating_duration_since(read_at) >= DEVICE_STATES_REFRESH_INTERVAL
            });
        if is_due {
            self.refresh_device_states(ids, now);
        }
    }

    /// Reads the name, UID, and state of every device in `ids` for the Microphones menu. The
    /// failures it shows are the last `mute_all`'s.
    fn refresh_device_states(&mut self, ids: &[AudioDeviceID], now: Instant) {
        let mut states = Vec::with_capacity(ids.len());
        for id in ids {
            let name = self.backend.device_name(*id).unwrap_or_else(|err| {
                trace!("Could not read the name of audio device {}: {}", id, err);
                unnamed_device(*id)
            });
            let key = device_key(&self.backend, *id);
            let reading = self.read_device(*id, &key);
            if let Err(err) = &reading {
                trace!("Could not read audio device {}: {}: {}", id, name, err);
            }
            states.push(self.device_state(name, &key, reading.ok()));
        }
        self.snapshot = DeviceSnapshot {
            states,
            ids: ids.to_vec(),
            read_at: Some(now),
            is_stale: false,
        };
    }

    /// Restores the connected devices among `uids` that the journal lists.
    fn release_newly_excluded(&mut self, uids: &BTreeSet<String>) {
        if uids.is_empty() || self.journal.is_empty() {
            return;
        }
        let devices: Vec<_> = match journaled_devices(&self.backend, &self.journal) {
            Ok(devices) => devices
                .into_iter()
                .filter(|(_, key)| is_excluded_by(uids, key))
                .collect(),
            Err(err) => {
                // The next muted poll, or exit, restores them.
                warn!(
                    "Could not list the excluded input devices to restore: {}",
                    err
                );
                return;
            }
        };
        for (_, key) in &devices {
            self.released_excluded.insert(key.clone());
        }
        let failures = restore_devices(&mut self.backend, &mut self.journal, &devices);
        if !failures.is_empty() {
            error!(
                "Failed to restore {} excluded input device(s): {}",
                failures.len(),
                failures.join("; ")
            );
        }
        self.journal.save_logged();
    }

    /// Removes every device listener, then undoes every journaled change on connected devices and
    /// saves pending records. Restoring twice is a no-op, so every exit path can call it.
    pub fn restore_on_exit(&mut self) -> Result<()> {
        // First, so the restore's own writes don't wake the event loop.
        self.stop_listening();
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

        self.muted = self.is_muted_all();
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

    /// Has CoreAudio call `signal` when a device comes or goes, and when an input device's
    /// native mute or input volume changes while the app enforces mute. From then on, every poll
    /// adds and removes device listeners as devices come and go, until exit removes them all.
    pub fn start_listening(&mut self, signal: &'static DeviceChangeSignal) {
        signal.set_enforcing_mute(self.desired_muted);
        self.listeners = Some(DeviceListeners::new(signal));
        let now = Instant::now();
        match self.get_input_device_ids() {
            Ok(ids) => self.sync_listeners(&ids, now),
            Err(err) => {
                self.sync_listeners(&[], now);
                if let Some(listeners) = &mut self.listeners {
                    // The next poll lists the input devices again and listens to them.
                    listeners.has_unregistered_device = true;
                    listeners
                        .failures
                        .report(&err.context("list input devices to listen to"), now);
                }
            }
        }
        if self.is_listening() {
            info!("Listening for audio device changes");
        }
    }

    /// Whether CoreAudio listeners report every change a poll looks for: they are all
    /// registered. The event loop then polls less often.
    pub fn is_listening(&self) -> bool {
        self.listeners
            .as_ref()
            .is_some_and(DeviceListeners::is_complete)
    }

    /// Registers the listeners that are missing: on the device list, and on each input device in
    /// `ids`, which retries those that failed before. Removes the listeners of devices no longer
    /// in `ids`. Without such changes, it makes no CoreAudio call.
    fn sync_listeners(&mut self, ids: &[AudioDeviceID], now: Instant) {
        let Some(listeners) = &mut self.listeners else {
            return;
        };
        let signal = listeners.signal;
        let gone: Vec<AudioDeviceID> = listeners
            .devices
            .keys()
            .filter(|id| !ids.contains(id))
            .copied()
            .collect();
        for id in gone {
            let Some(selectors) = listeners.devices.remove(&id) else {
                continue;
            };
            // A device that went away took its listeners along, so removing them can fail.
            if let Err(err) = self
                .backend
                .stop_listening_to_device(id, &selectors, signal)
            {
                trace!(
                    "Could not remove the listeners of audio device {}: {}",
                    id,
                    err
                );
            }
        }
        let mut failures = Vec::new();
        if !listeners.is_listening_to_device_list {
            match self.backend.listen_to_device_list(signal) {
                Ok(()) => listeners.is_listening_to_device_list = true,
                Err(err) => failures.push(err),
            }
        }
        let mut has_unregistered_device = false;
        for id in ids {
            if listeners.devices.contains_key(id) {
                continue;
            }
            match self.backend.listen_to_device(*id, signal) {
                Ok(selectors) => {
                    trace!(
                        "Listening to {} control(s) of audio device {}",
                        selectors.len(),
                        id
                    );
                    listeners.devices.insert(*id, selectors);
                }
                Err(err) => {
                    has_unregistered_device = true;
                    failures.push(err);
                }
            }
        }
        listeners.has_unregistered_device = has_unregistered_device;
        match combined_error(failures) {
            Some(err) => listeners.failures.report(&err, now),
            None => listeners.failures.clear(),
        }
    }

    /// Removes every listener for good. Exit calls it before restoring, so no listener outlives
    /// the app's last poll.
    fn stop_listening(&mut self) {
        let Some(listeners) = self.listeners.take() else {
            return;
        };
        let signal = listeners.signal;
        let mut failures = Vec::new();
        if listeners.is_listening_to_device_list {
            if let Err(err) = self.backend.stop_listening_to_device_list(signal) {
                failures.push(err);
            }
        }
        for (id, selectors) in &listeners.devices {
            if let Err(err) = self
                .backend
                .stop_listening_to_device(*id, selectors, signal)
            {
                failures.push(err);
            }
        }
        match combined_error(failures) {
            // Harmless: a device that left since the last poll took its listeners along.
            Some(err) => warn!("Failed to remove every audio device listener: {}", err),
            None => trace!("Removed every audio device listener"),
        }
    }

    /// Runs on every poll while the user's intent is unmuted.
    pub fn watch_unmuted(&mut self) {
        self.watch_unmuted_at(Instant::now());
    }

    /// [`Self::watch_unmuted`] at `now`, which decides whether the device states are due a read.
    fn watch_unmuted_at(&mut self, now: Instant) {
        self.restore_reconnected_devices();
        let ids = match self.get_input_device_ids() {
            Ok(ids) => ids,
            Err(err) => {
                trace!("Could not list input devices to record levels: {}", err);
                return;
            }
        };
        // Reconnects while unmuted count too: the next mute retries such a device at once.
        self.note_input_devices(&ids);
        self.sync_listeners(&ids, now);
        self.record_audible_levels(&ids);
        self.refresh_device_states_if_due(&ids, now);
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

    /// Notes which input devices a poll found, so a device that ignores writes gets its next
    /// write as soon as it reconnects.
    fn note_input_devices(&mut self, ids: &[AudioDeviceID]) {
        // A device without a UID comes back under a new session key; its old entry is dead.
        self.write_behavior
            .retain(|key, _| !matches!(key, DeviceKey::Session(id) if !ids.contains(id)));
        for behavior in self.write_behavior.values_mut() {
            if let WriteBehavior::IgnoresWrites(retry) = behavior {
                if !ids.contains(&retry.audio_device_id) {
                    retry.has_disconnected = true;
                }
            }
        }
    }

    /// Records the level of every device in `ids` that is volume-controlled and audible now.
    fn record_audible_levels(&mut self, ids: &[AudioDeviceID]) {
        for id in ids {
            if let Err(err) = self.record_audible_level(*id) {
                trace!("Could not record the input level of device {}: {}", id, err);
            }
        }
    }

    fn record_audible_level(&mut self, audio_device_id: AudioDeviceID) -> Result<()> {
        // Same test as `state_control`, reading the volume only once. Devices muted natively
        // keep their level, so they need no record.
        if self.backend.is_mute_settable(audio_device_id)?
            && self.write_behavior_of(audio_device_id) != Some(WriteBehavior::IgnoresNativeMute)
        {
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

    /// Mutes or unmutes every device: `Some(state)` sets that state, and `None` toggles the one
    /// the tray and popup show, `muted`. A toggle never flips `desired_muted`: after a mute that
    /// left a device live, the UI still shows unmuted, so the next press mutes again and retries
    /// that device rather than unmuting the ones that did mute.
    pub fn toggle(&mut self, state: Option<bool>) -> Result<&Self> {
        let state = target_state(state, self.muted);
        self.mute_all(state)
    }

    pub fn should_enforce_mute(&self) -> bool {
        self.desired_muted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_changes::DeviceChange;
    use crate::store::MemoryStore;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct Device {
        name: String,
        uid: String,
        input: bool,
        mute: Option<bool>,
        /// Whether `mute` can be set; a readable native mute may still be read-only.
        mute_settable: bool,
        volume: Option<f32>,
        /// Reading its name fails.
        fail_name: bool,
        fail_uid: bool,
        fail_input_channels: bool,
        /// Reading the native mute fails, as for a device that stops responding.
        fail_get_mute: bool,
        fail_set_mute: bool,
        fail_set_volume: bool,
        ignore_set_mute: bool,
        ignore_set_volume: bool,
        /// Registering its listeners fails.
        fail_listen: bool,
    }

    impl Device {
        fn new(name: &str, mute: Option<bool>, volume: Option<f32>) -> Self {
            Self {
                name: name.to_string(),
                uid: uid_of(name),
                input: true,
                mute,
                mute_settable: mute.is_some(),
                volume,
                fail_name: false,
                fail_uid: false,
                fail_input_channels: false,
                fail_get_mute: false,
                fail_set_mute: false,
                fail_set_volume: false,
                ignore_set_mute: false,
                ignore_set_volume: false,
                fail_listen: false,
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

        /// Reports a native mute that can be read but not set.
        fn read_only_mute(name: &str, muted: bool, volume: Option<f32>) -> Self {
            Self {
                mute_settable: false,
                ..Self::new(name, Some(muted), volume)
            }
        }

        /// Accepts native mute and input volume writes without an error, but neither changes,
        /// like Microsoft Teams Audio.
        fn ignoring_writes(name: &str) -> Self {
            Self {
                ignore_set_mute: true,
                ignore_set_volume: true,
                ..Self::native(name, false)
            }
        }
    }

    /// A write that reached a settable control.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Write {
        Mute(AudioDeviceID, bool),
        Volume(AudioDeviceID, f32),
    }

    impl Write {
        fn device(self) -> AudioDeviceID {
            match self {
                Write::Mute(id, _) | Write::Volume(id, _) => id,
            }
        }
    }

    fn uid_of(name: &str) -> String {
        format!("{} UID", name)
    }

    fn key(name: &str) -> DeviceKey {
        DeviceKey::Uid(uid_of(name))
    }

    /// The UIDs of the devices named `names`, as `Settings::excluded_uids` gives them.
    fn uids(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| uid_of(name)).collect()
    }

    #[derive(Default)]
    struct FakeBackend {
        devices: HashMap<AudioDeviceID, Device>,
        ids: Vec<AudioDeviceID>,
        /// Makes `device_ids` fail, as when the device list can't be read.
        fail_device_ids: bool,
        /// When set, each device write records whether this journal store named the device.
        journal_probe: Option<MemoryStore>,
        journaled_at_write: Vec<(AudioDeviceID, bool)>,
        /// Every write that reached a settable control, in order.
        writes: Vec<Write>,
        /// The registered device list listener's signal, which `plug` and `unplug` notify.
        device_list_listener: Option<&'static DeviceChangeSignal>,
        /// Per device, the selectors of its registered listeners and their signal, which a
        /// change of a listened control notifies, as CoreAudio does for any change.
        device_listeners: HashMap<
            AudioDeviceID,
            (
                Vec<AudioObjectPropertySelector>,
                &'static DeviceChangeSignal,
            ),
        >,
        /// Makes `listen_to_device_list` fail.
        fail_device_list_listener: bool,
        /// Listener registrations and removals that reached the backend.
        listener_calls: usize,
    }

    impl FakeBackend {
        fn with_devices(devices: Vec<(AudioDeviceID, Device)>) -> Self {
            let ids = devices.iter().map(|(id, _)| *id).collect();
            let devices = devices.into_iter().collect();
            Self {
                devices,
                ids,
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
            let device = self.devices.remove(&id).unwrap();
            self.report_device_list_change();
            device
        }

        /// Connects a device, e.g. one unplugged earlier, under a new ID.
        fn plug(&mut self, id: AudioDeviceID, device: Device) {
            self.ids.push(id);
            self.devices.insert(id, device);
            self.report_device_list_change();
        }

        fn report_device_list_change(&self) {
            if let Some(signal) = self.device_list_listener {
                signal.notify(DeviceChange::DeviceList);
            }
        }

        /// Reports a change of the device's control `selector` to a listener on it.
        fn report_control_change(&self, id: AudioDeviceID, selector: AudioObjectPropertySelector) {
            if let Some((selectors, signal)) = self.device_listeners.get(&id) {
                if selectors.contains(&selector) {
                    signal.notify(DeviceChange::Controls);
                }
            }
        }

        /// Another app, or the user in Sound settings, sets the device's native mute.
        fn change_mute_externally(&mut self, id: AudioDeviceID, state: bool) {
            self.device_mut(id).unwrap().mute = Some(state);
            self.report_control_change(id, kAudioDevicePropertyMute);
        }

        /// Another app, or the user in Sound settings, sets the device's input volume.
        fn change_volume_externally(&mut self, id: AudioDeviceID, volume: f32) {
            self.device_mut(id).unwrap().volume = Some(volume);
            self.report_control_change(id, kAudioDevicePropertyVolumeScalar);
        }

        fn observe_write(&mut self, write: Write) {
            self.writes.push(write);
            let id = write.device();
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
            if self.fail_device_ids {
                return Err(anyhow!("fake device list failure"));
            }
            Ok(self.ids.clone())
        }

        fn device_name(&self, audio_device_id: AudioDeviceID) -> Result<String> {
            let device = self.device(audio_device_id)?;
            if device.fail_name {
                return Err(anyhow!("fake name failure"));
            }
            Ok(device.name.clone())
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
            let device = self.device(audio_device_id)?;
            Ok(device.mute.is_some() && device.mute_settable)
        }

        fn get_mute(&self, audio_device_id: AudioDeviceID) -> Result<Option<bool>> {
            let device = self.device(audio_device_id)?;
            if device.fail_get_mute {
                return Err(anyhow!("fake mute read failure"));
            }
            Ok(device.mute)
        }

        fn set_mute(&mut self, audio_device_id: AudioDeviceID, state: bool) -> Result<Option<()>> {
            // Like CoreAudioBackend, which checks AudioObjectIsPropertySettable first.
            if !self.is_mute_settable(audio_device_id)? {
                return Ok(None);
            }
            self.observe_write(Write::Mute(audio_device_id, state));
            let device = self.device_mut(audio_device_id)?;
            if device.fail_set_mute {
                return Err(anyhow!("fake native mute failure"));
            }
            let Some(muted) = &mut device.mute else {
                return Ok(None);
            };
            if !device.ignore_set_mute && *muted != state {
                *muted = state;
                self.report_control_change(audio_device_id, kAudioDevicePropertyMute);
            }
            Ok(Some(()))
        }

        /// Fake writes show at once or never, so tests don't sleep between readbacks.
        fn readback_retry_delay(&self) -> Duration {
            Duration::ZERO
        }

        fn get_volume(&self, audio_device_id: AudioDeviceID) -> Result<Option<f32>> {
            Ok(self.device(audio_device_id)?.volume)
        }

        fn set_volume(
            &mut self,
            audio_device_id: AudioDeviceID,
            volume: f32,
        ) -> Result<Option<()>> {
            if self.device(audio_device_id)?.volume.is_none() {
                return Ok(None);
            }
            self.observe_write(Write::Volume(audio_device_id, volume));
            let device = self.device_mut(audio_device_id)?;
            if device.fail_set_volume {
                return Err(anyhow!("fake volume failure"));
            }
            if !device.ignore_set_volume && device.volume != Some(volume) {
                device.volume = Some(volume);
                self.report_control_change(audio_device_id, kAudioDevicePropertyVolumeScalar);
            }
            Ok(Some(()))
        }

        fn listen_to_device_list(&mut self, signal: &'static DeviceChangeSignal) -> Result<()> {
            self.listener_calls += 1;
            if self.fail_device_list_listener {
                return Err(anyhow!("fake device list listener failure"));
            }
            self.device_list_listener = Some(signal);
            Ok(())
        }

        fn stop_listening_to_device_list(
            &mut self,
            _signal: &'static DeviceChangeSignal,
        ) -> Result<()> {
            self.listener_calls += 1;
            self.device_list_listener = None;
            Ok(())
        }

        /// Listens to the native mute and the input volume, those the device has.
        fn listen_to_device(
            &mut self,
            audio_device_id: AudioDeviceID,
            signal: &'static DeviceChangeSignal,
        ) -> Result<Vec<AudioObjectPropertySelector>> {
            self.listener_calls += 1;
            let device = self.device(audio_device_id)?;
            if device.fail_listen {
                return Err(anyhow!("fake listener failure"));
            }
            let mut selectors = Vec::new();
            if device.mute.is_some() {
                selectors.push(kAudioDevicePropertyMute);
            }
            if device.volume.is_some() {
                selectors.push(kAudioDevicePropertyVolumeScalar);
            }
            self.device_listeners
                .insert(audio_device_id, (selectors.clone(), signal));
            Ok(selectors)
        }

        /// Like CoreAudio, fails for a device that went away, whose listeners went with it.
        fn stop_listening_to_device(
            &mut self,
            audio_device_id: AudioDeviceID,
            _selectors: &[AudioObjectPropertySelector],
            _signal: &'static DeviceChangeSignal,
        ) -> Result<()> {
            self.listener_calls += 1;
            self.device_listeners.remove(&audio_device_id);
            self.device(audio_device_id).map(|_| ())
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
            self.launch_excluding(backend, &[])
        }

        /// Launches with `excluded_devices` listing the devices named `excluded`.
        fn launch_excluding(
            &self,
            backend: FakeBackend,
            excluded: &[&str],
        ) -> MicController<FakeBackend> {
            MicController::with_backend(
                backend,
                RestoreJournal::load(self.journal.boxed()),
                InputLevels::load(self.levels.boxed()),
                uids(excluded),
            )
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
        MicController::with_backend(
            CoreAudioBackend,
            RestoreJournal::load(stores.journal.boxed()),
            InputLevels::load(stores.levels.boxed()),
            BTreeSet::new(),
        );

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
    fn a_toggle_flips_the_shown_state_and_a_request_sets_its_own() {
        assert!(target_state(None, false));
        assert!(!target_state(None, true));
        assert!(!target_state(Some(false), true));
        assert!(target_state(Some(true), true));
    }

    #[test]
    fn a_toggle_after_a_partial_mute_mutes_again_and_never_unmutes_the_muted_devices() {
        let mut broken = Device::native("USB Mic", false);
        broken.fail_set_mute = true;
        let mut controller = controller(FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, broken),
        ]));

        assert!(controller.toggle(None).is_err());
        // The USB mic stays live, so the tray and popup still show unmuted.
        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));

        // Pressing Mute again mutes again, retrying the USB mic.
        assert!(controller.toggle(None).is_err());
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert!(controller.should_enforce_mute());
        assert!(!controller.backend.writes.contains(&Write::Mute(1, false)));

        controller.backend.device_mut(2).unwrap().fail_set_mute = false;
        controller.toggle(None).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn a_toggle_unmutes_when_every_device_reads_muted_though_the_intent_is_unmuted() {
        let mut controller = controller(FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::native("Headset", false)),
        ]));
        // While unmuted, the headset's own button mutes it, and excluding the built-in mic
        // leaves only muted devices: the tray and popup show muted, which nobody asked for.
        controller.backend.change_mute_externally(2, true);
        controller.set_excluded(uids(&["Built-in"]));
        assert!(controller.muted);
        assert!(!controller.should_enforce_mute());

        controller.toggle(None).unwrap();

        assert!(!controller.muted, "the press unmutes, as the tray shows");
        assert!(!controller.should_enforce_mute());
        assert_eq!(controller.backend.writes, vec![Write::Mute(2, false)]);
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
    fn native_mute_that_ignores_writes_falls_back_to_volume() {
        let mut device = Device::native("Built-in", false);
        device.ignore_set_mute = true;
        let stores = Stores::default();
        let mut controller = stores.launch(FakeBackend::with_devices(vec![(1, device)]));

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(0.0));
        // Both accepted writes stay journaled, so exit undoes whichever takes effect.
        let journal = stores.journal_json();
        assert_eq!(journal["devices"]["Built-in UID"]["native_mute"], true);
        assert_eq!(journal["devices"]["Built-in UID"]["saved_volume"], 1.0);
    }

    #[test]
    fn device_whose_native_mute_ignores_writes_stays_muted_by_volume() {
        let mut device = Device::native("Built-in", false);
        device.ignore_set_mute = true;
        let mut controller = controller(FakeBackend::with_devices(vec![(1, device)]));
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().volume = Some(0.4);
        controller.backend.writes.clear();

        controller.toggle(Some(true)).unwrap();

        assert_eq!(controller.backend.writes, vec![Write::Volume(1, 0.0)]);

        controller.mute_all(false).unwrap();

        assert!(!controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().volume, Some(1.0));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn device_ignoring_writes_is_left_out_and_does_not_block_muted() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::ignoring_writes("Teams Audio")),
        ]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
    }

    #[test]
    fn only_a_device_ignoring_writes_does_not_claim_muted() {
        let backend = FakeBackend::with_devices(vec![(1, Device::ignoring_writes("Teams Audio"))]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn volume_controlled_device_ignoring_writes_is_left_out() {
        let mut continuity = Device::fallback("Continuity", 0.65);
        continuity.ignore_set_volume = true;
        let backend = FakeBackend::with_devices(vec![
            (1, continuity),
            (2, Device::native("Built-in", false)),
        ]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    /// The writes a device seemed to ignore took effect after all, too late for the readback.
    fn apply_late(device: &mut Device) {
        device.ignore_set_mute = false;
        device.ignore_set_volume = false;
        device.mute = Some(true);
        device.volume = Some(0.0);
    }

    #[test]
    fn writes_a_device_ignored_stay_journaled_so_exit_undoes_a_late_change() {
        let stores = Stores::default();
        let mut controller = stores.launch(FakeBackend::with_devices(vec![(
            1,
            Device::ignoring_writes("Teams Audio"),
        )]));
        controller.mute_all(true).unwrap();
        let journal = stores.journal_json();
        assert_eq!(journal["devices"]["Teams Audio UID"]["native_mute"], true);
        assert_eq!(journal["devices"]["Teams Audio UID"]["saved_volume"], 1.0);
        apply_late(controller.backend.device_mut(1).unwrap());

        controller.restore_on_exit().unwrap();

        let teams = controller.backend.device(1).unwrap();
        assert_eq!(teams.mute, Some(false));
        assert_eq!(teams.volume, Some(1.0));
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn exit_restore_of_a_device_that_still_ignores_writes_is_harmless() {
        let stores = Stores::default();
        let mut controller = stores.launch(FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::ignoring_writes("Teams Audio")),
        ]));
        controller.mute_all(true).unwrap();

        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        let teams = controller.backend.device(2).unwrap();
        assert_eq!(teams.mute, Some(false));
        assert_eq!(teams.volume, Some(1.0));
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn unmute_undoes_a_late_change_on_a_device_that_ignored_writes() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::ignoring_writes("Teams Audio"),
        )]));
        controller.mute_all(true).unwrap();
        apply_late(controller.backend.device_mut(1).unwrap());

        controller.mute_all(false).unwrap();

        let teams = controller.backend.device(1).unwrap();
        assert_eq!(teams.mute, Some(false));
        assert_eq!(teams.volume, Some(1.0));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn unmuting_a_device_that_ignores_writes_writes_nothing_and_clears_its_journal() {
        let mut controller = controller(FakeBackend::with_devices(vec![(
            1,
            Device::ignoring_writes("Teams Audio"),
        )]));
        controller.mute_all(true).unwrap();
        controller.backend.writes.clear();

        controller.mute_all(false).unwrap();

        assert!(controller.backend.writes.is_empty());
        assert!(controller.journal.is_empty());
    }

    fn built_in_and_teams() -> FakeBackend {
        FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::ignoring_writes("Teams Audio")),
        ])
    }

    fn failure_log() -> FailureLog {
        FailureLog::new("Failed to test", "Tests work again")
    }

    #[test]
    fn failure_log_reports_a_new_failure_at_once() {
        let mut log = failure_log();

        assert_eq!(
            log.record_failure("boom", Instant::now()),
            FailureReport::New
        );
    }

    #[test]
    fn failure_log_suppresses_identical_repeats_then_reports_their_count() {
        let start = Instant::now();
        let mut log = failure_log();
        log.record_failure("boom", start);
        let mut poll = start;
        for _ in 0..299 {
            poll += Duration::from_millis(200);
            assert_eq!(log.record_failure("boom", poll), FailureReport::Suppressed);
        }

        let report_at = start + FAILURE_REPORT_INTERVAL;
        assert_eq!(
            log.record_failure("boom", report_at),
            FailureReport::Repeated {
                count: 300,
                since: FAILURE_REPORT_INTERVAL
            }
        );
        // The quiet period starts over.
        assert_eq!(
            log.record_failure("boom", report_at + Duration::from_secs(1)),
            FailureReport::Suppressed
        );
    }

    #[test]
    fn failure_log_reports_a_different_failure_at_once() {
        let start = Instant::now();
        let mut log = failure_log();
        log.record_failure("boom", start);

        assert_eq!(
            log.record_failure("bang", start + Duration::from_secs(1)),
            FailureReport::New
        );
        assert_eq!(
            log.record_failure("bang", start + Duration::from_secs(2)),
            FailureReport::Suppressed
        );
    }

    #[test]
    fn failure_log_ends_a_run_of_failures_once_with_its_count() {
        let start = Instant::now();
        let mut log = failure_log();
        assert_eq!(log.record_success(), None);
        log.record_failure("boom", start);
        log.record_failure("boom", start + Duration::from_secs(1));
        log.record_failure("bang", start + Duration::from_secs(2));

        assert_eq!(log.record_success(), Some(3));
        assert_eq!(log.record_success(), None);
        assert_eq!(
            log.record_failure("boom", start + Duration::from_secs(3)),
            FailureReport::New
        );
    }

    #[test]
    fn failure_log_report_and_clear_drive_the_same_run_of_failures() {
        let start = Instant::now();
        let mut log = failure_log();

        log.report(&anyhow!("boom"), start);
        assert_eq!(
            log.record_failure("boom", start + Duration::from_secs(1)),
            FailureReport::Suppressed
        );
        log.clear();

        assert_eq!(log.record_success(), None);
    }

    #[test]
    fn write_retry_is_due_after_the_interval_or_a_reconnect() {
        let start = Instant::now();
        let retry = WriteRetry::new(2, start);

        assert!(!retry.is_due(2, start + Duration::from_secs(59)));
        assert!(retry.is_due(2, start + IGNORED_WRITES_RETRY_INTERVAL));
        assert!(retry.is_due(9, start));
        let reconnected = WriteRetry {
            has_disconnected: true,
            ..retry
        };
        assert!(reconnected.is_due(2, start));
    }

    #[test]
    fn device_ignoring_writes_gets_no_writes_until_its_retry_is_due() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        controller.backend.writes.clear();

        controller
            .mute_all_at(true, start + Duration::from_secs(30))
            .unwrap();
        assert!(controller.backend.writes.is_empty());

        controller
            .mute_all_at(true, start + IGNORED_WRITES_RETRY_INTERVAL)
            .unwrap();

        assert_eq!(
            controller.backend.writes,
            vec![Write::Mute(2, true), Write::Volume(2, 0.0)]
        );
        assert!(controller.muted);
    }

    #[test]
    fn device_that_starts_responding_is_muted_at_its_next_retry() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        let teams = controller.backend.device_mut(2).unwrap();
        teams.ignore_set_mute = false;
        teams.ignore_set_volume = false;

        controller
            .mute_all_at(true, start + Duration::from_secs(30))
            .unwrap();
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(false));

        let retry_at = start + IGNORED_WRITES_RETRY_INTERVAL;
        controller.mute_all_at(true, retry_at).unwrap();

        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
        assert!(controller.muted);

        // From now on it is enforced like any other device.
        controller.backend.device_mut(2).unwrap().mute = Some(false);
        controller
            .mute_all_at(true, retry_at + Duration::from_secs(1))
            .unwrap();
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn device_that_ignored_writes_is_retried_when_it_reconnects_under_a_new_id() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        let mut teams = controller.backend.unplug(2);
        teams.ignore_set_mute = false;
        controller.backend.plug(9, teams);

        controller
            .mute_all_at(true, start + Duration::from_secs(1))
            .unwrap();

        assert_eq!(controller.backend.device(9).unwrap().mute, Some(true));
        assert!(controller.muted);
    }

    #[test]
    fn device_that_ignored_writes_is_retried_when_it_returns_under_the_same_id() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        let mut teams = controller.backend.unplug(2);
        controller
            .mute_all_at(true, start + Duration::from_secs(1))
            .unwrap();
        teams.ignore_set_mute = false;
        controller.backend.plug(2, teams);

        controller
            .mute_all_at(true, start + Duration::from_secs(2))
            .unwrap();

        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn device_that_ignored_writes_and_returned_while_unmuted_is_retried_at_the_next_mute() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        controller.mute_all(false).unwrap();
        let mut teams = controller.backend.unplug(2);
        controller.watch_unmuted();
        teams.ignore_set_mute = false;
        controller.backend.plug(2, teams);
        controller.watch_unmuted();

        controller
            .mute_all_at(true, start + Duration::from_secs(2))
            .unwrap();

        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn device_still_ignoring_writes_at_its_retry_waits_for_the_next_one() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        let retry_at = start + IGNORED_WRITES_RETRY_INTERVAL;
        controller.mute_all_at(true, retry_at).unwrap();
        controller.backend.writes.clear();

        controller
            .mute_all_at(true, retry_at + Duration::from_secs(30))
            .unwrap();

        assert!(controller.backend.writes.is_empty());
        assert!(controller.muted);
    }

    #[test]
    fn levels_are_recorded_for_a_device_muted_by_volume_after_its_native_mute_ignored_writes() {
        let mut device = Device::native("Built-in", false);
        device.ignore_set_mute = true;
        let mut controller = controller(FakeBackend::with_devices(vec![(1, device)]));
        controller.mute_all(true).unwrap();
        controller.mute_all(false).unwrap();
        controller.backend.device_mut(1).unwrap().volume = Some(0.4);

        controller.watch_unmuted();

        assert_eq!(controller.levels.get(&key("Built-in")), Some(0.4));
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

    #[test]
    fn read_only_native_mute_is_muted_and_read_by_volume() {
        let backend = FakeBackend::with_devices(vec![(
            1,
            Device::read_only_mute("Virtual Mic", false, Some(0.65)),
        )]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        let device = controller.backend.device(1).unwrap();
        assert_eq!(device.volume, Some(0.0));
        assert_eq!(device.mute, Some(false));
    }

    #[test]
    fn read_only_native_mute_without_volume_does_not_block_other_devices() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::read_only_mute("Virtual Mic", false, None)),
            (2, Device::native("Built-in", false)),
        ]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
    }

    #[test]
    fn only_a_read_only_native_mute_does_not_claim_muted() {
        let backend =
            FakeBackend::with_devices(vec![(1, Device::read_only_mute("Virtual Mic", true, None))]);
        let mut controller = controller(backend);

        controller.mute_all(true).unwrap();

        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn startup_reads_a_read_only_native_mute_device_by_its_volume() {
        let backend = FakeBackend::with_devices(vec![(
            1,
            Device::read_only_mute("Virtual Mic", false, Some(0.0)),
        )]);

        let controller = controller(backend);

        assert!(controller.muted);
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
    fn mute_polls_do_not_write_to_devices_that_already_read_muted() {
        let mut controller = controller(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.backend.writes.clear();

        for _ in 0..3 {
            controller.toggle(Some(true)).unwrap();
        }

        assert!(controller.backend.writes.is_empty());
        assert!(controller.muted);
    }

    #[test]
    fn muting_devices_that_already_read_muted_writes_and_journals_nothing() {
        let stores = Stores::default();
        let mut controller = stores.launch(FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", true)),
            (2, Device::fallback("USB Mic", 0.0)),
        ]));

        controller.mute_all(true).unwrap();

        assert!(controller.backend.writes.is_empty());
        assert_eq!(stores.journal.saves(), 0);
        assert!(controller.muted);
    }

    #[test]
    fn unmuting_devices_that_already_read_unmuted_writes_nothing() {
        let mut controller = controller(native_and_volume_devices());

        controller.mute_all(false).unwrap();

        assert!(controller.backend.writes.is_empty());
        assert!(!controller.muted);
    }

    #[test]
    fn mute_polls_re_mute_devices_someone_else_unmuted_and_exit_restores_them() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(1).unwrap().mute = Some(false);
        controller.backend.device_mut(2).unwrap().volume = Some(0.4);
        controller.backend.writes.clear();

        controller.toggle(Some(true)).unwrap();

        assert_eq!(
            controller.backend.writes,
            vec![Write::Mute(1, true), Write::Volume(2, 0.0)]
        );
        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
        let journal = stores.journal_json();
        assert_eq!(journal["devices"]["Built-in UID"]["native_mute"], true);
        assert_eq!(journal["devices"]["USB Mic UID"]["saved_volume"], 0.65);

        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
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

    #[test]
    fn journal_file_alone_restores_devices_like_the_panic_hook() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        // The panic hook reads restore.json with its own backend, never the controller.
        let mut backend = controller.backend;
        let mut journal = RestoreJournal::load(stores.journal.boxed());

        let failures = restore_journaled_devices(&mut backend, &mut journal);
        journal.save().unwrap();

        assert!(failures.is_empty());
        assert_eq!(backend.device(1).unwrap().mute, Some(false));
        assert_eq!(backend.device(2).unwrap().volume, Some(0.65));
        assert_eq!(stores.journal.contents(), None);
    }

    // Devices and device lists that can't be read

    #[test]
    fn devices_whose_name_streams_or_mute_cant_be_read_dont_stop_the_launch() {
        let mut unnamed = Device::native("Unnamed", true);
        unnamed.fail_name = true;
        let mut unchecked = Device::fallback("USB Mic", 0.0);
        unchecked.fail_input_channels = true;
        let mut unreadable = Device::native("Headset", true);
        unreadable.fail_get_mute = true;
        let backend = FakeBackend::with_devices(vec![
            (1, unnamed),
            (2, unchecked),
            (3, unreadable),
            (4, Device::native("Built-in", true)),
        ]);

        let controller = controller(backend);

        use DeviceStatus::*;
        assert_eq!(
            statuses(&controller),
            [
                ("Audio device 1", Muted),
                ("USB Mic", MutedByVolume),
                ("Headset", Failed),
                ("Built-in", Muted),
            ]
        );
        assert!(
            !controller.muted,
            "a device it can't read never shows as muted"
        );
        assert!(!controller.should_enforce_mute());
    }

    #[test]
    fn a_device_whose_input_streams_cant_be_read_is_muted_with_the_others_and_reported() {
        let mut controller = controller(native_and_volume_devices());
        controller
            .backend
            .device_mut(2)
            .unwrap()
            .fail_input_channels = true;

        let err = controller.mute_all(true).unwrap_err();

        assert!(err.to_string().contains("USB Mic"), "{err}");
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        // It may be a microphone, so it is muted too.
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
        assert!(controller.muted);
        assert_eq!(
            status_of(&controller, "USB Mic"),
            Some(DeviceStatus::Failed)
        );

        // The next poll reads it again.
        controller
            .backend
            .device_mut(2)
            .unwrap()
            .fail_input_channels = false;
        controller.toggle(Some(true)).unwrap();
        assert_eq!(
            status_of(&controller, "USB Mic"),
            Some(DeviceStatus::MutedByVolume)
        );
    }

    #[test]
    fn a_device_whose_name_cant_be_read_is_muted_with_the_others_and_reported() {
        let mut controller = controller(native_and_volume_devices());
        controller.backend.device_mut(1).unwrap().fail_name = true;

        let err = controller.mute_all(true).unwrap_err();

        assert!(err.to_string().contains("Audio device 1"), "{err}");
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
        assert!(controller.muted);
        assert_eq!(
            status_of(&controller, "Audio device 1"),
            Some(DeviceStatus::Failed)
        );

        controller.backend.device_mut(1).unwrap().fail_name = false;
        controller.toggle(Some(true)).unwrap();
        assert_eq!(
            status_of(&controller, "Built-in"),
            Some(DeviceStatus::Muted)
        );
    }

    #[test]
    fn a_device_list_that_cant_be_read_at_launch_leaves_no_devices_until_a_poll_reads_it() {
        let mut backend = native_and_volume_devices();
        backend.fail_device_ids = true;

        let mut controller = controller(backend);

        assert!(!controller.muted);
        assert!(controller.device_states().is_empty());

        // The user mutes; the poll keeps trying until it can list the devices.
        assert!(controller.mute_all(true).is_err());
        assert!(controller.should_enforce_mute());
        controller.backend.fail_device_ids = false;
        controller.toggle(Some(true)).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
    }

    #[test]
    fn a_journal_left_by_an_unclean_exit_waits_for_a_poll_that_can_list_the_devices() {
        let stores = Stores::default();
        let mut crashed = stores.launch(native_and_volume_devices());
        crashed.mute_all(true).unwrap();
        let mut backend = crashed.backend;
        backend.fail_device_ids = true;

        let mut relaunched = stores.launch(backend);

        assert_eq!(relaunched.journal.len(), 2);
        relaunched.backend.fail_device_ids = false;
        relaunched.watch_unmuted();

        assert_eq!(relaunched.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(relaunched.backend.device(2).unwrap().volume, Some(0.65));
        assert_eq!(stores.journal.contents(), None);
    }

    // Excluded devices

    #[test]
    fn muted_needs_a_controlled_device_and_every_controlled_device_muted() {
        let muted = Reading::State(MuteControl::Native, Some(true));
        let unmuted = Reading::State(MuteControl::Volume, Some(false));
        let unreadable = Reading::State(MuteControl::Native, None);
        let left_out = [
            Reading::Excluded,
            Reading::NoControl,
            Reading::IgnoresWrites,
        ];
        let mut left_out_and_muted = left_out.to_vec();
        left_out_and_muted.push(muted);

        assert!(are_controlled_devices_muted([muted]));
        assert!(are_controlled_devices_muted(left_out_and_muted));
        assert!(!are_controlled_devices_muted([muted, unmuted]));
        assert!(!are_controlled_devices_muted(left_out));
        assert!(!are_controlled_devices_muted([unreadable]));
        assert!(!are_controlled_devices_muted([]));
    }

    #[test]
    fn excluded_device_is_never_written_and_does_not_count_toward_muted() {
        let mut controller =
            Stores::default().launch_excluding(native_and_volume_devices(), &["USB Mic"]);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.writes, vec![Write::Mute(1, true)]);
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
    }

    #[test]
    fn excluded_device_is_never_unmuted() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", true)),
            (2, Device::native("Steam Mic", true)),
        ]);
        let mut controller = Stores::default().launch_excluding(backend, &["Steam Mic"]);

        controller.mute_all(false).unwrap();

        assert_eq!(controller.backend.writes, vec![Write::Mute(1, false)]);
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(true));
        assert!(!controller.muted);
    }

    #[test]
    fn only_excluded_devices_do_not_claim_muted() {
        let backend = FakeBackend::with_devices(vec![(1, Device::native("Built-in", true))]);
        let mut controller = Stores::default().launch_excluding(backend, &["Built-in"]);
        assert!(!controller.muted);

        controller.mute_all(true).unwrap();

        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
        assert!(controller.backend.writes.is_empty());
    }

    #[test]
    fn startup_leaves_an_unmuted_excluded_device_out_of_the_mute_state() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", true)),
            (2, Device::native("Steam Mic", false)),
        ]);

        let controller = Stores::default().launch_excluding(backend, &["Steam Mic"]);

        assert!(controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn excluding_devices_the_app_muted_undoes_its_changes_by_the_journal() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();

        assert!(controller.set_excluded(uids(&["Built-in", "USB Mic"])));

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert_eq!(stores.journal.contents(), None);
        assert!(!controller.muted);
        assert!(controller.should_enforce_mute());
    }

    #[test]
    fn excluded_device_stays_audible_while_mute_is_enforced() {
        let mut controller = controller(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.set_excluded(uids(&["USB Mic"]));
        controller.backend.writes.clear();

        for _ in 0..3 {
            controller.toggle(Some(true)).unwrap();
        }

        assert!(controller.backend.writes.is_empty());
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert!(controller.muted);
    }

    #[test]
    fn excluding_devices_muted_before_launch_leaves_them_muted() {
        let backend = FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", true)),
            (2, Device::fallback("USB Mic", 0.0)),
        ]);
        let mut controller = controller(backend);
        controller.mute_all(true).unwrap();

        controller.set_excluded(uids(&["Built-in", "USB Mic"]));

        assert!(controller.backend.writes.is_empty());
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
    }

    #[test]
    fn including_a_device_while_muted_mutes_it_on_the_next_poll() {
        let mut controller =
            Stores::default().launch_excluding(native_and_volume_devices(), &["USB Mic"]);
        controller.mute_all(true).unwrap();

        assert!(controller.set_excluded(BTreeSet::new()));
        assert!(!controller.muted, "the USB mic is live until the poll");

        controller.toggle(Some(true)).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
    }

    #[test]
    fn unchanged_exclusions_change_nothing() {
        let mut controller =
            Stores::default().launch_excluding(native_and_volume_devices(), &["USB Mic"]);

        assert!(!controller.set_excluded(uids(&["USB Mic"])));
    }

    #[test]
    fn exclusions_follow_the_uid_not_the_name_or_the_id() {
        let mut first = Device::native("USB Microphone", false);
        first.uid = "first UID".to_string();
        let mut second = Device::native("USB Microphone", false);
        second.uid = "second UID".to_string();
        let mut controller = controller(FakeBackend::with_devices(vec![(1, first), (2, second)]));
        controller.set_excluded(BTreeSet::from(["second UID".to_string()]));

        controller.mute_all(true).unwrap();
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().mute, Some(false));

        // Back under a new ID and a new name, it is still excluded.
        let mut second = controller.backend.unplug(2);
        second.name = "Renamed Microphone".to_string();
        controller.backend.plug(7, second);
        controller.toggle(Some(true)).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(7).unwrap().mute, Some(false));
    }

    #[test]
    fn device_without_a_uid_is_never_excluded() {
        let mut device = Device::native("No UID", false);
        device.fail_uid = true;
        let backend = FakeBackend::with_devices(vec![(1, device)]);
        let mut controller = Stores::default().launch_excluding(backend, &["No UID"]);

        controller.mute_all(true).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
    }

    #[test]
    fn excluded_device_that_reconnects_while_muted_is_restored_once() {
        let mut controller = controller(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        let usb = controller.backend.unplug(2);
        controller.set_excluded(uids(&["USB Mic"]));
        controller.toggle(Some(true)).unwrap();
        assert_eq!(controller.journal.saved_volume(&key("USB Mic")), Some(0.65));

        controller.backend.plug(9, usb);
        controller.toggle(Some(true)).unwrap();

        assert_eq!(controller.backend.device(9).unwrap().volume, Some(0.65));
        assert!(controller.journal.get(&key("USB Mic")).is_none());
        assert!(controller.muted);
        controller.backend.writes.clear();
        controller.toggle(Some(true)).unwrap();
        assert!(controller.backend.writes.is_empty());
    }

    #[test]
    fn failed_restore_of_an_excluded_device_waits_for_exit() {
        let stores = Stores::default();
        let mut controller = stores.launch(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(2).unwrap().fail_set_volume = true;

        controller.set_excluded(uids(&["USB Mic"]));

        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
        assert_eq!(
            stores.journal_json()["devices"]["USB Mic UID"]["saved_volume"],
            0.65
        );
        controller.backend.writes.clear();
        controller.toggle(Some(true)).unwrap();
        assert!(
            controller.backend.writes.is_empty(),
            "mute polls don't retry it"
        );

        controller.backend.device_mut(2).unwrap().fail_set_volume = false;
        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert_eq!(stores.journal.contents(), None);
    }

    #[test]
    fn user_unmute_retries_restoring_an_excluded_device() {
        let mut controller = controller(native_and_volume_devices());
        controller.mute_all(true).unwrap();
        controller.backend.device_mut(2).unwrap().fail_set_volume = true;
        controller.set_excluded(uids(&["USB Mic"]));
        controller.backend.device_mut(2).unwrap().fail_set_volume = false;

        controller.mute_all(false).unwrap();

        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert!(controller.journal.is_empty());
    }

    #[test]
    fn excluding_a_device_that_ignores_writes_ends_its_write_retries() {
        let start = Instant::now();
        let mut controller = controller(built_in_and_teams());
        controller.mute_all_at(true, start).unwrap();
        controller.set_excluded(uids(&["Teams Audio"]));
        assert!(controller.journal.get(&key("Teams Audio")).is_none());
        controller.backend.writes.clear();

        controller
            .mute_all_at(true, start + IGNORED_WRITES_RETRY_INTERVAL)
            .unwrap();

        assert!(controller.backend.writes.is_empty());
        assert!(controller.muted);
    }

    // Device states for the Microphones menu

    fn statuses(controller: &MicController<FakeBackend>) -> Vec<(&str, DeviceStatus)> {
        controller
            .device_states()
            .iter()
            .map(|state| (state.name.as_str(), state.status))
            .collect()
    }

    fn status_of(controller: &MicController<FakeBackend>, name: &str) -> Option<DeviceStatus> {
        controller
            .device_states()
            .iter()
            .find(|state| state.name == name)
            .map(|state| state.status)
    }

    #[test]
    fn device_status_shows_exclusions_then_failures_then_the_reading() {
        use DeviceStatus::*;
        let native = |state| Some(Reading::State(MuteControl::Native, state));
        let volume = |state| Some(Reading::State(MuteControl::Volume, state));
        let cases = [
            (native(Some(true)), false, Muted),
            (volume(Some(true)), false, MutedByVolume),
            (native(Some(false)), false, Unmuted),
            (volume(Some(false)), false, Unmuted),
            (Some(Reading::IgnoresWrites), false, IgnoresWrites),
            (Some(Reading::NoControl), false, NoControl),
            (Some(Reading::Excluded), false, Excluded),
            (Some(Reading::Excluded), true, Excluded),
            (native(Some(false)), true, Failed),
            (native(None), false, Failed),
            (None, false, Failed),
        ];
        for (reading, has_failed, status) in cases {
            assert_eq!(
                device_status(reading, has_failed),
                status,
                "{reading:?}, failed {has_failed}"
            );
        }
    }

    #[test]
    fn device_states_show_what_a_mute_did_to_each_device() {
        let mut broken = Device::native("Broken Mic", false);
        broken.fail_set_mute = true;
        let backend = FakeBackend::with_devices(vec![
            (1, Device::native("Built-in", false)),
            (2, Device::fallback("USB Mic", 0.65)),
            (3, Device::ignoring_writes("Teams Audio")),
            (4, Device::no_control("iPhone Mic")),
            (5, broken),
            (6, Device::native("Steam Mic", false)),
        ]);
        let mut controller = Stores::default().launch_excluding(backend, &["Steam Mic"]);

        assert!(controller.mute_all(true).is_err());

        use DeviceStatus::*;
        assert_eq!(
            statuses(&controller),
            [
                ("Built-in", Muted),
                ("USB Mic", MutedByVolume),
                ("Teams Audio", IgnoresWrites),
                ("iPhone Mic", NoControl),
                ("Broken Mic", Failed),
                ("Steam Mic", Excluded),
            ]
        );
        let uids: Vec<_> = controller
            .device_states()
            .iter()
            .map(|state| state.uid.clone())
            .collect();
        assert_eq!(uids[0].as_deref(), Some("Built-in UID"));
        assert!(uids.iter().all(Option::is_some));
    }

    #[test]
    fn device_states_are_read_at_startup_and_follow_each_toggle() {
        let mut controller = controller(native_and_volume_devices());
        use DeviceStatus::*;
        assert_eq!(
            statuses(&controller),
            [("Built-in", Unmuted), ("USB Mic", Unmuted)]
        );

        controller.toggle(None).unwrap();
        assert_eq!(
            statuses(&controller),
            [("Built-in", Muted), ("USB Mic", MutedByVolume)]
        );

        controller.toggle(None).unwrap();
        assert_eq!(
            statuses(&controller),
            [("Built-in", Unmuted), ("USB Mic", Unmuted)]
        );
    }

    #[test]
    fn device_whose_unmute_failed_shows_failed_until_the_next_toggle() {
        let mut controller = controller(native_and_volume_devices());
        let start = Instant::now();
        controller.mute_all_at(true, start).unwrap();
        controller.backend.device_mut(2).unwrap().fail_set_volume = true;
        assert!(controller.mute_all_at(false, start).is_err());

        controller.watch_unmuted_at(start + DEVICE_STATES_REFRESH_INTERVAL);
        assert_eq!(
            status_of(&controller, "USB Mic"),
            Some(DeviceStatus::Failed)
        );

        controller.backend.device_mut(2).unwrap().fail_set_volume = false;
        controller.mute_all(false).unwrap();
        assert_eq!(
            status_of(&controller, "USB Mic"),
            Some(DeviceStatus::Unmuted)
        );
    }

    #[test]
    fn device_states_while_unmuted_are_read_again_once_the_interval_has_passed() {
        let mut controller = controller(native_and_volume_devices());
        // After the startup read, so that read is never in the future.
        let start = Instant::now();
        // Muted by someone else; the unmuted poll writes nothing.
        controller.backend.device_mut(1).unwrap().mute = Some(true);

        controller.watch_unmuted_at(start + Duration::from_secs(1));
        assert_eq!(
            status_of(&controller, "Built-in"),
            Some(DeviceStatus::Unmuted)
        );

        controller.watch_unmuted_at(start + DEVICE_STATES_REFRESH_INTERVAL);
        assert_eq!(
            status_of(&controller, "Built-in"),
            Some(DeviceStatus::Muted)
        );
        assert!(controller.backend.writes.is_empty());
    }

    #[test]
    fn device_states_while_unmuted_show_a_device_that_comes_or_goes_at_once() {
        let start = Instant::now();
        let mut controller = controller(native_and_volume_devices());

        controller.backend.plug(5, Device::native("Headset", false));
        controller.watch_unmuted_at(start);
        assert_eq!(
            status_of(&controller, "Headset"),
            Some(DeviceStatus::Unmuted)
        );

        controller.backend.unplug(2);
        controller.watch_unmuted_at(start);
        assert_eq!(
            statuses(&controller),
            [
                ("Built-in", DeviceStatus::Unmuted),
                ("Headset", DeviceStatus::Unmuted)
            ]
        );
    }

    #[test]
    fn device_states_show_a_new_exclusion_at_the_next_poll() {
        let start = Instant::now();
        let mut controller = controller(native_and_volume_devices());

        controller.set_excluded(uids(&["USB Mic"]));
        controller.watch_unmuted_at(start);

        assert_eq!(
            status_of(&controller, "USB Mic"),
            Some(DeviceStatus::Excluded)
        );
    }

    #[test]
    fn device_states_while_unmuted_show_an_unreadable_device_as_failed() {
        let mut controller = controller(native_and_volume_devices());
        let start = Instant::now();
        controller.backend.device_mut(1).unwrap().fail_get_mute = true;

        controller.watch_unmuted_at(start + DEVICE_STATES_REFRESH_INTERVAL);

        assert_eq!(
            status_of(&controller, "Built-in"),
            Some(DeviceStatus::Failed)
        );
    }

    #[test]
    fn device_without_a_uid_is_listed_without_one() {
        let mut device = Device::native("No UID", false);
        device.fail_uid = true;

        let controller = controller(FakeBackend::with_devices(vec![(1, device)]));

        assert_eq!(
            controller.device_states(),
            [DeviceState {
                uid: None,
                name: "No UID".to_string(),
                status: DeviceStatus::Unmuted,
            }]
        );
    }

    #[test]
    fn core_audio_listens_to_the_controls_it_reads_and_to_the_device_list() {
        let addresses = CoreAudioBackend::listened_addresses();
        let selectors = addresses.map(|address| address.mSelector);
        assert_eq!(
            selectors,
            [
                kAudioDevicePropertyMute,
                kAudioDevicePropertyVolumeScalar,
                AUDIO_HARDWARE_SERVICE_DEVICE_PROPERTY_VIRTUAL_MAIN_VOLUME
            ]
        );
        for address in addresses {
            assert_eq!(address.mScope, kAudioDevicePropertyScopeInput);
            assert_eq!(address.mElement, kAudioObjectPropertyElementMain);
        }

        let device_list = CoreAudioBackend::device_list_address();
        assert_eq!(device_list.mSelector, kAudioHardwarePropertyDevices);
        assert_eq!(device_list.mScope, kAudioObjectPropertyScopeGlobal);
        assert_eq!(device_list.mElement, kAudioObjectPropertyElementMain);
        assert_eq!(SYSTEM_OBJECT, 1);
    }

    /// A signal for one test, leaked like the app's, and how often it woke the loop.
    fn counting_signal() -> (&'static DeviceChangeSignal, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let signal = Box::leak(Box::new(DeviceChangeSignal::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        })));
        (signal, wakes)
    }

    /// A controller listening for device changes, its signal, and how often that woke the loop.
    fn listening(
        backend: FakeBackend,
    ) -> (
        MicController<FakeBackend>,
        &'static DeviceChangeSignal,
        Arc<AtomicUsize>,
    ) {
        let mut controller = controller(backend);
        let (signal, wakes) = counting_signal();
        controller.start_listening(signal);
        (controller, signal, wakes)
    }

    /// A built-in mic with native mute, a USB mic muted by volume, and speakers without input.
    fn listened_devices() -> FakeBackend {
        let mut speakers = Device::native("Speakers", false);
        speakers.input = false;
        let mut backend = native_and_volume_devices();
        backend.plug(3, speakers);
        backend
    }

    fn listened_selectors(
        controller: &MicController<FakeBackend>,
        id: AudioDeviceID,
    ) -> Option<Vec<AudioObjectPropertySelector>> {
        let (selectors, _) = controller.backend.device_listeners.get(&id)?;
        Some(selectors.clone())
    }

    const NATIVE_AND_VOLUME: [AudioObjectPropertySelector; 2] =
        [kAudioDevicePropertyMute, kAudioDevicePropertyVolumeScalar];

    #[test]
    fn listening_covers_the_device_list_and_the_controls_of_every_input_device() {
        let (controller, signal, _) = listening(listened_devices());

        assert!(controller.is_listening());
        let device_list_listener = controller.backend.device_list_listener;
        assert!(device_list_listener.is_some_and(|registered| std::ptr::eq(registered, signal)));
        assert_eq!(
            listened_selectors(&controller, 1),
            Some(NATIVE_AND_VOLUME.to_vec())
        );
        assert_eq!(
            listened_selectors(&controller, 2),
            Some(vec![kAudioDevicePropertyVolumeScalar])
        );
        assert_eq!(listened_selectors(&controller, 3), None, "no input");
    }

    #[test]
    fn a_device_without_controls_has_nothing_to_listen_to_and_counts_as_listened() {
        let backend = FakeBackend::with_devices(vec![(1, Device::no_control("Virtual"))]);

        let (controller, _, _) = listening(backend);

        assert!(controller.is_listening());
        assert_eq!(listened_selectors(&controller, 1), Some(Vec::new()));
    }

    #[test]
    fn listeners_follow_devices_that_come_and_go() {
        let (mut controller, signal, wakes) = listening(listened_devices());

        controller.backend.unplug(1);
        controller.backend.plug(5, Device::native("Headset", false));
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "one wake-up for both");
        assert!(signal.take());
        controller.watch_unmuted();

        assert_eq!(listened_selectors(&controller, 1), None);
        assert_eq!(
            listened_selectors(&controller, 5),
            Some(NATIVE_AND_VOLUME.to_vec())
        );
        // Removing the listeners of a device that went away fails, which is fine.
        assert!(controller.is_listening());
    }

    #[test]
    fn an_external_unmute_while_muted_wakes_the_loop_once_and_its_poll_mutes_again() {
        let (mut controller, signal, wakes) = listening(listened_devices());
        controller.mute_all(true).unwrap();
        // The poll after the app's own writes takes the changes they reported.
        signal.take();
        let woken = wakes.load(Ordering::Relaxed);

        controller.backend.change_mute_externally(1, false);
        controller.backend.change_volume_externally(2, 0.4);
        assert_eq!(wakes.load(Ordering::Relaxed), woken + 1);
        assert!(signal.take());
        // The poll the wake-up runs: `update_mic` enforcing mute.
        controller.toggle(Some(true)).unwrap();

        assert!(controller.muted);
        assert_eq!(controller.backend.device(1).unwrap().mute, Some(true));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.0));
    }

    #[test]
    fn the_poll_after_the_apps_own_writes_writes_nothing_and_wakes_nothing() {
        let (mut controller, signal, wakes) = listening(listened_devices());

        controller.mute_all(true).unwrap();
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "own writes report changes"
        );
        assert!(signal.take());
        controller.backend.writes.clear();
        controller.toggle(Some(true)).unwrap();

        assert!(controller.backend.writes.is_empty());
        assert!(!signal.take(), "no write, so no change to report");
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn control_changes_while_unmuted_wake_nothing_but_a_new_device_does() {
        let (mut controller, signal, wakes) = listening(listened_devices());
        controller.mute_all(true).unwrap();
        controller.mute_all(false).unwrap();
        signal.take();
        let woken = wakes.load(Ordering::Relaxed);

        controller.backend.change_mute_externally(1, true);
        controller.backend.change_volume_externally(2, 0.2);
        assert_eq!(wakes.load(Ordering::Relaxed), woken);
        assert!(!signal.take());

        controller.backend.plug(5, Device::native("Headset", false));
        assert_eq!(wakes.load(Ordering::Relaxed), woken + 1);
    }

    #[test]
    fn exit_removes_every_listener_before_it_restores() {
        let (mut controller, signal, wakes) = listening(listened_devices());
        controller.mute_all(true).unwrap();
        signal.take();
        let woken = wakes.load(Ordering::Relaxed);

        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.device(1).unwrap().mute, Some(false));
        assert_eq!(controller.backend.device(2).unwrap().volume, Some(0.65));
        assert!(controller.backend.device_list_listener.is_none());
        assert!(controller.backend.device_listeners.is_empty());
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            woken,
            "the restore woke nothing"
        );
        assert!(!controller.is_listening());
    }

    #[test]
    fn listening_ends_for_good_at_exit() {
        let (mut controller, _, _) = listening(listened_devices());
        controller.restore_on_exit().unwrap();
        let calls = controller.backend.listener_calls;

        controller.backend.plug(5, Device::native("Headset", false));
        controller.watch_unmuted();
        controller.mute_all(true).unwrap();
        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.listener_calls, calls);
        assert!(controller.backend.device_listeners.is_empty());
    }

    #[test]
    fn a_device_whose_listeners_fail_leaves_listening_incomplete_until_a_poll_registers_it() {
        let mut backend = listened_devices();
        backend.device_mut(2).unwrap().fail_listen = true;

        let (mut controller, _, _) = listening(backend);
        assert!(!controller.is_listening());
        assert_eq!(
            listened_selectors(&controller, 1),
            Some(NATIVE_AND_VOLUME.to_vec()),
            "the other devices are listened to"
        );
        controller.watch_unmuted();
        assert!(!controller.is_listening(), "it still fails");

        controller.backend.device_mut(2).unwrap().fail_listen = false;
        controller.watch_unmuted();
        assert!(controller.is_listening());
        assert_eq!(
            listened_selectors(&controller, 2),
            Some(vec![kAudioDevicePropertyVolumeScalar])
        );
    }

    #[test]
    fn a_failed_device_list_listener_is_registered_by_a_later_poll() {
        let mut backend = listened_devices();
        backend.fail_device_list_listener = true;

        let (mut controller, _, _) = listening(backend);
        assert!(!controller.is_listening());

        controller.backend.fail_device_list_listener = false;
        controller.mute_all(true).unwrap();
        assert!(controller.is_listening());
        assert!(controller.backend.device_list_listener.is_some());
    }

    #[test]
    fn polls_without_device_changes_make_no_listener_calls() {
        let (mut controller, _, _) = listening(listened_devices());
        let calls = controller.backend.listener_calls;

        controller.mute_all(true).unwrap();
        controller.toggle(Some(true)).unwrap();
        controller.mute_all(false).unwrap();
        controller.watch_unmuted();

        assert_eq!(controller.backend.listener_calls, calls);
    }

    #[test]
    fn a_controller_that_never_listens_registers_nothing() {
        let mut controller = controller(listened_devices());

        controller.mute_all(true).unwrap();
        controller.watch_unmuted();
        controller.restore_on_exit().unwrap();

        assert_eq!(controller.backend.listener_calls, 0);
        assert!(!controller.is_listening());
    }
}
