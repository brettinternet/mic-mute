use crate::config::AppVars;
use anyhow::{Context, Result};
use log::trace;
use muda::{
    accelerator::{Accelerator, Code, Modifiers},
    CheckMenuItem, Menu, MenuItem, MenuId, PredefinedMenuItem,
};
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

fn get_image(muted: bool, theme: Theme) -> Result<(Vec<u8>, u32, u32)> {
    const LIGHT_MIC_ON: &[u8] = include_bytes!("../assets/images/mic-light.png");
    const LIGHT_MIC_OFF: &[u8] = include_bytes!("../assets/images/mic-off-light.png");

    let image = match theme {
        Theme::Light if muted => LIGHT_MIC_OFF,
        Theme::Light if !muted => LIGHT_MIC_ON,
        Theme::Dark if muted => LIGHT_MIC_OFF,
        _ => LIGHT_MIC_ON,
    };

    let image_buff = image::load_from_memory(image)
        .context("Failed to open icon path")?
        .into_rgba8();
    let (width, height) = image_buff.dimensions();
    let rgba = image_buff.into_raw();
    Ok((rgba, width, height))
}

fn get_icon(muted: bool, theme: Theme) -> Result<Icon> {
    trace!("Fetching icons");
    let (icon_rgba, icon_width, icon_height) = get_image(muted, theme)?;
    let icon = Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .context("Failed to open icon")?;
    Ok(icon)
}

unsafe impl Send for Tray {}
unsafe impl Sync for Tray {}

impl fmt::Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TrayIcon ID: {:?}", self.systray.id())
    }
}

pub struct Tray {
    pub systray: TrayIcon,
    pub toggle_mute: MenuItem,
    pub toggle_camera: MenuItem,
    pub launch_at_login: CheckMenuItem,
    pub show_in_dock: CheckMenuItem,
    pub preferences: MenuItem,
    pub quit: MenuItem,
}

impl Tray {
    pub fn new(
        muted: bool,
        theme: Theme,
        app_vars: AppVars,
        login_enabled: bool,
        dock_visible: bool,
    ) -> Result<Self> {
        trace!("Creating tray icon");
        let icon = get_icon(muted, theme)?;
        let tray_menu = Menu::new();
        let mute_shortcut = Accelerator::new(Some(Modifiers::SHIFT | Modifiers::META), Code::KeyA);
        let toggle_mute = MenuItem::new(get_mute_menu_text(muted), true, Some(mute_shortcut));
        let camera_shortcut = Accelerator::new(Some(Modifiers::SHIFT | Modifiers::META), Code::KeyO);
        let toggle_camera = MenuItem::new("Toggle Camera", true, Some(camera_shortcut));
        let launch_at_login = CheckMenuItem::new("Launch at Login", true, login_enabled, None);
        let show_in_dock = CheckMenuItem::new("Show in Dock", true, dock_visible, None);
        let preferences = MenuItem::new("Preferences\u{2026}", true, None);
        let quit = MenuItem::new("Exit", true, None);

        tray_menu
            .append_items(&[
                &toggle_mute,
                &toggle_camera,
                &PredefinedMenuItem::separator(),
                &launch_at_login,
                &show_in_dock,
                &preferences,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .context("Failed to append menu items")?;

        let systray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip(format!("{} service is running", app_vars.name))
            .with_icon(icon)
            .with_menu_on_left_click(true)
            .build()
            .context("Failed to create tray icon")?;

        trace!("Tray item created");
        let tray = Self {
            systray,
            toggle_mute,
            toggle_camera,
            launch_at_login,
            show_in_dock,
            preferences,
            quit,
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

    pub fn toggle_mute_id(&self) -> &MenuId {
        self.toggle_mute.id()
    }

    pub fn toggle_camera_id(&self) -> &MenuId {
        self.toggle_camera.id()
    }

    pub fn launch_at_login_id(&self) -> &MenuId {
        self.launch_at_login.id()
    }

    pub fn show_in_dock_id(&self) -> &MenuId {
        self.show_in_dock.id()
    }

    pub fn preferences_id(&self) -> &MenuId {
        self.preferences.id()
    }

    pub fn quit_id(&self) -> &MenuId {
        self.quit.id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_mute_menu_text_muted() {
        assert_eq!(get_mute_menu_text(true), "Unmute");
    }

    #[test]
    fn test_get_mute_menu_text_unmuted() {
        assert_eq!(get_mute_menu_text(false), "Mute");
    }
}
