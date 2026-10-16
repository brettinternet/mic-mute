//! The global mute shortcut: the one parser for `mic_shortcut` and its hotkey
//! registration. The tray label and the About text are derived from [`Shortcut`].

use crate::settings::ShortcutConfig;
use anyhow::{anyhow, bail, Context, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyManager,
};
use log::{error, info, trace, warn};
use parking_lot::Mutex;
use std::fmt;

/// A validated mute shortcut: one key and its modifiers.
///
/// `Code` and `Modifiers` are global-hotkey's keyboard-types (0.7) types. muda uses 0.8, so
/// `tray.rs::accelerator_for` builds the tray accelerator from the same values by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    mods: Modifiers,
    code: Code,
}

impl Shortcut {
    pub fn modifiers(self) -> Modifiers {
        self.mods
    }

    pub fn code(self) -> Code {
        self.code
    }

    pub fn hotkey(self) -> HotKey {
        HotKey::new(Some(self.mods), self.code)
    }
}

/// Formats the shortcut as macOS menus show it, e.g. `⇧⌘M`.
impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Apple's modifier order: Control, Option, Shift, Command.
        const GLYPHS: [(Modifiers, &str); 4] = [
            (Modifiers::CONTROL, "⌃"),
            (Modifiers::ALT, "⌥"),
            (Modifiers::SHIFT, "⇧"),
            (Modifiers::SUPER, "⌘"),
        ];
        for (modifier, glyph) in GLYPHS {
            if self.mods.contains(modifier) {
                f.write_str(glyph)?;
            }
        }
        f.write_str(&key_label(self.code))
    }
}

/// `KeyM` → `M` and `Digit1` → `1`; other keys keep their name, such as `F13` or `Space`.
fn key_label(code: Code) -> String {
    let name = code.to_string();
    let short = ["Key", "Digit"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix).filter(|rest| rest.len() == 1));
    short.unwrap_or(name.as_str()).to_string()
}

/// Parses `mic_shortcut`. Modifier and key names are case-insensitive.
///
/// # Errors
///
/// An unknown modifier or key, a modifier or a combination given as the key, and a key
/// without modifiers other than F13–F24: a plain key would be taken from every app.
pub fn parse_shortcut(config: &ShortcutConfig) -> Result<Shortcut> {
    let mut mods = Modifiers::empty();
    for token in &config.modifiers {
        mods |= parse_modifier(token).ok_or_else(|| {
            anyhow!(
                "unknown modifier {token:?} (use shift, meta/cmd/command, ctrl/control, \
                 or alt/option)"
            )
        })?;
    }
    let code = parse_key(&config.key)?;
    if mods.is_empty() && !works_without_modifiers(code) {
        bail!(
            "key {:?} needs a modifier; only F13–F24 work without one",
            config.key
        );
    }
    Ok(Shortcut { mods, code })
}

fn parse_modifier(token: &str) -> Option<Modifiers> {
    match token.trim().to_ascii_lowercase().as_str() {
        "shift" => Some(Modifiers::SHIFT),
        "meta" | "cmd" | "command" => Some(Modifiers::SUPER),
        "ctrl" | "control" => Some(Modifiers::CONTROL),
        "alt" | "option" => Some(Modifiers::ALT),
        _ => None,
    }
}

/// Parses one key name with global-hotkey's grammar (`M`, `KeyM`, `F13`, `Space`, …).
fn parse_key(key: &str) -> Result<Code> {
    let key = key.trim();
    if key.is_empty() {
        bail!("key is empty");
    }
    if parse_modifier(key).is_some() {
        bail!("key {key:?} is a modifier; list it under modifiers");
    }
    // global-hotkey joins a combination with `+`; a single token never has modifiers.
    if key.len() > 1 && key.contains('+') {
        bail!("key {key:?} is a combination; list its modifiers under modifiers");
    }
    key.parse::<HotKey>()
        .map(|hotkey| hotkey.key)
        .map_err(|_| anyhow!("unknown key {key:?}"))
}

/// F13–F24 have no standard use, so they may be bound without modifiers.
fn works_without_modifiers(code: Code) -> bool {
    matches!(
        code,
        Code::F13
            | Code::F14
            | Code::F15
            | Code::F16
            | Code::F17
            | Code::F18
            | Code::F19
            | Code::F20
            | Code::F21
            | Code::F22
            | Code::F23
            | Code::F24
    )
}

/// Why the configured shortcut isn't the registered one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutProblem {
    /// `mic_shortcut` doesn't parse.
    Invalid { reason: String },
    /// `mic_shortcut` parses, but registering it failed.
    NotRegistered { shortcut: Shortcut, reason: String },
}

