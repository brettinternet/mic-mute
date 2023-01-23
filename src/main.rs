// Use better Apple logging support? https://lib.rs/crates/oslog
use env_logger::{Builder, Env};
use log::{debug, error, info, trace};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use std::{process, thread};

mod audio;
mod tray;

use audio::AudioController;
use tray::Tray;

enum Message {
    Mute,
    Unmute,
}

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");

    let (tx, rx) = mpsc::channel::<Message>();
    trace!("Spawning message listener thread");
    thread::spawn(move || {
        let controller = AudioController::new(|msg| error!("{}", msg));

        trace!("Listening for messages on app actions channel");
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Message::Mute) => {
                    controller.mute_all(true);
                }
                Ok(Message::Unmute) => {
                    controller.mute_all(false);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    error!("App actions channel disconnected");
                    process::exit(1);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });

    let tx2 = tx.clone();
    let tx3 = tx.clone();
    let mut tray = Tray::new().unwrap();
    tray.add_mic_mute(move || tx2.send(Message::Mute).unwrap());
    tray.add_mic_unmute(move || tx3.send(Message::Unmute).unwrap());
    tray.run();
}
