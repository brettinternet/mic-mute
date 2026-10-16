mod about;
mod camera;
mod config;
mod device_changes;
mod device_records;
mod event_loop;
mod feedback;
mod icons;
mod launch_at_login;
mod logging;
mod mic;
mod mic_command;
mod mute_triggers;
mod notification;
mod popup;
mod popup_content;
mod push_to_talk;
mod session_events;
mod settings;
mod settings_window;
mod shortcuts;
mod store;
mod tray;
mod ui;
mod url_scheme;
mod utils;

use crate::camera::CameraController;
use crate::event_loop::{restore_locked_microphone_on_exit, start};
use crate::mic::{LaunchIntent, MicController};
use crate::mute_triggers::{LastMuteState, MUTE_STATE_FILE_NAME};
use crate::settings::Settings;
use crate::utils::arc_lock;
use log::{info, trace};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Signals that end the app; the watcher thread restores the microphones before exiting.
const SHUTDOWN_SIGNALS: [libc::c_int; 4] =
    [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Runs `restore`, then `exit`, under one write guard of `lock`; `exit` ends the process, so the
/// guard is never given back. The signal watcher exits this way: a mute the main thread was about
/// to run (the shortcut, the tray, a session event, the launch) can't take the controller between
/// the restore and the exit. The main thread then waits on the lock until the process ends.
fn restore_then_exit<T>(lock: &RwLock<T>, restore: impl FnOnce(&mut T), exit: impl FnOnce(&T)) {
    let mut guard = lock.write();
    restore(&mut guard);
    exit(&guard);
}

/// Logs the panic to the unified log and restores the microphones from restore.json when a
/// panic is about to end the app, then runs the previous hook, which prints the panic to stderr.
fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The installed app's stderr goes nowhere: without this, Console never shows a panic.
        logging::log_panic(info);
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
    // stderr and the unified log; RUST_LOG overrides the default filter for both.
    logging::init().expect("main installs the logger first, once");
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

    // Decided before the controller exists: restoring what an unclean exit left muted, only for
    // the loop to mute it again once it runs, would leave the microphones live in between.
    let last_state = LastMuteState::load(store::state_file(MUTE_STATE_FILE_NAME));
    let launch_mute = mute_triggers::launch_mute_reason(&settings, last_state.muted());
    let intent = match launch_mute {
        Some(_) => LaunchIntent::Mute,
        None => LaunchIntent::KeepHardwareState,
    };
    // Never fails: devices it can't read yet count as failed, and every poll tries them again.
    let controller = MicController::new(settings.excluded_uids(), intent);
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
            restore_then_exit(
                &shutdown_controller,
                restore_locked_microphone_on_exit,
                |_| std::process::exit(0),
            );
        }
    });

    let camera = CameraController::new().unwrap();
    let camera_muted = camera.muted;
    let camera = arc_lock(camera);
    trace!("Camera controller initialized, muted={}", camera_muted);

    // The loop creates the tray, popup, and hotkey once it runs: tray-icon requires that.
    let event_loop = event_loop::create();
    let settings = arc_lock(settings);
    start(
        event_loop,
        controller,
        camera,
        settings,
        last_state,
        launch_mute,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_exit_runs_under_the_write_guard_its_restore_took() {
        let lock = RwLock::new(Vec::new());
        let mut has_exited = false;

        restore_then_exit(
            &lock,
            |steps| steps.push("restore"),
            |steps| {
                assert_eq!(*steps, ["restore"]);
                assert!(lock.is_locked_exclusive(), "nothing can mute in between");
                has_exited = true;
            },
        );

        assert!(has_exited);
        assert!(!lock.is_locked(), "only a test's exit returns");
    }
}
