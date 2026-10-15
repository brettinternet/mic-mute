use crate::config::AppVars;
use crate::icons;
use crate::shortcuts::Shortcut;
use anyhow::{Context, Result};
use log::trace;
use muda::accelerator::{Key, KeyAccelerator};
use muda::{accelerator::Accelerator, CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use std::fmt;
use tao::window::Theme;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";

pub fn get_mute_menu_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

/// The tray icon for `muted`, built from the cached render; the theme doesn't change it yet.
fn get_icon(muted: bool, _theme: Theme) -> Result<Icon> {
    let rendered = icons::tray_mic(muted).rendered()?;
    let icon = Icon::from_rgba(rendered.rgba.clone(), rendered.width, rendered.height)
        .context("Failed to open icon")?;
    Ok(icon)
}

/// The Mute item's key-equivalent label for `shortcut`.
fn accelerator_for(shortcut: Shortcut) -> Accelerator {
    Accelerator::new(Some(shortcut.modifiers()), shortcut.code())
}

impl fmt::Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TrayIcon ID: {:?}", self.systray.id())
    }
}

pub struct Tray {
    pub systray: TrayIcon,
    pub toggle_mute: MenuItem,
    pub launch_at_login: CheckMenuItem,
    pub show_in_dock: CheckMenuItem,
    pub show_popup: CheckMenuItem,
    pub about: MenuItem,
    pub quit: MenuItem,
    /// The menu `systray` shows; kept to insert and remove the settings notice.
    menu: Menu,
    /// Tooltip while settings.json is usable.
    tooltip: String,
    /// Disabled first menu item, and the separator below it, while settings.json is unusable.
    settings_notice: Option<(MenuItem, PredefinedMenuItem)>,
}

impl Tray {
    pub fn new(
        muted: bool,
        theme: Theme,
        app_vars: AppVars,
        login_enabled: bool,
        dock_visible: bool,
        popup_visible: bool,
    ) -> Result<Self> {
        trace!("Creating tray icon");
        let icon = get_icon(muted, theme)?;
        let tray_menu = Menu::new();
        // The shortcut label is set later with `update_accelerators`: muda fails to
        // build the whole menu when an item has a key it can't show (e.g. CapsLock).
        let toggle_mute = MenuItem::new(get_mute_menu_text(muted), true, None);
        let launch_at_login = CheckMenuItem::new("Launch at Login", true, login_enabled, None);
        let show_in_dock = CheckMenuItem::new("Show in Dock", true, dock_visible, None);
        let show_popup = CheckMenuItem::new("Show Popup", true, popup_visible, None);
        let about = MenuItem::new("About", true, None);
        let quit = MenuItem::new("Exit", true, None);

        tray_menu
            .append_items(&[
                &toggle_mute,
                &PredefinedMenuItem::separator(),
                &launch_at_login,
                &show_in_dock,
                &show_popup,
                &about,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .context("Failed to append menu items")?;

        let tooltip = format!("{} service is running", app_vars.name);
        let systray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu.clone()))
            .with_tooltip(&tooltip)
            .with_icon(icon)
            .with_menu_on_left_click(true)
            .build()
            .context("Failed to create tray icon")?;

        trace!("Tray item created");
        let tray = Self {
            systray,
            toggle_mute,
            launch_at_login,
            show_in_dock,
            show_popup,
            about,
            quit,
            menu: tray_menu,
            tooltip,
            settings_notice: None,
        };
        Ok(tray)
    }

    pub fn update(&mut self, muted: bool, theme: Theme) -> Result<()> {
        trace!("Updating tray with {} state", get_mute_menu_text(muted));
        self.update_icon(muted, theme)?;
        self.update_menu(muted)?;
        Ok(())
    }

    fn update_icon(&mut self, muted: bool, theme: Theme) -> Result<()> {
        let icon = get_icon(muted, theme)?;
        self.systray.set_icon(Some(icon))?;
        trace!("Updated tray icon");
        Ok(())
    }

    fn update_menu(&mut self, muted: bool) -> Result<()> {
        self.toggle_mute.set_text(get_mute_menu_text(muted));
        trace!("Updated tray menu");
        Ok(())
    }

