#[macro_use]
extern crate objc;

// Use better Apple logging support? https://lib.rs/crates/oslog
use env_logger::{Builder, Env};
use log::{info, trace};
mod audio;
mod config;
mod event_loop;
mod popup;
mod popup_content;
mod shortcuts;
mod tray;
mod ui;
mod utils;

use audio::AudioController;
use event_loop::start;
use ui::UI;
use utils::arc_lock;

pub enum Message {
    ToggleMic,
    HidePopup,
    Exit,
}

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");
    let controller = arc_lock(AudioController::new().unwrap());
    trace!("Controller initialized {:?}", controller);
    let controller_main = controller.clone();
    let controller_loop = controller.clone();
    let (ui, event_loop, event_ids) = UI::new(controller_main).unwrap();
    trace!("UI initialized");
    let ui = arc_lock(ui);
    start(event_loop, event_ids, ui, controller_loop);
}
