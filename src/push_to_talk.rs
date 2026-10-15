//! What the mute shortcut does in each `mode`. In `toggle`, each press mutes or unmutes. In
//! `push_to_talk`, holding the shortcut unmutes and releasing it mutes again: the resting state
//! is muted, so a launch and a switch into the mode mute.
//!
//! A release can get lost. The screen can lock or the Mac sleep while the key is held, and once
//! the hotkey is re-registered its release carries the old hotkey's id. Each of those ends the
//! hold with a mute (`interrupt`, `on_settings_change`), so a lost release never leaves a
//! microphone open.

use crate::mic_command::MicCommand;
use crate::settings::ShortcutMode;
use global_hotkey::HotKeyState;

/// Whether a launch in `mode` mutes: push-to-talk starts in its resting state.
pub fn mutes_at_launch(mode: ShortcutMode) -> bool {
    mode == ShortcutMode::PushToTalk
}

/// The shortcut's hold in `push_to_talk` mode.
#[derive(Debug, Default)]
pub struct PushToTalk {
    /// Whether a press unmuted and its release hasn't arrived yet.
    is_held: bool,
}

impl PushToTalk {
    /// What a press or release of the shortcut does in `mode`.
    pub fn on_key(&mut self, mode: ShortcutMode, state: HotKeyState) -> Option<MicCommand> {
        match (mode, state) {
            (ShortcutMode::Toggle, HotKeyState::Pressed) => Some(MicCommand::Toggle),
            (ShortcutMode::Toggle, HotKeyState::Released) => None,
            // A press repeated while held, such as a media key's auto-repeat, changes nothing.
            (ShortcutMode::PushToTalk, HotKeyState::Pressed) if self.is_held => None,
            (ShortcutMode::PushToTalk, HotKeyState::Pressed) => {
                self.is_held = true;
                Some(MicCommand::Unmute)
            }
            // Releasing mutes even without a press first: muting in the resting state is
            // harmless, and it closes a hold whose press came before a mode switch.
            (ShortcutMode::PushToTalk, HotKeyState::Released) => {
                self.is_held = false;
                Some(MicCommand::Mute)
            }
        }
    }

    /// A session event that can swallow the release (the screen locked, the Mac or its
    /// displays slept, another user took over) ends the hold. In `push_to_talk` mode it mutes,
    /// held or not: that's the resting state, also after the tray or a URL unmuted.
    pub fn interrupt(&mut self, mode: ShortcutMode) -> Option<MicCommand> {
        self.is_held = false;
        (mode == ShortcutMode::PushToTalk).then_some(MicCommand::Mute)
    }

