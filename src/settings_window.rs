//! The Settings window: every setting in settings.json as a native control, except
//! `excluded_devices`, which the tray's Microphones menu edits next to each device's state.
//!
//! It is a plain `NSWindow`, never run modally, so mute enforcement, the hotkey, and the
//! settings reload keep running while it is open. Like About's buttons, its controls only record
//! that they changed (`SettingsTarget`). The event loop takes the changes
//! (`SettingsWindow::take_events`), saves each one the way the tray toggles do, and then shows the
//! settings in use again (`SettingsWindow::refresh`), so changes made elsewhere (the tray,
//! About → Reset Settings, a hand edit of the file) show here too.
//!
//! Change… records a new mute shortcut. While it records, the mute hotkey is unregistered
//! (`Shortcuts::suspend`), so pressing the current shortcut reaches the recorder instead of
//! toggling mute, and a local `NSEvent` monitor takes the window's key events: the first key
//! that makes, with the modifiers held, a shortcut `record_press` accepts becomes the shortcut.
//! Esc, Cancel, another control, the window losing focus or closing, and 30 seconds without a
//! key press end the recording, and the event loop registers the hotkey again
//! (`Shortcuts::resume`).
use crate::settings::{
    Fallback, LaunchState, Lenient, LoadError, MicOffFlash, MicOnFlash, PopupPosition, PopupSize,
    Settings, ShortcutConfig, ShortcutMode,
};
use crate::shortcuts::{current_status, key_label, parse_shortcut, Shortcut, ShortcutStatus};
use anyhow::{Context, Result};
use block2::RcBlock;
use global_hotkey::hotkey::{Code, Modifiers};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSFont,
    NSGridCell, NSGridCellPlacement, NSGridRow, NSGridRowAlignment, NSGridView, NSLayoutAttribute,
    NSPopUpButton, NSStackView, NSStepper, NSTextField, NSUserInterfaceLayoutOrientation, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSInteger, NSPoint, NSRange, NSRect, NSSize, NSString,
};
use std::cell::Cell;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The window's title.
const WINDOW_TITLE: &str = "Mic Mute Settings";
/// How wide the notice, the footer, and the notes under the controls wrap, in points.
const WIDE_TEXT_WIDTH: f64 = 440.;
const NOTE_WIDTH: f64 = 320.;
/// The margin around the window's content, in points.
const CONTENT_MARGIN: f64 = 20.;
/// How far the "Mic on" and "Mic off" steppers move per click, in milliseconds.
const MILLIS_STEP_MS: f64 = 100.;
/// The milliseconds the disabled "Mic off" field shows while the popup stays up while muted,
/// which checking Hide “Mic off” after sets: one second, as long as "Mic on" shows by default.
const MIC_OFF_DEFAULT_MS: u64 = MicOnFlash::DEFAULT_MS;
const URL_UNMUTE_WARNING: &str = "Any app, and any web page through your browser, can open a \
    com.brettinternet.mic-mute:// URL. With this on, any of them can unmute your microphones.";
const FOOTER_TEXT: &str = "Changes apply at once and are saved to \
    ~/Library/Application Support/mic-mute/settings.json. Choose which microphones Mic Mute \
    mutes in the tray's Microphones menu.";

/// A setting the window changed, with its new value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsChange {
    LaunchAtLogin(bool),
    ShowInDock(bool),
    Shortcut(ShortcutConfig),
    Mode(ShortcutMode),
    ShowPopup(bool),
    PopupPosition(PopupPosition),
    PopupSize(PopupSize),
    /// Milliseconds; more than `MicOnFlash::MAX_MS` is capped.
    PopupMicOnMs(u64),
    /// Milliseconds, more than `MicOffFlash::MAX_MS` capped; `None` keeps the popup up while
    /// muted.
    PopupMicOffMs(Option<u64>),
    PlaySound(bool),
    ShowNotification(bool),
    LaunchState(LaunchState),
    MuteOnLock(bool),
    MuteOnSleep(bool),
    MuteOnDisplaySleep(bool),
    AllowUrlUnmute(bool),
}

impl SettingsChange {
    /// Sets the changed setting in `settings`. Returns whether that changed `settings`, so an
    /// unchanged value (a pop-up item chosen again) isn't saved.
    pub fn apply_to(&self, settings: &mut Settings) -> bool {
        match self {
            Self::LaunchAtLogin(on) => replace(&mut settings.launch_at_login, *on),
            Self::ShowInDock(on) => replace(&mut settings.show_in_dock, *on),
            Self::Shortcut(config) => replace(&mut settings.mic_shortcut, config.clone()),
            Self::Mode(mode) => replace(&mut settings.mode, *mode),
            Self::ShowPopup(on) => replace(&mut settings.show_popup, *on),
            // A chosen value replaces one the file sets that this build doesn't accept.
            Self::PopupPosition(position) => {
                replace(&mut settings.popup_position, Lenient::new(*position))
            }
            Self::PopupSize(size) => replace(&mut settings.popup_size, Lenient::new(*size)),
            Self::PopupMicOnMs(ms) => replace(
                &mut settings.popup_mic_on_ms,
                Lenient::new(MicOnFlash::from_millis(*ms)),
            ),
            Self::PopupMicOffMs(ms) => replace(
                &mut settings.popup_mic_off_ms,
                Lenient::new(ms.map_or(MicOffFlash::WHILE_MUTED, MicOffFlash::from_millis)),
            ),
            Self::PlaySound(on) => replace(&mut settings.play_sound, *on),
            Self::ShowNotification(on) => replace(&mut settings.show_notification, *on),
            Self::LaunchState(state) => replace(&mut settings.launch_state, *state),
            Self::MuteOnLock(on) => replace(&mut settings.mute_on_lock, *on),
            Self::MuteOnSleep(on) => replace(&mut settings.mute_on_sleep, *on),
            Self::MuteOnDisplaySleep(on) => replace(&mut settings.mute_on_display_sleep, *on),
            Self::AllowUrlUnmute(on) => replace(&mut settings.allow_url_unmute, *on),
        }
    }
}

/// Stores `value` in `slot`; returns whether it differed.
fn replace<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

/// What the event loop has to do for the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowEvent {
    /// Save and apply the change.
    Changed(SettingsChange),
    /// Open settings.json (the invalid-file notice's button).
    OpenSettingsFile,
    /// The recorder started: unregister the mute hotkey, so pressing it reaches the recorder.
    RecordingStarted,
    /// The recorder stopped: register the mute hotkey again. A recorded shortcut follows as
    /// `Changed`, so it replaces the hotkey that is back by then.
    RecordingEnded,
}

/// Why settings.json can't be used, as the notice at the top of the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileProblem {
    /// The file can't be read, rather than being invalid.
    is_unreadable: bool,
    /// The error with the file's path and, for invalid JSON, the line and column.
    detail: String,
    fallback: Fallback,
}

impl FileProblem {
    pub fn new(err: &LoadError, fallback: Fallback) -> Self {
        Self {
            is_unreadable: err.position().is_none(),
            detail: err.to_string(),
            fallback,
        }
    }

    fn title(&self) -> &'static str {
        if self.is_unreadable {
            "settings.json can't be read"
        } else {
            "settings.json is invalid"
        }
    }

    /// What runs meanwhile, and what happens to changes made in the window.
    fn note(&self) -> String {
        let running_on = match self.fallback {
            Fallback::Defaults => "Mic Mute is using the default settings.",
            Fallback::Previous => "Mic Mute keeps the settings it had before the file broke.",
        };
        format!(
            "{running_on} Changes made here apply at once, but Mic Mute doesn't save them while \
             the file is broken. Fix the file, or delete it to start from the defaults: within \
             two seconds, its settings then replace the changes made here."
        )
    }
}

// The pop-up buttons' items, in order. NSPopUpButton needs every title in a menu to differ.
const MODE_CHOICES: [(ShortcutMode, &str); 2] = [
    (ShortcutMode::Toggle, "Toggle: each press mutes or unmutes"),
    (ShortcutMode::PushToTalk, "Push to talk: hold to unmute"),
];
const POSITION_CHOICES: [(PopupPosition, &str); 6] = [
    (PopupPosition::TopLeft, "Top left"),
    (PopupPosition::TopCenter, "Top center"),
    (PopupPosition::TopRight, "Top right"),
    (PopupPosition::BottomLeft, "Bottom left"),
    (PopupPosition::BottomCenter, "Bottom center"),
    (PopupPosition::BottomRight, "Bottom right"),
];
const SIZE_CHOICES: [(PopupSize, &str); 3] = [
    (PopupSize::Small, "Small"),
    (PopupSize::Medium, "Medium"),
    (PopupSize::Large, "Large"),
];
const LAUNCH_STATE_CHOICES: [(LaunchState, &str); 3] = [
    (LaunchState::Keep, "Leave the microphones as they are"),
    (LaunchState::Muted, "Mute the microphones"),
    (LaunchState::Last, "Mute them if they were muted at quit"),
];

/// The item of `choices` that shows `value`; the first when none does.
fn index_of<T: Copy + PartialEq>(choices: &[(T, &str)], value: T) -> usize {
    choices
        .iter()
        .position(|&(choice, _)| choice == value)
        .unwrap_or(0)
}

/// The value of item `index`, an `NSPopUpButton` selection; `None` for -1, no selection.
fn choice_at<T: Copy>(choices: &[(T, &str)], index: NSInteger) -> Option<T> {
    let index = usize::try_from(index).ok()?;
    choices.get(index).map(|&(choice, _)| choice)
}

