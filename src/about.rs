//! The About window: the mute shortcut in use, the settings file's location, and the
//! version, with Open Settings and Reset Settings buttons.
//!
//! It is an `NSAlert`'s panel shown without `runModal`: a modal session would stall the
//! event-loop closure, and with it mute enforcement, the hotkey, and the settings reload.
//! The buttons only record the click; the closure takes it with `AboutWindow::take_click`,
//! which closes the window, and then runs `run_action`.
use crate::settings::Settings;
use crate::shortcuts::{current_status, ShortcutStatus};
use anyhow::{anyhow, bail, Context, Result};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::NSString;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

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

/// The settings after About → Reset: every setting this build manages at its default except
/// `launch_at_login`, which mirrors the system login item. Fields this build doesn't manage
/// are kept. List a new setting here if Reset should cover it.
fn reset_settings(current: &Settings) -> Settings {
    let defaults = Settings::default();
    Settings {
        mic_shortcut: defaults.mic_shortcut,
        show_in_dock: defaults.show_in_dock,
        show_popup: defaults.show_popup,
        // Inputs besides the tray menu.
        mode: defaults.mode,
        allow_url_unmute: defaults.allow_url_unmute,
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

/// The About window's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AboutButton {
    /// Only closes the window.
    Ok,
    OpenSettings,
    ResetSettings,
}

impl AboutButton {
    /// In the order the alert adds them, from the right. The first is the default button,
    /// which Return presses.
    const ALL: [Self; 3] = [Self::Ok, Self::OpenSettings, Self::ResetSettings];

    pub fn title(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::OpenSettings => "Open Settings",
            Self::ResetSettings => "Reset Settings",
        }
    }

    /// The button's `NSControl` tag, an `NSInteger`: the response `runModal` returned for
    /// it, counting from `NSAlertFirstButtonReturn` (1000).
    fn tag(self) -> i64 {
        match self {
            Self::Ok => 1000,
            Self::OpenSettings => 1001,
            Self::ResetSettings => 1002,
        }
    }

    fn from_tag(tag: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|button| button.tag() == tag)
    }
}

/// Runs `button`'s action on `settings`, the settings in use: Open Settings opens the file,
/// and Reset Settings saves `reset_settings` and adopts it. Returns whether `settings`
/// changed, so the caller applies them.
pub fn run_action(button: AboutButton, settings: &mut Settings) -> Result<bool> {
    match button {
        AboutButton::Ok => Ok(false),
        AboutButton::OpenSettings => {
            open_settings_file(settings)?;
            Ok(false)
        }
        AboutButton::ResetSettings => {
            let reset = reset_settings(settings);
            reset.save().context("Failed to save the reset settings")?;
            *settings = reset;
            Ok(true)
        }
    }
}

thread_local! {
    /// The button clicked since the event loop last took a click. AppKit sends actions on
    /// the main thread only, where the event loop runs.
    static CLICKED: Cell<Option<AboutButton>> = const { Cell::new(None) };
}

/// Keeps the first click until the event loop takes it.
fn record_click(button: AboutButton) {
    // `try_with` fails only while the thread exits, when nothing would act on the click.
    let _ = CLICKED.try_with(|clicked| {
        if clicked.get().is_none() {
            clicked.set(Some(button));
        }
    });
}

fn take_recorded_click() -> Option<AboutButton> {
    CLICKED.try_with(Cell::take).ok().flatten()
}

/// The Objective-C class of the About buttons' target, declared at runtime.
const TARGET_CLASS_NAME: &str = "MicMuteAboutButtonTarget";

/// Every About button's action: `-[MicMuteAboutButtonTarget aboutButtonClicked:]`.
fn button_action() -> Sel {
    sel!(aboutButtonClicked:)
}

/// The implementation of `aboutButtonClicked:`. AppKit calls it from inside its event
/// dispatch, so it only records the click: no AppKit calls, no locks, and no panics, which
/// can't unwind into Objective-C and would abort the app.
extern "C" fn about_button_clicked(_target: &Object, _cmd: Sel, sender: id) {
    if sender.is_null() {
        return;
    }
    // SAFETY: the sender is the clicked NSButton; `-[NSControl tag]` returns an NSInteger.
    let tag: i64 = unsafe { msg_send![sender, tag] };
    if let Some(button) = AboutButton::from_tag(tag) {
        record_click(button);
    }
}