    /// Settings were applied: the mode went from `old` to `new`, and `is_hotkey_replaced` says
    /// whether the mute hotkey was re-registered. Mutes when entering `push_to_talk`, and when
    /// a hold ends without its release: on leaving the mode, or once the release would carry
    /// the old hotkey's id.
    pub fn on_settings_change(
        &mut self,
        old: ShortcutMode,
        new: ShortcutMode,
        is_hotkey_replaced: bool,
    ) -> Option<MicCommand> {
        if old == new && !is_hotkey_replaced {
            return None;
        }
        let was_held = std::mem::take(&mut self.is_held);
        let enters_push_to_talk = old != new && new == ShortcutMode::PushToTalk;
        (enters_push_to_talk || was_held).then_some(MicCommand::Mute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HotKeyState::{Pressed, Released};
    use ShortcutMode::{PushToTalk as Ptt, Toggle};

    fn keys(
        hold: &mut PushToTalk,
        mode: ShortcutMode,
        states: &[HotKeyState],
    ) -> Vec<Option<MicCommand>> {
        states
            .iter()
            .map(|state| hold.on_key(mode, *state))
            .collect()
    }

    #[test]
    fn toggle_mode_toggles_on_each_press_and_ignores_releases() {
        let mut hold = PushToTalk::default();
        let commands = keys(&mut hold, Toggle, &[Pressed, Released, Pressed, Released]);
        assert_eq!(
            commands,
            [
                Some(MicCommand::Toggle),
                None,
                Some(MicCommand::Toggle),
                None
            ]
        );
    }

    #[test]
    fn holding_unmutes_and_releasing_mutes() {
        let mut hold = PushToTalk::default();
        let commands = keys(&mut hold, Ptt, &[Pressed, Released, Pressed, Released]);
        assert_eq!(
            commands,
            [
                Some(MicCommand::Unmute),
                Some(MicCommand::Mute),
                Some(MicCommand::Unmute),
                Some(MicCommand::Mute)
            ]
        );
    }

    #[test]
    fn a_press_repeated_while_held_changes_nothing() {
        let mut hold = PushToTalk::default();
        let commands = keys(&mut hold, Ptt, &[Pressed, Pressed, Pressed, Released]);
        assert_eq!(
            commands,
            [Some(MicCommand::Unmute), None, None, Some(MicCommand::Mute)]
        );
    }

    #[test]
    fn a_release_without_a_press_mutes() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));
    }

    #[test]
    fn an_interruption_mutes_and_the_next_press_unmutes_again() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));

        // The screen locked; the release never arrives.
        assert_eq!(hold.interrupt(Ptt), Some(MicCommand::Mute));

        assert_eq!(
            hold.on_key(Ptt, Pressed),
            Some(MicCommand::Unmute),
            "not stuck held"
        );
        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
    }

    #[test]
    fn an_interruption_mutes_in_push_to_talk_mode_even_without_a_hold() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.interrupt(Ptt), Some(MicCommand::Mute));
    }

    #[test]
    fn an_interruption_in_toggle_mode_changes_nothing() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Toggle, Pressed), Some(MicCommand::Toggle));
        assert_eq!(hold.interrupt(Toggle), None);
    }

    #[test]
    fn switching_to_push_to_talk_mutes() {
        let mut hold = PushToTalk::default();
        assert_eq!(
            hold.on_settings_change(Toggle, Ptt, false),
            Some(MicCommand::Mute)
        );
    }

    #[test]
    fn switching_to_push_to_talk_while_the_key_is_down_mutes_and_the_release_mutes_again() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Toggle, Pressed), Some(MicCommand::Toggle));

        assert_eq!(
            hold.on_settings_change(Toggle, Ptt, false),
            Some(MicCommand::Mute)
        );

        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));
    }

    #[test]
    fn switching_to_toggle_while_held_mutes_and_ignores_the_release() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));

        assert_eq!(
            hold.on_settings_change(Ptt, Toggle, false),
            Some(MicCommand::Mute)
        );

        assert_eq!(hold.on_key(Toggle, Released), None);
        assert_eq!(hold.on_key(Toggle, Pressed), Some(MicCommand::Toggle));
    }

    #[test]
    fn switching_to_toggle_without_a_hold_keeps_the_state() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));
        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
        assert_eq!(hold.on_settings_change(Ptt, Toggle, false), None);
    }

    #[test]
    fn a_replaced_hotkey_ends_the_hold_with_a_mute() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));

        assert_eq!(
            hold.on_settings_change(Ptt, Ptt, true),
            Some(MicCommand::Mute)
        );

        assert_eq!(
            hold.on_key(Ptt, Pressed),
            Some(MicCommand::Unmute),
            "not stuck held"
        );
    }

    #[test]
    fn a_replaced_hotkey_without_a_hold_changes_nothing() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_settings_change(Ptt, Ptt, true), None);
        assert_eq!(hold.on_settings_change(Toggle, Toggle, true), None);
    }

    #[test]
    fn other_settings_changes_keep_the_hold() {
        let mut hold = PushToTalk::default();
        assert_eq!(hold.on_key(Ptt, Pressed), Some(MicCommand::Unmute));

        assert_eq!(hold.on_settings_change(Ptt, Ptt, false), None);

        assert_eq!(hold.on_key(Ptt, Pressed), None, "still held");
        assert_eq!(hold.on_key(Ptt, Released), Some(MicCommand::Mute));
    }

    #[test]
    fn only_push_to_talk_mutes_at_launch() {
        assert!(mutes_at_launch(Ptt));
        assert!(!mutes_at_launch(Toggle));
    }
}
