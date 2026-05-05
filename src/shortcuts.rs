use crate::settings::{Settings, ShortcutConfig};
use anyhow::{Context, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyManager,
};

#[allow(dead_code)]
pub struct Shortcuts {
    hotkeys_manager: GlobalHotKeyManager,
    pub mic_hotkey: HotKey,
}

fn modifiers_from_config(config: &ShortcutConfig) -> Modifiers {
    let mut mods = Modifiers::empty();
    for m in &config.modifiers {
        match m.as_str() {
            "shift" => mods |= Modifiers::SHIFT,
            "meta" | "cmd" | "command" => mods |= Modifiers::META,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" => mods |= Modifiers::ALT,
            _ => {}
        }
    }
    mods
}

fn code_from_str(key: &str) -> Code {
    match key.to_uppercase().as_str() {
        "A" => Code::KeyA,
        "B" => Code::KeyB,
        "C" => Code::KeyC,
        "D" => Code::KeyD,
        "E" => Code::KeyE,
        "F" => Code::KeyF,
        "G" => Code::KeyG,
        "H" => Code::KeyH,
        "I" => Code::KeyI,
        "J" => Code::KeyJ,
        "K" => Code::KeyK,
        "L" => Code::KeyL,
        "M" => Code::KeyM,
        "N" => Code::KeyN,
        "O" => Code::KeyO,
        "P" => Code::KeyP,
        "Q" => Code::KeyQ,
        "R" => Code::KeyR,
        "S" => Code::KeyS,
        "T" => Code::KeyT,
        "U" => Code::KeyU,
        "V" => Code::KeyV,
        "W" => Code::KeyW,
        "X" => Code::KeyX,
        "Y" => Code::KeyY,
        "Z" => Code::KeyZ,
        "F1" => Code::F1,
        "F2" => Code::F2,
        "F3" => Code::F3,
        "F4" => Code::F4,
        "F5" => Code::F5,
        "F6" => Code::F6,
        "F7" => Code::F7,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        "F11" => Code::F11,
        "F12" => Code::F12,
        "F13" => Code::F13,
        "F14" => Code::F14,
        "F15" => Code::F15,
        "F16" => Code::F16,
        "F17" => Code::F17,
        "F18" => Code::F18,
        "F19" => Code::F19,
        "F20" => Code::F20,
        "F21" => Code::F21,
        "F22" => Code::F22,
        "F23" => Code::F23,
        "F24" => Code::F24,
        _ => Code::KeyA,
    }
}

fn hotkey_from_config(config: &ShortcutConfig) -> HotKey {
    let mods = modifiers_from_config(config);
    let code = code_from_str(&config.key);
    HotKey::new(Some(mods), code)
}

impl Shortcuts {
    pub fn new(settings: &Settings) -> Result<Self> {
        let hotkeys_manager = GlobalHotKeyManager::new().unwrap();

        let mic_hotkey = hotkey_from_config(&settings.mic_shortcut);

        hotkeys_manager
            .register(mic_hotkey)
            .context("Failed to register mic hotkey")?;

        Ok(Self {
            hotkeys_manager,
            mic_hotkey,
        })
    }

    /// Unregister the current hotkeys and register new ones from updated settings.
    pub fn reload(&mut self, settings: &Settings) -> Result<()> {
        let _ = self.hotkeys_manager.unregister(self.mic_hotkey);

        self.mic_hotkey = hotkey_from_config(&settings.mic_shortcut);

        self.hotkeys_manager
            .register(self.mic_hotkey)
            .context("Failed to register mic hotkey")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutConfig;

    #[test]
    fn test_code_from_str_uppercase() {
        assert!(matches!(code_from_str("A"), Code::KeyA));
        assert!(matches!(code_from_str("V"), Code::KeyV));
    }

    #[test]
    fn test_code_from_str_lowercase() {
        assert!(matches!(code_from_str("a"), Code::KeyA));
        assert!(matches!(code_from_str("v"), Code::KeyV));
    }

    #[test]
    fn test_code_from_str_function_keys() {
        assert!(matches!(code_from_str("F1"), Code::F1));
        assert!(matches!(code_from_str("F13"), Code::F13));
        assert!(matches!(code_from_str("F24"), Code::F24));
    }

    #[test]
    fn test_hotkey_from_config_no_modifiers() {
        let config = ShortcutConfig {
            modifiers: vec![],
            key: "F13".to_string(),
        };
        let mods = modifiers_from_config(&config);
        assert!(mods.is_empty());
    }

    #[test]
    fn test_modifiers_from_config() {
        let config = ShortcutConfig {
            modifiers: vec!["shift".to_string(), "meta".to_string()],
            key: "A".to_string(),
        };
        let mods = modifiers_from_config(&config);
        assert!(mods.contains(Modifiers::SHIFT));
        assert!(mods.contains(Modifiers::META));
        assert!(!mods.contains(Modifiers::CONTROL));
    }

    #[test]
    fn test_modifiers_from_config_all() {
        let config = ShortcutConfig {
            modifiers: vec![
                "shift".to_string(),
                "ctrl".to_string(),
                "alt".to_string(),
                "meta".to_string(),
            ],
            key: "A".to_string(),
        };
        let mods = modifiers_from_config(&config);
        assert!(mods.contains(Modifiers::SHIFT));
        assert!(mods.contains(Modifiers::CONTROL));
        assert!(mods.contains(Modifiers::ALT));
        assert!(mods.contains(Modifiers::META));
    }
}
