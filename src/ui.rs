use crate::config::AppVars;
use crate::event_loop::{EventIds, EventLoopTarget};
use crate::popup::Popup;
use crate::settings::Settings;
use crate::shortcuts::Shortcuts;
use crate::tray::Tray;
use anyhow::{Context, Result};
use log::trace;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Event loop must remain on the main thread and doesn't implement Copy
#[allow(dead_code)]
pub struct UI {
    tray: Tray,
    popup: Popup,
    shortcuts: Shortcuts,
    /// The registered mute hotkey's id, shared with the event loop as
    /// `EventIds::shortcut_mic`.
    shortcut_mic: Arc<AtomicU32>,
    mic_muted: bool,
    camera_muted: bool,
}

impl UI {
    /// Creates the popup, the hotkey manager, and the tray icon on `target`. Call it only from
    /// the running loop, on `StartCause::Init` or later: on macOS, tray-icon needs the loop
    /// running, not just created.
    pub fn new(
        target: &EventLoopTarget,
        mic_muted: bool,
        camera_muted: bool,
        app_vars: AppVars,
        settings: &Settings,
    ) -> Result<(Self, EventIds)> {
        let popup = Popup::new(target, mic_muted, settings.show_popup)
            .context("Failed to setup popup window")?;
        let shortcuts =
            Shortcuts::new(&settings.mic_shortcut).context("Failed to setup shortcuts")?;
        let tray = Tray::new(
            mic_muted,
            !camera_muted,
            app_vars,
            settings.launch_at_login,
            settings.show_in_dock,
            settings.show_popup,
        )
        .context("Failed to create system tray")?;
        let shortcut_mic = Arc::new(AtomicU32::new(shortcuts.registered_id()));

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute_id().clone(),
            button_launch_at_login: tray.launch_at_login_id().clone(),
            button_show_in_dock: tray.show_in_dock_id().clone(),
            button_show_popup: tray.show_popup_id().clone(),
            button_about: tray.about_id().clone(),
            button_quit: tray.quit_id().clone(),
            shortcut_mic: shortcut_mic.clone(),
        };

        let mut ui = Self {
            tray,
            popup,
            shortcuts,
            shortcut_mic,
            mic_muted,
            camera_muted,
        };
        ui.sync_shortcut();
        Ok((ui, event_ids))
    }

    pub fn update_mic(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI mic state {}", muted);
        self.mic_muted = muted;
        self.tray
            .update(muted, !self.camera_muted)
            .context("Failed to update UI tray")?;
        self.popup
            .update_with_camera(muted, self.camera_muted)
            .context("Failed to update UI popup")?;
        Ok(self)
    }

    /// `muted` is `CameraController::muted`: `true` while no camera is in use.
    pub fn update_camera(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI camera state {}", muted);
        self.camera_muted = muted;
        self.tray
            .update(self.mic_muted, !muted)
            .context("Failed to update UI tray for camera")?;
        self.popup
            .update_with_camera(self.mic_muted, muted)
            .context("Failed to update UI popup for camera")?;
        Ok(self)
    }

    pub fn hide_popup(&mut self) -> Result<&mut Self> {
        self.popup.hide().context("Failed to hide UI popup")?;
        Ok(self)
    }

    pub fn set_popup_enabled(&mut self, enabled: bool) -> Result<()> {
        self.popup
            .set_enabled(enabled)
            .context("Failed to apply popup setting")?;
        self.tray.show_popup.set_checked(enabled);
        Ok(())
    }

    /// Apply all settings to the live app state.
    /// Safe to call whenever settings change — all operations are idempotent.
    pub fn apply_settings(&mut self, settings: &Settings) -> Result<()> {
        // An invalid or unregistrable shortcut keeps the registered one; log it and
        // still apply the other settings.
        if let Err(e) = self.shortcuts.reload(&settings.mic_shortcut) {
            log::error!("Failed to apply mic_shortcut setting: {:#}", e);
        }
        self.sync_shortcut();

        // Sync popup visibility with the persisted setting
        self.set_popup_enabled(settings.show_popup)?;

        // Sync dock visibility and its tray checkbox
        self.tray.show_in_dock.set_checked(settings.show_in_dock);
        crate::launch_at_login::set_dock_visible(settings.show_in_dock);

        // Sync launch-at-login plist and its tray checkbox
        self.tray
            .launch_at_login
            .set_checked(settings.launch_at_login);
        if let Err(e) = crate::launch_at_login::set(settings.launch_at_login) {
            log::error!("Failed to apply launch_at_login setting: {}", e);
        }

        Ok(())
    }

    /// Hands the registered hotkey's id to the event loop and shows the shortcut in the
    /// tray menu. Publishing the id here, not through the caller, keeps the hotkey
    /// working even when a later step of `apply_settings` fails.
    fn sync_shortcut(&mut self) {
        let registered = self.shortcuts.registered();
        self.shortcut_mic
            .store(self.shortcuts.registered_id(), Ordering::Relaxed);
        if let Err(e) = self.tray.update_accelerators(registered) {
            log::warn!("Failed to update the tray menu's mute shortcut: {:#}", e);
        }
    }

    /// The registered mute hotkey's id; 0 when no hotkey is registered.
    pub fn mic_shortcut_id(&self) -> u32 {
        self.shortcuts.registered_id()
    }

    pub fn detect(&mut self) -> Result<&mut Self> {
        self.popup
            .detect_cursor_monitor()
            .context("Failed to update UI popup placement")?;
        Ok(self)
    }

    /// Shows `notice` in the tray menu and tooltip while settings.json is unusable; `None`
    /// removes it. Unlike a dialog, this never pauses mute enforcement.
    pub fn set_settings_notice(&mut self, notice: Option<&str>) -> Result<()> {
        self.tray
            .set_settings_notice(notice)
            .context("Failed to update settings notice")
    }
}
