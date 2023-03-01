use crate::config;
use crate::event_loop::{create, EventIds, EventLoopMessage, EventLoopProxyMessage};
use crate::popup::Popup;
use crate::shortcuts::Shortcuts;
use crate::tray::Tray;
use crate::AudioController;
use anyhow::{Context, Result};
use cocoa::{appkit::NSApp, base::nil};
use log::trace;
use objc::{msg_send, sel, sel_impl};
use std::sync::{Arc, RwLock};

/// Event loop must remain on the main thread and doesn't implement Copy
pub struct UI {
    tray: Tray,
    popup: Popup,
    shortcuts: Shortcuts,
    event_message: EventLoopProxyMessage,
}

unsafe impl Send for UI {}
unsafe impl Sync for UI {}

impl UI {
    pub fn new(
        controller: Arc<RwLock<AudioController>>,
    ) -> Result<(Self, EventLoopMessage, EventIds)> {
        let controller = controller.read().unwrap();
        let muted = controller.muted;
        let event_loop = create();
        let tray = Tray::new(muted).unwrap();
        let popup = Popup::new(&event_loop, muted).context("Failed to setup popup window")?;
        let shortcuts = Shortcuts::new().context("Failed to setup shortcuts")?;

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute.id(),
            button_quit: tray.quit.id(),
            shortcut_shift_meta_a: shortcuts.shift_meta_a.id(),
        };

        let event_message = event_loop.create_proxy();

        let ui = Self {
            tray,
            popup,
            shortcuts,
            event_message,
        };
        Ok((ui, event_loop, event_ids))
    }

    pub fn update(&mut self, muted: bool) -> Result<&mut Self> {
        trace!("Updating UI with state {}", muted);
        self.tray.update(muted)?;
        self.popup.update(muted)?;
        Ok(self)
    }

    pub fn hide_popup(&mut self) -> Result<&mut Self> {
        self.popup.hide()?;
        Ok(self)
    }

    pub fn detect(&mut self) -> Result<&mut Self> {
        self.popup.detect_cursor_monitor()?;
        if self.popup.cursor_on_separate_monitor {
            self.popup.update_placement()?;
        }
        Ok(self)
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