impl fmt::Display for ShortcutProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { reason } => write!(f, "The mic_shortcut setting is invalid: {reason}"),
            Self::NotRegistered { shortcut, reason } => {
                write!(f, "{shortcut} could not be registered: {reason}")
            }
        }
    }
}

/// The mute shortcut as the About window reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortcutStatus {
    /// The shortcut the global hotkey is registered for; `None` leaves only the tray menu.
    pub registered: Option<Shortcut>,
    /// Why the configured shortcut isn't the registered one, if it isn't.
    pub problem: Option<ShortcutProblem>,
}

/// `Shortcuts` publishes its status here because `about::AboutWindow` has no access to `UI`.
static STATUS: Mutex<ShortcutStatus> = Mutex::new(ShortcutStatus {
    registered: None,
    problem: None,
});

/// The mute shortcut's status as `Shortcuts` last published it.
pub fn current_status() -> ShortcutStatus {
    STATUS.lock().clone()
}

/// What [`Shortcuts::reload`] has to do.
#[derive(Debug, PartialEq, Eq)]
enum Reload {
    /// The configured shortcut is already registered.
    Unchanged,
    /// Register this shortcut, then unregister the current one.
    Register(Shortcut),
}

/// Plans the move from the `registered` shortcut to `config`.
///
/// # Errors
///
/// When `config` is invalid; the registered shortcut then stays.
fn plan_reload(registered: Option<Shortcut>, config: &ShortcutConfig) -> Result<Reload> {
    let configured = parse_shortcut(config)?;
    if registered == Some(configured) {
        Ok(Reload::Unchanged)
    } else {
        Ok(Reload::Register(configured))
    }
}

/// Whether `problem` is the default shortcut failing to register, which makes retrying
/// the default as a fallback pointless.
fn is_default_failure(problem: &ShortcutProblem, default: Shortcut) -> bool {
    matches!(problem, ShortcutProblem::NotRegistered { shortcut, .. } if *shortcut == default)
}

/// Registers the mute shortcut as a global hotkey. Main thread only.
pub struct Shortcuts {
    manager: GlobalHotKeyManager,
    status: ShortcutStatus,
}

impl Shortcuts {
    /// Registers `config`. When it is invalid or can't be registered, logs an error and
    /// registers the default shortcut instead; when that fails too, runs without one.
    ///
    /// # Errors
    ///
    /// Only when the hotkey manager can't be created.
    pub fn new(config: &ShortcutConfig) -> Result<Self> {
        let manager =
            GlobalHotKeyManager::new().context("Failed to create the global hotkey manager")?;
        let mut shortcuts = Self {
            manager,
            status: ShortcutStatus::default(),
        };
        if let Err(e) = shortcuts.reload(config) {
            error!("Failed to apply mic_shortcut setting: {:#}", e);
            shortcuts.register_default();
        }
        Ok(shortcuts)
    }

    /// Registers `config` in place of the registered shortcut. Does nothing when it is
    /// already registered, so it is safe to rerun after every settings reload.
    ///
    /// # Errors
    ///
    /// When `config` is invalid or can't be registered; the registered shortcut stays.
    pub fn reload(&mut self, config: &ShortcutConfig) -> Result<()> {
        let new = match plan_reload(self.status.registered, config) {
            Ok(Reload::Unchanged) => {
                trace!("Mute shortcut unchanged");
                self.status.problem = None;
                self.publish();
                return Ok(());
            }
            Ok(Reload::Register(new)) => new,
            Err(e) => {
                self.status.problem = Some(ShortcutProblem::Invalid {
                    reason: format!("{e:#}"),
                });
                self.publish();
                return Err(e);
            }
        };

        // Register before unregistering, so a failure leaves the old shortcut working.
        if let Err(e) = self.manager.register(new.hotkey()) {
            self.status.problem = Some(ShortcutProblem::NotRegistered {
                shortcut: new,
                reason: e.to_string(),
            });
            self.publish();
            return Err(e).with_context(|| format!("Failed to register the {new} mute shortcut"));
        }
        if let Some(old) = self.status.registered {
            if let Err(e) = self.manager.unregister(old.hotkey()) {
                warn!(
                    "Failed to unregister the previous {} mute shortcut: {}",
                    old, e
                );
            }
        }
        info!("Registered the {} mute shortcut", new);
        self.status = ShortcutStatus {
            registered: Some(new),
            problem: None,
        };
        self.publish();
        Ok(())
    }

