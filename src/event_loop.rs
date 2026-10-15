use crate::about::{self, AboutWindow};
use crate::camera::CameraController;
use crate::config::AppVars;
use crate::launch_at_login;
use crate::mic::{FailureLog, MicController};
use crate::settings::Settings;
use crate::ui::UI;
use crate::utils;
use anyhow::Result;
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use muda::{MenuEvent, MenuId};
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopWindowTarget};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

const POLL_INTERVAL_MILLIS: u64 = 200;
/// How long the popup stays up after an unmute.
const HIDE_POPUP_DELAY: Duration = Duration::from_secs(1);
/// The exit code when the tray, popup, or hotkey can't be created.
const UI_FAILURE_EXIT_CODE: i32 = 1;

#[derive(Debug)]
pub enum Message {
    CameraStateChanged(bool),
}

pub type EventLoopMessage = EventLoop<Message>;
/// What the running loop hands its closure in place of the `EventLoop`; windows are built on it.
pub type EventLoopTarget = EventLoopWindowTarget<Message>;

pub fn create() -> EventLoopMessage {
    EventLoopBuilder::<Message>::with_user_event().build()
}

/// Whether `event` starts the running loop: the earliest point at which tray-icon may create
/// the tray icon on macOS. Created before, a fullscreen app can show the icon twice.
fn is_loop_start(event: &Event<Message>) -> bool {
    matches!(event, Event::NewEvents(StartCause::Init))
}

/// Creates the popup, the hotkey, and the tray icon from the current state. Call it only once
/// the loop runs (`is_loop_start`).
fn create_ui(
    target: &EventLoopTarget,
    controller: &RwLock<MicController>,
    camera: &RwLock<CameraController>,
    settings: &RwLock<Settings>,
) -> Result<(RwLock<UI>, EventIds)> {
    let mic_muted = controller.read().muted;
    let camera_muted = camera.read().muted;
    let (ui, event_ids) = UI::new(
        target,
        mic_muted,
        camera_muted,
        AppVars::new(),
        &settings.read(),
    )?;
    trace!("UI initialized");
    Ok((RwLock::new(ui), event_ids))
}

pub struct EventIds {
    pub button_toggle_mute: MenuId,
    pub button_launch_at_login: MenuId,
    pub button_show_in_dock: MenuId,
    pub button_show_popup: MenuId,
    pub button_about: MenuId,
    pub button_quit: MenuId,
    pub button_open_sound_settings: MenuId,
    pub shortcut_mic: Arc<AtomicU32>,
}

/// Toggles, or re-enforces mute while the user wants it; otherwise watches unmuted devices.
/// Returns whether a toggle just unmuted, so the popup should hide after a delay.
fn update_mic(ui: &RwLock<UI>, controller: &RwLock<MicController>, toggle: bool) -> bool {
    let mut controller = controller.write();
    if toggle || controller.should_enforce_mute() {
        let state = if toggle { None } else { Some(true) };
        match controller.toggle(state).map(|_| ()) {
            Ok(()) => controller.update_failures.clear(),
            Err(err) => controller.update_failures.report(&err, Instant::now()),
        }
        let mut ui = ui.write();
        if let Err(err) = ui.update_mic(controller.muted) {
            log::error!("Failed to update microphone UI: {:#}", err);
        }
    } else {
        controller.watch_unmuted();
    }
    controller.save_input_levels_if_due();
    toggle && !controller.muted
}

/// Hands the devices settings.json excludes to the controller. A tray click, a hand edit, and
/// About → Reset Settings all change them in `settings`, so the poll applies each of them here.
/// Returns whether they changed.
fn apply_exclusions(controller: &RwLock<MicController>, settings: &RwLock<Settings>) -> bool {
    let excluded = settings.read().excluded_uids();
    controller.write().set_excluded(excluded)
}

/// Shows each input device's state in the tray's Microphones menu. The tray touches only the
/// rows that change, so this runs on every poll.
fn show_devices(
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    settings: &RwLock<Settings>,
    failures: &mut FailureLog,
) {
    let excluded = settings.read().excluded_devices.clone();
    let controller = controller.read();
    match ui
        .write()
        .update_devices(controller.device_states(), &excluded)
    {
        Ok(()) => failures.clear(),
        Err(err) => failures.report(&err, Instant::now()),
    }
}

