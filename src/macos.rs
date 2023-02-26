use crate::config;
use cocoa::{
    appkit::{NSApp, NSApplication, NSApplicationActivationPolicy::*},
    base::{id, nil, BOOL, NO, YES},
};
use objc::{class, msg_send, sel, sel_impl};

pub fn hide_dock() {
    unsafe {
        NSApp().setActivationPolicy_(NSApplicationActivationPolicyAccessory);
    }
}

fn quit_gui() {
    unsafe {
        let () = msg_send!(NSApp(), terminate: nil);
    };
}

pub fn quit() -> bool {
    quit_gui();
    std::process::Command::new("pkill")
        .arg(config::get_app_name())
        .status()
        .ok();
    true
}
