use crate::utils::write_atomically;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    // Inputs besides the tray menu: the shortcut's mode (`push_to_talk`) and URLs
    // (`url_scheme`).
    /// What the mute shortcut does: toggle, or unmute only while held.
    #[serde(default)]
    pub mode: ShortcutMode,
    /// Whether `unmute` and `toggle` URLs may run. Off by default: any app or web page can open
    /// a URL, so by default URLs can only mute and show the state.
    #[serde(default)]
    pub allow_url_unmute: bool,
    /// Fields this version doesn't know (written by a newer version or a fork), saved back
    /// unchanged. Empty when the file has none, as in `impl Default`.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_shortcut: ShortcutConfig::default(),
            show_in_dock: false,
            launch_at_login: false,
            show_popup: true,
            // Inputs besides the tray menu.
            mode: ShortcutMode::Toggle,
            allow_url_unmute: false,
            extra: serde_json::Map::new(),
        }
    }
}

fn default_show_popup() -> bool {
    true
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

/// What `Settings::save` did.
#[derive(Debug)]
pub enum SaveOutcome {
    /// The file now holds these settings; `mtime` is its new modification time.
    Saved { mtime: Option<SystemTime> },
    /// The file on disk is invalid, so it was left untouched. The change applies only in
    /// memory, until the file is fixed or the app quits.
    KeptInMemory(LoadError),
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
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(LoadError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        serde_json::from_slice(&data).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })
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

    /// Saves to settings.json (see `save_to`) and remembers the file's new mtime so the
    /// settings poll doesn't reload the app's own write.
    pub fn save(&self) -> Result<SaveOutcome> {
        let path = Self::config_path().context("Failed to resolve the settings directory")?;
        let outcome = self.save_to(&path)?;
        match &outcome {
            SaveOutcome::Saved { mtime } => {
                *OWN_SAVE_MTIME.lock() = *mtime;
            }
            SaveOutcome::KeptInMemory(err) => {
                log::warn!("Keeping the settings change in memory only: {}", err);
            }
        }
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
            mtime: file_mtime(path),
        })
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
            mode: ShortcutMode::PushToTalk,
            allow_url_unmute: true,
            extra: serde_json::Map::new(),
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");
        assert!(!loaded.show_popup);
        assert_eq!(loaded.mode, ShortcutMode::PushToTalk);
        assert!(loaded.allow_url_unmute);

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
        "excluded_devices": ["Teams Audio"],
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
        assert_eq!(saved["excluded_devices"], original["excluded_devices"]);
        assert_eq!(saved["future"], original["future"]);
        assert_eq!(saved["show_popup"], serde_json::json!(false));
    }

    #[test]
    fn known_fields_never_land_in_extra() {
        let loaded: Settings = serde_json::from_str(FILE_WITH_UNKNOWN_FIELDS).unwrap();
        for known in [
            "mic_shortcut",
            "show_in_dock",
            "launch_at_login",
            "show_popup",
            "mode",
            "allow_url_unmute",
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
                "launch_at_login",
                "mic_shortcut",
                "mode",
                "show_in_dock",
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
        assert_eq!(
            saved["excluded_devices"],
            serde_json::json!(["Teams Audio"])
        );
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

        let SaveOutcome::Saved { mtime } = outcome else {
            panic!("expected Saved, got {outcome:?}");
        };
        assert!(mtime.is_some());
        assert_eq!(mtime, file_mtime(&path));
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
}