fn titles<'a, T>(choices: &'a [(T, &'static str)]) -> impl Iterator<Item = &'static str> + 'a {
    choices.iter().map(|&(_, title)| title)
}

/// The milliseconds typed into the "Mic on" field: a whole number, spaces around it allowed.
/// `None` for anything else, which leaves the setting as it is.
fn parse_millis(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// The "Mic on" flash in whole milliseconds.
fn mic_on_millis(flash: MicOnFlash) -> u64 {
    u64::try_from(flash.duration().as_millis()).unwrap_or(MicOnFlash::MAX_MS)
}

/// The "Mic off" flash in whole milliseconds; `None` while the popup stays up while muted.
fn mic_off_millis(flash: MicOffFlash) -> Option<u64> {
    flash
        .duration()
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(MicOffFlash::MAX_MS))
}

/// The mute shortcut in use, as menus show it.
fn shortcut_text(status: &ShortcutStatus) -> String {
    status
        .registered
        .map_or_else(|| "None".to_string(), |shortcut| shortcut.to_string())
}

/// Why the configured shortcut isn't the one in use, and what works instead; `None` when it is.
fn shortcut_note(status: &ShortcutStatus) -> Option<String> {
    let problem = status.problem.as_ref()?;
    let fallback = match status.registered {
        Some(registered) => format!("{registered} is used instead."),
        None => "Mute from the tray menu.".to_string(),
    };
    Some(format!("{problem}. {fallback}"))
}

/// The popup values settings.json sets that this build doesn't use as written; `None` when the
/// file has none.
fn popup_note(settings: &Settings) -> Option<String> {
    let problems = settings.feedback_problems();
    let lines: Vec<String> = problems
        .iter()
        .map(|problem| format!("settings.json: {problem}."))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

// The shortcut recorder.

/// How long the recorder waits for a shortcut, counted again from each rejected key, before it
/// gives up and the hotkey comes back.
const RECORDING_TIMEOUT: Duration = Duration::from_secs(30);
/// `kVK_Escape`: without modifiers, it cancels the recording.
const ESCAPE_KEY_CODE: u16 = 0x35;
/// `kVK_ANSI_W`: ⌘W closes the window.
const W_KEY_CODE: u16 = 0x0d;
/// How many key presses, and how many edit commands, the monitor keeps between two passes of
/// the event loop.
const QUEUE_LEN: usize = 4;
const RECORD_TITLE: &str = "Change…";
const CANCEL_RECORDING_TITLE: &str = "Cancel";
const RECORDING_PROMPT: &str = "Type the new shortcut…";
const RECORDING_HINT: &str = "Press the new shortcut, or Esc to cancel.";

/// macOS virtual key codes (HIToolbox's `kVK_*`) and the keys global-hotkey registers for them:
/// the inverse of global-hotkey 0.8's `key_to_scancode`, so a recorded key registers as the
/// same physical key. Caps Lock, which AppKit reports only as a modifier change, and
/// PrintScreen (0x46, which no Mac keyboard sends) are left out.
const KEY_CODES: &[(u16, Code)] = &[
    (0x00, Code::KeyA),
    (0x01, Code::KeyS),
    (0x02, Code::KeyD),
    (0x03, Code::KeyF),
    (0x04, Code::KeyH),
    (0x05, Code::KeyG),
    (0x06, Code::KeyZ),
    (0x07, Code::KeyX),
    (0x08, Code::KeyC),
    (0x09, Code::KeyV),
    (0x0b, Code::KeyB),
    (0x0c, Code::KeyQ),
    (0x0d, Code::KeyW),
    (0x0e, Code::KeyE),
    (0x0f, Code::KeyR),
    (0x10, Code::KeyY),
    (0x11, Code::KeyT),
    (0x12, Code::Digit1),
    (0x13, Code::Digit2),
    (0x14, Code::Digit3),
    (0x15, Code::Digit4),
    (0x16, Code::Digit6),
    (0x17, Code::Digit5),
    (0x18, Code::Equal),
    (0x19, Code::Digit9),
    (0x1a, Code::Digit7),
    (0x1b, Code::Minus),
    (0x1c, Code::Digit8),
    (0x1d, Code::Digit0),
    (0x1e, Code::BracketRight),
    (0x1f, Code::KeyO),
    (0x20, Code::KeyU),
    (0x21, Code::BracketLeft),
    (0x22, Code::KeyI),
    (0x23, Code::KeyP),
    (0x24, Code::Enter),
    (0x25, Code::KeyL),
    (0x26, Code::KeyJ),
    (0x27, Code::Quote),
    (0x28, Code::KeyK),
    (0x29, Code::Semicolon),
    (0x2a, Code::Backslash),
    (0x2b, Code::Comma),
    (0x2c, Code::Slash),
    (0x2d, Code::KeyN),
    (0x2e, Code::KeyM),
    (0x2f, Code::Period),
    (0x30, Code::Tab),
    (0x31, Code::Space),
    (0x32, Code::Backquote),
    (0x33, Code::Backspace),
    (0x35, Code::Escape),
    (0x40, Code::F17),
    (0x41, Code::NumpadDecimal),
    (0x43, Code::NumpadMultiply),
    (0x45, Code::NumpadAdd),
    (0x47, Code::NumLock),
    (0x48, Code::AudioVolumeUp),
    (0x49, Code::AudioVolumeDown),
    (0x4a, Code::AudioVolumeMute),
    (0x4b, Code::NumpadDivide),
    (0x4c, Code::NumpadEnter),
    (0x4e, Code::NumpadSubtract),
    (0x4f, Code::F18),
    (0x50, Code::F19),
    (0x51, Code::NumpadEqual),
    (0x52, Code::Numpad0),
    (0x53, Code::Numpad1),
    (0x54, Code::Numpad2),
    (0x55, Code::Numpad3),
    (0x56, Code::Numpad4),
    (0x57, Code::Numpad5),
    (0x58, Code::Numpad6),
    (0x59, Code::Numpad7),
    (0x5a, Code::F20),
    (0x5b, Code::Numpad8),
    (0x5c, Code::Numpad9),
    (0x60, Code::F5),
    (0x61, Code::F6),
    (0x62, Code::F7),
    (0x63, Code::F3),
    (0x64, Code::F8),
    (0x65, Code::F9),
    (0x67, Code::F11),
    (0x69, Code::F13),
    (0x6a, Code::F16),
    (0x6b, Code::F14),
    (0x6d, Code::F10),
    (0x6f, Code::F12),
    (0x71, Code::F15),
    (0x72, Code::Insert),
    (0x73, Code::Home),
    (0x74, Code::PageUp),
    (0x75, Code::Delete),
    (0x76, Code::F4),
    (0x77, Code::End),
    (0x78, Code::F2),
    (0x79, Code::PageDown),
    (0x7a, Code::F1),
    (0x7b, Code::ArrowLeft),
    (0x7c, Code::ArrowRight),
    (0x7d, Code::ArrowDown),
    (0x7e, Code::ArrowUp),
];

/// The key at `key_code`; `None` for a key global-hotkey can't register.
fn code_for_key(key_code: u16) -> Option<Code> {
    KEY_CODES
        .iter()
        .find(|&&(code, _)| code == key_code)
        .map(|&(_, key)| key)
}

/// The modifiers a shortcut can hold, in Apple's order, with their settings.json names.
const MODIFIERS: [(NSEventModifierFlags, &str, &str); 4] = [
    (NSEventModifierFlags::Control, "ctrl", "⌃"),
    (NSEventModifierFlags::Option, "alt", "⌥"),
    (NSEventModifierFlags::Shift, "shift", "⇧"),
    (NSEventModifierFlags::Command, "meta", "⌘"),
];

/// The settings.json names of the shortcut modifiers in `flags`, in Apple's order. Caps Lock,
/// Fn, and the keypad flag don't count.
fn modifier_names(flags: NSEventModifierFlags) -> Vec<String> {
    MODIFIERS
        .iter()
        .filter(|&&(flag, _, _)| flags.contains(flag))
        .map(|&(_, name, _)| name.to_string())
        .collect()
}

/// The shortcut modifiers in `flags` as menus show them, e.g. `⌃⌥`.
fn modifier_glyphs(flags: NSEventModifierFlags) -> String {
    MODIFIERS
        .iter()
        .filter(|&&(flag, _, _)| flags.contains(flag))
        .map(|&(_, _, glyph)| glyph)
        .collect()
}

/// Whether `flags` hold ⌘ and no other shortcut modifier.
fn is_command_only(flags: NSEventModifierFlags) -> bool {
    modifier_names(flags) == ["meta"]
}

/// A key the recorder caught: its virtual key code and the modifiers held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyPress {
    key_code: u16,
    modifiers: NSEventModifierFlags,
}

/// What a key press means to the recorder.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recorded {
    /// The new shortcut, as settings.json writes it.
    Shortcut(ShortcutConfig),
    /// Esc without modifiers.
    Cancel,
    /// Why the press can't be the shortcut; the recorder keeps waiting.
    Rejected(String),
}

/// Whether `code` types a character or edits text: a letter, a digit, punctuation, the space
/// bar, the keypad, Tab, Return, Delete, and the keys that move the insertion point. ⇧ and ⌥
/// change what such a key does in text rather than make it a shortcut.
fn is_text_key(code: Code) -> bool {
    key_label(code).chars().count() == 1
        || matches!(
            code,
            Code::Backquote
                | Code::Minus
                | Code::Equal
                | Code::BracketLeft
                | Code::BracketRight
                | Code::Backslash
                | Code::Semicolon
                | Code::Quote
                | Code::Comma
                | Code::Period
                | Code::Slash
                | Code::Space
                | Code::Tab
                | Code::Enter
                | Code::NumpadEnter
                | Code::Backspace
                | Code::Delete
                | Code::ArrowLeft
                | Code::ArrowRight
                | Code::ArrowUp
                | Code::ArrowDown
                | Code::Home
                | Code::End
                | Code::PageUp
                | Code::PageDown
                | Code::Numpad0
                | Code::Numpad1
                | Code::Numpad2
                | Code::Numpad3
                | Code::Numpad4
                | Code::Numpad5
                | Code::Numpad6
                | Code::Numpad7
                | Code::Numpad8
                | Code::Numpad9
                | Code::NumpadDecimal
                | Code::NumpadAdd
                | Code::NumpadSubtract
                | Code::NumpadMultiply
                | Code::NumpadDivide
                | Code::NumpadEqual
        )
}

/// Whether `shortcut` holds only ⇧ and ⌥ on a text key: as a global hotkey, it would take that
/// key from typing and editing in every app (⌥2 types ™ on a US layout and @ on others; ⇧↩
/// starts a new line in chat apps).
fn takes_text_key(shortcut: Shortcut) -> bool {
    (Modifiers::SHIFT | Modifiers::ALT).contains(shortcut.modifiers())
        && is_text_key(shortcut.code())
}

/// `text` as a sentence's start: its first letter capitalized.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// What `press` means while recording. It is the shortcut only when `parse_shortcut` accepts
/// it, and it doesn't take a text key from every app.
fn record_press(press: KeyPress) -> Recorded {
    let modifiers = modifier_names(press.modifiers);
    if press.key_code == ESCAPE_KEY_CODE && modifiers.is_empty() {
        return Recorded::Cancel;
    }
    let Some(code) = code_for_key(press.key_code) else {
        return Recorded::Rejected("Mic Mute can't use that key in a shortcut".to_string());
    };
    let config = ShortcutConfig {
        modifiers,
        key: key_label(code),
    };
    match parse_shortcut(&config) {
        Err(e) => Recorded::Rejected(capitalized(&format!("{e:#}"))),
        Ok(shortcut) if takes_text_key(shortcut) => Recorded::Rejected(format!(
            "{shortcut} types or edits text in every app; add ⌃ or ⌘"
        )),
        Ok(_) => Recorded::Shortcut(config),
    }
}

/// A recording in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Recording {
    /// When the recorder gives up.
    deadline: Instant,
    /// The modifiers held now, shown until a key completes the shortcut.
    held: NSEventModifierFlags,
    /// Why the last key can't be the shortcut.
    rejection: Option<String>,
}

impl Recording {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + RECORDING_TIMEOUT,
            held: NSEventModifierFlags::empty(),
            rejection: None,
        }
    }
}

/// What the recorder does after a pass of the event loop.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Keep recording.
    Wait,
    /// Stop without a shortcut: Esc, or the timeout.
    Stop,
    /// Stop with this shortcut.
    Record(ShortcutConfig),
}

/// Runs the key `presses` and the latest `held` modifiers caught since the last pass through
/// `recording` at `now`.
fn advance(
    recording: &mut Recording,
    presses: &[KeyPress],
    held: Option<NSEventModifierFlags>,
    now: Instant,
) -> Step {
    if let Some(held) = held {
        recording.held = held;
    }
    for &press in presses {
        match record_press(press) {
            Recorded::Shortcut(config) => return Step::Record(config),
            Recorded::Cancel => return Step::Stop,
            Recorded::Rejected(reason) => {
                recording.rejection = Some(reason);
                recording.deadline = now + RECORDING_TIMEOUT;
            }
        }
    }
    if now >= recording.deadline {
        return Step::Stop;
    }
    Step::Wait
}

/// The shortcut row's text, its note and whether that is an error, and the button's title.
fn shortcut_row(
    status: &ShortcutStatus,
    recording: Option<&Recording>,
) -> (String, Option<String>, bool, &'static str) {
    let Some(recording) = recording else {
        let note = shortcut_note(status);
        return (shortcut_text(status), note, true, RECORD_TITLE);
    };
    let text = if recording.held.is_empty() {
        RECORDING_PROMPT.to_string()
    } else {
        format!("{}…", modifier_glyphs(recording.held))
    };
    let (note, is_error) = match &recording.rejection {
        Some(reason) => (format!("{reason}. {RECORDING_HINT}"), true),
        None => (RECORDING_HINT.to_string(), false),
    };
    (text, Some(note), is_error, CANCEL_RECORDING_TITLE)
}

/// A key event in the window, with only what is valid to read for its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyEvent {
    Down {
        key_code: u16,
        modifiers: NSEventModifierFlags,
        is_repeat: bool,
    },
    ModifiersChanged(NSEventModifierFlags),
}

/// A text-editing shortcut the window handles itself: the app has no Edit menu, which would
/// otherwise send it to the focused text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditCommand {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl EditCommand {
    /// The command ⌘ and `key_code` (`kVK_ANSI_X`, `C`, `V`, `A`) give.
    fn for_key(key_code: u16) -> Option<Self> {
        match key_code {
            0x07 => Some(Self::Cut),
            0x08 => Some(Self::Copy),
            0x09 => Some(Self::Paste),
            0x00 => Some(Self::SelectAll),
            _ => None,
        }
    }

    /// The responder action an Edit menu item would send.
    fn action(self) -> Sel {
        match self {
            Self::Cut => sel!(cut:),
            Self::Copy => sel!(copy:),
            Self::Paste => sel!(paste:),
            Self::SelectAll => sel!(selectAll:),
        }
    }
}

