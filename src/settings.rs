use crate::utils::write_atomically;
use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutConfig {
    #[serde(default)]
    pub modifiers: Vec<String>, // ["shift", "meta", "ctrl", "alt"]
    pub key: String, // "A", "M", "F13", etc.
}

impl Default for ShortcutConfig {
    /// ⌘⇧M. Not ⌘M: that is the system-wide Minimize shortcut.
    fn default() -> Self {
        Self {
            modifiers: vec!["shift".to_string(), "meta".to_string()],
            key: "M".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mic_shortcut: ShortcutConfig,
    #[serde(default)]
    pub show_in_dock: bool,
    #[serde(default)]
    pub launch_at_login: bool,
    #[serde(default = "default_show_popup")]
    pub show_popup: bool,

    /// Input devices Mic Mute leaves alone: it never mutes, unmutes, or enforces them, and
    /// they don't count toward the mute state. Empty by default, so every device is muted.
    #[serde(default)]
    pub excluded_devices: Vec<ExcludedDevice>,

    // Inputs besides the tray menu: the shortcut's mode (`push_to_talk`), URLs (`url_scheme`),
    // and automatic mute (`mute_triggers`), which never unmutes.
    /// What the mute shortcut does: toggle, or unmute only while held.
    #[serde(default)]
    pub mode: ShortcutMode,
    /// Whether `unmute` and `toggle` URLs may run. Off by default: any app or web page can open
    /// a URL, so by default URLs can only mute and show the state.
    #[serde(default)]
    pub allow_url_unmute: bool,
    /// Whether a launch mutes: never, always, or when the user last chose mute.
    #[serde(default)]
    pub launch_state: LaunchState,
    /// Mute when the screen locks or another user's session becomes active.
    #[serde(default)]
    pub mute_on_lock: bool,
    /// Mute when the Mac goes to sleep.
    #[serde(default)]
    pub mute_on_sleep: bool,
    /// Mute when the displays go to sleep.
    #[serde(default)]
    pub mute_on_display_sleep: bool,

    // Feedback on a mute change: a sound, a notification, and the popup's position, size,
    // and "Mic on" flash.
    /// Plays a system sound when the user mutes or unmutes.
    #[serde(default)]
    pub play_sound: bool,
    /// Posts a macOS notification when the user mutes or unmutes.
    #[serde(default)]
    pub show_notification: bool,
    /// Where the popup sits on the cursor's monitor.
    #[serde(default)]
    pub popup_position: Lenient<PopupPosition>,
    /// How large the popup is.
    #[serde(default)]
    pub popup_size: Lenient<PopupSize>,
    /// How long the popup shows "Mic on" after the user unmutes.
    #[serde(default)]
    pub popup_mic_on_ms: Lenient<MicOnFlash>,
    /// Fields this version doesn't know (written by a newer version or a fork), saved back
    /// unchanged. Empty when the file has none, as in `impl Default`.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// What settings.json held when these settings were loaded from it or saved to it, so
    /// `update` can tell a hand edit made since. Not a setting: it's never saved.
    #[serde(skip)]
    pub file_version: FileVersion,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_shortcut: ShortcutConfig::default(),
            show_in_dock: false,
            launch_at_login: false,
            show_popup: true,
            excluded_devices: Vec::new(),
            // Inputs besides the tray menu.
            mode: ShortcutMode::Toggle,
            allow_url_unmute: false,
            launch_state: LaunchState::Keep,
            mute_on_lock: false,
            mute_on_sleep: false,
            mute_on_display_sleep: false,
            // Feedback on a mute change.
            play_sound: false,
            show_notification: false,
            popup_position: Lenient::default(),
            popup_size: Lenient::default(),
            popup_mic_on_ms: Lenient::default(),
            extra: serde_json::Map::new(),
            // The defaults don't come from the file.
            file_version: FileVersion::Unknown,
        }
    }
}

fn default_show_popup() -> bool {
    true
}

/// An input device excluded from muting, identified by its CoreAudio device UID, which stays
/// the same across reconnects, restarts, and renames. `name` is only for showing the device
/// while it isn't connected.
///
/// In the file, an entry is `{ "uid": …, "name": … }`, or a bare UID string, as upstream's
/// `excluded_devices` writes it; an entry without a name is saved back as a bare string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ExcludedDeviceEntry", into = "ExcludedDeviceEntry")]
pub struct ExcludedDevice {
    pub uid: String,
    pub name: String,
}

/// How an `excluded_devices` entry is written in settings.json.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ExcludedDeviceEntry {
    Uid(String),
    Device {
        uid: String,
        #[serde(default)]
        name: String,
    },
}

impl From<ExcludedDeviceEntry> for ExcludedDevice {
    fn from(entry: ExcludedDeviceEntry) -> Self {
        match entry {
            ExcludedDeviceEntry::Uid(uid) => Self {
                uid,
                name: String::new(),
            },
            ExcludedDeviceEntry::Device { uid, name } => Self { uid, name },
        }
    }
}

impl From<ExcludedDevice> for ExcludedDeviceEntry {
    fn from(device: ExcludedDevice) -> Self {
        if device.name.is_empty() {
            Self::Uid(device.uid)
        } else {
            Self::Device {
                uid: device.uid,
                name: device.name,
            }
        }
    }
}

impl Settings {
    /// The UIDs of the excluded devices. An empty UID matches no device, so it's left out.
    pub fn excluded_uids(&self) -> BTreeSet<String> {
        self.excluded_devices
            .iter()
            .filter(|device| !device.uid.is_empty())
            .map(|device| device.uid.clone())
            .collect()
    }

    /// Whether the device with `uid` is excluded.
    pub fn is_device_excluded(&self, uid: &str) -> bool {
        self.excluded_devices.iter().any(|device| device.uid == uid)
    }

    /// Excludes `device`, saving its name with it, or, when `is_excluded` is false, includes
    /// it again by removing every entry with its UID. A device already excluded keeps its
    /// entry. A tray click sets the state opposite to the one the menu showed, rather than
    /// toggling, so it means the same when `update` first reloads a hand edit.
    pub fn set_device_excluded(&mut self, device: &ExcludedDevice, is_excluded: bool) {
        if !is_excluded {
            self.excluded_devices
                .retain(|entry| entry.uid != device.uid);
        } else if !self.is_device_excluded(&device.uid) {
            self.excluded_devices.push(device.clone());
        }
    }
}

// Inputs besides the tray menu.

/// What the mute shortcut does (`mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutMode {
    /// Each press mutes or unmutes.
    #[default]
    Toggle,
    /// Holding the shortcut unmutes; releasing it mutes again.
    PushToTalk,
}

/// What a launch does with the microphones (`launch_state`). None of the choices unmutes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchState {
    /// Leave them as they are: muted when every microphone is muted.
    #[default]
    Keep,
    /// Mute every microphone.
    Muted,
    /// Mute if the user last chose mute (`mute-state.json`); otherwise like `Keep`.
    Last,
}

// Feedback on a mute change: the popup's position, size, and "Mic on" flash. These are read
// leniently (`Lenient`): a value this build doesn't accept falls back to its default with a
// warning instead of making the whole file invalid.

/// A setting whose value in settings.json may be wrong without breaking the file: a value this
/// build doesn't accept falls back (`LenientValue::parse`), `problem` says why for the log, and
/// the file's value is saved back unchanged, so a typo stays where the user can fix it and a
/// newer version's value survives.
#[derive(Clone, Debug, PartialEq)]
pub struct Lenient<T> {
    value: T,
    /// The file's value and what's wrong with it, when `value` isn't what it says.
    rejected: Option<(serde_json::Value, String)>,
}

