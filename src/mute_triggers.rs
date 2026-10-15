//! Automatic mute: whether a launch mutes (`launch_state`), which session events mute in toggle
//! mode (`mute_on_lock`, `mute_on_sleep`, `mute_on_display_sleep`), and `mute-state.json`, the
//! mute state the user last chose, which `launch_state: "last"` brings back. Nothing here ever
//! unmutes.
//!
//! macOS has no public lid-close notification. Closing the lid sleeps the Mac and its displays
//! (`mute_on_sleep`, `mute_on_display_sleep`) unless an external display keeps it awake
//! (clamshell mode); then nothing reports the lid, and nothing mutes.

use crate::push_to_talk;
use crate::session_events::SessionEvent;
use crate::settings::{LaunchState, Settings};
use crate::store::StateStore;
use anyhow::{Context, Result};
use log::{error, info, trace, warn};
use serde::{Deserialize, Serialize};

pub const MUTE_STATE_FILE_NAME: &str = "mute-state.json";

/// Why a launch mutes, or `None` when it leaves the microphones as they are. `last_muted` is
/// the state the user last chose, if known.
pub fn launch_mute_reason(settings: &Settings, last_muted: Option<bool>) -> Option<&'static str> {
    if push_to_talk::mutes_at_launch(settings.mode) {
        return Some("the shortcut is in push_to_talk mode");
    }
    match settings.launch_state {
        LaunchState::Keep => None,
        LaunchState::Muted => Some("launch_state is \"muted\""),
        LaunchState::Last if last_muted == Some(true) => {
            Some("launch_state is \"last\", and the microphones were muted at the last quit")
        }
        LaunchState::Last => None,
    }
}

/// The setting that makes `event` mute in toggle mode, if it is on. (In push-to-talk mode
/// every session event mutes: `PushToTalk::interrupt`.)
pub fn trigger_for(event: SessionEvent, settings: &Settings) -> Option<&'static str> {
    let (name, is_on) = match event {
        SessionEvent::ScreenLocked | SessionEvent::SessionResignedActive => {
            ("mute_on_lock", settings.mute_on_lock)
        }
        SessionEvent::WillSleep => ("mute_on_sleep", settings.mute_on_sleep),
        SessionEvent::ScreensDidSleep => ("mute_on_display_sleep", settings.mute_on_display_sleep),
        // Not a lock: the screen saver can end without a password.
        SessionEvent::ScreenSaverStarted => return None,
    };
    is_on.then_some(name)
}

#[derive(Debug, Serialize, Deserialize)]
struct MuteStateFile {
    muted: bool,
}

/// The mute state the user last chose, mirrored to `mute-state.json` next to settings.json.
/// It lives in its own file so the app never rewrites the user's settings to track state.
pub struct LastMuteState {
    store: Box<dyn StateStore>,
    /// What the file holds; `None` while it is missing or unreadable.
    saved: Option<bool>,
    save_failing: bool,
}

impl LastMuteState {
    /// Reads the file. A missing or unreadable file leaves the state unknown.
    pub fn load(store: Box<dyn StateStore>) -> Self {
        let saved = match store.load() {
            Ok(None) => None,
            Ok(Some(text)) => match serde_json::from_str::<MuteStateFile>(&text) {
                Ok(file) => Some(file.muted),
                Err(err) => {
                    warn!("Ignoring unreadable {}: {}", MUTE_STATE_FILE_NAME, err);
                    None
                }
            },
            Err(err) => {
                warn!("Failed to read {}: {:#}", MUTE_STATE_FILE_NAME, err);
                None
            }
        };
        Self {
            store,
            saved,
            save_failing: false,
        }
    }

    /// Whether the user last chose mute; `None` when unknown.
    pub fn muted(&self) -> Option<bool> {
        self.saved
    }

    /// Records the user's choice, writing the file only when it changes. A failed write is
    /// logged once until a write succeeds, and retried at the next call.
    pub fn record(&mut self, muted: bool) {
        if self.saved == Some(muted) {
            return;
        }
        match self.write(muted) {
            Ok(()) => {
                self.saved = Some(muted);
                trace!("Recorded muted={} in {}", muted, MUTE_STATE_FILE_NAME);
                if self.save_failing {
                    info!("Saved {} again", MUTE_STATE_FILE_NAME);
                    self.save_failing = false;
                }
            }
            Err(err) => {
                if self.save_failing {
                    trace!("Still failing to save {}: {:#}", MUTE_STATE_FILE_NAME, err);
                } else {
                    error!("Failed to save {}: {:#}", MUTE_STATE_FILE_NAME, err);
                    self.save_failing = true;
                }
            }
        }
    }

