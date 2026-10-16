//! The Settings window: every setting in settings.json as a native control, except
//! `excluded_devices`, which the tray's Microphones menu edits next to each device's state.
//!
//! It is a plain `NSWindow`, never run modally, so mute enforcement, the hotkey, and the
//! settings reload keep running while it is open. Like About's buttons, its controls only record
//! that they changed (`SettingsTarget`). The event loop takes the changes
//! (`SettingsWindow::take_events`), saves each one the way the tray toggles do, and then shows the
//! settings in use again (`SettingsWindow::refresh`), so changes made elsewhere (the tray,
//! About → Reset Settings, a hand edit of the file) show here too.
use crate::settings::{
    Fallback, LaunchState, Lenient, LoadError, MicOnFlash, PopupPosition, PopupSize, Settings,
    ShortcutMode,
};
use crate::shortcuts::{current_status, ShortcutStatus};
use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSGridCell, NSGridCellPlacement, NSGridRow, NSGridRowAlignment,
    NSGridView, NSLayoutAttribute, NSPopUpButton, NSStackView, NSStepper, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSInteger, NSPoint, NSRange, NSRect, NSSize, NSString,
};
use std::sync::atomic::{AtomicU32, Ordering};

/// The window's title.
const WINDOW_TITLE: &str = "Mic Mute Settings";
/// How wide the notice, the footer, and the notes under the controls wrap, in points.
const WIDE_TEXT_WIDTH: f64 = 440.;
const NOTE_WIDTH: f64 = 320.;
/// The margin around the window's content, in points.
const CONTENT_MARGIN: f64 = 20.;
/// How far the "Mic on" stepper moves per click, in milliseconds.
const MIC_ON_STEP_MS: f64 = 100.;
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
    Mode(ShortcutMode),
    ShowPopup(bool),
    PopupPosition(PopupPosition),
    PopupSize(PopupSize),
    /// Milliseconds; more than `MicOnFlash::MAX_MS` is capped.
    PopupMicOnMs(u64),
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
    /// Open settings.json in the text editor (the invalid-file notice's button).
    OpenSettingsFile,
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

/// What the window shows: one field per control, so `refresh` touches only what changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    problem: Option<FileProblem>,
    launch_at_login: bool,
    launch_at_login_title: &'static str,
    show_in_dock: bool,
    shortcut: String,
    shortcut_note: Option<String>,
    mode: usize,
    show_popup: bool,
    position: usize,
    size: usize,
    mic_on_ms: u64,
    popup_note: Option<String>,
    play_sound: bool,
    show_notification: bool,
    launch_state: usize,
    mute_on_lock: bool,
    mute_on_sleep: bool,
    mute_on_display_sleep: bool,
    allow_url_unmute: bool,
}

