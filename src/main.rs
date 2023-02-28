// Use better Apple logging support? https://lib.rs/crates/oslog
use env_logger::{Builder, Env};
use log::{error, info, trace};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use std::{process, thread};
mod audio;
mod config;
mod popup;
mod shortcuts;
mod tray;
mod ui;
mod utils;
use async_std::task;

use audio::AudioController;
use ui::{start, UI};
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
    let controller_message = controller.clone();
    let (tx, rx) = mpsc::channel::<Message>();
    let (ui, event_loop, event_ids) = UI::new(controller_main).unwrap();
    let ui = arc_lock(ui);
    trace!("UI initialized");
    let ui_message = ui.clone();
    let tx_main = tx.clone();
    let tx_message = tx.clone();
    trace!("Spawning message listener thread");
    thread::spawn(move || {
        trace!("Listening for messages on app actions channel");
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Message::ToggleMic) => {
                    trace!("ToggleMic message received");
                    let mut controller = controller_message.write().unwrap();
                    let mut ui = ui_message.write().unwrap();
                    controller.toggle(None).unwrap();
                    ui.update(controller.muted).unwrap();
                    if !controller.muted {
                        let tx_message = tx_message.clone();
                        task::spawn(async move {
                            task::sleep(Duration::from_secs(1)).await;
                            // let tx_message = tx_message.lock().await;
                            tx_message.send(Message::HidePopup).unwrap();
                        });
                    }
                }
                Ok(Message::HidePopup) => {
                    let mut ui = ui_message.write().unwrap();
                    ui.hide_popup().unwrap();
                }
                Ok(Message::Exit) => {
                    let mut controller = controller_message.write().unwrap();
                    controller.toggle(Some(false)).unwrap();
                    let ui = ui_message.write().unwrap();
                    ui.quit();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    error!("App actions channel disconnected");
                    process::exit(1);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });

    start(event_loop, event_ids, tx_main);
}
