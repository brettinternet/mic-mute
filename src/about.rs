//! The About window: the mute shortcut in use, the settings file's location, and the
//! version, with Open Settings and Reset Settings buttons.
//!
//! It is an `NSAlert`'s panel shown without `runModal`: a modal session would stall the
//! event-loop closure, and with it mute enforcement, the hotkey, and the settings reload.
//! The buttons only record the click; the closure takes it with `AboutWindow::take_click`,
//! which closes the window, and then runs `run_action`.
use crate::settings::Settings;
use crate::shortcuts::{current_status, ShortcutStatus};
use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker};
use objc2_app_kit::{NSAlert, NSApplication};
use objc2_foundation::{NSInteger, NSString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::thread::{self, JoinHandle};

/// The file `Settings::save` writes.
fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json"))
}

/// `open -t path`: opens `path` in the default text editor, the app macOS opens plain text
/// with, rather than in the app it opens `.json` files with, which may be Xcode.
fn open_in_text_editor(path: &Path) -> Command {
    let mut command = Command::new("/usr/bin/open");
    command.arg("-t").arg(path);
    command
}

/// Waits for `open`, which exits once the editor has the file, on a thread of its own: the
/// event loop never blocks, and no zombie process is left. The thread only waits and logs a
/// failure; it touches no AppKit object, takes no lock, and can't panic. A thread that can't
/// start leaves that one `open` unreaped.
fn wait_for_open(mut open: Child, path: PathBuf) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("open-settings".to_string())
        .spawn(move || match open.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                log::error!(
                    "Failed to open {}: open -t ended with {}",
                    path.display(),
                    status
                )
            }
            Err(e) => log::error!("Failed to wait for open -t {}: {}", path.display(), e),
        })
        .context("Failed to start the thread that waits for open")
}

/// Opens settings.json in the default text editor. A fresh install has no file yet and `open`
/// fails on a missing path, so the file is first written with `settings`, the settings in use.
/// An existing file opens as it is, even an invalid one.
pub fn open_settings_file(settings: &Settings) -> Result<()> {
    let path = settings_path().context("Failed to locate the settings directory")?;
    if matches!(path.try_exists(), Ok(false)) {
        settings
            .save()
            .context("Failed to create the settings file")?;
    }
    let open = open_in_text_editor(&path)
        .spawn()
        .with_context(|| format!("Failed to open {}", path.display()))?;
    // Dropping the handle detaches the thread, which ends with `open`.
    wait_for_open(open, path)?;
    Ok(())
}

/// The settings after About → Reset: every setting this build manages at its default except
/// `launch_at_login`, which mirrors the system login item. Resetting `excluded_devices` makes
/// Mic Mute mute every microphone again. Fields this build doesn't manage are kept. List a new
/// setting here if Reset should cover it.
fn reset_settings(current: &Settings) -> Settings {
    let defaults = Settings::default();
    Settings {
        mic_shortcut: defaults.mic_shortcut,
        show_in_dock: defaults.show_in_dock,
        show_popup: defaults.show_popup,
        excluded_devices: defaults.excluded_devices,
        // Inputs besides the tray menu.
        mode: defaults.mode,
        allow_url_unmute: defaults.allow_url_unmute,
        launch_state: defaults.launch_state,
        mute_on_lock: defaults.mute_on_lock,
        mute_on_sleep: defaults.mute_on_sleep,
        mute_on_display_sleep: defaults.mute_on_display_sleep,
        // Feedback on a mute change: sound and notification off, the popup as it has always
        // been, up while muted. macOS keeps its own answer about notifications.
        play_sound: defaults.play_sound,
        show_notification: defaults.show_notification,
        popup_position: defaults.popup_position,
        popup_size: defaults.popup_size,
        popup_mic_on_ms: defaults.popup_mic_on_ms,
        popup_mic_off_ms: defaults.popup_mic_off_ms,
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
            Self::OpenSettings => "Open settings.json",
            Self::ResetSettings => "Reset Settings",
        }
    }

    /// The button's `NSControl` tag: the response `runModal` returned for it, counting from
    /// `NSAlertFirstButtonReturn` (1000).
    fn tag(self) -> NSInteger {
        match self {
            Self::Ok => 1000,
            Self::OpenSettings => 1001,
            Self::ResetSettings => 1002,
        }
    }

    fn from_tag(tag: NSInteger) -> Option<Self> {
        Self::ALL.into_iter().find(|button| button.tag() == tag)
    }
}