/// A value `Lenient` reads from one settings.json key.
pub trait LenientValue: Copy + Default {
    /// The setting's key in settings.json.
    const KEY: &'static str;

    /// The value `json` stands for, or the value to use instead and why.
    fn parse(json: &serde_json::Value) -> Result<Self, (Self, String)>;

    fn to_json(self) -> serde_json::Value;
}

impl<T: LenientValue> Lenient<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            rejected: None,
        }
    }

    /// The value in use.
    pub fn value(&self) -> T {
        self.value
    }

    /// Why the file's value isn't used as written, e.g. `popup_size "huge" isn't one of
    /// "small", "medium", "large"; using "medium"`.
    pub fn problem(&self) -> Option<String> {
        self.rejected.as_ref().map(|(json, reason)| {
            format!(
                "{} {} {}; using {}",
                T::KEY,
                json,
                reason,
                self.value.to_json()
            )
        })
    }
}

impl<T: LenientValue> Default for Lenient<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: LenientValue> Serialize for Lenient<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.rejected {
            Some((json, _)) => json.serialize(serializer),
            None => self.value.to_json().serialize(serializer),
        }
    }
}

impl<'de, T: LenientValue> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Ok(match T::parse(&json) {
            Ok(value) => Self::new(value),
            Err((value, reason)) => Self {
                value,
                rejected: Some((json, reason)),
            },
        })
    }
}

/// The entry of `all` that `json` names, in any letter case, or why it names none.
fn parse_name<T: Copy>(
    json: &serde_json::Value,
    all: &[T],
    name: fn(T) -> &'static str,
) -> Result<T, String> {
    json.as_str()
        .and_then(|text| {
            all.iter()
                .copied()
                .find(|&entry| name(entry).eq_ignore_ascii_case(text))
        })
        .ok_or_else(|| {
            let names: Vec<String> = all
                .iter()
                .map(|&entry| format!("\"{}\"", name(entry)))
                .collect();
            format!("isn't one of {}", names.join(", "))
        })
}

/// `popup_position`: where the popup sits on the cursor's monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    /// Where the popup has always been.
    #[default]
    BottomCenter,
    BottomRight,
}

impl PopupPosition {
    pub const ALL: [Self; 6] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top_left",
            Self::TopCenter => "top_center",
            Self::TopRight => "top_right",
            Self::BottomLeft => "bottom_left",
            Self::BottomCenter => "bottom_center",
            Self::BottomRight => "bottom_right",
        }
    }
}

impl LenientValue for PopupPosition {
    const KEY: &'static str = "popup_position";

    fn parse(json: &serde_json::Value) -> Result<Self, (Self, String)> {
        parse_name(json, &Self::ALL, Self::name).map_err(|reason| (Self::default(), reason))
    }

    fn to_json(self) -> serde_json::Value {
        self.name().into()
    }
}

/// `popup_size`: how large the popup, its text, and its icons are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupSize {
    Small,
    /// The size the popup has always had.
    #[default]
    Medium,
    Large,
}

impl PopupSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

impl LenientValue for PopupSize {
    const KEY: &'static str = "popup_size";

    fn parse(json: &serde_json::Value) -> Result<Self, (Self, String)> {
        parse_name(json, &Self::ALL, Self::name).map_err(|reason| (Self::default(), reason))
    }

    fn to_json(self) -> serde_json::Value {
        self.name().into()
    }
}

/// `popup_mic_on_ms`: how long the popup stays up showing "Mic on" after the user unmutes; 0
/// hides it at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicOnFlash {
    /// At most `MAX_MS`.
    ms: u64,
}

impl MicOnFlash {
    /// One second: the flash the popup has always had.
    pub const DEFAULT_MS: u64 = 1_000;
    /// Ten seconds. "Mic on" confirms an unmute; it isn't meant to stay up.
    pub const MAX_MS: u64 = 10_000;

    /// A flash of `ms` milliseconds, capped at `MAX_MS`.
    pub fn from_millis(ms: u64) -> Self {
        Self {
            ms: ms.min(Self::MAX_MS),
        }
    }

    pub fn duration(self) -> Duration {
        Duration::from_millis(self.ms)
    }
}

impl Default for MicOnFlash {
    fn default() -> Self {
        Self::from_millis(Self::DEFAULT_MS)
    }
}

impl LenientValue for MicOnFlash {
    const KEY: &'static str = "popup_mic_on_ms";

    /// A whole number of milliseconds; a longer flash than `MAX_MS` is capped, anything else
    /// gets the default.
    fn parse(json: &serde_json::Value) -> Result<Self, (Self, String)> {
        match json.as_u64() {
            Some(ms) if ms <= Self::MAX_MS => Ok(Self::from_millis(ms)),
            Some(ms) => Err((
                Self::from_millis(ms),
                format!("is above the maximum, {}", Self::MAX_MS),
            )),
            None => Err((
                Self::default(),
                format!(
                    "isn't a whole number of milliseconds from 0 to {}",
                    Self::MAX_MS
                ),
            )),
        }
    }

    fn to_json(self) -> serde_json::Value {
        self.ms.into()
    }
}