/// What the key-event monitor does with a key event in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Catch {
    /// A key for the recorder.
    Press(KeyPress),
    /// The modifiers held while recording.
    Held(NSEventModifierFlags),
    /// A key repeat while recording: nobody gets it.
    Swallow,
    /// ⌘W: close the window.
    Close,
    /// ⌘X, ⌘C, ⌘V, or ⌘A outside a recording: edit the focused text.
    Edit(EditCommand),
    /// Not the monitor's business: AppKit handles it as usual.
    Pass,
}

impl Catch {
    /// Whether AppKit must not handle the event too.
    fn swallows(self) -> bool {
        !matches!(self, Self::Held(_) | Self::Pass)
    }
}

/// What the key-event monitor does with `event`, a key event in the window. ⌘W closes the
/// window even while recording: as a global shortcut, it would take Close from every app.
fn catch(event: KeyEvent, is_recording: bool) -> Catch {
    match event {
        KeyEvent::ModifiersChanged(modifiers) if is_recording => Catch::Held(modifiers),
        KeyEvent::ModifiersChanged(_) => Catch::Pass,
        KeyEvent::Down {
            key_code: W_KEY_CODE,
            modifiers,
            ..
        } if is_command_only(modifiers) => Catch::Close,
        KeyEvent::Down {
            is_repeat: true, ..
        } if is_recording => Catch::Swallow,
        KeyEvent::Down {
            key_code,
            modifiers,
            ..
        } if is_recording => Catch::Press(KeyPress {
            key_code,
            modifiers,
        }),
        KeyEvent::Down {
            key_code,
            modifiers,
            ..
        } if is_command_only(modifiers) => {
            EditCommand::for_key(key_code).map_or(Catch::Pass, Catch::Edit)
        }
        KeyEvent::Down { .. } => Catch::Pass,
    }
}

/// What the window shows: one field per control, so `refresh` touches only what changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    problem: Option<FileProblem>,
    launch_at_login: bool,
    launch_at_login_title: &'static str,
    show_in_dock: bool,
    shortcut: String,
    shortcut_note: Option<String>,
    is_shortcut_note_error: bool,
    record_title: &'static str,
    mode: usize,
    show_popup: bool,
    position: usize,
    size: usize,
    mic_on_ms: u64,
    /// `None` while the popup stays up while muted, which unchecks Hide “Mic off” after.
    mic_off_ms: Option<u64>,
    popup_note: Option<String>,
    play_sound: bool,
    show_notification: bool,
    launch_state: usize,
    mute_on_lock: bool,
    mute_on_sleep: bool,
    mute_on_display_sleep: bool,
    allow_url_unmute: bool,
}

/// The window's contents for `settings`, the shortcut's `status`, the file's `problem`, and
/// the `recording` in progress. `needs_approval`: macOS waits for the user to approve the login
/// item.
fn view_for(
    settings: &Settings,
    status: &ShortcutStatus,
    problem: Option<&FileProblem>,
    needs_approval: bool,
    recording: Option<&Recording>,
) -> View {
    let (shortcut, shortcut_note, is_shortcut_note_error, record_title) =
        shortcut_row(status, recording);
    View {
        problem: problem.cloned(),
        launch_at_login: settings.launch_at_login,
        launch_at_login_title: crate::launch_at_login::menu_title(
            settings.launch_at_login && needs_approval,
        ),
        show_in_dock: settings.show_in_dock,
        shortcut,
        shortcut_note,
        is_shortcut_note_error,
        record_title,
        mode: index_of(&MODE_CHOICES, settings.mode),
        show_popup: settings.show_popup,
        position: index_of(&POSITION_CHOICES, settings.popup_position.value()),
        size: index_of(&SIZE_CHOICES, settings.popup_size.value()),
        mic_on_ms: mic_on_millis(settings.popup_mic_on_ms.value()),
        mic_off_ms: mic_off_millis(settings.popup_mic_off_ms.value()),
        popup_note: popup_note(settings),
        play_sound: settings.play_sound,
        show_notification: settings.show_notification,
        launch_state: index_of(&LAUNCH_STATE_CHOICES, settings.launch_state),
        mute_on_lock: settings.mute_on_lock,
        mute_on_sleep: settings.mute_on_sleep,
        mute_on_display_sleep: settings.mute_on_display_sleep,
        allow_url_unmute: settings.allow_url_unmute,
    }
}

/// The window's controls that send an action. A control's tag is its position in `ALL` plus
/// one, so none has the default tag, 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    LaunchAtLogin,
    ShowInDock,
    RecordShortcut,
    Mode,
    ShowPopup,
    PopupPosition,
    PopupSize,
    MicOnField,
    MicOnStepper,
    /// The Hide “Mic off” after checkbox.
    HideMicOff,
    MicOffField,
    MicOffStepper,
    PlaySound,
    ShowNotification,
    LaunchState,
    MuteOnLock,
    MuteOnSleep,
    MuteOnDisplaySleep,
    AllowUrlUnmute,
    OpenSettingsFile,
}

impl Control {
    const ALL: [Self; 20] = [
        Self::LaunchAtLogin,
        Self::ShowInDock,
        Self::RecordShortcut,
        Self::Mode,
        Self::ShowPopup,
        Self::PopupPosition,
        Self::PopupSize,
        Self::MicOnField,
        Self::MicOnStepper,
        Self::HideMicOff,
        Self::MicOffField,
        Self::MicOffStepper,
        Self::PlaySound,
        Self::ShowNotification,
        Self::LaunchState,
        Self::MuteOnLock,
        Self::MuteOnSleep,
        Self::MuteOnDisplaySleep,
        Self::AllowUrlUnmute,
        Self::OpenSettingsFile,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|&control| control == self)
            .unwrap_or(0)
    }

    fn tag(self) -> NSInteger {
        // At most `ALL.len()`, so it always fits.
        NSInteger::try_from(self.index() + 1).unwrap_or(NSInteger::MAX)
    }

    fn from_tag(tag: NSInteger) -> Option<Self> {
        Self::ALL.into_iter().find(|control| control.tag() == tag)
    }

    /// The control's bit in `SettingsTarget`'s ivar.
    fn bit(self) -> u32 {
        1 << self.index()
    }
}

/// What the key-event monitor shares with the window: whether the recorder records, and what
/// the monitor caught for the event loop. A local monitor runs on the main thread, inside
/// AppKit's event dispatch, as the event loop's closure does, so plain `Cell`s do.
#[derive(Debug, Default)]
struct KeyCatcher {
    is_recording: Cell<bool>,
    /// The key presses caught since the loop last took them, oldest first; once it is full,
    /// later ones are dropped.
    presses: Cell<[Option<KeyPress>; QUEUE_LEN]>,
    /// The edit commands caught since the loop last took them, the same way.
    edits: Cell<[Option<EditCommand>; QUEUE_LEN]>,
    /// The modifiers held at the last modifier change while recording.
    held: Cell<Option<NSEventModifierFlags>>,
    /// ⌘W was pressed in the window.
    is_close_requested: Cell<bool>,
}

impl KeyCatcher {
    /// What the monitor does with `event`, a key event in the window. Keeps what the event
    /// loop acts on until it takes it.
    fn catch(&self, event: KeyEvent) -> Catch {
        let caught = catch(event, self.is_recording.get());
        match caught {
            Catch::Press(press) => enqueue(&self.presses, press),
            Catch::Held(modifiers) => self.held.set(Some(modifiers)),
            Catch::Close => self.is_close_requested.set(true),
            Catch::Edit(command) => enqueue(&self.edits, command),
            Catch::Swallow | Catch::Pass => {}
        }
        caught
    }

    fn take_presses(&self) -> Vec<KeyPress> {
        self.presses.take().into_iter().flatten().collect()
    }

    fn take_edits(&self) -> Vec<EditCommand> {
        self.edits.take().into_iter().flatten().collect()
    }

    fn take_held(&self) -> Option<NSEventModifierFlags> {
        self.held.take()
    }

    fn take_close_request(&self) -> bool {
        self.is_close_requested.take()
    }

    /// Starts or stops handing the window's keys to the recorder. Either way it drops the keys
    /// caught so far, which belong to no recording.
    fn set_recording(&self, is_recording: bool) {
        self.is_recording.set(is_recording);
        self.presses.take();
        self.held.take();
    }
}

/// Adds `item` to the first free slot of `queue`; drops it when `queue` is full.
fn enqueue<T: Copy>(queue: &Cell<[Option<T>; QUEUE_LEN]>, item: T) {
    let mut items = queue.get();
    if let Some(slot) = items.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(item);
        queue.set(items);
    }
}

/// `event` as the recorder sees it; `None` for any other type. `keyCode` and `isARepeat` raise
/// an exception on other events, so they're read for key-down events only.
fn key_event(event: &NSEvent) -> Option<KeyEvent> {
    let modifiers = event.modifierFlags();
    let event_type = event.r#type();
    if event_type == NSEventType::KeyDown {
        Some(KeyEvent::Down {
            key_code: event.keyCode(),
            modifiers,
            is_repeat: event.isARepeat(),
        })
    } else if event_type == NSEventType::FlagsChanged {
        Some(KeyEvent::ModifiersChanged(modifiers))
    } else {
        None
    }
}

/// Adds the app's local key-event monitor for `window`. It hands the window's key events to
/// `catcher` and swallows the ones it takes: every key press while the recorder records, ⌘W,
/// which closes the window, and the edit commands. It runs inside AppKit's event dispatch, so it only reads the event
/// and updates `catcher`: no other AppKit call, no locks, no panics. Events of other windows
/// pass untouched.
fn add_key_monitor(
    window: &NSWindow,
    catcher: Rc<KeyCatcher>,
    mtm: MainThreadMarker,
) -> Option<Retained<AnyObject>> {
    let window: *const NSWindow = window;
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the monitor a valid event for the duration of the call.
        let ns_event = unsafe { event.as_ref() };
        let is_window_event = ns_event
            .window(mtm)
            .is_some_and(|event_window| ptr::eq(Retained::as_ptr(&event_window), window));
        let caught = match key_event(ns_event) {
            Some(key) if is_window_event => catcher.catch(key),
            _ => Catch::Pass,
        };
        if caught.swallows() {
            ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    let mask = NSEventMask::KeyDown | NSEventMask::FlagsChanged;
    // SAFETY: the handler returns the event it was handed, or null to swallow it.
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `SettingsTarget` has no `Drop`.
    #[unsafe(super(NSObject))]
    // The controls whose action AppKit sent since the event loop last took them, one bit per
    // `Control`.
    #[ivars = AtomicU32]
    struct SettingsTarget;

    impl SettingsTarget {
        /// Every control's action. AppKit sends it from inside its event dispatch, so it only
        /// records which control sent it: no AppKit call besides reading the sender's tag, no
        /// locks, and no panics, which can't unwind into Objective-C.
        #[unsafe(method(settingChanged:))]
        fn setting_changed(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else {
                return;
            };
            // SAFETY: the senders are the window's controls; -[NSControl tag] returns an
            // NSInteger.
            let tag: NSInteger = unsafe { msg_send![sender, tag] };
            if let Some(control) = Control::from_tag(tag) {
                self.ivars().fetch_or(control.bit(), Ordering::AcqRel);
            }
        }
    }
);

impl SettingsTarget {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(AtomicU32::new(0));
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// The controls that sent their action since the last call, in `Control::ALL` order.
    fn take_changed(&self) -> Vec<Control> {
        let bits = self.ivars().swap(0, Ordering::AcqRel);
        Control::ALL
            .into_iter()
            .filter(|control| bits & control.bit() != 0)
            .collect()
    }
}

/// Every control's action: `-[SettingsTarget settingChanged:]`.
fn action() -> Sel {
    sel!(settingChanged:)
}

fn label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

/// A label that wraps at `width` points; its text can be selected and copied.
fn wrapping_label(text: &str, width: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setPreferredMaxLayoutWidth(width);
    label
}

/// A small, dimmed wrapping label, for explanations under a control.
fn note_label(text: &str, width: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = wrapping_label(text, width, mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    label
}

fn section_header(title: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let header = label(title, mtm);
    header.setFont(Some(
        &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
    ));
    header
}

fn checkbox(
    title: &str,
    control: Control,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: `target` implements the action, and the window keeps it alive as long as its
    // controls.
    let checkbox = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action()),
            mtm,
        )
    };
    checkbox.setTag(control.tag());
    checkbox
}

fn push_button(
    title: &str,
    control: Control,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: as in `checkbox`.
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action()),
            mtm,
        )
    };
    button.setTag(control.tag());
    button
}

