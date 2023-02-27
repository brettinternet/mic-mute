use crate::config;
use anyhow::{Context, Result};
use log::trace;
use tray_icon::{
    icon::Icon,
    menu::{
        accelerator::{Accelerator, Code, Modifiers},
        Menu, MenuItem,
    },
    TrayIcon, TrayIconBuilder,
};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";

pub fn get_mute_menu_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

fn get_icon(muted: bool) -> Result<Icon> {
    trace!("Fetching icons");
    // const DARK_MIC_ON: &[u8] = include_bytes!("../mic.png");
    // const DARK_MIC_OFF: &[u8] = include_bytes!("../mic-off.png");
    const LIGHT_MIC_ON: &[u8] = include_bytes!("../mic-light.png");
    const LIGHT_MIC_OFF: &[u8] = include_bytes!("../mic-off-light.png");

    let icon = match dark_light::detect() {
        dark_light::Mode::Light if muted => LIGHT_MIC_OFF,
        dark_light::Mode::Light if !muted => LIGHT_MIC_ON,
        dark_light::Mode::Dark if muted => LIGHT_MIC_OFF,
        _ => LIGHT_MIC_ON,
    };

    let (icon_rgba, icon_width, icon_height) = {
        let image = image::load_from_memory(icon)
            .context("Failed to open icon path")?
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };

    let icon = tray_icon::icon::Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .context("Failed to open icon")?;

    Ok(icon)
}

unsafe impl Send for Tray {}
unsafe impl Sync for Tray {}

pub struct Tray {
    pub systray: TrayIcon,
    pub toggle_mute: MenuItem,
    pub quit: MenuItem,
}

impl Tray {
    pub fn new(muted: bool) -> Result<Self> {
        trace!("Creating tray icon");
        let app_name = config::get_app_name();
        let icon = get_icon(muted)?;
        let tray_menu = Menu::new();
        let mute_shortcut = Accelerator::new(Some(Modifiers::SHIFT | Modifiers::META), Code::KeyA);
        let toggle_mute = MenuItem::new(get_mute_menu_text(muted), true, Some(mute_shortcut));
        let quit = MenuItem::new("Exit".to_owned(), true, None);
        tray_menu.append_items(&[&toggle_mute, &quit]);

        let systray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip(format!("{} service is running", app_name))
            .with_icon(icon)
            .build()
            .context("Failed to create tray icon")?;

        trace!("Tray item created");
        let tray = Self {
            systray,
            toggle_mute,
            quit,
        };
        Ok(tray)
    }

    pub fn update(&mut self, muted: bool) -> Result<()> {
        trace!("Updating tray with {} state", get_mute_menu_text(muted));
        self.update_icon(muted)?;
        self.update_menu(muted)?;
        Ok(())
    }

    fn update_icon(&mut self, muted: bool) -> Result<()> {
        let icon = get_icon(muted)?;
        self.systray.set_icon(Some(icon))?;
        trace!("Updated tray icon");
        Ok(())
    }

    fn update_menu(&mut self, muted: bool) -> Result<()> {
        self.toggle_mute.set_text(get_mute_menu_text(muted));
        trace!("Updated tray menu");
        Ok(())
    }
}
