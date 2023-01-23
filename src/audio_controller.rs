use std::fmt;
use std::fmt::{Debug, Formatter};
use std::result::Result;

#[derive(Debug)]
pub struct AudioError {
    pub msg: String,
}

/// Trait that describes an audio input device.
pub trait AudioInputDeviceTrait {
    /// Collect names of audio devices
    fn names(&self) -> Result<Vec<String>, AudioError>;

    /// Sets the mute state of the audio device.
    fn set_mute(&self, audio_device_id: u32, state: bool) -> Result<bool, AudioError>;

    /// Set the mute state for all audio devices.
    fn set_mute_all(&self, state: bool) -> Result<bool, AudioError>;
}

impl Debug for dyn AudioInputDeviceTrait {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("AudioInputDevice")
            .field("names", &self.names().unwrap())
            .finish()
    }
}
