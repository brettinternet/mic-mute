use crate::config;
use crate::popup::Popup;
use crate::shortcuts::Shortcuts;
use crate::tray::Tray;
use crate::AudioController;
use crate::Message;
use anyhow::{Context, Result};
use cocoa::{
    appkit::{NSApp, NSApplication, NSApplicationActivationPolicy::*},
    base::nil,
};
use global_hotkey::GlobalHotKeyEvent;
use log::trace;
use objc::{msg_send, sel, sel_impl};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::RwLock;
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::macos::ActivationPolicy;
use tao::platform::macos::EventLoopExtMacOS;
use tray_icon::menu::MenuEvent;

pub struct EventIds {
    button_toggle_mute: u32,
    button_quit: u32,
    shortcut_shift_meta_a: u32,
}

/// Event loop must remain on the main thread and doesn't implement Copy
pub struct UI {
    tray: Tray,
    popup: Popup,
    shortcuts: Shortcuts,
}

unsafe impl Send for UI {}
unsafe impl Sync for UI {}

impl UI {
    pub fn new(
        controller: Arc<RwLock<AudioController>>,
    ) -> Result<(Self, EventLoop<()>, EventIds)> {
        let controller = controller.read().unwrap();
        let muted = controller.muted;
        let event_loop = EventLoop::new();
        let tray = Tray::new(muted).unwrap();
        let popup = Popup::new(&event_loop, muted).context("Failed to setup popup window")?;
        let shortcuts = Shortcuts::new().context("Failed to setup shortcuts")?;

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute.id(),
            button_quit: tray.quit.id(),
            shortcut_shift_meta_a: shortcuts.shift_meta_a.id(),
        };

        let ui = Self {
            tray,
            popup,
            shortcuts,
        };
        Ok((ui, event_loop, event_ids))
    }

    pub fn update(&mut self, muted: bool) -> Result<()> {
        trace!("Updating UI with state {}", muted);
        self.tray.update(muted)?;
        self.popup.update(muted)?;
        Ok(())
    }

    pub fn hide_popup(&mut self) -> Result<()> {
        self.popup.hide()?;
        Ok(())
    }

    pub fn quit(&self) -> bool {
        self.quit_gui();
        std::process::Command::new("pkill")
            .arg(config::get_app_name())
            .status()
            .ok();
        true
    }

    fn quit_gui(&self) {
        unsafe {
            let () = msg_send!(NSApp(), terminate: nil);
        };
    }
}

pub fn start(mut event_loop: EventLoop<()>, event_ids: EventIds, tx: Sender<Message>) {
    let EventIds {
        button_toggle_mute,
        button_quit,
        shortcut_shift_meta_a,
    } = event_ids;

    trace!("Starting event loop");
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // *control_flow = ControlFlow::Poll;

        // trace!("event: {:?}", event);
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            trace!("Tray menu event: {:?}", event);
            match event {
                MenuEvent { id } if id == button_quit => {
                    trace!("Exit tray menu item selected");
                    tx.send(Message::Exit).unwrap();
                    *control_flow = ControlFlow::Exit;
                }
                MenuEvent { id } if id == button_toggle_mute => {
                    trace!("Toggle mic tray menu item selected");
                    tx.send(Message::ToggleMic).unwrap();
                }
                _ => {}
            }
        }

        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if shortcut_shift_meta_a == event.id {
                trace!("Toggle mic shortcut activated");
                tx.send(Message::ToggleMic).unwrap();
            }
        }
    });
}
