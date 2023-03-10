use crate::{mic::MicController, ui::UI};
use async_std::task;
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use tao::platform::macos::ActivationPolicy;
use tao::platform::macos::EventLoopExtMacOS;
use tray_icon::menu::MenuEvent;

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

fn toggle_mic(
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    proxy: EventLoopProxyMessage,
) {
    let mut controller = controller.write().unwrap();
    controller.toggle(None).unwrap();
    let mut ui = ui.write().unwrap();
    ui.update(controller.muted).unwrap();
    if !controller.muted {
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

    trace!("Starting event loop");
    let proxy = event_loop.create_proxy();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(Message::HidePopup) => {
                let controller = controller.read().unwrap();
                if !controller.muted {
                    let mut ui = ui.write().unwrap();
                    ui.hide_popup().unwrap();
                }
            }
            Event::WindowEvent { .. } => {
                // println!("event: {:?}", event);
            }
            _ => {
                // trace!("event: {:?}", event);
                let mut ui = ui.write().unwrap();
                ui.detect().unwrap();
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
                    toggle_mic(ui.clone(), controller.clone(), proxy.clone());
                }
                _ => {}
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if shortcut_shift_meta_a == event.id {
                trace!("Toggle mic shortcut activated");
                toggle_mic(ui.clone(), controller.clone(), proxy.clone());
            }
        }
    });
}
