use crate::about::{self, AboutWindow};
use crate::camera::CameraController;
use crate::config::AppVars;
use crate::device_changes::DeviceChangeSignal;
use crate::launch_at_login;
use crate::mic::{FailureLog, MicController};
use crate::mic_command::MicCommand;
use crate::mute_triggers::{self, LastMuteState, MUTE_STATE_FILE_NAME};
use crate::push_to_talk::PushToTalk;
use crate::session_events::SessionEvents;
use crate::settings::Settings;
use crate::store;
use crate::ui::UI;
use crate::url_scheme::{UrlAction, UrlActions};
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
/// The shortest time from one poll to the next that a device change runs: a device whose
/// controls keep changing, as when another app fights the mute, can't make the loop poll more
/// often than this.
const DEVICE_CHANGE_POLL_SPACING: Duration = Duration::from_millis(50);
/// How long the popup stays up after an unmute.
const HIDE_POPUP_DELAY: Duration = Duration::from_secs(1);
/// How long a `status` URL shows the popup.
const STATUS_POPUP_DURATION: Duration = Duration::from_secs(2);
/// The exit code when the tray, popup, or hotkey can't be created.
const UI_FAILURE_EXIT_CODE: i32 = 1;

#[derive(Debug)]
pub enum Message {
    CameraStateChanged(bool),
    /// A CoreAudio listener reported a device change (`DeviceChangeSignal`).
    DevicesChanged,
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

/// Runs a change the user asked for, and records the resulting intent for
/// `launch_state: "last"`. Returns when to hide the popup: a moment after an unmute.
fn request_mic(
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    last_state: &mut LastMuteState,
    command: MicCommand,
) -> Option<Instant> {
    let has_unmuted = update_mic(ui, controller, Some(command));
    let intent = controller.read().should_enforce_mute();
    last_state.record(intent);
    has_unmuted.then(|| Instant::now() + HIDE_POPUP_DELAY)
}

/// Keeps the popup up until the later of `hide_at` and `until`; `None` sorts before every
/// deadline.
fn keep_popup_until(hide_at: &mut Option<Instant>, until: Option<Instant>) {
    *hide_at = (*hide_at).max(until);
}

/// When a device change reported at `now` runs the poll: at once, unless the last poll ran less
/// than [`DEVICE_CHANGE_POLL_SPACING`] before. An earlier pending deadline stays.
fn device_change_poll_at(pending: Option<Instant>, last_poll: Instant, now: Instant) -> Instant {
    let at = now.max(last_poll + DEVICE_CHANGE_POLL_SPACING);
    pending.map_or(at, |pending| pending.min(at))
}

/// Runs a URL's action and returns when to hide the popup. An action that can unmute runs only
/// with `allow_url_unmute`: any app or web page can open the URL.
fn run_url_action(
    action: UrlAction,
    ui: &RwLock<UI>,
    controller: &RwLock<MicController>,
    last_state: &mut LastMuteState,
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
    request_mic(ui, controller, last_state, command)
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
    // Actions of opened URLs, run once the UI exists.
    let mut url_actions = UrlActions::default();
    // The shortcut's hold in push_to_talk mode.
    let mut push_to_talk = PushToTalk::default();
    // Screen lock and sleep, watched from Init on: they end a push-to-talk hold, and the
    // mute_on_* settings mute on them.
    let mut session_events: Option<SessionEvents> = None;
    // The mute state the user last chose, for `launch_state: "last"`.
    let mut last_state = LastMuteState::load(store::state_file(MUTE_STATE_FILE_NAME));

    let poll_interval = Duration::from_millis(POLL_INTERVAL_MILLIS);
    // Start in the past so the first iteration triggers the poll immediately.
    let mut last_poll = Instant::now() - poll_interval;
    // When the poll a reported device change asked for runs; see `device_change_poll_at`.
    let mut device_poll_at: Option<Instant> = None;
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

    // CoreAudio calls its listeners on its own thread or the main run loop; they only post
    // DevicesChanged here. Leaked: a listener can run while it is being removed, so the signal
    // must outlive every registration.
    let proxy_devices = event_loop.create_proxy();
    let device_changes: &'static DeviceChangeSignal =
        Box::leak(Box::new(DeviceChangeSignal::new(move || {
            // Fails only once the loop has ended, when there is nothing left to wake.
            proxy_devices.send_event(Message::DevicesChanged).ok();
        })));
    controller.write().start_listening(device_changes);

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
        let mut is_launching = false;
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
            match SessionEvents::new() {
                Ok(events) => session_events = Some(events),
                Err(err) => log::error!(
                    "Failed to watch for screen lock and sleep, which end a push-to-talk hold \
                     and drive the mute_on_* settings: {:#}",
                    err
                ),
            }
            is_launching = true;
        }
        let Some((ui, event_ids)) = &ui_and_ids else {
            return;
        };
        // Push-to-talk and `launch_state` can mute at launch; nothing unmutes.
        if is_launching {
            let reason = mute_triggers::launch_mute_reason(&settings.read(), last_state.muted());
            if let Some(reason) = reason {
                log::info!("Muting at launch: {}", reason);
                request_mic(ui, &controller, &mut last_state, MicCommand::Mute);
            }
            // Without a mute, the intent adopted from the hardware is the latest state.
            let intent = controller.read().should_enforce_mute();
            last_state.record(intent);
        }
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