fn pop_up<'a>(
    items: impl Iterator<Item = &'a str>,
    control: Control,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSPopUpButton> {
    let pop_up =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
    for item in items {
        pop_up.addItemWithTitle(&NSString::from_str(item));
    }
    pop_up.setTag(control.tag());
    // SAFETY: as in `checkbox`.
    unsafe {
        pop_up.setTarget(Some(target));
        pop_up.setAction(Some(action()));
    }
    pop_up
}

/// A field for a whole number of milliseconds.
fn millis_field(
    control: Control,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    field.setTag(control.tag());
    // SAFETY: as in `checkbox`.
    unsafe {
        field.setTarget(Some(target));
        field.setAction(Some(action()));
    }
    // Also send the action when editing ends with Tab or a click elsewhere, not only Return.
    if let Some(cell) = field.cell() {
        cell.setSendsActionOnEndEditing(true);
    }
    field
        .widthAnchor()
        .constraintEqualToConstant(64.)
        .setActive(true);
    field
}

/// A stepper from 0 to `max_ms` milliseconds, `MILLIS_STEP_MS` per click.
fn millis_stepper(
    control: Control,
    max_ms: u64,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSStepper> {
    let stepper = NSStepper::initWithFrame(NSStepper::alloc(mtm), NSRect::ZERO);
    stepper.setMinValue(0.);
    stepper.setMaxValue(max_ms as f64);
    stepper.setIncrement(MILLIS_STEP_MS);
    stepper.setValueWraps(false);
    stepper.setTag(control.tag());
    // SAFETY: as in `checkbox`.
    unsafe {
        stepper.setTarget(Some(target));
        stepper.setAction(Some(action()));
    }
    stepper
}

/// A stack of `views`, top to bottom or left to right, `spacing` points apart.
fn stack(
    views: &[&NSView],
    orientation: NSUserInterfaceLayoutOrientation,
    spacing: f64,
    mtm: MainThreadMarker,
) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    stack.setOrientation(orientation);
    stack.setSpacing(spacing);
    if orientation == NSUserInterfaceLayoutOrientation::Vertical {
        stack.setAlignment(NSLayoutAttribute::Leading);
    } else {
        stack.setAlignment(NSLayoutAttribute::CenterY);
    }
    stack
}

fn set_checked(button: &NSButton, is_checked: bool) {
    button.setState(if is_checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
}

fn is_checked(button: &NSButton) -> bool {
    button.state() == NSControlStateValueOn
}

fn select(pop_up: &NSPopUpButton, index: usize) {
    pop_up.selectItemAtIndex(NSInteger::try_from(index).unwrap_or(0));
}

/// Shows `ms` in a milliseconds field and its stepper.
fn show_millis(field: &NSTextField, stepper: &NSStepper, ms: u64) {
    field.setStringValue(&NSString::from_str(&ms.to_string()));
    stepper.setIntegerValue(NSInteger::try_from(ms).unwrap_or(0));
}

/// A stepper's milliseconds; 0 for a negative value, which its minimum rules out.
fn stepper_millis(stepper: &NSStepper) -> u64 {
    u64::try_from(stepper.integerValue()).unwrap_or(0)
}

/// Shows `text` in a note under a control, hiding its grid row while there is none.
fn show_note(label: &NSTextField, row: &NSGridRow, text: Option<&str>) {
    label.setStringValue(&NSString::from_str(text.unwrap_or_default()));
    row.setHidden(text.is_none());
}

/// Whether `field` of the shown view differs from the new one; always for a first view.
fn differs<T: PartialEq>(old: Option<&View>, new: &View, field: impl Fn(&View) -> &T) -> bool {
    old.is_none_or(|old| field(old) != field(new))
}

/// The window's controls, and the views `refresh` updates.
struct Controls {
    /// The vertical stack that is the window's content view.
    content: Retained<NSStackView>,
    /// The invalid-file notice above the settings; hidden while the file is usable.
    notice: Retained<NSStackView>,
    notice_title: Retained<NSTextField>,
    notice_detail: Retained<NSTextField>,
    notice_note: Retained<NSTextField>,
    launch_at_login: Retained<NSButton>,
    show_in_dock: Retained<NSButton>,
    shortcut: Retained<NSTextField>,
    /// Change… while idle, Cancel while recording.
    record: Retained<NSButton>,
    shortcut_note: Retained<NSTextField>,
    shortcut_note_row: Retained<NSGridRow>,
    mode: Retained<NSPopUpButton>,
    show_popup: Retained<NSButton>,
    position: Retained<NSPopUpButton>,
    size: Retained<NSPopUpButton>,
    mic_on_field: Retained<NSTextField>,
    mic_on_stepper: Retained<NSStepper>,
    /// Hide “Mic off” after: on, the popup hides the field's milliseconds after each mute.
    hide_mic_off: Retained<NSButton>,
    /// Enabled only while `hide_mic_off` is checked, as is `mic_off_stepper`.
    mic_off_field: Retained<NSTextField>,
    mic_off_stepper: Retained<NSStepper>,
    popup_note: Retained<NSTextField>,
    popup_note_row: Retained<NSGridRow>,
    play_sound: Retained<NSButton>,
    show_notification: Retained<NSButton>,
    launch_state: Retained<NSPopUpButton>,
    mute_on_lock: Retained<NSButton>,
    mute_on_sleep: Retained<NSButton>,
    mute_on_display_sleep: Retained<NSButton>,
    allow_url_unmute: Retained<NSButton>,
}

/// A grid row: a section header across both columns, or a label (or nothing) and a control.
enum Row<'a> {
    Header(&'a NSView),
    Setting(&'a NSView, &'a NSView),
}

/// The index of the row whose control is `control`, so a row added above it needs no
/// renumbering. 0 for a control `rows` lacks, which `Controls::new` never looks up.
fn row_index(rows: &[Row], control: &NSView) -> NSInteger {
    rows.iter()
        .position(|row| matches!(row, Row::Setting(_, view) if ptr::eq(*view, control)))
        .and_then(|index| NSInteger::try_from(index).ok())
        .unwrap_or(0)
}

impl Controls {
    /// Builds every control, sending its action to `target`, and lays them out.
    fn new(target: &AnyObject, mtm: MainThreadMarker) -> Self {
        let vertical = NSUserInterfaceLayoutOrientation::Vertical;
        let horizontal = NSUserInterfaceLayoutOrientation::Horizontal;

        let notice_title = label("", mtm);
        notice_title.setFont(Some(
            &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
        ));
        notice_title.setTextColor(Some(&NSColor::systemRedColor()));
        let notice_detail = wrapping_label("", WIDE_TEXT_WIDTH, mtm);
        let notice_note = note_label("", WIDE_TEXT_WIDTH, mtm);
        let open_file = push_button("Open settings.json", Control::OpenSettingsFile, target, mtm);
        let notice = stack(
            &[&notice_title, &notice_detail, &notice_note, &open_file],
            vertical,
            6.,
            mtm,
        );
        notice.setHidden(true);

        let launch_at_login = checkbox(
            crate::launch_at_login::menu_title(false),
            Control::LaunchAtLogin,
            target,
            mtm,
        );
        let show_in_dock = checkbox("Show in Dock", Control::ShowInDock, target, mtm);

        let shortcut = label("", mtm);
        let record = push_button(RECORD_TITLE, Control::RecordShortcut, target, mtm);
        let shortcut_row = stack(&[&shortcut, &record], horizontal, 8., mtm);
        let shortcut_note = note_label("", NOTE_WIDTH, mtm);
        let mode = pop_up(titles(&MODE_CHOICES), Control::Mode, target, mtm);

        let show_popup = checkbox(
            "Show the popup while muted",
            Control::ShowPopup,
            target,
            mtm,
        );
        let position = pop_up(
            titles(&POSITION_CHOICES),
            Control::PopupPosition,
            target,
            mtm,
        );
        let size = pop_up(titles(&SIZE_CHOICES), Control::PopupSize, target, mtm);
        let mic_on_field = millis_field(Control::MicOnField, target, mtm);
        let mic_on_stepper = millis_stepper(Control::MicOnStepper, MicOnFlash::MAX_MS, target, mtm);
        let ms = label("ms", mtm);
        let mic_on = stack(&[&mic_on_field, &ms, &mic_on_stepper], horizontal, 6., mtm);
        let hide_mic_off = checkbox("Hide “Mic off” after", Control::HideMicOff, target, mtm);
        let mic_off_field = millis_field(Control::MicOffField, target, mtm);
        let mic_off_stepper =
            millis_stepper(Control::MicOffStepper, MicOffFlash::MAX_MS, target, mtm);
        let mic_off_ms = label("ms", mtm);
        let mic_off = stack(
            &[&hide_mic_off, &mic_off_field, &mic_off_ms, &mic_off_stepper],
            horizontal,
            6.,
            mtm,
        );
        let popup_note = note_label("", NOTE_WIDTH, mtm);

        let play_sound = checkbox(
            "Play a sound when the microphones mute or unmute",
            Control::PlaySound,
            target,
            mtm,
        );
        let show_notification = checkbox(
            "Show a notification when they mute or unmute",
            Control::ShowNotification,
            target,
            mtm,
        );

        let launch_state = pop_up(
            titles(&LAUNCH_STATE_CHOICES),
            Control::LaunchState,
            target,
            mtm,
        );
        let mute_on_lock = checkbox(
            "Mute when the screen locks",
            Control::MuteOnLock,
            target,
            mtm,
        );
        let mute_on_sleep = checkbox(
            "Mute when the Mac goes to sleep",
            Control::MuteOnSleep,
            target,
            mtm,
        );
        let mute_on_display_sleep = checkbox(
            "Mute when the displays go to sleep",
            Control::MuteOnDisplaySleep,
            target,
            mtm,
        );

        let allow_url_unmute = checkbox(
            "Let URLs unmute the microphones",
            Control::AllowUrlUnmute,
            target,
            mtm,
        );
        let url_warning = note_label(URL_UNMUTE_WARNING, NOTE_WIDTH, mtm);

        let empty = NSGridCell::emptyContentView(mtm);
        let headers = [
            section_header("General", mtm),
            section_header("Shortcut", mtm),
            section_header("Popup", mtm),
            section_header("Feedback", mtm),
            section_header("Automatic mute", mtm),
            section_header("External control", mtm),
        ];
        let [general, shortcut_header, popup_header, feedback, automatic, external] = &headers;
        let labels = [
            label("Mute shortcut:", mtm),
            label("Shortcut mode:", mtm),
            label("Position:", mtm),
            label("Size:", mtm),
            label("Show “Mic on” for:", mtm),
            label("At launch:", mtm),
        ];
        let [shortcut_label, mode_label, position_label, size_label, mic_on_label, launch_label] =
            &labels;
        let rows = [
            Row::Header(general),
            Row::Setting(&empty, &launch_at_login),
            Row::Setting(&empty, &show_in_dock),
            Row::Header(shortcut_header),
            Row::Setting(shortcut_label, &shortcut_row),
            Row::Setting(&empty, &shortcut_note),
            Row::Setting(mode_label, &mode),
            Row::Header(popup_header),
            Row::Setting(&empty, &show_popup),
            Row::Setting(position_label, &position),
            Row::Setting(size_label, &size),
            Row::Setting(mic_on_label, &mic_on),
            Row::Setting(&empty, &mic_off),
            Row::Setting(&empty, &popup_note),
            Row::Header(feedback),
            Row::Setting(&empty, &play_sound),
            Row::Setting(&empty, &show_notification),
            Row::Header(automatic),
            Row::Setting(launch_label, &launch_state),
            Row::Setting(&empty, &mute_on_lock),
            Row::Setting(&empty, &mute_on_sleep),
            Row::Setting(&empty, &mute_on_display_sleep),
            Row::Header(external),
            Row::Setting(&empty, &allow_url_unmute),
            Row::Setting(&empty, &url_warning),
        ];
        // The rows of the two notes, which hide while there's nothing to say.
        let shortcut_note_index = row_index(&rows, &shortcut_note);
        let popup_note_index = row_index(&rows, &popup_note);
        let grid = grid(&rows, mtm);
        let shortcut_note_row = grid.rowAtIndex(shortcut_note_index);
        let popup_note_row = grid.rowAtIndex(popup_note_index);

        let footer = note_label(FOOTER_TEXT, WIDE_TEXT_WIDTH, mtm);
        let content = stack(&[&notice, &grid, &footer], vertical, 18., mtm);
        content.setEdgeInsets(NSEdgeInsets {
            top: CONTENT_MARGIN,
            left: CONTENT_MARGIN,
            bottom: CONTENT_MARGIN,
            right: CONTENT_MARGIN,
        });

        Self {
            content,
            notice,
            notice_title,
            notice_detail,
            notice_note,
            launch_at_login,
            show_in_dock,
            shortcut,
            record,
            shortcut_note,
            shortcut_note_row,
            mode,
            show_popup,
            position,
            size,
            mic_on_field,
            mic_on_stepper,
            hide_mic_off,
            mic_off_field,
            mic_off_stepper,
            popup_note,
            popup_note_row,
            play_sound,
            show_notification,
            launch_state,
            mute_on_lock,
            mute_on_sleep,
            mute_on_display_sleep,
            allow_url_unmute,
        }
    }

    /// Shows `new`, touching only what differs from `old`; `is_mic_on_stale` and
    /// `is_mic_off_stale` rewrite the "Mic on" and "Mic off" fields after an edit, which may have
    /// left text the setting doesn't hold. Returns whether the content's size may have changed.
    fn show(
        &self,
        old: Option<&View>,
        new: &View,
        is_mic_on_stale: bool,
        is_mic_off_stale: bool,
    ) -> bool {
        let mut is_resized = false;
        if differs(old, new, |view| &view.problem) {
            is_resized = true;
            match &new.problem {
                Some(problem) => {
                    self.notice_title
                        .setStringValue(&NSString::from_str(problem.title()));
                    self.notice_detail
                        .setStringValue(&NSString::from_str(&problem.detail));
                    self.notice_note
                        .setStringValue(&NSString::from_str(&problem.note()));
                    self.notice.setHidden(false);
                }
                None => self.notice.setHidden(true),
            }
        }
        if differs(old, new, |view| &view.launch_at_login) {
            set_checked(&self.launch_at_login, new.launch_at_login);
        }
        if differs(old, new, |view| &view.launch_at_login_title) {
            is_resized = true;
            self.launch_at_login
                .setTitle(&NSString::from_str(new.launch_at_login_title));
        }
        if differs(old, new, |view| &view.show_in_dock) {
            set_checked(&self.show_in_dock, new.show_in_dock);
        }
        if differs(old, new, |view| &view.shortcut) {
            is_resized = true;
            self.shortcut
                .setStringValue(&NSString::from_str(&new.shortcut));
        }
        if differs(old, new, |view| &view.record_title) {
            is_resized = true;
            self.record.setTitle(&NSString::from_str(new.record_title));
        }
        if differs(old, new, |view| &view.is_shortcut_note_error) {
            let color = if new.is_shortcut_note_error {
                NSColor::systemRedColor()
            } else {
                NSColor::secondaryLabelColor()
            };
            self.shortcut_note.setTextColor(Some(&color));
        }
        if differs(old, new, |view| &view.shortcut_note) {
            is_resized = true;
            show_note(
                &self.shortcut_note,
                &self.shortcut_note_row,
                new.shortcut_note.as_deref(),
            );
        }
        if differs(old, new, |view| &view.mode) {
            select(&self.mode, new.mode);
        }
        if differs(old, new, |view| &view.show_popup) {
            set_checked(&self.show_popup, new.show_popup);
        }
        if differs(old, new, |view| &view.position) {
            select(&self.position, new.position);
        }
        if differs(old, new, |view| &view.size) {
            select(&self.size, new.size);
        }
        if is_mic_on_stale || differs(old, new, |view| &view.mic_on_ms) {
            show_millis(&self.mic_on_field, &self.mic_on_stepper, new.mic_on_ms);
        }
        if is_mic_off_stale || differs(old, new, |view| &view.mic_off_ms) {
            let is_hiding = new.mic_off_ms.is_some();
            set_checked(&self.hide_mic_off, is_hiding);
            let ms = new.mic_off_ms.unwrap_or(MIC_OFF_DEFAULT_MS);
            show_millis(&self.mic_off_field, &self.mic_off_stepper, ms);
            self.mic_off_field.setEnabled(is_hiding);
            self.mic_off_stepper.setEnabled(is_hiding);
        }
        if differs(old, new, |view| &view.popup_note) {
            is_resized = true;
            show_note(
                &self.popup_note,
                &self.popup_note_row,
                new.popup_note.as_deref(),
            );
        }
        if differs(old, new, |view| &view.play_sound) {
            set_checked(&self.play_sound, new.play_sound);
        }
        if differs(old, new, |view| &view.show_notification) {
            set_checked(&self.show_notification, new.show_notification);
        }
        if differs(old, new, |view| &view.launch_state) {
            select(&self.launch_state, new.launch_state);
        }
        if differs(old, new, |view| &view.mute_on_lock) {
            set_checked(&self.mute_on_lock, new.mute_on_lock);
        }
        if differs(old, new, |view| &view.mute_on_sleep) {
            set_checked(&self.mute_on_sleep, new.mute_on_sleep);
        }
        if differs(old, new, |view| &view.mute_on_display_sleep) {
            set_checked(&self.mute_on_display_sleep, new.mute_on_display_sleep);
        }
        if differs(old, new, |view| &view.allow_url_unmute) {
            set_checked(&self.allow_url_unmute, new.allow_url_unmute);
        }
        is_resized
    }
}

/// A two-column grid of `rows`: labels right-aligned in the first column, controls left-aligned
/// in the second, and section headers across both.
fn grid(rows: &[Row], mtm: MainThreadMarker) -> Retained<NSGridView> {
    let cells: Vec<Retained<NSArray<NSView>>> = rows
        .iter()
        .map(|row| match *row {
            Row::Header(header) => {
                NSArray::from_slice(&[header, &NSGridCell::emptyContentView(mtm)])
            }
            Row::Setting(label, control) => NSArray::from_slice(&[label, control]),
        })
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&cells), mtm);
    grid.setRowSpacing(8.);
    grid.setColumnSpacing(8.);
    grid.setRowAlignment(NSGridRowAlignment::None);
    grid.setYPlacement(NSGridCellPlacement::Center);
    grid.columnAtIndex(0)
        .setXPlacement(NSGridCellPlacement::Trailing);
    grid.columnAtIndex(1)
        .setXPlacement(NSGridCellPlacement::Leading);
    for (index, row) in rows.iter().enumerate() {
        let (Row::Header(_), Ok(index)) = (row, NSInteger::try_from(index)) else {
            continue;
        };
        grid.mergeCellsInHorizontalRange_verticalRange(
            NSRange::new(0, 2),
            NSRange::new(index.unsigned_abs(), 1),
        );
        let row = grid.rowAtIndex(index);
        if index > 0 {
            row.setTopPadding(12.);
        }
        grid.cellAtColumnIndex_rowIndex(0, index)
            .setXPlacement(NSGridCellPlacement::Leading);
    }
    grid
}

