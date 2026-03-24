use crate::camera::CameraController;
use crate::launch_at_login;
use crate::mic::MicController;
use crate::preferences::show_preferences;
use crate::settings::Settings;
use crate::ui::UI;
use crate::utils::Throttle;
use async_std::task;
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use muda::{MenuEvent, MenuId};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

const THROTTLE_TIMEOUT_MILLIS: u64 = 200;

#[derive(Debug)]
pub enum Message {
    HidePopup,
}

pub type EventLoopMessage = EventLoop<Message>;
pub type EventLoopProxyMessage = tao::event_loop::EventLoopProxy<Message>;

pub fn create() -> EventLoopMessage {
    EventLoopBuilder::<Message>::with_user_event().build()
}

pub struct EventIds {
    pub button_toggle_mute: MenuId,
    pub button_toggle_camera: MenuId,
    pub button_launch_at_login: MenuId,
    pub button_show_in_dock: MenuId,
    pub button_preferences: MenuId,
    pub button_quit: MenuId,
    pub shortcut_mic: u32,
    pub shortcut_camera: u32,
}

fn update_mic(
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    proxy: EventLoopProxyMessage,
    toggle: bool,
) {
    let mut controller = controller.write().unwrap();
    if toggle || controller.muted {
        let state = if toggle { None } else { Some(controller.muted) };
        controller.toggle(state).unwrap();
        let device_name = controller.active_device_name();
        let mut ui = ui.write().unwrap();
        ui.update_mic(controller.muted, device_name.as_deref()).unwrap();
    }
    if toggle && !controller.muted {
        task::spawn(async move {
            task::sleep(Duration::from_secs(1)).await;
            proxy.send_event(Message::HidePopup).unwrap();
        });
    }
}

fn update_camera(
    ui: Arc<RwLock<UI>>,
    camera: Arc<RwLock<CameraController>>,
    proxy: EventLoopProxyMessage,
    toggle: bool,
) {
    let mut camera = camera.write().unwrap();
    if toggle || camera.muted {
        let state = if toggle { None } else { Some(camera.muted) };
        camera.toggle(state).unwrap();
        let mut ui = ui.write().unwrap();
        ui.update_camera(camera.muted).unwrap();
    }
    if toggle && !camera.muted {
        task::spawn(async move {
            task::sleep(Duration::from_secs(1)).await;
            proxy.send_event(Message::HidePopup).unwrap();
        });
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
        button_toggle_camera,
        button_launch_at_login,
        button_show_in_dock,
        button_preferences,
        button_quit,
        shortcut_mic,
        shortcut_camera,
    } = event_ids;

    let mut throttle = Throttle::new(Duration::from_millis(THROTTLE_TIMEOUT_MILLIS));

    trace!("Starting event loop");
    let proxy = event_loop.create_proxy();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Poll;

        match event {
            Event::UserEvent(Message::HidePopup) => {
                let mic_controller = controller.read().unwrap();
                let cam_controller = camera.read().unwrap();
                if !mic_controller.muted && !cam_controller.muted {
                    let mut ui = ui.write().unwrap();
                    ui.hide_popup().unwrap();
                }
            }
            _ => {
                if throttle.available() {
                    update_mic(ui.clone(), controller.clone(), proxy.clone(), false);
                    let mut ui = ui.write().unwrap();
                    ui.detect().unwrap();
                    throttle.accept().unwrap_or(());
                }
            }
        };

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            trace!("Tray menu event: {:?}", event);
            if event.id == button_quit {
                trace!("Exit tray menu item selected");
                let mut mic = controller.write().unwrap();
                mic.toggle(Some(false)).unwrap();
                let mut cam = camera.write().unwrap();
                cam.toggle(Some(false)).unwrap();
                *control_flow = ControlFlow::Exit;
            } else if event.id == button_toggle_mute {
                trace!("Toggle mic tray menu item selected");
                update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
            } else if event.id == button_toggle_camera {
                trace!("Toggle camera tray menu item selected");
                update_camera(ui.clone(), camera.clone(), proxy.clone(), true);
            } else if event.id == button_launch_at_login {
                trace!("Launch at login toggled");
                let enabled = launch_at_login::is_enabled();
                if let Err(e) = launch_at_login::set(!enabled) {
                    log::error!("Launch at login error: {}", e);
                }
            } else if event.id == button_show_in_dock {
                trace!("Show in dock toggled");
                let mut s = settings.write().unwrap();
                s.show_in_dock = !s.show_in_dock;
                let visible = s.show_in_dock;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                launch_at_login::set_dock_visible(visible);
            } else if event.id == button_preferences {
                trace!("Preferences tray menu item selected");
                let mut s = settings.write().unwrap();
                if let Err(e) = show_preferences(&mut s) {
                    log::error!("Preferences error: {}", e);
                }
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if shortcut_mic == event.id() {
                trace!("Toggle mic shortcut activated");
                update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
            } else if shortcut_camera == event.id() {
                trace!("Toggle camera shortcut activated");
                update_camera(ui.clone(), camera.clone(), proxy.clone(), true);
            }
        }
    });
}