        // A device came or went, or a control changed while mute is enforced: poll soon.
        if let Event::UserEvent(Message::DevicesChanged) = event {
            device_poll_at = Some(device_change_poll_at(
                device_poll_at,
                last_poll,
                Instant::now(),
            ));
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
                let hide_at = request_mic(ui, &controller, &mut last_state, MicCommand::Toggle);
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
                ui.write().set_launch_at_login(enabled);
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
                let old_mode = s.mode;
                match about::run_action(button, &mut s) {
                    Ok(true) => Some((old_mode, s.clone())),
                    Ok(false) => None,
                    Err(e) => {
                        log::error!("About → {} failed: {:#}", button.title(), e);
                        None
                    }
                }
            };
            // Reset Settings: apply every setting at once.
            if let Some((old_mode, new_settings)) = changed {
                let hotkey_before = shortcut_mic.load(Ordering::Relaxed);
                {
                    let mut ui = ui.write();
                    if let Err(e) = ui.apply_settings(&new_settings) {
                        log::error!("Failed to apply settings: {}", e);
                    } else {
                        shortcut_mic.store(ui.mic_shortcut_id(), Ordering::Relaxed);
                    }
                }
                let is_hotkey_replaced = shortcut_mic.load(Ordering::Relaxed) != hotkey_before;
                let mute = push_to_talk.on_settings_change(
                    old_mode,
                    new_settings.mode,
                    is_hotkey_replaced,
                );
                if let Some(command) = mute {
                    log::info!("Muting: the reset changed the shortcut's mode or hotkey");
                    request_mic(ui, &controller, &mut last_state, command);
                }
            }
        }

        // Every queued shortcut event, in order: push-to-talk needs the press and the release.
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if shortcut_mic.load(Ordering::Relaxed) != event.id() {
                continue;
            }
            let mode = settings.read().mode;
            if let Some(command) = push_to_talk.on_key(mode, event.state()) {
                trace!(
                    "Mute shortcut {:?} in {:?} mode: {:?}",
                    event.state(),
                    mode,
                    command
                );
                let hide_at = request_mic(ui, &controller, &mut last_state, command);
                keep_popup_until(&mut hide_popup_at, hide_at);
            }
        }

        // URL actions, including any that arrived before the UI existed.
        while let Some(action) = url_actions.pop() {
            let allow_url_unmute = settings.read().allow_url_unmute;
            let hide_at =
                run_url_action(action, ui, &controller, &mut last_state, allow_url_unmute);
            keep_popup_until(&mut hide_popup_at, hide_at);
        }

        // After the shortcut and URLs, so a mute here wins. In push_to_talk mode every event
        // mutes, since it can swallow a held shortcut's release; in toggle mode, the mute_on_*
        // settings choose. Nothing unmutes on unlock or wake.
        let received = session_events.as_ref().map(SessionEvents::take);
        for event in received.unwrap_or_default() {
            let (mode, trigger) = {
                let settings = settings.read();
                (settings.mode, mute_triggers::trigger_for(event, &settings))
            };
            let reason = match push_to_talk.interrupt(mode) {
                Some(_) => Some("push to talk"),
                None => trigger,
            };
            match reason {
                Some(reason) => {
                    log::info!("{}: muting ({})", event.description(), reason);
                    request_mic(ui, &controller, &mut last_state, MicCommand::Mute);
                }
                None => trace!("{}", event.description()),
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
                            let old_mode = settings.read().mode;
                            *settings.write() = new_settings.clone();
                            settings_fallback = Fallback::Previous;
                            let hotkey_before = shortcut_mic.load(Ordering::Relaxed);
                            {
                                let mut ui_w = ui.write();
                                if let Err(e) = ui_w.apply_settings(&new_settings) {
                                    log::error!("Failed to apply reloaded settings: {}", e);
                                } else {
                                    shortcut_mic.store(ui_w.mic_shortcut_id(), Ordering::Relaxed);
                                    trace!("Settings reloaded from settings.json");
                                }
                            }
                            let is_hotkey_replaced =
                                shortcut_mic.load(Ordering::Relaxed) != hotkey_before;
                            let mute = push_to_talk.on_settings_change(
                                old_mode,
                                new_settings.mode,
                                is_hotkey_replaced,
                            );
                            if let Some(command) = mute {
                                log::info!(
                                    "Muting: settings.json changed the shortcut's mode or hotkey"
                                );
                                request_mic(ui, &controller, &mut last_state, command);
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

        // Poll mic state and cursor-monitor position on a 200 ms interval, and soon after a
        // reported device change.
        let is_device_poll_due = device_poll_at.is_some_and(|at| Instant::now() >= at);
        if is_poll_requested || is_device_poll_due || last_poll.elapsed() >= poll_interval {
            last_poll = Instant::now();
            device_poll_at = None;
            // Before the poll reads the devices, so a change during the read wakes the loop again.
            device_changes.take();
            let exclusions_changed = apply_exclusions(&controller, &settings);
            update_mic(ui, &controller, None);
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
            for deadline in [hide_popup_at, device_poll_at].into_iter().flatten() {
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
            Event::UserEvent(Message::DevicesChanged),
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

    #[test]
    fn a_device_change_polls_at_once_unless_a_poll_just_ran() {
        let last_poll = Instant::now();
        let spaced = last_poll + DEVICE_CHANGE_POLL_SPACING;

        let later = spaced + Duration::from_millis(1);
        assert_eq!(device_change_poll_at(None, last_poll, later), later);
        let soon = last_poll + Duration::from_millis(10);
        assert_eq!(device_change_poll_at(None, last_poll, soon), spaced);
    }

    #[test]
    fn a_burst_of_device_changes_keeps_the_earliest_poll() {
        let last_poll = Instant::now();
        let first = last_poll + DEVICE_CHANGE_POLL_SPACING;

        let next = device_change_poll_at(Some(first), last_poll, first + Duration::from_secs(1));

        assert_eq!(next, first);
    }
}
