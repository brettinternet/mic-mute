use crate::about::{self, AboutWindow};
use crate::camera::CameraController;
use crate::config::AppVars;
use crate::launch_at_login;
use crate::mic::MicController;
use crate::mic_command::MicCommand;
use crate::settings::Settings;
use crate::ui::UI;
use crate::url_scheme::{UrlAction, UrlActions};
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
/// How long a `status` URL shows the popup.
const STATUS_POPUP_DURATION: Duration = Duration::from_secs(2);
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
    pub shortcut_mic: Arc<AtomicU32>,
}

/// Runs `request`. Without one, it is the poll: re-mutes while the user wants mute, otherwise
/// watches unmuted devices. Returns whether a request just unmuted, so the popup should hide
/// after a delay.
fn update_mic(
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    request: Option<MicCommand>,
) -> bool {
    let mut controller = controller.write();
    let state = match request {
        Some(command) => command.mute_state(),
        None if controller.should_enforce_mute() => Some(true),
        None => {
            controller.watch_unmuted();
            controller.save_input_levels_if_due();
            return false;
        }
    };
    match controller.toggle(state).map(|_| ()) {
        Ok(()) => controller.update_failures.clear(),
        Err(err) => controller.update_failures.report(&err, Instant::now()),
    }
    if let Err(err) = ui.write().update_mic(controller.muted) {
        log::error!("Failed to update microphone UI: {:#}", err);
    }
    controller.save_input_levels_if_due();
    request.is_some() && !controller.muted
}

/// Runs a change the user asked for. Returns when to hide the popup: a moment after an unmute.
fn request_mic(
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    command: MicCommand,
) -> Option<Instant> {
    update_mic(ui, controller, Some(command)).then(|| Instant::now() + HIDE_POPUP_DELAY)
}

/// Keeps the popup up until the later of `hide_at` and `until`; `None` sorts before every
/// deadline.
fn keep_popup_until(hide_at: &mut Option<Instant>, until: Option<Instant>) {
    *hide_at = (*hide_at).max(until);
}

/// Runs a URL's action and returns when to hide the popup. An action that can unmute runs only
/// with `allow_url_unmute`: any app or web page can open the URL.
fn run_url_action(
    action: UrlAction,
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    allow_url_unmute: bool,
) -> Option<Instant> {
    if !action.is_allowed(allow_url_unmute) {
        log::warn!(
            "Refused the {} URL action: it can unmute, which needs \"allow_url_unmute\": true in \
             settings.json",
            action.name()
        );
        return None;
    }
    let Some(command) = action.command() else {
        let muted = controller.read().muted;
        log::info!(
            "Running the {} URL action: the microphones are {}",
            action.name(),
            if muted { "muted" } else { "unmuted" }
        );
        if let Err(err) = ui.write().show_popup_status() {
            log::error!("Failed to show the status popup: {:#}", err);
        }
        return Some(Instant::now() + STATUS_POPUP_DURATION);
    };
    log::info!("Running the {} URL action", action.name());
    request_mic(ui, controller, command)
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
    // Actions of opened URLs, run once the UI exists.
    let mut url_actions = UrlActions::default();

    let poll_interval = Duration::from_millis(POLL_INTERVAL_MILLIS);
    // Start in the past so the first iteration triggers the poll immediately.
    let mut last_poll = Instant::now() - poll_interval;
    let mut hide_popup_at: Option<Instant> = None;
    let mut is_exiting = false;

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
        // Parse and log each URL now; its action runs below, once the UI exists.
        if let Event::Opened { urls } = &event {
            url_actions.push_urls(urls.iter().map(|url| url.as_str()));
        }
        if ui_and_ids.is_none() {
            // Until Init, tao delivers at most `Event::Opened` (a launch by URL), queued above.
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
            shortcut_mic,
        } = event_ids;
        let mut exit_requested = false;

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
            if event.id == button_quit {
                trace!("Exit tray menu item selected");
                exit_requested = true;
            } else if event.id == button_toggle_mute {
                trace!("Toggle mic tray menu item selected");
                let hide_at = request_mic(ui, &controller, MicCommand::Toggle);
                keep_popup_until(&mut hide_popup_at, hide_at);
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
                    let hide_at = request_mic(ui, &controller, MicCommand::Toggle);
                    keep_popup_until(&mut hide_popup_at, hide_at);
                }
            }
        }

        // URL actions, including any that arrived before the UI existed.
        while let Some(action) = url_actions.pop() {
            let allow_url_unmute = settings.read().allow_url_unmute;
            let hide_at = run_url_action(action, ui, &controller, allow_url_unmute);
            keep_popup_until(&mut hide_popup_at, hide_at);
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
        if last_poll.elapsed() >= poll_interval {
            last_poll = Instant::now();
            update_mic(ui, &controller, None);
            if let Err(err) = ui.write().detect() {
                log::error!("Failed to update popup placement: {:#}", err);
            }
        }

        // Hide the popup a moment after an unmute or a status URL, unless it stays up while
        // muted.
        if hide_popup_at.is_some_and(|deadline| Instant::now() >= deadline) {
            hide_popup_at = None;
            if let Err(err) = ui.write().hide_popup_unless_pinned() {
                log::error!("Failed to hide popup: {:#}", err);
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
        // A launch by URL can open it before Init; its action waits for the UI.
        assert!(!is_loop_start(&Event::Opened { urls: Vec::new() }));
    }

    #[test]
    fn the_popup_stays_up_until_the_latest_deadline() {
        let now = Instant::now();
        let soon = now + HIDE_POPUP_DELAY;
        let later = now + STATUS_POPUP_DURATION;

        let mut hide_at = None;
        keep_popup_until(&mut hide_at, None);
        assert_eq!(hide_at, None);
        keep_popup_until(&mut hide_at, Some(later));
        keep_popup_until(&mut hide_at, Some(soon));
        assert_eq!(
            hide_at,
            Some(later),
            "an earlier deadline doesn't cut it short"
        );
        keep_popup_until(&mut hide_at, None);
        assert_eq!(
            hide_at,
            Some(later),
            "a request that didn't unmute keeps it"
        );
    }
}