    /// Shows `mic_shortcut`, the registered shortcut, on the Mute item; `None` shows none.
    ///
    /// # Errors
    ///
    /// When muda can't show the key (e.g. CapsLock or a media key); the item then shows
    /// no shortcut rather than a stale one.
    pub fn update_accelerators(&mut self, mic_shortcut: Option<Shortcut>) -> Result<()> {
        let Some(shortcut) = mic_shortcut else {
            return self.clear_mute_accelerator();
        };
        let shown = self
            .toggle_mute
            .set_accelerator(Some(accelerator_for(shortcut)))
            .with_context(|| format!("Failed to show the {shortcut} shortcut in the tray menu"));
        if shown.is_err() {
            self.clear_mute_accelerator()?;
        }
        shown
    }

    /// muda 0.19 on macOS keeps an item's old key equivalent on `set_accelerator(None)`
    /// and on a failed `set_accelerator`; an empty key equivalent is what it gives items
    /// created without one.
    fn clear_mute_accelerator(&mut self) -> Result<()> {
        self.toggle_mute
            .set_key_accelerator(Some(KeyAccelerator::new(
                None,
                Key::Character(String::new()),
            )))
            .context("Failed to clear the mute shortcut in the tray menu")
    }

    pub fn toggle_mute_id(&self) -> &MenuId {
        self.toggle_mute.id()
    }

    pub fn launch_at_login_id(&self) -> &MenuId {
        self.launch_at_login.id()
    }

    pub fn show_in_dock_id(&self) -> &MenuId {
        self.show_in_dock.id()
    }

    pub fn show_popup_id(&self) -> &MenuId {
        self.show_popup.id()
    }

    pub fn about_id(&self) -> &MenuId {
        self.about.id()
    }

    pub fn quit_id(&self) -> &MenuId {
        self.quit.id()
    }

    /// Shows `notice` as a disabled first menu item and as the tooltip; `None` removes it.
    /// Does nothing when the notice is unchanged.
    pub fn set_settings_notice(&mut self, notice: Option<&str>) -> Result<()> {
        let shown = self.settings_notice.as_ref().map(|(item, _)| item.text());
        if shown.as_deref() == notice {
            return Ok(());
        }
        match (notice, self.settings_notice.take()) {
            (Some(text), Some((item, separator))) => {
                item.set_text(text);
                self.settings_notice = Some((item, separator));
            }
            (Some(text), None) => {
                let item = MenuItem::new(text, false, None);
                let separator = PredefinedMenuItem::separator();
                self.menu
                    .prepend_items(&[&item, &separator])
                    .context("Failed to add settings notice to tray menu")?;
                self.settings_notice = Some((item, separator));
            }
            (None, Some((item, separator))) => {
                self.menu
                    .remove(&item)
                    .and_then(|()| self.menu.remove(&separator))
                    .context("Failed to remove settings notice from tray menu")?;
            }
            (None, None) => {}
        }
        self.systray
            .set_tooltip(Some(notice.unwrap_or(&self.tooltip)))
            .context("Failed to update tray tooltip")?;
        trace!("Updated settings notice: {:?}", notice);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ShortcutConfig;
    use crate::shortcuts::parse_shortcut;
    use muda::accelerator::{Code, Modifiers};

    #[test]
    fn accelerator_matches_muda_parse_of_the_same_shortcut() {
        let shortcut = parse_shortcut(&ShortcutConfig::default()).unwrap();
        let expected: Accelerator = "shift+cmd+KeyM".parse().unwrap();
        assert_eq!(accelerator_for(shortcut), expected);
    }

    #[test]
    fn accelerator_keeps_every_modifier() {
        let config = ShortcutConfig {
            modifiers: ["ctrl", "alt", "shift", "meta"].map(String::from).to_vec(),
            key: "F13".to_string(),
        };
        let accelerator = accelerator_for(parse_shortcut(&config).unwrap());
        let all = Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER;
        assert_eq!(accelerator.modifiers(), all);
        assert_eq!(accelerator.key(), Code::F13);
    }

    #[test]
    fn test_get_mute_menu_text_muted() {
        assert_eq!(get_mute_menu_text(true), "Unmute");
    }

    #[test]
    fn test_get_mute_menu_text_unmuted() {
        assert_eq!(get_mute_menu_text(false), "Mute");
    }
}
