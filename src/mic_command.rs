//! A change to the microphones that the user asks for, from the tray, the shortcut, or a URL,
//! or that a session event calls for. The event loop runs it through `MicController::toggle`.

/// What to do with the user's mute intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicCommand {
    /// Unmute when muted, mute otherwise.
    Toggle,
    Mute,
    Unmute,
}

impl MicCommand {
    /// The state `MicController::toggle` takes, where `None` toggles.
    pub fn mute_state(self) -> Option<bool> {
        match self {
            Self::Toggle => None,
            Self::Mute => Some(true),
            Self::Unmute => Some(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_map_to_the_controllers_toggle_state() {
        assert_eq!(MicCommand::Toggle.mute_state(), None);
        assert_eq!(MicCommand::Mute.mute_state(), Some(true));
        assert_eq!(MicCommand::Unmute.mute_state(), Some(false));
    }
}
