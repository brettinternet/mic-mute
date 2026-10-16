use crate::config::AppVars;
use crate::event_loop::{EventIds, EventLoopTarget};
use crate::feedback::{Feedback, MicUpdate};
use crate::mic::DeviceState;
use crate::popup::{Popup, PopupLayout};
use crate::settings::{ExcludedDevice, Settings};
use crate::settings_window::SettingsChange;
use crate::shortcuts::Shortcuts;
use crate::tray::Tray;
use anyhow::{Context, Result};
use log::trace;
use muda::MenuId;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The popup's position and size from `settings`.
fn popup_layout(settings: &Settings) -> PopupLayout {
    PopupLayout {
        position: settings.popup_position.value(),
        size: settings.popup_size.value(),
    }
}

/// Logs each feedback setting whose value in the file isn't used as written.
fn log_feedback_problems(settings: &Settings) {
    for problem in settings.feedback_problems() {
        log::warn!("settings.json: {}", problem);
    }
}

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
    /// The sound and notification on a mute change.
    feedback: Feedback,
    /// How long the popup shows "Mic on" after the user unmutes (`popup_mic_on_ms`).
    mic_on_flash: Duration,
    /// The `launch_at_login` value macOS last took, or that startup mirrored from it; `None`
    /// after a failed push (`launch_at_login::should_push`).
    launch_at_login_applied: Option<bool>,
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
        log_feedback_problems(settings);
        let popup = Popup::new(
            target,
            mic_muted,
            settings.show_popup,
            popup_layout(settings),
        )
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
            button_settings_window: tray.settings_window_id().clone(),
            button_about: tray.about_id().clone(),
            button_quit: tray.quit_id().clone(),
            button_open_sound_settings: tray.open_sound_settings_id().clone(),
            shortcut_mic: shortcut_mic.clone(),
        };

        let mut ui = Self {
            tray,
            popup,
            shortcuts,
            shortcut_mic,
            mic_muted,
            camera_muted,
            feedback: Feedback::new(settings.play_sound, settings.show_notification),
            mic_on_flash: settings.popup_mic_on_ms.value().duration(),
            // main() mirrored the login item into the setting before the loop started.
            launch_at_login_applied: Some(settings.launch_at_login),
        };
        ui.show_launch_at_login(settings.launch_at_login);
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

    /// Ends a popup flash: hides the popup unless it stays up while muted.
    pub fn hide_popup_unless_pinned(&mut self) -> Result<&mut Self> {
        self.popup
            .hide_unless_pinned()
            .context("Failed to hide UI popup")?;
        Ok(self)
    }

    /// Shows the popup with the current state, also while `show_popup` is off, until
    /// `hide_popup_unless_pinned`.
    pub fn show_popup_status(&mut self) -> Result<&mut Self> {
        self.popup
            .show_status()
            .context("Failed to show the status popup")?;
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

        // Before the steps that can return early: it logs its own errors.
        self.apply_feedback(settings);

        // Sync popup visibility with the persisted setting
        self.set_popup_enabled(settings.show_popup)?;

        // Sync dock visibility and its tray checkbox
        self.tray.show_in_dock.set_checked(settings.show_in_dock);
        crate::launch_at_login::set_dock_visible(settings.show_in_dock);

        self.set_launch_at_login(settings.launch_at_login);

        Ok(())
    }

    /// Applies one setting the Settings window changed; `settings` already holds it. Only the
    /// part of the app that uses the setting is touched: unlike `apply_settings`, a change to
    /// another setting doesn't push `launch_at_login` to macOS again.
    pub fn apply_window_change(
        &mut self,
        change: &SettingsChange,
        settings: &Settings,
    ) -> Result<()> {
        match change {
            SettingsChange::Shortcut(config) => {
                // An invalid or unregistrable shortcut keeps the registered one, and the
                // shortcut's status says why; the tray shows whichever is registered.
                let reloaded = self.shortcuts.reload(config);
                self.sync_shortcut();
                reloaded?;
            }
            SettingsChange::LaunchAtLogin(enabled) => self.set_launch_at_login(*enabled),
            SettingsChange::ShowInDock(visible) => {
                self.tray.show_in_dock.set_checked(*visible);
                crate::launch_at_login::set_dock_visible(*visible);
            }
            SettingsChange::ShowPopup(enabled) => self.set_popup_enabled(*enabled)?,
            SettingsChange::PopupPosition(_)
            | SettingsChange::PopupSize(_)
            | SettingsChange::PopupMicOnMs(_)
            | SettingsChange::PlaySound(_)
            | SettingsChange::ShowNotification(_) => self.apply_feedback(settings),
            // Read where they're used: the event loop reads these from `settings`.
            SettingsChange::Mode(_)
            | SettingsChange::LaunchState(_)
            | SettingsChange::MuteOnLock(_)
            | SettingsChange::MuteOnSleep(_)
            | SettingsChange::MuteOnDisplaySleep(_)
            | SettingsChange::AllowUrlUnmute(_) => {}
        }
        Ok(())
    }

    /// Applies the sound, the notification, and the popup's position, size, and "Mic on" flash,
    /// logging the values the file sets that aren't used as written.
    fn apply_feedback(&mut self, settings: &Settings) {
        log_feedback_problems(settings);
        self.feedback
            .set_options(settings.play_sound, settings.show_notification);
        self.mic_on_flash = settings.popup_mic_on_ms.value().duration();
        if let Err(e) = self.popup.set_layout(popup_layout(settings)) {
            log::error!("Failed to apply the popup's position and size: {:#}", e);
        }
    }

    /// Plays the sound and posts the notification the settings ask for after `update` took the
    /// microphones from `was_muted` to `muted`: only a request that changed the state gives
    /// either (`feedback::change_to_report`).
    pub fn give_feedback(&mut self, update: MicUpdate, was_muted: bool, muted: bool) {
        self.feedback.on_mic_update(update, was_muted, muted);
    }

    /// How long the popup stays up showing "Mic on" after the user unmutes.
    pub fn mic_on_flash(&self) -> Duration {
        self.mic_on_flash
    }

    /// Registers or removes the login item (a LaunchAgent on macOS 11 and 12) when `enabled`
    /// differs from what macOS last took or startup mirrored from it, then shows the result on
    /// the tray's Launch at Login item. `apply_settings` runs this after every settings change,
    /// so an unchanged value leaves macOS alone: pushing it again would register a login item
    /// the user removed in System Settings. A tray click flips the value, so it pushes unless
    /// macOS already has the new one.
    pub fn set_launch_at_login(&mut self, enabled: bool) {
        if crate::launch_at_login::should_push(enabled, self.launch_at_login_applied) {
            self.launch_at_login_applied = match crate::launch_at_login::set(enabled) {
                Ok(()) => Some(enabled),
                Err(e) => {
                    log::error!("Failed to apply launch_at_login setting: {:#}", e);
                    None
                }
            };
        }
        self.show_launch_at_login(enabled);
    }

    /// Checks the tray item for `enabled` and, while macOS waits for the user to approve the
    /// login item, says where to approve it.
    fn show_launch_at_login(&self, enabled: bool) {
        let needs_approval = enabled && crate::launch_at_login::needs_approval();
        let item = &self.tray.launch_at_login;
        item.set_checked(enabled);
        item.set_text(crate::launch_at_login::menu_title(needs_approval));
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

    /// Unregisters the mute hotkey while the Settings window records a new shortcut, so the
    /// recorder hears its keys; `resume_shortcut` registers it again.
    pub fn suspend_shortcut(&mut self) -> Result<()> {
        self.shortcuts.suspend()
    }

    /// Registers the hotkey `suspend_shortcut` unregistered again.
    pub fn resume_shortcut(&mut self) -> Result<()> {
        let resumed = self.shortcuts.resume();
        self.sync_shortcut();
        resumed
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

    /// Shows each input device's state, and the `excluded` devices that aren't connected, in
    /// the tray's Microphones menu; only rows that change are touched.
    pub fn update_devices(
        &mut self,
        states: &[DeviceState],
        excluded: &[ExcludedDevice],
    ) -> Result<()> {
        self.tray
            .update_devices(states, excluded)
            .context("Failed to update the Microphones menu")
    }

    /// The device a click on the Microphones menu item `id` excludes or includes, if any.
    pub fn clicked_device(&self, id: &MenuId) -> Option<ExcludedDevice> {
        self.tray.clicked_device(id)
    }
}
