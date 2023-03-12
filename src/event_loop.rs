use crate::mic::MicController;
use crate::ui::UI;
use crate::utils::Throttle;
use async_std::task;
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::MenuEvent;

// Timeout for mouse detect and device re-mute
const THROTTLE_TIMEOUT_MILLIS: u64 = 200;

#[derive(Debug)]
pub enum Message {
    HidePopup,
}

pub type EventLoopMessage = EventLoop<Message>;
pub type EventLoopProxyMessage = EventLoopProxy<Message>;

pub fn create() -> EventLoopMessage {
    EventLoop::<Message>::with_user_event()
}

pub struct EventIds {
    pub button_toggle_mute: u32,
    pub button_quit: u32,
    pub shortcut_shift_meta_a: u32,
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
        let mut ui = ui.write().unwrap();
        ui.update(controller.muted).unwrap();
    }
    if toggle && !controller.muted {
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
) {
    let EventIds {
        button_toggle_mute,
        button_quit,
        shortcut_shift_meta_a,
    } = event_ids;

    let mut throttle = Throttle::new(Duration::from_millis(THROTTLE_TIMEOUT_MILLIS));

    trace!("Starting event loop");
    let proxy = event_loop.create_proxy();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Poll;

        match event {
            Event::UserEvent(Message::HidePopup) => {
                let controller = controller.read().unwrap();
                if !controller.muted {
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
            match event {
                MenuEvent { id } if id == button_quit => {
                    trace!("Exit tray menu item selected");
                    let mut controller = controller.write().unwrap();
                    controller.toggle(Some(false)).unwrap();
                    *control_flow = ControlFlow::Exit;
                }
                MenuEvent { id } if id == button_toggle_mute => {
                    trace!("Toggle mic tray menu item selected");
                    update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
                }
                _ => {}
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if shortcut_shift_meta_a == event.id {
                trace!("Toggle mic shortcut activated");
                update_mic(ui.clone(), controller.clone(), proxy.clone(), true);
            }
        }
    });
}
