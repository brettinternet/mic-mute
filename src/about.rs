/// About window for the app.
/// Shows version info, shortcut configuration, and a link to the GitHub repo via a native macOS NSAlert.
use crate::settings::Settings;
use crate::shortcuts::{current_status, ShortcutStatus};
use anyhow::{Context, Result};
use cocoa::base::nil;
use cocoa::foundation::NSString;
use objc::runtime::Object;
use std::path::PathBuf;
use std::process::Command;

/// The file `Settings::save` writes.
fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json"))
}

/// Opens settings.json in the default text editor. A fresh install has no file yet and
/// `open` fails on a missing path, so the file is first written with `settings`, the
/// settings in use. An existing file opens as it is, even an invalid one.
fn open_settings_file(settings: &Settings) -> Result<()> {
    let path = settings_path().context("Failed to locate the settings directory")?;
    if matches!(path.try_exists(), Ok(false)) {
        settings
            .save()
            .context("Failed to create the settings file")?;
    }
    Command::new("open")
        .arg("-t")
        .arg(&path)
        .spawn()
        .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(())
}

/// The settings after About → Reset: `mic_shortcut`, `show_in_dock`, and `show_popup` at
/// their defaults. Every other field is kept: `launch_at_login` mirrors the system login
/// item, and fields this build doesn't manage must survive. List a new setting here if
/// Reset should cover it.
fn reset_settings(current: &Settings) -> Settings {
    let defaults = Settings::default();
    Settings {
        mic_shortcut: defaults.mic_shortcut,
        show_in_dock: defaults.show_in_dock,
        show_popup: defaults.show_popup,
        ..current.clone()
    }
}

/// The alert's text: the registered shortcut (and why it isn't the configured one, if it
/// isn't), the settings path, and the version.
fn about_text(status: &ShortcutStatus, version: &str) -> String {
    let shortcut = status
        .registered
        .map_or_else(|| "none".to_string(), |shortcut| shortcut.to_string());
    let mut text = format!("Mute shortcut: {shortcut}");
    if let Some(problem) = &status.problem {
        let fallback = match status.registered {
            Some(registered) => format!("{registered} is used instead."),
            None => "Mute from the tray menu.".to_string(),
        };
        text.push_str(&format!("\n\n{problem}. {fallback}"));
    }
    text.push_str(&format!(
        "\n\nSettings:\n~/Library/Application Support/mic-mute/settings.json\n\nVersion: {version}\n\nSource:\ngithub.com/brettinternet/mic-mute"
    ));
    text
}

/// Show the About window as an NSAlert dialog.
/// Returns Ok(true) if settings were reset to defaults, Ok(false) if dismissed.
pub fn show_about(settings: &mut Settings) -> Result<bool> {
    let info = about_text(&current_status(), env!("CARGO_PKG_VERSION"));

    let response = unsafe {
        let alert: *mut Object = msg_send![class!(NSAlert), new];

        let title = NSString::alloc(nil).init_str("Mic Mute");
        let _: () = msg_send![alert, setMessageText: title];
        let _: () = msg_send![title, release];

        let info_str = NSString::alloc(nil).init_str(&info);
        let _: () = msg_send![alert, setInformativeText: info_str];
        let _: () = msg_send![info_str, release];

        let ok_str = NSString::alloc(nil).init_str("OK");
        let _: () = msg_send![alert, addButtonWithTitle: ok_str];
        let _: () = msg_send![ok_str, release];
        let open_str = NSString::alloc(nil).init_str("Open Settings");
        let _: () = msg_send![alert, addButtonWithTitle: open_str];
        let _: () = msg_send![open_str, release];
        let reset_str = NSString::alloc(nil).init_str("Reset Settings");
        let _: () = msg_send![alert, addButtonWithTitle: reset_str];
        let _: () = msg_send![reset_str, release];

        // 1000 = OK, 1001 = Open Settings, 1002 = Reset Settings
        let response: i64 = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        response
    };

    match response {
        1001 => {
            open_settings_file(settings)?;
            Ok(false)
        }
        1002 => {
            let reset = reset_settings(settings);
            reset.save().context("Failed to save the reset settings")?;
            *settings = reset;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutConfig;
    use crate::shortcuts::{parse_shortcut, Shortcut, ShortcutProblem};

    fn default_shortcut() -> Shortcut {
        parse_shortcut(&ShortcutConfig::default()).unwrap()
    }

    fn customized_settings(launch_at_login: bool) -> Settings {
        serde_json::from_str(&format!(
            r#"{{
                "mic_shortcut": {{ "modifiers": ["ctrl"], "key": "K" }},
                "show_in_dock": true,
                "launch_at_login": {launch_at_login},
                "show_popup": false,
                "excluded_devices": ["Teams Audio"]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn reset_restores_shortcut_dock_and_popup_defaults() {
        let reset = reset_settings(&customized_settings(true));
        let defaults = Settings::default();
        assert_eq!(reset.mic_shortcut, defaults.mic_shortcut);
        assert_eq!(reset.show_in_dock, defaults.show_in_dock);
        assert_eq!(reset.show_popup, defaults.show_popup);
    }

    #[test]
    fn reset_keeps_launch_at_login_and_every_field_it_does_not_reset() {
        let reset_fields = ["mic_shortcut", "show_in_dock", "show_popup"];
        for launch_at_login in [true, false] {
            let current = customized_settings(launch_at_login);
            let reset = serde_json::to_value(reset_settings(&current)).unwrap();
            let current = serde_json::to_value(&current).unwrap();
            assert_eq!(reset["launch_at_login"], launch_at_login);
            for (name, value) in current.as_object().unwrap() {
                if !reset_fields.contains(&name.as_str()) {
                    assert_eq!(reset.get(name), Some(value), "{name}");
                }
            }
        }
    }

    #[test]
    fn about_text_shows_the_registered_shortcut() {
        let status = ShortcutStatus {
            registered: Some(default_shortcut()),
            problem: None,
        };
        let text = about_text(&status, "1.2.3");
        assert!(
            text.starts_with("Mute shortcut: ⇧⌘M\n\nSettings:\n"),
            "{text}"
        );
        assert!(text.contains("\n\nVersion: 1.2.3\n\n"), "{text}");
    }

    #[test]
    fn about_text_notes_an_invalid_configured_shortcut() {
        let status = ShortcutStatus {
            registered: Some(default_shortcut()),
            problem: Some(ShortcutProblem::Invalid {
                reason: "unknown modifier \"cmnd\"".to_string(),
            }),
        };
        let text = about_text(&status, "1.2.3");
        let note = "Mute shortcut: ⇧⌘M\n\nThe mic_shortcut setting is invalid: unknown \
                    modifier \"cmnd\". ⇧⌘M is used instead.\n\nSettings:";
        assert!(text.starts_with(note), "{text}");
    }

    #[test]
    fn about_text_without_a_hotkey_points_to_the_tray_menu() {
        let status = ShortcutStatus {
            registered: None,
            problem: Some(ShortcutProblem::NotRegistered {
                shortcut: default_shortcut(),
                reason: "taken".to_string(),
            }),
        };
        let text = about_text(&status, "1.2.3");
        let note = "Mute shortcut: none\n\n⇧⌘M could not be registered: taken. Mute from the \
                    tray menu.\n\nSettings:";
        assert!(text.starts_with(note), "{text}");
    }
}