/// Resizes `window` to fit `content`, keeping its top edge where it is.
fn fit_to_content(window: &NSWindow, content: &NSView) {
    let size = content.fittingSize();
    let current = window.contentRectForFrameRect(window.frame());
    if current.size == size {
        return;
    }
    // AppKit's y axis grows up: keep the top, move the bottom.
    let top = current.origin.y + current.size.height;
    let content_rect = NSRect::new(NSPoint::new(current.origin.x, top - size.height), size);
    window.setFrame_display(window.frameRectForContentRect(content_rect), true);
}

/// The window, created on the first `show`, and hidden rather than freed when closed.
struct Built {
    window: Retained<NSWindow>,
    controls: Controls,
    /// Every control's target. Controls don't retain their target, so it lives as long as they
    /// do: it is dropped after the window and its controls.
    target: Retained<SettingsTarget>,
    /// What the key-event monitor caught for the event loop.
    catcher: Rc<KeyCatcher>,
    /// The key-event monitor (`add_key_monitor`), removed on drop.
    monitor: Option<Retained<AnyObject>>,
}

impl Drop for Built {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            // SAFETY: `monitor` is the token `addLocalMonitorForEventsMatchingMask` returned.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }
}

impl Built {
    fn new(mtm: MainThreadMarker) -> Self {
        let target = SettingsTarget::new();
        let controls = Controls::new(&target, mtm);
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        // Larger than its content, which `fit_to_content` then shrinks it to: a smaller window
        // would squeeze the controls' constraints until then.
        let frame = NSRect::new(NSPoint::ZERO, NSSize::new(600., 1000.));
        // SAFETY: a new window; `false` creates its backing store at once.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: `Retained` owns the window; closing it must only hide it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(WINDOW_TITLE));
        window.setContentView(Some(&controls.content));
        let catcher = Rc::new(KeyCatcher::default());
        let monitor = add_key_monitor(&window, Rc::clone(&catcher), mtm);
        if monitor.is_none() {
            log::warn!("Failed to watch the Settings window's keys; the recorder can't record");
        }
        Self {
            window,
            controls,
            target,
            catcher,
            monitor,
        }
    }
}

/// The Settings window. Main thread only: it holds AppKit objects, so it is neither `Send` nor
/// `Sync`. Opening it never blocks the caller.
#[derive(Default)]
pub struct SettingsWindow {
    built: Option<Built>,
    /// What the controls show; `None` until the first `refresh`.
    shown: Option<View>,
    /// The "Mic on" field was edited, so `refresh` writes the setting back into it.
    is_mic_on_stale: bool,
    /// The same for the "Mic off" field.
    is_mic_off_stale: bool,
    /// The shortcut recording in progress.
    recording: Option<Recording>,
}

impl SettingsWindow {
    /// Shows the window with `settings` and the file's `problem`, or brings it to the front.
    pub fn show(&mut self, settings: &Settings, problem: Option<&FileProblem>) -> Result<()> {
        let mtm = MainThreadMarker::new().context("The Settings window needs the main thread")?;
        let is_new = self.built.is_none();
        if is_new {
            self.built = Some(Built::new(mtm));
            self.shown = None;
        }
        self.refresh(settings, problem);
        let Some(built) = &self.built else {
            return Ok(());
        };
        if is_new {
            fit_to_content(&built.window, &built.controls.content);
            built.window.center();
        }
        let app = NSApplication::sharedApplication(mtm);
        // A tray click doesn't activate an accessory app; without this the window would open
        // behind other apps' windows. `activate` replaces it only from macOS 14 on.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        built.window.makeKeyAndOrderFront(None);
        Ok(())
    }

    /// Whether the window is on screen.
    pub fn is_open(&self) -> bool {
        self.built
            .as_ref()
            .is_some_and(|built| built.window.isVisible())
    }

    /// What the user did in the window since the last call, at `now`: the recorder's progress
    /// first, then each changed control in `Control::ALL` order.
    pub fn take_events(&mut self, now: Instant) -> Vec<WindowEvent> {
        let Some(built) = &self.built else {
            return Vec::new();
        };
        let changed = built.target.take_changed();
        let presses = built.catcher.take_presses();
        let held = built.catcher.take_held();
        if built.catcher.take_close_request() {
            built.window.performClose(None);
        }
        for command in built.catcher.take_edits() {
            if let Some(responder) = built.window.firstResponder() {
                // SAFETY: the edit actions take their sender, and nil is allowed. A responder
                // that doesn't handle one passes it up the responder chain.
                unsafe { responder.tryToPerform_with(command.action(), None) };
            }
        }
        // The hotkey stays unregistered only while the user records in this window: losing
        // focus, closing, or using another control ends the recording.
        let is_focused = built.window.isVisible() && built.window.isKeyWindow();
        let is_other_control_used = changed
            .iter()
            .any(|&control| control != Control::RecordShortcut);
        let was_recording = self.recording.is_some();
        let step = match &mut self.recording {
            None => None,
            Some(_) if !is_focused || is_other_control_used => Some(Step::Stop),
            Some(recording) => Some(advance(recording, &presses, held, now)),
        };
        let mut events = Vec::new();
        match step {
            Some(Step::Stop) => events.push(self.stop_recording()),
            Some(Step::Record(config)) => {
                events.push(self.stop_recording());
                events.push(WindowEvent::Changed(SettingsChange::Shortcut(config)));
            }
            Some(Step::Wait) | None => {}
        }
        for control in changed {
            match control {
                Control::RecordShortcut if self.recording.is_some() => {
                    events.push(self.stop_recording());
                }
                // A click that ended the recording above doesn't start another.
                Control::RecordShortcut if !was_recording => {
                    events.push(self.start_recording(now));
                }
                Control::RecordShortcut => {}
                Control::OpenSettingsFile => events.push(WindowEvent::OpenSettingsFile),
                _ => events.extend(self.change_for(control).map(WindowEvent::Changed)),
            }
        }
        events
    }

