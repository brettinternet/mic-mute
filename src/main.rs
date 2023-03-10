mod config;
mod event_loop;
mod mic;
mod popup;
mod popup_content;
mod shortcuts;
mod tray;
mod ui;
mod utils;
// TODO: Use better Apple logging support? https://lib.rs/crates/oslog

#[macro_use]
extern crate objc;

use env_logger::{Builder, Env};
use event_loop::start;
use log::{info, trace};
use mic::MicController;
use ui::UI;
use utils::arc_lock;

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");
    let controller = arc_lock(MicController::new().unwrap());
    trace!("Controller initialized {:?}", controller);
    let controller_main = controller.clone();
    let controller_loop = controller.clone();
    let (ui, event_loop, event_ids) = UI::new(controller_main).unwrap();
    trace!("UI initialized");
    let ui = arc_lock(ui);
    start(event_loop, event_ids, ui, controller_loop);
}
