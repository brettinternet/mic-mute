mod about;
mod camera;
mod config;
mod event_loop;
mod icons;
mod launch_at_login;
mod mic;
mod popup;
mod popup_content;
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
use crate::event_loop::{restore_microphone_on_exit, start};
use crate::mic::MicController;
use crate::settings::Settings;
use crate::ui::UI;
use crate::utils::arc_lock;
use env_logger::{Builder, Env};
use log::{info, trace};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn main() {
    // RUST_LOG overrides this. Release builds skip the 200 ms poll's per-tick trace output.
    let default_filter = if cfg!(debug_assertions) {
        "trace"
    } else {
        "info"
    };
    Builder::from_env(Env::default().default_filter_or(default_filter)).init();
    info!("Starting app");

    // An invalid settings.json is never written over: run on the defaults until it's fixed.
    let (mut settings, is_settings_file_valid) = match Settings::load() {
        Ok(settings) => (settings, true),
        Err(err) => {
            log::warn!("{}; using the default settings until it's fixed", err);
            (Settings::default(), false)
        }
    };

    // On first run (or after upgrading from a version without launch_at_login in
    // settings), adopt the existing plist state so we don't silently disable it.
    let plist_enabled = launch_at_login::is_enabled();
    if plist_enabled != settings.launch_at_login {
        settings.launch_at_login = plist_enabled;
        if is_settings_file_valid {
            if let Err(e) = settings.save() {
                log::error!("Failed to save settings: {}", e);
            }
        }
    }
    // The app may have moved since the LaunchAgent was written; repoint it at this bundle.
    if settings.launch_at_login {
        if let Err(e) = launch_at_login::refresh() {
            log::error!("Failed to refresh launch at login: {}", e);
        }
    }

    let app_vars = AppVars::new();

    let controller = MicController::new().unwrap();
    let mic_muted = controller.muted;
    let controller = arc_lock(controller);
    trace!("Mic controller initialized {:?}", controller);

    // Register SIGTERM/SIGINT handlers. The signal handler only sets a flag;
    // a background thread performs microphone cleanup before exiting.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
    let shutdown_controller = controller.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("Signal received — restoring microphone state before exit");
            restore_microphone_on_exit(&shutdown_controller);
            std::process::exit(0);
        }
    });

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
