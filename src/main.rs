mod camera;
mod config;
mod event_loop;
mod mic;
mod popup;
mod popup_content;
mod preferences;
mod settings;
mod shortcuts;
mod tray;
mod ui;
mod utils;
// TODO: Use better Apple logging support? https://lib.rs/crates/oslog

#[macro_use]
extern crate objc;

use crate::camera::CameraController;
use crate::config::AppVars;
use crate::event_loop::start;
use crate::mic::MicController;
use crate::settings::Settings;
use crate::ui::UI;
use crate::utils::arc_lock;
use env_logger::{Builder, Env};
use log::{info, trace};

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");

    let settings = Settings::load();
    let app_vars = AppVars::new();

    let controller = MicController::new().unwrap();
    let mic_muted = controller.muted;
    let controller = arc_lock(controller);
    trace!("Mic controller initialized {:?}", controller);

    let camera = CameraController::new().unwrap();
    let camera_muted = camera.muted;
    let camera = arc_lock(camera);
    trace!("Camera controller initialized, muted={}", camera_muted);

    let (ui, event_loop, event_ids) =
        UI::new(mic_muted, camera_muted, app_vars, &settings).unwrap();
    trace!("UI initialized");
    let ui = arc_lock(ui);
    let settings = arc_lock(settings);
    start(event_loop, event_ids, ui, controller, camera, settings);
}
