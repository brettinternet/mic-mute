use crate::macos::hide_dock;
use crate::tray::{get_mute_menu_text, Tray};
use crate::AudioController;
use crate::Message;
use anyhow::Result;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use log::trace;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::RwLock;
use tao::{
    accelerator::{Accelerator, AcceleratorId, SysMods},
    event::Event,
    event_loop::{ControlFlow, DeviceEventFilter, EventLoop},
    global_shortcut::ShortcutManager,
    keyboard::KeyCode,
    window::WindowBuilder,
};
use tray_icon::menu::MenuEvent;

fn init_tray(muted: bool) -> Arc<RwLock<Tray>> {
    let tray = Tray::new(muted).unwrap();
    trace!("Tray initialized");
    let rwlock = RwLock::new(tray);
    Arc::new(rwlock)
}

pub struct UI {
    pub tray: Arc<RwLock<Tray>>,
    event_loop: Box<EventLoop<()>>,
    controller: Arc<RwLock<AudioController>>,
}

impl UI {
    pub fn new(controller_ref: Arc<RwLock<AudioController>>) -> Result<Self> {
        let event_loop = EventLoop::new();
        let a = event_loop.available_monitors();
        let controller = controller_ref.clone();
        let muted = controller.read().unwrap().muted;
        let tray = init_tray(muted);
        // let window = WindowBuilder::new().build(&event_loop).unwrap();

        let ui = Self {
            tray,
            event_loop: Box::new(event_loop),
            controller: controller_ref,
        };
        Ok(ui)
    }

    pub fn start(self, tx: Sender<Message>) {
        trace!("Starting event loop");
        let mut dock_hidden = false;
        let controller = self.controller.clone();
        let tray = self.tray.clone();
        let tray = tray.read().unwrap();
        let (quit_id, toggle_mute_id) = (tray.quit.id(), tray.toggle_mute.id());
        drop(tray);
        let event_loop = *self.event_loop;

        let hotkeys_manager = GlobalHotKeyManager::new().unwrap();
        let hotkey = HotKey::new(Some(Modifiers::SHIFT | Modifiers::META), Code::KeyA);
        hotkeys_manager.register(hotkey).unwrap();
        let global_hotkey_channel = GlobalHotKeyEvent::receiver();

        event_loop.run(move |_event, _, control_flow| {
            if !dock_hidden {
                hide_dock();
                dock_hidden = true;
            }
            *control_flow = ControlFlow::Wait;
            // *control_flow = ControlFlow::Poll;

            if let Ok(event) = MenuEvent::receiver().try_recv() {
                trace!("Tray menu event: {:?}", event);
                match event {
                    MenuEvent { id } if id == quit_id => {
                        trace!("Exit tray menu item selected");
                        tx.send(Message::Exit).unwrap();
                        *control_flow = ControlFlow::Exit;
                    }
                    MenuEvent { id } if id == toggle_mute_id => {
                        let controller = controller.read().unwrap();
                        trace!(
                            "{} tray menu item selected",
                            get_mute_menu_text(controller.muted)
                        );
                        tx.send(Message::ToggleMic).unwrap();
                    }
                    _ => {}
                }
            }

            if let Ok(event) = global_hotkey_channel.try_recv() {
                if hotkey.id() == event.id {
                    let controller = controller.read().unwrap();
                    trace!(
                        "{} shortcut activated",
                        get_mute_menu_text(controller.muted)
                    );
                    tx.send(Message::ToggleMic).unwrap();
                }
            }
        });
    }
}