pub fn restore_microphone_on_exit(controller: &Arc<RwLock<MicController>>) {
    if let Err(err) = controller.write().restore_on_exit() {
        log::error!("Failed to restore microphone state on exit: {}", err);
    }
}

/// Runs the event loop until the app quits. The loop creates the tray, popup, and hotkey
/// itself, on `StartCause::Init`.
pub fn start(
    mut event_loop: EventLoop<Message>,
    controller: Arc<RwLock<MicController>>,
    camera: Arc<RwLock<CameraController>>,
    settings: Arc<RwLock<Settings>>,
) {
    // `None` until the loop runs; see `is_loop_start`.
    let mut ui_and_ids: Option<(RwLock<UI>, EventIds)> = None;
    let mut about_window = AboutWindow::default();

    let poll_interval = Duration::from_millis(POLL_INTERVAL_MILLIS);
    // Start in the past so the first iteration triggers the poll immediately.
    let mut last_poll = Instant::now() - poll_interval;
    let mut hide_popup_at: Option<Instant> = None;
    let mut is_exiting = false;
    let mut devices_menu_failures = FailureLog::new(
        "Failed to update the Microphones menu",
        "The Microphones menu updates again",
    );

    // Poll the settings file for changes every 2 seconds so edits to
    // settings.json take effect without restarting the app.
    let settings_poll_interval = Duration::from_secs(2);
    let mut last_settings_check = Instant::now();
    let mut last_settings_mtime = Settings::mtime();
    // main() runs on the defaults when settings.json is invalid at startup; the first loop
    // pass says so in the tray. A file that breaks later keeps the previous settings.
    use crate::settings::Fallback;
    let mut startup_settings_notice = Settings::load()
        .err()
        .map(|err| err.notice(Fallback::Defaults));
    let mut settings_fallback = if startup_settings_notice.is_some() {
        Fallback::Defaults
    } else {
        Fallback::Previous
    };

    // Camera detection runs expensive Cocoa/CMIO calls; offload to a background
    // thread so it never blocks the main event loop. Results are delivered back
    // via a user event.
    let proxy_camera = event_loop.create_proxy();
    let camera_bg = camera.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let active =
            objc::rc::autoreleasepool(|| camera_bg.read().is_running_anywhere().unwrap_or(false));
        proxy_camera
            .send_event(Message::CameraStateChanged(active))
            .ok();
    });

    trace!("Starting event loop");
    // Set activation policy based on persisted show_in_dock before the loop starts.
    let initial_show_in_dock = settings.read().show_in_dock;
    event_loop.set_activation_policy(if initial_show_in_dock {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    });
    event_loop.run(move |event, target, control_flow| {
        // Every NSApp terminate: path (Dock → Quit, ⌘Q, logout, AppleScript `quit`) and
        // ControlFlow::Exit end the loop here. After tray → Exit, restoring again is a no-op.
        if let Event::LoopDestroyed = event {
            restore_microphone_on_exit(&controller);
            return;
        }
        // Tray → Exit already restored the microphones; never re-mute them before the end.
        if is_exiting {
            *control_flow = ControlFlow::Exit;
            return;
        }
        if ui_and_ids.is_none() {
            // Until Init, tao delivers at most `Event::Opened` (a launch by URL), which the
            // app ignores.
            if !is_loop_start(&event) {
                return;
            }
            match create_ui(target, &controller, &camera, &settings) {
                Ok(created) => ui_and_ids = Some(created),
                Err(err) => {
                    log::error!("Failed to create the tray, popup, or hotkey: {:#}", err);
                    is_exiting = true;
                    restore_microphone_on_exit(&controller);
                    *control_flow = ControlFlow::ExitWithCode(UI_FAILURE_EXIT_CODE);
                    return;
                }
            }
        }
        let Some((ui, event_ids)) = &ui_and_ids else {
            return;
        };
        let EventIds {
            button_toggle_mute,
            button_launch_at_login,
            button_show_in_dock,
            button_show_popup,
            button_about,
            button_quit,
            button_open_sound_settings,
            shortcut_mic,
        } = event_ids;
        let mut exit_requested = false;
        // Set to run the poll in this pass rather than at its next deadline.
        let mut is_poll_requested = false;

        if let Event::UserEvent(Message::CameraStateChanged(active)) = event {
            let muted = !active;
            if muted != camera.read().muted {
                camera.write().muted = muted;
                if let Err(err) = ui.write().update_camera(muted) {
                    log::error!("Failed to update camera UI: {:#}", err);
                }
            }
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            trace!("Tray menu event: {:?}", event);
            // Its own statement: an `if let` would hold the UI lock through every branch.
            let clicked_device = ui.read().clicked_device(&event.id);
            if event.id == button_quit {
                trace!("Exit tray menu item selected");
                exit_requested = true;
            } else if event.id == button_toggle_mute {
                trace!("Toggle mic tray menu item selected");
                if update_mic(ui, &controller, true) {
                    hide_popup_at = Some(Instant::now() + HIDE_POPUP_DELAY);
                }
            } else if event.id == button_launch_at_login {
                trace!("Launch at login toggled");
                let mut s = settings.write();
                s.launch_at_login = !s.launch_at_login;
                let enabled = s.launch_at_login;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                if let Err(e) = launch_at_login::set(enabled) {
                    log::error!("Launch at login error: {}", e);
                }
            } else if event.id == button_show_in_dock {
                trace!("Show in dock toggled");
                let mut s = settings.write();
                s.show_in_dock = !s.show_in_dock;
                let visible = s.show_in_dock;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                launch_at_login::set_dock_visible(visible);
            } else if event.id == button_show_popup {
                trace!("Show popup toggled");
                let mut s = settings.write();
                s.show_popup = !s.show_popup;
                let visible = s.show_popup;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                if let Err(e) = ui.write().set_popup_enabled(visible) {
                    log::error!("Failed to apply popup setting: {}", e);
                }
            } else if event.id == button_about {
                trace!("About tray menu item selected");
                if let Err(e) = about_window.show() {
                    log::error!("Failed to show the About window: {:#}", e);
                }
            } else if let Some(device) = clicked_device {
                trace!("Microphones menu: {} ({}) toggled", device.name, device.uid);
                let mut s = settings.write();
                s.toggle_device_exclusion(&device.uid, &device.name);
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                // The poll hands the change to the controller and shows it; run it now.
                is_poll_requested = true;
            } else if event.id == button_open_sound_settings {
                trace!("Open Sound Settings tray menu item selected");
                if let Err(e) = utils::open_sound_input_settings() {
                    log::error!("Failed to open Sound settings: {:#}", e);
                }
            }
        }

        // About's buttons only record the click; taking it closes the window. The action runs
        // here, so About never stops the loop.
        if let Some(button) = about_window.take_click() {
            trace!("About → {} clicked", button.title());
            let changed = {
                let mut s = settings.write();
                match about::run_action(button, &mut s) {
                    Ok(true) => Some(s.clone()),
                    Ok(false) => None,
                    Err(e) => {
                        log::error!("About → {} failed: {:#}", button.title(), e);
                        None
                    }
                }
            };
            // Reset Settings: apply every setting at once.
            if let Some(new_settings) = changed {
                let mut ui = ui.write();
                if let Err(e) = ui.apply_settings(&new_settings) {
                    log::error!("Failed to apply settings: {}", e);
                } else {
                    shortcut_mic.store(ui.mic_shortcut_id(), Ordering::Relaxed);
                }
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            // Only act on key-down; global-hotkey fires both Pressed and Released
            if event.state() == global_hotkey::HotKeyState::Pressed {
                let id = event.id();
                if shortcut_mic.load(Ordering::Relaxed) == id {
                    trace!("Toggle mic shortcut activated");
                    if update_mic(ui, &controller, true) {
                        hide_popup_at = Some(Instant::now() + HIDE_POPUP_DELAY);
                    }
                }
            }
        }

        // Once: settings.json was unusable at startup, so main() fell back to the defaults.
        if let Some(notice) = startup_settings_notice.take() {
            if let Err(e) = ui.write().set_settings_notice(Some(&notice)) {
                log::error!("Failed to show settings notice: {}", e);
            }
        }

        // Reload settings if the file has been modified since we last checked.
        if last_settings_check.elapsed() >= settings_poll_interval {
            last_settings_check = Instant::now();
            let current_mtime = Settings::mtime();
            if current_mtime != last_settings_mtime {
                last_settings_mtime = current_mtime;
                let notice = if Settings::take_own_save(current_mtime) {
                    // Saved by the app itself (a tray toggle or About): memory already
                    // matches the file, and the app only ever writes a valid file.
                    trace!("settings.json changed by the app's own save");
                    settings_fallback = Fallback::Previous;
                    None
                } else {
                    trace!("settings.json changed on disk — reloading");
                    match Settings::load() {
                        Ok(new_settings) => {
                            *settings.write() = new_settings.clone();
                            settings_fallback = Fallback::Previous;
                            let mut ui_w = ui.write();
                            if let Err(e) = ui_w.apply_settings(&new_settings) {
                                log::error!("Failed to apply reloaded settings: {}", e);
                            } else {
                                shortcut_mic.store(ui_w.mic_shortcut_id(), Ordering::Relaxed);
                                trace!("Settings reloaded from settings.json");
                            }
                            None
                        }
                        Err(err) => {
                            // Apply nothing from a broken file. Only a new mtime triggers a
                            // reload, so this warns once per change to the file.
                            log::warn!("{}; keeping the current settings", err);
                            Some(err.notice(settings_fallback))
                        }
                    }
                };
                if let Err(e) = ui.write().set_settings_notice(notice.as_deref()) {
                    log::error!("Failed to update settings notice: {}", e);
                }
            }
        }

        // Poll mic state and cursor-monitor position on a 200 ms interval.
        if is_poll_requested || last_poll.elapsed() >= poll_interval {
            last_poll = Instant::now();
            let exclusions_changed = apply_exclusions(&controller, &settings);
            update_mic(ui, &controller, false);
            if exclusions_changed {
                // Exclusions change the mute state while unmuted too, which `update_mic`
                // leaves off the UI.
                let muted = controller.read().muted;
                if let Err(err) = ui.write().update_mic(muted) {
                    log::error!("Failed to update microphone UI: {:#}", err);
                }
            }
            show_devices(ui, &controller, &settings, &mut devices_menu_failures);
            if let Err(err) = ui.write().detect() {
                log::error!("Failed to update popup placement: {:#}", err);
            }
        }

        // Hide the popup a moment after an unmute, unless the mic was muted again.
        if hide_popup_at.is_some_and(|deadline| Instant::now() >= deadline) {
            hide_popup_at = None;
            let mic_controller = controller.read();
            if !mic_controller.muted {
                if let Err(err) = ui.write().hide_popup() {
                    log::error!("Failed to hide popup: {:#}", err);
                }
            }
        }

        if exit_requested {
            is_exiting = true;
            restore_microphone_on_exit(&controller);
            *control_flow = ControlFlow::Exit;
        } else {
            // Sleep until the next scheduled check rather than spinning.
            let next_poll = last_poll + poll_interval;
            let next_settings = last_settings_check + settings_poll_interval;
            let mut next_wake = next_poll.min(next_settings);
            if let Some(deadline) = hide_popup_at {
                next_wake = next_wake.min(deadline);
            }
            *control_flow = ControlFlow::WaitUntil(next_wake);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_is_created_when_the_loop_starts_and_not_before() {
        assert!(is_loop_start(&Event::NewEvents(StartCause::Init)));
        let later_events = [
            Event::NewEvents(StartCause::Poll),
            Event::MainEventsCleared,
            Event::UserEvent(Message::CameraStateChanged(true)),
            Event::LoopDestroyed,
        ];
        for event in later_events {
            assert!(!is_loop_start(&event), "{event:?}");
        }
    }
}
