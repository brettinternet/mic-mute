use crate::config::AppVars;
use crate::event_loop::{create, EventIds, EventLoopMessage};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use crate::popup::Popup;
use crate::settings::Settings;
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
    pub fn new(
        mic_muted: bool,
        camera_muted: bool,
        app_vars: AppVars,
        settings: &Settings,
    ) -> Result<(Self, EventLoopMessage, EventIds)> {
        let event_loop = create();
        let popup = Popup::new(&event_loop, mic_muted).context("Failed to setup popup window")?;
        let theme = popup.get_theme();
        let tray = Tray::new(
            mic_muted,
            theme,
            app_vars,
            settings.launch_at_login,
            settings.show_in_dock,
            &settings.mic_shortcut,
        )
        .context("Failed to create system tray")?;
        let shortcuts =
            Shortcuts::new(settings).context("Failed to setup shortcuts")?;

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute_id().clone(),
            button_launch_at_login: tray.launch_at_login_id().clone(),
            button_show_in_dock: tray.show_in_dock_id().clone(),
            button_about: tray.about_id().clone(),
            button_quit: tray.quit_id().clone(),
            shortcut_mic: Arc::new(AtomicU32::new(shortcuts.mic_hotkey.id())),
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

    pub fn update_mic(&mut self, muted: bool, active_device_name: Option<&str>) -> Result<&mut Self> {
        trace!("Updating UI mic state {}", muted);
        self.mic_muted = muted;
        self.tray
            .update(muted, self.popup.get_theme())
            .context("Failed to update UI tray")?;
        self.popup
            .update_with_camera(muted, self.camera_muted, active_device_name)
            .context("Failed to update UI popup")?;
        Ok(self)
    }

    pub fn update_camera(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI camera state {}", muted);
        self.camera_muted = muted;
        self.popup
            .update_with_camera(self.mic_muted, muted, None)
            .context("Failed to update UI popup for camera")?;
        Ok(self)
    }

    pub fn hide_popup(&mut self) -> Result<&mut Self> {
        self.popup.hide().context("Failed to hide UI popup")?;
        Ok(self)
    }

    /// Apply all settings to the live app state.
    /// Safe to call whenever settings change — all operations are idempotent.
    pub fn apply_settings(&mut self, settings: &Settings) -> Result<()> {
        // Re-register hotkeys and update tray accelerator labels
        self.shortcuts.reload(settings)?;
        self.tray
            .update_accelerators(&settings.mic_shortcut)
            .context("Failed to update tray accelerators")?;

        // Sync dock visibility and its tray checkbox
        self.tray.show_in_dock.set_checked(settings.show_in_dock);
        crate::launch_at_login::set_dock_visible(settings.show_in_dock);

        // Sync launch-at-login plist and its tray checkbox
        self.tray.launch_at_login.set_checked(settings.launch_at_login);
        if let Err(e) = crate::launch_at_login::set(settings.launch_at_login) {
            log::error!("Failed to apply launch_at_login setting: {}", e);
        }

        Ok(())
    }

    pub fn mic_shortcut_id(&self) -> u32 {
        self.shortcuts.mic_hotkey.id()
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