/// Runs `button`'s action on `settings`, the settings in use: Open Settings opens the file,
/// and Reset Settings resets them with `reset_settings` through `Settings::update`, which
/// first reloads a file edited since the app last read it and logs a failed save. Returns
/// whether `settings` changed, so the caller applies them all.
pub fn run_action(button: AboutButton, settings: &mut Settings) -> Result<bool> {
    match button {
        AboutButton::Ok => Ok(false),
        AboutButton::OpenSettings => {
            open_settings_file(settings)?;
            Ok(false)
        }
        AboutButton::ResetSettings => {
            let update = settings.update(|s| *s = reset_settings(s));
            Ok(update.is_changed())
        }
    }
}

/// What `AboutButtonTarget` holds while no click waits: no button's tag.
const NO_CLICK: NSInteger = 0;

define_class!(
    /// The About buttons' target. Its action only records the click, in an atomic the event loop
    /// takes (`AboutWindow::take_click`), so a click sent on any thread counts.
    // SAFETY: NSObject has no subclassing requirements, and the class doesn't implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = AtomicIsize]
    struct AboutButtonTarget;

    impl AboutButtonTarget {
        /// Every About button's action. AppKit calls it from inside its event dispatch, so it
        /// only reads the sender's tag and records the click: no other AppKit call, no lock,
        /// and no panic, which can't unwind into Objective-C and would abort the app.
        #[unsafe(method(aboutButtonClicked:))]
        fn about_button_clicked(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else {
                return;
            };
            // SAFETY: the sender is the clicked NSButton; `-[NSControl tag]` returns an
            // NSInteger.
            let tag: NSInteger = unsafe { msg_send![sender, tag] };
            if let Some(button) = AboutButton::from_tag(tag) {
                self.record_click(button);
            }
        }
    }

    unsafe impl NSObjectProtocol for AboutButtonTarget {}
);