impl Settings {
    /// What's wrong with the feedback settings the file sets to values this build doesn't use
    /// as written; one line each, for the log.
    pub fn feedback_problems(&self) -> Vec<String> {
        [
            self.popup_position.problem(),
            self.popup_size.problem(),
            self.popup_mic_on_ms.problem(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Why settings.json can't be used.
#[derive(Debug)]
pub enum LoadError {
    /// The file exists but can't be read.
    Read { path: PathBuf, source: io::Error },
    /// The file isn't valid settings JSON: a syntax error, a value of the wrong type, or an
    /// empty file.
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl LoadError {
    /// One-based line and column of a parse error.
    pub fn position(&self) -> Option<(usize, usize)> {
        match self {
            Self::Parse { source, .. } => Some((source.line(), source.column())),
            Self::Read { .. } => None,
        }
    }

    /// One-line notice for the tray, e.g.
    /// `settings.json is invalid (line 3, column 5) — using defaults`.
    pub fn notice(&self, fallback: Fallback) -> String {
        let path = self.path();
        let file = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        let problem = match self.position() {
            Some((line, column)) => format!("is invalid (line {line}, column {column})"),
            None => "can't be read".to_string(),
        };
        let fallback = match fallback {
            Fallback::Defaults => "using defaults",
            Fallback::Previous => "keeping previous settings",
        };
        format!("{file} {problem} — {fallback}")
    }

    fn path(&self) -> &Path {
        match self {
            Self::Read { path, .. } | Self::Parse { path, .. } => path,
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "Failed to read {}: {}", path.display(), source)
            }
            Self::Parse { path, source } => write!(
                f,
                "{} is invalid at line {}, column {}: {}",
                path.display(),
                source.line(),
                source.column(),
                serde_message(source)
            ),
        }
    }
}

// Display already includes the cause's message, so `source()` stays `None`.
impl std::error::Error for LoadError {}

/// serde_json's message without the " at line L column C" suffix its Display adds;
/// `LoadError` reports the position itself.
fn serde_message(err: &serde_json::Error) -> String {
    let message = err.to_string();
    let position = format!(" at line {} column {}", err.line(), err.column());
    match message.strip_suffix(&position) {
        Some(stripped) => stripped.to_string(),
        None => message,
    }
}

/// What the app runs on while settings.json is unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The built-in defaults: the file was already unusable at startup.
    Defaults,
    /// The settings in use before the file broke.
    Previous,
}

/// What `Settings::save_to` did.
#[derive(Debug)]
pub enum SaveOutcome {
    /// The file now holds these settings, at `version`.
    Saved { version: FileVersion },
    /// The file on disk is invalid, so it was left untouched. The change applies only in
    /// memory, until the file is fixed or the app quits.
    KeptInMemory(LoadError),
}

/// What `Settings::update` did.
#[derive(Debug)]
pub enum Update {
    /// The change changed nothing, so nothing was saved.
    Unchanged,
    /// The change was saved, or kept in memory while the file is invalid. Apply it alone.
    Changed(Result<SaveOutcome>),
    /// settings.json changed since these settings were loaded from it or saved to it, so they
    /// were reloaded from it before the change, and the change went into the file's settings;
    /// `None` when the file already had it, so nothing was saved. Apply every setting, as a
    /// reload does, not only the change.
    Reloaded(Option<Result<SaveOutcome>>),
}

impl Update {
    /// Whether the settings changed: by the change, or by reloading the file.
    pub fn is_changed(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    /// What saving did; `None` when nothing was saved because nothing needed to be.
    fn saved(&self) -> Option<&Result<SaveOutcome>> {
        match self {
            Self::Unchanged | Self::Reloaded(None) => None,
            Self::Changed(saved) | Self::Reloaded(Some(saved)) => Some(saved),
        }
    }
}

/// What settings.json held when some settings were loaded from it or saved to it. A save
/// compares it with the file (`is_current`) to tell whether the file changed since: a hand
/// edit the settings poll hasn't reloaded yet, or a fix to a file that was invalid.
#[derive(Clone, Copy, Debug, Default)]
pub enum FileVersion {
    /// Not known: the settings didn't come from the file, as the defaults that stand in for a
    /// file that was invalid at startup. No file is at this version.
    #[default]
    Unknown,
    /// There was no file.
    Missing,
    /// The file's modification time and a hash of its contents.
    Present {
        mtime: Option<SystemTime>,
        contents_hash: u64,
    },
}

impl FileVersion {
    fn of(contents: &[u8], mtime: Option<SystemTime>) -> Self {
        let mut hasher = DefaultHasher::new();
        contents.hash(&mut hasher);
        Self::Present {
            mtime,
            contents_hash: hasher.finish(),
        }
    }

    /// Whether the file is still at this version, `now` being the version it is at now: still
    /// missing, or with the same contents and the same modification time, known on both sides.
    fn is_current(self, now: Self) -> bool {
        match (self, now) {
            (Self::Missing, Self::Missing) => true,
            (
                Self::Present {
                    mtime,
                    contents_hash,
                },
                Self::Present {
                    mtime: now_mtime,
                    contents_hash: now_hash,
                },
            ) => is_same_mtime(mtime, now_mtime) && contents_hash == now_hash,
            _ => false,
        }
    }

    fn mtime(self) -> Option<SystemTime> {
        match self {
            Self::Present { mtime, .. } => mtime,
            Self::Unknown | Self::Missing => None,
        }
    }
}

/// Modification time of settings.json right after this process last saved it, until the
/// settings poll sees it (`Settings::take_own_save`).
static OWN_SAVE_MTIME: Mutex<Option<SystemTime>> = Mutex::new(None);

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Two modification times match only when both are known.
fn is_same_mtime(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    a.is_some() && a == b
}

/// Remembers the app's own save for `Settings::take_own_save`, or logs why it wrote nothing.
fn note_save_outcome(outcome: &SaveOutcome) {
    match outcome {
        SaveOutcome::Saved { version } => {
            *OWN_SAVE_MTIME.lock() = version.mtime();
        }
        SaveOutcome::KeptInMemory(err) => {
            log::warn!("Keeping the settings change in memory only: {}", err);
        }
    }
}

impl Settings {
    /// Loads settings.json. A missing file gives the defaults; an unreadable or invalid file is
    /// an error, so callers can keep the settings they have instead of resetting them.
    pub fn load() -> Result<Self, LoadError> {
        match Self::config_path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    /// Loads settings from `path` without ever writing it. A missing file gives the defaults.
    pub fn load_from(path: &Path) -> Result<Self, LoadError> {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    file_version: FileVersion::Missing,
                    ..Self::default()
                })
            }
            Err(source) => {
                return Err(LoadError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        let mut settings: Self =
            serde_json::from_slice(&data).map_err(|source| LoadError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        // Read after the contents: an edit in between changes the hash, so it still counts.
        settings.file_version = FileVersion::of(&data, file_mtime(path));
        Ok(settings)
    }

    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json"))
    }

    /// Returns the last-modified time of the settings file, or None if it doesn't exist.
    pub fn mtime() -> Option<SystemTime> {
        Self::config_path().and_then(|p| file_mtime(&p))
    }

    /// Whether `mtime`, a changed value of `Settings::mtime()`, is this process's last
    /// `save()`: the in-memory settings already match the file, so there is nothing to
    /// reload. Forgets that save either way, so only the first change after it can match.
    pub fn take_own_save(mtime: Option<SystemTime>) -> bool {
        let own_save = OWN_SAVE_MTIME.lock().take();
        is_same_mtime(mtime, own_save)
    }

    /// Saves these settings as they are to settings.json (see `save_to`) and remembers the
    /// file's new mtime so the settings poll doesn't reload the app's own write. Only for
    /// writing a missing file; save a change with `update`, which keeps a hand edit.
    pub fn save(&self) -> Result<SaveOutcome> {
        let path = Self::config_path().context("Failed to resolve the settings directory")?;
        let outcome = self.save_to(&path)?;
        note_save_outcome(&outcome);
        Ok(outcome)
    }

    /// Writes these settings, unknown fields included, to `path` atomically: `<name>.tmp`
    /// first, then a rename, so a crash never leaves a half-written file.
    ///
    /// Never replaces an existing file that is unreadable or invalid, so a hand edit in
    /// progress isn't lost: that writes nothing and returns `SaveOutcome::KeptInMemory`.
    pub fn save_to(&self, path: &Path) -> Result<SaveOutcome> {
        if let Err(err) = Self::load_from(path) {
            return Ok(SaveOutcome::KeptInMemory(err));
        }
        let mut data = serde_json::to_vec_pretty(self).context("Failed to serialize settings")?;
        data.push(b'\n');
        write_atomically(path, &data)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(SaveOutcome::Saved {
            version: FileVersion::of(&data, file_mtime(path)),
        })
    }

    /// Applies `change` and saves the settings to settings.json with `update_at`. Logs a save
    /// that failed or that an invalid file prevented, and remembers the file's new mtime so the
    /// settings poll doesn't reload the app's own write.
    pub fn update(&mut self, change: impl FnOnce(&mut Self)) -> Update {
        let update = match Self::config_path() {
            Some(path) => self.update_at(&path, change),
            None => {
                let is_changed = self.apply(change);
                if is_changed {
                    Update::Changed(Err(anyhow!("Failed to resolve the settings directory")))
                } else {
                    Update::Unchanged
                }
            }
        };
        if let Update::Reloaded(_) = update {
            log::info!(
                "settings.json changed since Mic Mute last read it; reloaded it before the change"
            );
        }
        match update.saved() {
            Some(Ok(outcome)) => note_save_outcome(outcome),
            Some(Err(e)) => log::error!("Failed to save settings: {:#}", e),
            None => {}
        }
        update
    }

    /// Applies `change` to these settings and saves them to `path` with `save_to`, so a
    /// change never saves over a hand edit the settings poll hasn't reloaded yet.
    ///
    /// When the file changed since these settings were loaded from it or saved to it (its
    /// modification time or its contents differ from `file_version`), they are reloaded from
    /// it first, and the change goes into the file's settings: an edit made just before, or a
    /// fix to a file that was invalid at startup, is kept rather than replaced. An invalid
    /// file is never reloaded or written: the change then applies only in memory. Otherwise
    /// these settings end up as what was saved, change included, and they keep the change when
    /// writing fails, too. A change that changes nothing saves nothing.
    pub fn update_at(&mut self, path: &Path, change: impl FnOnce(&mut Self)) -> Update {
        let is_reloaded = match Self::load_from(path) {
            Ok(on_disk) if !self.file_version.is_current(on_disk.file_version) => {
                *self = on_disk;
                true
            }
            Ok(_) => false,
            Err(err) => {
                let is_changed = self.apply(change);
                return if is_changed {
                    Update::Changed(Ok(SaveOutcome::KeptInMemory(err)))
                } else {
                    Update::Unchanged
                };
            }
        };
        let saved = self.apply(change).then(|| {
            let saved = self.save_to(path);
            if let Ok(SaveOutcome::Saved { version }) = &saved {
                self.file_version = *version;
            }
            saved
        });
        match (is_reloaded, saved) {
            (true, saved) => Update::Reloaded(saved),
            (false, Some(saved)) => Update::Changed(saved),
            (false, None) => Update::Unchanged,
        }
    }

    /// Runs `change` on these settings; returns whether it changed what they save as.
    fn apply(&mut self, change: impl FnOnce(&mut Self)) -> bool {
        let before = serde_json::to_value(&*self).ok();
        change(self);
        // Settings that can't be serialized count as changed, so saving them says why.
        before.is_none() || serde_json::to_value(&*self).ok() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_shortcut() {
        let sc = ShortcutConfig::default();
        assert_eq!(sc.key, "M");
        assert!(sc.modifiers.contains(&"shift".to_string()));
        assert!(sc.modifiers.contains(&"meta".to_string()));
    }
    #[test]
    fn test_default_settings_show_popup() {
        assert!(Settings::default().show_popup);
    }

    #[test]
    fn test_settings_json_missing_show_popup_defaults_to_enabled() {
        let loaded: Settings = serde_json::from_str(
            r#"{
                "mic_shortcut": {
                    "key": "F13"
                }
            }"#,
        )
        .unwrap();

        assert!(loaded.show_popup);
    }

    #[test]
    fn test_settings_json_round_trip() {
        let s = Settings::default();

        let json = serde_json::to_string(&s).unwrap();
        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");
        assert!(loaded.show_popup);

        // A file saved while the default was ⌘⇧A keeps its shortcut.
        let saved_before: Settings = serde_json::from_str(
            r#"{ "mic_shortcut": { "modifiers": ["shift", "meta"], "key": "A" } }"#,
        )
        .unwrap();
        assert_eq!(saved_before.mic_shortcut.key, "A");
    }

    #[test]
    fn test_settings_json_missing_shortcut_modifiers() {
        let loaded: Settings = serde_json::from_str(
            r#"{
                "mic_shortcut": {
                    "key": "F13"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(loaded.mic_shortcut.key, "F13");
        assert!(loaded.mic_shortcut.modifiers.is_empty());
    }

    #[test]
    fn test_settings_save_and_load() {
        use std::fs;

        // Use a temp path for testing
        let tmp_dir = std::env::temp_dir().join("mic-mute-test-settings");
        let tmp_path = tmp_dir.join("settings.json");
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::create_dir_all(&tmp_dir);

        let s = Settings {
            mic_shortcut: ShortcutConfig {
                modifiers: vec!["shift".to_string()],
                key: "M".to_string(),
            },
            show_in_dock: false,
            launch_at_login: false,
            show_popup: false,
            excluded_devices: vec![ExcludedDevice {
                uid: "MSLoopbackDriverDevice_UID".to_string(),
                name: "Microsoft Teams Audio".to_string(),
            }],
            mode: ShortcutMode::PushToTalk,
            allow_url_unmute: true,
            launch_state: LaunchState::Last,
            mute_on_lock: true,
            mute_on_sleep: true,
            mute_on_display_sleep: true,
            play_sound: true,
            show_notification: true,
            popup_position: Lenient::new(PopupPosition::TopRight),
            popup_size: Lenient::new(PopupSize::Large),
            popup_mic_on_ms: Lenient::new(MicOnFlash::from_millis(0)),
            extra: serde_json::Map::new(),
            file_version: FileVersion::Unknown,
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");
        assert!(!loaded.show_popup);
        assert_eq!(loaded.excluded_devices, s.excluded_devices);
        assert_eq!(loaded.mode, ShortcutMode::PushToTalk);
        assert!(loaded.allow_url_unmute);
        assert_eq!(loaded.launch_state, LaunchState::Last);
        assert!(loaded.mute_on_lock && loaded.mute_on_sleep && loaded.mute_on_display_sleep);
        assert!(loaded.play_sound && loaded.show_notification);
        assert_eq!(loaded.popup_position, s.popup_position);
        assert_eq!(loaded.popup_size, s.popup_size);
        assert_eq!(loaded.popup_mic_on_ms, s.popup_mic_on_ms);

        let _ = fs::remove_file(&tmp_path);
    }

    /// A unique empty directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "mic-mute-settings-test-{}-{}",
                std::process::id(),
                id
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn settings_path(&self) -> PathBuf {
            self.0.join("settings.json")
        }

        fn tmp_path(&self) -> PathBuf {
            self.0.join("settings.json.tmp")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const FILE_WITH_UNKNOWN_FIELDS: &str = r#"{
        "mic_shortcut": { "modifiers": ["meta"], "key": "F13" },
        "show_popup": false,
        "excluded_devices": ["Teams Audio UID"],
        "skipped_devices": ["Teams Audio"],
        "future": { "nested": [1, 2.5, null, "text"], "flag": true }
    }"#;

    #[test]
    fn unknown_fields_round_trip_through_serde() {
        let loaded: Settings = serde_json::from_str(FILE_WITH_UNKNOWN_FIELDS).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "F13");
        assert!(!loaded.show_popup);
        assert_eq!(loaded.extra.len(), 2);

        let saved: serde_json::Value = serde_json::to_value(&loaded).unwrap();
        let original: serde_json::Value = serde_json::from_str(FILE_WITH_UNKNOWN_FIELDS).unwrap();
        assert_eq!(saved["skipped_devices"], original["skipped_devices"]);
        assert_eq!(saved["future"], original["future"]);
        assert_eq!(saved["show_popup"], serde_json::json!(false));
        assert_eq!(saved["excluded_devices"], original["excluded_devices"]);
    }

    #[test]
    fn known_fields_never_land_in_extra() {
        let loaded: Settings = serde_json::from_str(FILE_WITH_UNKNOWN_FIELDS).unwrap();
        for known in [
            "mic_shortcut",
            "show_in_dock",
            "launch_at_login",
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
        ] {
            assert!(
                !loaded.extra.contains_key(known),
                "{known} leaked into extra"
            );
        }
    }

    #[test]
    fn empty_object_gets_defaults_and_no_extra_fields() {
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        let defaults = Settings::default();
        assert_eq!(loaded.mic_shortcut.key, defaults.mic_shortcut.key);
        assert_eq!(
            loaded.mic_shortcut.modifiers,
            defaults.mic_shortcut.modifiers
        );
        assert_eq!(loaded.show_in_dock, defaults.show_in_dock);
        assert_eq!(loaded.launch_at_login, defaults.launch_at_login);
        assert_eq!(loaded.show_popup, defaults.show_popup);
        assert_eq!(loaded.excluded_devices, defaults.excluded_devices);
        assert_eq!(loaded.play_sound, defaults.play_sound);
        assert_eq!(loaded.show_notification, defaults.show_notification);
        assert_eq!(loaded.popup_position, defaults.popup_position);
        assert_eq!(loaded.popup_size, defaults.popup_size);
        assert_eq!(loaded.popup_mic_on_ms, defaults.popup_mic_on_ms);
        assert!(loaded.extra.is_empty());
    }

    #[test]
    fn default_settings_serialize_without_extra_keys() {
        let value = serde_json::to_value(Settings::default()).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "allow_url_unmute",
                "excluded_devices",
                "launch_at_login",
                "launch_state",
                "mic_shortcut",
                "mode",
                "mute_on_display_sleep",
                "mute_on_lock",
                "mute_on_sleep",
                "play_sound",
                "popup_mic_on_ms",
                "popup_position",
                "popup_size",
                "show_in_dock",
                "show_notification",
                "show_popup"
            ]
        );
    }

    #[test]
    fn save_to_writes_pretty_json_and_leaves_no_temp_file() {
        let dir = TempDir::new();
        let path = dir.settings_path();
        let settings = Settings {
            show_in_dock: true,
            ..Settings::default()
        };

        settings.save_to(&path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.ends_with("}\n"),
            "missing trailing newline: {written:?}"
        );
        let reloaded: Settings = serde_json::from_str(&written).unwrap();
        assert!(reloaded.show_in_dock);
        assert!(!dir.tmp_path().exists());
    }

    #[test]
    fn save_to_keeps_unknown_fields() {
        let dir = TempDir::new();
        let path = dir.settings_path();
        let mut settings: Settings = serde_json::from_str(FILE_WITH_UNKNOWN_FIELDS).unwrap();
        settings.show_in_dock = true;

        settings.save_to(&path).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["skipped_devices"], serde_json::json!(["Teams Audio"]));
        assert_eq!(saved["future"]["nested"][1], serde_json::json!(2.5));
        assert_eq!(saved["show_in_dock"], serde_json::json!(true));
    }

    #[test]
    fn save_to_creates_missing_directories() {
        let dir = TempDir::new();
        let path = dir.0.join("nested").join("mic-mute").join("settings.json");

        Settings::default().save_to(&path).unwrap();

        assert!(path.is_file());
    }

    #[test]
    fn save_to_replaces_an_existing_file() {
        let dir = TempDir::new();
        let path = dir.settings_path();
        std::fs::write(&path, r#"{ "show_popup": true }"#).unwrap();
        let settings = Settings {
            show_popup: false,
            ..Settings::default()
        };

        settings.save_to(&path).unwrap();

        let saved: Settings =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!saved.show_popup);
        assert!(!dir.tmp_path().exists());
    }

    #[test]
    fn save_to_through_a_symlink_keeps_the_link() {
        let dir = TempDir::new();
        let target = dir.0.join("dotfiles-settings.json");
        std::fs::write(&target, "{}").unwrap();
        let link = dir.settings_path();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let settings = Settings {
            show_in_dock: true,
            ..Settings::default()
        };

        settings.save_to(&link).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let saved: Settings =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert!(saved.show_in_dock);
        assert!(!dir.0.join("dotfiles-settings.json.tmp").exists());
    }

    /// Line 3 lacks its trailing comma; serde_json reports the next key, at line 4, column 3.
    const MISSING_COMMA: &str =
        "{\n  \"show_in_dock\": false,\n  \"show_popup\": true\n  \"launch_at_login\": true\n}\n";

    fn write_file(dir: &TempDir, contents: &str) -> PathBuf {
        let path = dir.settings_path();
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn load_from_missing_file_gives_defaults() {
        let dir = TempDir::new();

        let loaded = Settings::load_from(&dir.settings_path()).unwrap();

        assert!(loaded.show_popup);
        assert!(!loaded.launch_at_login);
        assert!(loaded.extra.is_empty());
        assert!(
            !dir.settings_path().exists(),
            "loading must not create the file"
        );
    }

    #[test]
    fn load_from_invalid_json_reports_path_line_and_column() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
        assert_eq!(err.position(), Some((4, 3)));
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("line 4, column 3"), "{message}");
        assert_eq!(
            message.matches("line 4").count(),
            1,
            "position repeated: {message}"
        );
    }

    #[test]
    fn load_from_wrong_value_type_reports_its_position() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{\n  \"show_popup\": \"yes\"\n}\n");

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
        assert_eq!(err.position().map(|(line, _)| line), Some(2));
        assert!(err.to_string().contains("invalid type"), "{err}");
    }