/// The target class, declared and registered on first use.
fn target_class() -> Result<&'static Class> {
    static CLASS: OnceLock<Option<&'static Class>> = OnceLock::new();
    CLASS
        .get_or_init(declare_target_class)
        .ok_or_else(|| anyhow!("Failed to declare the Objective-C class {TARGET_CLASS_NAME}"))
}

fn declare_target_class() -> Option<&'static Class> {
    let Some(mut decl) = ClassDecl::new(TARGET_CLASS_NAME, class!(NSObject)) else {
        // The name is taken. Use that class only if it has the action.
        return Class::get(TARGET_CLASS_NAME)
            .filter(|class| class.instance_method(button_action()).is_some());
    };
    // SAFETY: `about_button_clicked` takes the receiver, the selector, and the one object
    // argument of `aboutButtonClicked:`, and returns nothing.
    unsafe {
        decl.add_method(
            button_action(),
            about_button_clicked as extern "C" fn(&Object, Sel, id),
        );
    }
    Some(decl.register())
}

/// A new alert showing `text`, laid out and centered, whose buttons send `aboutButtonClicked:`
/// to `target`. The caller owns the alert (`new` retains it) and releases it once.
///
/// # Safety
///
/// Main thread only, like every AppKit call; `target` must outlive the alert's buttons.
unsafe fn new_alert(text: &str, target: id) -> Result<id> {
    let alert: id = msg_send![class!(NSAlert), new];
    if alert.is_null() {
        bail!("Failed to create the About alert");
    }

    let title = NSString::alloc(nil).init_str("Mic Mute");
    let _: () = msg_send![alert, setMessageText: title];
    let _: () = msg_send![title, release];

    let info = NSString::alloc(nil).init_str(text);
    let _: () = msg_send![alert, setInformativeText: info];
    let _: () = msg_send![info, release];

    for button in AboutButton::ALL {
        let title = NSString::alloc(nil).init_str(button.title());
        // The alert owns the button.
        let ns_button: id = msg_send![alert, addButtonWithTitle: title];
        let _: () = msg_send![title, release];
        let _: () = msg_send![ns_button, setTag: button.tag()];
        // Replaces the alert's own action, which only ends a modal session.
        let _: () = msg_send![ns_button, setTarget: target];
        let _: () = msg_send![ns_button, setAction: button_action()];
    }

    // runModal would lay out and position the panel; without it, do both here.
    let _: () = msg_send![alert, layout];
    let window: id = msg_send![alert, window];
    // `AboutWindow::close` orders the panel out; nothing may free it behind the alert's back.
    let _: () = msg_send![window, setReleasedWhenClosed: NO];
    let _: () = msg_send![window, center];
    Ok(alert)
}

/// An alert on screen, with the text it shows.
struct OpenAlert {
    /// Retained; `AboutWindow::close` releases it once.
    alert: id,
    text: String,
}

/// The About window. Main thread only: it holds AppKit objects, so it is neither `Send` nor
/// `Sync`. Opening it never blocks the caller.
#[derive(Default)]
pub struct AboutWindow {
    open: Option<OpenAlert>,
    /// The buttons' target, created on the first `show` and released on drop. Controls don't
    /// retain their target, so it outlives every alert.
    target: Option<id>,
}

impl AboutWindow {
    /// Shows About, or brings the open window to the front. An open window whose text is out
    /// of date (the shortcut in use has changed) is replaced.
    pub fn show(&mut self) -> Result<()> {
        let text = about_text(&current_status(), env!("CARGO_PKG_VERSION"));
        if self.open.as_ref().is_some_and(|open| open.text != text) {
            self.close();
        }
        let alert = match &self.open {
            Some(open) => open.alert,
            None => {
                let target = self.button_target()?;
                // SAFETY: main thread (`AboutWindow` isn't `Send`); `target` lives until drop,
                // after `close` has released every alert.
                let alert = unsafe { new_alert(&text, target)? };
                self.open = Some(OpenAlert { alert, text });
                alert
            }
        };
        unsafe {
            let window: id = msg_send![alert, window];
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            // A tray click doesn't activate an accessory app; without this the window would
            // open behind other apps' windows, without keyboard focus.
            let _: () = msg_send![app, activateIgnoringOtherApps: YES];
            let _: () = msg_send![window, makeKeyAndOrderFront: nil];
        }
        Ok(())
    }