    fn start_recording(&mut self, now: Instant) -> WindowEvent {
        if let Some(built) = &self.built {
            built.catcher.set_recording(true);
        }
        self.recording = Some(Recording::new(now));
        WindowEvent::RecordingStarted
    }

    fn stop_recording(&mut self) -> WindowEvent {
        if let Some(built) = &self.built {
            built.catcher.set_recording(false);
        }
        self.recording = None;
        WindowEvent::RecordingEnded
    }

    /// The change `control` shows now; `None` when it holds no usable value.
    fn change_for(&mut self, control: Control) -> Option<SettingsChange> {
        let controls = &self.built.as_ref()?.controls;
        let change = match control {
            Control::LaunchAtLogin => {
                SettingsChange::LaunchAtLogin(is_checked(&controls.launch_at_login))
            }
            Control::ShowInDock => SettingsChange::ShowInDock(is_checked(&controls.show_in_dock)),
            Control::Mode => SettingsChange::Mode(choice_at(
                &MODE_CHOICES,
                controls.mode.indexOfSelectedItem(),
            )?),
            Control::ShowPopup => SettingsChange::ShowPopup(is_checked(&controls.show_popup)),
            Control::PopupPosition => SettingsChange::PopupPosition(choice_at(
                &POSITION_CHOICES,
                controls.position.indexOfSelectedItem(),
            )?),
            Control::PopupSize => SettingsChange::PopupSize(choice_at(
                &SIZE_CHOICES,
                controls.size.indexOfSelectedItem(),
            )?),
            Control::MicOnField => {
                // Whatever was typed, the field shows the setting again afterwards.
                let text = controls.mic_on_field.stringValue().to_string();
                self.is_mic_on_stale = true;
                SettingsChange::PopupMicOnMs(parse_millis(&text)?)
            }
            Control::MicOnStepper => {
                SettingsChange::PopupMicOnMs(stepper_millis(&controls.mic_on_stepper))
            }
            // Checking it sets the milliseconds the disabled field showed.
            Control::HideMicOff => SettingsChange::PopupMicOffMs(
                is_checked(&controls.hide_mic_off)
                    .then(|| stepper_millis(&controls.mic_off_stepper)),
            ),
            Control::MicOffField => {
                let text = controls.mic_off_field.stringValue().to_string();
                self.is_mic_off_stale = true;
                // An edit that ends because the checkbox was unchecked changes nothing.
                if !is_checked(&controls.hide_mic_off) {
                    return None;
                }
                SettingsChange::PopupMicOffMs(Some(parse_millis(&text)?))
            }
            Control::MicOffStepper if is_checked(&controls.hide_mic_off) => {
                SettingsChange::PopupMicOffMs(Some(stepper_millis(&controls.mic_off_stepper)))
            }
            Control::MicOffStepper => return None,
            Control::PlaySound => SettingsChange::PlaySound(is_checked(&controls.play_sound)),
            Control::ShowNotification => {
                SettingsChange::ShowNotification(is_checked(&controls.show_notification))
            }
            Control::LaunchState => SettingsChange::LaunchState(choice_at(
                &LAUNCH_STATE_CHOICES,
                controls.launch_state.indexOfSelectedItem(),
            )?),
            Control::MuteOnLock => SettingsChange::MuteOnLock(is_checked(&controls.mute_on_lock)),
            Control::MuteOnSleep => {
                SettingsChange::MuteOnSleep(is_checked(&controls.mute_on_sleep))
            }
            Control::MuteOnDisplaySleep => {
                SettingsChange::MuteOnDisplaySleep(is_checked(&controls.mute_on_display_sleep))
            }
            Control::AllowUrlUnmute => {
                SettingsChange::AllowUrlUnmute(is_checked(&controls.allow_url_unmute))
            }
            Control::RecordShortcut | Control::OpenSettingsFile => return None,
        };
        Some(change)
    }

    /// Shows `settings`, the shortcut's status, and the file's `problem`, touching only the
    /// controls whose value changed, so it can run on every pass of the event loop.
    pub fn refresh(&mut self, settings: &Settings, problem: Option<&FileProblem>) {
        let Some(built) = &self.built else {
            return;
        };
        let view = view_for(
            settings,
            &current_status(),
            problem,
            crate::launch_at_login::needs_approval(),
            self.recording.as_ref(),
        );
        if self.shown.as_ref() == Some(&view) && !self.is_mic_on_stale && !self.is_mic_off_stale {
            return;
        }
        let is_resized = built.controls.show(
            self.shown.as_ref(),
            &view,
            self.is_mic_on_stale,
            self.is_mic_off_stale,
        );
        self.is_mic_on_stale = false;
        self.is_mic_off_stale = false;
        if is_resized {
            fit_to_content(&built.window, &built.controls.content);
        }
        self.shown = Some(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutConfig;
    use crate::shortcuts::{parse_shortcut, ShortcutProblem};
    use std::path::Path;

    fn default_shortcut() -> crate::shortcuts::Shortcut {
        parse_shortcut(&ShortcutConfig::default()).unwrap()
    }

    fn status_ok() -> ShortcutStatus {
        ShortcutStatus {
            registered: Some(default_shortcut()),
            problem: None,
        }
    }

    #[test]
    fn every_change_sets_its_setting_and_reports_it_once() {
        let changes = [
            SettingsChange::LaunchAtLogin(true),
            SettingsChange::ShowInDock(true),
            SettingsChange::Shortcut(ShortcutConfig {
                modifiers: vec!["ctrl".to_string()],
                key: "F13".to_string(),
            }),
            SettingsChange::Mode(ShortcutMode::PushToTalk),
            SettingsChange::ShowPopup(false),
            SettingsChange::PopupPosition(PopupPosition::TopRight),
            SettingsChange::PopupSize(PopupSize::Large),
            SettingsChange::PopupMicOnMs(2_500),
            SettingsChange::PopupMicOffMs(Some(1_500)),
            SettingsChange::PlaySound(true),
            SettingsChange::ShowNotification(true),
            SettingsChange::LaunchState(LaunchState::Last),
            SettingsChange::MuteOnLock(true),
            SettingsChange::MuteOnSleep(true),
            SettingsChange::MuteOnDisplaySleep(true),
            SettingsChange::AllowUrlUnmute(true),
        ];
        let mut settings = Settings::default();
        for change in &changes {
            assert!(change.apply_to(&mut settings), "{change:?}");
            assert!(!change.apply_to(&mut settings), "{change:?} again");
        }
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["launch_at_login"], true);
        assert_eq!(saved["show_in_dock"], true);
        assert_eq!(
            saved["mic_shortcut"],
            serde_json::json!({ "modifiers": ["ctrl"], "key": "F13" })
        );
        assert_eq!(saved["mode"], "push_to_talk");
        assert_eq!(saved["show_popup"], false);
        assert_eq!(saved["popup_position"], "top_right");
        assert_eq!(saved["popup_size"], "large");
        assert_eq!(saved["popup_mic_on_ms"], 2_500);
        assert_eq!(saved["popup_mic_off_ms"], 1_500);
        assert_eq!(saved["play_sound"], true);
        assert_eq!(saved["show_notification"], true);
        assert_eq!(saved["launch_state"], "last");
        assert_eq!(saved["mute_on_lock"], true);
        assert_eq!(saved["mute_on_sleep"], true);
        assert_eq!(saved["mute_on_display_sleep"], true);
        assert_eq!(saved["allow_url_unmute"], true);
    }

    #[test]
    fn a_change_keeps_the_other_settings_and_unknown_fields() {
        let mut settings: Settings =
            serde_json::from_str(r#"{ "show_popup": false, "future": [1, 2] }"#).unwrap();

        assert!(SettingsChange::PlaySound(true).apply_to(&mut settings));

        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["show_popup"], false);
        assert_eq!(saved["future"], serde_json::json!([1, 2]));
        assert_eq!(saved["play_sound"], true);
    }

    #[test]
    fn a_long_mic_on_flash_is_capped() {
        let mut settings = Settings::default();
        assert!(SettingsChange::PopupMicOnMs(60_000).apply_to(&mut settings));
        assert_eq!(
            settings.popup_mic_on_ms.value(),
            MicOnFlash::from_millis(10_000)
        );
        assert!(settings.feedback_problems().is_empty());
    }

    #[test]
    fn unchecking_hide_mic_off_keeps_the_popup_up_while_muted_again() {
        let mut settings = Settings::default();
        assert!(SettingsChange::PopupMicOffMs(Some(2_000)).apply_to(&mut settings));
        assert_eq!(
            settings.popup_mic_off_ms.value().duration(),
            Some(Duration::from_secs(2))
        );

        assert!(SettingsChange::PopupMicOffMs(None).apply_to(&mut settings));
        assert!(
            !SettingsChange::PopupMicOffMs(None).apply_to(&mut settings),
            "already off"
        );

        assert_eq!(settings.popup_mic_off_ms.value(), MicOffFlash::WHILE_MUTED);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["popup_mic_off_ms"], serde_json::Value::Null);
    }

    #[test]
    fn a_long_mic_off_flash_is_capped() {
        let mut settings = Settings::default();
        assert!(SettingsChange::PopupMicOffMs(Some(60_000)).apply_to(&mut settings));
        assert_eq!(
            settings.popup_mic_off_ms.value(),
            MicOffFlash::from_millis(MicOffFlash::MAX_MS)
        );
        assert!(settings.feedback_problems().is_empty());
    }

