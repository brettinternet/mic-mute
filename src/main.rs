// Use better Apple logging support? https://lib.rs/crates/oslog
use env_logger::{Builder, Env};
use log::{debug, error, info, trace};
use std::sync::{
    mpsc::{self, RecvTimeoutError},
    Arc, RwLock,
};
use std::time::Duration;
use std::{process, thread};
mod audio;
mod config;
mod macos;
mod tray;
mod ui;
mod utils;

use audio::AudioController;
use macos::quit;
use ui::UI;

pub enum Message {
    ToggleMic,
    Exit,
}

fn init_controller() -> Arc<RwLock<AudioController>> {
    let controller = AudioController::new().unwrap();
    trace!("Controller initialized {:?}", controller);
    let rwlock = RwLock::new(controller);
    Arc::new(rwlock)
}

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");

    let controller = init_controller();
    let ui_controller = controller.clone();
    let ui = UI::new(ui_controller).unwrap();
    let (tx, rx) = mpsc::channel::<Message>();
    let tray = ui.tray.clone();
    trace!("Spawning message listener thread");
    let message_controller = controller.clone();
    thread::spawn(move || {
        trace!("Listening for messages on app actions channel");
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Message::ToggleMic) => {
                    trace!("ToggleMic message received");
                    let mut controller = message_controller.write().unwrap();
                    trace!("TOGGLE MIC RECEIVED -before {}", controller.muted);
                    controller.toggle().unwrap();
                    trace!("TRAY POISONED? {}", tray.is_poisoned());
                    let mut tray = tray.write().unwrap();
                    trace!("TRAY LOCK---------");
                    tray.update(controller.muted).unwrap();
                    trace!("TOGGLE MIC RECEIVED -after");
                }
                Ok(Message::Exit) => {
                    quit();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    error!("App actions channel disconnected");
                    process::exit(1);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });

    ui.start(tx);
}
