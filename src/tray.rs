use crate::config::AppVars;
use crate::icons::{self, IconVariant, TrayImage};
use crate::shortcuts::Shortcut;
use anyhow::{Context, Result};
use log::trace;
use muda::accelerator::{Key, KeyAccelerator};
use muda::{accelerator::Accelerator, CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use std::fmt;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";
/// The tooltip's last line while a camera is in use, which explains the icon's badge.
const CAMERA_IN_USE_TEXT: &str = "Camera in use";

pub fn get_mute_menu_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

/// What the tray shows: its icon with its template flag, the Mute item's text, and whether the
/// tooltip says a camera is in use. Light and dark menu bars need no state: macOS tints the
/// unmuted template icon, and the muted red icon reads on both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrayView {
    image: TrayImage,
    menu_text: &'static str,
    camera_in_use: bool,
}

impl TrayView {
    fn new(muted: bool, camera_in_use: bool) -> Self {
        Self {
            image: icons::tray_mic(muted, camera_in_use),
            menu_text: get_mute_menu_text(muted),
            camera_in_use,
        }
    }
}

/// The tooltip: `notice` while settings.json is unusable, else `base`, and a second line while a
/// camera is in use.
fn tooltip_text(base: &str, notice: Option<&str>, camera_in_use: bool) -> String {
    let first = notice.unwrap_or(base);
    if camera_in_use {
        format!("{first}\n{CAMERA_IN_USE_TEXT}")
    } else {
        first.to_string()
    }
}

/// A tray icon of `variant`, built from its cached render.
fn get_icon(variant: IconVariant) -> Result<Icon> {
    let rendered = variant.rendered()?;
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
    /// The tooltip's first line while settings.json is usable.
    tooltip: String,
    /// Disabled first menu item, and the separator below it, while settings.json is unusable.
    settings_notice: Option<(MenuItem, PredefinedMenuItem)>,
    /// What the icon, the Mute item, and the tooltip show now; `update` leaves matching parts
    /// alone.
    shown: TrayView,
}

impl Tray {
    pub fn new(
        muted: bool,
        camera_in_use: bool,
        app_vars: AppVars,
        login_enabled: bool,
        dock_visible: bool,
        popup_visible: bool,
    ) -> Result<Self> {
        trace!("Creating tray icon");
        let shown = TrayView::new(muted, camera_in_use);
        let icon = get_icon(shown.image.variant)?;
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
            .with_tooltip(tooltip_text(&tooltip, None, shown.camera_in_use))
            .with_icon(icon)
            .with_icon_as_template(shown.image.is_template)
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
            shown,
        };
        Ok(tray)
    }

    /// Shows `muted` and `camera_in_use` in the icon, the Mute item, and the tooltip, touching
    /// only the parts that change, so a repeated call with the same state costs nothing.
    pub fn update(&mut self, muted: bool, camera_in_use: bool) -> Result<()> {
        let next = TrayView::new(muted, camera_in_use);
        self.update_menu(next.menu_text);
        self.update_icon(next.image)?;
        self.update_tooltip(next.camera_in_use)
    }

    /// Sets the icon and its template flag in one call: tray-icon's `set_icon` always clears the
    /// flag. A failed icon stays unrecorded, so the next `update` retries it.
    fn update_icon(&mut self, image: TrayImage) -> Result<()> {
        if image == self.shown.image {
            return Ok(());
        }
        let icon = get_icon(image.variant)?;
        self.systray
            .set_icon_with_as_template(Some(icon), image.is_template)?;
        self.shown.image = image;
        trace!("Updated tray icon: {:?}", image);
        Ok(())
    }

    fn update_tooltip(&mut self, camera_in_use: bool) -> Result<()> {
        if camera_in_use == self.shown.camera_in_use {
            return Ok(());
        }
        let notice = self.settings_notice.as_ref().map(|(item, _)| item.text());
        self.show_tooltip(notice.as_deref(), camera_in_use)?;
        self.shown.camera_in_use = camera_in_use;
        trace!("Updated tray tooltip: camera in use {}", camera_in_use);
        Ok(())
    }

    fn show_tooltip(&self, notice: Option<&str>, camera_in_use: bool) -> Result<()> {
        self.systray
            .set_tooltip(Some(tooltip_text(&self.tooltip, notice, camera_in_use)))
            .context("Failed to update tray tooltip")
    }

    fn update_menu(&mut self, text: &'static str) {
        if text == self.shown.menu_text {
            return;
        }
        self.toggle_mute.set_text(text);
        self.shown.menu_text = text;
        trace!("Updated tray menu: {}", text);
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

    /// Shows `notice` as a disabled first menu item and as the tooltip's first line; `None`
    /// removes it. Does nothing when the notice is unchanged.
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
        self.show_tooltip(notice, self.shown.camera_in_use)?;
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
    fn tray_view_shows_a_template_icon_only_while_unmuted() {
        for camera_in_use in [false, true] {
            let muted = TrayView::new(true, camera_in_use);
            let unmuted = TrayView::new(false, camera_in_use);
            assert!(!muted.image.is_template, "camera in use {camera_in_use}");
            assert!(unmuted.image.is_template, "camera in use {camera_in_use}");
            assert_ne!(muted.image.variant, unmuted.image.variant);
            assert_eq!(
                (muted.menu_text, unmuted.menu_text),
                (UNMUTE_TEXT, MUTE_TEXT)
            );
        }
    }

    #[test]
    fn tray_view_changes_only_the_icon_variant_and_tooltip_with_the_camera() {
        for muted in [false, true] {
            let idle = TrayView::new(muted, false);
            let in_use = TrayView::new(muted, true);
            assert_ne!(idle.image.variant, in_use.image.variant, "muted {muted}");
            assert_eq!(idle.image.is_template, in_use.image.is_template);
            assert_eq!(idle.menu_text, in_use.menu_text);
            assert_eq!((idle.camera_in_use, in_use.camera_in_use), (false, true));
        }
    }

    #[test]
    fn tooltip_adds_a_camera_line_only_while_a_camera_is_in_use() {
        let base = "Mic Mute service is running";
        assert_eq!(tooltip_text(base, None, false), base);
        assert_eq!(
            tooltip_text(base, None, true),
            "Mic Mute service is running\nCamera in use"
        );
    }

    #[test]
    fn tooltip_shows_the_settings_notice_instead_of_the_base_text() {
        let notice = "settings.json is invalid";
        assert_eq!(tooltip_text("base", Some(notice), false), notice);
        assert_eq!(
            tooltip_text("base", Some(notice), true),
            "settings.json is invalid\nCamera in use"
        );
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
