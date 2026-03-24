use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutConfig {
    pub modifiers: Vec<String>, // ["shift", "meta", "ctrl", "alt"]
    pub key: String,            // "A", "M", etc.
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            modifiers: vec!["shift".to_string(), "meta".to_string()],
            key: "A".to_string(),
        }
    }
}

fn default_camera_shortcut() -> ShortcutConfig {
    ShortcutConfig {
        modifiers: vec!["shift".to_string(), "meta".to_string()],
        key: "O".to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mic_shortcut: ShortcutConfig,
    #[serde(default = "default_camera_shortcut")]
    pub camera_shortcut: ShortcutConfig,
    #[serde(default)]
    pub show_in_dock: bool,
    #[serde(default)]
    pub launch_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_shortcut: ShortcutConfig::default(),
            camera_shortcut: default_camera_shortcut(),
            show_in_dock: false,
            launch_at_login: false,
        }
    }
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
    fn test_settings_default() {
        let s = Settings::default();
        assert_eq!(s.camera_shortcut.key, "O");
        assert!(s.camera_shortcut.modifiers.contains(&"shift".to_string()));
        assert!(s.camera_shortcut.modifiers.contains(&"meta".to_string()));
    }

    #[test]
    fn test_settings_json_round_trip() {
        let mut s = Settings::default();
        s.camera_shortcut = ShortcutConfig {
            modifiers: vec!["shift".to_string(), "meta".to_string()],
            key: "V".to_string(),
        };

        let json = serde_json::to_string(&s).unwrap();
        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "A");
        assert_eq!(loaded.camera_shortcut.key, "V");
    }

    #[test]
    fn test_settings_save_and_load() {
        use std::fs;
        use std::path::PathBuf;

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
            camera_shortcut: ShortcutConfig {
                modifiers: vec!["shift".to_string(), "meta".to_string()],
                key: "O".to_string(),
            },
            show_in_dock: false,
            launch_at_login: false,
        };

        let json = serde_json::to_string_pretty(&s).unwrap();
        fs::write(&tmp_path, &json).unwrap();

        let loaded: Settings = serde_json::from_str(&fs::read_to_string(&tmp_path).unwrap()).unwrap();
        assert_eq!(loaded.mic_shortcut.key, "M");

        let _ = fs::remove_file(&tmp_path);
    }
}