    #[test]
    fn hiding_mic_off_or_not_replaces_a_value_the_file_got_wrong() {
        for change in [
            SettingsChange::PopupMicOffMs(None),
            SettingsChange::PopupMicOffMs(Some(800)),
        ] {
            let mut settings: Settings =
                serde_json::from_str(r#"{ "popup_mic_off_ms": "1s" }"#).unwrap();
            assert_eq!(settings.popup_mic_off_ms.value(), MicOffFlash::WHILE_MUTED);

            assert!(change.apply_to(&mut settings), "{change:?}");

            assert!(settings.feedback_problems().is_empty(), "{change:?}");
        }
    }

    #[test]
    fn choosing_a_popup_value_replaces_one_the_file_got_wrong() {
        let mut settings: Settings =
            serde_json::from_str(r#"{ "popup_position": "middle" }"#).unwrap();
        assert_eq!(settings.popup_position.value(), PopupPosition::BottomCenter);

        // The value in use, chosen explicitly, still replaces the file's typo.
        assert!(SettingsChange::PopupPosition(PopupPosition::BottomCenter).apply_to(&mut settings));

        assert!(settings.feedback_problems().is_empty());
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["popup_position"], "bottom_center");
    }

    #[test]
    fn every_pop_up_item_round_trips_through_its_index() {
        fn check<T: Copy + PartialEq + std::fmt::Debug>(choices: &[(T, &str)]) {
            for (index, &(value, title)) in choices.iter().enumerate() {
                assert_eq!(index_of(choices, value), index, "{title}");
                let index = NSInteger::try_from(index).unwrap();
                assert_eq!(choice_at(choices, index), Some(value), "{title}");
            }
            let titles: std::collections::BTreeSet<_> = choices.iter().map(|c| c.1).collect();
            assert_eq!(
                titles.len(),
                choices.len(),
                "NSPopUpButton needs unique titles"
            );
        }
        check(&MODE_CHOICES);
        check(&POSITION_CHOICES);
        check(&SIZE_CHOICES);
        check(&LAUNCH_STATE_CHOICES);
    }

    #[test]
    fn pop_ups_list_every_value_of_their_setting() {
        for position in PopupPosition::ALL {
            assert!(
                POSITION_CHOICES.iter().any(|c| c.0 == position),
                "{position:?}"
            );
        }
        for size in PopupSize::ALL {
            assert!(SIZE_CHOICES.iter().any(|c| c.0 == size), "{size:?}");
        }
        for state in [LaunchState::Keep, LaunchState::Muted, LaunchState::Last] {
            assert!(
                LAUNCH_STATE_CHOICES.iter().any(|c| c.0 == state),
                "{state:?}"
            );
        }
        for mode in [ShortcutMode::Toggle, ShortcutMode::PushToTalk] {
            assert!(MODE_CHOICES.iter().any(|c| c.0 == mode), "{mode:?}");
        }
    }

    #[test]
    fn no_selection_or_an_index_past_the_items_chooses_nothing() {
        assert_eq!(choice_at(&SIZE_CHOICES, -1), None);
        assert_eq!(choice_at(&SIZE_CHOICES, 3), None);
        assert_eq!(choice_at(&SIZE_CHOICES, NSInteger::MIN), None);
    }

    #[test]
    fn the_mic_on_field_takes_whole_milliseconds_only() {
        assert_eq!(parse_millis("1000"), Some(1_000));
        assert_eq!(parse_millis(" 250 "), Some(250));
        assert_eq!(parse_millis("0"), Some(0));
        assert_eq!(parse_millis("60000"), Some(60_000));
        for rejected in ["", " ", "abc", "-5", "1.5", "1,000", "1 000", "3s"] {
            assert_eq!(parse_millis(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn the_view_shows_every_setting() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "launch_at_login": true,
                "show_in_dock": true,
                "mode": "push_to_talk",
                "show_popup": false,
                "popup_position": "top_left",
                "popup_size": "small",
                "popup_mic_on_ms": 0,
                "popup_mic_off_ms": 2500,
                "play_sound": true,
                "show_notification": true,
                "launch_state": "muted",
                "mute_on_lock": true,
                "mute_on_sleep": true,
                "mute_on_display_sleep": true,
                "allow_url_unmute": true
            }"#,
        )
        .unwrap();

        let view = view_for(&settings, &status_ok(), None, false, None);

        assert!(view.launch_at_login && view.show_in_dock && !view.show_popup);
        assert_eq!(view.launch_at_login_title, "Launch at Login");
        assert_eq!(view.shortcut, "⇧⌘M");
        assert_eq!(view.shortcut_note, None);
        assert_eq!(MODE_CHOICES[view.mode].0, ShortcutMode::PushToTalk);
        assert_eq!(POSITION_CHOICES[view.position].0, PopupPosition::TopLeft);
        assert_eq!(SIZE_CHOICES[view.size].0, PopupSize::Small);
        assert_eq!(view.mic_on_ms, 0);
        assert_eq!(view.mic_off_ms, Some(2_500));
        assert_eq!(view.popup_note, None);
        assert!(view.play_sound && view.show_notification);
        assert_eq!(
            LAUNCH_STATE_CHOICES[view.launch_state].0,
            LaunchState::Muted
        );
        assert!(view.mute_on_lock && view.mute_on_sleep && view.mute_on_display_sleep);
        assert!(view.allow_url_unmute);
        assert_eq!(view.problem, None);
    }

    #[test]
    fn the_view_of_the_defaults_matches_a_fresh_install() {
        let view = view_for(&Settings::default(), &status_ok(), None, false, None);
        assert!(!view.launch_at_login && !view.show_in_dock && view.show_popup);
        assert_eq!(MODE_CHOICES[view.mode].0, ShortcutMode::Toggle);
        assert_eq!(
            POSITION_CHOICES[view.position].0,
            PopupPosition::BottomCenter
        );
        assert_eq!(SIZE_CHOICES[view.size].0, PopupSize::Medium);
        assert_eq!(view.mic_on_ms, MicOnFlash::DEFAULT_MS);
        assert_eq!(
            view.mic_off_ms, None,
            "Hide “Mic off” after is unchecked: the popup stays up while muted"
        );
        assert_eq!(LAUNCH_STATE_CHOICES[view.launch_state].0, LaunchState::Keep);
        assert!(!view.allow_url_unmute);
    }

    #[test]
    fn launch_at_login_points_to_system_settings_while_it_waits_for_approval() {
        let on = Settings {
            launch_at_login: true,
            ..Settings::default()
        };
        let waiting = view_for(&on, &status_ok(), None, true, None);
        assert_eq!(
            waiting.launch_at_login_title,
            crate::launch_at_login::menu_title(true)
        );
        // Off, there is nothing to approve.
        let off = view_for(&Settings::default(), &status_ok(), None, true, None);
        assert_eq!(
            off.launch_at_login_title,
            crate::launch_at_login::menu_title(false)
        );
    }

    #[test]
    fn the_shortcut_note_explains_a_shortcut_that_isnt_in_use() {
        let invalid = ShortcutStatus {
            registered: Some(default_shortcut()),
            problem: Some(ShortcutProblem::Invalid {
                reason: "unknown key \"Foo\"".to_string(),
            }),
        };
        assert_eq!(
            shortcut_note(&invalid).as_deref(),
            Some("The mic_shortcut setting is invalid: unknown key \"Foo\". ⇧⌘M is used instead.")
        );

        let none = ShortcutStatus {
            registered: None,
            problem: Some(ShortcutProblem::NotRegistered {
                shortcut: default_shortcut(),
                reason: "taken".to_string(),
            }),
        };
        let view = view_for(&Settings::default(), &none, None, false, None);
        assert_eq!(view.shortcut, "None");
        assert_eq!(
            view.shortcut_note.as_deref(),
            Some("⇧⌘M could not be registered: taken. Mute from the tray menu.")
        );
    }

    #[test]
    fn the_popup_note_lists_values_the_file_got_wrong() {
        let settings: Settings =
            serde_json::from_str(r#"{ "popup_size": "huge", "popup_mic_on_ms": 60000 }"#).unwrap();

        let view = view_for(&settings, &status_ok(), None, false, None);

        assert_eq!(SIZE_CHOICES[view.size].0, PopupSize::Medium);
        assert_eq!(view.mic_on_ms, MicOnFlash::MAX_MS);
        assert_eq!(
            view.popup_note.as_deref(),
            Some(
                "settings.json: popup_size \"huge\" isn't one of \"small\", \"medium\", \
                 \"large\"; using \"medium\".\nsettings.json: popup_mic_on_ms 60000 is above \
                 the maximum, 10000; using 10000."
            )
        );
    }

    #[test]
    fn the_popup_note_lists_a_mic_off_value_the_file_got_wrong() {
        let settings: Settings = serde_json::from_str(r#"{ "popup_mic_off_ms": "1s" }"#).unwrap();

        let view = view_for(&settings, &status_ok(), None, false, None);

        assert_eq!(view.mic_off_ms, None, "the popup stays up while muted");
        assert_eq!(
            view.popup_note.as_deref(),
            Some(
                "settings.json: popup_mic_off_ms \"1s\" isn't null or a whole number of \
                 milliseconds from 0 to 10000; using null."
            )
        );
    }

    fn load_error(contents: &str) -> LoadError {
        let dir = std::env::temp_dir().join(format!(
            "mic-mute-settings-window-test-{}-{}",
            std::process::id(),
            contents.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, contents).unwrap();
        let err = Settings::load_from(&path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        err
    }

    #[test]
    fn the_notice_names_the_file_line_and_column() {
        let err = load_error("{\n  \"show_popup\": true\n  \"show_in_dock\": true\n}\n");
        let problem = FileProblem::new(&err, Fallback::Defaults);

        assert_eq!(problem.title(), "settings.json is invalid");
        assert!(
            problem.detail.contains("settings.json"),
            "{}",
            problem.detail
        );
        assert!(
            problem.detail.contains("line 3, column 3"),
            "{}",
            problem.detail
        );
        assert!(problem
            .note()
            .starts_with("Mic Mute is using the default settings."));
        let later = FileProblem::new(&err, Fallback::Previous);
        assert!(later
            .note()
            .starts_with("Mic Mute keeps the settings it had"));
        assert!(later.note().contains("doesn't save them"));

        let view = view_for(
            &Settings::default(),
            &status_ok(),
            Some(&problem),
            false,
            None,
        );
        assert_eq!(view.problem, Some(problem));
    }

    #[test]
    fn an_unreadable_file_gets_its_own_title() {
        let err = LoadError::Read {
            path: Path::new("/tmp/settings.json").to_path_buf(),
            source: std::io::Error::other("denied"),
        };
        assert_eq!(
            FileProblem::new(&err, Fallback::Previous).title(),
            "settings.json can't be read"
        );
    }

    #[test]
    fn every_control_round_trips_through_its_tag() {
        for control in Control::ALL {
            assert_ne!(control.tag(), 0, "{control:?} has the default tag");
            assert_eq!(
                Control::from_tag(control.tag()),
                Some(control),
                "{control:?}"
            );
        }
        assert_eq!(Control::from_tag(0), None);
        assert_eq!(Control::from_tag(-1), None);
        let bits: u32 = Control::ALL.iter().map(|control| control.bit()).sum();
        assert_eq!(
            bits.count_ones() as usize,
            Control::ALL.len(),
            "bits overlap"
        );
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and the class has no `Drop`.
        #[unsafe(super(NSObject))]
        #[ivars = NSInteger]
        struct FakeControl;

        impl FakeControl {
            #[unsafe(method(tag))]
            fn tag(&self) -> NSInteger {
                *self.ivars()
            }
        }
    );

    impl FakeControl {
        fn new(tag: NSInteger) -> Retained<Self> {
            let this = Self::alloc().set_ivars(tag);
            unsafe { msg_send![super(this), init] }
        }
    }

    #[test]
    fn the_target_records_the_senders_control_and_ignores_others() {
        let target = SettingsTarget::new();
        let send = |sender: Option<&AnyObject>| unsafe {
            let _: () = msg_send![&*target, settingChanged: sender];
        };
        let show_popup = FakeControl::new(Control::ShowPopup.tag());
        let unknown = FakeControl::new(999);

        send(Some(&show_popup));
        send(Some(&unknown));
        send(None);

        assert_eq!(target.take_changed(), [Control::ShowPopup]);
    }

    #[test]
    fn the_target_hands_over_each_changed_control_once_in_order() {
        let target = SettingsTarget::new();
        for control in [
            Control::PlaySound,
            Control::LaunchAtLogin,
            Control::PlaySound,
        ] {
            let sender = FakeControl::new(control.tag());
            let _: () = unsafe { msg_send![&*target, settingChanged: Some(&*sender)] };
        }

        assert_eq!(
            target.take_changed(),
            [Control::LaunchAtLogin, Control::PlaySound]
        );
        assert!(target.take_changed().is_empty());
    }

    fn press(key_code: u16, modifiers: NSEventModifierFlags) -> KeyPress {
        KeyPress {
            key_code,
            modifiers,
        }
    }

    fn down(key_code: u16, modifiers: NSEventModifierFlags) -> KeyEvent {
        KeyEvent::Down {
            key_code,
            modifiers,
            is_repeat: false,
        }
    }

    fn shift_command() -> NSEventModifierFlags {
        NSEventModifierFlags::Shift | NSEventModifierFlags::Command
    }

    fn no_modifiers() -> NSEventModifierFlags {
        NSEventModifierFlags::empty()
    }

    const KEY_M: u16 = 0x2e;
    const KEY_C: u16 = 0x08;
    const KEY_2: u16 = 0x13;
    const KEY_F5: u16 = 0x60;
    const KEY_F13: u16 = 0x69;
    const KEY_SPACE: u16 = 0x31;
    const KEY_NUMPAD_0: u16 = 0x52;
    const KEY_RETURN: u16 = 0x24;
    const KEY_LEFT_ARROW: u16 = 0x7b;
    const KEY_DELETE: u16 = 0x33;
    const KEY_FN: u16 = 0x3f;

    #[test]
    fn every_recorded_key_parses_back_to_the_same_key() {
        for &(key_code, code) in KEY_CODES {
            let config = ShortcutConfig {
                modifiers: vec!["ctrl".to_string(), "meta".to_string()],
                key: key_label(code),
            };
            let shortcut = parse_shortcut(&config)
                .unwrap_or_else(|e| panic!("key code {key_code:#04x}: {e:#}"));
            assert_eq!(shortcut.code(), code, "key code {key_code:#04x}");
            assert_eq!(code_for_key(key_code), Some(code));
        }
    }

    #[test]
    fn no_two_key_codes_share_a_key() {
        let mut key_codes: Vec<u16> = KEY_CODES.iter().map(|&(key_code, _)| key_code).collect();
        let mut keys: Vec<String> = KEY_CODES.iter().map(|&(_, key)| key.to_string()).collect();
        key_codes.sort_unstable();
        key_codes.dedup();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(key_codes.len(), KEY_CODES.len());
        assert_eq!(keys.len(), KEY_CODES.len());
    }

    #[test]
    fn keys_global_hotkey_cant_register_are_unknown() {
        // Caps Lock, Fn, and the JIS keyboard's Eisu and Kana keys.
        for key_code in [0x39, KEY_FN, 0x66, 0x68] {
            assert_eq!(code_for_key(key_code), None, "key code {key_code:#04x}");
        }
    }

    #[test]
    fn a_press_records_the_shortcut_as_settings_json_writes_it() {
        assert_eq!(
            record_press(press(KEY_M, shift_command())),
            Recorded::Shortcut(ShortcutConfig::default())
        );
        let every_modifier = NSEventModifierFlags::Control
            | NSEventModifierFlags::Option
            | NSEventModifierFlags::Shift
            | NSEventModifierFlags::Command;
        assert_eq!(
            record_press(press(KEY_F13, every_modifier)),
            Recorded::Shortcut(ShortcutConfig {
                modifiers: ["ctrl", "alt", "shift", "meta"].map(String::from).to_vec(),
                key: "F13".to_string(),
            })
        );
    }

    #[test]
    fn caps_lock_fn_and_the_keypad_flag_arent_modifiers() {
        let flags = shift_command()
            | NSEventModifierFlags::CapsLock
            | NSEventModifierFlags::Function
            | NSEventModifierFlags::NumericPad;
        assert_eq!(
            record_press(press(KEY_M, flags)),
            Recorded::Shortcut(ShortcutConfig::default())
        );
    }

    #[test]
    fn escape_cancels_unless_a_modifier_is_held() {
        assert_eq!(
            record_press(press(ESCAPE_KEY_CODE, no_modifiers())),
            Recorded::Cancel
        );
        assert_eq!(
            record_press(press(ESCAPE_KEY_CODE, NSEventModifierFlags::CapsLock)),
            Recorded::Cancel
        );
        assert_eq!(
            record_press(press(ESCAPE_KEY_CODE, NSEventModifierFlags::Control)),
            Recorded::Shortcut(ShortcutConfig {
                modifiers: vec!["ctrl".to_string()],
                key: "Escape".to_string(),
            })
        );
    }

    #[test]
    fn a_key_without_modifiers_is_rejected_unless_parse_shortcut_takes_it() {
        assert_eq!(
            record_press(press(KEY_M, no_modifiers())),
            Recorded::Rejected(
                "Key \"M\" needs a modifier; only F13–F24 work without one".to_string()
            )
        );
        assert_eq!(
            record_press(press(KEY_F13, no_modifiers())),
            Recorded::Shortcut(ShortcutConfig {
                modifiers: Vec::new(),
                key: "F13".to_string(),
            })
        );
    }

    #[test]
    fn shift_or_option_alone_on_a_text_key_is_rejected() {
        assert_eq!(
            record_press(press(KEY_M, NSEventModifierFlags::Shift)),
            Recorded::Rejected("⇧M types or edits text in every app; add ⌃ or ⌘".to_string())
        );
        let shift = NSEventModifierFlags::Shift;
        let option = NSEventModifierFlags::Option;
        let text_keys = [
            (KEY_2, option),
            (KEY_M, shift | option),
            (KEY_SPACE, shift),
            (KEY_NUMPAD_0, option),
            (KEY_RETURN, shift),
            (KEY_LEFT_ARROW, option),
            (KEY_DELETE, option),
        ];
        for (key_code, flags) in text_keys {
            let recorded = record_press(press(key_code, flags));
            assert!(
                matches!(&recorded, Recorded::Rejected(reason) if reason.contains("edits text")),
                "{key_code:#04x}: {recorded:?}"
            );
        }
        // Function keys and Esc don't edit text, and ⌃ or ⌘ make a text key a shortcut.
        let shortcuts = [
            (KEY_F5, option),
            (ESCAPE_KEY_CODE, shift),
            (
                KEY_M,
                NSEventModifierFlags::Control | NSEventModifierFlags::Shift,
            ),
            (KEY_M, NSEventModifierFlags::Command),
        ];
        for (key_code, flags) in shortcuts {
            let recorded = record_press(press(key_code, flags));
            assert!(
                matches!(recorded, Recorded::Shortcut(_)),
                "{key_code:#04x}: {recorded:?}"
            );
        }
    }

    #[test]
    fn a_key_global_hotkey_cant_register_is_rejected() {
        assert_eq!(
            record_press(press(KEY_FN, NSEventModifierFlags::Command)),
            Recorded::Rejected("Mic Mute can't use that key in a shortcut".to_string())
        );
    }

    #[test]
    fn the_recorder_stops_at_the_first_key_that_is_a_shortcut() {
        let now = Instant::now();
        let mut recording = Recording::new(now);
        let presses = [
            press(KEY_M, no_modifiers()),
            press(KEY_M, shift_command()),
            press(KEY_F13, no_modifiers()),
        ];
        assert_eq!(
            advance(&mut recording, &presses, None, now),
            Step::Record(ShortcutConfig::default())
        );
    }

    #[test]
    fn escape_or_the_timeout_stops_the_recorder_without_a_shortcut() {
        let start = Instant::now();
        let mut recording = Recording::new(start);
        let almost = start + RECORDING_TIMEOUT - Duration::from_millis(1);
        assert_eq!(advance(&mut recording, &[], None, almost), Step::Wait);
        let timeout = start + RECORDING_TIMEOUT;
        assert_eq!(advance(&mut recording, &[], None, timeout), Step::Stop);

        let mut recording = Recording::new(start);
        let escape = [press(ESCAPE_KEY_CODE, no_modifiers())];
        assert_eq!(advance(&mut recording, &escape, None, start), Step::Stop);
    }

    #[test]
    fn a_rejected_key_says_why_and_restarts_the_timeout() {
        let start = Instant::now();
        let mut recording = Recording::new(start);
        let later = start + Duration::from_secs(20);
        let plain_m = [press(KEY_M, no_modifiers())];
        assert_eq!(advance(&mut recording, &plain_m, None, later), Step::Wait);
        assert_eq!(recording.deadline, later + RECORDING_TIMEOUT);

        let view = view_for(
            &Settings::default(),
            &status_ok(),
            None,
            false,
            Some(&recording),
        );
        assert_eq!(
            view.shortcut_note.as_deref(),
            Some(
                "Key \"M\" needs a modifier; only F13–F24 work without one. Press the new \
                 shortcut, or Esc to cancel."
            )
        );
        assert!(view.is_shortcut_note_error);
        let first_deadline = start + RECORDING_TIMEOUT;
        assert_eq!(
            advance(&mut recording, &[], None, first_deadline),
            Step::Wait
        );
    }

    #[test]
    fn while_recording_the_view_shows_the_modifiers_held_and_cancel() {
        let now = Instant::now();
        let mut recording = Recording::new(now);
        let settings = Settings::default();
        let view = view_for(&settings, &status_ok(), None, false, Some(&recording));
        assert_eq!(view.shortcut, RECORDING_PROMPT);
        assert_eq!(view.shortcut_note.as_deref(), Some(RECORDING_HINT));
        assert!(!view.is_shortcut_note_error);
        assert_eq!(view.record_title, CANCEL_RECORDING_TITLE);

        let held = NSEventModifierFlags::Control | NSEventModifierFlags::Option;
        assert_eq!(advance(&mut recording, &[], Some(held), now), Step::Wait);
        let view = view_for(&settings, &status_ok(), None, false, Some(&recording));
        assert_eq!(view.shortcut, "⌃⌥…");

        advance(&mut recording, &[], Some(no_modifiers()), now);
        let view = view_for(&settings, &status_ok(), None, false, Some(&recording));
        assert_eq!(view.shortcut, RECORDING_PROMPT);

        let idle = view_for(&settings, &status_ok(), None, false, None);
        assert_eq!(idle.shortcut, "⇧⌘M");
        assert_eq!(idle.record_title, RECORD_TITLE);
    }

    #[test]
    fn outside_a_recording_the_monitor_takes_only_close_and_the_edit_commands() {
        let command = NSEventModifierFlags::Command;
        assert_eq!(catch(down(W_KEY_CODE, command), false), Catch::Close);
        let edits = [
            (0x07, EditCommand::Cut),
            (KEY_C, EditCommand::Copy),
            (0x09, EditCommand::Paste),
            (0x00, EditCommand::SelectAll),
        ];
        for (key_code, edit) in edits {
            assert_eq!(catch(down(key_code, command), false), Catch::Edit(edit));
            assert_eq!(catch(down(key_code, shift_command()), false), Catch::Pass);
        }
        assert_eq!(catch(down(W_KEY_CODE, no_modifiers()), false), Catch::Pass);
        assert_eq!(catch(down(W_KEY_CODE, shift_command()), false), Catch::Pass);
        assert_eq!(catch(down(KEY_M, shift_command()), false), Catch::Pass);
        assert_eq!(
            catch(KeyEvent::ModifiersChanged(command), false),
            Catch::Pass
        );
    }

    #[test]
    fn while_recording_the_monitor_takes_every_key_and_still_closes_on_command_w() {
        let flags = shift_command();
        assert_eq!(
            catch(down(KEY_M, flags), true),
            Catch::Press(press(KEY_M, flags))
        );
        // ⌘C is a shortcut to record then, not Copy.
        let command = NSEventModifierFlags::Command;
        assert_eq!(
            catch(down(KEY_C, command), true),
            Catch::Press(press(KEY_C, command))
        );
        let repeat = KeyEvent::Down {
            key_code: KEY_M,
            modifiers: flags,
            is_repeat: true,
        };
        assert_eq!(catch(repeat, true), Catch::Swallow);
        assert_eq!(
            catch(KeyEvent::ModifiersChanged(flags), true),
            Catch::Held(flags)
        );
        assert_eq!(catch(down(W_KEY_CODE, command), true), Catch::Close);
    }

    #[test]
    fn the_monitor_swallows_only_the_keys_it_handles() {
        assert!(Catch::Press(press(KEY_M, shift_command())).swallows());
        assert!(Catch::Swallow.swallows());
        assert!(Catch::Close.swallows());
        assert!(Catch::Edit(EditCommand::Copy).swallows());
        // Modifier changes still reach AppKit, which tracks them for every window.
        assert!(!Catch::Held(shift_command()).swallows());
        assert!(!Catch::Pass.swallows());
    }

    #[test]
    fn the_catcher_keeps_presses_only_while_recording_and_hands_them_over_once() {
        let catcher = KeyCatcher::default();
        let flags = shift_command();
        assert_eq!(catcher.catch(down(KEY_M, flags)), Catch::Pass);
        assert!(catcher.take_presses().is_empty());

        catcher.set_recording(true);
        for key_code in [0x00, 0x01, 0x02, 0x03, 0x04] {
            catcher.catch(down(key_code, flags));
        }
        catcher.catch(KeyEvent::ModifiersChanged(NSEventModifierFlags::Control));
        let kept: Vec<u16> = catcher
            .take_presses()
            .iter()
            .map(|press| press.key_code)
            .collect();
        assert_eq!(
            kept,
            [0x00, 0x01, 0x02, 0x03],
            "a full queue drops later keys"
        );
        assert!(catcher.take_presses().is_empty());
        assert_eq!(catcher.take_held(), Some(NSEventModifierFlags::Control));
        assert_eq!(catcher.take_held(), None);
    }

    #[test]
    fn starting_or_stopping_a_recording_drops_the_keys_caught_before() {
        let catcher = KeyCatcher::default();
        catcher.set_recording(true);
        catcher.catch(down(KEY_M, shift_command()));
        catcher.catch(KeyEvent::ModifiersChanged(shift_command()));
        catcher.set_recording(false);
        assert!(catcher.take_presses().is_empty());
        assert_eq!(catcher.take_held(), None);
    }

    #[test]
    fn the_catcher_hands_over_close_and_edit_commands_once() {
        let catcher = KeyCatcher::default();
        let command = NSEventModifierFlags::Command;
        catcher.catch(down(0x00, command));
        catcher.catch(down(KEY_C, command));
        catcher.catch(down(W_KEY_CODE, command));
        assert_eq!(
            catcher.take_edits(),
            [EditCommand::SelectAll, EditCommand::Copy]
        );
        assert!(catcher.take_edits().is_empty());
        assert!(catcher.take_close_request());
        assert!(!catcher.take_close_request());
    }
}
