use crate::macos::hide_dock;
use crate::tray::{get_mute_menu_text, Tray};
use crate::AudioController;
use crate::Message;
use anyhow::Result;
use log::trace;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::RwLock;
use tao::event_loop::{ControlFlow, EventLoop};
use tray_icon::{menu::MenuEvent, TrayEvent};

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
        let controller = controller_ref.clone();
        let muted = controller.read().unwrap().muted;
        let tray = init_tray(muted);

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
        event_loop.run(move |_event, _, control_flow| {
            if !dock_hidden {
                hide_dock();
                dock_hidden = true;
            }
            *control_flow = ControlFlow::Wait;
            // *control_flow = ControlFlow::Poll;

            // if let Ok(event) = TrayEvent::receiver().try_recv() {
            //     trace!("Tray event: {:?}", event);
            // }

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
        });
    }
}
