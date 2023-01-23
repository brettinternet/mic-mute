// use coreaudio::audio_unit::AudioUnit;

// use crate::audio_controller::{AudioError, AudioInputDeviceTrait};

// /// Trait that describes the audio subsystem.
// pub trait AudioControllerTrait {
//     /// Create a new connection to the audio subsystem.
//     #[allow(clippy::new_ret_no_self)]
//     fn new() -> Box<dyn AudioControllerTrait>
//     where
//         Self: Sized;

//     /// Gets the input audio units
//     fn get_audio_units(&self) -> Result<Box<dyn AudioInputDeviceTrait>, AudioError>;
// }

// pub struct MicController<'a> {
//     audio_units: Option<&'a dyn AudioInputDeviceTrait>,
// }

// impl MicController<'_> {
//     pub fn new<T: AudioControllerTrait>() -> Self {
//         MicController {
//             audio_units: {
//                 let audio = Box::leak(T::new());
//                 Some(Box::leak(audio.get_comms_devices().unwrap()))
//             },
//         }
//     }

//     pub fn device_names(&self) -> Result<Vec<String>, AudioError> {
//         match self.comms_devices {
//             Some(c) => c.names(),
//             None => Ok(Vec::new()),
//         }
//     }

//     pub fn mute(&mut self, state: bool) -> Result<(), AudioError> {
//         match self.comms_devices {
//             Some(c) => c.set_mute_all(state).map(|_| ()),
//             None => Ok(()),
//         }
//     }
// }
