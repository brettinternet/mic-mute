use crate::config::AppVars;
use crate::event_loop::{create, EventIds, EventLoopMessage};
use crate::popup::Popup;
use crate::shortcuts::Shortcuts;
use crate::tray::Tray;
use anyhow::{Context, Result};
use log::trace;

/// Event loop must remain on the main thread and doesn't implement Copy
#[allow(dead_code)]
pub struct UI {
    tray: Tray,
    popup: Popup,
    shortcuts: Shortcuts,
    mic_muted: bool,
    camera_muted: bool,
}

unsafe impl Send for UI {}
unsafe impl Sync for UI {}

impl UI {
    pub fn new(mic_muted: bool, camera_muted: bool, app_vars: AppVars) -> Result<(Self, EventLoopMessage, EventIds)> {
        let event_loop = create();
        let popup = Popup::new(&event_loop, mic_muted).context("Failed to setup popup window")?;
        let theme = popup.get_theme();
        let tray = Tray::new(mic_muted, theme, app_vars).context("Failed to create system tray")?;
        let shortcuts = Shortcuts::new().context("Failed to setup shortcuts")?;

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute_id().clone(),
            button_toggle_camera: tray.toggle_camera_id().clone(),
            button_quit: tray.quit_id().clone(),
            shortcut_shift_meta_a: shortcuts.shift_meta_a.id(),
            shortcut_shift_meta_v: shortcuts.shift_meta_v.id(),
        };

        let ui = Self {
            tray,
            popup,
            shortcuts,
            mic_muted,
            camera_muted,
        };
        Ok((ui, event_loop, event_ids))
    }

    pub fn update_mic(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI mic state {}", muted);
        self.mic_muted = muted;
        self.tray
            .update(muted, self.popup.get_theme())
            .context("Failed to update UI tray")?;
        self.popup
            .update_with_camera(muted, self.camera_muted)
            .context("Failed to update UI popup")?;
        Ok(self)
    }

    pub fn update_camera(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI camera state {}", muted);
        self.camera_muted = muted;
        self.popup
            .update_with_camera(self.mic_muted, muted)
            .context("Failed to update UI popup for camera")?;
        Ok(self)
    }

    /// Legacy update method (mic only)
    pub fn update(&mut self, muted: bool) -> Result<&mut Self> {
        self.update_mic(muted)
    }

    pub fn hide_popup(&mut self) -> Result<&mut Self> {
        self.popup.hide().context("Failed to hide UI popup")?;
        Ok(self)
    }

    pub fn detect(&mut self) -> Result<&mut Self> {
        self.popup
            .detect_cursor_monitor()
            .context("Failed to detect UI cursor monitor")?;
        if self.popup.cursor_on_separate_monitor {
            self.popup
                .update_placement()
                .context("Failed to update UI popup placement")?;
        }
        Ok(self)
    }
}