/// The window's contents for `settings`, the shortcut's `status`, and the file's `problem`.
/// `needs_approval`: macOS waits for the user to approve the login item.
fn view_for(
    settings: &Settings,
    status: &ShortcutStatus,
    problem: Option<&FileProblem>,
    needs_approval: bool,
) -> View {
    View {
        problem: problem.cloned(),
        launch_at_login: settings.launch_at_login,
        launch_at_login_title: crate::launch_at_login::menu_title(
            settings.launch_at_login && needs_approval,
        ),
        show_in_dock: settings.show_in_dock,
        shortcut: shortcut_text(status),
        shortcut_note: shortcut_note(status),
        mode: index_of(&MODE_CHOICES, settings.mode),
        show_popup: settings.show_popup,
        position: index_of(&POSITION_CHOICES, settings.popup_position.value()),
        size: index_of(&SIZE_CHOICES, settings.popup_size.value()),
        mic_on_ms: mic_on_millis(settings.popup_mic_on_ms.value()),
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
    Mode,
    ShowPopup,
    PopupPosition,
    PopupSize,
    MicOnField,
    MicOnStepper,
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
    const ALL: [Self; 16] = [
        Self::LaunchAtLogin,
        Self::ShowInDock,
        Self::Mode,
        Self::ShowPopup,
        Self::PopupPosition,
        Self::PopupSize,
        Self::MicOnField,
        Self::MicOnStepper,
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
    shortcut_note: Retained<NSTextField>,
    shortcut_note_row: Retained<NSGridRow>,
    mode: Retained<NSPopUpButton>,
    show_popup: Retained<NSButton>,
    position: Retained<NSPopUpButton>,
    size: Retained<NSPopUpButton>,
    mic_on_field: Retained<NSTextField>,
    mic_on_stepper: Retained<NSStepper>,
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
        let shortcut_note = note_label("", NOTE_WIDTH, mtm);
        shortcut_note.setTextColor(Some(&NSColor::systemRedColor()));
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
        let mic_on_field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
        mic_on_field.setTag(Control::MicOnField.tag());
        // SAFETY: as in `checkbox`.
        unsafe {
            mic_on_field.setTarget(Some(target));
            mic_on_field.setAction(Some(action()));
        }
        // Also send the action when editing ends with Tab or a click elsewhere, not only Return.
        if let Some(cell) = mic_on_field.cell() {
            cell.setSendsActionOnEndEditing(true);
        }
        mic_on_field
            .widthAnchor()
            .constraintEqualToConstant(64.)
            .setActive(true);
        let mic_on_stepper = NSStepper::initWithFrame(NSStepper::alloc(mtm), NSRect::ZERO);
        mic_on_stepper.setMinValue(0.);
        mic_on_stepper.setMaxValue(MicOnFlash::MAX_MS as f64);
        mic_on_stepper.setIncrement(MIC_ON_STEP_MS);
        mic_on_stepper.setValueWraps(false);
        mic_on_stepper.setTag(Control::MicOnStepper.tag());
        // SAFETY: as in `checkbox`.
        unsafe {
            mic_on_stepper.setTarget(Some(target));
            mic_on_stepper.setAction(Some(action()));
        }
        let ms = label("ms", mtm);
        let mic_on = stack(&[&mic_on_field, &ms, &mic_on_stepper], horizontal, 6., mtm);
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
            Row::Setting(shortcut_label, &shortcut),
            Row::Setting(&empty, &shortcut_note),
            Row::Setting(mode_label, &mode),
            Row::Header(popup_header),
            Row::Setting(&empty, &show_popup),
            Row::Setting(position_label, &position),
            Row::Setting(size_label, &size),
            Row::Setting(mic_on_label, &mic_on),
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
        const SHORTCUT_NOTE_ROW: NSInteger = 5;
        const POPUP_NOTE_ROW: NSInteger = 12;
        let grid = grid(&rows, mtm);
        let shortcut_note_row = grid.rowAtIndex(SHORTCUT_NOTE_ROW);
        let popup_note_row = grid.rowAtIndex(POPUP_NOTE_ROW);

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
            shortcut_note,
            shortcut_note_row,
            mode,
            show_popup,
            position,
            size,
            mic_on_field,
            mic_on_stepper,
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

    /// Shows `new`, touching only what differs from `old`; `is_mic_on_stale` rewrites the
    /// "Mic on" field after an edit, which may have left text the setting doesn't hold.
    /// Returns whether the content's size may have changed.
    fn show(&self, old: Option<&View>, new: &View, is_mic_on_stale: bool) -> bool {
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
            let ms = new.mic_on_ms;
            self.mic_on_field
                .setStringValue(&NSString::from_str(&ms.to_string()));
            self.mic_on_stepper
                .setIntegerValue(NSInteger::try_from(ms).unwrap_or(0));
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
        Self {
            window,
            controls,
            target,
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

    /// What the user did in the window since the last call, in `Control::ALL` order.
    pub fn take_events(&mut self) -> Vec<WindowEvent> {
        let Some(built) = &self.built else {
            return Vec::new();
        };
        built
            .target
            .take_changed()
            .into_iter()
            .filter_map(|control| match control {
                Control::OpenSettingsFile => Some(WindowEvent::OpenSettingsFile),
                _ => self.change_for(control).map(WindowEvent::Changed),
            })
            .collect()
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
            Control::MicOnStepper => SettingsChange::PopupMicOnMs(
                u64::try_from(controls.mic_on_stepper.integerValue()).unwrap_or(0),
            ),
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
            Control::OpenSettingsFile => return None,
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
        );
        if self.shown.as_ref() == Some(&view) && !self.is_mic_on_stale {
            return;
        }
        let is_resized = built
            .controls
            .show(self.shown.as_ref(), &view, self.is_mic_on_stale);
        self.is_mic_on_stale = false;
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
            SettingsChange::Mode(ShortcutMode::PushToTalk),
            SettingsChange::ShowPopup(false),
            SettingsChange::PopupPosition(PopupPosition::TopRight),
            SettingsChange::PopupSize(PopupSize::Large),
            SettingsChange::PopupMicOnMs(2_500),
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
        assert_eq!(saved["mode"], "push_to_talk");
        assert_eq!(saved["show_popup"], false);
        assert_eq!(saved["popup_position"], "top_right");
        assert_eq!(saved["popup_size"], "large");
        assert_eq!(saved["popup_mic_on_ms"], 2_500);
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

        let view = view_for(&settings, &status_ok(), None, false);

        assert!(view.launch_at_login && view.show_in_dock && !view.show_popup);
        assert_eq!(view.launch_at_login_title, "Launch at Login");
        assert_eq!(view.shortcut, "⇧⌘M");
        assert_eq!(view.shortcut_note, None);
        assert_eq!(MODE_CHOICES[view.mode].0, ShortcutMode::PushToTalk);
        assert_eq!(POSITION_CHOICES[view.position].0, PopupPosition::TopLeft);
        assert_eq!(SIZE_CHOICES[view.size].0, PopupSize::Small);
        assert_eq!(view.mic_on_ms, 0);
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
        let view = view_for(&Settings::default(), &status_ok(), None, false);
        assert!(!view.launch_at_login && !view.show_in_dock && view.show_popup);
        assert_eq!(MODE_CHOICES[view.mode].0, ShortcutMode::Toggle);
        assert_eq!(
            POSITION_CHOICES[view.position].0,
            PopupPosition::BottomCenter
        );
        assert_eq!(SIZE_CHOICES[view.size].0, PopupSize::Medium);
        assert_eq!(view.mic_on_ms, MicOnFlash::DEFAULT_MS);
        assert_eq!(LAUNCH_STATE_CHOICES[view.launch_state].0, LaunchState::Keep);
        assert!(!view.allow_url_unmute);
    }

    #[test]
    fn launch_at_login_points_to_system_settings_while_it_waits_for_approval() {
        let on = Settings {
            launch_at_login: true,
            ..Settings::default()
        };
        let waiting = view_for(&on, &status_ok(), None, true);
        assert_eq!(
            waiting.launch_at_login_title,
            crate::launch_at_login::menu_title(true)
        );
        // Off, there is nothing to approve.
        let off = view_for(&Settings::default(), &status_ok(), None, true);
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
        let view = view_for(&Settings::default(), &none, None, false);
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

        let view = view_for(&settings, &status_ok(), None, false);

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

        let view = view_for(&Settings::default(), &status_ok(), Some(&problem), false);
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
}
