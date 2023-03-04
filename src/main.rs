#[macro_use]
extern crate objc;

// Use better Apple logging support? https://lib.rs/crates/oslog
use env_logger::{Builder, Env};
use log::{info, trace};
use std::sync::{Arc, RwLock};
mod audio;
mod config;
mod event_loop;
mod popup;
mod popup_content;
mod shortcuts;
mod tray;
mod ui;
use anyhow::{Context, Result};
mod utils;

use audio::AudioController;
// use event_loop::start;
use kas::event::UpdateId;
use kas::prelude::*;
use kas::theme::TextClass;
use shortcuts::Shortcuts;
use tray::Tray;
use ui::UI;
use utils::arc_lock;

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");
    let controller = arc_lock(AudioController::new().unwrap());
    trace!("Controller initialized {:?}", controller);
    let controller_main = controller.clone();
    let controller_loop = controller.clone();
    let controller = controller_main.read().unwrap();
    let theme = kas::theme::FlatTheme::new();
    let shell = kas::shell::DefaultShell::new(theme).unwrap();
    let tray = Tray::new(controller.muted).unwrap();
    // let (ui, event_loop, event_ids) = UI::new(controller_main).unwrap();
    trace!("UI initialized");
    // let ui = arc_lock(ui);
    // start(event_loop, event_ids, ui, controller_loop);
    let proxy = shell.create_proxy();
    let update_id = UpdateId::new();
    let shortcuts = Shortcuts::new()
        .context("Failed to setup shortcuts")
        .unwrap();
    let event_ids = EventIds {
        button_toggle_mute: tray.toggle_mute.id(),
        button_quit: tray.quit.id(),
        shortcut_shift_meta_a: shortcuts.shift_meta_a.id(),
    };
    let widget = Indicator::new(controller_loop, tray, event_ids, update_id);
    shell.with(widget).unwrap().run()
}

#[derive(Debug)]
pub struct EventIds {
    pub button_toggle_mute: u32,
    pub button_quit: u32,
    pub shortcut_shift_meta_a: u32,
}

impl_scope! {
    #[derive(Debug)]
    #[widget]
    struct Indicator {
        core: widget_core!(),
        update_id: UpdateId,
        loading_text: Text<&'static str>,
        loaded: bool,
        event_ids: EventIds,
        tray: Tray,
        controller: Arc<RwLock<AudioController>>,
    }

    impl Self {
        fn new(controller: Arc<RwLock<AudioController>>, tray: Tray, event_ids: EventIds, update_id: UpdateId) -> Self {
            Self {
                core: Default::default(),
                tray,
                controller,
                update_id,
                loading_text: Text::new("Loading..."),
                loaded: false,
                event_ids,
            }
        }
    }
    impl Layout for Indicator {
        fn size_rules(&mut self, mgr: SizeMgr, _: AxisInfo) -> SizeRules {
            SizeRules::fixed_scaled(100.0, 10.0, mgr.scale_factor())
        }

        fn set_rect(&mut self, mgr: &mut ConfigMgr, rect: Rect) {
            self.core.rect = rect;
            let align = Some(AlignPair::new(Align::Center, Align::Center));
            mgr.text_set_size(
                &mut self.loading_text,
                TextClass::Label(false),
                rect.size,
                align,
            );
        }

        // fn row()

        fn draw(&mut self, mut draw: DrawMgr) {
            if !self.loaded {
                draw.text(self.core.rect, &self.loading_text, TextClass::Label(false));
            // } else {
            //     let draw = draw.draw_device();
            //     draw.rect((self.rect()).cast());
            }
        }
    }

    impl Widget for Indicator {
        fn handle_event(&mut self, mgr: &mut EventMgr, event: Event) -> Response {
            match event {
                Event::Update { id, .. } if id == self.update_id => {
                    self.loaded = true;
                    mgr.redraw(self.id());
                    Response::Used
                }
                _ => Response::Unused,
            }
        }
    }

    impl Window for Self {
        fn title(&self) -> &str {
            "Async event demo"
        }
    }
}
