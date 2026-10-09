use crate::about::show_about;
use crate::camera::CameraController;
use crate::launch_at_login;
use crate::mic::MicController;
use crate::settings::Settings;
use crate::ui::UI;
use async_std::task;
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use muda::{MenuEvent, MenuId};
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

const POLL_INTERVAL_MILLIS: u64 = 200;

#[derive(Debug)]
pub enum Message {
    HidePopup,
    CameraStateChanged(bool),
}

pub type EventLoopMessage = EventLoop<Message>;
pub type EventLoopProxyMessage = tao::event_loop::EventLoopProxy<Message>;

pub fn create() -> EventLoopMessage {
    EventLoopBuilder::<Message>::with_user_event().build()
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

fn update_mic(
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    proxy: EventLoopProxyMessage,
    toggle: bool,
) {
    let mut controller = controller.write();
    if toggle || controller.should_enforce_mute() {
        let state = if toggle { None } else { Some(true) };
        if let Err(err) = controller.toggle(state) {
            log::error!("Failed to update microphone mute state: {}", err);
        }
        let device_name = controller.active_device_name();
        let mut ui = ui.write();
        ui.update_mic(controller.muted, device_name.as_deref())
            .unwrap();
    }
    if toggle && !controller.muted {
        task::spawn(async move {
            task::sleep(Duration::from_secs(1)).await;
            proxy.send_event(Message::HidePopup).unwrap();
        });
    }
}

fn refresh_skip_mics(
    ui: &Arc<RwLock<UI>>,
    controller: &Arc<RwLock<MicController>>,
    settings: &Arc<RwLock<Settings>>,
) {
    let devices = match controller.read().input_devices() {
        Ok(devices) => devices,
        Err(err) => {
            log::error!("Failed to list input devices: {}", err);
            return;
        }
    };
    let settings = settings.read();
    if let Err(err) = ui.write().update_skip_mics(&devices, &settings) {
        log::error!("Failed to update skip mics menu: {}", err);
    }
}

fn apply_excluded_devices(
    ui: &Arc<RwLock<UI>>,
    controller: &Arc<RwLock<MicController>>,
    excluded_devices: &[String],
) {
    let mut controller = controller.write();
    controller.set_excluded_devices(excluded_devices);
    // Settings can change the aggregate status even when mute enforcement is off.
    let device_name = controller.active_device_name();
    if let Err(err) = ui
        .write()
        .update_mic(controller.muted, device_name.as_deref())
    {
        log::error!("Failed to refresh microphone status: {}", err);
    }
}

pub fn restore_microphone_on_exit(controller: &Arc<RwLock<MicController>>) {
    if let Err(err) = controller.write().restore_on_exit() {
        log::error!("Failed to restore microphone state on exit: {}", err);
    }
}

pub fn start(
    mut event_loop: EventLoop<Message>,
    event_ids: EventIds,
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    camera: Arc<RwLock<CameraController>>,
    settings: Arc<RwLock<Settings>>,
) {
    let EventIds {
        button_toggle_mute,
        button_launch_at_login,
        button_show_in_dock,
        button_show_popup,
        button_about,
        button_quit,
        shortcut_mic,
    } = event_ids;

    let poll_interval = Duration::from_millis(POLL_INTERVAL_MILLIS);
    // Start in the past so the first iteration triggers the poll immediately.
    let mut last_poll = Instant::now() - poll_interval;

    // Poll the settings file for changes every 2 seconds so edits to
    // settings.json take effect without restarting the app.
    let settings_poll_interval = Duration::from_secs(2);
    let mut last_settings_check = Instant::now();
    let mut last_settings_mtime = Settings::mtime();

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

    refresh_skip_mics(&ui, &controller, &settings);

    trace!("Starting event loop");
    let proxy = event_loop.create_proxy();
    // Set activation policy based on persisted show_in_dock before the loop starts.
    let initial_show_in_dock = settings.read().show_in_dock;
    event_loop.set_activation_policy(if initial_show_in_dock {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    });
    event_loop.run(move |event, _, control_flow| {
        let mut exit_requested = false;

        match event {
            Event::UserEvent(Message::HidePopup) => {
                let mic_controller = controller.read();
                if !mic_controller.muted {
                    let mut ui = ui.write();
                    ui.hide_popup().unwrap();
                }
            }
            Event::UserEvent(Message::CameraStateChanged(active)) => {
                let muted = !active;
                if muted != camera.read().muted {
                    camera.write().muted = muted;
                    ui.write().update_camera(muted).unwrap();
                }
            }
            _ => {}
        };

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            trace!("Tray menu event: {:?}", event);
            let skip_mic = ui.read().skip_mic_for(&event.id);
            if event.id == button_quit {
                trace!("Exit tray menu item selected");
                exit_requested = true;
            } else if event.id == button_toggle_mute {
                trace!("Toggle mic tray menu item selected");
                update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
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
            } else if let Some(device) = skip_mic {
                let already_excluded = settings.read().is_device_excluded(&device.uid);
                if already_excluded || UI::confirm_exclude_device(&device) {
                    let mut updated = settings.read().clone();
                    updated.toggle_excluded_device(&device.uid);
                    match updated.save() {
                        Ok(()) => {
                            *settings.write() = updated.clone();
                            apply_excluded_devices(&ui, &controller, &updated.excluded_devices);
                        }
                        Err(err) => log::error!("Failed to save device exclusion: {}", err),
                    }
                }
                // Undo the native checkbox toggle on cancellation or save failure.
                refresh_skip_mics(&ui, &controller, &settings);
            } else if event.id == button_about {
                trace!("About tray menu item selected");
                let mut s = settings.write();
                match show_about(&mut s) {
                    Ok(true) => {
                        // Reset to Default clicked — apply all settings immediately
                        apply_excluded_devices(&ui, &controller, &s.excluded_devices);
                        let mut ui = ui.write();
                        if let Err(e) = ui.apply_settings(&s) {
                            log::error!("Failed to apply settings: {}", e);
                        } else {
                            shortcut_mic.store(ui.mic_shortcut_id(), Ordering::Relaxed);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => log::error!("Preferences error: {}", e),
                }
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            // Only act on key-down; global-hotkey fires both Pressed and Released
            if event.state() == global_hotkey::HotKeyState::Pressed {
                let id = event.id();
                if shortcut_mic.load(Ordering::Relaxed) == id {
                    trace!("Toggle mic shortcut activated");
                    update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
                }
            }
        }

        // Reload settings if the file has been modified since we last checked.
        if last_settings_check.elapsed() >= settings_poll_interval {
            last_settings_check = Instant::now();
            let current_mtime = Settings::mtime();
            if current_mtime != last_settings_mtime {
                last_settings_mtime = current_mtime;
                trace!("settings.json changed on disk — reloading");
                let new_settings = Settings::load();
                let mut s = settings.write();
                *s = new_settings.clone();
                drop(s);
                apply_excluded_devices(&ui, &controller, &new_settings.excluded_devices);
                let mut ui_w = ui.write();
                if let Err(e) = ui_w.apply_settings(&new_settings) {
                    log::error!("Failed to apply reloaded settings: {}", e);
                } else {
                    shortcut_mic.store(ui_w.mic_shortcut_id(), Ordering::Relaxed);
                    trace!("Settings reloaded from settings.json");
                }
            }
            // Pick up plugged/unplugged devices on the same cadence.
            refresh_skip_mics(&ui, &controller, &settings);
        }

        // Poll mic state and cursor-monitor position on a 200 ms interval.
        if last_poll.elapsed() >= poll_interval {
            last_poll = Instant::now();
            update_mic(ui.clone(), controller.clone(), proxy.clone(), false);
            let mut ui_w = ui.write();
            ui_w.detect().unwrap();
        }

        if exit_requested {
            restore_microphone_on_exit(&controller);
            *control_flow = ControlFlow::Exit;
        } else {
            // Sleep until the next scheduled check rather than spinning.
            let next_poll = last_poll + poll_interval;
            let next_settings = last_settings_check + settings_poll_interval;
            *control_flow = ControlFlow::WaitUntil(next_poll.min(next_settings));
        }
    });
}
