mod about;
mod camera;
mod config;
mod device_records;
mod event_loop;
mod icons;
mod launch_at_login;
mod mic;
mod popup;
mod popup_content;
mod settings;
mod shortcuts;
mod store;
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

/// Signals that end the app; the watcher thread restores the microphones before exiting.
const SHUTDOWN_SIGNALS: [libc::c_int; 4] =
    [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Restores the microphones from restore.json when a panic is about to end the app, then
/// runs the previous hook, which prints the panic.
fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only a main-thread panic ends the app: tao stops the event loop and re-raises it,
        // and AppKit callbacks abort. A panic on a background thread (the camera poller, the
        // signal watcher) leaves the loop running and still enforcing mute, so restoring
        // there would unmute a user who is muted.
        if unsafe { libc::pthread_main_np() } == 1 {
            mic::restore_from_journal_file();
        }
        previous_hook(info);
    }));
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
    // Before anything can mute a device, so any later panic still restores them.
    install_panic_hook();

    // An invalid settings.json is never written over: run on the defaults until it's fixed.
    let (mut settings, is_settings_file_valid) = match Settings::load() {
        Ok(settings) => (settings, true),
        Err(err) => {
            log::warn!("{}; using the default settings until it's fixed", err);
            (Settings::default(), false)
        }
    };

    // What macOS starts at login wins over the setting: mirror it, so the tray shows the truth
    // and a stale setting doesn't silently remove the login item. `reconcile` also moves a
    // LaunchAgent from an earlier version to a login item (macOS 13+).
    if let Some(state) = launch_at_login::reconcile() {
        if state.is_on() != settings.launch_at_login {
            settings.launch_at_login = state.is_on();
            if is_settings_file_valid {
                if let Err(e) = settings.save() {
                    log::error!("Failed to save settings: {}", e);
                }
            }
        }
    }

    let app_vars = AppVars::new();

    let controller = MicController::new().unwrap();
    let mic_muted = controller.muted;
    let controller = arc_lock(controller);
    trace!("Mic controller initialized {:?}", controller);

    // Register the shutdown signal handlers. The signal handler only sets a flag;
    // a background thread performs microphone cleanup before exiting.
    for signal in SHUTDOWN_SIGNALS {
        let previous =
            unsafe { libc::signal(signal, handle_signal as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            log::error!(
                "Failed to handle signal {}: {}",
                signal,
                std::io::Error::last_os_error()
            );
        }
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