    /// The registered shortcut; `None` when only the tray menu can mute.
    pub fn registered(&self) -> Option<Shortcut> {
        self.status.registered
    }

    /// The id global-hotkey reports for the registered shortcut, or 0 when none is
    /// registered. No registered hotkey has id 0: it would need no modifiers and the
    /// first `Code`, and only F13–F24 are allowed without modifiers.
    pub fn registered_id(&self) -> u32 {
        self.status
            .registered
            .map_or(0, |shortcut| shortcut.hotkey().id())
    }

    /// The startup fallback after the configured shortcut failed.
    fn register_default(&mut self) {
        let default = match parse_shortcut(&ShortcutConfig::default()) {
            Ok(default) => default,
            Err(e) => {
                error!("Failed to parse the default mute shortcut: {:#}", e);
                return;
            }
        };
        let already_failed = self
            .status
            .problem
            .as_ref()
            .is_some_and(|problem| is_default_failure(problem, default));
        if !already_failed {
            match self.manager.register(default.hotkey()) {
                Ok(()) => {
                    info!("Registered the default {} mute shortcut instead", default);
                    self.status.registered = Some(default);
                    self.publish();
                    return;
                }
                Err(e) => error!(
                    "Failed to register the default {} mute shortcut: {}",
                    default, e
                ),
            }
        }
        error!("No mute shortcut is registered; mute from the tray menu");
    }

