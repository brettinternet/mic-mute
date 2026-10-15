//! Feedback on a mute change besides the tray and the popup: a system sound (`play_sound`) and
//! a macOS notification (`show_notification`). Only a request that changes the microphones'
//! state gives either (`change_to_report`); the poll's re-mutes never do.
use crate::notification::Notifier;
use anyhow::{bail, Result};
use cocoa::base::{id, nil, BOOL, NO};
use cocoa::foundation::NSString;

/// What made the event loop update the microphones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicUpdate {
    /// A request: the shortcut, the tray's Mute item, a URL, or an automatic mute the user set
    /// up (`event_loop::request_mic`).
    Request,
    /// The 200 ms poll muted devices again while the user wants them muted.
    Enforcement,
}

/// The state to report after `update` took the microphones from `was_muted` to `muted`: only a
/// request that changed it. The poll's re-mutes report nothing, and neither does a request that
/// changed nothing, such as a mute while muted, or a mute that failed: silence then, rather than
/// a sound that claims a change.
pub fn change_to_report(update: MicUpdate, was_muted: bool, muted: bool) -> Option<bool> {
    (update == MicUpdate::Request && was_muted != muted).then_some(muted)
}

/// The system sound for a mute change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Mute,
    Unmute,
}

impl Sound {
    /// The sound for a change that left the microphones `muted`.
    pub fn for_state(muted: bool) -> Self {
        if muted {
            Self::Mute
        } else {
            Self::Unmute
        }
    }

    /// The sound's name in /System/Library/Sounds, which `+[NSSound soundNamed:]` searches.
    pub fn name(self) -> &'static str {
        match self {
            // A hollow knock: closed.
            Self::Mute => "Bottle",
            // A bright tap: live.
            Self::Unmute => "Tink",
        }
    }
}

/// The sound and notification settings in use. Main thread only, like the AppKit calls it makes.
#[derive(Default)]
pub struct Feedback {
    play_sound: bool,
    notifier: Notifier,
}

impl Feedback {
    pub fn new(play_sound: bool, show_notification: bool) -> Self {
        let mut feedback = Self::default();
        feedback.set_options(play_sound, show_notification);
        feedback
    }

    /// Applies `play_sound` and `show_notification`. Repeating the values in use does nothing,
    /// so `UI::apply_settings` can run it after every reload.
    pub fn set_options(&mut self, play_sound: bool, show_notification: bool) {
        if play_sound && !self.play_sound {
            preload_sounds();
        }
        self.play_sound = play_sound;
        self.notifier.set_enabled(show_notification);
    }

    /// Plays the sound and posts the notification the settings ask for after `update` took the
    /// microphones from `was_muted` to `muted` (see `change_to_report`). Returns at once: macOS
    /// plays and shows them in the background.
    pub fn on_mic_update(&mut self, update: MicUpdate, was_muted: bool, muted: bool) {
        let Some(muted) = change_to_report(update, was_muted, muted) else {
            return;
        };
        if self.play_sound {
            let sound = Sound::for_state(muted);
            if let Err(err) = play(sound) {
                log::warn!("Failed to play the {} sound: {:#}", sound.name(), err);
            }
        }
        if self.notifier.is_enabled() {
            self.notifier.post(muted);
        }
    }
}

/// `+[NSSound soundNamed:]`: AppKit loads the sound on first use and keeps it under its name,
/// so the caller doesn't own it.
fn named_sound(sound: Sound) -> Result<id> {
    unsafe {
        let name = NSString::alloc(nil).init_str(sound.name());
        let ns_sound: id = msg_send![class!(NSSound), soundNamed: name];
        let _: () = msg_send![name, release];
        if ns_sound.is_null() {
            bail!("macOS has no sound named {}", sound.name());
        }
        Ok(ns_sound)
    }
}

/// Plays `sound` through the default output device; nothing here touches an input device.
/// NSSound plays in the background, so this returns at once.
fn play(sound: Sound) -> Result<()> {
    let ns_sound = named_sound(sound)?;
    unsafe {
        // An NSSound plays once at a time: restart it when a quick second change replays it.
        let _: BOOL = msg_send![ns_sound, stop];
        let started: BOOL = msg_send![ns_sound, play];
        if started == NO {
            bail!("NSSound didn't start playing");
        }
    }
    Ok(())
}

/// Loads both sounds, so the first change doesn't wait for their files.
fn preload_sounds() {
    for sound in [Sound::Mute, Sound::Unmute] {
        if let Err(err) = named_sound(sound) {
            log::warn!("Failed to load the {} sound: {:#}", sound.name(), err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn enforcement_never_reports_a_change() {
        for (was_muted, muted) in [(false, true), (true, false), (true, true), (false, false)] {
            assert_eq!(
                change_to_report(MicUpdate::Enforcement, was_muted, muted),
                None,
                "{was_muted} → {muted}"
            );
        }
    }

    #[test]
    fn a_request_reports_the_state_it_changed_to() {
        assert_eq!(
            change_to_report(MicUpdate::Request, false, true),
            Some(true)
        );
        assert_eq!(
            change_to_report(MicUpdate::Request, true, false),
            Some(false)
        );
    }

    #[test]
    fn a_request_that_changed_nothing_reports_nothing() {
        // A mute while muted, or a mute whose writes all failed.
        assert_eq!(change_to_report(MicUpdate::Request, true, true), None);
        assert_eq!(change_to_report(MicUpdate::Request, false, false), None);
    }

    #[test]
    fn muting_and_unmuting_play_different_sounds() {
        assert_eq!(Sound::for_state(true), Sound::Mute);
        assert_eq!(Sound::for_state(false), Sound::Unmute);
        assert_ne!(Sound::Mute.name(), Sound::Unmute.name());
    }

    /// Checks the files, without loading or playing them.
    #[test]
    fn both_sounds_are_macos_system_sounds() {
        for sound in [Sound::Mute, Sound::Unmute] {
            let path = Path::new("/System/Library/Sounds").join(format!("{}.aiff", sound.name()));
            assert!(path.is_file(), "{} is missing", path.display());
        }
    }
}
