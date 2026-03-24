/// Preferences window for configuring shortcuts.
/// Shows current shortcut configuration via a native macOS NSAlert.
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

/// Show the preferences window as an NSAlert dialog.
/// Returns Ok(true) if user pressed "Reset to Default", Ok(false) if dismissed.
pub fn show_preferences(settings: &mut Settings) -> Result<bool> {
    let mic_shortcut_str = format_shortcut(&settings.mic_shortcut);

    let reset_clicked = unsafe {
        let alert: *mut Object = msg_send![class!(NSAlert), new];

        let title = NSString::alloc(nil).init_str("Mic Mute Preferences");
        let _: () = msg_send![alert, setMessageText: title];

        let info = format!(
            "Current mic shortcut: {}\n\nTo change, edit ~/Library/Application Support/mic-mute/settings.json",
            mic_shortcut_str
        );
        let info_str = NSString::alloc(nil).init_str(&info);
        let _: () = msg_send![alert, setInformativeText: info_str];

        // Add buttons (first added = rightmost = default)
        let ok_str = NSString::alloc(nil).init_str("OK");
        let _: () = msg_send![alert, addButtonWithTitle: ok_str];
        let reset_str = NSString::alloc(nil).init_str("Reset to Default");
        let _: () = msg_send![alert, addButtonWithTitle: reset_str];

        // Run the alert modal (returns 1000 for first button, 1001 for second, etc.)
        let response: i64 = msg_send![alert, runModal];
        // 1001 = second button = "Reset to Default"
        response == 1001
    };

    if reset_clicked {
        settings.mic_shortcut = ShortcutConfig::default();
        settings.save()?;
    }

    Ok(reset_clicked)
}