    fn publish(&self) {
        *STATUS.lock() = self.status.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(modifiers: &[&str], key: &str) -> ShortcutConfig {
        ShortcutConfig {
            modifiers: modifiers.iter().map(|m| m.to_string()).collect(),
            key: key.to_string(),
        }
    }

    fn default_shortcut() -> Shortcut {
        parse_shortcut(&ShortcutConfig::default()).unwrap()
    }

    #[test]
    fn default_config_parses_to_shift_command_m() {
        let expected = Shortcut {
            mods: Modifiers::SHIFT | Modifiers::SUPER,
            code: Code::KeyM,
        };
        assert_eq!(default_shortcut(), expected);
    }

    #[test]
    fn modifier_names_are_case_insensitive() {
        let parsed = parse_shortcut(&config(&["Shift", "CMD", "Control", "OPTION"], "m")).unwrap();
        let all = Modifiers::SHIFT | Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT;
        assert_eq!(parsed.modifiers(), all);
        assert_eq!(parsed.code(), Code::KeyM);
    }

    #[test]
    fn modifier_aliases_share_a_flag() {
        let aliases = [
            ("shift", Modifiers::SHIFT),
            ("meta", Modifiers::SUPER),
            ("cmd", Modifiers::SUPER),
            ("command", Modifiers::SUPER),
            ("ctrl", Modifiers::CONTROL),
            ("control", Modifiers::CONTROL),
            ("alt", Modifiers::ALT),
            ("option", Modifiers::ALT),
        ];
        for (alias, flag) in aliases {
            let parsed = parse_shortcut(&config(&[alias], "M")).unwrap();
            assert_eq!(parsed.modifiers(), flag, "{alias}");
        }
    }

    #[test]
    fn unknown_modifier_is_an_error_that_names_it() {
        let err = parse_shortcut(&config(&["shift", "hyper"], "M")).unwrap_err();
        assert!(
            err.to_string().contains("unknown modifier \"hyper\""),
            "{err}"
        );
    }

    #[test]
    fn unknown_key_is_an_error_instead_of_a() {
        let err = parse_shortcut(&config(&["shift", "meta"], "Foo")).unwrap_err();
        assert!(err.to_string().contains("unknown key \"Foo\""), "{err}");
    }

    #[test]
    fn empty_key_is_an_error() {
        assert!(parse_shortcut(&config(&["meta"], " ")).is_err());
    }

    #[test]
    fn modifier_as_key_is_an_error() {
        let err = parse_shortcut(&config(&["meta"], "Shift")).unwrap_err();
        assert!(err.to_string().contains("is a modifier"), "{err}");
    }

    #[test]
    fn combination_as_key_is_an_error() {
        for key in ["cmd+M", "M+N", "shift+"] {
            let err = parse_shortcut(&config(&["meta"], key)).unwrap_err();
            assert!(err.to_string().contains("is a combination"), "{key}: {err}");
        }
    }

    #[test]
    fn plain_key_is_rejected() {
        for key in ["M", "F12", "Space"] {
            let err = parse_shortcut(&config(&[], key)).unwrap_err();
            assert!(err.to_string().contains("needs a modifier"), "{key}: {err}");
        }
    }

    #[test]
    fn plain_f13_to_f24_are_allowed() {
        for (key, code) in [("F13", Code::F13), ("f20", Code::F20), ("F24", Code::F24)] {
            let parsed = parse_shortcut(&config(&[], key)).unwrap();
            assert_eq!(parsed.modifiers(), Modifiers::empty(), "{key}");
            assert_eq!(parsed.code(), code, "{key}");
        }
    }

    #[test]
    fn key_names_are_case_insensitive() {
        let keys = [
            ("a", Code::KeyA),
            ("V", Code::KeyV),
            ("keym", Code::KeyM),
            ("1", Code::Digit1),
            ("f1", Code::F1),
            ("space", Code::Space),
        ];
        for (key, code) in keys {
            let parsed = parse_shortcut(&config(&["ctrl"], key)).unwrap();
            assert_eq!(parsed.code(), code, "{key}");
        }
    }

    #[test]
    fn hotkey_uses_the_parsed_modifiers_and_key() {
        let expected = HotKey::new(Some(Modifiers::SHIFT | Modifiers::META), Code::KeyM);
        assert_eq!(default_shortcut().hotkey(), expected);
    }

    #[test]
    fn glyphs_follow_apple_modifier_order() {
        assert_eq!(default_shortcut().to_string(), "⇧⌘M");
        let all = parse_shortcut(&config(&["meta", "shift", "alt", "ctrl"], "M")).unwrap();
        assert_eq!(all.to_string(), "⌃⌥⇧⌘M");
    }

    #[test]
    fn glyphs_shorten_only_letter_and_digit_names() {
        let label = |mods, code| Shortcut { mods, code }.to_string();
        assert_eq!(label(Modifiers::SUPER, Code::Digit1), "⌘1");
        assert_eq!(label(Modifiers::empty(), Code::F13), "F13");
        assert_eq!(
            label(Modifiers::SHIFT | Modifiers::SUPER, Code::Space),
            "⇧⌘Space"
        );
        assert_eq!(
            label(Modifiers::SUPER, Code::KeyboardLayoutSelect),
            "⌘KeyboardLayoutSelect"
        );
    }

    #[test]
    fn reload_leaves_the_registered_shortcut_alone() {
        let plan = plan_reload(Some(default_shortcut()), &ShortcutConfig::default()).unwrap();
        assert_eq!(plan, Reload::Unchanged);
    }

    #[test]
    fn reload_treats_a_respelled_shortcut_as_unchanged() {
        let respelled = config(&["COMMAND", "Shift"], "keyM");
        let plan = plan_reload(Some(default_shortcut()), &respelled).unwrap();
        assert_eq!(plan, Reload::Unchanged);
    }

    #[test]
    fn reload_registers_a_changed_shortcut() {
        let plan = plan_reload(Some(default_shortcut()), &config(&["ctrl", "alt"], "M")).unwrap();
        let expected = Shortcut {
            mods: Modifiers::CONTROL | Modifiers::ALT,
            code: Code::KeyM,
        };
        assert_eq!(plan, Reload::Register(expected));
    }

    #[test]
    fn reload_registers_when_nothing_is_registered() {
        let plan = plan_reload(None, &ShortcutConfig::default()).unwrap();
        assert_eq!(plan, Reload::Register(default_shortcut()));
    }

    #[test]
    fn reload_rejects_an_invalid_shortcut() {
        assert!(plan_reload(Some(default_shortcut()), &config(&["cmnd"], "M")).is_err());
    }

    #[test]
    fn default_is_retried_unless_it_failed_itself() {
        let default = default_shortcut();
        let custom = Shortcut {
            mods: Modifiers::CONTROL,
            code: Code::KeyM,
        };
        let invalid = ShortcutProblem::Invalid {
            reason: "unknown key \"Foo\"".to_string(),
        };
        let not_registered = |shortcut| ShortcutProblem::NotRegistered {
            shortcut,
            reason: "taken".to_string(),
        };
        assert!(!is_default_failure(&invalid, default));
        assert!(!is_default_failure(&not_registered(custom), default));
        assert!(is_default_failure(&not_registered(default), default));
    }

    #[test]
    fn problem_text_names_the_cause() {
        let invalid = ShortcutProblem::Invalid {
            reason: "unknown key \"Foo\"".to_string(),
        };
        assert_eq!(
            invalid.to_string(),
            "The mic_shortcut setting is invalid: unknown key \"Foo\""
        );
        let not_registered = ShortcutProblem::NotRegistered {
            shortcut: default_shortcut(),
            reason: "taken".to_string(),
        };
        assert_eq!(
            not_registered.to_string(),
            "⇧⌘M could not be registered: taken"
        );
    }
}
