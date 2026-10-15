use crate::utils::write_atomically;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutConfig {
    #[serde(default)]
    pub modifiers: Vec<String>, // ["shift", "meta", "ctrl", "alt"]
    pub key: String, // "A", "M", "F13", etc.
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            modifiers: vec!["shift".to_string(), "meta".to_string()],
            key: "A".to_string(),
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
            extra: serde_json::Map::new(),
        }
    }
}

fn default_show_popup() -> bool {
    true
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from_file().unwrap_or_default()
    }

    fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json"))
    }

    fn load_from_file() -> Option<Self> {
        let path = Self::config_path()?;
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Returns the last-modified time of the settings file, or None if it doesn't exist.
    pub fn mtime() -> Option<std::time::SystemTime> {
        Self::config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path().context("Failed to resolve the settings directory")?;
        self.save_to(&path)
    }

    /// Writes these settings, unknown fields included, to `path` atomically: `<name>.tmp`
    /// first, then a rename, so a crash never leaves a half-written file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut data = serde_json::to_vec_pretty(self).context("Failed to serialize settings")?;
        data.push(b'\n');
        write_atomically(path, &data).with_context(|| format!("Failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_shortcut() {
        let sc = ShortcutConfig::default();
        assert_eq!(sc.key, "A");
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
        assert_eq!(loaded.mic_shortcut.key, "A");
        assert!(loaded.show_popup);
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
            extra: serde_json::Map::new(),
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");
        assert!(!loaded.show_popup);

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
                "launch_at_login",
                "mic_shortcut",
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
}
