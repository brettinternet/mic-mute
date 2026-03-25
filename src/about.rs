/// About window for the app.
/// Shows version info, shortcut configuration, and a link to the GitHub repo via a native macOS NSAlert.
use crate::settings::{Settings, ShortcutConfig};
use anyhow::Result;
use cocoa::base::nil;
use cocoa::foundation::NSString;
use objc::runtime::Object;

fn format_shortcut(config: &ShortcutConfig) -> String {
    let mut parts = vec![];
    for modifier in &config.modifiers {
        match modifier.as_str() {
            "shift" => parts.push("⇧"),
            "meta" | "cmd" | "command" => parts.push("⌘"),
            "ctrl" | "control" => parts.push("⌃"),
            "alt" | "option" => parts.push("⌥"),
            _ => {}
        }
    }
    parts.push(config.key.as_str());
    parts.join("")
}

/// Show the About window as an NSAlert dialog.
/// Returns Ok(true) if settings were reset to defaults, Ok(false) if dismissed.
pub fn show_about(settings: &mut Settings) -> Result<bool> {
    let mic_str = format_shortcut(&settings.mic_shortcut);

    let reset_clicked = unsafe {
        let alert: *mut Object = msg_send![class!(NSAlert), new];

        let title = NSString::alloc(nil).init_str("Mic Mute");
        let _: () = msg_send![alert, setMessageText: title];
        let _: () = msg_send![title, release];

        let info = format!(
            "Mute shortcut: {mic_str}\n\nSettings:\n~/Library/Application Support/mic-mute/settings.json\n\nSource:\ngithub.com/brettinternet/mic-mute"
        );
        let info_str = NSString::alloc(nil).init_str(&info);
        let _: () = msg_send![alert, setInformativeText: info_str];
        let _: () = msg_send![info_str, release];

        let ok_str = NSString::alloc(nil).init_str("OK");
        let _: () = msg_send![alert, addButtonWithTitle: ok_str];
        let _: () = msg_send![ok_str, release];
        let reset_str = NSString::alloc(nil).init_str("Reset settings");
        let _: () = msg_send![alert, addButtonWithTitle: reset_str];
        let _: () = msg_send![reset_str, release];

        // 1000 = first button (OK), 1001 = second button (Reset to Default)
        let response: i64 = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        response == 1001
    };

    if reset_clicked {
        settings.mic_shortcut = ShortcutConfig::default();
        settings.save()?;
    }

    Ok(reset_clicked)
}
