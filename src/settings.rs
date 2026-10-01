use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// Input device names (case-insensitive) that Mic Mute never mutes or checks.
    #[serde(default)]
    pub excluded_devices: Vec<String>,
    /// Leave devices that accept a mute but don't apply it out of the mute status.
    #[serde(default = "default_skip_unresponsive_devices")]
    pub skip_unresponsive_devices: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_shortcut: ShortcutConfig::default(),
            show_in_dock: false,
            launch_at_login: false,
            show_popup: true,
            excluded_devices: Vec::new(),
            skip_unresponsive_devices: true,
        }
    }
}

/// Key used to compare device names: trimmed and case-insensitive.
pub fn device_name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

fn default_show_popup() -> bool {
    true
}

fn default_skip_unresponsive_devices() -> bool {
    true
}

impl Settings {
    pub fn is_device_excluded(&self, name: &str) -> bool {
        let key = device_name_key(name);
        self.excluded_devices
            .iter()
            .any(|excluded| device_name_key(excluded) == key)
    }

    /// Add the device to `excluded_devices`, or remove it if already there.
    pub fn toggle_excluded_device(&mut self, name: &str) {
        if self.is_device_excluded(name) {
            let key = device_name_key(name);
            self.excluded_devices
                .retain(|excluded| device_name_key(excluded) != key);
        } else {
            self.excluded_devices.push(name.trim().to_string());
        }
    }

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
        if let Some(path) = Self::config_path() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let data = serde_json::to_string_pretty(self)?;
            std::fs::write(path, data)?;
        }
        Ok(())
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
        assert!(loaded.excluded_devices.is_empty());
        assert!(loaded.skip_unresponsive_devices);
    }

    #[test]
    fn test_toggle_excluded_device() {
        let mut s = Settings::default();

        s.toggle_excluded_device("Microsoft Teams Audio");
        assert!(s.is_device_excluded("microsoft teams audio "));

        s.toggle_excluded_device("MICROSOFT TEAMS AUDIO");
        assert!(s.excluded_devices.is_empty());
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
            excluded_devices: vec!["Microsoft Teams Audio".to_string()],
            skip_unresponsive_devices: false,
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings =
            serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");
        assert!(!loaded.show_popup);
        assert_eq!(loaded.excluded_devices, vec!["Microsoft Teams Audio"]);
        assert!(!loaded.skip_unresponsive_devices);

        let _ = fs::remove_file(&tmp_path);
    }
}