    #[test]
    fn load_from_empty_file_is_an_error() {
        let dir = TempDir::new();
        let path = write_file(&dir, "");

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
    }

    #[test]
    fn load_from_unreadable_path_is_a_read_error() {
        let dir = TempDir::new();
        let path = dir.settings_path();
        std::fs::create_dir(&path).unwrap();

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Read { .. }));
        assert_eq!(err.position(), None);
        assert_eq!(
            err.notice(Fallback::Defaults),
            "settings.json can't be read — using defaults"
        );
    }

    #[test]
    fn notice_names_the_file_position_and_fallback() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);
        let err = Settings::load_from(&path).unwrap_err();

        assert_eq!(
            err.notice(Fallback::Defaults),
            "settings.json is invalid (line 4, column 3) — using defaults"
        );
        assert_eq!(
            err.notice(Fallback::Previous),
            "settings.json is invalid (line 4, column 3) — keeping previous settings"
        );
    }

    #[test]
    fn serde_message_drops_only_the_position_suffix() {
        let err = serde_json::from_str::<Settings>("{\"show_popup\": tru}").unwrap_err();

        let message = serde_message(&err);

        assert!(!message.is_empty());
        assert!(!message.contains(" at line "), "{message}");
        assert!(err.to_string().starts_with(&message));
    }

    #[test]
    fn save_to_never_replaces_an_invalid_file() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);
        let settings = Settings {
            show_in_dock: true,
            ..Settings::default()
        };

        let outcome = settings.save_to(&path).unwrap();

        assert!(matches!(
            outcome,
            SaveOutcome::KeptInMemory(LoadError::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MISSING_COMMA);
        assert!(!dir.tmp_path().exists());
    }

    #[test]
    fn save_to_never_replaces_an_empty_file() {
        let dir = TempDir::new();
        let path = write_file(&dir, "");

        let outcome = Settings::default().save_to(&path).unwrap();

        assert!(matches!(outcome, SaveOutcome::KeptInMemory(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn save_to_writes_again_once_the_file_is_fixed() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);
        let settings = Settings {
            show_in_dock: true,
            ..Settings::default()
        };
        assert!(matches!(
            settings.save_to(&path).unwrap(),
            SaveOutcome::KeptInMemory(_)
        ));

        std::fs::write(&path, "{ \"show_popup\": false }").unwrap();
        let outcome = settings.save_to(&path).unwrap();

        assert!(matches!(outcome, SaveOutcome::Saved { .. }));
        assert!(Settings::load_from(&path).unwrap().show_in_dock);
    }

    #[test]
    fn save_to_reports_the_written_files_mtime() {
        let dir = TempDir::new();
        let path = dir.settings_path();

        let outcome = Settings::default().save_to(&path).unwrap();

        let SaveOutcome::Saved { version } = outcome else {
            panic!("expected Saved, got {outcome:?}");
        };
        assert!(version.mtime().is_some());
        assert_eq!(version.mtime(), file_mtime(&path));
        let loaded = Settings::load_from(&path).unwrap();
        assert!(version.is_current(loaded.file_version));
    }

    #[test]
    fn mtimes_match_only_when_both_are_known_and_equal() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let later = t + std::time::Duration::from_nanos(1);

        assert!(is_same_mtime(Some(t), Some(t)));
        assert!(!is_same_mtime(Some(t), Some(later)));
        assert!(!is_same_mtime(Some(t), None));
        assert!(!is_same_mtime(None, Some(t)));
        assert!(!is_same_mtime(None, None));
    }

    // The only test that touches OWN_SAVE_MTIME; save() itself never runs in tests.
    #[test]
    fn own_save_matches_only_the_first_change_after_it() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        *OWN_SAVE_MTIME.lock() = Some(t);

        assert!(Settings::take_own_save(Some(t)));
        assert!(!Settings::take_own_save(Some(t)), "a match is forgotten");

        *OWN_SAVE_MTIME.lock() = Some(t);
        assert!(!Settings::take_own_save(Some(
            t + std::time::Duration::from_secs(1)
        )));
        assert!(
            !Settings::take_own_save(Some(t)),
            "a mismatch is forgotten too"
        );
    }

    // update_at: a change saved before the settings poll reloads a hand edit.

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_change_after_a_hand_edit_keeps_the_edit_and_the_change() {
        let dir = TempDir::new();
        let path = write_file(&dir, r#"{ "show_popup": true }"#);
        let mut settings = Settings::load_from(&path).unwrap();
        std::fs::write(
            &path,
            r#"{ "show_popup": false, "mode": "push_to_talk", "future": [1] }"#,
        )
        .unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(
            matches!(
                update,
                Update::Reloaded(Some(Ok(SaveOutcome::Saved { .. })))
            ),
            "{update:?}"
        );
        let saved = read_json(&path);
        assert_eq!(saved["show_popup"], serde_json::json!(false));
        assert_eq!(saved["mode"], serde_json::json!("push_to_talk"));
        assert_eq!(saved["future"], serde_json::json!([1]));
        assert_eq!(saved["show_in_dock"], serde_json::json!(true));
        // The settings in memory are what was saved.
        assert_eq!(serde_json::to_value(&settings).unwrap(), saved);
        assert_eq!(settings.mode, ShortcutMode::PushToTalk);
    }

    #[test]
    fn a_change_leaves_an_invalid_file_untouched() {
        let dir = TempDir::new();
        let path = write_file(&dir, r#"{ "show_popup": true }"#);
        let mut settings = Settings::load_from(&path).unwrap();
        std::fs::write(&path, MISSING_COMMA).unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(
            matches!(
                update,
                Update::Changed(Ok(SaveOutcome::KeptInMemory(LoadError::Parse { .. })))
            ),
            "{update:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MISSING_COMMA);
        assert!(!dir.tmp_path().exists());
        assert!(settings.show_in_dock, "the change applies in memory");
        assert!(settings.show_popup);
    }

    #[test]
    fn a_change_to_the_defaults_leaves_a_file_invalid_since_startup_untouched() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);
        assert!(Settings::load_from(&path).is_err());
        let mut settings = Settings::default();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(
            matches!(update, Update::Changed(Ok(SaveOutcome::KeptInMemory(_)))),
            "{update:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MISSING_COMMA);
        assert!(settings.show_in_dock);
    }

    #[test]
    fn a_change_after_fixing_a_file_invalid_at_startup_goes_into_the_fixed_file() {
        let dir = TempDir::new();
        let path = write_file(&dir, MISSING_COMMA);
        // main() runs on the defaults while the file is invalid; this change isn't saved.
        let mut settings = Settings::default();
        settings.update_at(&path, |s| s.play_sound = true);
        std::fs::write(
            &path,
            r#"{
                "mic_shortcut": { "modifiers": ["ctrl"], "key": "F13" },
                "show_popup": false,
                "launch_at_login": true
            }"#,
        )
        .unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(
            matches!(
                update,
                Update::Reloaded(Some(Ok(SaveOutcome::Saved { .. })))
            ),
            "{update:?}"
        );
        let saved = Settings::load_from(&path).unwrap();
        assert_eq!(saved.mic_shortcut.key, "F13");
        assert!(!saved.show_popup && saved.launch_at_login && saved.show_in_dock);
        assert!(
            !saved.play_sound,
            "the fixed file replaces changes it never saved"
        );
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
    }

    #[test]
    fn a_change_to_an_unchanged_file_saves_without_reloading_it() {
        let dir = TempDir::new();
        let path = write_file(&dir, r#"{ "show_popup": true }"#);
        let mut settings = Settings::load_from(&path).unwrap();
        // Only in memory, so a reload would drop it.
        settings.play_sound = true;

        let first = settings.update_at(&path, |s| s.show_in_dock = true);
        // The app's own save is the version these settings know now.
        let second = settings.update_at(&path, |s| s.show_notification = true);

        assert!(
            matches!(first, Update::Changed(Ok(SaveOutcome::Saved { .. }))),
            "{first:?}"
        );
        assert!(
            matches!(second, Update::Changed(Ok(SaveOutcome::Saved { .. }))),
            "{second:?}"
        );
        let saved = Settings::load_from(&path).unwrap();
        assert!(saved.play_sound && saved.show_in_dock && saved.show_notification);
    }

    #[test]
    fn a_change_creates_a_file_missing_at_load_without_reloading() {
        let dir = TempDir::new();
        let path = dir.settings_path();
        let mut settings = Settings::load_from(&path).unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(
            matches!(update, Update::Changed(Ok(SaveOutcome::Saved { .. }))),
            "{update:?}"
        );
        assert!(Settings::load_from(&path).unwrap().show_in_dock);
    }

    #[test]
    fn a_change_that_changes_nothing_writes_nothing() {
        let dir = TempDir::new();
        let path = write_file(&dir, r#"{"show_in_dock":true}"#);
        let mut settings = Settings::load_from(&path).unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(matches!(update, Update::Unchanged), "{update:?}");
        assert!(!update.is_changed());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"show_in_dock":true}"#
        );
    }

    #[test]
    fn a_hand_edit_that_already_has_the_change_is_adopted_without_a_write() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{}");
        let mut settings = Settings::load_from(&path).unwrap();
        let edit = r#"{"show_in_dock":true,"show_popup":false}"#;
        std::fs::write(&path, edit).unwrap();

        let update = settings.update_at(&path, |s| s.show_in_dock = true);

        assert!(matches!(update, Update::Reloaded(None)), "{update:?}");
        assert!(update.is_changed());
        assert!(settings.show_in_dock && !settings.show_popup);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edit);
        // The edit is the version these settings know now.
        let next = settings.update_at(&path, |s| s.play_sound = true);
        assert!(matches!(next, Update::Changed(_)), "{next:?}");
    }

    #[test]
    fn excluding_a_device_a_hand_edit_already_excluded_keeps_it_excluded() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{}");
        let mut settings = Settings::load_from(&path).unwrap();
        std::fs::write(
            &path,
            r#"{ "excluded_devices": ["MSLoopbackDriverDevice_UID"] }"#,
        )
        .unwrap();
        // The menu showed the device as included, so the click excludes it.
        let is_excluded = !settings.is_device_excluded("MSLoopbackDriverDevice_UID");

        let update = settings.update_at(&path, |s| {
            s.set_device_excluded(&teams_audio(), is_excluded)
        });

        assert!(matches!(update, Update::Reloaded(None)), "{update:?}");
        assert!(settings.is_device_excluded("MSLoopbackDriverDevice_UID"));
    }

    #[test]
    fn a_file_version_is_current_only_with_the_same_contents_and_mtime() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let version = FileVersion::of(b"{}", Some(t));

        assert!(version.is_current(FileVersion::of(b"{}", Some(t))));
        assert!(!version.is_current(FileVersion::of(b"{ }", Some(t))));
        assert!(!version.is_current(FileVersion::of(b"{}", Some(t + Duration::from_nanos(1)))));
        assert!(!version.is_current(FileVersion::of(b"{}", None)));
        assert!(!FileVersion::of(b"{}", None).is_current(FileVersion::of(b"{}", None)));
        assert!(!version.is_current(FileVersion::Missing));
        assert!(FileVersion::Missing.is_current(FileVersion::Missing));
        for now in [FileVersion::Unknown, FileVersion::Missing, version] {
            assert!(!FileVersion::Unknown.is_current(now), "{now:?}");
        }
    }

    // excluded_devices

    fn teams_audio() -> ExcludedDevice {
        ExcludedDevice {
            uid: "MSLoopbackDriverDevice_UID".to_string(),
            name: "Microsoft Teams Audio".to_string(),
        }
    }

    #[test]
    fn default_settings_exclude_no_devices() {
        assert!(Settings::default().excluded_devices.is_empty());
        assert!(Settings::default().excluded_uids().is_empty());
    }

    #[test]
    fn settings_json_missing_excluded_devices_excludes_none() {
        let loaded: Settings =
            serde_json::from_str(r#"{ "mic_shortcut": { "key": "F13" }, "show_popup": false }"#)
                .unwrap();

        assert!(loaded.excluded_devices.is_empty());
        assert!(loaded.extra.is_empty());
    }

    #[test]
    fn excluded_devices_round_trip_with_their_uid_and_name() {
        let settings = Settings {
            excluded_devices: vec![teams_audio()],
            ..Settings::default()
        };

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            value["excluded_devices"],
            serde_json::json!([
                { "uid": "MSLoopbackDriverDevice_UID", "name": "Microsoft Teams Audio" }
            ])
        );
        let loaded: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.excluded_devices, vec![teams_audio()]);
    }

    #[test]
    fn excluded_devices_accept_bare_uids_and_save_them_bare() {
        // Upstream's excluded_devices lists bare UIDs; an entry may also omit its name.
        let loaded: Settings = serde_json::from_str(
            r#"{ "excluded_devices": ["Steam UID", { "uid": "Teams UID" }] }"#,
        )
        .unwrap();

        let uids: Vec<_> = loaded
            .excluded_devices
            .iter()
            .map(|device| (device.uid.as_str(), device.name.as_str()))
            .collect();
        assert_eq!(uids, [("Steam UID", ""), ("Teams UID", "")]);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap()["excluded_devices"],
            serde_json::json!(["Steam UID", "Teams UID"])
        );
    }

    #[test]
    fn excluded_uids_leave_out_empty_uids() {
        let settings: Settings =
            serde_json::from_str(r#"{ "excluded_devices": ["", "Teams UID", "Teams UID"] }"#)
                .unwrap();

        assert_eq!(
            settings.excluded_uids().into_iter().collect::<Vec<_>>(),
            ["Teams UID"]
        );
    }

    #[test]
    fn a_device_is_excluded_then_included_again() {
        let mut settings = Settings::default();

        settings.set_device_excluded(&teams_audio(), true);
        assert_eq!(settings.excluded_devices, vec![teams_audio()]);
        assert!(settings.is_device_excluded("MSLoopbackDriverDevice_UID"));

        settings.set_device_excluded(&teams_audio(), false);
        assert!(settings.excluded_devices.is_empty());
        assert!(!settings.is_device_excluded("MSLoopbackDriverDevice_UID"));
    }

    #[test]
    fn excluding_an_excluded_device_or_including_an_included_one_changes_nothing() {
        let mut settings: Settings =
            serde_json::from_str(r#"{ "excluded_devices": ["MSLoopbackDriverDevice_UID"] }"#)
                .unwrap();
        let before = settings.excluded_devices.clone();

        settings.set_device_excluded(&teams_audio(), true);
        assert_eq!(
            settings.excluded_devices, before,
            "keeps the existing entry"
        );

        let steam = ExcludedDevice {
            uid: "Steam UID".to_string(),
            name: "Steam Streaming Microphone".to_string(),
        };
        settings.set_device_excluded(&steam, false);
        assert_eq!(settings.excluded_devices, before);
    }

    #[test]
    fn including_a_device_removes_every_entry_with_its_uid() {
        let mut settings: Settings = serde_json::from_str(
            r#"{ "excluded_devices": ["Teams UID", "Steam UID", { "uid": "Teams UID", "name": "Teams" }] }"#,
        )
        .unwrap();
        let teams = ExcludedDevice {
            uid: "Teams UID".to_string(),
            name: "Teams".to_string(),
        };

        settings.set_device_excluded(&teams, false);

        assert_eq!(
            settings.excluded_uids().into_iter().collect::<Vec<_>>(),
            ["Steam UID"]
        );
    }

    #[test]
    fn an_excluded_device_entry_of_the_wrong_type_makes_the_file_invalid() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{\n  \"excluded_devices\": [42]\n}\n");

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
        assert_eq!(err.position().map(|(line, _)| line), Some(2));
    }

    // Inputs besides the tray menu: URLs, the shortcut's mode, and automatic mute.

    #[test]
    fn url_unmute_is_off_by_default_and_when_missing() {
        assert!(!Settings::default().allow_url_unmute);
        let loaded: Settings = serde_json::from_str(r#"{ "show_popup": false }"#).unwrap();
        assert!(!loaded.allow_url_unmute);
    }

    #[test]
    fn url_unmute_round_trips() {
        let settings = Settings {
            allow_url_unmute: true,
            ..Settings::default()
        };
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["allow_url_unmute"], serde_json::json!(true));
        let loaded: Settings = serde_json::from_value(json).unwrap();
        assert!(loaded.allow_url_unmute);
    }

    #[test]
    fn mode_is_toggle_by_default_and_when_missing() {
        assert_eq!(Settings::default().mode, ShortcutMode::Toggle);
        let loaded: Settings = serde_json::from_str(r#"{ "show_popup": false }"#).unwrap();
        assert_eq!(loaded.mode, ShortcutMode::Toggle);
    }

    #[test]
    fn mode_round_trips_in_snake_case() {
        for (mode, text) in [
            (ShortcutMode::Toggle, "toggle"),
            (ShortcutMode::PushToTalk, "push_to_talk"),
        ] {
            let settings = Settings {
                mode,
                ..Settings::default()
            };
            let json = serde_json::to_value(&settings).unwrap();
            assert_eq!(json["mode"], serde_json::json!(text));
            let loaded: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(loaded.mode, mode);
        }
    }

    #[test]
    fn an_unknown_mode_makes_the_file_invalid() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{\n  \"mode\": \"hold\"\n}\n");

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
        assert_eq!(err.position().map(|(line, _)| line), Some(2));
    }

    #[test]
    fn automatic_mute_is_off_by_default_and_when_missing() {
        let loaded: Settings = serde_json::from_str(r#"{ "show_popup": false }"#).unwrap();
        for settings in [Settings::default(), loaded] {
            assert_eq!(settings.launch_state, LaunchState::Keep);
            assert!(!settings.mute_on_lock);
            assert!(!settings.mute_on_sleep);
            assert!(!settings.mute_on_display_sleep);
        }
    }

    #[test]
    fn automatic_mute_settings_round_trip() {
        for (launch_state, text) in [
            (LaunchState::Keep, "keep"),
            (LaunchState::Muted, "muted"),
            (LaunchState::Last, "last"),
        ] {
            let settings = Settings {
                launch_state,
                mute_on_lock: true,
                mute_on_sleep: false,
                mute_on_display_sleep: true,
                ..Settings::default()
            };
            let json = serde_json::to_value(&settings).unwrap();
            assert_eq!(json["launch_state"], serde_json::json!(text));
            let loaded: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(loaded.launch_state, launch_state);
            assert!(loaded.mute_on_lock);
            assert!(!loaded.mute_on_sleep);
            assert!(loaded.mute_on_display_sleep);
        }
    }

    #[test]
    fn an_unknown_launch_state_makes_the_file_invalid() {
        let dir = TempDir::new();
        let path = write_file(&dir, "{\n  \"launch_state\": \"unmuted\"\n}\n");

        let err = Settings::load_from(&path).unwrap_err();

        assert!(matches!(err, LoadError::Parse { .. }));
        assert_eq!(err.position().map(|(line, _)| line), Some(2));
    }

    // Feedback on a mute change: sound, notification, and the popup's position, size, and
    // "Mic on" flash.

    #[test]
    fn feedback_defaults_keep_todays_behavior() {
        let defaults = Settings::default();
        assert!(!defaults.play_sound);
        assert!(!defaults.show_notification);
        assert_eq!(defaults.popup_position.value(), PopupPosition::BottomCenter);
        assert_eq!(defaults.popup_size.value(), PopupSize::Medium);
        assert_eq!(
            defaults.popup_mic_on_ms.value().duration(),
            Duration::from_secs(1)
        );
        assert!(defaults.feedback_problems().is_empty());
    }

    #[test]
    fn settings_json_missing_feedback_fields_gets_the_defaults() {
        let loaded: Settings =
            serde_json::from_str(r#"{ "mic_shortcut": { "key": "F13" }, "show_popup": false }"#)
                .unwrap();
        let defaults = Settings::default();

        assert_eq!(loaded.play_sound, defaults.play_sound);
        assert_eq!(loaded.show_notification, defaults.show_notification);
        assert_eq!(loaded.popup_position, defaults.popup_position);
        assert_eq!(loaded.popup_size, defaults.popup_size);
        assert_eq!(loaded.popup_mic_on_ms, defaults.popup_mic_on_ms);
        assert!(loaded.feedback_problems().is_empty());
        assert!(loaded.extra.is_empty());
    }

    #[test]
    fn feedback_settings_round_trip() {
        for (position, size) in PopupPosition::ALL
            .into_iter()
            .zip(PopupSize::ALL.into_iter().cycle())
        {
            let settings = Settings {
                play_sound: true,
                show_notification: true,
                popup_position: Lenient::new(position),
                popup_size: Lenient::new(size),
                popup_mic_on_ms: Lenient::new(MicOnFlash::from_millis(2_500)),
                ..Settings::default()
            };

            let json = serde_json::to_value(&settings).unwrap();
            assert_eq!(json["popup_position"], serde_json::json!(position.name()));
            assert_eq!(json["popup_size"], serde_json::json!(size.name()));
            assert_eq!(json["popup_mic_on_ms"], serde_json::json!(2_500));
            assert_eq!(json["play_sound"], serde_json::json!(true));
            assert_eq!(json["show_notification"], serde_json::json!(true));

            let loaded: Settings = serde_json::from_value(json).unwrap();
            assert!(loaded.play_sound && loaded.show_notification);
            assert_eq!(loaded.popup_position.value(), position);
            assert_eq!(loaded.popup_size.value(), size);
            assert_eq!(loaded.popup_mic_on_ms, settings.popup_mic_on_ms);
            assert!(
                loaded.feedback_problems().is_empty(),
                "{position:?} {size:?}"
            );
        }
    }

    #[test]
    fn popup_names_are_read_in_any_letter_case_and_saved_in_lowercase() {
        let loaded: Settings =
            serde_json::from_str(r#"{ "popup_position": "Top_Right", "popup_size": "LARGE" }"#)
                .unwrap();

        assert_eq!(loaded.popup_position.value(), PopupPosition::TopRight);
        assert_eq!(loaded.popup_size.value(), PopupSize::Large);
        assert!(loaded.feedback_problems().is_empty());
        let saved = serde_json::to_value(&loaded).unwrap();
        assert_eq!(saved["popup_position"], serde_json::json!("top_right"));
        assert_eq!(saved["popup_size"], serde_json::json!("large"));
    }

    const FILE_WITH_UNUSABLE_FEEDBACK_VALUES: &str = r#"{
        "show_popup": false,
        "popup_position": "top-right",
        "popup_size": 2,
        "popup_mic_on_ms": "3s"
    }"#;

    #[test]
    fn unusable_feedback_values_fall_back_without_invalidating_the_file() {
        let dir = TempDir::new();
        let path = write_file(&dir, FILE_WITH_UNUSABLE_FEEDBACK_VALUES);

        let loaded = Settings::load_from(&path).unwrap();

        assert!(!loaded.show_popup, "the file's other settings still apply");
        assert_eq!(loaded.popup_position.value(), PopupPosition::BottomCenter);
        assert_eq!(loaded.popup_size.value(), PopupSize::Medium);
        assert_eq!(loaded.popup_mic_on_ms.value(), MicOnFlash::default());
        assert_eq!(
            loaded.feedback_problems(),
            [
                "popup_position \"top-right\" isn't one of \"top_left\", \"top_center\", \
                 \"top_right\", \"bottom_left\", \"bottom_center\", \"bottom_right\"; using \
                 \"bottom_center\"",
                "popup_size 2 isn't one of \"small\", \"medium\", \"large\"; using \"medium\"",
                "popup_mic_on_ms \"3s\" isn't a whole number of milliseconds from 0 to 10000; \
                 using 1000",
            ]
        );
    }

    #[test]
    fn unusable_feedback_values_are_saved_back_unchanged() {
        let dir = TempDir::new();
        let path = write_file(&dir, FILE_WITH_UNUSABLE_FEEDBACK_VALUES);
        let mut settings = Settings::load_from(&path).unwrap();
        settings.show_in_dock = true;

        settings.save_to(&path).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["popup_position"], serde_json::json!("top-right"));
        assert_eq!(saved["popup_size"], serde_json::json!(2));
        assert_eq!(saved["popup_mic_on_ms"], serde_json::json!("3s"));
        assert_eq!(saved["show_in_dock"], serde_json::json!(true));
    }

    #[test]
    fn mic_on_flash_is_capped_at_ten_seconds() {
        let cases = [
            (0, 0),
            (1, 1),
            (3_000, 3_000),
            (10_000, 10_000),
            (60_000, 10_000),
        ];
        for (ms, expected) in cases {
            assert_eq!(
                MicOnFlash::from_millis(ms).duration(),
                Duration::from_millis(expected),
                "{ms} ms"
            );
        }
        assert_eq!(
            MicOnFlash::from_millis(u64::MAX).duration(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn a_longer_mic_on_flash_is_capped_with_a_warning_and_kept_in_the_file() {
        let loaded: Settings = serde_json::from_str(r#"{ "popup_mic_on_ms": 60000 }"#).unwrap();

        assert_eq!(
            loaded.popup_mic_on_ms.value().duration(),
            Duration::from_secs(10)
        );
        assert_eq!(
            loaded.feedback_problems(),
            ["popup_mic_on_ms 60000 is above the maximum, 10000; using 10000"]
        );
        let saved = serde_json::to_value(&loaded).unwrap();
        assert_eq!(saved["popup_mic_on_ms"], serde_json::json!(60_000));
    }

    #[test]
    fn mic_on_flash_accepts_only_whole_milliseconds() {
        for accepted in ["0", "1", "2500", "10000"] {
            let json: serde_json::Value = serde_json::from_str(accepted).unwrap();
            assert!(MicOnFlash::parse(&json).is_ok(), "{accepted}");
        }
        for rejected in ["-1", "1.5", "1000.0", "\"1000\"", "null", "true", "[1000]"] {
            let json: serde_json::Value = serde_json::from_str(rejected).unwrap();
            let (fallback, _) = MicOnFlash::parse(&json).unwrap_err();
            assert_eq!(fallback, MicOnFlash::default(), "{rejected}");
        }
    }
}
