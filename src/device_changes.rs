//! Wakes the event loop when CoreAudio reports a device change: a device came or went, or an
//! input device's native mute or input volume changed. `MicController` registers the listeners
//! (`AudioBackend::listen_to_device_list`, `listen_to_device`), all calling [`property_listener`]
//! with one [`DeviceChangeSignal`]. The loop then polls the microphones at once, so it undoes an
//! external unmute within milliseconds, and its regular poll is only a safety net.

use objc2_core_audio::{kAudioObjectSystemObject, AudioObjectID, AudioObjectPropertyAddress};
use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a listener call reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceChange {
    /// A device came or went: `kAudioHardwarePropertyDevices` on the system object.
    DeviceList,
    /// An input device's native mute or input volume changed.
    Controls,
}

impl DeviceChange {
    /// The change a listener call for `object_id` reports. The app listens only to the device
    /// list on the system object, and only to controls on devices.
    fn reported_by(object_id: AudioObjectID) -> Self {
        if object_id == kAudioObjectSystemObject as AudioObjectID {
            Self::DeviceList
        } else {
            Self::Controls
        }
    }
}

/// Wakes the event loop once per burst of device changes: the first change since the loop last
/// took one calls `wake`; later ones find it pending. The loop takes the change before it reads
/// the devices, so a change during the read wakes it again.
pub struct DeviceChangeSignal {
    is_pending: AtomicBool,
    /// Mirrors `MicController::should_enforce_mute`. A control change matters only while the app
    /// enforces mute: while the user wants the mics live, the poll reads their levels on its own
    /// schedule, and a call's automatic gain control can't keep waking the loop.
    is_enforcing_mute: AtomicBool,
    /// Posts to the event loop. Runs on CoreAudio's notification thread or the main run loop.
    wake: Box<dyn Fn() + Send + Sync>,
}

impl DeviceChangeSignal {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            is_pending: AtomicBool::new(false),
            is_enforcing_mute: AtomicBool::new(false),
            wake: Box::new(wake),
        }
    }

    /// Records a change, and wakes the loop for the first one since [`Self::take`] that matters.
    /// Returns whether it woke the loop.
    pub fn notify(&self, change: DeviceChange) -> bool {
        if change == DeviceChange::Controls && !self.is_enforcing_mute.load(Ordering::Relaxed) {
            return false;
        }
        if self.is_pending.swap(true, Ordering::AcqRel) {
            return false;
        }
        (self.wake)();
        true
    }

    /// Takes the pending change; returns whether one arrived since the last call.
    pub fn take(&self) -> bool {
        self.is_pending.swap(false, Ordering::AcqRel)
    }

    pub fn set_enforcing_mute(&self, is_enforcing: bool) {
        self.is_enforcing_mute
            .store(is_enforcing, Ordering::Relaxed);
    }
}

/// The listener proc of every property the app listens to. CoreAudio calls it on its notification
/// thread or the main run loop, with the `client_data` given at registration: a
/// `&'static DeviceChangeSignal`. It only signals the event loop: no locks, no AppKit, and no
/// unwinding into CoreAudio.
///
/// # Safety
///
/// `client_data` must be null or point to a `DeviceChangeSignal` that is never freed.
pub unsafe extern "C-unwind" fn property_listener(
    object_id: AudioObjectID,
    _number_addresses: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    // SAFETY: the app registers leaked signals only, so one outlives every call, including a
    // call that races the listener's removal.
    let Some(signal) = (unsafe { client_data.cast::<DeviceChangeSignal>().as_ref() }) else {
        return 0;
    };
    let change = DeviceChange::reported_by(object_id);
    // `notify` doesn't panic, but a panic must never unwind into CoreAudio: abort, as one in an
    // `extern "C"` function would.
    if panic::catch_unwind(AssertUnwindSafe(|| signal.notify(change))).is_err() {
        std::process::abort();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_audio::{
        kAudioDevicePropertyMute, kAudioDevicePropertyScopeInput, kAudioObjectPropertyElementMain,
    };
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// A signal and how often it woke the loop.
    fn counting_signal() -> (DeviceChangeSignal, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let signal = DeviceChangeSignal::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        (signal, wakes)
    }

    #[test]
    fn a_burst_of_changes_wakes_the_loop_once() {
        let (signal, wakes) = counting_signal();
        signal.set_enforcing_mute(true);

        assert!(signal.notify(DeviceChange::Controls));
        assert!(!signal.notify(DeviceChange::Controls));
        assert!(!signal.notify(DeviceChange::DeviceList));

        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(signal.take());
        assert!(!signal.take(), "taking clears the change");
    }

    #[test]
    fn a_change_after_the_loop_took_one_wakes_it_again() {
        let (signal, wakes) = counting_signal();
        signal.notify(DeviceChange::DeviceList);
        signal.take();

        assert!(signal.notify(DeviceChange::DeviceList));

        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn control_changes_wake_the_loop_only_while_mute_is_enforced() {
        let (signal, wakes) = counting_signal();

        assert!(!signal.notify(DeviceChange::Controls));
        assert!(!signal.take(), "an ignored change isn't pending");
        assert!(
            signal.notify(DeviceChange::DeviceList),
            "the device list always counts"
        );
        signal.take();

        signal.set_enforcing_mute(true);
        assert!(signal.notify(DeviceChange::Controls));

        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_core_audio_listener_signals_its_client_data() {
        let (signal, wakes) = counting_signal();
        let client_data = (&signal as *const DeviceChangeSignal)
            .cast_mut()
            .cast::<c_void>();
        let mut address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let addresses = NonNull::from(&mut address);
        let device_id: AudioObjectID = 42;
        let system_object = kAudioObjectSystemObject as AudioObjectID;

        // Not enforcing mute: the device's mute change is ignored, the device list isn't.
        assert_eq!(
            unsafe { property_listener(device_id, 1, addresses, client_data) },
            0
        );
        assert!(!signal.take());
        unsafe { property_listener(system_object, 1, addresses, client_data) };
        assert!(signal.take());

        signal.set_enforcing_mute(true);
        unsafe { property_listener(device_id, 1, addresses, client_data) };
        assert!(signal.take());
        assert_eq!(wakes.load(Ordering::Relaxed), 2);

        // Without client data there is nothing to signal.
        assert_eq!(
            unsafe { property_listener(device_id, 1, addresses, std::ptr::null_mut()) },
            0
        );
    }
}