impl AboutButtonTarget {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(AtomicIsize::new(NO_CLICK));
        // SAFETY: NSObject's `init`, which the class inherits.
        unsafe { msg_send![super(this), init] }
    }

    /// Keeps the first click until the event loop takes it.
    fn record_click(&self, button: AboutButton) {
        // The exchange fails while an earlier click waits, which then stays.
        let _ = self.ivars().compare_exchange(
            NO_CLICK,
            button.tag(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// The click recorded since the last call, if any.
    fn take_click(&self) -> Option<AboutButton> {
        AboutButton::from_tag(self.ivars().swap(NO_CLICK, Ordering::AcqRel))
    }
}

/// A new alert showing `text`, laid out and centered, whose buttons send `aboutButtonClicked:`
/// to `target`.
///
/// # Safety
///
/// `target` must outlive the alert's buttons: controls don't retain their target.
unsafe fn new_alert(
    mtm: MainThreadMarker,
    text: &str,
    target: &AboutButtonTarget,
) -> Retained<NSAlert> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Mic Mute"));
    alert.setInformativeText(&NSString::from_str(text));

    let target: &AnyObject = target;
    for button in AboutButton::ALL {
        // The alert owns the button.
        let ns_button = alert.addButtonWithTitle(&NSString::from_str(button.title()));
        ns_button.setTag(button.tag());
        // Replaces the alert's own action, which only ends a modal session.
        // SAFETY: `target` handles `aboutButtonClicked:` and, by the caller's promise, outlives
        // the button.
        unsafe {
            ns_button.setTarget(Some(target));
            ns_button.setAction(Some(sel!(aboutButtonClicked:)));
        }
    }

    // runModal would lay out and position the panel; without it, do both here.
    alert.layout();
    let window = alert.window();
    // `AboutWindow::close` orders the panel out; nothing may free it behind the alert's back.
    // SAFETY: `false` only stops AppKit from releasing the window when it closes.
    unsafe { window.setReleasedWhenClosed(false) };
    window.center();
    alert
}

/// An alert on screen, with the text it shows.
struct OpenAlert {
    alert: Retained<NSAlert>,
    text: String,
}

/// The About window. Main thread only: it holds AppKit objects, so it is neither `Send` nor
/// `Sync`. Opening it never blocks the caller.
#[derive(Default)]
pub struct AboutWindow {
    open: Option<OpenAlert>,
    /// The buttons' target, created on the first `show`. Controls don't retain their target, so
    /// it outlives every alert: dropping the window closes the open alert first.
    target: Option<Retained<AboutButtonTarget>>,
}

impl AboutWindow {
    /// Shows About, or brings the open window to the front. An open window whose text is out
    /// of date (the shortcut in use has changed) is replaced.
    pub fn show(&mut self) -> Result<()> {
        let mtm = MainThreadMarker::new().context("About must open on the main thread")?;
        let text = about_text(&current_status(), env!("CARGO_PKG_VERSION"));
        if self.open.as_ref().is_some_and(|open| open.text != text) {
            self.close();
        }
        let target = self.target.get_or_insert_with(AboutButtonTarget::new);
        let open = self.open.get_or_insert_with(|| {
            // SAFETY: `target` lives until `AboutWindow` drops, after `close` has released
            // every alert.
            let alert = unsafe { new_alert(mtm, &text, target) };
            OpenAlert { alert, text }
        });
        let app = NSApplication::sharedApplication(mtm);
        // A tray click doesn't activate an accessory app; without this the window would open
        // behind other apps' windows, without keyboard focus.
        #[allow(deprecated)] // Its replacement, `activate`, needs macOS 14; Mic Mute runs on 11.
        app.activateIgnoringOtherApps(true);
        open.alert.window().makeKeyAndOrderFront(None);
        Ok(())
    }

    /// The button clicked since the last call, if any. Every button closes the window, so a
    /// click is returned after the window has closed.
    pub fn take_click(&mut self) -> Option<AboutButton> {
        let button = self.target.as_ref()?.take_click()?;
        // Only an open window has buttons to click; any other click is stale.
        let was_open = self.open.is_some();
        self.close();
        was_open.then_some(button)
    }

    /// Orders the window out and releases its alert; does nothing when About is closed.
    fn close(&mut self) {
        if let Some(open) = self.open.take() {
            open.alert.window().orderOut(None);
        }
    }
}

impl Drop for AboutWindow {
    /// Closes the open alert before the target goes, since its buttons point at the target.
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutConfig;
    use crate::shortcuts::{parse_shortcut, Shortcut, ShortcutProblem};
    use objc2::msg_send;
    use std::ffi::OsStr;

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
                "excluded_devices": [{{ "uid": "Teams UID", "name": "Teams Audio" }}],
                "mode": "push_to_talk",
                "allow_url_unmute": true,
                "launch_state": "last",
                "mute_on_lock": true,
                "mute_on_sleep": true,
                "mute_on_display_sleep": true,
                "play_sound": true,
                "show_notification": true,
                "popup_position": "top_right",
                "popup_size": "large",
                "popup_mic_on_ms": 3000,
                "popup_mic_off_ms": 2000,
                "future_setting": {{ "nested": [1, "two"] }}
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn reset_restores_shortcut_dock_popup_and_exclusion_defaults() {
        let reset = reset_settings(&customized_settings(true));
        let defaults = Settings::default();
        assert_eq!(reset.mic_shortcut, defaults.mic_shortcut);
        assert_eq!(reset.show_in_dock, defaults.show_in_dock);
        assert_eq!(reset.show_popup, defaults.show_popup);
        assert_eq!(reset.excluded_devices, defaults.excluded_devices);
    }

    #[test]
    fn reset_restores_toggle_mode_and_turns_url_unmute_off() {
        let reset = reset_settings(&customized_settings(true));
        assert_eq!(reset.mode, Settings::default().mode);
        assert!(!reset.allow_url_unmute);
    }

    #[test]
    fn reset_turns_automatic_mute_off() {
        let reset = reset_settings(&customized_settings(true));
        assert_eq!(reset.launch_state, Settings::default().launch_state);
        assert!(!reset.mute_on_lock);
        assert!(!reset.mute_on_sleep);
        assert!(!reset.mute_on_display_sleep);
    }

    #[test]
    fn reset_turns_sound_and_notification_off() {
        let reset = reset_settings(&customized_settings(true));
        assert!(!reset.play_sound);
        assert!(!reset.show_notification);
    }

    #[test]
    fn reset_restores_the_popup_layout_and_flash() {
        let reset = reset_settings(&customized_settings(true));
        let defaults = Settings::default();
        assert_eq!(reset.popup_position, defaults.popup_position);
        assert_eq!(reset.popup_size, defaults.popup_size);
        assert_eq!(reset.popup_mic_on_ms, defaults.popup_mic_on_ms);
        assert_eq!(reset.popup_mic_off_ms, defaults.popup_mic_off_ms);
        let saved = serde_json::to_value(&reset).unwrap();
        assert_eq!(
            saved["popup_mic_off_ms"],
            serde_json::Value::Null,
            "the popup stays up while muted again"
        );
    }

    #[test]
    fn reset_replaces_popup_values_this_build_does_not_accept() {
        let mut current = customized_settings(true);
        current.popup_position = serde_json::from_str("\"middle\"").unwrap();
        assert!(current.popup_position.problem().is_some());
        let reset = reset_settings(&current);
        assert!(reset.feedback_problems().is_empty());
        let saved = serde_json::to_value(&reset).unwrap();
        assert_eq!(saved["popup_position"], serde_json::json!("bottom_center"));
    }

    #[test]
    fn reset_keeps_launch_at_login_and_every_field_it_does_not_reset() {
        let reset_fields = [
            "mic_shortcut",
            "show_in_dock",
            "show_popup",
            "excluded_devices",
            "mode",
            "allow_url_unmute",
            "launch_state",
            "mute_on_lock",
            "mute_on_sleep",
            "mute_on_display_sleep",
            "play_sound",
            "show_notification",
            "popup_position",
            "popup_size",
            "popup_mic_on_ms",
            "popup_mic_off_ms",
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
        assert_eq!(titles, ["OK", "Open settings.json", "Reset Settings"]);
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
        let target = AboutButtonTarget::new();
        target.record_click(AboutButton::OpenSettings);
        target.record_click(AboutButton::ResetSettings);
        assert_eq!(target.take_click(), Some(AboutButton::OpenSettings));
        assert_eq!(target.take_click(), None);
    }

    #[test]
    fn ok_changes_no_settings() {
        let mut settings = customized_settings(true);
        let before = serde_json::to_value(&settings).unwrap();
        assert!(!run_action(AboutButton::Ok, &mut settings).unwrap());
        assert_eq!(serde_json::to_value(&settings).unwrap(), before);
    }

    define_class!(
        /// A stand-in for the clicked NSButton, whose `tag` is Reset Settings'.
        // SAFETY: NSObject has no subclassing requirements, and the class doesn't implement
        // `Drop`.
        #[unsafe(super(NSObject))]
        struct FakeAboutButton;

        impl FakeAboutButton {
            #[unsafe(method(tag))]
            fn tag(&self) -> NSInteger {
                AboutButton::ResetSettings.tag()
            }
        }
    );

    impl FakeAboutButton {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: NSObject's `init`, which the class inherits.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Sends `aboutButtonClicked:` from `sender` to `target`, as AppKit does on a click.
    fn click(target: &AboutButtonTarget, sender: Option<&AnyObject>) {
        // SAFETY: the action takes one object, which may be nil, and returns nothing.
        unsafe {
            let _: () = msg_send![target, aboutButtonClicked: sender];
        }
    }

    #[test]
    fn target_records_the_clicked_buttons_tag() {
        let target = AboutButtonTarget::new();
        let button = FakeAboutButton::new();
        let sender: &AnyObject = &button;
        click(&target, Some(sender));
        assert_eq!(target.take_click(), Some(AboutButton::ResetSettings));
    }

    #[test]
    fn target_ignores_a_nil_sender() {
        let target = AboutButtonTarget::new();
        click(&target, None);
        assert_eq!(target.take_click(), None);
    }

    #[test]
    fn a_click_without_an_open_window_is_stale() {
        let target = AboutButtonTarget::new();
        target.record_click(AboutButton::ResetSettings);
        let mut about = AboutWindow {
            open: None,
            target: Some(target),
        };
        assert_eq!(about.take_click(), None);
        assert_eq!(
            about.take_click(),
            None,
            "taking the stale click used it up"
        );
    }

    #[test]
    fn open_settings_opens_the_file_in_the_default_text_editor() {
        let path = Path::new("/Users/someone/Library/Application Support/mic-mute/settings.json");
        let command = open_in_text_editor(path);
        assert_eq!(command.get_program(), "/usr/bin/open");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, [OsStr::new("-t"), path.as_os_str()]);
    }

    #[test]
    fn waiting_for_open_reaps_it_even_when_it_fails() {
        // Stands in for `open`, which tests never run, failing as `open` can.
        let child = Command::new("/usr/bin/false")
            .spawn()
            .expect("false starts");
        let pid = libc::pid_t::try_from(child.id()).expect("the pid fits a pid_t");
        wait_for_open(child, PathBuf::from("/nonexistent/settings.json"))
            .expect("the thread starts")
            .join()
            .expect("the thread doesn't panic");
        // SAFETY: with WNOHANG and no status pointer, waitpid only reports whether `pid` is a
        // child of this process; it would reap one left unreaped.
        let found = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        let error = std::io::Error::last_os_error();
        assert_eq!(found, -1, "the child was left unreaped");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD), "{error}");
    }
}