    /// The button clicked since the last call, if any. Every button closes the window, so a
    /// click is returned after the window has closed.
    pub fn take_click(&mut self) -> Option<AboutButton> {
        let button = take_recorded_click()?;
        // Only an open window has buttons to click; any other click is stale.
        let was_open = self.open.is_some();
        self.close();
        was_open.then_some(button)
    }

    /// Orders the window out and releases its alert; does nothing when About is closed.
    fn close(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        unsafe {
            let window: id = msg_send![open.alert, window];
            let _: () = msg_send![window, orderOut: nil];
            let _: () = msg_send![open.alert, release];
        }
    }

    fn button_target(&mut self) -> Result<id> {
        if let Some(target) = self.target {
            return Ok(target);
        }
        let class = target_class()?;
        let target: id = unsafe { msg_send![class, new] };
        if target.is_null() {
            bail!("Failed to create the About buttons' target");
        }
        self.target = Some(target);
        Ok(target)
    }
}

impl Drop for AboutWindow {
    fn drop(&mut self) {
        self.close();
        if let Some(target) = self.target.take() {
            unsafe {
                let _: () = msg_send![target, release];
            }
        }
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
                "mode": "push_to_talk",
                "allow_url_unmute": true,
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
    fn reset_restores_toggle_mode_and_turns_url_unmute_off() {
        let reset = reset_settings(&customized_settings(true));
        assert_eq!(reset.mode, Settings::default().mode);
        assert!(!reset.allow_url_unmute);
    }

    #[test]
    fn reset_keeps_launch_at_login_and_every_field_it_does_not_reset() {
        let reset_fields = [
            "mic_shortcut",
            "show_in_dock",
            "show_popup",
            "mode",
            "allow_url_unmute",
        ];
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

    #[test]
    fn buttons_keep_their_titles_with_ok_as_the_default() {
        let titles = AboutButton::ALL.map(AboutButton::title);
        assert_eq!(titles, ["OK", "Open Settings", "Reset Settings"]);
    }

    #[test]
    fn every_button_round_trips_through_its_tag() {
        for button in AboutButton::ALL {
            assert_eq!(
                AboutButton::from_tag(button.tag()),
                Some(button),
                "{button:?}"
            );
        }
    }

    #[test]
    fn tags_that_name_no_button_are_ignored() {
        for tag in [0, -1, 999, 1003] {
            assert_eq!(AboutButton::from_tag(tag), None, "{tag}");
        }
    }

    #[test]
    fn the_first_click_is_kept_until_taken() {
        record_click(AboutButton::OpenSettings);
        record_click(AboutButton::ResetSettings);
        assert_eq!(take_recorded_click(), Some(AboutButton::OpenSettings));
        assert_eq!(take_recorded_click(), None);
    }

    #[test]
    fn ok_changes_no_settings() {
        let mut settings = customized_settings(true);
        let before = serde_json::to_value(&settings).unwrap();
        assert!(!run_action(AboutButton::Ok, &mut settings).unwrap());
        assert_eq!(serde_json::to_value(&settings).unwrap(), before);
    }

    /// A stand-in for the clicked NSButton, whose `tag` is Reset Settings'.
    extern "C" fn fake_button_tag(_this: &Object, _cmd: Sel) -> i64 {
        AboutButton::ResetSettings.tag()
    }

    fn fake_button_class() -> &'static Class {
        static CLASS: OnceLock<&'static Class> = OnceLock::new();
        CLASS.get_or_init(|| {
            let mut decl = ClassDecl::new("MicMuteTestFakeAboutButton", class!(NSObject)).unwrap();
            unsafe {
                decl.add_method(
                    sel!(tag),
                    fake_button_tag as extern "C" fn(&Object, Sel) -> i64,
                );
            }
            decl.register()
        })
    }

    /// Sends `aboutButtonClicked:` from `sender` to a new target, as AppKit does on a click.
    fn click(sender: id) {
        let class = target_class().unwrap();
        unsafe {
            let target: id = msg_send![class, new];
            let _: () = msg_send![target, aboutButtonClicked: sender];
            let _: () = msg_send![target, release];
        }
    }

    #[test]
    fn target_records_the_clicked_buttons_tag() {
        unsafe {
            let button: id = msg_send![fake_button_class(), new];
            click(button);
            let _: () = msg_send![button, release];
        }
        assert_eq!(take_recorded_click(), Some(AboutButton::ResetSettings));
    }

    #[test]
    fn target_ignores_a_nil_sender() {
        click(nil);
        assert_eq!(take_recorded_click(), None);
    }
}