    fn write(&self, muted: bool) -> Result<()> {
        let mut text = serde_json::to_string_pretty(&MuteStateFile { muted })
            .with_context(|| format!("Failed to serialize {}", MUTE_STATE_FILE_NAME))?;
        text.push('\n');
        self.store.save(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutMode;
    use crate::store::MemoryStore;

    fn settings(launch_state: LaunchState) -> Settings {
        Settings {
            launch_state,
            ..Settings::default()
        }
    }

    #[test]
    fn keep_never_mutes_at_launch() {
        for last_muted in [None, Some(false), Some(true)] {
            let reason = launch_mute_reason(&settings(LaunchState::Keep), last_muted);
            assert_eq!(reason, None, "{last_muted:?}");
        }
    }

    #[test]
    fn muted_always_mutes_at_launch() {
        for last_muted in [None, Some(false), Some(true)] {
            let reason = launch_mute_reason(&settings(LaunchState::Muted), last_muted);
            assert!(reason.is_some(), "{last_muted:?}");
        }
    }

    #[test]
    fn last_mutes_only_after_a_muted_quit() {
        let last = settings(LaunchState::Last);
        assert!(launch_mute_reason(&last, Some(true)).is_some());
        assert_eq!(
            launch_mute_reason(&last, Some(false)),
            None,
            "never unmutes"
        );
        assert_eq!(launch_mute_reason(&last, None), None);
    }

    #[test]
    fn push_to_talk_mutes_at_launch_whatever_the_launch_state() {
        for launch_state in [LaunchState::Keep, LaunchState::Muted, LaunchState::Last] {
            let ptt = Settings {
                mode: ShortcutMode::PushToTalk,
                launch_state,
                ..Settings::default()
            };
            let reason = launch_mute_reason(&ptt, Some(false));
            assert_eq!(reason, Some("the shortcut is in push_to_talk mode"));
        }
    }

    #[test]
    fn no_session_event_mutes_by_default() {
        for event in SessionEvent::ALL {
            assert_eq!(trigger_for(event, &Settings::default()), None, "{event:?}");
        }
    }

    #[test]
    fn each_trigger_setting_mutes_on_its_events() {
        let all_on = Settings {
            mute_on_lock: true,
            mute_on_sleep: true,
            mute_on_display_sleep: true,
            ..Settings::default()
        };
        let expected = [
            (SessionEvent::ScreenLocked, Some("mute_on_lock")),
            (SessionEvent::SessionResignedActive, Some("mute_on_lock")),
            (SessionEvent::WillSleep, Some("mute_on_sleep")),
            (SessionEvent::ScreensDidSleep, Some("mute_on_display_sleep")),
            (SessionEvent::ScreenSaverStarted, None),
        ];
        assert_eq!(expected.len(), SessionEvent::ALL.len());
        for (event, setting) in expected {
            assert_eq!(trigger_for(event, &all_on), setting, "{event:?}");
        }
    }

    #[test]
    fn a_trigger_setting_covers_only_its_own_events() {
        let sleep_only = Settings {
            mute_on_sleep: true,
            ..Settings::default()
        };
        for event in SessionEvent::ALL {
            let expected = (event == SessionEvent::WillSleep).then_some("mute_on_sleep");
            assert_eq!(trigger_for(event, &sleep_only), expected, "{event:?}");
        }
    }

    #[test]
    fn a_missing_or_unreadable_file_is_unknown() {
        assert_eq!(
            LastMuteState::load(MemoryStore::default().boxed()).muted(),
            None
        );
        let corrupt = MemoryStore::with_contents("{ muted: yes");
        assert_eq!(LastMuteState::load(corrupt.boxed()).muted(), None);
    }

    #[test]
    fn the_file_holds_the_last_choice() {
        let store = MemoryStore::with_contents("{ \"muted\": true }");
        assert_eq!(LastMuteState::load(store.boxed()).muted(), Some(true));
    }

    #[test]
    fn recording_writes_only_changes() {
        let store = MemoryStore::default();
        let mut last = LastMuteState::load(store.boxed());

        last.record(true);
        last.record(true);
        assert_eq!(store.saves(), 1);
        last.record(false);
        assert_eq!(store.saves(), 2);

        assert_eq!(last.muted(), Some(false));
        let reloaded = LastMuteState::load(store.boxed());
        assert_eq!(reloaded.muted(), Some(false));
    }

    #[test]
    fn recording_the_saved_state_writes_nothing() {
        let store = MemoryStore::with_contents("{ \"muted\": true }");
        let mut last = LastMuteState::load(store.boxed());
        last.record(true);
        assert_eq!(store.saves(), 0);
    }

    #[test]
    fn recording_replaces_an_unreadable_file() {
        let store = MemoryStore::with_contents("not json");
        let mut last = LastMuteState::load(store.boxed());
        last.record(false);
        assert_eq!(LastMuteState::load(store.boxed()).muted(), Some(false));
    }

    #[test]
    fn a_failed_write_is_retried_at_the_next_record() {
        let store = MemoryStore::with_contents("{ \"muted\": false }");
        let mut last = LastMuteState::load(store.boxed());
        store.set_fail_saves(true);

        last.record(true);
        assert_eq!(
            last.muted(),
            Some(false),
            "the file still holds the old state"
        );

        store.set_fail_saves(false);
        last.record(true);
        assert_eq!(store.saves(), 1);
        assert_eq!(LastMuteState::load(store.boxed()).muted(), Some(true));
    }
}
